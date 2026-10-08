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

//! The `tcp_inet` socket **wire vocabulary**.
//!
//! The byte-level vocabulary the `tcp_inet` driver speaks to the Erlang side: the control
//! codes and result tags the C driver uses (`INET_REP_*`, `INET_AF_*`, `INET_TYPE_*`,
//! `INET_PROTO_*`, `TCP_SHUT_*`), the C's `ctl_reply` shapes, the packed-address codec
//! (`inet_set_faddress`/`inet_set_address`) and the lowercase-errno renderer
//! (`erl_errno_id`).
//!
//! This is a **private** detail of the [`super`] module: every name the crate exposes at
//! `crate::drivers::inet::*` is re-exported there, so this split is invisible to callers.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`.

use beam_common::Result;

use crate::error;

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

/// A decoded IPv4 endpoint from the packed address of the `bind`/`connect` verbs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct InetAddr {
    /// The port, in host byte order.
    pub(super) port: u16,
    /// The four IPv4 address octets, in network (= textual) order.
    pub(super) addr: [u8; 4],
}

impl InetAddr {
    /// The `sockaddr_in` this endpoint names.
    pub(super) fn sockaddr_in(self) -> libc::sockaddr_in {
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

// ---------------------------------------------------------------------------
// The C reply shapes (`ctl_reply`, `inet_drv.c:2260`)
// ---------------------------------------------------------------------------

/// The success reply `[INET_REP_OK]` (the C's `ctl_reply_ok`, `inet_drv.c:2280`).
pub(super) fn ctl_ok() -> Vec<u8> {
    vec![INET_REP_OK]
}

/// The reply `[INET_REP_ERROR] ++ name` (the C's `ctl_xerror`, `inet_drv.c:2307`).
pub(super) fn ctl_xerror(name: &str) -> Vec<u8> {
    let mut reply = Vec::with_capacity(1 + name.len());
    reply.push(INET_REP_ERROR);
    reply.extend_from_slice(name.as_bytes());
    reply
}

/// The reply `[INET_REP_ERROR] ++ errno_name(err)` (the C's `ctl_error`, `inet_drv.c:2300`).
pub(super) fn ctl_error(err: std::io::Error) -> Vec<u8> {
    ctl_xerror(&errno_name(err.raw_os_error().unwrap_or(0)))
}

/// The lowercase name the oracle reports for `errno` (`erl_errno_id`/`errno_str`,
/// `inet_drv.c:2249`).
///
/// The C asks the libc for the name (`strerrorname_np`/`strerror`) and lowercases it; this
/// floor keeps a table of the codes a TCP socket can actually raise and renders anything else
/// as `errno_<N>`, which is exactly the C's fallback format
/// (`ERTS_UNKNOWN_ERRNO_FORMAT`).
pub(super) fn errno_name(errno: i32) -> String {
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
pub(super) fn parse_address(arg: &[u8]) -> Result<InetAddr> {
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
