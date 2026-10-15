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

//! The **delivery** half of `os:set_signal/2`: the process's one [`SignalFacility`], its
//! registration with the runtime's port poller, and the drain that turns a handled signal into
//! a value the caller can deliver.
//!
//! [`super::PortRegistry`] owns the runtime's one poller, so this module — a child of the port
//! layer — is where the signal self-pipe's readiness enters the runtime. The *answer surface*
//! (`os:set_signal/2`'s `{ok, OldDisposition}`/`badarg` and the kernel-only `sigstop` rule)
//! stays in [`crate::context::signal`], which owns the caller's validation and delegates the
//! facility to [`install`] here.
//!
//! This module is `pub(crate)` for exactly one reason: `crate::context::mod` declares its
//! `signal` child **privately**, so [`crate::context::signal`] can name `crate::port::watch`
//! (a `pub mod port`'s child) but not the other way round.
//!
//! # Ownership and lifetime: one facility for the process
//!
//! An Erlang signal disposition is **process-global** in ERTS: `os:set_signal/2` ends in
//! `sys_signal` (`erts/emulator/sys/unix/sys.c`), a plain `sigaction(2)` on the emulator
//! process, so the disposition outlives the calling Erlang process and is shared by every
//! process in the node. The Rust runtime reproduces that with **one** [`SignalFacility`] held
//! in the `thread_local!` slot [`FACILITY`], created lazily by the first `os:set_signal/2` and
//! never dropped while the runtime lives — so a later call observes exactly the disposition an
//! earlier one installed (a per-call facility would be dropped at the end of the call, and
//! `SignalFacility::drop` restores the prior dispositions; see
//! `crate::context::signal`'s `the_facility_is_shared_across_calls`).
//!
//! A `thread_local` (rather than a field of [`Runtime`](crate::Runtime)) is sound for exactly
//! the reason [`crate::context::trace`]'s slot is: the runtime is single-threaded (`!Send` — it
//! owns `Rc`/`RefCell` tables and raw `*mut Eterm` heaps), so this slot *is* the process's one
//! facility in practice. `beam_io` documents the single-instance rule the other way round: a
//! second live facility re-points the trampoline's write end at its own pipe, which this module
//! never allows.
//!
//! # The oracle's delivery path
//!
//! In ERTS a caught signal is written to a self-pipe (`smp_sig_notify`); the signal dispatcher
//! reads it and calls `signal_notify_requested(signum)`
//! (`erts/emulator/sys/unix/sys.c:589-604`), which resolves
//! `erts_whereis_name_to_id(NULL, am_erl_signal_server)` and — only if that process exists —
//! queues `{notify, SigName}` to it with `am_system` as the sender. If no such process exists,
//! nothing is sent.
//!
//! [`SignalWatch`] reproduces the mechanism: it registers the facility's read end
//! ([`SignalFacility::read_fd`]) with the runtime's poller under the sentinel key
//! [`SIGNAL_KEY`], and [`SignalWatch::drain`] consumes the handled signals and names them. The
//! *message* (`{notify, SignalName}`, sender `system`) is built and delivered by the port
//! layer's two message pumps — see [`crate::port::PortEvent::Signal`].
//!
//! Registering the descriptor only *wakes* a blocked poll; the record of what arrived is
//! `beam_io`'s process-global bitmask (the pipe bytes are deliberately discarded — see
//! `beam_io::signal`'s coalescing policy). The drain is therefore **unconditional** at each
//! poll step rather than keyed to the readiness event: a signal delivered while the runtime was
//! not polling its own descriptor is still reported, exactly as ERTS's bitmask reports it.
//!
//! # An armed-but-idle watch is not pending work
//!
//! The self-pipe exists so the **pipe byte** answers one question — *has anything been caught?*
//! — while the bitmask answers *what*: `beam_io`'s trampoline is lock-free and async-signal-safe
//! and so may only write the ready-made byte, and [`SignalWatch::drain`] clears both the pipe and
//! the bitmask. That makes readiness a sound, **non-destructive** probe of "a signal was caught
//! and no drain has consumed it yet" ([`SignalWatch::has_pending_signal`]) — the question the
//! scheduler's quiescence test asks.
//!
//! The distinction matters because an *armed* `handle` disposition whose pipe holds **no bytes**
//! is an **idle descriptor**: it is not pending work, and a round that blocks on it is pure
//! sleep. A pipe that **has** bytes is pending work and must still be delivered. The probe never
//! decides *whether* a signal is delivered: [`crate::port::PortRegistry::pump`] harvests the
//! facility unconditionally at the top of every round (`take_signals` → [`SignalWatch::drain`]),
//! so a caught signal is delivered whether or not a round blocked on the pipe. The probe only
//! decides whether a round needs to run at all, or may block.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::os::fd::RawFd;
use std::time::Duration;

use beam_common::Result;
use beam_io::signal::{Disposition, Signal, SignalFacility};
use beam_io::{Event, Interest, Poller, Token};

/// The poller key the signal self-pipe is registered under.
///
/// It must never collide with a **port** key: `beam_io::PortRegistry::schedule_events` maps an
/// event's key to a port and silently ignores an unknown one, and
/// `beam_process::PortTable::alloc` only ever hands out numbers below `1 << 60`. `u64::MAX` is
/// the poller's own [`beam_io::WAKER_KEY`], so the sentinel sits immediately below it — outside
/// the port-number space and outside every other reserved key. The port layer also *filters*
/// this key out before handing a poll batch to the port scheduler, so nothing but
/// [`SignalWatch`] can ever see it.
pub(crate) const SIGNAL_KEY: u64 = u64::MAX - 1;

/// The one [`SignalFacility`] of a runtime, held in the `FACILITY` slot — and *leaked* instead
/// of dropped by a **test** build.
///
/// [`SignalFacility::drop`] restores every disposition the facility changed, and a signal
/// disposition is **process-global** (that is the whole point of the module). In production the
/// slot is dropped exactly once, when the runtime's single thread exits, and restoring the priors
/// there is the correct behaviour.
///
/// A libtest thread, by contrast, is torn down the instant its test function returns — while
/// sibling signal tests keep running in the same process. That teardown's restore would set, say,
/// `SIGUSR1` back to `SIG_DFL` between another test's `kill(1)` and its handler, and `SIGUSR1`'s
/// default action terminates the whole test binary (`signal: 10`). It is not part of any test
/// body, so the shared `test_guard` lock cannot exclude it.
///
/// The **test** build therefore leaks the facility at that point. The teardown drop is the only
/// `sigaction(2)` call outside a guarded test body, so removing it leaves every disposition change
/// inside a test that holds `test_guard` — and the dispositions are what the tests assert on.
/// Production keeps the restoring `Drop` (the runtime is single-threaded and never drops the
/// facility itself, so the leak never happens there anyway).
struct FacilitySlot(Option<SignalFacility>);

impl FacilitySlot {
    /// An empty slot, before any `os:set_signal/2`.
    const fn new() -> FacilitySlot {
        FacilitySlot(None)
    }

    /// The live facility, or `None` before the first `os:set_signal/2`.
    fn get_mut(&mut self) -> Option<&mut SignalFacility> {
        self.0.as_mut()
    }

    /// Fills an empty slot and hands the freshly created facility back.
    fn set(&mut self, facility: SignalFacility) -> &mut SignalFacility {
        self.0.insert(facility)
    }
}

/// A test build leaks the facility rather than dropping it, so a libtest thread's teardown can
/// never restore the process-global dispositions under a concurrently running signal test (see
/// [`FacilitySlot`]).
///
/// `mem::forget` keeps the facility — and therefore its self-pipe, which the process-global
/// trampoline still points at — alive for the rest of the test binary, exactly as if the runtime
/// thread had not exited.
#[cfg(test)]
impl Drop for FacilitySlot {
    fn drop(&mut self) {
        if let Some(facility) = self.0.take() {
            std::mem::forget(facility);
        }
    }
}

thread_local! {
    /// The runtime's one long-lived signal facility, created by the first `os:set_signal/2`
    /// and kept for the process's life (see the module docs on the single-instance rule).
    ///
    /// A [`FacilitySlot`] rather than a bare `Option<SignalFacility>` because the constructor
    /// is fallible (the first call either fills the slot or reports the pipe-creation failure,
    /// and a failed creation leaves the slot empty so a later call can try again) *and*
    /// because a test build must not let the facility's `Drop` run (see [`FacilitySlot`]).
    static FACILITY: RefCell<FacilitySlot> = const { RefCell::new(FacilitySlot::new()) };

    /// The signals whose **current** disposition is [`Disposition::Handle`], mirrored from the
    /// facility by [`install`].
    ///
    /// The mirror exists so [`SignalWatch::is_pollable`] can answer without a syscall, and it is
    /// a *precise* mirror: a later `ignore`/`default` for the same signal removes the entry
    /// again. It matters because only a `handle` disposition makes the self-pipe readable —
    /// `beam_io::signal`'s trampoline writes the pipe only for a signal whose handler it
    /// installed — so a facility that has only ever seen `ignore`/`default` can never produce a
    /// notification and must not keep the runtime polling.
    static HANDLED: RefCell<BTreeSet<Signal>> = RefCell::new(BTreeSet::new());
}

/// Installs `disposition` for `signal` and records it in the handled-signal mirror — the
/// mechanism behind `os:set_signal/2`'s `sigaction(2)`.
///
/// Argument validation (the oracle's `badarg`) and the kernel-only `sigstop` rule belong to the
/// caller ([`crate::context::signal`]), which holds the caller's atom table; this function is
/// total over the two closed enums.
///
/// # Errors
///
/// Only a genuine I/O failure from a refused `pipe`/`sigaction` syscall. A failed install leaves
/// the mirror untouched, so a signal the OS refused is not believed to be handled (and the
/// facility slot, if the failure was the pipe creation, stays empty so the next call retries).
pub(crate) fn install(signal: Signal, disposition: Disposition) -> Result<()> {
    with_facility(|facility| facility.set(signal, disposition))?;
    HANDLED.with(|handled| {
        let mut handled = handled.borrow_mut();
        if disposition == Disposition::Handle {
            handled.insert(signal);
        } else {
            handled.remove(&signal);
        }
    });
    Ok(())
}

/// Runs the fallible `f` against the process's one signal facility, creating it on first use.
///
/// # Errors
///
/// A failed creation of the facility (the self-pipe) is a real I/O failure and is reported,
/// never swallowed; the slot stays empty so the next call retries. `f`'s own failure is
/// returned untouched.
pub(crate) fn with_facility<R>(f: impl FnOnce(&mut SignalFacility) -> Result<R>) -> Result<R> {
    FACILITY.with(|slot| {
        let mut slot = slot.borrow_mut();
        let facility = match slot.get_mut() {
            Some(facility) => facility,
            None => slot.set(SignalFacility::new()?),
        };
        f(facility)
    })
}

/// Runs `f` against the facility **if one exists**, without creating one.
///
/// `None` when no `os:set_signal/2` has run yet. Creating a facility here would open a
/// self-pipe (and supersede the trampoline's write end) for a runtime that never asked for
/// signal handling, so the read-only probes of this module deliberately never do.
fn with_existing_facility<R>(f: impl FnOnce(&mut SignalFacility) -> R) -> Option<R> {
    FACILITY.with(|slot| slot.borrow_mut().get_mut().map(f))
}

/// Whether any signal's current disposition is [`Disposition::Handle`] (see [`HANDLED`]).
fn has_handled_signal() -> bool {
    HANDLED.with(|handled| !handled.borrow().is_empty())
}

/// The runtime's registration of the signal self-pipe with its one port poller.
///
/// Owned by [`super::PortRegistry`] (the poller's owner) rather than by the thread-local
/// facility, because a poller token is only meaningful for the poll set it was minted from.
/// The watch is *lazy*: the first `os:set_signal/2` creates the facility, so the descriptor to
/// register does not exist when the registry is built — [`SignalWatch::sync`] registers it on
/// the first poll round after a `handle` disposition has been installed.
#[derive(Debug)]
pub(crate) struct SignalWatch {
    /// The poller token of the registered self-pipe, once one exists.
    token: Option<Token>,
    /// The descriptor `token` names (the facility's self-pipe read end).
    fd: Option<RawFd>,
}

impl SignalWatch {
    /// A watch with nothing registered.
    pub(crate) const fn new() -> SignalWatch {
        SignalWatch {
            token: None,
            fd: None,
        }
    }

    /// Whether the self-pipe is registered with the poller.
    pub(crate) fn is_registered(&self) -> bool {
        self.token.is_some()
    }

    /// Whether [`SignalWatch::sync`] has a descriptor to **register** — the poll-set half of the
    /// watch, *not* the "is there work to wait for?" question (that is
    /// [`SignalWatch::has_pending_signal`]).
    ///
    /// True when the self-pipe is already registered, **or** when a `handle` disposition is
    /// installed — in which case [`SignalWatch::sync`] will register the descriptor, so a
    /// caught signal can still be observed even with **zero** live ports. A facility that has
    /// only ever seen `ignore`/`default` is not pollable: nothing can ever make its pipe
    /// readable.
    pub(crate) fn is_pollable(&self) -> bool {
        self.is_registered() || has_handled_signal()
    }

    /// Whether the trampoline has **caught** a signal that no [`drain`](Self::drain) has
    /// consumed yet — a **non-destructive** readiness probe of the self-pipe.
    ///
    /// The record of *what* arrived is `beam_io`'s process-global bitmask, which only
    /// `SignalFacility::drain` reads (and clears); the self-pipe is the record of *whether*
    /// anything arrived — the lock-free, async-signal-safe trampoline writes it on the signal's
    /// way in, and [`drain`](Self::drain) clears both. This probe therefore asks the **pipe**
    /// (a throwaway [`Poller`] registered on the facility's read end and polled with a zero
    /// timeout, so it neither blocks nor consumes) rather than the bitmask: draining would
    /// destroy the very evidence the caller needs.
    ///
    /// That caller is the scheduler's quiescence test. An **armed but idle** `handle`
    /// disposition is not pending work (its pipe can only become readable when a signal
    /// actually arrives), whereas a pipe holding bytes *is* — a caught signal not yet
    /// delivered. The probe decides only whether a round needs to run, never whether a signal
    /// is delivered: [`crate::port::PortRegistry::pump`] drains the facility unconditionally at
    /// the top of every round, so a caught signal is harvested whether or not a round blocked.
    ///
    /// `false` when there is no facility (no `os:set_signal/2` has run, so nothing can have been
    /// caught) or when the throwaway probe cannot be built — the honest "no evidence of a caught
    /// signal", never a fabricated readiness.
    pub(crate) fn has_pending_signal(&self) -> bool {
        let Some(fd) = with_existing_facility(|facility| facility.read_fd()) else {
            return false;
        };
        let Ok(mut poller) = Poller::new() else {
            return false;
        };
        if poller.register(fd, Interest::READABLE, SIGNAL_KEY).is_err() {
            return false;
        }
        match poller.poll(Some(Duration::ZERO)) {
            // Readiness is reported per registered key, and the sentinel cannot collide with the
            // poller's own waker (see [`SIGNAL_KEY`]).
            Ok(events) => events.iter().any(|event| event.key() == SIGNAL_KEY),
            Err(_) => false,
        }
    }

    /// Registers the facility's self-pipe with `poller` when there is something to observe.
    ///
    /// Called at the top of every poll round, so a `handle` disposition installed *after* the
    /// registry was built (the normal case — the facility itself is created by
    /// `os:set_signal/2`) is picked up on the next round. Idempotent: the descriptor is
    /// registered exactly once.
    ///
    /// # Errors
    ///
    /// The poller's own registration failure (`BeamError::Io`, or `BeamError::SystemLimit` when
    /// the token space is exhausted) — the same errors a port's registration reports. A
    /// `handle` disposition the OS accepted but whose registration then fails is a genuine
    /// failure and is surfaced rather than silently leaving signals undelivered.
    pub(crate) fn sync(&mut self, poller: &mut Poller) -> Result<()> {
        // Nothing to register: either the descriptor is already in the poll set, or no
        // `handle` disposition has ever been installed, in which case the self-pipe can never
        // become readable and there is nothing to observe.
        if !self.is_pollable() || self.is_registered() {
            return Ok(());
        }
        // `has_handled_signal()` can only be true after `install` created the facility, so the
        // `Some` arm is the invariant; the `None` arm is unreachable and simply means "nothing
        // to register" rather than being an error.
        let Some(fd) = with_existing_facility(|facility| facility.read_fd()) else {
            return Ok(());
        };
        let token = poller.register(fd, Interest::READABLE, SIGNAL_KEY)?;
        self.token = Some(token);
        self.fd = Some(fd);
        Ok(())
    }

    /// Whether `events` reports readiness on the signal self-pipe.
    pub(crate) fn ready(&self, events: &[Event]) -> bool {
        events.iter().any(|event| event.key() == SIGNAL_KEY)
    }

    /// Consumes the handled signals delivered since the last drain, as their atom names.
    ///
    /// The names are [`Signal::name`]'s `&'static str`s — exactly the atoms
    /// `os:set_signal/2` accepts and ERTS puts in the `{notify, SignalName}` tuple. Nothing is
    /// reported when no facility exists (no `os:set_signal/2` ran, so no signal can have been
    /// handled by it).
    pub(crate) fn drain(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        with_existing_facility(|facility| {
            for signal in facility.drain() {
                names.push(signal.name());
            }
        });
        names
    }
}

/// The process-global test guard for the disposition-mutating tests in this crate's lib tests.
///
/// A signal disposition is **process-global** (that is the point of this module), so every test
/// that changes one serializes on this one lock — the tests in `crate::context::signal`, in this
/// module and in `crate::port` all run in the same test binary. A poisoned lock (a test panicked
/// while holding it) is recovered rather than propagated: the lock guards OS state, not shared
/// Rust data.
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

#[cfg(test)]
pub(crate) mod watcher {
    use super::*;

    use std::process::Command;
    use std::time::Duration;

    use beam_io::signal::Disposition;

    /// Sends `signal` to **this** process with the external `kill(1)` program.
    ///
    /// `libc::raise` would need `unsafe` (this crate is `#![deny(unsafe_code)]`), so the signal
    /// is sent the way an outside actor sends it. The exit status is checked so a missing `kill`
    /// is a loud failure rather than an unnoticed no-op.
    pub(crate) fn send_to_self(signal: Signal) {
        let pid = std::process::id().to_string();
        // `kill -USR1 1234` — the signal name without its `sig` prefix, upper-cased.
        let flag = format!("-{}", signal.name().trim_start_matches("sig").to_uppercase());
        let status = Command::new("kill")
            .arg(&flag)
            .arg(&pid)
            .status()
            .unwrap_or_else(|error| panic!("cannot run `kill {flag} {pid}`: {error}"));
        assert!(status.success(), "`kill {flag} {pid}` failed: {status:?}");
    }

    /// The disposition the kernel currently holds for `signal` (a `sigaction(2)` query).
    pub(crate) fn current(signal: Signal) -> Disposition {
        with_facility(|facility| facility.disposition(signal)).expect("a disposition query")
    }

    /// Whether the trampoline has **caught** a signal that no drain has consumed yet — a
    /// **non-destructive** probe of the self-pipe.
    ///
    /// The record of what arrived is `beam_io`'s process-global bitmask, which only
    /// [`SignalFacility::drain`] reads (and clears) — so a test that wants to prove a signal was
    /// really caught *before* asserting what a delivery path does with it asks the **pipe**
    /// instead: the trampoline writes it on the signal's way in, and a throwaway
    /// [`Poller`] can see the readiness without consuming anything (draining would destroy the
    /// very evidence the test needs). `false` when no facility exists: no `os:set_signal/2` ran,
    /// so nothing can have been caught.
    ///
    /// Forwards to the production predicate ([`SignalWatch::has_pending_signal`]) so there is
    /// one implementation of the probe; the watch it is asked of is stateless (the answer comes
    /// from the process-global facility), so a fresh one is the same question.
    pub(crate) fn pending() -> bool {
        SignalWatch::new().has_pending_signal()
    }

    /// Waits (bounded) for the trampoline to catch a signal [`send_to_self`] sent.
    ///
    /// `kill(1)` returns as soon as the kernel has queued the signal, and the handler that writes
    /// the self-pipe runs asynchronously, so a `drain` immediately after `send_to_self` is a race.
    /// Bounded, so a genuine regression (a signal that never arrives) still fails loudly.
    pub(crate) fn wait_until_pending() {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if pending() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the handled signal never reached the self-pipe");
    }

    #[test]
    fn a_watch_with_no_disposition_is_not_pollable() {
        let _guard = test_guard();
        let mut watch = SignalWatch::new();
        assert!(!watch.is_registered());
        assert!(
            !watch.is_pollable(),
            "an ignore/default-only facility has no readable self-pipe"
        );
        // Nothing to register ⇒ `sync` is a no-op, not an error.
        let mut poller = Poller::new().expect("a poller");
        watch.sync(&mut poller).expect("sync with nothing to watch");
        assert!(!watch.is_registered());
        assert!(watch.drain().is_empty());
    }

    #[test]
    fn a_handle_disposition_registers_the_self_pipe_once() {
        let _guard = test_guard();
        let mut poller = Poller::new().expect("a poller");
        let mut watch = SignalWatch::new();

        let before = current(Signal::SigUsr1);
        install(Signal::SigUsr1, Disposition::Handle).expect("install handle on sigusr1");
        assert!(watch.is_pollable());
        assert!(!watch.is_registered());
        watch.sync(&mut poller).expect("sync registers the self-pipe");
        assert!(watch.is_registered());

        // Idempotent: a second round does not re-register (which `mio` would reject).
        let fd = watch.fd.expect("the registered fd");
        watch.sync(&mut poller).expect("a second sync is a no-op");
        assert_eq!(watch.fd, Some(fd));

        // The registered signal is reported by the drain and by `ready`.
        send_to_self(Signal::SigUsr1);
        wait_until_pending();
        let token = watch.token.expect("the registered token");
        assert!(watch.ready(&[Event::new(token, SIGNAL_KEY, beam_io::Ready::default())]));
        assert!(!watch.ready(&[Event::new(token, 7, beam_io::Ready::default())]));
        assert_eq!(watch.drain(), vec!["sigusr1"]);
        // Handed out exactly once.
        assert!(watch.drain().is_empty());

        install(Signal::SigUsr1, before).expect("restore sigusr1");
    }
}