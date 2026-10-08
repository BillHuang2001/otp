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

//! Integration evidence for the timer service (`erlang:send_after`-style timers).
//!
//! Pins the observable contract of `beam_process::TimerService`: a scheduled timer fires into
//! the [`TimerSink`] with the exact target and message; timers fire in deadline order, with
//! equal deadlines broken by insertion order; `cancel` prevents firing and reports `false`
//! for an already-cancelled or already-fired ref; `pending`/`next_deadline` track the wheel;
//! and the `shutdown`/`join` lifecycle stops the thread.
//!
//! Flakiness is avoided by never `thread::sleep`-ing to prove correctness: the sink forwards
//! every fire over an `mpsc` channel and the tests block on `recv_timeout` with a generous
//! budget (5 s) — far larger than the tens-of-milliseconds deadlines they schedule. The one
//! negative assertion (a cancelled timer never fires) waits a window several times longer than
//! the cancelled deadline, which can only make a *buggy* timer more likely to be caught.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_process::prelude::*;
use beam_term::prelude::{make_small, Eterm};

/// A generous upper bound for a legitimate delivery (scheduled deadlines are ~20–30 ms).
const RECV: Duration = Duration::from_secs(5);

/// A [`TimerSink`] that forwards every fire over a channel for the test to observe.
struct Recorder {
    tx: Sender<(ProcessId, Eterm)>,
}

impl TimerSink for Recorder {
    fn timer_fired(&self, target: ProcessId, message: Eterm) {
        // A closed receiver (test already finished) must never panic the timer thread.
        let _ = self.tx.send((target, message));
    }
}

/// Starts a service on a fresh recording sink, returning the service and the receive end.
fn started() -> (Arc<TimerService>, Receiver<(ProcessId, Eterm)>) {
    let (tx, rx) = mpsc::channel();
    let svc = Arc::new(TimerService::new());
    svc.start(Arc::new(Recorder { tx }));
    (svc, rx)
}

#[test]
fn scheduled_timer_fires_into_the_sink() {
    let (svc, rx) = started();
    let pid = ProcessId::new(7, 3);
    let msg = make_small(1234);

    svc.send_after(Duration::from_millis(20), pid, msg);

    assert_eq!(
        rx.recv_timeout(RECV).expect("the timer fires"),
        (pid, msg),
        "the exact target and message arrive"
    );

    svc.shutdown();
    svc.join();
}

#[test]
fn equal_delay_timers_fire_in_insertion_order() {
    let (svc, rx) = started();
    let pid = ProcessId::new(1, 0);
    let first = make_small(1);
    let second = make_small(2);

    // Same nominal delay, scheduled back-to-back: `now` only increases between the two calls,
    // so `first` has the earliest `(deadline, sequence)` and must fire first.
    svc.send_after(Duration::from_millis(30), pid, first);
    svc.send_after(Duration::from_millis(30), pid, second);

    assert_eq!(rx.recv_timeout(RECV).expect("first fires").1, first);
    assert_eq!(rx.recv_timeout(RECV).expect("second fires").1, second);

    svc.shutdown();
    svc.join();
}

#[test]
fn earlier_deadline_fires_before_a_later_inserted_later_deadline() {
    let (svc, rx) = started();
    let pid = ProcessId::new(2, 0);
    let slow = make_small(100);
    let fast = make_small(200);

    // `slow` is inserted first but has the later deadline; deadline order must win.
    svc.send_after(Duration::from_millis(250), pid, slow);
    svc.send_after(Duration::from_millis(20), pid, fast);

    assert_eq!(rx.recv_timeout(RECV).expect("fast fires first").1, fast);
    assert_eq!(rx.recv_timeout(RECV).expect("slow fires second").1, slow);

    svc.shutdown();
    svc.join();
}

#[test]
fn cancel_prevents_firing_and_is_idempotent() {
    let (svc, rx) = started();
    let pid = ProcessId::new(3, 0);

    let cancelled = svc.send_after(Duration::from_millis(40), pid, make_small(1));
    assert!(svc.cancel(cancelled), "a live timer cancels");
    assert!(
        !svc.cancel(cancelled),
        "an already-cancelled ref is not live"
    );

    // Nothing must arrive within a window well beyond the cancelled timer's deadline.
    assert!(
        matches!(
            rx.recv_timeout(Duration::from_millis(300)),
            Err(RecvTimeoutError::Timeout)
        ),
        "a cancelled timer never fires"
    );

    // A live timer still fires, and afterwards its ref can no longer be cancelled.
    let live = svc.send_after(Duration::from_millis(20), pid, make_small(2));
    assert_eq!(
        rx.recv_timeout(RECV).expect("live timer fires").1,
        make_small(2)
    );
    assert!(
        !svc.cancel(live),
        "a fired timer can no longer be cancelled"
    );

    svc.shutdown();
    svc.join();
}

#[test]
fn pending_accounts_for_schedule_cancel_and_fire() {
    let (svc, rx) = started();
    let pid = ProcessId::new(4, 0);
    assert_eq!(svc.pending(), 0);

    let _a = svc.send_after(Duration::from_millis(30), pid, make_small(1));
    let _b = svc.send_after(Duration::from_millis(30), pid, make_small(2));
    let far = svc.send_after(Duration::from_secs(3000), pid, make_small(3));
    assert_eq!(svc.pending(), 3, "three scheduled");

    assert!(svc.cancel(far));
    assert_eq!(svc.pending(), 2, "cancel decrements");

    // Both remaining timers fire (the count is decremented before the sink runs).
    rx.recv_timeout(RECV).expect("the first two fire");
    rx.recv_timeout(RECV).expect("the first two fire");
    assert_eq!(svc.pending(), 0, "firing decrements");

    svc.shutdown();
    svc.join();
}

#[test]
fn next_deadline_tracks_the_earliest_live_timer() {
    // No thread needed: this is pure bookkeeping.
    let svc = TimerService::new();
    let pid = ProcessId::new(5, 0);
    assert_eq!(svc.next_deadline(), None);

    let before = Instant::now();
    let far = svc.send_after(Duration::from_secs(60), pid, make_small(1));
    assert!(svc.next_deadline().expect("a deadline") >= before + Duration::from_secs(59));

    let near = svc.send_after(Duration::from_secs(30), pid, make_small(2));
    let earliest = svc.next_deadline().expect("a deadline");
    assert!(earliest >= before + Duration::from_secs(29));
    assert!(
        earliest < before + Duration::from_secs(60),
        "the 30 s timer is earliest"
    );

    assert!(svc.cancel(near));
    assert!(
        svc.next_deadline().expect("a deadline") >= before + Duration::from_secs(59),
        "cancelling the nearest exposes the next"
    );
    assert!(svc.cancel(far));
    assert_eq!(svc.next_deadline(), None);
}

#[test]
fn a_timer_scheduled_before_start_fires_once_started() {
    let (tx, rx) = mpsc::channel();
    let svc = Arc::new(TimerService::new());
    let pid = ProcessId::new(6, 0);
    let msg = make_small(77);

    // Zero delay: due on the very next tick, but only once the thread exists.
    svc.send_after(Duration::ZERO, pid, msg);
    assert!(!svc.is_running());
    assert_eq!(svc.pending(), 1);

    svc.start(Arc::new(Recorder { tx }));
    assert!(svc.is_running());
    assert_eq!(
        rx.recv_timeout(RECV).expect("fires once started"),
        (pid, msg)
    );

    svc.shutdown();
    svc.join();
}

#[test]
fn shutdown_stops_the_thread_and_join_returns() {
    let (tx, _rx) = mpsc::channel();
    let svc = Arc::new(TimerService::new());
    svc.start(Arc::new(Recorder { tx }));

    svc.send_after(
        Duration::from_secs(3600),
        ProcessId::new(1, 0),
        make_small(1),
    );
    assert!(svc.is_running());
    assert_eq!(svc.pending(), 1);

    svc.shutdown();
    assert!(!svc.is_running());
    assert_eq!(svc.pending(), 0, "shutdown drops pending timers");

    // `join` returns once the thread has observed the shutdown flag and exited.
    svc.join();

    // Both operations are idempotent.
    svc.shutdown();
    svc.join();
    assert!(!svc.is_running());
}
