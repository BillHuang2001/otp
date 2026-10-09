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
//! Every raise targets the test process itself — no child process is ever spawned to deliver
//! a signal. A raise goes either to the whole process (`raise_self`, `libc::kill(getpid(), …)`)
//! or, where a test needs deterministic delivery, to the **calling thread** (`raise_thread`,
//! `pthread_kill(pthread_self(), …)` — POSIX delivers at least one of the target thread's
//! unblocked pending signals *before* it returns, so the handler has always run by the next
//! statement). A raised signal whose disposition is `Handle` is always observed through the
//! self-pipe before the facility is allowed to restore `SIG_DFL`, so a straggler delivery
//! cannot terminate the test process; the coalescing and multi-signal tests deliberately use
//! signals whose default action is *not* "terminate" (`SIGWINCH`, `SIGCONT`, `SIGCHLD`).

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

/// Raises `signo` on the **calling thread** and returns only once its disposition has run.
///
/// `pthread_kill(self, …)` is used instead of `kill(getpid(), …)` whenever a test needs
/// deterministic delivery: a process-directed `kill` may be handed to *any* thread that does
/// not block the signal (in a libtest binary, typically an idle harness thread), whereas POSIX
/// requires `pthread_kill` to deliver at least one of the target thread's unblocked pending
/// signals **before it returns**. As the target is this thread and the signal is unblocked, the
/// trampoline has always returned by the next statement — so `drain` right afterwards observes
/// every delivery of the burst, with no sleeping and no polling.
///
/// # Errors
///
/// Panics if `pthread_kill(2)` fails (it never should for a valid signo targeting self).
fn raise_thread(signo: libc::c_int) {
    // SAFETY: `pthread_self` has no preconditions; `pthread_kill` to the calling thread with a
    // valid signal number is well formed and only posts a signal to this thread.
    let rc = unsafe { libc::pthread_kill(libc::pthread_self(), signo) };
    assert_eq!(rc, 0, "pthread_kill(self, {signo}) succeeds");
}

/// The number of bytes currently readable on `fd` — `ioctl(FIONREAD)`.
///
/// Used to prove the coalescing rule's direction: the self-pipe can hold one byte per
/// delivery while `drain` reports one entry per *distinct* signal, so the byte count is not
/// the source of truth.
fn bytes_available(fd: RawFd) -> libc::c_int {
    let mut count: libc::c_int = 0;
    // SAFETY: `FIONREAD` writes one `c_int` through the out-pointer, which is valid and
    // writable for the call, and `fd` is a live descriptor.
    let rc = unsafe { libc::ioctl(fd, libc::FIONREAD, std::ptr::addr_of_mut!(count)) };
    assert_eq!(rc, 0, "FIONREAD succeeds on a live pipe");
    count
}

/// `fcntl(fd, F_GETFD)`: `None` when `fd` names a live descriptor, else the failing errno
/// (`EBADF` for a descriptor `Drop` has already closed).
fn fcntl_errno(fd: RawFd) -> Option<i32> {
    // SAFETY: `fcntl` with `F_GETFD` only inspects/reports the (possibly closed) descriptor
    // number, takes no pointer and has no other precondition.
    let rc = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if rc >= 0 {
        None
    } else {
        std::io::Error::last_os_error().raw_os_error()
    }
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

#[test]
fn the_rejected_names_are_exactly_the_ones_the_oracle_rejects() {
    // The C's `!`-commented rows plus the `#ifdef SIGINFO` row (`signalterm_to_signum`,
    // `erts/emulator/sys/unix/sys.c`). The oracle answers `badarg` for every one of them, and
    // this crate deliberately excludes the platform-conditional `siginfo` on all targets.
    for rejected in [
        "sigint", "sigill", "sigsegv", "sigttin", "sigttou", "siginfo", "sigkill", "sigpipe",
        "sigurg", "sigio", "sigusr3", "sigfoo", "sighups", "sigtstp ", " sigtstp",
    ] {
        assert_eq!(
            Signal::from_name(rejected),
            None,
            "{rejected:?} is rejected"
        );
    }
}

#[test]
fn every_signal_owns_a_distinct_signo() {
    for (index, sig) in Signal::ALL.iter().enumerate() {
        assert_eq!(
            Signal::from_signo(sig.signo()),
            Some(*sig),
            "{} resolves from its signo",
            sig.name()
        );
        for other in &Signal::ALL[index + 1..] {
            assert_ne!(
                sig.signo(),
                other.signo(),
                "{} and {} must not share a signo",
                sig.name(),
                other.name()
            );
        }
    }
    // A signo this facility does not manage maps to nothing.
    assert_eq!(Signal::from_signo(libc::SIGINT), None);
    assert_eq!(Signal::from_signo(0), None);
    assert_eq!(Signal::from_signo(-1), None);
}

#[test]
fn three_signals_are_reported_in_all_order_not_arrival_order() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    // All three have non-terminating default actions (winch: ignore, cont: continue, chld:
    // ignore), so a straggler after `Drop` restores the default cannot terminate the test.
    for sig in [Signal::SigWinch, Signal::SigCont, Signal::SigChld] {
        facility
            .set(sig, Disposition::Handle)
            .expect("install the trampoline");
    }

    // Arrival order is the exact reverse of `Signal::ALL` order, and every delivery has
    // completed its handler by the time `drain` runs.
    raise_thread(libc::SIGWINCH);
    raise_thread(libc::SIGCONT);
    raise_thread(libc::SIGCHLD);

    assert_eq!(
        facility.drain(),
        vec![Signal::SigChld, Signal::SigCont, Signal::SigWinch],
        "one entry per distinct signal, in Signal::ALL order"
    );
}

#[test]
fn interleaved_repeats_of_two_signals_coalesce_to_one_entry_each() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    facility
        .set(Signal::SigWinch, Disposition::Handle)
        .expect("install winch");
    facility
        .set(Signal::SigCont, Disposition::Handle)
        .expect("install cont");

    // Six deliveries, interleaved: three of each. An implementation that reported once per
    // *delivery*, or in *arrival* order, could not produce the ALL-ordered single-entry pair
    // asserted below.
    for _ in 0..3 {
        raise_thread(libc::SIGWINCH);
        raise_thread(libc::SIGCONT);
    }

    assert_eq!(facility.drain(), vec![Signal::SigCont, Signal::SigWinch]);
    assert!(
        facility.drain().is_empty(),
        "none of the six deliveries is reported twice"
    );
}

#[test]
fn each_drain_interval_reports_its_own_delivery() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    facility
        .set(Signal::SigWinch, Disposition::Handle)
        .expect("install the trampoline");

    // "Exactly once per drain *interval*": a fresh delivery after a drain is a fresh report.
    raise_thread(libc::SIGWINCH);
    assert_eq!(facility.drain(), vec![Signal::SigWinch], "first interval");
    raise_thread(libc::SIGWINCH);
    assert_eq!(
        facility.drain(),
        vec![Signal::SigWinch],
        "a delivery after a drain is its own report"
    );
    assert!(
        facility.drain().is_empty(),
        "but still only once per interval"
    );
}

#[test]
fn the_pipe_byte_count_is_not_the_source_of_truth() {
    let _guard = guard();
    let mut facility = SignalFacility::new().expect("a self-pipe");
    facility
        .set(Signal::SigWinch, Disposition::Handle)
        .expect("install the trampoline");

    // Eight synchronous deliveries ⇒ eight separate `write(2)` calls; a pipe never coalesces
    // writes, so the self-pipe physically holds one byte per delivery ...
    for _ in 0..8 {
        raise_thread(libc::SIGWINCH);
    }
    let bytes = bytes_available(facility.read_fd());
    assert!(
        bytes >= 2,
        "the self-pipe holds one byte per delivery, not {bytes}"
    );

    // ... yet `drain` reports one entry per *distinct* signal, not one per byte, and empties
    // the pipe so its readiness does not persist.
    assert_eq!(facility.drain(), vec![Signal::SigWinch]);
    assert!(
        facility.drain().is_empty(),
        "no byte produced a second report"
    );
    assert_eq!(
        bytes_available(facility.read_fd()),
        0,
        "`drain` consumed every byte"
    );
}

#[test]
fn drop_restores_every_disposition_it_changed() {
    let _guard = guard();
    let signals = [
        Signal::SigUsr1,
        Signal::SigUsr2,
        Signal::SigWinch,
        Signal::SigCont,
    ];
    let before: Vec<Disposition> = {
        let mut facility = SignalFacility::new().expect("a self-pipe");
        let before = signals
            .iter()
            .map(|sig| facility.disposition(*sig).expect("query"))
            .collect();
        for sig in signals {
            facility
                .set(sig, Disposition::Ignore)
                .expect("install ignore");
        }
        for sig in signals {
            assert_eq!(
                facility.disposition(sig).expect("query"),
                Disposition::Ignore,
                "{} was changed",
                sig.name()
            );
        }
        before
    };

    let facility = SignalFacility::new().expect("a self-pipe");
    for (sig, want) in signals.iter().zip(before) {
        assert_eq!(
            facility.disposition(*sig).expect("query"),
            want,
            "drop restores {} to its pre-facility disposition",
            sig.name()
        );
    }
}

#[test]
fn drop_restores_the_prior_from_before_the_first_change() {
    let _guard = guard();
    let before = {
        let mut facility = SignalFacility::new().expect("a self-pipe");
        let before = facility.disposition(Signal::SigWinch).expect("query");
        // Three changes to the same signal: only the *first* prior is the truth, so a `set`
        // that captured the previous action every time would restore `Ignore` below instead.
        facility
            .set(Signal::SigWinch, Disposition::Ignore)
            .expect("install ignore");
        facility
            .set(Signal::SigWinch, Disposition::Handle)
            .expect("install handle");
        facility
            .set(Signal::SigWinch, Disposition::Ignore)
            .expect("install ignore again");
        assert_eq!(
            facility.disposition(Signal::SigWinch).expect("query"),
            Disposition::Ignore,
            "the last change is live before the drop"
        );
        before
    };

    let facility = SignalFacility::new().expect("a self-pipe");
    assert_eq!(
        facility.disposition(Signal::SigWinch).expect("query"),
        before,
        "drop restores the disposition from before the *first* change, not the last"
    );
}

#[test]
fn drop_closes_the_self_pipe_and_the_facility_can_be_recreated() {
    let _guard = guard();
    let read_fd = {
        let facility = SignalFacility::new().expect("a self-pipe");
        let read_fd = facility.read_fd();
        assert!(read_fd >= 0, "a real descriptor is exposed");
        assert_eq!(fcntl_errno(read_fd), None, "the read end is live");
        read_fd
    };

    // `Drop` closed it: the number no longer names an open descriptor, so a later readiness
    // report for it is impossible.
    assert_eq!(
        fcntl_errno(read_fd),
        Some(libc::EBADF),
        "drop closes the self-pipe's read end"
    );

    // ... and the facility can be recreated and used end-to-end afterwards.
    let mut facility = SignalFacility::new().expect("a self-pipe");
    facility
        .set(Signal::SigWinch, Disposition::Handle)
        .expect("install the trampoline");
    raise_thread(libc::SIGWINCH);
    assert_eq!(facility.drain(), vec![Signal::SigWinch]);
}
