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

//! The dirty scheduler pool — dedicated worker threads for dirty-CPU and dirty-IO work.
//!
//! A NIF or driver that may block for a long time (a file-system call, a crypto primitive, a
//! blocking `recv`) must not stall a normal scheduler. ERTS therefore keeps **two independent
//! dirty pools** — one for dirty **CPU-bound** work and one for dirty **IO-bound** work — each
//! a fixed set of worker threads. A normal scheduler *hands off* a unit of work to the right
//! pool, the pool runs it to completion on a dirty worker thread, and the finished job's
//! **completion is routed back to a normal scheduler**, which decides what to do with it
//! (re-enter a process into a run queue, or deliver a message and make the target runnable).
//!
//! [`DirtyPool`] is that structure: [`DirtyKind::Cpu`] and [`DirtyKind::Io`] each own a job
//! queue ([`parking_lot::Mutex`] + [`parking_lot::Condvar`] over a `VecDeque`) and `N` worker
//! threads ([`DirtyPool::start`]). [`DirtyPool::submit`] is the hand-off; the worker thread
//! runs the [`DirtyJob`] closure and reports its [`DirtyOutcome`] through the
//! [`DirtyCompletionSink`] seam (`DirtyPool::set_sink`), which the scheduler pool implements
//! (Part II §H, Stage 5).
//!
//! # Scope of this floor
//!
//! * **No live Erlang heap data crosses the seam.** A [`DirtyJob`] is a plain Rust closure; the
//!   hand-off carries no terms. A NIF/driver-grade dirty job that *owns* heap terms (and must
//!   copy/reference them across a heap boundary, as the C `ErtsDirtyJob` does) is a follow-up —
//!   this module models the routing/hand-off/completion *shapes*, not the term plumbing.
//! * **No integration with real NIF/driver code.** That lives in `beam_abi`/`beam_bifs` and is
//!   out of scope here.
//! * [`DirtyOutcome`] is the dirty analogue of a run-scheduling result: it is *routing data* the
//!   normal scheduler acts on. It is deliberately **not** a variant of
//!   [`crate::scheduler::RunOutcome`] (that enum is frozen and matched exhaustively elsewhere) —
//!   the dirty routing result stays local to this module.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_process.h`
//! (`ERTS_DIRTY_CPU_SCHEDULER`/`ERTS_DIRTY_IO_SCHEDULER`, `erts_no_dirty_cpu_schedulers`,
//! `erts_no_dirty_io_schedulers`, `ErtsDirtyJob`) and `erts/emulator/beam/erl_process.c` (the
//! dirty pools, `erts_schedule_dirty_job` and the dirty-completion → normal-scheduler path),
//! plus `erts/emulator/beam/erl_nfunc_sched.c` (the NIF trampoline that schedules dirty work).

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use beam_term::prelude::Eterm;
use parking_lot::{Condvar, Mutex};

use crate::pid::ProcessId;

/// The two dirty scheduler kinds.
///
/// Mirrors the C `ERTS_DIRTY_CPU_SCHEDULER`/`ERTS_DIRTY_IO_SCHEDULER`: dirty-CPU work (a NIF that
/// burns CPU for a long time) and dirty-IO work (a NIF that blocks on I/O). They are served by
/// **independent** pools so a burst of dirty-IO jobs cannot starve dirty-CPU jobs (and vice
/// versa).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DirtyKind {
    /// Dirty CPU-bound work (the C `ERTS_DIRTY_CPU_SCHEDULER`).
    Cpu,
    /// Dirty IO-bound work (the C `ERTS_DIRTY_IO_SCHEDULER`).
    Io,
}

impl DirtyKind {
    /// Both dirty kinds, in the C enumeration order (`dirty_cpu_scheduler` then
    /// `dirty_io_scheduler`).
    pub const ALL: [DirtyKind; 2] = [DirtyKind::Cpu, DirtyKind::Io];

    /// The scheduler-name string for this kind, as [`erlang:system_info/1`]'s
    /// `dirty_cpu_schedulers`/`dirty_io_schedulers` names them (and as a dirty worker thread is
    /// named).
    pub const fn name(self) -> &'static str {
        match self {
            DirtyKind::Cpu => "dirty_cpu_scheduler",
            DirtyKind::Io => "dirty_io_scheduler",
        }
    }
}

/// What a finished dirty job reports back to a normal scheduler.
///
/// A dirty job runs to completion on a dirty worker thread; the *normal* scheduler owns the
/// process-level consequences, so the worker does not touch processes directly — it hands one of
/// these to the [`DirtyCompletionSink`] and the scheduler pool interprets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtyOutcome {
    /// The job is done; nothing further to signal.
    Done,
    /// The named process may run again on a normal scheduler (re-enter a run queue).
    Reschedule(ProcessId),
    /// Deliver `message` to `target` and then make it runnable (a normal-scheduler action).
    Deliver {
        /// The process that should receive `message`.
        target: ProcessId,
        /// The message term to deliver onto the target's heap.
        message: Eterm,
    },
}

/// A unit of dirty work. Runs to completion on a dirty worker thread.
///
/// It is a plain Rust closure — the hand-off currently carries **no live Erlang heap data** (see
/// the module docs); a term-owning dirty job is a documented follow-up.
pub type DirtyJob = Box<dyn FnOnce() -> DirtyOutcome + Send + 'static>;

/// The seam the dirty pool signals completion through — implemented by the scheduler pool.
///
/// [`DirtyPool`] calls it **once per finished dirty job**, from the dirty worker thread that ran
/// the job, so it must be `Send + Sync`. If it is installed *after* jobs have already finished,
/// their outcomes are buffered (never dropped) and flushed to it — see
/// [`DirtyPool::set_sink`].
pub trait DirtyCompletionSink: Send + Sync {
    /// Called once per finished dirty job, from the dirty worker thread.
    fn dirty_completed(&self, outcome: DirtyOutcome);
}

/// The completion channel for one dirty pool: hands a finished job's [`DirtyOutcome`] to the
/// installed [`DirtyCompletionSink`], or **buffers** it until a sink is installed.
///
/// This is the "no completion is silently lost" guarantee: a job that runs before
/// [`DirtyPool::set_sink`] is called has its outcome queued here and flushed the moment a sink
/// arrives.
struct SinkSlot {
    state: Mutex<SinkState>,
}

/// The sink plus the not-yet-delivered outcomes buffered before it was installed.
#[derive(Default)]
struct SinkState {
    sink: Option<Arc<dyn DirtyCompletionSink>>,
    buffered: Vec<DirtyOutcome>,
}

impl SinkSlot {
    /// An empty slot with no sink installed and nothing buffered.
    fn new() -> SinkSlot {
        SinkSlot {
            state: Mutex::new(SinkState::default()),
        }
    }

    /// Records a finished job: hands it straight to the sink, or buffers it (in order) until a
    /// sink is installed.
    ///
    /// The sink is invoked **without** this slot's lock held, so a sink that re-enters the pool
    /// (e.g. [`DirtyPool::set_sink`] or [`DirtyPool::pending`]) cannot deadlock.
    fn dispatch(&self, outcome: DirtyOutcome) {
        let mut state = self.state.lock();
        // Clone the `Arc` out of the lock, then call the sink lock-free.
        match state.sink.clone() {
            Some(sink) => {
                drop(state);
                sink.dirty_completed(outcome);
            }
            None => state.buffered.push(outcome),
        }
    }

    /// Installs (or replaces) the sink and returns every outcome buffered before the install, so
    /// the caller can flush them to the new sink.
    ///
    /// The swap and the drain happen under one lock, so no outcome can slip between "the sink
    /// becomes visible" and "the buffer is emptied": a concurrent [`SinkSlot::dispatch`] either
    /// sees no sink (and is drained here) or sees this sink (and is delivered directly).
    fn install(&self, sink: Arc<dyn DirtyCompletionSink>) -> Vec<DirtyOutcome> {
        let mut state = self.state.lock();
        let buffered = std::mem::take(&mut state.buffered);
        state.sink = Some(sink);
        buffered
    }
}

/// One dirty pool's shared state: the job queue, the completion channel, the pending count and
/// the shutdown flag. Shared (`Arc`) between the pool handle and its worker threads.
struct Queue {
    jobs: Mutex<VecDeque<DirtyJob>>,
    available: Condvar,
    /// Queued + currently-running jobs (decremented when the job's closure returns).
    pending: AtomicUsize,
    shutdown: AtomicBool,
    sink: SinkSlot,
}

impl Queue {
    /// A fresh, empty queue in the accepting state.
    fn new() -> Arc<Queue> {
        Arc::new(Queue {
            jobs: Mutex::new(VecDeque::new()),
            available: Condvar::new(),
            pending: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            sink: SinkSlot::new(),
        })
    }

    /// Enqueues a job and wakes one worker.
    ///
    /// After [`Queue::request_shutdown`] the job is **dropped** (the pool is winding down and no
    /// worker will run it); this never panics.
    fn submit(&self, job: DirtyJob) {
        let mut jobs = self.jobs.lock();
        if self.shutdown.load(Ordering::Acquire) {
            return; // dropped — the pool has been shut down
        }
        jobs.push_back(job);
        self.pending.fetch_add(1, Ordering::AcqRel);
        self.available.notify_one();
    }

    /// The number of jobs queued or currently running.
    fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    /// Asks every worker to exit once the queue is drained, waking the idle ones.
    ///
    /// The `jobs` lock is held across the store + notify (see the body) so the wake-up cannot be
    /// lost and [`DirtyPool::join`] cannot hang.
    fn request_shutdown(&self) {
        // Take the `jobs` lock so this cannot race a worker that has just read `shutdown` as
        // false and is about to register on the condvar: the worker holds `jobs` until `wait`
        // atomically releases it, so once we hold the lock any waiter is already registered and
        // the notify below cannot be lost.
        let _jobs = self.jobs.lock();
        self.shutdown.store(true, Ordering::Release);
        self.available.notify_all();
    }

    /// Blocks until a job is available, then takes it; returns `None` when the pool is shut down
    /// **and** the queue is empty.
    ///
    /// Jobs still queued at shutdown are drained (run) before the worker exits, so a graceful
    /// shutdown never abandons already-submitted work.
    fn take_job(&self) -> Option<DirtyJob> {
        let mut jobs = self.jobs.lock();
        loop {
            if let Some(job) = jobs.pop_front() {
                return Some(job);
            }
            if self.shutdown.load(Ordering::Acquire) {
                return None;
            }
            self.available.wait(&mut jobs);
        }
    }
}

/// The body of a dirty worker thread: pull jobs until shut down, run each, and route its
/// outcome.
///
/// A panicking job is **isolated**: the panic is caught (after the default hook has already
/// printed it — it is never silent), the worker survives, the job's `pending` slot is released
/// and no outcome is routed for it. One misbehaving job therefore cannot kill a worker and
/// starve the pool.
fn worker_loop(queue: Arc<Queue>) {
    while let Some(job) = queue.take_job() {
        let outcome = catch_unwind(AssertUnwindSafe(job)).ok();
        // Release the pending slot *before* routing, so once a sink observes the completion the
        // counter already reflects it (a `pending` that has drained is a `pending` whose
        // completions have all been routed).
        queue.pending.fetch_sub(1, Ordering::AcqRel);
        if let Some(outcome) = outcome {
            queue.sink.dispatch(outcome);
        }
    }
}

/// One dirty pool: its worker count and their join handles plus the shared [`Queue`].
struct WorkerPool {
    workers: usize,
    queue: Arc<Queue>,
    handles: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    /// Spawns `workers` threads draining a fresh queue.
    ///
    /// Each thread is named `"<kind>-<index>"` (e.g. `dirty_cpu_scheduler-0`) for diagnosis.
    /// Failing to spawn a requested worker is a startup-resource failure and is reported loudly
    /// (there is no partial pool).
    fn start(kind: DirtyKind, workers: usize) -> WorkerPool {
        let queue = Queue::new();
        let mut handles = Vec::with_capacity(workers);
        for ix in 0..workers {
            let worker_queue = Arc::clone(&queue);
            let handle = std::thread::Builder::new()
                .name(format!("{}-{}", kind.name(), ix))
                .spawn(move || worker_loop(worker_queue))
                .expect("spawning a dirty worker thread");
            handles.push(handle);
        }
        WorkerPool {
            workers,
            queue,
            handles,
        }
    }

    /// Requests shutdown and blocks until every worker thread has exited.
    fn join(self) {
        self.queue.request_shutdown();
        for handle in self.handles {
            // A worker that died because a job panicked already reported its panic through the
            // default hook when it happened; `join` only waits for the thread to be gone.
            let _ = handle.join();
        }
    }
}

/// A pool of dirty-CPU and dirty-IO worker threads.
///
/// Start it with [`DirtyPool::start`], install a [`DirtyCompletionSink`] with
/// [`DirtyPool::set_sink`] (before or after submitting jobs — see below), hand work off with
/// [`DirtyPool::submit`], and finish with [`DirtyPool::shutdown`] + [`DirtyPool::join`].
///
/// # Sink semantics
///
/// A [`DirtyCompletionSink`] may be installed **before or after** jobs are submitted. If a job
/// finishes before any sink is installed, its [`DirtyOutcome`] is **buffered** (never dropped)
/// and flushed to the sink the moment it is installed. If [`DirtyPool::set_sink`] is called
/// again, the new sink replaces the old one; later completions go to the new sink.
pub struct DirtyPool {
    cpu: WorkerPool,
    io: WorkerPool,
}

impl DirtyPool {
    /// Starts a pool with `cpu` dirty-CPU workers and `io` dirty-IO workers.
    ///
    /// A kind with `0` workers runs nothing: jobs submitted to it are **buffered** (queued and
    /// counted by [`DirtyPool::pending`]) and discarded when the pool is joined. This never
    /// panics — see [`DirtyPool::submit`].
    pub fn start(cpu: usize, io: usize) -> DirtyPool {
        DirtyPool {
            cpu: WorkerPool::start(DirtyKind::Cpu, cpu),
            io: WorkerPool::start(DirtyKind::Io, io),
        }
    }

    /// Installs the completion sink, flushing any outcomes buffered before it existed.
    ///
    /// The flush happens after the sink is made visible, so a completion racing with this call is
    /// delivered exactly once (either through the flush or directly).
    pub fn set_sink(&self, sink: Arc<dyn DirtyCompletionSink>) {
        let buffered = self.cpu.queue.sink.install(Arc::clone(&sink));
        for outcome in buffered {
            sink.dirty_completed(outcome);
        }
        let buffered = self.io.queue.sink.install(Arc::clone(&sink));
        for outcome in buffered {
            sink.dirty_completed(outcome);
        }
    }

    /// The number of dirty-CPU worker threads.
    pub fn cpu_schedulers(&self) -> usize {
        self.cpu.workers
    }

    /// The number of dirty-IO worker threads.
    pub fn io_schedulers(&self) -> usize {
        self.io.workers
    }

    /// The number of worker threads serving `kind`.
    pub fn schedulers(&self, kind: DirtyKind) -> usize {
        self.pool(kind).workers
    }

    /// Hands a job off to the `kind` pool.
    ///
    /// After [`DirtyPool::shutdown`] the job is silently **dropped** (the pool is winding down);
    /// this method never panics, and neither does submitting to a pool with no workers for the
    /// kind (there the job is buffered — see [`DirtyPool::start`]).
    pub fn submit(&self, kind: DirtyKind, job: DirtyJob) {
        self.pool(kind).queue.submit(job);
    }

    /// The number of jobs of `kind` that are queued or currently running.
    pub fn pending(&self, kind: DirtyKind) -> usize {
        self.pool(kind).queue.pending()
    }

    /// Asks both pools' workers to exit (after draining their queues).
    pub fn shutdown(&self) {
        self.cpu.queue.request_shutdown();
        self.io.queue.request_shutdown();
    }

    /// Requests shutdown and blocks until every worker thread has exited.
    ///
    /// Safe to call without a preceding [`DirtyPool::shutdown`]: it shuts down first itself.
    pub fn join(self) {
        let DirtyPool { cpu, io } = self;
        cpu.join();
        io.join();
    }

    /// The pool for `kind`.
    fn pool(&self, kind: DirtyKind) -> &WorkerPool {
        match kind {
            DirtyKind::Cpu => &self.cpu,
            DirtyKind::Io => &self.io,
        }
    }
}

impl Default for DirtyPool {
    /// A minimal *functional* pool: one dirty-CPU and one dirty-IO worker.
    ///
    /// A real runtime sizes the pools from the machine/`+SDcpu`/`+SDio` (like the C
    /// `erts_no_dirty_cpu_schedulers`/`erts_no_dirty_io_schedulers`); this default only exists so
    /// a caller that does not care can still have every submitted job run.
    fn default() -> DirtyPool {
        DirtyPool::start(1, 1)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pure bookkeeping: the kind identities and the [`SinkSlot`] buffer /
    //! flush (deterministic, no threads).

    use std::sync::Arc;

    use beam_term::prelude::make_small;

    use super::*;

    /// A sink that records every outcome handed to it.
    #[derive(Default)]
    struct Recorder {
        seen: Mutex<Vec<DirtyOutcome>>,
    }

    impl Recorder {
        fn seen(&self) -> Vec<DirtyOutcome> {
            self.seen.lock().clone()
        }
    }

    impl DirtyCompletionSink for Recorder {
        fn dirty_completed(&self, outcome: DirtyOutcome) {
            self.seen.lock().push(outcome);
        }
    }

    #[test]
    fn kind_names_match_the_scheduler_identities() {
        assert_eq!(DirtyKind::Cpu.name(), "dirty_cpu_scheduler");
        assert_eq!(DirtyKind::Io.name(), "dirty_io_scheduler");
        assert_eq!(DirtyKind::ALL, [DirtyKind::Cpu, DirtyKind::Io]);
    }

    #[test]
    fn outcomes_before_a_sink_are_buffered_in_order_then_flushed() {
        let slot = SinkSlot::new();
        let first = DirtyOutcome::Done;
        let second = DirtyOutcome::Reschedule(ProcessId::new(1, 2));
        slot.dispatch(first);
        slot.dispatch(second);

        // Nothing has been delivered yet — the sink did not exist.
        let sink = Arc::new(Recorder::default());
        assert!(sink.seen().is_empty(), "no sink was installed yet");

        // Installing the sink hands back both buffered outcomes, in submission order.
        let buffered = slot.install(sink.clone());
        assert_eq!(buffered, vec![first, second]);
        for outcome in buffered {
            sink.dirty_completed(outcome);
        }
        assert_eq!(sink.seen(), vec![first, second]);
    }

    #[test]
    fn outcomes_after_a_sink_go_through_immediately() {
        let slot = SinkSlot::new();
        let sink = Arc::new(Recorder::default());
        assert!(slot.install(sink.clone()).is_empty());

        let delivered = DirtyOutcome::Deliver {
            target: ProcessId::new(3, 4),
            message: make_small(42),
        };
        slot.dispatch(delivered);
        assert_eq!(sink.seen(), vec![delivered]);
    }

    #[test]
    fn installing_a_second_sink_rebinds_future_completions() {
        let slot = SinkSlot::new();
        let first = Arc::new(Recorder::default());
        assert!(slot.install(first.clone()).is_empty());
        slot.dispatch(DirtyOutcome::Done);

        let second = Arc::new(Recorder::default());
        // The rebind itself has nothing to flush...
        assert!(slot.install(second.clone()).is_empty());
        slot.dispatch(DirtyOutcome::Reschedule(ProcessId::new(9, 0)));

        assert_eq!(first.seen(), vec![DirtyOutcome::Done]);
        assert_eq!(
            second.seen(),
            vec![DirtyOutcome::Reschedule(ProcessId::new(9, 0))]
        );
    }
}
