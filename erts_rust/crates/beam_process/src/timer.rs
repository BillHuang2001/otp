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

//! Timer service — the runtime's high-level timers (`erlang:send_after/3`,
//! `erlang:start_timer/3` and `receive ... after T`).
//!
//! A [`TimerService`] is a process-wide wheel of pending timers driven by **one** background
//! thread. A timer is scheduled with [`TimerService::send_after`] (a monotonic deadline of
//! `now + delay`, a target process and a message), returns an opaque [`TimerRef`] used to
//! [`TimerService::cancel`] it, and — unless it is cancelled first — is handed exactly once,
//! in deadline order, to the [`TimerSink`] the service was started with. The scheduler pool
//! supplies that sink (it delivers the message into the target process and wakes a
//! scheduler); this crate owns only the bookkeeping and the timing.
//!
//! # Design: a deadline min-heap (not a hashed wheel)
//!
//! The legacy C (`erts/emulator/beam/erl_hl_timer.c`) spreads timers over a multi-level
//! hashed timer wheel because it must service millions of timers on many schedulers with
//! `O(1)` insert/cancel. This crate's Milestone-1 service is a single-threaded `BinaryHeap`
//! ordered by `(deadline, insertion sequence)`. That keeps the **observable** semantics
//! identical — deadlines are monotonic, and a timer with an earlier deadline (or, for equal
//! deadlines, an earlier insertion) fires first; the C's wheel is FIFO within a slot, so
//! equal-deadline timers also fire in insertion order — while being far simpler and
//! `O(log n)` per operation. Cancellation is `O(1)` (a `HashSet` of live refs) and leaves a
//! lazily discarded heap tombstone.
//!
//! # The timer thread and its wake strategy
//!
//! [`TimerService::start`] spawns one thread that blocks on a `parking_lot` condvar until the
//! earliest deadline — or indefinitely, if none is pending — **never** a busy loop. A
//! `send_after` for an earlier deadline and a [`TimerService::shutdown`] both notify the
//! condvar, so the thread re-evaluates its sleep immediately. Firing happens **outside** the
//! service lock, so a slow sink cannot stall a concurrent `send_after`/`cancel`; because a
//! single thread performs the pops in heap order, the deadline-order guarantee cannot be
//! violated by a scheduling race.
//!
//! # Message contract (thread validity)
//!
//! The stored message is a bare [`Eterm`] machine word (`Copy`). It is copied by value onto
//! the timer thread and later handed to the sink running on that thread — the service never
//! dereferences it. A message is therefore only sound if it is **valid on any thread**: an
//! immediate (a small int, an atom, a pid) or a literal-area/interned term that lives as long
//! as the runtime does. A term resident in a per-process heap is *not* valid here and would
//! need an off-heap handle (a documented follow-up); passing one is a caller contract
//! violation, not something this crate can detect.
//!
//! # Lifecycle
//!
//! * Timers may be scheduled **before** [`TimerService::start`]; they stay pending and fire
//!   once the thread starts.
//! * [`TimerService::start`] is idempotent — a second call is a no-op.
//! * [`TimerService::shutdown`] is **terminal**: it clears every pending timer, bumps the
//!   service generation (so every outstanding [`TimerRef`] can no longer match) and wakes the
//!   thread to exit; [`TimerService::join`] blocks until it has. A shut-down service fires
//!   nothing scheduled afterwards.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_hl_timer.{c,h}`.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use beam_term::prelude::Eterm;
use parking_lot::{Condvar, Mutex};

use crate::pid::ProcessId;

/// Name given to the timer thread, for `ps`/debugging visibility.
const TIMER_THREAD_NAME: &str = "beam-timer";

/// Fallback offset (~68 years) used when `now + delay` would overflow an [`Instant`].
///
/// Timers are far-from-infinite in practice; saturating here keeps [`TimerService::send_after`]
/// total for an absurd `Duration` instead of panicking on arithmetic overflow.
const SATURATING_DELAY: Duration = Duration::from_secs(1 << 31);

/// An opaque handle to a scheduled timer, returned by [`TimerService::send_after`].
///
/// The handle carries both the timer's unique `id` (a monotonic counter that is never reused)
/// and the service `generation` that was current when it was scheduled. A stale handle can
/// therefore never match a fresh timer: after a [`TimerService::shutdown`] bumps the
/// generation, an old reference matches nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerRef {
    /// Unique, monotonically increasing timer id (never reused within a service's lifetime).
    id: u64,
    /// The service generation at scheduling time (bumped by [`TimerService::shutdown`]).
    generation: u64,
}

impl TimerRef {
    /// The timer's unique id (monotonic and never reused).
    pub const fn id(self) -> u64 {
        self.id
    }

    /// The service generation the timer was scheduled in.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// What a fired timer does — implemented by the scheduler pool, which delivers `message` into
/// the target process and wakes a scheduler.
///
/// The sink is shared with the timer thread, so it must be `Send + Sync`.
pub trait TimerSink: Send + Sync {
    /// Called **exactly once** per successfully scheduled (and not cancelled) timer, in
    /// deadline order. `message` is the `Eterm` passed to [`TimerService::send_after`]; it is
    /// valid on any thread (see the module-level message contract).
    fn timer_fired(&self, target: ProcessId, message: Eterm);
}

/// A pending timer in the service heap.
struct HeapEntry {
    /// Absolute monotonic deadline.
    deadline: Instant,
    /// Insertion sequence — the tie-break that gives equal deadlines FIFO order.
    seq: u64,
    /// The handle identifying this timer (also the key in the live set).
    reference: TimerRef,
    /// The process to deliver `message` to.
    target: ProcessId,
    /// The message to deliver.
    message: Eterm,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reversed so that `BinaryHeap` (a max-heap) yields the *earliest* deadline first,
        // and — for equal deadlines — the *earliest* insertion first.
        other
            .deadline
            .cmp(&self.deadline)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}

impl Eq for HeapEntry {}

/// The mutable timer-wheel state, guarded by the service [`Mutex`].
#[derive(Default)]
struct State {
    /// Pending timers, a min-heap by `(deadline, seq)`.
    heap: BinaryHeap<HeapEntry>,
    /// The refs that are still live (scheduled and not cancelled/fired), for `O(1)` cancel.
    live: HashSet<TimerRef>,
    /// Number of live timers (scheduled and not yet fired or cancelled).
    pending: usize,
    /// Monotonic source of unique timer ids.
    next_id: u64,
    /// Monotonic source of insertion sequence numbers (the heap tie-break).
    next_seq: u64,
    /// The current service generation (bumped by `shutdown`).
    generation: u64,
    /// Whether the timer thread has been started.
    started: bool,
    /// Whether the service has been shut down (terminal).
    shutdown: bool,
}

impl State {
    /// Drops cancelled tombstones sitting at the head of the heap.
    ///
    /// Cancellation is lazy: [`TimerService::cancel`] removes the ref from `live` but leaves
    /// its heap entry in place. Any tombstone deeper than a live entry can never be the
    /// minimum, so popping only the cancelled heads is enough to expose the earliest live
    /// deadline.
    fn prune_cancelled(&mut self) {
        loop {
            let cancelled_top = match self.heap.peek() {
                Some(top) => !self.live.contains(&top.reference),
                None => false,
            };
            if !cancelled_top {
                break;
            }
            self.heap.pop();
        }
    }
}

/// The shared, `Arc`-cloned service state (the public [`TimerService`] also holds the thread
/// handle, which is deliberately kept *out* of here to avoid a reference cycle with the
/// thread's own `Arc<Inner>`).
struct Inner {
    /// The guarded wheel state.
    state: Mutex<State>,
    /// Signalled whenever the earliest deadline, the pending set or the shutdown flag change.
    cv: Condvar,
}

impl Inner {
    /// Blocks until one timer is due, then returns `(target, message)` for it.
    ///
    /// Returns `None` once the service has been shut down. Spurious wakeups and races with
    /// concurrent `send_after`/`cancel` are absorbed by the surrounding loop.
    fn wait_next(&self) -> Option<(ProcessId, Eterm)> {
        // What to do after inspecting the head of the heap.
        enum Next {
            /// Nothing pending: block until notified.
            Wait,
            /// Block for at most this long, then re-check.
            WaitFor(Duration),
            /// The head is due: pop and fire it.
            Fire,
        }

        let mut state = self.state.lock();
        loop {
            if state.shutdown {
                return None;
            }
            state.prune_cancelled();

            let now = Instant::now();
            let next = match state.heap.peek() {
                None => Next::Wait,
                Some(top) if top.deadline <= now => Next::Fire,
                Some(top) => Next::WaitFor(top.deadline.saturating_duration_since(now)),
            };

            match next {
                Next::Wait => self.cv.wait(&mut state),
                Next::WaitFor(timeout) => {
                    let _ = self.cv.wait_for(&mut state, timeout);
                }
                Next::Fire => {
                    let entry = state.heap.pop().expect("peek() saw an entry");
                    state.live.remove(&entry.reference);
                    state.pending = state.pending.saturating_sub(1);
                    // Fire outside the lock: a slow sink must not stall scheduling.
                    return Some((entry.target, entry.message));
                }
            }
        }
    }
}

/// The runtime-wide timer service.
///
/// Construct with [`TimerService::new`], start it once with [`TimerService::start`] (passing
/// the [`TimerSink`] the scheduler pool owns), then schedule with
/// [`TimerService::send_after`]. See the module documentation for the design and lifecycle.
pub struct TimerService {
    /// State shared with the timer thread.
    inner: Arc<Inner>,
    /// The timer thread's handle, taken by [`TimerService::join`].
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl TimerService {
    /// Creates a timer service that is **constructed but not yet running**.
    ///
    /// Timers may be scheduled immediately; they fire once [`TimerService::start`] is called.
    pub fn new() -> TimerService {
        TimerService {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                cv: Condvar::new(),
            }),
            thread: Mutex::new(None),
        }
    }

    /// Spawns the timer thread, driving every pending and future timer into `sink`.
    ///
    /// Idempotent: a second call (or a call after [`TimerService::shutdown`]) is a no-op.
    /// Timers scheduled before this call are picked up by the new thread.
    pub fn start(self: &Arc<Self>, sink: Arc<dyn TimerSink>) {
        {
            let mut state = self.inner.state.lock();
            if state.started || state.shutdown {
                return;
            }
            state.started = true;
        }

        let inner = Arc::clone(&self.inner);
        let handle = std::thread::Builder::new()
            .name(TIMER_THREAD_NAME.to_string())
            .spawn(move || {
                while let Some((target, message)) = inner.wait_next() {
                    sink.timer_fired(target, message);
                }
            })
            .expect("failed to spawn the timer thread");

        *self.thread.lock() = Some(handle);
    }

    /// Schedules `message` for delivery to `target` after `delay`.
    ///
    /// The deadline is monotonic (`Instant::now() + delay`); a zero `delay` fires on the next
    /// tick. Returns the [`TimerRef`] needed to [`TimerService::cancel`] it. May be called
    /// before [`TimerService::start`] (the timer then fires once the thread starts); a
    /// shut-down service accepts the call but never fires it.
    pub fn send_after(&self, delay: Duration, target: ProcessId, message: Eterm) -> TimerRef {
        let mut state = self.inner.state.lock();

        let reference = TimerRef {
            id: state.next_id,
            generation: state.generation,
        };
        state.next_id = state.next_id.wrapping_add(1);
        let seq = state.next_seq;
        state.next_seq = state.next_seq.wrapping_add(1);

        let deadline = deadline_after(Instant::now(), delay);
        state.heap.push(HeapEntry {
            deadline,
            seq,
            reference,
            target,
            message,
        });
        state.live.insert(reference);
        state.pending += 1;

        // Wake the timer thread if it is sleeping for a later deadline (or forever).
        self.inner.cv.notify_one();
        reference
    }

    /// Cancels a scheduled timer, reporting whether it was still live.
    ///
    /// Returns `true` exactly once per successfully scheduled timer (if cancelled before it
    /// fires); `false` for an unknown, already-cancelled or already-fired reference.
    pub fn cancel(&self, timer: TimerRef) -> bool {
        let mut state = self.inner.state.lock();
        if state.live.remove(&timer) {
            state.pending = state.pending.saturating_sub(1);
            true
        } else {
            false
        }
    }

    /// The number of timers currently scheduled and not yet fired or cancelled.
    pub fn pending(&self) -> usize {
        self.inner.state.lock().pending
    }

    /// The earliest live deadline, or `None` when no timer is pending.
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut state = self.inner.state.lock();
        state.prune_cancelled();
        state.heap.peek().map(|entry| entry.deadline)
    }

    /// Whether the timer thread is currently running (started and not yet shut down).
    pub fn is_running(&self) -> bool {
        let state = self.inner.state.lock();
        state.started && !state.shutdown
    }

    /// Asks the timer thread to exit and drops every pending timer.
    ///
    /// Terminal and idempotent: after this call the service fires nothing, `pending()` is
    /// zero and [is_running](TimerService::is_running) is `false`. The thread exits
    /// asynchronously; call [`TimerService::join`] to wait for it.
    pub fn shutdown(&self) {
        let mut state = self.inner.state.lock();
        state.shutdown = true;
        state.generation = state.generation.wrapping_add(1);
        state.heap.clear();
        state.live.clear();
        state.pending = 0;
        self.inner.cv.notify_all();
    }

    /// Blocks until the timer thread has exited.
    ///
    /// Returns immediately if the service was never started or the thread has already joined.
    /// Call it after [`TimerService::shutdown`]; calling it on a running, non-shut-down
    /// service blocks until that service is shut down from another thread.
    pub fn join(&self) {
        let handle = self.thread.lock().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

impl Default for TimerService {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TimerService {
    fn drop(&mut self) {
        // Hygiene: never leave the timer thread blocked forever. We deliberately do not join
        // here (a `join` from the timer thread's own sink would deadlock); the thread sees the
        // shutdown flag and exits on its own.
        self.shutdown();
    }
}

/// The absolute deadline for `now + delay`, saturating rather than overflowing.
fn deadline_after(now: Instant, delay: Duration) -> Instant {
    now.checked_add(delay)
        .unwrap_or_else(|| now + SATURATING_DELAY)
}

#[cfg(test)]
mod tests {
    //! Unit tests for the pieces that need no timer thread and no sleeping: reference
    //! identity/generation, the heap's `(deadline, sequence)` ordering, and the pending /
    //! cancel bookkeeping (scheduled with a far-future deadline so nothing ever fires).

    use super::*;
    use beam_term::prelude::make_small;

    /// A sink that ignores every timer (only used to start/stop the thread).
    struct NullSink;

    impl TimerSink for NullSink {
        fn timer_fired(&self, _target: ProcessId, _message: Eterm) {}
    }

    fn pid() -> ProcessId {
        ProcessId::new(1, 0)
    }

    #[test]
    fn timer_refs_are_unique_and_share_a_generation() {
        let svc = TimerService::new();
        let a = svc.send_after(Duration::from_secs(3600), pid(), make_small(1));
        let b = svc.send_after(Duration::from_secs(3600), pid(), make_small(2));
        assert_ne!(a, b, "every scheduled timer gets a distinct handle");
        assert_ne!(a.id(), b.id(), "ids are unique");
        assert_eq!(
            a.generation(),
            b.generation(),
            "same (pre-shutdown) generation"
        );
        assert_eq!(a.id(), a.id());
        assert!(svc.cancel(a));
        assert!(!svc.cancel(a), "an already-cancelled ref is not live");
    }

    #[test]
    fn unknown_and_stale_refs_never_cancel() {
        let svc = TimerService::new();
        let bogus = TimerRef {
            id: 424_242,
            generation: 0,
        };
        assert!(!svc.cancel(bogus), "an unknown ref cancels nothing");

        // A ref from a previous generation never matches, even with a colliding id.
        let live = svc.send_after(Duration::from_secs(3600), pid(), make_small(1));
        let stale = TimerRef {
            id: live.id(),
            generation: live.generation().wrapping_add(1),
        };
        assert!(!svc.cancel(stale), "a stale generation never matches");
        assert_eq!(svc.pending(), 1, "the live timer is untouched");
        assert!(svc.cancel(live));
    }

    #[test]
    fn heap_orders_by_deadline_then_insertion_sequence() {
        let base = Instant::now();
        let entry = |millis: u64, seq: u64| HeapEntry {
            deadline: base + Duration::from_millis(millis),
            seq,
            reference: TimerRef {
                id: seq,
                generation: 0,
            },
            target: ProcessId::new(seq, 0),
            message: make_small(seq as i64),
        };

        let mut heap = BinaryHeap::new();
        heap.push(entry(10, 5)); // later insertion at 10 ms
        heap.push(entry(5, 9)); // earliest deadline, latest sequence
        heap.push(entry(10, 1)); // equal deadline, earlier sequence
        heap.push(entry(20, 0));

        let order: Vec<u64> = std::iter::from_fn(|| heap.pop().map(|e| e.seq)).collect();
        assert_eq!(
            order,
            vec![9, 1, 5, 0],
            "earliest deadline first; equal deadlines in insertion order"
        );
    }

    #[test]
    fn pending_and_next_deadline_track_schedule_and_cancel() {
        let svc = TimerService::new();
        assert_eq!(svc.pending(), 0);
        assert_eq!(svc.next_deadline(), None);

        let far = svc.send_after(Duration::from_secs(3600), pid(), make_small(1));
        let near = svc.send_after(Duration::from_secs(1800), pid(), make_small(2));
        assert_eq!(svc.pending(), 2);
        assert!(svc.next_deadline().is_some());

        assert!(svc.cancel(near));
        assert_eq!(svc.pending(), 1);
        // The earliest live deadline is the far one now that `near` is gone.
        assert!(svc.next_deadline().is_some());

        assert!(svc.cancel(far));
        assert_eq!(svc.pending(), 0);
        assert_eq!(svc.next_deadline(), None);
    }

    #[test]
    fn scheduling_before_start_is_allowed_and_cancellable() {
        let svc = TimerService::new();
        assert!(!svc.is_running());
        let timer = svc.send_after(Duration::from_millis(1), pid(), make_small(7));
        assert_eq!(svc.pending(), 1);
        assert!(svc.next_deadline().is_some());
        assert!(svc.cancel(timer));
        assert_eq!(svc.pending(), 0);
    }

    #[test]
    fn start_is_idempotent_and_shutdown_is_terminal() {
        let svc = Arc::new(TimerService::new());
        assert!(!svc.is_running());

        svc.start(Arc::new(NullSink));
        assert!(svc.is_running());
        svc.start(Arc::new(NullSink)); // no-op
        assert!(svc.is_running());

        svc.shutdown();
        assert!(!svc.is_running());
        svc.join();

        svc.start(Arc::new(NullSink)); // terminal: no-op
        assert!(!svc.is_running());
    }

    #[test]
    fn shutdown_drops_pending_and_join_is_safe_without_start() {
        let svc = TimerService::new();
        let _timer = svc.send_after(Duration::from_secs(3600), pid(), make_small(1));
        assert_eq!(svc.pending(), 1);
        svc.shutdown();
        assert_eq!(svc.pending(), 0, "shutdown drops pending timers");
        assert_eq!(svc.next_deadline(), None);
        svc.join(); // never started: returns immediately
    }

    #[test]
    fn huge_delay_saturates_instead_of_overflowing() {
        // Must not panic on an unrepresentable `now + delay`.
        let svc = TimerService::new();
        let timer = svc.send_after(Duration::MAX, pid(), make_small(1));
        assert_eq!(svc.pending(), 1);
        assert!(svc.next_deadline().is_some());
        assert!(svc.cancel(timer));
    }
}
