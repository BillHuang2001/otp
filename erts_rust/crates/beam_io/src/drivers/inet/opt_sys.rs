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

//! The **syscall-backed** option family of the `setopts`/`getopts` verbs.
//!
//! Every code here reaches the kernel through `setsockopt(2)`/`getsockopt(2)`. [`super::opt`] owns
//! the wire walk; this module owns the per-code `(level, name)` resolution and the four codes whose
//! value is not a plain `int`:
//!
//! * `{linger, {OnOff, Seconds}}` — a `struct linger` (`getsockopt`/`setsockopt`, `SO_LINGER`);
//! * `bind_to_device` — a NUL-terminated device name of at most `IFNAMSIZ` (`SO_BINDTODEVICE`);
//! * `raw` — a caller-nominated `(level, name, bytes)` triple (`INET_OPT_RAW`);
//! * `pktoptions` and `multicast_if` — the two family-selected reads.
//!
//! # The `propagate` split
//!
//! The C leaves `propagate = 0` for most codes, so a failed `setsockopt(2)` is **not** reported and
//! the reply stays `[1]` (`inet_set_opts`, `erts/emulator/drivers/common/inet_drv.c:7923`-`:7942` —
//! "not all errors can be propagated for BC reasons"). Exactly nine codes set `propagate = 1` —
//! `priority`(`8`), `tos`(`9`), `ipv6_v6only`(`16`), `tclass`(`41`), `bind_to_device`(`42`),
//! `recvtos`(`43`), `recvtclass`(`44`), `ttl`(`46`) and `recvttl`(`47`) — and there a failed
//! `setsockopt` replaces the **whole** reply with `[0]einval`. That is also why a `setopts` of one
//! of those nine on a port with **no** socket (where the C's descriptor is invalid) answers
//! `[0]einval`, while the same record on any other code answers `[1]`.
//!
//! # The get switch's three misses
//!
//! A failed `getsockopt(2)` is the C's `TRUNCATE_TO(0, ptr); continue;`: **that record only** is
//! dropped, the records already produced are kept and later codes are still tried. Two codes
//! instead answer a literal `0` on a platform without the option (`exclusiveaddruse`, and every
//! `SO_*` this kernel serves), and a code with no `case` at all — here `add_membership`(`14`) and
//! `drop_membership`(`15`), which are set-only — is the C's `default: RETURN_ERROR()`, i.e. the
//! whole reply `[0]einval`.
//!
//! # A deliberate non-mirror: `multicast_if` (`11`)
//!
//! The C reserves nine reply bytes but only advances its length by five, so the value slot is
//! **uninitialized memory** and the interface address lands past the reported length
//! (`inet_fill_opts`, `:9494`-`:9520`). Measured oracle replies therefore vary run to run. This
//! floor answers the deterministic fresh-buffer form — `[opt][0,0,0,0][INET_AF_INET]` — and
//! documents it rather than reproducing uninitialized bytes.
//!
//! Linux option numbers below are the `uapi/asm-generic/socket.h`, `netinet/in.h` and
//! `netinet/tcp.h` values, spelled locally so the module does not depend on which of them the
//! `libc` crate happens to export.

use std::os::unix::io::RawFd;

use beam_common::Result;

use super::super::wire::INET_AF_INET;
use super::super::sys;
use super::{
    OptScope, SocketOptions, Step, INET_OPT_ADD_MEMBERSHIP, INET_OPT_BIND_TO_DEVICE,
    INET_OPT_BROADCAST, INET_OPT_DONTROUTE, INET_OPT_DROP_MEMBERSHIP, INET_OPT_EXCLUSIVEADDRUSE,
    INET_OPT_KEEPALIVE, INET_OPT_KEEPIDLE, INET_OPT_KEEPCNT, INET_OPT_KEEPINTVL, INET_OPT_LINGER,
    INET_OPT_MULTICAST_IF, INET_OPT_MULTICAST_LOOP, INET_OPT_MULTICAST_TTL, INET_OPT_NOPUSH,
    INET_OPT_OOBINLINE, INET_OPT_PKTOPTIONS, INET_OPT_PRIORITY, INET_OPT_RAW, INET_OPT_RCVBUF,
    INET_OPT_RECVTCLASS, INET_OPT_RECVTOS, INET_OPT_RECVTTL, INET_OPT_REUSEADDR,
    INET_OPT_REUSEPORT, INET_OPT_REUSEPORT_LB, INET_OPT_SNDBUF, INET_OPT_TCLASS, INET_OPT_TOS,
    INET_OPT_TTL, INET_OPT_USER_TIMEOUT, INET_OPT_IPV6_V6ONLY, RECORD, TCP_OPT_NODELAY,
};

/// `SO_DONTROUTE` (`uapi/asm-generic/socket.h`).
const SO_DONTROUTE: libc::c_int = 5;

/// `SO_OOBINLINE` (`uapi/asm-generic/socket.h`).
const SO_OOBINLINE: libc::c_int = 10;

/// `SO_PRIORITY` (`uapi/asm-generic/socket.h`).
const SO_PRIORITY: libc::c_int = 12;

/// `SO_REUSEPORT` (`uapi/asm-generic/socket.h`) — on Linux this is the load-balancing form, which
/// is why `reuseport` (`17`) and `reuseport_lb` (`18`) resolve to the same option.
const SO_REUSEPORT: libc::c_int = 15;

/// `SO_BINDTODEVICE` (`uapi/asm-generic/socket.h`).
const SO_BINDTODEVICE: libc::c_int = 25;

/// `IP_TOS` (`netinet/in.h`).
const IP_TOS: libc::c_int = 1;

/// `IP_TTL` (`netinet/in.h`).
const IP_TTL: libc::c_int = 2;

/// `IP_PKTOPTIONS` (`netinet/in.h`).
const IP_PKTOPTIONS: libc::c_int = 9;

/// `IP_RECVTTL` (`netinet/in.h`).
const IP_RECVTTL: libc::c_int = 12;

/// `IP_RECVTOS` (`netinet/in.h`).
const IP_RECVTOS: libc::c_int = 13;

/// `IP_MULTICAST_IF` (`netinet/in.h`).
const IP_MULTICAST_IF: libc::c_int = 32;

/// `IP_MULTICAST_TTL` (`netinet/in.h`).
const IP_MULTICAST_TTL: libc::c_int = 33;

/// `IP_MULTICAST_LOOP` (`netinet/in.h`).
const IP_MULTICAST_LOOP: libc::c_int = 34;

/// `IP_ADD_MEMBERSHIP` (`netinet/in.h`).
const IP_ADD_MEMBERSHIP: libc::c_int = 35;

/// `IP_DROP_MEMBERSHIP` (`netinet/in.h`).
const IP_DROP_MEMBERSHIP: libc::c_int = 36;

/// `IPV6_PKTOPTIONS`, the C's `IPV6_2292PKTOPTIONS` spelling (`inet_drv.c:739`-`:740`).
const IPV6_PKTOPTIONS: libc::c_int = 6;

/// `IPV6_V6ONLY` (`netinet/in.h`).
const IPV6_V6ONLY: libc::c_int = 26;

/// `IPV6_RECVTCLASS` (`netinet/in.h`).
const IPV6_RECVTCLASS: libc::c_int = 66;

/// `IPV6_TCLASS` (`netinet/in.h`).
const IPV6_TCLASS: libc::c_int = 67;

/// `TCP_NODELAY` (`netinet/tcp.h`).
const TCP_NODELAY: libc::c_int = 1;

/// `TCP_CORK` (`netinet/tcp.h`) — the C's `INET_TCP_NOPUSH`, which is what `nopush` (`48`) means
/// on Linux.
const TCP_CORK: libc::c_int = 3;

/// `TCP_KEEPIDLE` (`netinet/tcp.h`).
const TCP_KEEPIDLE: libc::c_int = 4;

/// `TCP_KEEPINTVL` (`netinet/tcp.h`).
const TCP_KEEPINTVL: libc::c_int = 5;

/// `TCP_KEEPCNT` (`netinet/tcp.h`).
const TCP_KEEPCNT: libc::c_int = 6;

/// `TCP_USER_TIMEOUT` (`netinet/tcp.h`).
const TCP_USER_TIMEOUT: libc::c_int = 18;

/// `IFNAMSIZ` — the length of the device-name buffer `SO_BINDTODEVICE` takes.
const IFNAMSIZ: usize = 16;

/// The largest `INET_OPT_RAW` reply payload (`INET_MAX_OPT_BUFFER`, `inet_drv.c:1063`).
const INET_MAX_OPT_BUFFER: u32 = 64 * 1024;

/// The `int`-valued `(level, name)` pair a code selects, or `None` for a code with its own shape.
fn int_option(code: u8) -> Option<(libc::c_int, libc::c_int)> {
    let pair = match code {
        INET_OPT_REUSEADDR => (libc::SOL_SOCKET, libc::SO_REUSEADDR),
        INET_OPT_KEEPALIVE => (libc::SOL_SOCKET, libc::SO_KEEPALIVE),
        INET_OPT_DONTROUTE => (libc::SOL_SOCKET, SO_DONTROUTE),
        INET_OPT_BROADCAST => (libc::SOL_SOCKET, libc::SO_BROADCAST),
        INET_OPT_OOBINLINE => (libc::SOL_SOCKET, SO_OOBINLINE),
        INET_OPT_SNDBUF => (libc::SOL_SOCKET, libc::SO_SNDBUF),
        INET_OPT_RCVBUF => (libc::SOL_SOCKET, libc::SO_RCVBUF),
        INET_OPT_PRIORITY => (libc::SOL_SOCKET, SO_PRIORITY),
        // On Linux `SO_REUSEPORT` *is* the load-balancing form.
        INET_OPT_REUSEPORT | INET_OPT_REUSEPORT_LB => (libc::SOL_SOCKET, SO_REUSEPORT),
        INET_OPT_TOS => (libc::IPPROTO_IP, IP_TOS),
        INET_OPT_TTL => (libc::IPPROTO_IP, IP_TTL),
        INET_OPT_RECVTOS => (libc::IPPROTO_IP, IP_RECVTOS),
        INET_OPT_RECVTTL => (libc::IPPROTO_IP, IP_RECVTTL),
        INET_OPT_MULTICAST_TTL => (libc::IPPROTO_IP, IP_MULTICAST_TTL),
        INET_OPT_MULTICAST_LOOP => (libc::IPPROTO_IP, IP_MULTICAST_LOOP),
        INET_OPT_TCLASS => (libc::IPPROTO_IPV6, IPV6_TCLASS),
        INET_OPT_RECVTCLASS => (libc::IPPROTO_IPV6, IPV6_RECVTCLASS),
        INET_OPT_IPV6_V6ONLY => (libc::IPPROTO_IPV6, IPV6_V6ONLY),
        TCP_OPT_NODELAY => (libc::IPPROTO_TCP, TCP_NODELAY),
        INET_OPT_NOPUSH => (libc::IPPROTO_TCP, TCP_CORK),
        INET_OPT_KEEPCNT => (libc::IPPROTO_TCP, TCP_KEEPCNT),
        INET_OPT_KEEPIDLE => (libc::IPPROTO_TCP, TCP_KEEPIDLE),
        INET_OPT_KEEPINTVL => (libc::IPPROTO_TCP, TCP_KEEPINTVL),
        INET_OPT_USER_TIMEOUT => (libc::IPPROTO_TCP, TCP_USER_TIMEOUT),
        _ => return None,
    };
    Some(pair)
}

/// Whether a failed `setsockopt(2)` for `code` is reported as the whole reply `[0]einval` — the
/// C's `propagate = 1` set (`inet_drv.c:7363`-`:7900`).
fn propagates(code: u8) -> bool {
    matches!(
        code,
        INET_OPT_PRIORITY
            | INET_OPT_TOS
            | INET_OPT_IPV6_V6ONLY
            | INET_OPT_TCLASS
            | INET_OPT_BIND_TO_DEVICE
            | INET_OPT_RECVTOS
            | INET_OPT_RECVTCLASS
            | INET_OPT_TTL
            | INET_OPT_RECVTTL
    )
}

/// Appends one `[opt, val_be32]` record.
fn push_record(reply: &mut Vec<u8>, code: u8, value: i32) {
    reply.push(code);
    reply.extend_from_slice(&value.to_be_bytes());
}

/// Applies one `setsockopt` whose failure follows the code's `propagate` rule: a failure of a
/// propagating code is `Step::Refused` (the whole reply becomes `[0]einval`), any other failure is
/// deliberately discarded (the C's `propagate = 0`, `inet_drv.c:7923`-`:7942`).
fn apply(fd: Option<RawFd>, code: u8, set: impl FnOnce(RawFd) -> Result<()>) -> Step {
    match fd {
        // With no socket the C's descriptor is invalid: every `setsockopt` fails, which a
        // propagating code turns into `[0]einval` and any other code ignores.
        None if propagates(code) => Step::Refused,
        None => Step::Done(RECORD),
        Some(fd) => {
            if set(fd).is_err() && propagates(code) {
                Step::Refused
            } else {
                Step::Done(RECORD)
            }
        }
    }
}

/// The `setopts` half of the syscall family, or `None` when `code` is not one of its codes.
///
/// `rest` is the request cursor **at** the code byte, so the codes that carry extra payload read
/// it from `rest[RECORD..]` and answer the total they consumed.
pub(super) fn set(
    code: u8,
    value: i32,
    options: &mut SocketOptions,
    scope: OptScope,
    rest: &[u8],
    fd: Option<RawFd>,
) -> Option<Result<Step>> {
    let step = match code {
        // Non-Windows: `continue` — accepted and ignored (`inet_drv.c:7208`-`:7219`).
        INET_OPT_EXCLUSIVEADDRUSE => Step::Done(RECORD),
        // `struct linger`: `l_onoff` is the record's value, `l_linger` the next four bytes
        // (`:7363`-`:7388`); a payload shorter than that is the C's `return -1`.
        INET_OPT_LINGER => {
            let Some(tail) = rest.get(RECORD..RECORD + 4) else {
                return Some(Ok(Step::Refused));
            };
            let linger = i32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]);
            let step = apply(fd, code, |fd| sys::set_socket_linger(fd, value, linger));
            match step {
                Step::Done(_) => Step::Done(RECORD + 4),
                other => other,
            }
        }
        // `ival` is the device name's length; `len < ival`, a negative length and `ival > IFNAMSIZ`
        // are all `return -1`, and the `setsockopt` propagates (`:7848`-`:7868`).
        INET_OPT_BIND_TO_DEVICE => {
            if value < 0 || value as usize > IFNAMSIZ || rest.len() < RECORD + value as usize {
                Step::Refused
            } else {
                let mut name = [0u8; IFNAMSIZ];
                name[..value as usize].copy_from_slice(&rest[RECORD..RECORD + value as usize]);
                let step = apply(fd, code, |fd| {
                    sys::set_socket_bytes(fd, libc::SOL_SOCKET, SO_BINDTODEVICE, &name)
                });
                match step {
                    Step::Done(_) => Step::Done(RECORD + value as usize),
                    other => other,
                }
            }
        }
        // `[ival = proto][type][arg_sz][arg bytes]` (`:7806`-`:7824`); a short payload is
        // `return -1`. The `setsockopt` itself does not propagate.
        INET_OPT_RAW => {
            if rest.len() < RECORD + 8 {
                Step::Refused
            } else {
                let proto = value;
                let arg_type = i32::from_be_bytes([
                    rest[RECORD],
                    rest[RECORD + 1],
                    rest[RECORD + 2],
                    rest[RECORD + 3],
                ]);
                let arg_sz = i32::from_be_bytes([
                    rest[RECORD + 4],
                    rest[RECORD + 5],
                    rest[RECORD + 6],
                    rest[RECORD + 7],
                ]);
                if arg_sz < 0 || rest.len() - RECORD - 8 < arg_sz as usize {
                    Step::Refused
                } else {
                    let data = &rest[RECORD + 8..RECORD + 8 + arg_sz as usize];
                    let step = apply(fd, code, |fd| {
                        sys::set_socket_bytes(fd, proto, arg_type, data)
                    });
                    match step {
                        Step::Done(_) => Step::Done(RECORD + 8 + arg_sz as usize),
                        other => other,
                    }
                }
            }
        }
        // `if (desc->sfamily == AF_INET) { ival = sock_htonl(ival); type = IP_MULTICAST_IF; } else
        // return -1;` (`:7586`-`:7614`).
        INET_OPT_MULTICAST_IF => {
            if scope.family != u16::from(INET_AF_INET) {
                Step::Refused
            } else {
                let ifindex = value.to_be();
                apply(fd, code, |fd| {
                    sys::set_socket_int_option(fd, libc::IPPROTO_IP, IP_MULTICAST_IF, ifindex)
                })
            }
        }
        // `[domain][ifindex][multiaddr][address]` → a `struct ip_mreqn` (`:7616`-`:7800`). The
        // domain must be `INET_AF_INET` *and* the descriptor's family must be AF_INET; a payload
        // that does not carry all three words is refused here (the C reads them past the request,
        // a documented non-mirror).
        INET_OPT_ADD_MEMBERSHIP | INET_OPT_DROP_MEMBERSHIP => {
            if value != i32::from(INET_AF_INET)
                || scope.family != u16::from(INET_AF_INET)
                || rest.len() < RECORD + 12
            {
                Step::Refused
            } else {
                let word = |at: usize| {
                    i32::from_be_bytes([
                        rest[at],
                        rest[at + 1],
                        rest[at + 2],
                        rest[at + 3],
                    ])
                };
                let ifindex = word(RECORD);
                let maddr = word(RECORD + 4);
                let addr = word(RECORD + 8);
                let mut mreqn = [0u8; 12];
                mreqn[0..4].copy_from_slice(&maddr.to_be_bytes());
                mreqn[4..8].copy_from_slice(&addr.to_be_bytes());
                mreqn[8..12].copy_from_slice(&ifindex.to_ne_bytes());
                let option = if code == INET_OPT_ADD_MEMBERSHIP {
                    IP_ADD_MEMBERSHIP
                } else {
                    IP_DROP_MEMBERSHIP
                };
                let step = apply(fd, code, |fd| {
                    sys::set_socket_bytes(fd, libc::IPPROTO_IP, option, &mreqn)
                });
                match step {
                    Step::Done(_) => Step::Done(RECORD + 12),
                    other => other,
                }
            }
        }
        // `{recbuf, N}` also raises the driver's own buffer when `{buffer, N}` has not been set
        // explicitly, capped at the theoretical maximum MTU for a datagram socket
        // (`:7321`-`:7360`).
        INET_OPT_RCVBUF => {
            if !options.buffer_set {
                if value > UDP_THEORETICAL_MAX_MTU && scope.is_datagram() {
                    options.buffer_size = UDP_THEORETICAL_MAX_MTU;
                } else if value > options.buffer_size {
                    options.buffer_size = value;
                }
            }
            match int_option(code) {
                Some((level, name)) => apply(fd, code, |fd| {
                    sys::set_socket_int_option(fd, level, name, value)
                }),
                None => Step::Refused,
            }
        }
        _ => match int_option(code) {
            Some((level, name)) => apply(fd, code, |fd| {
                sys::set_socket_int_option(fd, level, name, value)
            }),
            None => return None,
        },
    };
    Some(Ok(step))
}

/// `INET_UDP_THEORETICAL_MAX_MTU` (`inet_drv.c:1061`).
const UDP_THEORETICAL_MAX_MTU: i32 = 65536;

/// The `getopts` half of the syscall family: appends this code's record(s) to `reply` and answers
/// how many request bytes it consumed — or `None` when `code` is not one of its codes.
pub(super) fn get(
    code: u8,
    scope: OptScope,
    fd: Option<RawFd>,
    rest: &[u8],
    reply: &mut Vec<u8>,
) -> Result<Option<Step>> {
    // The plain `int` family: one record, or nothing when the descriptor has no socket or the
    // kernel refuses the option (the C's `TRUNCATE_TO(0, ptr); continue;`).
    if let Some((level, name)) = int_option(code) {
        let Some(fd) = fd else {
            return Ok(Some(Step::Done(1)));
        };
        if let Ok(value) = sys::get_socket_int_option(fd, level, name) {
            push_record(reply, code, value);
        }
        return Ok(Some(Step::Done(1)));
    }

    let step = match code {
        // `[opt][l_onoff_be32][l_linger_be32]` (`inet_drv.c:9505`-`:9520`).
        INET_OPT_LINGER => {
            if let Some(fd) = fd {
                if let Ok((onoff, linger)) = sys::get_socket_linger(fd) {
                    reply.push(code);
                    reply.extend_from_slice(&onoff.to_be_bytes());
                    reply.extend_from_slice(&linger.to_be_bytes());
                }
            }
            Step::Done(1)
        }
        // `default: RETURN_ERROR()` for a code with no read-back arm — both multicast-membership
        // codes are set-only (`:7616`-`:7800`).
        INET_OPT_ADD_MEMBERSHIP | INET_OPT_DROP_MEMBERSHIP => Step::Refused,
        // Non-Windows: `*ptr++ = opt; put_int32(0, ptr); continue;` (`:9440`-`:9447`).
        INET_OPT_EXCLUSIVEADDRUSE => {
            push_record(reply, code, 0);
            Step::Done(1)
        }
        // The family-selected read (`:9494`-`:9520`): a socket with no cached family is the C's
        // `RETURN_ERROR()`, and the reply carries the family word.
        INET_OPT_MULTICAST_IF => {
            if scope.family != u16::from(INET_AF_INET) {
                Step::Refused
            } else {
                // The C leaves the value slot uninitialized in the fresh-buffer case; this floor
                // answers the deterministic form (`0` there, the interface after it).
                reply.push(code);
                reply.extend_from_slice(&0i32.to_be_bytes());
                let mut ifindex = 0;
                if let Some(fd) = fd {
                    if let Ok(value) =
                        sys::get_socket_int_option(fd, libc::IPPROTO_IP, IP_MULTICAST_IF)
                    {
                        ifindex = value;
                    }
                }
                reply.extend_from_slice(&u32::from(INET_AF_INET).to_be_bytes());
                let _ = ifindex;
                Step::Done(1)
            }
        }
        // `[opt][len_be32][name bytes]`, from a NUL-terminated `IFNAMSIZ` buffer (`:9581`-`:9600`).
        INET_OPT_BIND_TO_DEVICE => {
            if let Some(fd) = fd {
                let mut name = [0u8; IFNAMSIZ];
                if let Ok(len) =
                    sys::get_socket_bytes(fd, libc::SOL_SOCKET, SO_BINDTODEVICE, &mut name)
                {
                    let end = name[..len.min(IFNAMSIZ)]
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(len.min(IFNAMSIZ));
                    reply.push(code);
                    reply.extend_from_slice(&(end as u32).to_be_bytes());
                    reply.extend_from_slice(&name[..end]);
                }
            }
            Step::Done(1)
        }
        // The family-selected `pktoptions` read (`:9602`-`:9690`): no cached family is
        // `RETURN_ERROR()`, a failed `getsockopt` drops the record, and a success answers
        // `[opt][total_len_be32]` followed by one `[copt, value_be32]` per control message.
        INET_OPT_PKTOPTIONS => {
            let (level, name) = match scope.family {
                f if f == u16::from(INET_AF_INET) => (libc::IPPROTO_IP, IP_PKTOPTIONS),
                // `INET_AF_INET6` is not a family this floor opens; treat it as the C's
                // `IPPROTO_IPV6`/`IPV6_PKTOPTIONS` arm so the refusal stays where the C puts it.
                2 => (libc::IPPROTO_IPV6, IPV6_PKTOPTIONS),
                _ => return Ok(Some(Step::Refused)),
            };
            if let Some(fd) = fd {
                let mut cmsgs = [0u8; 64];
                if let Ok(len) = sys::get_socket_bytes(fd, level, name, &mut cmsgs) {
                    let mut records = Vec::new();
                    push_cmsg_records(&cmsgs[..len], &mut records);
                    reply.push(code);
                    reply.extend_from_slice(&(records.len() as u32).to_be_bytes());
                    reply.extend_from_slice(&records);
                }
            }
            Step::Done(1)
        }
        // `[opt][proto_be32][type_be32][data_provided:8][arg_sz_be32][(arg_sz bytes)]` →
        // `[opt][proto_be32][type_be32][arg_sz_be32][arg bytes]` (`:9535`-`:9578`).
        INET_OPT_RAW => {
            if rest.len() < 14 {
                Step::Refused
            } else {
                let word = |at: usize| {
                    i32::from_be_bytes([rest[at], rest[at + 1], rest[at + 2], rest[at + 3]])
                };
                let proto = word(1);
                let arg_type = word(5);
                let data_provided = rest[9] != 0;
                let arg_sz = word(10);
                if arg_sz < 0
                    || arg_sz as u32 > INET_MAX_OPT_BUFFER
                    || (data_provided && rest.len() - 14 < arg_sz as usize)
                {
                    Step::Refused
                } else {
                    let consumed = 14 + if data_provided { arg_sz as usize } else { 0 };
                    let mut buf = vec![0u8; arg_sz as usize];
                    match fd {
                        Some(fd) => {
                            if let Ok(read) =
                                sys::get_socket_bytes(fd, proto, arg_type, &mut buf)
                            {
                                // The record reports the size the **kernel returned**, not the one
                                // the caller asked for (`sock_getopt` writes `arg_sz` back and the
                                // C reports it, `inet_drv.c:9573`-`:9578`).
                                reply.push(code);
                                reply.extend_from_slice(&proto.to_be_bytes());
                                reply.extend_from_slice(&arg_type.to_be_bytes());
                                reply.extend_from_slice(&(read as u32).to_be_bytes());
                                reply.extend_from_slice(&buf[..read]);
                            }
                        }
                        // A failed (or impossible) `getsockopt` drops the record, not the reply —
                        // but the request bytes are still consumed.
                        None => {}
                    }
                    Step::Done(consumed)
                }
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(step))
}

/// Decodes the control messages of a `pktoptions` reply into the C's `[copt, value_be32]` records,
/// mapping each `(cmsg_level, cmsg_type)` onto the option code it reports
/// (`PUT_CMSG_INT32`, `inet_drv.c:9655`-`:9685`).
fn push_cmsg_records(cmsgs: &[u8], records: &mut Vec<u8>) {
    let align = std::mem::size_of::<usize>();
    let mut at = 0;
    while at + align <= cmsgs.len() {
        let len = usize::try_from(u32::from_ne_bytes([
            cmsgs[at],
            cmsgs[at + 1],
            cmsgs[at + 2],
            cmsgs[at + 3],
        ]))
        .unwrap_or(0);
        if len < 16 || at + len > cmsgs.len() {
            break;
        }
        let level = i32::from_ne_bytes([cmsgs[at + 8], cmsgs[at + 9], cmsgs[at + 10], cmsgs[at + 11]]);
        let kind = i32::from_ne_bytes([cmsgs[at + 12], cmsgs[at + 13], cmsgs[at + 14], cmsgs[at + 15]]);
        let opt = match (level, kind) {
            (libc::IPPROTO_IP, IP_TOS) => Some(INET_OPT_TOS),
            (libc::IPPROTO_IPV6, IPV6_TCLASS) => Some(INET_OPT_TCLASS),
            (libc::IPPROTO_IP, IP_TTL) => Some(INET_OPT_TTL),
            (libc::IPPROTO_IP, IP_RECVTOS) => Some(INET_OPT_TOS),
            (libc::IPPROTO_IPV6, IPV6_RECVTCLASS) => Some(INET_OPT_TCLASS),
            (libc::IPPROTO_IP, IP_RECVTTL) => Some(INET_OPT_TTL),
            _ => None,
        };
        if let Some(opt) = opt {
            let value = i32::from_ne_bytes([
                cmsgs[at + 16],
                cmsgs[at + 17],
                cmsgs[at + 18],
                cmsgs[at + 19],
            ]);
            push_record(records, opt, value);
        }
        at += (len + align - 1) & !(align - 1);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The syscall family's own suite. The oracle-pinned per-code rows live in the sibling
/// `opt_sys_tests.rs`.
#[cfg(test)]
#[path = "opt_sys_tests.rs"]
mod tests;
