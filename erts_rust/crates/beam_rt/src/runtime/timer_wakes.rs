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

//! **Which pending deadline could wake which process** — the run route's wait predicate.
//!
//! The runtime's timer heap ([`crate::timers`]) knows *when* the next deadline is, but not
//! *whom* it can wake: a `Deliver` message and a `Resume` (a finite `receive … after T`) are
//! the only two actions, and both name their target inside the pending entry. The `run`
//! route ([`Runtime::run_scheduled`]) waits for **one** process, so a deadline aimed
//! somewhere else is not work it has to wait for — and the boot leaves exactly such a
//! deadline behind (`inet_db`'s hourly `erlang:send_after(3600000, self(), refresh_timeout)`
//! cache refresh, a `Deliver` to a kernel process's pid).
//!
//! [`TimerWakes`] is that missing half of the heap, kept in the runtime (the heap itself is
//! untouched): every deadline the runtime schedules or cancels passes through
//! [`TimerSlot`] — the wrapper [`Runtime::timers_mut`] hands out — or through
//! [`Runtime::schedule_receive_timeout`], and every deadline that **fires** is forgotten by
//! [`Runtime::take_due_timers`]. So the mirror holds exactly the pending deadlines and can
//! answer [`TimerWakes::can_wake`]: "could a pending timer wake this process?".
//!
//! A **registered-name** target resolves at fire time (exactly as the oracle resolves it), so
//! it is attributed to no pid and is conservatively counted as *possibly* waking any process:
//! over-counting only costs a wake-up check, under-counting would let the run route declare a
//! stall a live deadline could have broken.

use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant};

use beam_process::ProcessId;
use beam_term::Eterm;

use crate::hasher::FastHashMap;
use crate::timers::{FiredTimer, TimerAction, TimerHandle, TimerTarget, Timers};

use super::Runtime;

/// The pending deadlines of a [`Runtime`], keyed by what they can wake.
///
/// See the module documentation; the only consumer is the run route's
/// wait/quiescence test ([`Runtime::is_quiescent`], [`Runtime::timer_wait`]).
///
/// Both halves are [`FastHashMap`]s over the crate's own [`crate::hasher`]: their keys are
/// copy/hash-only ids this runtime mints (the caller's reference term, the runner's
/// [`TimerHandle`]) and every site is an `insert`/`get`/`remove` or a fire-time scan whose
/// *answer* is order-independent ([`TimerWakes::forget_action`]), never an observable
/// iteration — so the probe is hasher-independent and the order is unobservable either way.
#[derive(Default)]
pub(crate) struct TimerWakes {
    /// Every pending `erlang:send_after/3,4` / `erlang:start_timer/3,4` deadline, keyed by the
    /// reference term the caller holds, recording where its message will be delivered.
    deliver: FastHashMap<Eterm, TimerTarget>,
    /// Every pending **receive timeout** (`receive … after T`), keyed by the handle the runner
    /// cancels with, recording the pid it would resume.
    resume: FastHashMap<TimerHandle, ProcessId>,
}

impl TimerWakes {
    /// Records the `Deliver` deadline `reference` identifies.
    pub(crate) fn note_deliver(&mut self, reference: Eterm, target: TimerTarget) {
        self.deliver.insert(reference, target);
    }

    /// Forgets the `Deliver` deadline `reference` identifies — a cancellation, or the
    /// bookkeeping half of a fire.
    pub(crate) fn forget_deliver(&mut self, reference: Eterm) {
        self.deliver.remove(&reference);
    }

    /// Records the receive deadline `handle` identifies, which would resume `pid`.
    pub(crate) fn note_resume(&mut self, handle: TimerHandle, pid: ProcessId) {
        self.resume.insert(handle, pid);
    }

    /// Forgets the receive deadline `handle` identifies.
    pub(crate) fn forget_resume(&mut self, handle: TimerHandle) {
        self.resume.remove(&handle);
    }

    /// Forgets the deadline a **fired** timer stood for.
    ///
    /// A fired action names its target but not the caller's reference handle, so the record is
    /// found by target: equal targets are interchangeable, and every fired timer has exactly
    /// one record, so removing any match is exact. The scan is linear, so which of several
    /// equal-target records the map's hash order reaches first is unobservable — the
    /// [`FastHashMap`] iteration order changes no answer the one reader,
    /// [`TimerWakes::can_wake`], can see.
    pub(crate) fn forget_action(&mut self, action: &TimerAction) {
        match action {
            TimerAction::Deliver { target, .. } => {
                let matched = self
                    .deliver
                    .iter()
                    .find(|(_, recorded)| *recorded == target)
                    .map(|(reference, _)| *reference);
                if let Some(reference) = matched {
                    self.deliver.remove(&reference);
                }
            }
            TimerAction::Resume { pid, .. } => {
                let matched = self
                    .resume
                    .iter()
                    .find(|(_, recorded)| *recorded == pid)
                    .map(|(handle, _)| *handle);
                if let Some(handle) = matched {
                    self.resume.remove(&handle);
                }
            }
        }
    }

    /// Whether a pending deadline could wake `pid` — a `Deliver` aimed at its pid (or at a
    /// registered name, which resolves at fire time), or its own parked
    /// `receive … after T` deadline.
    ///
    /// An `any()` over each mirror, so the answer is independent of the maps' iteration order.
    pub(crate) fn can_wake(&self, pid: ProcessId) -> bool {
        self.resume.values().any(|recorded| *recorded == pid)
            || self.deliver.values().any(|target| match target {
                TimerTarget::Pid(recorded) => *recorded == pid,
                TimerTarget::Name(_) => true,
            })
    }
}

/// The runtime's timer heap, wrapped so that **every** scheduling and cancellation is
/// mirrored into the [`TimerWakes`] records the run route reads.
///
/// This is what [`Runtime::timers_mut`] hands out; it derefs to the heap, so every other
/// heap operation (`read_timer`'s remaining time, the cancellation predicates) is reached
/// unchanged, while [`TimerSlot::schedule`] and [`TimerSlot::cancel`] additionally keep the
/// mirror exact.
pub(crate) struct TimerSlot<'a> {
    /// The heap itself.
    timers: &'a mut Timers,
    /// The record of what its pending deadlines can wake.
    wakes: &'a mut TimerWakes,
}

impl<'a> TimerSlot<'a> {
    /// Wraps the runtime's heap and its wake records.
    pub(crate) fn new(timers: &'a mut Timers, wakes: &'a mut TimerWakes) -> TimerSlot<'a> {
        TimerSlot { timers, wakes }
    }

    /// Schedules `message` for `target` at `delay` from now — the heap's own
    /// [`Timers::schedule`], with the wake record kept in step.
    pub(crate) fn schedule(
        &mut self,
        delay: Duration,
        target: TimerTarget,
        from: Eterm,
        message: Eterm,
        reference: Eterm,
    ) {
        self.wakes.note_deliver(reference, target);
        self.timers.schedule(delay, target, from, message, reference);
    }

    /// Cancels the pending timer `reference` identifies — the heap's own [`Timers::cancel`],
    /// with the wake record dropped (a timer that already fired had its record dropped by
    /// [`Runtime::take_due_timers`], so forgetting it here again is a no-op).
    pub(crate) fn cancel(&mut self, reference: Eterm) -> Option<Duration> {
        let remaining = self.timers.cancel(reference);
        self.wakes.forget_deliver(reference);
        remaining
    }
}

impl Deref for TimerSlot<'_> {
    type Target = Timers;

    fn deref(&self) -> &Timers {
        &*self.timers
    }
}

impl DerefMut for TimerSlot<'_> {
    fn deref_mut(&mut self) -> &mut Timers {
        &mut *self.timers
    }
}

impl Runtime {
    /// Marks `pid` as **the process the run route is waiting for**, returning the previous
    /// marker so the caller can restore it.
    ///
    /// While a marker is set, a pending deadline only counts as work when it could wake that
    /// process (see [`TimerWakes::can_wake`]); with no marker — every other route — any
    /// pending deadline counts, exactly as before.
    pub(crate) fn set_run_wake_pid(&mut self, pid: Option<ProcessId>) -> Option<ProcessId> {
        std::mem::replace(&mut self.run_wake_pid, pid)
    }

    /// Removes the deadlines that have passed *and* forgets each one in the wake records —
    /// the fire path must not leave a stale "this pid can still be woken" behind.
    pub(crate) fn take_due_timers(&mut self, now: Instant) -> Vec<FiredTimer> {
        let fired = self.timers.take_due(now);
        for timer in &fired {
            self.timer_wake_targets.forget_action(&timer.action);
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use beam_process::ProcessId;
    use beam_term::{make_small, Atom, Eterm};

    use crate::hasher::FastHashMap;
    use crate::timers::{TimerAction, TimerHandle, TimerTarget, Timers};

    use super::TimerWakes;

    /// The **type pin** for the `Deliver` mirror: this signature stops compiling the moment the
    /// field reverts to `RandomState`'s `HashMap` (the `assert_fast_hashed` pattern the mailbox
    /// scan-state map pins in `crate::mailbox`).
    fn assert_fast_hashed_deliver(map: &FastHashMap<Eterm, TimerTarget>) -> usize {
        map.len()
    }

    /// The type pin for the `Resume` mirror, as above.
    fn assert_fast_hashed_resume(map: &FastHashMap<TimerHandle, ProcessId>) -> usize {
        map.len()
    }

    /// The wake mirror is a pair of [`FastHashMap`]s **and** still answers its contract: a
    /// pending deadline can wake exactly the processes it targets, and forgetting it stops
    /// that. The hasher swap changes the probe cost and the (unobserved) order, nothing else —
    /// so the pin is never a bare type check.
    #[test]
    fn the_wake_mirror_is_fast_hashed_and_answers_its_contract() {
        let mut wakes = TimerWakes::default();
        let own = ProcessId::new(3, 0);
        let other = ProcessId::new(4, 0);

        // A receive timeout: recorded, wakable while pending, inert once forgotten.
        let mut timers = Timers::new();
        let handle = timers.schedule_resume(Duration::from_secs(60), own, 7, 9);
        wakes.note_resume(handle, own);
        assert!(
            wakes.can_wake(own),
            "a pending receive deadline wakes its own process"
        );
        assert!(!wakes.can_wake(other), "and no other process");
        assert_eq!(
            assert_fast_hashed_resume(&wakes.resume),
            1,
            "the resume mirror holds exactly the pending deadline"
        );
        wakes.forget_resume(handle);
        assert!(!wakes.can_wake(own), "a forgotten deadline wakes nobody");

        // A `Deliver` aimed at a pid wakes only that pid.
        let reference = make_small(42);
        wakes.note_deliver(reference, TimerTarget::Pid(other));
        assert!(wakes.can_wake(other), "a pid-targeted deadline wakes its pid");
        assert!(!wakes.can_wake(own), "and not a different pid");
        assert_eq!(
            assert_fast_hashed_deliver(&wakes.deliver),
            1,
            "the deliver mirror holds exactly the pending deadline"
        );
        wakes.forget_deliver(reference);
        assert!(!wakes.can_wake(other), "the cancellation drops the record");

        // A name target resolves at fire time, so it conservatively counts as waking anyone.
        wakes.note_deliver(make_small(43), TimerTarget::Name(Atom::from_raw(7)));
        assert!(wakes.can_wake(own) && wakes.can_wake(other));
        assert_eq!(assert_fast_hashed_deliver(&wakes.deliver), 1);
    }

    /// The one linear scan ([`TimerWakes::forget_action`]) removes **a** record whose target
    /// matches, never a particular one — which is what makes the hash order unobservable: two
    /// deadlines sharing a target are interchangeable, so the removed *count* (all the only
    /// reader consults) is the same whichever the scan reaches first.
    #[test]
    fn forgetting_a_fired_deadline_preserves_a_shared_target() {
        let mut wakes = TimerWakes::default();
        let pid = ProcessId::new(5, 0);
        wakes.note_deliver(make_small(1), TimerTarget::Pid(pid));
        wakes.note_deliver(make_small(2), TimerTarget::Pid(pid));

        let fired = TimerAction::Deliver {
            target: TimerTarget::Pid(pid),
            from: make_small(0),
            message: make_small(0),
        };
        // Exactly one of the two records is forgotten, so the pid stays wakable.
        wakes.forget_action(&fired);
        assert_eq!(
            wakes.deliver.len(),
            1,
            "one fired deadline forgets exactly one record"
        );
        assert!(wakes.can_wake(pid), "the still-pending deadline keeps the pid wakable");

        // The second fires too: now no delivery can wake the pid any more.
        wakes.forget_action(&fired);
        assert!(wakes.deliver.is_empty(), "both records are gone");
        assert!(!wakes.can_wake(pid));
    }
}
