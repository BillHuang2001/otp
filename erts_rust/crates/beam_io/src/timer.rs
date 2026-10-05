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
        todo!("a timeout with no deadline")
    }

    /// A timeout expiring `d` from now.
    pub fn after(d: Duration) -> Timeout {
        let _ = d;
        todo!("a deadline `d` from now")
    }

    /// A timeout expiring at the absolute instant `deadline`.
    pub fn at(deadline: Instant) -> Timeout {
        let _ = deadline;
        todo!("a deadline at the given absolute instant")
    }

    /// The remaining duration to wait, or `None` for an infinite timeout.
    ///
    /// Returns `Some(Duration::ZERO)` once the deadline has elapsed.
    pub fn poll_timeout(&self) -> Option<Duration> {
        todo!("deadline.map(|d| d.saturating_duration_since(Instant::now()))")
    }

    /// Whether the deadline has already elapsed (always `false` for an infinite timeout).
    pub fn is_expired(&self) -> bool {
        todo!("self.deadline.is_some_and(|d| Instant::now() >= d)")
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
        let _ = (key, deadline);
        todo!("insert or replace the (deadline, key) entry")
    }

    /// The earliest scheduled deadline, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        todo!("min over self.entries")
    }

    /// The duration until the earliest deadline, or `None` when the timer is empty.
    ///
    /// Returns `Some(Duration::ZERO)` when the earliest deadline has already elapsed.
    pub fn poll_timeout(&self) -> Option<Duration> {
        todo!("next_deadline().map(|d| d.saturating_duration_since(Instant::now()))")
    }

    /// Removes and returns every key whose deadline is `<= now`.
    pub fn pop_expired(&mut self, now: Instant) -> Vec<u64> {
        let _ = now;
        todo!("drain the entries at or before `now` and return their keys")
    }

    /// Cancels `key`, reporting whether it was scheduled.
    pub fn cancel(&mut self, key: u64) -> bool {
        let _ = key;
        todo!("remove the entry for `key` and report whether one was present")
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
