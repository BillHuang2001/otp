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

//! Integration tests for the **port-task** layer of [`PortRegistry`]: the
//! scheduling → execution → notification path an `mio` event drives, over real OS
//! descriptors.
//!
//! Where `tests/port.rs` exercises the reactor dispatch (`handle_events`) and
//! `tests/poller.rs` the raw readiness loop, this suite pins the *scheduled* shape the
//! legacy C has (`erts_port_task_schedule`/`erts_port_task_execute`,
//! `erts/emulator/beam/erl_port_task.{c,h}`): a readiness event **enqueues** a
//! [`PortTaskKind`] rather than calling a driver inline, a round runs the queue under a
//! reduction budget, and the round reports what the port produced as a
//! [`PortNotification`] — the seam the runtime turns into `{Port, {data, Data}}`.
//!
//! The last test is the **wake-path evidence**: a `PortNotification::Data` is exactly what
//! a runtime hands to `beam_process` to wake a process blocked in `receive`, so it
//! reproduces that hop against a real [`ProcessTable`] plus [`Scheduler`].

use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use beam_io::prelude::*;
use beam_process::prelude::*;
use beam_term::prelude::AtomTable;

fn poller() -> Poller {
    Poller::new().expect("a poll set")
}

fn fd_port() -> Box<dyn Driver> {
    Box::new(FdDriver::pipe().expect("a pipe"))
}

/// Polls and dispatches (scheduling + running tasks, **without** harvesting the round's
/// notifications) until `id` has buffered at least `want` bytes, returning them.
///
/// Uses [`PortRegistry::handle_events`], which schedules and runs the port tasks but does
/// not harvest them into [`PortNotification`]s, so the bytes are still reachable through
/// [`Port::drain_input`] — exactly as `tests/port.rs` drives the reactor.
fn pump_until_input(
    poller: &mut Poller,
    registry: &mut PortRegistry,
    id: PortId,
    want: usize,
) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut received = Vec::new();
    while received.len() < want {
        assert!(
            Instant::now() < deadline,
            "the reactor never delivered {want} bytes"
        );
        let events = poller
            .poll(Some(Duration::from_millis(200)))
            .expect("poll succeeds");
        registry
            .handle_events(poller, &events)
            .expect("events dispatch");
        received.extend(registry.get_mut(id).expect("live").drain_input());
    }
    received
}

/// A scheduled `OUTPUT` task writes the queued bytes and a separate scheduled `INPUT` task
/// reads them back, the latter reporting what it read as a [`PortNotification::Data`].
///
/// The evidence: the write and the read are **two separate scheduled tasks**, never an
/// inline driver call. The first round runs exactly the `OUTPUT` task
/// (`ran == 1`, `reds_used == PORT_REDS_OUTPUT`, no notification) and nothing has been
/// read yet; only the second round — driven by a read-readiness event — runs the `INPUT`
/// task (`reds_used == PORT_REDS_INPUT`), which reports the `b"ping"` it just read.
#[test]
fn scheduled_output_and_input_tasks_end_to_end() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry.open(&mut poller, fd_port()).expect("the port opens");

    // `port_command` only queues the bytes and schedules an OUTPUT task: nothing is written
    // yet (that is what `erlang:port_command/2` returning `true` immediately means).
    registry
        .port_command(&mut poller, id, b"ping", PortCommandOptions::new())
        .expect("the command is queued");
    assert_eq!(
        registry.get(id).expect("live").output_len(),
        4,
        "the command bytes are queued, not written"
    );

    let write_round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the output task runs");
    assert_eq!(write_round.ran, 1, "exactly the OUTPUT task ran");
    assert_eq!(write_round.reds_used, PORT_REDS_OUTPUT);
    assert!(
        write_round.notifications.is_empty(),
        "an OUTPUT task reports nothing to its owner"
    );
    assert_eq!(
        registry.get(id).expect("live").input_len(),
        0,
        "the write alone read nothing: the read is a separate scheduled task"
    );

    // Read readiness schedules the INPUT task; only then does the byte come back.
    assert!(registry.schedule_readiness(
        id.number(),
        Readiness {
            readable: true,
            ..Readiness::default()
        }
    ));
    let read_round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the input task runs");
    assert_eq!(read_round.ran, 1, "exactly the INPUT task ran");
    assert_eq!(read_round.reds_used, PORT_REDS_INPUT);
    assert_eq!(
        read_round.notifications,
        vec![PortNotification::Data {
            port: id,
            bytes: b"ping".to_vec()
        }]
    );

    registry.close(&mut poller, id).expect("closes");
    assert!(poller.is_empty(), "closing deregisters the port's fd");
}

/// An **EOF / read-closed** readiness event still reaches the driver's `ready_input`: it
/// *enqueues* an `INPUT` task rather than being dropped, and that task reads no bytes.
///
/// Two independent pieces of evidence:
///
/// 1. Over a **real** `socketpair(2)`, a full close of the peer (`drop(peer)`) is reported
///    by `mio` as a read-closed/readable event; scheduling it leaves an `INPUT` task
///    pending *immediately* — before it is executed — so the event is not lost; running it
///    yields no [`PortNotification::Data`] (end-of-file produces no bytes).
/// 2. `poller::Ready` has no public constructor, so a real [`Event`] cannot express
///    "read-closed with `readable == false`". That rule is pinned directly through
///    [`Readiness`] on a second read-armed port: a pure EOF readiness still schedules an
///    `INPUT` task that runs (and reads nothing).
#[test]
fn a_pure_eof_event_still_reaches_the_driver() {
    let (peer, end) = UnixStream::pair().expect("socketpair");
    // SAFETY: `end.as_raw_fd()` names an open descriptor of a live stream; `dup(2)` returns
    // a fresh descriptor for the same socket, which `FdDriver::read_only` then owns and
    // closes exactly once (on `stop`/`Drop`).
    let dupfd = unsafe { libc::dup(end.as_raw_fd()) };
    assert!(dupfd >= 0, "dup(2) succeeded");

    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry
        .open(&mut poller, Box::new(FdDriver::read_only(dupfd)))
        .expect("the read-only port starts");

    // Drive the port like `tests/port.rs`: the bytes arrive before the EOF.
    (&peer).write_all(b"bye").expect("the peer accepts the write");
    assert_eq!(
        pump_until_input(&mut poller, &mut registry, id, 3),
        b"bye",
        "the bytes arrive through the reactor before the EOF"
    );

    // A full close of the peer turns the read fd into end-of-file (`EPOLLHUP`).
    drop(peer);
    let events = poller
        .poll(Some(Duration::from_secs(2)))
        .expect("poll succeeds");
    assert!(
        registry.schedule_events(&events) >= 1,
        "the read-closed event names the live port"
    );
    assert_eq!(
        registry.get(id).expect("live").next_task(),
        Some(PortTaskKind::Input),
        "an EOF event ENQUEUES an INPUT task immediately rather than being dropped"
    );

    let eof_round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the EOF task runs");
    assert_eq!(eof_round.ran, 1, "the EOF event ran exactly one INPUT task");
    assert!(
        !eof_round
            .notifications
            .iter()
            .any(|n| matches!(n, PortNotification::Data { .. })),
        "end-of-file yields no bytes, so no Data notification is owed"
    );

    // The pure-EOF rule a real `Event` cannot express: read-closed with `readable == false`.
    let id2 = registry
        .open(&mut poller, fd_port())
        .expect("a second, read-armed port");
    assert!(registry.schedule_readiness(
        id2.number(),
        Readiness {
            read_closed: true,
            ..Readiness::default()
        }
    ));
    assert_eq!(
        registry.get(id2).expect("live").next_task(),
        Some(PortTaskKind::Input),
        "pure EOF (no readable bit) still schedules an INPUT task"
    );
    let pure_eof = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the pure-EOF task runs");
    assert_eq!(pure_eof.ran, 1);
    assert!(
        pure_eof.notifications.is_empty(),
        "a pure EOF reads nothing, so it owes no data"
    );

    registry.close(&mut poller, id).expect("closes");
    registry.close(&mut poller, id2).expect("closes");
    assert!(poller.is_empty(), "closing both ports deregisters their fds");
}

/// The per-round reduction budget bounds how much runs: a zero budget runs nothing — and
/// loses nothing — and exactly one task's worth of reductions runs exactly one task.
///
/// `INPUT` costs the same `CONTEXT_REDS/100` as `OUTPUT` (`PORT_REDS_INPUT ==
/// PORT_REDS_OUTPUT`), so a `PORT_REDS_OUTPUT` budget runs the single highest-priority
/// task (an `INPUT` task, priority 0) and leaves the other queued.
#[test]
fn the_reduction_budget_bounds_a_round() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry.open(&mut poller, fd_port()).expect("the port opens");

    // Two tasks scheduled by hand, so the round has a queue to bound.
    let port = registry.get_mut(id).expect("live");
    port.queue_output(b"q");
    assert!(port.schedule_task(PortTaskKind::Output));
    assert!(port.schedule_task(PortTaskKind::Input));
    assert_eq!(port.pending_tasks(), 2);

    // A zero budget runs nothing and leaves both tasks queued.
    let none = registry.execute_port_tasks(&mut poller, 0).expect("runs");
    assert_eq!(
        (none.ran, none.reds_used),
        (0, 0),
        "a zero budget runs no task"
    );
    assert_eq!(
        registry.get(id).expect("live").pending_tasks(),
        2,
        "nothing ran, so nothing was lost"
    );

    // Exactly one task's reductions run exactly one task.
    let one = registry
        .execute_port_tasks(&mut poller, PORT_REDS_OUTPUT)
        .expect("runs");
    assert_eq!(
        (one.ran, one.reds_used),
        (1, PORT_REDS_OUTPUT),
        "the budget bounds the round to a single task"
    );
    assert_eq!(
        registry.get(id).expect("live").pending_tasks(),
        1,
        "the second task stays queued for the next round"
    );

    // The default round budget is one whole `ERTS_PORT_REDS_EXECUTE`.
    assert_eq!(DEFAULT_READINESS_RED_BUDGET, PORT_REDS_EXECUTE);

    registry.close(&mut poller, id).expect("closes");
}

/// THE WAKE-PATH EVIDENCE: the bytes a scheduled port task read ([`PortNotification::Data`])
/// are exactly what a runtime hands to `beam_process` to wake a process blocked in
/// `receive`.
///
/// Phase 1 produces the notification from the port-task layer: `port_command` schedules an
/// `OUTPUT` task that writes `b"ping"` into a pipe, then a read-readiness event schedules an
/// `INPUT` task that reads it back and reports it.
///
/// Phase 2 feeds that notification's port into a real [`ProcessTable`]: the receiver is
/// [`ProcessState::Waiting`] (blocked in `receive`), the delivery from the sender's pid
/// **wakes** it (`DeliverOutcome::Woke`), and the single-threaded [`Scheduler`] absorbs the
/// wake into its run queue (`resume_woken` enqueues the receiver).
///
/// The runtime's BIF layer builds the real `{Port, {data, Data}}` tuple around these bytes;
/// here the port id immediate stands in for the payload, and `beam_io` only *reports* the
/// notification — process scheduling stays in `beam_process`.
#[test]
fn a_port_data_notification_wakes_a_blocked_process() {
    // --- phase 1: a scheduled task produces the Data notification -------------------
    let mut poller = poller();
    let mut registry = PortRegistry::new();
    let id = registry.open(&mut poller, fd_port()).expect("the port opens");

    registry
        .port_command(&mut poller, id, b"ping", PortCommandOptions::new())
        .expect("the command is queued");
    let write_round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the OUTPUT task writes into the pipe");
    assert_eq!(write_round.ran, 1);
    assert!(write_round.notifications.is_empty());

    assert!(registry.schedule_readiness(
        id.number(),
        Readiness {
            readable: true,
            ..Readiness::default()
        }
    ));
    let read_round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the INPUT task reads the bytes back");
    assert_eq!(read_round.ran, 1);
    let (port, bytes) = match read_round.notifications.as_slice() {
        [PortNotification::Data { port, bytes }] => (*port, bytes.clone()),
        other => panic!("expected exactly one Data notification, got {other:?}"),
    };
    assert_eq!(port, id, "the notification names the port that produced it");
    assert_eq!(bytes, b"ping".to_vec());

    // --- phase 2: that notification wakes a blocked receive ------------------------
    let mut atoms = AtomTable::new();
    let mut table = ProcessTable::new(SignalAtoms::from_table(&mut atoms));
    let sender = table.spawn_default().expect("a sender");
    let receiver = table.spawn_default().expect("a receiver");
    // The receiver is blocked in `receive`, waiting for a message to arrive.
    table
        .get_mut(receiver)
        .expect("live")
        .set_state(ProcessState::Waiting);

    // The runtime delivers the port's message to the blocked receiver, which wakes it.
    // `deliver_simple` is the no-off-heap form of `deliver` — both terms here are immediates
    // (a pid immediate and the port-id immediate), so nothing needs off-heap resolution.
    let outcome = table
        .deliver_simple(receiver, sender.to_eterm(), id.to_eterm())
        .expect("the delivery succeeds");
    assert_eq!(outcome, DeliverOutcome::Woke);
    assert!(outcome.woke());
    assert_eq!(
        table.get(receiver).expect("live").state(),
        ProcessState::Runnable,
        "the blocked receiver was made runnable"
    );
    // `woken()` is the non-clearing view; the wake must still be pending for the scheduler.
    assert!(table.woken().contains(&receiver));

    let mut scheduler = Scheduler::new();
    assert_eq!(
        scheduler.resume_woken(&mut table),
        1,
        "the scheduler absorbs the one wake"
    );
    assert!(
        scheduler.run_queue().contains(receiver),
        "the woken receiver is now on the run queue"
    );

    registry.close(&mut poller, id).expect("closes");
}

/// A [`PortNotification::Closed`] **never names a reused port number**: a closed port keeps its
/// number reserved until the reporting round mints the notification, so a `close` followed by an
/// `open` before that round cannot make the deferred close name the freshly opened (live) port.
///
/// The evidence is the observable invariant a consumer depends on — a `Closed` always names a
/// number that is *not* a live port — plus the re-open that had to take a different number.
#[test]
fn a_closed_notification_never_names_a_reused_port_number() {
    let mut poller = poller();
    let mut registry = PortRegistry::new();

    let first = registry.open(&mut poller, fd_port()).expect("the port opens");
    registry.close(&mut poller, first).expect("the port closes");

    // Re-opening before the reporting round must NOT take the closed port's number.
    let second = registry
        .open(&mut poller, fd_port())
        .expect("a second port opens");
    assert_ne!(
        second, first,
        "an unreported close keeps its number out of circulation"
    );
    assert!(registry.contains(second));

    let round = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    let closed = match round.notifications.as_slice() {
        [PortNotification::Closed { port }] => *port,
        other => panic!("expected exactly one Closed notification, got {other:?}"),
    };
    assert_eq!(closed, first, "the notification names the port that closed");
    assert!(
        !registry.contains(closed),
        "a Closed notification never names a live (reused) port"
    );

    // The round retired the number, so it is free for the next open — with no second report.
    let reused = registry
        .open(&mut poller, fd_port())
        .expect("a third port opens");
    assert_eq!(reused, first, "the reported close retired the number");
    let later = registry
        .execute_port_tasks(&mut poller, DEFAULT_READINESS_RED_BUDGET)
        .expect("the round runs");
    assert!(later.notifications.is_empty(), "the close is reported once");

    registry.close(&mut poller, second).expect("closes");
    registry.close(&mut poller, reused).expect("closes");
}
