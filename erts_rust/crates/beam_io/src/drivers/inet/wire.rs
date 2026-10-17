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

use std::os::unix::io::RawFd;

use beam_common::Result;

use crate::error;

use super::sys;

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

/// The datagram socket type (`INET_TYPE_DGRAM`, `inet_drv.c:767`) — the type the `udp_inet`
/// driver's `open` verb requires.
pub const INET_TYPE_DGRAM: u8 = 2;

/// The default TCP protocol (`INET_PROTO_DEFAULT`, `inet_drv.c:771`).
pub const INET_PROTO_DEFAULT: u8 = 0;

/// The explicit TCP protocol (`INET_PROTO_TCP`, `inet_drv.c:772`).
pub const INET_PROTO_TCP: u8 = 1;

/// The explicit UDP protocol (`INET_PROTO_UDP`, `inet_drv.c:773`) — the only provisional
/// protocol the `udp_inet` driver's `open` verb accepts besides [`INET_PROTO_DEFAULT`].
pub const INET_PROTO_UDP: u8 = 2;

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
        libc::EBADF => Some("ebadf"),
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
// The reply shapes of the shared `inet_ctl` arms
// ---------------------------------------------------------------------------
//
// Both drivers delegate `GETSTAT`/`GETTYPE`/`GETFD`/`GETSTATUS`/`NAME`/`PEER` to the *same*
// `inet_ctl` function (`inet_drv.c:10988`), so their reply shapes are built once here rather than
// once per driver. Each builder is a faithful port of the corresponding C arm; where this floor
// cannot produce the C's payload it is a typed error, never a fabricated reply.

/// The `INET_REQ_GETSTAT` reply (the C's `inet_fill_stat`, `inet_drv.c:10732`).
///
/// An **empty** request answers the bare success tag — the C's `fill_stat` writes a record only
/// for each requested counter, so nothing requested means nothing appended (measured `[1]` on
/// both drivers' bare ports).
///
/// # Errors
///
/// [`beam_common::BeamError`] carrying the typed "not served" reason for a non-empty request: the
/// C's statistics counters (`recv_cnt`/`send_cnt`/`recv_oct`/`send_oct`/…) live on the driver's
/// descriptor and are not tracked by this floor, and answering a zeroed counter would be a wrong
/// value rather than a missing one.
pub(super) fn getstat_reply(arg: &[u8]) -> Result<Vec<u8>> {
    if arg.is_empty() {
        return Ok(ctl_ok());
    }
    Err(error::unsupported(
        "the C's inet statistics counters (inet_fill_stat) are not tracked by this driver",
    ))
}

/// The `INET_REQ_GETTYPE` reply: the descriptor's **cached** `{Family, Type}` pair
/// (`inet_drv.c:11165`-`:11203`, `case INET_REQ_GETTYPE`).
///
/// The C arm has **no `IS_OPEN` guard**: it switches on the descriptor's cached `sfamily` and only
/// then on its cached `stype`. Both are set when a socket is opened or adopted (`inet_ctl_open`,
/// `:5214`-`:5216`; `inet_ctl_fdopen`, `:5265`) and are started at `-1` by `inet_init_desc`
/// (`:10925`); no close path clears them. So a port that never opened a socket still carries the
/// unset pair and answers `einval` (measured `[0]einval` on a bare port of either driver), while a
/// port whose socket has been closed reports the type it was opened with (measured
/// `[1,0,0,0,1,0,0,0,1]` after a `tcp_inet` close and `[1,0,0,0,1,0,0,0,2]` after a `udp_inet` one).
///
/// `family`/`stype` are the wire tags with `0` meaning "unset": only [`INET_AF_INET`] with
/// [`INET_TYPE_STREAM`] or [`INET_TYPE_DGRAM`] is served, and anything else is the C's
/// `ctl_error(EINVAL)` (IPv6/SCTP are out of scope, so their tags never appear here).
pub(super) fn gettype_reply(family: u16, stype: u16) -> Vec<u8> {
    if family != u16::from(INET_AF_INET) {
        return ctl_xerror("einval");
    }
    if stype != u16::from(INET_TYPE_STREAM) && stype != u16::from(INET_TYPE_DGRAM) {
        return ctl_xerror("einval");
    }
    let mut reply = ctl_ok();
    reply.extend_from_slice(&u32::from(INET_AF_INET).to_be_bytes());
    reply.extend_from_slice(&u32::from(stype).to_be_bytes());
    reply
}

/// The `INET_REQ_GETFD` reply: the driver's descriptor number, big-endian
/// (`inet_drv.c:11160`, `case INET_REQ_GETFD`).
///
/// A socket-less port answers [`ctl_xerror`]`("einval")` — the C's `if (!IS_OPEN) return
/// ctl_error(EINVAL)` (measured `[0]einval`).
pub(super) fn getfd_reply(fd: Option<RawFd>) -> Vec<u8> {
    match fd {
        Some(fd) => {
            let mut reply = ctl_ok();
            reply.extend_from_slice(&fd.to_be_bytes());
            reply
        }
        None => ctl_xerror("einval"),
    }
}

/// The `INET_REQ_GETSTATUS` reply: the C's `INET_STATE_*` bit set, big-endian
/// (`inet_drv.c:11175`, `case INET_REQ_GETSTATUS`).
///
/// There is no `IS_OPEN` guard in the C — the state bits are the driver's own — so a socket-less
/// port answers the `0` (`Closed`) it is really in (measured `[1,0,0,0,0]`).
pub(super) fn getstatus_reply(state_code: u32) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&state_code.to_be_bytes());
    reply
}

/// The `INET_REQ_NAME` reply: the port's own endpoint, as a packed address
/// (`inet_drv.c:11329`, `case INET_REQ_NAME`).
///
/// The C arm has **no `IS_OPEN` guard**: it calls `getsockname(2)` on the descriptor unconditionally,
/// so a socket-less port answers the syscall's own errno (measured `[0]ebadf`) rather than a
/// fabricated `einval`. This floor reproduces that: no descriptor ⇒ the `EBADF` refusal, an open
/// socket ⇒ `[1] ++ pack_address(...)` (measured the 8-byte `[1,1,<port_hi>,<port_lo>,a,b,c,d]`), a
/// failing syscall ⇒ `[0] ++ errno_name(errno)`.
pub(super) fn name_reply(fd: Option<RawFd>) -> Result<Vec<u8>> {
    let Some(fd) = fd else {
        return Ok(ctl_xerror(&errno_name(libc::EBADF)));
    };
    match sys::local_address(fd) {
        Ok(address) => {
            let mut reply = ctl_ok();
            reply.extend_from_slice(&pack_address(address.port, address.addr));
            Ok(reply)
        }
        Err(err) => Ok(ctl_error(err)),
    }
}

/// The `INET_REQ_PEER` reply: the peer's endpoint, as a packed address
/// (`inet_drv.c:11280`, `case INET_REQ_PEER`).
///
/// The C guards on the connection, not on the descriptor: `if (!(desc->state & INET_F_ACTIVE))
/// return ctl_error(ENOTCONN)` (measured `[0]enotconn` on a bare port **and** on a listening one).
/// A connected socket answers `[1] ++ pack_address(...)` (measured `[1,1,225,36,127,0,0,1]`); a
/// failing `getpeername(2)` answers `[0] ++ errno_name(errno)`.
pub(super) fn peer_reply(fd: Option<RawFd>, connected: bool) -> Result<Vec<u8>> {
    if !connected {
        return Ok(ctl_xerror("enotconn"));
    }
    let Some(fd) = fd else {
        // A connected state with no descriptor cannot happen; refuse rather than call into the
        // kernel with a dangling number.
        return Ok(ctl_xerror("enotconn"));
    };
    match sys::peer_address(fd) {
        Ok(address) => {
            let mut reply = ctl_ok();
            reply.extend_from_slice(&pack_address(address.port, address.addr));
            Ok(reply)
        }
        Err(err) => Ok(ctl_error(err)),
    }
}// ---------------------------------------------------------------------------
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

/// Encodes an IPv4 endpoint into the C's packed address, the `INET_AF_INET` 7-byte form
/// `[INET_AF_INET, port_hi, port_lo, a, b, c, d]`.
///
/// This is the inverse of [`parse_address`] (the C's `inet_get_address`, `inet_drv.c:4626`) and
/// the **single** source of truth for the byte layout the `udp_inet` driver's `recv` reply uses
/// to report a datagram's source address.
pub(super) fn pack_address(port: u16, addr: [u8; 4]) -> Vec<u8> {
    let [hi, lo] = port.to_be_bytes();
    vec![INET_AF_INET, hi, lo, addr[0], addr[1], addr[2], addr[3]]
}

/// Splits a leading packed address off `arg`, returning it with the bytes that follow.
///
/// The `udp_inet` `send` verb's argument is `[address][data]` (the C's
/// `[address][anc_len:32][data]` without the ancillary-data length, which is out of scope), so
/// the address has to be consumed before the payload can be taken. Both this and
/// [`parse_address`] derive the address length from the family tag, so the layout stays
/// single-sourced.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] for a truncated argument or an unserved family tag, as in
/// [`parse_address`].
pub(super) fn split_address(arg: &[u8]) -> Result<(InetAddr, &[u8])> {
    let consumed = match arg.first() {
        Some(&INET_AF_INET) => 7,
        Some(&INET_AF_ANY) | Some(&INET_AF_LOOPBACK) => 3,
        _ => return Err(error::badarg()),
    };
    if arg.len() < consumed {
        return Err(error::badarg());
    }
    let address = parse_address(&arg[..consumed])?;
    Ok((address, &arg[consumed..]))
}
