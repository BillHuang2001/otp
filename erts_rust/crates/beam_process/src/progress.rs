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

//! Thread progress — the cross-scheduler safepoint barrier.
//!
//! Every participating OS thread (a **managed thread** — in ERTS the schedulers, the
//! system-message dispatcher and the aux thread) frequently *confirms* the current global
//! progress generation by calling [`ManagedThread::update`]. A thread that needs a guarantee
//! about every other thread — "no thread still holds a pointer into the structure I am about
//! to free" — bumps the global generation and then calls [`ThreadProgress::wait`], which
//! blocks until every **active** managed thread has confirmed (`progress() >=`) that
//! generation. This is the mechanism ERTS uses *instead of* reference-counting every access
//! to a shared structure when it needs to know when the structure can safely be deallocated
//! (see the internal document `erts/emulator/internal_doc/ThreadProgress.md`); it replaces
//! per-access cache-line ping-pong with one fixed, cheap per-thread store.
//!
//! # The protocol
//!
//! * One global, monotonically increasing **generation** ([`ThreadProgress::bump`]).
//! * A registry of **managed threads**, each with its own `progress` value
//!   ([`ThreadProgress::managed_thread`]).
//! * A managed thread confirms progress by storing the current generation into its own slot
//!   ([`ManagedThread::update`]). A managed thread must do this frequently — at every point
//!   at which it is safely interruptible (between process slices, when going to sleep and
//!   when waking up, per the C's `erts_thr_progress_update()`).
//! * A thread reaches the safepoint with [`ThreadProgress::wait`]: it blocks until every
//!   active managed thread has confirmed the generation it passed. `wait` never changes the
//!   generation; [`ThreadProgress::safepoint`] is the `bump` + `wait` combination.
//! * An **idle/parked** managed thread sets itself inactive
//!   ([`ManagedThread::set_active`]) — it is already at a safepoint by definition and is
//!   therefore excluded from the wait. In the C this is the `WAITING` state a scheduler
//!   enters in `erts_thr_progress_prepare_wait()`.
//! * A waiter calls the registered [`ThreadProgress::set_waker`] closure before blocking so
//!   a partially-idle pool can be nudged to make the required progress (the C's
//!   `erts_thr_progress_wakeup()`); it is re-invoked periodically while blocking, so a nudge
//!   racing with the block cannot strand the waiter.
//! * [`ThreadProgress::block`]/[`ThreadProgress::unblock`] is the nested "thread progress is
//!   currently blocked" counter queried by [`ThreadProgress::is_blocking`].
//!
//! # Deviations from the C (documented, contained)
//!
//! * The C bumps the generation by 2 and reserves one value as the in-band `WAITING`
//!   sentinel; here the parked state is the explicit `active` flag instead, and every
//!   generation value is usable. Generation arithmetic is `wrapping_add`, so the barrier
//!   never panics (a wrap needs 2^64 generations and is not modelled with the C's wrap-aware
//!   comparison).
//! * The C tracks `is_blocking` per thread (`tpd->is_blocking`) plus a global managed-thread
//!   block count used to park the other schedulers; this crate owns the global nested
//!   counter the SMP scheduler pool will consult, with the "park the other schedulers" half
//!   belonging to that pool.
//! * Unmanaged-thread delay handles (`erts_thr_progress_unmanaged_delay`) and
//!   `ErtsThrPrgrLaterOp` delayed operations are not modelled; the SMP scheduler pool and
//!   the delayed-dealloc coupling with `beam_alloc` are the follow-ups that need them.
//!
//! # Errors and `unsafe`
//!
//! Nothing here is fallible (there is no runtime data to validate) and nothing here is
//! `unsafe`: the barrier is an ordinary `parking_lot` mutex + condition variable over atomic
//! per-thread slots. No method panics.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_thr_progress.{c,h}`
//! (`erts_thr_progress_update`, `erts_thr_progress_active`, `erts_thr_progress_block`/
//! `unblock`/`is_blocking`, `wakeup_managed`) and `erts/emulator/internal_doc/ThreadProgress.md`.

use std::cell::Cell;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

/// How long [`ThreadProgress::wait`] sleeps before re-invoking the registered waker.
///
/// The waker is called once *before* the waiter blocks; re-invoking it on a timeout closes
/// the (narrow) race where a nudge is sent between the predicate check and the block, so a
/// waiter can never sleep through the release it is waiting for.
const NUDGE_INTERVAL: Duration = Duration::from_millis(1);

/// Hands out the instance identities used by the per-thread "current managed thread" binding.
static NEXT_INSTANCE_ID: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The managed-thread slot the current OS thread last *drove* (`update`/`set_active`),
    /// as `(instance id, slot id)`.
    ///
    /// This is the Rust equivalent of the C reading its per-thread `ErtsThrPrgrData` out of
    /// thread-local storage: it lets [`ThreadProgress::wait`] recognize the caller's **own**
    /// slot and not wait for itself (the C parks its own `confirmed` value at `WAITING`
    /// while it waits, for the same reason).
    static CURRENT_SLOT: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

/// The slot id the current thread is driving on `instance`, if any.
fn current_slot(instance: usize) -> Option<usize> {
    CURRENT_SLOT.with(|cell| match cell.get() {
        Some((bound_instance, slot)) if bound_instance == instance => Some(slot),
        _ => None,
    })
}

/// Records that the current thread drives slot `slot` of `instance`.
fn bind_current(instance: usize, slot: usize) {
    CURRENT_SLOT.with(|cell| cell.set(Some((instance, slot))));
}

/// The per-managed-thread progress slot.
struct Slot {
    /// Registration index — stable for the lifetime of the registration, and equal to the
    /// slot's position in [`State::slots`].
    id: usize,
    /// The global generation this thread last confirmed.
    progress: AtomicU64,
    /// Whether the thread still has to reach the safepoint; `false` means the thread is
    /// parked and therefore already at a safepoint.
    active: AtomicBool,
}

/// The parts of a [`ThreadProgress`] that need mutual exclusion.
struct State {
    /// The registered managed threads, in registration order (`slots[id].id == id`).
    slots: Vec<Arc<Slot>>,
    /// The number of nested [`ThreadProgress::block`] calls.
    blocking: usize,
}

/// The state shared by a [`ThreadProgress`] and all of its clones.
struct Inner {
    /// Identity of this tracker, used by the per-thread current-slot binding.
    instance: usize,
    /// The global generation.
    generation: AtomicU64,
    /// The managed-thread registry and the blocking counter.
    state: Mutex<State>,
    /// Signalled whenever a predicate input changes (`update`, `set_active`).
    updated: Condvar,
    /// Invoked by [`ThreadProgress::wait`] to wake parked managed threads.
    waker: Mutex<Option<Arc<dyn Fn() + Send + Sync + 'static>>>,
}

/// The cross-scheduler thread-progress barrier.
///
/// Cheap to clone: every clone shares the same generation, managed-thread registry and
/// blocking counter (the tracker is the runtime-wide barrier, not a per-thread object).
/// Every method takes `&self` (interior mutability + atomics), so a `&mut` borrow of a
/// holder — e.g. `Scheduler::progress_mut()` — calls it just as well as a `&` borrow.
///
/// ```
/// use beam_process::ThreadProgress;
///
/// let progress = ThreadProgress::new();
/// assert_eq!(progress.generation(), 0);
///
/// // With nobody registered to wait for, a safepoint is just a bump.
/// assert_eq!(progress.safepoint(), 1);
/// assert_eq!(progress.generation(), 1);
/// ```
pub struct ThreadProgress {
    /// The shared barrier state.
    inner: Arc<Inner>,
}

impl ThreadProgress {
    /// A fresh barrier at generation 0 with no managed threads and no blocked threads.
    pub fn new() -> ThreadProgress {
        ThreadProgress {
            inner: Arc::new(Inner {
                instance: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
                generation: AtomicU64::new(0),
                state: Mutex::new(State {
                    slots: Vec::new(),
                    blocking: 0,
                }),
                updated: Condvar::new(),
                waker: Mutex::new(None),
            }),
        }
    }

    /// The current global generation.
    ///
    /// This is the value a managed thread confirms with [`ManagedThread::update`] and the
    /// value [`ThreadProgress::wait`] is asked to wait for.
    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// Advances the global generation and returns the new value.
    ///
    /// Monotonic: concurrent bumpers each observe a distinct value (the increment is a single
    /// atomic `fetch_add`). The counter wraps rather than panicking, which needs 2^64
    /// generations to be observable.
    pub fn bump(&self) -> u64 {
        // A generation bump is a synchronizing event in the C (threads issue a full memory
        // barrier when they reach a new generation), so the read-modify-write is AcqRel.
        self.inner
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1)
    }

    /// Blocks until every **active** managed thread has reached `generation` (i.e. every
    /// active thread's [`ManagedThread::progress`] `>= generation`), then returns.
    ///
    /// * Returns immediately when no managed thread is registered, or when every registered
    ///   thread is inactive or already at/after `generation` — this is the single-scheduler
    ///   case, so a `Scheduler` that is the only participant never blocks.
    /// * The caller's **own** slot is never waited for: a thread that drives a managed slot
    ///   cannot confirm progress while it is blocked, so waiting on itself could only
    ///   deadlock it. (The C parks its own `confirmed` value at `WAITING` for exactly this
    ///   reason.)
    /// * The registered waker ([`ThreadProgress::set_waker`]) is invoked by *every* call —
    ///   including one whose predicate is already satisfied — and re-invoked every
    ///   [`NUDGE_INTERVAL`] while blocked, so a pool whose threads are parked can still be
    ///   nudged into the safepoint before the waiter sleeps and if a nudge is lost to the
    ///   race between the predicate check and the block. An unset waker is a no-op: actively
    ///   looping managed threads reach the safepoint on their own, without one.
    /// * Never changes [`ThreadProgress::generation`]. Never panics.
    pub fn wait(&self, generation: u64) {
        let caller = current_slot(self.inner.instance);
        loop {
            // Nudge before checking/blocking: a thread that had already parked must be
            // released to confirm progress, and doing it before the predicate check means a
            // `wait` that has to block has always nudged at least once.
            self.nudge();

            let mut state = self.inner.state.lock();
            if self.reached(&state, generation, caller) {
                return;
            }
            // The lock is released while sleeping, and a timeout (rather than an unbounded
            // sleep) re-runs the now-armed nudge on the next iteration.
            let _ = self.inner.updated.wait_for(&mut state, NUDGE_INTERVAL);
        }
    }

    /// `bump` then `wait`: advances the generation, waits for every active managed thread to
    /// reach it, and returns the generation of the safepoint.
    ///
    /// This is the one-call safepoint used before an operation that needs the guarantee
    /// ("every thread has passed the point I am about to publish/retire").
    pub fn safepoint(&self) -> u64 {
        let generation = self.bump();
        self.wait(generation);
        generation
    }

    /// Registers the calling thread as a managed (progress-reporting) thread.
    ///
    /// The new thread starts at progress 0 and **active**: an active thread must reach every
    /// safepoint it is asked for, so a thread that is about to park calls
    /// [`ManagedThread::set_active`]`(false)`. There is no unregister in this milestone —
    /// registrations live as long as the barrier (the C unregisters at shutdown).
    pub fn managed_thread(&self) -> ManagedThread {
        let mut state = self.inner.state.lock();
        let id = state.slots.len();
        let slot = Arc::new(Slot {
            id,
            progress: AtomicU64::new(0),
            active: AtomicBool::new(true),
        });
        state.slots.push(Arc::clone(&slot));
        drop(state);
        ManagedThread {
            inner: Arc::clone(&self.inner),
            slot,
        }
    }

    /// The number of registered managed threads (active or not).
    pub fn managed_count(&self) -> usize {
        self.inner.state.lock().slots.len()
    }

    /// Registers the closure [`ThreadProgress::wait`] calls to nudge parked managed threads.
    ///
    /// The waker is invoked outside every lock, at least once before the waiter blocks and
    /// again on each [`NUDGE_INTERVAL`] timeout. Calling this again replaces the waker.
    pub fn set_waker(&self, waker: Arc<dyn Fn() + Send + Sync + 'static>) {
        *self.inner.waker.lock() = Some(waker);
    }

    /// Whether thread progress is currently blocked ([`ThreadProgress::block`] without a
    /// matching [`ThreadProgress::unblock`]).
    ///
    /// Mirrors `erts_thr_progress_is_blocking()`: while this is true a managed thread must
    /// not pass the safepoint it is being held at.
    pub fn is_blocking(&self) -> bool {
        self.blocking_count() > 0
    }

    /// Blocks thread progress. Nestable — each call must be matched by one
    /// [`ThreadProgress::unblock`].
    pub fn block(&self) {
        let mut state = self.inner.state.lock();
        state.blocking = state.blocking.saturating_add(1);
    }

    /// Releases one [`ThreadProgress::block`]. An unbalanced call is a no-op (the counter
    /// saturates at zero instead of underflowing).
    pub fn unblock(&self) {
        let mut state = self.inner.state.lock();
        state.blocking = state.blocking.saturating_sub(1);
    }

    /// The number of nested [`ThreadProgress::block`] calls outstanding (diagnostic).
    pub fn blocking_count(&self) -> usize {
        self.inner.state.lock().blocking
    }

    /// Whether every active managed thread (other than `caller`) has reached `generation`.
    fn reached(&self, state: &State, generation: u64, caller: Option<usize>) -> bool {
        state.slots.iter().all(|slot| {
            if Some(slot.id) == caller {
                // Never wait for the calling thread itself (it cannot make progress while
                // blocked) — the C's WAITING self-exclusion.
                return true;
            }
            if !slot.active.load(Ordering::Acquire) {
                // An inactive (idle/parked) thread is already at a safepoint.
                return true;
            }
            slot.progress.load(Ordering::Acquire) >= generation
        })
    }

    /// Invokes the registered waker, if any. Called without holding any lock, so a waker may
    /// itself touch the barrier.
    fn nudge(&self) {
        let waker = self.inner.waker.lock().clone();
        if let Some(waker) = waker {
            waker();
        }
    }
}

impl Clone for ThreadProgress {
    /// A clone of the handle to the *same* barrier: generation, managed-thread registry,
    /// waker and blocking counter are shared.
    fn clone(&self) -> ThreadProgress {
        ThreadProgress {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for ThreadProgress {
    fn default() -> ThreadProgress {
        ThreadProgress::new()
    }
}

impl fmt::Debug for ThreadProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.inner.state.lock();
        f.debug_struct("ThreadProgress")
            .field("generation", &self.inner.generation.load(Ordering::Acquire))
            .field("managed_threads", &state.slots.len())
            .field(
                "active_threads",
                &state
                    .slots
                    .iter()
                    .filter(|slot| slot.active.load(Ordering::Acquire))
                    .count(),
            )
            .field("blocking", &state.blocking)
            .finish()
    }
}

/// A handle to one managed thread's progress slot, returned by
/// [`ThreadProgress::managed_thread`].
///
/// Send it to (or make it on) the thread that will report progress. Every method takes
/// `&self`: the slot is atomic, so no `&mut` handle is needed.
pub struct ManagedThread {
    /// The shared barrier.
    inner: Arc<Inner>,
    /// This thread's slot.
    slot: Arc<Slot>,
}

impl ManagedThread {
    /// The generation this thread last confirmed (0 before the first
    /// [`ManagedThread::update`]).
    pub fn progress(&self) -> u64 {
        self.slot.progress.load(Ordering::Acquire)
    }

    /// Confirms progress: stores the current global generation into this thread's slot and
    /// wakes every waiter.
    ///
    /// A managed thread must call this frequently (between process slices, when parking and
    /// when waking) — it is the only way a waiter's predicate can become true. The generation
    /// is read *before* the store, so a concurrent [`ThreadProgress::bump`] may make this
    /// confirmation insufficient for the new generation; the next `update` covers it.
    ///
    /// Also binds the calling OS thread to this slot, which is how a later
    /// [`ThreadProgress::wait`] on that thread recognizes its own slot.
    pub fn update(&self) {
        bind_current(self.inner.instance, self.slot.id);
        let generation = self.inner.generation.load(Ordering::Acquire);
        self.slot.progress.store(generation, Ordering::Release);
        // Taking the state lock before notifying serializes with a waiter's predicate check,
        // so a progress confirmation can never be lost between that check and the block.
        let _guard = self.inner.state.lock();
        self.inner.updated.notify_all();
    }

    /// Whether this thread still has to reach a safepoint.
    ///
    /// An active thread is waited for; an inactive (parked) one is not, because parking
    /// happens at a safepoint.
    pub fn is_active(&self) -> bool {
        self.slot.active.load(Ordering::Acquire)
    }

    /// Marks this thread active (`on == true`) or idle/parked (`on == false`).
    ///
    /// Either direction changes a waiter's predicate, so waiters are woken. Binding the
    /// calling OS thread to this slot happens here as well as in [`ManagedThread::update`].
    pub fn set_active(&self, on: bool) {
        bind_current(self.inner.instance, self.slot.id);
        self.slot.active.store(on, Ordering::Release);
        let _guard = self.inner.state.lock();
        self.inner.updated.notify_all();
    }

    /// This thread's stable registration index (its position in the registry).
    pub fn id(&self) -> usize {
        self.slot.id
    }
}

impl fmt::Debug for ManagedThread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagedThread")
            .field("id", &self.slot.id)
            .field("progress", &self.progress())
            .field("active", &self.is_active())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the generation counter, the registry bookkeeping and the degenerate
    //! (no managed thread / not the caller's slot) waits. The real multi-threaded protocol
    //! tests live in `tests/progress.rs`.

    use super::*;

    #[test]
    fn generation_starts_at_zero_and_bumps_monotonically() {
        let progress = ThreadProgress::new();
        assert_eq!(progress.generation(), 0);
        assert_eq!(progress.bump(), 1);
        assert_eq!(progress.generation(), 1);
        assert_eq!(progress.bump(), 2);
        assert_eq!(progress.generation(), 2);
    }

    #[test]
    fn wait_is_a_no_op_for_any_generation() {
        let progress = ThreadProgress::new();
        progress.wait(0);
        progress.wait(7);
        assert_eq!(
            progress.generation(),
            0,
            "waiting never advances the generation"
        );
    }

    #[test]
    fn the_barrier_and_its_handles_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ThreadProgress>();
        assert_send_sync::<ManagedThread>();
    }

    #[test]
    fn a_clone_shares_the_state() {
        let progress = ThreadProgress::new();
        let clone = progress.clone();
        assert_eq!(clone.bump(), 1);
        assert_eq!(
            progress.generation(),
            1,
            "the clone bumps the same generation"
        );

        let managed = progress.managed_thread();
        assert_eq!(clone.managed_count(), 1, "the clone sees the same registry");
        assert_eq!(managed.id(), 0);
        assert_eq!(managed.progress(), 0);
        assert!(managed.is_active(), "a new managed thread is active");

        progress.block();
        clone.block();
        assert!(clone.is_blocking());
        assert_eq!(progress.blocking_count(), 2);
    }

    #[test]
    fn safepoint_without_managed_threads_only_bumps() {
        let progress = ThreadProgress::new();
        assert_eq!(progress.safepoint(), 1);
        assert_eq!(progress.safepoint(), 2);
        assert_eq!(progress.generation(), 2);
    }
}
