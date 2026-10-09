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
//! Integration evidence for the **port-aware scheduler entry points**.
//!
//! A process that monitors a port (`erlang:monitor(port, Port)`) leaves two halves behind: the
//! outgoing [`PortWatch`] on the watcher and the incoming [`PortMonitor`] in the [`PortTable`].
//! The process finalizer drains the outgoing half and removes the matching incoming entry — but
//! only when it is handed the port table (`ProcessTable::terminate_with_ports`, and its
//! schedule-in sibling `ProcessTable::service_signals_with_ports`). `tests/port_monitor_cleanup.rs`
//! pins that at the *table* level; this file pins it at the **scheduler** level, where the two
//! deaths normally happen:
//!
//! * a process returning `RunOutcome::Exit(reason)` from a slice is terminated by the
//!   schedule-out step, and
//! * a process carrying a deferred fatal signal is terminated by the schedule-in step (before it
//!   ever reaches the runner).
//!
//! Driving the scheduler through the port-less `Scheduler::run_slice`/`Scheduler::run_ready`
//! cannot clean the port side (no handle), so `Scheduler::run_slice_with_ports` /
//! `Scheduler::run_ready_with_ports` are the adoption an orchestrator that owns both tables
//! (`beam_rt`) owes. Both halves are pinned below, together with the port-less control that
//! proves the variant is what adds the cleanup, and with the two wake-absorption paths (after a
//! slice and on the empty-queue early return) that the port-aware forms must preserve.
//!
//! The port-aware slice helpers themselves (`run_one_with_ports`/`slice_begin_with_ports`/
//! `slice_end_with_ports`) are `pub(crate)` — the SMP-reuse seam, exactly like the port-less
//! `run_one`/`slice_begin`/`slice_end` — so this file drives them through the public
//! `Scheduler::*_with_ports` entry points that route into them.
//!
//! The oracle cross-check for the *end state* (a dead port watcher disappears from
//! `erlang:port_info(Port, monitored_by)`) already lives in `tests/port_monitor_cleanup.rs` and is
//! deliberately not duplicated here.

use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::{make_small, AtomTable, Eterm};

/// A process table plus the port table it is driven with.
struct Fixture {
    table: ProcessTable,
    ports: PortTable,
}

impl Fixture {
    fn new() -> Fixture {
        let atoms = SignalAtoms::from_table(&mut AtomTable::new());
        Fixture {
            table: ProcessTable::new(atoms),
            ports: PortTable::new(),
        }
    }

    /// A fresh process owned by the fixture's table.
    fn spawn(&mut self) -> ProcessId {
        self.table.spawn_default().expect("room in the pid table")
    }

    /// A fresh port owned by `owner`.
    fn port(&mut self, owner: ProcessId) -> PortId {
        self.ports
            .alloc_owned(owner)
            .expect("room in the port number space")
    }

    /// Places an incoming port monitor for `watcher` on `port` (the `erlang:monitor(port, Port)`
    /// path) and returns its reference.
    fn watch_port(&mut self, watcher: ProcessId, port: PortId) -> MonitorRef {
        self.table
            .monitor_port_term(watcher, port.to_eterm(), &mut self.ports)
            .expect("the watcher is live and the port is allocated")
    }

    /// The single incoming monitor on `port`, panicking unless exactly one is present.
    fn sole_monitor(&self, port: PortId) -> PortMonitor {
        let monitors = self.ports.monitors(port);
        assert_eq!(monitors.len(), 1, "exactly one incoming monitor");
        monitors[0]
    }

    /// Pins the state a live port watcher is in: one incoming monitor on the port *and* the
    /// matching outgoing watch on the watcher itself.
    fn assert_watched(&self, watcher: ProcessId, port: PortId) -> MonitorRef {
        let monitor = self.sole_monitor(port);
        assert_eq!(
            monitor.watcher, watcher,
            "the port lists the watcher as monitoring it"
        );
        assert_eq!(
            self.table
                .get(watcher)
                .expect("the watcher is live")
                .port_monitors()
                .len(),
            1,
            "the outgoing half is recorded on the watcher"
        );
        monitor.reference
    }
}

/// A runner that records every process it is asked to run, reporting `Exit(reason)` for `target`
/// and `Yield` for anyone else.
struct ExitRunner {
    target: ProcessId,
    reason: Eterm,
    ran: Vec<ProcessId>,
}

impl ExitRunner {
    fn new(target: ProcessId, reason: Eterm) -> ExitRunner {
        ExitRunner {
            target,
            reason,
            ran: Vec::new(),
        }
    }
}

impl ProcessRunner for ExitRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        _table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        self.ran.push(pid);
        if pid == self.target {
            Ok(RunOutcome::Exit(self.reason))
        } else {
            Ok(RunOutcome::Yield)
        }
    }
}

/// A runner modelling two processes: `a` blocks on an empty receive until `b` sends it `42`; `b`
/// sends and then blocks itself.
struct SendThenWait {
    a: ProcessId,
    b: ProcessId,
}

impl ProcessRunner for SendThenWait {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        if pid == self.a {
            let process = table.get_mut(pid).expect("a is live");
            match process.receive(|message| message.payload() == make_small(42)) {
                Some(_) => Ok(RunOutcome::Exit(make_small(0))),
                None => {
                    // Block on the empty receive: the runner owns the state transition.
                    process.set_state(ProcessState::Waiting);
                    Ok(RunOutcome::Wait)
                }
            }
        } else if pid == self.b {
            table
                .deliver_simple(self.a, pid.to_eterm(), make_small(42))
                .expect("a is live and the immediate always fits");
            table
                .get_mut(pid)
                .expect("b is live")
                .set_state(ProcessState::Waiting);
            Ok(RunOutcome::Wait)
        } else {
            Ok(RunOutcome::Yield)
        }
    }
}

// ---------------------------------------------------------------------------
// Schedule-out: a process exiting from a slice
// ---------------------------------------------------------------------------

#[test]
fn a_death_during_a_slice_cleans_the_dangling_port_monitor() {
    let mut fx = Fixture::new();
    let owner = fx.spawn();
    let watcher = fx.spawn();
    let port = fx.port(owner);
    let reference = fx.watch_port(watcher, port);
    assert_eq!(fx.assert_watched(watcher, port), reference);

    let mut scheduler = Scheduler::new();
    scheduler.enqueue(watcher);
    let mut runner = ExitRunner::new(watcher, make_small(1));
    let ran = scheduler
        .run_slice_with_ports(&mut fx.table, &mut fx.ports, &mut runner)
        .expect("the runner never fails");

    assert_eq!(ran, Some(watcher), "the slice ran the dequeued pid");
    assert_eq!(runner.ran, vec![watcher], "exactly one slice");
    assert!(!fx.table.contains(watcher), "the Exit terminated the watcher");
    assert!(
        fx.ports.monitors(port).is_empty(),
        "the dead watcher's incoming port monitor is gone — `port_info(Port, monitored_by)` \
         stops listing it"
    );
    assert!(
        fx.ports.contains(port),
        "the port itself is untouched by the watcher's death"
    );
}

#[test]
fn a_death_through_the_port_less_slice_leaves_the_port_monitor_dangling() {
    // The control: the port-less `run_slice` has no `PortTable`, so the incoming entry stays
    // (exactly the delegation `tests/port_monitor_cleanup.rs` pins at the table level). This is
    // what makes `run_slice_with_ports` the adoption a runtime owning both tables owes.
    let mut fx = Fixture::new();
    let owner = fx.spawn();
    let watcher = fx.spawn();
    let port = fx.port(owner);
    let reference = fx.watch_port(watcher, port);

    let mut scheduler = Scheduler::new();
    scheduler.enqueue(watcher);
    let mut runner = ExitRunner::new(watcher, make_small(1));
    let ran = scheduler
        .run_slice(&mut fx.table, &mut runner)
        .expect("the runner never fails");

    assert_eq!(ran, Some(watcher));
    assert!(!fx.table.contains(watcher), "the Exit terminated the watcher");
    assert_eq!(
        fx.sole_monitor(port).reference,
        reference,
        "without the port table the incoming entry cannot be removed"
    );
}

#[test]
fn run_ready_with_ports_cleans_the_death_of_every_slice() {
    let mut fx = Fixture::new();
    let owner = fx.spawn();
    let watcher = fx.spawn();
    let peer = fx.spawn();
    let port = fx.port(owner);
    fx.watch_port(watcher, port);

    let mut scheduler = Scheduler::with_slice(10);
    scheduler.enqueue(watcher);
    scheduler.enqueue(peer);
    let mut runner = ExitRunner::new(watcher, make_small(3));
    let slices = scheduler
        .run_ready_with_ports(&mut fx.table, &mut fx.ports, &mut runner, Some(2))
        .expect("the runner never fails");

    assert_eq!(slices, 2, "the watcher exited, the peer yielded");
    assert_eq!(runner.ran, vec![watcher, peer], "FIFO order preserved");
    assert!(!fx.table.contains(watcher));
    assert!(fx.table.contains(peer), "the yielding peer survives");
    assert!(
        fx.ports.monitors(port).is_empty(),
        "the loop's schedule-out cleaned the port monitor"
    );
}

// ---------------------------------------------------------------------------
// Schedule-in: a deferred fatal signal
// ---------------------------------------------------------------------------

#[test]
fn a_deferred_fatal_signal_terminates_at_schedule_in_and_cleans_the_port_monitor() {
    let mut fx = Fixture::new();
    let owner = fx.spawn();
    let target = fx.spawn();
    let sender = fx.spawn();
    let port = fx.port(owner);
    let reference = fx.watch_port(target, port);
    assert_eq!(fx.assert_watched(target, port), reference);

    // The single scheduler marks the executing process `Running`; a signal sent to it is
    // therefore deferred (see the crate `CONTEXT.md`, the eager-vs-queued rule).
    fx.table
        .get_mut(target)
        .expect("target")
        .set_state(ProcessState::Running);
    fx.table
        .send_exit(sender, target, make_small(7))
        .expect("a live target");
    assert!(fx.table.contains(target), "the signal is deferred");

    let mut scheduler = Scheduler::new();
    scheduler.enqueue(target);
    // A runner that would run anybody *except* the target: it must never be entered for the
    // target, because the schedule-in servicing terminates it first.
    let mut runner = ExitRunner::new(ProcessId::new(9999, 0), make_small(0));
    let ran = scheduler
        .run_slice_with_ports(&mut fx.table, &mut fx.ports, &mut runner)
        .expect("the runner never fails");

    // `run_slice`-parity: the pid was dequeued, so a pid is reported back (the pre-existing
    // port-less `run_slice` behaves identically when servicing kills the pid).
    assert_eq!(ran, Some(target));
    assert!(
        runner.ran.is_empty(),
        "the deferred signal terminated it at schedule-in, before the runner was entered"
    );
    assert!(!fx.table.contains(target), "the schedule-in servicing killed it");
    assert!(
        fx.ports.monitors(port).is_empty(),
        "the schedule-in cascade cleaned the target's incoming port monitor"
    );
}

#[test]
fn a_deferred_fatal_signal_at_schedule_in_without_ports_leaves_the_port_monitor() {
    // The control for the schedule-in half.
    let mut fx = Fixture::new();
    let owner = fx.spawn();
    let target = fx.spawn();
    let sender = fx.spawn();
    let port = fx.port(owner);
    let reference = fx.watch_port(target, port);

    fx.table
        .get_mut(target)
        .expect("target")
        .set_state(ProcessState::Running);
    fx.table
        .send_exit(sender, target, make_small(7))
        .expect("a live target");

    let mut scheduler = Scheduler::new();
    scheduler.enqueue(target);
    let mut runner = ExitRunner::new(ProcessId::new(9999, 0), make_small(0));
    let ran = scheduler
        .run_slice(&mut fx.table, &mut runner)
        .expect("the runner never fails");

    assert_eq!(ran, Some(target));
    assert!(runner.ran.is_empty(), "it never reached the runner");
    assert!(!fx.table.contains(target));
    assert_eq!(
        fx.sole_monitor(port).reference,
        reference,
        "the port-less schedule-in cannot reach the port table"
    );
}

// ---------------------------------------------------------------------------
// The port-aware forms preserve the wake-absorption contract
// ---------------------------------------------------------------------------

#[test]
fn run_slice_with_ports_absorbs_a_wake_produced_during_the_slice() {
    let mut fx = Fixture::new();
    let a = fx.spawn();
    let b = fx.spawn();
    let mut scheduler = Scheduler::new();
    scheduler.enqueue(a);
    scheduler.enqueue(b);
    let mut runner = SendThenWait { a, b };

    // Slice 1: `a` finds an empty mailbox and blocks (leaving the run queue).
    assert_eq!(
        scheduler
            .run_slice_with_ports(&mut fx.table, &mut fx.ports, &mut runner)
            .expect("the runner never fails"),
        Some(a)
    );
    assert_eq!(
        fx.table.get(a).expect("a is live").state(),
        ProcessState::Waiting
    );
    assert!(!scheduler.run_queue().contains(a));

    // Slice 2: `b` sends to the blocked `a`, then blocks too. The wake is absorbed into the
    // very same sweep, so a `run_ready_with_ports` loop would not exit with `a` still pending.
    assert_eq!(
        scheduler
            .run_slice_with_ports(&mut fx.table, &mut fx.ports, &mut runner)
            .expect("the runner never fails"),
        Some(b)
    );
    assert_eq!(
        fx.table.get(a).expect("a is live").state(),
        ProcessState::Runnable,
        "the send woke `a`"
    );
    assert!(
        scheduler.run_queue().contains(a),
        "the port-aware slice re-enqueued `a` in the same sweep"
    );
}

#[test]
fn run_slice_with_ports_on_an_empty_queue_absorbs_a_pending_wake() {
    let mut fx = Fixture::new();
    let a = fx.spawn();
    let b = fx.spawn();
    // `a` is parked in `receive`; a send from *outside* a slice wakes it, but the orchestrator
    // has not absorbed that wake yet.
    fx.table
        .get_mut(a)
        .expect("a")
        .set_state(ProcessState::Waiting);
    let outcome = fx
        .table
        .deliver_simple(a, b.to_eterm(), make_small(42))
        .expect("a is live");
    assert_eq!(outcome, DeliverOutcome::Woke);

    let mut scheduler = Scheduler::new();
    assert!(scheduler.run_queue().is_empty());
    let mut runner = ExitRunner::new(ProcessId::new(9999, 0), make_small(0));
    assert_eq!(
        scheduler
            .run_slice_with_ports(&mut fx.table, &mut fx.ports, &mut runner)
            .expect("the runner never fails"),
        None,
        "an empty run queue yields no pid"
    );
    assert!(
        scheduler.run_queue().contains(a),
        "the empty-queue early return absorbed the pending wake"
    );
    assert!(runner.ran.is_empty());
}
