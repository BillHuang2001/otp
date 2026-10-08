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

//! Integration tests for the `mio`-backed [`Poller`] over **real OS descriptors**.
//!
//! A `socketpair(2)` (`std::os::unix::net::UnixStream::pair`) stands in for a port's
//! device: writing to one end makes the other readable, which is exactly the readiness the
//! reactor must observe. Timeouts are generous (1–2 s) so the suite is not timing-flaky.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::prelude::*;

fn socketpair() -> (UnixStream, UnixStream) {
    UnixStream::pair().expect("socketpair(2) succeeds")
}

#[test]
fn readable_readiness_is_reported_for_the_registered_key() {
    let mut poller = Poller::new().expect("a poll set");
    let (peer, local) = socketpair();
    let key = 7;

    let token = poller
        .register(local.as_raw_fd(), Interest::READABLE, key)
        .expect("the descriptor registers");
    assert_eq!(poller.registration(token), Some(key));
    assert_eq!(poller.len(), 1);

    // Nothing has been written yet, so a short poll reports no readiness.
    assert!(
        poller
            .poll(Some(Duration::from_millis(20)))
            .expect("poll succeeds")
            .is_empty(),
        "an idle descriptor is not ready"
    );

    (&peer)
        .write_all(b"ping")
        .expect("the peer accepts the write");
    let events = poller
        .poll(Some(Duration::from_secs(1)))
        .expect("poll succeeds");

    assert_eq!(events.len(), 1, "only the peer-written descriptor is ready");
    assert_eq!(events[0].key(), key);
    assert_eq!(events[0].token(), token);
    assert!(events[0].ready().readable());
    assert!(!events[0].ready().writable());
}

#[test]
fn poll_with_no_events_returns_empty_after_the_timeout() {
    let mut poller = Poller::new().expect("a poll set");
    let timeout = Duration::from_millis(50);

    let started = Instant::now();
    let events = poller.poll(Some(timeout)).expect("poll succeeds");
    let elapsed = started.elapsed();

    assert!(events.is_empty(), "no registration is ready");
    assert!(
        elapsed >= Duration::from_millis(40),
        "poll must block for the timeout, waited only {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "poll must not overshoot the timeout, waited {elapsed:?}"
    );
}

#[test]
fn waker_interrupts_a_blocking_poll() {
    let mut poller = Poller::new().expect("a poll set");
    let waker = poller.waker().expect("a waker");

    let started = Instant::now();
    std::thread::scope(|scope| {
        // The helper thread *borrows* the waker; the poller owns the wakeup handle, so the
        // handle outlives this thread regardless.
        let waker = &waker;
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            waker.wake().expect("the waker fires");
        });

        // Block for much longer than the waker takes to fire.
        let events = poller
            .poll(Some(Duration::from_secs(2)))
            .expect("poll succeeds");
        assert_eq!(events.len(), 1, "the waker is the only registration");
        assert_eq!(events[0].key(), WAKER_KEY);
        assert!(events[0].ready().readable());
    });

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the wake short-circuits the 2 s timeout, took {:?}",
        started.elapsed()
    );
}

#[test]
fn waker_can_be_obtained_repeatedly_and_each_handle_fires() {
    let mut poller = Poller::new().expect("a poll set");

    // The poller owns a single wakeup handle; obtaining several handles to it must not
    // panic (mio permits only one `Waker` per poll set) and they are freely droppable.
    let first = poller.waker().expect("the first waker");
    let second = poller.waker().expect("the second waker");

    first.wake().expect("the first waker fires");
    let events = poller
        .poll(Some(Duration::from_secs(2)))
        .expect("poll succeeds");
    assert_eq!(events.len(), 1, "the waker is the only registration");
    assert_eq!(events[0].key(), WAKER_KEY);

    // Dropping one handle leaves the poller's wakeup functional for the others.
    drop(first);

    second.wake().expect("the second waker fires");
    let events = poller
        .poll(Some(Duration::from_secs(2)))
        .expect("poll succeeds");
    assert_eq!(events.len(), 1, "the waker is the only registration");
    assert_eq!(events[0].key(), WAKER_KEY);
}

#[test]
fn a_waker_clone_from_another_thread_wakes_a_later_poll() {
    let mut poller = Poller::new().expect("a poll set");
    let waker = poller.waker().expect("a waker");

    // A woken poll collects its readiness.
    waker.wake().expect("the waker fires");
    assert_eq!(
        poller
            .poll(Some(Duration::from_secs(1)))
            .expect("poll succeeds")[0]
            .key(),
        WAKER_KEY
    );

    // A clone moved into another thread wakes a *subsequent* blocking poll.
    let clone = poller.waker().expect("another waker");
    std::thread::scope(|scope| {
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            clone.wake().expect("the cloned waker fires");
        });
        let events = poller
            .poll(Some(Duration::from_secs(2)))
            .expect("poll succeeds");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].key(), WAKER_KEY);
    });
}
#[test]
fn reregister_switches_the_interest_mask() {
    let mut poller = Poller::new().expect("a poll set");
    let (peer, local) = socketpair();
    let key = 11;

    let token = poller
        .register(local.as_raw_fd(), Interest::READABLE, key)
        .expect("the descriptor registers");

    (&peer).write_all(b"x").expect("the peer accepts the write");
    let events = poller
        .poll(Some(Duration::from_secs(1)))
        .expect("poll succeeds");
    assert_eq!(events.len(), 1);
    assert!(events[0].ready().readable());
    assert_eq!(events[0].key(), key);

    // Drain the byte so the descriptor is no longer readable, then flip the interest to
    // writability only.
    let mut byte = [0u8; 1];
    (&local).read_exact(&mut byte).expect("the byte arrives");

    poller
        .reregister(local.as_raw_fd(), token, Interest::WRITABLE)
        .expect("the descriptor re-registers");
    let events = poller
        .poll(Some(Duration::from_secs(1)))
        .expect("poll succeeds");
    assert_eq!(events.len(), 1, "a drained socket is writable");
    assert_eq!(events[0].key(), key);
    assert_eq!(events[0].token(), token);
    assert!(events[0].ready().writable());
}

#[test]
fn deregister_stops_events() {
    let mut poller = Poller::new().expect("a poll set");
    let (peer, local) = socketpair();
    let key = 3;

    let token = poller
        .register(local.as_raw_fd(), Interest::READABLE, key)
        .expect("the descriptor registers");
    (&peer).write_all(b"a").expect("the peer accepts the write");
    assert_eq!(
        poller
            .poll(Some(Duration::from_secs(1)))
            .expect("poll succeeds")
            .len(),
        1
    );

    let mut drained = [0u8; 8];
    assert_eq!(
        (&local).read(&mut drained).expect("the byte arrives"),
        1,
        "the byte is still buffered"
    );

    poller
        .deregister(local.as_raw_fd(), token)
        .expect("the descriptor de-registers");
    assert_eq!(poller.registration(token), None);
    assert!(poller.is_empty());

    // New data on a de-registered descriptor produces no further events.
    (&peer)
        .write_all(b"bb")
        .expect("the peer accepts the write");
    assert!(
        poller
            .poll(Some(Duration::from_millis(50)))
            .expect("poll succeeds")
            .is_empty(),
        "a de-registered descriptor never reports readiness again"
    );
}

#[test]
fn registering_the_same_fd_twice_fails() {
    let mut poller = Poller::new().expect("a poll set");
    let (_peer, local) = socketpair();

    poller
        .register(local.as_raw_fd(), Interest::READABLE, 1)
        .expect("the first registration succeeds");
    let err = poller
        .register(local.as_raw_fd(), Interest::BOTH, 2)
        .expect_err("the second registration fails");
    assert!(
        matches!(err, BeamError::Io(_)),
        "an already-registered fd maps to BeamError::Io, got {err:?}"
    );
    assert_eq!(poller.len(), 1, "the failed registration changed nothing");
}
