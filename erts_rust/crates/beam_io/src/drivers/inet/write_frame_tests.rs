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

//! The **write-side framing** suite: the `{packet, 1|2|4}` length header the C prepends on the way
//! **out** (`tcp_sendv`, `inet_drv.c:13744`-`:13769`, and the non-vector `tcp_send` at
//! `:13861`-`:13885`).
//!
//! Real ERTS frames on write as well as on read, on **both** of the driver's write routes: the
//! named `send` control verb ([`super::TcpInetDriver::verb_send`]) and the
//! `erlang:port_command/2` path every `gen_tcp:send/2` takes (`prim_inet:send/3` →
//! `erlang:port_command/2` → [`crate::Driver::output`] → [`super::TcpInetDriver::write_command`]).
//! This suite pins the second route — over a real loopback socket, through
//! [`crate::PortRegistry`] (so [`crate::Port`]'s write-accounting check is in the path), with the
//! far end read by the test and the bytes compared exactly.
//!
//! # The two numbers the header must not confuse
//!
//! `prim_inet:send/3` prefixes every command with a `term_to_binary/2` monitor reference
//! (`erts/preloaded/src/prim_inet.erl:578`-`:585`), so the driver's `init_caller` strip
//! (`inet_drv.c:1827`-`:1851`) takes those bytes off **before** `tcp_sendv` reads
//! `len = ev->size`. The header therefore announces the **payload** length, never the payload plus
//! the tag — and `inet_output_count(INETP(desc), len + h_len)` (`:13763`) counts the payload plus
//! the header, never the tag. Both facts are asserted below with a real, non-empty tag.
//!
//! # The packet code the driver sees
//!
//! `{packet, 4}` is translated by the **Erlang** side: `prim_inet`'s `type_opt_1(packet)` maps the
//! caller's value onto the C's `TCP_PB_*` enum (`prim_inet.erl:1754`-`:1771`,
//! `lib/kernel/src/inet_int.hrl:216`-`:230`), so `{packet, raw|1|2|4|line}` reaches the driver as
//! `0|1|2|3|8`. The `setopts` records below carry those **internal** codes — the same ones
//! [`super::packet::PacketType::from_code`] decodes.
//!
//! # Oracle
//!
//! Every byte sequence is cross-checked against the system `erl` (`gen_tcp` on both ends, the
//! receiver in `{packet, raw}` mode so the test sees exactly what went on the wire). The oracle
//! cases skip **loudly** when `erl` is absent or does not answer, and never fail for that reason.
use std::io::{ErrorKind, Read};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::IntoRawFd;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::opcode_tests::{bare_port as bare_tcp_port, erl_stdout, op, Rig};use super::wire::ctl_ok;
use super::{TcpInetDriver, INET_REP_OK};

/// `INET_OPT_PACKET` — the `{packet, …}` option code (`inet_drv.c:637`).
const INET_OPT_PACKET: u8 = 23;

// The C's `enum PacketParseType` values the `{packet, …}` option is translated to on the Erlang
// side (`lib/kernel/src/inet_int.hrl:216`-`:230`). The driver never sees the caller's `4`.
const TCP_PB_RAW: u32 = 0;
const TCP_PB_1: u32 = 1;
const TCP_PB_2: u32 = 2;
const TCP_PB_4: u32 = 3;
const TCP_PB_LINE_LF: u32 = 8;

// `INET_STAT_*` op codes (`inet_drv.c:961`-`:970`) used by the counter cases.
const STAT_SEND_CNT: u8 = 5;
const STAT_SEND_MAX: u8 = 6;
const STAT_SEND_OCT: u8 = 10;

/// The tag `prim_inet:send/3` builds is a `term_to_binary/2` local reference; the bytes themselves
/// are never inspected by the C (`init_caller` is size-only), so the suite uses the same eight-byte
/// local ref the sibling `tests` suite does.
const CALLER_REF_TAG: &[u8] = &[131, 100, 0, 8, b'r', b'e', b'f', b'0'];

/// The bound on every wait below. Each wait ends the instant its condition holds (`read_exact`
/// completes, the output queue drains), so this only stops a genuinely broken driver from hanging
/// the suite — it is never a latency budget tuned for an idle host.
const HANG_GUARD: Duration = Duration::from_secs(20);

/// A connected `tcp_inet` port plus the far end of the connection.
///
/// The driver adopts a **duplicate** of the accepted socket (`try_clone().into_raw_fd()`, so it
/// owns a descriptor this test knows and no `unsafe` is needed to make one); the original accepted
/// socket is dropped, which leaves the connection up through the duplicate alone. The far end
/// (`peer`) is the test's, so every byte the driver writes is observed exactly.
struct Wired {
    port: Rig,
    peer: TcpStream,
}

impl Wired {
    fn new() -> Wired {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
        let peer = TcpStream::connect(listener.local_addr().expect("an address"))
            .expect("the far end connects");
        let (server, _accepted) = listener.accept().expect("the port's end is accepted");
        let fd = server
            .try_clone()
            .expect("a second descriptor for the same socket")
            .into_raw_fd();
        let driver = TcpInetDriver::from_socket(fd).expect("the driver adopts the socket");
        assert!(driver.state().is_connected(), "the adopted socket has a peer");
        Wired {
            port: Rig::new(Box::new(driver)),
            peer,
        }
    }

    /// Sets `{packet, Code}` through the numeric `setopts`(7) verb, with the **internal**
    /// `TCP_PB_*` code (`prim_inet`'s translation, see the module docs).
    fn set_packet(&mut self, code: u32) {
        let mut record = vec![INET_OPT_PACKET];
        record.extend_from_slice(&code.to_be_bytes());
        assert_eq!(
            self.port.control(&op(7), &record),
            ctl_ok(),
            "INET_REQ_SETOPTS(7): the `packet` option"
        );
    }

    /// Queues `tag` + `payload` as one `erlang:port_command/2` command — the exact wire form
    /// `prim_inet:send/3` builds — and runs the round that writes it.
    ///
    /// The round is asserted to be `Ok`: a driver that reports a byte count its output queue did
    /// not shrink by is the typed `unsupported` from [`crate::Port`]'s accounting check, which is
    /// the failure mode a framing header pushed during the callback would produce.
    fn command(&mut self, tag: &[u8], payload: &[u8]) {
        let bytes = prefixed_command(tag, payload);
        match self.port.command(&bytes) {
            Ok(()) => {}
            Err(err) => panic!("the port rejected the command: {err:?}"),
        }
    }

    /// Reads exactly `len` bytes off the far end — the test's own end of the connection.
    fn read_exactly(&mut self, len: usize) -> Vec<u8> {
        self.peer
            .set_read_timeout(Some(HANG_GUARD))
            .expect("a read timeout");
        let mut buf = vec![0u8; len];
        self.peer
            .read_exact(&mut buf)
            .unwrap_or_else(|err| panic!("the peer never received {len} bytes: {err}"));
        buf
    }

    /// Asserts the far end has **nothing else** to read: the driver wrote exactly what the caller
    /// expected and not one byte more.
    fn assert_nothing_else(&mut self) {
        self.peer
            .set_read_timeout(Some(Duration::from_millis(200)))
            .expect("a read timeout");
        let mut spare = [0u8; 64];
        match self.peer.read(&mut spare) {
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Ok(0) => panic!("the peer saw the connection close instead of a quiet stream"),
            Ok(n) => panic!(
                "the driver wrote {} unexpected byte(s) beyond the payload: {:?}",
                n,
                &spare[..n]
            ),
            Err(err) => panic!("the peer's extra-byte probe failed: {err}"),
        }
    }

    /// The port's current output-queue depth (the C's `driver_sizeq`).
    fn output_len(&mut self) -> usize {
        self.port
            .registry
            .get(self.port.id)
            .expect("a live port")
            .output_len()
    }

    /// Reads whatever is left on the far end — an empty vector once the connection is closed.
    fn read_from_closed_peer(&mut self) -> Vec<u8> {
        self.peer
            .set_read_timeout(Some(HANG_GUARD))
            .expect("a read timeout");
        let mut rest = Vec::new();
        self.peer
            .read_to_end(&mut rest)
            .expect("the peer reads to end-of-file");
        rest
    }
    /// One reactor round: poll for readiness (write readiness re-drives a stalled write through
    /// `ready_output`) and run the port tasks it scheduled.
    fn poll_round(&mut self) {
        let events = match self.port.poller.poll(Some(Duration::from_millis(20))) {
            Ok(events) => events,
            Err(err) => panic!("the poll set failed: {err:?}"),
        };
        match self
            .port
            .registry
            .handle_events(&mut self.port.poller, &events)
        {
            Ok(_outcome) => {}
            Err(err) => panic!("the reactor round failed: {err:?}"),
        }
    }

    /// One `INET_REQ_GETSTAT`(11) call for the 64-bit octet counter `code`.
    fn stat64(&mut self, code: u8) -> u64 {
        let reply = self.port.control(&op(11), &[code]);
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

    /// One `INET_REQ_GETSTAT`(11) call for the 32-bit counter `code`.
    fn stat32(&mut self, code: u8) -> u32 {
        let reply = self.port.control(&op(11), &[code]);
        assert_eq!(
            reply.len(),
            6,
            "a 32-bit counter record is [tag, op, 4 bytes]"
        );
        assert_eq!(reply[0], INET_REP_OK, "the leading success tag");
        assert_eq!(reply[1], code, "the requested op byte, echoed");
        u32::from_be_bytes([reply[2], reply[3], reply[4], reply[5]])
    }
}

/// The `erlang:port_command/2` command wire form `prim_inet:send/3` builds: a big-endian 16-bit tag
/// length, the tag, then the caller's data (`prim_inet.erl:584`).
fn prefixed_command(tag: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + tag.len() + payload.len());
    out.extend_from_slice(&(tag.len() as u16).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(payload);
    out
}

/// The 1-/2-/4-byte big-endian header the C's `put_int8`/`put_int16`/`put_int32` would write for a
/// payload of `len` bytes.
fn expected_header(width: usize, len: usize) -> Vec<u8> {
    match width {
        1 => vec![len as u8],
        2 => (len as u16).to_be_bytes().to_vec(),
        4 => (len as u32).to_be_bytes().to_vec(),
        other => panic!("no such header width: {other}"),
    }
}

// -----------------------------------------------------------------------
// The framed codes: the header announces the payload, never the tag
// -----------------------------------------------------------------------

/// `{packet, 4}` puts a four-byte big-endian payload length in front of the payload on the
/// `erlang:port_command/2` path — the C's `put_int32` arm of `tcp_sendv`'s `switch(desc->inet.htype)`
/// (`inet_drv.c:13744`-`:13769`).
///
/// The command carries `prim_inet`'s real (non-empty) tag, so the assertion is also the
/// tag-exclusion rule: `init_caller_iov` (`:1827`-`:1851`) reduces `ev->size` by the tag before
/// `tcp_sendv` reads `len`, so the header announces `5` — the payload — and not `13`.
#[test]
fn packet_4_frames_a_port_command_with_the_payload_length() {
    let mut wired = Wired::new();
    wired.set_packet(TCP_PB_4);
    wired.command(CALLER_REF_TAG, b"hello");

    let mut expected = expected_header(4, 5);
    expected.extend_from_slice(b"hello");
    assert_eq!(
        wired.read_exactly(expected.len()),
        expected,
        "the peer sees the header, then the payload, and the tag never goes on the wire"
    );
    wired.assert_nothing_else();
}

/// `{packet, 2}` — the C's `put_int16` arm (`inet_drv.c:13750`-`:13757`).
#[test]
fn packet_2_frames_a_port_command_with_the_payload_length() {
    let mut wired = Wired::new();
    wired.set_packet(TCP_PB_2);
    wired.command(CALLER_REF_TAG, b"hello");

    let mut expected = expected_header(2, 5);
    expected.extend_from_slice(b"hello");
    assert_eq!(wired.read_exactly(expected.len()), expected);
    wired.assert_nothing_else();
}

/// `{packet, 1}` — the C's `put_int8` arm (`inet_drv.c:13745`-`:13749`).
#[test]
fn packet_1_frames_a_port_command_with_the_payload_length() {
    let mut wired = Wired::new();
    wired.set_packet(TCP_PB_1);
    wired.command(CALLER_REF_TAG, b"hello");

    let mut expected = expected_header(1, 5);
    expected.extend_from_slice(b"hello");
    assert_eq!(wired.read_exactly(expected.len()), expected);
    wired.assert_nothing_else();
}

/// A **zero-length** tag (`<<0,0>>`) is still a caller prefix — `init_caller` consumes its length
/// word and keeps an empty tag buffer (`inet_drv.c:1827`-`:1851`) — so the payload is framed and the
/// two tag bytes never reach the wire.
///
/// Two framing commands in a row also pin the **per-command** latch: the header is decided once per
/// command, and the second command is framed afresh (a latch that outlived its command would either
/// drop the second header or emit two).
#[test]
fn a_zero_length_tag_is_stripped_and_each_command_is_framed_alone() {
    let mut wired = Wired::new();
    wired.set_packet(TCP_PB_4);

    wired.command(&[], b"one");
    let mut expected = expected_header(4, 3);
    expected.extend_from_slice(b"one");

    wired.command(&[], b"two");
    expected.extend_from_slice(&expected_header(4, 3));
    expected.extend_from_slice(b"two");

    assert_eq!(
        wired.read_exactly(expected.len()),
        expected,
        "two headers, one per command, each announcing its own payload"
    );
    wired.assert_nothing_else();
}

/// Every other `{packet, …}` code takes the C's `default: h_len = 0` arm
/// (`inet_drv.c:13769`-`:13771`): the payload goes out verbatim, with no header at all.
///
/// `raw` and `line` are the two the runtime actually uses on this path (`gen_tcp:send/2` under
/// `{packet,0}` and `{packet,line}`); a payload carrying the line delimiter is included so a
/// "helpfully" re-framed line port cannot pass.
#[test]
fn raw_and_line_codes_write_the_payload_verbatim() {
    for (code, label) in [(TCP_PB_RAW, "raw"), (TCP_PB_LINE_LF, "line")] {
        let mut wired = Wired::new();
        wired.set_packet(code);
        wired.command(CALLER_REF_TAG, b"ab\ncd");
        assert_eq!(
            wired.read_exactly(5),
            b"ab\ncd".to_vec(),
            "`{{packet,{label}}}` writes the payload unchanged"
        );
        wired.assert_nothing_else();
    }
}

/// An **empty** payload under `{packet, 4}` still writes the bare four-byte header: the C's
/// `(h_len == 0) && (len == 0)` shortcut (`inet_drv.c:13798`) does not apply, because `h_len` is
/// `4` (`:13806`-`:13810` requeue/`send` the header alone).
///
/// Measured on OTP 29.1.1 / erts-17.1: `gen_tcp:send(C, <<>>)` under `{packet,4}` answers `ok`
/// and the receiver reads exactly `<<0,0,0,0>>`.
#[test]
fn an_empty_payload_under_packet_4_writes_the_bare_header() {
    let mut wired = Wired::new();
    wired.set_packet(TCP_PB_4);
    wired.command(CALLER_REF_TAG, b"");

    assert_eq!(
        wired.read_exactly(4),
        vec![0, 0, 0, 0],
        "the zero-length body is announced, not skipped"
    );
    wired.assert_nothing_else();
}

/// A payload longer than the header width keeps its whole body and truncates only the header —
/// the C stores `put_int8(300, buf)`'s `300 & 0xff` (`inet_drv.c:13745`-`:13749`) and sends the
/// entire payload regardless.
#[test]
fn packet_1_truncates_the_header_but_keeps_the_whole_payload() {
    let mut wired = Wired::new();
    wired.set_packet(TCP_PB_1);
    let payload = vec![b'a'; 300];
    wired.command(CALLER_REF_TAG, &payload);

    let mut expected = vec![44u8];
    expected.extend_from_slice(&payload);
    assert_eq!(expected.len(), 301);
    assert_eq!(
        wired.read_exactly(expected.len()),
        expected,
        "the header is 300 as u8; the body is all 300 bytes"
    );
    wired.assert_nothing_else();
}

/// A **short write** leaves the header and the payload queued together and re-drives the write
/// through `ready_output`; the peer must then see exactly one header followed by the whole payload.
///
/// 16 MiB can never fit in the socket's send buffer plus the peer's receive window, so a peer that
/// reads only after the first round is guaranteed to force a partial write (the technique
/// [`super::stat_tests`]'s `send_pend` rows use). A second framing of the retained remainder would
/// put a header announcing the *remainder* length inside the payload; a lost header would show up
/// as a short first read. Every round is asserted `Ok`, so a write-accounting mismatch fails here.
#[test]
fn a_short_write_frames_once_and_delivers_every_byte() {
    const PAYLOAD: usize = 16 * 1024 * 1024;
    let mut wired = Wired::new();
    wired.set_packet(TCP_PB_4);

    let payload: Vec<u8> = (0..PAYLOAD).map(|i| (i % 251) as u8).collect();
    let mut expected = expected_header(4, PAYLOAD);
    expected.extend_from_slice(&payload);
    let expected_len = expected.len();

    // The command and its first (partial) write: 16 MiB can never fit in the socket's send buffer
    // plus the peer's receive window, so a strict remainder is guaranteed to be queued.
    //
    // The draining reader is started **after** these assertions, never before: a reader running
    // beside the command's own write can drain the socket between the driver's `send(2)` calls, so
    // on a loaded host the whole command would be accepted and `queued == 0` — a flake in the
    // instrument, not a divergence. A non-reading peer makes the remainder deterministic.
    wired.command(CALLER_REF_TAG, &payload);
    let queued = wired.output_len();
    assert!(
        queued > 0,
        "the kernel could not take the whole command; a strict remainder must be queued"
    );
    assert!(queued < expected_len, "a strict remainder of the command");

    let mut reader = wired.peer.try_clone().expect("a second descriptor");
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut got = vec![0u8; expected_len];
        let read = reader.read_exact(&mut got);
        let _ = tx.send((read, got));
    });

    let started = Instant::now();
    while wired.output_len() > 0 {
        // A non-empty queue arms write readiness, so every round re-drives the retained remainder.
        wired.poll_round();
        assert!(
            started.elapsed() < HANG_GUARD,
            "the write never drained ({} byte(s) still queued)",
            wired.output_len()
        );
    }
    let (read, got) = rx.recv_timeout(HANG_GUARD).expect("the peer received it all");
    read.expect("the peer reads the whole command");
    handle.join().expect("the reader thread");
    assert_eq!(
        got, expected,
        "one header, then the whole payload — no second header, no missing byte"
    );
    wired.assert_nothing_else();

    // The whole command was counted once, at issue time (the C's `inet_output_count(len+h_len)`).
    assert_eq!(
        wired.stat64(STAT_SEND_OCT),
        (PAYLOAD + 4) as u64,
        "payload plus header, counted once"
    );
    assert_eq!(wired.stat32(STAT_SEND_CNT), 1, "one command, one counting");
    assert_eq!(wired.stat32(STAT_SEND_MAX) as usize, PAYLOAD + 4);
}

// -----------------------------------------------------------------------
// The output counters — `inet_output_count(len + h_len)`
// -----------------------------------------------------------------------

/// `send_oct`/`send_max`/`send_cnt` count the payload **plus** the framing header, once per command
/// at issue time (`inet_output_count(INETP(desc), len + h_len)`, `inet_drv.c:13763`), and the
/// caller tag is not part of either figure.
///
/// The oracle for the same three commands answers `[9,9,1]`, `[11,9,2]`, `[15,9,3]` for
/// `{send_oct, send_max, send_cnt}`: `{packet,4}` + `"hello"` = 9, a raw `"hi"` adds 2, and the
/// empty `{packet,4}` body adds its 4 header bytes.
#[test]
fn send_oct_counts_the_header_and_the_payload_like_the_oracle() {
    let mut wired = Wired::new();

    wired.set_packet(TCP_PB_4);
    wired.command(CALLER_REF_TAG, b"hello");
    assert_eq!(wired.read_exactly(9), {
        let mut e = expected_header(4, 5);
        e.extend_from_slice(b"hello");
        e
    });
    assert_eq!(wired.stat64(STAT_SEND_OCT), 9, "5 payload + 4 header");
    assert_eq!(wired.stat32(STAT_SEND_MAX) as usize, 9);
    assert_eq!(wired.stat32(STAT_SEND_CNT), 1);

    wired.set_packet(TCP_PB_RAW);
    wired.command(CALLER_REF_TAG, b"hi");
    assert_eq!(wired.read_exactly(2), b"hi".to_vec());
    assert_eq!(wired.stat64(STAT_SEND_OCT), 11, "a raw command adds its 2 bytes");
    assert_eq!(wired.stat32(STAT_SEND_MAX) as usize, 9, "the peak is unchanged");
    assert_eq!(wired.stat32(STAT_SEND_CNT), 2);

    wired.set_packet(TCP_PB_4);
    wired.command(CALLER_REF_TAG, b"");
    assert_eq!(wired.read_exactly(4), vec![0, 0, 0, 0]);
    assert_eq!(wired.stat64(STAT_SEND_OCT), 15, "the bare header's 4 bytes");
    assert_eq!(wired.stat32(STAT_SEND_MAX) as usize, 9);
    assert_eq!(wired.stat32(STAT_SEND_CNT), 3);
}

/// A command refused for being **unconnected** is neither framed nor counted: the C returns from
/// `tcp_inet_commandv`'s `!IS_CONNECTED` arm (`inet_drv.c:12674`-`:12692`) **before** `tcp_sendv`
/// (whose `inet_output_count` at `:13763` is the only counting site), so `send_oct`/`send_cnt` stay
/// where they were while `erlang:port_command/2` still answers `true`.
///
/// Measured on the oracle, on a `open_port({spawn_driver,"tcp_inet"},[])` port after
/// `setopts` `{packet,4}`: `getstat` answers `[1,10,0,0,0,0,0,0,0,0]` both before and after the
/// command (`port_control(P, 11+magic, [10])`), and `port_command` answers `true`.
#[test]
fn an_unconnected_command_is_never_framed_or_counted() {
    let mut port = bare_tcp_port();
    let mut record = vec![INET_OPT_PACKET];
    record.extend_from_slice(&TCP_PB_4.to_be_bytes());
    assert_eq!(
        port.control(&op(7), &record),
        ctl_ok(),
        "the `packet` option is a descriptor field: a socket-less port keeps it"
    );
    assert_eq!(
        port.control(&op(11), &[STAT_SEND_OCT]),
        vec![1, STAT_SEND_OCT, 0, 0, 0, 0, 0, 0, 0, 0],
        "nothing counted yet"
    );

    // The tag is well formed, so the command reaches the connection guard instead of killing the
    // port (`init_caller` is size-only and runs first).
    let command = prefixed_command(&[], b"abc");
    port.command(&command)
        .expect("`erlang:port_command/2` answers `true` for a refused command");

    assert_eq!(
        port.control(&op(11), &[STAT_SEND_OCT, STAT_SEND_CNT]),
        vec![1, STAT_SEND_OCT, 0, 0, 0, 0, 0, 0, 0, 0, STAT_SEND_CNT, 0, 0, 0, 0],
        "a refused command is charged nothing — no payload, no header"
    );
}

/// A **malformed** command (`init_caller`'s FALSE: too short for the length word, or a length that
/// does not fit) is never framed: it is dropped, charged nothing by `inet_output_count` and kills
/// the port through `driver_failure_posix(EINVAL)` (`inet_drv.c:12657`-`:12667`, `io.c:7273`).
///
/// The framing therefore must not run before the strip: a header pushed first would have to be
/// consumed again by this arm. The failure is a **latch**, not a return value, so the round still
/// answers `Ok` (`erlang:port_command/2` answers `true`) and the teardown arrives through the
/// failure channel.
#[test]
fn a_malformed_command_is_never_framed() {
    let mut wired = Wired::new();
    wired.set_packet(TCP_PB_4);

    // One byte can never hold the 16-bit tag length word.
    let malformed = vec![0u8];
    wired
        .port
        .command(&malformed)
        .expect("`erlang:port_command/2` answers `true`: the callback returns normally");

    // `driver_failure_posix(EINVAL)` reaped the port after the round.
    assert!(
        !wired.port.contains(),
        "`driver_failure_posix(EINVAL)` tears the port down after the round"
    );
    let failures = wired.port.registry.take_failures();
    assert_eq!(failures.len(), 1, "exactly one failure was latched");
    assert_eq!(failures[0].port, wired.port.id, "the port that reported it");
    assert_eq!(
        failures[0].failure.reason.atom_name().as_deref(),
        Some("einval"),
        "the errno atom the oracle renders as `{{'EXIT',P,einval}}`"
    );
    // No extra-byte probe here: the reaped port's socket is closed, so the peer's next read is a
    // clean end-of-file — the same connection teardown the oracle performs on that path.
    assert_eq!(
        wired.read_from_closed_peer(),
        Vec::<u8>::new(),
        "the port's teardown closed the connection"
    );
}// -----------------------------------------------------------------------
// The oracle cross-check (skips loudly when `erl` is absent)
// -----------------------------------------------------------------------

/// The write-side framing of the oracle's `gen_tcp:send/2`, byte for byte, on `{packet,4}`,
/// `{packet,2}`, the empty `{packet,4}` body, `{packet,1}` with an over-width payload and a
/// `{packet,raw}` command.
///
/// The receiver is in `{packet, raw}` mode and asks for an exact byte count, so it sees precisely
/// what the sender put on the wire — which is the whole question this suite answers. The same
/// logical sends are then driven through this floor's `erlang:port_command/2` path and compared
/// with the oracle's bytes.
///
/// Measured on OTP 29.1.1 / erts-17.1: `[0,0,0,5,"hello"]`, `[0,5,"hello"]`, `[0,0,0,0]`,
/// `[44, 300 × "a"]` and `"xyz"`.
#[test]
fn the_write_side_framing_agrees_with_the_oracle() {
    let Some(stdout) = erl_stdout(
        "{ok,L}=gen_tcp:listen(0,[binary,{active,false},{packet,raw},{ip,{127,0,0,1}}]), \
         {ok,P}=inet:port(L), \
         {ok,C}=gen_tcp:connect({127,0,0,1},P,[binary,{active,false},{packet,raw}]), \
         {ok,S}=gen_tcp:accept(L), \
         Big=binary:copy(<<$a>>,300), \
         Rows=[{4,<<\"hello\">>,9},{2,<<\"hello\">>,7},{4,<<>>,4},{1,Big,301},{0,<<\"xyz\">>,3}], \
         Out=[begin \
                ok=inet:setopts(C,[{packet,N}]), \
                ok=gen_tcp:send(C,D), \
                {ok,B}=gen_tcp:recv(S,Len,5000), \
                binary_to_list(B) \
              end || {N,D,Len} <- Rows], \
         io:format(\"~w|\",[Out]), \
         gen_tcp:close(S), gen_tcp:close(C), gen_tcp:close(L)",
    ) else {
        return;
    };
    // `[[...],[...],...]|` — the five rows the oracle put on the wire.
    let Some(oracle) = parse_rows(&stdout) else {
        panic!("the oracle answered an unparsable dump: {stdout:?}");
    };
    assert_eq!(oracle.len(), 5, "one row per send: {stdout:?}");

    let payload = vec![b'a'; 300];
    let mut rust: Vec<Vec<u8>> = Vec::new();
    for (code, body, width) in [
        (TCP_PB_4, b"hello".to_vec(), Some(4usize)),
        (TCP_PB_2, b"hello".to_vec(), Some(2)),
        (TCP_PB_4, Vec::new(), Some(4)),
        (TCP_PB_1, payload, Some(1)),
        (TCP_PB_RAW, b"xyz".to_vec(), None),
    ] {
        let mut wired = Wired::new();
        wired.set_packet(code);
        wired.command(CALLER_REF_TAG, &body);
        let want = width.map_or_else(|| body.clone(), |w| {
            let mut framed = expected_header(w, body.len());
            framed.extend_from_slice(&body);
            framed
        });
        let got = wired.read_exactly(want.len());
        assert_eq!(got, want, "the framing this floor produces for code {code}");
        wired.assert_nothing_else();
        rust.push(got);
    }

    for (index, (oracle_row, rust_row)) in oracle.iter().zip(rust.iter()).enumerate() {
        assert_eq!(
            oracle_row, rust_row,
            "row #{index} disagrees with the oracle's write-side framing"
        );
        assert!(!oracle_row.is_empty(), "row #{index} is never empty");
    }
}

/// Parses `[[1,2],[3],[4]]|` into the rows it holds — the shape the oracle expression above prints.
fn parse_rows(dump: &str) -> Option<Vec<Vec<u8>>> {
    let body = dump.strip_suffix('|')?.trim();
    let inner = body.strip_prefix("[[")?.strip_suffix("]]")?;
    if inner.is_empty() {
        return Some(Vec::new());
    }
    let mut rows = Vec::new();
    for row in inner.split("],[") {
        let mut bytes = Vec::new();
        for item in row.split(',') {
            bytes.push(item.trim().parse::<u8>().ok()?);
        }
        rows.push(bytes);
    }
    Some(rows)
}
