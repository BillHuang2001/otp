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

//! Descriptor-close facility — a safe `close(2)` floor plus the [`OwnedFd`] RAII wrapper.
//!
//! A descriptor the *runtime* decides not to adopt has to be released, and `beam_rt` denies
//! `unsafe_code`, so the raw syscall cannot live there. This module is the crate's **safe, public**
//! close primitive — the same role [`crate::fs_access`] plays for `access(2)`/`geteuid(2)` and
//! [`crate::cpu_time`] for `getrusage(2)`.
//!
//! It is deliberately *not* the crate's only `close(2)`: the drivers close the descriptors **they**
//! own in their own modules (`drivers/fd.rs`, `drivers/efile.rs`, `drivers/spawn.rs`,
//! `drivers/inet/sys.rs`, `poller.rs`, `signal.rs`), exactly as the C drivers close their own
//! descriptors. What this module owns is the *hand-off* case — a descriptor arriving from outside a
//! driver ([`InetAsyncResult::Accepted`]) that the runtime must either adopt or release, with no
//! `unsafe` of its own.
//!
//! # Why the runtime needs it
//!
//! An asynchronous socket operation can hand its owner a descriptor it must then dispose of
//! itself: `PACKET_REQ_RECV`/`ACCEPT` complete with [`InetAsyncResult::Accepted`], which is
//! *owned* — `beam_io` never stores it and never closes it, by contract
//! (`src/completion.rs`). The runtime either adopts it into a second port (`from_socket`,
//! `INET_REQ_FDOPEN`) or closes it. Before this module existed the close was a
//! `drop(FdDriver::read_only(fd))` idiom — dragging a whole `Driver` (and its `Drop`) into the
//! runtime just to reach `close(2)`.
//!
//! [`InetAsyncResult::Accepted`]: crate::completion::InetAsyncResult::Accepted
//!
//! # See also
//!
//! `drivers/inet/sys.rs` has a **private** helper of the same name (`fn close_fd`, used for the
//! socket descriptors that driver owns — `close(2)` plus the `EBADF`-tolerant error mapping the
//! driver's teardown needs). The two are intentionally distinct: that one is an internal detail of
//! the inet driver, this one is the crate's public seam. Do not confuse them, and do not call the
//! private one from outside `drivers::inet`.
//!
//! # Two ways to use it
//!
//! * [`close_fd`] — the bare call: close *now* and learn whether the kernel refused.
//! * [`OwnedFd`] — the RAII form: take a `RawFd` that nobody else owns and let its `Drop` (or an
//!   explicit [`OwnedFd::close`]) release it exactly once.
//!
//! # Do not double-own a descriptor
//!
//! A raw descriptor number carries no ownership. Everything in this crate that *takes* one —
//! [`FdDriver::from_fds`](crate::drivers::FdDriver), [`EfileDriver`](crate::drivers::EfileDriver),
//! [`TcpInetDriver::from_socket`](crate::drivers::TcpInetDriver), [`UdpInetDriver`], the spawn
//! driver's `{fd, N}` adoption — closes it on `stop`/`Drop`. Passing the same number to
//! [`OwnedFd::from_raw`] **as well** means two owners of one descriptor: the second close either
//! reports `EBADF` or, worse, closes a descriptor the kernel has meanwhile handed to somebody
//! else. Wrap a descriptor in an [`OwnedFd`] only when *this* is the sole owner (an accepted
//! socket the runtime is about to discard, a descriptor it opened itself), and use
//! [`OwnedFd::into_raw`] to hand ownership to a driver.
//!
//! [`UdpInetDriver`]: crate::drivers::UdpInetDriver
//!
//! # Safety
//!
//! `unsafe` appears **only** in the `close(2)` call inside [`close_fd`], with an inline
//! `// SAFETY:` rationale. Every other item here is safe Rust, and no caller ever has to uphold
//! a safety contract.

use std::fmt;
use std::os::unix::io::{AsRawFd, RawFd};

use beam_common::{BeamError, Result};

/// Close `fd` — the crate's **safe, public** `close(2)` primitive, errno-preserving.
///
/// This is the form a caller outside the drivers uses (the runtime, for a descriptor it owns or
/// decided not to adopt); a driver closes the descriptors it owns inside its own module (see the
/// module docs, and `drivers/inet/sys.rs`'s private same-named helper).
///
/// The descriptor is released whatever this returns (a failed `close(2)` still drops the
/// process's reference to the file description on every unix this crate targets), so a caller
/// must **not** treat an error as "still open, try again later".
///
/// # Errors
///
/// * [`BeamError::Io`] carrying `close(2)`'s own `errno` — `EBADF` for a descriptor that is not
///   open (a re-close, a number never handed out, a negative value). The failure is reported
///   honestly rather than swallowed, so a double close is observable instead of silent.
///
/// A wait interrupted by a signal (`EINTR`) is retried, the same tolerance
/// [`try_access`](crate::fs_access::try_access) gives `access(2)` and
/// [`Poller`](crate::poller::Poller) gives `poll(2)`. Note the Linux caveat: their `close(2)`
/// man page states the descriptor *is* released even when `EINTR` is reported, so the retry is
/// there for unices whose semantics leave it pending — a caller that must not risk closing a
/// reused number should hold the descriptor in an [`OwnedFd`] and not rely on the retry.
pub fn close_fd(fd: RawFd) -> Result<()> {
    loop {
        // SAFETY: `close(2)` takes only the descriptor number by value. It dereferences no
        // pointer this process supplies, retains nothing after returning and writes nothing into
        // our memory, so any `c_int` is a valid argument: a number that is not an open descriptor
        // fails with `EBADF` instead of touching foreign memory. The failure is read back
        // immediately (`errno` is thread-local and nothing else runs in between).
        let rc = unsafe { libc::close(fd) };
        if rc == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(BeamError::Io(err));
    }
}

/// A descriptor this process owns exclusively: closed **exactly once**, on [`OwnedFd::close`] or
/// on `Drop`.
///
/// Construct it with [`OwnedFd::from_raw`] from a descriptor nobody else owns (see the
/// module-level double-ownership warning), read the number with [`OwnedFd::as_raw_fd`] or the
/// [`AsRawFd`] impl, give ownership away with [`OwnedFd::into_raw`], or release it eagerly and
/// observe the kernel's answer with [`OwnedFd::close`].
///
/// `Drop` ignores the result — a destructor must not panic — and [`OwnedFd::close`] is the form
/// that reports. Not `Send`/`Sync` by omission? It is both (`RawFd` is a `c_int`); a descriptor
/// may legitimately be closed from any thread, exactly as `close(2)` allows.
#[derive(Eq, PartialEq, Hash)]
pub struct OwnedFd {
    fd: RawFd,
}

impl OwnedFd {
    /// Take ownership of `fd`.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] for a negative descriptor: there is no descriptor to own, and
    /// wrapping `-1` would make [`OwnedFd::close`] report a spurious `EBADF`.
    ///
    /// Nothing else is validated — the number is *not* probed with `fcntl(2)`. Ownership is a
    /// caller-side contract: `fd` must be open and must not be closed by anyone else.
    pub fn from_raw(fd: RawFd) -> Result<OwnedFd> {
        if fd < 0 {
            return Err(BeamError::BadArg);
        }
        Ok(OwnedFd { fd })
    }

    /// The descriptor number, for handing to a syscall that only borrows it.
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd
    }

    /// Relinquish ownership: return the number **without** closing it.
    ///
    /// Use this to pass a descriptor to a driver (which then owns it — see the module-level
    /// double-ownership warning) or to any other owner. Because `self` is consumed, no `Drop`
    /// runs, so the number stays open.
    pub fn into_raw(self) -> RawFd {
        let fd = self.fd;
        // `self` is consumed here; skipping its destructor is exactly the point (another owner
        // takes over the descriptor), so no `close(2)` is issued.
        std::mem::forget(self);
        fd
    }

    /// Close the descriptor now, reporting the kernel's answer, and consume the handle so no
    /// second close can happen.
    ///
    /// # Errors
    ///
    /// [`BeamError::Io`] with `close(2)`'s `errno` — see [`close_fd`]. The handle is consumed
    /// either way: `Drop` runs no further close, so an error is reported once, never retried
    /// behind the caller's back.
    pub fn close(self) -> Result<()> {
        // Consuming the handle first is what makes the exactly-once guarantee structural: the
        // `Drop` that would close a second time is skipped by `into_raw`.
        close_fd(self.into_raw())
    }
}

impl Drop for OwnedFd {
    fn drop(&mut self) {
        // A destructor cannot report (`Drop::drop` returns nothing) and must not panic, so the
        // kernel's answer is deliberately discarded here — [`OwnedFd::close`] is the form that
        // reports. The descriptor is released in every case.
        let _ = close_fd(self.fd);
    }
}

impl AsRawFd for OwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl fmt::Debug for OwnedFd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedFd").field("fd", &self.fd).finish()
    }
}

#[cfg(test)]
mod tests {
    //! # Why no test re-closes a descriptor number it just released
    //!
    //! `close(2)` answers `EBADF` for a released number **only while that number is still free**.
    //! Cargo runs a lib's tests on several threads of one process, so the moment a test closes `N`
    //! the kernel may hand `N` to another test's `UnixStream`/`File`/port — and a "second close is
    //! `EBADF`" assertion would then close *that* owner's descriptor. Measured: the process aborts
    //! with `fatal runtime error: IO Safety violation: owned file descriptor already closed`, i.e.
    //! one test's harness destroys another test's descriptor (std's `OwnedFd::drop` finds `EBADF`).
    //!
    //! The wrapper's documented guarantee — an `EBADF` is **reported**, never swallowed — is
    //! therefore pinned on descriptor numbers that can never be allocated (`-1`, and a value above
    //! any `RLIMIT_NOFILE` the kernel will ever grant), and every "the descriptor really went away"
    //! claim is pinned on the deterministic peer observation (`EPIPE`), never on a re-close.
    use super::{close_fd, OwnedFd};
    use beam_common::BeamError;
    use std::io::{ErrorKind, Write};
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::os::unix::net::UnixStream;

    /// A number `close(2)` always refuses and no allocation can ever produce: descriptor numbers are
    /// `int`s below the process's `RLIMIT_NOFILE`, which the kernel caps well under this one, so no
    /// thread can turn it into a live descriptor while a test asserts `EBADF` on it.
    const UNALLOCATABLE_FD: RawFd = RawFd::MAX - 1;

    /// A `socketpair(2)` whose first end is returned as a bare descriptor number
    /// (`std::mem::forget` keeps the `UnixStream` from closing it) plus the surviving peer.
    ///
    /// The second end is the crate-independent closure probe: writing to it fails with `EPIPE`
    /// exactly when the first end's descriptor has been released, which lets a test observe the
    /// close without a second syscall wrapper (`fcntl(2)`/`dup(2)`) of its own.
    fn pair() -> (RawFd, UnixStream) {
        let (owned, peer) = UnixStream::pair().expect("a socketpair");
        let fd = owned.as_raw_fd();
        std::mem::forget(owned);
        (fd, peer)
    }

    /// Did `peer` observe its peer's descriptor being closed? (`EPIPE` on write.)
    fn peer_sees_closed(peer: &mut UnixStream) -> bool {
        match peer.write_all(b"probe") {
            Ok(()) => false,
            Err(err) => {
                assert_eq!(
                    err.kind(),
                    ErrorKind::BrokenPipe,
                    "an unexpected write error: {err}"
                );
                true
            }
        }
    }

    /// The `errno` behind an error, if it carries one.
    fn errno(err: &BeamError) -> Option<i32> {
        match err {
            BeamError::Io(io) => io.raw_os_error(),
            _ => None,
        }
    }

    #[test]
    fn a_real_descriptor_closes_once_and_the_peer_observes_the_release() {
        let (fd, mut peer) = pair();
        assert!(!peer_sees_closed(&mut peer), "the peer starts out open");

        close_fd(fd).expect("the first close succeeds");
        assert!(peer_sees_closed(&mut peer), "the peer saw the close");
    }

    #[test]
    fn a_descriptor_that_was_never_open_reports_ebadf_instead_of_succeeding() {
        // -1 is the conventional "no descriptor" value; `close(2)` refuses it with `EBADF`.
        let err = close_fd(-1).expect_err("closing -1 cannot succeed");
        assert_eq!(errno(&err), Some(libc::EBADF));

        // The same refusal on a *plausible-looking* number that no allocation can ever produce, so
        // the reported `EBADF` comes from the wrapper's own error path and not from a number that
        // some other test happened to leave closed (see the module docs).
        let err = close_fd(UNALLOCATABLE_FD).expect_err("an unallocatable number cannot be closed");
        assert_eq!(errno(&err), Some(libc::EBADF));
    }

    #[test]
    fn into_raw_hands_the_descriptor_over_without_closing_it() {
        let (fd, mut peer) = pair();
        let owned = OwnedFd::from_raw(fd).expect("an owned descriptor");

        let raw = owned.into_raw();
        assert_eq!(raw, fd, "into_raw returns the same number");
        assert!(
            !peer_sees_closed(&mut peer),
            "into_raw must leave the descriptor open"
        );

        // ... and the number really is still closable afterwards (the caller owns it now).
        close_fd(raw).expect("the new owner closes it");
        assert!(peer_sees_closed(&mut peer), "the peer saw that close");
    }

    #[test]
    fn drop_closes_the_descriptor_exactly_once() {
        let (fd, mut peer) = pair();
        {
            let owned = OwnedFd::from_raw(fd).expect("an owned descriptor");
            assert!(!peer_sees_closed(&mut peer), "still open inside the scope");
            drop(owned);
        }
        assert!(
            peer_sees_closed(&mut peer),
            "Drop must close the descriptor"
        );
        // Exactly once: `Drop` runs once (the handle is consumed by it, never cloned), and the
        // single `close(2)` it made is what the peer observed above. A second `close(2)` cannot be
        // asked for: the number is free for another test to allocate, so re-closing it would close
        // that test's descriptor instead (see the module docs).
    }

    #[test]
    fn close_consumes_the_handle_so_a_second_close_is_impossible() {
        let (fd, mut peer) = pair();
        let owned = OwnedFd::from_raw(fd).expect("an owned descriptor");
        assert_eq!(owned.as_raw_fd(), fd, "as_raw_fd mirrors the number");
        assert_eq!(owned.as_raw_fd(), AsRawFd::as_raw_fd(&owned));
        assert!(
            format!("{owned:?}").contains(&fd.to_string()),
            "Debug shows it"
        );

        // `close(self)` takes the handle by value, so a second close of *this* owner is not
        // expressible; the release itself is observed through the peer.
        owned.close().expect("the explicit close succeeds");
        assert!(peer_sees_closed(&mut peer), "the descriptor is gone");
    }

    #[test]
    fn a_negative_descriptor_is_a_badarg_rather_than_a_wrapped_handle() {
        assert!(matches!(OwnedFd::from_raw(-1), Err(BeamError::BadArg)));
        // The value is not probed otherwise: ownership is a caller contract, so a plausible
        // number is accepted, and closing it really releases the descriptor it names.
        let (fd, mut peer) = pair();
        let owned = OwnedFd::from_raw(fd).expect("ownership is a caller contract");
        owned.close().expect("the accepted number closes once");
        assert!(peer_sees_closed(&mut peer), "that close released the fd");
    }
}
