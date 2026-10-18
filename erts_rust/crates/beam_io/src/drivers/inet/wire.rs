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
//! `INET_PROTO_*`, `TCP_SHUT_*`), the interface option records (`INET_IFOPT_*`/`INET_IFF_*`), the
//! C's `ctl_reply` shapes, the packed-address codec (`inet_set_faddress`/`inet_set_address`) and
//! the lowercase-errno renderer (`erl_errno_id`).
//!
//! This is a **private** detail of the [`super`] module: every name the crate exposes at
//! `crate::drivers::inet::*` is re-exported there, so this split is invisible to callers.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`.

use std::os::unix::io::RawFd;

use beam_common::Result;

use crate::error;

use super::opcode::Opcode;
use super::sys;

/// The result tag of a successful control operation (`INET_REP_OK`, `inet_drv.c:870`).
pub const INET_REP_OK: u8 = 1;

/// The result tag of a failed control operation (`INET_REP_ERROR`, `inet_drv.c:869`).
pub const INET_REP_ERROR: u8 = 0;

/// The address tag of an explicit IPv4 address (`INET_AF_INET`, `inet_drv.c:757`).
pub const INET_AF_INET: u8 = 1;

/// The address tag of an explicit IPv6 address (`INET_AF_INET6`, `inet_drv.c:758`) — the tag
/// the interface records carry for an `AF_INET6` endpoint.
pub const INET_AF_INET6: u8 = 2;
/// The address tag of `INADDR_ANY` (`INET_AF_ANY`, `inet_drv.c:759`).
pub const INET_AF_ANY: u8 = 3;

/// The `ADDR` option record of an `INET_REQ_IFGET`/`IFSET` operand or reply (`INET_IFOPT_ADDR`,
/// `inet_drv.c:952`).
pub(super) const INET_IFOPT_ADDR: u8 = 1;

/// The `BROADADDR` interface option record (`INET_IFOPT_BROADADDR`, `inet_drv.c:953`).
pub(super) const INET_IFOPT_BROADADDR: u8 = 2;

/// The `DSTADDR` interface option record (`INET_IFOPT_DSTADDR`, `inet_drv.c:954`).
pub(super) const INET_IFOPT_DSTADDR: u8 = 3;

/// The `MTU` interface option record (`INET_IFOPT_MTU`, `inet_drv.c:955`) — a big-endian 32-bit
/// value in an `IFGET` reply and a bare four-byte value in an `IFSET` operand.
pub(super) const INET_IFOPT_MTU: u8 = 4;

/// The `NETMASK` interface option record (`INET_IFOPT_NETMASK`, `inet_drv.c:956`).
pub(super) const INET_IFOPT_NETMASK: u8 = 5;

/// The `FLAGS` interface option record (`INET_IFOPT_FLAGS`, `inet_drv.c:957`) — always a
/// big-endian 32-bit [`INET_IFF_*`](INET_IFF_UP) bit set, in both an `IFGET` reply and an `IFSET`
/// operand.
pub(super) const INET_IFOPT_FLAGS: u8 = 6;

/// The `HWADDR` interface option record (`INET_IFOPT_HWADDR`, `inet_drv.c:958`) — in an `IFGET`
/// reply a big-endian 16-bit length followed by that many hardware-address bytes, in an `IFSET`
/// operand the same pair starting **after** the option byte.
pub(super) const INET_IFOPT_HWADDR: u8 = 7;

/// The driver's own `UP` flag bit (`INET_IFF_UP`, `inet_drv.c:973`).
pub(super) const INET_IFF_UP: u32 = 0x0001;

/// The driver's own `BROADCAST` flag bit (`INET_IFF_BROADCAST`, `inet_drv.c:974`).
pub(super) const INET_IFF_BROADCAST: u32 = 0x0002;

/// The driver's own `LOOPBACK` flag bit (`INET_IFF_LOOPBACK`, `inet_drv.c:975`).
pub(super) const INET_IFF_LOOPBACK: u32 = 0x0004;

/// The driver's own `POINTTOPOINT` flag bit (`INET_IFF_POINTTOPOINT`, `inet_drv.c:976`).
pub(super) const INET_IFF_POINTTOPOINT: u32 = 0x0008;

/// The driver's own `RUNNING` flag bit (`INET_IFF_RUNNING`, `inet_drv.c:977`), which the C
/// **emulates** from the kernel's `IFF_UP`.
pub(super) const INET_IFF_RUNNING: u32 = 0x0010;

/// The driver's own `MULTICAST` flag bit (`INET_IFF_MULTICAST`, `inet_drv.c:978`).
pub(super) const INET_IFF_MULTICAST: u32 = 0x0020;

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

/// The one `INET_REQ_SUBSCRIBE` op there is (`INET_SUBS_EMPTY_OUT_Q`, `inet_drv.c:850`) — "tell
/// me when your output queue empties". Any other operand byte is the C's `return -1`.
pub const INET_SUBS_EMPTY_OUT_Q: u8 = 1;
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

/// The `INET_REQ_SUBSCRIBE`(24) reply — the **shared** `inet_ctl` arm both socket drivers reach
/// (`inet_subscribe`, `inet_drv.c:10819`-`:10843`).
///
/// The C walks the operand one byte at a time and builds `[INET_REP_OK] ++ (op ++ be32(outq))*`,
/// where `outq` is `driver_sizeq(desc->port)` — the port's **output queue** byte count at that
/// moment. `queued_output_bytes` is that count ([`crate::DriverContext::output`]'s
/// `ByteQueue::len`), never a constant: on this floor's ports the queue is normally empty when a
/// control call runs (a command's bytes are moved by the `Output` port task, so `port_control`
/// sees them only after a write was refused and the remainder stayed queued), which is why the
/// oracle also reports `0` in the ordinary probes — but a port holding a refused write reports
/// its real depth, as the C does.
///
/// An operand byte that is not [`INET_SUBS_EMPTY_OUT_Q`] makes the C `return -1`, which the
/// `inet_ctl` arm turns into `badarg`; the whole reply is discarded, so a valid byte **before**
/// the bad one is not preserved either. That is [`error::badarg`] here.
///
/// # The notification half
///
/// The C also *saves the caller as a subscriber* when the count is `> 0` and later sends it
/// `{empty_out_q, Port}` from `send_empty_out_q_msgs`. This crate has no driver→owner message
/// channel (the [`crate::PortNotification`] seam carries `Data`/`Closed` only), so **no
/// notification is ever sent** — the reply is exact, the subscription is not honoured. A caller
/// that only waits for the reply (all of `prim_inet`'s uses) sees the C's behaviour exactly; one
/// that waits for the message sees nothing.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] for an operand byte the C's `switch` has no case for.
pub(super) fn subscribe_reply(queued_output_bytes: usize, arg: &[u8]) -> Result<Vec<u8>> {
    let mut reply = ctl_ok();
    for &op in arg {
        if op != INET_SUBS_EMPTY_OUT_Q {
            return Err(error::badarg());
        }
        reply.push(op);
        // `put_int32(val, dst)` writes the low 32 bits of the C's `unsigned long val`
        // (`inet_drv.c:10841`), so the render is a truncating cast of the queue depth.
        reply.extend_from_slice(&(queued_output_bytes as u32).to_be_bytes());
    }
    Ok(reply)
}
/// The `INET_REQ_GETSERVBYNAME`(17) reply — the shared `inet_ctl` arm both socket drivers reach
/// (`inet_ctl_getservbyname`, `inet_drv.c:11493`).
///
/// The operand is `L1 Name-String L2 Proto-String`, and the C's arithmetic is reproduced
/// literally, including its **asymmetry**: the name length is checked with `n >= len` *after* the
/// length byte was consumed — the `=` is deliberate, because the test has to leave room for the
/// protocol length byte that follows (`:11501`) — while the protocol length is only `n > len`
/// (`:11508`). A lookup the services database does not answer is the C's `NULL` ⇒ `einval`, so a
/// host without a `/etc/services` entry for a name refuses it on both sides.
///
/// Success is `[1] ++ port_be16`, the port being the C's `sock_ntohs(srv->s_port)` written back
/// big-endian.
pub(super) fn getservbyname_reply(arg: &[u8]) -> Vec<u8> {
    if arg.len() < 2 {
        return ctl_xerror("einval");
    }
    // `n = get_int8(buf); buf++; len--; if (n >= len) error;`
    let namlen = usize::from(arg[0]);
    let mut rest = &arg[1..];
    if namlen >= rest.len() {
        return ctl_xerror("einval");
    }
    let (name, tail) = rest.split_at(namlen);
    // `n = get_int8(buf); buf++; len--; if (n > len) error;`
    let protolen = usize::from(tail[0]);
    rest = &tail[1..];
    if protolen > rest.len() {
        return ctl_xerror("einval");
    }
    let (proto, _) = rest.split_at(protolen);
    match sys::service_port(name, proto) {
        Some(port) => {
            let mut reply = ctl_ok();
            reply.extend_from_slice(&port.to_be_bytes());
            reply
        }
        None => ctl_xerror("einval"),
    }
}
/// The `INET_REQ_GETSERVBYPORT`(18) reply — the shared `inet_ctl` arm (`inet_drv.c:11525`).
///
/// The operand is `P1 P0 L1 Proto-String`: a two-byte big-endian port and the protocol length
/// byte, whose `n > len` check comes *after* `len -= 3` (`:11540`). A `NULL` lookup is
/// `ctl_error(EINVAL)`; success is `[1] ++ s_name`, the raw bytes with no NUL.
pub(super) fn getservbyport_reply(arg: &[u8]) -> Vec<u8> {
    if arg.len() < 3 {
        return ctl_xerror("einval");
    }
    let port = u16::from_be_bytes([arg[0], arg[1]]);
    let protolen = usize::from(arg[2]);
    let rest = &arg[3..];
    if protolen > rest.len() {
        return ctl_xerror("einval");
    }
    let (proto, _) = rest.split_at(protolen);
    match sys::service_name(port, proto) {
        Some(name) => {
            let mut reply = ctl_ok();
            reply.extend_from_slice(&name);
            reply
        }
        None => ctl_xerror("einval"),
    }
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

/// The `INET_REQ_GETSTAT` reply (the C's `inet_fill_stat`, `inet_drv.c:10732`-`:10792`).
///
/// The reply is the leading [`INET_REP_OK`] tag followed by one record per requested
/// `INET_STAT_*` op byte **in request order**: the op byte itself, then the counter. Ops 9
/// (`recv_oct`) and 10 (`send_oct`) carry a big-endian **64-bit** value (`put_int64`, the
/// `ARCH_64` path at `:10768`-`:10785`); every other op carries the C's `put_int32` big-endian
/// **32-bit** value (`:10788`), which truncates the running doubles (`(unsigned long)
/// desc->recv_avg`, `(unsigned long) fabs(desc->recv_dvi)`) and the peak sizes.
///
/// `send_pend` (op 8) is the C's `driver_sizeq(desc->port)` — the port's **current** output-queue
/// byte count, which the caller reads off the [`crate::DriverContext`] rather than off the
/// counter block.
///
/// An **empty** request answers the bare success tag — the C's `fill_stat` writes a record only
/// for each requested counter, so nothing requested means nothing appended (measured `[1]` on
/// both drivers' bare ports). There is **no `IS_OPEN` guard**: a socket-less port answers fully
/// zeroed counters.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] for a request containing an op byte outside 1-10: the C's
/// `inet_fill_stat` `default: return -1` (`:10786`) makes the driver's `control` callback return
/// negative, which `erlang:port_control/3` raises as the bare atom `badarg` — the whole reply is
/// discarded, so nothing partial escapes.
pub(super) fn getstat_reply(
    arg: &[u8],
    stats: &super::stat::InetStat,
    send_pend: usize,
) -> Result<Vec<u8>> {
    let mut reply = ctl_ok();
    for &op in arg {
        // The C's `put_int32` copies the low 32 bits (`sys.h:1286`), so every 32-bit record
        // truncates rather than saturating — `as u32` on an integer is that same truncation.
        let word: u32 = match op {
            1 => stats.recv_cnt as u32,
            2 => stats.recv_max as u32,
            3 => stats.recv_avg as u32,
            4 => stats.recv_dvi.abs() as u32,
            5 => stats.send_cnt as u32,
            6 => stats.send_max as u32,
            7 => stats.send_avg as u32,
            8 => send_pend as u32,
            9 => {
                reply.push(op);
                reply.extend_from_slice(&stats.recv_oct.to_be_bytes());
                continue;
            }
            10 => {
                reply.push(op);
                reply.extend_from_slice(&stats.send_oct.to_be_bytes());
                continue;
            }
            // The C's `default: return -1` — propagated as `badarg` by `port_control/3`.
            _ => return Err(error::badarg()),
        };
        reply.push(op);
        reply.extend_from_slice(&word.to_be_bytes());
    }
    Ok(reply)
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
        Ok(address) => Ok(packed_addr_reply(address)),
        Err(err) => Ok(ctl_error(err)),
    }
}

/// The success reply carrying one packed address — `[1, INET_AF_INET, port_hi, port_lo, a, b, c, d]`.
///
/// The `INET_REQ_NAME`/`INET_REQ_PEER`/`INET_REQ_GETLADDRS`/`INET_REQ_GETPADDRS` arms all answer
/// through it (`ctl_reply(INET_REP_OK, tbuf, sz, …)` with `tbuf` from `inet_get_address`,
/// `inet_drv.c:11296`, `:11361`), and so does the fake address a `INET_REQ_SETNAME`/`SETPEER`
/// installed.
pub(super) fn packed_addr_reply(address: InetAddr) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&pack_address(address.port, address.addr));
    reply
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
        Ok(address) => Ok(packed_addr_reply(address)),
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

/// The first byte of every `term_to_binary/1` blob: the external-term-format version byte
/// (`VERSION_MAGIC`, decimal `131` / `0x83`, `erts/emulator/beam/external.h`).
///
/// It is the discriminator [`split_caller_ref`] keys on: `prim_inet` wraps its caller reference
/// in `term_to_binary(Mref, [local])` (`erts/preloaded/src/prim_inet.erl:575`-`:620`), so the
/// tag's first byte is always this one.
pub(super) const ETF_VERSION_MAGIC: u8 = 131;

/// Strips the `prim_inet` **caller-reference prefix**, if `arg` carries one: the wire form is
/// `[tag_len:be16][tag_bytes][rest]`, the tag being `term_to_binary(Mref, [local])` for
/// `Mref = monitor(port, S)` (`erts/preloaded/src/prim_inet.erl:575`-`:620` for `send/3`,
/// `:650`-`:700` for `do_sendto/4`).
///
/// The C strips exactly this prefix in `init_caller`/`init_caller_iov`
/// (`erts/emulator/drivers/common/inet_drv.c:1820`-`:1890`): read the 16-bit `n`, skip it and the
/// `n` tag bytes, keeping the tag so the later `{inet_reply, S, Reply, Ref}` message can echo it
/// (`inet_reply_caller_ref`, `:2610`-`:2660`). The C only does so when `driver_caller(port)` is
/// an internal pid.
///
/// **This crate's [`Driver`](crate::Driver)/[`DriverContext`](crate::DriverContext) seam carries
/// no caller**, so the receiving side has to decide from the payload alone: a `[n:be16][n
/// bytes]` prefix whose `n` is non-zero, fits inside `arg`, and whose first tag byte is
/// [`ETF_VERSION_MAGIC`] is treated as a caller reference.
///
/// Returns the bytes **after** the prefix, or `None` when `arg` is not in that form (the caller
/// then falls back to the plain `[address][data]` grammar).
///
/// # Only a fallback — the plain form is authoritative
///
/// A plain command's first byte is an address family tag and byte 2 is the target port's **low**
/// byte, so a plain command *can* satisfy this shape (e.g. `n` = `0x0100 + port_hi` with
/// `port_lo == 0x83`). [`UdpInetDriver::verb_send`](super::udp) therefore tries the plain
/// `[address][data]` grammar **first** and only calls this function when that refuses; a genuinely
/// prefixed command always survives that order because a `prim_inet` tag's 16-bit length has a
/// zero high byte (`term_to_binary/1` of a monitor reference is ~15 bytes), never a valid address
/// tag (1/3/4).
///
/// Residual: a *prefixed* command whose tag length is ≥ 256 would have a first byte of 1/3/4 and
/// be read as plain. `prim_inet:send/3`'s reference tag is far smaller, so that cannot arise from
/// the protocol this function exists for.
pub(super) fn split_caller_ref(arg: &[u8]) -> Option<&[u8]> {
    let n = usize::from(u16::from_be_bytes([*arg.first()?, *arg.get(1)?]));
    // `init_caller`'s `*size_p < 2` / `*size_p < n` guards: a zero-length or otherwise
    // non-fitting tag carries no caller reference at all.
    if n == 0 {
        return None;
    }
    if *arg.get(2)? != ETF_VERSION_MAGIC {
        return None;
    }
    // `None` when `2 + n` overruns the argument: the C's `*size_p < n` refusal.
    arg.get(2 + n..)
}

/// The outcome of decoding the C's datagram command operand,
/// `[address][anc_len:be32][anc_data][data]` (`packet_inet_command`,
/// `erts/emulator/drivers/common/inet_drv.c:15205`-`:15230`).
pub(super) enum DatagramCommand<'a> {
    /// `anc_len == 0`: send `data` to `address` (`inet_drv.c:15220`).
    Send(InetAddr, &'a [u8]),
    /// The C's `return_einval` (`:15205`/`:15212`): fewer than four bytes follow the address, or
    /// fewer than the ancillary-length word asks for. The caller renders it as
    /// `[0] ++ "einval"` (`ctl_xerror`), exactly as the C does.
    Einval,
}

/// Decodes the C's `[address][anc_len:be32][anc_data][data]` datagram operand — the body
/// `packet_inet_command` runs once the caller-reference prefix is off
/// (`inet_drv.c:15205`-`:15230`).
///
/// The ancillary length must be **zero**: the C's non-zero arm builds an `msghdr` with the
/// caller's cmsgs and calls `sendmsg(2)` (`:15220`-`:15230`), a path this floor has no seam for,
/// so it is a typed [`error::unsupported`] rather than a send with the ancillary data silently
/// dropped or sent as payload.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] when no packed address leads the argument (the C's
/// `inet_set_faddress` failure, which it answers with `inet_reply_error_am(xerror)`), and the
/// typed `unsupported` raise for a non-zero ancillary length.
pub(super) fn split_datagram(arg: &[u8]) -> Result<DatagramCommand<'_>> {
    let (address, rest) = split_address(arg)?;
    if rest.len() < 4 {
        // `if (len < 4) goto return_einval` (`:15205`).
        return Ok(DatagramCommand::Einval);
    }
    let anc_len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    let data = &rest[4..];
    if data.len() < anc_len {
        // `if (len < anc_len) goto return_einval` (`:15212`).
        return Ok(DatagramCommand::Einval);
    }
    if anc_len != 0 {
        return Err(error::unsupported(
            "udp ancillary data (sendmsg cmsg) is not served by this driver floor",
        ));
    }
    Ok(DatagramCommand::Send(address, data))
}

/// What one `INET_REQ_SETNAME`/`INET_REQ_SETPEER` operand asks of the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum FakeAddress {
    /// `len == 0`: the C clears the pointer and answers `[1]`
    /// (`inet_drv.c:11307`/`:11392`).
    Clear,
    /// A well-formed address: store it and answer `[1]`.
    Store(InetAddr),
    /// The C's `ctl_xerror(name)` payload — `inet_set_faddress`'s own `str_einval`/
    /// `str_eafnosupport`, or the `len < 2` guard before it (`:11310`/`:11395`).
    Refuse(&'static str),
}

/// Decodes the operand of `INET_REQ_SETNAME`(19)/`INET_REQ_SETPEER`(20) — the C's
/// `inet_set_faddress(desc->sfamily, &desc->name_addr, &buf, &len)` (`inet_drv.c:11397`,
/// `:11313`).
///
/// `cached_family` is the descriptor's cached `sfamily` (`inet_drv.c:10925`) as a wire tag, with
/// `0` for "no socket yet". It matters only for the [`INET_AF_ANY`]/[`INET_AF_LOOPBACK`] short
/// form: `inet_set_faddress` switches on the **descriptor's** family there (`:4823`), so on a port
/// that never opened a socket (`sfamily == -1`) that form is `str_einval` — measured `[0]einval`
/// for `<<INET_AF_LOOPBACK,0,80>>` on a bare port, `[1]` after an `open`. The explicit
/// [`INET_AF_INET`] form overrides the family with its own tag, so it is accepted on a bare port.
///
/// A tag this driver's IPv4-only scope (`open`/`bind`/`connect` refuse IPv6 the same way) does not
/// serve is `eafnosupport` — which is also the C's answer for a tag no `case` of
/// `inet_set_faddress` matches (`:4885`).
pub(super) fn parse_fake_address(cached_family: u16, arg: &[u8]) -> FakeAddress {
    let Some((&tag, rest)) = arg.split_first() else {
        return FakeAddress::Clear;
    };
    match tag {
        INET_AF_INET => {
            let &[hi, lo, a, b, c, d] = rest else {
                return FakeAddress::Refuse("einval");
            };
            FakeAddress::Store(InetAddr {
                port: u16::from_be_bytes([hi, lo]),
                addr: [a, b, c, d],
            })
        }
        INET_AF_ANY | INET_AF_LOOPBACK => {
            let &[hi, lo] = rest else {
                return FakeAddress::Refuse("einval");
            };
            if cached_family != u16::from(INET_AF_INET) {
                return FakeAddress::Refuse("einval");
            }
            FakeAddress::Store(InetAddr {
                port: u16::from_be_bytes([hi, lo]),
                addr: if tag == INET_AF_ANY {
                    [0, 0, 0, 0]
                } else {
                    [127, 0, 0, 1]
                },
            })
        }
        _ => FakeAddress::Refuse("eafnosupport"),
    }
}

// ---------------------------------------------------------------------------
// The interface requests: `FDOPEN`(13), `GETIFLIST`(21), `IFGET`(22), `IFSET`(23),
// `GETIFADDRS`(25)
// ---------------------------------------------------------------------------
//
// All five are **shared `inet_ctl` arms** (`inet_drv.c:11067`-`:11108`), so both socket drivers
// reach the same builders here. Each builder returns the **reply** rather than an error: every
// failure the C has in these arms is a `ctl_error`/`ctl_xerror` payload, never a raise.

/// The C's `inet_ctl_fdopen` pre-adoption check (`inet_drv.c:5252`-`:5257`) — everything the
/// `INET_REQ_FDOPEN`(13) arm does *before* it takes the descriptor over.
///
/// The operand has already passed the driver-specific length/family/type ladder, so
/// `arg` is `[af, type, fd_be32]` (six bytes ⇒ `bound = 1`, `:5264`) or
/// `[af, type, fd_be32, bound_be32]`. With `bound == 0` the C adopts the number **blind** — no
/// syscall at all — which is why a garbage descriptor is `[1]` there (measured `[1]` for
/// `<<1,2,0,0,0,9,0,0,0,0>>`). With a non-zero `bound` it runs `sock_name(s, …)` first
/// (`⇒ ctl_error(sock_errno())`: `enotsock` for a descriptor that is not a socket, `ebadf` for a
/// closed number) and only then compares the family (`⇒ ctl_error(EINVAL)`).
///
/// `Ok(())` is "the C would adopt"; `Err(reply)` is the refusal reply the arm must answer.
///
/// # The adoption itself
///
/// This floor does **not** adopt: `desc->prebound = 1` (`:5266`) makes the C leave a handed-over
/// descriptor alone for the port's whole life, whereas [`crate::Port`]'s `stop`/`Drop` closes
/// whatever the driver owns — so adopting would close a descriptor the caller still owns. The
/// successful form is therefore the typed "not served" gap, not a fabricated `[1]`.
pub(super) fn fdopen_precheck(arg: &[u8]) -> std::result::Result<(), Vec<u8>> {
    const FIXED_LEN: usize = 6;
    let (fd_at, bound) = match arg.len() {
        // `if (len == 6) bound = 1; else bound = get_int32(buf+2+4);` (`:5264`-`:5265`).
        FIXED_LEN => (&arg[2..], true),
        10 => (&arg[2..FIXED_LEN], arg[6..10] != [0, 0, 0, 0]),
        _ => return Err(ctl_xerror("einval")),
    };
    // `s = (SOCKET)get_int32(buf+2)` — the C's descriptor is a 32-bit big-endian number.
    let [a, b, c, d] = [fd_at[0], fd_at[1], fd_at[2], fd_at[3]];
    let fd = RawFd::from_be_bytes([a, b, c, d]);
    if !bound {
        return Ok(());
    }
    match sys::socket_family(fd) {
        Err(err) => Err(ctl_error(err)),
        // `if (name.sa.sa_family != domain) return ctl_error(EINVAL)`: both ladders above refuse
        // every family tag but `INET_AF_INET`, so `domain` is always `AF_INET`.
        Ok(family) if libc::c_int::from(family) != libc::AF_INET => Err(ctl_xerror("einval")),
        Ok(_) => Ok(()),
    }
}

/// The C's `IFGET_FLAGS` (`inet_drv.c:5476`): the kernel's `IFF_*` bits reduced into the driver's
/// own [`INET_IFF_*`](INET_IFF_UP) vocabulary, which is what an `IFGET`/`GETIFADDRS` record
/// carries.
///
/// `IFF_RUNNING` has no `INET_IFF_*` bit of its own — the C **emulates** it from `IFF_UP`
/// (`:5481`, "emulate running ?"), which is why a loopback interface reports `0x15`
/// (`UP|LOOPBACK|RUNNING`, measured on this host).
pub(super) fn ifget_flags(cflags: libc::c_int) -> u32 {
    let mut flags = 0u32;
    if cflags & libc::IFF_UP != 0 {
        flags |= INET_IFF_UP | INET_IFF_RUNNING;
    }
    if cflags & libc::IFF_BROADCAST != 0 {
        flags |= INET_IFF_BROADCAST;
    }
    if cflags & libc::IFF_LOOPBACK != 0 {
        flags |= INET_IFF_LOOPBACK;
    }
    if cflags & libc::IFF_POINTOPOINT != 0 {
        flags |= INET_IFF_POINTTOPOINT;
    }
    if cflags & libc::IFF_MULTICAST != 0 {
        flags |= INET_IFF_MULTICAST;
    }
    flags
}

/// The reply of one of the four interface requests, whichever driver asked — the
/// `INET_REQ_GETIFLIST`/`IFGET`/`IFSET`/`GETIFADDRS` arms of `inet_ctl` (`inet_drv.c:11067`,
/// `:11078`, `:11089`, `:11100`).
///
/// Each of the four opens with `if (!IS_OPEN(desc)) return ctl_xerror(EXBADPORT)` — `socket` is
/// the driver's descriptor, so `None` is that refusal (measured `[0]exbadport` for all four on a
/// socket-less `udp_inet`/`tcp_inet` port). `op` is known by the caller to be one of the four; a
/// different opcode answers the `inet_ctl` `default:` arm's `[0]exbadport`.
pub(super) fn interface_reply(op: Opcode, socket: Option<RawFd>, arg: &[u8]) -> Vec<u8> {
    let Some(fd) = socket else {
        return ctl_xerror("exbadport");
    };
    match op {
        Opcode::Getiflist => getiflist_reply(fd),
        Opcode::Ifget => ifget_reply(fd, arg),
        Opcode::Ifset => ifset_reply(arg),
        Opcode::Getifaddrs => getifaddrs_reply(),
        _ => ctl_xerror("exbadport"),
    }
}

/// The `INET_REQ_GETIFLIST`(21) reply — `inet_ctl_getiflist` (`inet_drv.c:5671`).
///
/// `[1]` followed by every `AF_INET`/`AF_INET6` interface name in `SIOCGIFCONF` order, each
/// NUL-terminated (`:5694`-`:5700`: `strncpy(sp, ifr_name, IFNAMSIZ); sp[IFNAMSIZ] = '\0';
/// sp += strlen(sp), ++sp;`). Measured on this host as
/// `[1] ++ "lo\0enp44s0\0wlp46s0f0\0nb-personal\0"` — an interface without an IPv4 address is
/// absent, which is the kernel's `SIOCGIFCONF` answer and not a filter here.
///
/// A failing `SIOCGIFCONF` walk is `ctl_error(sock_errno())` (`:5682`), i.e. an `[0]`-tagged
/// errno reply.
pub(super) fn getiflist_reply(fd: RawFd) -> Vec<u8> {
    match sys::ifconf_names(fd) {
        Ok(names) => {
            let mut reply = ctl_ok();
            for name in names {
                reply.extend_from_slice(&name);
                reply.push(0);
            }
            reply
        }
        Err(err) => ctl_error(err),
    }
}

/// The C's `sbuf[BUFSIZ]` reply buffer of `inet_ctl_ifget` (`inet_drv.c:5826`).
///
/// `BUFSIZ` is a libc **header** constant (8192 with glibc, 1024 with musl), not one the `libc`
/// crate exposes on every target this crate builds for, so it is spelled here with the glibc value
/// the oracle was measured against. A record that does not fit is the arm's `goto error`
/// (`buf_check`, `:5470`) ⇒ the whole reply becomes `[0]einval`, previous records included.
const IFGET_REPLY_CAP: usize = 8192;

/// The `INET_REQ_IFGET`(22) reply — `inet_ctl_ifget` (`inet_drv.c:5821`).
///
/// The operand is `Name-Length(1) Name Option-Code(1)*`. The reply is `[1]` followed by one
/// `[code][payload]` record per **served** option code, in operand order; a failing `SIOCGIF*`
/// emits **no** record for `ADDR`/`BROADADDR`/`DSTADDR`/`NETMASK`/`MTU`/`HWADDR` (`break` out of
/// the `switch`, `:5845`, `:5915`, …) but `FLAGS` still emits one, reporting `0` (`:5955`:
/// `if (ioctl(SIOCGIFFLAGS) < 0) flags = 0; else flags = ifreq.ifr_flags;`). Measured on this host
/// with the interface `nonexist0`: `<<9,"nonexist0",6>>` ⇒ `[1,6,0,0,0,0]`.
///
/// The name guard is the C's `if ((len == 0) || ((namlen = get_int8(buf)) > len)) goto error`
/// (`:5831`), plus one determinisation: a name whose declared length **fills** the operand
/// (`namlen == len`) makes the C's unsigned `len -= namlen+1` underflow and the `while (len--)`
/// walk whatever memory follows the operand. The measured oracle answer for that operand
/// (`<<1>>`) is `[0]einval`, and this floor answers that deterministically instead of reading past
/// the operand.
pub(super) fn ifget_reply(fd: RawFd, arg: &[u8]) -> Vec<u8> {
    let Some((name, options)) = split_ifreq_operand(arg) else {
        return ctl_xerror("einval");
    };
    let mut req = sys::IfReq::named(name);
    let mut reply = ctl_ok();
    for &option in options {
        let record = match option {
            INET_IFOPT_ADDR => option_record(fd, &mut req, sys::IfReqIo::Addr),
            INET_IFOPT_BROADADDR => option_record(fd, &mut req, sys::IfReqIo::BroadAddr),
            INET_IFOPT_DSTADDR => option_record(fd, &mut req, sys::IfReqIo::DstAddr),
            INET_IFOPT_NETMASK => option_record(fd, &mut req, sys::IfReqIo::Netmask),
            // `put_int32(n, sptr)` over the C's `int ifreq.ifr_mtu` (`:5952`).
            INET_IFOPT_MTU => match sys::ifreq_ioctl(fd, &mut req, sys::IfReqIo::Mtu) {
                Ok(()) => with_option(INET_IFOPT_MTU, &(req.mtu() as u32).to_be_bytes()),
                Err(_) => None,
            },
            // `SIOCGIFHWADDR` writes `ifr_hwaddr.sa_data`, and the C copies `IFHWADDRLEN` raw bytes
            // of it behind a big-endian length (`:5861`-`:5870`) — a fixed six, not the encoded
            // length `sockaddr_to_buf` would produce for the same member.
            INET_IFOPT_HWADDR => match sys::ifreq_ioctl(fd, &mut req, sys::IfReqIo::HwAddr) {
                Ok(()) => {
                    let mut payload = (sys::HWADDR_LEN as u16).to_be_bytes().to_vec();
                    payload.extend_from_slice(&req.hwaddr());
                    with_option(INET_IFOPT_HWADDR, &payload)
                }
                Err(_) => None,
            },
            // The one arm whose failure is still a record: `flags = 0` (`:5955`).
            INET_IFOPT_FLAGS => {
                let flags = match sys::ifreq_ioctl(fd, &mut req, sys::IfReqIo::Flags) {
                    Ok(()) => req.flags(),
                    Err(_) => 0,
                };
                with_option(INET_IFOPT_FLAGS, &ifget_flags(flags).to_be_bytes())
            }
            // The C's `default: goto error` (`:5975`).
            _ => return ctl_xerror("einval"),
        };
        match record {
            Some(record) if IFGET_REPLY_CAP.saturating_sub(reply.len()) >= record.len() => {
                reply.extend_from_slice(&record)
            }
            // `buf_check` failed, or the syscall arm `break`ed without a record: only the first
            // case is an error, and the C's `break` leaves the reply untouched.
            Some(_) => return ctl_xerror("einval"),
            None => {}
        }
    }
    reply
}

/// One `[code][sockaddr]` record of an `IFGET` reply, or `None` when the ioctl failed.
///
/// Every address-shaped arm is `buf_check(sptr, s_end, 1); *sptr++ = code;` followed by
/// `sockaddr_to_buf(&ifreq.ifr_<member>, sptr, s_end)` (`:5843`-`:5945`), so the payload is
/// [`sys::SockAddr`]'s encoding — `[INET_AF_INET, a, b, c, d]`, `[INET_AF_INET6, 16 bytes]` or the
/// big-endian length plus hardware bytes of a link-layer address. An address that encodes to
/// nothing is the C's `P_ == ptr` ⇒ the option byte is backed out again.
fn option_record(fd: RawFd, req: &mut sys::IfReq, which: sys::IfReqIo) -> Option<Vec<u8>> {
    match sys::ifreq_ioctl(fd, req, which) {
        Ok(()) => with_option(option_code(which), &req.addr().body),
        Err(_) => None,
    }
}

/// The option byte an [`sys::IfReqIo`] reports under (`INET_IFOPT_*`, `inet_drv.c:952`-`:958`).
fn option_code(which: sys::IfReqIo) -> u8 {
    match which {
        sys::IfReqIo::Addr => INET_IFOPT_ADDR,
        sys::IfReqIo::BroadAddr => INET_IFOPT_BROADADDR,
        sys::IfReqIo::DstAddr => INET_IFOPT_DSTADDR,
        sys::IfReqIo::Netmask => INET_IFOPT_NETMASK,
        sys::IfReqIo::Mtu => INET_IFOPT_MTU,
        sys::IfReqIo::Flags => INET_IFOPT_FLAGS,
        sys::IfReqIo::HwAddr => INET_IFOPT_HWADDR,
    }
}

/// `[code][payload]`, or `None` when the payload is empty (the C's backed-out option byte).
fn with_option(code: u8, payload: &[u8]) -> Option<Vec<u8>> {
    if payload.is_empty() {
        return None;
    }
    let mut record = Vec::with_capacity(1 + payload.len());
    record.push(code);
    record.extend_from_slice(payload);
    Some(record)
}

/// The `INET_REQ_IFSET`(23) reply — `inet_ctl_ifset` (`inet_drv.c:6017`).
///
/// The operand is `Name-Length(1) Name Option(1)*` with each option's own value inline:
/// `ADDR`/`BROADADDR`/`DSTADDR`/`NETMASK` take one `buf_to_sockaddr` record, `HWADDR` a big-endian
/// 16-bit length plus that many bytes, `MTU` and `FLAGS` a bare big-endian 32-bit value. Anything
/// truncated or unknown is the C's `goto error` ⇒ `[0]einval`; a fully parsed operand is `[1]`,
/// **regardless of whether the interface exists** (measured `<<2,"lo",4,0,0,0,0>>` ⇒ `[1]`).
///
/// # The writes are not performed
///
/// The C issues one `SIOCSIFADDR`/`SIOCSIFBRDADDR`/`SIOCSIFDSTADDR`/`SIOCSIFNETMASK`/
/// `SIOCSIFHWADDR`/`SIOCSIFMTU`/`SIOCSIFFLAGS` per option and **discards every result** (`(void)
/// ioctl(…)`, `:6055`, `:6075`, `:6086`, `:6094`, `:6110`, `:6127`), so the reply cannot report a
/// privileged failure and the oracle answers `[1]` for the same operands this floor does. What this
/// floor does **not** do is the `(void)`-discarded write itself: an interface's address, netmask,
/// broadcast/destination address, MTU, hardware address or flags are left exactly as they were, so
/// a caller that expected the interface to change sees nothing change. Reproducing the writes
/// needs no new seam — [`sys::ifreq_ioctl`] takes the `SIOCSIF*` request numbers — but a test that
/// exercised them would mutate the host's own network configuration, so the parse-only form is
/// what ships and the divergence is stated here rather than hidden.
pub(super) fn ifset_reply(arg: &[u8]) -> Vec<u8> {
    let options = match split_ifreq_operand(arg) {
        Some((_, options)) => options,
        // A name that overruns the operand is the C's explicit `namlen > len` ⇒ `goto error`; one
        // that exactly fills it leaves `buf` past `b_end`, so the value loop runs zero times.
        None if arg.is_empty() || usize::from(arg[0]) > arg.len() => return ctl_xerror("einval"),
        None => &[],
    };
    let mut at = 0usize;
    while at < options.len() {
        let option = options[at];
        at += 1;
        match option {
            INET_IFOPT_ADDR | INET_IFOPT_BROADADDR | INET_IFOPT_DSTADDR | INET_IFOPT_NETMASK => {
                match buf_to_sockaddr(&options, at) {
                    Some(next) => at = next,
                    None => return ctl_xerror("einval"),
                }
            }
            // `buf_check(buf, b_end, 2)` for the length, then `buf_check(buf, b_end, hwalen)`.
            INET_IFOPT_HWADDR => match options.get(at..at + 2) {
                Some([hi, lo]) => {
                    let hwalen = usize::from(u16::from_be_bytes([*hi, *lo]));
                    if options.len().saturating_sub(at + 2) < hwalen {
                        return ctl_xerror("einval");
                    }
                    at += 2 + hwalen;
                }
                _ => return ctl_xerror("einval"),
            },
            // A bare four-byte value (`buf_check(buf, b_end, 4)`, `:6105`, `:6117`).
            INET_IFOPT_MTU | INET_IFOPT_FLAGS => {
                if options.len().saturating_sub(at) < 4 {
                    return ctl_xerror("einval");
                }
                at += 4;
            }
            // The C's `default: goto error` (`:6134`).
            _ => return ctl_xerror("einval"),
        }
    }
    ctl_ok()
}

/// The C's `buf_to_sockaddr` (`inet_drv.c:5440`) over an `IFSET` operand: the offset just past the
/// record that starts at `at`, or `None` for the C's `NULL` (an unknown family tag or a truncated
/// record).
fn buf_to_sockaddr(arg: &[u8], at: usize) -> Option<usize> {
    let (&tag, rest) = arg.get(at..)?.split_first()?;
    let consumed = match tag {
        INET_AF_INET => 4,
        INET_AF_INET6 => 16,
        _ => return None,
    };
    if rest.len() < consumed {
        return None;
    }
    Some(at + 1 + consumed)
}

/// Splits the `IFGET`/`IFSET` operand into its interface-name and option halves.
///
/// `None` when the declared name does not fit the operand (`namlen + 1 > len`, the C's `namlen >
/// len` guard plus the `namlen == len` case the C underflows on); otherwise the name bytes
/// (`at most IFNAMSIZ - 1`, `:5834`) and the option bytes that follow.
fn split_ifreq_operand(arg: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&namlen, rest) = arg.split_first()?;
    let namlen = usize::from(namlen);
    let name_end = namlen + 1;
    if name_end > arg.len() {
        return None;
    }
    Some((&rest[..namlen], &arg[name_end..]))
}

/// The `INET_REQ_GETIFADDRS`(25) reply — `inet_ctl_getifaddrs` (`inet_drv.c:6611`).
///
/// `[1]` followed by the `getifaddrs(3)` list in the libc's own order, one record stream per node:
/// the name (latin-1 → UTF-8, `:6629`/`:6616`), a NUL, the `FLAGS` record, then — only when the
/// node has an address — the `ADDR`/`NETMASK` records of an `AF_INET`/`AF_INET6` node or the
/// `HWADDR` record of a link-layer node, and finally a NUL. So the interface name is what opens a
/// record stream and the trailing NUL byte is what closes it, which is why an interface with one
/// address per family appears three times on a dual-stack host (measured: the `AF_PACKET` pass
/// first, then `AF_INET`, then `AF_INET6`).
///
/// A link-layer address is only reported when [`sys::entry_addr_need`] says the encoding is longer
/// than three bytes (`:6769`, the C's `need > 3`), which is what makes a loopback `HWADDR` record
/// appear at all (its six zero bytes) while skipping a two-byte one.
///
/// A failing `getifaddrs(3)` is `ctl_error(save_errno)` (`:6649`), an `[0]`-tagged errno reply.
pub(super) fn getifaddrs_reply() -> Vec<u8> {
    match sys::ifaddrs_entries() {
        Ok(entries) => {
            let mut reply = ctl_ok();
            for entry in &entries {
                reply.extend_from_slice(&latin1_to_utf8(&entry.name));
                reply.push(0);
                reply.push(INET_IFOPT_FLAGS);
                reply.extend_from_slice(&ifget_flags(entry.flags as libc::c_int).to_be_bytes());
                if entry.family != sys::NO_ADDRESS {
                    if entry.family == libc::AF_INET || entry.family == libc::AF_INET6 {
                        push_ifopt(&mut reply, INET_IFOPT_ADDR, &entry.addr);
                        if let Some(netmask) = &entry.netmask {
                            push_ifopt(&mut reply, INET_IFOPT_NETMASK, netmask);
                        }
                        // `ifa_dstaddr` and `ifa_broadaddr` are one union member, so which record
                        // it becomes is decided by the node's flags (`:6703`-`:6710`); with
                        // neither flag set the member is not reported at all.
                        if let Some(ifu) = &entry.ifu {
                            if entry.flags & libc::IFF_POINTOPOINT as u32 != 0 {
                                push_ifopt(&mut reply, INET_IFOPT_DSTADDR, ifu);
                            } else if entry.flags & libc::IFF_BROADCAST as u32 != 0 {
                                push_ifopt(&mut reply, INET_IFOPT_BROADADDR, ifu);
                            }
                        }
                    } else if sys::is_link_layer(entry.family) && sys::entry_addr_need(entry) > 3 {
                        push_ifopt(&mut reply, INET_IFOPT_HWADDR, &entry.addr);
                    }
                }
                reply.push(0);
            }
            reply
        }
        Err(err) => ctl_error(err),
    }
}

/// `SOCKADDR_TO_BUF` (`inet_drv.c:6636`) — `[code][payload]`, or nothing when the address encodes
/// to no bytes (the C's `if (P_ == buf_p) buf_p--`).
fn push_ifopt(reply: &mut Vec<u8>, code: u8, payload: &[u8]) {
    if payload.is_empty() {
        return;
    }
    reply.push(code);
    reply.extend_from_slice(payload);
}

/// The C's `utf8_encode` (`inet_drv.c:6617`): a latin-1 interface name widened to UTF-8, so a byte
/// in `0x80..=0xFF` becomes two bytes and everything else is copied.
fn latin1_to_utf8(name: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(name.len());
    for &byte in name {
        if byte & 0x80 == 0 {
            encoded.push(byte);
        } else {
            encoded.push(0xc0 | (byte >> 6));
            encoded.push(0x80 | (byte & 0x3f));
        }
    }
    encoded
}
