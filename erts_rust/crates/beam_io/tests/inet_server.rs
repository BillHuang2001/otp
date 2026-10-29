// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The **end-to-end TCP server data path** over `beam_io`'s public seams: listen → accept → adopt
//! the accepted descriptor → request/response over framing + active modes → shutdown → close.
//!
//! This is the seam a Cowboy/ranch listener is driven through, and the one chain the existing
//! suites leave out: `tests/inet_accept.rs` pins the accept *chain* (completion shapes only),
//! `tests/active_mode.rs` pins framed/active delivery on a `from_socket` connection and
//! `src/drivers/inet/accept_opts_tests.rs` pins option inheritance at the driver level, but
//! nothing carries an **accepted** socket through a served request/response. `tests/support/server.rs`
//! holds the plumbing (the poller + registry rig, the numeric control vocabulary, the
//! condition-pumped round recipe and the raw-`port_control` oracle dump).
//!
//! Every wait is a condition pumped under a hang guard; the negative rows pump the reactor for a
//! bounded budget and assert nothing arrived in any round. The oracle legs run the same sequence
//! against the system runtime's `tcp_inet` driver and `gen_tcp` and **skip loudly** without `erl`.

mod support;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use beam_io::drivers::inet::{
    INET_ACTIVE, INET_LOPT_ACTIVE, INET_LOPT_PACKET, INET_ONCE, INET_PASSIVE, INET_REP_OK,
};
use beam_io::drivers::inet::packet::PacketType;
use beam_io::prelude::*;
use beam_process::PortId;

use support::server::*;
use support::skip;

/// The hang guard a blocking read from the foreign peer sits under (a socket option, never a
/// sleep used as a correctness device).
const PEER_TIMEOUT: Duration = Duration::from_secs(5);

/// `gen_tcp:listen/2`'s option block as the `prim_inet` records it: `[binary, {active,false},
/// {reuseaddr,true}, {nodelay,true}, {packet,0}, {recbuf,N}, {sndbuf,N}, {buffer,N},
/// {keepalive,true}]`. `{backlog,N}` is **not** a `setopts` record in the C — it is the
/// `INET_REQ_LISTEN`(27) operand — so it is the [`ServerRig::listen`] `backlog` argument.
fn listener_options() -> Vec<Vec<u8>> {
    vec![
        record(INET_LOPT_MODE, INET_MODE_BINARY),
        record(INET_LOPT_ACTIVE, INET_PASSIVE),
        record(INET_OPT_REUSEADDR, 1),
        record(TCP_OPT_NODELAY_CODE, 1),
        record(INET_LOPT_PACKET, 0),
        record(INET_OPT_RCVBUF_CODE, 4096),
        record(INET_OPT_SNDBUF_CODE, 4096),
        record(INET_LOPT_BUFFER, 4096),
        record(INET_OPT_KEEPALIVE_CODE, 1),
    ]
}

/// The option codes a `getopts` of that block reads back — the nine codes `inet:getopts/2`
/// asks for, in `#options` order (`reuseaddr`, `keepalive`, `sndbuf`, `recbuf`, `nodelay`,
/// `buffer`, `active`, `packet`, `mode`).
fn getopt_codes() -> Vec<u8> {
    vec![
        INET_OPT_REUSEADDR,
        INET_OPT_KEEPALIVE_CODE,
        INET_OPT_SNDBUF_CODE,
        INET_OPT_RCVBUF_CODE,
        TCP_OPT_NODELAY_CODE,
        INET_LOPT_BUFFER,
        INET_LOPT_ACTIVE,
        INET_LOPT_PACKET,
        INET_LOPT_MODE,
    ]
}

// ---------------------------------------------------------------------------
// 1. The listener option block, oracle-cross-checked
// ---------------------------------------------------------------------------

/// `gen_tcp:listen/2`'s whole option block on a real listener: the `open`(1) → `setopts`(7) →
/// `bind`(6, 127.0.0.1:0) → `listen`(27, backlog) → `name`(5) ladder, with a `getopts`(8) of
/// every code reading back **exactly** what the system runtime's `tcp_inet` reads back.
///
/// The kernel-assigned port and the descriptors are the only ephemeral values; every other byte
/// is compared. Linux doubles a requested `SO_RCVBUF`/`SO_SNDBUF`, which is why `{recbuf,4096}`
/// reads back `8192` on both sides — the oracle leg is what pins it.
#[test]
fn the_listener_option_block_reads_back_what_the_oracle_reads_back() {
    let _serial = serial();
    let mut rig = ServerRig::new();
    let listener = rig.open_tcp();
    let opts = listener_options();
    let codes = getopt_codes();
    let backlog: u16 = 5;

    // The whole ladder, one control call at a time.
    assert_eq!(
        rig.op(listener, INET_REQ_OPEN, &open_arg()),
        ctl_ok(),
        "INET_REQ_OPEN(1)"
    );
    assert_eq!(
        rig.op(listener, INET_REQ_SETOPTS, &records(&opts)),
        ctl_ok(),
        "INET_REQ_SETOPTS(7) accepts the whole block"
    );
    let bound = rig.op(listener, INET_REQ_BIND, &loopback_arg(0));
    assert_eq!(bound.len(), 3, "`bind` replies `[1, port_hi, port_lo]`, got {bound:?}");
    assert_eq!(bound[0], INET_REP_OK, "INET_REQ_BIND(6), got {bound:?}");
    let port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(port, 0, "the kernel assigned an ephemeral port");
    assert_eq!(
        rig.op(listener, INET_REQ_LISTEN, &backlog.to_be_bytes()),
        ctl_ok(),
        "INET_REQ_LISTEN(27, {backlog})"
    );
    assert_eq!(
        rig.name(listener),
        vec![INET_REP_OK, INET_AF_INET, bound[1], bound[2], 127, 0, 0, 1],
        "INET_REQ_NAME(5) reports the endpoint `bind` just allocated"
    );
    assert_eq!(
        rig.getstatus(listener),
        LISTENING_STATUS,
        "a bare listener reports INET_STATE_LISTENING"
    );
    let ours = rig.getopts(listener, &codes);
    assert_eq!(
        ours.len(),
        1 + 5 * codes.len(),
        "one `[code, value_be32]` record per requested code, got {ours:?}"
    );

    // The oracle: the same sequence on a real `tcp_inet` port.
    let sequence = vec![
        (INET_REQ_OPEN, open_arg().to_vec()),
        (INET_REQ_SETOPTS, records(&opts)),
        (INET_REQ_BIND, loopback_arg(0)),
        (INET_REQ_LISTEN, backlog.to_be_bytes().to_vec()),
        (INET_REQ_GETOPTS, codes.clone()),
    ];
    let Some(theirs) = erl_reply_dump("tcp_inet", &sequence) else {
        return;
    };
    assert_eq!(
        theirs.len(),
        sequence.len(),
        "one oracle reply per request: {theirs:?}"
    );
    assert_eq!(theirs[0], ctl_ok(), "the oracle's `open` row");
    assert_eq!(theirs[1], ctl_ok(), "the oracle's `setopts` row");
    assert_eq!(
        theirs[2].len(),
        3,
        "the oracle's `bind` row is `[1, port_hi, port_lo]`: {theirs:?}"
    );
    assert_eq!(theirs[2][0], INET_REP_OK, "the oracle's `bind` tag: {theirs:?}");
    assert_ne!(
        u16::from_be_bytes([theirs[2][1], theirs[2][2]]),
        0,
        "the oracle got an ephemeral port too"
    );
    assert_eq!(theirs[3], ctl_ok(), "the oracle's `listen` row");
    assert_eq!(
        ours, theirs[4],
        "the option block reads back byte-for-byte what the oracle's `tcp_inet` reads back \
         (codes {codes:?})"
    );

    rig.close_port(listener);
}

// ---------------------------------------------------------------------------
// 2. The accept loop and the adopted descriptor
// ---------------------------------------------------------------------------

/// Three sequential connections through one listener: each `accept`(26) answers a fresh id, each
/// completion hands over its **own** descriptor, the adopted port reports the connected state, the
/// client's address and its descriptor — and the listener goes back to a bare
/// `INET_STATE_LISTENING` every time, so it keeps accepting (no burned ids, never `ealready`).
#[test]
fn three_sequential_connections_are_accepted_and_adopted() {
    let _serial = serial();
    let mut rig = ServerRig::new();
    let listener = rig.open_tcp();
    let bound = rig.listen(listener, &listener_options(), 5);
    assert_eq!(rig.getstatus(listener), LISTENING_STATUS);

    for index in 0..3u16 {
        // A queued accept is visible in the state bits, and it never answers `ealready`.
        let id = rig.queue_accept(listener);
        assert_eq!(id, index, "accept #{index} answers its own id");
        // One queued accept is `INET_F_ACC`; the sticky `INET_F_MULTI_CLIENT` bit behind
        // `MULTI_ACCEPT_STATUS` needs **two** accepts queued at once (`tests/inet_accept.rs`'s
        // `9` → `41` → `297` row), which a one-at-a-time accept loop never does.
        assert_eq!(
            rig.getstatus(listener),
            ONE_ACCEPT_STATUS,
            "the queued accept is reported in the state bits (connection #{index})"
        );

        let peer = rig.client(bound);
        let drain = rig.wait_for_completions(listener, "the queued accept completes");
        let (done_id, fd) = drain
            .accepted()
            .unwrap_or_else(|| panic!("the accept hands a descriptor over: {drain:?}"));
        assert_eq!(done_id, id, "the completion echoes the id its reply handed out");
        assert!(fd >= 0, "the handed-over descriptor is real: {fd}");

        let connection = rig.adopt(listener, fd);
        assert_eq!(
            rig.ports.driver_as::<TcpInetDriver>(connection).and_then(TcpInetDriver::socket),
            Some(fd),
            "the adopted driver owns the descriptor the accept handed over"
        );
        assert_eq!(
            rig.getstatus(connection),
            CONNECTED_STATUS,
            "the adopted descriptor is a connected stream socket"
        );
        assert_eq!(
            rig.getfd(connection),
            fd,
            "GETFD(14) reports the descriptor the port owns"
        );
        let peer_port = peer.local_addr().expect("the client is bound").port();
        let [hi, lo] = peer_port.to_be_bytes();
        assert_eq!(
            rig.peer(connection),
            vec![INET_REP_OK, INET_AF_INET, hi, lo, 127, 0, 0, 1],
            "PEER(4) reports the client's endpoint"
        );
        assert_eq!(
            rig.getstatus(listener),
            LISTENING_STATUS,
            "the drained accept list reports INET_STATE_LISTENING again, so the listener still \
             accepts (connection #{index})"
        );

        // The connection really carries that peer's bytes. The socket is `{active,false}`, so
        // the read is the passive one `prim_inet:recv/3` issues — a queued `TCP_REQ_RECV`(42) —
        // never a mere readiness event (a passive socket is not read for readiness alone).
        peer_write(&peer, b"ping");
        let recv_id = rig.queue_recv(connection);
        let (drain, read) = rig.wait_for_data(connection, "the connection reads its own peer's bytes");
        assert_eq!(
            drain.completion_for(connection).map(|done| done.event.clone()),
            Some(CompletionEvent::InetAsync {
                id: recv_id,
                result: InetAsyncResult::Data(read.clone()),
            }),
            "the completion echoes the id the receive reply handed out"
        );
        assert_eq!(read.flattened(), b"ping");

        rig.close_port(connection);
        drop(peer);
        assert_eq!(
            rig.getstatus(listener),
            LISTENING_STATUS,
            "closing a connection leaves the listener accepting"
        );
    }

    rig.close_port(listener);
}

// ---------------------------------------------------------------------------
// 3. No descriptor leak across the accept loop
// ---------------------------------------------------------------------------

/// The three-connection loop above opens exactly two descriptors per connection (the accepted
/// socket and the client's) on top of the listener's one, and gives every one of them back: the
/// count returns to the pre-listener baseline. This is what catches an `Accepted { fd }` dropped
/// on the floor — an unowned descriptor is a descriptor nobody closes.
#[test]
fn the_accept_loop_leaks_no_descriptors() {
    let _serial = serial();
    let mut rig = ServerRig::new();
    let Some(baseline) = open_fd_count() else {
        skip("the descriptor-count check skipped: /proc/self/fd is unavailable");
        return;
    };
    let listener = rig.open_tcp();
    let bound = rig.listen(listener, &[], 5);
    assert_eq!(
        open_fd_count(),
        Some(baseline + 1),
        "the listener owns exactly one descriptor"
    );

    let mut held = Vec::new();
    for index in 0..3 {
        let (connection, peer, fd, _) = rig.accept_connection(listener, bound);
        assert!(
            fd >= 0,
            "connection #{index} handed over a real descriptor"
        );
        assert_eq!(
            open_fd_count(),
            Some(baseline + 1 + 2 * (index as usize + 1)),
            "connection #{index} adds exactly the accepted descriptor and the client's, on top \
             of the listener's"
        );
        held.push((connection, peer));
    }

    rig.close_port(listener);
    for (connection, peer) in held {
        rig.close_port(connection);
        drop(peer);
    }
    assert_eq!(
        open_fd_count(),
        Some(baseline),
        "every descriptor the accept loop opened is given back"
    );
}

// ---------------------------------------------------------------------------
// 4. Served request/response on a passive socket
// ---------------------------------------------------------------------------

/// The served request/response on a **passive** accepted socket: a `TCP_REQ_RECV`(42) answers the
/// request as an [`InetAsyncResult::Data`] completion, the answer leaves through the caller-tagged
/// `port_command` route, its `{inet_reply,…}` returns the tag **verbatim**, and the client reads
/// exactly the bytes back.
#[test]
fn a_passive_request_is_served_and_answered_through_the_caller_tag() {
    let _serial = serial();
    let (mut rig, _, connection, mut peer) = one_connection(&listener_options());
    let request = b"GET / HTTP/1.1\r\n\r\n";
    peer_write(&peer, request);

    // `{active,false}`: the read is the one `prim_inet:recv/3` issues, and no readiness event
    // may read a passive socket on its own.
    let recv_id = rig.queue_recv(connection);
    let (drain, read) = rig.wait_for_data(connection, "the passive request is read");
    assert_eq!(read.flattened(), request, "the request arrives verbatim");
    assert_eq!(
        read.header,
        Vec::<u8>::new(),
        "a raw-mode read carries no header prefix"
    );
    assert_eq!(
        read.body.as_deref(),
        Some(&request[..]),
        "the whole request is the body of one read"
    );
    assert_eq!(
        drain.completion_for(connection).map(|done| done.event.clone()),
        Some(CompletionEvent::InetAsync {
            id: recv_id,
            result: InetAsyncResult::Data(read.clone()),
        }),
        "the completion echoes the id the receive reply handed out"
    );

    // The answer.
    let answer = b"HTTP/1.1 200 OK\r\n\r\nhi";
    rig.send_command(connection, answer);
    let drain = rig.wait_until("the answer's {inet_reply,...} arrives", |drain| {
        drain.reply_for(connection).is_some()
    });
    assert_eq!(
        drain.reply_for(connection),
        Some(&InetReply::Ok),
        "the command answers ok"
    );
    let tags: Vec<Vec<u8>> = drain
        .completions
        .iter()
        .filter_map(|done| match &done.event {
            CompletionEvent::InetReply { caller_ref, .. } => caller_ref.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(
        tags,
        vec![GOLDEN_TAG.to_vec()],
        "the `prim_inet` caller tag crosses the command path verbatim"
    );
    assert_eq!(
        peer_read_exact(&mut peer, answer.len()),
        answer,
        "the client reads the exact bytes back"
    );

    rig.close_port(connection);
}

// ---------------------------------------------------------------------------
// 5. `{packet, 4}` framing in both directions
// ---------------------------------------------------------------------------

/// `{packet, 4}` on the accepted socket, framed **both** ways: the client's
/// `<<Len:32, Request/binary>>` is delivered as the stripped payload, and the reply is written with
/// a header announcing the payload length. The raw wire bytes the client reads are compared with
/// the oracle's.
#[test]
fn packet_4_framing_holds_in_both_directions() {
    let _serial = serial();
    let (mut rig, _, connection, mut peer) = one_connection(&listener_options());
    rig.setopts(
        connection,
        &[record(INET_LOPT_PACKET, i32::from(PacketType::Len4.code()))],
    );

    let request = b"GET / HTTP/1.1\r\n\r\n";
    let mut framed_request = (request.len() as u32).to_be_bytes().to_vec();
    framed_request.extend_from_slice(request);
    peer_write(&peer, &framed_request);
    rig.queue_recv(connection);
    let (_, read) = rig.wait_for_data(connection, "the framed request is read");
    assert_eq!(
        read.flattened(),
        request,
        "the four-byte length header is stripped and only the payload is delivered"
    );
    assert_eq!(
        read.header,
        Vec::<u8>::new(),
        "a framed read carries no header prefix"
    );

    let answer = b"HTTP/1.1 200 OK\r\n\r\nhi";
    rig.send_command(connection, answer);
    rig.wait_until("the framed answer's {inet_reply,...} arrives", |drain| {
        drain.reply_for(connection).is_some()
    });
    let mut expected = (answer.len() as u32).to_be_bytes().to_vec();
    expected.extend_from_slice(answer);
    assert_eq!(
        peer_read_exact(&mut peer, expected.len()),
        expected,
        "the reply's header announces the payload length on the wire"
    );

    // The oracle: the same two frames through `gen_tcp` and a raw client socket.
    // The oracle's `erl_eval` route chokes on a parenthesized size next to an inline string
    // segment, so the frame is built in steps — the bytes are what matter.
    let expr = concat!(
        "{ok, L} = gen_tcp:listen(0, [binary, {active,false}, {ip,{127,0,0,1}}]), ",
        "{ok, Port} = inet:port(L), ",
        "{ok, C} = gen_tcp:connect({127,0,0,1}, Port, [binary, {active,false}]), ",
        "{ok, S} = gen_tcp:accept(L), ",
        "ok = inet:setopts(S, [{packet, 4}]), ",
        "R = list_to_binary(\"GET / HTTP/1.1\\r\\n\\r\\n\"), ",
        "H = <<(byte_size(R)):32>>, ",
        "F = <<H/binary, R/binary>>, ",
        "ok = gen_tcp:send(C, F), ",
        "{ok, Got} = gen_tcp:recv(S, 0), ",
        "A = list_to_binary(\"HTTP/1.1 200 OK\\r\\n\\r\\nhi\"), ",
        "ok = gen_tcp:send(S, A), ",
        "{ok, Wire} = gen_tcp:recv(C, 0), ",
        "S1 = fun(B) -> lists:flatten(lists:join(\",\", ",
        "[integer_to_list(X) || X <- binary_to_list(B)])) end, ",
        "io:format(\"got=~s~n\", [S1(Got)]), ",
        "io:format(\"wire=~s~n\", [S1(Wire)])"
    );
    let Some(stdout) = run_erl(expr) else {
        return;
    };
    assert_eq!(
        field(&stdout, "got").map(bytes_of),
        Some(request.to_vec()),
        "the oracle's framed read is the same stripped payload, so the framing agrees: {stdout}"
    );
    assert_eq!(
        field(&stdout, "wire").map(bytes_of),
        Some(expected.clone()),
        "the oracle's framed reply is the same wire image: {stdout}"
    );

    rig.close_port(connection);
}

// ---------------------------------------------------------------------------
// 6. `{active, once}` — the ranch/cowboy re-arm loop
// ---------------------------------------------------------------------------

/// `{active,once}` on the accepted socket: exactly **one** [`ActiveInputEvent::Data`] per grant,
/// the mode silently back to `{active,false}`, a second write delivering nothing until the owner
/// re-arms through `setopts`(7) — the loop a ranch acceptor runs for every request.
#[test]
fn active_once_delivers_exactly_one_message_and_needs_a_re_arm() {
    let _serial = serial();
    let (mut rig, _, connection, peer) = one_connection(&listener_options());
    rig.setopts(connection, &[record(INET_LOPT_ACTIVE, INET_ONCE)]);
    assert_eq!(
        rig.ports.active_mode(connection).expect("the port is live"),
        ActiveMode::Once,
        "the granted mode is `{{active,once}}`"
    );

    peer_write(&peer, b"first");
    let drain = rig.wait_for_deliveries(1, "the granted delivery arrives");
    assert_eq!(drain.bodies(), vec![b"first".to_vec()], "exactly one delivery");
    assert_eq!(
        drain.passive(),
        0,
        "a spent `{{active,once}}` is spent silently, with no `{{tcp_passive,_}}`"
    );
    assert_eq!(
        rig.ports.active_mode(connection).expect("the port is live"),
        ActiveMode::False,
        "the grant reverted to `{{active,false}}`"
    );

    let quiet_before = rig.assert_quiet(QUIET_BUDGET, "no second delivery on the spent grant");
    peer_write(&peer, b"second");
    let quiet_after = rig.assert_quiet(QUIET_BUDGET, "an un-re-armed socket delivers nothing");
    let drained = quiet_before.rounds + quiet_after.rounds;
    assert!(drained > 0, "the quiet windows really pumped the reactor");

    rig.setopts(connection, &[record(INET_LOPT_ACTIVE, INET_ONCE)]);
    let rearmed = rig.wait_for_deliveries(1, "the re-armed delivery arrives");
    assert_eq!(
        rearmed.bodies(),
        vec![b"second".to_vec()],
        "the re-arm delivers the bytes that waited in the kernel"
    );

    // The oracle rows: the same four observations through `gen_tcp`.
    let expr = concat!(
        "{ok, L} = gen_tcp:listen(0, [binary, {active,false}, {ip,{127,0,0,1}}]), ",
        "{ok, Port} = inet:port(L), ",
        "{ok, C} = gen_tcp:connect({127,0,0,1}, Port, [binary, {active,false}]), ",
        "{ok, S} = gen_tcp:accept(L), ",
        "ok = inet:setopts(S, [{active, once}]), ",
        "Take = fun(T) -> receive {tcp, S, D} -> {ok, D} after T -> none end end, ",
        "ok = gen_tcp:send(C, <<\"first\">>), ",
        "A = Take(2000), B = Take(300), ",
        "ok = gen_tcp:send(C, <<\"second\">>), ",
        "D = Take(300), ",
        "ok = inet:setopts(S, [{active, once}]), ",
        "E = Take(2000), ",
        "S1 = fun(none) -> \"none\"; ({ok, B2}) -> lists:flatten(lists:join(\",\", ",
        "[integer_to_list(X) || X <- binary_to_list(B2)])) end, ",
        "io:format(\"first=~s~n\", [S1(A)]), ",
        "io:format(\"quiet=~s~n\", [S1(B)]), ",
        "io:format(\"second=~s~n\", [S1(D)]), ",
        "io:format(\"rearmed=~s~n\", [S1(E)])"
    );
    let Some(stdout) = run_erl(expr) else {
        return;
    };
    assert_eq!(field(&stdout, "first"), Some("102,105,114,115,116"), "the oracle's first delivery: {stdout}");
    assert_eq!(field(&stdout, "quiet"), Some("none"), "the oracle is quiet on a spent grant: {stdout}");
    assert_eq!(field(&stdout, "second"), Some("none"), "the oracle is quiet while un-armed: {stdout}");
    assert_eq!(
        field(&stdout, "rearmed"),
        Some("115,101,99,111,110,100"),
        "the oracle's re-armed delivery: {stdout}"
    );

    rig.close_port(connection);
}

// ---------------------------------------------------------------------------
// 7. `{active, N}` — the credit and its `Passive` report
// ---------------------------------------------------------------------------

/// `{active, 2}` on the accepted socket: two writes deliver two messages, the second exhausts the
/// grant and reports one [`ActiveInputEvent::Passive`] (the `{{tcp_passive,S}}` a ranch listener
/// re-arms on), and a third write produces nothing.
#[test]
fn active_n_delivers_its_credit_then_reports_passive() {
    let _serial = serial();
    let (mut rig, _, connection, peer) = one_connection(&listener_options());
    rig.setopts(connection, &[active_multi(2)]);
    assert_eq!(
        rig.ports.active_mode(connection).expect("the port is live"),
        ActiveMode::Multi { remaining: 2 },
        "the granted credit is `{{active,2}}`"
    );

    peer_write(&peer, b"one");
    let first = rig.wait_for_deliveries(1, "the first granted delivery arrives");
    assert_eq!(first.bodies(), vec![b"one".to_vec()]);
    assert_eq!(
        rig.ports.active_mode(connection).expect("the port is live"),
        ActiveMode::Multi { remaining: 1 },
        "one grant is spent"
    );

    peer_write(&peer, b"two");
    let second = rig.wait_until("the credit is spent and reported", |drain| {
        drain.passive() >= 1
    });
    assert_eq!(
        second.bodies(),
        vec![b"two".to_vec()],
        "the second write is the second delivery (the first was drained by the earlier wait)"
    );
    assert_eq!(
        second.passive(),
        1,
        "the exhausted credit is reported exactly once as `{{tcp_passive,_}}`: {second:?}"
    );
    assert_eq!(
        rig.ports.active_mode(connection).expect("the port is live"),
        ActiveMode::False,
        "the spent grant leaves the socket passive"
    );

    peer_write(&peer, b"three");
    rig.assert_quiet(QUIET_BUDGET, "a passive socket delivers nothing more");

    rig.close_port(connection);
}

// ---------------------------------------------------------------------------
// 8. `{active, true}` and the peer's close
// ---------------------------------------------------------------------------

/// `{active,true}` and a peer close: the bytes the peer wrote before it closed are still delivered
/// (a complete buffered unit wins over the pending close), the driver latches the close
/// ([`TcpInetDriver::peer_closed`], mirrored by `Driver::port_closed`), and nothing is delivered
/// after it — the ordering that makes `{{tcp,S,Data}}` precede `{{tcp_closed,S}}`.
#[test]
fn active_true_delivers_the_staged_bytes_before_the_peer_close() {
    let _serial = serial();
    let (mut rig, _, connection, peer) = one_connection(&listener_options());
    rig.setopts(connection, &[record(INET_LOPT_ACTIVE, INET_ACTIVE)]);
    assert!(
        !rig.ports.port_closed(connection).expect("the port is live"),
        "a fresh connection has no peer close"
    );

    peer_write(&peer, b"bye");
    let drain = rig.wait_for_deliveries(1, "the last bytes are delivered");
    assert_eq!(drain.bodies(), vec![b"bye".to_vec()]);

    peer.shutdown(std::net::Shutdown::Both).expect("the peer half-closes");
    drop(peer);
    let _ = rig.wait_for_peer_close(connection, "the peer's close is latched");
    assert!(
        rig.ports.port_closed(connection).expect("the port is live"),
        "the driver mirrors the latched peer close through `Driver::port_closed`"
    );
    assert!(
        rig.ports
            .driver_as::<TcpInetDriver>(connection)
            .is_some_and(TcpInetDriver::eof),
        "the read half saw end-of-file"
    );
    rig.assert_quiet(QUIET_BUDGET, "nothing is delivered after the peer's close");

    rig.close_port(connection);
}

// ---------------------------------------------------------------------------
// 9. `shutdown(write)` followed by a close
// ---------------------------------------------------------------------------

/// `gen_tcp:shutdown(S, write)` on the accepted socket: `TCP_REQ_SHUTDOWN`(44) with `TCP_SHUT_WR`
/// answers `[1]`, the client sees end-of-file, the **read** half still works (the client's later
/// bytes arrive), and the subsequent `INET_REQ_CLOSE`(2) leaves the C's socket-less state — where
/// `PEER`(4), `NAME`(5) and `GETFD`(14) answer its refusals.
///
/// The refusal bytes are the measured oracle rows: `peer` → `[0]enotconn`, `name` → `[0]ebadf`,
/// `getfd` → `[0]einval`, `getstatus` → `[1,0,0,0,0]`.
#[test]
fn shutdown_write_then_close_leaves_the_oracles_refusals() {
    let _serial = serial();
    let (mut rig, _, connection, mut peer) = one_connection(&listener_options());

    assert_eq!(
        rig.shutdown_write(connection),
        ctl_ok(),
        "`gen_tcp:shutdown(S, write)` answers ok"
    );
    assert_eq!(
        rig.getstatus(connection),
        CONNECTED_STATUS,
        "a write-side shutdown does not close the socket"
    );

    // The client writes before it notices the FIN, so the read half really has data to deliver.
    peer_write(&peer, b"after");
    rig.queue_recv_len(connection, 4);
    let (_, read) = rig.wait_for_data(connection, "the read half still works after the shutdown");
    assert_eq!(
        read.flattened(),
        b"afte",
        "the read half delivers the client's bytes after the write half closed"
    );

    assert!(peer_read_to_eof(&mut peer).is_empty(), "the client sees end-of-file, no bytes");
    rig.queue_recv_len(connection, 0);
    let (_, rest) = rig.wait_for_data(connection, "the tail of the client's write is delivered");
    assert_eq!(rest.flattened(), b"r", "the unread tail of the same write arrives");

    assert_eq!(rig.close_socket(connection), ctl_ok(), "INET_REQ_CLOSE(2) answers ok");
    assert_eq!(
        rig.peer(connection),
        ctl_xerror("enotconn"),
        "PEER(4) on the closed socket answers the C's `enotconn`"
    );
    assert_eq!(
        rig.name(connection),
        ctl_xerror("ebadf"),
        "NAME(5) on the closed socket answers the C's `ebadf`"
    );
    assert_eq!(
        rig.op(connection, INET_REQ_GETFD, b""),
        ctl_xerror("einval"),
        "GETFD(14) on the closed socket answers the C's `einval`"
    );
    assert_eq!(
        rig.getstatus(connection),
        0,
        "the closed socket reports the C's zeroed state bits"
    );

    // The oracle rows for the same ladder.
    let expr = concat!(
        "{ok, L} = gen_tcp:listen(0, [binary, {active,false}, {ip,{127,0,0,1}}]), ",
        "{ok, Port} = inet:port(L), ",
        "{ok, C} = gen_tcp:connect({127,0,0,1}, Port, [binary, {active,false}]), ",
        "{ok, S} = gen_tcp:accept(L), ",
        "Shut = gen_tcp:shutdown(S, write), ",
        "Send = gen_tcp:send(C, <<\"after\">>), ",
        "R1 = gen_tcp:recv(S, 4), ",
        "Eof = gen_tcp:recv(C, 0), ",
        "R2 = gen_tcp:recv(S, 0), ",
        "Close = gen_tcp:close(S), ",
        "S1 = fun({ok, B}) -> lists:flatten(lists:join(\",\", ",
        "[integer_to_list(X) || X <- binary_to_list(B)])); ",
        "(Other) -> atom_to_list(element(1, Other)) end, ",
        "io:format(\"shut=~p~n\", [Shut]), ",
        "io:format(\"send=~p~n\", [Send]), ",
        "io:format(\"read1=~s~n\", [S1(R1)]), ",
        "io:format(\"eof=~p~n\", [element(1, Eof)]), ",
        "io:format(\"read2=~s~n\", [S1(R2)]), ",
        "io:format(\"close=~p~n\", [Close])"
    );
    let Some(stdout) = run_erl(expr) else {
        return;
    };
    assert_eq!(field(&stdout, "shut"), Some("ok"), "the oracle's shutdown row: {stdout}");
    assert_eq!(
        field(&stdout, "send"),
        Some("ok"),
        "the oracle's client write after the write-side shutdown still succeeds: {stdout}"
    );
    assert_eq!(field(&stdout, "read1"), Some("97,102,116,101"), "the oracle's first read: {stdout}");
    assert_eq!(
        field(&stdout, "eof"),
        Some("error"),
        "the oracle's client sees the peer's end-of-file: {stdout}"
    );
    assert_eq!(field(&stdout, "read2"), Some("114"), "the oracle's tail read: {stdout}");
    assert_eq!(field(&stdout, "close"), Some("ok"), "the oracle's close row: {stdout}");

    rig.close_port(connection);
}

// ---------------------------------------------------------------------------
// 10. `{packet, line}`
// ---------------------------------------------------------------------------

/// `{packet,line}` on the accepted socket: one frame per delimiter, the delimiter **kept** in the
/// body (the C's `TCP_PB_LINE_LF`). The oracle rows are the two lines of one write, the delimiter
/// included, and then a wait that times out because no second frame is complete.
#[test]
fn packet_line_frames_one_line_per_delimiter_and_keeps_it() {
    let _serial = serial();
    let (mut rig, _, connection, peer) = one_connection(&listener_options());
    rig.setopts(
        connection,
        &[record(INET_LOPT_PACKET, i32::from(PacketType::LineLf.code()))],
    );

    peer_write(&peer, b"alpha\nbeta\n");
    rig.queue_recv(connection);
    let (_, first) = rig.wait_for_data(connection, "the first line frame is read");
    assert_eq!(
        first.flattened(),
        b"alpha\n",
        "one frame per delimiter, the delimiter kept"
    );

    rig.queue_recv(connection);
    let drain = rig.wait_until("the second line frame is read", |drain| {
        !drain.data_for(connection).is_empty()
    });
    let second = drain
        .data_for(connection)
        .first()
        .map(|read| (*read).clone())
        .expect("the second line frame");
    assert_eq!(
        second.flattened(),
        b"beta\n",
        "the trailing line is its own frame, delimiter kept"
    );

    // The oracle: the same two frames, and a third read that finds no complete line.
    let expr = concat!(
        "{ok, L} = gen_tcp:listen(0, [binary, {active,false}, {ip,{127,0,0,1}}]), ",
        "{ok, Port} = inet:port(L), ",
        "{ok, C} = gen_tcp:connect({127,0,0,1}, Port, [binary, {active,false}]), ",
        "{ok, S} = gen_tcp:accept(L), ",
        "ok = inet:setopts(S, [{packet, line}]), ",
        "ok = gen_tcp:send(C, <<\"alpha\\nbeta\\n\">>), ",
        "S1 = fun({ok, B}) -> lists:flatten(lists:join(\",\", ",
        "[integer_to_list(X) || X <- binary_to_list(B)])); ",
        "(Other) -> atom_to_list(element(1, Other)) end, ",
        "io:format(\"line1=~s~n\", [S1(gen_tcp:recv(S, 0))]), ",
        "io:format(\"line2=~s~n\", [S1(gen_tcp:recv(S, 0))]), ",
        "io:format(\"line3=~s~n\", [S1(gen_tcp:recv(S, 0, 300))])"
    );
    let Some(stdout) = run_erl(expr) else {
        return;
    };
    assert_eq!(
        field(&stdout, "line1"),
        Some("97,108,112,104,97,10"),
        "the oracle's first line frame, delimiter kept: {stdout}"
    );
    assert_eq!(
        field(&stdout, "line2"),
        Some("98,101,116,97,10"),
        "the oracle's second line frame: {stdout}"
    );
    assert_eq!(
        field(&stdout, "line3"),
        Some("error"),
        "the oracle finds no third complete line: {stdout}"
    );

    rig.close_port(connection);
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A rig with a loopback listener and **one accepted + adopted** connection.
fn one_connection(opts: &[Vec<u8>]) -> (ServerRig, PortId, PortId, TcpStream) {
    let mut rig = ServerRig::new();
    let listener = rig.open_tcp();
    let bound = rig.listen(listener, opts, 5);
    let (connection, peer, _, _) = rig.accept_connection(listener, bound);
    (rig, listener, connection, peer)
}

/// Writes `bytes` from the foreign peer (a short write is retried, never a fixed sleep).
fn peer_write(peer: &TcpStream, bytes: &[u8]) {
    let mut peer = peer;
    peer.write_all(bytes).expect("the peer writes");
    peer.flush().expect("the peer flushes");
}

/// Reads exactly `len` bytes from the foreign peer, under a read timeout as the hang guard.
fn peer_read_exact(peer: &mut TcpStream, len: usize) -> Vec<u8> {
    peer.set_read_timeout(Some(PEER_TIMEOUT))
        .expect("a read timeout is a socket option");
    let mut buf = vec![0u8; len];
    peer.read_exact(&mut buf)
        .unwrap_or_else(|err| panic!("the peer reads {len} byte(s): {err}"));
    buf
}

/// Reads from the foreign peer until end-of-file and answers everything it saw before it (empty
/// when the peer's write half was shut down).
fn peer_read_to_eof(peer: &mut TcpStream) -> Vec<u8> {
    peer.set_read_timeout(Some(PEER_TIMEOUT))
        .expect("a read timeout is a socket option");
    let mut got = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        match peer.read(&mut buf) {
            Ok(0) => return got,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(err) => panic!("the peer reads to end-of-file: {err}"),
        }
    }
}

/// The suite's process-wide serialization lock.
///
/// [`the_accept_loop_leaks_no_descriptors`] counts `/proc/self/fd`, which is **process-wide**: a
/// connection another case opens in parallel would move the count out from under it. Every case in
/// this file therefore holds this lock for its lifetime, so the descriptor arithmetic is exact
/// rather than merely likely, and a case that panicked poisons nothing.
static SERIAL: Mutex<()> = Mutex::new(());

/// Acquires [`SERIAL`], tolerating a poisoned lock (a panicking case must not cascade).
fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The bytes of one comma-separated oracle dump field.
fn bytes_of(field: &str) -> Vec<u8> {
    parse_dump(field)
        .unwrap_or_else(|| panic!("a comma-separated byte dump: {field:?}"))
        .into_iter()
        .next()
        .unwrap_or_default()
}
