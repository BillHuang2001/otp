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

//! Oracle-cross-checked `INET_REQ_GETSTAT` cover for the **datagram** driver and the
//! accepted-socket copy.
//!
//! A sibling `#[path]` test module of `stat.rs`. Two facts are pinned here: `udp_inet` counts
//! payload octets only (no framing header exists), and `tcp_inet_copy` deliberately does **not**
//! inherit the listener's counters (`inet_drv.c:11795`-`:11858`), so an accepted port starts at
//! an all-zero block and counts its own traffic.
//!
//! # Why `udp_inet` has no header to add
//!
//! `{packet, N}` framing is a **stream** facility: `tcp_send`/`tcp_sendv` prepend a 1/2/4-byte
//! length word and then charge the payload **plus** that header
//! (`inet_output_count(desc, len + h_len)`, `inet_drv.c:13763`/`:13879`). A datagram is a message,
//! not a stream, so `packet_inet_command` writes it verbatim — `inet_output_count(desc, len)` with
//! the **payload** length only (`:15163`, the empty-ancillary-data arm; `:15220` is the
//! `sendmsg`-with-cmsgs one this floor refuses) — and `{packet, 0}`'s `hlen` of zero is the only
//! code a datagram port has. This floor reproduces that in one shared place,
//! [`UdpInetDriver`]'s private `send_datagram`, which both write routes reach: the named `send`
//! verb and the queued `erlang:port_command/2` one.
//!
//! # Why the accepted socket starts at zero
//!
//! `tcp_inet_copy` (`inet_drv.c:11795`-`:11858`) copies a fixed **option** field set off the
//! listener's descriptor — `inet.mode`, `inet.exitf`, `inet.deliver`, `inet.htype`, `inet.psize`,
//! `inet.hsz`, `inet.bufsz`, the watermarks, `send_timeout*`, the `tcp_add_flags` subset — and
//! nothing else. `desc->stat` is not on that list, so the accepted descriptor keeps the all-zero
//! block `inet_init_desc` gave it and counts only what it moves itself; the listener never counts
//! the accepted connection's traffic at all. This floor's
//! [`TcpInetDriver::from_accepted`](super::super::TcpInetDriver::from_accepted) inherits the same
//! option set through `SocketOptions::inherited_from` and likewise leaves `InetStat` alone.
//!
//! # The counters
//!
//! `INET_REQ_GETSTAT`(11) takes one byte per `INET_STAT_*` op and answers `[1]` followed by one
//! record per requested op **in request order** — five bytes for ops 1-8, nine for the two octet
//! counters 9/10 (`inet_fill_stat`, `inet_drv.c:10732`-`:10792`). Every row below requests all ten
//! in the C's own order, so one row is one whole block:
//! `[recv_cnt, recv_max, recv_avg, recv_dvi, send_cnt, send_max, send_avg, send_pend, recv_oct,
//! send_oct]`. The arithmetic behind each value is [`super::InetStat`]'s, pinned there; this suite
//! pins **which** call site feeds it and **with what length**.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::os::unix::io::RawFd;
use std::thread;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_process::PortId;

use super::super::opcode_tests::{escript_stdout, loopback_arg, op, open_arg, Rig};
use super::super::opcode_tests_udp::open_arg as udp_open_arg;
use super::super::wire::ctl_ok;
use super::super::{make_tcp_inet, TcpInetDriver, UdpInetDriver, INET_REP_OK};
use crate::completion::{CompletionEvent, InetAsyncResult, InetReply};

/// `INET_REQ_GETSTAT` (`inet_drv.c:816`) — the shared `inet_ctl` arm both drivers answer.
const GETSTAT: u32 = 11;
/// `TCP_REQ_ACCEPT` (`inet_drv.c:106`): `[Timeout(4)]`, zero meaning `INET_INFINITY`.
const ACCEPT: u32 = 26;
/// `TCP_REQ_LISTEN` (`inet_drv.c:104`): `[backlog_hi, backlog_lo]`.
const LISTEN: u32 = 27;

/// The `INET_STAT_*` op codes, in the order this suite requests them (`inet_drv.c:961`-`:970`).
const RECV_CNT: u8 = 1;
const RECV_MAX: u8 = 2;
const RECV_AVG: u8 = 3;
const RECV_DVI: u8 = 4;
const SEND_CNT: u8 = 5;
const SEND_MAX: u8 = 6;
const SEND_AVG: u8 = 7;
const SEND_PEND: u8 = 8;
const RECV_OCT: u8 = 9;
const SEND_OCT: u8 = 10;

/// The whole counter block in the C's own op order — one `getstat` call, one row.
const ALL_CODES: &[u8] = &[
    RECV_CNT, RECV_MAX, RECV_AVG, RECV_DVI, SEND_CNT, SEND_MAX, SEND_AVG, SEND_PEND, RECV_OCT,
    SEND_OCT,
];

/// The accept's `Timeout(4)` operand in milliseconds: 5000, a hang guard and never a latency
/// budget (the reactor round that completes it is condition-driven).
const ACCEPT_TIMEOUT_MS: [u8; 4] = [0, 0, 0x13, 0x88];

/// Wall-clock hang guards. Both wait for a **condition** and give up only when the condition is
/// plainly never going to hold.
const ACCEPT_WAIT: Duration = Duration::from_secs(10);
const READ_WAIT: Duration = Duration::from_secs(10);

/// The `prim_inet` caller-reference tag as it looks on the wire (`term_to_binary(Mref, [local])`,
/// which always starts with the ETF `VERSION_MAGIC` 131; see
/// [`super::super::wire::split_command_prefix`]).
const CALLER_REF_TAG: &[u8] = &[131, 100, 0, 8, b'r', b'e', b'f', b'0'];

/// The expected `udp_inet` send-counter rows, in the order the probe drives them.
///
/// Each row is `[recv_cnt, recv_max, recv_avg, recv_dvi, send_cnt, send_max, send_avg, send_pend,
/// recv_oct, send_oct]`. Measured on OTP 29.1.1 / erts-17.1 with `gen_udp:send/4` +
/// `inet:getstat/2` (see [`the_datagram_send_counter_rows_agree_with_the_erl_oracle`]):
///
/// ```text
/// fresh        [0,0,0,0, 0,0,0,0,  0,0]
/// after_hello  [0,0,0,0, 1,5,5,0,  0,5]     5-byte datagram
/// after_empty  [0,0,0,0, 2,5,2,0,  0,5]     zero-length datagram
/// after_ten    [0,0,0,0, 3,10,5,0, 0,15]    10-byte datagram
/// after_empty2 [0,0,0,0, 4,10,3,0, 0,15]    zero-length datagram
/// ```
///
/// `send_avg` follows `inet_output_count`'s **floating-point** update with `avg` captured before
/// the update (`:11567`/`:11578`): `5 → 2.5 → 5 → 3.75`, rendered through `inet_fill_stat`'s
/// 32-bit truncation (`:10732`-`:10792`) as `5, 2, 5, 3`. The `recv_*` half never moves: nothing
/// is ever read on this socket, and a datagram send is not an input.
const UDP_EXPECTED: [(&str, [u64; 10]); 5] = [
    ("fresh", [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ("after_hello", [0, 0, 0, 0, 1, 5, 5, 0, 0, 5]),
    ("after_empty", [0, 0, 0, 0, 2, 5, 2, 0, 0, 5]),
    ("after_ten", [0, 0, 0, 0, 3, 10, 5, 0, 0, 15]),
    ("after_empty2", [0, 0, 0, 0, 4, 10, 3, 0, 0, 15]),
];

/// The expected `tcp_inet` counter rows for the listener/accepted pair, in the order the probe
/// drives them — the same ten codes as [`UDP_EXPECTED`].
///
/// Measured on OTP 29.1.1 / erts-17.1 with `gen_tcp:listen/2` + `gen_tcp:accept/1` +
/// `inet:getstat/2` on **both** sockets:
///
/// ```text
/// listener_before_accept [0,0,0,0, 0,0,0,0, 0,0]
/// accepted_fresh         [0,0,0,0, 0,0,0,0, 0,0]   the client already wrote 5 bytes
/// listener_after_accept  [0,0,0,0, 0,0,0,0, 0,0]
/// accepted_after_sends   [0,0,0,0, 2,2,1,0, 0,2]   `"zz"` then a zero-length send
/// accepted_after_recv    [1,5,5,0, 2,2,1,0, 5,2]
/// listener_final         [0,0,0,0, 0,0,0,0, 0,0]
/// ```
///
/// `accepted_fresh` being all-zero is the `tcp_inet_copy` fact: the listener's `desc->stat` is not
/// on the copy list (`:11795`-`:11858`), so the accepted descriptor starts where `inet_init_desc`
/// left it even though the client's 5 bytes are already sitting in the socket's receive buffer —
/// the counters advance on **delivery**, not on arrival. `accepted_after_sends`'s `send_avg` is
/// `2` then `2 + (0 - 2)/2 = 1`, and `accepted_after_recv`'s `recv_avg` of `5` with `recv_dvi`
/// `0` is the first-sample pair. The listener never counts the accepted connection's traffic.
const TCP_EXPECTED: [(&str, [u64; 10]); 6] = [
    ("listener_before_accept", [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ("accepted_fresh", [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ("listener_after_accept", [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
    ("accepted_after_sends", [0, 0, 0, 0, 2, 2, 1, 0, 0, 2]),
    ("accepted_after_recv", [1, 5, 5, 0, 2, 2, 1, 0, 5, 2]),
    ("listener_final", [0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
];

/// The `erl` probe for [`UDP_EXPECTED`] — the same four datagrams over `gen_udp:send/4`, which
/// takes the **queued** `erlang:port_command/2` route (`prim_inet:sendto/4` →
/// `packet_inet_command`), with a `gen_udp:recv/3` drain at the end proving every datagram
/// (the two zero-length ones included) really reached the wire.
const UDP_PROBE: &str = r#"
main(_) ->
    Codes = [recv_cnt, recv_max, recv_avg, recv_dvi,
             send_cnt, send_max, send_avg, send_pend, recv_oct, send_oct],
    {ok, R} = gen_udp:open(0, [binary, {active, false}, {ip, {127, 0, 0, 1}}]),
    {ok, Target} = inet:port(R),
    {ok, S} = gen_udp:open(0, [binary, {active, false}, {ip, {127, 0, 0, 1}}]),
    Row = fun (Tag, Payload) ->
                  case Payload of
                      none -> ok;
                      _ -> ok = gen_udp:send(S, {127, 0, 0, 1}, Target, Payload)
                  end,
                  {ok, L} = inet:getstat(S, Codes),
                  io:format("~s ~s~n",
                            [Tag, string:join([integer_to_list(V) || {_, V} <- L], ",")])
          end,
    Row("fresh", none),
    Row("after_hello", <<"hello">>),
    Row("after_empty", <<>>),
    Row("after_ten", <<"0123456789">>),
    Row("after_empty2", <<>>),
    [begin
         {ok, {_, _, D}} = gen_udp:recv(R, 0, 2000),
         io:format("wire ~w~n", [binary_to_list(D)])
     end || _ <- [1, 2, 3, 4]],
    ok.
"#;

/// The `erl` probe for [`TCP_EXPECTED`] — a listener and an accepted socket, with the client
/// writing its 5 bytes **before** the accept, then a `"zz"`/zero-length send pair and a 5-byte
/// read on the accepted socket.
const TCP_PROBE: &str = r#"
main(_) ->
    Codes = [recv_cnt, recv_max, recv_avg, recv_dvi,
             send_cnt, send_max, send_avg, send_pend, recv_oct, send_oct],
    Row = fun (Tag, Sock) ->
                  {ok, L} = inet:getstat(Sock, Codes),
                  io:format("~s ~s~n",
                            [Tag, string:join([integer_to_list(V) || {_, V} <- L], ",")])
          end,
    {ok, L} = gen_tcp:listen(0, [binary, {active, false}, {reuseaddr, true},
                                 {ip, {127, 0, 0, 1}}]),
    {ok, Port} = inet:port(L),
    Row("listener_before_accept", L),
    {ok, C} = gen_tcp:connect({127, 0, 0, 1}, Port, [binary, {active, false}]),
    ok = gen_tcp:send(C, <<"hello">>),
    {ok, A} = gen_tcp:accept(L, 2000),
    Row("accepted_fresh", A),
    Row("listener_after_accept", L),
    ok = gen_tcp:send(A, <<"zz">>),
    ok = gen_tcp:send(A, <<>>),
    Row("accepted_after_sends", A),
    {ok, Five} = gen_tcp:recv(A, 5, 2000),
    io:format("read ~w~n", [binary_to_list(Five)]),
    Row("accepted_after_recv", A),
    Row("listener_final", L),
    {ok, Two} = gen_tcp:recv(C, 2, 2000),
    io:format("client ~w~n", [binary_to_list(Two)]),
    ok.
"#;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// One `INET_REQ_GETSTAT`(11) call for the whole counter block on `port`, decoded into the ten
/// values in [`ALL_CODES`] order.
///
/// The decoder is the wire shape `inet_fill_stat` builds (`inet_drv.c:10732`-`:10792`): the `[1]`
/// tag, then one `[op, value_be]` record per requested op **in request order**, four bytes wide
/// except for the two octet counters. It is local to this file because the sibling suites keep
/// their own; the shape it asserts is the one the oracle row above already pinned.
fn getstat_on(rig: &mut Rig, port: PortId) -> Vec<u64> {
    let reply = control_on(rig, port, &op(GETSTAT), ALL_CODES);
    assert_eq!(
        reply.first(),
        Some(&INET_REP_OK),
        "`getstat` answers the [1] result tag: {reply:?}"
    );
    let mut values = Vec::with_capacity(ALL_CODES.len());
    let mut at = 1usize;
    for &code in ALL_CODES {
        assert_eq!(
            reply.get(at).copied(),
            Some(code),
            "the requested op byte {code} is echoed back: {reply:?}"
        );
        let width = if matches!(code, RECV_OCT | SEND_OCT) {
            8
        } else {
            4
        };
        let record = reply
            .get(at + 1..at + 1 + width)
            .unwrap_or_else(|| panic!("op {code} carries a {width}-byte record: {reply:?}"));
        let mut word = [0u8; 8];
        word[8 - width..].copy_from_slice(record);
        values.push(u64::from_be_bytes(word));
        at += 1 + width;
    }
    assert_eq!(
        at,
        reply.len(),
        "the reply carries nothing past the last record: {reply:?}"
    );
    values
}

/// The whole counter block of `port` compared against one expected row.
fn expect_row(rig: &mut Rig, port: PortId, expected: (&str, [u64; 10])) {
    let (label, want) = expected;
    assert_eq!(
        getstat_on(rig, port),
        want.to_vec(),
        "the oracle's `{label}` row [recv_cnt, recv_max, recv_avg, recv_dvi, \
         send_cnt, send_max, send_avg, send_pend, recv_oct, send_oct]"
    );
}

/// One control call on **any** registry port — [`Rig::control`] is pinned to `rig.id`, and the
/// accepted connection is a second port of the same registry.
fn control_on(rig: &mut Rig, port: PortId, command: &str, arg: &[u8]) -> Vec<u8> {
    match rig.registry.port_control(&mut rig.poller, port, command, arg) {
        Ok(reply) => reply,
        Err(err) => panic!("the `{command}` control call on {port:?} failed: {err:?}"),
    }
}

/// How many bytes currently sit in `port`'s input queue.
fn input_len_on(rig: &Rig, port: PortId) -> usize {
    rig.registry
        .get(port)
        .expect("a live port")
        .input_len()
}

/// Drains up to `len` bytes out of `port`'s input queue.
fn take_input_on(rig: &mut Rig, port: PortId, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let read = rig
        .registry
        .get_mut(port)
        .expect("a live port")
        .read_input(&mut buf);
    buf.truncate(read);
    buf
}

/// The `{inet_reply, …}` completions staged since the last drain, in delivery order — the C's
/// `inet_reply_finish` (`inet_drv.c:2614`), which `prim_inet` blocks on after a
/// `gen_udp:send/4`.
fn drained_replies(rig: &mut Rig) -> Vec<(Option<Vec<u8>>, InetReply)> {
    rig.completions()
        .into_iter()
        .filter_map(|done| match done.event {
            CompletionEvent::InetReply { caller_ref, reply } => Some((caller_ref, reply)),
            CompletionEvent::InetAsync { .. } => None,
        })
        .collect()
}

/// A real `std::net::UdpSocket` peer on loopback — an independent receiver, so the assertions are
/// on the datagrams that reach the wire rather than on this driver's own queue. The read timeout
/// is a **hang guard**, never a latency budget.
fn udp_peer() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("a loopback datagram peer");
    socket
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("a read timeout");
    socket
}

/// Receives exactly one datagram from `socket` — a **zero-length** datagram included, which is
/// why the length is taken from the return value rather than asserted non-zero.
fn receive_one(socket: &UdpSocket) -> Vec<u8> {
    let mut buf = [0u8; 1024];
    let (len, _from) = socket
        .recv_from(&mut buf)
        .expect("the peer receives a datagram");
    buf[..len].to_vec()
}

/// The `prim_inet` datagram command wire form for the **queued** route:
/// `[tag_len:be16][tag_bytes][address][anc_len:be32][anc_data][data]`
/// (`prim_inet:sendto/4`, `erts/preloaded/src/prim_inet.erl:650`-`:700`).
///
/// The caller-reference prefix is not decoration: `port_command`'s write path runs the C's
/// `init_caller` first (`inet_drv.c:15055`-`:15065`), which reads the first two bytes as a length
/// and fails the port when they do not fit. A plain `[address][payload]` datagram command starts
/// with an address family tag of 1/3/4, so its "length" is `0x0100 + port_hi` and the port dies;
/// a `prim_inet` tag's length word has a zero high byte, so it always fits.
fn prefixed_command(target: u16, payload: &[u8]) -> Vec<u8> {
    let mut command = Vec::new();
    command.extend_from_slice(&(CALLER_REF_TAG.len() as u16).to_be_bytes());
    command.extend_from_slice(CALLER_REF_TAG);
    command.extend_from_slice(&loopback_arg(target));
    // `anc_len == 0`: the empty-ancillary-data arm, `inet_drv.c:15220`.
    command.extend_from_slice(&0u32.to_be_bytes());
    command.extend_from_slice(payload);
    command
}

/// An `open`ed and `bind`ed `udp_inet` registry port, plus the loopback port the kernel gave it.
fn bound_datagram_port() -> (Rig, u16) {
    let mut rig = Rig::new(Box::new(UdpInetDriver::new()));
    assert_eq!(
        rig.control(&op(1), &udp_open_arg()),
        ctl_ok(),
        "`INET_REQ_OPEN` creates the datagram socket"
    );
    let bound = rig.control(&op(6), &loopback_arg(0));
    assert_eq!(
        bound.len(),
        3,
        "`INET_REQ_BIND` answers `[1, port_hi, port_lo]`, got {bound:?}"
    );
    assert_eq!(bound[0], INET_REP_OK, "`bind`'s result tag, got {bound:?}");
    let port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(port, 0, "the kernel assigned an ephemeral port");
    (rig, port)
}

/// A listener port: `open` + `bind` + `listen` + one queued `accept`, all through the registry.
/// Returns the bound loopback port.
fn listening_port(rig: &mut Rig) -> u16 {
    assert_eq!(
        rig.control(&op(1), &open_arg()),
        ctl_ok(),
        "`INET_REQ_OPEN` creates the listening socket"
    );
    let bound = rig.control(&op(6), &loopback_arg(0));
    assert_eq!(
        bound.len(),
        3,
        "`INET_REQ_BIND` answers `[1, port_hi, port_lo]`, got {bound:?}"
    );
    assert_eq!(bound[0], INET_REP_OK, "`bind`'s result tag, got {bound:?}");
    let port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(port, 0, "the kernel assigned an ephemeral port");
    assert_eq!(
        rig.control(&op(LISTEN), &[0, 5]),
        ctl_ok(),
        "`INET_REQ_LISTEN` answers `[1]`"
    );
    assert_eq!(
        rig.control(&op(ACCEPT), &ACCEPT_TIMEOUT_MS),
        vec![INET_REP_OK, 0, 0],
        "the accept queues and answers its id: `[1] ++ <<0:16>>` (`send_async_ok`, \
         `inet_drv.c:2493`)"
    );
    port
}

/// Drives the registry's reactor rounds until the accept queued as `id` completes, returning the
/// descriptor it handed over — a polled condition under [`ACCEPT_WAIT`], never a sleep-then-assert.
///
/// [`Rig::round`] alone would not do: the completion comes from a **readiness** event
/// ([`TcpInetDriver::ready_input`]'s accept ladder), so the loop is the runtime's own
/// `poller.poll` → [`crate::PortRegistry::handle_events`] → `take_completions`.
fn drive_until_accepted(rig: &mut Rig, id: u16) -> RawFd {
    let deadline = Instant::now() + ACCEPT_WAIT;
    loop {
        let events = match rig.poller.poll(Some(Duration::from_millis(100))) {
            Ok(events) => events,
            Err(BeamError::Io(err)) if err.kind() == ErrorKind::Interrupted => Vec::new(),
            Err(err) => panic!("the poll failed: {err:?}"),
        };
        rig.registry
            .handle_events(&mut rig.poller, &events)
            .expect("the ready events are dispatched");
        for done in rig.registry.take_completions() {
            if let CompletionEvent::InetAsync {
                id: got,
                result: InetAsyncResult::Accepted { fd },
            } = done.event
            {
                assert_eq!(got, id, "the completion carries the accept's own id");
                return fd;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the accept never completed within {ACCEPT_WAIT:?}"
        );
    }
}

/// The accepted half of [`listening_port`]: a real client that writes `payload` **before** the
/// accept completes, the registry-driven accept, and
/// [`TcpInetDriver::from_accepted`](super::super::TcpInetDriver::from_accepted) opened as a
/// **second** registry port — the C's `tcp_inet_copy` route the runtime runs.
fn accept_through_the_registry(
    rig: &mut Rig,
    listener: PortId,
    bound: u16,
    payload: &[u8],
) -> (PortId, TcpStream) {
    let mut client = TcpStream::connect(("127.0.0.1", bound)).expect("a real loopback client");
    client
        .write_all(payload)
        .expect("the client's bytes reach the wire before the accept");
    let fd = drive_until_accepted(rig, 0);
    let accepted = {
        let listener_driver = rig
            .registry
            .driver_as::<TcpInetDriver>(listener)
            .expect("the listener port's own tcp_inet driver");
        TcpInetDriver::from_accepted(fd, listener_driver)
            .expect("the accepted descriptor is adopted with the listener's options")
    };
    let accepted = rig
        .registry
        .open(&mut rig.poller, Box::new(accepted))
        .expect("the accepted connection opens as a second port");
    assert_ne!(
        accepted, listener,
        "the accepted connection is a port of its own"
    );
    (accepted, client)
}

/// Drives the `recv` verb on `port` until `len` bytes have reached its input queue.
///
/// A `recv` that finds nothing delivers nothing (and therefore counts nothing), so repeating it is
/// safe: the counter still advances exactly once, when the bytes really arrive. The loop only
/// bounds a genuinely broken driver — it is a condition wait, never a latency budget.
fn recv_until(rig: &mut Rig, port: PortId, len: usize) -> Vec<u8> {
    let deadline = Instant::now() + READ_WAIT;
    loop {
        if input_len_on(rig, port) >= len {
            return take_input_on(rig, port, len);
        }
        let _reply = control_on(rig, port, "recv", &[]);
        if input_len_on(rig, port) >= len {
            return take_input_on(rig, port, len);
        }
        assert!(
            Instant::now() < deadline,
            "`recv` never delivered {len} bytes (input queue: {})",
            input_len_on(rig, port)
        );
        thread::sleep(Duration::from_millis(2));
    }
}

/// The oracle probe's stdout split into its `tag v1,v2,…` counter rows and its
/// `label [b,b,…]` payload lines, in the order the probe printed them.
fn split_oracle_output(stdout: &str) -> (Vec<(String, Vec<u64>)>, Vec<(String, Vec<u8>)>) {
    let mut rows = Vec::new();
    let mut payloads = Vec::new();
    for line in stdout.lines().filter(|line| !line.is_empty()) {
        let (label, rest) = line
            .split_once(' ')
            .unwrap_or_else(|| panic!("the probe's line `{line}` has a label and a value"));
        if rest.starts_with('[') {
            payloads.push((label.to_string(), parse_int_list(rest)));
        } else {
            let values = rest
                .split(',')
                .map(|value| {
                    value
                        .trim()
                        .parse::<u64>()
                        .unwrap_or_else(|err| panic!("`{value}` is not a counter: {err}"))
                })
                .collect();
            rows.push((label.to_string(), values));
        }
    }
    (rows, payloads)
}

/// An Erlang `~w`-printed integer list (`[104,101,108,108,111]`, or `[]`) as bytes.
fn parse_int_list(text: &str) -> Vec<u8> {
    let inner = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| panic!("`{text}` is not a bracketed integer list"));
    if inner.is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|value| {
            value
                .trim()
                .parse::<u8>()
                .unwrap_or_else(|err| panic!("`{value}` is not an octet: {err}"))
        })
        .collect()
}

/// The oracle's counter rows compared against one expected table, labels included — so a probe
/// that stops printing a row, or prints it in another order, cannot pass silently.
fn assert_oracle_rows(
    probe: &str,
    got: &[(String, Vec<u64>)],
    want: &[(&str, [u64; 10])],
) {
    let want: Vec<(String, Vec<u64>)> = want
        .iter()
        .map(|(tag, row)| ((*tag).to_string(), row.to_vec()))
        .collect();
    assert_eq!(got, want.as_slice(), "the oracle probe `{probe}` counter rows");
}

// ---------------------------------------------------------------------------
// Case C — `udp_inet` counts payload octets only
// ---------------------------------------------------------------------------

/// `udp_inet`'s `INET_REQ_GETSTAT` send counters see the **payload** and nothing else, on **both**
/// write routes.
///
/// `packet_inet_command` has no write-side framing to add: a datagram is a message, so the C runs
/// `inet_output_count(desc, len)` at `inet_drv.c:15163` with the payload length alone — there is
/// no `h_len` term, unlike the stream path's `:13763`/`:13879`. The C's `{packet, 0}` is the only
/// code a datagram port can hold, and its header is zero bytes, so "payload + header" and
/// "payload" are the same number here; the distinction this test exists for is that the *prefix*
/// bytes of the queued wire form — the caller tag, the packed address and the ancillary-length
/// word, 17 bytes in all — are **not** counted, even though they are what the output queue
/// consumes.
///
/// Sequence, mixing the two routes so neither can drift from the other, each step asserted
/// against [`UDP_EXPECTED`] and against the datagram a real loopback peer receives:
///
/// | step | route | payload | oracle row |
/// |---|---|---|---|
/// | — | — | — | `[0,0,0,0, 0,0,0,0, 0,0]` |
/// | named `send` verb | `verb_send`, `[address][data]` | `"hello"` | `[0,0,0,0, 1,5,5,0, 0,5]` |
/// | queued `port_command` | `write_pending`, `[tag][address][0:32][data]` | `""` | `[0,0,0,0, 2,5,2,0, 0,5]` |
/// | queued `port_command` | same | 10 bytes | `[0,0,0,0, 3,10,5,0, 0,15]` |
/// | named `send` verb | same | `""` | `[0,0,0,0, 4,10,3,0, 0,15]` |
#[test]
fn the_datagram_driver_counts_payload_octets_only() {
    let peer = udp_peer();
    let target = peer.local_addr().expect("the peer's address").port();
    let (mut rig, _bound) = bound_datagram_port();
    let port = rig.id;

    // Fresh: the block `inet_init_desc` leaves, with nothing counted either way.
    expect_row(&mut rig, port, UDP_EXPECTED[0]);

    // Step 1 — the **named** `send` verb: `[packed address][payload]`, rule 1 of
    // `wire::split_address` (`udp.rs::verb_send`), no ancillary-length word and no caller tag.
    let mut arg = loopback_arg(target);
    arg.extend_from_slice(b"hello");
    assert_eq!(
        control_on(&mut rig, port, "send", &arg),
        ctl_ok(),
        "the named `send` verb answers `[1]`"
    );
    assert_eq!(
        receive_one(&peer),
        b"hello".to_vec(),
        "the 5-byte datagram reached the wire"
    );
    expect_row(&mut rig, port, UDP_EXPECTED[1]);

    // Step 2 — the **queued** route with a zero-length payload: the whole 17-byte command is
    // consumed off the output queue while `send_datagram` counts zero octets.
    let empty = prefixed_command(target, &[]);
    assert_eq!(
        empty.len(),
        2 + CALLER_REF_TAG.len() + 3 + 4,
        "the command is a prefix, a packed address and an ancillary-length word, no payload"
    );
    rig.command(&empty)
        .expect("the queued zero-length datagram is written");
    assert_eq!(
        drained_replies(&mut rig),
        vec![(Some(CALLER_REF_TAG.to_vec()), InetReply::Ok)],
        "the queued route answers its caller tag (`inet_reply_finish`, `inet_drv.c:2614`)"
    );
    assert_eq!(
        receive_one(&peer),
        Vec::<u8>::new(),
        "a zero-length datagram is still a datagram on the wire"
    );
    expect_row(&mut rig, port, UDP_EXPECTED[2]);

    // Step 3 — the queued route again, this time with a 10-byte payload: `send_max` moves to 10,
    // and `send_oct` is the payload total (15), never the queue's byte count (15 + 27).
    rig.command(&prefixed_command(target, b"0123456789"))
        .expect("the queued 10-byte datagram is written");
    assert_eq!(
        drained_replies(&mut rig),
        vec![(Some(CALLER_REF_TAG.to_vec()), InetReply::Ok)],
        "one reply per queued command"
    );
    assert_eq!(
        receive_one(&peer),
        b"0123456789".to_vec(),
        "the 10-byte datagram reached the wire"
    );
    expect_row(&mut rig, port, UDP_EXPECTED[3]);

    // Step 4 — the named verb with a zero-length payload, so both routes are exercised with both
    // payload shapes. `send_avg` is 5 then `5 + (0 - 5)/4 = 3.75`, rendered as `3`.
    assert_eq!(
        control_on(&mut rig, port, "send", &loopback_arg(target)),
        ctl_ok(),
        "a zero-length named `send` answers `[1]`"
    );
    assert_eq!(
        receive_one(&peer),
        Vec::<u8>::new(),
        "the second zero-length datagram reached the wire"
    );
    expect_row(&mut rig, port, UDP_EXPECTED[4]);
}

/// The same sequence under the live `erl` oracle: `gen_udp:send/4` + `inet:getstat/2`, with a
/// `gen_udp:recv/3` drain proving every datagram — the two zero-length ones included — really
/// reached the wire.
///
/// `gen_udp:send/4` is the **queued** route (`prim_inet:sendto/4` → `packet_inet_command`), which
/// is where the "no header, payload only" fact matters: the driver is handed a command carrying a
/// caller tag, a packed address and an ancillary-length word, and still charges
/// `inet_output_count(desc, len)` the payload alone (`inet_drv.c:15163`).
///
/// Measured on OTP 29.1.1 / erts-17.1; the rows are [`UDP_EXPECTED`] verbatim.
#[test]
fn the_datagram_send_counter_rows_agree_with_the_erl_oracle() {
    let Some(stdout) = escript_stdout("stat_send_udp", UDP_PROBE, &[]) else {
        return;
    };
    let (rows, payloads) = split_oracle_output(&stdout);
    assert_oracle_rows("udp", &rows, &UDP_EXPECTED);
    assert_eq!(
        payloads,
        vec![
            ("wire".to_string(), b"hello".to_vec()),
            ("wire".to_string(), Vec::new()),
            ("wire".to_string(), b"0123456789".to_vec()),
            ("wire".to_string(), Vec::new()),
        ],
        "the oracle's receiver saw all four datagrams, the empty ones included"
    );
}

// ---------------------------------------------------------------------------
// Case E — the accepted socket starts at zero and counts its own traffic
// ---------------------------------------------------------------------------

/// `tcp_inet_copy` copies **options**, never counters: an accepted `tcp_inet` port answers an
/// all-zero `INET_REQ_GETSTAT` block even with the client's data already in its receive buffer,
/// counts only its own traffic afterwards, and never moves the listener's block.
///
/// The C's copy list (`inet_drv.c:11795`-`:11858`) names `inet.mode`, `inet.exitf`,
/// `inet.deliver`, `inet.htype`/`psize`/`hsz`, `inet.bufsz`, the watermarks, `send_timeout*` and
/// the `tcp_add_flags` subset. `desc->stat` is not on it, so the accepted descriptor keeps the
/// zeroed block `inet_init_desc` gave it — and the counters advance on **delivery**, not on
/// arrival, which is why the client's 5 pre-accept bytes move nothing.
///
/// `send_avg` on the accepted port is `2` after `"zz"` (`n = 1`) and `2 + (0 - 2)/2 = 1` after the
/// zero-length send (`n = 2`); `recv_avg` 5 with `recv_dvi` 0 is the first input sample. Rows are
/// [`TCP_EXPECTED`], measured on OTP 29.1.1 / erts-17.1.
#[test]
fn an_accepted_socket_starts_at_an_all_zero_block_and_counts_its_own_traffic() {
    let mut rig = Rig::new(make_tcp_inet().expect("the tcp_inet driver"));
    let listener = rig.id;
    let bound = listening_port(&mut rig);
    expect_row(&mut rig, listener, TCP_EXPECTED[0]);

    let (accepted, mut client) = accept_through_the_registry(&mut rig, listener, bound, b"hello");
    // The client's 5 bytes are already in the socket's receive buffer; the accepted block is still
    // zero, because `tcp_inet_copy` does not carry `desc->stat` across.
    expect_row(&mut rig, accepted, TCP_EXPECTED[1]);
    expect_row(&mut rig, listener, TCP_EXPECTED[2]);

    // The accepted port's own traffic, through the named `send` verb — `{packet, 0}` by default,
    // so `header_len` is zero and the counted length is the payload (`inet_drv.c:13763`).
    assert_eq!(
        control_on(&mut rig, accepted, "send", b"zz"),
        ctl_ok(),
        "the accepted port's named `send` verb answers `[1]`"
    );
    assert_eq!(
        control_on(&mut rig, accepted, "send", &[]),
        ctl_ok(),
        "a zero-length send answers `[1]` too"
    );
    expect_row(&mut rig, accepted, TCP_EXPECTED[3]);
    // The listener counted nothing for either send: they are not its traffic.
    expect_row(&mut rig, listener, TCP_EXPECTED[2]);

    // Reading the client's 5 bytes credits `recv_*` once with the delivered length.
    let five = recv_until(&mut rig, accepted, 5);
    assert_eq!(five, b"hello".to_vec(), "the accepted port read the client's 5 bytes");
    expect_row(&mut rig, accepted, TCP_EXPECTED[4]);
    expect_row(&mut rig, listener, TCP_EXPECTED[5]);

    // And the two sends really reached the client.
    let mut got = [0u8; 2];
    client
        .read_exact(&mut got)
        .expect("the client reads the accepted port's `zz`");
    assert_eq!(&got, b"zz", "the accepted port's writes are on the wire");
}

/// The same listener/accepted script under the live `erl` oracle: `gen_tcp:listen/2` +
/// `gen_tcp:accept/1` + `inet:getstat/2` on **both** sockets, the client writing 5 bytes before
/// the accept.
///
/// Measured on OTP 29.1.1 / erts-17.1; the rows are [`TCP_EXPECTED`] verbatim.
#[test]
fn the_accepted_socket_counter_rows_agree_with_the_erl_oracle() {
    let Some(stdout) = escript_stdout("stat_accepted", TCP_PROBE, &[]) else {
        return;
    };
    let (rows, payloads) = split_oracle_output(&stdout);
    assert_oracle_rows("tcp", &rows, &TCP_EXPECTED);
    assert_eq!(
        payloads,
        vec![
            ("read".to_string(), b"hello".to_vec()),
            ("client".to_string(), b"zz".to_vec()),
        ],
        "the accepted socket read the client's 5 bytes and the client got the two it sent"
    );
}
