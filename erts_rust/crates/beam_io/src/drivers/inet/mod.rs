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

//! [`TcpInetDriver`] — the built-in `tcp_inet` TCP socket driver.
//!
//! The Rust counterpart of the legacy C TCP socket driver (`tcp_inet`), the driver
//! `gen_tcp`/`inet_tcp` sit on: it owns **one** socket (a listening one or a connected one),
//! registers it with the poller and moves bytes between the socket and the port's two byte
//! queues.
//!
//! # Registered name and the control vocabulary
//!
//! The driver registers under `"tcp_inet"` — the name the oracle's `port_info(Port, name)`
//! reports for a TCP port, and the name the C passes to `driver_create_port` when it makes a
//! port for an accepted connection (`erts/emulator/drivers/common/inet_drv.c:1348`,
//! `:11834`).
//!
//! Where the C decodes a magic-offset command integer plus a packed binary, this floor takes
//! a **named** operation plus raw argument bytes in [`Driver::control`] (the runtime owns the
//! Erlang term decoding, which is out of scope here). The verbs, and the C request each one
//! mirrors, are:
//!
//! | verb | C request | argument | reply |
//! |---|---|---|---|
//! | `open` | `INET_REQ_OPEN` (`:11961`) | `[AF, TYPE, PROTO]` (3 bytes) | `[1]` |
//! | `bind` | `INET_REQ_BIND` (`:11409`) | packed address | `[1, port_hi, port_lo]` |
//! | `listen` | `INET_REQ_LISTEN` (`:12042`) | `[backlog_hi, backlog_lo]` | `[1]` |
//! | `accept` | `INET_REQ_ACCEPT` (`:12144`) | — | `[1, fd_be32]`, or `[1]` when nothing is pending |
//! | `connect` | `INET_REQ_CONNECT` (`:12064`) | packed address | `[1]` |
//! | `send` | `tcp_inet_command` (`:12629`) | the bytes to send | `[1]` |
//! | `recv` | `TCP_REQ_RECV` (`:12268`) | — | `[1]` |
//! | `shutdown` | `TCP_REQ_SHUTDOWN` (`:12339`) | `[how]` | `[1]` |
//! | `close` | `INET_REQ_CLOSE` (`:12259`) | — | `[1]` |
//!
//! A reply is the C's `ctl_reply` shape: a one-byte result tag ([`INET_REP_OK`] or
//! [`INET_REP_ERROR`], `:869-871`) followed by a payload — `ctl_reply`/`ctl_reply_ok`/
//! `ctl_error`/`ctl_xerror` (`:2260`-`:2311`). An **operational** failure (a socket that is
//! not connected, a descriptor that was never opened) is that `[0]`-tagged reply carrying the
//! lowercase errno name the oracle uses (`erl_errno_id`, `:2249`), while a **malformed
//! argument** or an **unknown verb** is [`beam_common::BeamError::BadArg`] — the crate's
//! argument-fault convention. The C reports the former as `ctl_error(EINVAL)`; this floor
//! reports it as `BadArg`.
//!
//! # The packed address
//!
//! `bind`/`connect` decode the C's packed address (`inet_set_faddress`/`inet_set_address`,
//! `:4795`/`:4706`): `[INET_AF_INET, port_hi, port_lo, a, b, c, d]` (7 bytes) for an explicit
//! IPv4 address, or `[INET_AF_ANY | INET_AF_LOOPBACK, port_hi, port_lo]` (3 bytes) for
//! `0.0.0.0`/`127.0.0.1` (`INET_AF_*`, `:756-763`). DNS resolution and IPv6/UDP are out of
//! scope for this floor, so any other family tag is a `BadArg`.
//!
//! # Readiness and flow control
//!
//! The poller's backend is **edge-triggered** (`mio` registers descriptors with `EPOLLET`), so
//! every callback drains to `EAGAIN`: [`Driver::ready_input`] reads until the socket is empty
//! (or reports EOF) and the write path loops until the output queue is drained or the kernel
//! send buffer is full.
//!
//! Unlike the built-in [`crate::drivers::FdDriver`] — which has a separate read fd and write
//! fd, and therefore two poller registrations — a socket is **one** descriptor, so this
//! driver keeps a single registration and flips its [`Interest`] mask: readable while
//! connected, plus writable while the output queue is non-empty (a short write leaves the
//! remainder queued and **arms** write readiness) and while an asynchronous `connect` is
//! completing. A fully drained queue drops writability again so the port cannot spin.
//!
//! `read(2) == 0` is end-of-file, and a connection reset (`ECONNRESET`/`ECONNABORTED` on a
//! read, `ECONNRESET`/`EPIPE` on a write) is the same event: the peer's close is **latched**
//! ([`TcpInetDriver::peer_closed`]), the socket's poller registration is released so a dead
//! descriptor cannot keep producing readiness, and the socket is not touched again. Every
//! later `recv`/`send` then answers the C's `[0] ++ "closed"` **once** and `[0] ++ "enotconn"`
//! after that — the C's `TCP_ADDF_DELAYED_CLOSE_RECV`/`TCP_ADDF_DELAYED_CLOSE_SEND`
//! bookkeeping (`inet_drv.c:855`-`:856`, `:12279`, `:12682`), which is how a caller that
//! ignores its send errors still learns about the close from its next receive — instead of
//! reading `0` repeatedly or writing into a socket the peer has gone from. Output still queued
//! when the close is latched can never be delivered, so it is dropped (the C's
//! `tcp_clear_output`, `inet_drv.c:12786`). Writes use `send(2)` with `MSG_NOSIGNAL`, so a
//! vanished peer is never a process-killing `SIGPIPE`.
//!
//! # One socket, one port (the `accept` divergence)
//!
//! The C's `INET_REQ_ACCEPT` creates a **second** Erlang port for the accepted socket
//! (`tcp_inet_copy` + `driver_create_port` + `async_ok_port`, `:12236`-`:12252`) and answers
//! the control call with a two-byte async id. Milestone-1's driver seam has no
//! port-creation capability — one [`crate::Driver`] is one port — so `accept` **transfers this
//! port to the connection**: the listener is deregistered and closed, the accepted descriptor
//! becomes the port's socket (registered [`Interest::READABLE`]) and the reply carries the
//! **new descriptor** (`[1] ++ fd_be32`) where the C carries the async id. The driver keeps
//! ownership of that descriptor; it is reported for correlation only. Multi-accept (one
//! listener serving many connections) is therefore deferred, as are the C's async-op
//! machinery (`INET_MAX_ASYNC = 1`, `:1069`) and its timeout handling.
//!
//! # Ownership
//!
//! The driver **owns** its descriptors: [`TcpInetDriver::from_socket`] adopts a caller's
//! socket, `open`/`accept` create their own, and every one of them is closed on `stop`, on the
//! `close` verb and on `Drop`; nothing else may close them. All raw socket syscalls
//! (`socket`/`bind`/`listen`/`accept`/`connect`/`getsockname`/`getsockopt`/`read`/`send`/
//! `shutdown`/`close`) live in this module, and every `unsafe` block carries a `// SAFETY:`
//! rationale.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c` and the
//! Erlang half `erts/preloaded/src/prim_inet.erl` (the magic number and the packed argument
//! encoders).

use std::os::unix::io::RawFd;

use beam_common::Result;

use crate::driver::{Driver, DriverContext};
use crate::drivers::fd::set_nonblocking;
use crate::error;
use crate::poller::{Interest, Token};

mod sys;
mod wire;

// The wire vocabulary (control codes, packed-address codec, errno renderer) lives in the
// private `wire` submodule; the constants are re-exported so callers keep reaching them at
// `crate::drivers::inet::*`.
pub use self::wire::{
    INET_AF_ANY, INET_AF_INET, INET_AF_LOOPBACK, INET_PROTO_DEFAULT, INET_PROTO_TCP,
    INET_REP_ERROR, INET_REP_OK, INET_TYPE_STREAM, TCP_SHUT_RD, TCP_SHUT_RDWR, TCP_SHUT_WR,
};
use self::wire::{ctl_error, ctl_ok, ctl_xerror, errno_name, parse_address};

// The raw `libc` socket wrappers (`socket`/`accept`/`bind`/`listen`/`getsockopt`/`shutdown`/
// `close`) live in the private `sys` submodule, so every `unsafe` block of the driver sits in
// one place.
use self::sys::{
    accept_socket, close_fd, has_peer, is_listening, local_port, shutdown_socket, socket_error,
    socket_fd,
};

/// The driver's registered name — the C `tcp_inet` entry (`inet_drv.c:1348`), which is also
/// what the oracle reports from `port_info(Port, name)` for a TCP port.
pub const DRIVER_NAME: &str = "tcp_inet";

/// The read buffer size of a single [`Driver::ready_input`] syscall.
const READ_CHUNK: usize = 64 * 1024;

/// The lifecycle state of a TCP port.
///
/// It mirrors the shape of the C descriptor's `inet.state` (`INET_STATE_*`): the socket does
/// not exist until `open`, `bind` leaves it open, `listen` makes it a listener, and
/// `connect`/`accept` make it a connection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InetState {
    /// No socket (the initial state, and the state [`TcpInetDriver::close`] leaves behind).
    Closed,
    /// A socket exists but is neither listening nor connected.
    Open,
    /// The socket is listening (`listen`).
    Listening,
    /// An asynchronous `connect` is in progress.
    Connecting,
    /// The socket is connected.
    Connected,
}

impl InetState {
    /// Whether the socket is connected ([`InetState::Connected`]).
    pub fn is_connected(self) -> bool {
        self == InetState::Connected
    }

    /// Whether the socket is listening ([`InetState::Listening`]).
    pub fn is_listening(self) -> bool {
        self == InetState::Listening
    }
}

/// The peer-close bookkeeping the C keeps in `TCP_ADDF_DELAYED_CLOSE_RECV`/
/// `TCP_ADDF_DELAYED_CLOSE_SEND` (`inet_drv.c:855`-`:856`).
///
/// Once the peer's close (end-of-file, or a connection reset) has been observed the socket is
/// unusable, but the C does **not** reply `closed` forever: the *first* `recv` and the *first*
/// `send` after the close report `closed`, and every later one reports `enotconn`. That is what
/// lets a caller which ignores its send errors still learn about the close from its next
/// receive (`inet_drv.c:12798`-`:12819`, `:13662`-`:13684`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct PeerClose {
    /// The peer's end-of-file (or a connection reset) has been observed.
    seen: bool,
    /// Whether a `recv` has already reported the close.
    recv_reported: bool,
    /// Whether a `send` has already reported the close.
    send_reported: bool,
}

impl PeerClose {
    /// Records that the peer's close has just been observed, arming the one-shot `closed`
    /// answer for **both** directions.
    fn observe(&mut self) {
        self.seen = true;
        self.recv_reported = false;
        self.send_reported = false;
    }

    /// The reply the next `recv` owes, or `None` while the connection is still usable.
    fn recv_reply(&mut self) -> Option<&'static str> {
        if !self.seen {
            return None;
        }
        if self.recv_reported {
            return Some("enotconn");
        }
        self.recv_reported = true;
        Some("closed")
    }

    /// The reply the next `send` owes, or `None` while the connection is still usable.
    fn send_reply(&mut self) -> Option<&'static str> {
        if !self.seen {
            return None;
        }
        if self.send_reported {
            return Some("enotconn");
        }
        self.send_reported = true;
        Some("closed")
    }
}

/// The TCP socket driver: one listening or connected socket behind one port.
///
/// Constructed either socket-less ([`TcpInetDriver::new`], the [`crate::DriverRegistry`]
/// factory the runtime uses, which then calls the `open` verb) or around an existing socket
/// ([`TcpInetDriver::from_socket`]).
pub struct TcpInetDriver {
    /// The socket this port owns, if one has been opened.
    socket: Option<RawFd>,
    /// The poller token of the socket's registration, once registered.
    token: Option<Token>,
    /// The interest mask currently registered for the socket (one registration: a socket is
    /// a single descriptor, so read and write readiness share the token).
    interest: Interest,
    /// The lifecycle state.
    state: InetState,
    /// Set once the socket reported end-of-file, so no further reads are attempted.
    read_eof: bool,
    /// How far the peer's close has been observed and reported ([`PeerClose`]).
    peer_close: PeerClose,
    /// A `shutdown` deferred because the output queue was not empty yet (the C's
    /// `TCP_ADDF_PENDING_SHUT_WR`/`TCP_ADDF_PENDING_SHUT_RDWR`, `inet_drv.c:12362`) — applied
    /// as soon as the queue drains.
    pending_shutdown: Option<u8>,
    /// The lowercase errno name of a failed **asynchronous** `connect`, if one failed after
    /// the control call had already answered `ok` (there is no reply channel left by then).
    connect_error: Option<String>,
}

impl TcpInetDriver {
    /// A TCP driver with no socket yet: the state a freshly opened port starts in.
    ///
    /// The socket is created by the `open` verb (or adopted by
    /// [`TcpInetDriver::from_socket`]), mirroring the C where `tcp_inet_start` builds only the
    /// descriptor and `INET_REQ_OPEN` creates the socket.
    pub fn new() -> TcpInetDriver {
        TcpInetDriver {
            socket: None,
            token: None,
            interest: Interest::READABLE,
            state: InetState::Closed,
            read_eof: false,
            peer_close: PeerClose::default(),
            pending_shutdown: None,
            connect_error: None,
        }
    }

    /// Adopts an existing socket, taking ownership of `fd` (closed on `stop`/`close`/`Drop`).
    ///
    /// The socket's state is read from the kernel, exactly as the C's `INET_REQ_FDOPEN` does
    /// (`inet_ctl_fdopen`, `inet_drv.c:5250`-`:5260`): a listening socket (`SO_ACCEPTCONN`)
    /// starts in [`InetState::Listening`], a socket that has a peer (`getpeername(2)`) in
    /// [`InetState::Connected`], and anything else in [`InetState::Open`]. `fd` is put into
    /// non-blocking mode by [`Driver::start`].
    ///
    /// A socket whose peer has already closed is adopted as connected; the close is discovered
    /// by the first read (which answers [`TcpInetDriver::peer_closed`]).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when `fd` is not a usable socket (`getsockopt` fails).
    pub fn from_socket(fd: RawFd) -> Result<TcpInetDriver> {
        let state = if is_listening(fd)? {
            InetState::Listening
        } else if has_peer(fd)? {
            InetState::Connected
        } else {
            InetState::Open
        };
        Ok(TcpInetDriver {
            socket: Some(fd),
            token: None,
            interest: Interest::READABLE,
            state,
            read_eof: false,
            peer_close: PeerClose::default(),
            pending_shutdown: None,
            connect_error: None,
        })
    }

    /// The socket this port owns, if one has been opened.
    pub fn socket(&self) -> Option<RawFd> {
        self.socket
    }

    /// The lifecycle state.
    pub fn state(&self) -> InetState {
        self.state
    }

    /// Whether the socket reported end-of-file.
    pub fn eof(&self) -> bool {
        self.read_eof
    }

    /// Whether the peer's close has been observed — a read end-of-file, or a connection reset
    /// seen by a read or a write.
    ///
    /// Once this is true the connection is gone: the socket is not touched again, output still
    /// queued was dropped, and the driver's `recv`/`send` answer the C's `[0] ++ "closed"`
    /// (once) and then `[0] ++ "enotconn"` (see [`PeerClose`]). The runtime reads it to emit
    /// its `{tcp_closed, Port}` notification and to fail a command the driver could not
    /// deliver with `{error, closed}` — it is the **only** new accessor the runtime needs; no
    /// [`crate::PortNotification`] variant is involved.
    pub fn peer_closed(&self) -> bool {
        self.peer_close.seen
    }

    /// The lowercase errno name of an asynchronous `connect` that failed after the control
    /// call had answered `ok`, if any.
    ///
    /// The control reply for `connect` is sent as soon as the connection attempt starts (the
    /// C answers `[1] ++ async_id` then too and reports the outcome as an
    /// `{inet_async, …}` message), so this is where the runtime reads that outcome from:
    /// `Some("econnrefused")` for a refused connection. It stays set until the socket is
    /// replaced or closed.
    pub fn connect_error(&self) -> Option<&str> {
        self.connect_error.as_deref()
    }

    /// Whether a socket has been opened (`INET_STATE_OPEN` in the C's sense: anything but
    /// [`InetState::Closed`]).
    fn has_socket(&self) -> bool {
        self.socket.is_some()
    }

    /// The connected socket, or `BadArg` when there is none.
    fn connected_socket(&self) -> Result<RawFd> {
        if !self.state.is_connected() {
            return Err(error::badarg());
        }
        self.socket.ok_or_else(error::badarg)
    }

    /// Makes `fd` this driver's socket and registers it [`Interest::READABLE`].
    ///
    /// A failed registration closes `fd` and leaves the driver socket-less, so nothing leaks.
    fn adopt(&mut self, ctx: &mut DriverContext<'_>, fd: RawFd) -> Result<()> {
        set_nonblocking(fd)?;
        match ctx.register(fd, Interest::READABLE) {
            Ok(token) => {
                self.socket = Some(fd);
                self.token = Some(token);
                self.interest = Interest::READABLE;
                self.read_eof = false;
                self.peer_close = PeerClose::default();
                self.connect_error = None;
                Ok(())
            }
            Err(err) => {
                close_fd(fd);
                Err(err)
            }
        }
    }

    /// Marks the socket as a fresh, usable connection.
    ///
    /// A `connect`/`accept` that succeeded replaces whatever the previous socket was doing, so
    /// the peer-close bookkeeping starts over too.
    fn mark_connected(&mut self) {
        self.state = InetState::Connected;
        self.read_eof = false;
        self.peer_close = PeerClose::default();
    }
    /// Changes the socket's registered interest mask (registering it the first time).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when there is no socket, or whatever the poller returns.
    fn set_interest(&mut self, ctx: &mut DriverContext<'_>, interest: Interest) -> Result<()> {
        let fd = self.socket.ok_or_else(error::badarg)?;
        match self.token {
            Some(token) => ctx.reregister(fd, token, interest)?,
            None => self.token = Some(ctx.register(fd, interest)?),
        }
        self.interest = interest;
        Ok(())
    }

    /// Removes the socket's poller registration, if it has one.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the poller rejects the de-registration.
    fn deregister(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        match (self.token.take(), self.socket) {
            (Some(token), Some(fd)) => ctx.deregister(fd, token),
            _ => Ok(()),
        }
    }

    /// Closes the owned descriptor without touching the poll set, and forgets it.
    ///
    /// Used after [`TcpInetDriver::deregister`] and by `Drop` (which has no poller to
    /// de-register with). Taking the field first makes it idempotent.
    fn close_socket(&mut self) {
        if let Some(fd) = self.socket.take() {
            close_fd(fd);
        }
        self.read_eof = false;
        self.peer_close = PeerClose::default();
        self.pending_shutdown = None;
        self.state = InetState::Closed;
    }

    /// De-registers **and** closes the current socket, returning the de-registration outcome.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the poller rejects the de-registration; the descriptor is
    /// closed either way.
    fn release_socket(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let result = self.deregister(ctx);
        self.close_socket();
        result
    }

    /// Reads the socket into the port input queue, draining to `EAGAIN`.
    ///
    /// A no-op unless the socket is connected (a listening socket is read through `accept`,
    /// not `read(2)`) and the peer's close has not already been observed. Returns the number of
    /// bytes read.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the read fails irrecoverably.
    fn read_available(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        if !self.state.is_connected() || self.peer_close.seen {
            return Ok(0);
        }
        let fd = self.connected_socket()?;
        let mut read = 0usize;
        let mut buf = [0u8; READ_CHUNK];
        loop {
            // SAFETY: `fd` is an open, non-blocking socket owned by this driver and `buf` is
            // a writable array of `READ_CHUNK` bytes, so `read(2)` writes at most `READ_CHUNK`
            // bytes into it.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), READ_CHUNK) };
            if n > 0 {
                let n = n as usize;
                ctx.input().push(&buf[..n]);
                read += n;
                // Edge-triggered readiness: keep reading until the socket is empty.
                continue;
            }
            if n == 0 {
                // The peer closed its end: latch the close so this `recv` answers `closed`
                // and no further read is ever attempted.
                self.read_eof = true;
                self.latch_peer_close(ctx)?;
                return Ok(read);
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return Ok(read),
                // A reset (or an aborted connection) is the peer's close, not a read failure:
                // the C's `tcp_recv` routes `ECONNRESET` to `tcp_recv_closed`
                // (`inet_drv.c:13202`).
                Some(libc::ECONNRESET) | Some(libc::ECONNABORTED) => {
                    self.read_eof = true;
                    self.latch_peer_close(ctx)?;
                    return Ok(read);
                }
                _ => return Err(error::io(err)),
            }
        }
    }

    /// Latches the peer's close and drops the output that can no longer be delivered.
    ///
    /// This is the C's close path (`tcp_recv_closed`/`tcp_send_error`): the socket is
    /// de-registered from the poller (a dead descriptor must not keep reporting readiness, the
    /// rule `crate::drivers::FdDriver` follows at end-of-file) and its queued output is
    /// discarded (`tcp_clear_output`, `inet_drv.c:12786`), because nothing can ever reach the
    /// peer again. The **next** `recv`/`send` answers `closed` and every later one `enotconn`
    /// (see [`PeerClose`]).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the poller rejects the de-registration.
    fn latch_peer_close(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.peer_close.observe();
        self.pending_shutdown = None;
        ctx.output().clear();
        self.deregister(ctx)
    }

    /// Drains the port's output queue to the socket, returning the number of bytes written.
    ///
    /// A short write (`EAGAIN`) leaves the remainder queued and arms write readiness; a fully
    /// drained queue drops write readiness again (keeping readability armed) and applies a
    /// `shutdown` that was deferred because the queue was not empty (the C's
    /// `TCP_ADDF_PENDING_SHUT_*`).
    ///
    /// A write that fails because the peer is gone (`ECONNRESET`/`EPIPE`) is **not** an I/O
    /// error: it latches the peer's close (see [`TcpInetDriver::peer_closed`]), exactly as the
    /// C's `tcp_send_error` routes to `tcp_recv_closed` (`inet_drv.c:13440`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when the socket is not connected, or
    /// `BeamError::Io` when the write fails irrecoverably.
    fn write_pending(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        if self.peer_close.seen {
            // The peer is gone: the queued output was dropped when the close was latched and
            // the socket must not be touched again.
            return Ok(0);
        }
        let fd = self.connected_socket()?;
        let mut written = 0usize;
        let mut stalled = false;
        loop {
            let front = ctx.output().front();
            if front.is_empty() {
                break;
            }
            let len = front.len();
            // SAFETY: `fd` is an open, non-blocking socket owned by this driver and `front`
            // points to `len` initialized bytes — `send(2)` reads at most that many and never
            // retains the pointer. `MSG_NOSIGNAL` keeps a vanished peer's `EPIPE` a return
            // value instead of a process-killing signal.
            let n = unsafe { libc::send(fd, front.as_ptr().cast(), len, libc::MSG_NOSIGNAL) };
            if n > 0 {
                let n = n as usize;
                ctx.output().consume(n);
                written += n;
                continue;
            }
            if n == 0 {
                // No progress: treat it like a full buffer and let readiness re-drive us
                // instead of spinning here.
                stalled = true;
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // A signal interrupted the write; retry.
                Some(libc::EINTR) => continue,
                // The kernel send buffer is full (`EWOULDBLOCK` shares `EAGAIN`'s value on
                // every platform this crate targets).
                Some(libc::EAGAIN) => {
                    stalled = true;
                    break;
                }
                // The peer vanished: a reset, or a write once its close was known. That is the
                // connection ending, so it is latched (and reported by the caller's reply)
                // rather than surfaced as a bare I/O error.
                Some(libc::ECONNRESET) | Some(libc::EPIPE) => {
                    self.latch_peer_close(ctx)?;
                    return Ok(written);
                }
                _ => return Err(error::io(err)),
            }
        }

        if ctx.output().is_empty() {
            if let Some(how) = self.pending_shutdown.take() {
                shutdown_socket(fd, how)?;
            }
            if self.interest.is_writable() {
                self.set_interest(ctx, Interest::READABLE)?;
            }
        } else if stalled && !self.interest.is_writable() {
            self.set_interest(ctx, Interest::BOTH)?;
        }
        Ok(written)
    }

    /// Completes an asynchronous `connect` signalled by write readiness.
    ///
    /// The pending socket error is taken (`SO_ERROR`, which `getsockopt` also clears): zero
    /// means the connection is up (read readiness is armed and the state becomes
    /// [`InetState::Connected`]); anything else means it failed, and the descriptor is closed
    /// — mirroring the C, which tears the descriptor down on a failed connect — with the
    /// errno recorded in [`TcpInetDriver::connect_error`].
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when there is no socket, or whatever the poller returns.
    fn finish_connect(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let fd = self.socket.ok_or_else(error::badarg)?;
        let err = socket_error(fd)?;
        if err == 0 {
            self.mark_connected();
            self.set_interest(ctx, Interest::READABLE)
        } else {
            self.release_socket(ctx)?;
            self.connect_error = Some(errno_name(err));
            Ok(())
        }
    }

    /// The `open` verb: creates the socket and registers it for read readiness.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::BadArg`] for an argument that is not the three-byte
    /// `[AF_INET, SOCK_STREAM, default|tcp]` record; `BeamError::Io` when the socket cannot be
    /// created.
    fn verb_open(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if self.has_socket() {
            // The C has no such guard (its OPEN would leak the old socket); this floor
            // refuses to replace a live socket with the operational-error reply instead.
            return Ok(ctl_xerror("einval"));
        }
        let [family, kind, protocol] = arg else {
            return Err(error::badarg());
        };
        // AF_INET / SOCK_STREAM / default-or-TCP only; IPv6, UDP and SCTP are out of scope.
        if *family != INET_AF_INET || *kind != INET_TYPE_STREAM {
            return Err(error::badarg());
        }
        if *protocol != INET_PROTO_DEFAULT && *protocol != INET_PROTO_TCP {
            return Err(error::badarg());
        }
        let fd = socket_fd()?;
        self.adopt(ctx, fd)?;
        self.state = InetState::Open;
        Ok(ctl_ok())
    }

    /// The `bind` verb: binds the socket and reports the port it ended up on.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for a malformed packed address; `BeamError::Io` when `bind(2)` or
    /// the follow-up `getsockname(2)` fails.
    fn verb_bind(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.has_socket() {
            return Ok(ctl_xerror("exbadport"));
        }
        let address = parse_address(arg)?;
        let fd = self.socket.ok_or_else(error::badarg)?;
        let sa = address.sockaddr_in();
        // SAFETY: `fd` is an open socket owned by this driver and `sa` is a fully initialized
        // `sockaddr_in`, so the kernel reads at most `size_of::<sockaddr_in>()` bytes of it
        // and never retains the pointer.
        let rc = unsafe {
            libc::bind(
                fd,
                std::ptr::addr_of!(sa).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Ok(ctl_error(std::io::Error::last_os_error()));
        }
        // A zero port asks the kernel for one, exactly as the C re-reads the address then
        // (`inet_drv.c:11445`); report the port actually bound either way.
        let port = if address.port == 0 {
            local_port(fd)?
        } else {
            address.port
        };
        let mut reply = ctl_ok();
        reply.extend_from_slice(&port.to_be_bytes());
        Ok(reply)
    }

    /// The `listen` verb: turns the socket into a listener.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for an argument that is not a two-byte backlog; `BeamError::Io`
    /// when `listen(2)` fails.
    fn verb_listen(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.has_socket() {
            return Ok(ctl_xerror("exbadport"));
        }
        let [hi, lo] = arg else {
            return Err(error::badarg());
        };
        let backlog = i32::from(u16::from_be_bytes([*hi, *lo])).min(i32::MAX);
        let fd = self.socket.ok_or_else(error::badarg)?;
        // SAFETY: `fd` is an open socket owned by this driver and `listen(2)` takes no
        // pointer arguments.
        if unsafe { libc::listen(fd, backlog) } != 0 {
            return Ok(ctl_error(std::io::Error::last_os_error()));
        }
        self.state = InetState::Listening;
        Ok(ctl_ok())
    }

    /// The `accept` verb: accepts one pending connection and hands the port to it.
    ///
    /// See the module docs for why the port transfers to the connection instead of a second
    /// port being created. The reply is `[1] ++ fd_be32` for a connection, and a bare `[1]`
    /// when none is pending yet (`EAGAIN`) — retry when the port reports read readiness.
    ///
    /// # Errors
    ///
    /// `BeamError::Io` when `accept(2)` fails for a reason other than `EAGAIN`, when the
    /// accepted descriptor cannot be put in non-blocking mode, or when the poller rejects its
    /// registration.
    fn verb_accept(&mut self, ctx: &mut DriverContext<'_>) -> Result<Vec<u8>> {
        if !self.state.is_listening() {
            return Ok(ctl_xerror("einval"));
        }
        let listener = self.socket.ok_or_else(error::badarg)?;
        let accepted = match accept_socket(listener) {
            Ok(fd) => fd,
            // Nothing to accept yet: the caller retries when the port reports readability.
            Err(err) if err.raw_os_error() == Some(libc::EAGAIN) => return Ok(ctl_ok()),
            Err(err) => return Ok(ctl_error(err)),
        };
        // The port follows its connection: release the listener, adopt the accepted socket.
        self.release_socket(ctx)?;
        if let Err(err) = self.adopt(ctx, accepted) {
            self.close_socket();
            return Err(err);
        }
        self.mark_connected();
        let mut reply = ctl_ok();
        reply.extend_from_slice(&(accepted as i32).to_be_bytes());
        Ok(reply)
    }

    /// The `connect` verb: connects the socket to a literal IPv4 address.
    ///
    /// A connection that completes immediately arms read readiness and answers `[1]`; one
    /// that reports `EINPROGRESS` arms **write** readiness, moves to
    /// [`InetState::Connecting`] and also answers `[1]` — the outcome then arrives through
    /// [`Driver::ready_output`] (see [`TcpInetDriver::connect_error`]).
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for a malformed packed address.
    fn verb_connect(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        match self.state {
            InetState::Closed => return Ok(ctl_xerror("exbadport")),
            InetState::Connected => return Ok(ctl_xerror("eisconn")),
            InetState::Connecting => return Ok(ctl_xerror("einval")),
            InetState::Open | InetState::Listening => {}
        }
        let address = parse_address(arg)?;
        let fd = self.socket.ok_or_else(error::badarg)?;
        let sa = address.sockaddr_in();
        // SAFETY: `fd` is an open, non-blocking socket owned by this driver and `sa` is a
        // fully initialized `sockaddr_in`, so the kernel reads at most
        // `size_of::<sockaddr_in>()` bytes of it and never retains the pointer.
        let rc = unsafe {
            libc::connect(
                fd,
                std::ptr::addr_of!(sa).cast(),
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if rc == 0 {
            self.mark_connected();
            self.set_interest(ctx, Interest::READABLE)?;
            return Ok(ctl_ok());
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINPROGRESS) => {
                self.state = InetState::Connecting;
                self.set_interest(ctx, Interest::WRITABLE)?;
                Ok(ctl_ok())
            }
            // `EISCONN` and `EALREADY` are the racy spellings of "it is already connected or
            // connecting"; report them as the C does rather than failing the control call.
            Some(libc::EISCONN) => {
                self.mark_connected();
                self.set_interest(ctx, Interest::READABLE)?;
                Ok(ctl_ok())
            }
            _ => Ok(ctl_error(err)),
        }
    }

    /// The `send` verb: queues `arg` for output and drains as much as the kernel accepts.
    ///
    /// This is the `tcp_inet_command` path (`inet_drv.c:12629`), which is the port's *output*
    /// callback in the C; the remainder stays queued (with write readiness armed) exactly as
    /// a short `write(2)` leaves it.
    ///
    /// Once the peer's close is known the verb answers `[0] ++ "closed"` **once** and
    /// `[0] ++ "enotconn"` after that (the C's `tcp_inet_commandv` plus its
    /// `TCP_ADDF_DELAYED_CLOSE_SEND` flag, `inet_drv.c:12682`); a `send` whose write is what
    /// discovers the close answers `closed` too.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` when the socket is not connected, or the write failure
    /// [`TcpInetDriver::write_pending`] reports.
    fn verb_send(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        if let Some(name) = self.peer_close.send_reply() {
            return Ok(ctl_xerror(name));
        }
        ctx.output().push(arg);
        let _written = self.write_pending(ctx)?;
        if self.peer_close.seen {
            // This write is what found the peer gone, so it answers `closed` itself; the C
            // does the same (`inet_reply_error_am(…, am_closed)`, `inet_drv.c:13672`) and
            // arms the next receive for `closed` as well.
            self.peer_close.send_reported = true;
            return Ok(ctl_xerror("closed"));
        }
        Ok(ctl_ok())
    }

    /// The `recv` verb: reads everything available into the port input queue.
    ///
    /// Once the peer's close is known the verb answers `[0] ++ "closed"` **once** and
    /// `[0] ++ "enotconn"` after that (the C's `TCP_ADDF_DELAYED_CLOSE_RECV`,
    /// `inet_drv.c:12279`); a `recv` whose read is what discovers the close answers `closed`
    /// too, and the bytes that read collected stay in the port input queue for the runtime to
    /// deliver.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for a non-empty argument (the C's `TCP_REQ_RECV` carries a timeout
    /// and a length this floor does not honor, so accepting them would silently ignore
    /// them — see the module docs), or the read failure [`TcpInetDriver::read_available`]
    /// reports.
    fn verb_recv(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        if let Some(name) = self.peer_close.recv_reply() {
            return Ok(ctl_xerror(name));
        }
        if !arg.is_empty() {
            return Err(error::badarg());
        }
        let read = self.read_available(ctx)?;
        if read == 0 && self.peer_close.seen {
            // This read is what found the peer gone, so it answers `closed` itself.
            self.peer_close.recv_reported = true;
            return Ok(ctl_xerror("closed"));
        }
        Ok(ctl_ok())
    }

    /// The `shutdown` verb: shuts one or both directions of the connection down.
    ///
    /// A `SHUT_WR`/`SHUT_RDWR` with output still queued is **deferred** until the queue drains
    /// (the C's `TCP_ADDF_PENDING_SHUT_WR`/`TCP_ADDF_PENDING_SHUT_RDWR`), so no queued byte is
    /// silently dropped.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for an argument that is not a single `how` byte.
    fn verb_shutdown(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        let [how] = arg else {
            return Err(error::badarg());
        };
        let fd = self.connected_socket()?;
        if *how != TCP_SHUT_RD && !ctx.output().is_empty() {
            if *how == TCP_SHUT_WR || *how == TCP_SHUT_RDWR {
                self.pending_shutdown = Some(*how);
                return Ok(ctl_ok());
            }
        }
        match shutdown_socket(fd, *how) {
            Ok(()) => Ok(ctl_ok()),
            Err(err) => Ok(ctl_error(err)),
        }
    }

    /// The `close` verb: de-registers and closes the socket, leaving the port open but idle.
    ///
    /// [`crate::PortControl::CLOSE`] reserves this name at the [`crate::PortRegistry`] level,
    /// so this verb is reached only by a caller driving the port's driver directly.
    ///
    /// # Errors
    ///
    /// `BeamError::Io` when the poller rejects the de-registration.
    fn verb_close(&mut self, ctx: &mut DriverContext<'_>) -> Result<Vec<u8>> {
        self.release_socket(ctx)?;
        Ok(ctl_ok())
    }
}

impl Default for TcpInetDriver {
    fn default() -> TcpInetDriver {
        TcpInetDriver::new()
    }
}

impl Driver for TcpInetDriver {
    fn name(&self) -> &str {
        DRIVER_NAME
    }

    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // A socket-less port (the `"tcp_inet"` factory) registers nothing here: its socket is
        // created by the `open` verb, exactly as the C's `tcp_inet_start` leaves the socket
        // to `INET_REQ_OPEN`.
        if let Some(fd) = self.socket {
            set_nonblocking(fd)?;
            self.token = Some(ctx.register(fd, Interest::READABLE)?);
            self.interest = Interest::READABLE;
        }
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.write_pending(ctx)
    }

    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // A listening socket becomes readable when a connection is pending; accepting is
        // command-driven (`accept`), so there is nothing to read here.
        self.read_available(ctx).map(|_read| ())
    }

    fn ready_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        if self.state == InetState::Connecting {
            self.finish_connect(ctx)?;
            return Ok(0);
        }
        self.write_pending(ctx)
    }

    fn control(
        &mut self,
        ctx: &mut DriverContext<'_>,
        command: &str,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        match command {
            "open" => self.verb_open(ctx, arg),
            "bind" => self.verb_bind(arg),
            "listen" => self.verb_listen(arg),
            "accept" => self.verb_accept(ctx),
            "connect" => self.verb_connect(ctx, arg),
            "send" => self.verb_send(ctx, arg),
            "recv" => self.verb_recv(ctx, arg),
            "shutdown" => self.verb_shutdown(ctx, arg),
            "close" => self.verb_close(ctx),
            _ => Err(error::badarg()),
        }
    }

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.release_socket(ctx)
    }
}

impl Drop for TcpInetDriver {
    fn drop(&mut self) {
        // `stop` already released the socket on the normal teardown path; this covers a driver
        // dropped without ever being stopped (a `start` that failed inside
        // `PortRegistry::open`, for instance). A `Drop` cannot report anything, so the
        // descriptor is simply released — the same rule `FdDriver` follows.
        self.close_socket();
    }
}

/// The [`crate::DriverFactory`] of the built-in `"tcp_inet"` driver.
///
/// A plain `fn` (not a closure) because [`crate::DriverFactory`] is a function pointer: it
/// captures nothing and builds a fresh socket-less [`TcpInetDriver`] each call.
pub fn make_tcp_inet() -> Result<Box<dyn Driver>> {
    Ok(Box::new(TcpInetDriver::new()))
}

#[cfg(test)]
mod tests;
