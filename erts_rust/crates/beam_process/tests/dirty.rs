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

//! Integration evidence for the dirty scheduler pool ([`beam_process::dirty`]).
//!
//! Pins the M1 dirty-scheduler guarantees:
//!
//! * jobs submitted to a pool run **on dirty worker threads** (not the submitting thread) and
//!   every completion is routed to the installed [`DirtyCompletionSink`];
//! * a `Reschedule(pid)` / `Deliver { .. }` outcome is routed **verbatim**;
//! * [`DirtyPool::pending`] rises on submit and drains back to zero;
//! * `N` workers run `N` jobs **concurrently** (a [`Barrier`] proves it without sleeping);
//! * the [`DirtyPool`] lifecycle: `shutdown`/`join`, and submitting after shutdown;
//! * a pool with `0` workers buffers (never drops-on-submit, never panics) and discards the
//!   buffered jobs at `join`;
//! * a sink installed **after** a job was submitted still receives that job's completion.
//!
//! # Flakiness
//!
//! Correctness is proven with **synchronization primitives** — [`Barrier`]s, channels and
//! [`mpsc::Receiver::recv_timeout`] — never with `thread::sleep`. Every blocking wait has a
//! generous timeout, so a wedged pool fails the test rather than hanging it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Barrier};
use std::thread::{self, ThreadId};
use std::time::Duration;

use beam_process::prelude::*;
use beam_term::prelude::make_small;
use parking_lot::{Condvar, Mutex};

/// A generous upper bound for any single wait; a healthy pool finishes in microseconds.
const TIMEOUT: Duration = Duration::from_secs(20);

/// A completion sink that forwards every outcome to a channel, so the test thread can assert on
/// completions without polling. The [`mpsc::Sender`] sits behind a mutex (a `Sender` is `Send`
/// but not `Sync`) to satisfy the sink's `Send + Sync` bound.
struct RecordingSink {
    tx: Mutex<mpsc::Sender<DirtyOutcome>>,
}

impl RecordingSink {
    fn new(tx: mpsc::Sender<DirtyOutcome>) -> RecordingSink {
        RecordingSink { tx: Mutex::new(tx) }
    }
}

impl DirtyCompletionSink for RecordingSink {
    fn dirty_completed(&self, outcome: DirtyOutcome) {
        // The receiver may already be gone once a test has finished asserting; a failed send is
        // not interesting (and must not panic on the worker thread).
        let _ = self.tx.lock().send(outcome);
    }
}

/// A two-party gate: jobs block on it until the test opens it, which makes the peak `pending`
/// count observable deterministically.
struct Gate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl Gate {
    fn new() -> Gate {
        Gate {
            open: Mutex::new(false),
            opened: Condvar::new(),
        }
    }

    /// Blocks the caller until the gate is opened.
    fn wait(&self) {
        let mut open = self.open.lock();
        while !*open {
            self.opened.wait(&mut open);
        }
    }

    /// Releases everyone waiting on the gate.
    fn open(&self) {
        let mut open = self.open.lock();
        *open = true;
        self.opened.notify_all();
    }
}

/// Every completion arrives, the jobs ran on dirty worker threads, and `pending` drains to zero.
#[test]
fn jobs_run_on_dirty_workers_and_every_completion_is_routed() {
    let pool = DirtyPool::start(2, 2);
    assert_eq!(pool.cpu_schedulers(), 2);
    assert_eq!(pool.io_schedulers(), 2);

    let (tx, rx) = mpsc::channel();
    pool.set_sink(Arc::new(RecordingSink::new(tx)));

    const K: usize = 8;
    let ran = Arc::new(AtomicUsize::new(0));
    let ids = Arc::new(Mutex::new(Vec::<ThreadId>::new()));

    for i in 0..K {
        for kind in DirtyKind::ALL {
            let ran = Arc::clone(&ran);
            let ids = Arc::clone(&ids);
            pool.submit(
                kind,
                Box::new(move || {
                    ran.fetch_add(1, Ordering::SeqCst);
                    ids.lock().push(thread::current().id());
                    if i % 2 == 0 {
                        DirtyOutcome::Done
                    } else {
                        DirtyOutcome::Reschedule(ProcessId::new(i as u64, 0))
                    }
                }),
            );
        }
    }

    // Every completion is routed within the timeout (the receiver never hangs: a wedged pool
    // fails the test instead).
    for _ in 0..(K * 2) {
        rx.recv_timeout(TIMEOUT)
            .expect("every dirty completion arrives within the timeout");
    }

    assert_eq!(
        ran.load(Ordering::SeqCst),
        K * 2,
        "every submitted job ran exactly once"
    );

    // `pending` drains to zero once all completions have been observed.
    assert_eq!(pool.pending(DirtyKind::Cpu), 0);
    assert_eq!(pool.pending(DirtyKind::Io), 0);

    // The jobs ran on dirty worker threads, never on the submitting thread.
    let ids = ids.lock().clone();
    assert_eq!(ids.len(), K * 2, "each job recorded its thread id");
    let caller = thread::current().id();
    assert!(
        ids.iter().all(|id| *id != caller),
        "dirty jobs must run off the submitting thread"
    );

    pool.shutdown();
    pool.join();
}

/// `Reschedule(pid)` and `Deliver { .. }` outcomes are routed verbatim to the sink.
#[test]
fn completion_outcomes_are_routed_verbatim() {
    let pool = DirtyPool::start(1, 1);
    let (tx, rx) = mpsc::channel();
    pool.set_sink(Arc::new(RecordingSink::new(tx)));

    let reschedule_pid = ProcessId::new(11, 22);
    let deliver_pid = ProcessId::new(33, 44);
    let message = make_small(1234);

    pool.submit(DirtyKind::Cpu, Box::new(|| DirtyOutcome::Done));
    pool.submit(
        DirtyKind::Cpu,
        Box::new(move || DirtyOutcome::Reschedule(reschedule_pid)),
    );
    pool.submit(
        DirtyKind::Io,
        Box::new(move || DirtyOutcome::Deliver {
            target: deliver_pid,
            message,
        }),
    );

    let mut got = Vec::new();
    for _ in 0..3 {
        got.push(
            rx.recv_timeout(TIMEOUT)
                .expect("every outcome is routed within the timeout"),
        );
    }

    assert!(got.contains(&DirtyOutcome::Done), "Done routed: {got:?}");
    assert!(
        got.contains(&DirtyOutcome::Reschedule(reschedule_pid)),
        "Reschedule routed verbatim: {got:?}"
    );
    assert!(
        got.contains(&DirtyOutcome::Deliver {
            target: deliver_pid,
            message,
        }),
        "Deliver routed verbatim: {got:?}"
    );

    pool.join();
}

/// `pending` counts queued + running: it is at its peak while jobs are in flight and returns to
/// zero once they complete.
#[test]
fn pending_rises_on_submit_and_drains_after_completion() {
    let pool = DirtyPool::start(2, 0);
    let (tx, rx) = mpsc::channel();
    pool.set_sink(Arc::new(RecordingSink::new(tx)));

    const K: usize = 4;
    let gate = Arc::new(Gate::new());
    let (started_tx, started_rx) = mpsc::channel();

    for _ in 0..K {
        let gate = Arc::clone(&gate);
        let started_tx = started_tx.clone();
        pool.submit(
            DirtyKind::Cpu,
            Box::new(move || {
                let _ = started_tx.send(());
                gate.wait();
                DirtyOutcome::Done
            }),
        );
    }
    drop(started_tx);

    // Wait until at least one job is actually running (not merely queued)...
    started_rx
        .recv_timeout(TIMEOUT)
        .expect("a submitted job starts running");

    // ...then every one of the K jobs is still outstanding: 2 running (blocked on the gate) +
    // 2 queued, and none can complete while the gate is shut.
    assert_eq!(
        pool.pending(DirtyKind::Cpu),
        K,
        "pending counts both running and queued jobs"
    );

    // Open the gate; all K jobs finish and their completions arrive.
    gate.open();
    for _ in 0..K {
        rx.recv_timeout(TIMEOUT).expect("every completion arrives");
    }
    assert_eq!(
        pool.pending(DirtyKind::Cpu),
        0,
        "pending drains once every job has completed"
    );

    pool.join();
}

/// A pool with `N` workers runs `N` jobs at the same time (the barrier only releases if all `N`
/// are resident on distinct workers).
#[test]
fn n_workers_run_n_jobs_concurrently() {
    const N: usize = 4;
    let pool = DirtyPool::start(N, 0);
    assert_eq!(pool.cpu_schedulers(), N);

    let (tx, rx) = mpsc::channel();
    pool.set_sink(Arc::new(RecordingSink::new(tx)));

    // The barrier has N jobs + the test thread as participants.
    let barrier = Arc::new(Barrier::new(N + 1));
    let (started_tx, started_rx) = mpsc::channel();

    for _ in 0..N {
        let barrier = Arc::clone(&barrier);
        let started_tx = started_tx.clone();
        pool.submit(
            DirtyKind::Cpu,
            Box::new(move || {
                let _ = started_tx.send(());
                barrier.wait();
                DirtyOutcome::Done
            }),
        );
    }
    drop(started_tx);

    // Every job signals before it blocks on the barrier; receiving all N signals proves all N
    // are concurrently resident (a pool with fewer workers would time out here).
    for _ in 0..N {
        started_rx
            .recv_timeout(TIMEOUT)
            .expect("every job starts concurrently");
    }
    // The jobs are all at (or about to reach) the barrier, so this cannot block.
    barrier.wait();

    for _ in 0..N {
        rx.recv_timeout(TIMEOUT).expect("every job completes");
    }
    assert_eq!(pool.pending(DirtyKind::Cpu), 0);

    pool.shutdown();
    pool.join();
}

/// The lifecycle: `shutdown` then `join` returns once the workers have exited; submitting after
/// shutdown does not panic and the job is dropped.
#[test]
fn shutdown_join_and_submit_after_shutdown() {
    let pool = DirtyPool::start(1, 1);
    pool.shutdown();
    // No job was outstanding, so nothing is pending...
    assert_eq!(pool.pending(DirtyKind::Cpu), 0);
    // ...and a post-shutdown submit is dropped (never panic, never run).
    pool.submit(DirtyKind::Cpu, Box::new(|| DirtyOutcome::Done));
    assert_eq!(
        pool.pending(DirtyKind::Cpu),
        0,
        "a job submitted after shutdown is dropped, not queued"
    );
    pool.join();
}

/// A pool with no workers buffers jobs (never drops them on submit, never panics); the buffered
/// jobs are discarded when the pool is joined.
#[test]
fn zero_workers_buffers_jobs_without_panicking() {
    let pool = DirtyPool::start(0, 0);
    assert_eq!(pool.cpu_schedulers(), 0);
    assert_eq!(pool.io_schedulers(), 0);

    pool.submit(DirtyKind::Cpu, Box::new(|| DirtyOutcome::Done));
    pool.submit(DirtyKind::Io, Box::new(|| DirtyOutcome::Done));

    // Documented behaviour: with no worker for a kind the job is *buffered* — enqueued and
    // counted by `pending`, awaiting a worker that does not exist.
    assert_eq!(pool.pending(DirtyKind::Cpu), 1);
    assert_eq!(pool.pending(DirtyKind::Io), 1);

    // Joining has no workers to wait for and discards the buffered jobs.
    pool.join();
}

/// A sink installed *after* a job was submitted still receives that job's completion (the
/// outcome is buffered until the sink exists).
#[test]
fn sink_installed_after_submit_receives_the_completion() {
    let pool = DirtyPool::start(1, 1);
    let expected = DirtyOutcome::Reschedule(ProcessId::new(5, 6));
    pool.submit(DirtyKind::Cpu, Box::new(move || expected));

    // Install the sink only after the hand-off: whether the job finished before or after this
    // call, its completion must still be delivered exactly once.
    let (tx, rx) = mpsc::channel();
    pool.set_sink(Arc::new(RecordingSink::new(tx)));

    let outcome = rx
        .recv_timeout(TIMEOUT)
        .expect("the completion is delivered even though the sink arrived late");
    assert_eq!(outcome, DirtyOutcome::Reschedule(ProcessId::new(5, 6)));

    pool.join();
}

/// A panicking job is isolated: it does not kill the worker, its `pending` slot is released, and
/// the pool keeps draining the queue.
#[test]
fn a_panicking_job_does_not_kill_the_pool() {
    let pool = DirtyPool::start(1, 0);
    let (tx, rx) = mpsc::channel();
    pool.set_sink(Arc::new(RecordingSink::new(tx)));

    // The panic is reported by the default hook (never silently swallowed) and caught on the
    // worker thread, which then picks up the next job.
    pool.submit(
        DirtyKind::Cpu,
        Box::new(|| -> DirtyOutcome { panic!("dirty job boom") }),
    );
    pool.submit(DirtyKind::Cpu, Box::new(|| DirtyOutcome::Done));

    let outcome = rx
        .recv_timeout(TIMEOUT)
        .expect("the worker survives the panic and completes the next job");
    assert_eq!(outcome, DirtyOutcome::Done);
    assert_eq!(
        pool.pending(DirtyKind::Cpu),
        0,
        "the panicking job's pending slot is released"
    );

    pool.join();
}

/// The default pool is a functional one (one worker of each kind), not an inert zero-worker pool.
#[test]
fn default_pool_has_one_worker_of_each_kind() {
    let pool = DirtyPool::default();
    assert_eq!(pool.cpu_schedulers(), 1);
    assert_eq!(pool.io_schedulers(), 1);
    assert_eq!(pool.schedulers(DirtyKind::Cpu), 1);
    assert_eq!(pool.schedulers(DirtyKind::Io), 1);
    pool.join();
}

/// Regression: `Queue::request_shutdown` must take the `jobs` lock so its notification cannot be
/// lost to a worker that has just read `shutdown` as false and is about to park. Before the fix
/// the worker could sleep forever and `DirtyPool::join` would hang; repeating a start-with-idle-
/// workers + `join` cycle drives that interleaving often enough to catch a regression.
///
/// No `thread::sleep` is used. `join` blocks until every worker has exited, so if the wake-up is
/// lost this test hangs rather than passing — which is exactly the failure mode being guarded
/// against.
#[test]
fn repeated_start_join_does_not_lose_the_shutdown_wakeup() {
    for _ in 0..2_000 {
        let pool = DirtyPool::start(1, 0);
        pool.join();
    }
}
