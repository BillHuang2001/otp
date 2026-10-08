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

//! Scheduler statistics: the public snapshots and the internal atomic counters the pool and its
//! workers update.
//!
//! The counters are deliberately plain atomics rather than a lock: a worker updates them on every
//! slice (~thousands of times a second) while an observer reads them lazily for introspection
//! (`erlang:statistics(scheduler_wall_time)`, `statistics(run_queue)` and the `system_info`
//! scheduler counts). Nothing here is a synchronisation primitive — a snapshot is a *relaxed*
//! view, exact only once the pool is idle (see [`crate::smp::SchedulerPool::wait_idle`]).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// A pool-wide snapshot of the scheduler counters.
///
/// Read it with [`crate::smp::SchedulerPool::stats`]; the `run_queue_len` field is the
/// `statistics(run_queue)` observable and the dirty/timer fields mirror the C
/// `erlang:statistics(dirty_*)`/timer-queue counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerStats {
    /// The number of normal scheduler threads in the pool.
    pub schedulers: usize,
    /// Total queued work: the injector plus every scheduler's local run queue.
    pub run_queue_len: usize,
    /// The total slices run across every scheduler.
    pub slices: u64,
    /// The total number of pids stolen across every scheduler.
    pub steals: u64,
    /// How many schedulers are currently running a slice.
    pub active: usize,
    /// Jobs queued or running in the dirty-CPU pool.
    pub dirty_cpu_pending: usize,
    /// Jobs queued or running in the dirty-IO pool.
    pub dirty_io_pending: usize,
    /// Timers currently scheduled and not yet fired or cancelled.
    pub timers_pending: usize,
}

/// A single scheduler's counters.
///
/// Read the whole vector with [`crate::smp::SchedulerPool::scheduler_stats`]; per-scheduler
/// introspection is what `system_info(schedulers)` and the balance/steal diagnostics need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerStat {
    /// The scheduler's stable index (`0..schedulers`).
    pub index: usize,
    /// This scheduler's **local** run-queue length (the injector is excluded).
    pub run_queue_len: usize,
    /// Slices this scheduler has run.
    pub slices: u64,
    /// Pids this scheduler has stolen from other schedulers.
    pub steals: u64,
    /// Whether this scheduler is currently running a slice.
    pub active: bool,
}

/// The mutable atomic counters one worker owns and the pool reads.
///
/// One instance per scheduler index; the pool holds them (in an `Arc<Vec<..>>`) and each worker
/// updates its own entry, so no two threads ever write the same counter.
pub(crate) struct WorkerCounters {
    /// Slices run by this worker.
    slices: AtomicU64,
    /// Pids stolen by this worker.
    steals: AtomicU64,
    /// Whether this worker is currently running a slice.
    active: AtomicBool,
}

impl WorkerCounters {
    /// A fresh, zeroed counter set.
    pub(crate) fn new() -> WorkerCounters {
        WorkerCounters {
            slices: AtomicU64::new(0),
            steals: AtomicU64::new(0),
            active: AtomicBool::new(false),
        }
    }

    /// Records one completed slice.
    pub(crate) fn record_slice(&self) {
        self.slices.fetch_add(1, Ordering::Relaxed);
    }

    /// Records `n` stolen pids.
    pub(crate) fn add_steals(&self, n: usize) {
        self.steals.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Marks this worker active (`on == true`) or idle (`on == false`).
    pub(crate) fn set_active(&self, on: bool) {
        self.active.store(on, Ordering::Relaxed);
    }

    /// A snapshot of these counters, paired with the scheduler `index` and its current local
    /// run-queue `run_queue_len`.
    pub(crate) fn snapshot(&self, index: usize, run_queue_len: usize) -> SchedulerStat {
        SchedulerStat {
            index,
            run_queue_len,
            slices: self.slices.load(Ordering::Relaxed),
            steals: self.steals.load(Ordering::Relaxed),
            active: self.active.load(Ordering::Relaxed),
        }
    }
}

/// The shared **in-flight** counter: pids that have been dequeued for execution (or are in the
/// window between a steal and their local re-enqueue) but whose slice has not finished yet.
///
/// [`crate::smp::SchedulerPool::wait_idle`] requires this to be zero *and* every queue to be
/// empty, otherwise a pid that has just been dequeued would momentarily look like no work at all
/// and a wrongly-idle pool would be reported. Sharing it behind an atomic means a dequeue and its
/// matching decrement can be paired without a lock: the dequeue increments it *while holding the
/// source queue's lock* (so a concurrent idle observer cannot slip between the pop and the
/// increment).
pub(crate) struct InFlight {
    /// The number of dequeued-but-not-yet-finished pids.
    count: AtomicUsize,
}

impl InFlight {
    /// A zeroed in-flight counter.
    pub(crate) fn new() -> InFlight {
        InFlight {
            count: AtomicUsize::new(0),
        }
    }

    /// Adds `n` in-flight pids.
    pub(crate) fn add(&self, n: usize) {
        self.count.fetch_add(n, Ordering::AcqRel);
    }

    /// Removes `n` in-flight pids (saturating at zero so an unbalanced call can never wrap).
    pub(crate) fn sub(&self, n: usize) {
        let mut current = self.count.load(Ordering::Acquire);
        loop {
            let next = current.saturating_sub(n);
            match self.count.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// The current in-flight count.
    pub(crate) fn get(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }
}

impl Default for InFlight {
    fn default() -> InFlight {
        InFlight::new()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the atomic counters and the snapshot shapes.

    use super::*;

    #[test]
    fn worker_counters_snapshot() {
        let counters = WorkerCounters::new();
        let snapshot = counters.snapshot(3, 7);
        assert_eq!(snapshot.index, 3);
        assert_eq!(snapshot.run_queue_len, 7);
        assert_eq!(snapshot.slices, 0);
        assert_eq!(snapshot.steals, 0);
        assert!(!snapshot.active);

        counters.record_slice();
        counters.record_slice();
        counters.add_steals(5);
        counters.set_active(true);

        let snapshot = counters.snapshot(0, 0);
        assert_eq!(snapshot.slices, 2);
        assert_eq!(snapshot.steals, 5);
        assert!(snapshot.active);
    }

    #[test]
    fn inflight_add_sub_and_saturate() {
        let inflight = InFlight::new();
        assert_eq!(inflight.get(), 0);
        inflight.add(3);
        assert_eq!(inflight.get(), 3);
        inflight.sub(1);
        assert_eq!(inflight.get(), 2);
        // An over-large decrement saturates at zero instead of wrapping.
        inflight.sub(100);
        assert_eq!(inflight.get(), 0);
    }
}
