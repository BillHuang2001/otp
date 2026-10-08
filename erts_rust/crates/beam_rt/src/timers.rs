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

//! Runtime-local timer deadlines — the queue behind `erlang:send_after/3` and a finite
//! `receive … after T`.
//!
//! A [`Timers`] value is a **min-heap of absolute deadlines** owned by the
//! [`Runtime`](crate::Runtime) and advanced **only** by the scheduler loop
//! ([`Runtime::run_scheduler`](crate::Runtime::run_scheduler) and friends, see
//! [`scheduler_pass`](crate::runtime)): the loop fires every deadline that has passed and,
//! when nothing else can run, sleeps until the earliest one — exactly the way the same loop
//! clamps its wait to the port poller's next deadline.
//!
//! A deadline carries a [`TimerAction`]: a `send_after` message to deliver, or a
//! **receive timeout** that resumes a parked process at its `after` clause with no message.
//!
//! # Why a runtime-local heap and not `beam_process::TimerService`
//!
//! `beam_process` already ships a timer wheel, but it is driven by **one background
//! thread** whose [`TimerSink`](beam_process::TimerSink) must be `Send + Sync`. `Runtime`
//! is not `Send` — it owns `Rc`/`RefCell` tables, a `dlopen`ed NIF registry, and every term
//! it hands out lives in a pointer-stable arena — so no thread can hold a sink into it. The
//! runtime is single-threaded by construction, so the wheel degenerates to a plain heap that
//! the scheduler itself drains; the **observable** order and cancellation semantics are the
//! ones that service documents: deadlines are absolute and monotonic, an earlier deadline
//! fires first, equal deadlines fire in insertion order, and cancellation is `O(1)` with a
//! lazily discarded heap tombstone.
//!
//! Legacy C ground truth (read-only): `erts/emulator/beam/erl_bif_timer.c` (the
//! `send_after`/`cancel_timer` semantics) and `erl_hl_timer.c` (the wheel).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::time::{Duration, Instant};

use beam_process::ProcessId;
use beam_term::prelude::{is_atom, is_small, signed_value, atom_val, Atom, Eterm};

/// Fallback offset (~68 years) used when `now + delay` would overflow an [`Instant`].
///
/// The legacy C saturates an out-of-range timer time the same way. Without it a caller's
/// absurd delay would panic on the arithmetic instead of degrading to "fires very far in
/// the future" (`beam_process::timer` pins the same property for its own wheel).
const SATURATING_DELAY: Duration = Duration::from_secs(1 << 31);

/// Where a fired timer delivers its message.
///
/// A **pid** is resolved when the timer is scheduled (the oracle's `send_after` accepts a
/// pid, which pins the recipient even if the pid dies — a dead pid simply receives
/// nothing); a **registered name** is resolved when the timer *fires*, so a name registered
/// in the meantime is found, exactly as the oracle's name lookup behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimerTarget {
    /// Deliver to this pid.
    Pid(ProcessId),
    /// Deliver to whatever process holds this registered name at fire time.
    Name(Atom),
}

/// The delay of a timer BIF: a non-negative integer of milliseconds that a monotonic
/// deadline can actually represent (the oracle's `parse_timeout_pos` validation,
/// `erl_hl_timer.c:2567`).
///
/// Shared by **every** timer entry point — the runtime-native
/// `erlang:send_after/3`/[`cancel_timer/1`](crate::native) and the [`RuntimeHooks`] timer
/// family (`erlang:start_timer/3,4`, `send_after/4`, `cancel_timer/2`) — so the validation
/// of the time argument can never drift between them.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-integer, a negative integer, or an integer whose
/// deadline would overflow the runtime's monotonic clock — the oracle's `badarg` (its
/// time validation fails too) rather than a silently saturated deadline that would fire at
/// the wrong moment.
///
/// [`RuntimeHooks`]: beam_bifs::RuntimeHooks
pub(crate) fn timer_delay(term: Eterm) -> beam_common::Result<Duration> {
    if !is_small(term) {
        return Err(beam_common::BeamError::BadArg);
    }
    let millis = u64::try_from(signed_value(term))
        .map_err(|_| beam_common::BeamError::BadArg)?;
    let delay = Duration::from_millis(millis);
    if Instant::now().checked_add(delay).is_none() {
        return Err(beam_common::BeamError::BadArg);
    }
    Ok(delay)
}

/// The destination of a timer: a pid (pinned now) or a registered name (resolved when the
/// timer fires).
///
/// Shared with the [`RuntimeHooks`] timer family for the same reason as [`timer_delay`].
///
/// # Errors
///
/// [`BeamError::BadArg`] for anything but a pid or an atom — a port, a `{Name, Node}` tuple
/// (the C's `is_not_internal_pid(rcvr) && is_not_atom(rcvr)` check, `erl_hl_timer.c:1799`),
/// or any other term.
///
/// [`RuntimeHooks`]: beam_bifs::RuntimeHooks
pub(crate) fn timer_target(term: Eterm) -> beam_common::Result<TimerTarget> {
    if let Ok(pid) = ProcessId::from_eterm(term) {
        return Ok(TimerTarget::Pid(pid));
    }
    if is_atom(term) {
        return Ok(TimerTarget::Name(atom_val(term)));
    }
    Err(beam_common::BeamError::BadArg)
}

/// What a timer does when its deadline passes.
///
/// The heap carries both runtime deadlines in one structure because they are the same
/// thing — an absolute time at which a blocked process becomes runnable:
///
/// * [`TimerAction::Deliver`] — the `erlang:send_after/3` message, delivered into the
///   target's mailbox (and the target's own `receive` scan decides whether it fits);
/// * [`TimerAction::Resume`] — a finite `receive … after T` deadline: the parked process
///   resumes at the **timed-out** path with **no** message delivered, so the `after`
///   clause runs (C `wait.execute`: on timeout "control will be transferred to the
///   `timeout` instruction following the `wait_timeout` instruction",
///   `../../erts/emulator/beam/emu/msg_instrs.tab:356-366`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum TimerAction {
    /// Deliver `message` to `target`.
    Deliver {
        /// The process (or registered name) to deliver to.
        target: TimerTarget,
        /// The sender handed to the delivery (the process that scheduled the timer).
        from: Eterm,
        /// The message to deliver.
        message: Eterm,
    },
    /// Resume the process parked in a `receive` with a finite timeout.
    Resume {
        /// The parked process.
        pid: ProcessId,
        /// The function index of the timed-out path to resume at.
        func: u32,
        /// The code offset of the timed-out path to resume at.
        pc: u32,
    },
}

/// One timer that has reached its deadline and left the heap.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FiredTimer {
    /// What the deadline requires of the scheduler.
    pub(crate) action: TimerAction,
}

/// An opaque identity for a pending timer within one [`Timers`] heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct TimerId(u64);

/// A pending timer in the service heap.
#[derive(Debug, Clone, Copy)]
struct HeapEntry {
    /// Absolute monotonic deadline.
    deadline: Instant,
    /// Insertion sequence — the tie-break that gives equal deadlines FIFO order.
    seq: u64,
    /// The identity of the timer (also the key in the pending map).
    id: TimerId,
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

/// A live timer's payload, keyed by its [`TimerId`].
struct Pending {
    /// The absolute deadline (the heap entry's key, kept here too so a cancellation can
    /// answer the remaining time without scanning the heap).
    deadline: Instant,
    /// What the deadline requires of the scheduler.
    action: TimerAction,
    /// The `#Ref<>` term `erlang:send_after/3` returned to the caller — the handle
    /// `erlang:cancel_timer/1` takes, and the reason a fired reference is still recognised
    /// as a timer reference (see [`Timers::cancel`]).
    ///
    /// `None` for a runtime-internal deadline (a receive timeout) that no caller-visible
    /// term addresses: it is cancelled by its [`TimerHandle`], never by `cancel_timer/1`.
    reference: Option<Eterm>,
}

/// A handle to a runtime-internal deadline that no `#Ref<>` term addresses — a receiver's
/// finite `receive … after T` timeout.
///
/// [`Timers::schedule_resume`] mints one and [`Timers::cancel_handle`] consumes it when the
/// receiver is woken by a real message first (so the deadline cannot also fire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TimerHandle(u64);

/// The runtime's pending timer deadlines.
///
/// See the module documentation; the only consumer is the scheduler loop in
/// [`crate::runtime`].
#[derive(Default)]
pub(crate) struct Timers {
    /// Pending timers, a min-heap by `(deadline, seq)` (with cancelled tombstones).
    heap: BinaryHeap<HeapEntry>,
    /// The live timers, keyed by id.
    pending: HashMap<TimerId, Pending>,
    /// Every timer **reference** this heap minted, mapped to the timer it identified:
    /// `Some(id)` while it is pending, `None` once it has fired or been cancelled.
    ///
    /// This is what makes `erlang:cancel_timer/1`'s three answers exact: an unknown term is
    /// not a timer reference at all, a `Some` one is cancellable (its remaining time is
    /// answered), and a `None` one already fired (the oracle's `false`).
    refs: HashMap<Eterm, Option<TimerId>>,
    /// Monotonic source of unique timer ids.
    next_id: u64,
    /// Monotonic source of insertion sequence numbers (the heap tie-break).
    next_seq: u64,
}

impl Timers {
    /// An empty deadline heap.
    pub(crate) fn new() -> Timers {
        Timers::default()
    }

    /// Schedules `message` for delivery to `target` at `delay` from now, returning nothing:
    /// the caller's handle is `reference` (the `#Ref<>` term `send_after/3` returns).
    ///
    /// `reference` is recorded as pending, so a later [`Timers::cancel`] of it succeeds.
    pub(crate) fn schedule(
        &mut self,
        delay: Duration,
        target: TimerTarget,
        from: Eterm,
        message: Eterm,
        reference: Eterm,
    ) {
        let deadline = deadline_after(Instant::now(), delay);
        let id = self.insert(
            deadline,
            TimerAction::Deliver {
                target,
                from,
                message,
            },
            Some(reference),
        );
        self.refs.insert(reference, Some(id));
    }

    /// Schedules a **receive timeout**: on expiry the parked process `pid` resumes at
    /// `(func, pc)` — the timed-out path — with no message delivered.
    ///
    /// Returns the [`TimerHandle`] the runtime cancels with [`Timers::cancel_handle`] when a
    /// message wakes the receiver before the deadline (so a receive that gets a message does
    /// not also fire its timeout).
    pub(crate) fn schedule_resume(
        &mut self,
        delay: Duration,
        pid: ProcessId,
        func: u32,
        pc: u32,
    ) -> TimerHandle {
        let deadline = deadline_after(Instant::now(), delay);
        let id = self.insert(deadline, TimerAction::Resume { pid, func, pc }, None);
        TimerHandle(id.0)
    }

    /// Cancels the pending deadline `handle` identifies, answering whether it was still
    /// pending (and therefore will not fire).
    ///
    /// A no-op — `false` — for a deadline that already fired or was already cancelled, so a
    /// receiver that lost the race to its own timeout cancels nothing.
    pub(crate) fn cancel_handle(&mut self, handle: TimerHandle) -> bool {
        // The heap entry stays in place as a tombstone and is dropped lazily.
        self.pending.remove(&TimerId(handle.0)).is_some()
    }

    /// Records one live timer and returns its identity.
    ///
    /// `reference` is the caller-visible `#Ref<>` handle when there is one (an
    /// `erlang:send_after/3` timer), `None` for a runtime-internal deadline; it is the
    /// caller's job to index it in [`Timers::refs`].
    fn insert(&mut self, deadline: Instant, action: TimerAction, reference: Option<Eterm>) -> TimerId {
        let id = TimerId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.heap.push(HeapEntry { deadline, seq, id });
        self.pending.insert(
            id,
            Pending {
                deadline,
                action,
                reference,
            },
        );
        id
    }

    /// Whether `reference` is a timer reference **this** heap minted (pending, fired or
    /// cancelled) — the test `erlang:cancel_timer/1` uses to tell `badarg` (not a reference)
    /// from `false` (a reference that is not cancellable).
    pub(crate) fn is_timer_ref(&self, reference: Eterm) -> bool {
        self.refs.contains_key(&reference)
    }

    /// Cancels the pending timer `reference` identifies, answering how long was left.
    ///
    /// Returns `Some(remaining)` when the timer was still pending (it will not fire), and
    /// `None` for a reference that already fired, was already cancelled, or is not a timer
    /// reference at all (the caller distinguishes the last case with
    /// [`Timers::is_timer_ref`]). The heap entry is left in place as a tombstone and dropped
    /// lazily.
    pub(crate) fn cancel(&mut self, reference: Eterm) -> Option<Duration> {
        let id = match self.refs.get_mut(&reference) {
            Some(slot @ Some(_)) => {
                let id = slot.expect("matched a pending timer");
                *slot = None;
                id
            }
            _ => return None,
        };
        let pending = self.pending.remove(&id).expect("a pending id has a payload");
        // The heap entry stays in place as a tombstone and is dropped lazily.
        Some(pending.deadline.saturating_duration_since(Instant::now()))
    }

    /// The earliest **live** deadline, or `None` when nothing is pending.
    ///
    /// Cancelled tombstones at the head of the heap are dropped on the way, so the answer is
    /// the deadline the scheduler should sleep until.
    pub(crate) fn next_deadline(&mut self) -> Option<Instant> {
        self.prune_cancelled();
        self.heap.peek().map(|entry| entry.deadline)
    }

    /// Removes and returns every timer whose deadline has passed (all of them, in deadline
    /// order), so the scheduler can deliver their messages and wake the receivers.
    pub(crate) fn take_due(&mut self, now: Instant) -> Vec<FiredTimer> {
        self.prune_cancelled();
        let mut fired = Vec::new();
        while let Some(top) = self.heap.peek() {
            if top.deadline > now {
                break;
            }
            let entry = self.heap.pop().expect("peek() saw an entry");
            let Some(pending) = self.pending.remove(&entry.id) else {
                continue; // a tombstone that was not at the head when we pruned
            };
            // A caller-visible reference is no longer cancellable, but it stays *known* so
            // a later `cancel_timer/1` answers the oracle's `false` instead of `badarg`.
            if let Some(reference) = pending.reference {
                self.refs.insert(reference, None);
            }
            fired.push(FiredTimer {
                action: pending.action,
            });
        }
        fired
    }

    /// The number of live (pending, not cancelled) timers.
    ///
    /// Test-only instrumentation: the scheduler loop only ever asks for the *earliest*
    /// deadline (which prunes cancelled tombstones on the way), so nothing in the runtime
    /// needs a count.
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Drops cancelled tombstones sitting at the head of the heap.
    ///
    /// Cancellation is lazy: [`Timers::cancel`] removes the payload but leaves the heap
    /// entry in place. Any tombstone deeper than a live entry can never be the minimum, so
    /// popping only the cancelled heads is enough to expose the earliest live deadline.
    fn prune_cancelled(&mut self) {
        while let Some(top) = self.heap.peek() {
            if self.pending.contains_key(&top.id) {
                break;
            }
            self.heap.pop();
        }
    }
}

/// `now + delay`, saturating instead of panicking when the sum is unrepresentable.
fn deadline_after(now: Instant, delay: Duration) -> Instant {
    now.checked_add(delay)
        .unwrap_or_else(|| now.checked_add(SATURATING_DELAY).unwrap_or(now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{make_small, signed_value};

    fn pid(n: u64) -> ProcessId {
        ProcessId::new(n, 0)
    }

    /// A timer is addressed by the reference term the caller got; any distinct word works
    /// as a stand-in for a real `#Ref<>` here (the heap only hashes it).
    fn reference(n: u64) -> Eterm {
        make_small(-(n as i64) - 1)
    }

    fn schedule(timers: &mut Timers, millis: u64, seq: u64) -> Eterm {
        let reference = reference(seq);
        timers.schedule(
            Duration::from_millis(millis),
            TimerTarget::Pid(pid(seq)),
            make_small(1),
            make_small(seq as i64),
            reference,
        );
        reference
    }

    /// The `(target, message)` of a fired **delivery** deadline; panics on a resume one.
    fn delivered(timer: &FiredTimer) -> (TimerTarget, i64) {
        match timer.action {
            TimerAction::Deliver { target, message, .. } => (target, signed_value(message)),
            other => panic!("expected a delivery, got {other:?}"),
        }
    }

    #[test]
    fn heap_orders_by_deadline_then_insertion_sequence() {
        let base = Instant::now();
        let entry = |millis: u64, seq: u64| HeapEntry {
            deadline: base + Duration::from_millis(millis),
            seq,
            id: TimerId(seq),
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
    fn a_scheduled_timer_is_the_next_deadline_and_fires_once() {
        let mut timers = Timers::new();
        assert_eq!(timers.pending_count(), 0);
        assert_eq!(timers.next_deadline(), None);

        let reference = schedule(&mut timers, 0, 1);
        assert_eq!(timers.pending_count(), 1);
        assert!(timers.is_timer_ref(reference));
        assert!(timers.next_deadline().is_some());

        // A deadline in the past is due immediately.
        let fired = timers.take_due(Instant::now());
        assert_eq!(fired.len(), 1);
        assert_eq!(
            delivered(&fired[0]),
            (TimerTarget::Pid(pid(1)), 1),
            "the delivered target and message"
        );
        assert_eq!(timers.pending_count(), 0);
        assert_eq!(timers.next_deadline(), None, "it is gone from the heap");
        assert!(
            timers.is_timer_ref(reference),
            "a fired reference is still a timer reference"
        );
        assert!(timers.take_due(Instant::now()).is_empty(), "it fires once");
    }

    #[test]
    fn take_due_reports_every_passed_deadline_in_order() {
        let mut timers = Timers::new();
        schedule(&mut timers, 0, 1);
        schedule(&mut timers, 0, 2);
        // A deadline in the future is *not* due.
        schedule(&mut timers, 3_600_000, 3);

        let fired = timers.take_due(Instant::now());
        let messages: Vec<i64> = fired.iter().map(|timer| delivered(timer).1).collect();
        assert_eq!(messages, vec![1, 2], "equal deadlines keep insertion order");
        assert_eq!(timers.pending_count(), 1, "the far timer is still pending");
        assert!(timers.next_deadline().is_some());
    }

    #[test]
    fn cancel_answers_the_remaining_time_and_never_fires() {
        let mut timers = Timers::new();
        let reference = schedule(&mut timers, 3_600_000, 7);
        let remaining = timers.cancel(reference).expect("it was pending");
        assert!(
            remaining <= Duration::from_millis(3_600_000),
            "the remaining time cannot exceed the delay: {remaining:?}"
        );
        assert_eq!(timers.pending_count(), 0);
        assert_eq!(timers.next_deadline(), None, "a cancelled head is pruned");
        assert!(timers.take_due(Instant::now() + Duration::from_secs(7200)).is_empty());
        // Cancelling twice, and cancelling a fired reference, both answer `None`.
        assert_eq!(timers.cancel(reference), None);
        assert!(timers.is_timer_ref(reference));
    }

    #[test]
    fn cancel_of_a_deep_tombstone_leaves_the_earlier_deadline() {
        let mut timers = Timers::new();
        let early = schedule(&mut timers, 1_800_000, 1);
        let late = schedule(&mut timers, 3_600_000, 2);

        assert!(timers.cancel(late).is_some());
        assert_eq!(timers.pending_count(), 1);
        // The earlier live deadline is still exposed (the tombstone is not the head).
        assert!(timers.next_deadline().is_some());
        assert_eq!(
            timers.take_due(Instant::now() + Duration::from_secs(3600)).len(),
            1,
            "only the earlier live deadline fires"
        );
        assert!(timers.cancel(early).is_none(), "it fired");
    }

    #[test]
    fn an_unknown_reference_is_not_a_timer_reference() {
        let mut timers = Timers::new();
        assert!(!timers.is_timer_ref(reference(1)));
        assert_eq!(timers.cancel(reference(1)), None);
    }

    #[test]
    fn a_huge_delay_saturates_instead_of_overflowing() {
        let mut timers = Timers::new();
        schedule(&mut timers, u64::MAX, 1);
        assert_eq!(timers.pending_count(), 1);
        assert!(timers.next_deadline().is_some());
    }

    #[test]
    fn a_receive_deadline_fires_a_resume_and_is_not_a_timer_reference() {
        // A receive timeout is a runtime-internal deadline: its action is a `Resume` at the
        // timed-out path, and it mints **no** caller-visible timer reference (so
        // `cancel_timer/1` can never reach it).
        let mut timers = Timers::new();
        let _handle = timers.schedule_resume(Duration::from_millis(0), pid(4), 2, 11);
        assert_eq!(timers.pending_count(), 1);
        assert!(timers.next_deadline().is_some());
        // No reference term addresses it: `is_timer_ref` is false for any term.
        assert!(!timers.is_timer_ref(reference(4)));
        assert_eq!(timers.cancel(reference(4)), None);

        let fired = timers.take_due(Instant::now());
        assert_eq!(fired.len(), 1);
        assert!(matches!(
            fired[0].action,
            TimerAction::Resume {
                pid: p,
                func: 2,
                pc: 11
            } if p == pid(4)
        ));
        assert_eq!(timers.pending_count(), 0, "it fires exactly once");
    }

    #[test]
    fn cancel_handle_removes_a_pending_receive_deadline() {
        // The message-wake path: the receiver got a message before the deadline, so the
        // runtime cancels the deadline by its handle and it can never fire.
        let mut timers = Timers::new();
        let handle = timers.schedule_resume(Duration::from_millis(1_000), pid(5), 0, 3);
        assert!(timers.cancel_handle(handle), "it was pending");
        assert_eq!(timers.pending_count(), 0);
        assert_eq!(timers.next_deadline(), None, "the cancelled head is pruned");
        // Cancelling twice, and cancelling a fired handle, both answer `false`.
        assert!(!timers.cancel_handle(handle));
        let other = timers.schedule_resume(Duration::from_millis(0), pid(6), 0, 4);
        assert_eq!(timers.take_due(Instant::now()).len(), 1, "it fired");
        assert!(!timers.cancel_handle(other), "a fired deadline is not cancellable");
    }
}
