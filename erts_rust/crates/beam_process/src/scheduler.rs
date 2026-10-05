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
use crate::process::ProcessState;
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

    /// Makes `pid` runnable by appending it to the run queue.
    pub fn enqueue(&mut self, pid: ProcessId) {
        self.run_queue.enqueue(pid);
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
    /// # Errors
    ///
    /// Propagates the runner's and the table's errors.
    pub fn run_slice<R: ProcessRunner>(
        &mut self,
        table: &mut ProcessTable,
        runner: &mut R,
    ) -> Result<Option<ProcessId>> {
        let Some(pid) = self.run_queue.dequeue() else {
            return Ok(None);
        };

        // The process is off the queue and executing.
        if let Some(process) = table.get_mut(pid) {
            process.flags_mut().set_in_run_queue(false);
            process.set_state(ProcessState::Running);
        }

        let outcome = runner.run(pid, table, self.reductions_per_slice)?;
        self.slices = self.slices.wrapping_add(1);

        match outcome {
            RunOutcome::Yield => {
                // Still runnable: back to the tail of the queue (round-robin fairness).
                if let Some(process) = table.get_mut(pid) {
                    process.flags_mut().set_in_run_queue(true);
                    process.set_state(ProcessState::Runnable);
                }
                self.run_queue.enqueue(pid);
            }
            // The runner owns the state transition for a waiting/suspended process; the
            // scheduler only takes it out of the ready queue.
            RunOutcome::Wait | RunOutcome::Suspend => {}
            RunOutcome::Exit(reason) => {
                // Termination propagates to the process's links and monitors.
                table.terminate(pid, reason)?;
            }
        }

        Ok(Some(pid))
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
        let mut executed: u64 = 0;
        while !self.run_queue.is_empty() {
            if let Some(max) = max_slices {
                if executed >= max {
                    break;
                }
            }
            match self.run_slice(table, runner)? {
                Some(_) => executed = executed.wrapping_add(1),
                None => break,
            }
        }
        Ok(executed)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the constructor defaults, the queue accessors and the
    //! `max_slices`/empty-queue edge cases of [`Scheduler::run_ready`].

    use super::*;

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
}
