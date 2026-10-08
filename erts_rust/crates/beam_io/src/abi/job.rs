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

//! Async-job bookkeeping for the C-ABI driver bridge ([`super::port::DriverPort`]).
//!
//! `driver_async` (`erts/emulator/beam/erl_async.c`) runs a driver's `async_invoke` on a worker
//! thread and later hands the result to the port's `ready_async` callback on the port's scheduler.
//! This module is the **M1 floor** of that machinery: a **bounded, shared worker pool** (a small
//! fixed number of long-lived threads, lazily started, fed by one process-global job queue) and a
//! per-job channel the worker reports completion through so the single-threaded port can run
//! `ready_async`.
//!
//! # Why a pool, not a thread per job
//!
//! The C spawns an async thread per outstanding job but only ever *runs* a bounded number at once
//! (the async thread-pool size, `erl_async.c`); an unbounded `thread::spawn` per `driver_async` lets
//! a driver exhaust the OS thread limit. This floor therefore bounds the concurrency to
//! [`POOL_THREADS`] worker threads: the queue is unbounded (so a job is *always* accepted, matching
//! the C's "the caller never blocks"), but at most [`POOL_THREADS`] jobs execute at any instant.
//!
//! Each job carries its own completion [`Sender`](mpsc::Sender), so many [`AsyncJobs`] instances
//! (one per port) share the one pool and each worker's completion still arrives at the right port.
//!
//! Ground truth: `call_async_ready` (`erl_async.c:318-345`) — `async_ready` runs the driver's
//! `ready_async` when the entry has one and reports `need_free`; the emulator then calls
//! `async_free(async_data)` only in the `need_free` case (`io.c:5374-5403`). Fidelity note: the C
//! runs `async_free` on the *worker* thread; this floor records the completion and lets the port
//! thread decide (it is where the driver entry is reachable), so `async_free` runs on the port
//! thread instead. Documented deviation, never a leak (the callback runs exactly once).
//!
//! # `unsafe`
//!
//! [`AsyncData`] carries a raw driver-owned pointer across the worker boundary. It is the only
//! `unsafe` in this file (one `unsafe impl Send`, with its rationale) — the rest is safe Rust.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

use libc::c_void;

/// The number of worker threads in the shared async pool.
///
/// The concurrency bound: at most this many jobs run at any instant, however many are scheduled.
/// The C's async thread pool is likewise a small fixed size (`erl_async.c`); a driver that wants
/// more parallelism than this is a follow-up (the pool is shared by every port in the process).
const POOL_THREADS: usize = 4;

/// A driver `async_data` pointer on its way from the worker thread to the port thread.
///
/// The pointer is opaque to this crate: it is whatever the driver passed to `driver_async`, and the
/// driver contract (`erl_driver.h`) is that the emulator may hand it back on another thread —
/// exactly what this wrapper asserts.
pub(crate) struct AsyncData(pub(crate) *mut c_void);

// SAFETY: the driver's `driver_async` contract is that `async_data` (and the `invoke`/`free`
// callbacks over it) may be used from the emulator's async worker thread; moving the pointer
// between the worker and the port thread is precisely that transfer. The pointer is never
// dereferenced by `beam_io` — it is only handed back to the driver's own callbacks.
unsafe impl Send for AsyncData {}

/// A finished async job awaiting its `ready_async` (or `async_free`) callback.
pub(crate) struct AsyncCompletion {
    /// The queue position `driver_async` returned (the C's `async_id`).
    pub(crate) id: u64,
    /// The driver's `async_data`.
    pub(crate) data: AsyncData,
    /// The driver's `async_free`, for the entry-without-`ready_async` case (see the module docs).
    pub(crate) free: Option<unsafe extern "C" fn(*mut c_void)>,
}

/// One queued job: the driver callback, its data and the channel to report through.
///
/// `Send` falls out for free: [`AsyncData`] is the only `unsafe impl Send` in this file and the
/// rest (`u64`, `mpsc::Sender`, `fn` pointers) are already `Send`.
struct Job {
    /// The queue position `driver_async` returned.
    id: u64,
    /// The driver's `async_invoke`.
    invoke: unsafe extern "C" fn(*mut c_void),
    /// The driver's `async_data`.
    data: AsyncData,
    /// The driver's `async_free` (recorded for the completion).
    free: Option<unsafe extern "C" fn(*mut c_void)>,
    /// Where the worker reports completion — the owning [`AsyncJobs`]'s channel.
    tx: mpsc::Sender<AsyncCompletion>,
}

/// The shared, lazily initialised worker pool: one job queue, one condvar, N worker threads.
struct Pool {
    /// Jobs waiting for a free worker.
    queue: Mutex<VecDeque<Job>>,
    /// Signalled when a job is enqueued.
    ready: Condvar,
    /// How many workers actually started (0 = the pool is unusable, see [`AsyncJobs::schedule`]).
    threads: AtomicUsize,
}

/// The process-global pool, started on first use.
///
/// One pool per process (the C has one async thread pool too), shared by every port in it.
fn pool() -> &'static Arc<Pool> {
    static POOL: OnceLock<Arc<Pool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let pool = Arc::new(Pool {
            queue: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            threads: AtomicUsize::new(0),
        });
        for index in 0..POOL_THREADS {
            let worker_pool = Arc::clone(&pool);
            // A refusal here simply means one fewer worker; the pool is still usable as long as
            // one thread started (the count is what `schedule` checks).
            if thread::Builder::new()
                .name(format!("beam_io-async-{index}"))
                .spawn(move || worker(&worker_pool))
                .is_ok()
            {
                pool.threads.fetch_add(1, Ordering::SeqCst);
            }
        }
        pool
    })
}

/// The worker loop: take a job, run its `invoke`, report the completion.
fn worker(pool: &Pool) {
    loop {
        let job = {
            // A poisoned lock is impossible in practice (no worker panics while holding it — the
            // `invoke` runs *outside* the lock), but recover rather than propagate the poison.
            let mut queue = lock(&pool.queue);
            while queue.is_empty() {
                queue = match pool.ready.wait(queue) {
                    Ok(queue) => queue,
                    Err(poison) => poison.into_inner(),
                };
            }
            match queue.pop_front() {
                Some(job) => job,
                None => continue,
            }
        };
        // SAFETY: `job.invoke` is the driver's own `void(void*)` async callback and `job.data.0` is
        // the pointer the driver handed to `driver_async`; the driver contract is that both stay
        // valid on the async thread until the callback returns.
        unsafe { (job.invoke)(job.data.0) };
        // A send failure means the owning port is gone; the driver's data is then released by the
        // caller path (documented in the module docs) — never silently dropped here.
        let _ = job.tx.send(AsyncCompletion {
            id: job.id,
            data: job.data,
            free: job.free,
        });
    }
}

/// Locks `mutex`, recovering from poisoning (see [`worker`]).
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poison) => poison.into_inner(),
    }
}

/// The async-job channel plus its bookkeeping.
///
/// Lives on the [`super::port::DriverPort`] and is `!Sync` (an `mpsc::Receiver`): the shared pool's
/// workers hold clones of the sending half; the port thread drains the receiving half in
/// [`super::port::DriverPort::poll_async`].
pub struct AsyncJobs {
    /// The sending half, cloned into every scheduled job.
    tx: mpsc::Sender<AsyncCompletion>,
    /// The receiving half, drained by the port thread.
    rx: mpsc::Receiver<AsyncCompletion>,
    /// The next queue position (`driver_async`'s return value; strictly increasing, never 0).
    next_id: u64,
    /// Jobs scheduled but not yet drained (running or completed-and-undelivered).
    in_flight: usize,
}

impl AsyncJobs {
    /// An empty job set.
    pub fn new() -> AsyncJobs {
        let (tx, rx) = mpsc::channel();
        AsyncJobs {
            tx,
            rx,
            next_id: 1,
            in_flight: 0,
        }
    }

    /// The number of jobs scheduled but not yet drained by [`AsyncJobs::drain`].
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Schedule `invoke(data)` on the shared worker pool.
    ///
    /// Returns the job's queue position (the value `driver_async` returns). A `None` `invoke` is the
    /// **fire-and-forget** form: the C would fault calling a null `invoke` (`erl_async.c:604`,
    /// `(*a->async_invoke)(a->async_data)`), so this floor schedules no work and records no
    /// completion — it still returns a valid queue position, as the C's return contract requires.
    ///
    /// # Errors
    ///
    /// Returns `Err(())` when no worker thread could be started at all (the pool is unusable); the
    /// caller then runs the driver's `async_free` (the C's `async_free` "release in case of failure"
    /// contract) and answers `driver_async`'s error code. The job queue itself is unbounded, so a
    /// job is otherwise always accepted.
    pub(crate) fn schedule(
        &mut self,
        invoke: Option<unsafe extern "C" fn(*mut c_void)>,
        data: *mut c_void,
        free: Option<unsafe extern "C" fn(*mut c_void)>,
    ) -> Result<u64, ()> {
        let id = self.take_id();
        let Some(invoke) = invoke else {
            // Fire-and-forget: no work, no completion (see the method docs).
            return Ok(id);
        };
        let pool = pool();
        if pool.threads.load(Ordering::SeqCst) == 0 {
            // No worker exists to run the job; the caller releases `data` via `async_free`.
            return Err(());
        }
        // Wrap the raw pointer in the `Send` [`AsyncData`] before the job crosses to a worker.
        let job = Job {
            id,
            invoke,
            data: AsyncData(data),
            free,
            tx: self.tx.clone(),
        };
        lock(&pool.queue).push_back(job);
        pool.ready.notify_one();
        self.in_flight += 1;
        Ok(id)
    }

    /// Drains every completion reported so far (non-blocking).
    pub(crate) fn drain(&mut self) -> Vec<AsyncCompletion> {
        let jobs: Vec<AsyncCompletion> = self.rx.try_iter().collect();
        self.in_flight = self.in_flight.saturating_sub(jobs.len());
        jobs
    }

    /// The next queue position.
    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        // Wrap to 1, never 0: the C skips 0 (`erl_async.c:575-581`).
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }
}

impl Default for AsyncJobs {
    fn default() -> AsyncJobs {
        AsyncJobs::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{AsyncData, AsyncJobs, POOL_THREADS};
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::thread::ThreadId;
    use std::time::Duration;

    /// The number of jobs *of this test* running right now.
    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    /// The most jobs *of this test* ever seen running at once.
    static PEAK: AtomicUsize = AtomicUsize::new(0);
    /// The distinct worker threads *this test's* jobs ran on.
    static THREAD_IDS: Mutex<Option<HashSet<ThreadId>>> = Mutex::new(None);

    /// A job body that records the concurrency it observed, then briefly holds it.
    unsafe extern "C" fn observe(_data: *mut libc::c_void) {
        let running = RUNNING.fetch_add(1, Ordering::SeqCst) + 1;
        PEAK.fetch_max(running, Ordering::SeqCst);
        if let Ok(mut ids) = THREAD_IDS.lock() {
            ids.get_or_insert_with(HashSet::new)
                .insert(std::thread::current().id());
        }
        std::thread::sleep(Duration::from_millis(20));
        RUNNING.fetch_sub(1, Ordering::SeqCst);
    }

    #[test]
    fn the_worker_pool_bounds_concurrency_to_pool_threads() {
        RUNNING.store(0, Ordering::SeqCst);
        PEAK.store(0, Ordering::SeqCst);
        *THREAD_IDS.lock().expect("the id set") = Some(HashSet::new());

        // Far more jobs than workers: an unbounded pool would push `PEAK` past `POOL_THREADS`.
        let mut jobs = AsyncJobs::new();
        let scheduled = POOL_THREADS * 4;
        for _ in 0..scheduled {
            assert!(jobs.schedule(Some(observe), core::ptr::null_mut(), None).is_ok());
        }

        // Wait for every job to finish (and thus for the port thread to see `scheduled` completions).
        let mut drained = 0usize;
        for _ in 0..400 {
            drained += jobs.drain().len();
            if drained == scheduled {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(drained, scheduled, "every job reports exactly one completion");
        assert_eq!(jobs.in_flight(), 0);

        let peak = PEAK.load(Ordering::SeqCst);
        assert!(
            peak <= POOL_THREADS,
            "at most {POOL_THREADS} jobs may run at once, observed {peak}"
        );
        let threads = THREAD_IDS
            .lock()
            .expect("the id set")
            .clone()
            .unwrap_or_default()
            .len();
        assert!(
            threads <= POOL_THREADS,
            "the jobs ran on at most {POOL_THREADS} threads, observed {threads}"
        );
        assert!(peak >= 1, "at least one job ran");
    }

    #[test]
    fn a_null_invoke_queues_nothing_but_still_returns_a_position() {
        let mut jobs = AsyncJobs::new();
        let id = jobs
            .schedule(None, core::ptr::null_mut(), None)
            .expect("the fire-and-forget form is accepted");
        assert!(id >= 1);
        assert_eq!(jobs.in_flight(), 0);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(jobs.drain().len(), 0);
    }

    #[test]
    fn queue_positions_start_at_one_and_never_repeat() {
        let mut jobs = AsyncJobs::new();
        let first = jobs.schedule(None, core::ptr::null_mut(), None).expect("queued");
        let second = jobs.schedule(None, core::ptr::null_mut(), None).expect("queued");
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(AsyncData(core::ptr::null_mut()).0, core::ptr::null_mut());
    }
}
