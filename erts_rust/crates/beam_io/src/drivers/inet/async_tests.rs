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

//! The `tcp_inet` **asynchronous-operation queue**'s suite: the request/reply exchange of
//! `TCP_REQ_RECV`(42)/`INET_REQ_ACCEPT`(26) and the `{inet_async, Port, Id, Result}` /
//! `{inet_reply, Port, Reply[, Tag]}` completions they produce later.
//!
//! Every case drives the runtime's own `erlang:port_control/3`/`port_command/2` path through the
//! [`Rig`] the numeric-opcode suite defines (`port_control` takes the C opcode's decimal string, so
//! that is exactly what these tests send), and drains the completions with
//! [`PortRegistry::take_completions`](crate::PortRegistry::take_completions) — the same seam
//! `beam_rt` reads. What is pinned here:
//!
//! * the **id sequence** (`NEW_ASYNC_ID`, `inet_drv.c:1549`) — first id `0`, the counter is taken
//!   before the capacity check so a **refused** enqueue still consumes one, and the 16-bit field
//!   carries into its high byte at id 256;
//! * the two **queue disciplines**: the single receive slot (`INET_MAX_ASYNC`, `:1069`, a second
//!   `recv` is `[0]ealready`) versus the multi-operation accept list (`enq_multi_op`, `:12194`,
//!   never `ealready`), including the `INET_F_ACC`/`INET_F_MULTI_CLIENT` state bits
//!   `INET_REQ_GETSTATUS` reports;
//! * the full `TCP_REQ_RECV`/`INET_REQ_ACCEPT` **guard ladders**, in the C's order and with the
//!   C's replies (`[0]enotconn`/`[0]einval`/`[0]enomem`);
//! * the **separation of the two halves**: a control reply is the id and nothing else, the outcome
//!   always arrives through the completion channel, exactly once;
//! * the **data shapes** — a body-only [`ReadMessage`] for a raw read, "exactly `n` bytes" for a
//!   non-zero `Length`, the header-stripped payload under `{packet, 2}` and the
//!   delimiter-**keeping** body under `{packet, line}`;
//! * the **three completion production points**: an operation whose data has already arrived
//!   (the round's `drive_async`), a deadline (the port timer, armed from
//!   `Driver::next_timeout`), and a teardown (`{error, closed}` from `stop`/the peer's close);
//! * the **send path's caller tag**: `init_caller`'s `[n:be16][n ETF bytes]` prefix is stripped
//!   from the wire bytes and echoed **verbatim** in the `{inet_reply, …}` completion, while a
//!   zero-length tag stages no completion at all.
//!
//! The `udp_inet` half has its own suite (`udp/async_tests.rs`); it reuses the same queue
//! machinery and adds the datagram shapes.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::io::{IntoRawFd, RawFd};
use std::time::{Duration, Instant};

use super::super::opcode_tests::{bare_port, listening_port, op, opened_port, Rig};
use super::super::opt::{INET_INFINITY, INET_LOPT_ACTIVE, INET_LOPT_MODE, INET_LOPT_PACKET};
use super::super::wire::{ctl_ok, ctl_xerror};
use super::super::{TcpInetDriver, INET_ACTIVE, INET_PASSIVE};
use crate::completion::{CompletionEvent, InetAsyncResult, InetReason, InetReply, PortCompletion};
use crate::port_task::ReadMessage;
use beam_common::BeamError;

/// `INET_REQ_SETOPTS` (`inet_drv.c:805`-`:847`): the option-record request.
const INET_REQ_SETOPTS: u32 = 7;
/// `INET_REQ_GETOPTS`: the option read-back request.
const INET_REQ_GETOPTS: u32 = 8;
/// `INET_REQ_GETSTATUS`: the descriptor-state request that carries the accept bits.
const INET_REQ_GETSTATUS: u32 = 16;
/// `INET_REQ_ACCEPT` (`:12144`): the asynchronous (multi-operation) accept.
const INET_REQ_ACCEPT: u32 = 26;
/// `TCP_REQ_RECV` (`:12268`): the asynchronous receive.
const TCP_REQ_RECV: u32 = 42;

/// The C's `TCP_MAX_PACKET_SIZE` (`inet_drv.c:1308`) plus one: the smallest `Length` the driver
/// refuses with `[0]enomem`.
const LENGTH_TOO_LARGE: u32 = 0x0400_0001;

/// The `{packet, 2}` option value (`TCP_PB_2`), which frames both directions with a 2-byte header.
const PACKET_2: i32 = 2;
/// The `{packet, line}` option value (`TCP_PB_LINE_LF`, `:1300`).
const PACKET_LINE: i32 = 8;
/// The `{mode, binary}` option value (`INET_MODE_BINARY`, `inet_drv.c:779`).
const MODE_BINARY: i32 = 1;

/// The upper bound a condition wait may take before the test fails: a hang guard, never a
/// wall-clock budget tuned for an idle host.
const HANG_GUARD: Duration = Duration::from_secs(5);
/// How long one condition-wait iteration sleeps between rounds.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// The `Timeout(4), Length(4)` operand of `TCP_REQ_RECV` (`inet_drv.c:12293`).
fn recv_operand(timeout_ms: i32, length: u32) -> Vec<u8> {
    let mut arg = timeout_ms.to_be_bytes().to_vec();
    arg.extend_from_slice(&length.to_be_bytes());
    arg
}

/// The `Timeout(4)` operand of `INET_REQ_ACCEPT` (`inet_drv.c:12160`).
fn accept_operand(timeout_ms: i32) -> Vec<u8> {
    timeout_ms.to_be_bytes().to_vec()
}

/// One five-byte socket-option record, `[code, value_be32]` (`inet_set_opts`, `:6908`).
fn option_record(code: u8, value: i32) -> Vec<u8> {
    let mut record = vec![code];
    record.extend_from_slice(&value.to_be_bytes());
    record
}

/// Sets one socket option through `INET_REQ_SETOPTS`(7), asserting the `[1]` reply.
fn set_option(port: &mut Rig, code: u8, value: i32) {
    assert_eq!(
        port.control(&op(INET_REQ_SETOPTS), &option_record(code, value)),
        ctl_ok(),
        "INET_REQ_SETOPTS({code}) is accepted"
    );
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A `tcp_inet` port whose socket is already connected, **plus the far end**.
///
/// [`connection()`](super::super::opcode_tests::connection) builds the same pair, but it keeps its
/// peer private — and every send-path case here has to *read* what the driver wrote — so this
/// suite builds its own pair and hands the peer to the test. The driver adopts a duplicate of the
/// accepted socket (no `unsafe` needed to make a descriptor), exactly as the sibling suite does.
fn linked() -> (Rig, TcpStream) {
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
    (Rig::new(Box::new(driver)), peer)
}

/// Whether a raw descriptor is still open — `fcntl(fd, F_GETFD)` (the probe an accepted
/// descriptor's ownership is pinned with).
fn fd_is_open(fd: RawFd) -> bool {
    // SAFETY: `fcntl(2)` with `F_GETFD` only reads the descriptor's flags; it neither writes
    // through nor retains the number, so any integer is a legal argument.
    unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
}

/// Closes a raw descriptor this suite owns.
fn close_fd(fd: RawFd) {
    // SAFETY: `fd` came out of an `InetAsyncResult::Accepted` completion, which hands ownership to
    // the consumer and never stores or closes it — so it is closed exactly once, here.
    unsafe { libc::close(fd) };
}

// ---------------------------------------------------------------------------
// Completion helpers
// ---------------------------------------------------------------------------

/// The `(id, result)` pairs of every `{inet_async, …}` completion in `completions`.
fn async_events(completions: &[PortCompletion]) -> Vec<(u16, InetAsyncResult)> {
    completions
        .iter()
        .filter_map(|completion| match &completion.event {
            CompletionEvent::InetAsync { id, result } => Some((*id, result.clone())),
            CompletionEvent::InetReply { .. } => None,
        })
        .collect()
}

/// Rounds the port until at least `want` completions have been drained, returning the ones it
/// collected.
///
/// A completion is produced by the *socket*, not by the control call, so a test that connects a
/// peer or closes its end waits for the **condition** (bounded by [`HANG_GUARD`]) instead of
/// sleeping for a fixed budget: the loop rounds, drains and re-rounds until the events appear.
fn await_events(port: &mut Rig, want: usize) -> Vec<(u16, InetAsyncResult)> {
    let guard = Instant::now() + HANG_GUARD;
    let mut events = Vec::new();
    while events.len() < want {
        port.round().expect("a port-task round");
        events.extend(async_events(&port.completions()));
        if events.len() >= want {
            break;
        }
        assert!(
            Instant::now() < guard,
            "only {} of {want} completions arrived within {HANG_GUARD:?}: {events:?}",
            events.len()
        );
        std::thread::sleep(POLL_INTERVAL);
    }
    events
}

// ---------------------------------------------------------------------------
// The id counter (`NEW_ASYNC_ID`)
// ---------------------------------------------------------------------------

/// A fresh descriptor's first asynchronous operation gets id **0**, a second concurrent receive is
/// refused with `[0]ealready`, and the refused enqueue still **consumes** an id: the C takes the
/// id before any capacity check (`NEW_ASYNC_ID`, `inet_drv.c:1549`, then the `enq_async < 0` test
/// at `:12307`), so once the first operation has completed the next one is id **2**.
#[test]
fn the_first_id_is_zero_and_a_refused_enqueue_still_consumes_one() {
    let (mut port, mut peer) = linked();

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0],
        "the first asynchronous operation answers [1] ++ <<0:16/big>>"
    );
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        ctl_xerror("ealready"),
        "INET_MAX_ASYNC is one: the receive slot refuses a second operation"
    );

    // Free the slot, so the third call is accepted and shows where the counter stands.
    peer.write_all(b"x").expect("the peer writes one byte");
    let events = await_events(&mut port, 1);
    assert_eq!(events.len(), 1, "the first receive completed: {events:?}");
    assert_eq!(events[0].0, 0, "the completion echoes the id its reply handed out");

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 2],
        "the refused enqueue took id 1, so the third operation is id 2"
    );
}

/// The id counter is 16 bits wide and carries into its high byte: measured on a fresh listener the
/// sequence runs `[1,0,0]` … `[1,0,255]`, then `[1,1,0]` (id 256) — the C's `op_ref++ & 0xffff`
/// (`:1549`), a 65536 wrap rather than a 256 one. Accepts queue without any connection, so the
/// whole run needs no peer.
#[test]
fn the_id_counter_carries_into_the_high_byte_at_id_256() {
    let (mut port, _bound) = listening_port();

    for expected in 0..=u16::from(u8::MAX) {
        let mut want = ctl_ok();
        want.extend_from_slice(&expected.to_be_bytes());
        assert_eq!(
            port.control(&op(INET_REQ_ACCEPT), &accept_operand(INET_INFINITY)),
            want,
            "accept #{expected} answers [1] ++ <<{expected}:16/big>>"
        );
    }

    let mut want = ctl_ok();
    want.extend_from_slice(&256u16.to_be_bytes());
    assert_eq!(
        port.control(&op(INET_REQ_ACCEPT), &accept_operand(INET_INFINITY)),
        want,
        "the 257th operation is id 256: [1,0,255] -> [1,1,0]"
    );
    assert_eq!(want, vec![1, 1, 0], "the carry into the high byte");
}

// ---------------------------------------------------------------------------
// The multi-operation accept list
// ---------------------------------------------------------------------------

/// Accepts are a **list** (`enq_multi_op`, `inet_drv.c:12194`/`:12209`): N queued accepts each get
/// their own id and never `ealready`, `INET_REQ_GETSTATUS` reports the C's `INET_F_ACC`
/// (one pending) / `INET_F_MULTI_CLIENT` (two or more) bits while the list is non-empty, and the
/// completions arrive in **enqueue order** as `InetAsyncResult::Accepted`.
///
/// The accepted descriptors belong to the completion's consumer (the runtime), so the driver's
/// teardown must leave every one of them open.
#[test]
fn queued_accepts_each_get_their_own_id_and_the_state_bits_report_them() {
    let (mut port, bound) = listening_port();

    assert_eq!(
        port.control(&op(INET_REQ_ACCEPT), &accept_operand(INET_INFINITY)),
        vec![1, 0, 0],
        "the first accept is id 0"
    );
    assert_eq!(
        port.control(&op(INET_REQ_GETSTATUS), &[]),
        vec![1, 0, 0, 0, 41],
        "one pending accept is INET_STATE_ACCEPTING (INET_F_LISTEN | INET_F_ACC)"
    );

    assert_eq!(
        port.control(&op(INET_REQ_ACCEPT), &accept_operand(INET_INFINITY)),
        vec![1, 0, 1],
        "a second accept joins the list instead of answering ealready"
    );
    let mut expected = ctl_ok();
    expected.extend_from_slice(&297u32.to_be_bytes());
    assert_eq!(
        port.control(&op(INET_REQ_GETSTATUS), &[]),
        expected,
        "two pending accepts add INET_F_MULTI_CLIENT (9 | 0x120 = 297)"
    );

    // Two real connections, taken off the listener during one round.
    let peers: Vec<TcpStream> = (0..2)
        .map(|_| TcpStream::connect(("127.0.0.1", bound)).expect("a loopback connection"))
        .collect();
    let events = await_events(&mut port, 2);
    assert_eq!(events.len(), 2, "both queued accepts complete: {events:?}");
    assert_eq!(events[0].0, 0, "the completions arrive in enqueue order");
    assert_eq!(events[1].0, 1);

    let mut accepted = Vec::new();
    for (id, result) in &events {
        let InetAsyncResult::Accepted { fd } = result else {
            panic!("accept {id} completed with {result:?}");
        };
        assert!(fd_is_open(*fd), "accept {id} handed over a live descriptor");
        accepted.push(*fd);
    }
    assert_ne!(
        accepted[0], accepted[1],
        "two connections are two descriptors"
    );

    port.close();
    assert!(!port.contains(), "the port is gone");
    for fd in &accepted {
        assert!(
            fd_is_open(*fd),
            "the driver never stores an accepted descriptor, so its teardown cannot close it"
        );
        close_fd(*fd);
    }
    drop(peers);
}

// ---------------------------------------------------------------------------
// The guard ladders
// ---------------------------------------------------------------------------

/// `TCP_REQ_RECV`(42) walks the C's guard ladder in the C's order (`inet_drv.c:12265`-`:12325`):
/// the connection guard first (so a wrong-length operand on a port with no peer is `enotconn`, not
/// `einval`), then `active` (an active-mode socket has no passive receive), then the operand
/// length, then the "exactly `n` bytes only for the raw header type" rule, and finally the
/// `TCP_MAX_PACKET_SIZE` cap.
#[test]
fn a_receive_walks_the_c_guard_ladder_in_order() {
    // The connection guard precedes the operand-length guard.
    let mut bare = bare_port();
    assert_eq!(
        bare.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        ctl_xerror("enotconn"),
        "there is nothing to receive from without a connection"
    );
    assert_eq!(
        bare.control(&op(TCP_REQ_RECV), &[1, 2]),
        ctl_xerror("enotconn"),
        "the connection guard comes first, so the short operand is not the reason"
    );

    let (mut port, _peer) = linked();

    // `active` mode: the very operand that is legal below is refused while the socket is active.
    set_option(&mut port, INET_LOPT_ACTIVE, INET_ACTIVE);
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        ctl_xerror("einval"),
        "an active-mode socket has no passive receive (`:12290`)"
    );
    set_option(&mut port, INET_LOPT_ACTIVE, INET_PASSIVE);

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)[..7]),
        ctl_xerror("einval"),
        "the operand must be exactly eight bytes"
    );

    // "exactly n bytes" exists only for the raw header type (`:12296`).
    set_option(&mut port, INET_LOPT_PACKET, PACKET_2);
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 4)),
        ctl_xerror("einval"),
        "a non-zero Length under a framing {{packet, N}} is EINVAL"
    );
    set_option(&mut port, INET_LOPT_PACKET, 0);

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, LENGTH_TOO_LARGE)),
        ctl_xerror("enomem"),
        "Length > TCP_MAX_PACKET_SIZE is ENOMEM (`:12301`)"
    );

    // Every refusal above was taken before the id, so the first accepted operation is still id 0.
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 4)),
        vec![1, 0, 0],
        "the same descriptor accepts exactly four bytes in raw mode"
    );
}

/// `INET_REQ_ACCEPT`(26) has one combined guard — the socket must be listening **and** the operand
/// must be the four-byte `Timeout` — and both faults answer the C's `[0]einval`
/// (`inet_drv.c:12160`-`:12170`).
#[test]
fn an_accept_validates_its_operand_and_the_listening_state() {
    let (mut listening, _bound) = listening_port();
    assert_eq!(
        listening.control(&op(INET_REQ_ACCEPT), &[]),
        ctl_xerror("einval"),
        "the C's `if (len != 4) ctl_error(EINVAL)`"
    );
    assert_eq!(
        listening.control(&op(INET_REQ_ACCEPT), &[0, 0, 0, 0, 0]),
        ctl_xerror("einval"),
        "a five-byte operand is not the four-byte Timeout"
    );

    let mut opened = opened_port();
    assert_eq!(
        opened.control(&op(INET_REQ_ACCEPT), &accept_operand(INET_INFINITY)),
        ctl_xerror("einval"),
        "there is nothing to accept from a socket that is not listening"
    );

    assert_eq!(
        listening.control(&op(INET_REQ_ACCEPT), &accept_operand(INET_INFINITY)),
        vec![1, 0, 0],
        "a well-formed operand on a listener really queues, and no refusal consumed an id"
    );
}

// ---------------------------------------------------------------------------
// The two halves of an exchange
// ---------------------------------------------------------------------------

/// The control reply carries the **id and nothing else**: the completion is staged later, by the
/// socket, and is drained exactly once.
#[test]
fn the_control_reply_never_carries_the_completion() {
    let (mut port, mut peer) = linked();

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0],
        "the reply is the id"
    );
    assert!(
        port.completions().is_empty(),
        "and the data is not smuggled through it"
    );
    assert!(
        port.next_timeout().is_none(),
        "an infinite timeout arms no timer on the port"
    );

    peer.write_all(b"abc").expect("the peer writes");

    port.round().expect("one round");
    assert_eq!(
        async_events(&port.completions()),
        vec![(
            0,
            InetAsyncResult::Data(ReadMessage::body_only(b"abc".to_vec()))
        )],
        "the round's async drive point completes the operation"
    );
    port.round().expect("a second round");
    assert!(
        port.completions().is_empty(),
        "a completion is drained once, never re-reported"
    );
}

// ---------------------------------------------------------------------------
// Data shapes
// ---------------------------------------------------------------------------

/// A raw read is delivered as a body-only [`ReadMessage`]: no header, and a present body — the
/// `driver_output` shape the runtime renders as `{ok, Binary}` (`inet_async_data`,
/// `inet_drv.c:3162`).
#[test]
fn a_receive_delivers_the_raw_byte_stream_as_a_body_only_read() {
    let (mut port, mut peer) = linked();
    peer.write_all(b"abcdef").expect("the peer writes");

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0]
    );
    let events = await_events(&mut port, 1);
    let (id, result) = &events[0];
    assert_eq!(*id, 0, "the completion echoes the reply's id");
    let InetAsyncResult::Data(read) = result else {
        panic!("the receive completed with {result:?}");
    };
    assert!(read.header.is_empty(), "a raw read has no header split");
    assert_eq!(read.body.as_deref(), Some(&b"abcdef"[..]));
}

/// A non-zero `Length` means "exactly this many bytes" (raw header type only), and the bytes left
/// in the driver's buffer serve the **next** operation.
#[test]
fn a_receive_with_a_length_takes_exactly_that_many_bytes() {
    let (mut port, mut peer) = linked();
    peer.write_all(b"abcdefgh").expect("the peer writes");

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 4)),
        vec![1, 0, 0]
    );
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(
            0,
            InetAsyncResult::Data(ReadMessage::body_only(b"abcd".to_vec()))
        )],
        "exactly four bytes, never the whole buffer"
    );

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 1]
    );
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(
            1,
            InetAsyncResult::Data(ReadMessage::body_only(b"efgh".to_vec()))
        )],
        "the remainder was retained, not dropped"
    );
}

/// Under `{packet, 2}` the framing header is **stripped**: the completion carries the payload only
/// (`tcp_remain`/`tcp_reply_data`, `inet_drv.c:12967`/`:4279`).
#[test]
fn a_packet_framed_receive_delivers_the_payload_without_its_header() {
    let (mut port, mut peer) = linked();
    set_option(&mut port, INET_LOPT_PACKET, PACKET_2);
    peer.write_all(&[0, 5, b'h', b'e', b'l', b'l', b'o'])
        .expect("the peer writes one {packet,2} frame");

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0]
    );
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(
            0,
            InetAsyncResult::Data(ReadMessage::body_only(b"hello".to_vec()))
        )],
        "the two-byte length header is the frame's, not the payload's"
    );
}

/// Under `{packet, line}` the delimiter **stays** in the body — the C's `packet_get_body` returns
/// the line "as is", exactly as the oracle's `gen_tcp:recv` reports it.
#[test]
fn a_line_framed_receive_keeps_its_delimiter() {
    let (mut port, mut peer) = linked();
    set_option(&mut port, INET_LOPT_PACKET, PACKET_LINE);
    peer.write_all(b"hello\n").expect("the peer writes one line");

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0]
    );
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(
            0,
            InetAsyncResult::Data(ReadMessage::body_only(b"hello\n".to_vec()))
        )],
        "a line frame keeps its delimiter"
    );
}

/// `{mode, list|binary}` is **driver state**: it is stored and reported byte-exactly, and which
/// term the owner builds from a read (`{ok, Binary}` versus `{ok, IntList}`) is the runtime's to
/// render — the driver carries the same [`ReadMessage`] either way.
#[test]
fn the_mode_option_is_stored_and_left_to_the_runtime_to_render() {
    let (mut port, mut peer) = linked();
    set_option(&mut port, INET_LOPT_MODE, MODE_BINARY);

    let mut expected = ctl_ok();
    expected.push(INET_LOPT_MODE);
    expected.extend_from_slice(&MODE_BINARY.to_be_bytes());
    assert_eq!(
        port.control(&op(INET_REQ_GETOPTS), &[INET_LOPT_MODE]),
        expected,
        "{{mode, binary}} reads back as [1, 24, 0, 0, 0, 1]"
    );

    peer.write_all(b"bin").expect("the peer writes");
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0]
    );
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(
            0,
            InetAsyncResult::Data(ReadMessage::body_only(b"bin".to_vec()))
        )],
        "the bytes cross the channel; the list/binary rendering is the runtime's"
    );
}

// ---------------------------------------------------------------------------
// The deadline (the port timer)
// ---------------------------------------------------------------------------

/// A `Timeout(4)` operand arms the port's `TIMEOUT` task from the operation's deadline
/// (`Driver::next_timeout` + `Driver::timeout`, the C's `driver_set_timer` and
/// `tcp_inet_recv_timeout`, `inet_drv.c:12480`). The reply is still the id; the `{error, timeout}`
/// arrives through the completion channel once the deadline has passed — exactly once.
#[test]
fn a_receive_timeout_completes_after_its_reply() {
    let (mut port, _peer) = linked();

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(50, 0)),
        vec![1, 0, 0],
        "the control reply is the id, never the outcome"
    );
    assert!(
        port.completions().is_empty(),
        "the deadline has not been reported yet"
    );
    assert!(
        port.next_timeout().is_some(),
        "a pending operation declares its deadline to the port"
    );

    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(0, InetAsyncResult::Error(InetReason::Timeout))],
        "the timer reports {{error, timeout}} for the operation's own id"
    );
    assert!(
        port.next_timeout().is_none(),
        "an empty queue declares no deadline, so the port clears its TIMEOUT task"
    );
    port.round().expect("a second round");
    assert!(
        port.completions().is_empty(),
        "the timeout is reported exactly once"
    );
}

/// A **zero** timeout is a deadline of "now": the reply is unchanged and the operation fails at the
/// next round — never inside the control call.
#[test]
fn a_zero_timeout_is_due_at_the_next_round() {
    let (mut port, _peer) = linked();
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(0, 0)),
        vec![1, 0, 0],
        "a zero deadline does not change the reply"
    );
    assert!(
        port.completions().is_empty(),
        "and does not report from the control call either"
    );

    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(0, InetAsyncResult::Error(InetReason::Timeout))]
    );
}

/// The C attempts the receive at enqueue time, which is why a zero timeout sees data that has
/// **already arrived** instead of reporting a timeout (`tcp_recv` runs before the timer can fire).
#[test]
fn a_zero_timeout_still_delivers_data_that_already_arrived() {
    let (mut port, mut peer) = linked();
    peer.write_all(b"hi").expect("the peer writes");

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(0, 0)),
        vec![1, 0, 0]
    );
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(
            0,
            InetAsyncResult::Data(ReadMessage::body_only(b"hi".to_vec()))
        )],
        "the buffered bytes win over the deadline"
    );
}

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

/// A peer's close with a receive pending completes it with `{error, closed}` — the C's
/// `tcp_recv_closed` → `async_error_am(am_closed)` (`inet_drv.c:13180`) — exactly once.
#[test]
fn a_peer_close_completes_a_pending_receive_once() {
    let (mut port, peer) = linked();
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0]
    );

    drop(peer);
    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(0, InetAsyncResult::Error(InetReason::Closed))],
        "the operation ends as {{error, closed}}"
    );
    port.round().expect("a second round");
    assert!(
        port.completions().is_empty(),
        "the close is reported exactly once"
    );
}

/// Closing a listener drains its whole accept list as `{error, closed}` in enqueue order — the C's
/// `async_error_am_all(am_closed)` (`inet_drv.c:2597`-`:2608`) — and the registry's close path
/// hands those completions over exactly once.
#[test]
fn closing_a_listener_completes_every_queued_accept_once() {
    let (mut port, _bound) = listening_port();
    assert_eq!(
        port.control(&op(INET_REQ_ACCEPT), &accept_operand(INET_INFINITY)),
        vec![1, 0, 0]
    );
    assert_eq!(
        port.control(&op(INET_REQ_ACCEPT), &accept_operand(INET_INFINITY)),
        vec![1, 0, 1]
    );

    port.close();
    assert!(!port.contains(), "the port is gone");
    assert_eq!(
        async_events(&port.completions()),
        vec![
            (0, InetAsyncResult::Error(InetReason::Closed)),
            (1, InetAsyncResult::Error(InetReason::Closed)),
        ],
        "every queued accept is answered, in enqueue order"
    );
    assert!(
        port.completions().is_empty(),
        "the teardown completions are drained once"
    );
}

/// The same teardown rule for a connection: `Driver::stop` drains the receive slot before the
/// descriptor goes away.
#[test]
fn closing_a_connection_completes_a_pending_receive_once() {
    let (mut port, _peer) = linked();
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0]
    );

    port.close();
    assert_eq!(
        async_events(&port.completions()),
        vec![(0, InetAsyncResult::Error(InetReason::Closed))]
    );
    assert!(port.completions().is_empty(), "drained once");
}

// ---------------------------------------------------------------------------
// The accepted descriptor's ownership
// ---------------------------------------------------------------------------

/// An `accept` completion hands the connection's descriptor to its **consumer**: the driver never
/// stores it, so the port's own teardown cannot close it (the runtime either adopts it or closes
/// it — `InetAsyncResult::Accepted`'s contract).
#[test]
fn an_accepted_descriptor_is_handed_over_owned() {
    let (mut port, bound) = listening_port();
    assert_eq!(
        port.control(&op(INET_REQ_ACCEPT), &accept_operand(INET_INFINITY)),
        vec![1, 0, 0]
    );

    let peer = TcpStream::connect(("127.0.0.1", bound)).expect("a loopback connection");
    let events = await_events(&mut port, 1);
    let InetAsyncResult::Accepted { fd } = &events[0].1 else {
        panic!("the accept completed with {:?}", events[0].1);
    };
    assert!(fd_is_open(*fd), "a live connection descriptor");

    port.close();
    assert!(
        fd_is_open(*fd),
        "the descriptor outlives the port: nothing here remembers it"
    );
    close_fd(*fd);
    drop(peer);
}

/// A failed accept carries **no** descriptor — nothing was taken off the listener, so there is
/// nothing for the owner to adopt or close.
#[test]
fn an_accept_timeout_carries_no_descriptor() {
    let (mut port, _bound) = listening_port();
    assert_eq!(
        port.control(&op(INET_REQ_ACCEPT), &accept_operand(0)),
        vec![1, 0, 0],
        "a zero deadline still answers the id"
    );
    assert!(port.completions().is_empty(), "never inside the reply");

    let events = await_events(&mut port, 1);
    assert_eq!(
        events,
        vec![(0, InetAsyncResult::Error(InetReason::Timeout))],
        "the timeout reports the reason and no port"
    );
    port.round().expect("a second round");
    assert!(port.completions().is_empty(), "reported exactly once");
}

// ---------------------------------------------------------------------------
// The send path's caller tag (`init_caller` / `inet_reply_finish`)
// ---------------------------------------------------------------------------

/// The golden `prim_inet` command prefix: `[n:be16][n bytes of `term_to_binary(Mref,[local])`]`
/// (`prim_inet.erl`'s `send/3`), i.e. a 2-byte length of 26 followed by the 26-byte ETF image of
/// the caller's monitor reference.
const GOLDEN_CALLER_PREFIX: [u8; 28] = [
    0, 26, 131, 121, 254, 153, 216, 200, 90, 0, 3, 106, 0, 0, 0, 0, 0, 0, 11, 218, 194, 188, 0, 11,
    244, 41, 135, 150,
];

/// A command whose payload starts with the `init_caller` prefix has that prefix **stripped** from
/// the bytes that reach the socket and echoed **verbatim** in one `{inet_reply, Port, ok, Tag}`
/// completion — the C's `inet_reply_finish` (`inet_drv.c:2614`-`:2637`), which is what
/// `prim_inet:send/3` blocks on.
#[test]
fn a_command_answers_its_caller_tag_verbatim() {
    let (mut port, mut peer) = linked();
    let mut command = GOLDEN_CALLER_PREFIX.to_vec();
    command.extend_from_slice(b"hello");

    port.command(&command).expect("the command is accepted");

    let completions = port.completions();
    assert_eq!(completions.len(), 1, "one command, one reply: {completions:?}");
    match &completions[0].event {
        CompletionEvent::InetReply { caller_ref, reply } => {
            assert_eq!(reply, &InetReply::Ok, "the write drained the queue");
            let tag = caller_ref
                .as_deref()
                .expect("the command carried a caller tag");
            assert_eq!(
                tag,
                &GOLDEN_CALLER_PREFIX[2..],
                "the tag crosses verbatim: the prefix minus its two-byte length"
            );
            assert_eq!(tag.len(), 26, "the ETF image's own length");
        }
        other => panic!("expected an {{inet_reply, …}} completion, got {other:?}"),
    }

    // The peer sees the payload only — the prefix never reaches the wire.
    peer.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("a read timeout on the far end");
    let mut payload = [0u8; 5];
    peer.read_exact(&mut payload)
        .expect("the payload reaches the peer");
    assert_eq!(&payload, b"hello", "the 28 prefix bytes were stripped");
    let err = peer
        .read(&mut [0u8; 1])
        .expect_err("nothing else was written");
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "the socket carries no trailing bytes: {err:?}"
    );
}

/// A `[0,0]`-prefixed command carries a **zero-length** tag: `init_caller`
/// (`inet_drv.c:1854`-`:1881`) still consumes the two length bytes — the tag is simply empty — so
/// the payload reaches the peer unchanged while `inet_reply_finish` stages **no**
/// `{inet_reply, …}` for it (`:2628`). Measured on the oracle: `port_command(P, <<0:16,"raw">>)`
/// delivers exactly `"raw"`, and the caller's mailbox stays empty.
#[test]
fn a_zero_length_caller_tag_is_consumed_and_stages_no_reply() {
    let (mut port, mut peer) = linked();
    let mut command = vec![0, 0];
    command.extend_from_slice(b"raw");

    port.command(&command).expect("the command is accepted");
    assert!(
        port.completions().is_empty(),
        "an empty tag has no caller to answer"
    );
    assert!(port.contains(), "and the port is untouched");

    peer.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("a read timeout on the far end");
    let mut payload = [0u8; 3];
    peer.read_exact(&mut payload)
        .expect("the payload reaches the peer");
    assert_eq!(
        &payload,
        b"raw",
        "the two length bytes are consumed, the payload is not"
    );
    let err = peer
        .read(&mut [0u8; 1])
        .expect_err("nothing else was written");
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "the socket carries no trailing bytes: {err:?}"
    );
}
/// The **registry** half of the socket-less kill row: `init_caller`'s `FALSE` (`tcp_inet_command`
/// `inet_drv.c:12634`, `tcp_inet_commandv` `:12667`) is only a **request**
/// (`driver_failure_posix`, `io.c:7273`) — the port is reaped after the round, so
/// `erlang:port_command/2` still answers `true` and every later command on the number is the
/// oracle's `badarg`.
///
/// The driver-level half (the charged bytes, the latched `posix(einval)`, nothing written) is
/// [`a_socket_less_malformed_command_closes_the_port_with_einval`](super::tests); the oracle
/// sequence is [`the_socket_less_command_sequence_matches_the_erl_oracle`](super::tests).
#[test]
fn a_socket_less_command_closes_the_port() {
    let mut port = bare_port();
    port.command(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        .expect("the C accepts the command and charges it: `port_command/2` answers `true`");
    assert!(
        !port.contains(),
        "`driver_failure_posix(EINVAL)` is reaped into the owner's `{{'EXIT',P,einval}}`"
    );
    assert!(
        port.completions().is_empty(),
        "a malformed command has no caller reference to answer"
    );
    assert!(
        matches!(port.command(&[1]), Err(BeamError::BadArg)),
        "every later command on the dead port is the oracle's `badarg`"
    );
    assert!(
        matches!(
            port.try_control(&op(INET_REQ_GETSTATUS), &[]),
            Err(BeamError::BadArg)
        ),
        "and so is every later control call"
    );
}
