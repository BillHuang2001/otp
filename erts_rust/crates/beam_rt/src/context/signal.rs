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

//! The runtime half of `os:set_signal/2`: the process-global signal dispositions.
//!
//! `beam_bifs` registers `os:set_signal/2` as one of its `RuntimeHooks`-gated keys (it is in
//! [`hook_sets::RUNTIME_HOOKS_BIFS`](beam_bifs::hook_sets), so
//! [`RuntimeContext::uses_runtime_hooks`](super::RuntimeContext::uses_runtime_hooks) installs
//! the bridge for it) and [`RuntimeHooksBridge`](super::RuntimeHooksBridge) serves the call
//! from here. The *mechanism* — the twelve POSIX signal numbers, the `sigaction(2)` install,
//! the async-signal-safe trampoline and the self-pipe — is `beam_io::signal`'s, the module
//! that owns the `unsafe` this crate's `#![deny(unsafe_code)]` cannot contain. This module
//! owns the *runtime* half: the one long-lived facility and the KRTS's exact answer surface.
//!
//! # Ownership and lifetime: one facility for the process
//!
//! An Erlang signal disposition is **process-global** in ERTS: `os:set_signal/2` ends in
//! `sys_signal` (`erts/emulator/sys/unix/sys.c`), a plain `sigaction(2)` on the emulator
//! process, so the disposition outlives the calling Erlang process and is shared by every
//! process in the node. The Rust runtime reproduces that with **one** [`SignalFacility`]
//! held in the `thread_local!` slot [`FACILITY`] and created lazily by the first
//! `os:set_signal/2`; it is never dropped while the runtime lives, so a later call observes
//! exactly the disposition an earlier one installed (a per-call facility would be dropped at
//! the end of the call, and `SignalFacility::drop` restores the prior dispositions — see
//! `the_facility_is_shared_across_calls` below).
//!
//! A `thread_local` (rather than a field of [`Runtime`](crate::Runtime)) is sound for exactly
//! the reason [`context::trace`](super::trace)'s slot is: the runtime is single-threaded
//! (`!Send` — it owns `Rc`/`RefCell` tables and raw `*mut Eterm` heaps), so this slot *is*
//! the process's one facility in practice. `beam_io` documents the single-instance rule the
//! other way round: a second live facility re-points the trampoline's write end at its own
//! pipe, which this module never allows.
//!
//! # The oracle's exact surface
//!
//! `os_set_signal_2` (`erts/emulator/beam/erl_bif_os.c:145-160`) and
//! `signalterm_to_signum`/`erts_set_signal` (`erts/emulator/sys/unix/sys.c:698-767`):
//!
//! * the **option** must be one of the atoms `ignore`/`default`/`handle` — anything else is
//!   `badarg`;
//! * the **signal** must be an atom `signalterm_to_signum` maps — anything else is `badarg`;
//! * then the answer is the bare atom `ok`. Which disposition was requested only selects the
//!   `sigaction(2)` handler; the C ignores that call's result.
//!
//! The accepted names are exactly [`Signal::ALL`]'s twelve; `sigkill`, `sigttin`/`sigttou`
//! (commented out in the C table) and `siginfo` (absent from this platform's libc) are
//! `badarg`, verified against the live oracle (OTP 29/`erts-17.1`).
//!
//! # Kernel-only signals: `sigstop`
//!
//! `SIGSTOP` (and `SIGKILL`, which is not a BEAM signal name at all) is *kernel-only*:
//! `sigaction(2)` refuses **every** disposition change for it with `EINVAL`. The C's
//! `sys_signal` consequently fails and is ignored, and the oracle still answers `ok` for
//! `os:set_signal(sigstop, handle|default|ignore)`. [`install`] therefore does not issue the
//! syscall for such a signal: the disposition provably cannot change, so nothing is lost and
//! nothing is swallowed — there is no error to swallow (see [`is_kernel_only`]).
//!
//! # The `{notify, Signal}` leg is NOT wired (documented gap)
//!
//! ERTS's emulator turns a handled signal into a message to the process registered as
//! `erl_signal_server`: `generic_signal_handler` wakes the emulator, which sends
//! `{notify, SignalName}` to that name. Two things are missing here, and neither is
//! fabricated:
//!
//! 1. **No recipient.** This runtime boots no `erl_signal_server` (the differential corpus
//!    never registers one), so `whereis(erl_signal_server)` finds nothing to deliver to. A
//!    message is never sent to a name that has no live process.
//! 2. **No drain point.** [`SignalFacility::read_fd`] has to be registered with the runtime's
//!    poller and drained from its reactor loop; both the poller (the private
//!    `PortRegistry::poller` in `src/port.rs`) and the loop that would drive it are outside
//!    this node, so a handled signal is recorded in the facility's bitmask and never
//!    delivered. This is a *delivery* gap only — the disposition itself is installed exactly
//!    as the oracle installs it.
//!
//! The wiring, when it lands, is: register [`SignalFacility::read_fd`] with the process
//! poller, call [`SignalFacility::drain`] on readiness, and for each returned [`Signal`]
//! deliver the 2-tuple `{notify, Signal::name()}` to the process
//! [`whereis`](beam_process::ProcessTable::whereis) resolves for the interned
//! `erl_signal_server` atom — through [`ProcessTable::deliver_simple`](beam_process::ProcessTable::deliver_simple),
//! so a blocked receiver is woken.
//!
//! # Errors
//!
//! Every fallible API returns [`beam_common::Result`]. The **only** error this module can
//! produce is a genuine I/O failure from `beam_io` (a refused `pipe`/`sigaction` syscall) —
//! the argument surface is validated by the caller ([`RuntimeHooksBridge::set_signal`](super::RuntimeHooksBridge)),
//! which owns the oracle's `badarg`. No `unwrap()`/`panic!` on runtime data, no `unsafe`.

use std::cell::RefCell;

use beam_common::Result;
use beam_io::signal::{Disposition, Signal, SignalFacility};

thread_local! {
    /// The runtime's one long-lived signal facility, created by the first
    /// `os:set_signal/2` and kept for the process's life (see the module docs on the
    /// single-instance rule).
    ///
    /// `RefCell<Option<..>>` rather than a `OnceLock` because the constructor is fallible:
    /// the first call either fills the slot or reports the pipe-creation failure, and a
    /// failed creation leaves the slot empty so a later call can try again.
    static FACILITY: RefCell<Option<SignalFacility>> = const { RefCell::new(None) };
}

/// Installs `disposition` for `signal` — the body of the `os:set_signal/2` arm.
///
/// Argument validation (the oracle's `badarg`) belongs to the caller, which holds the
/// caller's atom table; this function is total over the two closed enums.
///
/// # Errors
///
/// Only a genuine I/O failure from a refused `pipe`/`sigaction` syscall.
pub(super) fn install(signal: Signal, disposition: Disposition) -> Result<()> {
    if is_kernel_only(signal) {
        // The kernel refuses every disposition change for a kernel-only signal, so the
        // install cannot take effect; ERTS's `sys_signal` fails and is ignored here too
        // (the oracle answers `ok`). Not calling at all is the honest form of "ignored":
        // there is no error to swallow.
        return Ok(());
    }
    with_facility(|facility| facility.set(signal, disposition))
}

/// Whether the kernel refuses *every* disposition change for `signal`.
///
/// `SIGSTOP` and `SIGKILL` are the two kernel-only signals; `sigkill` is not a BEAM signal
/// name (`signalterm_to_signum` maps no such atom), so `sigstop` is the only member of the
/// accepted set. Verified on this platform: `sigaction(2)` answers `EINVAL` for all three
/// dispositions of `SIGSTOP`, and succeeds for every other accepted signal.
fn is_kernel_only(signal: Signal) -> bool {
    matches!(signal, Signal::SigStop)
}

/// Runs the fallible `f` against the process's one signal facility, creating it on first
/// use.
///
/// # Errors
///
/// A failed creation of the facility (the self-pipe) is a real I/O failure and is reported,
/// never swallowed; the slot stays empty so the next call retries. `f`'s own failure is
/// returned untouched.
fn with_facility<R>(f: impl FnOnce(&mut SignalFacility) -> Result<R>) -> Result<R> {
    FACILITY.with(|slot| {
        let mut slot = slot.borrow_mut();
        let facility = match slot.as_mut() {
            Some(facility) => facility,
            None => slot.insert(SignalFacility::new()?),
        };
        f(facility)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Mutex, MutexGuard};

    /// The dispositions are **process-global** (that is the point of this module), so every
    /// test that changes one serializes on this lock. A poisoned lock (a test panicked while
    /// holding it) is recovered rather than propagated — the lock guards OS state, not
    /// shared Rust data.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Acquires the process-global test guard.
    fn guard() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// The disposition the kernel currently holds for `signal`.
    fn current(signal: Signal) -> Disposition {
        with_facility(|facility| facility.disposition(signal)).expect("a disposition query")
    }

    /// The signals the round-trip test mutates.
    ///
    /// `sigchld` is deliberately **absent**: its disposition is process-global too, and
    /// `SIG_IGN` for `SIGCHLD` makes the kernel reap children automatically, which would
    /// disturb a concurrently running test in this binary that waits for a child
    /// (`std::process::Child::wait` would report `ECHILD`). Its name mapping is pinned by
    /// [`the_oracle_surface_is_the_twelve_signals_and_three_dispositions`]. `sigstop` is
    /// exercised separately — installing it never reaches the OS.
    const MUTATED: [Signal; 10] = [
        Signal::SigHup,
        Signal::SigQuit,
        Signal::SigAbrt,
        Signal::SigAlrm,
        Signal::SigTerm,
        Signal::SigUsr1,
        Signal::SigUsr2,
        Signal::SigTstp,
        Signal::SigCont,
        Signal::SigWinch,
    ];

    #[test]
    fn every_accepted_signal_installs_every_disposition_and_reads_it_back() {
        let _guard = guard();
        for signal in MUTATED {
            let before = current(signal);
            for disposition in Disposition::ALL {
                install(signal, disposition).expect("the OS accepts the disposition");
                assert_eq!(
                    current(signal),
                    disposition,
                    "{signal:?} must hold the disposition just installed"
                );
            }
            install(signal, before).expect("the prior disposition is restored");
        }
    }

    #[test]
    fn the_facility_is_shared_across_calls() {
        let _guard = guard();
        let usr1_before = current(Signal::SigUsr1);
        let usr2_before = current(Signal::SigUsr2);

        install(Signal::SigUsr1, Disposition::Ignore).expect("install ignore on sigusr1");
        // A facility created per call would have been dropped — and its `Drop` restores the
        // prior dispositions — before this second call, so `sigusr1` would be back at its
        // prior disposition. It is not: the two calls share one facility.
        install(Signal::SigUsr2, Disposition::Handle).expect("install handle on sigusr2");
        assert_eq!(current(Signal::SigUsr1), Disposition::Ignore);
        assert_eq!(current(Signal::SigUsr2), Disposition::Handle);

        install(Signal::SigUsr1, usr1_before).expect("restore sigusr1");
        install(Signal::SigUsr2, usr2_before).expect("restore sigusr2");
    }

    #[test]
    fn kernel_only_signals_are_accepted_without_touching_the_os() {
        let _guard = guard();
        assert!(is_kernel_only(Signal::SigStop));
        assert!(
            Signal::ALL
                .iter()
                .filter(|signal| is_kernel_only(**signal))
                .count()
                == 1,
            "sigstop is the only kernel-only signal in the accepted set"
        );

        let before = current(Signal::SigStop);
        for disposition in Disposition::ALL {
            // The oracle answers `ok` for every disposition of `sigstop` even though the
            // kernel refuses all of them; so does this arm.
            install(Signal::SigStop, disposition).expect("sigstop is accepted");
        }
        // Nothing was installed, so the (immutable) `SIG_DFL` slot is still what it was.
        assert_eq!(current(Signal::SigStop), before);
        assert_eq!(before, Disposition::Default);
    }

    /// The accepted surface, oracle-verified (OTP 29/`erts-17.1`): the twelve signals of
    /// `Signal::ALL` and the three dispositions of `Disposition::ALL` — and nothing else.
    #[test]
    fn the_oracle_surface_is_the_twelve_signals_and_three_dispositions() {
        for signal in Signal::ALL {
            assert_eq!(
                Signal::from_name(signal.name()),
                Some(signal),
                "{} resolves to its signal",
                signal.name()
            );
        }
        // Rejected by the oracle with `badarg`: `sigkill` (no C mapping), `sigttin`/`sigttou`
        // (commented out in the C table), `siginfo` (no `SIGINFO` in this libc), an unknown
        // name, and the uppercase atom (a different atom to the oracle).
        for name in [
            "sigkill", "sigttin", "sigttou", "siginfo", "sigfoo", "", "SIGTERM",
        ] {
            assert_eq!(Signal::from_name(name), None, "{name} must be rejected");
        }
        for disposition in Disposition::ALL {
            assert_eq!(
                Disposition::from_name(disposition.name()),
                Some(disposition)
            );
        }
        for name in ["foo", "", "Default", "Handle"] {
            assert_eq!(
                Disposition::from_name(name),
                None,
                "{name} must be rejected"
            );
        }
    }
}
