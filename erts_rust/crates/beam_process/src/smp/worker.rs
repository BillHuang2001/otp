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

//! The scheduler-thread loop and the pool's park/notify state ([`PoolSignals`]).
//!
//! One [`Worker`] runs on one OS thread. It owns the runner for its index, a
//! [`ManagedThread`] progress slot and a local buffer of requeues, and it executes the
//! *frozen* single-process slice logic by calling the extracted
//! [`crate::scheduler::run_one`] / [`crate::scheduler::drain_woken_into`] helpers — the SMP
//! change adds **concurrency**, it does not re-implement the slice.
//!
//! # Locking discipline
//!
//! For each iteration the worker (1) dequeues a pid from a queue, (2) takes the process-table lock,
//! runs a **quantum** of the slice and lets the two helpers *buffer* every requeue into `buf` (they
//! run while the table lock is held, so their `enqueue` closure must not touch a queue lock), (3)
//! drops the table lock, and only then (4) flushes `buf` into the local queue. The two lock
//! families are therefore never held simultaneously.
//!
//! The table lock is **not** held for a whole slice: a slice of `reductions_per_slice` reductions is
//! run as `ceil(reductions_per_slice / lock_quantum)` quanta, each under its own short lock
//! acquisition ([`run_locked_quanta`]). That is what lets an independent process (or a cross-thread
//! send / timer / dirty completion) make progress *inside* another process's slice instead of
//! waiting for the whole slice; the slice semantics (schedule-in, own reduction budget, one requeue
//! at the process's own priority) are unchanged, because the schedule-in/schedule-out halves are
//! the frozen [`slice_begin`]/[`slice_end`] pair the single-threaded scheduler uses.
//!
//! The one deliberate exception is the park predicate: [`PoolSignals::park`] holds the signals
//! mutex and re-checks the queue array (a queue lock) so an enqueue cannot be lost between the
//! "nothing to do" decision and the sleep. The order is always *signals mutex → queue lock*;
//! the reverse never happens because every notifier releases its queue lock before calling
//! [`PoolSignals::notify_work`], so the hierarchy is consistent and cannot deadlock.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_common::{BeamError, Result};
use parking_lot::{Condvar, Mutex};

use crate::pid::ProcessId;
use crate::progress::ThreadProgress;
use crate::run_queue::Priority;
use crate::scheduler::{drain_woken_into, slice_begin, slice_end, ProcessRunner, RunOutcome};
use crate::smp::queues::{Acquired, Queues};
use crate::smp::stats::WorkerCounters;
use crate::smp::SharedProcessTable;

/// The shared park/notify state used by the workers, the pool and the timer/dirty sinks.
///
/// A worker parks on the [`work`](PoolSignals::park) condition variable while it has nothing to
/// run; [`wait_for_idle`](PoolSignals::wait_for_idle) parks a caller on the second condition
/// variable until the whole pool is quiescent. A single `state` mutex guards both, so a
/// notification can never be lost between a predicate check and the sleep (the notifier takes
/// the same mutex before signalling).
pub(crate) struct PoolSignals {
    /// Serialises a waiter's predicate check against every notifier.
    state: Mutex<()>,
    /// Signalled when new work may be available (a worker should re-check the queues).
    work: Condvar,
    /// Signalled whenever the pool may have become idle.
    idle: Condvar,
    /// Set by [`PoolSignals::request_stop`]; makes every worker leave its loop promptly.
    stop: AtomicBool,
}

impl PoolSignals {
    /// Fresh signals with nothing parked and no stop requested.
    pub(crate) fn new() -> PoolSignals {
        PoolSignals {
            state: Mutex::new(()),
            work: Condvar::new(),
            idle: Condvar::new(),
            stop: AtomicBool::new(false),
        }
    }

    /// Wakes every parked worker so it re-checks the queues (new work was enqueued).
    pub(crate) fn notify_work(&self) {
        let _guard = self.state.lock();
        self.work.notify_all();
    }

    /// Wakes every idle-waiter (the pool may have become quiescent).
    pub(crate) fn notify_idle(&self) {
        let _guard = self.state.lock();
        self.idle.notify_all();
    }

    /// Wakes **every** waiter on both condition variables.
    ///
    /// Registered as the [`ThreadProgress`] waker (so `safepoint` can nudge a fully-parked pool up
    /// to the required generation) and used by [`PoolSignals::request_stop`].
    pub(crate) fn notify_all(&self) {
        let _guard = self.state.lock();
        self.work.notify_all();
        self.idle.notify_all();
    }

    /// Requests shutdown: sets the stop flag and wakes everyone.
    pub(crate) fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.notify_all();
    }

    /// Whether shutdown has been requested.
    pub(crate) fn is_stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Parks the calling worker until work may be available.
    ///
    /// `has_work` is re-checked **under the signals mutex**, immediately before waiting: if new
    /// work arrived between the worker's last queue probe and this call, `has_work` is true and
    /// the worker returns at once instead of parking (which is what makes the wake-up
    /// loss-free). Returns immediately when shutdown has been requested.
    pub(crate) fn park<F: FnOnce() -> bool>(&self, has_work: F) {
        let mut guard = self.state.lock();
        if self.stop.load(Ordering::Acquire) || has_work() {
            return;
        }
        self.work.wait(&mut guard);
    }

    /// Blocks until the pool is idle (every queue empty and no in-flight pid) or `timeout`
    /// elapses, returning whether it is idle. `None` waits indefinitely.
    pub(crate) fn wait_for_idle(&self, queues: &Queues, timeout: Option<Duration>) -> bool {
        let deadline = timeout.map(|delay| deadline_after(Instant::now(), delay));
        let mut guard = self.state.lock();
        loop {
            if queues.is_idle() {
                return true;
            }
            match deadline {
                None => self.idle.wait(&mut guard),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return queues.is_idle();
                    }
                    if self.idle.wait_for(&mut guard, deadline - now).timed_out() {
                        return queues.is_idle();
                    }
                }
            }
        }
    }
}

/// Saturating `now + delay`, so an unrepresentable delay cannot panic (it clamps to `now`,
/// i.e. an immediate deadline, mirroring `timer.rs`).
fn deadline_after(now: Instant, delay: Duration) -> Instant {
    now.checked_add(delay).unwrap_or(now)
}

/// Stores `error` as the pool's first error, unless one is already recorded.
///
/// A worker cannot return an error to any caller (it runs on a detached thread), so the first
/// one is kept for the pool's [`crate::smp::SchedulerPool::take_error`]; later errors are dropped
/// so the *first* failure is the one a caller observes. This never panics on runtime data.
pub(crate) fn record_error(slot: &Mutex<Option<BeamError>>, error: BeamError) {
    let mut guard = slot.lock();
    if guard.is_none() {
        *guard = Some(error);
    }
}

/// One scheduler thread: its identity, its runner and the shared pool state it works against.
pub(crate) struct Worker<R: ProcessRunner + Send + 'static> {
    /// This scheduler's stable index.
    index: usize,
    /// The per-slice reduction budget.
    reductions: u32,
    /// The maximum number of reductions run per table-lock acquisition (see
    /// [`crate::scheduler::DEFAULT_LOCK_QUANTUM`]).
    lock_quantum: u32,
    /// The shared process table.
    table: Arc<SharedProcessTable>,
    /// The shared queue array.
    queues: Arc<Queues>,
    /// The shared park/notify state.
    signals: Arc<PoolSignals>,
    /// The pool-visible first-error slot.
    errors: Arc<Mutex<Option<BeamError>>>,
    /// The per-scheduler counters.
    counters: Arc<Vec<WorkerCounters>>,
    /// The thread-progress barrier.
    progress: ThreadProgress,
    /// The execution-engine seam for this scheduler.
    runner: R,
}

impl<R: ProcessRunner + Send + 'static> Worker<R> {
    /// Builds a worker for `index`, obtaining its runner from `make_runner`.
    pub(crate) fn new(
        index: usize,
        reductions: u32,
        lock_quantum: u32,
        table: Arc<SharedProcessTable>,
        queues: Arc<Queues>,
        signals: Arc<PoolSignals>,
        errors: Arc<Mutex<Option<BeamError>>>,
        counters: Arc<Vec<WorkerCounters>>,
        progress: ThreadProgress,
        make_runner: &(dyn Fn(usize) -> R + Send + Sync),
    ) -> Worker<R> {
        Worker {
            index,
            reductions,
            lock_quantum,
            table,
            queues,
            signals,
            errors,
            counters,
            progress,
            runner: make_runner(index),
        }
    }

    /// Runs the scheduler loop until shutdown is requested.
    ///
    /// Each iteration updates thread progress (a safepoint opportunity), then acquires work
    /// (own → injector → steal) and runs one slice, or parks when there is none.
    pub(crate) fn run(mut self) {
        let managed = self.progress.managed_thread();
        let counter = &self.counters[self.index];
        let mut buf: Vec<(ProcessId, Priority)> = Vec::new();

        loop {
            // A safepoint opportunity between slices (and on every wake-up).
            managed.update();
            if self.signals.is_stopping() {
                break;
            }

            match self.queues.acquire(self.index) {
                Acquired::Pid(pid) => {
                    counter.set_active(true);
                    buf.clear();
                    // Run the slice as several quanta, re-acquiring the table lock between them
                    // (never a queue lock while it is held); every requeue the helpers make is
                    // buffered in `buf`.
                    if let Err(error) = run_locked_quanta(
                        &self.table,
                        pid,
                        &mut self.runner,
                        self.reductions,
                        self.lock_quantum,
                        |p, priority| buf.push((p, priority)),
                    ) {
                        record_error(&self.errors, error);
                    }
                    counter.record_slice();
                    // Requeue the yielded pid(s) *before* decrementing the in-flight count:
                    // `finish_slice` lowers the shared in-flight counter, so flushing `buf` after
                    // it would leave a window where the queues are empty AND in-flight is zero,
                    // letting `Queues::is_idle()` report idle while a yielded pid is still only in
                    // `buf`. Enqueueing first keeps the pid counted in-flight, so `is_idle` (which
                    // requires both the queue length and the in-flight count to be zero) can never
                    // report idle in that gap.
                    if !buf.is_empty() {
                        self.queues.enqueue_local_batch(self.index, &buf);
                        self.signals.notify_work();
                    }
                    self.queues.finish_slice();
                    counter.set_active(false);
                    self.signals.notify_idle();
                }
                Acquired::Stolen(stolen) => {
                    buf.clear();
                    {
                        // Read each stolen pid's priority under a brief table lock — with **no**
                        // queue lock held (the victim lock was already released by `acquire`).
                        let table = self.table.lock();
                        for &pid in &stolen {
                            let priority = table
                                .get(pid)
                                .map(|process| process.priority())
                                .unwrap_or(Priority::Normal);
                            buf.push((pid, priority));
                        }
                    }
                    let count = stolen.len();
                    self.queues.enqueue_local_batch(self.index, &buf);
                    counter.add_steals(count);
                    // The stolen pids are now queued locally: they leave the in-flight window.
                    self.queues.finish_steal(count);
                    self.signals.notify_work();
                    self.signals.notify_idle();
                }
                Acquired::Idle => {
                    // Nothing to run: park at a safepoint (an inactive managed thread is already
                    // at a safepoint by definition, so it is excluded from the barrier's wait).
                    counter.set_active(false);
                    managed.set_active(false);
                    managed.update();
                    self.signals
                        .park(|| self.queues.has_takeable_work(self.index));
                    managed.set_active(true);
                    managed.update();
                }
            }
        }

        managed.set_active(false);
        counter.set_active(false);
    }
}

/// Runs one slice of the already-dequeued `pid` while holding the process-table lock for at most
/// `quantum` reductions at a time.
///
/// The slice is [`slice_begin`] (schedule-in) → one runner call per quantum, each under its own
/// lock acquisition → [`slice_end`] (schedule-out) with the final outcome. The process keeps its
/// whole `budget` and stays out of the run queue for the entire slice; only the *lock* is released
/// between quanta, so a delivery/signal/another scheduler's slice is serviced without waiting for
/// the whole slice to finish.
///
/// Between two quanta the process is in no run queue, is still `Running` and is counted in-flight
/// by its worker, so no other scheduler can pick it up (every wake-sweep guard requires the state
/// to be `Runnable`). A peer may however **terminate** it (a link exit, an `exit/2`, a monitor on a
/// dying peer) — its links/monitors are resolved under that same lock — so every quantum re-checks
/// that the pid is still in the table and stops the slice when it is gone.
///
/// # Errors
///
/// Propagates the schedule-in/schedule-out, signal-servicing, runner and termination errors.
fn run_locked_quanta<R, F>(
    table: &SharedProcessTable,
    pid: ProcessId,
    runner: &mut R,
    budget: u32,
    quantum: u32,
    mut enqueue: F,
) -> Result<()>
where
    R: ProcessRunner,
    F: FnMut(ProcessId, Priority),
{
    let quantum = quantum.max(1);
    let mut remaining = budget.max(1);

    // Schedule-in (mark the process executing + apply its queued signals), then absorb whatever
    // wake the servicing produced. Nothing to run when the pid vanished or the signals killed it.
    let ready = {
        let mut guard = table.lock();
        let ready = slice_begin(pid, &mut guard)?;
        drain_woken_into(&mut guard, &mut enqueue);
        ready
    };
    if !ready {
        return Ok(());
    }

    loop {
        let chunk = remaining.min(quantum);
        let outcome = {
            let mut guard = table.lock();
            if !guard.contains(pid) {
                // Terminated by a peer while the lock was released between quanta.
                return Ok(());
            }
            let outcome = runner.run(pid, &mut guard, chunk)?;
            drain_woken_into(&mut guard, &mut enqueue);
            outcome
        };

        match outcome {
            // Still inside the slice: run another quantum under a fresh lock acquisition.
            RunOutcome::Yield if remaining > chunk => remaining -= chunk,
            // The slice is over: apply the outcome (requeue at the process's own priority on a
            // yield, leave the queue on wait/suspend, terminate on exit), then refresh the wake
            // list one last time.
            outcome => {
                let mut guard = table.lock();
                slice_end(pid, &mut guard, outcome, &mut enqueue)?;
                drain_woken_into(&mut guard, &mut enqueue);
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the park/notify state and the error slot.

    use super::*;
    use std::sync::mpsc;
    use std::thread;

    #[test]
    fn park_returns_immediately_when_has_work() {
        let signals = PoolSignals::new();
        // `park` must not block: the predicate says there is work.
        signals.park(|| true);
    }

    #[test]
    fn park_returns_immediately_after_stop() {
        let signals = PoolSignals::new();
        signals.request_stop();
        assert!(signals.is_stopping());
        signals.park(|| false);
    }

    #[test]
    fn notify_wakes_a_parked_thread() {
        let signals = Arc::new(PoolSignals::new());
        let (tx, rx) = mpsc::channel();
        let waiter = Arc::clone(&signals);
        let handle = thread::spawn(move || {
            waiter.park(|| false);
            tx.send(()).expect("the receiver is alive");
        });
        // Nudge until the waiter has parked and observed the notification.
        for _ in 0..1000 {
            signals.notify_work();
            if rx.recv_timeout(Duration::from_millis(50)).is_ok() {
                handle.join().expect("the waiter thread joins");
                return;
            }
        }
        panic!("the parked worker was never woken");
    }

    #[test]
    fn wait_for_idle_on_a_fresh_pool_is_true() {
        let signals = PoolSignals::new();
        let queues = Queues::new(2);
        assert!(signals.wait_for_idle(&queues, Some(Duration::from_secs(1))));
        assert!(signals.wait_for_idle(&queues, None));
    }

    #[test]
    fn wait_for_idle_times_out_when_work_is_queued() {
        let signals = PoolSignals::new();
        let queues = Queues::new(1);
        queues.enqueue_injector(ProcessId::new(1, 0), Priority::Normal);
        assert!(!signals.wait_for_idle(&queues, Some(Duration::from_millis(30))));
    }

    #[test]
    fn record_error_keeps_the_first() {
        let slot = Mutex::new(None);
        record_error(&slot, BeamError::BadArg);
        record_error(&slot, BeamError::BadArith);
        let recorded = slot.lock().take().expect("an error was recorded");
        assert!(matches!(recorded, BeamError::BadArg));
        assert!(slot.lock().is_none());
    }
}
