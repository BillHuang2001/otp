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

//! The `tcp_inet` driver's **raw socket syscalls**.
//!
//! Every `unsafe` block of the driver lives here: the thin `libc` wrappers the control
//! verbs are built on — `socket(2)`/`accept(2)`/`bind(2)`/`listen(2)`/`connect(2)`/
//! `getsockname(2)`/`getsockopt(2)`/`setsockopt(2)`/`shutdown(2)`/`close(2)` — plus the two
//! kernel state probes the adopted-descriptor path needs ([`is_listening`] over
//! `SO_ACCEPTCONN` and [`has_peer`] over `getpeername(2)`).
//!
//! Each wrapper carries its own `// SAFETY:` rationale; the driver's *ownership* rules
//! (which descriptor is closed when, and by whom) stay with the driver in [`super`]. The
//! `bind`/`connect` address marshalling lives in [`super::wire`], and the `read(2)`/
//! `send(2)` byte movement stays in [`super`] because it drives the port's byte queues.
//!
//! This is a **private** detail of the [`super`] module: nothing here is re-exported at
//! `crate::drivers::inet::*`, so this split is invisible to callers.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`.

use std::os::unix::io::RawFd;

use beam_common::Result;

use super::wire::InetAddr;
use crate::drivers::fd::set_nonblocking;
use crate::error;

/// Creates a non-blocking IPv4 TCP socket.
///
/// # Errors
///
/// `BeamError::Io` when `socket(2)` or the non-blocking `fcntl(2)` fails.
pub(super) fn socket_fd() -> Result<RawFd> {
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
/// `O_NONBLOCK`); the driver's `adopt` does that.
///
/// # Errors
///
/// The raw `accept(2)` failure, including `EAGAIN` when nothing is pending.
pub(super) fn accept_socket(fd: RawFd) -> std::io::Result<RawFd> {
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
/// out connected instead of merely open.
///
/// # Errors
///
/// `BeamError::Io` when `getpeername(2)` fails for a reason other than "not connected"
/// (`ENOTCONN`), which is the ordinary "no peer yet" answer.
pub(super) fn has_peer(fd: RawFd) -> Result<bool> {
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
pub(super) fn local_port(fd: RawFd) -> Result<u16> {
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
    // SAFETY: the socket is AF_INET, so the kernel filled in a `sockaddr_in` at the start of
    // the `sockaddr_storage`, which is at least as large and at least as aligned as it.
    let sin: libc::sockaddr_in = unsafe { std::ptr::read(addr_ptr_as_sockaddr_in(&addr)) };
    Ok(u16::from_be(sin.sin_port))
}

/// The local endpoint of `fd` (`getsockname(2)`), the C's `sockname`
/// (`inet_drv.c:4890`, `inet_get_address`'s encoder of `INET_REQ_NAME`).
///
/// # Errors
///
/// The raw `getsockname(2)` failure (`EBADF` for a descriptor that is not a socket) and the typed
/// "not an IPv4 endpoint" refusal for a family this floor does not encode — a wrong family tag
/// would be a wrong address, so it is never guessed.
pub(super) fn local_address(fd: RawFd) -> std::io::Result<InetAddr> {
    // SAFETY: an all-zero `sockaddr_storage` is a valid value of that type and is exactly the
    // out-parameter `getsockname(2)` expects; `len` starts as its size and may be shrunk.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `addr`/`len` are a valid address buffer and its length, so the kernel writes at
    // most `len` bytes into `addr`. `fd` is the caller's descriptor; a descriptor that is not a
    // socket is reported by the syscall as `EBADF`/`ENOTSOCK`, not by any promise made here.
    if unsafe { libc::getsockname(fd, std::ptr::addr_of_mut!(addr).cast(), &mut len) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    decode_inet_address(&addr)
}

/// The peer endpoint of `fd` (`getpeername(2)`), the C's `sockpeer`
/// (`inet_drv.c:11280`, the body of `INET_REQ_PEER`).
///
/// # Errors
///
/// The raw `getpeername(2)` failure (`ENOTCONN` for an unconnected socket) and the typed "not an
/// IPv4 endpoint" refusal, as in [`local_address`].
pub(super) fn peer_address(fd: RawFd) -> std::io::Result<InetAddr> {
    // SAFETY: an all-zero `sockaddr_storage` is a valid value of that type and is exactly the
    // out-parameter `getpeername(2)` expects; `len` starts as its size and may be shrunk.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `addr`/`len` are a valid address buffer and its length, so the kernel writes at
    // most `len` bytes into `addr`. `fd` is the caller's descriptor; an unconnected peer is
    // reported by the syscall as `ENOTCONN`, not by any promise made here.
    if unsafe { libc::getpeername(fd, std::ptr::addr_of_mut!(addr).cast(), &mut len) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    decode_inet_address(&addr)
}

/// Decodes the `sockaddr` the kernel filled into `addr` as an IPv4 endpoint.
///
/// # Errors
///
/// [`std::io::ErrorKind::Unsupported`] for a family other than `AF_INET`: the drivers are
/// `AF_INET`-only, so a different family means the descriptor is not one this floor can describe,
/// and inventing a packed `INET_AF_INET` address for it would be a wrong value.
fn decode_inet_address(addr: &libc::sockaddr_storage) -> std::io::Result<InetAddr> {
    // SAFETY: `addr` is a fully initialized `sockaddr_storage`, whose first field is the
    // `sa_family_t` every `sockaddr` variant starts with, so reading it is well defined for any
    // family the kernel wrote.
    let family = unsafe { (*std::ptr::from_ref(addr).cast::<libc::sockaddr>()).sa_family };
    if family != libc::AF_INET as libc::sa_family_t {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the endpoint is not an AF_INET address",
        ));
    }
    // SAFETY: the family check above proves the kernel filled in a `sockaddr_in` at the start of
    // the `sockaddr_storage`, which is at least as large and at least as aligned as it.
    let sin: libc::sockaddr_in = unsafe { std::ptr::read(addr_ptr_as_sockaddr_in(addr)) };
    Ok(InetAddr {
        port: u16::from_be(sin.sin_port),
        addr: sin.sin_addr.s_addr.to_ne_bytes(),
    })
}
/// Whether `fd` is a listening socket (`SO_ACCEPTCONN`).
///
/// # Errors
///
/// `BeamError::Io` when `fd` is not a usable socket.
pub(super) fn is_listening(fd: RawFd) -> Result<bool> {
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
pub(super) fn socket_error(fd: RawFd) -> Result<i32> {
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

/// The cached socket-type tag of `fd`: `1` ([`super::wire::INET_TYPE_STREAM`]) or `2`
/// ([`super::wire::INET_TYPE_DGRAM`]), and `0` for a descriptor whose `SO_TYPE` is neither.
///
/// The C descriptor records the kernel's `SO_TYPE` in `inet.stype` when it opens or adopts a
/// socket (`inet_ctl_open`, `inet_drv.c:5215`; `inet_ctl_fdopen`, `:5265`) and reports it from
/// `INET_REQ_GETTYPE` (`:11165`). `0` is the "unset" value that makes `GETTYPE` answer the C's
/// `einval` rather than a type this floor does not serve.
///
/// # Errors
///
/// `BeamError::Io` when `getsockopt(2)` fails (`fd` is not a socket).
pub(super) fn socket_type(fd: RawFd) -> Result<u16> {
    // SAFETY: `option` is a writable `c_int` and `len` its valid size, which is exactly the
    // out-parameter pair `getsockopt(2)` expects for a `c_int`-valued option.
    let mut option: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` is the caller's socket, and `option`/`len` are a valid buffer and size for
    // the `SO_TYPE` option.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            std::ptr::addr_of_mut!(option).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    let stype = match option {
        libc::SOCK_STREAM => 1,
        libc::SOCK_DGRAM => 2,
        _ => 0,
    };
    Ok(stype)
}

/// The host's name, from `gethostname(2)` — the `INET_REQ_GETHOSTNAME` reply payload
/// (`inet_drv.c:11219`, the C's `sock_hostname`, a **local** syscall with no resolver
/// involved).
///
/// The trailing NUL (and anything the kernel truncated after it) is trimmed, so the bytes are
/// exactly the C's `ctl_reply(INET_REP_OK, tbuf, strlen(tbuf))` payload.
///
/// # Errors
///
/// `BeamError::Io` when `gethostname(2)` fails.
pub(super) fn hostname() -> Result<Vec<u8>> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a writable array of `buf.len()` bytes; the kernel writes at most that
    // many bytes into it (truncating a longer name) and never retains the pointer.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(buf[..end].to_vec())
}

/// Reads an `int`-valued socket option of `fd` (`getsockopt(2)`).
///
/// This is the C's `sock_getopt` for the option family whose value is a plain `int`: the
/// boolean flags (`SO_KEEPALIVE`, `TCP_NODELAY`) and the buffer sizes (`SO_RCVBUF`,
/// `SO_SNDBUF`). The kernel's own value is returned **verbatim**, so a buffer size reports
/// whatever the kernel actually stored — on Linux a doubled request, which is exactly what
/// the oracle's `recbuf`/`sndbuf` report too.
///
/// # Errors
///
/// `BeamError::Io` when `getsockopt(2)` fails (an option the descriptor does not serve, or a
/// descriptor that is no longer a socket).
pub(super) fn get_socket_int_option(
    fd: RawFd,
    level: libc::c_int,
    name: libc::c_int,
) -> Result<i32> {
    // SAFETY: `value` is a writable `c_int` and `len` its exact size, which is the
    // out-parameter pair `getsockopt(2)` expects for an `int`-valued option, so the kernel
    // writes at most `len` bytes into `value`.
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `fd` is a socket owned by this driver, `level`/`name` name a socket option, and
    // `value`/`len` are a valid buffer and its size.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            level,
            name,
            std::ptr::addr_of_mut!(value).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    Ok(value)
}

/// Writes an `int`-valued socket option of `fd` (`setsockopt(2)`), the companion of
/// [`get_socket_int_option`].
///
/// # Errors
///
/// `BeamError::Io` when `setsockopt(2)` fails.
pub(super) fn set_socket_int_option(
    fd: RawFd,
    level: libc::c_int,
    name: libc::c_int,
    value: i32,
) -> Result<()> {
    let value: libc::c_int = value;
    // SAFETY: `value` is a local `c_int` and its size is passed as the option length, so the
    // kernel reads only those initialized bytes and never retains the pointer.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            std::ptr::addr_of!(value).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Shuts `fd` down in the direction `how` names (`SHUT_*`).
///
/// # Errors
///
/// The raw `shutdown(2)` failure (an out-of-range `how` is the kernel's `EINVAL`).
pub(super) fn shutdown_socket(fd: RawFd, how: u8) -> std::io::Result<()> {
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
pub(super) fn close_fd(fd: RawFd) {
    // SAFETY: `fd` is a socket this driver created (`socket(2)`/`accept(2)`) or adopted
    // (`from_socket`), and it is closed exactly once by the ownership rules in `super`.
    let _ = unsafe { libc::close(fd) };
}

/// Views the start of `addr` as the `sockaddr_in` it holds.
///
/// A `sockaddr_storage` is at least as large and at least as aligned as a `sockaddr_in`, and
/// the socket is AF_INET, so the kernel filled in exactly that structure.
fn addr_ptr_as_sockaddr_in(addr: &libc::sockaddr_storage) -> *const libc::sockaddr_in {
    std::ptr::from_ref(addr).cast()
}

// ---------------------------------------------------------------------------
// Datagram (`udp_inet`) syscalls
// ---------------------------------------------------------------------------

/// Creates a non-blocking IPv4 UDP socket (`SOCK_DGRAM`), the `udp_inet` counterpart of
/// [`socket_fd`].
///
/// # Errors
///
/// `BeamError::Io` when `socket(2)` or the non-blocking `fcntl(2)` fails.
pub(super) fn dgram_socket_fd() -> Result<RawFd> {
    // SAFETY: `socket(2)` takes three integers and needs no pointer arguments.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
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

/// Binds `fd` to the endpoint `address` names (`bind(2)`).
///
/// The single `bind(2)` call site, shared by the TCP and UDP `bind` verbs (the C's `sock_bind`).
///
/// # Errors
///
/// The raw `bind(2)` failure.
pub(super) fn bind_socket(fd: RawFd, address: &InetAddr) -> std::io::Result<()> {
    let sa = address.sockaddr_in();
    // SAFETY: `fd` is an open socket owned by this driver and `sa` is a fully initialized
    // `sockaddr_in`, so the kernel reads at most `size_of::<sockaddr_in>()` bytes of it and
    // never retains the pointer.
    let rc = unsafe {
        libc::bind(
            fd,
            std::ptr::addr_of!(sa).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Associates `fd` with the default peer `address` (`connect(2)` on a datagram socket).
///
/// A UDP `connect(2)` is synchronous: it only records a default destination and installs a
/// filter, so a `0` return means "connected" (the C's `sock_connect` on the UDP branch,
/// `inet_drv.c:14802`).
///
/// # Errors
///
/// The raw `connect(2)` failure.
pub(super) fn connect_socket(fd: RawFd, address: &InetAddr) -> std::io::Result<()> {
    let sa = address.sockaddr_in();
    // SAFETY: `fd` is an open socket owned by this driver and `sa` is a fully initialized
    // `sockaddr_in`, so the kernel reads at most `size_of::<sockaddr_in>()` bytes of it and
    // never retains the pointer.
    let rc = unsafe {
        libc::connect(
            fd,
            std::ptr::addr_of!(sa).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Sends one datagram from `fd` to `address` (`sendto(2)`, the C's `sock_sendto`).
///
/// # Errors
///
/// The raw `sendto(2)` failure (`EMSGSIZE` for an over-long datagram).
pub(super) fn send_to(fd: RawFd, buf: &[u8], address: &InetAddr) -> std::io::Result<usize> {
    let sa = address.sockaddr_in();
    // SAFETY: `fd` is an open socket owned by this driver, `buf` points to `buf.len()`
    // initialized bytes the kernel only reads, and `sa` is a fully initialized `sockaddr_in`,
    // so the kernel reads at most `size_of::<sockaddr_in>()` bytes of it and never retains the
    // pointer. `MSG_NOSIGNAL` keeps a vanished peer a return value, not a fatal signal.
    let n = unsafe {
        libc::sendto(
            fd,
            buf.as_ptr().cast(),
            buf.len(),
            libc::MSG_NOSIGNAL,
            std::ptr::addr_of!(sa).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Sends one datagram on the connected socket `fd` (`send(2)`, the C's `sock_send`).
///
/// # Errors
///
/// The raw `send(2)` failure.
pub(super) fn send_connected(fd: RawFd, buf: &[u8]) -> std::io::Result<usize> {
    // SAFETY: `fd` is an open socket owned by this driver and `buf` points to `buf.len()`
    // initialized bytes the kernel only reads and never retains. `MSG_NOSIGNAL` keeps a
    // vanished peer a return value rather than a process-killing `SIGPIPE`.
    let n = unsafe {
        libc::send(
            fd,
            buf.as_ptr().cast(),
            buf.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(n as usize)
}

/// Receives one datagram from `fd` into `buf`, decoding the source endpoint
/// (`recvfrom(2)`, the C's `sock_recvfrom`).
///
/// Returns `(bytes_received, source)`; the source is decoded for an AF_INET peer.
///
/// # Errors
///
/// The raw `recvfrom(2)` failure (`EAGAIN` when nothing is pending).
pub(super) fn recv_from(fd: RawFd, buf: &mut [u8]) -> std::io::Result<(usize, InetAddr)> {
    // SAFETY: an all-zero `sockaddr_storage` is a valid value of that type and is exactly the
    // out-parameter `recvfrom(2)` expects; `len` starts as its size and the kernel may shrink
    // it.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `fd` is an open socket owned by this driver, `buf` is a writable array of
    // `buf.len()` bytes, and `addr`/`len` are a valid address buffer and its length, so the
    // kernel writes at most `buf.len()` bytes into `buf` and at most `len` bytes into `addr`.
    let n = unsafe {
        libc::recvfrom(
            fd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            std::ptr::addr_of_mut!(addr).cast(),
            &mut len,
        )
    };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the socket is AF_INET, so the kernel filled in a `sockaddr_in` at the start of
    // the `sockaddr_storage`, which is at least as large and at least as aligned as it.
    let sin: libc::sockaddr_in = unsafe { std::ptr::read(addr_ptr_as_sockaddr_in(&addr)) };
    let source = InetAddr {
        port: u16::from_be(sin.sin_port),
        addr: sin.sin_addr.s_addr.to_ne_bytes(),
    };
    Ok((n as usize, source))
}

// ---------------------------------------------------------------------------
// The descriptor-family probe, the service database and the interface ioctls
// ---------------------------------------------------------------------------
//
// Three families of raw calls the shared `inet_ctl` arms need, all of which the C driver reaches
// through its `sock_*` macros or through `ioctl(2)`/`getservby*(3)`/`getifaddrs(3)` directly:
//
// * [`socket_family`] — the `getsockname(2)` half of `inet_ctl_fdopen`'s bound check
//   (`inet_drv.c:5252`), which needs the **errno** of a failed call so the reply can render
//   `enotsock`/`ebadf` the way `ctl_error(sock_errno())` does;
// * [`service_port`]/[`service_name`] — `getservbyname(3)`/`getservbyport(3)` behind the C's
//   `sock_getservbyname`/`sock_getservbyport` (`inet_drv.c:11518`, `:11547`);
// * the `ifreq`/`ifconf`/`getifaddrs(3)` interface walk behind `INET_REQ_IFGET`/`IFSET`/
//   `GETIFLIST`/`GETIFADDRS` (`:5671`, `:5821`, `:6017`, `:6754`).
//
// Nothing here decides a *reply*: each wrapper is the raw capability, and the C's arithmetic,
// guard order and record encoding live in [`super::inet_ctl`].

/// The address family of `fd` (`getsockname(2)`), with the syscall's errno preserved.
///
/// This is the probe `inet_ctl_fdopen` makes before it compares the family: the C calls
/// `sock_name(s, …)` first, so a descriptor that is not a socket is `ctl_error(sock_errno())`
/// (`ENOTSOCK`, or `EBADF` for a closed number) and only a *successful* call can be a family
/// mismatch (`ctl_error(EINVAL)`, `inet_drv.c:5255`). Only the family is returned — the
/// pre-bound path needs nothing else, and [`local_address`] already serves the endpoint.
///
/// # Errors
///
/// The raw `getsockname(2)` failure, errno intact.
pub(super) fn socket_family(fd: RawFd) -> std::io::Result<libc::sa_family_t> {
    // SAFETY: an all-zero `sockaddr_storage` is a valid value of that type and is exactly the
    // out-parameter `getsockname(2)` expects; `len` starts as its size and may be shrunk.
    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: `addr`/`len` are a valid address buffer and its length, so the kernel writes at
    // most `len` bytes into `addr`. `fd` is the caller's descriptor; a descriptor that is not a
    // socket is reported by the syscall as `EBADF`/`ENOTSOCK`, not by any promise made here.
    if unsafe { libc::getsockname(fd, std::ptr::addr_of_mut!(addr).cast(), &mut len) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: every `sockaddr` variant starts with the `sa_family_t` the kernel just wrote, and
    // `sockaddr_storage` is at least as large and as aligned as a `sockaddr`.
    Ok(unsafe { (*std::ptr::from_ref(&addr).cast::<libc::sockaddr>()).sa_family })
}
/// The NUL-terminated `CString` a NUL-containing byte slice would truncate to.
///
/// The C copies its operand into a `char[256]` and appends `'\0'` (`inet_drv.c:11507`,
/// `:11513`), so an embedded NUL makes the lookup see a shorter string. Reproducing that
/// truncation keeps the two sides' answers identical for a hostile operand.
///
/// # Errors
///
/// Never: the truncated slice contains no interior NUL, so `CString::new` cannot fail.
fn c_string(bytes: &[u8]) -> std::ffi::CString {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    std::ffi::CString::new(&bytes[..end]).expect("the slice is truncated at the first NUL")
}
/// `getservbyname(3)` — the C's `sock_getservbyname` (`inet_drv.c:11518`).
///
/// Returns the service **port in host byte order** (the C's `sock_ntohs(srv->s_port)`).
///
/// # The returned pointer is a static
///
/// `getservbyname(3)` hands back a pointer into a static buffer that the next lookup (of any
/// service) overwrites, so every value this function needs is copied out before it returns and
/// nothing is freed — the exact discipline the C follows.
pub(super) fn service_port(name: &[u8], proto: &[u8]) -> Option<u16> {
    let name = c_string(name);
    let proto = c_string(proto);
    // SAFETY: both arguments are valid NUL-terminated strings that outlive the call, which is
    // what `getservbyname(3)` requires; the call retains neither pointer.
    let entry = unsafe { libc::getservbyname(name.as_ptr(), proto.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is the non-NULL `servent` the lookup just returned; its `s_port` field is
    // initialized by libc. The value is read (and the port converted) before anything can
    // overwrite the static.
    let port = unsafe { (*entry).s_port };
    Some(u16::from_be(port as u16))
}
/// `getservbyport(3)` — the C's `sock_getservbyport` (`inet_drv.c:11547`).
///
/// `port` is the **host-order** port; the `c_int` handed to libc is `htons`'s result
/// (`i32::from(port.to_be())`), exactly the C's `sock_htons(port)`.
///
/// Returns the service name's raw bytes (no NUL), copied out of the static `servent` before
/// the next lookup can overwrite it.
pub(super) fn service_name(port: u16, proto: &[u8]) -> Option<Vec<u8>> {
    let proto = c_string(proto);
    // SAFETY: `proto` is a valid NUL-terminated string that outlives the call and the port is
    // the network-ordered value `getservbyport(3)` expects; the call retains no pointer.
    let entry = unsafe { libc::getservbyport(i32::from(port.to_be()), proto.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: `entry` is the non-NULL `servent` the lookup just returned and `s_name` is a
    // NUL-terminated string libc owns. It is copied (and its length taken) before anything can
    // overwrite the static, never retained and never freed.
    let name = unsafe { std::ffi::CStr::from_ptr((*entry).s_name) };
    Some(name.to_bytes().to_vec())
}
/// A `struct sockaddr` decoded the way the C's `sockaddr_to_buf` (`inet_drv.c:5373`) sees it.
///
/// `family` is the kernel's `sa_family`; `body` is [`sockaddr_to_buf`]'s encoding of the
/// address, which is **empty** for a family that encoder has no case for (the C returns its
/// output pointer unchanged there, which is how a caller tells "nothing written" from a record).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct SockAddr {
    /// The `sa_family` the kernel reported.
    pub(super) family: libc::c_int,
    /// The `sockaddr_to_buf` encoding: `[INET_AF_INET, a, b, c, d]`, `[INET_AF_INET6, 16 bytes]`,
    /// `[alen_hi, alen_lo, hw bytes…]` for a link-layer address, or empty.
    pub(super) body: Vec<u8>,
}
/// The C's `sockaddr_to_buf` encoding of `addr` (`inet_drv.c:5373`) plus the family it read.
///
/// IPv4 (and family `0`) become `INET_AF_INET` + the four address bytes; IPv6 becomes
/// `INET_AF_INET6` + sixteen; a link-layer address (`AF_PACKET`) becomes a big-endian length
/// followed by that many hardware-address bytes. Anything else encodes to nothing, which is the
/// C's `return ptr` (no bytes written).
fn sockaddr_to_buf(addr: &libc::sockaddr) -> SockAddr {
    let family = addr.sa_family as libc::c_int;
    let body = if family == libc::AF_INET || family == 0 {
        // SAFETY: the family check above proves the buffer holds a `sockaddr_in` (or is a
        // zeroed `sockaddr`, whose first four post-family bytes are address bytes either way),
        // and a `sockaddr` is at least as large and at least as aligned as one.
        let sin: libc::sockaddr_in = unsafe { std::ptr::read(std::ptr::from_ref(addr).cast()) };
        let mut body = Vec::with_capacity(5);
        body.push(super::wire::INET_AF_INET);
        body.extend_from_slice(&sin.sin_addr.s_addr.to_ne_bytes());
        body
    } else if family == libc::AF_INET6 {
        // SAFETY: the family check above proves the buffer holds a `sockaddr_in6`, and a
        // `sockaddr` is at least as large and at least as aligned as one.
        let sin6: libc::sockaddr_in6 = unsafe { std::ptr::read(std::ptr::from_ref(addr).cast()) };
        let mut body = Vec::with_capacity(17);
        body.push(super::wire::INET_AF_INET6);
        body.extend_from_slice(&sin6.sin6_addr.s6_addr);
        body
    } else if family == libc::AF_PACKET {
        // SAFETY: the family check above proves the buffer holds a `sockaddr_ll`, and a
        // `sockaddr` is at least as large and at least as aligned as one.
        let sll: libc::sockaddr_ll = unsafe { std::ptr::read(std::ptr::from_ref(addr).cast()) };
        let alen = usize::from(sll.sll_halen).min(sll.sll_addr.len());
        let mut body = Vec::with_capacity(2 + alen);
        body.extend_from_slice(&(alen as u16).to_be_bytes());
        body.extend_from_slice(&sll.sll_addr[..alen]);
        body
    } else {
        Vec::new()
    };
    SockAddr { family, body }
}
/// One `ifreq` the interface ioctls mutate in place, owned as an opaque value.
///
/// The C keeps a single `struct ifreq` across an `INET_REQ_IFGET`/`IFSET` operand and reuses it
/// for every option byte — the name is written once, each ioctl overwrites the union — so this
/// mirrors that: [`IfReq::named`] sets the name, [`ifreq_ioctl`] runs one request against it and
/// the accessors read the union afterwards.
#[derive(Clone, Copy, Debug)]
pub(super) struct IfReq(libc::ifreq);
/// The interface ioctl an [`IfReq`] can be driven with — the `SIOCGIF*` names the C's
/// `INET_REQ_IFGET` arm switches on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum IfReqIo {
    /// `SIOCGIFADDR` (`inet_drv.c:5843`).
    Addr,
    /// `SIOCGIFBRDADDR` (`:5911`).
    BroadAddr,
    /// `SIOCGIFDSTADDR` (`:5922`).
    DstAddr,
    /// `SIOCGIFNETMASK` (`:5933`).
    Netmask,
    /// `SIOCGIFMTU` (`:5949`).
    Mtu,
    /// `SIOCGIFFLAGS` (`:5959`).
    Flags,
    /// `SIOCGIFHWADDR` (`:5861`).
    HwAddr,
}
impl IfReqIo {
    /// The `ioctl(2)` request number this variant names.
    fn request(self) -> libc::Ioctl {
        let request = match self {
            IfReqIo::Addr => libc::SIOCGIFADDR,
            IfReqIo::BroadAddr => libc::SIOCGIFBRDADDR,
            IfReqIo::DstAddr => libc::SIOCGIFDSTADDR,
            IfReqIo::Netmask => libc::SIOCGIFNETMASK,
            IfReqIo::Mtu => libc::SIOCGIFMTU,
            IfReqIo::Flags => libc::SIOCGIFFLAGS,
            IfReqIo::HwAddr => libc::SIOCGIFHWADDR,
        };
        request as libc::Ioctl
    }
}
impl IfReq {
    /// A zeroed `ifreq` naming the interface `name` — the C's `sys_memset`/`sys_memcpy` pair
    /// (`inet_drv.c:5833`, `:6019`), which keeps at most `IFNAMSIZ - 1` name bytes.
    pub(super) fn named(name: &[u8]) -> IfReq {
        // SAFETY: an all-zero `ifreq` is a valid value of that type (every member of the union is
        // all-zero, and the name is assigned below).
        let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
        let limit = req.ifr_name.len().saturating_sub(1);
        for (slot, &byte) in req.ifr_name.iter_mut().zip(name.iter()).take(limit) {
            *slot = byte as libc::c_char;
        }
        IfReq(req)
    }
    /// The `ifru_addr` member as an encoded [`SockAddr`] — what an `ADDR`/`BROADADDR`/
    /// `DSTADDR`/`NETMASK` read leaves behind.
    ///
    /// On Linux `ifr_broadaddr`/`ifr_dstaddr`/`ifr_netmask` are all macro aliases of `ifr_addr`
    /// (one union member), which is why the C's `NETMASK` arm is documented as reading
    /// `ifreq.ifr_addr` when the `ifr_netmask` macro is absent (`inet_drv.c:5941`).
    pub(super) fn addr(&self) -> SockAddr {
        // SAFETY: reading any union member is sound for a `#[repr(C)]` union of plain data; the
        // `sockaddr` member is fully initialized (all-zero, or written by the kernel) and is at
        // least as large as the other members this type is used with.
        sockaddr_to_buf(unsafe { &self.0.ifr_ifru.ifru_addr })
    }
    /// The `ifru_mtu` member (`ifr_mtu`), the `MTU` record's value.
    pub(super) fn mtu(&self) -> i32 {
        // SAFETY: reading any union member is sound for a `#[repr(C)]` union of plain data, and
        // `ifru_mtu` is an `int` the kernel wrote.
        unsafe { self.0.ifr_ifru.ifru_mtu }
    }
    /// The `ifru_flags` member (`ifr_flags`), as the C's `int flags`.
    pub(super) fn flags(&self) -> i32 {
        // SAFETY: reading any union member is sound for a `#[repr(C)]` union of plain data, and
        // `ifru_flags` is a `short` the kernel wrote.
        i32::from(unsafe { self.0.ifr_ifru.ifru_flags })
    }
    /// The `ifru_hwaddr` member's `sa_data`, the `HWADDR` record's payload.
    ///
    /// The C copies `IFHWADDRLEN` (= 6) raw bytes out of `ifr_hwaddr.sa_data`
    /// (`inet_drv.c:5867`), not the `sa_family` that precedes them.
    pub(super) fn hwaddr(&self) -> Vec<u8> {
        // SAFETY: reading any union member is sound for a `#[repr(C)]` union of plain data, and
        // `ifru_hwaddr` is a `sockaddr` whose `sa_data` array is fully initialized.
        let sa = unsafe { &self.0.ifr_ifru.ifru_hwaddr };
        sa.sa_data[..HWADDR_LEN].iter().map(|&b| b as u8).collect()
    }
}
/// The number of raw bytes an `HWADDR` record carries (`IFHWADDRLEN`, `inet_drv.c:5800`).
pub(super) const HWADDR_LEN: usize = 6;
/// Drives one interface ioctl against `req` (`ioctl(2)` on the driver's socket).
///
/// Only the reading (`SIOCGIF*`) direction exists: the `INET_REQ_IFSET` arm answers purely from
/// its operand's parse and performs none of the writes it decodes, so no `SIOCSIF*` request is
/// ever issued from this crate (`super::wire::ifset_reply` states that divergence).
///
/// # Errors
///
/// The raw `ioctl(2)` failure — `ENODEV` for an interface that does not exist, which every C call
/// site renders as "no record for this option" (or, for `FLAGS`, as a zero bit set).
pub(super) fn ifreq_ioctl(fd: RawFd, req: &mut IfReq, which: IfReqIo) -> std::io::Result<()> {
    // SAFETY: `fd` is the caller's socket, `req` is a live `ifreq` this type owns and the
    // request is one of the `SIOCGIF*` numbers, all of which take an `ifreq*` as their third
    // argument — so the kernel reads/writes at most `size_of::<ifreq>()` bytes of it and never
    // retains the pointer.
    if unsafe { libc::ioctl(fd, which.request(), &mut req.0) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The interface names a `SIOCGIFCONF` walk reports, in kernel order — the C's `get_ifconf`
/// (`inet_drv.c:5671`) without the reply framing.
///
/// The C grows a heap buffer in `10 * sizeof(struct ifreq)` steps until `SIOCGIFCONF` stops
/// reporting a longer `ifc_len`, retries an `EINVAL` while it has not yet seen any length (the
/// documented first-call behaviour of some kernels), and then walks the entries with
/// `n = sizeof(ifr_name) + SIZEA(ifr_addr)`, raised to `sizeof(struct ifreq)` when that is
/// smaller — 40 bytes on every platform this crate builds for. Only `AF_INET`/`AF_INET6`
/// entries are reported, which is why the C's `switch` has no other case.
///
/// # Errors
///
/// The `ioctl(2)` failure the C renders through `ctl_error(sock_errno())` (`ENOTTY` for a
/// descriptor that is not a socket, `EINVAL` for an unusable buffer).
pub(super) fn ifconf_names(fd: RawFd) -> std::io::Result<Vec<Vec<u8>>> {
    let slot = std::mem::size_of::<libc::ifreq>();
    let mut buflen = 100 * slot;
    let mut seen: Option<usize> = None;
    loop {
        let mut buf = vec![0u8; buflen];
        // SAFETY: an all-zero `ifconf` is a valid value of that type; both members are assigned
        // before the ioctl, and `ifcu_buf` points at the `buf` local's `buflen` writable bytes.
        let mut ifc: libc::ifconf = unsafe { std::mem::zeroed() };
        ifc.ifc_len = buflen as libc::c_int;
        ifc.ifc_ifcu.ifcu_buf = buf.as_mut_ptr().cast();
        // SAFETY: `fd` is the caller's socket, `SIOCGIFCONF` takes an `ifconf*` third argument,
        // and `ifc` is a live `ifconf` whose buffer is `buflen` writable bytes — so the kernel
        // writes at most that many bytes into `buf` and never retains any pointer.
        let rc = unsafe { libc::ioctl(fd, libc::SIOCGIFCONF as libc::Ioctl, &mut ifc) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINVAL) || seen.is_some() {
                return Err(err);
            }
        } else {
            let reported = ifc.ifc_len.max(0) as usize;
            if seen == Some(reported) {
                return Ok(walk_ifconf(&buf, reported.min(buflen), slot));
            }
            seen = Some(reported);
        }
        buflen += 10 * slot;
    }
}
/// Splits a filled `SIOCGIFCONF` buffer into the `AF_INET`/`AF_INET6` interface names.
///
/// The entry stride is the C's `n = sizeof(ifr_name) + SIZEA(ifr_addr)`, raised to
/// `sizeof(struct ifreq)`; the walk stops when the next entry would overrun the reported length.
fn walk_ifconf(buf: &[u8], reported: usize, slot: usize) -> Vec<Vec<u8>> {
    // SAFETY: `ifr_name`'s type is the same `[c_char; IFNAMSIZ]` array on every `ifreq` this
    // crate builds against, so the constant size is a property of the type, not of the value.
    let name_len = std::mem::size_of::<libc::c_char>() * libc::IFNAMSIZ;
    // SAFETY: `sockaddr` is a plain `#[repr(C)]` struct, so its size is likewise a type property.
    let addr_len = std::mem::size_of::<libc::sockaddr>();
    let stride = (name_len + addr_len).max(slot);
    let mut names = Vec::new();
    let mut offset = 0usize;
    while offset + stride <= reported {
        let entry = &buf[offset..offset + stride];
        offset += stride;
        // SAFETY: `entry` is exactly `stride` bytes starting at `offset`, which is a whole,
        // zero-initialized `ifreq` the kernel filled in; a `sockaddr` is the same size and
        // alignment as the union member it aliases, so reading the front of the entry as one is
        // well defined.
        let family = unsafe { (*entry.as_ptr().cast::<libc::sockaddr>()).sa_family } as libc::c_int;
        if family == libc::AF_INET || family == libc::AF_INET6 {
            let end = entry.iter().position(|&b| b == 0).unwrap_or(name_len);
            names.push(entry[..end.min(name_len)].to_vec());
        }
        if offset >= reported {
            break;
        }
    }
    names
}
/// One `getifaddrs(3)` record, decoded into the pieces the C's `INET_REQ_GETIFADDRS` encoder
/// reads (`inet_drv.c:6754`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct IfAddrEntry {
    /// The interface name's raw bytes (`ifa_name`, up to its NUL).
    pub(super) name: Vec<u8>,
    /// The kernel's `ifa_flags`, which the C reduces through `IFGET_FLAGS`.
    pub(super) flags: u32,
    /// `ifa_addr`'s `sa_family`, or [`NO_ADDRESS`] when `ifa_addr` is NULL.
    pub(super) family: libc::c_int,
    /// [`sockaddr_to_buf`]'s encoding of `ifa_addr` (empty when it encodes to nothing).
    pub(super) addr: Vec<u8>,
    /// [`sockaddr_to_buf`]'s encoding of `ifa_netmask`, when it is present.
    pub(super) netmask: Option<Vec<u8>>,
    /// [`sockaddr_to_buf`]'s encoding of `ifa_dstaddr`/`ifa_broadaddr` — the `ifa_ifu` union —
    /// when it is present. Which of the two it is depends on the interface's flags, which is the
    /// caller's `IFF_POINTOPOINT`/`IFF_BROADCAST` decision (`:6745`).
    pub(super) ifu: Option<Vec<u8>>,
}
/// The `family` an [`IfAddrEntry`] carries when `ifa_addr` was NULL.
pub(super) const NO_ADDRESS: libc::c_int = -1;
/// The `getifaddrs(3)` list, walked once and copied out — the C's `call_getifaddrs` plus the
/// per-entry decode of its encoder.
///
/// The returned order is the libc's list order, which the C also consumes verbatim: the reply's
/// record order is therefore the host's, not a sorted one, on both sides.
///
/// # Errors
///
/// The `getifaddrs(3)` errno, which the C renders through `ctl_error` (`:6704`).
pub(super) fn ifaddrs_entries() -> std::io::Result<Vec<IfAddrEntry>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs(3)` takes one out-parameter, a valid `*mut *mut ifaddrs` pointing at
    // the `head` local, which it fills with an owned list (or NULL). The list is released below
    // through the matching `freeifaddrs` exactly once.
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut entries = Vec::new();
    let mut current = head;
    while !current.is_null() {
        // SAFETY: `current` is one node of the list `getifaddrs(3)` just returned, which stays
        // alive until the `freeifaddrs` below; every field read is a field of that node, and the
        // `&str`-free decode copies out what it needs so no pointer escapes this loop.
        entries.push(unsafe { decode_ifaddr(&*current) });
        current = unsafe { (*current).ifa_next };
    }
    // SAFETY: `head` is the list `getifaddrs(3)` returned and has not been freed yet;
    // `freeifaddrs(3)` releases exactly that list and nothing else.
    unsafe { libc::freeifaddrs(head) };
    Ok(entries)
}
/// Decodes one `ifaddrs` node.
///
/// # Safety
///
/// `node` must be a live node of a list returned by `getifaddrs(3)`: its `ifa_name` must be a
/// NUL-terminated string and its address pointers must either be NULL or point at a valid
/// `sockaddr`.
unsafe fn decode_ifaddr(node: &libc::ifaddrs) -> IfAddrEntry {
    let name = if node.ifa_name.is_null() {
        Vec::new()
    } else {
        // SAFETY: the caller guarantees `ifa_name` is a NUL-terminated string owned by the list;
        // it is copied immediately and never retained.
        unsafe { std::ffi::CStr::from_ptr(node.ifa_name) }
            .to_bytes()
            .to_vec()
    };
    let addr = if node.ifa_addr.is_null() {
        None
    } else {
        // SAFETY: the caller guarantees a non-NULL `ifa_addr` points at a valid `sockaddr`.
        Some(sockaddr_to_buf(unsafe { &*node.ifa_addr }))
    };
    let netmask = if node.ifa_netmask.is_null() {
        None
    } else {
        // SAFETY: the caller guarantees a non-NULL `ifa_netmask` points at a valid `sockaddr`.
        Some(sockaddr_to_buf(unsafe { &*node.ifa_netmask }).body)
    };
    let ifu = if node.ifa_ifu.is_null() {
        None
    } else {
        // SAFETY: the caller guarantees a non-NULL `ifa_ifu` points at a valid `sockaddr`.
        Some(sockaddr_to_buf(unsafe { &*node.ifa_ifu }).body)
    };
    IfAddrEntry {
        name,
        flags: node.ifa_flags,
        family: addr.as_ref().map_or(NO_ADDRESS, |addr| addr.family),
        addr: addr.map(|addr| addr.body).unwrap_or_default(),
        netmask,
        ifu,
    }
}
/// The byte count the C's `sockaddr_bufsz_need` (`inet_drv.c:5419`) would report for an
/// [`IfAddrEntry`]'s address, derived from the encoded body: the C asks whether the link-layer
/// address is longer than three bytes before emitting a `HWADDR` record (`inet_drv.c:6769`).
pub(super) fn entry_addr_need(entry: &IfAddrEntry) -> usize {
    entry.addr.len()
}
/// Whether `addr` re-encodes as a hardware address of the shape `INET_REQ_GETIFADDRS` emits.
///
/// Split out so the caller never has to reconstruct a `sockaddr` just to ask
/// [`entry_addr_need`] a question the family already answers.
pub(super) fn is_link_layer(family: libc::c_int) -> bool {
    family == libc::AF_PACKET
}
