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

//! The `mio`-backed event poller/reactor and its readiness newtypes.
//!
//! [`Poller`] wraps a portable [`mio::Poll`] (epoll on Linux, kqueue elsewhere) and
//! exposes a **mio-independent** registration API: callers register **raw file
//! descriptors** ([`RawFd`]) with an [`Interest`] mask and an opaque `u64` key, then
//! receive owned [`Event`]s from [`Poller::poll`]. This mirrors the legacy C poll loop's
//! one-shot readiness model (`erts/emulator/sys/common/erl_poll.c`, `erl_check_io.c`)
//! while letting a future `io_uring`/`/dev/poll` backend be swapped in behind the same
//! API.
//!
//! [`Token`] `0` is reserved for the poller's own asynchronous [`Waker`]; user
//! registrations start at token `1`. The `u64` "key" travels opaquely with every event so
//! the reactor ([`crate::port::PortRegistry::handle_events`]) can look the owning port up
//! by its port number without the poller knowing what a port is. The key of a waker event
//! is [`WAKER_KEY`].
//!
//! # The regular-file fallback readiness source
//!
//! `mio`'s Linux backend is `epoll`, and `epoll` refuses **regular files**: `EPOLL_CTL_ADD`
//! answers `EPERM` for a file (or a directory). The reference runtime has no such
//! limitation — it uses a `poll(2)`/select-based readiness path
//! (`erts/emulator/sys/common/erl_poll.c`) in which a regular file is trivially *always*
//! ready. To match that behaviour without abandoning `mio` for the descriptors it serves
//! well, [`Poller::register`] keeps an internal **fallback** set: a descriptor `mio`
//! rejects as unsupported (a regular file) is watched with a non-blocking `poll(2)`
//! instead, and [`Poller::poll`] **merges** the readiness of the two sources into the same
//! [`Event`]/[`Ready`] shape. A regular file reports [`Ready::readable`] (`POLLIN`) when it
//! has data or has reached end-of-file, and [`Ready::writable`] (`POLLOUT`) when it can be
//! written — which is essentially always, exactly as `poll(2)` defines it.
//!
//! The fallback set is wired through [`Poller::register`]/[`Poller::reregister`]/
//! [`Poller::deregister`]/[`Poller::poll`]/[`Poller::registration`]/[`Poller::len`]/
//! [`Poller::is_empty`] consistently, so a caller cannot tell which source holds a
//! registration apart from the fact that the descriptor is a regular file. Because a
//! fallback descriptor is *level*-triggered and always ready, [`Poller::poll`] never blocks
//! indefinitely while the fallback set is non-empty (see [`FALLBACK_POLL_CAP`]), and the
//! driver that owns the descriptor is expected to de-register it once it reaches
//! end-of-file — otherwise it would report readiness forever.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::io::Result as IoResult;
use std::os::unix::io::RawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_common::Result;

/// The key carried by the poller's own waker event.
///
/// It is [`u64::MAX`], chosen so it cannot collide with a port number used as a
/// registration key.
pub const WAKER_KEY: u64 = u64::MAX;

/// The longest [`Poller::poll`] will wait on the `mio` backend while a **fallback**
/// registration exists.
///
/// A fallback descriptor is a regular file, which `poll(2)` reports ready essentially
/// always; it can therefore never be the thing a blocking wait is waiting for. Capping the
/// `mio` wait to this short slice keeps [`Poller::poll`] responsive to the fallback set —
/// and stops `poll(None)` (wait forever) from blocking on `mio` while a fallback descriptor
/// has readiness to report. When the fallback probe already produced events, no wait is
/// taken at all.
const FALLBACK_POLL_CAP: Duration = Duration::from_millis(1);

/// One registration served by the fallback readiness source instead of `mio`.
///
/// Linux `epoll` (and some other `mio` backends) refuse a **regular file**: `EPOLL_CTL_ADD`
/// answers `EPERM`. Such a descriptor is watched with `poll(2)` instead, which reports a
/// regular file as readable — and, when it is open for writing, writable — essentially
/// always.
#[derive(Clone, Copy, Debug)]
struct FallbackReg {
    /// The watched descriptor.
    fd: RawFd,
    /// The readiness directions the caller is interested in.
    interest: Interest,
}

/// An opaque registration handle returned by [`Poller::register`].
///
/// Token `0` is reserved for the poller's internal [`Waker`]; user registrations are
/// assigned increasing tokens starting at `1`. A token is stable for the life of the
/// registration and is how a caller re-registers or de-registers a descriptor.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Token(usize);

impl Token {
    /// The raw index of this token.
    pub fn get(self) -> usize {
        self.0
    }
}

/// A readiness mask requested at registration time.
///
/// It is a tiny bitset: whether the caller wants to be notified when the descriptor is
/// readable and/or writable. Use the [`Interest::READABLE`], [`Interest::WRITABLE`] and
/// [`Interest::BOTH`] constants or the [`Interest::readable`]/[`Interest::writable`]
/// constructors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Interest {
    bits: u8,
}

impl Interest {
    /// Interested in readability only.
    pub const READABLE: Interest = Interest { bits: 0b01 };

    /// Interested in writability only.
    pub const WRITABLE: Interest = Interest { bits: 0b10 };

    /// Interested in both readiness directions.
    pub const BOTH: Interest = Interest { bits: 0b11 };

    /// An [`Interest`] in readability only (same as [`Interest::READABLE`]).
    pub const fn readable() -> Interest {
        Interest::READABLE
    }

    /// An [`Interest`] in writability only (same as [`Interest::WRITABLE`]).
    pub const fn writable() -> Interest {
        Interest::WRITABLE
    }

    /// Whether this mask includes readability.
    pub fn is_readable(self) -> bool {
        self.bits & 0b01 != 0
    }

    /// Whether this mask includes writability.
    pub fn is_writable(self) -> bool {
        self.bits & 0b10 != 0
    }
}

/// The readiness actually observed for a descriptor.
///
/// Unlike [`Interest`], a [`Ready`] value reports what the operating system observed in a
/// [`Poller::poll`] round: readability, writability, an error condition and the
/// read/write half-close (EOF) edges.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Ready {
    readable: bool,
    writable: bool,
    error: bool,
    read_closed: bool,
    write_closed: bool,
}

impl Ready {
    /// Whether the descriptor is readable.
    pub fn readable(self) -> bool {
        self.readable
    }

    /// Whether the descriptor is writable.
    pub fn writable(self) -> bool {
        self.writable
    }

    /// Whether the descriptor reported an error condition.
    pub fn is_error(self) -> bool {
        self.error
    }

    /// Whether the read half of the descriptor was closed by the peer (EOF for reads).
    pub fn is_read_closed(self) -> bool {
        self.read_closed
    }

    /// Whether the write half of the descriptor was closed by the peer.
    pub fn is_write_closed(self) -> bool {
        self.write_closed
    }
}

/// One readiness event produced by [`Poller::poll`].
///
/// It pairs the opaque [`Token`] assigned at registration with the caller's `u64` key and
/// the observed [`Ready`] mask, so the reactor can find the owning port by key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Event {
    token: Token,
    key: u64,
    ready: Ready,
}

impl Event {
    /// Builds an event from its parts (used by the poller and by the port registry's
    /// tests).
    pub fn new(token: Token, key: u64, ready: Ready) -> Event {
        Event { token, key, ready }
    }

    /// The registration token this event belongs to.
    pub fn token(self) -> Token {
        self.token
    }

    /// The caller key supplied at registration time (a port number for port events).
    pub fn key(self) -> u64 {
        self.key
    }

    /// The observed readiness.
    pub fn ready(self) -> Ready {
        self.ready
    }
}

/// The reactor: a `mio::Poll` plus the token→key registration bookkeeping.
///
/// Register descriptors with [`Poller::register`] (giving each an [`Interest`] and a `u64`
/// key), drain readiness with [`Poller::poll`] and hand the returned [`Event`]s to the
/// port registry. Token `0` is reserved for the poller's own [`Waker`], which it creates
/// once and hands out shared handles to via [`Poller::waker`]; user tokens start at `1`.
///
/// A descriptor `mio` refuses as unsupported — a regular file — is kept in an internal
/// `poll(2)`-based **fallback** set instead, and its readiness is merged into
/// [`Poller::poll`]'s result (see the module docs).
pub struct Poller {
    /// The underlying portable poll set.
    poll: mio::Poll,
    /// The reusable event buffer drained by [`Poller::poll`].
    events: mio::Events,
    /// The single wakeup handle for this poll set, shared with every [`Waker`] handed out.
    waker: Arc<mio::Waker>,
    /// Token index → the caller key it was registered with.
    registrations: HashMap<usize, u64>,
    /// Registrations the `mio` backend refuses (regular files: `EPERM`), served by the
    /// `poll(2)`-based fallback source. These tokens are **also** in `registrations`, so
    /// `len`/`is_empty`/`registration` count both sources.
    fallbacks: HashMap<usize, FallbackReg>,
    /// The next token index to hand out (starts after the waker's token `0`).
    next_token: usize,
}

/// Translates this crate's [`Interest`] mask into `mio`'s.
///
/// `mio::Interest` is a non-zero bitset, so an empty mask (no readiness direction
/// requested) falls back to readability, which is always a legal registration.
fn to_mio_interest(interest: Interest) -> mio::Interest {
    match (interest.is_readable(), interest.is_writable()) {
        (true, true) => mio::Interest::READABLE.add(mio::Interest::WRITABLE),
        (false, true) => mio::Interest::WRITABLE,
        _ => mio::Interest::READABLE,
    }
}

/// Maps a `mio` event's readiness onto this crate's [`Ready`].
fn to_ready(event: &mio::event::Event) -> Ready {
    Ready {
        readable: event.is_readable(),
        writable: event.is_writable(),
        error: event.is_error(),
        read_closed: event.is_read_closed(),
        write_closed: event.is_write_closed(),
    }
}

/// Whether a `mio` registration failure means the descriptor cannot be served by the `mio`
/// readiness backend at all.
///
/// `epoll_ctl` reports `EPERM` for a target that does not support `epoll` — notably a
/// regular file or a directory — and some other backends use `EINVAL` for the same case.
/// Everything else (an already-registered descriptor's `EEXIST`, an unknown token's
/// `ENOENT`, `EBADF`, …) is a genuine failure and is reported to the caller unchanged.
fn needs_fallback(err: &std::io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EPERM) | Some(libc::EINVAL))
}

impl Poller {
    /// Creates an empty poller.
    ///
    /// The poller's single wakeup handle is created here, under the reserved token `0`;
    /// [`Poller::waker`] hands out cheap shared handles to it.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` if the operating system refuses to create the poll set or
    /// its wakeup handle.
    pub fn new() -> Result<Poller> {
        let poll = mio::Poll::new().map_err(crate::error::io)?;
        let waker = mio::Waker::new(poll.registry(), mio::Token(0)).map_err(crate::error::io)?;
        Ok(Poller {
            poll,
            events: mio::Events::with_capacity(1024),
            waker: Arc::new(waker),
            registrations: HashMap::new(),
            fallbacks: HashMap::new(),
            next_token: 1,
        })
    }

    /// Registers `fd` with the poller, requesting `interest` and tagging it with `key`.
    ///
    /// Returns the [`Token`] assigned to the registration. Registering the same
    /// `mio`-served `fd` twice is an error (`BeamError::Io`, `AlreadyExists` underlying) —
    /// the `mio` poll set has one membership per descriptor. A fallback registration over a
    /// regular file has no such exclusive membership and is a separate entry, so registering
    /// the same regular file twice yields two independent registrations.
    ///
    /// A descriptor the `mio` backend refuses as unsupported — a **regular file**
    /// (`EPOLL_CTL_ADD` ⇒ `EPERM`) — is registered with the poller's internal
    /// `poll(2)`-based fallback source instead, so it registers successfully and reports
    /// readiness through [`Poller::poll`] like any other descriptor (see the module docs).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the operating system rejects the registration for any
    /// reason other than the regular-file case, or `BeamError::SystemLimit` when the token
    /// space is exhausted.
    pub fn register(&mut self, fd: RawFd, interest: Interest, key: u64) -> Result<Token> {
        let index = self.next_token;
        if index == usize::MAX {
            return Err(crate::error::system_limit());
        }
        // `SourceFd` borrows the fd only for the duration of the syscall; the local
        // binding gives it a place expression to borrow, and the scope ends the borrow
        // before the descriptor is stored in the fallback set below.
        let mut fd = fd;
        let registered = {
            let mut source = mio::unix::SourceFd(&mut fd);
            self.poll.registry().register(
                &mut source,
                mio::Token(index),
                to_mio_interest(interest),
            )
        };
        match registered {
            Ok(()) => {}
            // A descriptor `mio` cannot watch (a regular file) is served by the fallback
            // `poll(2)` set rather than failing the registration.
            Err(err) if needs_fallback(&err) => {
                self.fallbacks
                    .insert(index, FallbackReg { fd, interest });
            }
            Err(err) => return Err(crate::error::io(err)),
        }
        self.registrations.insert(index, key);
        self.next_token = index.saturating_add(1);
        Ok(Token(index))
    }

    /// Changes the [`Interest`] mask of a previously registered descriptor.
    ///
    /// The descriptor's source (the `mio` poll set or the regular-file fallback set) is
    /// preserved unchanged.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when `token` is unknown or the operating system rejects the
    /// re-registration.
    pub fn reregister(&mut self, fd: RawFd, token: Token, interest: Interest) -> Result<()> {
        if let Some(reg) = self.fallbacks.get_mut(&token.get()) {
            reg.fd = fd;
            reg.interest = interest;
            return Ok(());
        }
        let mut fd = fd;
        let reregistered = {
            let mut source = mio::unix::SourceFd(&mut fd);
            self.poll.registry().reregister(
                &mut source,
                mio::Token(token.get()),
                to_mio_interest(interest),
            )
        };
        match reregistered {
            Ok(()) => Ok(()),
            // The descriptor turned out to need the fallback after all: move the
            // registration across, keeping its token and key.
            Err(err) => {
                if needs_fallback(&err) {
                    if self.registrations.contains_key(&token.get()) {
                        self.fallbacks
                            .insert(token.get(), FallbackReg { fd, interest });
                        return Ok(());
                    }
                }
                Err(crate::error::io(err))
            }
        }
    }

    /// Removes a registration, freeing its token.
    ///
    /// It is removed from whichever source holds it (the `mio` poll set or the fallback
    /// set).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when `token` is unknown or the operating system rejects the
    /// de-registration.
    pub fn deregister(&mut self, fd: RawFd, token: Token) -> Result<()> {
        if self.fallbacks.remove(&token.get()).is_some() {
            self.registrations.remove(&token.get());
            return Ok(());
        }
        let mut fd = fd;
        let mut source = mio::unix::SourceFd(&mut fd);
        self.poll
            .registry()
            .deregister(&mut source)
            .map_err(crate::error::io)?;
        self.registrations.remove(&token.get());
        Ok(())
    }

    /// Waits for readiness and returns the observed events.
    ///
    /// The events are returned **owned** so the caller can take `&mut Poller` again to
    /// re-register while iterating them. The readiness of the `mio` poll set and of the
    /// regular-file fallback set is **merged** into a single list. An empty result means the
    /// timeout elapsed with no readiness; `timeout = None` blocks until an event arrives
    /// (interruptible by [`Waker::wake`]) — except while a fallback registration exists, when
    /// the wait is capped at [`FALLBACK_POLL_CAP`] so the always-ready fallback descriptor
    /// cannot deadlock the wait (see the module docs).
    ///
    /// # An interrupted wait is retried, never reported as a failure
    ///
    /// A signal delivered to the process interrupts a blocking wait with `EINTR`, and —
    /// unlike most blocking syscalls — `epoll_wait`, `poll` and `select` are **never**
    /// restarted after a handler returns, not even under `SA_RESTART`. `signal(7)` says of
    /// them that they *"are never restarted after being interrupted by a signal handler,
    /// regardless of the use of `SA_RESTART`"*. A handled signal — such as one installed by
    /// [`crate::signal::SignalFacility`] — would therefore surface as a spurious
    /// `BeamError::Io` and break the runtime's poll loop, which drives this method. `poll`
    /// instead **retries** an [`ErrorKind::Interrupted`] wait, so a handled signal is never
    /// observable as a failure here.
    ///
    /// The retry **preserves the deadline**: for `timeout = Some(d)` the time already spent is
    /// subtracted from the budget, so repeated signals can never make the call block longer
    /// than `d`; for `timeout = None` the wait simply resumes. A signal facility is expected
    /// to be registered with this poller (that is what makes its self-pipe readiness an
    /// event), so a retry after `EINTR` normally returns the pipe's readiness at once instead
    /// of blocking again.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the wait fails for any reason other than
    /// [`ErrorKind::Interrupted`].
    pub fn poll(&mut self, timeout: Option<Duration>) -> Result<Vec<Event>> {
        // The budget is measured from here, so that a retry after an interrupted wait can
        // never extend the overall wait beyond the caller's requested `timeout`.
        let started = Instant::now();
        loop {
            // Probe the fallback set first: a regular file is level-triggered, so `poll(2)`
            // reports it ready as soon as it is watchable.
            let mut merged = match self.poll_fallbacks() {
                Ok(events) => events,
                // The fallback probe is a `poll(2)` too, so it is never `SA_RESTART`-restarted
                // either; retry it (with the remaining budget) rather than failing.
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => return Err(crate::error::io(err)),
            };
            let fallback_ready = !merged.is_empty();

            // Never block the `mio` wait indefinitely while a fallback registration exists;
            // if the fallback probe already produced events, do not wait on `mio` at all.
            let remaining = timeout.map(|limit| limit.saturating_sub(started.elapsed()));
            let mio_timeout = if self.fallbacks.is_empty() {
                remaining
            } else if fallback_ready {
                Some(Duration::ZERO)
            } else {
                // A fallback descriptor is always ready, so it can never be what a blocking
                // wait waits for: keep the `mio` wait short.
                let budget = remaining.unwrap_or(FALLBACK_POLL_CAP);
                Some(budget.min(FALLBACK_POLL_CAP))
            };

            match self.poll.poll(&mut self.events, mio_timeout) {
                Ok(()) => {}
                // `epoll_wait` is never restarted after a signal handler runs, so an `EINTR`
                // here is expected whenever a handled signal lands mid-wait. Retry instead of
                // failing; once the budget is spent `remaining` is zero, so the retried wait
                // returns immediately and the loop still cannot overshoot the deadline.
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => return Err(crate::error::io(err)),
            }

            let registrations = &self.registrations;
            for event in self.events.iter() {
                let index = event.token().0;
                // Token `0` is the waker (and any token the map no longer knows is reported as
                // a waker key, so it can never be mistaken for a port).
                let key = registrations.get(&index).copied().unwrap_or(WAKER_KEY);
                merged.push(Event::new(Token(index), key, to_ready(event)));
            }
            return Ok(merged);
        }
    }

    /// Probes the fallback (regular-file) registrations with a non-blocking `poll(2)` and
    /// returns an [`Event`] for each descriptor that is ready.
    ///
    /// A regular file reports `POLLIN` (and `POLLOUT` when it is open for writing)
    /// essentially always. It is the *driver's* job to read the descriptor to end-of-file
    /// and then de-register it, so a regular-file registration does not report readiness
    /// forever.
    ///
    /// The wait is `poll(2)` with a zero timeout, but its result is still reported as a
    /// [`std::io::Error`] rather than folded into "no readiness": the caller retries an
    /// [`ErrorKind::Interrupted`] probe and surfaces every other failure.
    fn poll_fallbacks(&self) -> IoResult<Vec<Event>> {
        if self.fallbacks.is_empty() {
            return Ok(Vec::new());
        }
        let mut fds: Vec<libc::pollfd> = Vec::with_capacity(self.fallbacks.len());
        let mut indices: Vec<usize> = Vec::with_capacity(self.fallbacks.len());
        // Iterate deterministically so a multi-descriptor batch is stable across runs.
        let mut sorted: Vec<usize> = self.fallbacks.keys().copied().collect();
        sorted.sort_unstable();
        for index in sorted {
            let reg = self
                .fallbacks
                .get(&index)
                .copied()
                .expect("the key came from this map");
            let mut events: libc::c_short = 0;
            if reg.interest.is_readable() {
                events |= libc::POLLIN;
            }
            if reg.interest.is_writable() {
                events |= libc::POLLOUT;
            }
            fds.push(libc::pollfd {
                fd: reg.fd,
                events,
                revents: 0,
            });
            indices.push(index);
        }
        // SAFETY: `fds` is a valid, writable array of initialized `pollfd`s and its length
        // is passed as the element count; a zero timeout makes this a non-blocking probe
        // that `poll(2)` only reads (the `fd`/`events` fields) and writes (the `revents`
        // fields). `poll` retains no pointer beyond the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) };
        if ready < 0 {
            // Report the failure (an interrupted probe included) instead of silently
            // treating it as "no readiness"; the caller retries `EINTR` and surfaces the rest.
            return Err(std::io::Error::last_os_error());
        }
        if ready == 0 {
            return Ok(Vec::new());
        }
        let mut events = Vec::new();
        for (pfd, &index) in fds.iter().zip(indices.iter()) {
            if pfd.revents == 0 {
                continue;
            }
            let ready = Ready {
                readable: pfd.revents & libc::POLLIN != 0,
                writable: pfd.revents & libc::POLLOUT != 0,
                error: pfd.revents & libc::POLLERR != 0,
                read_closed: pfd.revents & libc::POLLHUP != 0,
                write_closed: pfd.revents & libc::POLLHUP != 0,
            };
            let key = self
                .registrations
                .get(&index)
                .copied()
                .unwrap_or(WAKER_KEY);
            events.push(Event::new(Token(index), key, ready));
        }
        Ok(events)
    }

    /// Returns a thread-safe [`Waker`] that unblocks a concurrent [`Poller::poll`].
    ///
    /// The poller owns a **single** wakeup handle (created in [`Poller::new`]); this returns
    /// a cheap cloneable handle to it. Call it as many times as needed, from any thread — the
    /// wakeup handle stays alive for as long as the [`Poller`] does.
    ///
    /// The woken event carries the poller's reserved token `0` and [`WAKER_KEY`] as its
    /// key.
    ///
    /// # Errors
    ///
    /// This call cannot fail; it returns `Result` for API symmetry with the other poller
    /// methods.
    pub fn waker(&self) -> Result<Waker> {
        Ok(Waker(Arc::clone(&self.waker)))
    }
    /// The caller key a token was registered with, if the token is still live.
    ///
    /// A token is live whichever source holds it (the `mio` poll set or the regular-file
    /// fallback set).
    pub fn registration(&self, token: Token) -> Option<u64> {
        self.registrations.get(&token.get()).copied()
    }

    /// The number of live registrations (excluding the internal waker), counting both the
    /// `mio` poll set and the regular-file fallback set.
    pub fn len(&self) -> usize {
        self.registrations.len()
    }

    /// Whether the poller has no live registrations (in either source).
    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }
}

/// An asynchronous notification handle that unblocks a [`Poller::poll`].
///
/// It is a cheap [`Arc`]-backed handle to `mio`'s portable wakeup (eventfd/pipe), created
/// by (and owned by) the [`Poller`]; the woken event carries [`WAKER_KEY`]. Obtained from
/// [`Poller::waker`], it may be cloned and used any number of times from any thread;
/// `Waker::wake` is safe to call from any thread.
pub struct Waker(Arc<mio::Waker>);

impl Waker {
    /// Wakes the associated [`Poller`], interrupting a blocking [`Poller::poll`].
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the underlying wakeup fails.
    pub fn wake(&self) -> Result<()> {
        self.0.wake().map_err(crate::error::io)
    }
}

#[cfg(test)]
mod tests {
    use super::{to_mio_interest, Event, Interest, Poller, Ready, Token, WAKER_KEY};
    use beam_common::BeamError;
    use std::io::Write;
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    #[test]
    fn interest_constants_and_constructors_agree() {
        assert_eq!(Interest::readable(), Interest::READABLE);
        assert_eq!(Interest::writable(), Interest::WRITABLE);

        assert!(Interest::READABLE.is_readable());
        assert!(!Interest::READABLE.is_writable());

        assert!(Interest::WRITABLE.is_writable());
        assert!(!Interest::WRITABLE.is_readable());

        assert!(Interest::BOTH.is_readable());
        assert!(Interest::BOTH.is_writable());
    }

    #[test]
    fn interest_maps_onto_mio() {
        assert!(to_mio_interest(Interest::READABLE).is_readable());
        assert!(!to_mio_interest(Interest::READABLE).is_writable());

        assert!(to_mio_interest(Interest::WRITABLE).is_writable());
        assert!(!to_mio_interest(Interest::WRITABLE).is_readable());

        let both = to_mio_interest(Interest::BOTH);
        assert!(both.is_readable() && both.is_writable());
    }

    #[test]
    fn ready_and_event_accessors() {
        let ready = Ready {
            readable: true,
            writable: false,
            error: false,
            read_closed: true,
            write_closed: false,
        };
        assert!(ready.readable());
        assert!(!ready.writable());
        assert!(!ready.is_error());
        assert!(ready.is_read_closed());
        assert!(!ready.is_write_closed());
        assert_eq!(
            Ready::default(),
            Ready {
                readable: false,
                writable: false,
                error: false,
                read_closed: false,
                write_closed: false
            }
        );

        let event = Event::new(Token(3), 42, ready);
        assert_eq!(event.token(), Token(3));
        assert_eq!(event.token().get(), 3);
        assert_eq!(event.key(), 42);
        assert_eq!(event.ready(), ready);
    }

    #[test]
    fn a_fresh_poller_has_no_registrations() {
        let poller = Poller::new().expect("a poll set");
        assert!(poller.is_empty());
        assert_eq!(poller.len(), 0);
        assert_eq!(poller.registration(Token(1)), None);
    }

    #[test]
    fn waker_key_cannot_collide_with_a_port_number() {
        assert_eq!(WAKER_KEY, u64::MAX);
    }

    #[test]
    fn waker_handles_are_cheap_repeatable_clones() {
        let poller = Poller::new().expect("a poll set");
        // The poller owns the single wakeup handle; obtaining several handles to it must
        // not panic (mio permits only one `Waker` per poll set).
        let first = poller.waker().expect("a waker");
        let second = poller.waker().expect("a second waker");
        first.wake().expect("the first handle fires");
        second.wake().expect("the second handle fires");
        drop(first);
        second.wake().expect("the surviving handle still fires");
    }
    #[test]
    fn register_assigns_tokens_from_one_and_records_the_key() {
        let mut poller = Poller::new().expect("a poll set");
        let (_peer, fd) = UnixStream::pair().expect("socketpair");

        let first = poller
            .register(fd.as_raw_fd(), Interest::READABLE, 7)
            .expect("the fd registers");
        assert_eq!(first, Token(1), "user tokens start after the waker's 0");
        assert_eq!(poller.registration(first), Some(7));
        assert_eq!(poller.len(), 1);
        assert!(!poller.is_empty());

        // Registering the very same descriptor twice is rejected by the OS.
        let err = poller
            .register(fd.as_raw_fd(), Interest::READABLE, 8)
            .expect_err("a duplicate registration fails");
        assert!(matches!(err, BeamError::Io(_)), "mapped to BeamError::Io");
        assert_eq!(poller.len(), 1, "the failed registration recorded nothing");

        poller
            .deregister(fd.as_raw_fd(), first)
            .expect("deregisters");
        assert_eq!(poller.registration(first), None);
        assert!(poller.is_empty());
    }

    #[test]
    fn reregister_and_deregister_an_unknown_token_map_to_io() {
        let mut poller = Poller::new().expect("a poll set");
        let (_peer, fd) = UnixStream::pair().expect("socketpair");
        // Token 1 was never registered with this poller.
        let err = poller
            .reregister(fd.as_raw_fd(), Token(1), Interest::WRITABLE)
            .expect_err("an unknown token cannot be re-registered");
        assert!(matches!(err, BeamError::Io(_)));

        let err = poller
            .deregister(fd.as_raw_fd(), Token(1))
            .expect_err("an unknown token cannot be de-registered");
        assert!(matches!(err, BeamError::Io(_)));
    }

    /// Creates a unique temporary **regular file** holding `contents` and opens a fresh
    /// descriptor over it with `flags` (the caller owns and closes the descriptor).
    ///
    /// A regular file is exactly the descriptor `mio`'s `epoll` backend refuses, so every
    /// test below travels the poller's fallback readiness source.
    fn temp_regular_file(tag: &str, contents: &[u8], flags: libc::c_int) -> (PathBuf, RawFd) {
        let path = std::env::temp_dir().join(format!(
            "beam_io_poller_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("a clock")
                .as_nanos()
        ));
        std::fs::write(&path, contents).expect("the fixture file is written");
        let mut c_path = path.as_os_str().as_encoded_bytes().to_vec();
        c_path.push(0);
        // SAFETY: `c_path` is a NUL-terminated path owned by this call; `open(2)` only
        // reads it and returns a fresh descriptor owned by the caller.
        let fd = unsafe { libc::open(c_path.as_ptr().cast(), flags | libc::O_CLOEXEC) };
        assert!(fd >= 0, "the regular file opens");
        (path, fd)
    }

    /// Closes a descriptor and removes the fixture file a `temp_regular_file` created.
    fn cleanup(path: &PathBuf, fd: RawFd) {
        // SAFETY: `fd` was created by `temp_regular_file` and is not used again.
        let _ = unsafe { libc::close(fd) };
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_regular_file_registers_through_the_fallback_and_reports_readiness() {
        let (path, fd) = temp_regular_file("read", b"hello", libc::O_RDONLY);

        let mut poller = Poller::new().expect("a poll set");
        let token = poller
            .register(fd, Interest::READABLE, 42)
            .expect("a regular file registers (epoll would answer EPERM)");
        assert_eq!(token, Token(1), "user tokens still start after the waker's 0");
        assert_eq!(poller.registration(token), Some(42));
        assert_eq!(poller.len(), 1, "the fallback registration counts towards len");
        assert!(!poller.is_empty());

        // A regular file is always reported readable.
        let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].key(), 42);
        assert_eq!(events[0].token(), token);
        assert!(events[0].ready().readable(), "a regular file is always readable");
        assert!(!events[0].ready().is_error());
        // `poll(None)` (wait forever) must not block while a fallback registration exists.
        let started = Instant::now();
        let events = poller.poll(None).expect("poll succeeds");
        assert!(!events.is_empty(), "the fallback descriptor is ready");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "poll(None) returned promptly instead of blocking, took {:?}",
            started.elapsed()
        );

        poller
            .deregister(fd, token)
            .expect("the fallback registration de-registers");
        assert_eq!(poller.registration(token), None);
        assert!(poller.is_empty());

        cleanup(&path, fd);
    }

    #[test]
    fn the_mio_and_fallback_sources_merge_into_one_event_batch() {
        let (path, file_fd) = temp_regular_file("merge", b"data", libc::O_RDONLY);
        let (peer, local) = UnixStream::pair().expect("socketpair");

        let mut poller = Poller::new().expect("a poll set");
        let mio_token = poller
            .register(local.as_raw_fd(), Interest::READABLE, 7)
            .expect("the socket registers with mio");
        let fallback_token = poller
            .register(file_fd, Interest::READABLE, 9)
            .expect("the regular file registers with the fallback source");
        assert_eq!(poller.len(), 2, "len counts both sources");

        // Only the always-ready fallback descriptor is ready before the socket is written.
        let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
        assert_eq!(events.len(), 1, "the idle socket is not ready");
        assert_eq!(events[0].key(), 9);
        assert_eq!(events[0].token(), fallback_token);

        // Writing to the socket makes the mio descriptor ready too; both merge into one
        // batch.
        (&peer).write_all(b"x").expect("the peer accepts the write");
        let mut events = poller
            .poll(Some(Duration::from_secs(1)))
            .expect("poll succeeds");
        events.sort_by_key(|event| event.key());
        assert_eq!(events.len(), 2, "the mio and fallback events merge");
        assert_eq!(events[0].key(), 7);
        assert_eq!(events[0].token(), mio_token);
        assert!(events[0].ready().readable());
        assert_eq!(events[1].key(), 9);
        assert!(events[1].ready().readable());

        // Deregistering removes each from whichever set holds it.
        poller
            .deregister(local.as_raw_fd(), mio_token)
            .expect("the mio registration de-registers");
        assert_eq!(poller.len(), 1);
        poller
            .deregister(file_fd, fallback_token)
            .expect("the fallback registration de-registers");
        assert!(poller.is_empty());

        cleanup(&path, file_fd);
    }

    #[test]
    fn reregister_switches_the_interest_of_a_fallback_registration() {
        let (path, fd) = temp_regular_file("rw", b"content", libc::O_RDWR);

        let mut poller = Poller::new().expect("a poll set");
        let token = poller
            .register(fd, Interest::READABLE, 5)
            .expect("the regular file registers");

        let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
        assert_eq!(events.len(), 1);
        assert!(events[0].ready().readable());
        assert!(
            !events[0].ready().writable(),
            "only readability was requested"
        );

        poller
            .reregister(fd, token, Interest::WRITABLE)
            .expect("the fallback registration re-registers");
        let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].token(), token);
        assert!(
            events[0].ready().writable(),
            "a descriptor open for writing is writable"
        );
        assert!(
            !events[0].ready().readable(),
            "readability is no longer requested"
        );

        poller.deregister(fd, token).expect("de-registers");
        assert!(poller.is_empty());

        cleanup(&path, fd);
    }

    #[test]
    fn two_fallback_registrations_over_one_descriptor_are_independent() {
        // Two fallback registrations over the same descriptor are independent entries: the
        // poller keeps no per-fd index for the fallback set, so both are accepted (a
        // regular file has no exclusive epoll membership to clash with). Deregistering one
        // leaves the other live.
        let (path, fd) = temp_regular_file("dup", b"x", libc::O_RDONLY);
        let mut poller = Poller::new().expect("a poll set");
        let first = poller
            .register(fd, Interest::READABLE, 1)
            .expect("the first fallback registration");
        let second = poller
            .register(fd, Interest::READABLE, 2)
            .expect("a second fallback registration is accepted");
        assert_ne!(first, second);
        assert_eq!(poller.len(), 2);

        let events = poller.poll(Some(Duration::ZERO)).expect("poll succeeds");
        assert_eq!(events.len(), 2, "both registrations report readiness");

        poller.deregister(fd, first).expect("the first de-registers");
        assert_eq!(poller.len(), 1);
        poller.deregister(fd, second).expect("the second de-registers");
        assert!(poller.is_empty());

        cleanup(&path, fd);
    }
}
