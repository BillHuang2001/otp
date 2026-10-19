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

//! The **C async-job pool** — `erl_async.c`'s `ErtsAsync` machinery, i.e. the emulator's `+A N`.
//!
//! A driver asks for work to run off its port's thread with
//!
//! ```text
//! long driver_async(ErlDrvPort ix, unsigned int *key, void (*async_invoke)(void *),
//!                   void *async_data, void (*async_free)(void *));
//! ```
//!
//! (`erts/emulator/beam/erl_driver.h:605-607`). With the pool **enabled** (`+A N`, `N > 0`) the C
//! allocates an `ErtsAsync` record, allocates a monotonic job id, enqueues the record on one of `N`
//! worker queues and returns the id (`erl_async.c:541-601`). A worker thread named
//! `erts_async_%d` pops it and runs `(*async_invoke)(async_data)` (`:416-442`), then enqueues the
//! record on the currently-scheduled scheduler's *ready* queue (`async_reply`, `:347-367`). The
//! scheduler later drains that queue (`erts_check_async_ready`, `:463-487`) and, for each record,
//! applies the **completion policy** `call_async_ready` (`:318-345`): if the port is gone, run
//! `async_free`; otherwise call the port's `ready_async` callback through `async_ready`
//! (`io.c:5374-5406`) and run `async_free` only when that reported `need_free` — i.e. when the
//! driver has **no** `ready_async` callback. The driver's own `ready_async` therefore *takes
//! ownership* of the data (it answers `need_free = 0`), and `async_free` never runs twice.
//!
//! With the pool **disabled** (`+A 0`) the C runs the job **inline on the caller's thread** and then
//! applies exactly the same completion policy, returning `0` instead of an id (`:603-616`).
//!
//! [`AsyncJob`] (the sibling [`super::async_job`]) is the safe handle for one such job. This module
//! is the pool around it — the piece a runtime that forbids `unsafe` (the sibling `beam_rt` is
//! `#![deny(unsafe_code)]`) cannot write itself, because transferring a `driver_async` job to a
//! worker thread is a contract over raw C pointers.
//!
//! # The division of labour with the host
//!
//! The pool owns the threads, the queues, the id counter and the `async_free` bookkeeping. It does
//! **not** own the port table, so it cannot call the driver's `ready_async` callback (that needs the
//! port's `ErlDrvData` and the entry itself). A host therefore provides two hooks on
//! [`DriverHost`](super::host::DriverHost) — [`async_pool`](super::host::DriverHost::async_pool)
//! (hand the pool to `driver_async`) and
//! [`deliver_ready_async`](super::host::DriverHost::deliver_ready_async) (the C's `async_ready`:
//! call the entry's `ready_async`, if any, and answer
//! [`ReadyDelivery`]) — and drives the pool from its scheduler:
//!
//! * the host's scheduler **drains** the pool ([`AsyncPool::drain_ready`]) on a timer tick or
//!   whenever [`AsyncPool::wait_ready`] returns `true`, passing `deliver_ready_async` as the hook;
//! * the host calls [`AsyncPool::port_closed`] when a port dies, so the pool's queued/ready jobs are
//!   released without a delivery (the C's "port is gone" branch, `:320-330`);
//! * the host keeps the driver's shared object **mapped** while
//!   [`AsyncPool::pending_for_port`] is non-zero — the C pins it with
//!   `erts_ddll_reference_referenced_driver` at submit (`:236-237`) and releases it at completion
//!   (`erts_ddll_dereference_driver`, `:343-344`). That reference duty is the host's here; nothing
//!   in this module can do it.
//!
//! # `unsafe`
//!
//! Exactly one `unsafe` item exists in this file — the `unsafe impl Send` for the internal job
//! record, whose `// SAFETY:` rationale explains why moving a `driver_async` job between threads is
//! sound. Everything else — the queues, the locks, the id counter, the completion policy — is safe
//! Rust over `std::sync`/`std::thread`, and every `AsyncJob` callback invocation goes through
//! `AsyncJob`'s own safe methods.

use core::fmt;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libc::{c_long, c_uint, c_void};

use crate::driver::async_job::AsyncJob;
use crate::driver::types::ErlDrvPort;

/// The C's `async_ready(...)` answer: whether the **caller** must release the job's data
/// (`io.c:5374-5406`).
///
/// This is the inverse of "the data was consumed", and it is the value
/// [`AsyncPool::drain_ready`] and [`CompletedAsync::complete`] gate `async_free` on:
///
/// * [`NeedFree`](ReadyDelivery::NeedFree) — the C's `need_free == 1` (`io.c:5376`): no
///   `ready_async` callback ran (the driver has none, `:5382`), the port no longer exists
///   (`erl_async.c:320-330`), or the host delivered nothing. The job's `async_free` **must** run.
/// * [`Consumed`](ReadyDelivery::Consumed) — the C's `need_free == 0` (`io.c:5400`): the entry's
///   `ready_async` callback actually ran and took ownership of `async_data`. `async_free` must
///   **not** run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyDelivery {
    /// `need_free == 1`: the caller releases the data with the job's `async_free`.
    NeedFree,
    /// `need_free == 0`: the driver's own `ready_async` callback took the data.
    Consumed,
}

/// What `driver_async` did with a job — the C's two return paths (`erl_async.c:594-616`).
#[derive(Debug)]
pub enum AsyncSubmit {
    /// The job was enqueued on a worker: the C's `return id` (`:600`). The id is always `> 0`.
    Queued(c_long),
    /// No pool (`+A 0`): the job **already ran on the calling thread** (the C's
    /// `(*a->async_invoke)(a->async_data)`, `:604`). The caller applies the completion policy now
    /// through [`CompletedAsync::complete`] and reports `0` to the driver.
    Inline(CompletedAsync),
}

/// A finished job awaiting its completion policy — the C's ready-queue entry (`ErtsAsync` on the
/// scheduler's `ErtsAsyncReadyQ`, `erl_async.c:347-367`).
///
/// The pool produces these from [`AsyncPool::drain`]/[`AsyncPool::drain_ready`] and on the inline
/// (`+A 0`) path. Whichever way it is disposed of, the job's `async_free` runs **exactly once**:
/// [`finish`](CompletedAsync::finish) runs it, [`hand_off`](CompletedAsync::hand_off) says the
/// driver's `ready_async` took the data (so nothing is released), [`complete`](CompletedAsync::complete)
/// decides between the two from a `need_free` hook, and **dropping** the value without either is the
/// C's "give up on it" path — the inner [`AsyncJob`]'s own `Drop` releases it. A forgotten
/// `CompletedAsync` therefore leaks nothing and can never double-free.
#[derive(Debug)]
pub struct CompletedAsync {
    /// The job and the identity it was submitted with. Its `job` field is `Some` until the value is
    /// disposed of; after that the `AsyncJob` has been consumed (and its `Drop` cannot free again).
    record: PooledJob,
}

impl CompletedAsync {
    /// The job id the C returned to the driver (`a->async_id`, `erl_async.c:583`) — `0` on the
    /// inline path (`:572-573`).
    pub fn id(&self) -> c_long {
        self.record.id
    }

    /// The port the job was submitted for (`a->port`, `erl_async.c:566`). The `ErlDrvPort` is
    /// opaque here: the pool stores it as its address and hands the same operand back.
    pub fn port(&self) -> ErlDrvPort {
        self.record.port as ErlDrvPort
    }

    /// The `async_data` pointer — what the C passes to `ready_async` as `ErlDrvThreadData`
    /// (`a->async_data`, `erl_async.c:568`). Never dereferenced by this crate.
    pub fn data(&self) -> *mut c_void {
        self.record.data
    }

    /// Whether the port this job belongs to has been marked gone ([`AsyncPool::port_closed`]) — the
    /// C's `!p` branch (`erl_async.c:320-330`).
    pub fn port_is_gone(&self) -> bool {
        self.record.gone
    }

    /// Release the job: run its `async_free`, exactly once, and consume the value. This is the C's
    /// `if (async_free) async_free(async_data)` after a `need_free` answer (`:333-337`).
    pub fn finish(mut self) {
        if let Some(job) = self.record.job.take() {
            job.finish();
        }
    }

    /// The driver's own `ready_async` callback took ownership of the data: release **nothing** (the
    /// C's `need_free = 0`, `io.c:5400`).
    pub fn hand_off(mut self) {
        if let Some(job) = self.record.job.take() {
            job.hand_off();
        }
    }

    /// Apply the C's completion policy — `call_async_ready` (`erl_async.c:318-345`) over
    /// `async_ready` (`io.c:5374-5406`) — and return its answer.
    ///
    /// `need_free` is the delivery hook (in practice
    /// [`DriverHost::deliver_ready_async`](super::host::DriverHost::deliver_ready_async)): it calls
    /// the port's `ready_async` callback, if the driver has one, and says whether the caller must
    /// still release the data. It is **not called at all** when the port is gone — the C looks the
    /// port up first and frees without delivering (`:320-330`) — in which case this returns
    /// [`ReadyDelivery::NeedFree`] and has already freed.
    pub fn complete<F>(self, mut need_free: F) -> ReadyDelivery
    where
        F: FnMut(ErlDrvPort, *mut c_void) -> ReadyDelivery,
    {
        if self.record.gone {
            // The C's `if (!p) { if (a->async_free) a->async_free(a->async_data); }` (`:324-330`):
            // no `ready_async` delivery for a port that is no longer there.
            self.finish();
            return ReadyDelivery::NeedFree;
        }
        let answer = need_free(self.port(), self.data());
        match answer {
            ReadyDelivery::NeedFree => self.finish(),
            ReadyDelivery::Consumed => self.hand_off(),
        }
        answer
    }
}

/// One job in the pool's custody — the C's `ErtsAsync` record (`erl_async.c:94-118`,
/// `:562-583`), in either phase (queued for a worker, or ready for the scheduler).
#[derive(Debug)]
struct PooledJob {
    /// `a->async_id` (`:583`): the value `driver_async` returned, `0` for the inline path.
    id: c_long,
    /// `a->port` (`:566`) — the port term in the C, the `ErlDrvPort` operand here, held as its
    /// address because the shared state must stay `Send`.
    port: usize,
    /// `a->async_data` (`:568`), the `ErlDrvThreadData` `ready_async` receives.
    data: *mut c_void,
    /// Set when the port this job belongs to has been closed ([`AsyncPool::port_closed`]).
    gone: bool,
    /// The job's callbacks; `None` once the value has been disposed of.
    job: Option<AsyncJob>,
}

// SAFETY: this is the module's one `unsafe` item, and the only non-`Send` part of the record is the
// [`AsyncJob`] it owns (which holds the driver's `async_invoke`/`async_free` raw function pointers
// and its `async_data` raw pointer — raw pointers are `!Send`). Moving the record between threads is
// nevertheless sound:
//
// * **Exclusive ownership at every step.** The record is created on the submitting (port) thread,
//   moved into the shared state under a `Mutex`, taken out by exactly **one** worker, run once, and
//   put back under the same `Mutex` — from where exactly one thread takes it out again (the host's
//   drain). No two threads ever own one job, and every hand-over is a mutex release/acquire pair.
// * **The C contract already requires thread-agnostic callbacks.** `driver_async`'s job is defined
//   to have `async_invoke` run on an `erts_async_%d` worker thread (`erl_async.c:416-442`) and
//   `async_free` on whichever scheduler drains the ready queue (`:463-487`), so a driver that passes
//   these callbacks has already promised they are safe to call from another thread — that is the
//   ABI's own precondition, not an assumption added here.
// * **No Rust code dereferences the pointers.** This module never reads `data`; `AsyncJob` only
//   calls the two C function pointers the driver supplied, through its own `unsafe` blocks whose
//   contract is the caller's promise in `AsyncJob::new`.
unsafe impl Send for PooledJob {}

/// The pool's shared, mutex-protected state.
struct State {
    /// One queue per worker — the C's `ErtsAsyncQ async->queue[i]` (`erl_async.c:116`, `:128-133`).
    /// Index `qix` is the queue `driver_async` selected (`:585-593`), and only worker `qix` drains
    /// it.
    queues: Vec<VecDeque<PooledJob>>,
    /// Finished jobs waiting for a scheduler to apply the completion policy — the C's
    /// `ErtsAsyncReadyQ` (`:136-140`).
    ready: VecDeque<CompletedAsync>,
    /// The id counter (`async->init.data.id`, `:170`; incremented at `:576`).
    next_id: u64,
    /// Set when the pool is being torn down; workers exit at their next wake-up.
    shutdown: bool,
}

/// The mutex + condvar pair every phase transition goes through. `wake` is notified on submit (a
/// worker has work) **and** on completion (a drained job became ready), so both waiters share it.
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    /// How many worker threads are actually running. Read by [`AsyncPool::submit`] for the
    /// `id % threads` / `key % threads` queue selection (`erl_async.c:585-593`), and by
    /// [`AsyncPool::threads`].
    ///
    /// An atomic rather than a plain field because a worker thread's index is only known once it
    /// has been *spawned*: if the OS refuses a thread, the pool keeps the ones it has (the indices
    /// stay compact, `0..workers`), so `0..workers` is exactly the set of drained queues.
    workers: AtomicUsize,
}

impl Shared {
    /// Lock the state, recovering from a poisoned mutex (a panic while the lock was held must not
    /// turn every later pool operation into a panic — the state itself is consistent at every
    /// release point, so the guard is simply taken back).
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Wait for a notification, recovering a poisoned lock the same way as [`Shared::lock`].
    fn wait<'a>(&self, guard: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.wake
            .wait(guard)
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

/// The pool's owned half: the shared state plus the worker handles. Dropping the **last**
/// [`AsyncPool`] handle drops this, which *is* the pool shutdown (see `Drop`).
struct Inner {
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
}

impl Drop for Inner {
    /// Shut the pool down — the Rust analogue of `erts_exit_flush_async` (`erl_async.c:445-461`),
    /// which terminates each worker with a sentinel job and joins it.
    ///
    /// Three things happen, in this order, and the order matters:
    ///
    /// 1. `shutdown` is set under the lock and every worker is woken; each finishes the job it is
    ///    running (if any) and exits at its next loop turn.
    /// 2. Every handle is **joined**, so no worker can still be touching the state (or pushing onto
    ///    the ready queue) afterwards. A worker parked forever inside a driver's `async_invoke`
    ///    blocks this join, exactly as the C's `erts_thr_join` does (`:459-460`).
    /// 3. Every job still in a queue or in the ready deque is released through `async_free`
    ///    **exactly once** — dropping a `PooledJob` drops its `AsyncJob`, whose `Drop` is the single
    ///    release point. The C does not bother (`:451-456`: "we do not bother to clean everything
    ///    up since we are about to terminate the runtime system"), but a Rust pool can be dropped at
    ///    any time, so leaking a driver's `async_data` would be a real leak.
    ///
    /// A `JoinHandle`'s result is deliberately discarded: it can only be an error if a worker
    /// panicked (i.e. the driver's `async_invoke` unwound into Rust), and a `Drop` must not panic
    /// (that would abort while already unwinding). The C cannot report this either.
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock();
            state.shutdown = true;
        }
        self.shared.wake.notify_all();
        for handle in std::mem::take(&mut self.handles) {
            handle.join().ok();
        }
        let mut state = self.shared.lock();
        for queue in &mut state.queues {
            queue.clear();
        }
        state.ready.clear();
    }
}

/// The C async-job pool (`erl_async.c`'s `ErtsAsyncData`), i.e. the emulator's `+A N`.
///
/// `AsyncPool::new(N)` starts `N` worker threads named `erts_async_1` … `erts_async_N` (the C names
/// them `"erts_async_%d", i+1`, `erl_async.c:204`); `N == 0` is the emulator's `+A 0`, where a job
/// runs inline on the submitting thread. Job ids come from a monotonic counter that starts at `1`,
/// never yields `0` and wraps back into the positive range (`:572-583`) — there is **no** free list
/// and **no** id reuse in this OTP tree, because a driver can observe the id.
///
/// Cloning an `AsyncPool` is a cheap `Arc` clone; the workers live until the last clone is dropped.
#[derive(Clone)]
pub struct AsyncPool {
    inner: Arc<Inner>,
}

impl AsyncPool {
    /// `erts_init_async` (`erl_async.c:143-217`) — build a pool of `threads` workers.
    ///
    /// `threads == 0` is the `+A 0` configuration: no threads are created and every job runs inline
    /// (see [`AsyncPool::submit`]). If the OS refuses to create a thread, the pool keeps the workers
    /// it already has (a `+A 0` pool in the limit case) rather than panicking — the constructor has
    /// no failure channel, and every queue index stays inside `0..threads()`.
    pub fn new(threads: usize) -> AsyncPool {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                queues: (0..threads).map(|_| VecDeque::new()).collect(),
                ready: VecDeque::new(),
                next_id: 0,
                shutdown: false,
            }),
            wake: Condvar::new(),
            workers: AtomicUsize::new(0),
        });

        let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(threads);
        for _ in 0..threads {
            // The index is the number of workers already running, so a refused spawn leaves the
            // indices compact and `0..workers` is exactly the set of drained queues.
            let index = handles.len();
            let worker_shared = Arc::clone(&shared);
            let spawned = std::thread::Builder::new()
                .name(format!("erts_async_{}", index + 1))
                .spawn(move || worker(worker_shared, index));
            match spawned {
                Ok(handle) => {
                    handles.push(handle);
                    shared.workers.store(handles.len(), Ordering::Relaxed);
                }
                Err(_) => break,
            }
        }

        AsyncPool {
            inner: Arc::new(Inner { shared, handles }),
        }
    }

    /// The number of worker threads actually running — `erts_async_max_threads` (`erl_async.c:120`,
    /// `:586`). `0` means `+A 0`: jobs run inline.
    pub fn threads(&self) -> usize {
        self.inner.shared.workers.load(Ordering::Relaxed)
    }

    /// Hand a job to the pool — the body of `driver_async` (`erl_async.c:541-617`).
    ///
    /// `key` is the driver's queue hint: `Some(key)` selects queue `*key % threads` and **writes
    /// that index back** into `*key` (`:590-592`); `None` selects `id % threads` (`:585-587`). With
    /// no workers both select queue `0` and the write-back still happens (`:587`, `:591`), so the
    /// key is `0` after an inline submit.
    ///
    /// Returns [`AsyncSubmit::Queued(id)`](AsyncSubmit::Queued) with the positive job id when the
    /// job was enqueued (`:600`), or [`AsyncSubmit::Inline`] when there is no pool — in which case
    /// the job has **already run on this thread** (`:604`) and the caller must apply the completion
    /// policy ([`CompletedAsync::complete`]) and report `0`.
    ///
    /// The work queue is unbounded: the only `-1` the C has is an invalid port operand (`:556-558`),
    /// which is the caller's check, not the pool's. (`erts_async_max_queue_length`,
    /// `erts_async_disable` and `driver_async_cancel` do not exist in this tree.)
    pub fn submit(
        &self,
        port: ErlDrvPort,
        key: Option<&mut c_uint>,
        job: AsyncJob,
    ) -> AsyncSubmit {
        let shared = &self.inner.shared;
        let threads = self.threads();
        let mut state = shared.lock();

        // The id (`:572-583`): `0` with no pool, else the next counter value.
        let id = if threads == 0 {
            0
        } else {
            allocate_id(&mut state.next_id)
        };

        // Queue selection and the key write-back (`:585-593`).
        //
        // The write-back is *not* conditional on having workers: `*key` is assigned inside the
        // `key != NULL` arm of the C's `if`, so a `+A 0` submit sets it to `qix = 0` (`:590-592`).
        let qix = if let Some(key) = key {
            let qix = if threads > 0 {
                (*key as usize) % threads
            } else {
                0 // `(erts_async_max_threads > 0) ? … : 0` — `+A 0` always selects queue 0.
            };
            *key = qix as c_uint;
            qix
        } else if threads > 0 {
            (id as u64 % threads as u64) as usize
        } else {
            0
        };

        if threads == 0 {
            // `+A 0`: run the job inline on the caller's thread (`:603-616`). The lock is released
            // first — the driver's `async_invoke` must not run under the pool's mutex.
            drop(state);
            let data = job.data();
            job.invoke();
            return AsyncSubmit::Inline(CompletedAsync {
                record: PooledJob {
                    id,
                    port: port as usize,
                    data,
                    gone: false,
                    job: Some(job),
                },
            });
        }

        let data = job.data();
        state.queues[qix].push_back(PooledJob {
            id,
            port: port as usize,
            data,
            gone: false,
            job: Some(job),
        });
        drop(state);
        // **All** workers, never one: the queues are per-worker, so the job may belong to a worker
        // other than an arbitrary one — `notify_one` could wake a worker whose own queue is empty
        // and lose the wake-up.
        shared.wake.notify_all();
        AsyncSubmit::Queued(id)
    }

    /// How many finished jobs are waiting for their completion policy.
    pub fn ready_len(&self) -> usize {
        self.inner.shared.lock().ready.len()
    }

    /// Block until at least one job is ready, or `timeout` elapses. Returns whether anything is
    /// ready (`false` means the pool stayed idle for the whole timeout).
    ///
    /// A host's scheduler calls this to be woken by completions instead of polling — the Rust
    /// analogue of the C's `erts_notify_check_async_ready_queue` wake-up (`erl_async.c:174`).
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.inner.shared.lock();
        loop {
            if !state.ready.is_empty() {
                return true;
            }
            let left = match deadline.and_then(|end| end.checked_duration_since(Instant::now())) {
                Some(left) => left,
                None => return false,
            };
            let (guard, _timed_out) = self
                .inner
                .shared
                .wake
                .wait_timeout(state, left)
                .unwrap_or_else(|poison| poison.into_inner());
            state = guard;
        }
    }

    /// Take every finished job out of the pool, leaving the completion policy to the caller.
    ///
    /// Each returned [`CompletedAsync`] releases its data exactly once: through
    /// [`finish`](CompletedAsync::finish), [`hand_off`](CompletedAsync::hand_off),
    /// [`complete`](CompletedAsync::complete), or its own `Drop` if the caller forgets it.
    pub fn drain(&self) -> Vec<CompletedAsync> {
        self.inner.shared.lock().ready.drain(..).collect()
    }

    /// Drain the ready queue and apply the completion policy to every job — the C's
    /// `erts_check_async_ready` loop (`erl_async.c:463-487`) over `call_async_ready` (`:318-345`).
    ///
    /// `need_free` is the delivery hook (in practice
    /// [`DriverHost::deliver_ready_async`](super::host::DriverHost::deliver_ready_async)); it runs
    /// **outside** the pool's lock, so it may call back into the pool. Returns how many jobs were
    /// completed. The C's per-call bound (`ERTS_MAX_ASYNC_READY_CALLS_IN_SEQ`, 20, `:469`) exists to
    /// bound one scheduler's work-tick; a host reproduces it by calling this again on the next tick,
    /// so the pool itself drains everything.
    pub fn drain_ready<F>(&self, mut need_free: F) -> usize
    where
        F: FnMut(ErlDrvPort, *mut c_void) -> ReadyDelivery,
    {
        let drained = self.drain();
        let completed = drained.len();
        for item in drained {
            // `complete` takes its hook by value; the caller's `FnMut` is reused across the loop.
            item.complete(&mut need_free);
        }
        completed
    }

    /// Mark every job of `port` that the pool can still see as **gone**, so its completion releases
    /// the data without a `ready_async` delivery — the C's `!p` branch (`erl_async.c:320-330`).
    /// Returns how many jobs were marked.
    ///
    /// The job itself still runs: the C declares no cancellation at all (`:434` runs
    /// `async_invoke` whatever the port's state), so `async_invoke` is never skipped and
    /// `async_free` is never skipped either.
    ///
    /// A job a worker is **in the middle of running** is in neither queue, so it cannot be marked
    /// here. That case is covered by the delivery hook's own contract: a host that is asked to
    /// deliver for a port that no longer exists answers
    /// [`ReadyDelivery::NeedFree`](ReadyDelivery::NeedFree) without a callback — exactly the C's
    /// port lookup in `call_async_ready` (`:320-323`). `port_closed` is the cheap path for the jobs
    /// the pool already holds; the hook is what makes it race-free.
    pub fn port_closed(&self, port: ErlDrvPort) -> usize {
        let target = port as usize;
        let mut state = self.inner.shared.lock();
        let mut marked = 0;
        for queue in &mut state.queues {
            for job in queue.iter_mut() {
                if job.port == target && !job.gone {
                    job.gone = true;
                    marked += 1;
                }
            }
        }
        for item in &mut state.ready {
            if item.record.port == target && !item.record.gone {
                item.record.gone = true;
                marked += 1;
            }
        }
        marked
    }

    /// How many jobs of `port` the pool holds — queued for a worker **or** ready for a scheduler.
    ///
    /// A host uses this to decide whether the driver's shared object may be unloaded: the C pins it
    /// with `erts_ddll_reference_referenced_driver` at submit (`erl_async.c:236-237`) and releases
    /// that reference once the job is completed (`:343-344`), so `pending_for_port > 0` means a C
    /// callback of that driver can still be entered.
    pub fn pending_for_port(&self, port: ErlDrvPort) -> usize {
        let target = port as usize;
        let state = self.inner.shared.lock();
        let queued = state
            .queues
            .iter()
            .flatten()
            .filter(|job| job.port == target)
            .count();
        let ready = state
            .ready
            .iter()
            .filter(|item| item.record.port == target)
            .count();
        queued + ready
    }
}

impl fmt::Debug for AsyncPool {
    /// Reports the pool's shape and backlog — never a job's `async_data` payload.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AsyncPool")
            .field("threads", &self.threads())
            .field("ready_len", &self.ready_len())
            .finish_non_exhaustive()
    }
}

/// One worker's main loop — `async_main` (`erl_async.c:416-442`).
///
/// Pop this worker's own queue under the lock, else wait for a notification (or exit on shutdown);
/// then run `(*async_invoke)(async_data)` **outside** the lock (`:434`), push the job onto the ready
/// queue (`async_reply`, `:347-367`) and wake the scheduler side.
///
/// A worker never calls the driver's `ready_async` callback — only the host's scheduler does, when it
/// drains the ready queue. That is the C's structure too (`:463-487` runs on a scheduler thread), and
/// it is what keeps the port's `ErlDrvData`/driver-entry ownership on the scheduler side.
fn worker(shared: Arc<Shared>, index: usize) {
    loop {
        let job = {
            let mut state = shared.lock();
            loop {
                if let Some(job) = state.queues[index].pop_front() {
                    break job;
                }
                if state.shutdown {
                    return;
                }
                state = shared.wait(state);
            }
        };

        // `(*a->async_invoke)(a->async_data)` (`:434`), outside the lock.
        if let Some(job) = &job.job {
            job.invoke();
        }

        let mut state = shared.lock();
        state.ready.push_back(CompletedAsync { record: job });
        drop(state);
        shared.wake.notify_all();
    }
}

/// The next job id — the C's `do { id = erts_atomic_inc_read_nob(&counter); } while (id == 0);`
/// followed by `if (id < 0) id *= -1;` (`erl_async.c:575-580`).
fn allocate_id(next_id: &mut u64) -> c_long {
    loop {
        *next_id = next_id.wrapping_add(1);
        if *next_id == 0 {
            continue; // `while (id == 0)`: 0 is never a job id.
        }
        return normalize_id(*next_id);
    }
}

/// Bring a raw counter word into the C's "always positive" domain (`erl_async.c:578-580`).
///
/// The C counter is an `erts_atomic_t` (a signed word) and the code tests `id < 0` after the
/// increment, multiplying by `-1` to get back into the positive range. Two edge values need a
/// documented answer:
///
/// * `0` stays `0` — the caller ([`allocate_id`]) skips it, so it never becomes a job id;
/// * the signed minimum (`i64::MIN`, i.e. the raw word `1 << 63`) has no positive counterpart, so it
///   maps to `1`. The C would take `id *= -1` (signed overflow, undefined) and then trip its
///   `ASSERT(id > 0)`; answering `1` keeps the "positive, never 0" contract instead of aborting.
fn normalize_id(raw: u64) -> c_long {
    let id = raw as c_long;
    if id < 0 {
        id.checked_neg().unwrap_or(1)
    } else {
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::sync::Mutex;
    use std::thread::ThreadId;

    /// A non-null `async_data` address no callback ever dereferences (each test uses its own
    /// `n`-derived value so the pointers stay distinguishable).
    fn payload(n: usize) -> *mut c_void {
        (0x5eed_0000usize + n) as *mut c_void
    }

    /// An opaque port operand (the pool only stores and hands back its address).
    fn handle(n: usize) -> ErlDrvPort {
        (0x1000_0000usize + n) as ErlDrvPort
    }

    /// A `NULL`-free callback: the C's `async_free`, which only counts.
    unsafe extern "C" fn noop(_: *mut c_void) {}

    /// Spin (bounded) until `pred` holds, so a slow or loaded machine cannot flake a test into a
    /// false failure — only a genuine lost wake-up runs out of time.
    fn wait_until(pred: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        pred()
    }

    /// The ids a driver sees are positive and strictly increasing, and a job that never reached a
    /// scheduler is still released exactly once when the pool goes away.
    #[test]
    fn async_ids_are_positive_and_strictly_increasing() {
        static INVOKES: AtomicUsize = AtomicUsize::new(0);
        static FREES: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn job_invoke(_: *mut c_void) {
            INVOKES.fetch_add(1, SeqCst);
        }

        unsafe extern "C" fn job_free(_: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
        }

        let base_free = FREES.load(SeqCst);
        let pool = AsyncPool::new(2);
        let mut ids = Vec::new();
        for n in 0..8 {
            // SAFETY: two live C callbacks of the frozen signature and a never-dereferenced payload.
            let job = unsafe { AsyncJob::new(Some(job_invoke), Some(job_free), payload(n)) }
                .expect("a job with an `async_invoke` is a job");
            match pool.submit(handle(1), None, job) {
                AsyncSubmit::Queued(id) => ids.push(id),
                AsyncSubmit::Inline(_) => panic!("a 2-thread pool never runs a job inline"),
            }
        }

        assert!(ids.iter().all(|id| *id > 0), "ids are positive: {ids:?}");
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "ids strictly increase: {ids:?}"
        );
        assert_eq!(ids[0], 1, "the counter starts at 1 (`erl_async.c:576`)");

        // Dropping the last handle joins the workers and releases everything still queued/ready.
        drop(pool);
        assert_eq!(
            FREES.load(SeqCst) - base_free,
            8,
            "every job is released exactly once, wherever it was in the pool"
        );
        assert_eq!(
            INVOKES.load(SeqCst),
            8,
            "no job is skipped by the shutdown (and none runs twice)"
        );
    }

    /// The counter's two edge behaviours: `0` is never handed out and a wrapped (negative) word is
    /// brought back into the positive range (`erl_async.c:575-580`).
    #[test]
    fn the_id_counter_wraps_back_into_the_positive_range() {
        assert_eq!(normalize_id(1), 1);
        assert_eq!(normalize_id(u64::MAX), 1, "-1 ⇒ `id *= -1` (:578-579)");
        assert_eq!(normalize_id(u64::MAX - 1), 2, "-2 ⇒ 2");
        assert_eq!(normalize_id(1 << 63), 1, "the signed minimum has no positive counterpart");
        assert_eq!(normalize_id(0), 0, "`0` is only ever the inline/invalid id");

        // `u64::MAX + 1` wraps to 0, which the `while (id == 0)` loop skips: the next id is 1, not 0.
        let mut counter = u64::MAX;
        assert_eq!(allocate_id(&mut counter), 1);
        assert_eq!(counter, 1, "the skipped 0 was consumed from the counter");
        assert_eq!(allocate_id(&mut counter), 2);
    }

    /// `async_invoke` really runs on a pool worker — not on the submitting thread — and the worker
    /// carries the C's name (`"erts_async_%d"`, `erl_async.c:204`).
    #[test]
    fn async_invoke_runs_on_a_named_pool_worker() {
        static SEEN: Mutex<Option<(ThreadId, String)>> = Mutex::new(None);

        unsafe extern "C" fn record_thread(_: *mut c_void) {
            let current = std::thread::current();
            let name = current.name().unwrap_or("").to_string();
            *SEEN.lock().unwrap_or_else(|poison| poison.into_inner()) = Some((current.id(), name));
        }

        let pool = AsyncPool::new(1);
        let submitter = std::thread::current().id();
        // SAFETY: `record_thread` is a live C callback for a payload it never reads.
        let job = unsafe { AsyncJob::new(Some(record_thread), None, payload(0)) }
            .expect("a job with an `async_invoke` is a job");
        let id = match pool.submit(handle(2), None, job) {
            AsyncSubmit::Queued(id) => id,
            AsyncSubmit::Inline(_) => panic!("a 1-thread pool queues"),
        };
        assert!(id > 0);

        assert!(
            wait_until(|| pool.ready_len() == 1),
            "the worker completes the job and makes it ready"
        );
        let (worker, name) = SEEN
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .expect("`async_invoke` ran");
        assert_ne!(
            worker, submitter,
            "`async_invoke` runs on a worker, not on the submitting thread"
        );
        assert_eq!(name, "erts_async_1", "the worker is named like the C's (`:204`)");

        let drained = pool.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].id(), id, "the completion carries the id the driver got");
        assert_eq!(drained[0].port(), handle(2));
        assert_eq!(drained[0].data(), payload(0));
        assert!(!drained[0].port_is_gone(), "the port is alive");
        assert_eq!(pool.ready_len(), 0, "`drain` empties the ready queue");
    }

    /// A `NeedFree` answer frees each job exactly once (and `drain_ready` empties the queue, so a
    /// second drain frees nothing again).
    #[test]
    fn a_need_free_answer_runs_async_free_once_per_job() {
        static FREES: AtomicUsize = AtomicUsize::new(0);
        static DELIVERED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

        unsafe extern "C" fn job_free(_: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
        }

        let base_free = FREES.load(SeqCst);
        let pool = AsyncPool::new(2);
        for n in 0..4 {
            // SAFETY: `job_free` is a live C callback for a never-dereferenced payload.
            let job = unsafe { AsyncJob::new(Some(noop), Some(job_free), payload(n)) }
                .expect("a job with an `async_invoke` is a job");
            assert!(matches!(pool.submit(handle(3), None, job), AsyncSubmit::Queued(_)));
        }
        assert!(
            wait_until(|| pool.ready_len() == 4),
            "all four jobs finish"
        );

        let completed = pool.drain_ready(|_port, data| {
            DELIVERED
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(data as usize);
            ReadyDelivery::NeedFree
        });
        assert_eq!(completed, 4, "`drain_ready` reports every job it completed");
        let mut delivered = DELIVERED
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        delivered.sort_unstable();
        let mut expected: Vec<usize> = (0..4).map(|n| payload(n) as usize).collect();
        expected.sort_unstable();
        assert_eq!(delivered, expected, "each job's own `async_data` was delivered");
        assert_eq!(
            FREES.load(SeqCst) - base_free,
            4,
            "`need_free` runs each job's `async_free` exactly once"
        );

        assert_eq!(
            pool.drain_ready(|_, _| ReadyDelivery::NeedFree),
            0,
            "a second drain finds nothing"
        );
        assert_eq!(
            FREES.load(SeqCst) - base_free,
            4,
            "and therefore frees nothing a second time"
        );
    }

    /// A `Consumed` answer means the driver's `ready_async` took the data: nothing is freed.
    #[test]
    fn a_consumed_answer_never_runs_async_free() {
        static FREES: AtomicUsize = AtomicUsize::new(0);
        static HOOKS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn job_free(_: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
        }

        let base_free = FREES.load(SeqCst);
        let pool = AsyncPool::new(1);
        for n in 0..3 {
            // SAFETY: `job_free` is a live C callback for a never-dereferenced payload.
            let job = unsafe { AsyncJob::new(Some(noop), Some(job_free), payload(n)) }
                .expect("a job with an `async_invoke` is a job");
            assert!(matches!(pool.submit(handle(4), None, job), AsyncSubmit::Queued(_)));
        }
        assert!(wait_until(|| pool.ready_len() == 3), "all three jobs finish");

        let completed = pool.drain_ready(|_, _| {
            HOOKS.fetch_add(1, SeqCst);
            ReadyDelivery::Consumed
        });
        assert_eq!(completed, 3);
        assert_eq!(HOOKS.load(SeqCst), 3, "the delivery hook ran once per job");
        assert_eq!(
            FREES.load(SeqCst) - base_free,
            0,
            "`Consumed` means the driver's `ready_async` took the data (`io.c:5400`)"
        );
    }

    /// A completion the caller simply drops releases the data exactly once — the C's "give up on
    /// it" path — and it cannot be released twice afterwards.
    #[test]
    fn a_completion_dropped_without_finish_frees_exactly_once() {
        static FREES: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn job_free(_: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
        }

        let base_free = FREES.load(SeqCst);
        let pool = AsyncPool::new(1);
        // SAFETY: `job_free` is a live C callback for a never-dereferenced payload.
        let job = unsafe { AsyncJob::new(Some(noop), Some(job_free), payload(7)) }
            .expect("a job with an `async_invoke` is a job");
        assert!(matches!(pool.submit(handle(5), None, job), AsyncSubmit::Queued(_)));
        assert!(wait_until(|| pool.ready_len() == 1), "the job finishes");

        let item = pool.drain().pop().expect("one ready job");
        assert_eq!(item.data(), payload(7));
        drop(item);
        assert_eq!(FREES.load(SeqCst) - base_free, 1, "dropping a completion frees once");

        // `finish` and `hand_off` are the two explicit answers; both consume the value.
        for n in 1..3 {
            // SAFETY: `job_free` is a live C callback for a never-dereferenced payload.
            let job = unsafe { AsyncJob::new(Some(noop), Some(job_free), payload(n)) }
                .expect("a job with an `async_invoke` is a job");
            assert!(matches!(pool.submit(handle(5), None, job), AsyncSubmit::Queued(_)));
        }
        assert!(wait_until(|| pool.ready_len() == 2), "both jobs finish");
        let mut drained = pool.drain();
        drained.pop().expect("a ready job").hand_off();
        drained.pop().expect("a ready job").finish();
        assert_eq!(
            FREES.load(SeqCst) - base_free,
            2,
            "`finish` frees one; `hand_off` frees none"
        );
    }

    /// Closing a port releases its jobs **without** a `ready_async` delivery, and the jobs still ran
    /// (`async_invoke` is never cancelled, `erl_async.c:434`).
    #[test]
    fn closing_a_port_frees_its_jobs_without_delivering_ready_async() {
        static FREES: AtomicUsize = AtomicUsize::new(0);
        static INVOKES: AtomicUsize = AtomicUsize::new(0);
        static HOOKS: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn job_invoke(_: *mut c_void) {
            INVOKES.fetch_add(1, SeqCst);
        }

        unsafe extern "C" fn job_free(_: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
        }

        let base_free = FREES.load(SeqCst);
        let base_invokes = INVOKES.load(SeqCst);
        let pool = AsyncPool::new(2);
        let doomed = handle(6);
        let other = handle(7);
        for n in 0..3 {
            // SAFETY: two live C callbacks of the frozen signature and a never-dereferenced payload.
            let job = unsafe { AsyncJob::new(Some(job_invoke), Some(job_free), payload(n)) }
                .expect("a job with an `async_invoke` is a job");
            assert!(matches!(pool.submit(doomed, None, job), AsyncSubmit::Queued(_)));
        }
        // SAFETY: as above.
        let job = unsafe { AsyncJob::new(Some(job_invoke), Some(job_free), payload(9)) }
            .expect("a job with an `async_invoke` is a job");
        assert!(matches!(pool.submit(other, None, job), AsyncSubmit::Queued(_)));

        // Wait until all four are visible in the ready queue, so the marking below is deterministic
        // (a job a worker is still *running* is in neither queue — see `port_closed`'s docs).
        assert!(wait_until(|| pool.ready_len() == 4), "all four jobs finish");
        assert_eq!(pool.pending_for_port(doomed), 3, "the doomed port's three jobs");
        assert_eq!(pool.pending_for_port(other), 1);

        assert_eq!(pool.port_closed(doomed), 3, "all three are marked gone");
        assert_eq!(pool.pending_for_port(doomed), 3, "they are still owed a completion");

        let completed = pool.drain_ready(|_, _| {
            HOOKS.fetch_add(1, SeqCst);
            ReadyDelivery::NeedFree
        });
        assert_eq!(completed, 4);
        assert_eq!(
            HOOKS.load(SeqCst),
            1,
            "only the live port's job reached the delivery hook"
        );
        assert_eq!(
            FREES.load(SeqCst) - base_free,
            4,
            "every job was released exactly once, the gone ones without a delivery"
        );
        assert_eq!(
            INVOKES.load(SeqCst) - base_invokes,
            4,
            "`async_invoke` ran for the closed port's jobs too"
        );
        assert_eq!(pool.pending_for_port(doomed), 0, "the queue is empty again");
    }

    /// `+A 0`: no workers, the job runs on the calling thread, the id is `0` and the key is written
    /// back as `0` (`erl_async.c:572-573`, `:587`, `:603-616`).
    #[test]
    fn a_zero_thread_pool_runs_the_job_inline() {
        static SEEN: Mutex<Option<ThreadId>> = Mutex::new(None);
        static FREES: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn record_thread(_: *mut c_void) {
            let current = std::thread::current();
            *SEEN.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(current.id());
        }

        unsafe extern "C" fn job_free(_: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
        }

        let pool = AsyncPool::new(0);
        assert_eq!(pool.threads(), 0, "`+A 0` creates no workers");
        let submitter = std::thread::current().id();
        let mut key: c_uint = 3;
        // SAFETY: two live C callbacks of the frozen signature and a never-dereferenced payload.
        let job = unsafe { AsyncJob::new(Some(record_thread), Some(job_free), payload(11)) }
            .expect("a job with an `async_invoke` is a job");
        let submitted = pool.submit(handle(8), Some(&mut key), job);

        let seen = *SEEN.lock().unwrap_or_else(|poison| poison.into_inner());
        assert_eq!(
            seen,
            Some(submitter),
            "the inline path runs `async_invoke` on the calling thread (`:604`)"
        );
        assert_eq!(
            key, 0,
            "`qix = 0` is written back even with no pool (`:590-593`)"
        );

        let item = match submitted {
            AsyncSubmit::Inline(item) => item,
            AsyncSubmit::Queued(_) => panic!("`+A 0` never queues"),
        };
        assert_eq!(item.id(), 0, "the inline path reports id 0 (`:572-573`)");
        assert_eq!(item.port(), handle(8));
        assert_eq!(item.data(), payload(11));
        // The same completion policy as the queued path (`:607-613`).
        assert_eq!(
            item.complete(|_, _| ReadyDelivery::NeedFree),
            ReadyDelivery::NeedFree
        );
        assert_eq!(FREES.load(SeqCst), 1, "the inline job's `async_free` ran once");
    }

    /// The `key` operand selects a queue and is written back as `*key % threads` (`:590-592`).
    #[test]
    fn a_given_key_selects_its_queue_and_is_written_back() {
        let pool = AsyncPool::new(4);
        for (raw, expected) in [(9u32, 1), (6, 2), (4, 0), (7, 3)] {
            let mut key = raw;
            // SAFETY: `noop` is a live C callback for a never-dereferenced payload.
            let job = unsafe { AsyncJob::new(Some(noop), None, payload(raw as usize)) }
                .expect("a job with an `async_invoke` is a job");
            assert!(matches!(
                pool.submit(handle(9), Some(&mut key), job),
                AsyncSubmit::Queued(_)
            ));
            assert_eq!(key, expected, "{raw} % 4 selects queue {expected}");
            assert!((key as usize) < pool.threads(), "the index is inside the pool");
        }
        assert!(wait_until(|| pool.ready_len() == 4), "all four jobs finish");
        assert_eq!(pool.drain_ready(|_, _| ReadyDelivery::NeedFree), 4);
    }

    /// Without a `key`, the queue is `id % threads` (`:585-587`) — observable as the name of the one
    /// worker that drains that queue.
    #[test]
    fn a_missing_key_selects_the_queue_by_id() {
        static SEEN: Mutex<Option<String>> = Mutex::new(None);

        unsafe extern "C" fn record_thread(_: *mut c_void) {
            let current = std::thread::current();
            *SEEN.lock().unwrap_or_else(|poison| poison.into_inner()) =
                Some(current.name().unwrap_or("").to_string());
        }

        let pool = AsyncPool::new(2);
        // SAFETY: `record_thread` is a live C callback for a payload it never reads.
        let job = unsafe { AsyncJob::new(Some(record_thread), None, payload(12)) }
            .expect("a job with an `async_invoke` is a job");
        let id = match pool.submit(handle(10), None, job) {
            AsyncSubmit::Queued(id) => id,
            AsyncSubmit::Inline(_) => panic!("a 2-thread pool queues"),
        };
        assert_eq!(id, 1, "the first job of a fresh pool");
        assert!(wait_until(|| pool.ready_len() == 1), "the job finishes");
        let name = SEEN
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .expect("`async_invoke` ran");
        // `id % 2 == 1` ⇒ queue 1 ⇒ worker 2 (workers are `erts_async_1`, `erts_async_2`).
        assert_eq!(name, "erts_async_2", "the id selected queue 1 (`:586-587`)");
        assert_eq!(pool.drain_ready(|_, _| ReadyDelivery::NeedFree), 1);
    }

    /// `wait_ready` times out on an idle pool and wakes as soon as a job completes.
    #[test]
    fn wait_ready_times_out_when_idle_and_wakes_on_completion() {
        unsafe extern "C" fn slow_invoke(_: *mut c_void) {
            std::thread::sleep(Duration::from_millis(10));
        }

        let pool = AsyncPool::new(1);
        assert!(
            !pool.wait_ready(Duration::from_millis(20)),
            "an idle pool times out"
        );
        assert_eq!(pool.ready_len(), 0);

        // SAFETY: `slow_invoke` is a live C callback for a payload it never reads.
        let job = unsafe { AsyncJob::new(Some(slow_invoke), None, payload(13)) }
            .expect("a job with an `async_invoke` is a job");
        assert!(matches!(pool.submit(handle(11), None, job), AsyncSubmit::Queued(_)));
        assert!(
            pool.wait_ready(Duration::from_secs(10)),
            "the completion wakes the waiter instead of the timeout"
        );
        assert_eq!(pool.ready_len(), 1);
        assert_eq!(pool.drain_ready(|_, _| ReadyDelivery::NeedFree), 1);
    }

    /// Dropping the last handle while jobs are still queued joins the workers and releases every
    /// job exactly once — and a non-last handle drop shuts nothing down.
    #[test]
    fn dropping_the_last_handle_releases_queued_and_ready_jobs_once() {
        static FREES: AtomicUsize = AtomicUsize::new(0);

        unsafe extern "C" fn slow_invoke(_: *mut c_void) {
            std::thread::sleep(Duration::from_millis(1));
        }

        unsafe extern "C" fn job_free(_: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
        }

        let base_free = FREES.load(SeqCst);
        let pool = AsyncPool::new(1);
        let clone = pool.clone();
        assert_eq!(clone.threads(), 1, "a clone shares the same workers");

        for n in 0..16 {
            // SAFETY: two live C callbacks of the frozen signature and a never-dereferenced payload.
            let job = unsafe { AsyncJob::new(Some(slow_invoke), Some(job_free), payload(n)) }
                .expect("a job with an `async_invoke` is a job");
            assert!(matches!(pool.submit(handle(12), None, job), AsyncSubmit::Queued(_)));
        }

        // The first drop is not the last handle: the pool must keep running.
        drop(pool);
        assert_eq!(FREES.load(SeqCst) - base_free, 0, "a clone keeps the pool alive");

        // The last drop is the shutdown: join, then release whatever is left, exactly once each.
        drop(clone);
        assert_eq!(
            FREES.load(SeqCst) - base_free,
            16,
            "every queued/ready job is released exactly once at shutdown"
        );
    }

    /// A burst across several workers: no lost wake-ups, no deadlock, and every job released once.
    #[test]
    fn a_burst_of_jobs_all_completes_and_is_released_once() {
        static INVOKES: AtomicUsize = AtomicUsize::new(0);
        static FREES: AtomicUsize = AtomicUsize::new(0);
        const JOBS: usize = 64;

        unsafe extern "C" fn job_invoke(_: *mut c_void) {
            INVOKES.fetch_add(1, SeqCst);
        }

        unsafe extern "C" fn job_free(_: *mut c_void) {
            FREES.fetch_add(1, SeqCst);
        }

        let base_invokes = INVOKES.load(SeqCst);
        let base_free = FREES.load(SeqCst);
        let pool = AsyncPool::new(4);
        for n in 0..JOBS {
            // SAFETY: two live C callbacks of the frozen signature and a never-dereferenced payload.
            let job = unsafe { AsyncJob::new(Some(job_invoke), Some(job_free), payload(n)) }
                .expect("a job with an `async_invoke` is a job");
            assert!(matches!(pool.submit(handle(13), None, job), AsyncSubmit::Queued(_)));
        }

        assert!(
            wait_until(|| pool.ready_len() == JOBS),
            "every job completed ({}/{JOBS} ready)",
            pool.ready_len()
        );
        assert_eq!(
            pool.drain_ready(|_, _| ReadyDelivery::NeedFree),
            JOBS,
            "one completion per job"
        );
        assert_eq!(INVOKES.load(SeqCst) - base_invokes, JOBS);
        assert_eq!(
            FREES.load(SeqCst) - base_free,
            JOBS,
            "each job's `async_free` ran exactly once"
        );
        assert_eq!(pool.ready_len(), 0);
    }
}
