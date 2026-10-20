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

//! The `INET_REQ_GETSTAT`(11) suite for **both** socket drivers: the reply shape the C's
//! `inet_fill_stat` builds (`inet_drv.c:10732`-`:10792`) and the accounting that feeds it
//! (`inet_input_count`/`inet_output_count`, `:11560`-`:11617`).
//!
//! Every case is driven through the runtime's own `erlang:port_control/3` route
//! ([`crate::PortRegistry::port_control`]) with the decimal command a live `beam_rt` forwards — so
//! the numeric opcode bridge, the reply rendering and the driver counters are exercised together.
//! The counter arithmetic itself is unit-tested in [`super::stat`]; this suite pins the **wire
//! shape** (record widths, request order, the zeroed block) and that the drivers advance the right
//! counter at the right call site.
//!
//! Nothing here needs `erl`: the bare-port reply below is the oracle's own bytes (measured on OTP
//! 29.1.1 / erts-17.1 for both drivers), and the live counters are asserted on the values this
//! floor's accounting must produce for the traffic it really moved over loopback.
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::os::unix::io::IntoRawFd;
use std::thread;
use std::time::Duration;

use super::opcode_tests::{bare_port as bare_tcp_port, loopback_arg, op, Rig};
use super::opcode_tests_udp::open_arg as udp_open_arg;
use super::wire::ctl_ok;
use super::{TcpInetDriver, UdpInetDriver, INET_REP_OK};

/// The `INET_STAT_*` op codes (`inet_drv.c:961`-`:970`).
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

/// One `INET_REQ_GETSTAT` call for the single op `code`, returned as its 32-bit value.
pub(super) fn stat32(port: &mut Rig, code: u8) -> u32 {
    let reply = port.control(&op(11), &[code]);
    assert_eq!(
        reply.len(),
        6,
        "a 32-bit counter record is [tag, op, 4 bytes]"
    );
    assert_eq!(reply[0], INET_REP_OK, "the leading success tag");
    assert_eq!(reply[1], code, "the requested op byte, echoed");
    u32::from_be_bytes([reply[2], reply[3], reply[4], reply[5]])
}

/// One `INET_REQ_GETSTAT` call for the octet op `code`, returned as its 64-bit value.
pub(super) fn stat64(port: &mut Rig, code: u8) -> u64 {
    let reply = port.control(&op(11), &[code]);
    assert_eq!(
        reply.len(),
        10,
        "an octet-counter record is [tag, op, 8 bytes]"
    );
    assert_eq!(reply[0], INET_REP_OK, "the leading success tag");
    assert_eq!(reply[1], code, "the requested op byte, echoed");
    let mut word = [0u8; 8];
    word.copy_from_slice(&reply[2..10]);
    u64::from_be_bytes(word)
}

/// The reply an all-zero counter block renders: the tag, ops 1-8 as five-byte records and the two
/// octet counters (9, 10) as nine-byte ones, in request order.
fn zeroed_counter_block() -> Vec<u8> {
    let mut reply = vec![1u8];
    for code in [RECV_CNT, RECV_MAX, RECV_AVG, RECV_DVI, SEND_CNT, SEND_MAX, SEND_AVG, SEND_PEND]
    {
        reply.push(code);
        reply.extend_from_slice(&0u32.to_be_bytes());
    }
    for code in [RECV_OCT, SEND_OCT] {
        reply.push(code);
        reply.extend_from_slice(&0u64.to_be_bytes());
    }
    reply
}

/// A connected `tcp_inet` port plus the far end of the connection, which the test writes to so the
/// driver has something to read.
///
/// The driver adopts a duplicate of the accepted socket (`try_clone().into_raw_fd()`, so it owns a
/// descriptor this test knows and no `unsafe` is needed to make one).
///
/// Both fields are `pub(super)` so the crate's other `getstat` suites (the send-counter half,
/// [`super::stat_send_tests`]) drive the same pair instead of building a second one.
pub(super) struct TcpPair {
    pub(super) port: Rig,
    pub(super) peer: TcpStream,
}

impl TcpPair {
    pub(super) fn new() -> TcpPair {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let peer = TcpStream::connect(listener.local_addr().expect("an address"))
            .expect("the far end connects");
        let (server, _accepted) = listener.accept().expect("the port's end is accepted");
        let fd = server
            .try_clone()
            .expect("a second descriptor for the same socket")
            .into_raw_fd();
        let driver = TcpInetDriver::from_socket(fd).expect("the driver adopts the socket");
        TcpPair {
            port: Rig::new(Box::new(driver)),
            peer,
        }
    }

    /// Drives the `recv` verb until `len` bytes have reached the port's input queue.
    ///
    /// A `recv` that finds nothing delivers nothing (and therefore counts nothing), so repeating it
    /// is safe: the counter still advances exactly once, when the bytes really arrive. The loop
    /// only bounds a genuinely broken driver — it is a condition wait, never a latency budget.
    pub(super) fn recv_until(&mut self, len: usize) {
        for _ in 0..500 {
            if self.port.input_len() >= len {
                return;
            }
            let _reply = self.port.control("recv", &[]);
            if self.port.input_len() >= len {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!(
            "the driver never received {len} bytes (input queue: {})",
            self.port.input_len()
        );
    }
}

// -----------------------------------------------------------------------
// The reply shape
// -----------------------------------------------------------------------

/// A socket-less `tcp_inet` port answers the C's zeroed counter block — the state
/// `inet_init_desc` leaves the descriptor in, and what the oracle answers for a bare
/// `open_port({spawn_driver,"tcp_inet"},[])` port (`inet_fill_stat` has **no** `IS_OPEN` guard).
///
/// `<<1,2,3,4,5,6,7,8,9,10>>` is therefore 59 bytes: the `[1]` tag, 8 five-byte records (40) and
/// 2 nine-byte records (18).
#[test]
fn a_bare_port_renders_the_zeroed_counter_block() {
    let expected = zeroed_counter_block();
    assert_eq!(expected.len(), 59, "1 + 8*5 + 2*9");

    let mut port = bare_tcp_port();
    assert_eq!(
        port.control(&op(11), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        expected,
        "the socket-less counter block, in request order"
    );

    // An empty request appends no record at all: just the reply tag.
    assert_eq!(port.control(&op(11), &[]), ctl_ok(), "nothing requested");
    // A single octet op still carries its full 64-bit word.
    assert_eq!(
        port.control(&op(11), &[RECV_OCT]),
        vec![1, 9, 0, 0, 0, 0, 0, 0, 0, 0],
        "one 64-bit octet record"
    );
}

/// A `udp_inet` port answers the same zeroed block: `inet_fill_stat` is the *shared* `inet_ctl`
/// arm (`inet_drv.c:10988`), so neither driver's switch changes the reply.
#[test]
fn a_bare_datagram_port_renders_the_same_zeroed_block() {
    let mut port = Rig::new(Box::new(UdpInetDriver::new()));
    assert_eq!(
        port.control(&op(11), &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        zeroed_counter_block()
    );
}

/// The records follow the **request order**, not the op order, and a repeated op is repeated in
/// the reply — the C's `while (len--) { *dst++ = op; … }` loop (`inet_drv.c:10740`).
#[test]
fn the_records_follow_the_request_order() {
    let mut port = bare_tcp_port();
    assert_eq!(
        port.control(&op(11), &[SEND_OCT, RECV_CNT, RECV_OCT]),
        vec![1, 10, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0],
        "an octet record, then a 32-bit one, then another octet record"
    );
}

// -----------------------------------------------------------------------
// The tcp_inet accounting
// -----------------------------------------------------------------------

/// A `send` over a connected socket moves `send_cnt`/`send_oct`/`send_max`/`send_avg`, counted
/// once per command at issue time (`inet_output_count(desc, len + h_len)`, `inet_drv.c:13763`).
///
/// Five bytes fit in the kernel's send buffer, so the whole command is written and `send_pend`
/// (`driver_sizeq`, the port's output-queue byte count) is back to zero — the deterministic
/// drained case.
#[test]
fn a_tcp_send_moves_the_output_counters() {
    let mut pair = TcpPair::new();
    let counters = |port: &mut Rig| {
        (
            stat32(port, SEND_CNT),
            stat32(port, SEND_MAX),
            stat32(port, SEND_AVG),
            stat64(port, SEND_OCT),
            stat32(port, SEND_PEND),
            stat32(port, RECV_CNT),
        )
    };

    assert_eq!(
        counters(&mut pair.port),
        (0, 0, 0, 0, 0, 0),
        "a fresh connection has counted nothing"
    );

    assert_eq!(
        pair.port.control("send", &[7u8; 5]),
        ctl_ok(),
        "the send verb"
    );
    assert_eq!(
        counters(&mut pair.port),
        (1, 5, 5, 5, 0, 0),
        "one command of five octets; the queue drained"
    );

    assert_eq!(
        pair.port.control("send", &[9u8; 15]),
        ctl_ok(),
        "a second, larger command"
    );
    assert_eq!(
        counters(&mut pair.port),
        // avg = 5 + (15 - 5)/2 = 10; the peak follows the larger command; 20 octets total.
        (2, 15, 10, 20, 0, 0),
        "the running average and the peak advanced"
    );
}

/// A `recv` over a connected socket moves the input counters by the bytes **delivered** to the
/// port (`inet_input_count(desc, len)`, `inet_drv.c:13064`) — with `{packet, raw}` that is every
/// byte read.
#[test]
fn a_tcp_recv_moves_the_input_counters() {
    let mut pair = TcpPair::new();
    use std::io::Write;
    pair.peer.write_all(b"abcdefghij").expect("the peer writes");
    pair.recv_until(10);

    assert_eq!(
        pair.port.take_input(10),
        b"abcdefghij".to_vec(),
        "the port input queue carries the delivered bytes"
    );
    assert_eq!(stat32(&mut pair.port, RECV_CNT), 1, "one delivery");
    assert_eq!(stat32(&mut pair.port, RECV_MAX), 10);
    assert_eq!(stat32(&mut pair.port, RECV_AVG), 10);
    assert_eq!(stat32(&mut pair.port, RECV_DVI), 0);
    assert_eq!(stat64(&mut pair.port, RECV_OCT), 10);
    assert_eq!(
        stat32(&mut pair.port, SEND_CNT),
        0,
        "receiving counted no output"
    );
}

/// Under `{packet, 2}` **both** directions count the framing header.
///
/// * output: `inet_output_count(desc, len + h_len)` — the C's `tcp_send` (`inet_drv.c:13763`;
///   its vector twin is `:13879`), where `h_len` is the 1/2/4-byte header the send path prepends;
/// * input: `inet_input_count(INETP(desc), len)` inside `tcp_deliver` (`:13064`), whose `len` came
///   out of `tcp_remain(desc, &len)` (`:12967`/`:13052`) as `packet_get_length`'s **total packet
///   length** —
///   so the *whole frame* is counted and only the payload is delivered
///   (`packet_get_body`/`packet_parser.h:152`).
///
/// Measured on the oracle for exactly this traffic (`{packet,2}`, one `<<0,3,"abc">>` in each
/// direction): `send_oct = 5`, `recv_oct = 5`, `recv_max = 5`.
#[test]
fn a_framed_tcp_read_and_write_count_the_framing_header() {
    let mut pair = TcpPair::new();
    // `INET_REQ_SETOPTS`(7) with the `{packet, 2}` record: code 23 = `INET_LOPT_PACKET`, value 2
    // (the C's `TCP_PB_2`, which is what `prim_inet` encodes for `{packet,2}`).
    let mut setopts = vec![23u8];
    setopts.extend_from_slice(&2i32.to_be_bytes());
    assert_eq!(pair.port.control(&op(7), &setopts), ctl_ok(), "setopts");

    assert_eq!(
        pair.port.control("send", &[1u8, 2, 3]),
        ctl_ok(),
        "a three-byte framed command"
    );
    assert_eq!(
        stat32(&mut pair.port, SEND_MAX),
        5,
        "three payload octets plus a two-byte length header"
    );
    assert_eq!(stat64(&mut pair.port, SEND_OCT), 5);

    use std::io::Write;
    pair.peer
        .write_all(&[0, 3, 10, 11, 12])
        .expect("the peer writes one framed message");
    pair.recv_until(3);
    assert_eq!(pair.port.take_input(3), vec![10, 11, 12], "the payload");
    assert_eq!(
        stat32(&mut pair.port, RECV_MAX),
        5,
        "the whole frame counts: three payload octets plus the two-byte header"
    );
    assert_eq!(stat64(&mut pair.port, RECV_OCT), 5);
    assert_eq!(stat32(&mut pair.port, RECV_CNT), 1);
    assert_eq!(stat32(&mut pair.port, RECV_AVG), 5, "the mean of one frame");
    assert_eq!(stat32(&mut pair.port, RECV_DVI), 0, "one sample, no deviation");
}

// -----------------------------------------------------------------------
// The framing-inclusive recv counters, against the oracle
// -----------------------------------------------------------------------

/// The five receive counters (`INET_STAT_*` ops 1, 2, 3, 4 and 9), in the C's op order.
const RECV_COUNTER_OPS: [u8; 5] = [RECV_CNT, RECV_MAX, RECV_AVG, RECV_DVI, RECV_OCT];

/// This floor's values for `ops`, read one `INET_REQ_GETSTAT` record at a time.
fn counters(port: &mut Rig, ops: &[u8]) -> Vec<u32> {
    ops.iter()
        .map(|&code| {
            if code == RECV_OCT {
                stat64(port, code) as u32
            } else {
                stat32(port, code)
            }
        })
        .collect()
}

/// The `[V || {_,V} <- Stat]` list the oracle expressions below print, parsed back into numbers.
///
/// `None` — after [`super::opcode_tests::erl_stdout`]'s own loud `SKIP:` banner — when `erl` is
/// absent or does not answer in time. A non-empty but unparsable dump is a bug in the probe and
/// fails the test.
fn oracle_counters(expression: &str) -> Option<Vec<u32>> {
    let Some(dump) = super::opcode_tests::erl_stdout(expression) else {
        return None;
    };
    let inner = dump
        .split_once('[')
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(list, _)| list)
        .unwrap_or_else(|| panic!("no counter list in the oracle's dump {dump:?}"));
    Some(
        inner
            .split(',')
            .map(|number| {
                number
                    .trim()
                    .parse::<u32>()
                    .unwrap_or_else(|_| panic!("an unparsable counter in {dump:?}"))
            })
            .collect(),
    )
}

/// Two `{packet, 2}` frames count their **whole** length on both sides of the wire, and the
/// running mean/deviation are the C's (`inet_input_count`, `inet_drv.c:11585`-`:11617`).
///
/// The oracle drives the same byte sequence — `<<0,3,"abc">>` then `<<0,7,"defghij">>`, one
/// `gen_tcp:recv/3` each — and reads `inet:getstat(S, [recv_cnt, recv_max, recv_avg, recv_dvi,
/// recv_oct])`. Both sides must answer `2, 9, 7, 1, 14`:
///
/// * `recv_oct` = 5 + 9 (the frame lengths, header included), `recv_max` = 9 (the larger frame);
/// * `recv_avg` `= 5 + (9 - 5)/2 = 7`;
/// * `recv_dvi` `= 0 + ((9 - 7) - 0)/2 = 1` — rendered through the C's `(unsigned long)
///   fabs(desc->recv_dvi)` (`inet_fill_stat`, `:10754`), so a negative deviation is its absolute
///   value.
///
/// This is the row the payload-only accounting used to report as `2, 7, 5, 1, 14` (a *well-formed
/// wrong value*, not a typed gap): the `{packet,N}` header is part of what the C counts.
#[test]
fn a_framed_tcp_recv_counts_the_whole_frame_like_the_oracle() {
    let mut pair = TcpPair::new();
    let mut setopts = vec![23u8];
    setopts.extend_from_slice(&2i32.to_be_bytes());
    assert_eq!(pair.port.control(&op(7), &setopts), ctl_ok(), "the `packet`/2 option");

    use std::io::Write;
    pair.peer
        .write_all(&[0, 3, b'a', b'b', b'c'])
        .expect("the peer writes the first frame");
    pair.peer
        .write_all(&[0, 7, b'd', b'e', b'f', b'g', b'h', b'i', b'j'])
        .expect("the peer writes the second frame");
    pair.recv_until(3);
    assert_eq!(pair.port.take_input(3), b"abc".to_vec(), "the first payload");
    pair.recv_until(7);
    assert_eq!(
        pair.port.take_input(7),
        b"defghij".to_vec(),
        "the second payload"
    );

    let rust = counters(&mut pair.port, &RECV_COUNTER_OPS);
    assert_eq!(
        rust,
        vec![2, 9, 7, 1, 14],
        "cnt, max, avg, dvi, oct: the whole frames, header included"
    );

    let Some(oracle) = oracle_counters(
        "{ok,L}=gen_tcp:listen(0,[binary,{active,false},{packet,2},{ip,{127,0,0,1}}]), \
         {ok,P}=inet:port(L), \
         {ok,C}=gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{packet,2}]), \
         {ok,S}=gen_tcp:accept(L), \
         ok=gen_tcp:send(C,<<\"abc\">>), \
         ok=gen_tcp:send(C,<<\"defghij\">>), \
         {ok,<<\"abc\">>}=gen_tcp:recv(S,0,5000), \
         {ok,<<\"defghij\">>}=gen_tcp:recv(S,0,5000), \
         {ok,St}=inet:getstat(S,[recv_cnt,recv_max,recv_avg,recv_dvi,recv_oct]), \
         io:format(\"~w|\",[[V || {_,V} <- St]]), \
         gen_tcp:close(S), gen_tcp:close(C), gen_tcp:close(L)",
    ) else {
        return;
    };
    assert_eq!(oracle, rust, "the oracle's framed counters");
}

/// A raw (`{packet, raw}`) read counts exactly the bytes it delivers — the C's `tcp_deliver`
/// over a `TCP_PB_RAW` buffer, whose `packet_get_length` answers "the whole buffer"
/// (`packet_parser.c:259`-`:264`).
///
/// Cross-checked against the oracle for the same three writes read as one burst.
#[test]
fn a_raw_tcp_recv_counts_the_delivered_bytes_like_the_oracle() {
    let mut pair = TcpPair::new();
    use std::io::Write;
    pair.peer.write_all(b"abc").expect("the peer writes");
    pair.recv_until(3);
    assert_eq!(pair.port.take_input(3), b"abc".to_vec());
    let rust = counters(&mut pair.port, &RECV_COUNTER_OPS);
    assert_eq!(rust, vec![1, 3, 3, 0, 3], "cnt, max, avg, dvi, oct");

    let Some(oracle) = oracle_counters(
        "{ok,L}=gen_tcp:listen(0,[binary,{active,false},{packet,0},{ip,{127,0,0,1}}]), \
         {ok,P}=inet:port(L), \
         {ok,C}=gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{packet,0}]), \
         {ok,S}=gen_tcp:accept(L), \
         ok=gen_tcp:send(C,<<\"abc\">>), \
         {ok,_}=gen_tcp:recv(S,3,5000), \
         {ok,St}=inet:getstat(S,[recv_cnt,recv_max,recv_avg,recv_dvi,recv_oct]), \
         io:format(\"~w|\",[[V || {_,V} <- St]]), \
         gen_tcp:close(S), gen_tcp:close(C), gen_tcp:close(L)",
    ) else {
        return;
    };
    assert_eq!(oracle, rust, "the oracle's raw counters");
}

// -----------------------------------------------------------------------
// send_pend — the port's output-queue depth (the C's `driver_sizeq`)
// -----------------------------------------------------------------------

/// `send_pend` (op 8) is the C's `driver_sizeq(desc->port)` (`inet_fill_stat`, `:10766`) — the
/// bytes still queued for the socket — which is this floor's `ctx.output().len()`
/// ([`super::wire::getstat_reply`]'s `send_pend` argument).
///
/// A command larger than the connection can absorb stalls: the peer never reads, so the kernel's
/// send buffer plus the peer's receive window fill up and the remainder stays in the port's output
/// queue (a short write, the C's `driver_enqv(ix, ev, n)` at the end of `tcp_send`). `send_pend`
/// must then be exactly that remainder, while `send_oct`/`send_max` already count the **whole**
/// command (`inet_output_count` runs at issue time, before anything is written).
#[test]
fn send_pend_reports_the_command_bytes_still_queued() {
    let mut pair = TcpPair::new();
    assert_eq!(stat32(&mut pair.port, SEND_PEND), 0, "nothing queued yet");

    // 16 MiB can never fit in the socket's send buffer *and* the peer's receive window, so a
    // non-reading peer guarantees a remainder — no timing involved.
    let payload = vec![0u8; 16 * 1024 * 1024];
    let mut setopts = vec![23u8];
    setopts.extend_from_slice(&2i32.to_be_bytes());
    assert_eq!(pair.port.control(&op(7), &setopts), ctl_ok(), "the `packet`/2 option");
    assert_eq!(
        pair.port.control("send", &payload),
        ctl_ok(),
        "the send verb queues what the socket cannot take"
    );

    let id = pair.port.id;
    let queued = pair
        .port
        .registry
        .get(id)
        .expect("a live port")
        .output_len();
    assert!(queued > 0, "the kernel could not take the whole command");
    assert!(
        queued < payload.len() + 2,
        "a strict remainder of the command is queued ({queued} of {})",
        payload.len() + 2
    );
    assert_eq!(
        stat32(&mut pair.port, SEND_PEND) as usize,
        queued,
        "`send_pend` is the port's output-queue depth"
    );
    assert_eq!(
        stat64(&mut pair.port, SEND_OCT),
        (payload.len() + 2) as u64,
        "the whole command was counted at issue, header included"
    );
    assert_eq!(stat32(&mut pair.port, SEND_MAX) as usize, payload.len() + 2);
    assert_eq!(stat32(&mut pair.port, SEND_CNT), 1);
}

/// The oracle's `driver_sizeq` and this floor's `ctx.output().len()` describe the same state: a
/// 16 MiB `{packet,2}` command to a peer that never reads leaves a non-empty queue on both sides,
/// and both count the command's **whole** length (payload + header) as output at issue time.
///
/// The remainder itself is whatever the kernel took, so only the invariants are compared — the
/// exact `send_pend` is *not* a portable number (measured on this host: 14 188 546 of 16 777 218).
#[test]
fn send_pend_agrees_with_the_oracle() {
    let Some(oracle) = oracle_counters(
        "{ok,L}=gen_tcp:listen(0,[binary,{active,false},{packet,2},{ip,{127,0,0,1}}]), \
         {ok,P}=inet:port(L), \
         {ok,C}=gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{packet,2}]), \
         {ok,S}=gen_tcp:accept(L), \
         Big=binary:copy(<<0>>,16*1024*1024), \
         ok=gen_tcp:send(S,Big), \
         {ok,St}=inet:getstat(S,[send_pend,send_oct,send_max,send_cnt]), \
         io:format(\"~w|\",[[V || {_,V} <- St]]), \
         gen_tcp:close(S), gen_tcp:close(C), gen_tcp:close(L)",
    ) else {
        return;
    };
    let [oracle_pend, oracle_oct, oracle_max, oracle_cnt] = oracle[..] else {
        panic!("the oracle answered {} counters, not four", oracle.len());
    };
    let whole: usize = 16 * 1024 * 1024 + 2;
    assert_eq!(oracle_oct, whole as u32, "the oracle counted the whole command");
    assert_eq!(oracle_max, whole as u32);
    assert_eq!(oracle_cnt, 1);
    assert!(oracle_pend > 0, "the oracle's queue kept a remainder");
    assert!(
        (oracle_pend as usize) < whole,
        "and it is a strict remainder"
    );

    // The same traffic through this floor, whose `send_pend` is its own output-queue depth.
    let mut pair = TcpPair::new();
    let mut setopts = vec![23u8];
    setopts.extend_from_slice(&2i32.to_be_bytes());
    assert_eq!(pair.port.control(&op(7), &setopts), ctl_ok(), "the `packet`/2 option");
    assert_eq!(pair.port.control("send", &vec![0u8; 16 * 1024 * 1024]), ctl_ok());
    let id = pair.port.id;
    let queued = pair.port.registry.get(id).expect("a live port").output_len();
    assert!(queued > 0 && queued < whole, "the same strict remainder");
    assert_eq!(stat32(&mut pair.port, SEND_PEND) as usize, queued);
    assert_eq!(stat64(&mut pair.port, SEND_OCT), whole as u64);
    assert_eq!(stat32(&mut pair.port, SEND_MAX) as usize, whole);
    assert_eq!(stat32(&mut pair.port, SEND_CNT), 1);
}
// -----------------------------------------------------------------------
// The udp_inet accounting
// -----------------------------------------------------------------------

/// A datagram send counts the **payload** only — never the packed address prefix
/// (`inet_output_count(desc, len)` at `inet_drv.c:15163`), and the datagram that comes back counts
/// its own payload (`udp_deliver`, `:15513`).
#[test]
fn a_udp_datagram_moves_both_counters() {
    let mut port = Rig::new(Box::new(UdpInetDriver::new()));
    assert_eq!(
        port.control(&op(1), &udp_open_arg()),
        ctl_ok(),
        "INET_REQ_OPEN"
    );
    let bound = port.control(&op(6), &loopback_arg(0));
    assert_eq!(bound[0], INET_REP_OK, "INET_REQ_BIND");
    let bound_port = u16::from_be_bytes([bound[1], bound[2]]);
    assert_ne!(bound_port, 0, "the kernel chose the port");

    let client = UdpSocket::bind("127.0.0.1:0").expect("a loopback datagram socket");
    let client_port = client.local_addr().expect("an address").port();

    // The driver sends to the test's socket: `[packed address][payload]`.
    let mut send_arg = loopback_arg(client_port);
    send_arg.extend_from_slice(b"ping!");
    assert_eq!(port.control("send", &send_arg), ctl_ok(), "the send verb");
    let mut buf = [0u8; 32];
    let (len, _from) = client.recv_from(&mut buf).expect("the datagram arrives");
    assert_eq!(&buf[..len], b"ping!", "the payload, without the prefix");
    assert_eq!(
        stat32(&mut port, SEND_CNT),
        1,
        "the prefix is not counted as output"
    );
    assert_eq!(stat64(&mut port, SEND_OCT), 5, "`ping!` is five octets");
    assert_eq!(stat32(&mut port, SEND_MAX), 5);

    // ... and the test's socket answers, which the driver reads with its `recv` verb.
    client
        .send_to(b"pong!!", ("127.0.0.1", bound_port))
        .expect("the reply datagram leaves");
    for _ in 0..500 {
        let reply = port.control("recv", &[]);
        if reply.len() == 8 {
            break;
        }
        assert_eq!(reply, ctl_ok(), "nothing pending yet");
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(port.input_len(), 6, "the datagram payload was delivered");
    assert_eq!(port.take_input(6), b"pong!!".to_vec());
    assert_eq!(stat32(&mut port, RECV_CNT), 1);
    assert_eq!(stat64(&mut port, RECV_OCT), 6);
    assert_eq!(stat32(&mut port, RECV_MAX), 6);
}
