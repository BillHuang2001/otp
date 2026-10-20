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
//! The **read path's ordering rule**: a complete buffered unit **wins over** a pending close.
//!
//! `tcp_recv` (`inet_drv.c:13130`) classifies what the drain collected **before** it acts on the
//! end-of-file, and its delivery points (`:13216`-`:13217`, `:13229`-`:13230`) run first: a
//! `{packet, N}` frame that completed in the same call as the peer's end-of-file reaches the owner,
//! and only a **later** call's `n == 0` goes to `tcp_recv_closed` (`:13206`-`:13208`). The same
//! ordering holds on the enqueue side, where the read comes first and the arming second
//! (`:12309`, then `driver_select` at `:12315`), so a payload already in hand is never starved by
//! the close that arrived with it.
//!
//! What is pinned here, all of it on a peer that writes and **then** closes:
//!
//! * a queued `TCP_REQ_RECV`(42) answers `{inet_async, Port, Id, {ok, Data}}` with the peer's
//!   bytes — **not** `{error, closed}` — and the close is owed to the operations that follow
//!   (`{error, closed}` for one queued receive, then the `closed`/`enotconn` ladders of
//!   [`PeerClose`](super::PeerClose));
//! * the same on a socket adopted from a numeric `INET_REQ_ACCEPT`(26) completion through
//!   `TcpInetDriver::from_accepted` — the C's `tcp_inet_copy` (`inet_drv.c:11795`);
//! * an **inherited** `{packet, 2}` complete record survives the close latch: `tcp_clear_input`
//!   (`:11705`-`:11715`, reached from `tcp_recv_closed` at `:12823`) frees only the *partial*
//!   framing buffer, never a frame the same call already decoded;
//! * with **no** operation queued, the port reports `{Port, {data, …}}` for the bytes and the
//!   peer's close separately (the latch's `port_closed()` observation), each exactly once, because
//!   `read_available` queues the bytes it read **before** it latches;
//! * and the system `erl` agrees: a `gen_tcp:recv/3` on a write-then-close burst answers
//!   `{ok, Data}`, then `{error, closed}`.
//!
//! The pre-fix code latched the close from `read_bytes` itself, which ran
//! `drain_error_all` (`async.rs`) over the queue **before** the payload was delivered, so row 1
//! observed `{error, closed}` and lost the peer's bytes.
use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use super::op_queue::async_tests::{await_events, linked, recv_operand};
use super::opcode_tests::{escript_stdout, listening_port, op, Rig};
use super::opt::{INET_INFINITY, INET_LOPT_PACKET};
use super::wire::{ctl_ok, ctl_xerror};
use super::TcpInetDriver;
use crate::completion::{CompletionEvent, InetAsyncResult};
use crate::port::DEFAULT_READINESS_RED_BUDGET;
use crate::port_task::PortNotification;
use beam_process::PortId;

/// The `{packet, 2}` option value (`TCP_PB_2`, `inet_drv.c:1300`): a 2-byte big-endian length
/// header.
const PACKET_2: i32 = 2;
/// `INET_REQ_ACCEPT` (`inet_drv.c:12144`): the asynchronous accept.
const INET_REQ_ACCEPT: u32 = 26;
/// `TCP_REQ_RECV` (`:12268`): the asynchronous receive.
const TCP_REQ_RECV: u32 = 42;

/// The upper bound a condition wait may take before the test fails — a hang guard, never a
/// wall-clock budget tuned for an idle host.
const HANG_GUARD: Duration = Duration::from_secs(10);
/// How long one condition-wait iteration waits / sleeps.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

// ---------------------------------------------------------------------------
// Wire helpers (the `INET_REQ_SETOPTS`/`TCP_REQ_RECV` operands)
// ---------------------------------------------------------------------------

/// One five-byte socket-option record, `[code, value_be32]` (`inet_set_opts`, `:6908`).
fn option_record(code: u8, value: i32) -> Vec<u8> {
    let mut record = vec![code];
    record.extend_from_slice(&value.to_be_bytes());
    record
}

/// Sets one socket option through `INET_REQ_SETOPTS`(7), asserting the `[1]` reply.
fn set_option(port: &mut Rig, code: u8, value: i32) {
    assert_eq!(
        port.control(&op(7), &option_record(code, value)),
        ctl_ok(),
        "INET_REQ_SETOPTS({code}) is accepted"
    );
}

// ---------------------------------------------------------------------------
// Completion helpers (a port that is not the `Rig`'s own)
// ---------------------------------------------------------------------------

/// The `(id, result)` pairs of every `{inet_async, …}` completion the registry has staged.
fn async_events(rig: &mut Rig) -> Vec<(u16, InetAsyncResult)> {
    rig.registry
        .take_completions()
        .into_iter()
        .filter_map(|completion| match completion.event {
            CompletionEvent::InetAsync { id, result } => Some((id, result)),
            CompletionEvent::InetReply { .. } => None,
        })
        .collect()
}

/// Rounds **every** port until at least `want` completions have been drained, returning them.
///
/// The sibling suites' [`await_events`] rounds the `Rig`'s own port only; rows 2 and 3 drive a
/// **second** port (the accepted socket), so this variant rounds the whole registry and drains
/// the shared completion channel.
fn await_any_events(rig: &mut Rig, want: usize) -> Vec<(u16, InetAsyncResult)> {
    let guard = Instant::now() + HANG_GUARD;
    let mut events = Vec::new();
    while events.len() < want {
        rig.registry
            .execute_port_tasks(&mut rig.poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("a port-task round");
        events.extend(async_events(rig));
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

/// Drives the reactor until the port reports at least one notification, returning every
/// notification the rounds produced.
///
/// `ready_input` is the only path that reads a socket with **no** operation queued, and it is
/// reached through the poller: `handle_events` schedules the readiness task and runs it, then
/// `execute_port_tasks` harvests the port's input into [`PortNotification`]s.
fn reactor_notifications(rig: &mut Rig) -> Vec<PortNotification> {
    let guard = Instant::now() + HANG_GUARD;
    let mut notifications = Vec::new();
    while notifications.is_empty() {
        let events = rig.poller.poll(Some(POLL_INTERVAL)).expect("the poller polls");
        rig.registry
            .handle_events(&mut rig.poller, &events)
            .expect("a reactor round");
        notifications.extend(
            rig.registry
                .execute_port_tasks(&mut rig.poller, DEFAULT_READINESS_RED_BUDGET)
                .expect("a port-task round")
                .notifications,
        );
        assert!(
            Instant::now() < guard,
            "no notification arrived within {HANG_GUARD:?}"
        );
    }
    notifications
}

// ---------------------------------------------------------------------------
// The accept route (`INET_REQ_ACCEPT` + `tcp_inet_copy`)
// ---------------------------------------------------------------------------

/// Accepts the pending connection on `rig`'s listener and opens a **second** port over it with
/// `TcpInetDriver::from_accepted` (the C's `tcp_inet_copy`, `inet_drv.c:11795`), so the accepted
/// driver inherits the listener's `{packet, …}` and the rest of `tcp_inet_copy`'s field set.
fn adopt_accepted(rig: &mut Rig) -> PortId {
    let guard = Instant::now() + HANG_GUARD;
    let fd: RawFd = loop {
        rig.registry
            .execute_port_tasks(&mut rig.poller, DEFAULT_READINESS_RED_BUDGET)
            .expect("a port-task round");
        let accepted = rig.registry.take_completions().into_iter().find_map(
            |completion| match completion.event {
                CompletionEvent::InetAsync {
                    result: InetAsyncResult::Accepted { fd },
                    ..
                } => Some(fd),
                _ => None,
            },
        );
        if let Some(fd) = accepted {
            break fd;
        }
        assert!(
            Instant::now() < guard,
            "the listener never reported an accepted socket within {HANG_GUARD:?}"
        );
        std::thread::sleep(POLL_INTERVAL);
    };
    // The downcast borrow ends with the block, so the child can be opened below.
    let child = {
        let listener = rig
            .registry
            .driver_as::<TcpInetDriver>(rig.id)
            .expect("the listener's own driver");
        TcpInetDriver::from_accepted(fd, listener).expect("the accepted socket adopts")
    };
    assert!(
        child.state().is_connected(),
        "the adopted connection has a peer"
    );
    rig.registry
        .open(&mut rig.poller, Box::new(child))
        .expect("the accepted port opens")
}

/// A listener holding a reverse connection whose far end has already written `payload` and closed.
///
/// Returns the `Rig` (its own port is the listener), the port over the accepted socket, and the
/// peer — held so the descriptor is not torn down under the test.
fn accepted_after_write_then_close(packet: Option<i32>) -> (Rig, PortId, TcpStream) {
    let (mut listener, bound) = listening_port();
    if let Some(value) = packet {
        set_option(&mut listener, INET_LOPT_PACKET, value);
    }
    assert_eq!(
        listener.control(&op(INET_REQ_ACCEPT), &INET_INFINITY.to_be_bytes()),
        vec![1, 0, 0],
        "the accept is queued while no connection exists"
    );

    // The peer writes and closes **before** the accept is serviced: the connection (with its
    // payload and its FIN) is in the listener's backlog, never read by the driver.
    let mut peer = TcpStream::connect(("127.0.0.1", bound)).expect("the peer connects");
    let wire: Vec<u8> = match packet {
        Some(PACKET_2) => {
            let mut framed = vec![0, 3];
            framed.extend_from_slice(b"abc");
            framed
        }
        _ => b"abc".to_vec(),
    };
    peer.write_all(&wire).expect("the peer writes its bytes");
    peer.shutdown(Shutdown::Write)
        .expect("the peer closes its write side");

    let child = adopt_accepted(&mut listener);
    (listener, child, peer)
}

// ---------------------------------------------------------------------------
// Row 1 — a queued receive on an already-connected socket
// ---------------------------------------------------------------------------

/// A queued `TCP_REQ_RECV`(42) is answered with the peer's bytes even though the **same** drain
/// also saw the peer's end of file — `tcp_recv` delivers what it collected (`inet_drv.c:13216`-
/// `:13217`, `:13229`-`:13230`) and only a later call's `n == 0` reaches `tcp_recv_closed`
/// (`:13206`-`:13208`).
///
/// The close the drain observed is deferred to the operation's success (never
/// `drain_error_all`'s `{error, closed}` for the payload's own receive), and the following
/// reads/writes walk the C's `PeerClose` ladders: the next read answers `closed`, the next write
/// `closed` (the clean end-of-file armed the send one-shot, `:12816`), then both answer
/// `enotconn`.
#[test]
fn a_queued_receive_delivers_the_peers_payload_before_its_close() {
    let (mut port, mut peer) = linked();

    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &recv_operand(INET_INFINITY, 0)),
        vec![1, 0, 0],
        "the receive is queued and answers [1] ++ <<0:16/big>>"
    );

    // The peer writes its payload and closes in one burst, before any round has read the socket.
    peer.write_all(b"abc").expect("the peer writes its payload");
    peer.shutdown(Shutdown::Write)
        .expect("the peer closes its write side");

    let events = await_events(&mut port, 1);
    assert_eq!(events.len(), 1, "exactly one completion: {events:?}");
    assert_eq!(events[0].0, 0, "the completion echoes its own id");
    match &events[0].1 {
        InetAsyncResult::Data(read) => assert_eq!(
            read.flattened(),
            b"abc".to_vec(),
            "the park's payload wins over the close the same read observed"
        ),
        other => panic!("the queued receive lost its payload and answered {other:?}"),
    }

    // The close is now known, and the *next* read is the one that reports it: the named `recv`
    // verb's `read(2)` sees `n == 0` and answers `[0]closed` (`tcp_recv_closed`).
    assert_eq!(
        port.control("recv", &[]),
        ctl_xerror("closed"),
        "the next read answers closed"
    );
    // The clean end-of-file armed the **send** one-shot (`inet_drv.c:12816`), so the first write
    // after the close answers `closed` and every later one `enotconn`.
    assert_eq!(
        port.control("send", b"x"),
        ctl_xerror("closed"),
        "the first write after the peer's close answers closed"
    );
    assert_eq!(
        port.control("send", b"x"),
        ctl_xerror("enotconn"),
        "and the next one enotconn"
    );
    assert_eq!(
        port.control("recv", &[]),
        ctl_xerror("enotconn"),
        "the receive ladder is exhausted too"
    );
}

// ---------------------------------------------------------------------------
// Row 2 — the same burst on a socket adopted from an accept completion
// ---------------------------------------------------------------------------

/// The **accept** variant: the peer connects, writes and closes before the accept is serviced, and
/// the socket adopted from that accept (`tcp_inet_copy`, `inet_drv.c:11795`) still delivers the
/// payload to its queued receive.
///
/// The C's `tcp_inet_copy` is what carries the listener's option block onto the new descriptor;
/// the read path's ordering is the same one row 1 pins, on a connection whose very first read
/// observes both the bytes and the FIN.
#[test]
fn an_accepted_socket_delivers_a_write_then_close_burst() {
    let (mut rig, child, _peer) = accepted_after_write_then_close(None);

    assert_eq!(
        rig.registry
            .port_control(
                &mut rig.poller,
                child,
                &op(TCP_REQ_RECV),
                &recv_operand(INET_INFINITY, 0),
            )
            .expect("the accepted port queues a receive"),
        vec![1, 0, 0],
        "the accepted socket's first asynchronous operation is id 0"
    );

    let events = await_any_events(&mut rig, 1);
    assert_eq!(events.len(), 1, "exactly one completion: {events:?}");
    match &events[0].1 {
        InetAsyncResult::Data(read) => assert_eq!(
            read.flattened(),
            b"abc".to_vec(),
            "the accepted socket delivers the burst's payload"
        ),
        other => panic!("the accepted socket lost the burst and answered {other:?}"),
    }

    // A second receive is answered from the deferred close, not from the socket.
    assert_eq!(
        rig.registry
            .port_control(
                &mut rig.poller,
                child,
                &op(TCP_REQ_RECV),
                &recv_operand(INET_INFINITY, 0),
            )
            .expect("the accepted port queues a second receive"),
        vec![1, 0, 1],
        "the second operation is id 1"
    );
    let events = await_any_events(&mut rig, 1);
    assert_eq!(
        events,
        vec![(1, InetAsyncResult::Error(crate::completion::InetReason::Closed))],
        "the close is owed to the *next* receive"
    );
}

// ---------------------------------------------------------------------------
// Row 3 — an inherited `{packet, 2}` record
// ---------------------------------------------------------------------------

/// An **inherited** `{packet, 2}` framing survives the close latch: `tcp_clear_input`
/// (`inet_drv.c:11705`-`:11715`, called from `tcp_recv_closed` at `:12823`) frees the *partial*
/// framing buffer only, so a record the same drain completed is decoded and delivered — its body,
/// never the framing header — before the close is applied.
///
/// The framing arrives through the accept path's inheritance (`tcp_inet_copy` copies
/// `inet.htype`, `:11818`), so this row also pins that the accepted descriptor reads under the
/// listener's packet type.
#[test]
fn a_complete_packet_2_record_survives_the_close_latch() {
    let (mut rig, child, _peer) = accepted_after_write_then_close(Some(PACKET_2));

    assert_eq!(
        rig.registry
            .port_control(
                &mut rig.poller,
                child,
                &op(TCP_REQ_RECV),
                &recv_operand(INET_INFINITY, 0),
            )
            .expect("the accepted port queues a receive"),
        vec![1, 0, 0],
        "the receive is queued on the inherited-framing port"
    );

    let events = await_any_events(&mut rig, 1);
    assert_eq!(events.len(), 1, "exactly one completion: {events:?}");
    match &events[0].1 {
        InetAsyncResult::Data(read) => {
            assert_eq!(
                read.flattened(),
                b"abc".to_vec(),
                "the record's body is delivered, its 2-byte length header stripped"
            );
            assert_eq!(
                read.header,
                Vec::<u8>::new(),
                "the default {{header, 0}} leaves no header prefix"
            );
        }
        other => panic!("the complete record was lost to the close latch: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Row 4 — no queued operation: the notification channel
// ---------------------------------------------------------------------------

/// With **no** operation queued, the reactor's read still reaches the owner: `read_available`
/// queues the bytes it read **before** it latches the close
/// (the C's `driver_output` inside `tcp_deliver` then `tcp_recv_closed`), so the round that
/// observes the end-of-file reports `{Port, {data, …}}` first and the close only through
/// `port_closed()` — each exactly once, never a second `Data`.
#[test]
fn a_write_then_close_reports_data_before_closed_exactly_once() {
    let (mut port, mut peer) = linked();

    peer.write_all(b"abc").expect("the peer writes its payload");
    peer.shutdown(Shutdown::Write)
        .expect("the peer closes its write side");

    let notifications = reactor_notifications(&mut port);
    let data: Vec<_> = notifications
        .iter()
        .filter_map(|notification| match notification {
            PortNotification::Data { port, read } => Some((*port, read.flattened())),
            PortNotification::Closed { .. } => None,
        })
        .collect();
    assert_eq!(
        data,
        vec![(port.id, b"abc".to_vec())],
        "the bytes reach the owner as one {{Port,{{data,…}}}} notification"
    );
    assert!(
        port.registry
            .port_closed(port.id)
            .expect("the port is live"),
        "the same round's latch reports the peer's close"
    );

    // Exactly once: a further round down the same path reports neither the bytes again nor a
    // second close observation.
    let again = port
        .registry
        .execute_port_tasks(&mut port.poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("a port-task round")
        .notifications;
    assert!(
        again.is_empty(),
        "the report is made once: {again:?}"
    );
    assert!(
        port.registry
            .port_closed(port.id)
            .expect("the port is live"),
        "and the close latch is sticky"
    );
}

// ---------------------------------------------------------------------------
// Row 5 — the `erl` oracle
// ---------------------------------------------------------------------------

/// The oracle answers the same burst the same way: `gen_tcp:recv/3` on a connection whose peer
/// already wrote `<<"abc">>` and closed answers `{ok, <<"abc">>}`, and only the **next** recv
/// answers `{error, closed}`.
///
/// `gen_tcp:recv/2,3` is `prim_inet:recv/2` → `TCP_REQ_RECV`(42) on the single receive slot with
/// no `after` clause, so this is the C's `tcp_recv` on the real driver — the same row the Rust
/// half pins. Skips **loudly** when `erl` is unavailable.
#[test]
fn the_oracle_also_answers_a_write_then_close_burst_with_the_data() {
    const PROBE: &str = r#"#!/usr/bin/env escript
main(_) ->
    {ok, L} = gen_tcp:listen(0, [binary, {active, false}, {reuseaddr, true},
                                 {ip, {127, 0, 0, 1}}]),
    {ok, Bound} = inet:port(L),
    {ok, C} = gen_tcp:connect({127, 0, 0, 1}, Bound, [binary, {active, false}]),
    ok = gen_tcp:send(C, <<"abc">>),
    ok = gen_tcp:close(C),
    {ok, S} = gen_tcp:accept(L, 5000),
    R1 = gen_tcp:recv(S, 0, 2000),
    R2 = gen_tcp:recv(S, 0, 2000),
    io:format("~w|~w", [R1, R2]).
"#;

    let Some(dump) = escript_stdout("write_then_close", PROBE, &[]) else {
        return;
    };
    let dump = dump.trim();
    assert!(
        dump.starts_with("{ok,<<97,98,99>>}"),
        "the oracle delivers the burst's payload first, not a close: {dump}"
    );
    assert!(
        dump.ends_with("|{error,closed}"),
        "and the close is owed to the next receive: {dump}"
    );
}
