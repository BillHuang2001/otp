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

//! The scheduler loop and the execution-engine seam: [`Scheduler`], [`RunOutcome`] and
//! [`ProcessRunner`].
//!
//! Milestone-1 runs **one** scheduler on **one** OS thread: it pops a pid from the run
//! queue, runs it for one reduction slice and interprets the outcome (requeue on yield,
//! leave the queue on wait/suspend, terminate on exit).
//!
//! # Scheduling order
//!
//! The run queue is served **highest-priority first** (MAX → HIGH → NORMAL → LOW, FIFO
//! within a level — see [`crate::run_queue::RunQueue`]). A process is requeued at **its own**
//! current priority when it yields, so a `process_flag(priority, …)` demotion made during a
//! slice takes effect at that process's next requeue, and a woken process resumes at its own
//! priority too.
//!
//! # The engine seam
//!
//! The scheduler must not know how a process executes, so the execution engine is reached
//! through the [`ProcessRunner`] trait rather than a dependency: `beam_engine` is **not** a
//! dependency of this crate (it depends on `beam_loader` only), and `beam_rt` supplies a
//! runner implemented over the engine's `ExecutionContext`. That keeps the Stage-1/Stage-5
//! DAG acyclic (see the crate `CONTEXT.md`, Design Decisions).
//!
//! # Resume-on-send: the `beam_rt` orchestration contract
//!
//! A single-threaded orchestrator (`beam_rt`) drives processes like this:
//!
//! * **spawn** ⇒ `let pid = table.spawn_default()?; scheduler.enqueue(pid);`
//! * **a runner that blocks on an empty `receive`** must set the process state to
//!   [`ProcessState::Waiting`] itself (via `table.get_mut(pid)`) *before* returning
//!   [`RunOutcome::Wait`] — the scheduler deliberately does not guess the transition.
//! * **send from inside a runner** ⇒ `table.deliver(_simple)(to, from, term)`; a send that
//!   wakes a blocked receiver is absorbed by [`Scheduler::resume_woken`] at the end of the
//!   slice, so the orchestrator need do nothing else.
//! * **send from outside a slice** ⇒ after `table.deliver(_simple)(…)` call
//!   `scheduler.resume_woken(&mut table)` so the woken receiver is enqueued.
//! * **run** ⇒ `scheduler.run_ready(&mut table, &mut runner, None)`.
//!
//! # Port-aware variants (the port-monitor cleanup adoption)
//!
//! An orchestrator that also owns a [`PortTable`] should drive the scheduler through
//! [`Scheduler::run_slice_with_ports`]/[`Scheduler::run_ready_with_ports`] (and, at the
//! lower level, [`run_one_with_ports`]/[`slice_begin_with_ports`]/[`slice_end_with_ports`]).
//! Those thread `&mut PortTable` into the schedule-in and schedule-out steps, so a process
//! that dies during a slice — or is terminated at schedule-in by a queued signal — also has
//! its matching *incoming* port-monitor entries removed from the port table (the last step of
//! `ProcessTable::terminate_into`). The port-less forms delegate with `None` and behave
//! exactly as before; they remain for callers that hold no port table.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_process.c`
//! (`scheduler_wait`/`schedule`), `erl_vm.h` (`CONTEXT_REDS`).

use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::pid::ProcessId;
use crate::port::PortTable;
use crate::process::ProcessState;
use crate::progress::ThreadProgress;
use crate::ptab::ProcessTable;
use crate::run_queue::{Priority, RunQueue};

/// The number of reductions a process runs before being pre-empted (the C
/// `CONTEXT_REDS`).
///
/// This value is handed to [`ProcessRunner::run`] as its `budget`. The **runner** owns the
/// reduction count — the scheduler never counts reductions itself — and pre-emption is
/// **cooperative** in Milestone 1: a runner that ignores `budget` and never returns
/// [`RunOutcome::Yield`] is never pre-empted by the scheduler. See [`ProcessRunner::run`] for
/// the full contract a runner must honor.
pub const DEFAULT_REDUCTIONS_PER_SLICE: u32 = 4000;

/// The default maximum number of reductions a parked-table scheduler runs **while holding the
/// process-table lock** (see [`crate::smp`]).
///
/// The SMP pool splits a slice's reduction budget into quanta of at most this many reductions and
/// **releases the table lock between quanta**, so another process (or a cross-thread send / timer /
/// dirty completion) can make progress *inside* another process's slice instead of waiting for the
/// whole slice to finish. The slice semantics are unchanged — the process keeps its own 4000
/// reduction budget, stays out of the run queue for the whole slice, and is requeued exactly once,
/// at its own priority — only the duration of one lock hold is bounded.
///
/// A runner that exhausts the budget it is handed (the real execution engine) therefore behaves
/// exactly as before; the only new nuance is that a runner returning `Yield` **before** spending
/// its quantum is continued within the same slice instead of being requeued (the pool cannot
/// observe how much of a quantum a runner consumed — see
/// `crate::smp::worker::run_locked_quanta`).
///
/// The trade-off is the number of lock acquisitions per slice (a slice of
/// [`DEFAULT_REDUCTIONS_PER_SLICE`] costs `ceil(4000 / 256) = 16` acquisitions instead of one): an
/// uncontended `parking_lot` lock is a handful of instructions, so the cost is negligible next to
/// the contention it removes.
pub const DEFAULT_LOCK_QUANTUM: u32 = 256;

/// What happened during one reduction slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The process used up its reductions and is still runnable: requeue it at the tail.
    Yield,
    /// The process blocked (e.g. an empty `receive`) and left the run queue.
    Wait,
    /// The process was suspended from outside and left the run queue.
    Suspend,
    /// The process terminated with this reason.
    Exit(Eterm),
}

/// The execution-engine seam: something that can run a process for a reduction budget.
///
/// Implemented by `beam_rt`/`beam_engine` over the engine's `ExecutionContext`; the
/// scheduler itself depends only on this trait.
///
/// # The runner contract (BINDING)
///
/// The runner **accounts reductions itself** — the scheduler never inspects or resets
/// `Process::reductions` — and must report the slice's outcome through [`RunOutcome`]:
///
/// * [`RunOutcome::Yield`] once it has spent (or is about to spend) its `budget`, so the
///   scheduler requeues the process at its own current priority;
/// * [`RunOutcome::Wait`]/[`RunOutcome::Suspend`] when the process blocks (the runner sets the
///   PCB state itself before returning — the scheduler deliberately does not guess it);
/// * [`RunOutcome::Exit`]`(reason)` when the process terminates.
///
/// `budget` is an **upper bound the runner is expected to honor**; it is not enforced. Pre-emption
/// is **cooperative** in Milestone 1: a runner that never returns `Yield` runs forever inside the
/// single runner call the scheduler makes, because the scheduler cannot interrupt it.
pub trait ProcessRunner {
    /// Runs process `pid` for up to `budget` reductions, mutating it — and, when needed,
    /// other processes through `table` — and reports what happened.
    ///
    /// # Errors
    ///
    /// Returns whatever the engine raises as a `BeamError`.
    fn run(&mut self, pid: ProcessId, table: &mut ProcessTable, budget: u32) -> Result<RunOutcome>;
}

/// The single-scheduler run loop.
pub struct Scheduler {
    run_queue: RunQueue,
    reductions_per_slice: u32,
    progress: ThreadProgress,
    slices: u64,
}

impl Scheduler {
    /// A scheduler with [`DEFAULT_REDUCTIONS_PER_SLICE`] reductions per slice.
    pub fn new() -> Scheduler {
        Scheduler::with_slice(DEFAULT_REDUCTIONS_PER_SLICE)
    }

    /// A scheduler with an explicit per-slice reduction budget.
    pub fn with_slice(reductions_per_slice: u32) -> Scheduler {
        Scheduler {
            run_queue: RunQueue::new(),
            reductions_per_slice,
            progress: ThreadProgress::new(),
            slices: 0,
        }
    }

    /// The per-slice reduction budget.
    pub fn reductions_per_slice(&self) -> u32 {
        self.reductions_per_slice
    }

    /// The run queue.
    pub fn run_queue(&self) -> &RunQueue {
        &self.run_queue
    }

    /// The run queue, mutably.
    pub fn run_queue_mut(&mut self) -> &mut RunQueue {
        &mut self.run_queue
    }

    /// Makes `pid` runnable by appending it to the run queue at **normal** priority.
    ///
    /// This is the ordinary spawn/requeue path; use [`Scheduler::enqueue_with_priority`] to
    /// place a process on a non-normal level.
    pub fn enqueue(&mut self, pid: ProcessId) {
        self.run_queue.enqueue(pid);
    }

    /// Makes `pid` runnable by appending it to the run queue at `priority`.
    pub fn enqueue_with_priority(&mut self, pid: ProcessId, priority: Priority) {
        self.run_queue.enqueue_with_priority(pid, priority);
    }

    /// Changes `pid`'s scheduling priority **and** moves it to the matching run-queue level if
    /// it is currently queued at a different one.
    ///
    /// This is the "a priority change reaches the run queue" path that `process_flag(priority,
    /// P)` and `spawn_opt(..., [{priority, P}])` need. The PCB priority alone only takes effect
    /// at the process's **next** requeue, so a priority change made on a process that is
    /// *sitting in a run queue* (e.g. a promotion by another process) would otherwise be
    /// deferred by up to a full slice. The order is deliberate:
    ///
    /// 1. Update the PCB through [`ProcessTable::set_priority`]. An **unknown** pid (the process
    ///    died) is a `TermNotFound` error, and the run queue is left **untouched** — nothing is
    ///    enqueued or moved before the PCB accepts the change.
    /// 2. If `pid` is queued at a level other than `priority`, remove it from that level and
    ///    append it to the **tail** of `priority`'s FIFO ([`RunQueue::reprioritize`]). A pid
    ///    that is not queued at all (it is running, in flight, waiting, suspended or already
    ///    terminated) or is already at `priority` is not touched: never double-enqueue, never
    ///    insert a pid that is not in the queue.
    ///
    /// # Errors
    ///
    /// Propagates [`ProcessTable::set_priority`]'s `TermNotFound` for an unknown pid.
    pub fn set_priority(
        &mut self,
        table: &mut ProcessTable,
        pid: ProcessId,
        priority: Priority,
    ) -> Result<()> {
        table.set_priority(pid, priority)?;
        self.run_queue.reprioritize(pid, priority);
        Ok(())
    }

    /// The thread-progress tracker.
    pub fn progress(&self) -> &ThreadProgress {
        &self.progress
    }

    /// The thread-progress tracker, mutably.
    pub fn progress_mut(&mut self) -> &mut ThreadProgress {
        &mut self.progress
    }

    /// Runs at most one slice: pop one pid, run it, then requeue (`Yield`), drop
    /// (`Wait`/`Suspend`) or terminate (`Exit`) it. Returns the pid that ran, or `None`
    /// when the run queue was empty.
    ///
    /// After the outcome is applied the scheduler **absorbs the table's wake list**: a send
    /// made during the slice that woke a blocked receiver is turned into a run-queue entry
    /// (see [`Scheduler::resume_woken`]), so `run_ready` does not exit while a freshly-woken
    /// process is still pending.
    ///
    /// This form has no [`PortTable`] handle: a process that dies during the slice has its
    /// outgoing port monitors drained but leaves the matching incoming entries in the port
    /// table. A caller holding the port table should use [`Scheduler::run_slice_with_ports`].
    ///
    /// # Errors
    ///
    /// Propagates the runner's and the table's errors.
    pub fn run_slice<R: ProcessRunner>(
        &mut self,
        table: &mut ProcessTable,
        runner: &mut R,
    ) -> Result<Option<ProcessId>> {
        self.run_slice_inner(table, None, runner)
    }

    /// [`Scheduler::run_slice`] with the [`PortTable`] in hand: a process that terminates
    /// during the slice also has its matching *incoming* port monitors removed from `ports`.
    ///
    /// This is the entry point an orchestrator that owns both tables (`beam_rt`) should use —
    /// the port-less [`Scheduler::run_slice`] cannot reach the port table, so the monitor a
    /// dying process held on a port would stay listed by `erlang:port_info(Port, monitored_by)`
    /// for a process that no longer exists.
    ///
    /// The slice semantics are otherwise identical: pop one pid, run it, then requeue
    /// (`Yield`), drop (`Wait`/`Suspend`) or terminate (`Exit`) it, and absorb the table's wake
    /// list (see [`Scheduler::run_slice`]).
    ///
    /// `ProcessTable` and `PortTable` are distinct types passed in separately, so `table` and
    /// `ports` are disjoint borrows.
    ///
    /// # Errors
    ///
    /// Propagates the runner's and the table's errors.
    pub fn run_slice_with_ports<R: ProcessRunner>(
        &mut self,
        table: &mut ProcessTable,
        ports: &mut PortTable,
        runner: &mut R,
    ) -> Result<Option<ProcessId>> {
        self.run_slice_inner(table, Some(ports), runner)
    }

    /// The shared body of [`Scheduler::run_slice`] and [`Scheduler::run_slice_with_ports`]:
    /// `ports` is `Some` only when the caller holds a [`PortTable`].
    fn run_slice_inner<R: ProcessRunner>(
        &mut self,
        table: &mut ProcessTable,
        ports: Option<&mut PortTable>,
        runner: &mut R,
    ) -> Result<Option<ProcessId>> {
        let mut ports = ports;
        let Some(pid) = self.run_queue.dequeue() else {
            // Nothing to run, but a wake produced by a send *outside* a slice (which the
            // orchestrator may not have absorbed yet) must still be turned into a run-queue
            // entry; absorbing here keeps a direct `run_slice` caller correct.
            self.resume_woken(table);
            return Ok(None);
        };

        // Read the budget into a local *before* the closure below borrows the run queue.
        let reductions = self.reductions_per_slice;
        let mut enqueue = |pid, priority| self.run_queue.enqueue_with_priority(pid, priority);
        match ports.as_deref_mut() {
            Some(ports) => run_one_with_ports(pid, table, ports, runner, reductions, &mut enqueue)?,
            None => run_one(pid, table, runner, reductions, &mut enqueue)?,
        };
        self.slices = self.slices.wrapping_add(1);

        // Absorb every wake produced during the slice (a send to a blocked receiver, or a
        // signal/trapped-`EXIT`/`{'DOWN'}` message) into this same sweep.
        self.resume_woken(table);

        Ok(Some(pid))
    }

    /// Turns the table's pending wakes (blocked processes a delivery/signal just made
    /// runnable) into run-queue entries, returning how many processes were enqueued.
    ///
    /// A woken pid is enqueued only when it is still a **live** process whose state is
    /// `Runnable` and that is not already in a run queue — a process can be woken and then
    /// terminated (or re-blocked) within the same slice, and a `Yield` already requeued the
    /// process that just ran, so its stale wake must not double-enqueue it. A woken process
    /// resumes at **its own** priority (not normal).
    ///
    /// This is the hook `beam_rt` calls after it sends to a blocked process **outside** a
    /// scheduler slice (`run_slice` calls it automatically at each slice's end). See the
    /// crate `CONTEXT.md` for the full single-threaded orchestration contract.
    pub fn resume_woken(&mut self, table: &mut ProcessTable) -> usize {
        drain_woken_into(table, |pid, priority| {
            self.run_queue.enqueue_with_priority(pid, priority)
        })
    }
    /// Runs slices until the run queue empties (or `max_slices` have run), returning the
    /// number of slices executed.
    ///
    /// The port-less form; a caller holding a [`PortTable`] should use
    /// [`Scheduler::run_ready_with_ports`] so a death inside any slice cleans the dying
    /// process's incoming port monitors too.
    ///
    /// # Errors
    ///
    /// Propagates the runner's and the table's errors.
    pub fn run_ready<R: ProcessRunner>(
        &mut self,
        table: &mut ProcessTable,
        runner: &mut R,
        max_slices: Option<u64>,
    ) -> Result<u64> {
        self.run_ready_inner(table, None, runner, max_slices)
    }

    /// [`Scheduler::run_ready`] with the [`PortTable`] in hand: every slice runs through
    /// [`Scheduler::run_slice_with_ports`], so a process that dies inside any of them cleans
    /// its incoming port monitors.
    ///
    /// This is the port-aware counterpart an orchestrator that owns both tables should drive;
    /// see [`Scheduler::run_slice_with_ports`] for why the port-less form is not enough.
    ///
    /// # Errors
    ///
    /// Propagates the runner's and the table's errors.
    pub fn run_ready_with_ports<R: ProcessRunner>(
        &mut self,
        table: &mut ProcessTable,
        ports: &mut PortTable,
        runner: &mut R,
        max_slices: Option<u64>,
    ) -> Result<u64> {
        self.run_ready_inner(table, Some(ports), runner, max_slices)
    }

    /// The shared body of [`Scheduler::run_ready`] and [`Scheduler::run_ready_with_ports`]:
    /// `ports` is `Some` only when the caller holds a [`PortTable`].
    fn run_ready_inner<R: ProcessRunner>(
        &mut self,
        table: &mut ProcessTable,
        ports: Option<&mut PortTable>,
        runner: &mut R,
        max_slices: Option<u64>,
    ) -> Result<u64> {
        let mut ports = ports;
        let mut executed: u64 = 0;
        while !self.run_queue.is_empty() {
            if let Some(max) = max_slices {
                if executed >= max {
                    break;
                }
            }
            match self.run_slice_inner(table, ports.as_deref_mut(), runner)? {
                Some(_) => executed = executed.wrapping_add(1),
                None => break,
            }
        }
        Ok(executed)
    }
}

/// Runs the already-dequeued process `pid` through one reduction slice and applies the
/// outcome, calling `enqueue(pid, priority)` to requeue it on a yield.
///
/// Returns `Ok(Some(outcome))`, or `Ok(None)` when there was nothing to run (the pid was
/// terminated concurrently, or servicing its pending signals terminated it). Callers own slice
/// accounting and the wake sweep.
///
/// The slice is [`slice_begin`] → one runner call for the whole `reductions` budget → [`slice_end`].
/// The SMP pool instead runs [`slice_begin`], *several* runner calls (each under its own lock
/// acquisition — see [`DEFAULT_LOCK_QUANTUM`]) and one [`slice_end`]; both fronts therefore share
/// the exact slice semantics.
///
/// # Parameters
///
/// * `pid` — the process to run; it must already have been dequeued.
/// * `table` — the process table (the PCB plus links/monitors/signals live here).
/// * `runner` — the execution-engine seam that runs the process for `reductions`.
/// * `reductions` — the per-slice reduction budget.
/// * `enqueue` — the requeue callback, invoked only on [`RunOutcome::Yield`] with the
///   process's priority **read after** the slice (so a `process_flag(priority, …)` the
///   runner just performed is honored, defaulting to normal when the process is gone).
///
/// # Errors
///
/// Propagates the signal-servicing, runner and termination errors.
pub(crate) fn run_one<R, F>(
    pid: ProcessId,
    table: &mut ProcessTable,
    runner: &mut R,
    reductions: u32,
    mut enqueue: F,
) -> Result<Option<RunOutcome>>
where
    R: ProcessRunner,
    F: FnMut(ProcessId, Priority),
{
    let mut ports: Option<&mut PortTable> = None;
    run_one_inner(pid, table, &mut ports, runner, reductions, &mut enqueue)
}

/// [`run_one`] with the [`PortTable`] in hand: the schedule-in step uses
/// [`slice_begin_with_ports`] and the schedule-out step [`slice_end_with_ports`], so a death
/// in either half cleans the dying process's incoming port monitors.
///
/// # Parameters
///
/// As [`run_one`], plus:
///
/// * `ports` — the port table, threaded into both slice halves.
///
/// # Errors
///
/// Propagates the signal-servicing, runner and termination errors.
pub(crate) fn run_one_with_ports<R, F>(
    pid: ProcessId,
    table: &mut ProcessTable,
    ports: &mut PortTable,
    runner: &mut R,
    reductions: u32,
    mut enqueue: F,
) -> Result<Option<RunOutcome>>
where
    R: ProcessRunner,
    F: FnMut(ProcessId, Priority),
{
    let mut ports: Option<&mut PortTable> = Some(ports);
    run_one_inner(pid, table, &mut ports, runner, reductions, &mut enqueue)
}

/// The shared body of [`run_one`] and [`run_one_with_ports`]: `ports` is `Some` only when the
/// caller holds a [`PortTable`]. Both slice halves are dispatched on it, so the slice logic
/// exists exactly once.
fn run_one_inner<R, F>(
    pid: ProcessId,
    table: &mut ProcessTable,
    ports: &mut Option<&mut PortTable>,
    runner: &mut R,
    reductions: u32,
    enqueue: &mut F,
) -> Result<Option<RunOutcome>>
where
    R: ProcessRunner,
    F: FnMut(ProcessId, Priority),
{
    let ready = match ports.as_deref_mut() {
        Some(ports) => slice_begin_with_ports(pid, table, ports)?,
        None => slice_begin(pid, table)?,
    };
    if !ready {
        return Ok(None);
    }
    let outcome = runner.run(pid, table, reductions)?;
    match ports.as_deref_mut() {
        Some(ports) => slice_end_with_ports(pid, table, ports, outcome, &mut *enqueue)?,
        None => slice_end(pid, table, outcome, &mut *enqueue)?,
    }
    Ok(Some(outcome))
}

/// The **schedule-in** step of one slice: take `pid` off the run queue (it is executing now) and
/// apply the signals queued for it (exit signals, monitor-DOWNs).
///
/// Returns `Ok(true)` when the process is ready to run and `Ok(false)` when there is nothing to
/// run: the pid was concurrently terminated between the dequeue and this lock, or servicing its
/// signals terminated it.
///
/// This is the process-table half of [`run_one`], factored out so the SMP pool can run a slice as
/// several **quanta** (one lock acquisition each) without duplicating the slice semantics. The
/// paired schedule-out half is [`slice_end`]. [`slice_begin_with_ports`] is the port-aware form.
///
/// # Errors
///
/// Propagates the signal-servicing errors.
pub(crate) fn slice_begin(pid: ProcessId, table: &mut ProcessTable) -> Result<bool> {
    let mut ports: Option<&mut PortTable> = None;
    slice_begin_inner(pid, table, &mut ports)
}

/// [`slice_begin`] with the [`PortTable`] in hand: the schedule-in signal servicing uses
/// [`ProcessTable::service_signals_with_ports`], so a queued signal that terminates the process
/// here also removes its incoming port monitors.
///
/// # Errors
///
/// Propagates the signal-servicing errors.
pub(crate) fn slice_begin_with_ports(
    pid: ProcessId,
    table: &mut ProcessTable,
    ports: &mut PortTable,
) -> Result<bool> {
    let mut ports: Option<&mut PortTable> = Some(ports);
    slice_begin_inner(pid, table, &mut ports)
}

/// The shared body of [`slice_begin`] and [`slice_begin_with_ports`]: `ports` is `Some` only
/// when the caller holds a [`PortTable`].
fn slice_begin_inner(
    pid: ProcessId,
    table: &mut ProcessTable,
    ports: &mut Option<&mut PortTable>,
) -> Result<bool> {
    // The process is off the queue and executing.
    match table.get_mut(pid) {
        Some(process) => {
            process.flags_mut().set_in_run_queue(false);
            process.set_state(ProcessState::Running);
        }
        // Terminated between the dequeue and this lock: nothing to run.
        None => return Ok(false),
    }

    // Schedule-in: apply any signals queued for the process (exit signals, monitor-DOWNs)
    // before it runs — this is where a signal that was deferred because the process was
    // executing (see the crate `CONTEXT.md`) finally lands. Servicing may terminate the
    // process, in which case there is nothing to run.
    match ports.as_deref_mut() {
        Some(ports) => table.service_signals_with_ports(ports, pid)?,
        None => table.service_signals(pid)?,
    }
    Ok(table.contains(pid))
}

/// The **schedule-out** step of one slice: apply `outcome`.
///
/// `enqueue(pid, priority)` is called only on [`RunOutcome::Yield`], with the process's priority
/// read **after** the slice (so a `process_flag(priority, …)` the runner just performed is
/// honored). A `Wait`/`Suspend` outcome leaves the queue (the runner owns the state transition)
/// and an `Exit` terminates the process, propagating to its links and monitors.
///
/// The scheduler does **not** count reductions here or anywhere: the runner owns the reduction
/// count and decides when to yield (see [`ProcessRunner`] — pre-emption is cooperative).
///
/// [`slice_end_with_ports`] is the port-aware form, whose `Exit` arm terminates through
/// `ProcessTable::terminate_with_ports`.
///
/// # Errors
///
/// Propagates the termination errors.
pub(crate) fn slice_end<F: FnMut(ProcessId, Priority)>(
    pid: ProcessId,
    table: &mut ProcessTable,
    outcome: RunOutcome,
    mut enqueue: F,
) -> Result<()> {
    let mut ports: Option<&mut PortTable> = None;
    slice_end_inner(pid, table, &mut ports, outcome, &mut enqueue)
}

/// [`slice_end`] with the [`PortTable`] in hand: a [`RunOutcome::Exit`] terminates through
/// [`ProcessTable::terminate_with_ports`], so the dying process's incoming port monitors are
/// removed from `ports` (the last step of `ProcessTable::terminate_into`). The `Yield` and
/// `Wait`/`Suspend` arms are unaffected.
///
/// # Errors
///
/// Propagates the termination errors.
pub(crate) fn slice_end_with_ports<F: FnMut(ProcessId, Priority)>(
    pid: ProcessId,
    table: &mut ProcessTable,
    ports: &mut PortTable,
    outcome: RunOutcome,
    mut enqueue: F,
) -> Result<()> {
    let mut ports: Option<&mut PortTable> = Some(ports);
    slice_end_inner(pid, table, &mut ports, outcome, &mut enqueue)
}

/// The shared body of [`slice_end`] and [`slice_end_with_ports`]: `ports` is `Some` only when
/// the caller holds a [`PortTable`].
fn slice_end_inner<F: FnMut(ProcessId, Priority)>(
    pid: ProcessId,
    table: &mut ProcessTable,
    ports: &mut Option<&mut PortTable>,
    outcome: RunOutcome,
    enqueue: &mut F,
) -> Result<()> {
    match outcome {
        RunOutcome::Yield => {
            // Still runnable: back to the tail of its own priority's queue. The priority is
            // read *after* the slice, honoring a `process_flag(priority, …)` the runner just
            // performed (defaulting to normal when the process is gone). The scheduler does not
            // touch the reduction counter — the runner owns it.
            let priority = table
                .get_mut(pid)
                .map(|process| {
                    process.flags_mut().set_in_run_queue(true);
                    process.set_state(ProcessState::Runnable);
                    process.priority()
                })
                .unwrap_or_default();
            enqueue(pid, priority);
        }
        // The runner owns the state transition for a waiting/suspended process; the
        // scheduler only takes it out of the ready queue.
        RunOutcome::Wait | RunOutcome::Suspend => {}
        RunOutcome::Exit(reason) => {
            // Termination propagates to the process's links and monitors — and, when the port
            // table is in hand, to its incoming port monitors.
            match ports.as_deref_mut() {
                Some(ports) => table.terminate_with_ports(ports, pid, reason)?,
                None => table.terminate(pid, reason)?,
            }
        }
    }
    Ok(())
}

/// Turns the table's pending wakes into run-queue entries via `enqueue`, returning how many
/// were enqueued. A woken pid is enqueued only when it is a live `Runnable` process not
/// already `in_run_queue` — a process can be woken and then terminated (or re-blocked)
/// within the same slice, and a `Yield` already requeued the process that just ran, so its
/// stale wake must not double-enqueue it. A woken process resumes at **its own** priority
/// (not normal).
///
/// # Parameters
///
/// * `table` — the process table whose wake list drives this sweep.
/// * `enqueue` — the run-queue insertion callback (`pid`, `priority`).
pub(crate) fn drain_woken_into<F: FnMut(ProcessId, Priority)>(
    table: &mut ProcessTable,
    mut enqueue: F,
) -> usize {
    let mut resumed = 0;
    for pid in table.drain_woken() {
        let skip = match table.get(pid) {
            Some(process) => {
                process.state() != ProcessState::Runnable || process.flags().in_run_queue()
            }
            None => true,
        };
        if skip {
            continue;
        }
        let priority = if let Some(process) = table.get_mut(pid) {
            process.flags_mut().set_in_run_queue(true);
            process.priority()
        } else {
            Priority::default()
        };
        enqueue(pid, priority);
        resumed += 1;
    }
    resumed
}

#[cfg(test)]
mod tests {
    //! Unit tests for the constructor defaults, the queue accessors, the
    //! `max_slices`/empty-queue edge cases of [`Scheduler::run_ready`], the priority-change
    //! path ([`Scheduler::set_priority`]) and spawning at a non-normal priority.

    use beam_gc::HeapPolicy;

    use super::*;
    use crate::process::Process;
    /// A runner that always yields, so a scheduler keeps cycling.
    struct YieldRunner {
        slices: u32,
    }

    impl ProcessRunner for YieldRunner {
        fn run(
            &mut self,
            _pid: ProcessId,
            _table: &mut ProcessTable,
            budget: u32,
        ) -> Result<RunOutcome> {
            self.slices += 1;
            assert_eq!(budget, DEFAULT_REDUCTIONS_PER_SLICE);
            Ok(RunOutcome::Yield)
        }
    }

    use crate::signal::SignalAtoms;
    use beam_term::prelude::AtomTable;

    fn table() -> ProcessTable {
        ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
    }

    #[test]
    fn constructors_and_accessors() {
        let scheduler = Scheduler::new();
        assert_eq!(scheduler.reductions_per_slice(), DEFAULT_REDUCTIONS_PER_SLICE);
        assert!(scheduler.run_queue().is_empty());
        assert_eq!(scheduler.progress().generation(), 0);

        let mut scheduler = Scheduler::with_slice(7);
        assert_eq!(scheduler.reductions_per_slice(), 7);
        scheduler.enqueue(ProcessId::new(1, 0));
        assert_eq!(scheduler.run_queue().len(), 1);
        assert!(scheduler.run_queue().contains(ProcessId::new(1, 0)));
        assert_eq!(scheduler.progress_mut().bump(), 1);
        assert_eq!(scheduler.progress().generation(), 1);
        scheduler.run_queue_mut().clear();
        assert!(scheduler.run_queue().is_empty());
    }

    #[test]
    fn run_slice_on_an_empty_queue_does_nothing() {
        let mut scheduler = Scheduler::new();
        let mut table = table();
        let mut runner = YieldRunner { slices: 0 };
        assert_eq!(
            scheduler
                .run_slice(&mut table, &mut runner)
                .expect("an empty queue cannot fail"),
            None
        );
        assert_eq!(runner.slices, 0);
    }

    #[test]
    fn run_ready_stops_at_max_slices() {
        let mut scheduler = Scheduler::new();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        scheduler.enqueue(pid);

        let mut runner = YieldRunner { slices: 0 };
        assert_eq!(
            scheduler
                .run_ready(&mut table, &mut runner, Some(2))
                .expect("the runner never fails"),
            2
        );
        assert_eq!(runner.slices, 2, "the runner ran exactly two slices");

        // `Some(0)` runs nothing at all.
        assert_eq!(
            scheduler
                .run_ready(&mut table, &mut runner, Some(0))
                .expect("no slices cannot fail"),
            0
        );

        // Without a bound the queue is drained (it never empties here, so bound it).
        assert_eq!(
            scheduler
                .run_ready(&mut table, &mut runner, Some(3))
                .expect("the runner never fails"),
            3
        );
    }

    #[test]
    fn set_priority_moves_a_queued_process_to_the_new_level() {
        let mut scheduler = Scheduler::new();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");

        scheduler.enqueue(pid);
        assert_eq!(
            scheduler.run_queue().priority_of(pid),
            Some(Priority::Normal)
        );
        let len_before = scheduler.run_queue().len();

        // A promotion moves the pid to the HIGH level without duplicating it.
        scheduler
            .set_priority(&mut table, pid, Priority::High)
            .expect("a live pid is reprioritized");
        assert_eq!(
            table.get(pid).expect("the process is live").priority(),
            Priority::High
        );
        assert_eq!(scheduler.run_queue().priority_of(pid), Some(Priority::High));
        assert_eq!(
            scheduler.run_queue().len(),
            len_before,
            "the pid moved, it was not duplicated"
        );

        // Setting the same priority again is a no-op (still exactly one entry).
        scheduler
            .set_priority(&mut table, pid, Priority::High)
            .expect("a live pid is reprioritized");
        assert_eq!(scheduler.run_queue().priority_of(pid), Some(Priority::High));
        assert_eq!(scheduler.run_queue().len(), len_before);

        // A demotion moves as well.
        scheduler
            .set_priority(&mut table, pid, Priority::Low)
            .expect("a live pid is reprioritized");
        assert_eq!(scheduler.run_queue().priority_of(pid), Some(Priority::Low));
        assert_eq!(scheduler.run_queue().len(), 1);
    }

    #[test]
    fn set_priority_on_an_unqueued_pid_only_updates_the_pcb() {
        let mut scheduler = Scheduler::new();
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        // Not enqueued (it is running, or has not been made runnable yet).
        assert_eq!(scheduler.run_queue().priority_of(pid), None);

        scheduler
            .set_priority(&mut table, pid, Priority::Max)
            .expect("a live pid is reprioritized");
        assert_eq!(
            table.get(pid).expect("the process is live").priority(),
            Priority::Max
        );
        assert_eq!(
            scheduler.run_queue().priority_of(pid),
            None,
            "a non-queued pid is never inserted"
        );
        assert_eq!(scheduler.run_queue().len(), 0);
    }

    #[test]
    fn set_priority_on_an_unknown_pid_errors_without_enqueuing() {
        let mut scheduler = Scheduler::new();
        let mut table = table();
        let unknown = ProcessId::new(4242, 0);

        let outcome = scheduler.set_priority(&mut table, unknown, Priority::High);
        assert!(
            matches!(outcome, Err(beam_common::BeamError::TermNotFound)),
            "an unknown pid must be TermNotFound"
        );
        assert_eq!(scheduler.run_queue().len(), 0, "nothing was enqueued");
        assert_eq!(scheduler.run_queue().priority_of(unknown), None);
    }

    #[test]
    fn a_process_born_at_high_priority_is_served_before_a_normal_one() {
        let mut scheduler = Scheduler::new();
        let mut table = table();

        // A process born at the default (NORMAL) priority, enqueued first.
        let normal = table.spawn_default().expect("a fresh process");
        scheduler.enqueue(normal);

        // A process born at HIGH via `with_policy_and_priority`, inserted and enqueued after.
        let high = ProcessId::new(1, 0);
        let process =
            Process::with_policy_and_priority(high, HeapPolicy::default(), Priority::High)
                .expect("the default heap allocates");
        table.insert(process).expect("the pid is in range");
        scheduler.enqueue_with_priority(high, Priority::High);

        // The HIGH process is served first even though it was enqueued second.
        assert_eq!(scheduler.run_queue_mut().dequeue(), Some(high));
        assert_eq!(scheduler.run_queue_mut().dequeue(), Some(normal));
    }
}
