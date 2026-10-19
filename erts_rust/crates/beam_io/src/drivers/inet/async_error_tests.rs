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

//! The **passive half** of the read-error delivery — a queued `TCP_REQ_RECV`(42) completed by the
//! peer's close — over **real loopback sockets**.
//!
//! The active half (`{tcp_error, S, Reason}` before `{tcp_closed, S}`) lives in
//! [`super::error_delivery_tests`]. A **passive** socket takes a different route out of the same
//! discovery: the C's `tcp_recv_error` drains the pending operation with
//! `async_error_am_all(INETP(desc), error_atom(err))` (`inet_drv.c:12877`) — the failing **errno**
//! — while `tcp_recv_closed` drains it with `async_error_am_all(INETP(desc), am_closed)`
//! (`:12829`), the plain `closed`. Which of the two ran is decided by `tcp_recv`'s classification
//! (`:13185`-`:13210`): `ECONNRESET` only reaches `tcp_recv_error` while
//! `TCP_ADDF_SHOW_ECONNRESET` is set (`{show_econnreset,true}`, armed at `:7178`), so the same RST
//! is an `econnreset` completion under the option and a `closed` one without it.
//!
//! In this crate that is exactly the `drain_error_all(queue, ctx, reason)`-vs-`drain_closed(queue,
//! ctx)` distinction (`src/drivers/inet/async.rs`): a regression that collapsed both to
//! `InetReason::Closed` — or that let the *reactor's* discovery overwrite the reason the queued
//! receive is owed — is what these cases catch. The three cases differ **only** in what the peer
//! does and in the option, so the completion's reason is the whole assertion.
//!
//! Every wait here is a **condition** under a hang guard (rounds are pumped until the completion
//! appears), never a wall-clock budget tuned for an idle host.

use super::opcode_tests::{loopback_arg, op, opened_port, Rig};
use super::opt::INET_INFINITY;
use super::wire::{ctl_ok, ctl_xerror};
use crate::completion::{InetAsyncResult, InetReason, PortCompletion};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

/// `INET_REQ_CONNECT` (`inet_drv.c:11993`).
const INET_REQ_CONNECT: u32 = 3;
/// `INET_REQ_SETOPTS` (`:11127`).
const INET_REQ_SETOPTS: u32 = 7;
/// `TCP_REQ_RECV` (`:12268`): the asynchronous receive whose completion these cases pin.
const TCP_REQ_RECV: u32 = 42;
/// `INET_LOPT_TCP_SHOW_ECONNRESET` (39, `:916`) with the value `1` — the option that sends a read
/// reset to `tcp_recv_error` (`:13185`-`:13196`) instead of `tcp_recv_closed`.
const SHOW_ECONNRESET: [u8; 5] = [39, 0, 0, 0, 1];
/// The same option with the value `0` — the default, spelled out so the "off" rigs are explicit.
const HIDE_ECONNRESET: [u8; 5] = [39, 0, 0, 0, 0];

/// How long a condition may stay unsatisfied before the case fails (a liveness bound only).
const HANG_GUARD: Duration = Duration::from_secs(20);
/// How long one condition-wait iteration pauses between rounds.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

// ---------------------------------------------------------------------------
// The fixtures
// ---------------------------------------------------------------------------

/// A `tcp_inet` port **connected out** to a loopback listener and left **passive** (`{active,
/// false}`, the default) with the given `{show_econnreset, …}` record, plus the far end (which the
/// test aborts or half-closes).
fn passive_connected(option: [u8; 5]) -> (Rig, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let address = listener.local_addr().expect("an address");
    let mut port = opened_port();
    let mut connect = vec![0u8, 0, 3, 232]; // a 1000 ms connect budget
    connect.extend_from_slice(&loopback_arg(address.port()));
    assert_eq!(
        port.control(&op(INET_REQ_CONNECT), &connect),
        ctl_ok(),
        "the port connects to the loopback listener"
    );
    let (peer, _) = listener.accept().expect("the peer side is accepted");
    assert_eq!(
        port.control(&op(INET_REQ_SETOPTS), &option),
        ctl_ok(),
        "the `{{show_econnreset, _}}` record is accepted"
    );
    // The default mode is passive; asserting it keeps this suite's half of the split explicit.
    assert_eq!(
        port.control(&op(INET_REQ_SETOPTS), &[22, 0, 0, 0, 0]),
        ctl_ok(),
        "`{{active,false}}` is accepted"
    );
    (port, peer)
}

/// Aborts the connection: `SO_LINGER` with a zero timeout makes `close(2)` send an **RST** instead
/// of an orderly FIN.
fn abort(peer: TcpStream) {
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: `linger` is a fully initialized `struct linger` and the descriptor is an open socket
    // owned by this test; `setsockopt` reads at most `size_of::<linger>()` bytes and retains
    // nothing.
    let rc = unsafe {
        libc::setsockopt(
            peer.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw const linger).cast(),
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "SO_LINGER is settable");
    drop(peer); // an abortive close: RST
}

/// Queues one infinite `TCP_REQ_RECV`(42) on the (passive, raw) port and returns the asynchronous
/// id its reply handed out — the id every completion below must echo.
fn pending_recv(port: &mut Rig) -> u16 {
    let mut operand = INET_INFINITY.to_be_bytes().to_vec();
    operand.extend_from_slice(&0u32.to_be_bytes()); // Length(4): "whatever has arrived"
    let reply = port.control(&op(TCP_REQ_RECV), &operand);
    assert_eq!(
        reply.len(),
        3,
        "an accepted receive replies `[1] ++ <<Id:16/big>>`: {reply:?}"
    );
    assert_eq!(reply[0], ctl_ok()[0], "the enqueue is accepted: {reply:?}");
    u16::from_be_bytes([reply[1], reply[2]])
}

// ---------------------------------------------------------------------------
// Completion helpers
// ---------------------------------------------------------------------------

/// The `(id, result)` pairs of every `{inet_async, …}` completion in `completions`.
fn async_events(completions: &[PortCompletion]) -> Vec<(u16, InetAsyncResult)> {
    completions
        .iter()
        .filter_map(|completion| match &completion.event {
            crate::completion::CompletionEvent::InetAsync { id, result } => {
                Some((*id, result.clone()))
            }
            crate::completion::CompletionEvent::InetReply { .. } => None,
        })
        .collect()
}

/// Pumps port-task rounds until at least `want` completions have been drained, under
/// [`HANG_GUARD`]. A round is [`crate::PortRegistry::execute_port_tasks`], the point a queued
/// operation is driven (`Driver::drive_async` → `try_complete_recv`).
fn await_events(port: &mut Rig, want: usize) -> Vec<(u16, InetAsyncResult)> {
    let deadline = Instant::now() + HANG_GUARD;
    let mut events = Vec::new();
    while events.len() < want {
        port.round().expect("a port-task round");
        events.extend(async_events(&port.completions()));
        if events.len() >= want {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {want} completions arrived within {HANG_GUARD:?}: {events:?}",
            events.len()
        );
        std::thread::sleep(POLL_INTERVAL);
    }
    events
}

/// The single `InetReason` a drain carried, asserting the shape (`Error`, never `Data`) and — the
/// contract every case here shares — that it is the reason `want` names by atom.
fn only_error_reason(events: &[(u16, InetAsyncResult)], want: InetReason) -> InetReason {
    assert_eq!(
        events.len(),
        1,
        "exactly one completion per pending operation: {events:?}"
    );
    match &events[0].1 {
        InetAsyncResult::Error(reason) => {
            assert_eq!(
                *reason,
                want,
                "the reason is `{}`: {events:?}",
                want.atom_name()
            );
            assert_eq!(
                reason.atom_name(),
                want.atom_name(),
                "rendered by `InetReason::atom_name`: {events:?}"
            );
            *reason
        }
        other => panic!("the completion is an error, not {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The passive receive ladder: which errno class the drain reports
// ---------------------------------------------------------------------------

/// `{show_econnreset,true}` + a pending `TCP_REQ_RECV` + a peer that aborts with an RST: the read
/// failure is `tcp_recv_error`'s, which drains the queued operation with
/// `async_error_am_all(error_atom(err))` (`inet_drv.c:12877`) — so the completion is the failing
/// **errno**, `{error, econnreset}`, and never the bare `closed` that the clean-close route
/// (`tcp_recv_closed`, `:12829`) reports.
///
/// A regression that drained `InetReason::Closed` here fails on the reason (and on the atom name);
/// one that completed the operation twice fails on the drain's length; one that armed the receive
/// ladder for a discovery the operation itself consumed fails on the trailing `enotconn`.
#[test]
fn a_pending_passive_receive_on_a_reset_answers_the_errno() {
    let (mut port, peer) = passive_connected(SHOW_ECONNRESET);
    let id = pending_recv(&mut port);
    abort(peer);

    let events = await_events(&mut port, 1);
    assert_eq!(
        events[0].0, id,
        "the completion echoes the id its reply gave"
    );
    let reason = only_error_reason(&events, InetReason::Errno(libc::ECONNRESET));
    assert!(
        matches!(reason, InetReason::Errno(errno) if errno == libc::ECONNRESET),
        "the reason carries the failing errno, not a synthesised atom: {reason:?}"
    );
    assert_eq!(
        reason.atom_name(),
        "econnreset",
        "the oracle's `{{error, econnreset}}` (`tcp_recv_error`, `:12877`)"
    );
    assert_ne!(
        reason.atom_name(),
        InetReason::Closed.atom_name(),
        "the reset is distinguishable from a plain close"
    );

    // Exactly once: no later round re-reports the operation it already answered.
    port.round().expect("a second round");
    assert!(
        port.completions().is_empty(),
        "the drain reports the pending operation exactly once"
    );
    port.round().expect("a third round");
    assert!(port.completions().is_empty(), "and never again");

    // The discovery answered the operation itself, so it armed **nothing** on the receive ladder:
    // the next `TCP_REQ_RECV` is the C's plain `enotconn` (`:12286`), not a second claim on the
    // same close.
    let mut operand = INET_INFINITY.to_be_bytes().to_vec();
    operand.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &operand),
        ctl_xerror("enotconn"),
        "`tcp_recv_error` arms no one-shot (`:12849`-`:12890`)"
    );
}

/// A **clean** peer half-close (FIN) with `{show_econnreset,true}` armed: an end-of-file is
/// `tcp_recv_closed`'s `n == 0` arm (`inet_drv.c:13206`) whatever the option says, so the pending
/// receive is drained with `am_closed` (`:12829`) — `{error, closed}`, never an errno.
#[test]
fn a_pending_passive_receive_on_a_clean_close_answers_closed() {
    let (mut port, peer) = passive_connected(SHOW_ECONNRESET);
    let id = pending_recv(&mut port);
    peer.shutdown(Shutdown::Write)
        .expect("the peer half-closes");

    let events = await_events(&mut port, 1);
    assert_eq!(
        events[0].0, id,
        "the completion echoes the id its reply gave"
    );
    let reason = only_error_reason(&events, InetReason::Closed);
    assert_eq!(
        reason.atom_name(),
        "closed",
        "the oracle's `{{error, closed}}` for a clean end-of-file"
    );
    assert_ne!(
        reason.atom_name(),
        InetReason::Errno(libc::ECONNRESET).atom_name(),
        "a clean close is distinguishable from a reset"
    );

    port.round().expect("a second round");
    assert!(
        port.completions().is_empty(),
        "the clean close is reported exactly once"
    );

    // The end-of-file armed only the *send* one-shot (`:12816`), so the receive ladder answers
    // `enotconn` here too.
    let mut operand = INET_INFINITY.to_be_bytes().to_vec();
    operand.extend_from_slice(&0u32.to_be_bytes());
    assert_eq!(
        port.control(&op(TCP_REQ_RECV), &operand),
        ctl_xerror("enotconn"),
        "`tcp_recv_closed` owes the receive nothing beyond the reply it already gave"
    );
    drop(peer);
}

/// The same RST with the **default** `{show_econnreset,false}`: `tcp_recv`'s `ECONNRESET`
/// classification (`inet_drv.c:13185`-`:13196`) sends the reset to `tcp_recv_closed`, so the
/// pending receive is drained with `am_closed` (`:12829`) — `{error, closed}`.
///
/// This is the row that makes the other two meaningful: the *same* peer abort, on the *same*
/// socket state, differs only by the option.
#[test]
fn a_pending_passive_receive_on_a_reset_without_show_econnreset_answers_closed() {
    let (mut port, peer) = passive_connected(HIDE_ECONNRESET);
    let id = pending_recv(&mut port);
    abort(peer);

    let events = await_events(&mut port, 1);
    assert_eq!(
        events[0].0, id,
        "the completion echoes the id its reply gave"
    );
    let reason = only_error_reason(&events, InetReason::Closed);
    assert_eq!(
        reason.atom_name(),
        "closed",
        "without the option a reset is a plain close (`tcp_recv_closed`, `:12783`)"
    );
    assert_ne!(
        reason.atom_name(),
        InetReason::Errno(libc::ECONNRESET).atom_name(),
        "the reset is reported as closed only while `{{show_econnreset,false}}` holds"
    );

    port.round().expect("a second round");
    assert!(
        port.completions().is_empty(),
        "the reset is reported exactly once"
    );
}
