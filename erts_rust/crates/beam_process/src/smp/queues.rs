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

//! The scheduler queue array: a shared **injector** plus one **local** run queue per scheduler,
//! and the work-acquisition protocol (own → injector → steal).
//!
//! This is the SMP counterpart of the C's per-scheduler `erts_runq` plus the global run queue
//! (`erts/emulator/beam/erl_process.c`). Work created outside a scheduler (a spawn, an
//! `erlang:send` from a timer/dirty sink, an I/O completion) enters the shared **injector**;
//! work a scheduler produces itself (a yield requeue, a woken receiver) stays on its own
//! **local** queue. An idle scheduler first drains its own queue, then the injector, then tries to
//! **steal** from its peers round-robin.
//!
//! # Locking discipline (see [`crate::smp`])
//!
//! Every queue here is an independent [`parking_lot::Mutex<RunQueue>`]. The worker loop takes a
//! queue lock, mutates, and releases it *before* touching the process table — the two lock
//! families are never nested. The in-flight bookkeeping is deliberately done **inside** the
//! critical section that pops a pid (see [`Queues::acquire`] and [`Queues::steal`]) so a
//! concurrent [`Queues::is_idle`] observer can never see a pid that is neither queued nor
//! in-flight.

use parking_lot::Mutex;

use crate::pid::ProcessId;
use crate::run_queue::{Priority, RunQueue};
use crate::smp::stats::InFlight;

/// The result of one work-acquisition attempt by a scheduler.
pub(crate) enum Acquired {
    /// A pid to run; the in-flight count already includes it. The caller must eventually call
    /// [`Queues::finish_slice`].
    Pid(ProcessId),
    /// Pids stolen from a peer, still counted as in-flight. The caller must read their priorities
    /// from the process table, re-enqueue them locally with [`Queues::enqueue_local_batch`], and
    /// then call [`Queues::finish_steal`] with the stolen count.
    Stolen(Vec<ProcessId>),
    /// No work anywhere: the caller should park.
    Idle,
}

/// The queue array shared by every scheduler thread.
pub(crate) struct Queues {
    /// Work created outside a scheduler; contended but short-lived.
    injector: Mutex<RunQueue>,
    /// One local run queue per scheduler index.
    locals: Vec<Mutex<RunQueue>>,
    /// The shared dequeued-but-not-finished counter.
    inflight: InFlight,
}

impl Queues {
    /// A queue array with `schedulers` local queues (`schedulers >= 1`).
    pub(crate) fn new(schedulers: usize) -> Queues {
        Queues {
            injector: Mutex::new(RunQueue::new()),
            locals: (0..schedulers)
                .map(|_| Mutex::new(RunQueue::new()))
                .collect(),
            inflight: InFlight::new(),
        }
    }

    /// Appends `pid` to the shared injector at `priority` (the spawn / cross-thread send path).
    pub(crate) fn enqueue_injector(&self, pid: ProcessId, priority: Priority) {
        self.injector.lock().enqueue_with_priority(pid, priority);
    }

    /// Appends a batch to the shared injector at each item's priority.
    pub(crate) fn enqueue_injector_batch(&self, items: &[(ProcessId, Priority)]) {
        let mut injector = self.injector.lock();
        for &(pid, priority) in items {
            injector.enqueue_with_priority(pid, priority);
        }
    }

    /// Appends a batch to scheduler `index`'s local queue at each item's priority.
    pub(crate) fn enqueue_local_batch(&self, index: usize, items: &[(ProcessId, Priority)]) {
        let mut local = self.locals[index].lock();
        for &(pid, priority) in items {
            local.enqueue_with_priority(pid, priority);
        }
    }

    /// Moves `pid` from whatever level holds it to the tail of `priority`'s FIFO **iff** it is
    /// currently queued at a different level, reporting whether it was moved.
    ///
    /// The shared injector and every local queue are searched in turn, taking **one queue lock
    /// at a time** and never holding two (see the module-level locking discipline). A pid that is
    /// not queued anywhere — it is in flight (being executed by a scheduler), waiting, suspended
    /// or terminated — is left untouched, so this never inserts a pid that was not already
    /// queued. The cost is one queue-lock acquisition per queue, i.e. `O(schedulers)`.
    pub(crate) fn reprioritize(&self, pid: ProcessId, priority: Priority) -> bool {
        {
            let mut injector = self.injector.lock();
            if injector.reprioritize(pid, priority) {
                return true;
            }
        }
        for local in &self.locals {
            let mut queue = local.lock();
            if queue.reprioritize(pid, priority) {
                return true;
            }
        }
        false
    }
    /// Tries to acquire one pid: own local queue → injector → steal from a peer.
    ///
    /// A pid obtained from the local queue or the injector has the in-flight count incremented
    /// **inside** the same critical section that popped it; a successful steal returns the stolen
    /// batch with the in-flight count already incremented (the caller finishes the hand-off).
    pub(crate) fn acquire(&self, index: usize) -> Acquired {
        {
            let mut local = self.locals[index].lock();
            if let Some(pid) = local.dequeue() {
                self.inflight.add(1);
                return Acquired::Pid(pid);
            }
        }
        {
            let mut injector = self.injector.lock();
            if let Some(pid) = injector.dequeue() {
                self.inflight.add(1);
                return Acquired::Pid(pid);
            }
        }
        self.steal(index)
    }

    /// Round-robin work stealing: starting at `(index + 1) % n` and wrapping, take up to half of
    /// the first non-empty peer queue (highest priority first — see
    /// [`RunQueue::split_off_half`]).
    fn steal(&self, index: usize) -> Acquired {
        let n = self.locals.len();
        // Start at the next scheduler and wrap; `n` steps visit every peer exactly once.
        for offset in 1..n {
            let victim = (index + offset) % n;
            let stolen = {
                let mut queue = self.locals[victim].lock();
                let stolen = queue.split_off_half();
                if !stolen.is_empty() {
                    // Count the window between "removed from the victim" and "enqueued locally"
                    // so the pids are never momentarily neither queued nor in-flight.
                    self.inflight.add(stolen.len());
                }
                stolen
            };
            if !stolen.is_empty() {
                return Acquired::Stolen(stolen);
            }
        }
        Acquired::Idle
    }

    /// Marks one in-flight pid (from [`Acquired::Pid`]) finished.
    pub(crate) fn finish_slice(&self) {
        self.inflight.sub(1);
    }

    /// Marks the stolen batch (from [`Acquired::Stolen`]) as no longer in-flight, once every
    /// stolen pid has been enqueued locally.
    pub(crate) fn finish_steal(&self, count: usize) {
        self.inflight.sub(count);
    }

    /// The injector's length.
    pub(crate) fn injector_len(&self) -> usize {
        self.injector.lock().len()
    }

    /// Scheduler `index`'s local length.
    pub(crate) fn local_len(&self, index: usize) -> usize {
        self.locals[index].lock().len()
    }

    /// Every scheduler's local length (the injector is excluded).
    pub(crate) fn local_lengths(&self) -> Vec<usize> {
        self.locals.iter().map(|q| q.lock().len()).collect()
    }

    /// The total queued work: the injector plus every local queue.
    pub(crate) fn total_len(&self) -> usize {
        let injector = self.injector_len();
        let locals: usize = self.locals.iter().map(|q| q.lock().len()).sum();
        injector + locals
    }

    /// Whether scheduler `index` could take work right now: its own local queue or the injector
    /// is non-empty. This is the predicate a parked worker re-checks (a peer's queue is excluded
    /// because a peer holding a single pid is not stealable and re-checking it would spin).
    pub(crate) fn has_takeable_work(&self, index: usize) -> bool {
        if !self.locals[index].lock().is_empty() {
            return true;
        }
        !self.injector.lock().is_empty()
    }

    /// Whether the whole pool is idle: every queue empty **and** no in-flight pid.
    ///
    /// The queue lengths are checked **before** the in-flight count, and the order is
    /// load-bearing. A pop and its matching in-flight increment happen in the same queue-lock
    /// critical section (see [`Queues::acquire`]/[`Queues::steal`]), so once `total_len()` observes
    /// an empty queue the pop's in-flight increment is already visible to the next read. Checking
    /// the count first could instead read `0`, let a worker pop the last pid, and then read a now
    /// empty queue — falsely reporting idle while a slice is still running.
    pub(crate) fn is_idle(&self) -> bool {
        self.total_len() == 0 && self.inflight.get() == 0
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the queue array: enqueue/dequeue across the injector and locals, the
    //! in-flight accounting, and the steal / idle predicates.

    use super::*;

    fn pid(n: u64) -> ProcessId {
        ProcessId::new(n, 0)
    }

    #[test]
    fn local_then_injector_then_idle() {
        let queues = Queues::new(2);
        assert!(matches!(queues.acquire(0), Acquired::Idle));
        assert!(queues.is_idle());

        queues.enqueue_injector(pid(1), Priority::Normal);
        assert!(!queues.is_idle());
        match queues.acquire(0) {
            Acquired::Pid(p) => assert_eq!(p, pid(1)),
            _ => panic!("the injector had work"),
        }
        // Dequeued but not finished: still not idle.
        assert!(!queues.is_idle());
        queues.finish_slice();
        assert!(queues.is_idle());

        assert_eq!(queues.total_len(), 0);
        assert_eq!(queues.injector_len(), 0);
        assert_eq!(queues.local_lengths(), vec![0, 0]);
    }

    #[test]
    fn local_queue_is_served_before_the_injector() {
        let queues = Queues::new(1);
        queues.enqueue_local_batch(0, &[(pid(1), Priority::Normal)]);
        queues.enqueue_injector(pid(2), Priority::Normal);
        queues.enqueue_injector(pid(3), Priority::Normal);

        match queues.acquire(0) {
            Acquired::Pid(p) => assert_eq!(p, pid(1), "own local queue first"),
            _ => panic!("work was available"),
        }
        queues.finish_slice();
        match queues.acquire(0) {
            Acquired::Pid(p) => assert_eq!(p, pid(2), "then the injector, FIFO"),
            _ => panic!("work was available"),
        }
        queues.finish_slice();
    }

    #[test]
    fn stealing_takes_half_and_counts_inflight() {
        let queues = Queues::new(2);
        // Put four pids on scheduler 1.
        queues.enqueue_local_batch(
            1,
            &[
                (pid(1), Priority::Normal),
                (pid(2), Priority::Normal),
                (pid(3), Priority::Normal),
                (pid(4), Priority::Normal),
            ],
        );
        assert_eq!(queues.local_len(1), 4);

        // Scheduler 0 has no local work and the injector is empty, so it steals from 1.
        match queues.acquire(0) {
            Acquired::Stolen(stolen) => {
                assert_eq!(stolen.len(), 2, "half of the victim's queue");
                assert_eq!(queues.local_len(1), 2, "the victim keeps half");
                // The stolen batch is still in-flight until it is re-enqueued locally.
                assert!(!queues.is_idle());
                queues.enqueue_local_batch(
                    0,
                    &stolen
                        .iter()
                        .map(|&p| (p, Priority::Normal))
                        .collect::<Vec<_>>(),
                );
                queues.finish_steal(stolen.len());
            }
            _ => panic!("scheduler 0 should have stolen from scheduler 1"),
        }
        assert_eq!(queues.local_len(0), 2);
        assert_eq!(queues.local_lengths(), vec![2, 2]);
        // Two pids on each local queue -> 4 total, none in-flight.
        assert_eq!(queues.total_len(), 4);
    }

    #[test]
    fn a_single_pid_is_not_stealable() {
        let queues = Queues::new(2);
        queues.enqueue_local_batch(1, &[(pid(9), Priority::Normal)]);
        // Scheduler 0 cannot take the lone pid and must park.
        assert!(matches!(queues.acquire(0), Acquired::Idle));
        assert!(!queues.has_takeable_work(0));
        assert_eq!(queues.local_len(1), 1, "the victim keeps its only pid");
    }

    #[test]
    fn has_takeable_work_sees_own_queue_and_injector() {
        let queues = Queues::new(2);
        assert!(!queues.has_takeable_work(0));
        queues.enqueue_local_batch(1, &[(pid(2), Priority::High)]);
        assert!(!queues.has_takeable_work(0), "a peer's queue is not 'own'");
        queues.enqueue_injector(pid(3), Priority::Low);
        assert!(queues.has_takeable_work(0), "the injector counts");
        queues.enqueue_local_batch(0, &[(pid(1), Priority::High)]);
        assert!(queues.has_takeable_work(0), "own local queue counts");
    }

    #[test]
    fn reprioritize_moves_a_queued_pid_between_levels() {
        let queues = Queues::new(2);
        // pid(1) at LOW and pid(2) at NORMAL, both on scheduler 0's local queue.
        queues.enqueue_local_batch(0, &[(pid(1), Priority::Low), (pid(2), Priority::Normal)]);
        assert_eq!(queues.local_len(0), 2);

        // Promote pid(1) to MAX: it must now be served before pid(2), and the total is unchanged.
        assert!(queues.reprioritize(pid(1), Priority::Max));
        assert_eq!(queues.total_len(), 2, "the pid moved, it was not duplicated");
        match queues.acquire(0) {
            Acquired::Pid(p) => assert_eq!(p, pid(1), "the promoted pid is served first"),
            _ => panic!("work was available"),
        }
        queues.finish_slice();
        match queues.acquire(0) {
            Acquired::Pid(p) => assert_eq!(p, pid(2)),
            _ => panic!("work was available"),
        }
        queues.finish_slice();

        // A pid that is not queued anywhere is never inserted.
        assert!(!queues.reprioritize(pid(9), Priority::High));
        assert_eq!(queues.total_len(), 0);
    }

    #[test]
    fn reprioritize_searches_the_injector_and_every_local_queue() {
        let queues = Queues::new(2);
        queues.enqueue_injector(pid(1), Priority::Low);
        queues.enqueue_local_batch(1, &[(pid(2), Priority::Low)]);

        // Already at the requested level: nothing moves.
        assert!(!queues.reprioritize(pid(1), Priority::Low));

        // The injector pid is found and moved (its queue length is unchanged).
        assert!(queues.reprioritize(pid(1), Priority::High));
        assert_eq!(queues.injector_len(), 1);
        assert_eq!(queues.total_len(), 2);

        // A peer (scheduler 1) local pid is found and moved too.
        assert!(queues.reprioritize(pid(2), Priority::Max));
        assert_eq!(queues.local_len(1), 1);
        assert_eq!(queues.total_len(), 2);
    }
}
