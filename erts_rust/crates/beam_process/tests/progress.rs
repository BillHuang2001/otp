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

//! Integration tests for the cross-scheduler thread-progress barrier.
//!
//! These tests are **deterministic**. A test only ever asserts that `wait` returned *before*
//! something happened when the barrier's own rules make that outcome impossible (so the
//! assertion cannot depend on scheduling speed), and a test that must show `wait` returning
//! gives the event a generous timeout so a failure panics loudly instead of hanging. No test
//! uses `thread::sleep` for correctness: cross-thread ordering is established with channels
//! (`recv_timeout`) and one `Barrier`, never with wall-clock delays.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use beam_process::ThreadProgress;

/// A generous upper bound for an event that the barrier *must* make happen. Exceeding it is
/// a failed test, not a retry.
const LONG: Duration = Duration::from_secs(10);

/// A short window used only to assert that something *cannot* have happened yet: the
/// waiter's predicate stays false until this test itself releases the missing thread, so
/// nothing can be received regardless of how fast or slow the machine is.
const SHORT: Duration = Duration::from_millis(200);

#[test]
fn wait_returns_only_after_every_active_managed_thread_reaches_the_generation() {
    const WORKERS: usize = 4;

    let progress = ThreadProgress::new();

    // Registering from the parent thread keeps `managed_count`/the registry ordering
    // deterministic before the workers start.
    let mut flags = Vec::with_capacity(WORKERS);
    let mut gates = Vec::with_capacity(WORKERS);
    let mut workers = Vec::with_capacity(WORKERS);
    for _ in 0..WORKERS {
        let managed = progress.managed_thread();
        let confirmed = Arc::new(AtomicBool::new(false));
        flags.push(Arc::clone(&confirmed));
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        gates.push(gate_tx);
        workers.push(thread::spawn(move || {
            assert!(
                gate_rx.recv_timeout(LONG).is_ok(),
                "the test must open every worker's gate"
            );
            // Publish the flag *before* confirming progress: an observer that has seen the
            // confirmed progress (release/acquire on the same slot) therefore also sees the
            // flag, so the flag assertion below cannot be stale.
            confirmed.store(true, Ordering::Release);
            managed.update();
        }));
    }
    assert_eq!(progress.managed_count(), WORKERS);

    let generation = progress.bump();

    // A dedicated waiter keeps the "did it block?" observation independent of the parent
    // thread's own timing.
    let (waited_tx, waited_rx) = mpsc::channel::<()>();
    let waiter = {
        let progress = progress.clone();
        thread::spawn(move || {
            progress.wait(generation);
            let _ = waited_tx.send(());
        })
    };

    // No worker has confirmed progress (they are all parked on their gates), so `wait`
    // cannot have returned.
    assert!(
        matches!(
            waited_rx.recv_timeout(SHORT),
            Err(RecvTimeoutError::Timeout)
        ),
        "wait returned while active managed threads had not reached the generation"
    );

    for gate in gates {
        gate.send(()).expect("every worker thread is alive");
    }

    assert!(
        waited_rx.recv_timeout(LONG).is_ok(),
        "wait must return once every active managed thread has confirmed progress \
         (a timeout means the barrier lost a confirmation)"
    );
    for (i, confirmed) in flags.iter().enumerate() {
        assert!(
            confirmed.load(Ordering::Acquire),
            "worker {i} cannot have confirmed progress"
        );
    }
    assert_eq!(
        progress.generation(),
        generation,
        "waiting must never advance the generation"
    );

    waiter.join().expect("the waiter thread must not panic");
    for worker in workers {
        worker.join().expect("the worker thread must not panic");
    }
}

#[test]
fn wait_returns_immediately_when_the_threads_are_already_at_the_generation() {
    let progress = ThreadProgress::new();
    let generation = progress.bump();
    let managed = progress.managed_thread();

    // A thread that is already at the generation (or past it) must not be waited for.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let worker = thread::spawn(move || {
        managed.update();
        let _ = ready_tx.send(());
        // Keep the slot alive while the waiter runs; a dropped sender would end this park.
        let _ = release_rx.recv_timeout(LONG);
    });
    ready_rx
        .recv_timeout(LONG)
        .expect("the managed thread must confirm progress");

    let (waited_tx, waited_rx) = mpsc::channel::<()>();
    let waiter = {
        let progress = progress.clone();
        thread::spawn(move || {
            progress.wait(generation);
            let _ = waited_tx.send(());
        })
    };
    assert!(
        waited_rx.recv_timeout(SHORT).is_ok(),
        "wait must not block when every active managed thread has already reached the generation"
    );

    waiter.join().expect("the waiter thread must not panic");
    release_tx.send(()).expect("the worker thread is alive");
    worker.join().expect("the worker thread must not panic");
}

/// Commands the command-driven worker of the inactivity test executes.
#[derive(Clone, Copy, Debug)]
enum Cmd {
    /// Return to the safepoint: mark this thread active again.
    Reactivate,
    /// Confirm the current global generation.
    Update,
    /// Leave the worker loop.
    Stop,
}

#[test]
fn an_inactive_managed_thread_is_excluded_and_reactivation_reincludes_it() {
    let progress = ThreadProgress::new();
    let generation = progress.bump(); // 1 — no managed thread has confirmed it yet

    let managed = progress.managed_thread();
    let (report_tx, report_rx) = mpsc::channel::<(u64, bool)>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
    let (ack_tx, ack_rx) = mpsc::channel::<()>();
    let worker = thread::spawn(move || {
        // Park: an idle thread is at a safepoint and must not be waited for.
        managed.set_active(false);
        report_tx
            .send((managed.progress(), managed.is_active()))
            .expect("the test must observe the parked state");
        while let Ok(cmd) = cmd_rx.recv_timeout(LONG) {
            match cmd {
                Cmd::Reactivate => managed.set_active(true),
                Cmd::Update => managed.update(),
                Cmd::Stop => break,
            }
            if ack_tx.send(()).is_err() {
                break;
            }
        }
    });

    let (parked_progress, parked_active) = report_rx
        .recv_timeout(LONG)
        .expect("the worker must report its parked state");
    assert!(!parked_active, "the worker parked itself");
    assert!(
        parked_progress < generation,
        "the parked worker never confirmed the generation, so excluding it is load-bearing"
    );

    // The one managed thread is inactive → nothing to wait for, even though it is behind.
    progress.wait(generation);
    assert_eq!(progress.generation(), generation);

    // Re-activating the thread must re-include it in the wait.
    cmd_tx
        .send(Cmd::Reactivate)
        .expect("the worker thread is alive");
    ack_rx
        .recv_timeout(LONG)
        .expect("the worker must re-activate");

    let (waited_tx, waited_rx) = mpsc::channel::<()>();
    let waiter = {
        let progress = progress.clone();
        thread::spawn(move || {
            progress.wait(generation);
            let _ = waited_tx.send(());
        })
    };
    assert!(
        matches!(
            waited_rx.recv_timeout(SHORT),
            Err(RecvTimeoutError::Timeout)
        ),
        "a re-activated managed thread must be waited for again"
    );

    cmd_tx
        .send(Cmd::Update)
        .expect("the worker thread is alive");
    ack_rx
        .recv_timeout(LONG)
        .expect("the worker must confirm progress");
    assert!(
        waited_rx.recv_timeout(LONG).is_ok(),
        "wait must return once the re-activated thread confirmed progress"
    );

    cmd_tx.send(Cmd::Stop).expect("the worker thread is alive");
    waiter.join().expect("the waiter thread must not panic");
    worker.join().expect("the worker thread must not panic");
}

#[test]
fn concurrent_bumps_produce_every_generation_exactly_once() {
    const BUMPERS: usize = 8;
    const BUMPS: usize = 1_000;
    const TOTAL: u64 = (BUMPERS * BUMPS) as u64;

    let progress = ThreadProgress::new();
    let mut bumpers = Vec::with_capacity(BUMPERS);
    for _ in 0..BUMPERS {
        let progress = progress.clone();
        bumpers.push(thread::spawn(move || {
            (0..BUMPS).map(|_| progress.bump()).collect::<Vec<u64>>()
        }));
    }

    let mut seen: Vec<u64> = bumpers
        .into_iter()
        .flat_map(|bumper| bumper.join().expect("a bumper thread must not panic"))
        .collect();
    seen.sort_unstable();

    assert_eq!(seen.len(), TOTAL as usize, "every bump returned a value");
    assert_eq!(
        seen,
        (1..=TOTAL).collect::<Vec<u64>>(),
        "the returned generations are exactly 1..=BUMPERS*BUMPS, with no gap and no repeat"
    );
    assert_eq!(progress.generation(), TOTAL, "the counter is monotonic");
}

#[test]
fn block_and_unblock_nest_and_never_underflow() {
    let progress = ThreadProgress::new();
    assert!(!progress.is_blocking());
    assert_eq!(progress.blocking_count(), 0);

    // An unbalanced unblock is a no-op, never an underflow panic.
    progress.unblock();
    assert_eq!(progress.blocking_count(), 0);
    assert!(!progress.is_blocking());

    progress.block();
    progress.block();
    assert!(progress.is_blocking());
    assert_eq!(progress.blocking_count(), 2);

    progress.unblock();
    assert!(
        progress.is_blocking(),
        "one nested block is still outstanding"
    );
    assert_eq!(progress.blocking_count(), 1);

    progress.unblock();
    assert!(!progress.is_blocking());
    assert_eq!(progress.blocking_count(), 0);

    // Every nested level is released independently, and only the last one unblocks.
    let clone = progress.clone();
    progress.block();
    clone.block();
    progress.unblock();
    assert!(progress.is_blocking());
    progress.unblock();
    assert!(!progress.is_blocking());
}

#[test]
fn safepoint_does_not_block_on_the_callers_own_managed_slot() {
    let progress = ThreadProgress::new();
    let managed = progress.managed_thread();

    let (result_tx, result_rx) = mpsc::channel::<u64>();
    let worker = {
        let progress = progress.clone();
        thread::spawn(move || {
            // The thread drives its own slot, and its own progress lags the generation it is
            // about to publish: the safepoint must not wait for itself.
            managed.update();
            let generation = progress.safepoint();
            let _ = result_tx.send(generation);
        })
    };

    assert_eq!(
        result_rx.recv_timeout(LONG).ok(),
        Some(1),
        "safepoint() must bump to 1 and must not deadlock on its own slot"
    );
    worker.join().expect("the safepoint thread must not panic");
    assert_eq!(progress.generation(), 1);
}

#[test]
fn wait_invokes_the_registered_waker_so_a_parked_pool_is_nudged() {
    let progress = ThreadProgress::new();

    let managed = progress.managed_thread();
    let (nudge_tx, nudge_rx) = mpsc::channel::<()>();
    progress.set_waker(Arc::new(move || {
        // A test-end (or already-returned) worker makes this fail; that is expected noise,
        // so the nudge's only job is to be *sent*.
        let _ = nudge_tx.send(());
    }));

    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let worker = thread::spawn(move || {
        ready_tx
            .send(())
            .expect("the test must observe the parked thread");
        // Parked but still ACTIVE: only the waker can release this thread, so a `wait` that
        // fails to nudge would deadlock here rather than silently pass.
        match nudge_rx.recv_timeout(LONG) {
            Ok(()) => managed.update(),
            Err(err) => panic!("the waker never nudged the parked managed thread: {err}"),
        }
    });
    ready_rx
        .recv_timeout(LONG)
        .expect("the parked worker must be started");

    let generation = progress.bump();
    let (waited_tx, waited_rx) = mpsc::channel::<()>();
    let waiter = {
        let progress = progress.clone();
        thread::spawn(move || {
            progress.wait(generation);
            let _ = waited_tx.send(());
        })
    };

    match waited_rx.recv_timeout(LONG) {
        Ok(()) => {}
        Err(RecvTimeoutError::Timeout) => panic!(
            "wait deadlocked: it did not invoke the waker that releases the parked managed thread"
        ),
        Err(err) => panic!("the waiter thread vanished: {err}"),
    }

    waiter.join().expect("the waiter thread must not panic");
    worker.join().expect("the worker thread must not panic");
}

#[test]
fn a_clone_shares_the_registry_generation_and_blocking_state() {
    let progress = ThreadProgress::new();
    let clone = progress.clone();

    // The generation counter is shared.
    assert_eq!(clone.bump(), 1);
    assert_eq!(progress.generation(), 1);

    // The registry is shared: a confirmation made through the original handle satisfies a
    // wait issued through the clone from another thread (that thread is not the confirming
    // one, so it cannot skip the slot).
    let managed = progress.managed_thread();
    assert_eq!(clone.managed_count(), 1);
    managed.update();
    assert_eq!(managed.progress(), 1);

    let (waited_tx, waited_rx) = mpsc::channel::<()>();
    let waiter = {
        let clone = clone.clone();
        thread::spawn(move || {
            clone.wait(1);
            let _ = waited_tx.send(());
        })
    };
    assert!(
        waited_rx.recv_timeout(SHORT).is_ok(),
        "a confirmation through the original handle must satisfy a wait through the clone"
    );

    // The blocking counter is shared.
    progress.block();
    clone.block();
    assert!(clone.is_blocking());
    assert_eq!(progress.blocking_count(), 2);
    progress.unblock();
    clone.unblock();
    assert!(!progress.is_blocking());

    // The waker is shared. `wait` always nudges once before it checks the predicate, so this
    // observes the shared waker even on the (here: immediately satisfied) fast path.
    let (nudged_tx, nudged_rx) = mpsc::channel::<()>();
    clone.set_waker(Arc::new(move || {
        let _ = nudged_tx.send(());
    }));
    progress.wait(1);
    assert!(
        nudged_rx.recv_timeout(LONG).is_ok(),
        "the waker registered through the clone must be invoked by the original's wait"
    );

    waiter.join().expect("the waiter thread must not panic");
}
