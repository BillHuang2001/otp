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

//! Multi-scheduler (SMP) pool: **N** scheduler threads, one run queue each, work stealing, the
//! timer service and the dirty pools.
//!
//! Where [`crate::scheduler::Scheduler`] is the single-threaded Milestone-1 loop, this module is
//! the SMP counterpart: a [`SchedulerPool`] owns `N` OS threads, each running the *same* frozen
//! slice logic against one shared [`ProcessTable`]. It is the Rust analogue of the C's
//! `scheduler` threads plus the global run queue in `erts/emulator/beam/erl_process.c`.
//!
//! # Topology
//!
//! * One [`SchedulerPool`] fragments work across **scheduler indices** `0..N` (N clamped to at
//!   least 1 — a runtime asking for one scheduler must work).
//! * Each scheduler owns a private **local** run queue ([`crate::run_queue::RunQueue`]) and a
//!   runner obtained from the `make_runner` factory.
//! * Work created outside a scheduler (a spawn, a timer/dirty completion, an I/O delivery) enters
//!   the shared **injector**. An idle scheduler drains its own queue, then the injector, then
//!   **steals** up to half of a peer's queue round-robin ([`crate::run_queue::RunQueue::split_off_half`]).
//! * One shared [`ThreadProgress`] barrier is bumped per safepoint; each scheduler registers a
//!   [`crate::progress::ManagedThread`] and marks itself *inactive* while parked, so a safepoint
//!   never waits on a sleeping scheduler.
//! * The process-table lock is held for at most one **reduction quantum** (configurable with
//!   [`SchedulerConfig::set_lock_quantum`], default
//!   [`crate::scheduler::DEFAULT_LOCK_QUANTUM`]) — a slice is several quanta with the lock released
//!   between them — so independent processes and cross-thread sends progress *inside* another
//!   process's slice. Slice semantics (schedule-in, own budget, one requeue) are unchanged; only
//!   the lock-hold duration is bounded.
//! * The [`crate::timer::TimerService`] and the [`crate::dirty::DirtyPool`]s are owned here; their
//!   sinks deliver into the shared table and then enqueue the woken pids through the injector.
//!
//! # `unsafe` and `Send`
//!
//! [`ProcessTable`] is `!Send` for six reasons — a raw `*mut Eterm` in [`beam_alloc::Heap`], a
//! `NonNull<Mso>` in [`beam_gc::MsoHandle`], the `Rc<AtomTable>` each process's dictionary holds,
//! and the three `Box<dyn …>` trait objects the table stores (`DistSignalRouter`,
//! `ProcessExitObserver` and `CollectionGate`, none `Send`). Wrapping it in the shared
//! [`SharedProcessTable`] therefore needs the two `unsafe impl`s on that type; the single-lock
//! argument and the full cause list live in their `// SAFETY:` comments. This is a **documented
//! handoff**: the durable fix is `Send` on `beam_alloc::Heap` and `beam_gc::MsoHandle`, an
//! `Arc<AtomTable>` dictionary and `Send` bounds on the three traits — all outside this crate's
//! scope.
//!
//! # Locking discipline (BINDING — follow it in every change)
//!
//! There are three lock families: the **queue locks** ([`parking_lot::Mutex<RunQueue>`] inside
//! `Queues`), the **table lock** ([`SharedProcessTable`]) and the **signals mutex**
//! (`PoolSignals`). The rules are:
//!
//! 1. **Never hold a queue lock while acquiring the table lock**, and never hold the table lock
//!    while acquiring a queue lock. The allowed orders are *queue → release → table* and
//!    *table → (buffer) → release → queue*.
//! 2. Because [`crate::scheduler::run_one`] / [`crate::scheduler::drain_woken_into`] invoke their
//!    requeue closure **while the table lock is held**, a worker must **buffer** those requeues
//!    into a local `Vec` and only touch the local queue lock after the table guard is dropped. The
//!    timer/dirty sinks do the same.
//! 3. The **only** nested acquisition is inside [`worker::PoolSignals::park`] /
//!    `wait_for_idle`, which hold the *signals* mutex and then check a queue: order *signals →
//!    queue*. The reverse never happens (a notifier releases its queue lock before signalling),
//!    so the hierarchy is consistent.
//! 4. The in-flight accounting is done *inside* the critical section that pops a pid, so a
//!    concurrent idle check can never observe a pid that is neither queued nor in-flight.
//!
//! # Lifecycle
//!
//! [`SchedulerPool::shutdown`] requests a stop and wakes every condvar; the workers finish the
//! slice in progress and exit. [`SchedulerPool::join`] additionally joins the scheduler threads,
//! shuts the timer service down and joins it, and shuts the dirty pool down and joins it. Pids
//! still queued at shutdown are **dropped** (the queues are abandoned; the processes stay in the
//! table). A worker error cannot be returned to a caller, so the first one is retained for
//! [`SchedulerPool::take_error`].
//!
//! Always finish a pool with [`SchedulerPool::join`]: a pool dropped **without** it leaves the
//! scheduler threads running (each holds its own handle to the shared state), so the runtime must
//! shut the pool down explicitly rather than rely on `Drop`.

mod pool;
mod queues;
mod stats;
mod worker;

pub use pool::SchedulerPool;
pub use stats::{SchedulerStat, SchedulerStats};

use std::sync::Arc;

use parking_lot::{Mutex, MutexGuard};

use crate::ptab::ProcessTable;

/// A [`ProcessTable`] shared across scheduler threads.
///
/// The whole table sits behind one `parking_lot::Mutex`; every read and every mutation a worker
/// makes goes through [`SharedProcessTable::lock`], so the single-threaded table semantics
/// (allocate, deliver, service signals, terminate) are unchanged — only *one* thread can be
/// inside them at a time.
///
/// A worker holds this lock for at most one **reduction quantum** of a slice, never for a whole
/// slice: a slice is run as `ceil(reductions_per_slice / lock_quantum)` quanta with the lock
/// released between them (see `crate::smp::worker::run_locked_quanta`). That is the granularity
/// that lets independent processes, cross-thread sends and timer/dirty completions make progress
/// while another process's slice is still in progress.
///
/// # `Send`/`Sync`
///
/// [`ProcessTable`] is `!Send`, so the automatic impls do not apply; the two `unsafe impl`s below
/// assert that confining every access to the inner mutex makes moving and sharing the wrapper
/// sound. The six `!Send` causes and the full argument are enumerated on those impls. This is a
/// **documented handoff**: the durable fix is `Send` on `beam_alloc::Heap` and `beam_gc::MsoHandle`,
/// an `Arc<AtomTable>` dictionary and `Send`-bounded `DistSignalRouter`/`ProcessExitObserver`/
/// `CollectionGate` traits, none of which this crate owns.
pub struct SharedProcessTable {
    /// The guarded table.
    inner: Mutex<ProcessTable>,
}

// SAFETY: `ProcessTable` is `!Send` for exactly six reasons, each accounted for here:
//   1. `beam_alloc::Heap.base: *mut Eterm` (`beam_alloc/src/heap.rs`), reached via
//      `Process.collector → beam_gc::AnyCollector → HeapSpace → beam_alloc::Heap`. The heap uniquely
//      owns the block; the pointer targets a carrier block owned by a `Send` (`Arc<dyn Allocator>`
//      over a `Send + Sync` allocator) whose address is stable across the move, and nothing aliases
//      it concurrently.
//   2. `beam_gc::MsoHandle.ptr: NonNull<Mso>` (`beam_gc/src/mso.rs`), reached through
//      `beam_message::OffHeapTable`. The handle uniquely owns the `Mso`, whose payload is an
//      immutable `Box<dyn Any + Send>`.
//   3. `Dict.atoms: Rc<AtomTable>` (`beam_process/src/dict.rs`) — `Rc` is `!Send` (its refcount is
//      not atomic). INVARIANT: every process's dictionary is built by `Dict::new()` and its
//      `Rc<AtomTable>` is NEVER cloned or shared outside this table, so no `Rc` is ever touched from
//      two threads. Do NOT clone a dictionary's `Rc<AtomTable>` to another thread, and do NOT adopt
//      `Dict::with_atoms` with an `Rc` the host also holds on another thread (it is test-only today).
//   4. `ProcessTable.router: Option<Box<dyn DistSignalRouter>>` (`ptab.rs`) — the trait object has
//      no `Send` bound.
//   5. `ProcessTable.exit_observer: Option<Box<dyn ProcessExitObserver>>` (`ptab.rs`) — likewise no
//      `Send` bound.
//   6. `ProcessTable.collection_gate: Option<Box<dyn CollectionGate>>` (`ptab.rs`) — likewise no
//      `Send` bound. All three trait objects are invoked only with the table lock held and carry no
//      thread-affine state.
//
// Every access to the wrapped table — every field, and every off-heap pointer it owns — goes
// through the single inner `parking_lot::Mutex`, and `SharedProcessTable` exposes the table ONLY
// via `lock`/`try_lock`, returning a `MutexGuard` that is itself `!Send` (it borrows the mutex). So
// at most one thread is ever inside the table and no `&mut ProcessTable` — nor any `!Send` value it
// owns — can move off the locking thread: those values are safe to move under the lock, and sharing
// `&self` across threads cannot create an aliasing or data race.
unsafe impl Send for SharedProcessTable {}

// SAFETY: `&SharedProcessTable` exposes only `lock`/`try_lock`, both of which return a `!Send`
// mutex guard, so cross-thread sharing is mediated entirely by the inner mutex — the same
// single-lock argument as the `Send` impl above.
unsafe impl Sync for SharedProcessTable {}

impl SharedProcessTable {
    /// Wraps `table` in a shared, thread-safe handle.
    pub fn new(table: ProcessTable) -> Arc<SharedProcessTable> {
        Arc::new(SharedProcessTable {
            inner: Mutex::new(table),
        })
    }

    /// Locks the table, blocking until the lock is free.
    ///
    /// The returned guard derefs to `&mut ProcessTable`; hold it only for the duration of one
    /// slice/signal/table operation and never acquire a queue lock while it is held (see the
    /// module-level locking discipline).
    pub fn lock(&self) -> MutexGuard<'_, ProcessTable> {
        self.inner.lock()
    }

    /// Tries to lock the table, returning `None` when it is already held.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, ProcessTable>> {
        self.inner.try_lock()
    }
}

impl std::fmt::Debug for SharedProcessTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedProcessTable")
            .field("locked", &self.inner.is_locked())
            .finish()
    }
}

/// How to build a [`SchedulerPool`].
///
/// A plain, by-value configuration (with chainable setters returning `&mut Self`) so a runtime can
/// assemble it incrementally. [`SchedulerConfig::new`] is the smallest working pool: **one**
/// scheduler, timers on, no dirty schedulers, the default reduction budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// Normal scheduler threads (clamped to at least 1 by [`SchedulerPool::start`]).
    schedulers: usize,
    /// Reductions per slice.
    reductions_per_slice: u32,
    /// The maximum number of reductions a scheduler runs per process-table lock acquisition
    /// (see [`crate::scheduler::DEFAULT_LOCK_QUANTUM`]). Clamped to at least 1.
    lock_quantum: u32,
    /// Dirty-CPU worker threads.
    dirty_cpu: usize,
    /// Dirty-IO worker threads.
    dirty_io: usize,
    /// Whether the timer service is started.
    timers: bool,
}

impl SchedulerConfig {
    /// The default configuration: 1 scheduler, 0 dirty workers, timers on, the default reduction
    /// budget and the default table-lock quantum.
    pub fn new() -> SchedulerConfig {
        SchedulerConfig {
            schedulers: 1,
            reductions_per_slice: crate::scheduler::DEFAULT_REDUCTIONS_PER_SLICE,
            lock_quantum: crate::scheduler::DEFAULT_LOCK_QUANTUM,
            dirty_cpu: 0,
            dirty_io: 0,
            timers: true,
        }
    }

    /// A configuration with `n` scheduler threads (a later [`SchedulerPool::start`] clamps `0`
    /// up to `1`).
    pub fn with_schedulers(n: usize) -> SchedulerConfig {
        let mut config = SchedulerConfig::new();
        config.set_schedulers(n);
        config
    }

    /// The configured number of scheduler threads.
    pub fn schedulers(&self) -> usize {
        self.schedulers
    }

    /// Sets the number of scheduler threads.
    pub fn set_schedulers(&mut self, n: usize) -> &mut Self {
        self.schedulers = n;
        self
    }

    /// The configured per-slice reduction budget.
    pub fn reductions_per_slice(&self) -> u32 {
        self.reductions_per_slice
    }

    /// Sets the per-slice reduction budget.
    pub fn set_reductions_per_slice(&mut self, n: u32) -> &mut Self {
        self.reductions_per_slice = n;
        self
    }

    /// The maximum number of reductions a scheduler runs per process-table lock acquisition.
    ///
    /// A slice of `reductions_per_slice` reductions is run as `ceil(reductions_per_slice /
    /// lock_quantum)` quanta, the table lock being released between them, so this bounds how long
    /// one scheduler can hold the lock (and therefore how long an independent process or a
    /// cross-thread send waits). A value `>= reductions_per_slice` reproduces the old
    /// one-lock-acquisition-per-slice behavior.
    pub fn lock_quantum(&self) -> u32 {
        self.lock_quantum
    }

    /// Sets the per-lock-acquisition reduction quantum (clamped to at least 1 by
    /// [`SchedulerPool::start`]).
    pub fn set_lock_quantum(&mut self, n: u32) -> &mut Self {
        self.lock_quantum = n;
        self
    }
    /// The configured number of dirty-CPU worker threads.
    pub fn dirty_cpu(&self) -> usize {
        self.dirty_cpu
    }

    /// Sets the number of dirty-CPU worker threads.
    pub fn set_dirty_cpu(&mut self, n: usize) -> &mut Self {
        self.dirty_cpu = n;
        self
    }

    /// The configured number of dirty-IO worker threads.
    pub fn dirty_io(&self) -> usize {
        self.dirty_io
    }

    /// Sets the number of dirty-IO worker threads.
    pub fn set_dirty_io(&mut self, n: usize) -> &mut Self {
        self.dirty_io = n;
        self
    }

    /// Whether the timer service is started by [`SchedulerPool::start`].
    pub fn timers(&self) -> bool {
        self.timers
    }

    /// Enables or disables the timer service.
    pub fn set_timers(&mut self, on: bool) -> &mut Self {
        self.timers = on;
        self
    }
}

impl Default for SchedulerConfig {
    fn default() -> SchedulerConfig {
        SchedulerConfig::new()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the configuration defaults and the shared-table wrapper.

    use super::*;
    use crate::scheduler::{DEFAULT_LOCK_QUANTUM, DEFAULT_REDUCTIONS_PER_SLICE};
    use crate::signal::SignalAtoms;
    use beam_term::prelude::AtomTable;

    fn table() -> ProcessTable {
        ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
    }

    #[test]
    fn config_defaults_and_setters() {
        let config = SchedulerConfig::new();
        assert_eq!(config.schedulers(), 1);
        assert_eq!(config.reductions_per_slice(), DEFAULT_REDUCTIONS_PER_SLICE);
        assert_eq!(config.lock_quantum(), DEFAULT_LOCK_QUANTUM);
        assert_eq!(config.dirty_cpu(), 0);
        assert_eq!(config.dirty_io(), 0);
        assert!(config.timers());
        assert_eq!(SchedulerConfig::default(), config);

        let mut config = SchedulerConfig::with_schedulers(8);
        config
            .set_reductions_per_slice(64)
            .set_lock_quantum(7)
            .set_dirty_cpu(2)
            .set_dirty_io(3)
            .set_timers(false);
        assert_eq!(config.schedulers(), 8);
        assert_eq!(config.reductions_per_slice(), 64);
        assert_eq!(config.lock_quantum(), 7);
        assert_eq!(config.dirty_cpu(), 2);
        assert_eq!(config.dirty_io(), 3);
        assert!(!config.timers());
    }

    #[test]
    fn shared_table_lock_and_try_lock() {
        let shared = SharedProcessTable::new(table());
        let pid = shared.lock().spawn_default().expect("a fresh process");
        assert!(shared.lock().contains(pid));
        // The previous guards were temporaries, released at the end of their statements.
        assert!(shared.try_lock().is_some(), "the table is free");

        let guard = shared.lock();
        assert!(shared.try_lock().is_none(), "try_lock fails while locked");
        drop(guard);
        assert!(shared.try_lock().is_some(), "the lock is free again");
    }

    #[test]
    fn shared_table_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<SharedProcessTable>>();
    }
}
