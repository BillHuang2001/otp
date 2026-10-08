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

//! Poll-deadline primitives: a single [`Timeout`] and a multi-deadline [`Timer`].
//!
//! The reactor needs a **time bound** on every blocking [`crate::poller::Poller::poll`]
//! call: [`Timeout`] computes the remaining duration to hand to the poller (or `None` to
//! block indefinitely), and [`Timer`] tracks the set of pending deadlines keyed by a `u64`
//! so the reactor can wake exactly the ports/processes whose timers fired. This is the
//! Rust analogue of the legacy C's timer wheel / poll timeout plumbing
//! (`erts/emulator/beam/erl_time_sup.c`, `erl_poll.c`).

use std::time::{Duration, Instant};

/// A single deadline, either infinite or a fixed [`Instant`].
#[derive(Clone, Copy, Debug)]
pub struct Timeout {
    /// `None` means "no deadline" (block forever); `Some` is the absolute expiry time.
    deadline: Option<Instant>,
}

impl Timeout {
    /// A timeout that never expires (the poller blocks until an event arrives).
    pub fn infinite() -> Timeout {
        Timeout { deadline: None }
    }

    /// A timeout expiring `d` from now.
    ///
    /// A duration so large that the deadline cannot be represented saturates to an
    /// infinite timeout rather than overflowing.
    pub fn after(d: Duration) -> Timeout {
        Timeout {
            deadline: Instant::now().checked_add(d),
        }
    }

    /// A timeout expiring at the absolute instant `deadline`.
    pub fn at(deadline: Instant) -> Timeout {
        Timeout {
            deadline: Some(deadline),
        }
    }

    /// The remaining duration to wait, or `None` for an infinite timeout.
    ///
    /// Returns `Some(Duration::ZERO)` once the deadline has elapsed.
    pub fn poll_timeout(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Whether the deadline has already elapsed (always `false` for an infinite timeout).
    pub fn is_expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }
}

/// A set of scheduled deadlines keyed by `u64`.
///
/// Entries are `(deadline, key)` pairs; the reactor schedules a key when it arms a timer,
/// asks [`Timer::poll_timeout`] for the nearest deadline to bound its next
/// [`crate::poller::Poller::poll`], and drains the fired keys with
/// [`Timer::pop_expired`].
pub struct Timer {
    /// The scheduled `(deadline, key)` pairs (order is an implementation detail).
    entries: Vec<(Instant, u64)>,
}

impl Timer {
    /// An empty timer.
    pub fn new() -> Timer {
        Timer {
            entries: Vec::new(),
        }
    }

    /// Schedules `key` to fire at `deadline`, replacing any earlier deadline for `key`.
    pub fn schedule(&mut self, key: u64, deadline: Instant) {
        match self.entries.iter_mut().find(|(_, k)| *k == key) {
            Some(entry) => entry.0 = deadline,
            None => self.entries.push((deadline, key)),
        }
    }

    /// The earliest scheduled deadline, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.entries.iter().map(|(deadline, _)| *deadline).min()
    }

    /// The duration until the earliest deadline, or `None` when the timer is empty.
    ///
    /// Returns `Some(Duration::ZERO)` when the earliest deadline has already elapsed.
    pub fn poll_timeout(&self) -> Option<Duration> {
        self.next_deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// Removes and returns every key whose deadline is `<= now`.
    pub fn pop_expired(&mut self, now: Instant) -> Vec<u64> {
        // `Vec::remove` shifts, so the returned keys stay in schedule order (stable).
        let mut expired = Vec::new();
        let mut i = 0;
        while i < self.entries.len() {
            if self.entries[i].0 <= now {
                expired.push(self.entries.remove(i).1);
            } else {
                i += 1;
            }
        }
        expired
    }

    /// Cancels `key`, reporting whether it was scheduled.
    pub fn cancel(&mut self, key: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|(_, k)| *k != key);
        self.entries.len() != before
    }

    /// The number of scheduled deadlines.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the timer holds no deadlines.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{Timeout, Timer};
    use std::time::{Duration, Instant};

    #[test]
    fn infinite_timeout_never_expires() {
        let timeout = Timeout::infinite();
        assert!(timeout.poll_timeout().is_none());
        assert!(!timeout.is_expired());
    }

    #[test]
    fn after_reports_the_remaining_duration() {
        let timeout = Timeout::after(Duration::from_millis(500));
        let remaining = timeout.poll_timeout().expect("a bounded deadline");
        assert!(remaining <= Duration::from_millis(500));
        assert!(remaining > Duration::from_millis(490));
        assert!(!timeout.is_expired());
    }

    #[test]
    fn an_elapsed_deadline_reports_zero_and_expired() {
        let timeout = Timeout::at(Instant::now() - Duration::from_millis(1));
        assert_eq!(timeout.poll_timeout(), Some(Duration::ZERO));
        assert!(timeout.is_expired());
    }

    #[test]
    fn a_huge_duration_saturates_to_infinite() {
        let timeout = Timeout::after(Duration::MAX);
        assert!(timeout.poll_timeout().is_none());
        assert!(!timeout.is_expired());
    }

    #[test]
    fn timer_is_empty_until_scheduled() {
        let mut timer = Timer::new();
        assert!(timer.is_empty());
        assert_eq!(timer.len(), 0);
        assert_eq!(timer.next_deadline(), None);
        assert_eq!(timer.poll_timeout(), None);
        assert!(!timer.cancel(1));

        let deadline = Instant::now() + Duration::from_secs(60);
        timer.schedule(1, deadline);
        assert_eq!(timer.len(), 1);
        assert_eq!(timer.next_deadline(), Some(deadline));
        assert!(timer.poll_timeout().is_some());
    }

    #[test]
    fn schedule_replaces_an_existing_deadline_for_the_same_key() {
        let now = Instant::now();
        let mut timer = Timer::new();
        timer.schedule(7, now + Duration::from_secs(10));
        timer.schedule(7, now + Duration::from_secs(1));
        assert_eq!(timer.len(), 1, "a key has at most one deadline");
        assert_eq!(timer.next_deadline(), Some(now + Duration::from_secs(1)));
    }

    #[test]
    fn next_deadline_is_the_minimum() {
        let now = Instant::now();
        let mut timer = Timer::new();
        timer.schedule(1, now + Duration::from_secs(30));
        timer.schedule(2, now + Duration::from_secs(5));
        timer.schedule(3, now + Duration::from_secs(10));
        assert_eq!(timer.next_deadline(), Some(now + Duration::from_secs(5)));
    }

    #[test]
    fn pop_expired_returns_only_due_keys_in_schedule_order() {
        let now = Instant::now();
        let mut timer = Timer::new();
        timer.schedule(1, now - Duration::from_secs(2));
        timer.schedule(2, now + Duration::from_secs(30));
        timer.schedule(3, now - Duration::from_secs(1));
        timer.schedule(4, now);

        // `now` itself counts as due (`deadline <= now`).
        assert_eq!(timer.pop_expired(now), vec![1, 3, 4]);
        assert_eq!(timer.len(), 1);
        assert!(timer.pop_expired(now).is_empty());
        assert_eq!(timer.next_deadline(), Some(now + Duration::from_secs(30)));
    }

    #[test]
    fn cancel_removes_only_the_named_key() {
        let now = Instant::now();
        let mut timer = Timer::new();
        timer.schedule(1, now + Duration::from_secs(1));
        timer.schedule(2, now + Duration::from_secs(2));

        assert!(timer.cancel(1));
        assert!(!timer.cancel(1), "a cancelled key is gone");
        assert_eq!(timer.len(), 1);
        assert_eq!(timer.next_deadline(), Some(now + Duration::from_secs(2)));
    }
}
