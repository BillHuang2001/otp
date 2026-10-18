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
//! The `udp_inet` **asynchronous-operation** suite: `PACKET_REQ_RECV`(60) and the
//! `{inet_async, Port, Id, Result}` completions it produces, plus the send path's
//! `{inet_reply, Port, Reply, Tag}`.
//!
//! Every case drives the runtime's own `erlang:port_control/3`/`port_command/2` path through the
//! [`Rig`] the numeric-opcode suite defines and drains completions with
//! [`PortRegistry::take_completions`](crate::PortRegistry::take_completions) — the seam `beam_rt`
//! reads. What is pinned here:
//!
//! * the C's guard ladder of `packet_inet_ctl`'s `PACKET_REQ_RECV` arm, in the C's order;
//! * the **separation of the two halves**: the control reply is the id and nothing else, the
//!   datagram arrives only through a later round's completion;
//! * the datagram's shape — the packed source address as the read's *header*, the payload as its
//!   body, so `[list]` mode's flat list and `[binary]` mode's improper `[Address | <<Payload>>]`
//!   are both derivable from one `ReadMessage`;
//! * the deadline (`{error, timeout}` **after** the reply) and the teardown
//!   (`{error, closed}`, exactly once, before the descriptor goes away);
//! * the send path's caller tag echoed verbatim, and the zero-length tag staging nothing;
//! * every one of those rows **cross-checked against the real system `udp_inet`** through an
//!   `erl` probe ([`erl_stdout`]), byte-compared, skipping **loudly** when `erl` is absent.
use std::net::UdpSocket;
use std::time::Duration;

use super::super::opcode_tests::{
    assert_agrees, erl_stdout, op, oracle_expression, oracle_replies, Call, Rule, Rig,
};
use super::super::wire::{
    ctl_ok, ctl_xerror, pack_address, INET_AF_INET, INET_AF_LOOPBACK, INET_PROTO_DEFAULT,
    INET_TYPE_DGRAM,
};
use super::UdpInetDriver;
use crate::completion::{CompletionEvent, InetAsyncResult, InetReason, InetReply};
use crate::port_task::ReadMessage;

/// `INET_REQ_OPEN` (`inet_drv.c:14577`).
const INET_REQ_OPEN: u32 = 1;
/// `INET_REQ_BIND` (`:11409`).
const INET_REQ_BIND: u32 = 6;
/// `INET_REQ_SETOPTS` (`:11111`).
const INET_REQ_SETOPTS: u32 = 7;
/// `PACKET_REQ_RECV` (`:14968`) — the datagram driver's asynchronous receive.
const PACKET_REQ_RECV: u32 = 60;
/// The `{active, true}` option code (`INET_LOPT_ACTIVE`).
const INET_LOPT_ACTIVE: u8 = 22;
/// The C's `TCP_MAX_PACKET_SIZE` plus one — a `Length` the **stream** arm refuses with
/// `[0]enomem` and the **datagram** arm ignores outright (`:14992`).
const LENGTH_TOO_LARGE: u32 = 0x0400_0001;
/// A bare `udp_inet` port (no socket yet).
fn bare_port() -> Rig {
    Rig::new(Box::new(UdpInetDriver::new()))
}

/// An opened `udp_inet` port: the `open` verb has created the datagram socket.
fn opened_port() -> Rig {
    let mut port = bare_port();
    assert_eq!(
        port.control(&op(INET_REQ_OPEN), &[
            INET_AF_INET,
            INET_TYPE_DGRAM,
            INET_PROTO_DEFAULT
        ]),
        ctl_ok(),
        "the datagram socket opens"
    );
    port
}

/// An opened port bound to an ephemeral loopback port, plus the port number the kernel chose.
fn bound_port() -> (Rig, u16) {
    let mut port = opened_port();
    let reply = port.control(&op(INET_REQ_BIND), &[INET_AF_LOOPBACK, 0, 0]);
    assert_eq!(reply.first(), Some(&1), "the bind succeeds: {reply:?}");
    let bound = u16::from_be_bytes([reply[1], reply[2]]);
    assert_ne!(bound, 0, "the kernel assigned a port");
    (port, bound)
}

/// The `Timeout(4), Length(4)` operand of `PACKET_REQ_RECV` (`inet_drv.c:14990`).
fn recv_operand(timeout_ms: i32, length: u32) -> Vec<u8> {
    let mut arg = timeout_ms.to_be_bytes().to_vec();
    arg.extend_from_slice(&length.to_be_bytes());
    arg
}

/// The async completions of one drain, in delivery order.
fn async_events(events: &[crate::completion::PortCompletion]) -> Vec<(u16, InetAsyncResult)> {
    events
        .iter()
        .filter_map(|event| match &event.event {
            CompletionEvent::InetAsync { id, result } => Some((*id, result.clone())),
            CompletionEvent::InetReply { .. } => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The guard ladder
// ---------------------------------------------------------------------------

#[test]
fn the_packet_recv_ladder_is_the_cs_and_in_the_cs_order() {
    // 1. no socket ⇒ `!IS_OPEN` ⇒ `[0]exbadport` (`:14980`), whatever the operand.
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(PACKET_REQ_RECV), &recv_operand(0, 0)),
        ctl_xerror("exbadport")
    );
    assert_eq!(bare.control(&op(PACKET_REQ_RECV), &[]), ctl_xerror("exbadport"));

    let mut port = opened_port();

    // 2. the operand must be exactly the eight-byte record (`:14988`).
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &[0, 0]),
        ctl_xerror("einval")
    );
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &[0; 4]),
        ctl_xerror("einval")
    );

    // 2b. an active-mode socket has no passive receive (`desc->active`, `:14988`).
    let mut active = vec![INET_LOPT_ACTIVE];
    active.extend_from_slice(&1i32.to_be_bytes());
    assert_eq!(active.len(), 5);
    assert_eq!(
        port.control(&op(INET_REQ_SETOPTS), &active),
        ctl_ok(),
        "{INET_LOPT_ACTIVE} is accepted"
    );
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        ctl_xerror("einval")
    );
    // Back to passive so the rest of the ladder is reachable.
    let mut passive = vec![INET_LOPT_ACTIVE];
    passive.extend_from_slice(&0i32.to_be_bytes());
    assert_eq!(port.control(&op(INET_REQ_SETOPTS), &passive), ctl_ok());

    // 3. a well-formed operand is accepted and answers the async id.
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    // 4. the single receive slot (`INET_MAX_ASYNC`, `:1069`) is taken ⇒ `[0]ealready`.
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        ctl_xerror("ealready")
    );
}

/// The operand's `Length` word is **ignored** by the datagram arm: the C says so at `:14992`
/// ("The 2nd arg, Length(4), is ignored for both UDP and SCTP protocols, since they are
/// msg-oriented"), which is why an over-long request is accepted rather than refused with the
/// `[0]enomem` the *stream* arm answers for the same operand (`:12301`). Measured on the oracle:
/// `[1,0,0]`, then `[0]ealready` — the operation really was queued.
#[test]
fn an_over_long_length_is_accepted_because_the_datagram_arm_ignores_it() {
    let mut port = opened_port();
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, LENGTH_TOO_LARGE)),
        vec![1, 0, 0],
        "the datagram arm never reads Length, so nothing refuses it"
    );
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        ctl_xerror("ealready"),
        "the accepted operation took the slot — it was queued, not refused"
    );
}

/// A refused enqueue still consumes an id (`NEW_ASYNC_ID` is taken before `enq_async`,
/// `:1549`): measured on the oracle as `[1,0,0]`, `[0]ealready`, then `[1,0,2]` once the first
/// operation has completed — and the second completion carries id `2`, never the discarded `1`.
#[test]
fn a_refused_enqueue_still_consumes_an_async_id() {
    let (mut port, bound) = bound_port();
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    // Refused, but `NEW_ASYNC_ID` was already taken (`:1549`).
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        ctl_xerror("ealready")
    );

    // Complete the first operation so the slot frees up again.
    let peer = UdpSocket::bind("127.0.0.1:0").expect("a peer socket");
    peer.send_to(b"first", ("127.0.0.1", bound)).expect("a datagram");
    port.round().expect("the round runs");
    let drained = async_events(&port.completions());
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].0, 0, "the first operation was id 0");

    // The refused id (1) is gone: the next accepted one is 2, not 1.
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 2],
        "the refused enqueue burned id 1"
    );
    peer.send_to(b"second", ("127.0.0.1", bound))
        .expect("a second datagram");
    port.round().expect("the round runs");
    let drained = async_events(&port.completions());
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].0, 2, "and the second completion is id 2");

    let mut fresh = opened_port();
    assert_eq!(
        fresh.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0],
        "a fresh port starts its id counter at 0 again"
    );
}// ---------------------------------------------------------------------------
// The reply/completion split and the datagram's shape
// ---------------------------------------------------------------------------

#[test]
fn the_reply_carries_no_data_and_the_datagram_completes_on_a_later_round() {
    let (mut port, bound) = bound_port();
    let peer = UdpSocket::bind("127.0.0.1:0").expect("a peer socket");

    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0],
        "the control reply is the id and nothing else"
    );
    assert!(port.completions().is_empty(), "nothing is completed inline");
    assert_eq!(port.take_input(64), Vec::<u8>::new(), "no bytes were staged");

    peer.send_to(b"dg2", ("127.0.0.1", bound)).expect("a datagram");

    // One round's async drive point completes the operation.
    port.round().expect("the round runs");
    let drained = async_events(&port.completions());
    assert_eq!(drained.len(), 1, "exactly one completion");
    let (id, result) = drained.into_iter().next().expect("one event");
    assert_eq!(id, 0);

    let source = peer.local_addr().expect("the peer address");
    let std::net::IpAddr::V4(address) = source.ip() else {
        panic!("an IPv4 peer");
    };
    let expected = ReadMessage {
        header: pack_address(source.port(), address.octets()),
        body: Some(b"dg2".to_vec()),
    };
    match result {
        InetAsyncResult::Data(read) => {
            assert_eq!(read.header, expected.header, "the packed source address");
            assert_eq!(read.body, expected.body, "the datagram payload");
            // `[list]` mode: one flat list with the address first (the C's `INET_MODE_LIST`
            // arm, `inet_async_binary_data` `:3900`).
            assert_eq!(read.flattened(), expected.flattened());
            // The header/body split survives: a `[binary]` port's improper tail is exactly the
            // datagram payload, so the read is **not** the body-only shape.
            assert!(!read.header.is_empty(), "the packed source address is the header");
            assert_eq!(read.body.as_deref(), Some(&b"dg2"[..]));
            assert!(read.has_body(), "`[binary]` mode keeps the improper tail");
            assert_eq!(read.len(), 7 + 3);
        }
        other => panic!("a data completion, not {other:?}"),
    }

    // Exactly once: a second round reports nothing.
    port.round().expect("the round runs");
    assert!(port.completions().is_empty());
}

#[test]
fn a_queued_receive_with_no_datagram_stays_pending_across_rounds() {
    let (mut port, _bound) = bound_port();
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    for _ in 0..3 {
        port.round().expect("the round runs");
        assert!(port.completions().is_empty(), "nothing arrived, nothing reported");
    }
}

// ---------------------------------------------------------------------------
// The deadline and the teardown
// ---------------------------------------------------------------------------

#[test]
fn a_zero_timeout_reports_the_timeout_after_the_reply() {
    let (mut port, _bound) = bound_port();
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(0, 0)),
        vec![1, 0, 0],
        "the reply comes first, the timeout later"
    );
    assert!(port.completions().is_empty());
    assert!(port.next_timeout().is_some(), "the deadline is armed");

    port.round().expect("the round runs");
    let drained = async_events(&port.completions());
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].0, 0);
    assert_eq!(drained[0].1, InetAsyncResult::Error(InetReason::Timeout));

    // Once only, and the queue is empty afterwards.
    port.round().expect("the round runs");
    assert!(port.completions().is_empty());
}

#[test]
fn a_close_with_an_operation_pending_answers_it_closed_exactly_once() {
    let (mut port, _bound) = bound_port();
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    port.close();
    let drained = async_events(&port.completions());
    assert_eq!(drained.len(), 1, "{drained:?}");
    assert_eq!(drained[0].0, 0);
    assert_eq!(drained[0].1, InetAsyncResult::Error(InetReason::Closed));
    assert!(port.completions().is_empty(), "never twice");
}

#[test]
fn a_still_pending_operation_keeps_the_port_warm_and_an_empty_queue_does_not() {
    let (mut port, _bound) = bound_port();
    assert!(port.next_timeout().is_none(), "nothing queued yet");
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    // `has_pending_async` is what makes the registry keep driving the port; an infinite timeout
    // declares no deadline, so the only liveness is readiness.
    assert!(port.next_timeout().is_none(), "INET_INFINITY arms no timer");
}

// ---------------------------------------------------------------------------
// The send path's `{inet_reply, …}`
// ---------------------------------------------------------------------------

/// The golden `term_to_binary(Mref, [local])` image the objective pins, echoed verbatim.
const GOLDEN_TAG: [u8; 26] = [
    131, 121, 254, 153, 216, 200, 90, 0, 3, 106, 0, 0, 0, 0, 0, 0, 11, 218, 194, 188, 0, 11, 244,
    41, 135, 150,
];

#[test]
fn the_send_path_echoes_the_caller_tag_verbatim() {
    let (mut port, _bound) = bound_port();
    let peer = UdpSocket::bind("127.0.0.1:0").expect("a peer socket");
    let target = peer.local_addr().expect("the peer address");
    let std::net::IpAddr::V4(address) = target.ip() else {
        panic!("an IPv4 peer");
    };

    // `[tag_len:be16][tag][address][anc_len:be32][data]` — the `prim_inet:do_sendto/4` shape.
    let mut command = (GOLDEN_TAG.len() as u16).to_be_bytes().to_vec();
    command.extend_from_slice(&GOLDEN_TAG);
    command.extend_from_slice(&pack_address(target.port(), address.octets()));
    command.extend_from_slice(&0u32.to_be_bytes());
    command.extend_from_slice(b"dg3");

    port.command(&command).expect("the command is written");

    let events = port.completions();
    let replies: Vec<&CompletionEvent> = events
        .iter()
        .map(|event| &event.event)
        .filter(|event| matches!(event, CompletionEvent::InetReply { .. }))
        .collect();
    assert_eq!(replies.len(), 1, "one reply for the tagged command: {events:?}");
    match replies[0] {
        CompletionEvent::InetReply { caller_ref, reply } => {
            assert_eq!(
                caller_ref.as_deref(),
                Some(&GOLDEN_TAG[..]),
                "the tag is echoed byte for byte, never decoded"
            );
            assert_eq!(*reply, InetReply::Ok);
        }
        other => panic!("an inet reply, not {other:?}"),
    }

    // The datagram itself reached the peer with the prefix and the address stripped.
    let mut buf = [0u8; 64];
    peer.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let (len, _from) = peer.recv_from(&mut buf).expect("the datagram arrives");
    assert_eq!(&buf[..len], b"dg3");
}

#[test]
fn a_command_without_a_caller_tag_stages_no_reply() {
    let (mut port, _bound) = bound_port();
    let peer = UdpSocket::bind("127.0.0.1:0").expect("a peer socket");
    let target = peer.local_addr().expect("the peer address");
    let std::net::IpAddr::V4(address) = target.ip() else {
        panic!("an IPv4 peer");
    };

    // A **plain** command: no `[n:be16][tag]` prefix at all.
    let mut command = pack_address(target.port(), address.octets());
    command.extend_from_slice(&0u32.to_be_bytes());
    command.extend_from_slice(b"plain");
    port.command(&command).expect("the command is written");

    let events = port.completions();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.event, CompletionEvent::InetReply { .. })),
        "the C's `inet_reply_finish` sends nothing without a caller: {events:?}"
    );
    assert!(port.contains(), "the port stays alive");

    let mut buf = [0u8; 64];
    peer.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let (len, _from) = peer.recv_from(&mut buf).expect("the datagram arrives");
    assert_eq!(&buf[..len], b"plain");
}

#[test]
fn a_socket_less_command_still_keeps_its_bytes_queued() {
    let mut port = bare_port();
    let error = port
        .command(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        .expect_err("a socket-less port refuses the command");
    assert!(
        matches!(error, beam_common::BeamError::BadArg),
        "the honest refusal, not a fabricated reply: {error:?}"
    );
    assert!(port.contains(), "the port stays alive");
}

/// A **zero-length** caller tag (`[0,0]`) is a well-formed prefix `init_caller` consumes
/// (`inet_drv.c:1854`-`:1881`): the two length bytes come off the queue and the peer receives the
/// payload alone — and, because `inet_reply_finish` has no tag to append, **no** `{inet_reply,…}`
/// is ever staged for it.
///
/// Measured on the oracle: `port_command(P, <<0:16, Address/binary, 0:32, "raw">>)` delivers
/// exactly `<<"raw">>` and the mailbox stays empty. Before the prefix was recognised the payload
/// went out as `<<0,0,"raw">>` — a wrong datagram, worse than a typed gap.
#[test]
fn a_zero_length_caller_tag_is_consumed_and_stages_no_reply() {
    let (mut port, _bound) = bound_port();
    let peer = UdpSocket::bind("127.0.0.1:0").expect("a peer socket");
    let target = peer.local_addr().expect("the peer address");
    let std::net::IpAddr::V4(address) = target.ip() else {
        panic!("an IPv4 peer");
    };

    let mut command = 0u16.to_be_bytes().to_vec();
    command.extend_from_slice(&pack_address(target.port(), address.octets()));
    command.extend_from_slice(&0u32.to_be_bytes());
    command.extend_from_slice(b"raw");
    let queued = command.len();
    port.command(&command).expect("the command is written");

    let events = port.completions();
    assert!(
        events
            .iter()
            .all(|event| !matches!(event.event, CompletionEvent::InetReply { .. })),
        "an empty tag has nothing to echo, so nothing is staged: {events:?}"
    );
    assert_eq!(
        port.take_input(64),
        Vec::<u8>::new(),
        "the whole command was consumed from the output queue, and the queue-only prefix \
         accounted for exactly its own bytes"
    );
    assert_eq!(queued, 2 + 7 + 4 + 3, "the shape of the command");

    let mut buf = [0u8; 64];
    peer.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let (len, _from) = peer.recv_from(&mut buf).expect("the datagram arrives");
    assert_eq!(&buf[..len], b"raw", "the zero-length prefix is not payload");
}

// ---------------------------------------------------------------------------
// The `erl` oracle cross-checks
// ---------------------------------------------------------------------------

/// The guard ladder of `PACKET_REQ_RECV`, row for row against the real `udp_inet` driver.
///
/// The call list is the in-crate [`the_packet_recv_ladder_is_the_cs_and_in_the_cs_order`] sequence
/// (socket-less, then open, then the operand / active-mode / slot guards, then the `Length`
/// word the datagram arm ignores), driven on both sides: `oracle_expression` opens a bare
/// `{spawn_driver,"udp_inet"}` port and this crate's [`Rig`] opens the bare driver.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_packet_recv_ladder_agrees_with_the_erl_oracle() {
    let active = {
        let mut arg = vec![INET_LOPT_ACTIVE];
        arg.extend_from_slice(&1i32.to_be_bytes());
        arg
    };
    let passive = {
        let mut arg = vec![INET_LOPT_ACTIVE];
        arg.extend_from_slice(&0i32.to_be_bytes());
        arg
    };
    let calls = [
        Call::req(PACKET_REQ_RECV, vec![0u8; 8], Rule::Exact),
        Call::req(PACKET_REQ_RECV, vec![], Rule::Exact),
        Call::req(
            INET_REQ_OPEN,
            vec![INET_AF_INET, INET_TYPE_DGRAM, INET_PROTO_DEFAULT],
            Rule::Exact,
        ),
        Call::req(PACKET_REQ_RECV, vec![0u8; 2], Rule::Exact),
        Call::req(INET_REQ_SETOPTS, active, Rule::Exact),
        Call::req(PACKET_REQ_RECV, recv_operand(-1, 0), Rule::Exact),
        Call::req(INET_REQ_SETOPTS, passive, Rule::Exact),
        Call::req(PACKET_REQ_RECV, recv_operand(-1, LENGTH_TOO_LARGE), Rule::Exact),
        Call::req(PACKET_REQ_RECV, recv_operand(-1, 0), Rule::Exact),
    ];

    let Some(dump) = erl_stdout(&oracle_expression("udp_inet", &calls)) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet PACKET_REQ_RECV ladder");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), calls.len(), "the oracle answered every call: {dump:?}");

    let mut port = bare_port();
    let rust = port.run(&calls);
    for (index, call) in calls.iter().enumerate() {
        let label = format!(
            "ladder row #{index} (opcode {}, operand {:?})",
            call.value, call.arg
        );
        assert_agrees(&label, call.rule, &oracle[index], &rust[index]);
    }
}

/// A **completed** passive receive: the datagram arrives as an `{inet_async, Port, Id, {ok, D}}`
/// whose `D` is the packed source address followed by the payload, and whose id is the one the
/// control reply handed out.
///
/// The oracle's `D` is printed flattened (`binary_to_list(iolist_to_binary(D))`), which is exactly
/// this floor's [`ReadMessage::flattened`] — the two agree byte for byte outside the ephemeral
/// source port, which the oracle reports itself so it can be checked against its own address.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn a_completed_receive_agrees_with_the_erl_oracle() {
    let expression = format!(
        "M = 16#03f1a300, \
         J = fun(R) -> io:format(\"~w|\", [R]) end, \
         P = open_port({{spawn_driver, \"udp_inet\"}}, [binary]), \
         J(erlang:port_control(P, M+1, <<1,2,0>>)), \
         [1,Hi,Lo] = erlang:port_control(P, M+6, <<4,0,0>>), \
         J(erlang:port_control(P, M+60, <<(-1):32,0:32>>)), \
         {{ok,S}} = gen_udp:open(0, [binary]), \
         {{ok,{{_,SPort}}}} = inet:sockname(S), \
         ok = gen_udp:send(S, {{127,0,0,1}}, Hi*256+Lo, <<\"dg\">>), \
         receive {{inet_async,P,Id,{{ok,Data}}}} -> \
             J([Id]), \
             J(binary_to_list(iolist_to_binary(Data))), \
             J([SPort bsr 8, SPort band 255]) \
         after 5000 -> J([254,254]) end, \
         erlang:port_close(P)"
    );
    let Some(dump) = erl_stdout(&expression) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet completed-receive cross-check");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), 5, "reply, reply, id, data, source port: {dump:?}");
    assert_eq!(oracle[0], vec![1], "the open");
    assert_eq!(oracle[1], vec![1, 0, 0], "the id reply of the queued receive");
    assert_eq!(oracle[2], vec![0], "the first id");
    assert_eq!(oracle[4].len(), 2, "the oracle's ephemeral source port");
    let oracle_source = [oracle[4][0], oracle[4][1]];
    assert_eq!(
        oracle[3],
        [vec![1], oracle_source.to_vec(), vec![127, 0, 0, 1], b"dg".to_vec()].concat(),
        "the oracle's data is [family, src_port, 127.0.0.1 | payload]"
    );

    // The same shape on this floor.
    let (mut port, bound) = bound_port();
    let peer = UdpSocket::bind("127.0.0.1:0").expect("a peer socket");
    assert_eq!(
        port.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    peer.send_to(b"dg", ("127.0.0.1", bound)).expect("a datagram");
    port.round().expect("the round runs");
    let drained = async_events(&port.completions());
    assert_eq!(drained.len(), 1);
    let (id, result) = drained.into_iter().next().expect("one event");
    assert_eq!(vec![id as u8], oracle[2], "the id matches the oracle's");

    let source = peer.local_addr().expect("the peer address");
    let std::net::IpAddr::V4(address) = source.ip() else {
        panic!("an IPv4 peer");
    };
    let [hi, lo] = source.port().to_be_bytes();
    let InetAsyncResult::Data(read) = result else {
        panic!("a data completion, not {result:?}");
    };
    assert_eq!(read.header, pack_address(source.port(), address.octets()));
    assert_eq!(read.body.as_deref(), Some(&b"dg"[..]));
    let flat = read.flattened();
    assert_eq!(
        flat,
        [vec![INET_AF_INET, hi, lo], vec![127, 0, 0, 1], b"dg".to_vec()].concat(),
        "the flattened read is the packed source address followed by the payload"
    );
    assert_eq!(
        flat[3..],
        oracle[3][3..],
        "the address octets and the payload agree with the oracle byte for byte"
    );
    assert_eq!(flat.len(), oracle[3].len());
}

/// The deadline and the teardown: `timeout = 0` answers `{error, timeout}` **after** the reply,
/// and a close with an operation pending answers `{error, closed}` exactly once.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_timeout_and_the_teardown_agree_with_the_erl_oracle() {
    let expression = format!(
        "M = 16#03f1a300, \
         J = fun(R) -> io:format(\"~w|\", [R]) end, \
         R = fun({{error,Atom}}) -> binary_to_list(atom_to_binary(Atom, utf8)) end, \
         P = open_port({{spawn_driver, \"udp_inet\"}}, [binary]), \
         J(erlang:port_control(P, M+1, <<1,2,0>>)), \
         J(erlang:port_control(P, M+6, <<4,0,0>>)), \
         J(erlang:port_control(P, M+60, <<0:32,0:32>>)), \
         receive {{inet_async,P,A,RA}} -> J([A]), J(R(RA)) \
         after 5000 -> J([254,254]) end, \
         Q = open_port({{spawn_driver, \"udp_inet\"}}, [binary]), \
         J(erlang:port_control(Q, M+1, <<1,2,0>>)), \
         J(erlang:port_control(Q, M+6, <<4,0,0>>)), \
         J(erlang:port_control(Q, M+60, <<(-1):32,0:32>>)), \
         erlang:port_close(Q), \
         receive {{inet_async,Q,B,RB}} -> J([B]), J(R(RB)) \
         after 5000 -> J([254,254]) end, \
         receive {{inet_async,Q,_,_}} -> J([1]) after 300 -> J([0]) end, \
         erlang:port_close(P)"
    );
    let Some(dump) = erl_stdout(&expression) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet deadline/teardown cross-check");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), 11, "four replies plus three observed messages: {dump:?}");
    assert_eq!(oracle[2], vec![1, 0, 0], "the timeout receive replies first");
    assert_eq!(oracle[3], vec![0], "with id 0");
    assert_eq!(oracle[4], b"timeout".to_vec(), "and the reason arrives after it");
    assert_eq!(oracle[7], vec![1, 0, 0], "the closed receive replies first");
    assert_eq!(oracle[8], vec![0], "with its own id 0");
    assert_eq!(oracle[9], b"closed".to_vec(), "answered error:closed");
    assert_eq!(oracle[10], vec![0], "exactly once, never twice");

    // This floor: the same two operations.
    let (mut timed, _bound) = bound_port();
    assert_eq!(
        timed.control(&op(PACKET_REQ_RECV), &recv_operand(0, 0)),
        vec![1, 0, 0]
    );
    assert!(timed.next_timeout().is_some(), "the deadline is armed");
    timed.round().expect("the round runs");
    let drained = async_events(&timed.completions());
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].1, InetAsyncResult::Error(InetReason::Timeout));

    let (mut closed, _bound) = bound_port();
    assert_eq!(
        closed.control(&op(PACKET_REQ_RECV), &recv_operand(-1, 0)),
        vec![1, 0, 0]
    );
    closed.close();
    let drained = async_events(&closed.completions());
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].1, InetAsyncResult::Error(InetReason::Closed));
    assert!(closed.completions().is_empty(), "never twice");
}

/// The send path: a real caller tag is echoed verbatim as `{inet_reply, Port, ok, Tag}`, and a
/// **zero-length** tag is consumed silently — the peer receives the payload alone and no reply is
/// ever staged.
///
/// The oracle builds its tag with `term_to_binary(make_ref(), [local])` and checks it comes back
/// as the *same* reference; this floor never decodes the image, so its half compares the echoed
/// bytes to the bytes it sent. Both sides are additionally compared on the payload the peer reads,
/// which is the observable the zero-length rule changes.
///
/// **Skips loudly** when `erl` is not on `PATH`.
#[test]
fn the_send_path_replies_agree_with_the_erl_oracle() {
    let expression = format!(
        "M = 16#03f1a300, \
         J = fun(R) -> io:format(\"~w|\", [R]) end, \
         P = open_port({{spawn_driver, \"udp_inet\"}}, [binary]), \
         J(erlang:port_control(P, M+1, <<1,2,0>>)), \
         J(erlang:port_control(P, M+6, <<4,0,0>>)), \
         {{ok,S}} = gen_udp:open(0, [binary]), \
         {{ok,{{_,SPort}}}} = inet:sockname(S), \
         Ref = make_ref(), \
         Tag = term_to_binary(Ref, [local]), \
         Addr = <<1, (SPort bsr 8):8, (SPort band 255):8, 127,0,0,1>>, \
         true = erlang:port_command(P, <<(byte_size(Tag)):16, Tag/binary, Addr/binary, 0:32, \"hey\">>), \
         receive {{inet_reply,P,Reply,T2}} -> \
             J(binary_to_list(atom_to_binary(Reply, utf8))), \
             J([case T2 =:= Ref of true -> 1; false -> 0 end]) \
         after 5000 -> J([254,254]) end, \
         receive {{udp,S,_,_,D}} -> J(binary_to_list(D)) after 5000 -> J([254,254]) end, \
         true = erlang:port_command(P, <<0:16, Addr/binary, 0:32, \"raw\">>), \
         receive {{udp,S,_,_,Z}} -> J(binary_to_list(Z)) after 5000 -> J([254,254]) end, \
         receive {{inet_reply,P,_,_}} -> J([1]) after 300 -> J([0]) end, \
         erlang:port_close(P)"
    );
    let Some(dump) = erl_stdout(&expression) else {
        eprintln!("SKIP: no `erl` on PATH — skipping the udp_inet send-path cross-check");
        return;
    };
    let oracle = oracle_replies(&dump);
    assert_eq!(oracle.len(), 7, "two replies plus five observations: {dump:?}");
    assert_eq!(oracle[2], b"ok".to_vec(), "the tagged command answers ok");
    assert_eq!(oracle[3], vec![1], "and echoes the caller's own reference");
    assert_eq!(oracle[4], b"hey".to_vec(), "the peer reads the payload alone");
    assert_eq!(oracle[5], b"raw".to_vec(), "a zero-length tag is consumed, not sent");
    assert_eq!(oracle[6], vec![0], "and stages no reply at all");

    // This floor: the same two commands.
    let (mut port, _bound) = bound_port();
    let peer = UdpSocket::bind("127.0.0.1:0").expect("a peer socket");
    let target = peer.local_addr().expect("the peer address");
    let std::net::IpAddr::V4(address) = target.ip() else {
        panic!("an IPv4 peer");
    };
    let mut command = (GOLDEN_TAG.len() as u16).to_be_bytes().to_vec();
    command.extend_from_slice(&GOLDEN_TAG);
    command.extend_from_slice(&pack_address(target.port(), address.octets()));
    command.extend_from_slice(&0u32.to_be_bytes());
    command.extend_from_slice(b"hey");
    port.command(&command).expect("the command is written");
    let replies: Vec<(Option<Vec<u8>>, InetReply)> = port
        .completions()
        .into_iter()
        .filter_map(|event| match event.event {
            CompletionEvent::InetReply { caller_ref, reply } => Some((caller_ref, reply)),
            CompletionEvent::InetAsync { .. } => None,
        })
        .collect();
    assert_eq!(replies.len(), 1, "one reply for the tagged command");
    assert_eq!(replies[0].0.as_deref(), Some(&GOLDEN_TAG[..]));
    assert_eq!(replies[0].1, InetReply::Ok);

    let mut buf = [0u8; 64];
    peer.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let (len, _from) = peer.recv_from(&mut buf).expect("the datagram arrives");
    assert_eq!(&buf[..len], oracle[4].as_slice(), "the same payload the oracle's peer read");

    let mut zero = 0u16.to_be_bytes().to_vec();
    zero.extend_from_slice(&pack_address(target.port(), address.octets()));
    zero.extend_from_slice(&0u32.to_be_bytes());
    zero.extend_from_slice(b"raw");
    port.command(&zero).expect("the command is written");
    assert!(
        port.completions().is_empty(),
        "a zero-length tag stages no reply"
    );
    let (len, _from) = peer.recv_from(&mut buf).expect("the second datagram arrives");
    assert_eq!(&buf[..len], oracle[5].as_slice(), "and the peer reads the payload alone");
}
