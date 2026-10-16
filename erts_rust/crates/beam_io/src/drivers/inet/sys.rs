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
