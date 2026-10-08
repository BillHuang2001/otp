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

//! Integration evidence for pid allocation and the round-robin run-queue order.
//!
//! Covers CONTEXT.md Test Strategy item 1: many spawns get distinct pids; a freed pid
//! number is reused with a **bumped serial** and the stale pid stops resolving; and a
//! recording [`ProcessRunner`] shows strict round-robin scheduling (each ready process runs
//! one slice in enqueue order, requeued at the tail on `Yield`), with a `Wait` process
//! leaving the queue and never being rerun.
//!
//! It also pins the **resume-on-send** contract: a message delivered to a blocked
//! (`Waiting`) receiver wakes it, and the scheduler absorbs that wake at the end of the
//! current slice so `run_ready` does not exit with a freshly-woken process still pending.

use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::{make_small, signed_value, AtomTable, Eterm};

/// An empty table with a fresh atom table behind its signal atoms.
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

/// A runner that records the order processes are run and yields — except the first time it
/// sees a process listed in `wait_for`, when it reports `Wait` (blocking).
struct Recorder {
    order: Vec<ProcessId>,
    wait_for: Vec<ProcessId>,
    seen: Vec<ProcessId>,
}

impl Recorder {
    fn new(wait_for: Vec<ProcessId>) -> Recorder {
        Recorder {
            order: Vec::new(),
            wait_for,
            seen: Vec::new(),
        }
    }
}

impl ProcessRunner for Recorder {
    fn run(
        &mut self,
        pid: ProcessId,
        _table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        self.order.push(pid);
        let first_sighting = !self.seen.contains(&pid);
        if first_sighting {
            self.seen.push(pid);
        }
        if first_sighting && self.wait_for.contains(&pid) {
            return Ok(RunOutcome::Wait);
        }
        Ok(RunOutcome::Yield)
    }
}

#[test]
fn many_spawns_get_distinct_pids() {
    let mut table = table();
    let mut pids = Vec::new();
    for _ in 0..100 {
        pids.push(table.spawn_default().expect("room in the pid space"));
    }

    let mut distinct = pids.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), pids.len(), "every spawn got a distinct pid");
    assert_eq!(table.len(), 100);
}

#[test]
fn a_freed_pid_number_is_reused_with_a_bumped_serial() {
    let mut table = table();
    let first = table.spawn_default().expect("room in the pid space");
    table.remove(first).expect("the pid was live");
    assert!(!table.contains(first));

    let reused = table.spawn_default().expect("the freed number is reused");
    assert_eq!(
        reused.number(),
        first.number(),
        "the freed number is handed out again"
    );
    assert!(
        reused.serial() > first.serial(),
        "reusing a slot bumps its serial"
    );
    assert!(table.contains(reused));
    // The stale pid names the previous occupant and must no longer resolve.
    assert!(!table.contains(first));
    assert!(table.get(first).is_none());
}

#[test]
fn run_ready_schedules_round_robin_in_enqueue_order() {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    let c = table.spawn_default().expect("c");
    scheduler.enqueue(a);
    scheduler.enqueue(b);
    scheduler.enqueue(c);

    let mut runner = Recorder::new(vec![]);
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(3))
            .expect("the runner never fails"),
        3
    );
    assert_eq!(runner.order, vec![a, b, c], "one slice each, in enqueue order");

    // Every yield requeues at the tail, so the next three slices repeat the order.
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(3))
            .expect("the runner never fails"),
        3
    );
    assert_eq!(runner.order, vec![a, b, c, a, b, c]);
}

#[test]
fn a_waiting_process_leaves_the_run_queue_and_is_not_rerun() {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    scheduler.enqueue(a);
    scheduler.enqueue(b);

    let mut runner = Recorder::new(vec![b]);
    // `a` yields (and is requeued); `b` waits (and leaves the queue).
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(2))
            .expect("the runner never fails"),
        2
    );
    assert_eq!(runner.order, vec![a, b]);
    assert!(!scheduler.run_queue().contains(b));
    assert!(scheduler.run_queue().contains(a));

    // Only `a` keeps running afterwards.
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(2))
            .expect("the runner never fails"),
        2
    );
    assert_eq!(runner.order, vec![a, b, a, a]);
}

/// A runner modelling two processes: `a` blocks on an empty receive until `b` sends it `42`.
struct ResumeRunner {
    a: ProcessId,
    b: ProcessId,
    a_runs: u32,
    b_runs: u32,
    received: Option<Eterm>,
}

impl ResumeRunner {
    fn new(a: ProcessId, b: ProcessId) -> ResumeRunner {
        ResumeRunner {
            a,
            b,
            a_runs: 0,
            b_runs: 0,
            received: None,
        }
    }
}

impl ProcessRunner for ResumeRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        if pid == self.a {
            self.a_runs += 1;
            let process = table.get_mut(pid).expect("a is live");
            match process.receive(|message| message.payload() == make_small(42)) {
                Some(message) => {
                    self.received = Some(message.payload());
                    Ok(RunOutcome::Exit(make_small(0)))
                }
                None => {
                    // Block on the empty receive: the runner owns the state transition.
                    process.set_state(ProcessState::Waiting);
                    Ok(RunOutcome::Wait)
                }
            }
        } else if pid == self.b {
            self.b_runs += 1;
            // Send the wake-up message, then block `b` itself.
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

#[test]
fn a_send_wakes_a_blocked_receiver_in_the_same_sweep() {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    scheduler.enqueue(a);
    scheduler.enqueue(b);

    let mut runner = ResumeRunner::new(a, b);

    // Slice 1: `a` finds an empty mailbox and blocks (leaving the run queue).
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(1))
            .expect("the runner never fails"),
        1
    );
    assert_eq!(
        table.get(a).expect("a is live").state(),
        ProcessState::Waiting
    );
    assert!(!scheduler.run_queue().contains(a));

    // Slice 2: `b` sends to the blocked `a`, then blocks too. The wake is absorbed into the
    // very same sweep, so `run_ready` does not exit with `a` still pending.
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(1))
            .expect("the runner never fails"),
        1
    );
    assert_eq!(
        table.get(a).expect("a is live").state(),
        ProcessState::Runnable,
        "the send woke `a`"
    );
    assert!(
        scheduler.run_queue().contains(a),
        "the same sweep re-enqueued `a`"
    );

    // Now `a` runs again, consumes the message and exits.
    scheduler
        .run_ready(&mut table, &mut runner, None)
        .expect("the runner never fails");
    assert_eq!(runner.received, Some(make_small(42)));
    assert_eq!(runner.a_runs, 2, "`a` ran once to block and once to receive");
    assert_eq!(runner.b_runs, 1);
    assert!(!table.contains(a), "`a` exited after receiving");
}

/// A runner driving a two-process counter ping-pong to a fixed `limit`.
///
/// Each process receives the current counter, and — while it is below `limit` — forwards
/// `value + 1` to its peer and blocks; the process that receives `limit` records it and exits.
/// Every forward send wakes the peer, so the chain only continues because of resume-on-send.
struct PingPong {
    a: ProcessId,
    b: ProcessId,
    limit: i64,
    final_value: Option<i64>,
    runs: u32,
}

impl ProcessRunner for PingPong {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        self.runs += 1;
        let peer = if pid == self.a { self.b } else { self.a };

        let value = {
            let process = table.get_mut(pid).expect("the process is live");
            match process.receive(|_| true) {
                Some(message) => signed_value(message.payload()),
                None => {
                    // No work queued: block (the runner owns the state transition).
                    process.set_state(ProcessState::Waiting);
                    return Ok(RunOutcome::Wait);
                }
            }
        };

        if value < self.limit {
            table
                .deliver_simple(peer, pid.to_eterm(), make_small(value + 1))
                .expect("the peer is live and small ints always fit");
            table
                .get_mut(pid)
                .expect("the process is live")
                .set_state(ProcessState::Waiting);
            Ok(RunOutcome::Wait)
        } else {
            self.final_value = Some(value);
            Ok(RunOutcome::Exit(make_small(0)))
        }
    }
}

#[test]
fn two_processes_ping_pong_until_the_bound() {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    scheduler.enqueue(a);
    scheduler.enqueue(b);

    // Kick off: hand `a` the counter at zero (it is runnable, so no wake is produced).
    table
        .deliver_simple(a, b.to_eterm(), make_small(0))
        .expect("a is live");

    let mut runner = PingPong {
        a,
        b,
        limit: 5,
        final_value: None,
        runs: 0,
    };
    let slices = scheduler
        .run_ready(&mut table, &mut runner, None)
        .expect("the runner never fails");

    assert_eq!(runner.final_value, Some(5), "the counter reached the bound");
    assert_eq!(slices, 6, "one slice per forward step, each ending in a block");
    assert_eq!(runner.runs, 6);
    assert!(
        !table.contains(b),
        "the process that received the bound exited"
    );
    // The peer is parked in `receive`, waiting for a message that never comes.
    assert_eq!(
        table.get(a).expect("a is live").state(),
        ProcessState::Waiting
    );
}