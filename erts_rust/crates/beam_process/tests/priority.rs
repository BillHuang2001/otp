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

//! Integration evidence for the four-level scheduling priorities and the `process_flag`
//! PCB support (`priority`, `sensitive`, `save_calls`).
//!
//! Mirrors the recording-`ProcessRunner` pattern of `tests/scheduler.rs`: the run queue is
//! served highest-priority first (MAX → HIGH → NORMAL → LOW, FIFO within a level), a process
//! is requeued at *its own* current priority on a yield (so a demotion made during a slice
//! takes effect at the next requeue), and a woken process resumes at its own priority.

use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::{make_small, AtomTable};

/// An empty table with a fresh atom table behind its signal atoms.
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

/// A runner that records the order processes are run and then **blocks** (leaves the run
/// queue). Used to show the full MAX→NORMAL→LOW precedence without the starvation a
/// perpetually-yielding higher-priority process would cause.
struct BlockingRecorder {
    order: Vec<ProcessId>,
}

impl BlockingRecorder {
    fn new() -> BlockingRecorder {
        BlockingRecorder { order: Vec::new() }
    }
}

impl ProcessRunner for BlockingRecorder {
    fn run(
        &mut self,
        pid: ProcessId,
        _table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        self.order.push(pid);
        Ok(RunOutcome::Wait)
    }
}

/// A runner that records the order processes are run and always yields (stays runnable), so
/// it exercises the requeue-at-own-priority path.
struct YieldRecorder {
    order: Vec<ProcessId>,
    /// A pid whose priority is set to `demote_to` on its first slice, then yielded.
    demote: Option<(ProcessId, Priority)>,
    demoted: bool,
}

impl YieldRecorder {
    fn new() -> YieldRecorder {
        YieldRecorder {
            order: Vec::new(),
            demote: None,
            demoted: false,
        }
    }

    fn with_demotion(pid: ProcessId, priority: Priority) -> YieldRecorder {
        YieldRecorder {
            order: Vec::new(),
            demote: Some((pid, priority)),
            demoted: false,
        }
    }
}

impl ProcessRunner for YieldRecorder {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        self.order.push(pid);
        if let Some((target, priority)) = self.demote {
            if target == pid && !self.demoted {
                self.demoted = true;
                table
                    .get_mut(pid)
                    .expect("the demoted process is live")
                    .set_priority(priority);
            }
        }
        Ok(RunOutcome::Yield)
    }
}

/// Spawns three processes, sets one to each of MAX / NORMAL / LOW on the PCB, and enqueues
/// them in "lowest precedence first" order (to prove the level wins, not insertion).
fn three_priority_processes(table: &mut ProcessTable, scheduler: &mut Scheduler) -> (ProcessId, ProcessId, ProcessId) {
    let low = table.spawn_default().expect("low");
    let normal = table.spawn_default().expect("normal");
    let max = table.spawn_default().expect("max");

    // The scheduler requeues on the *PCB's* priority, so the field and the enqueue level must
    // agree for the level to persist across slices.
    table.get_mut(max).expect("max is live").set_priority(Priority::Max);
    table
        .get_mut(normal)
        .expect("normal is live")
        .set_priority(Priority::Normal);
    table.get_mut(low).expect("low is live").set_priority(Priority::Low);

    scheduler.enqueue_with_priority(low, Priority::Low);
    scheduler.enqueue_with_priority(normal, Priority::Normal);
    scheduler.enqueue_with_priority(max, Priority::Max);
    (max, normal, low)
}

#[test]
fn higher_priority_runs_first_and_repeats() {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let (max, normal, low) = three_priority_processes(&mut table, &mut scheduler);

    // Each process runs once and then blocks (leaves the queue), so MAX→NORMAL→LOW is fully
    // observable in order.
    let mut runner = BlockingRecorder::new();
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(3))
            .expect("the runner never fails"),
        3
    );
    assert_eq!(
        runner.order,
        vec![max, normal, low],
        "MAX precedes NORMAL precedes LOW"
    );

    // Re-arm every level (scrambled again) and re-run: the precedence holds on the next round.
    scheduler.enqueue_with_priority(low, Priority::Low);
    scheduler.enqueue_with_priority(normal, Priority::Normal);
    scheduler.enqueue_with_priority(max, Priority::Max);
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(3))
            .expect("the runner never fails"),
        3
    );
    assert_eq!(
        runner.order,
        vec![max, normal, low, max, normal, low],
        "the MAX→NORMAL→LOW order repeats on the next round"
    );
}

#[test]
fn fifo_within_a_level() {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    let c = table.spawn_default().expect("c");

    // All three are normal by default; `enqueue` means "normal".
    scheduler.enqueue(a);
    scheduler.enqueue(b);
    scheduler.enqueue(c);

    let mut runner = YieldRecorder::new();
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(3))
            .expect("the runner never fails"),
        3
    );
    assert_eq!(runner.order, vec![a, b, c], "FIFO within the normal level");

    // Requeued at the tail of the same level, so the order repeats.
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(3))
            .expect("the runner never fails"),
        3
    );
    assert_eq!(runner.order, vec![a, b, c, a, b, c]);
}

#[test]
fn demotion_moves_the_process_to_the_low_level() {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let a = table.spawn_default().expect("a");
    let b = table.spawn_default().expect("b");
    scheduler.enqueue(a);
    scheduler.enqueue(b);

    // On `a`'s first slice the runner demotes it to LOW and yields.
    let mut runner = YieldRecorder::with_demotion(a, Priority::Low);
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(3))
            .expect("the runner never fails"),
        3
    );

    // (1) `a` is not re-run until `b` has run — it was requeued on the LOW level.
    assert_eq!(
        runner.order,
        vec![a, b, b],
        "the demoted `a` yields once, then serves behind `b`"
    );
    // (2) The run queue holds `a` at LOW and `b` at NORMAL.
    assert_eq!(scheduler.run_queue().priority_of(a), Some(Priority::Low));
    assert_eq!(scheduler.run_queue().priority_of(b), Some(Priority::Normal));
}

#[test]
fn a_woken_process_resumes_at_its_own_priority() {
    let mut table = table();
    let mut scheduler = Scheduler::new();
    let pid = table.spawn_default().expect("a process");

    // Park it: LOW priority, blocked in a receive.
    {
        let process = table.get_mut(pid).expect("the process is live");
        process.set_priority(Priority::Low);
        process.set_state(ProcessState::Waiting);
    }

    // A send wakes the blocked LOW-priority process (the caller owns the wake absorption).
    let outcome = table
        .deliver_simple(pid, pid.to_eterm(), make_small(7))
        .expect("the process is live and the immediate fits");
    assert_eq!(outcome, DeliverOutcome::Woke);
    assert_eq!(scheduler.resume_woken(&mut table), 1);

    assert_eq!(
        scheduler.run_queue().priority_of(pid),
        Some(Priority::Low),
        "the woken process is enqueued at its own priority, not normal"
    );
    assert_eq!(
        table.get(pid).expect("the process is live").state(),
        ProcessState::Runnable
    );
}

#[test]
fn table_set_priority_reaches_the_pcb_and_rejects_unknown_pids() {
    let mut table = table();
    let pid = table.spawn_default().expect("a process");
    assert_eq!(table.get(pid).expect("live").priority(), Priority::Normal);

    // `ProcessTable::set_priority` delegates to the PCB's own setter.
    table
        .set_priority(pid, Priority::High)
        .expect("the process is live");
    assert_eq!(table.get(pid).expect("live").priority(), Priority::High);

    // An unknown pid is a typed `TermNotFound`, not a panic.
    let unknown = ProcessId::new(9999, 0);
    assert!(matches!(
        table.set_priority(unknown, Priority::Low),
        Err(beam_common::BeamError::TermNotFound)
    ));
}
