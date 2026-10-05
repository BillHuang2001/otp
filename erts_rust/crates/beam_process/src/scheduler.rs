//! The scheduler loop and the execution-engine seam: [`Scheduler`], [`RunOutcome`] and
//! [`ProcessRunner`].
//!
//! Milestone-1 runs **one** scheduler on **one** OS thread: it pops a pid from the run
//! queue, runs it for one reduction slice and interprets the outcome (requeue on yield,
//! leave the queue on wait/suspend, terminate on exit).
//!
//! # The engine seam
//!
//! The scheduler must not know how a process executes, so the execution engine is reached
//! through the [`ProcessRunner`] trait rather than a dependency: `beam_engine` is **not** a
//! dependency of this crate (it depends on `beam_loader` only), and `beam_rt` supplies a
//! runner implemented over the engine's `ExecutionContext`. That keeps the Stage-1/Stage-5
//! DAG acyclic (see the crate `CONTEXT.md`, Design Decisions).
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_process.c`
//! (`scheduler_wait`/`schedule`), `erl_vm.h` (`CONTEXT_REDS`).

use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::pid::ProcessId;
use crate::progress::ThreadProgress;
use crate::ptab::ProcessTable;
use crate::run_queue::RunQueue;

/// The number of reductions a process runs before being pre-empted (the C
/// `CONTEXT_REDS`).
pub const DEFAULT_REDUCTIONS_PER_SLICE: u32 = 4000;

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
//
// TODO(impl): implement `Scheduler` (all bodies are stubs).
pub struct Scheduler {
    run_queue: RunQueue,
    reductions_per_slice: u32,
    progress: ThreadProgress,
    slices: u64,
}

impl Scheduler {
    /// A scheduler with [`DEFAULT_REDUCTIONS_PER_SLICE`] reductions per slice.
    pub fn new() -> Scheduler {
        unimplemented!("stub — to be implemented")
    }

    /// A scheduler with an explicit per-slice reduction budget.
    pub fn with_slice(reductions_per_slice: u32) -> Scheduler {
        unimplemented!("stub — to be implemented")
    }

    /// The per-slice reduction budget.
    pub fn reductions_per_slice(&self) -> u32 {
        unimplemented!("stub — to be implemented")
    }

    /// The run queue.
    pub fn run_queue(&self) -> &RunQueue {
        unimplemented!("stub — to be implemented")
    }

    /// The run queue, mutably.
    pub fn run_queue_mut(&mut self) -> &mut RunQueue {
        unimplemented!("stub — to be implemented")
    }

    /// Makes `pid` runnable by appending it to the run queue.
    pub fn enqueue(&mut self, pid: ProcessId) {
        unimplemented!("stub — to be implemented")
    }

    /// The thread-progress tracker.
    pub fn progress(&self) -> &ThreadProgress {
        unimplemented!("stub — to be implemented")
    }

    /// The thread-progress tracker, mutably.
    pub fn progress_mut(&mut self) -> &mut ThreadProgress {
        unimplemented!("stub — to be implemented")
    }

    /// Runs at most one slice: pop one pid, run it, then requeue (`Yield`), drop
    /// (`Wait`/`Suspend`) or terminate (`Exit`) it. Returns the pid that ran, or `None`
    /// when the run queue was empty.
    ///
    /// # Errors
    ///
    /// Propagates the runner's and the table's errors.
    pub fn run_slice<R: ProcessRunner>(
        &mut self,
        table: &mut ProcessTable,
        runner: &mut R,
    ) -> Result<Option<ProcessId>> {
        unimplemented!("stub — to be implemented")
    }

    /// Runs slices until the run queue empties (or `max_slices` have run), returning the
    /// number of slices executed.
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
        unimplemented!("stub — to be implemented")
    }
}
