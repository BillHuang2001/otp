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

//! Integration tests for the OS-signal facility over **real signals and a real poller**.
//!
//! The facility is process-global (one installed disposition set, one self-pipe write end),
//! so every test here takes the same global [`Mutex`] before touching it. The Rust test
//! harness runs tests on parallel threads; without the guard two tests could install
//! orthogonal `SIGUSR1` dispositions and observe each other's deliveries. The lock makes the
//! whole suite serial, which is the documented single-instance rule made concrete.
//!
//! Every raise targets the test process itself with `libc::kill(getpid(), …)` — no child
//! process is ever spawned to deliver a signal. A raised signal whose disposition is `Handle`
//! is always observed through the self-pipe before the facility is allowed to restore
//! `SIG_DFL`, so a straggler delivery cannot terminate the test process; the coalescing and
//! two-signal tests deliberately use signals whose default action is *not* "terminate"
//! (`SIGWINCH`, `SIGCONT`).

use std::os::unix::io::RawFd;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use beam_common::BeamError;
use beam_io::prelude::*;

/// Serializes the whole suite: the facility is process-global, so the tests must not run
/// concurrently. A poisoned lock (a test panicked while holding it) is recovered — the lock
/// guards scheduling, not shared Rust data.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Acquires the process-global test guard.
fn guard() -> MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

/// Waits (bounded) until `fd` reports readability — i.e. the trampoline wrote a byte.
///
/// A signal delivered while the calling thread is blocked in `poll(2)` makes `poll` fail
/// with `EINTR` (`poll` is never restarted by `SA_RESTART`); the handler has already written
/// its byte by then, so an interrupted wait is retried until the deadline.
fn wait_readable(fd: RawFd, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a valid, writable `pollfd`; `poll` reads/writes it and retains no
        // pointer beyond the call.
        let rc = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as libc::c_int) };
        if rc > 0 {
            return (pfd.revents & libc::POLLIN) != 0;
        }
        if rc == 0 || std::time::Instant::now() >= deadline {
            return false;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
}

/// Polls the real [`Poller`] (retrying an `EINTR`-interrupted wait) until it reports
/// readability for `token`/`key`, within `timeout`.
fn poll_until_readable(poller: &mut Poller, token: Token, key: u64, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match poller.poll(Some(remaining.max(Duration::from_millis(1)))) {
            Ok(events) => {
                if events.iter().any(|event| {
                    event.token() == token && event.key() == key && event.ready().readable()
                }) {
                    return true;
                }
                if std::time::Instant::now() >= deadline {
                    return false;
                }
            }
            // A handled signal interrupts `epoll_wait` with `EINTR`; retry until the deadline.
            Err(BeamError::Io(_)) => {
                if std::time::Instant::now() >= deadline {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
}

/// Raises `signo` on the current process.
///
/// # Errors
///
/// Panics if `kill(2)` fails (it never should for a valid signo targeting self).
fn raise_self(signo: libc::c_int) {
    // SAFETY: `getpid` has no preconditions; `kill` with a valid pid and signo is well
    // formed and only posts a signal to this process.
    let rc = unsafe { libc::kill(libc::getpid(), signo) };
    assert_eq!(rc, 0, "kill(getpid(), {signo}) succeeds");
}

/// Drains repeatedly (bounded) until at least `want` distinct signals have been reported,
/// then returns them in [`Signal::ALL`] order.
///
/// A raised signal is delivered asynchronously, so a single `drain` immediately after two
/// `kill`s may observe only the first (or second) arrival. Polling the drain until the
/// expected number of distinct signals is in hand removes that race; the 2 s bound only
/// exists so a genuinely lost signal fails the assertion instead of hanging.
fn drain_distinct(facility: &mut SignalFacility, want: usize, timeout: Duration) -> Vec<Signal> {
    let deadline = std::time::Instant::now() + timeout;
    let mut seen: Vec<Signal> = Vec::new();
    loop {
        for sig in facility.drain() {
            if !seen.contains(&sig) {
                seen.push(sig);
            }
        }
        if seen.len() >= want || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Signal::ALL
        .iter()
        .copied()
        .filter(|sig| seen.contains(sig))
        .collect()
}

#[test]
fn signal_names_round_trip_for_all_twelve() {
    assert_eq!(Signal::ALL.len(), 12);
    for sig in Signal::ALL {
        assert_eq!(Signal::from_name(sig.name()), Some(sig), "{}", sig.name());
    }
}

#[test]
fn unknown_signal_names_are_none() {
    assert_eq!(Signal::from_name("sigfoo"), None);
    assert_eq!(Signal::from_name(""), None);
    assert_eq!(Signal::from_name("SIGHUP"), None, "atoms are lowercase");
    // The oracle rejects these; they are not part of the accepted set.
    assert_eq!(Signal::from_name("sigttin"), None);
    assert_eq!(Signal::from_name("sigttou"), None);
    assert_eq!(Signal::from_name("sigint"), None);
}

#[test]
fn disposition_names_round_trip() {
    for disp in Disposition::ALL {
        assert_eq!(Disposition::from_name(disp.name()), Some(disp));
    }
    assert_eq!(Disposition::from_name("ignore "), None);
    assert_eq!(
        Disposition::from_name("default"),
        Some(Disposition::Default)
    );
}

#[test]
fn set_reports_each_disposition_back() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    for disp in Disposition::ALL {
        facility.set(Signal::SigUsr1, disp).expect("install");
        assert_eq!(facility.disposition(Signal::SigUsr1).expect("query"), disp);
    }
    facility
        .set(Signal::SigUsr1, Disposition::Default)
        .expect("restore");
}

#[test]
fn a_restored_disposition_reports_default() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    facility
        .set(Signal::SigUsr1, Disposition::Handle)
        .expect("install handle");
    facility
        .set(Signal::SigUsr1, Disposition::Default)
        .expect("restore default");
    assert_eq!(
        facility.disposition(Signal::SigUsr1).expect("query"),
        Disposition::Default
    );
}

#[test]
fn a_handled_signal_is_really_delivered_and_drained() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    facility
        .set(Signal::SigUsr1, Disposition::Handle)
        .expect("install the trampoline");

    raise_self(libc::SIGUSR1);
    assert!(
        wait_readable(facility.read_fd(), Duration::from_secs(2)),
        "the trampoline writes to the self-pipe"
    );
    assert_eq!(facility.drain(), vec![Signal::SigUsr1]);
    assert!(facility.drain().is_empty(), "the bit is consumed once");

    facility
        .set(Signal::SigUsr1, Disposition::Default)
        .expect("restore");
}

#[test]
fn repeated_deliveries_of_one_signal_are_coalesced() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    // `SIGWINCH`'s default action is ignore: a straggler after the restore is harmless.
    facility
        .set(Signal::SigWinch, Disposition::Handle)
        .expect("install the trampoline");

    for _ in 0..3 {
        raise_self(libc::SIGWINCH);
    }
    assert!(wait_readable(facility.read_fd(), Duration::from_secs(2)));
    // Distinct-signal, not distinct-delivery: reported exactly once.
    assert_eq!(facility.drain(), vec![Signal::SigWinch]);
}

#[test]
fn two_distinct_signals_are_both_reported() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    // Both signals have non-terminating default actions, so restoring is safe.
    facility
        .set(Signal::SigWinch, Disposition::Handle)
        .expect("install winch");
    facility
        .set(Signal::SigCont, Disposition::Handle)
        .expect("install cont");

    raise_self(libc::SIGWINCH);
    raise_self(libc::SIGCONT);
    // Reported in `Signal::ALL` order, once each — waiting for both deliveries.
    assert_eq!(
        drain_distinct(&mut facility, 2, Duration::from_secs(2)),
        vec![Signal::SigCont, Signal::SigWinch]
    );
}

#[test]
fn an_ignored_signal_is_not_reported_and_the_process_survives() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    facility
        .set(Signal::SigUsr2, Disposition::Ignore)
        .expect("install ignore");
    assert_eq!(
        facility.disposition(Signal::SigUsr2).expect("query"),
        Disposition::Ignore
    );

    raise_self(libc::SIGUSR2);
    std::thread::sleep(Duration::from_millis(50));
    assert!(
        facility.drain().is_empty(),
        "an ignored signal never reaches the trampoline"
    );
    // Reaching this point proves the process survived the raise.
}

#[test]
fn a_blocked_poller_is_woken_by_a_handled_signal() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    facility
        .set(Signal::SigUsr1, Disposition::Handle)
        .expect("install the trampoline");

    let mut poller = Poller::new().expect("a poll set");
    let key = 0x5151;
    let token = poller
        .register(facility.read_fd(), Interest::READABLE, key)
        .expect("the self-pipe registers");

    // No delivery yet: a short poll reports nothing.
    assert!(
        poller
            .poll(Some(Duration::from_millis(20)))
            .expect("poll succeeds")
            .is_empty(),
        "an idle self-pipe is not ready"
    );

    raise_self(libc::SIGUSR1);
    assert!(
        poll_until_readable(&mut poller, token, key, Duration::from_secs(2)),
        "the poller observes the self-pipe readiness for our token"
    );

    facility
        .set(Signal::SigUsr1, Disposition::Default)
        .expect("restore");
}

#[test]
fn drop_restores_the_prior_disposition() {
    let _guard = guard();
    let before = {
        let mut facility = SignalFacility::new().expect("a self-pipe");
        let before = facility.disposition(Signal::SigUsr1).expect("query");
        facility
            .set(Signal::SigUsr1, Disposition::Ignore)
            .expect("install ignore");
        assert_eq!(
            facility.disposition(Signal::SigUsr1).expect("query"),
            Disposition::Ignore
        );
        before
    };
    let facility = SignalFacility::new().expect("a self-pipe");
    assert_eq!(
        facility.disposition(Signal::SigUsr1).expect("query"),
        before,
        "drop restores what this facility changed"
    );
}

#[test]
fn the_facility_can_be_dropped_and_recreated() {
    let _guard = guard();
    let before = {
        let mut facility = SignalFacility::new().expect("a self-pipe");
        let before = facility.disposition(Signal::SigWinch).expect("query");
        facility
            .set(Signal::SigWinch, Disposition::Ignore)
            .expect("install ignore");
        assert!(facility.read_fd() >= 0);
        before
    };

    // A second facility sees the restored state and a fresh, live self-pipe.
    let mut facility = SignalFacility::new().expect("a self-pipe");
    assert!(facility.read_fd() >= 0, "a real descriptor is exposed");
    assert_eq!(
        facility.disposition(Signal::SigWinch).expect("query"),
        before
    );
    // And it still works end-to-end after the recreation.
    facility
        .set(Signal::SigWinch, Disposition::Handle)
        .expect("install the trampoline");
    raise_self(libc::SIGWINCH);
    assert!(wait_readable(facility.read_fd(), Duration::from_secs(2)));
    assert_eq!(facility.drain(), vec![Signal::SigWinch]);
}
