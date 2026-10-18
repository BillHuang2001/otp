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
//! (`inet_input_count`/`inet_output_count`, `:11560`-`:11623`).
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
fn stat32(port: &mut Rig, code: u8) -> u32 {
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
fn stat64(port: &mut Rig, code: u8) -> u64 {
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
struct TcpPair {
    port: Rig,
    peer: TcpStream,
}

impl TcpPair {
    fn new() -> TcpPair {
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
    fn recv_until(&mut self, len: usize) {
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

/// Under `{packet, 2}` the send counter counts the payload **plus** its length header
/// (`inet_output_count(desc, len + h_len)`), while the receive counter counts only the delivered
/// payload — the header is stripped by the frame decoder before delivery.
#[test]
fn a_framed_tcp_command_counts_its_length_header() {
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
    assert_eq!(stat32(&mut pair.port, RECV_MAX), 3, "the header is stripped");
    assert_eq!(stat64(&mut pair.port, RECV_OCT), 3);
    assert_eq!(stat32(&mut pair.port, RECV_CNT), 1);
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
