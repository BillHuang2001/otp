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
//!
//! The last two tests drive the poller with a **real handled signal** through a
//! [`SignalFacility`](beam_io::signal::SignalFacility): a signal landing while the thread is
//! blocked in `epoll_wait` interrupts that wait with `EINTR` (epoll is never
//! `SA_RESTART`-restarted, `signal(7)`), and the poller must retry rather than surface the
//! interruption as a failure.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use beam_common::BeamError;
use beam_io::prelude::*;

fn socketpair() -> (UnixStream, UnixStream) {
    UnixStream::pair().expect("socketpair(2) succeeds")
}

/// Serializes the signal-driven tests.
///
/// A signal disposition is **process-global**, so two tests installing handlers at the same
/// time could observe each other's deliveries (or restore each other's dispositions). The
/// Rust test harness runs tests on parallel threads, so every test that raises a signal takes
/// this lock. A poisoned lock (a test panicked while holding it) is recovered — it guards
/// scheduling, not shared Rust data. The tests that do not raise signals never take it.
static SIGNAL_LOCK: Mutex<()> = Mutex::new(());

/// Acquires the process-global signal test guard.
fn signal_guard() -> MutexGuard<'static, ()> {
    SIGNAL_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

/// Raises `signo` on the calling process.
///
/// Only ever called with a signal whose disposition the test installed as `Handle`
/// (`SIGUSR1`) or whose default action is not "terminate" (`SIGWINCH`), so a raise can never
/// kill the test process. A process-directed signal may be taken by *any* thread of the
/// process, so this is not what makes a poll interruptible — see [`interrupt_thread`].
fn raise_self(signo: libc::c_int) {
    // SAFETY: `getpid` has no preconditions; `kill` with a valid pid and signo is well formed
    // and only posts a signal to this process.
    let rc = unsafe { libc::kill(libc::getpid(), signo) };
    assert_eq!(rc, 0, "kill(getpid(), {signo}) succeeds");
}

/// Delivers `signo` to one **named** thread.
///
/// `kill(getpid(), …)` posts a *process*-directed signal, which the kernel hands to whichever
/// thread can take it — in the Rust test harness that is normally the idle harness thread, so
/// it would never interrupt the thread blocked in `epoll_wait`. Addressing the thread
/// directly is what makes the interrupted wait (and therefore the `EINTR` retry) reproducible.
/// `thread` must be a live handle of a thread that installed (or at least tolerates) the
/// disposition for `signo`.
fn interrupt_thread(thread: libc::pthread_t, signo: libc::c_int) {
    // SAFETY: `thread` is a live `pthread_t` the owning thread captured itself, and `signo` is
    // a valid signal number this thread has a disposition for; `pthread_kill` only posts the
    // signal.
    let rc = unsafe { libc::pthread_kill(thread, signo) };
    assert_eq!(rc, 0, "pthread_kill({signo}) succeeds");
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

/// A handled signal raised on the process is observed as the signal self-pipe's readiness
/// through a real poll, **not** as a failure of the wait.
#[test]
fn a_handled_signal_wakes_the_poll_instead_of_failing_it() {
    let _guard = signal_guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    facility
        .set(Signal::SigUsr1, Disposition::Handle)
        .expect("install the trampoline");

    let mut poller = Poller::new().expect("a poll set");
    let key = 0x5151;
    let token = poller
        .register(facility.read_fd(), Interest::READABLE, key)
        .expect("the self-pipe registers");

    // Nothing has been delivered yet, so the wait below really blocks (and the raise really
    // interrupts it) rather than reporting a stale readiness.
    assert!(
        poller
            .poll(Some(Duration::from_millis(20)))
            .expect("poll succeeds")
            .is_empty(),
        "an idle self-pipe is not ready"
    );

    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            raise_self(libc::SIGUSR1);
        });

        let events = poller
            .poll(Some(Duration::from_secs(2)))
            .expect("a handled signal is never reported as a poll failure");
        assert_eq!(
            events.len(),
            1,
            "the self-pipe is the only ready registration"
        );
        assert_eq!(events[0].token(), token);
        assert_eq!(events[0].key(), key);
        assert!(events[0].ready().readable());
    });

    // The delivery has been observed (its readiness is what the poll returned), so restoring
    // the default action — which *terminates* on `SIGUSR1` — cannot catch a straggler.
    facility
        .set(Signal::SigUsr1, Disposition::Default)
        .expect("restore");
}

/// An interrupt that lands on the **waiting thread** is retried, so the pipe's readiness is
/// still what the poll returns.
///
/// `epoll_wait` is never restarted after a signal handler runs (`SA_RESTART` does not apply to
/// epoll/poll/select, `signal(7)`), and `mio` surfaces that `EINTR` unchanged: this is exactly
/// the interruption `Poller::poll` must absorb.
#[test]
fn an_interrupted_wait_is_retried_and_the_readiness_is_returned() {
    let _guard = signal_guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    facility
        .set(Signal::SigUsr1, Disposition::Handle)
        .expect("install the trampoline");

    let mut poller = Poller::new().expect("a poll set");
    let key = 0x5151;
    let token = poller
        .register(facility.read_fd(), Interest::READABLE, key)
        .expect("the self-pipe registers");

    // SAFETY: `pthread_self` has no preconditions and returns this thread's own handle, which
    // stays valid for as long as the thread runs (i.e. until this scope ends).
    let polling_thread = unsafe { libc::pthread_self() };

    std::thread::scope(|scope| {
        scope.spawn(move || {
            // Land the signal on the waiting thread while it is blocked in `epoll_wait`, so
            // the wait really is interrupted rather than racing with the entry into it.
            std::thread::sleep(Duration::from_millis(100));
            interrupt_thread(polling_thread, libc::SIGUSR1);
        });

        let events = poller
            .poll(Some(Duration::from_secs(2)))
            .expect("an interrupted wait is retried, not reported as a failure");
        assert_eq!(
            events.len(),
            1,
            "the retried wait returns the self-pipe readiness"
        );
        assert_eq!(events[0].token(), token);
        assert_eq!(events[0].key(), key);
        assert!(events[0].ready().readable());
    });

    // The delivery has been observed (it is what woke the wait), so restoring the default
    // action — which *terminates* on `SIGUSR1` — cannot catch a straggler.
    facility
        .set(Signal::SigUsr1, Disposition::Default)
        .expect("restore");
}

/// An interrupted `poll(Some(..))` with nothing ready must not surface the interruption as an
/// error, and the retry must preserve the deadline: repeated signals can only ever shorten the
/// remaining budget, never restart it.
///
/// The signal's self-pipe is deliberately **not** registered with the poller, so there is no
/// event to return and the only observable outcome is how long the call waits.
#[test]
fn an_interrupted_poll_keeps_waiting_and_keeps_its_deadline() {
    let _guard = signal_guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    // `SIGWINCH`'s default action is ignore, so a straggler after the restore is harmless.
    facility
        .set(Signal::SigWinch, Disposition::Handle)
        .expect("install the trampoline");

    let mut poller = Poller::new().expect("a poll set");
    let timeout = Duration::from_millis(200);

    // SAFETY: as above — this thread's own, live handle.
    let polling_thread = unsafe { libc::pthread_self() };

    let mut waited = Duration::ZERO;
    std::thread::scope(|scope| {
        scope.spawn(move || {
            // Deliveries every 15 ms for 300 ms, each aimed at the waiting thread: a retry
            // that restarted the full timeout would not return until ~500 ms.
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(15));
                interrupt_thread(polling_thread, libc::SIGWINCH);
            }
        });

        let started = Instant::now();
        let events = poller
            .poll(Some(timeout))
            .expect("an interrupted poll is retried, not reported as a failure");
        waited = started.elapsed();
        assert!(events.is_empty(), "no registration is ready");
    });

    assert!(
        waited >= Duration::from_millis(150),
        "the interrupts must not cut the wait short, waited only {waited:?}"
    );
    assert!(
        waited < Duration::from_millis(350),
        "the retry must preserve the deadline, waited {waited:?}"
    );

    facility
        .set(Signal::SigWinch, Disposition::Default)
        .expect("restore");
}
