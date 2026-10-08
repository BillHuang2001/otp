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

//! Pre-emption and fairness under the cooperative reduction-quantum scheduler.
//!
//! Deliverable E3: the reduction-quantum model must actually hand the CPU over. This file
//! pins, with **exact measured slice counts** (no printouts, no wall-clock bounds):
//!
//! 1. two ready processes interleave strictly round-robin over a fixed slice horizon
//!    (`horizon = 10` -> 5/5; `horizon = 7` -> 4/3), each within one slice of the other;
//! 2. the same holds for 3 and 4 processes (`10` slices -> 4/3/3 and 3/3/2/2), with the
//!    assertion written so a hypothetical "one process runs to completion first" bug -- which
//!    would measure `10/0/0` (spread `10`, far outside the band `1`) -- FAILS it;
//! 3. [`RunOutcome::Wait`] leaves the ready queue: a process that blocks runs exactly once,
//!    is `Waiting`, is gone from the run queue, and the peer keeps running;
//! 4. [`RunOutcome::Yield`] requeues at the process's **own current** priority -- a demotion
//!    performed *during* the slice (`Priority::Low`) is honored at the requeue, so the peer's
//!    `Normal` level is served first;
//! 5. the truth about Milestone-1 pre-emption: it is **cooperative only**. The scheduler never
//!    interrupts a runner, so the interleaving is fixed by the runner's own yield points and is
//!    *invariant* under the reduction budget (`1` vs the crate default `4000` produce byte-
//!    identical schedules), and a runner may overrun its quantum without being stopped. The
//!    failure mode -- a runner that never returns hangs the single scheduler thread forever -- is
//!    stated as an `#[ignore]`d test, because it cannot be exercised without hanging the suite.
//!
//! # Anti-flakiness
//!
//! Everything here is **single-threaded and deterministic**: a fixed slice horizon
//! (`run_ready(..., Some(N))`), a pure `ProcessRunner` whose behaviour depends only on the pid
//! and its own call count, and exact integer assertions. There is no `std::thread::sleep`, no
//! channel, no barrier and no timing tolerance anywhere in this file; a fresh `ProcessTable`
//! per test makes the pid sequence deterministic too.
//!
//! `ProcessRunner` is the engine seam (the scheduler deliberately does not depend on
//! `beam_engine`), so the runners below stand in for the real execution engine: a
//! budget-respecting one models a compiled process that reaches a safepoint exactly when its
//! quantum is spent.

use beam_common::Result;
use beam_process::prelude::*;
use beam_term::prelude::AtomTable;

/// The per-slice reduction budget used by most tests here.
///
/// Small on purpose: a budget-respecting runner really consumes `budget` reductions of work,
/// and the assertions are about **slices**, not reductions, so the value only has to be a
/// plausible quantum (the invariance test later varies it across three orders of magnitude).
const QUANTUM: u32 = 7;

/// How far an overrunning runner exceeds its handed reduction budget per slice.
const OVERRUN_FACTOR: u32 = 8;

/// An empty process table with a fresh atom table behind its signal atoms.
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

// ---------------------------------------------------------------------------------------
// Per-pid counters (a `Vec` of pairs keeps the pid->count association explicit and ordered).
// ---------------------------------------------------------------------------------------

/// Adds `amount` to `pid`'s entry in `counts`, creating the entry when absent.
fn add(counts: &mut Vec<(ProcessId, u64)>, pid: ProcessId, amount: u64) {
    match counts.iter_mut().find(|(id, _)| *id == pid) {
        Some((_, total)) => *total += amount,
        None => counts.push((pid, amount)),
    }
}

/// `pid`'s total in `counts`, or `0` when it has no entry.
fn lookup(counts: &[(ProcessId, u64)], pid: ProcessId) -> u64 {
    counts
        .iter()
        .find(|(id, _)| *id == pid)
        .map(|(_, total)| *total)
        .unwrap_or(0)
}

/// The per-pid totals of `counts`, in the order of `pids`.
fn counts_for(counts: &[(ProcessId, u64)], pids: &[ProcessId]) -> Vec<u64> {
    pids.iter().map(|pid| lookup(counts, *pid)).collect()
}

/// `max - min` of a count vector (the fairness spread); `0` when empty.
fn spread(counts: &[u64]) -> u64 {
    let max = counts.iter().copied().max().unwrap_or(0);
    let min = counts.iter().copied().min().unwrap_or(0);
    max - min
}

/// The slice counts strict round-robin gives `procs` processes enqueued in order over
/// `horizon` slices: each slice serves the next process in rotation, so process `i` (0-based)
/// gets `ceil((horizon - i) / procs)` slices.
fn round_robin_counts(procs: usize, horizon: u64) -> Vec<u64> {
    let procs = procs as u64;
    (0..procs)
        .map(|i| {
            let remaining = horizon.saturating_sub(i);
            // `div_ceil` written out longhand: only `i == 0` can have `remaining == 0`.
            if remaining == 0 {
                0
            } else {
                (remaining + procs - 1) / procs
            }
        })
        .collect()
}

/// The run order strict round-robin produces over `horizon` slices: process index `i % procs`.
fn rotation(procs: usize, horizon: u64) -> Vec<usize> {
    (0..horizon as usize).map(|slice| slice % procs).collect()
}

/// A recorded run order translated to process indexes in `pids`.
fn order_indexes(order: &[ProcessId], pids: &[ProcessId]) -> Vec<usize> {
    order
        .iter()
        .map(|pid| {
            pids.iter()
                .position(|candidate| candidate == pid)
                .expect("every scheduled pid was spawned by this test")
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Test runner 1: a budget-respecting process (the real engine's contract).
// ---------------------------------------------------------------------------------------

/// A process that consumes exactly the reduction budget it is handed and then yields.
///
/// This is the shape of the real execution engine: one slice buys `budget` reductions, and the
/// runner returns [`RunOutcome::Yield`] as soon as that quantum is spent -- the scheduler itself
/// never interrupts it (a runner that kept going would never give the CPU up).
struct QuantumRunner {
    /// The budget the scheduler was configured with; every slice must be handed exactly this.
    budget: u32,
    /// Every slice, oldest first, as (pid, slice index). `len()` is the total slice count.
    slices: Vec<ProcessId>,
    /// Per-pid slice totals.
    slice_totals: Vec<(ProcessId, u64)>,
    /// Per-pid reduction totals (must be `slice_total * budget`).
    reduction_totals: Vec<(ProcessId, u64)>,
}

impl QuantumRunner {
    fn new(budget: u32) -> QuantumRunner {
        QuantumRunner {
            budget,
            slices: Vec::new(),
            slice_totals: Vec::new(),
            reduction_totals: Vec::new(),
        }
    }

    /// The whole run order, oldest first.
    fn order(&self) -> &[ProcessId] {
        &self.slices
    }

    /// How many slices `pid` was given.
    fn slices_for(&self, pid: ProcessId) -> u64 {
        lookup(&self.slice_totals, pid)
    }

    /// How many reductions `pid` actually executed across the run.
    fn reductions_for(&self, pid: ProcessId) -> u64 {
        lookup(&self.reduction_totals, pid)
    }
}

impl ProcessRunner for QuantumRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        _table: &mut ProcessTable,
        budget: u32,
    ) -> Result<RunOutcome> {
        assert_eq!(
            budget, self.budget,
            "the scheduler hands every slice the budget it was configured with"
        );

        self.slices.push(pid);
        add(&mut self.slice_totals, pid, 1);

        // Spend the quantum: exactly `budget` reductions of work, then give the CPU up. The
        // scheduler cannot interrupt a runner, so this `Yield` is the ONLY thing that makes a
        // peer run.
        let mut spent: u64 = 0;
        while spent < u64::from(budget) {
            spent += 1;
        }
        add(&mut self.reduction_totals, pid, spent);

        Ok(RunOutcome::Yield)
    }
}

/// One scripted measurement: `procs` processes enqueued in order at `Normal`, run for a fixed
/// `horizon` of slices with a per-slice budget of `budget`.
struct Run {
    /// The pids, in enqueue order.
    pids: Vec<ProcessId>,
    /// The recording runner.
    runner: QuantumRunner,
    /// The slice count `run_ready` reported (== `horizon`, since nobody ever blocks).
    executed: u64,
    /// The run queue length afterwards (nobody blocks, so every process is still ready).
    ready_after: usize,
}

/// Spawns `procs` processes, enqueues them in order, and runs exactly `horizon` slices with a
/// budget-respecting runner.
fn drive(procs: usize, horizon: u64, budget: u32) -> Run {
    assert!(procs > 0, "a schedule needs at least one process");
    let mut table = table();
    let mut scheduler = Scheduler::with_slice(budget);

    let mut pids = Vec::with_capacity(procs);
    for _ in 0..procs {
        let pid = table.spawn_default().expect("room in the pid space");
        scheduler.enqueue(pid);
        pids.push(pid);
    }

    let mut runner = QuantumRunner::new(budget);
    let executed = scheduler
        .run_ready(&mut table, &mut runner, Some(horizon))
        .expect("the runner never fails");

    assert_eq!(
        executed, horizon,
        "every slice in the horizon ran (a budget-respecting runner never blocks)"
    );
    for pid in &pids {
        assert_eq!(
            table.get(*pid).expect("the process is live").state(),
            ProcessState::Runnable,
            "a yielding process stays runnable"
        );
    }
    assert_eq!(
        scheduler.run_queue().len(),
        procs,
        "no process left the ready queue"
    );

    Run {
        pids,
        runner,
        executed,
        ready_after: scheduler.run_queue().len(),
    }
}

// ---------------------------------------------------------------------------------------
// 1. Two processes interleave strictly round-robin.
// ---------------------------------------------------------------------------------------

#[test]
fn two_processes_interleave_round_robin() {
    // An even horizon: the two processes get exactly half each.
    let run = drive(2, 10, QUANTUM);
    let measured = counts_for(&run.runner.slice_totals, &run.pids);

    assert_eq!(run.executed, 10);
    assert_eq!(run.ready_after, 2, "both processes are still ready");
    assert_eq!(
        measured,
        vec![5, 5],
        "over 10 slices each of two ready processes gets exactly 5"
    );
    assert_eq!(
        run.runner.slices_for(run.pids[0]),
        run.runner.slices_for(run.pids[1]),
        "an even horizon splits evenly"
    );
    assert!(spread(&measured) <= 1, "fairness band: {measured:?}");
    assert_eq!(
        order_indexes(run.runner.order(), &run.pids),
        rotation(2, 10),
        "strict alternation, in enqueue order"
    );
    // Every slice bought exactly one quantum of reductions for its process.
    assert_eq!(run.runner.reductions_for(run.pids[0]), 5 * u64::from(QUANTUM));
    assert_eq!(run.runner.reductions_for(run.pids[1]), 5 * u64::from(QUANTUM));

    // An odd horizon: the process enqueued first gets the extra slice.
    let run = drive(2, 7, QUANTUM);
    let measured = counts_for(&run.runner.slice_totals, &run.pids);
    assert_eq!(run.executed, 7);
    assert_eq!(
        measured,
        vec![4, 3],
        "over 7 slices the first process gets ceil(7/2) = 4 and the second floor(7/2) = 3"
    );
    assert_eq!(
        measured[0] - measured[1],
        1,
        "the band is exactly one slice for an odd horizon"
    );
    assert!(spread(&measured) <= 1, "fairness band: {measured:?}");
    assert_eq!(
        order_indexes(run.runner.order(), &run.pids),
        rotation(2, 7),
        "strict alternation, in enqueue order"
    );

    // A single slice horizon hands the CPU to the head of the queue only.
    let run = drive(2, 1, QUANTUM);
    assert_eq!(counts_for(&run.runner.slice_totals, &run.pids), vec![1, 0]);
}

// ---------------------------------------------------------------------------------------
// 2. Three and four processes stay within one slice of each other.
// ---------------------------------------------------------------------------------------

#[test]
fn several_processes_interleave_within_one_slice_of_each_other() {
    // The headline measurements, spelled out (see `round_robin_counts` below for the formula):
    //   3 processes, 10 slices -> 4 / 3 / 3
    //   4 processes, 10 slices -> 3 / 3 / 2 / 2
    for &(procs, horizon) in &[(3usize, 10u64), (4, 10), (3, 8), (4, 7), (5, 12)] {
        let run = drive(procs, horizon, QUANTUM);
        let measured = counts_for(&run.runner.slice_totals, &run.pids);
        let expected = round_robin_counts(procs, horizon);

        assert_eq!(run.executed, horizon);
        assert_eq!(
            measured, expected,
            "round-robin over {procs} processes for {horizon} slices"
        );
        assert_eq!(
            measured.iter().sum::<u64>(),
            horizon,
            "the measured slices account for the whole horizon"
        );
        // THE fairness assertion: a hypothetical bug that ran the first process to completion
        // before touching the others would measure e.g. [10, 0, 0] here -- spread 10 -- and
        // would fail this band.
        assert!(
            spread(&measured) <= 1,
            "every process must be within one slice of the others: {measured:?}"
        );
        assert_eq!(
            order_indexes(run.runner.order(), &run.pids),
            rotation(procs, horizon),
            "the run order is a plain rotation over the enqueued processes"
        );
    }

    // The exact expected vectors for the two headline cases, written out literally.
    assert_eq!(round_robin_counts(3, 10), vec![4, 3, 3]);
    assert_eq!(round_robin_counts(4, 10), vec![3, 3, 2, 2]);
    // And what the bug described above would have measured: a spread of the whole horizon.
    assert_eq!(spread(&[10, 0, 0]), 10);
}

// ---------------------------------------------------------------------------------------
// 3. `Wait` leaves the ready queue and is not rerun.
// ---------------------------------------------------------------------------------------

/// A runner that blocks exactly once, on its first slice of one designated pid.
///
/// It sets the process state to [`ProcessState::Waiting`] itself before reporting
/// [`RunOutcome::Wait`] -- the documented contract for a runner blocked in `receive`: the
/// scheduler deliberately does not guess the transition.
struct BlockOnceRunner {
    target: ProcessId,
    blocked: bool,
    order: Vec<ProcessId>,
    totals: Vec<(ProcessId, u64)>,
}

impl BlockOnceRunner {
    fn new(target: ProcessId) -> BlockOnceRunner {
        BlockOnceRunner {
            target,
            blocked: false,
            order: Vec::new(),
            totals: Vec::new(),
        }
    }

    fn count(&self, pid: ProcessId) -> u64 {
        lookup(&self.totals, pid)
    }
}

impl ProcessRunner for BlockOnceRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        self.order.push(pid);
        add(&mut self.totals, pid, 1);

        if pid == self.target && !self.blocked {
            self.blocked = true;
            table
                .get_mut(pid)
                .expect("the process is live")
                .set_state(ProcessState::Waiting);
            return Ok(RunOutcome::Wait);
        }
        Ok(RunOutcome::Yield)
    }
}

#[test]
fn wait_leaves_the_ready_queue_and_is_not_rerun() {
    let mut table = table();
    let mut scheduler = Scheduler::with_slice(QUANTUM);
    let blocked = table.spawn_default().expect("room in the pid space");
    let peer = table.spawn_default().expect("room in the pid space");
    scheduler.enqueue(blocked);
    scheduler.enqueue(peer);

    let mut runner = BlockOnceRunner::new(blocked);

    // Slice 1: `blocked` runs, blocks and leaves the queue; the remaining four slices of the
    // horizon all go to the peer.
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(5))
            .expect("the runner never fails"),
        5
    );
    assert_eq!(runner.order, vec![blocked, peer, peer, peer, peer]);
    assert_eq!(runner.count(blocked), 1, "the blocked process ran exactly once");
    assert_eq!(runner.count(peer), 4, "the peer ran every remaining slice");
    assert!(
        !scheduler.run_queue().contains(blocked),
        "a waiting process is not in the run queue"
    );
    assert!(
        scheduler.run_queue().contains(peer),
        "the peer keeps running"
    );
    assert_eq!(scheduler.run_queue().len(), 1);
    assert_eq!(
        table.get(blocked).expect("the process is live").state(),
        ProcessState::Waiting
    );
    assert!(
        !table
            .get(blocked)
            .expect("the process is live")
            .flags()
            .in_run_queue(),
        "the schedule-in step cleared the in-run-queue flag"
    );

    // The process is never rerun, however long the scheduler keeps going: it is not ready.
    let before = runner.count(blocked);
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(20))
            .expect("the runner never fails"),
        20
    );
    assert_eq!(
        runner.count(blocked),
        before,
        "a blocked process is never picked again"
    );
    assert_eq!(runner.count(peer), 24);
    assert_eq!(runner.order.len(), 25);
}

// ---------------------------------------------------------------------------------------
// 4. `Yield` requeues at the process's OWN current priority.
// ---------------------------------------------------------------------------------------

/// A runner that demotes its designated pid to [`Priority::Low`] during its first slice and
/// then yields, modelling `process_flag(priority, low)` inside a running process.
struct DemoteRunner {
    target: ProcessId,
    demoted: bool,
    order: Vec<ProcessId>,
}

impl ProcessRunner for DemoteRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        self.order.push(pid);
        if pid == self.target && !self.demoted {
            self.demoted = true;
            table
                .get_mut(pid)
                .expect("the process is live")
                .set_priority(Priority::Low);
        }
        Ok(RunOutcome::Yield)
    }
}

#[test]
fn yield_requeues_at_the_processes_own_current_priority() {
    let mut table = table();
    let mut scheduler = Scheduler::with_slice(QUANTUM);
    let demoted = table.spawn_default().expect("room in the pid space");
    let peer = table.spawn_default().expect("room in the pid space");
    scheduler.enqueue(demoted);
    scheduler.enqueue(peer);

    let mut runner = DemoteRunner {
        target: demoted,
        demoted: false,
        order: Vec::new(),
    };

    // One slice: `demoted` runs, lowers its own priority, yields.
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(1))
            .expect("the runner never fails"),
        1
    );
    assert_eq!(runner.order, vec![demoted]);
    assert_eq!(
        table.get(demoted).expect("the process is live").priority(),
        Priority::Low,
        "the PCB sees the demotion"
    );
    assert_eq!(
        scheduler.run_queue().priority_of(demoted),
        Some(Priority::Low),
        "the Yield requeued `demoted` at the priority it held AFTER the slice, not the one it \
         was dequeued with"
    );
    assert_eq!(
        scheduler.run_queue().priority_of(peer),
        Some(Priority::Normal),
        "the peer is untouched, still on Normal"
    );
    assert_eq!(scheduler.run_queue().len_at(Priority::Normal), 1);
    assert_eq!(scheduler.run_queue().len_at(Priority::Low), 1);

    // LOW is served last, so the next slice goes to the Normal peer even though `demoted` was
    // enqueued (at its new level) first.
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(1))
            .expect("the runner never fails"),
        1
    );
    assert_eq!(
        runner.order,
        vec![demoted, peer],
        "NORMAL outranks LOW, so the demoted process waits"
    );
}

// ---------------------------------------------------------------------------------------
// 5. Pre-emption is cooperative only.
// ---------------------------------------------------------------------------------------

/// The same scripted run as [`drive`], but with a runner that **ignores** the budget it is
/// handed: it records the budget (proving it flows through) and yields on its own schedule.
#[test]
fn the_budget_is_advisory_and_the_runner_owns_its_yield_points() {
    // Identical program, budgets three orders of magnitude apart. The scheduler never
    // interrupts a runner, so the budget cannot influence when a process gives the CPU up.
    let small = drive(2, 12, 1);
    let large = drive(2, 12, DEFAULT_REDUCTIONS_PER_SLICE);

    let small_counts = counts_for(&small.runner.slice_totals, &small.pids);
    let large_counts = counts_for(&large.runner.slice_totals, &large.pids);

    assert_eq!(small.executed, 12);
    assert_eq!(large.executed, 12);
    assert_eq!(
        small_counts,
        vec![6, 6],
        "budget 1: 12 slices split 6/6"
    );
    assert_eq!(
        large_counts,
        vec![6, 6],
        "budget 4000: the SAME 12 slices split 6/6 -- the budget changed nothing"
    );
    assert_eq!(
        small_counts, large_counts,
        "the interleaving is invariant under the reduction budget"
    );
    assert_eq!(
        order_indexes(small.runner.order(), &small.pids),
        order_indexes(large.runner.order(), &large.pids),
        "the run order is the runner's own yield schedule, not a function of the budget"
    );

    // The budget itself flows through faithfully: each slice really did buy `budget`
    // reductions, and the runner's ledger is exactly `slices * budget`.
    assert_eq!(small.runner.reductions_for(small.pids[0]), 6 * 1);
    assert_eq!(
        large.runner.reductions_for(large.pids[0]),
        6 * u64::from(DEFAULT_REDUCTIONS_PER_SLICE)
    );
    assert_eq!(
        small_counts.iter().sum::<u64>(),
        large_counts.iter().sum::<u64>(),
        "both runs account for exactly the 12 slices of the horizon"
    );
}

/// A runner that does far more work in a slice than the budget it was handed allows.
///
/// Nothing in the scheduler enforces the budget: there is no timer, no interrupt and no
/// safepoint check outside the runner. A long-running BIF/`NIF` (or a loop the engine has not
/// compiled a safepoint into) therefore overruns its quantum and the scheduler simply waits.
struct OverrunRunner {
    order: Vec<ProcessId>,
    /// "Work units" each process claims to have executed (>= 8x its reduction budget).
    claimed: Vec<(ProcessId, u64)>,
    /// Total runner invocations == total slices, since the runner always yields.
    calls: u64,
}

impl ProcessRunner for OverrunRunner {
    fn run(
        &mut self,
        pid: ProcessId,
        _table: &mut ProcessTable,
        budget: u32,
    ) -> Result<RunOutcome> {
        self.calls += 1;
        self.order.push(pid);
        // Deliberately ignore `budget` and run `OVERRUN_FACTOR` quanta of work in this one
        // slice. The scheduler is not consulted and cannot step in.
        add(
            &mut self.claimed,
            pid,
            u64::from(budget) * u64::from(OVERRUN_FACTOR),
        );
        Ok(RunOutcome::Yield)
    }
}

#[test]
fn a_runner_that_overruns_its_quantum_is_never_interrupted() {
    const BUDGET: u32 = 10;
    const HORIZON: u64 = 10;

    let mut table = table();
    let mut scheduler = Scheduler::with_slice(BUDGET);
    let a = table.spawn_default().expect("room in the pid space");
    let b = table.spawn_default().expect("room in the pid space");
    scheduler.enqueue(a);
    scheduler.enqueue(b);

    let mut runner = OverrunRunner {
        order: Vec::new(),
        claimed: Vec::new(),
        calls: 0,
    };
    assert_eq!(
        scheduler
            .run_ready(&mut table, &mut runner, Some(HORIZON))
            .expect("the runner never fails"),
        HORIZON
    );

    // One slice per runner call: overrunning the quantum does NOT produce extra slices, and the
    // scheduler neither truncated nor interrupted the runner.
    assert_eq!(runner.calls, HORIZON);
    assert_eq!(counts_for(&runner.claimed, &[a, b]), {
        let per_slice = u64::from(BUDGET) * u64::from(OVERRUN_FACTOR);
        vec![5 * per_slice, 5 * per_slice]
    });
    let claimed: u64 = runner.claimed.iter().map(|(_, total)| total).sum();
    assert_eq!(
        claimed,
        HORIZON * u64::from(BUDGET) * u64::from(OVERRUN_FACTOR),
        "every slice did the overrunning amount of work"
    );
    assert!(
        claimed > HORIZON * u64::from(BUDGET),
        "the run really did exceed the budget it was granted"
    );
    assert_eq!(
        order_indexes(&runner.order, &[a, b]),
        rotation(2, HORIZON),
        "the schedule is unchanged: fairness comes from the runner yielding, nothing else"
    );
    assert_eq!(
        scheduler.run_queue().len(),
        2,
        "no process was dropped by the overrun"
    );
}

/// The pathological runner: it never returns from `run`.
///
/// This is the "monopolizing process" the cooperative model cannot stop. It is used only by the
/// `#[ignore]`d failure-mode test below.
struct NeverYields;

impl ProcessRunner for NeverYields {
    fn run(
        &mut self,
        _pid: ProcessId,
        _table: &mut ProcessTable,
        _budget: u32,
    ) -> Result<RunOutcome> {
        // A real engine with no safepoint in a hot loop would spin here forever. There is no
        // mechanism for the scheduler to interrupt it: it is the same OS thread.
        loop {
            std::hint::spin_loop();
        }
    }
}

/// DOCUMENTED FAILURE MODE -- do **not** un-ignore this test, and never run `--ignored`.
///
/// It is the honest statement of Milestone-1 pre-emption: **the scheduler cannot pre-empt a
/// runner.** `Scheduler::run_ready(..., None)` runs until the ready queue is empty, and the
/// only way a slice ends is the runner returning `Yield` / `Wait` / `Suspend` / `Exit`. A
/// runner that never returns therefore hangs the single scheduler thread and nothing else on
/// that scheduler ever runs again -- the whole VM, if it is the only scheduler.
///
/// This cannot be tested directly (the assertion would never be reached, and the test harness
/// would hang), so the failure mode is pinned here as prose plus the exact call that exhibits
/// it. The *positive* half -- that the interleaving is entirely the runner's decision and is
/// unaffected by the reduction budget -- is asserted by
/// `the_budget_is_advisory_and_the_runner_owns_its_yield_points` and
/// `a_runner_that_overruns_its_quantum_is_never_interrupted`.
#[test]
#[ignore = "documented failure mode: this body spins forever by design; the scheduler has no \
            pre-emption mechanism, so running it would hang the test suite"]
fn a_runner_that_never_returns_would_hang_the_scheduler() {
    let mut table = table();
    let mut scheduler = Scheduler::with_slice(1);
    let pid = table.spawn_default().expect("room in the pid space");
    scheduler.enqueue(pid);

    let mut runner = NeverYields;
    // `NeverYields::run` never returns, so this call never returns: the scheduler is stuck
    // inside the runner for good and `pid` is never requeued, never mind the peers that would
    // be behind it. There is no timeout to escape it -- a `Some(n)` horizon would bound the
    // scheduler but not the runner, so even that would hang inside the first slice.
    let _ = scheduler.run_ready(&mut table, &mut runner, None);
}
