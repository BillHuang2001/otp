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
//! `read(2) == 0` is end-of-file: EOF is latched ([`TcpInetDriver::eof`]) and no further reads
//! are attempted. Writes use `send(2)` with `MSG_NOSIGNAL`, so a peer that went away is a typed
//! [`beam_common::BeamError::Io`] rather than a process-killing `SIGPIPE`.
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

/// The driver's registered name — the C `tcp_inet` entry (`inet_drv.c:1348`), which is also
/// what the oracle reports from `port_info(Port, name)` for a TCP port.
pub const DRIVER_NAME: &str = "tcp_inet";

/// The read buffer size of a single [`Driver::ready_input`] syscall.
const READ_CHUNK: usize = 64 * 1024;

/// The result tag of a successful control operation (`INET_REP_OK`, `inet_drv.c:870`).
pub const INET_REP_OK: u8 = 1;

/// The result tag of a failed control operation (`INET_REP_ERROR`, `inet_drv.c:869`).
pub const INET_REP_ERROR: u8 = 0;

/// The address tag of an explicit IPv4 address (`INET_AF_INET`, `inet_drv.c:757`).
pub const INET_AF_INET: u8 = 1;

/// The address tag of `INADDR_ANY` (`INET_AF_ANY`, `inet_drv.c:759`).
pub const INET_AF_ANY: u8 = 3;

/// The address tag of `INADDR_LOOPBACK` (`INET_AF_LOOPBACK`, `inet_drv.c:760`).
pub const INET_AF_LOOPBACK: u8 = 4;

/// The stream socket type (`INET_TYPE_STREAM`, `inet_drv.c:766`).
pub const INET_TYPE_STREAM: u8 = 1;

/// The default TCP protocol (`INET_PROTO_DEFAULT`, `inet_drv.c:771`).
pub const INET_PROTO_DEFAULT: u8 = 0;

/// The explicit TCP protocol (`INET_PROTO_TCP`, `inet_drv.c:772`).
pub const INET_PROTO_TCP: u8 = 1;

/// `shutdown(sock, SHUT_RD)` (`TCP_SHUT_RD`, `inet_drv.c:684`).
pub const TCP_SHUT_RD: u8 = 0;

/// `shutdown(sock, SHUT_WR)` (`TCP_SHUT_WR`, `inet_drv.c:683`).
pub const TCP_SHUT_WR: u8 = 1;

/// `shutdown(sock, SHUT_RDWR)` (`TCP_SHUT_RDWR`, `inet_drv.c:685`).
pub const TCP_SHUT_RDWR: u8 = 2;

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

/// A decoded IPv4 endpoint from the packed address of the `bind`/`connect` verbs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct InetAddr {
    /// The port, in host byte order.
    port: u16,
    /// The four IPv4 address octets, in network (= textual) order.
    addr: [u8; 4],
}

impl InetAddr {
    /// The `sockaddr_in` this endpoint names.
    fn sockaddr_in(self) -> libc::sockaddr_in {
        // SAFETY: an all-zero `sockaddr_in` is a valid value of that type (family 0, port 0,
        // address 0, zeroed padding); every field this driver cares about is assigned below.
        let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sa.sin_family = libc::AF_INET as libc::sa_family_t;
        sa.sin_port = self.port.to_be();
        sa.sin_addr = libc::in_addr {
            s_addr: u32::from_be_bytes(self.addr).to_be(),
        };
        sa
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
                self.connect_error = None;
                Ok(())
            }
            Err(err) => {
                close_fd(fd);
                Err(err)
            }
        }
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
    /// not `read(2)`) or end-of-file has already been seen.
    ///
    /// # Errors
    ///
    /// Returns `BeamError::Io` when the read fails irrecoverably.
    fn read_available(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if !self.state.is_connected() || self.read_eof {
            return Ok(());
        }
        let fd = self.connected_socket()?;
        let mut buf = [0u8; READ_CHUNK];
        loop {
            // SAFETY: `fd` is an open, non-blocking socket owned by this driver and `buf` is
            // a writable array of `READ_CHUNK` bytes, so `read(2)` writes at most `READ_CHUNK`
            // bytes into it.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), READ_CHUNK) };
            if n > 0 {
                ctx.input().push(&buf[..n as usize]);
                // Edge-triggered readiness: keep reading until the socket is empty.
                continue;
            }
            if n == 0 {
                // The peer closed its end: latch EOF so no further reads are attempted.
                self.read_eof = true;
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return Ok(()),
                _ => return Err(error::io(err)),
            }
        }
    }

    /// Drains the port's output queue to the socket, returning the number of bytes written.
    ///
    /// A short write (`EAGAIN`) leaves the remainder queued and arms write readiness; a fully
    /// drained queue drops write readiness again (keeping readability armed) and applies a
    /// `shutdown` that was deferred because the queue was not empty (the C's
    /// `TCP_ADDF_PENDING_SHUT_*`).
    ///
    /// # Errors
    ///
    /// Returns `BeamError::BadArg` when the socket is not connected, or
    /// `BeamError::Io` when the write fails irrecoverably.
    fn write_pending(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
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
            self.state = InetState::Connected;
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
        self.state = InetState::Connected;
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
            self.state = InetState::Connected;
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
                self.state = InetState::Connected;
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
    /// # Errors
    ///
    /// `BeamError::BadArg` when the socket is not connected, or the write failure
    /// [`TcpInetDriver::write_pending`] reports.
    fn verb_send(&mut self, ctx: &mut DriverContext<'_>, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        ctx.output().push(arg);
        let _written = self.write_pending(ctx)?;
        Ok(ctl_ok())
    }

    /// The `recv` verb: reads everything available into the port input queue.
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
        if !arg.is_empty() {
            return Err(error::badarg());
        }
        self.read_available(ctx)?;
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
        self.read_available(ctx)
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

// ---------------------------------------------------------------------------
// The C reply shapes (`ctl_reply`, `inet_drv.c:2260`)
// ---------------------------------------------------------------------------

/// The success reply `[INET_REP_OK]` (the C's `ctl_reply_ok`, `inet_drv.c:2280`).
fn ctl_ok() -> Vec<u8> {
    vec![INET_REP_OK]
}

/// The reply `[INET_REP_ERROR] ++ name` (the C's `ctl_xerror`, `inet_drv.c:2307`).
fn ctl_xerror(name: &str) -> Vec<u8> {
    let mut reply = Vec::with_capacity(1 + name.len());
    reply.push(INET_REP_ERROR);
    reply.extend_from_slice(name.as_bytes());
    reply
}

/// The reply `[INET_REP_ERROR] ++ errno_name(err)` (the C's `ctl_error`, `inet_drv.c:2300`).
fn ctl_error(err: std::io::Error) -> Vec<u8> {
    ctl_xerror(&errno_name(err.raw_os_error().unwrap_or(0)))
}

/// The lowercase name the oracle reports for `errno` (`erl_errno_id`/`errno_str`,
/// `inet_drv.c:2249`).
///
/// The C asks the libc for the name (`strerrorname_np`/`strerror`) and lowercases it; this
/// floor keeps a table of the codes a TCP socket can actually raise and renders anything else
/// as `errno_<N>`, which is exactly the C's fallback format
/// (`ERTS_UNKNOWN_ERRNO_FORMAT`).
fn errno_name(errno: i32) -> String {
    let known = match errno {
        libc::EACCES => Some("eacces"),
        libc::EADDRINUSE => Some("eaddrinuse"),
        libc::EADDRNOTAVAIL => Some("eaddrnotavail"),
        libc::EAFNOSUPPORT => Some("eafnosupport"),
        libc::EAGAIN => Some("eagain"),
        libc::EALREADY => Some("ealready"),
        libc::ECONNABORTED => Some("econnaborted"),
        libc::ECONNREFUSED => Some("econnrefused"),
        libc::ECONNRESET => Some("econnreset"),
        libc::EDESTADDRREQ => Some("edestaddrreq"),
        libc::EHOSTUNREACH => Some("ehostunreach"),
        libc::EINPROGRESS => Some("einprogress"),
        libc::EINTR => Some("eintr"),
        libc::EINVAL => Some("einval"),
        libc::EIO => Some("eio"),
        libc::EISCONN => Some("eisconn"),
        libc::EMSGSIZE => Some("emsgsize"),
        libc::ENETDOWN => Some("enetdown"),
        libc::ENETUNREACH => Some("enetunreach"),
        libc::ENOBUFS => Some("enobufs"),
        libc::ENOMEM => Some("enomem"),
        libc::ENOPROTOOPT => Some("enoprotoopt"),
        libc::ENOTCONN => Some("enotconn"),
        libc::ENOTSOCK => Some("enotsock"),
        libc::EOPNOTSUPP => Some("eopnotsupp"),
        libc::EPIPE => Some("epipe"),
        libc::EPROTONOSUPPORT => Some("eprotonosupport"),
        libc::EPROTOTYPE => Some("eprototype"),
        libc::ETIMEDOUT => Some("etimedout"),
        _ => None,
    };
    match known {
        Some(name) => name.to_string(),
        None => format!("errno_{errno}"),
    }
}

// ---------------------------------------------------------------------------
// Argument decoding
// ---------------------------------------------------------------------------

/// Decodes the C's packed address (`inet_set_faddress`/`inet_set_address`, `inet_drv.c:4795`)
/// into an IPv4 endpoint.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] for a truncated argument or a family tag this floor does
/// not serve (IPv6 is `INET_AF_INET6` and Unix-domain sockets `INET_AF_LOCAL`; the C answers
/// `eafnosupport` for an unknown tag and this floor treats the whole argument as faulty).
fn parse_address(arg: &[u8]) -> Result<InetAddr> {
    let (&tag, rest) = arg.split_first().ok_or_else(error::badarg)?;
    match tag {
        INET_AF_INET => {
            let &[hi, lo, a, b, c, d] = rest else {
                return Err(error::badarg());
            };
            Ok(InetAddr {
                port: u16::from_be_bytes([hi, lo]),
                addr: [a, b, c, d],
            })
        }
        INET_AF_ANY | INET_AF_LOOPBACK => {
            let &[hi, lo] = rest else {
                return Err(error::badarg());
            };
            Ok(InetAddr {
                port: u16::from_be_bytes([hi, lo]),
                addr: if tag == INET_AF_ANY {
                    [0, 0, 0, 0]
                } else {
                    [127, 0, 0, 1]
                },
            })
        }
        _ => Err(error::badarg()),
    }
}

// ---------------------------------------------------------------------------
// Raw socket syscalls
// ---------------------------------------------------------------------------

/// Creates a non-blocking IPv4 TCP socket.
///
/// # Errors
///
/// `BeamError::Io` when `socket(2)` or the non-blocking `fcntl(2)` fails.
fn socket_fd() -> Result<RawFd> {
    // SAFETY: `socket(2)` takes three integers and needs no pointer arguments.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    match set_nonblocking(fd) {
        Ok(()) => Ok(fd),
        Err(err) => {
            close_fd(fd);
            Err(err)
        }
    }
}

/// Accepts one pending connection on `fd`.
///
/// The accepted descriptor is **not** put in non-blocking mode here (Linux does not inherit
/// `O_NONBLOCK`); [`TcpInetDriver::adopt`] does that.
///
/// # Errors
///
/// The raw `accept(2)` failure, including `EAGAIN` when nothing is pending.
fn accept_socket(fd: RawFd) -> std::io::Result<RawFd> {
    // SAFETY: an all-zero `sockaddr_storage` is a valid value of that type and is exactly the
    // out-parameter `accept(2)` expects; `len` starts as its size and the kernel only ever
    // shrinks it.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `fd` is an open socket owned by this driver, and `addr`/`len` are a valid
    // address buffer and its length, so the kernel writes at most `len` bytes into `addr`.
    let accepted = unsafe { libc::accept(fd, std::ptr::addr_of_mut!(addr).cast(), &mut len) };
    if accepted < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(accepted)
    }
}

/// Whether `fd` has a peer, i.e. is a connected socket (`getpeername(2)`).
///
/// This is the C's `sock_peer` check of `inet_ctl_fdopen` (`inet_drv.c:5254`): a stream
/// socket that answers `getpeername(2)` is a connection, so an adopted descriptor can start
/// out [`InetState::Connected`] instead of merely open.
///
/// # Errors
///
/// `BeamError::Io` when `getpeername(2)` fails for a reason other than "not connected"
/// (`ENOTCONN`), which is the ordinary "no peer yet" answer.
fn has_peer(fd: RawFd) -> Result<bool> {
    // SAFETY: an all-zero `sockaddr_storage` is a valid value of that type and is exactly the
    // out-parameter `getpeername(2)` expects; `len` starts as its size and the kernel only
    // ever shrinks it.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `fd` is an open socket owned by this driver, and `addr`/`len` are a valid
    // address buffer and its length, so the kernel writes at most `len` bytes into `addr`.
    if unsafe { libc::getpeername(fd, std::ptr::addr_of_mut!(addr).cast(), &mut len) } == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        // Not connected (and `ENOTCONN`'s `EINVAL` spelling for a listening socket): no peer.
        Some(libc::ENOTCONN) | Some(libc::EINVAL) => Ok(false),
        _ => Err(error::io(err)),
    }
}

/// The local port `fd` is bound to (`getsockname(2)`).
///
/// # Errors
///
/// `BeamError::Io` when `getsockname(2)` fails.
fn local_port(fd: RawFd) -> Result<u16> {
    // SAFETY: an all-zero `sockaddr_storage` is a valid value of that type and is exactly the
    // out-parameter `getsockname(2)` expects; `len` starts as its size and may be shrunk.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `fd` is an open socket owned by this driver, and `addr`/`len` are a valid
    // address buffer and its length, so the kernel writes at most `len` bytes into `addr`.
    if unsafe { libc::getsockname(fd, std::ptr::addr_of_mut!(addr).cast(), &mut len) } != 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    // The socket was created AF_INET, so the buffer holds a `sockaddr_in`.
    let sin: libc::sockaddr_in = unsafe { std::ptr::read(addr_ptr_as_sockaddr_in(&addr)) };
    Ok(u16::from_be(sin.sin_port))
}

/// Whether `fd` is a listening socket (`SO_ACCEPTCONN`).
///
/// # Errors
///
/// `BeamError::Io` when `fd` is not a usable socket.
fn is_listening(fd: RawFd) -> Result<bool> {
    // SAFETY: `option` is a writable `c_int` and `len` its valid size, which is exactly the
    // out-parameter pair `getsockopt(2)` expects for a `c_int` option.
    let mut option: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` is the caller's socket, and `option`/`len` are a valid buffer and size for
    // a `c_int`-valued socket option.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ACCEPTCONN,
            std::ptr::addr_of_mut!(option).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    Ok(option != 0)
}

/// Takes the pending socket error of `fd` (`getsockopt(SO_ERROR)`), which also clears it.
///
/// # Errors
///
/// `BeamError::Io` when `getsockopt(2)` fails.
fn socket_error(fd: RawFd) -> Result<i32> {
    // SAFETY: `err` is a writable `c_int` and `len` its valid size, which is exactly the
    // out-parameter pair `getsockopt(2)` expects for `SO_ERROR`.
    let mut err: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` is an open socket owned by this driver, and `err`/`len` are a valid buffer
    // and size for `SO_ERROR`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            std::ptr::addr_of_mut!(err).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    Ok(err)
}

/// Shuts `fd` down in the direction `how` names (`SHUT_*`).
///
/// # Errors
///
/// The raw `shutdown(2)` failure (an out-of-range `how` is the kernel's `EINVAL`).
fn shutdown_socket(fd: RawFd, how: u8) -> std::io::Result<()> {
    // SAFETY: `fd` is an open socket owned by this driver and `shutdown(2)` takes no pointer
    // arguments.
    if unsafe { libc::shutdown(fd, how.into()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Closes `fd`, discarding the outcome.
///
/// There is no recovery from a failed `close(2)` and the only alternative — leaking the
/// descriptor — is worse, so the result is dropped deliberately. The caller `take()`s the
/// field it closes, so no descriptor is closed twice.
fn close_fd(fd: RawFd) {
    // SAFETY: `fd` is a socket this driver created (`socket(2)`/`accept(2)`) or adopted
    // (`from_socket`), and it is closed exactly once by the ownership rules above.
    let _ = unsafe { libc::close(fd) };
}

/// Views the start of `addr` as the `sockaddr_in` it holds.
///
/// A `sockaddr_storage` is at least as large and at least as aligned as a `sockaddr_in`, and
/// the socket is AF_INET, so the kernel filled in exactly that structure.
fn addr_ptr_as_sockaddr_in(addr: &libc::sockaddr_storage) -> *const libc::sockaddr_in {
    std::ptr::from_ref(addr).cast()
}

#[cfg(test)]
mod tests {
    use super::{
        ctl_error, ctl_ok, ctl_xerror, errno_name, make_tcp_inet, parse_address, socket_fd,
        InetAddr, InetState, TcpInetDriver, DRIVER_NAME, INET_AF_ANY, INET_AF_INET,
        INET_AF_LOOPBACK, INET_PROTO_DEFAULT, INET_REP_OK, INET_TYPE_STREAM, TCP_SHUT_RDWR,
        TCP_SHUT_WR,
    };
    use crate::driver::{Driver, DriverFlags};
    use crate::poller::Poller;
    use crate::port::{PortRegistry, PortState};
    use crate::port_task::{PortTaskKind, Readiness};
    use beam_common::BeamError;
    use beam_process::PortId;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::unix::io::{AsRawFd, RawFd};
    use std::time::{Duration, Instant};

    /// How long a helper waits for the kernel or the peer before failing the test.
    const WAIT: Duration = Duration::from_secs(5);

    fn poller() -> Poller {
        Poller::new().expect("a poll set")
    }

    /// A loopback listener the test owns (the driver is given its own copy of the fd).
    fn loopback_listener() -> TcpListener {
        TcpListener::bind("127.0.0.1:0").expect("a loopback listener")
    }

    /// The packed `open` argument: `[AF_INET, SOCK_STREAM, default protocol]`.
    fn open_arg() -> [u8; 3] {
        [INET_AF_INET, INET_TYPE_STREAM, INET_PROTO_DEFAULT]
    }

    /// The packed `bind`/`connect` argument naming `127.0.0.1:port`.
    fn loopback_arg(port: u16) -> Vec<u8> {
        let [hi, lo] = port.to_be_bytes();
        vec![INET_AF_LOOPBACK, hi, lo]
    }

    /// A copy of `fd` (`dup(2)`), so the driver and the test each own a descriptor.
    fn duplicate(fd: RawFd) -> RawFd {
        // SAFETY: `fd` is an open descriptor owned by the caller and `dup(2)` takes no pointer
        // arguments; it hands back a second descriptor for the same open file description.
        let dup = unsafe { libc::dup(fd) };
        assert!(dup >= 0, "dup(2) succeeded");
        dup
    }

    /// Opens a port over `driver`.
    fn open_port(poller: &mut Poller, driver: Box<dyn Driver>) -> (PortRegistry, PortId) {
        let mut registry = PortRegistry::new();
        let id = registry.open(poller, driver).expect("the port opens");
        (registry, id)
    }

    /// Runs one control verb on port `id`, panicking on the Rust error kind.
    fn control(
        registry: &mut PortRegistry,
        poller: &mut Poller,
        id: PortId,
        command: &str,
        arg: &[u8],
    ) -> Vec<u8> {
        registry
            .get_mut(id)
            .expect("the port is live")
            .control(poller, command, arg)
            .unwrap_or_else(|err| panic!("the `{command}` verb failed: {err:?}"))
    }

    /// The `open`/`bind`/`listen` sequence over a loopback ephemeral port; returns the port
    /// the kernel assigned.
    fn listen_on_loopback(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> u16 {
        assert_eq!(control(registry, poller, id, "open", &open_arg()), ctl_ok());
        let bound = control(registry, poller, id, "bind", &loopback_arg(0));
        assert_eq!(bound[0], INET_REP_OK);
        assert_eq!(bound.len(), 3, "`bind` replies [1, port_hi, port_lo]");
        let port = u16::from_be_bytes([bound[1], bound[2]]);
        assert_ne!(port, 0, "the kernel assigned a port");
        assert_eq!(control(registry, poller, id, "listen", &[0, 5]), ctl_ok());
        port
    }

    /// Accepts the connection queued for the listening port, retrying until it arrives;
    /// returns the descriptor the reply carried.
    fn accept(poller: &mut Poller, registry: &mut PortRegistry, id: PortId) -> RawFd {
        let deadline = Instant::now() + WAIT;
        loop {
            let reply = control(registry, poller, id, "accept", b"");
            assert_eq!(reply[0], INET_REP_OK);
            if reply.len() == 5 {
                return i32::from_be_bytes([reply[1], reply[2], reply[3], reply[4]]);
            }
            assert_eq!(reply.len(), 1, "an `accept` reply is [1] or [1] ++ fd_be32");
            assert!(Instant::now() < deadline, "no connection was accepted");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Drives the reactor until port `id`'s input queue holds at least `want` bytes,
    /// returning everything it read.
    fn read_into_queue(
        poller: &mut Poller,
        registry: &mut PortRegistry,
        id: PortId,
        want: usize,
    ) -> Vec<u8> {
        let deadline = Instant::now() + WAIT;
        let mut got = Vec::new();
        while got.len() < want {
            assert!(Instant::now() < deadline, "no input arrived");
            let events = poller
                .poll(Some(Duration::from_millis(100)))
                .expect("poll");
            registry
                .handle_events(poller, &events)
                .expect("dispatch the ready events");
            got.extend(registry.get_mut(id).expect("live").drain_input());
        }
        got
    }

    // -----------------------------------------------------------------------
    // Names, reply shapes and argument decoding
    // -----------------------------------------------------------------------

    #[test]
    fn the_driver_registers_under_the_oracle_name() {
        assert_eq!(DRIVER_NAME, "tcp_inet");
        let driver = make_tcp_inet().expect("the factory builds a driver");
        assert_eq!(driver.name(), "tcp_inet");
        assert!(driver.flags() == DriverFlags::empty());
    }

    #[test]
    fn the_reply_shape_helpers_match_the_c() {
        assert_eq!(ctl_ok(), vec![INET_REP_OK]);
        assert_eq!(ctl_xerror("exbadport"), b"\x00exbadport".to_vec());
        assert_eq!(
            ctl_error(std::io::Error::from_raw_os_error(libc::ENOTCONN)),
            b"\x00enotconn".to_vec()
        );
        assert_eq!(
            ctl_error(std::io::Error::from_raw_os_error(libc::ECONNREFUSED)),
            b"\x00econnrefused".to_vec()
        );
        assert_eq!(
            ctl_error(std::io::Error::from_raw_os_error(libc::EISCONN)),
            b"\x00eisconn".to_vec()
        );
        assert_eq!(
            ctl_error(std::io::Error::from_raw_os_error(libc::EINVAL)),
            b"\x00einval".to_vec()
        );
        // An unknown code keeps the C's `errno_<N>` fallback format.
        assert_eq!(ctl_xerror(&errno_name(-1)), b"\x00errno_-1".to_vec());
        assert_eq!(errno_name(0), "errno_0");
        assert_eq!(errno_name(libc::EAGAIN), "eagain");
        assert_eq!(errno_name(libc::EINPROGRESS), "einprogress");
    }

    #[test]
    fn the_packed_address_decodes_the_three_served_forms() {
        assert_eq!(
            parse_address(&[INET_AF_INET, 0x1f, 0x90, 10, 0, 0, 7]).expect("an IPv4 address"),
            InetAddr {
                port: 8080,
                addr: [10, 0, 0, 7]
            }
        );
        assert_eq!(
            parse_address(&[INET_AF_ANY, 0, 5]).expect("INADDR_ANY"),
            InetAddr {
                port: 5,
                addr: [0, 0, 0, 0]
            }
        );
        assert_eq!(
            parse_address(&[INET_AF_LOOPBACK, 0xff, 0xff]).expect("INADDR_LOOPBACK"),
            InetAddr {
                port: 0xffff,
                addr: [127, 0, 0, 1]
            }
        );
    }

    #[test]
    fn a_malformed_or_unsupported_address_is_bad_arg() {
        assert!(parse_address(&[]).is_err(), "an empty argument");
        assert!(
            parse_address(&[INET_AF_INET, 0, 1, 127]).is_err(),
            "truncated"
        );
        assert!(parse_address(&[INET_AF_ANY]).is_err(), "truncated");
        assert!(
            parse_address(&[2, 0, 1, 0, 0, 0, 0, 0, 0]).is_err(),
            "INET_AF_INET6 (out of scope)"
        );
        assert!(
            parse_address(&[5, 2, b'x']).is_err(),
            "INET_AF_LOCAL (out of scope)"
        );
        // The error is the crate's argument fault.
        assert!(matches!(parse_address(&[]), Err(BeamError::BadArg)));
    }

    #[test]
    fn the_sockaddr_in_of_an_endpoint_is_network_byte_order() {
        let sa = InetAddr {
            port: 0x1234,
            addr: [192, 168, 1, 2],
        }
        .sockaddr_in();
        assert_eq!(sa.sin_family as i32, libc::AF_INET);
        assert_eq!(sa.sin_port, 0x1234u16.to_be());
        assert_eq!(sa.sin_addr.s_addr, u32::from_be_bytes([192, 168, 1, 2]).to_be());
    }

    // -----------------------------------------------------------------------
    // Lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn a_fresh_driver_has_no_socket() {
        let driver = TcpInetDriver::new();
        assert_eq!(driver.state(), InetState::Closed);
        assert_eq!(driver.socket(), None);
        assert!(!driver.eof());
        assert_eq!(driver.connect_error(), None);
        assert_eq!(TcpInetDriver::default().state(), InetState::Closed);
    }

    #[test]
    fn from_socket_reads_the_state_from_the_kernel() {
        // A listening socket (`SO_ACCEPTCONN`).
        let listener = loopback_listener();
        let driver = TcpInetDriver::from_socket(duplicate(listener.as_raw_fd())).expect("adopts");
        assert_eq!(driver.state(), InetState::Listening);
        drop(driver); // closes only the duplicate, not the test's listener

        // A connected socket (has a peer).
        let address = listener.local_addr().expect("an address");
        let client = TcpStream::connect(address).expect("connects");
        let (server, _peer) = listener.accept().expect("accepts");
        let fd = duplicate(server.as_raw_fd());
        let driver = TcpInetDriver::from_socket(fd).expect("adopts");
        assert_eq!(driver.state(), InetState::Connected);
        assert_eq!(driver.socket(), Some(fd));
        drop(driver);
        let _ = client;

        // An opened but unconnected socket.
        let fd = socket_fd().expect("a fresh socket");
        let driver = TcpInetDriver::from_socket(fd).expect("adopts");
        assert_eq!(driver.state(), InetState::Open);

        // A descriptor that is not a socket is rejected, never dereferenced blindly.
        assert!(TcpInetDriver::from_socket(-1).is_err());
    }

    #[test]
    fn commands_before_open_report_the_c_operational_errors() {
        let mut poller = poller();
        let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));

        // No socket yet: the state errors the C answers with a `ctl_xerror` reply.
        assert_eq!(
            control(&mut registry, &mut poller, id, "bind", &loopback_arg(0)),
            b"\x00exbadport".to_vec()
        );
        assert_eq!(
            control(&mut registry, &mut poller, id, "listen", &[0, 5]),
            b"\x00exbadport".to_vec()
        );
        assert_eq!(
            control(&mut registry, &mut poller, id, "connect", &loopback_arg(1)),
            b"\x00exbadport".to_vec()
        );
        assert_eq!(
            control(&mut registry, &mut poller, id, "recv", b""),
            b"\x00enotconn".to_vec()
        );
        assert_eq!(
            control(&mut registry, &mut poller, id, "send", b"x"),
            b"\x00enotconn".to_vec()
        );
        assert_eq!(
            control(&mut registry, &mut poller, id, "shutdown", &[0]),
            b"\x00enotconn".to_vec()
        );
        assert_eq!(
            control(&mut registry, &mut poller, id, "accept", b""),
            b"\x00einval".to_vec(),
            "`accept` on a non-listener"
        );
        // `close` is unconditional in the C (`ctl_reply_ok`, `inet_drv.c:12259`).
        assert_eq!(control(&mut registry, &mut poller, id, "close", b""), ctl_ok());
        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn unknown_verbs_and_malformed_arguments_are_bad_arg() {
        let mut poller = poller();
        let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
        assert_eq!(registry.get(id).expect("live").driver_name(), "tcp_inet");
        assert_eq!(registry.get(id).expect("live").state(), PortState::Open);

        let mut verb = |command: &str, arg: &[u8]| {
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, command, arg)
        };
        assert!(matches!(verb("frobnicate", b""), Err(BeamError::BadArg)));
        // `open` needs its three-byte record ...
        assert!(matches!(
            verb("open", &[INET_AF_INET, 1]),
            Err(BeamError::BadArg)
        ));
        // ... whose second byte must be SOCK_STREAM ...
        assert!(matches!(
            verb("open", &[INET_AF_INET, 2, INET_PROTO_DEFAULT]),
            Err(BeamError::BadArg)
        ));
        // ... and whose protocol byte must be the default or TCP.
        assert!(matches!(
            verb("open", &[INET_AF_INET, INET_TYPE_STREAM, 9]),
            Err(BeamError::BadArg)
        ));
        // None of those failures created a socket.
        assert_eq!(verb("recv", b"").expect("a reply"), b"\x00enotconn".to_vec());

        // A valid `open` creates the socket ...
        assert_eq!(verb("open", &open_arg()).expect("opens"), ctl_ok());
        // ... after which the argument faults of the remaining verbs surface as `BadArg`
        // (the state guards that answer `exbadport`/`enotconn` come first, as in the C).
        assert!(matches!(verb("listen", &[9]), Err(BeamError::BadArg)));
        assert!(matches!(verb("bind", &[]), Err(BeamError::BadArg)));
        assert!(matches!(verb("connect", b""), Err(BeamError::BadArg)));
        // A second `open` is the operational `einval` (the C would leak the first socket
        // here, so this floor refuses instead).
        assert_eq!(
            verb("open", &open_arg()).expect("a reply"),
            b"\x00einval".to_vec()
        );
        registry.close(&mut poller, id).expect("closes");
    }

    // -----------------------------------------------------------------------
    // The verb round trips (real loopback sockets)
    // -----------------------------------------------------------------------

    #[test]
    fn the_open_bind_listen_accept_send_and_recv_verbs_round_trip() {
        let mut poller = poller();
        let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
        let port = listen_on_loopback(&mut poller, &mut registry, id);

        let mut client = TcpStream::connect(("127.0.0.1", port)).expect("the client connects");
        let accepted_fd = accept(&mut poller, &mut registry, id);
        assert!(accepted_fd >= 0, "the reply carried a real descriptor");

        // Peer -> port: the reactor reads the bytes into the port's input queue.
        client.write_all(b"ping").expect("the client writes");
        assert_eq!(
            read_into_queue(&mut poller, &mut registry, id, 4),
            b"ping",
            "the connection's bytes reach the port input queue"
        );

        // Port -> peer: the `send` verb drains the socket.
        assert_eq!(
            control(&mut registry, &mut poller, id, "send", b"pong"),
            ctl_ok()
        );
        assert_eq!(registry.get(id).expect("live").output_len(), 0);
        let mut reply = [0u8; 4];
        client.read_exact(&mut reply).expect("the client reads");
        assert_eq!(&reply, b"pong");

        // `shutdown` then `close`, and a second `close` is idempotent.
        assert_eq!(
            control(&mut registry, &mut poller, id, "shutdown", &[TCP_SHUT_WR]),
            ctl_ok()
        );
        assert_eq!(control(&mut registry, &mut poller, id, "close", b""), ctl_ok());
        assert_eq!(control(&mut registry, &mut poller, id, "close", b""), ctl_ok());
        registry.close(&mut poller, id).expect("closes the port");
    }

    #[test]
    fn an_accept_with_nothing_pending_answers_ok() {
        let listener = loopback_listener();
        let mut poller = poller();
        let (mut registry, id) = open_port(
            &mut poller,
            Box::new(TcpInetDriver::from_socket(duplicate(listener.as_raw_fd())).expect("adopts")),
        );
        assert_eq!(
            control(&mut registry, &mut poller, id, "accept", b""),
            ctl_ok(),
            "EAGAIN is `ok` with no descriptor (retry when the port reports readability)"
        );
        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn the_recv_and_send_verbs_work_on_an_adopted_connected_socket() {
        let listener = loopback_listener();
        let address = listener.local_addr().expect("an address");
        let mut client = TcpStream::connect(address).expect("connects");
        let (server, _peer) = listener.accept().expect("accepts");

        let mut poller = poller();
        let (mut registry, id) = open_port(
            &mut poller,
            Box::new(TcpInetDriver::from_socket(duplicate(server.as_raw_fd())).expect("adopts")),
        );

        // `recv` drives the read path directly (no poller needed): retry until the bytes the
        // client sent have arrived.
        client.write_all(b"hello").expect("the client writes");
        let deadline = Instant::now() + WAIT;
        let mut got = Vec::new();
        while got.len() < 5 {
            assert!(Instant::now() < deadline, "`recv` read nothing");
            assert_eq!(control(&mut registry, &mut poller, id, "recv", b""), ctl_ok());
            got.extend(registry.get_mut(id).expect("live").drain_input());
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got, b"hello");

        // ... and `send` writes back to the peer.
        assert_eq!(
            control(&mut registry, &mut poller, id, "send", b"world"),
            ctl_ok()
        );
        let mut reply = [0u8; 5];
        client.read_exact(&mut reply).expect("the client reads");
        assert_eq!(&reply, b"world");

        // `recv` with an argument is a `BadArg`: the C's timeout/length are not honored, so
        // accepting them would silently ignore them.
        assert!(matches!(
            registry
                .get_mut(id)
                .expect("live")
                .control(&mut poller, "recv", &[0, 0, 0, 0]),
            Err(BeamError::BadArg)
        ));
        registry.close(&mut poller, id).expect("closes");
    }

    // -----------------------------------------------------------------------
    // Flow control: readiness arming and short writes
    // -----------------------------------------------------------------------

    #[test]
    fn a_write_ready_event_is_ignored_until_write_readiness_is_armed() {
        let mut poller = poller();
        let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
        listen_on_loopback(&mut poller, &mut registry, id);

        // A listening socket arms only readability, so a write-ready event schedules nothing
        // (`PortArms`), while a read-ready one does.
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                writable: true,
                ..Readiness::default()
            }
        ));
        assert!(!registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output));
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                readable: true,
                ..Readiness::default()
            }
        ));
        assert!(registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Input));
        registry.close(&mut poller, id).expect("closes");
    }

    #[test]
    fn a_stalled_write_arms_write_readiness_and_defers_a_shutdown() {
        let mut poller = poller();
        let (mut registry, id) = open_port(&mut poller, make_tcp_inet().expect("a driver"));
        let port = listen_on_loopback(&mut poller, &mut registry, id);
        // The peer connects but never reads, so the kernel's buffers fill and stay full.
        let _client = TcpStream::connect(("127.0.0.1", port)).expect("connects");
        accept(&mut poller, &mut registry, id);

        // A small write is taken by the kernel straight away.
        assert_eq!(
            control(&mut registry, &mut poller, id, "send", b"tiny"),
            ctl_ok()
        );
        assert_eq!(registry.get(id).expect("live").output_len(), 0);

        // Keep offering bytes until the kernel stops taking them: the remainder stays in the
        // port output queue and write readiness is armed.
        let chunk = vec![b'x'; 256 * 1024];
        let deadline = Instant::now() + WAIT;
        while registry.get(id).expect("live").output_len() == 0 {
            assert!(Instant::now() < deadline, "the kernel accepted every byte");
            assert_eq!(
                control(&mut registry, &mut poller, id, "send", &chunk),
                ctl_ok()
            );
        }

        // The evidence that write readiness is armed: a write-ready event now schedules an
        // OUTPUT task (which the port-task round would run when the kernel drains).
        assert!(registry.schedule_readiness(
            id.number(),
            Readiness {
                writable: true,
                ..Readiness::default()
            }
        ));
        assert!(registry
            .get(id)
            .expect("live")
            .has_pending_task(PortTaskKind::Output));

        // A `SHUT_WR`/`SHUT_RDWR` with output still queued is deferred, never dropped.
        assert_eq!(
            control(&mut registry, &mut poller, id, "shutdown", &[TCP_SHUT_RDWR]),
            ctl_ok()
        );
        assert_eq!(
            control(&mut registry, &mut poller, id, "shutdown", &[TCP_SHUT_WR]),
            ctl_ok()
        );
        assert!(registry.get(id).expect("live").output_len() > 0);
        registry.close(&mut poller, id).expect("closes");
    }
}
