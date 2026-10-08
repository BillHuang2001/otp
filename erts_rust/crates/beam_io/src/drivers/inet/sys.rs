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
//! `getsockname(2)`/`getsockopt(2)`/`shutdown(2)`/`close(2)` — plus the two kernel state
//! probes the adopted-descriptor path needs ([`is_listening`] over `SO_ACCEPTCONN` and
//! [`has_peer`] over `getpeername(2)`).
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
