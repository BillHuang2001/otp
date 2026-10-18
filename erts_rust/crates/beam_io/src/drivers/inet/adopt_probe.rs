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

//! The **errno an `INET_REQ_FDOPEN`(13) adoption failure carries**, and the C reply it becomes.
//!
//! `inet_ctl_fdopen` (`inet_drv.c:5223`-`:5282`) never *raises* on a descriptor it cannot take
//! over: every failure inside it is a `ctl_error(...)` **reply**, i.e. `[INET_REP_ERROR] ++
//! errno_name(sock_errno())`:
//!
//! * `bound` non-zero and `sock_name(2)` fails ⇒ `ctl_error(sock_errno())` (`:5253`) —
//!   `enotsock` for a descriptor that is not a socket, `ebadf` for a closed number;
//! * `bound` non-zero and the families differ ⇒ `ctl_error(EINVAL)` (`:5255`);
//! * the descriptor's event/registration cannot be created ⇒ `ctl_error(sock_errno())` (`:5260`).
//!
//! [`adoption_reply`] is that reply channel for the failures the Rust adoption body
//! ([`super::TcpInetDriver::adopt_prebound`] / [`super::UdpInetDriver::adopt_prebound`]) can hit
//! *after* the operand ladder has passed: a failed `fcntl(F_SETFL, O_NONBLOCK)`, a failed
//! `getpeername(2)` probe, or a poller registration the operating system refused. Naming the errno
//! is what makes `erlang:port_control(Port, INET_REQ_FDOPEN, …)` answer `[0]"ebadf"` /
//! `[0]"enotsock"` / `[0]"eperm"` the way the C does, instead of collapsing every one of them
//! into a raise (no reply at all) or into a generic `[0]"einval"` (the wrong errno).
//!
//! The errno itself is already carried by the primitives: `drivers::fd::set_nonblocking`,
//! `sys::has_peer`, `sys::socket_family` and `Poller::register` all answer
//! `BeamError::Io(std::io::Error::from_raw_os_error(...))` preserved from the failed syscall, so
//! this module only *renders* it — it never synthesises one.
//!
//! The C's `EINVAL` channel (`ctl_error(EINVAL)`, `:5255`) is **not** this module's: the arm
//! answers it inline, because in both drivers the family mismatch is decided before any adoption
//! body runs (`wire::fdopen_precheck`).

use beam_common::BeamError;

use super::wire::ctl_error;

/// The POSIX errno a failed adoption carries, if it carries one.
///
/// Only a syscall failure can name an errno, so only [`BeamError::Io`] can answer `Some`. The
/// descriptor verdicts the C spells as errno replies all arrive that way; the crate's other
/// variants are internal limits (`SystemLimit` from the poller's token space, `BadArg` from an
/// argument fault) that have no errno to name.
pub(super) fn adoption_errno(err: &BeamError) -> Option<i32> {
    match err {
        BeamError::Io(io) => io.raw_os_error(),
        _ => None,
    }
}

/// The C's reply for a failed `INET_REQ_FDOPEN`(13) adoption — `Some` when the failure names an
/// errno, `None` when it does not (so the caller keeps the typed error).
///
/// `Some(reply)` is exactly `ctl_error(sock_errno())`: the one-byte `[INET_REP_ERROR]` tag
/// followed by the lowercase errno name (`wire::errno_name`, the C's `erl_errno_id`), with its
/// `errno_<N>` fallback for a code the table does not name. Measured against the system
/// `tcp_inet`/`udp_inet` driver in this floor's own process (a descriptor this process owns can
/// never be named to a second process): a closed descriptor is `[0]"ebadf"`, a non-socket
/// `[0]"enotsock"`, an `AF_INET6` socket under the IPv4 tag `[0]"einval"`.
///
/// `None` keeps the caller's [`BeamError`]: a failure with no errno has no faithful rendering in
/// this channel, and the C only ever answers a *fabricated* errno there. Fabricating one would be
/// the "wrong result" the workspace error strategy forbids, so the typed error (a raise) is the
/// honest outcome for those — they are internal limits, never a descriptor verdict.
pub(super) fn adoption_reply(err: &BeamError) -> Option<Vec<u8>> {
    let errno = adoption_errno(err)?;
    Some(ctl_error(std::io::Error::from_raw_os_error(errno)))
}

#[cfg(test)]
mod tests {
    use super::{adoption_errno, adoption_reply};
    use crate::drivers::inet::wire::{ctl_xerror, errno_name};
    use beam_common::BeamError;

    #[test]
    fn an_io_failure_is_rendered_as_the_c_errno_reply() {
        let err = BeamError::Io(std::io::Error::from_raw_os_error(libc::EBADF));
        assert_eq!(adoption_errno(&err), Some(libc::EBADF));
        assert_eq!(adoption_reply(&err), Some(ctl_xerror("ebadf")));

        let err = BeamError::Io(std::io::Error::from_raw_os_error(libc::ENOTSOCK));
        assert_eq!(adoption_reply(&err), Some(ctl_xerror("enotsock")));

        let err = BeamError::Io(std::io::Error::from_raw_os_error(libc::EPERM));
        assert_eq!(adoption_reply(&err), Some(ctl_xerror("eperm")));
    }

    #[test]
    fn an_errno_the_c_names_generically_keeps_the_errno_fallback() {
        // The C's fallback format is `errno_<N>` (`ERTS_UNKNOWN_ERRNO_FORMAT`), so an errno the
        // table does not know is still *named*, never flattened into `einval`.
        let err = BeamError::Io(std::io::Error::from_raw_os_error(4711));
        assert_eq!(errno_name(4711), "errno_4711");
        assert_eq!(
            adoption_reply(&err),
            Some(ctl_xerror("errno_4711")),
            "an unnamed errno renders through the C's `errno_<N>` fallback"
        );
    }

    #[test]
    fn a_failure_without_an_errno_keeps_the_typed_error() {
        // No errno to name: the reply channel has no faithful rendering, and the caller keeps the
        // `BeamError` (a raise) instead of an invented errno.
        for err in [
            BeamError::BadArg,
            BeamError::SystemLimit,
            BeamError::Io(std::io::Error::new(std::io::ErrorKind::Other, "no errno")),
        ] {
            assert_eq!(adoption_errno(&err), None, "{err:?} names no errno");
            assert_eq!(adoption_reply(&err), None, "{err:?} has no reply");
        }
    }

    #[test]
    fn the_reply_is_the_error_tag_plus_the_lowercase_name() {
        let reply = adoption_reply(&BeamError::Io(std::io::Error::from_raw_os_error(
            libc::ENOTSOCK,
        )))
        .expect("an errno reply");
        assert_eq!(reply[0], crate::drivers::inet::INET_REP_ERROR, "the `[0]` tag");
        assert_eq!(&reply[1..], b"enotsock", "the lowercase POSIX name");
    }
}
