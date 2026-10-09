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

//! The OS-signal facility: a self-pipe and an async-signal-safe trampoline.
//!
//! `beam_rt` denies `unsafe_code`, so every `sigaction`/signal-mask syscall lives here.
//! This module is the *whole* mechanism behind `os:set_signal/2`: it maps the twelve BEAM
//! signal atoms to their POSIX numbers, installs `SIG_DFL`/`SIG_IGN`/a "handle" trampoline
//! through `sigaction(2)`, wakes the runtime's reactor through a self-pipe, and reports the
//! handled signals in normal (non-signal) context.
//!
//! Legacy C ground truth (read-only): the accepted signal set and its `handle` semantics
//! are `signalterm_to_signum`/`signum_to_signalterm` in `erts/emulator/sys/unix/sys.c`
//! (`erts_set_signal`, via `sys_signal`), exposed to Erlang by `os:set_signal/2`
//! (`lib/kernel/src/os.erl`). The accepted set is exactly the twelve signals modelled by
//! [`Signal`] — `sigcont`/`sigwinch` are accepted, `sigttin`/`sigttou` are *not*; `siginfo`
//! is platform-conditional (`#ifdef SIGINFO`) and so is deliberately excluded here.
//!
//! # The self-pipe
//!
//! [`SignalFacility::new`] creates a non-blocking, close-on-exec pipe. The runtime registers
//! [`SignalFacility::read_fd`] with [`crate::poller::Poller`] (readiness ⇒ a handled signal
//! arrived) and calls [`SignalFacility::drain`] once the reactor reports it. A `signalfd`
//! variant is *not* used even on Linux: the pipe works on every unix target and the poller
//! already watches plain descriptors.
//!
//! # The trampoline is async-signal-safe
//!
//! A POSIX signal handler may call **only async-signal-safe** functions. The handler
//! installed by [`SignalFacility::set`] therefore does nothing but two things, both
//! async-signal-safe:
//!
//! 1. an atomic OR into a process-global bitmask (one bit per [`Signal`]), and
//! 2. a single `libc::write` of one byte (the signal number) to the self-pipe's write end.
//!
//! It never allocates, never takes a lock, never unwinds and never runs arbitrary Rust. The
//! signo → signal-name mapping that [`SignalFacility::drain`] performs happens in normal
//! context, after the handler has returned.
//!
//! # Coalescing policy (exact)
//!
//! `drain` reports **each distinct signal that has arrived since the previous `drain`
//! exactly once**, in [`Signal::ALL`] order (a fixed, deterministic order). Concretely:
//!
//! * On arrival the trampoline sets the signal's bit in a global atomic bitmask and writes
//!   one byte to the pipe.
//! * `drain` atomically *swaps the bitmask to zero* and then reports one [`Signal`] per set
//!   bit, then drains the pipe bytes.
//!
//! So a burst of `N` identical signals between two drains is reported **once**, not `N`
//! times: the signal is observed at least once for every drain interval in which it
//! occurred, but distinct *deliveries* are coalesced. The pipe byte count is intentionally
//! not the source of truth — POSIX does not queue standard signals, and the pipe itself may
//! coalesce writes — the bitmask is. A stray pipe byte therefore never yields a duplicate
//! report, and a dropped pipe byte never loses a signal (the bit is already set).
//!
//! Signals whose disposition is `Ignore` (or the OS default) never reach the trampoline, so
//! `drain` never reports them.
//!
//! # Process-global state, single instance
//!
//! The trampoline, the bitmask and the self-pipe's write end are **process-global** (they
//! must be reachable from the handler without capturing a context). Only **one**
//! [`SignalFacility`] should be alive at a time: a second [`SignalFacility::new`] re-points
//! the global write end at its own pipe, and — the pending bitmask being process-global too —
//! a signal delivered while both exist is reported by whichever facility drains first (its
//! byte lands in the newer facility's pipe). `Drop` restores only the signals *this* facility
//! changed and, **only if the global write end still points at its own pipe**, clears that end
//! and resets the pending bitmask — so a superseded facility neither re-points nor discards its
//! successor's plumbing, and dropping/recreating a facility is safe. The runtime owns the one
//! facility.
//!
//! # Handoff to `beam_rt`
//!
//! `beam_rt` owns the `beam_bifs` seam that backs `os:set_signal/2` (today a registered but
//! unbacked key) and the reactor loop that watches the facility:
//!
//! * construct one [`SignalFacility`] during boot and keep it alive for the runtime's life;
//! * translate `Signal::from_name/1` + `Disposition::from_name/1`, calling
//!   [`SignalFacility::set`] and answering the oracle's `ok` / `badarg`;
//! * register [`SignalFacility::read_fd`] with the process's [`crate::poller::Poller`] and,
//!   when that registration reports readiness, call [`SignalFacility::drain`] and deliver
//!   each returned [`Signal`] to the `erl_signal_server` as `{notify, SignalAtom}`.
//!
//! # Errors
//!
//! Every fallible API returns [`beam_common::Result`]: a failing `pipe`/`sigaction` syscall
//! is `BeamError::Io`; there is no argument fault expressible at this layer (`Signal` and
//! `Disposition` are closed enums), so `BeamError::BadArg` is never constructed here. The
//! module never panics, never `unwrap()`s runtime data and contains no `todo!`/`unreachable!`.

use std::io::Error as IoError;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use beam_common::Result;

/// The number of distinct signals this facility manages (one bit in the pending bitmask
/// each).
const SIGNAL_COUNT: usize = Signal::ALL.len();

/// The pending bitmask must fit in the [`AtomicU64`] the trampoline maintains.
const _: () = assert!(SIGNAL_COUNT <= 64);

/// A signal the runtime has handled since the last [`SignalFacility::drain`].
///
/// A bit per signal; the trampoline sets a bit with an atomic OR, `drain` swaps the whole
/// word to zero and reports the set bits. The value `0` means "no pending signal".
static PENDING: AtomicU64 = AtomicU64::new(0);

/// The self-pipe's write end, or `-1` when no [`SignalFacility`] is live.
///
/// The trampoline reads it with a plain atomic load and `write(2)`s the signal number to it.
/// It is deliberately an atomic rather than a `OnceLock` so the handler never has to take a
/// lock or run an initializer.
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// One of the twelve OS signals `os:set_signal/2` accepts.
///
/// The set is fixed by `signalterm_to_signum` (`erts/emulator/sys/unix/sys.c`) and the
/// `os:set_signal/2` spec (`lib/kernel/src/os.erl`): `sighup`, `sigquit`, `sigabrt`,
/// `sigalrm`, `sigterm`, `sigusr1`, `sigusr2`, `sigchld`, `sigstop`, `sigtstp`, `sigcont`
/// and `sigwinch`. `siginfo` is excluded because it only exists on platforms whose libc
/// defines `SIGINFO`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Signal {
    /// `sighup` — hangup (`libc::SIGHUP`).
    SigHup,
    /// `sigquit` — quit (`libc::SIGQUIT`).
    SigQuit,
    /// `sigabrt` — abort (`libc::SIGABRT`).
    SigAbrt,
    /// `sigalrm` — alarm clock (`libc::SIGALRM`).
    SigAlrm,
    /// `sigterm` — termination (`libc::SIGTERM`).
    SigTerm,
    /// `sigusr1` — user-defined 1 (`libc::SIGUSR1`).
    SigUsr1,
    /// `sigusr2` — user-defined 2 (`libc::SIGUSR2`).
    SigUsr2,
    /// `sigchld` — child status changed (`libc::SIGCHLD`).
    SigChld,
    /// `sigstop` — stop (`libc::SIGSTOP`), one of the accepted names even though it is
    /// **kernel-only**: `sigaction(2)` refuses *every* disposition change for it with
    /// `EINVAL`, so this is the one accepted signal whose disposition provably cannot be
    /// changed and [`SignalFacility::set`] with it always fails with
    /// [`beam_common::BeamError::Io`]. The legacy C's `sys_signal` fails in exactly the same
    /// way and *ignores* the result (`erts/emulator/sys/unix/sys.c:437-461`, `:754-767`),
    /// which is how the oracle still answers `ok` for `os:set_signal(sigstop, _)`.
    SigStop,
    /// `sigtstp` — terminal stop (`libc::SIGTSTP`).
    SigTstp,
    /// `sigcont` — continue if stopped (`libc::SIGCONT`).
    SigCont,
    /// `sigwinch` — terminal window size changed (`libc::SIGWINCH`).
    SigWinch,
}

impl Signal {
    /// Every accepted signal, in the fixed order [`SignalFacility::drain`] reports them.
    pub const ALL: [Signal; 12] = [
        Signal::SigHup,
        Signal::SigQuit,
        Signal::SigAbrt,
        Signal::SigAlrm,
        Signal::SigTerm,
        Signal::SigUsr1,
        Signal::SigUsr2,
        Signal::SigChld,
        Signal::SigStop,
        Signal::SigTstp,
        Signal::SigCont,
        Signal::SigWinch,
    ];

    /// The lowercase BEAM atom name of this signal (`"sighup"`, `"sigusr1"`, …).
    ///
    /// This is exactly the atom `os:set_signal/2` takes and [`Signal::from_name`] accepts.
    pub const fn name(self) -> &'static str {
        match self {
            Signal::SigHup => "sighup",
            Signal::SigQuit => "sigquit",
            Signal::SigAbrt => "sigabrt",
            Signal::SigAlrm => "sigalrm",
            Signal::SigTerm => "sigterm",
            Signal::SigUsr1 => "sigusr1",
            Signal::SigUsr2 => "sigusr2",
            Signal::SigChld => "sigchld",
            Signal::SigStop => "sigstop",
            Signal::SigTstp => "sigtstp",
            Signal::SigCont => "sigcont",
            Signal::SigWinch => "sigwinch",
        }
    }

    /// Resolves a BEAM atom name to a [`Signal`], or `None` for a name `os:set_signal/2`
    /// would reject.
    ///
    /// The match is **exact and case-sensitive**: the oracle's atoms are lowercase
    /// (`'SIGHUP'` is a different atom and is rejected).
    pub fn from_name(name: &str) -> Option<Signal> {
        Signal::ALL.iter().copied().find(|sig| sig.name() == name)
    }

    /// The POSIX signal number (`libc::SIGHUP`, `libc::SIGUSR1`, …).
    pub const fn signo(self) -> libc::c_int {
        match self {
            Signal::SigHup => libc::SIGHUP,
            Signal::SigQuit => libc::SIGQUIT,
            Signal::SigAbrt => libc::SIGABRT,
            Signal::SigAlrm => libc::SIGALRM,
            Signal::SigTerm => libc::SIGTERM,
            Signal::SigUsr1 => libc::SIGUSR1,
            Signal::SigUsr2 => libc::SIGUSR2,
            Signal::SigChld => libc::SIGCHLD,
            Signal::SigStop => libc::SIGSTOP,
            Signal::SigTstp => libc::SIGTSTP,
            Signal::SigCont => libc::SIGCONT,
            Signal::SigWinch => libc::SIGWINCH,
        }
    }

    /// Resolves a POSIX signal number to a [`Signal`], or `None` for one this facility does
    /// not manage.
    ///
    /// The trampoline uses this to turn the handler's `signo` into a bitmask index; it is
    /// pure computation (no allocation, no locks), so it is safe to call from a handler.
    pub fn from_signo(signo: libc::c_int) -> Option<Signal> {
        Signal::ALL.iter().copied().find(|sig| sig.signo() == signo)
    }

    /// The bit index of this signal in the pending bitmask — its position in
    /// [`Signal::ALL`].
    const fn bit(self) -> u32 {
        match self {
            Signal::SigHup => 0,
            Signal::SigQuit => 1,
            Signal::SigAbrt => 2,
            Signal::SigAlrm => 3,
            Signal::SigTerm => 4,
            Signal::SigUsr1 => 5,
            Signal::SigUsr2 => 6,
            Signal::SigChld => 7,
            Signal::SigStop => 8,
            Signal::SigTstp => 9,
            Signal::SigCont => 10,
            Signal::SigWinch => 11,
        }
    }
}

/// What happens when a signal arrives — the three options `os:set_signal/2` accepts.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Disposition {
    /// `default` — the operating system's default action (`SIG_DFL`).
    Default,
    /// `handle` — notify the runtime through the trampoline/self-pipe.
    Handle,
    /// `ignore` — discard the signal (`SIG_IGN`).
    Ignore,
}

impl Disposition {
    /// Every disposition, in the oracle's option order.
    pub const ALL: [Disposition; 3] = [
        Disposition::Default,
        Disposition::Handle,
        Disposition::Ignore,
    ];

    /// The lowercase BEAM atom name of this disposition (`"default"`/`"handle"`/`"ignore"`).
    pub const fn name(self) -> &'static str {
        match self {
            Disposition::Default => "default",
            Disposition::Handle => "handle",
            Disposition::Ignore => "ignore",
        }
    }

    /// Resolves a BEAM atom name to a [`Disposition`], or `None` for an unknown option.
    ///
    /// Exact and case-sensitive, like [`Signal::from_name`].
    pub fn from_name(name: &str) -> Option<Disposition> {
        Disposition::ALL
            .iter()
            .copied()
            .find(|disp| disp.name() == name)
    }
}

/// The process-global `SIG_DFL`/`SIG_IGN`/trampoline handler for one signal disposition.
///
/// Runs in signal context: it must perform only async-signal-safe work.
extern "C" fn trampoline(signo: libc::c_int) {
    // SAFETY: every operation below is async-signal-safe (POSIX). The handler performs no
    // allocation, takes no lock, unwinds nothing and runs no destructor:
    //   * `Signal::from_signo` is a pure scan of a constant array;
    //   * `PENDING.fetch_or` is a lock-free atomic read-modify-write on a `static`;
    //   * `WRITE_FD.load` is a lock-free atomic load;
    //   * `libc::write` to a non-blocking pipe (`WRITE_FD` is only ever set to an
    //     `O_NONBLOCK` fd) is itself async-signal-safe and cannot block.
    // `byte` is a local array on the signal stack; `libc::write` consumes it within the call.
    // A failed write is deliberately ignored: the bitmask, not the pipe, carries the signal.
    if let Some(sig) = Signal::from_signo(signo) {
        PENDING.fetch_or(1u64 << sig.bit(), Ordering::SeqCst);
    }
    let fd = WRITE_FD.load(Ordering::SeqCst);
    if fd >= 0 {
        let mut byte = [signo as u8];
        // SAFETY: `byte` is a valid, initialized 1-byte buffer and `fd` is a live,
        // non-blocking descriptor while the facility exists (see `SignalFacility::drop`,
        // which clears `WRITE_FD` before closing it). `write(2)` never retains the pointer.
        let _ = unsafe { libc::write(fd, byte.as_mut_ptr().cast(), 1) };
    }
}

/// Classifies a kernel `sigaction` handler slot into a [`Disposition`].
///
/// `SIG_DFL`/`SIG_IGN` are the two sentinel handler addresses; anything else is a real
/// handler — ours or another library's — which the runtime can only report as `Handle`.
fn classify(handler: libc::sighandler_t) -> Disposition {
    if handler == libc::SIG_DFL {
        Disposition::Default
    } else if handler == libc::SIG_IGN {
        Disposition::Ignore
    } else {
        Disposition::Handle
    }
}

/// A live OS-signal facility: the self-pipe, the installed dispositions and their priors.
///
/// Create one with [`SignalFacility::new`], install dispositions with
/// [`SignalFacility::set`], register [`SignalFacility::read_fd`] with the reactor and
/// collect arrivals with [`SignalFacility::drain`]. Dropping it restores every disposition
/// it changed and tears down the self-pipe (see the module docs on the single-instance
/// rule).
pub struct SignalFacility {
    /// The read end of the self-pipe — the fd the runtime registers with the poller.
    read_fd: RawFd,
    /// The write end of the self-pipe (also cached in the global [`WRITE_FD`]).
    write_fd: RawFd,
    /// The disposition that was installed before this facility first changed each signal,
    /// indexed by [`Signal::bit`]. `None` means this facility never touched that signal.
    prior: [Option<libc::sigaction>; SIGNAL_COUNT],
}

impl SignalFacility {
    /// Creates the facility: a close-on-exec, non-blocking self-pipe plus the global
    /// trampoline plumbing.
    ///
    /// The pipe's read end is what the runtime registers with [`crate::poller::Poller`]
    /// ([`SignalFacility::read_fd`]); the write end is published to the process-global
    /// [`WRITE_FD`] the trampoline writes to. No disposition is changed yet — call
    /// [`SignalFacility::set`] for that.
    ///
    /// # Errors
    ///
    /// `BeamError::Io` if the operating system refuses to create the pipe (or, on the
    /// portable fallback path, to set its flags).
    pub fn new() -> Result<SignalFacility> {
        let (read_fd, write_fd) = open_pipe()?;
        // A fresh facility starts from a clean slate: no stale bitmask, and the trampoline
        // points at this facility's pipe.
        PENDING.store(0, Ordering::SeqCst);
        WRITE_FD.store(write_fd, Ordering::SeqCst);
        Ok(SignalFacility {
            read_fd,
            write_fd,
            prior: [None; SIGNAL_COUNT],
        })
    }

    /// Installs `disp` for `sig`, remembering the prior disposition so [`Drop`] can restore
    /// it.
    ///
    /// * `Default` ⇒ `SIG_DFL` (the OS default action),
    /// * `Ignore` ⇒ `SIG_IGN`,
    /// * `Handle` ⇒ the module's async-signal-safe trampoline.
    ///
    /// The action is installed with an **empty** `sa_mask` (no signal is additionally
    /// blocked *while the handler runs*; the process signal mask is left untouched) and the
    /// `SA_RESTART` flag. `SA_RESTART` is chosen deliberately: the delivery channel is the
    /// self-pipe, so the runtime never needs `EINTR`-based logic, and auto-restarting an
    /// interrupted `read`/`write`/`poll` keeps a handled signal from disrupting unrelated
    /// blocking I/O. (The legacy C's `sys_signal` never sets `SA_RESTART`: its `sa_flags` is
    /// `SA_ONSTACK` for a real handler when the JIT runs on native Erlang stacks and `0`
    /// otherwise — `erts/emulator/sys/unix/sys.c:437-461` — and the C relies on explicit
    /// `EINTR` loops; `SA_RESTART` is this runtime's equivalent, documented choice.)
    ///
    /// The prior disposition is captured only the **first** time this facility changes a
    /// given signal, so repeated `set` calls still restore the true original on drop.
    ///
    /// # Errors
    ///
    /// `BeamError::Io` if `sigaction(2)` rejects the change. That is guaranteed for
    /// [`Signal::SigStop`], the kernel-only signal whose disposition the kernel refuses to
    /// change for *any* of the three dispositions (see [`Signal::SigStop`]) — the caller
    /// decides what a refusal means there, exactly as the oracle's `ok` comes from the C
    /// ignoring the failure.
    pub fn set(&mut self, sig: Signal, disp: Disposition) -> Result<()> {
        // SAFETY: `libc::sigaction` is a plain C struct of integer/pointer fields for which
        // an all-zero bit pattern is a valid, initialized value; `sa_sigaction`, `sa_flags`
        // and `sa_mask` are set below before the struct is used.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = match disp {
            Disposition::Default => libc::SIG_DFL,
            Disposition::Ignore => libc::SIG_IGN,
            // The handler address is stored in the `sighandler_t` slot. Cast through a raw
            // pointer (rather than a direct fn-item cast) exactly as the compiler suggests.
            Disposition::Handle => trampoline as *const () as libc::sighandler_t,
        };
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: `&mut action.sa_mask` is a valid, writable `sigset_t`; `sigemptyset`
        // only writes it (and is documented to always succeed).
        unsafe { libc::sigemptyset(&mut action.sa_mask) };

        // SAFETY: `action` is a fully initialized `sigaction` and `old` is a valid writable
        // out-parameter; `sigaction(2)` copies the previous action into `old`. Both pointers
        // are valid for the duration of the call.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::sigaction(sig.signo(), &action, &mut old) };
        if rc != 0 {
            return Err(crate::error::io(IoError::last_os_error()));
        }
        let slot = &mut self.prior[sig.bit() as usize];
        if slot.is_none() {
            *slot = Some(old);
        }
        Ok(())
    }

    /// Reports the disposition currently installed for `sig`, read back from the kernel.
    ///
    /// `SIG_DFL` reports [`Disposition::Default`], `SIG_IGN` reports [`Disposition::Ignore`],
    /// and any real handler — this facility's trampoline included — reports
    /// [`Disposition::Handle`].
    ///
    /// # Errors
    ///
    /// `BeamError::Io` if `sigaction(2)` fails.
    pub fn disposition(&self, sig: Signal) -> Result<Disposition> {
        // SAFETY: a `NULL` `act` makes this a pure query; `old` is a valid writable
        // out-parameter the kernel fills with the current action.
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::sigaction(sig.signo(), std::ptr::null(), &mut old) };
        if rc != 0 {
            return Err(crate::error::io(IoError::last_os_error()));
        }
        Ok(classify(old.sa_sigaction))
    }

    /// The fd the runtime registers with [`crate::poller::Poller`] (readiness ⇒ a handled
    /// signal arrived).
    ///
    /// The runtime owns the poller registration and its token; this crate only exposes the
    /// descriptor.
    pub fn read_fd(&self) -> RawFd {
        self.read_fd
    }

    /// Consumes the handled signals delivered since the last drain and returns them in
    /// [`Signal::ALL`] order.
    ///
    /// The exact coalescing policy is documented on the module (each *distinct* signal since
    /// the previous drain is reported exactly once, regardless of how many times it was
    /// delivered; unhandled/ignored signals are never reported). This is the only place the
    /// signo → [`Signal`] mapping runs, in normal context.
    pub fn drain(&mut self) -> Vec<Signal> {
        let bits = PENDING.swap(0, Ordering::SeqCst);
        self.drain_pipe();
        let mut out = Vec::new();
        for (index, sig) in Signal::ALL.iter().enumerate() {
            if bits & (1u64 << index) != 0 {
                out.push(*sig);
            }
        }
        out
    }

    /// Reads the self-pipe to end-of-available, so its readiness does not persist.
    ///
    /// The pipe bytes are deliberately discarded — the bitmask is the record of what
    /// arrived (see the module's coalescing policy). The read loop terminates because the
    /// write end is non-blocking (`EAGAIN`) and the buffer is bounded; an interrupted read
    /// (`EINTR`, another signal landing mid-drain) is simply retried.
    fn drain_pipe(&mut self) {
        let mut buf = [0u8; 128];
        loop {
            // SAFETY: `buf` is a valid, writable array of `buf.len()` bytes and `self.read_fd`
            // is this facility's live, non-blocking pipe read end; `read(2)` retains no
            // pointer beyond the call.
            let n = unsafe { libc::read(self.read_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                if (n as usize) < buf.len() {
                    break;
                }
                continue;
            }
            if n == 0 {
                break;
            }
            // `n < 0`: an interrupted read is retried, any other failure (notably `EAGAIN`)
            // ends the drain — the bitmask, not the pipe, carries the signals.
            if IoError::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
    }
}

impl Drop for SignalFacility {
    /// Restores every disposition this facility changed, then tears down the self-pipe.
    ///
    /// Safe even if [`SignalFacility::set`] was never called (nothing is restored, the pipe
    /// is still closed). The global write end is cleared **only if it still points at this
    /// facility's pipe**, and the pending bitmask is reset only when it does, so a facility
    /// that was already superseded neither re-points its successor's trampoline plumbing nor
    /// discards reports the successor has yet to collect. Nothing here can panic. After
    /// `drop`, no handler this facility installed remains installed and no dangling descriptor
    /// is left open.
    fn drop(&mut self) {
        for (index, sig) in Signal::ALL.iter().enumerate() {
            if let Some(prior) = self.prior[index].take() {
                // SAFETY: `prior` is an initialized `sigaction` the kernel returned from an
                // earlier `sigaction(2)`; `&prior` is a valid read-only pointer for the call
                // and `NULL` `oldact` is ignored.
                unsafe { libc::sigaction(sig.signo(), &prior, std::ptr::null_mut()) };
            }
        }
        // Only clear the global write end if it is still ours (a newer facility's fd must
        // survive); the bitmask is reset only when we *were* the live facility, so a
        // superseded facility cannot swallow its successor's pending reports.
        if WRITE_FD
            .compare_exchange(self.write_fd, -1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            PENDING.store(0, Ordering::SeqCst);
        }
        // SAFETY: both descriptors were created by `open_pipe` in `new` and are owned by this
        // facility; they are not used after the close (the global write end has been cleared
        // first, so the trampoline can no longer reach `write_fd`).
        unsafe {
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
    }
}

/// Creates the close-on-exec, non-blocking self-pipe and returns `(read_fd, write_fd)`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_pipe() -> Result<(RawFd, RawFd)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `fds` is a valid, writable array of two `c_int`s; `pipe2` writes both fds and
    // retains no pointer.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        return Err(crate::error::io(IoError::last_os_error()));
    }
    Ok((fds[0], fds[1]))
}

/// Portable fallback: `pipe(2)` followed by `fcntl(2)` to add `O_NONBLOCK` + `FD_CLOEXEC`.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn open_pipe() -> Result<(RawFd, RawFd)> {
    let mut fds: [libc::c_int; 2] = [0; 2];
    // SAFETY: `fds` is a valid, writable array of two `c_int`s; `pipe` writes both fds and
    // retains no pointer.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(crate::error::io(IoError::last_os_error()));
    }
    for &fd in &fds {
        if let Err(err) = set_nonblocking_cloexec(fd) {
            // Release both descriptors before reporting the failure — no fd leak.
            // SAFETY: both fds were just returned by `pipe(2)` and are owned here.
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(err);
        }
    }
    Ok((fds[0], fds[1]))
}

/// Sets `O_NONBLOCK` and `FD_CLOEXEC` on `fd` (the portable `pipe2` fallback).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn set_nonblocking_cloexec(fd: RawFd) -> Result<()> {
    // SAFETY: `fd` is a live descriptor owned by the caller; `fcntl` with these commands
    // only reads/replaces the descriptor's flag word and retains no pointer.
    let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if status < 0 {
        return Err(crate::error::io(IoError::last_os_error()));
    }
    // SAFETY: as above — a valid fd and an integer flag value.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, status | libc::O_NONBLOCK) } < 0 {
        return Err(crate::error::io(IoError::last_os_error()));
    }
    // SAFETY: as above.
    let descriptor = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if descriptor < 0 {
        return Err(crate::error::io(IoError::last_os_error()));
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor | libc::FD_CLOEXEC) } < 0 {
        return Err(crate::error::io(IoError::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    use beam_common::BeamError;

    /// The process-global facility makes tests serialize on the installed dispositions and
    /// the shared self-pipe, so every test that creates a [`SignalFacility`] holds this
    /// lock. A poisoned lock (a test panicked while holding it) is recovered rather than
    /// propagated — the lock guards scheduling, not shared Rust data.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Acquires the process-global test guard.
    fn guard() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// Waits (bounded) until `fd` reports readability, i.e. the trampoline has written a
    /// byte. Returns whether readiness was observed.
    ///
    /// A signal delivered while this thread is blocked in `poll(2)` makes `poll` fail with
    /// `EINTR` (unlike most blocking syscalls, `poll` is never restarted by `SA_RESTART`);
    /// the handler has already written its byte by then, so an interrupted wait is simply
    /// retried until the deadline.
    fn wait_readable(fd: RawFd, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: `pfd` is a valid, writable `pollfd`; `poll` reads/writes it and retains
            // no pointer.
            let rc = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as libc::c_int) };
            if rc > 0 {
                return (pfd.revents & libc::POLLIN) != 0;
            }
            if rc == 0 || std::time::Instant::now() >= deadline {
                return false;
            }
            // `rc < 0`: retry an interrupted wait, give up on any other failure.
            if IoError::last_os_error().raw_os_error() != Some(libc::EINTR) {
                return false;
            }
        }
    }

    #[test]
    fn signal_names_round_trip() {
        for sig in Signal::ALL {
            let name = sig.name();
            assert_eq!(
                Signal::from_name(name),
                Some(sig),
                "{name} must resolve back to its signal"
            );
            assert_eq!(name, name.to_lowercase(), "names are lowercase atoms");
        }
    }

    #[test]
    fn signal_numbers_round_trip() {
        for sig in Signal::ALL {
            assert_eq!(
                Signal::from_signo(sig.signo()),
                Some(sig),
                "a signal's signo must resolve back to it"
            );
        }
        // `SIGINT` is deliberately not in the accepted set (the oracle comments it out).
        assert_eq!(Signal::from_signo(libc::SIGINT), None);
        assert_eq!(Signal::from_signo(-1), None);
    }

    #[test]
    fn unknown_signal_names_are_none() {
        assert_eq!(Signal::from_name("sigfoo"), None);
        assert_eq!(Signal::from_name(""), None);
        // Exact, case-sensitive: the uppercase atom is a different atom to the oracle.
        assert_eq!(Signal::from_name("SIGHUP"), None);
        // `sigttin`/`sigttou` are *not* accepted by `os:set_signal/2`.
        assert_eq!(Signal::from_name("sigttin"), None);
        assert_eq!(Signal::from_name("sigttou"), None);
        assert_eq!(Signal::from_name("sigint"), None);
    }

    #[test]
    fn bit_indices_match_the_all_order() {
        for (index, sig) in Signal::ALL.iter().enumerate() {
            assert_eq!(sig.bit() as usize, index, "{} bit index", sig.name());
        }
    }

    #[test]
    fn the_accepted_set_is_the_twelve_oracle_signals() {
        // Matches `signalterm_to_signum` (`erts/emulator/sys/unix/sys.c`) exactly.
        let names: Vec<&str> = Signal::ALL.iter().map(|sig| sig.name()).collect();
        assert_eq!(
            names,
            vec![
                "sighup", "sigquit", "sigabrt", "sigalrm", "sigterm", "sigusr1", "sigusr2",
                "sigchld", "sigstop", "sigtstp", "sigcont", "sigwinch",
            ]
        );
    }

    #[test]
    fn disposition_names_round_trip() {
        for disp in Disposition::ALL {
            assert_eq!(Disposition::from_name(disp.name()), Some(disp));
        }
        assert_eq!(Disposition::from_name("sigterm"), None);
        assert_eq!(Disposition::from_name(""), None);
    }

    #[test]
    fn new_facility_exposes_a_live_read_fd() {
        let _guard = guard();
        let facility = SignalFacility::new().expect("a self-pipe");
        assert!(facility.read_fd() >= 0, "a real descriptor is exposed");
    }

    #[test]
    fn drain_with_no_delivery_is_empty() {
        let _guard = guard();
        let mut facility = SignalFacility::new().expect("a self-pipe");
        assert!(facility.drain().is_empty());
        assert!(facility.drain().is_empty(), "draining twice stays empty");
    }

    #[test]
    fn set_and_report_all_three_dispositions() {
        let _guard = guard();
        let mut facility = SignalFacility::new().expect("a self-pipe");
        for disp in Disposition::ALL {
            facility
                .set(Signal::SigUsr1, disp)
                .expect("SIGUSR1 accepts every disposition");
            assert_eq!(
                facility.disposition(Signal::SigUsr1).expect("a query"),
                disp,
                "{disp:?} is reported back"
            );
        }
        // Restore the process default before the facility drops.
        facility
            .set(Signal::SigUsr1, Disposition::Default)
            .expect("restore");
    }

    #[test]
    fn sigstop_is_the_one_accepted_signal_whose_disposition_cannot_change() {
        let _guard = guard();
        let mut facility = SignalFacility::new().expect("a self-pipe");
        // `SIGSTOP` is kernel-only: `sigaction(2)` refuses *every* disposition change for it
        // (`EINVAL`), so `set` answers the typed `BeamError::Io` instead of a silent success.
        // The refusal must stay observable here — the caller decides what it means (the oracle
        // answers `ok` because the C ignores the failed `sigaction`), so this layer never
        // swallows it. Only the *disposition* is touched: `SIGSTOP` is never raised.
        for disp in Disposition::ALL {
            match facility.set(Signal::SigStop, disp) {
                Err(BeamError::Io(_)) => {}
                other => {
                    panic!("sigstop's {disp:?} must be refused with BeamError::Io, got {other:?}")
                }
            }
            // A refused install changed nothing and recorded no prior for `Drop` to restore.
            assert_eq!(
                facility.disposition(Signal::SigStop).expect("a query"),
                Disposition::Default,
                "sigstop keeps its immutable default"
            );
            assert!(
                facility.prior[Signal::SigStop.bit() as usize].is_none(),
                "a refused install records no prior"
            );
        }
    }

    #[test]
    fn a_handled_signal_is_drained_once() {
        let _guard = guard();
        let mut facility = SignalFacility::new().expect("a self-pipe");
        facility
            .set(Signal::SigUsr1, Disposition::Handle)
            .expect("install the trampoline");

        // SAFETY: `getpid` has no preconditions and never fails.
        let pid = unsafe { libc::getpid() };
        // SAFETY: `kill` with a valid pid and a signal number is a well-formed call.
        assert_eq!(
            unsafe { libc::kill(pid, libc::SIGUSR1) },
            0,
            "kill succeeds"
        );

        // Observing the self-pipe's readiness proves the trampoline has run, so the single
        // delivery has been consumed and restoring `SIG_DFL` cannot terminate the test.
        assert!(
            wait_readable(facility.read_fd(), Duration::from_secs(2)),
            "the trampoline writes to the self-pipe"
        );
        assert_eq!(facility.drain(), vec![Signal::SigUsr1]);
        assert!(
            facility.drain().is_empty(),
            "the bit is cleared by the first drain"
        );

        facility
            .set(Signal::SigUsr1, Disposition::Default)
            .expect("restore");
    }

    #[test]
    fn repeated_deliveries_of_one_signal_are_coalesced() {
        let _guard = guard();
        let mut facility = SignalFacility::new().expect("a self-pipe");
        // `SIGWINCH`'s default action is ignore, so a straggler delivery after the facility
        // restores `SIG_DFL` cannot terminate the test.
        facility
            .set(Signal::SigWinch, Disposition::Handle)
            .expect("install the trampoline");

        // SAFETY: `getpid` has no preconditions; `kill` with a valid pid/signo is well formed.
        let pid = unsafe { libc::getpid() };
        for _ in 0..3 {
            // SAFETY: as above.
            assert_eq!(unsafe { libc::kill(pid, libc::SIGWINCH) }, 0);
        }
        assert!(wait_readable(facility.read_fd(), Duration::from_secs(2)));
        // Three (possibly coalesced) deliveries of one signal are reported exactly once.
        assert_eq!(facility.drain(), vec![Signal::SigWinch]);
    }

    #[test]
    fn an_ignored_signal_is_not_reported() {
        let _guard = guard();
        let mut facility = SignalFacility::new().expect("a self-pipe");
        facility
            .set(Signal::SigUsr2, Disposition::Ignore)
            .expect("install ignore");

        // SAFETY: `getpid` has no preconditions; `kill` with a valid pid/signo is well formed.
        let pid = unsafe { libc::getpid() };
        // SAFETY: as above.
        assert_eq!(
            unsafe { libc::kill(pid, libc::SIGUSR2) },
            0,
            "kill succeeds"
        );

        // An ignored signal never reaches the trampoline, so nothing is drained. No raise of
        // a terminate-action signal happens after the restore, so this is race-free.
        std::thread::sleep(Duration::from_millis(50));
        assert!(facility.drain().is_empty(), "an ignored signal is silent");
    }

    #[test]
    fn drop_restores_the_prior_disposition() {
        let _guard = guard();
        let before = {
            let mut facility = SignalFacility::new().expect("a self-pipe");
            let before = facility.disposition(Signal::SigUsr1).expect("a query");
            facility
                .set(Signal::SigUsr1, Disposition::Ignore)
                .expect("install ignore");
            assert_eq!(
                facility.disposition(Signal::SigUsr1).expect("a query"),
                Disposition::Ignore
            );
            before
        };
        // A fresh facility observes the restored (pre-existing) disposition.
        let facility = SignalFacility::new().expect("a self-pipe");
        assert_eq!(
            facility.disposition(Signal::SigUsr1).expect("a query"),
            before,
            "drop restores the disposition this facility changed"
        );
    }

    /// Reads whatever is available on `fd` (non-blocking) and reports `(n, errno)`.
    fn read_once(fd: RawFd, buf: &mut [u8]) -> (isize, Option<i32>) {
        // SAFETY: `buf` is a valid writable slice of `buf.len()` bytes and `fd` is the
        // facility's live, non-blocking pipe read end; `read(2)` retains no pointer.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            (n, IoError::last_os_error().raw_os_error())
        } else {
            (n, None)
        }
    }

    /// `fcntl(fd, F_GETFD)`: `None` when `fd` names a live descriptor, else its failing errno.
    fn fcntl_errno(fd: RawFd) -> Option<i32> {
        // SAFETY: `fcntl` with `F_GETFD` only inspects the (possibly closed) descriptor number
        // and retains no pointer.
        let rc = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if rc >= 0 {
            None
        } else {
            IoError::last_os_error().raw_os_error()
        }
    }

    #[test]
    fn drop_closes_both_pipe_ends() {
        let _guard = guard();
        let (read_fd, write_fd) = {
            let facility = SignalFacility::new().expect("a self-pipe");
            (facility.read_fd(), facility.write_fd)
        };
        assert_eq!(fcntl_errno(read_fd), Some(libc::EBADF), "read end closed");
        assert_eq!(fcntl_errno(write_fd), Some(libc::EBADF), "write end closed");
    }

    /// Raises `signo` on the calling thread and returns only once the disposition has run.
    ///
    /// `pthread_kill(self, …)` is deliberately used instead of `kill(getpid(), …)`: the latter
    /// is process-directed and the kernel may hand it to *any* thread that does not block the
    /// signal, whereas POSIX requires `pthread_kill` to deliver at least one of the target
    /// thread's unblocked pending signals **before it returns**. Since the target is this
    /// thread and its signals are unblocked, the trampoline has always returned by the next
    /// statement — which is what makes the assertions below deterministic rather than
    /// timing-dependent.
    fn raise_self_now(signo: libc::c_int) {
        // SAFETY: `pthread_self` has no preconditions; `pthread_kill` to the calling thread
        // with a valid signal number is well formed and only posts a signal to this thread.
        let rc = unsafe { libc::pthread_kill(libc::pthread_self(), signo) };
        assert_eq!(rc, 0, "pthread_kill(self, {signo}) succeeds");
    }

    #[test]
    fn the_trampoline_writes_exactly_one_byte_per_delivery() {
        let _guard = guard();
        let mut facility = SignalFacility::new().expect("a self-pipe");
        facility
            .set(Signal::SigUsr1, Disposition::Handle)
            .expect("install the trampoline");

        raise_self_now(libc::SIGUSR1);

        // Exactly one `write(2)` of exactly one byte, and that byte is the raw signal number —
        // the whole payload the async-signal-safe trampoline produces.
        let mut buf = [0u8; 8];
        assert_eq!(
            read_once(facility.read_fd(), &mut buf),
            (1, None),
            "one byte was written"
        );
        assert_eq!(
            buf[0] as libc::c_int,
            libc::SIGUSR1,
            "the byte is the signo"
        );
        // A second read finds nothing, so the handler wrote nothing else.
        let (n, errno) = read_once(facility.read_fd(), &mut buf);
        assert_eq!(n, -1);
        assert_eq!(errno, Some(libc::EAGAIN), "no further byte is written");
        // The bit — not the pipe — carries the report.
        assert_eq!(facility.drain(), vec![Signal::SigUsr1]);

        facility
            .set(Signal::SigUsr1, Disposition::Default)
            .expect("restore");
    }

    #[test]
    fn handle_installs_the_trampoline_with_sa_restart_and_an_empty_mask() {
        let _guard = guard();
        let mut facility = SignalFacility::new().expect("a self-pipe");
        facility
            .set(Signal::SigUsr1, Disposition::Handle)
            .expect("install the trampoline");

        // SAFETY: a `NULL` `act` makes `sigaction(2)` a pure query; `action` is a valid,
        // writable out-parameter the kernel fills with the current action.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: as above — a valid signal number and a live out-pointer.
        let rc = unsafe { libc::sigaction(Signal::SigUsr1.signo(), std::ptr::null(), &mut action) };
        assert_eq!(rc, 0, "the kernel answers the disposition query");

        // The handler really is *our* trampoline (the async-signal-safe one documented on the
        // module), not some other library's.
        assert_eq!(
            action.sa_sigaction, trampoline as *const () as libc::sighandler_t,
            "the trampoline is the installed handler"
        );
        assert_ne!(
            action.sa_flags & libc::SA_RESTART,
            0,
            "SA_RESTART is set (this runtime's documented equivalent of the C's EINTR loops)"
        );
        // An empty `sa_mask`: the handler blocks nothing extra while it runs. That is safe
        // precisely because it only touches atomics and `write(2)` — it can never deadlock
        // against a signal it refused to mask.
        for sig in Signal::ALL {
            // SAFETY: `action.sa_mask` is an initialized `sigset_t` the kernel just returned.
            let member = unsafe { libc::sigismember(&action.sa_mask, sig.signo()) };
            assert_eq!(member, 0, "{} is not in the handler's mask", sig.name());
        }

        facility
            .set(Signal::SigUsr1, Disposition::Default)
            .expect("restore");
    }

    #[test]
    fn a_stray_pipe_byte_is_not_a_signal() {
        let _guard = guard();
        let mut facility = SignalFacility::new().expect("a self-pipe");
        // A byte in the pipe that no bitmask bit backs (a left-over/coalesced write, or one
        // from a signal this facility does not map) must not fabricate a report: the pipe is
        // only a wake-up, the bitmask is the record.
        let fd = WRITE_FD.load(Ordering::SeqCst);
        assert!(fd >= 0, "a live facility publishes its write end");
        let mut byte = [0x2fu8];
        // SAFETY: `WRITE_FD` is this facility's live, non-blocking pipe write end and `byte` is
        // a valid, initialized 1-byte buffer; `write(2)` retains no pointer.
        let wrote = unsafe { libc::write(fd, byte.as_mut_ptr().cast(), 1) };
        assert_eq!(wrote, 1, "the stray byte lands in the pipe");
        assert!(
            facility.drain().is_empty(),
            "a stray byte with no pending bit reports nothing"
        );
        // `drain` consumed it, so readiness does not persist.
        assert_eq!(read_once(facility.read_fd(), &mut byte).0, -1);
    }

    #[test]
    fn a_pending_bit_without_a_pipe_byte_is_still_reported() {
        let _guard = guard();
        let mut facility = SignalFacility::new().expect("a self-pipe");
        // The converse of the stray-byte case: a delivery whose `write(2)` was dropped (a full
        // pipe, or no write end published) still set the bit, and the signal must not be lost.
        PENDING.fetch_or(1u64 << Signal::SigUsr2.bit(), Ordering::SeqCst);
        assert_eq!(facility.drain(), vec![Signal::SigUsr2]);
        assert!(facility.drain().is_empty(), "reported exactly once");
    }

    #[test]
    fn dropping_a_superseded_facility_does_not_clobber_its_successor() {
        let _guard = guard();
        let first = SignalFacility::new().expect("a self-pipe");
        let first_write = first.write_fd;
        let mut second = SignalFacility::new().expect("a self-pipe");
        let second_write = second.write_fd;
        assert_ne!(
            first_write, second_write,
            "each live facility owns its own pipe"
        );

        // A delivery the successor has not collected yet.
        PENDING.fetch_or(1u64 << Signal::SigUsr1.bit(), Ordering::SeqCst);
        drop(first);
        assert_eq!(
            WRITE_FD.load(Ordering::SeqCst),
            second_write,
            "the successor's write end is not cleared by a superseded drop"
        );
        assert_eq!(
            second.drain(),
            vec![Signal::SigUsr1],
            "the successor's pending report is not discarded by a superseded drop"
        );

        // Dropping the *live* facility clears the global write end.
        drop(second);
        assert_eq!(WRITE_FD.load(Ordering::SeqCst), -1);
        assert_eq!(PENDING.load(Ordering::SeqCst), 0);
    }
}
