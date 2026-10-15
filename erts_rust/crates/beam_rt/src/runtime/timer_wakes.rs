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

use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::time::{Duration, Instant};

use beam_process::ProcessId;
use beam_term::Eterm;

use crate::timers::{FiredTimer, TimerAction, TimerHandle, TimerTarget, Timers};

use super::Runtime;

/// The pending deadlines of a [`Runtime`], keyed by what they can wake.
///
/// See the module documentation; the only consumer is the run route's
/// wait/quiescence test ([`Runtime::is_quiescent`], [`Runtime::timer_wait`]).
#[derive(Default)]
pub(crate) struct TimerWakes {
    /// Every pending `erlang:send_after/3,4` / `erlang:start_timer/3,4` deadline, keyed by the
    /// reference term the caller holds, recording where its message will be delivered.
    deliver: HashMap<Eterm, TimerTarget>,
    /// Every pending **receive timeout** (`receive … after T`), keyed by the handle the runner
    /// cancels with, recording the pid it would resume.
    resume: HashMap<TimerHandle, ProcessId>,
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
    /// one record, so removing any match is exact.
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
