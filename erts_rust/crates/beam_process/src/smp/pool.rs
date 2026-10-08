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

//! [`SchedulerPool`] — the multi-scheduler (SMP) pool's public API and lifecycle.
//!
//! A pool owns `N` scheduler threads sharing one [`SharedProcessTable`], one queue array, one
//! thread-progress barrier, one [`TimerService`](crate::timer::TimerService) and one
//! [`DirtyPool`](crate::dirty::DirtyPool). It is the SMP counterpart of
//! [`crate::scheduler::Scheduler`]: the slice logic is identical (it is the shared
//! `crate::scheduler::{run_one, drain_woken_into}` helpers), only the concurrency around it is new.
//!
//! See the [module-level locking discipline](crate::smp) before changing any of this.

use std::marker::PhantomData;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use beam_common::{BeamError, Result};
use beam_gc::HeapPolicy;
use beam_term::prelude::Eterm;
use parking_lot::Mutex;

use crate::dirty::{DirtyCompletionSink, DirtyJob, DirtyKind, DirtyOutcome, DirtyPool};
use crate::pid::ProcessId;
use crate::process::{DeliverOutcome, ProcessState};
use crate::progress::ThreadProgress;
use crate::ptab::ProcessTable;
use crate::run_queue::Priority;
use crate::scheduler::{drain_woken_into, ProcessRunner};
use crate::smp::queues::Queues;
use crate::smp::stats::{SchedulerStat, SchedulerStats, WorkerCounters};
use crate::smp::worker::{record_error, PoolSignals, Worker};
use crate::smp::{SchedulerConfig, SharedProcessTable};
use crate::timer::{TimerRef, TimerService, TimerSink};

/// The multi-scheduler pool: `N` scheduler threads over one shared process table.
///
/// Start it with [`SchedulerPool::start`], feed work with [`SchedulerPool::spawn`] +
/// [`SchedulerPool::enqueue`] (or [`SchedulerPool::deliver_simple`] / timers / dirty jobs), observe
/// it with [`SchedulerPool::stats`] / [`SchedulerPool::wait_idle`], and stop it with
/// [`SchedulerPool::shutdown`] / [`SchedulerPool::join`].
///
/// The `R` type parameter is the execution-engine seam ([`ProcessRunner`]) each scheduler thread
/// owns; it is only used to name the type here (the runners live on the scheduler threads), hence
/// the [`PhantomData`].
pub struct SchedulerPool<R: ProcessRunner + Send + 'static> {
    /// The shared process table.
    table: Arc<SharedProcessTable>,
    /// The shared queue array (injector + one local queue per scheduler).
    queues: Arc<Queues>,
    /// The shared park/notify state.
    signals: Arc<PoolSignals>,
    /// The shared thread-progress barrier.
    progress: ThreadProgress,
    /// The first worker/table error seen by any worker (see [`SchedulerPool::take_error`]).
    errors: Arc<Mutex<Option<BeamError>>>,
    /// The per-scheduler counters.
    counters: Arc<Vec<WorkerCounters>>,
    /// The maximum number of reductions run per process-table lock acquisition (see
    /// [`SchedulerPool::lock_quantum`]).
    lock_quantum: u32,
    /// The scheduler thread handles, taken by [`SchedulerPool::join`].
    threads: Mutex<Vec<JoinHandle<()>>>,
    /// The pool's timer service (started only when the config enables timers).
    timer_service: Arc<TimerService>,
    /// The pool's dirty scheduler pool.
    dirty_pool: Arc<DirtyPool>,
    /// Names the runner type (each thread owns its own `R`).
    _runner: PhantomData<R>,
}

impl<R: ProcessRunner + Send + 'static> SchedulerPool<R> {
    /// Starts the pool: spawns `config.schedulers()` scheduler threads (clamped to at least **1**),
    /// starts the timer service when `config.timers()` is set, and starts the dirty pools.
    ///
    /// `make_runner` is called **once per scheduler index** (on the caller's thread) to build that
    /// scheduler's runner; it is shared across the threads behind an `Arc`.
    ///
    /// # Errors
    ///
    /// Returns [`BeamError::Io`] when a scheduler thread cannot be spawned; any already-spawned
    /// threads are stopped and joined first, so a failed `start` leaks nothing.
    pub fn start(
        config: SchedulerConfig,
        table: ProcessTable,
        make_runner: impl Fn(usize) -> R + Send + Sync + 'static,
    ) -> Result<SchedulerPool<R>> {
        // A runtime asking for one scheduler MUST work: clamp 0 up to 1.
        let schedulers = config.schedulers().max(1);
        let reductions = config.reductions_per_slice();
        let lock_quantum = config.lock_quantum().max(1);

        let table = SharedProcessTable::new(table);
        let queues = Arc::new(Queues::new(schedulers));
        let signals = Arc::new(PoolSignals::new());
        let progress = ThreadProgress::new();
        let errors: Arc<Mutex<Option<BeamError>>> = Arc::new(Mutex::new(None));
        let counters = Arc::new(
            (0..schedulers)
                .map(|_| WorkerCounters::new())
                .collect::<Vec<_>>(),
        );
        let timer_service = Arc::new(TimerService::new());
        let dirty_pool = Arc::new(DirtyPool::start(config.dirty_cpu(), config.dirty_io()));

        // The timer sink delivers into the shared table; `timer_service()` stays total even when
        // timers are disabled — it is then simply never started (see the accessor).
        if config.timers() {
            timer_service.start(Arc::new(PoolTimerSink {
                table: Arc::clone(&table),
                queues: Arc::clone(&queues),
                signals: Arc::clone(&signals),
                errors: Arc::clone(&errors),
            }));
        }

        // The dirty sink is always installed and always drains job completions back onto a normal
        // scheduler (`DirtyPool::start(0, 0)` is supported — jobs then buffer until shutdown).
        dirty_pool.set_sink(Arc::new(PoolDirtySink {
            table: Arc::clone(&table),
            queues: Arc::clone(&queues),
            signals: Arc::clone(&signals),
            errors: Arc::clone(&errors),
        }));

        // Let a safepoint nudge a fully-parked pool.
        {
            let signals = Arc::clone(&signals);
            progress.set_waker(Arc::new(move || signals.notify_all()));
        }

        let make_runner: Arc<dyn Fn(usize) -> R + Send + Sync> = Arc::new(make_runner);
        let mut threads = Vec::with_capacity(schedulers);
        for index in 0..schedulers {
            let worker = Worker::new(
                index,
                reductions,
                lock_quantum,
                Arc::clone(&table),
                Arc::clone(&queues),
                Arc::clone(&signals),
                Arc::clone(&errors),
                Arc::clone(&counters),
                progress.clone(),
                &*make_runner,
            );
            match thread::Builder::new()
                .name(format!("beam-sched-{index}"))
                .spawn(move || worker.run())
            {
                Ok(handle) => threads.push(handle),
                Err(error) => {
                    // Wind down the schedulers already started, then report the failure.
                    signals.request_stop();
                    for handle in threads {
                        let _ = handle.join();
                    }
                    return Err(BeamError::Io(error));
                }
            }
        }

        Ok(SchedulerPool {
            table,
            queues,
            signals,
            progress,
            errors,
            counters,
            lock_quantum,
            threads: Mutex::new(threads),
            timer_service,
            dirty_pool,
            _runner: PhantomData,
        })
    }

    /// The number of scheduler threads in the pool (always at least 1).
    pub fn schedulers(&self) -> usize {
        self.counters.len()
    }

    /// The pool's per-lock-acquisition reduction quantum (clamped to at least 1).
    ///
    /// A scheduler holds the process-table lock for at most this many reductions of a slice, so it
    /// also bounds how long an independent process or a cross-thread delivery waits behind a slice
    /// in progress. Configure it with [`SchedulerConfig::set_lock_quantum`].
    pub fn lock_quantum(&self) -> u32 {
        self.lock_quantum
    }

    /// A clone of the shared thread-progress barrier.
    ///
    /// The clone shares state with the one the schedulers registered their managed threads on, so
    /// [`ThreadProgress::safepoint`] on the clone waits for them.
    pub fn progress(&self) -> ThreadProgress {
        self.progress.clone()
    }

    /// The shared process table.
    ///
    /// Lock it with [`SharedProcessTable::lock`]; hold the guard only for one operation and never
    /// while acquiring a queue lock (see the module-level locking discipline).
    pub fn table(&self) -> Arc<SharedProcessTable> {
        Arc::clone(&self.table)
    }

    /// The timer service.
    ///
    /// Total even when timers are disabled: with [`SchedulerConfig::set_timers`]`(false)` it is
    /// returned **unstarted**, so scheduling a timer on it succeeds but never fires. That keeps the
    /// accessor infallible for callers that do not care whether the pool drives timers.
    pub fn timer_service(&self) -> Arc<TimerService> {
        Arc::clone(&self.timer_service)
    }

    /// The dirty scheduler pool.
    pub fn dirty_pool(&self) -> Arc<DirtyPool> {
        Arc::clone(&self.dirty_pool)
    }

    /// Spawns a process with the default heap policy. It is **not** enqueued; call
    /// [`SchedulerPool::enqueue`] (or a send) to make it runnable.
    ///
    /// # Errors
    ///
    /// Propagates [`ProcessTable::spawn_default`].
    pub fn spawn_default(&self) -> Result<ProcessId> {
        self.table.lock().spawn_default()
    }

    /// Spawns a process with a specific heap policy (not enqueued — see
    /// [`SchedulerPool::spawn_default`]).
    ///
    /// # Errors
    ///
    /// Propagates [`ProcessTable::spawn`].
    pub fn spawn(&self, policy: HeapPolicy) -> Result<ProcessId> {
        self.table.lock().spawn(policy)
    }

    /// Enqueues `pid` on the shared injector at **normal** priority and wakes a scheduler.
    pub fn enqueue(&self, pid: ProcessId) {
        self.enqueue_with_priority(pid, Priority::Normal);
    }

    /// Enqueues `pid` on the shared injector at `priority` and wakes a scheduler.
    pub fn enqueue_with_priority(&self, pid: ProcessId, priority: Priority) {
        self.queues.enqueue_injector(pid, priority);
        self.signals.notify_work();
    }

    /// Changes `pid`'s scheduling priority and, if it is sitting in a run queue, moves it to the
    /// matching level.
    ///
    /// The PCB is updated under the table lock, which is **released before** the queue search:
    /// the locking discipline forbids holding the table lock while taking a queue lock (see the
    /// module-level locking discipline). The search then costs one queue-lock acquisition per
    /// queue — the shared injector plus every local queue — so it is `O(schedulers)`.
    ///
    /// A pid that is currently **in flight** (being executed by a scheduler) is in no queue, so
    /// only the PCB update applies and its next requeue reads the new priority. A pid that is
    /// waiting/suspended/terminated is likewise not in a queue, and an **unknown** pid is
    /// reported as an error with every queue left untouched.
    ///
    /// # Errors
    ///
    /// Propagates [`ProcessTable::set_priority`]'s `TermNotFound` for an unknown pid; the queues
    /// are left untouched in that case.
    pub fn set_priority(&self, pid: ProcessId, priority: Priority) -> Result<()> {
        {
            let mut table = self.table.lock();
            table.set_priority(pid, priority)?;
        }
        self.queues.reprioritize(pid, priority);
        Ok(())
    }
    /// Delivers `term` to `to` from `from`, waking a blocked receiver through the injector.
    ///
    /// A send from outside a scheduler slice has no slice end to absorb the wake, so the pool
    /// drains the table's wake list itself and enqueues the woken pids (this is the thread-safe
    /// counterpart of `beam_rt`'s single-threaded `Scheduler::resume_woken` call).
    ///
    /// # Errors
    ///
    /// Propagates [`ProcessTable::deliver_simple`] (`TermNotFound` for an unknown target, an
    /// allocation error when the message does not fit).
    pub fn deliver_simple(&self, to: ProcessId, from: Eterm, term: Eterm) -> Result<DeliverOutcome> {
        let mut woken = Vec::new();
        let outcome = {
            let mut table = self.table.lock();
            let outcome = table.deliver_simple(to, from, term)?;
            drain_woken_into(&mut table, |pid, priority| woken.push((pid, priority)));
            outcome
        };
        requeue_woken(&self.queues, &self.signals, &woken);
        Ok(outcome)
    }

    /// Schedules `message` for delivery to `target` after `delay`.
    ///
    /// See [`TimerService::send_after`] for the message-validity contract.
    pub fn send_after(&self, delay: Duration, target: ProcessId, message: Eterm) -> TimerRef {
        self.timer_service.send_after(delay, target, message)
    }

    /// Submits a unit of dirty work to the `kind` pool.
    ///
    /// The job's [`DirtyOutcome`] is routed back onto a normal scheduler by the pool
    /// ([`DirtyOutcome::Reschedule`] re-enqueues the pid; [`DirtyOutcome::Deliver`] delivers and
    /// wakes).
    pub fn submit_dirty(&self, kind: DirtyKind, job: DirtyJob) {
        self.dirty_pool.submit(kind, job);
    }

    /// A pool-wide counter snapshot (the `statistics(...)` observable set).
    pub fn stats(&self) -> SchedulerStats {
        let per_scheduler = self.scheduler_stats();
        SchedulerStats {
            schedulers: per_scheduler.len(),
            run_queue_len: self.queues.total_len(),
            slices: per_scheduler.iter().map(|stat| stat.slices).sum(),
            steals: per_scheduler.iter().map(|stat| stat.steals).sum(),
            active: per_scheduler.iter().filter(|stat| stat.active).count(),
            dirty_cpu_pending: self.dirty_pool.pending(DirtyKind::Cpu),
            dirty_io_pending: self.dirty_pool.pending(DirtyKind::Io),
            timers_pending: self.timer_service.pending(),
        }
    }

    /// A per-scheduler counter snapshot (one entry per scheduler, indexed `0..schedulers`).
    pub fn scheduler_stats(&self) -> Vec<SchedulerStat> {
        self.counters
            .iter()
            .enumerate()
            .map(|(index, counters)| counters.snapshot(index, self.queues.local_len(index)))
            .collect()
    }

    /// The total queued work (the injector plus every local queue) — the
    /// `statistics(run_queue)` observable.
    pub fn run_queue_len(&self) -> usize {
        self.queues.total_len()
    }

    /// Every scheduler's **local** run-queue length (the injector is excluded).
    pub fn run_queue_lengths(&self) -> Vec<usize> {
        self.queues.local_lengths()
    }

    /// Blocks until every queue is empty **and** no pid is in flight, or `timeout` elapses;
    /// returns whether the pool is idle. `None` waits indefinitely.
    ///
    /// This is the deterministic idle signal to use in tests instead of sleeping: it is only
    /// reported once every spawned slice has finished, so counters read afterwards are exact.
    pub fn wait_idle(&self, timeout: Option<Duration>) -> bool {
        self.signals.wait_for_idle(&self.queues, timeout)
    }

    /// Reaches a thread-progress safepoint on the shared barrier and returns its generation.
    pub fn safepoint(&self) -> u64 {
        self.progress.safepoint()
    }

    /// Takes (and clears) the first worker/table error the pool observed, if any.
    ///
    /// A scheduler thread cannot return an error to a caller, so the **first** failure is retained
    /// here (`None` means every slice ran clean); later errors are dropped so this is the
    /// originating failure.
    pub fn take_error(&self) -> Option<BeamError> {
        self.errors.lock().take()
    }

    /// Requests shutdown: sets the stop flag and wakes every scheduler. The workers finish the
    /// slice in progress and exit.
    ///
    /// Any pids still queued are **dropped** — the queues are simply abandoned and the processes
    /// stay in the table (the runtime owns their fate). Call [`SchedulerPool::join`] to wait for
    /// the threads and stop the timer/dirty subsystems.
    pub fn shutdown(&self) {
        self.signals.request_stop();
    }

    /// Shuts the pool down and joins everything: the scheduler threads, the timer service and the
    /// dirty pool.
    ///
    /// The dirty pool is joined only when this is the last handle (an `Arc::try_unwrap`); if the
    /// caller still holds a clone from [`SchedulerPool::dirty_pool`], the pool is merely shut down
    /// (its workers exit, but the clone keeps it alive) and the caller must drop/join it.
    pub fn join(self) {
        self.signals.request_stop();

        let handles = std::mem::take(&mut *self.threads.lock());
        for handle in handles {
            let _ = handle.join();
        }

        self.timer_service.shutdown();
        self.timer_service.join();

        match Arc::try_unwrap(self.dirty_pool) {
            Ok(pool) => pool.join(),
            // Another handle is alive (see the doc comment): shut it down, do not force-join.
            Err(shared) => shared.shutdown(),
        }
    }
}

/// Enqueues woken pids onto the shared injector and wakes a scheduler.
///
/// Called only **after** the table guard has been dropped (see the module-level locking
/// discipline).
fn requeue_woken(queues: &Queues, signals: &PoolSignals, woken: &[(ProcessId, Priority)]) {
    if woken.is_empty() {
        return;
    }
    queues.enqueue_injector_batch(woken);
    signals.notify_work();
}

/// Delivers `message` to `target` and collects any woken receiver into `woken`.
///
/// The `from` word is `target.to_eterm()` (a self-send): the frozen mailbox envelope needs a
/// sender term and a timer/dirty completion has no meaningful sender pid, so the message appears
/// self-sent (a documented choice; the C uses the sender-less `erts_send_message` path).
///
/// A [`BeamError::TermNotFound`] is a **benign** no-op — the target died after the timer/job was
/// scheduled — while any other error is recorded as the pool's first error (never swallowed).
fn deliver_message(
    table: &SharedProcessTable,
    errors: &Mutex<Option<BeamError>>,
    target: ProcessId,
    message: Eterm,
    woken: &mut Vec<(ProcessId, Priority)>,
) {
    let mut guard = table.lock();
    match guard.deliver_simple(target, target.to_eterm(), message) {
        Ok(_) => {
            drain_woken_into(&mut guard, |pid, priority| woken.push((pid, priority)));
        }
        Err(BeamError::TermNotFound) => {}
        Err(error) => record_error(errors, error),
    }
}

/// The [`TimerSink`] the pool installs: delivers a fired timer's message and wakes the target.
struct PoolTimerSink {
    /// The shared process table.
    table: Arc<SharedProcessTable>,
    /// The shared queue array.
    queues: Arc<Queues>,
    /// The shared park/notify state.
    signals: Arc<PoolSignals>,
    /// The pool-visible first-error slot.
    errors: Arc<Mutex<Option<BeamError>>>,
}

impl TimerSink for PoolTimerSink {
    fn timer_fired(&self, target: ProcessId, message: Eterm) {
        let mut woken = Vec::new();
        deliver_message(&self.table, &self.errors, target, message, &mut woken);
        requeue_woken(&self.queues, &self.signals, &woken);
    }
}

/// The [`DirtyCompletionSink`] the pool installs: routes a finished dirty job's outcome back onto
/// a normal scheduler.
struct PoolDirtySink {
    /// The shared process table.
    table: Arc<SharedProcessTable>,
    /// The shared queue array.
    queues: Arc<Queues>,
    /// The shared park/notify state.
    signals: Arc<PoolSignals>,
    /// The pool-visible first-error slot.
    errors: Arc<Mutex<Option<BeamError>>>,
}

impl DirtyCompletionSink for PoolDirtySink {
    fn dirty_completed(&self, outcome: DirtyOutcome) {
        match outcome {
            DirtyOutcome::Done => {}
            DirtyOutcome::Reschedule(pid) => {
                // Make the pid runnable again and read its priority under one brief table lock.
                let priority = {
                    let mut table = self.table.lock();
                    match table.get_mut(pid) {
                        Some(process) => {
                            process.set_state(ProcessState::Runnable);
                            process.flags_mut().set_in_run_queue(true);
                            process.priority()
                        }
                        // The process died while the job ran: nothing to reschedule.
                        None => return,
                    }
                };
                self.queues.enqueue_injector(pid, priority);
                self.signals.notify_work();
            }
            DirtyOutcome::Deliver { target, message } => {
                let mut woken = Vec::new();
                deliver_message(&self.table, &self.errors, target, message, &mut woken);
                requeue_woken(&self.queues, &self.signals, &woken);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Small smoke tests for the pool's start/run/drain/lifecycle. The exhaustive SMP suite
    //! (stealing, priorities, timers, dirty routing, safepoints) is a separate follow-up task.

    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    use crate::scheduler::RunOutcome;
    use crate::signal::SignalAtoms;
    use beam_term::prelude::AtomTable;

    /// A runner that records that it ran and then blocks its process, so the pool drains.
    struct WaitRunner {
        /// How many times the runner has been entered (shared across schedulers).
        counter: StdArc<AtomicUsize>,
    }

    impl ProcessRunner for WaitRunner {
        fn run(
            &mut self,
            pid: ProcessId,
            table: &mut ProcessTable,
            _budget: u32,
        ) -> Result<RunOutcome> {
            // A blocked process leaves the run queue, so the pool empties once every pid has run.
            if let Some(process) = table.get_mut(pid) {
                process.set_state(ProcessState::Waiting);
            }
            self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(RunOutcome::Wait)
        }
    }

    fn table() -> ProcessTable {
        ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
    }

    fn wait_runner(counter: &StdArc<AtomicUsize>) -> impl Fn(usize) -> WaitRunner + Send + Sync {
        let counter = StdArc::clone(counter);
        move |_index: usize| WaitRunner {
            counter: StdArc::clone(&counter),
        }
    }

    #[test]
    fn pool_runs_and_drains() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let pool = SchedulerPool::start(
            SchedulerConfig::with_schedulers(2),
            table(),
            wait_runner(&counter),
        )
        .expect("a fresh pool starts");

        assert_eq!(pool.schedulers(), 2);
        assert_eq!(pool.scheduler_stats().len(), 2);
        assert_eq!(pool.run_queue_lengths().len(), 2);

        let count = 64;
        for _ in 0..count {
            let pid = pool.spawn_default().expect("a fresh process");
            pool.enqueue(pid);
        }

        assert!(
            pool.wait_idle(Some(Duration::from_secs(5))),
            "the pool drains to idle"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            count,
            "every pid ran exactly once"
        );
        assert_eq!(pool.run_queue_len(), 0, "no work is queued when idle");

        let stats = pool.stats();
        assert_eq!(stats.schedulers, 2);
        assert_eq!(stats.slices, count as u64);
        assert!(pool.take_error().is_none(), "no worker error");

        pool.shutdown();
        pool.join();
    }

    #[test]
    fn zero_schedulers_is_clamped_to_one() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let pool = SchedulerPool::start(
            SchedulerConfig::with_schedulers(0),
            table(),
            wait_runner(&counter),
        )
        .expect("a one-scheduler pool starts");

        assert_eq!(pool.schedulers(), 1, "the pool clamps 0 up to 1");
        assert_eq!(pool.scheduler_stats().len(), 1);

        let pid = pool.spawn_default().expect("a fresh process");
        pool.enqueue(pid);
        assert!(pool.wait_idle(Some(Duration::from_secs(5))));
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        pool.shutdown();
        pool.join();
    }

    #[test]
    fn safepoint_advances_with_a_parked_pool() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let pool = SchedulerPool::start(
            SchedulerConfig::with_schedulers(2),
            table(),
            wait_runner(&counter),
        )
        .expect("a fresh pool starts");

        // Every scheduler is parked; the registered waker must still let the safepoint complete.
        let generation = pool.safepoint();
        assert!(generation >= 1, "the safepoint advanced the generation");

        pool.shutdown();
        pool.join();
    }

    #[test]
    fn set_priority_updates_the_pcb_and_never_queues_a_non_queued_pid() {
        let counter = StdArc::new(AtomicUsize::new(0));
        let pool = SchedulerPool::start(
            SchedulerConfig::with_schedulers(1),
            table(),
            wait_runner(&counter),
        )
        .expect("a fresh pool starts");

        // A spawned pid is inserted but not enqueued, so it is in no queue.
        let pid = pool.spawn_default().expect("a fresh process");
        assert_eq!(pool.run_queue_len(), 0);

        pool.set_priority(pid, Priority::High)
            .expect("a live pid is reprioritized");
        assert_eq!(
            pool.table()
                .lock()
                .get(pid)
                .expect("the process is live")
                .priority(),
            Priority::High
        );
        assert_eq!(
            pool.run_queue_len(),
            0,
            "a non-queued pid is not inserted into any queue"
        );

        // An unknown pid is TermNotFound and changes nothing.
        let unknown = ProcessId::new(9999, 0);
        assert!(matches!(
            pool.set_priority(unknown, Priority::Max),
            Err(BeamError::TermNotFound)
        ));
        assert_eq!(pool.run_queue_len(), 0);

        pool.shutdown();
        pool.join();
    }
}
