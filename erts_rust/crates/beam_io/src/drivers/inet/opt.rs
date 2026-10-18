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

//! The `tcp_inet` **socket-option vocabulary** (`INET_REQ_SETOPTS`/`INET_REQ_GETOPTS`).
//!
//! The `setopts`/`getopts` control verbs speak the C's option wire verbatim, so every reply
//! byte can be cross-checked against the oracle:
//!
//! * `setopts` receives a sequence of **five-byte** records `[opt, val_be32]` — the C's
//!   `opt = *ptr++; ival = get_int32(ptr);` (`inet_set_opts`, `inet_drv.c:6915`-`:6918`, which
//!   walks the payload with `while (len >= 5)`, so a trailing partial record is ignored) — and
//!   answers the C's `ctl_reply_ok` `[1]`. The one record that is **seven** bytes is
//!   `{active, N}`: the C reads a trailing big-endian 16-bit `N` after the five-byte record and
//!   subtracts two more bytes from the payload length (`:6975`-`:6987`);
//! * `getopts` receives a sequence of **one-byte** option codes (the C's `while(len--)`, which
//!   is why an empty payload answers a bare `[1]`) and answers `[1]` followed by one
//!   `[opt, val_be32]` record per requested code, **in request order** (the C's
//!   `*ptr++ = opt; put_int32(ival, ptr);`, `inet_fill_opts`, `inet_drv.c:9141`-`:9751`). An
//!   `{active, N}` read-back appends a trailing two-byte big-endian counter after the value
//!   (`:9179`-`:9183`).
//!
//! # The served options
//!
//! Two families are served. The first is **syscall-backed** — each a direct
//! `setsockopt(2)`/`getsockopt(2)` whose value is a plain `int`:
//!
//! | code | `gen_tcp` option | socket option | C constant |
//! |---|---|---|---|
//! | `1` ([`INET_OPT_KEEPALIVE`]) | `keepalive` | `SO_KEEPALIVE` | `inet_drv.c:875` |
//! | `6` ([`INET_OPT_SNDBUF`]) | `sndbuf` | `SO_SNDBUF` | `inet_drv.c:880` |
//! | `7` ([`INET_OPT_RCVBUF`]) | `recbuf` | `SO_RCVBUF` | `inet_drv.c:881` |
//! | `10` ([`TCP_OPT_NODELAY`]) | `nodelay` | `TCP_NODELAY` | `inet_drv.c:884` |
//!
//! The C reads these four from `SOL_SOCKET`, except `nodelay` which is `IPPROTO_TCP`
//! (`inet_drv.c:7286`-`:7321`, `:7497`-`:7504`; `:9429`-`:9450`).
//!
//! The second family is **driver state**: the C keeps the value in the `inet`/`tcp` descriptor
//! and `getopts` reports it back verbatim. This floor stores each value in [`SocketOptions`]
//! and reports it (`getopts` is byte-exact against the oracle) but does **not act on it** — the
//! behaviours behind these options (active-mode delivery, packet framing, send timeouts) stay
//! handoffs, documented in the [`super`] module docs. In particular `{mode, …}` is only
//! *stored and reported*: the `{Port, {data, …}}` term a consumer builds from it (a binary
//! rather than a list) is rendered by the runtime, not by this driver.
//!
//! | code | `gen_tcp` option | stored as | default | gettable? |
//! |---|---|---|---|---|
//! | `22` ([`INET_LOPT_ACTIVE`]) | `{active, …}` | [`SocketOptions::active`] (+ [`SocketOptions::active_count`]) | `active,false` | yes |
//! | `23` ([`INET_LOPT_PACKET`]) | `{packet, …}` | [`SocketOptions::packet`] | `raw` | yes |
//! | `24` ([`INET_LOPT_MODE`]) | `{mode, list\|binary}` | [`SocketOptions::mode`] | `list` | yes, on either socket type |
//! | `25` ([`INET_LOPT_DELIVER`]) | `{deliver, port\|term}` | [`SocketOptions::deliver`] | `term` | yes, on either socket type |
//! | `30` ([`INET_LOPT_TCP_SEND_TIMEOUT`]) | `{send_timeout, …}` | [`SocketOptions::send_timeout`] | `infinity` | yes, on a stream socket |
//! | `32` ([`INET_LOPT_PACKET_SIZE`]) | `{packet_size, …}` | [`SocketOptions::packet_size`] | `0` | yes |
//! | `33` ([`INET_LOPT_UDP_READ_PACKETS`]) | `{read_packets, N}` | [`SocketOptions::read_packets`] | `5` | yes, on a cached datagram socket only |
//! | `35` ([`INET_LOPT_TCP_SEND_TIMEOUT_CLOSE`]) | `{send_timeout_close, …}` | [`SocketOptions::send_timeout_close`] | `false` | yes, on a stream socket |
//! | `40` ([`INET_LOPT_LINE_DELIM`]) | `{line_delim, …}` | [`SocketOptions::line_delim`] | `'\n'` | **no — set-only** |
//!
//! A code this floor does not serve is the C's `default: return -1`
//! (`inet_set_opts`, `inet_drv.c:7914`; `inet_fill_opts`, `:9719`), which the `inet_ctl` arms
//! turn into `ctl_error(EINVAL)` = `[0] ++ "einval"` (`:11062`, `:11128`) — the whole reply,
//! never a partial one, so a mixed request like `[keepalive, 200]` answers exactly that.
//!
//! # Faithfully mirrored quirks
//!
//! The oracle's observable behaviour for these two verbs has six quirks, each measured
//! against `erl` and reproduced here byte for byte:
//!
//! 1. **A configured buffer reports the kernel's value, not the requested one.** Linux doubles
//!    a `SO_RCVBUF`/`SO_SNDBUF` request, so `setopts` of 4096 reads back as 8192 (and the
//!    default `recbuf` of a fresh AF_INET stream socket reads back as 131072). Reporting the
//!    kernel's own `getsockopt(2)` value reproduces that exactly, with nothing hard-coded.
//! 2. **`setopts` answers `[1]` even when the kernel rejects the option.** The C leaves
//!    `propagate = 0` for this option family, so a failed `setsockopt` is deliberately not
//!    reported (`inet_set_opts`, `inet_drv.c:7898`, `:7923`-`:7942` — "not all errors can be
//!    propagated for BC reasons"). A caller that cares reads the value back with `getopts`.
//! 3. **Neither verb has an `IS_OPEN` guard.** The C dispatches `INET_REQ_SETOPTS`/
//!    `INET_REQ_GETOPTS` (`:11111`/`:11046`) without one, so on a port with no socket at all
//!    `setopts` answers `[1]` and `getopts` answers the stored options plus `[1]` with no record
//!    for a syscall-backed code (the `setsockopt`/`getsockopt` on the invalid descriptor fails,
//!    and the C's `TRUNCATE_TO(0, ptr)` drops the entry, `:9739`) — but an unserved code is
//!    still `[0]einval`, because the option switch precedes the syscall.
//! 4. **`getopts` of `line_delim` (`40`) is the C's `[0]einval` — the option is set-only.**
//!    `inet_fill_opts` has no `INET_LOPT_LINE_DELIM` arm (`:9155`-`:9720`), so the code reaches
//!    its `default:` and the returned -1 makes the caller replace the **entire** reply with the
//!    error (`RETURN_ERROR`, `:9091`-`:9098`; `:11062`): a multi-code request that *includes*
//!    `40` answers only `[0]einval`, dropping every code before it. `setopts` of `40` is served
//!    (`desc->delimiter = (char)ival`, `:7199`-`:7206`) and answers the bare `[1]`.
//! 5. **`send_timeout`/`send_timeout_close` are gated on `stype == SOCK_STREAM`.** The C guards
//!    both the store (`inet_set_opts`, `:7104`/`:7115`) and the read-back (`inet_fill_opts`,
//!    `:9237`/`:9247`), so before `open` — where `inet.stype` is `-1` (`:10925`) — a `getopts`
//!    of `30`/`35` contributes **no** record, and a `setopts` of either is silently ignored
//!    (still answering `[1]`). `inet.stype` is set by `open` (`:5215`) and is **not** cleared by
//!    `close`, which this floor mirrors.
//! 6. **`{mode, …}`/`{deliver, …}` carry no gate at all; `{read_packets, N}` is gated on the
//!    cached socket type.** The C stores and reports `mode`/`deliver` unconditionally —
//!    `inet_set_opts` `:6939`-`:6956` and `inet_fill_opts` `:9168`-`:9175` are straight
//!    `desc->mode = ival` / `put_int32(desc->mode, ptr)` with no `stype` test — so both codes
//!    are served identically on `tcp_inet` and on `udp_inet`. `read_packets` is the gated one:
//!    `if (desc->stype == SOCK_DGRAM)` (`:7138`-`:7146`, `:9258`-`:9270`), so on any other
//!    socket its store is skipped (**still answering `[1]`** — the `ival <= 0 ⇒ -1` check runs
//!    only inside the gate) and its read-back contributes nothing. The gate reads the
//!    **cached** `inet.stype`, so it stays a datagram socket's for the port's life — an opened
//!    and then closed `udp_inet` port still reports `read_packets`, where a never-opened one
//!    reports nothing.
//! 7. **`{active, N}` accumulates its counter.** The C **adds** `N` to the descriptor's running
//!    `active_count` (`:6987`) rather than replacing it, clamps the result at zero
//!    (`:6993`-`:6994`) and falls back to `{active, false}` when the counter reaches zero
//!    (`:6995`-`:6998`); a sum outside the signed 16-bit range is the C's `return -1`
//!    (`:6985`-`:6986`, hence a whole-reply `[0]einval`). Setting any other `{active, …}` value
//!    resets the counter to zero (`:6999`-`:7000`).
//!
//! A failed `getsockopt(2)` mirrors the C's `TRUNCATE_TO(0, ptr); continue;`
//! (`inet_drv.c:9734`-`:9741`): **that record only** is dropped, every record already produced is
//! kept, and the remaining codes are still tried — so a mix of succeeding and failing codes
//! answers the concatenation of the succeeding records.
//!
//! A **truncated `{active, N}` record** — a `setopts` whose multi form carries the value but not
//! the trailing 16-bit `N` — is the one payload this floor cannot mirror: the C reads those two
//! bytes past the payload (`:6977`), which is unreproducible, so it is the crate's
//! `BeamError::BadArg` rather than a fabricated counter.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`.

use std::os::unix::io::RawFd;

use beam_common::Result;

use super::sys::{get_socket_int_option, set_socket_int_option};
use super::wire::{ctl_ok, ctl_xerror, INET_TYPE_DGRAM, INET_TYPE_STREAM};
use crate::error;

/// The `keepalive` option code (`INET_OPT_KEEPALIVE`, `inet_drv.c:875`) — `SO_KEEPALIVE`.
pub const INET_OPT_KEEPALIVE: u8 = 1;

/// The `sndbuf` option code (`INET_OPT_SNDBUF`, `inet_drv.c:880`) — `SO_SNDBUF`.
pub const INET_OPT_SNDBUF: u8 = 6;

/// The `recbuf` option code (`INET_OPT_RCVBUF`, `inet_drv.c:881`) — `SO_RCVBUF`.
pub const INET_OPT_RCVBUF: u8 = 7;

/// The `nodelay` option code (`TCP_OPT_NODELAY`, `inet_drv.c:884`) — `TCP_NODELAY`.
pub const TCP_OPT_NODELAY: u8 = 10;

/// The `{active, …}` option code (`INET_LOPT_ACTIVE`, `inet_drv.c:897`) — driver state, not a
/// socket option.
pub const INET_LOPT_ACTIVE: u8 = 22;

/// The `{packet, …}` option code (`INET_LOPT_PACKET`, `inet_drv.c:898`) — driver state.
pub const INET_LOPT_PACKET: u8 = 23;

/// The `{send_timeout, …}` option code (`INET_LOPT_TCP_SEND_TIMEOUT`, `inet_drv.c:905`) —
/// driver state.
pub const INET_LOPT_TCP_SEND_TIMEOUT: u8 = 30;

/// The `{packet_size, …}` option code (`INET_LOPT_PACKET_SIZE`, `inet_drv.c:907`) — driver
/// state.
pub const INET_LOPT_PACKET_SIZE: u8 = 32;

/// The `{send_timeout_close, …}` option code (`INET_LOPT_TCP_SEND_TIMEOUT_CLOSE`,
/// `inet_drv.c:910`) — driver state.
pub const INET_LOPT_TCP_SEND_TIMEOUT_CLOSE: u8 = 35;

/// The `{line_delim, …}` option code (`INET_LOPT_LINE_DELIM`, `inet_drv.c:915`) — driver state,
/// **set-only** (`getopts` of this code is the C's `[0]einval`).
pub const INET_LOPT_LINE_DELIM: u8 = 40;

/// The `reuseaddr` option code (`INET_OPT_REUSEADDR`, `inet_drv.c:874`) — `SO_REUSEADDR`.
pub const INET_OPT_REUSEADDR: u8 = 0;

/// The `broadcast` option code (`INET_OPT_BROADCAST`, `inet_drv.c:878`) — `SO_BROADCAST`.
pub const INET_OPT_BROADCAST: u8 = 4;

/// The `{ttl, …}` option code (`INET_OPT_TTL`, `inet_drv.c:921`) — `IP_TTL`.
pub const INET_OPT_TTL: u8 = 46;

/// The `{mode, …}` option code (`INET_LOPT_MODE`, `inet_drv.c:899`) — driver state.
pub const INET_LOPT_MODE: u8 = 24;

/// The `{deliver, …}` option code (`INET_LOPT_DELIVER`, `inet_drv.c:900`) — driver state.
pub const INET_LOPT_DELIVER: u8 = 25;

/// The `{read_packets, …}` option code (`INET_LOPT_UDP_READ_PACKETS`, `inet_drv.c:908`) — driver
/// state, gated on a cached `SOCK_DGRAM` socket type (`inet_set_opts`, `:7138`;
/// `inet_fill_opts`, `:9258`).
pub const INET_LOPT_UDP_READ_PACKETS: u8 = 33;

/// The `{mode, list}` default (`INET_MODE_LIST`, `inet_drv.c:778`).
const INET_MODE_LIST: i32 = 0;

/// The `{deliver, term}` default (`INET_DELIVER_TERM`, `inet_drv.c:783`).
const INET_DELIVER_TERM: i32 = 1;

/// The `{read_packets, N}` default (`INET_PACKET_POLL`, `inet_drv.c:1072`).
const INET_PACKET_POLL: i32 = 5;

/// `{active, false}` (`INET_PASSIVE`, `inet_drv.c:786`) — the default, and what a drained
/// `{active, N}` counter falls back to.
pub const INET_PASSIVE: i32 = 0;

/// `{active, true}` (`INET_ACTIVE`, `inet_drv.c:787`).
pub const INET_ACTIVE: i32 = 1;

/// `{active, once}` (`INET_ONCE`, `inet_drv.c:788`).
pub const INET_ONCE: i32 = 2;

/// `{active, N}` (`INET_MULTI`, `inet_drv.c:789`) — the value that carries the trailing 16-bit
/// counter.
pub const INET_MULTI: i32 = 3;

/// `infinity` for `{send_timeout, …}` (`INET_INFINITY`, `inet_drv.c:1067`, `0xffffffff`) — the
/// default.
pub const INET_INFINITY: i32 = -1;

/// The `{packet, …}` default (`TCP_PB_RAW`, `packet_parser.h:35`): the raw byte stream.
const TCP_PB_RAW: i32 = 0;

/// The `{line_delim, …}` default (`desc->delimiter = '\n'`, `inet_drv.c:10934`).
const DEFAULT_LINE_DELIM: u8 = b'\n';

/// The bytes of one `[opt, val_be32]` record, the C's `while (len >= 5)` unit.
const RECORD: usize = 5;

/// The extra bytes an `{active, N}` record carries after `[opt, val_be32]`: the big-endian
/// 16-bit counter (`Sint16`, `inet_drv.c:1256`, `:6977`).
const ACTIVE_COUNTER: usize = 2;

/// The driver-state socket options: the C's `inet`/`tcp` descriptor fields the `setopts`/
/// `getopts` verbs read and write.
///
/// These are the options whose value the driver owns rather than the kernel: the C stores them
/// in the descriptor and `getopts` reports them verbatim. This floor stores and reports them
/// (see the module docs) but does not act on them yet — active-mode delivery, packet framing
/// and the send timeouts are the [`super`] module's documented handoffs.
#[derive(Clone, Copy, Debug)]
pub(super) struct SocketOptions {
    /// `{active, …}` (code [`INET_LOPT_ACTIVE`]) — the C's `inet.active`, one of
    /// [`INET_PASSIVE`]/[`INET_ACTIVE`]/[`INET_ONCE`]/[`INET_MULTI`].
    pub(super) active: i32,
    /// The `{active, N}` counter (code [`INET_LOPT_ACTIVE`] in its multi form) — the C's
    /// `inet.active_count`, a signed 16-bit running total.
    pub(super) active_count: i16,
    /// `{packet, …}` (code [`INET_LOPT_PACKET`]) — the C's `inet.htype`, default
    /// `raw` (`0`).
    pub(super) packet: i32,
    /// `{send_timeout, …}` (code [`INET_LOPT_TCP_SEND_TIMEOUT`]) in milliseconds, or
    /// [`INET_INFINITY`] (`0xffffffff`) — the C's `tcp.send_timeout`.
    pub(super) send_timeout: i32,
    /// `{packet_size, …}` (code [`INET_LOPT_PACKET_SIZE`]) — the C's `inet.psize`.
    pub(super) packet_size: i32,
    /// `{send_timeout_close, …}` (code [`INET_LOPT_TCP_SEND_TIMEOUT_CLOSE`]) — the C's
    /// `tcp.send_timeout_close`.
    pub(super) send_timeout_close: i32,
    /// `{line_delim, …}` (code [`INET_LOPT_LINE_DELIM`]) — the C's `inet.delimiter`. Stored but
    /// **not gettable** (`getopts` of code `40` is `[0]einval`).
    pub(super) line_delim: u8,
    /// `{mode, …}` (code [`INET_LOPT_MODE`]) — the C's `inet.mode`, default `list` (`0`). Stored
    /// and reported for **either** socket type: the C's store and read-back carry no `stype` gate
    /// (`inet_set_opts`, `inet_drv.c:6939`-`:6947`; `inet_fill_opts`, `:9168`-`:9171`).
    pub(super) mode: i32,
    /// `{deliver, …}` (code [`INET_LOPT_DELIVER`]) — the C's `inet.deliver`, default `term`
    /// (`1`). Likewise ungated (`inet_set_opts`, `:6949`-`:6956`; `inet_fill_opts`,
    /// `:9172`-`:9175`).
    pub(super) deliver: i32,
    /// `{read_packets, …}` (code [`INET_LOPT_UDP_READ_PACKETS`]) — the C's
    /// `inet.read_packets`, default `INET_PACKET_POLL` (`5`), gated on a **cached**
    /// `SOCK_DGRAM` socket type (`inet_set_opts`, `:7138`-`:7146`; `inet_fill_opts`,
    /// `:9258`-`:9270`).
    pub(super) read_packets: i32,
}

impl Default for SocketOptions {
    fn default() -> SocketOptions {
        SocketOptions {
            active: INET_PASSIVE,
            active_count: 0,
            packet: TCP_PB_RAW,
            send_timeout: INET_INFINITY,
            packet_size: 0,
            send_timeout_close: 0,
            line_delim: DEFAULT_LINE_DELIM,
            mode: INET_MODE_LIST,
            deliver: INET_DELIVER_TERM,
            read_packets: INET_PACKET_POLL,
        }
    }
}

/// The `(level, name)` pair a **syscall-backed** option code selects, or `None` for a code the
/// `setsockopt`/`getsockopt` family does not serve (the C's `default:` arm becoming
/// `[0]einval`).
fn resolve(code: u8) -> Option<(libc::c_int, libc::c_int)> {
    match code {
        INET_OPT_REUSEADDR => Some((libc::SOL_SOCKET, libc::SO_REUSEADDR)),
        INET_OPT_KEEPALIVE => Some((libc::SOL_SOCKET, libc::SO_KEEPALIVE)),
        INET_OPT_BROADCAST => Some((libc::SOL_SOCKET, libc::SO_BROADCAST)),
        INET_OPT_SNDBUF => Some((libc::SOL_SOCKET, libc::SO_SNDBUF)),
        INET_OPT_RCVBUF => Some((libc::SOL_SOCKET, libc::SO_RCVBUF)),
        TCP_OPT_NODELAY => Some((libc::IPPROTO_TCP, libc::TCP_NODELAY)),
        INET_OPT_TTL => Some((libc::IPPROTO_IP, libc::IP_TTL)),
        _ => None,
    }
}

/// The `setopts` verb: applies a sequence of `[opt, val_be32]` records (and the trailing 16-bit
/// counter of an `{active, N}` record).
///
/// `options` carries the driver-state options ([`SocketOptions`]) and `stype` is the port's
/// **cached** socket type ([`INET_TYPE_STREAM`]/[`INET_TYPE_DGRAM`], `0` = none) — the C's
/// `inet.stype` (`inet_drv.c:10925`, `:5214`), which is what the two gates read: the
/// `send_timeout`/`send_timeout_close` store runs only on a stream socket (`:7104`/`:7115`) and
/// the `{read_packets, N}` store only on a datagram one (`:7138`-`:7146`, where even the
/// `ival <= 0` refusal sits *inside* the gate). `{mode, …}`/`{deliver, …}` are ungated
/// (`:6939`-`:6956`). `fd` is the port's socket, or `None` while the port has none (the C's
/// invalid descriptor): the option codes are still validated — an unserved one answers
/// `[0]einval` exactly as the oracle does — but no syscall is attempted, and the reply is the
/// C's `[1]`.
///
/// # Errors
///
/// `BeamError::BadArg` for a truncated `{active, N}` record (a multi form without its trailing
/// counter — the C reads it past the payload, see the module docs). Never for a served option:
/// a rejected `setsockopt(2)` is not propagated, mirroring the C's `propagate = 0`, and a
/// `{active, N}` counter that leaves the signed 16-bit range is the C's `[0]einval` reply.
pub(super) fn set_options(
    options: &mut SocketOptions,
    stype: u16,
    fd: Option<RawFd>,
    arg: &[u8],
) -> Result<Vec<u8>> {
    let stream = stype == u16::from(INET_TYPE_STREAM);
    let datagram = stype == u16::from(INET_TYPE_DGRAM);
    // The C's `while (len >= 5)` (`inet_set_opts`, `inet_drv.c:6908`), where an `{active, N}`
    // arm additionally consumes two bytes (`:6977`-`:6979`): a trailing partial record is the
    // remainder and is ignored.
    let mut rest = arg;
    while rest.len() >= RECORD {
        let code = rest[0];
        let value = i32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]);
        let mut consumed = RECORD;
        match code {
            INET_LOPT_ACTIVE => {
                options.active = value;
                if value == INET_MULTI {
                    // The C reads the counter's 16-bit `N` past the five-byte record
                    // (`inet_drv.c:6977`). A record too short to carry it is the one payload
                    // this floor cannot mirror; refuse it rather than inventing a counter.
                    let counter = rest
                        .get(RECORD..RECORD + ACTIVE_COUNTER)
                        .ok_or_else(error::badarg)?;
                    let nval = i16::from_be_bytes([counter[0], counter[1]]);
                    consumed += ACTIVE_COUNTER;
                    // `ac = desc->active_count; ac += nval; if (ac > INT16_MAX || ac <
                    // INT16_MIN) return -1;` (`inet_drv.c:6976`-`:6986`) — the whole reply is
                    // replaced by `[0]einval` when the running total leaves the 16-bit range.
                    let total = i32::from(options.active_count) + i32::from(nval);
                    if total > i32::from(i16::MAX) || total < i32::from(i16::MIN) {
                        return Ok(ctl_xerror("einval"));
                    }
                    // `desc->active_count += nval; if (< 0) active_count = 0; if (== 0)
                    // active = INET_PASSIVE;` (`:6987`-`:6998`).
                    options.active_count = total.max(0) as i16;
                    if options.active_count == 0 {
                        options.active = INET_PASSIVE;
                    }
                } else {
                    // `:6999`-`:7000`: any non-multi `{active, …}` zeroes the counter.
                    options.active_count = 0;
                }
            }
            INET_LOPT_PACKET => options.packet = value,
            INET_LOPT_PACKET_SIZE => options.packet_size = value,
            // `desc->delimiter = (char)ival` (`inet_drv.c:7205`) — a one-byte field.
            INET_LOPT_LINE_DELIM => options.line_delim = value as u8,
            // Both send-timeout codes store only on a stream socket (`:7104`/`:7115`); on any
            // other type the C ignores the value and still answers `[1]`.
            INET_LOPT_TCP_SEND_TIMEOUT if stream => options.send_timeout = value,
            INET_LOPT_TCP_SEND_TIMEOUT_CLOSE if stream => options.send_timeout_close = value,
            INET_LOPT_TCP_SEND_TIMEOUT | INET_LOPT_TCP_SEND_TIMEOUT_CLOSE => {}
            // `desc->mode = ival` (`:6942`) and `desc->deliver = ival` (`:6954`) — a `continue`
            // out of the switch with no `stype` test at all, so both codes are served
            // identically on a stream and on a datagram socket.
            INET_LOPT_MODE => options.mode = value,
            INET_LOPT_DELIVER => options.deliver = value,
            // `if (desc->stype == SOCK_DGRAM) { if (ival <= 0) return -1; udesc->read_packets
            // = ival; } continue;` (`inet_drv.c:7138`-`:7146`): the refusal lives *inside* the
            // gate, so a non-datagram socket silently ignores the value and answers `[1]`.
            INET_LOPT_UDP_READ_PACKETS if datagram => {
                if value <= 0 {
                    return Ok(ctl_xerror("einval"));
                }
                options.read_packets = value;
            }
            INET_LOPT_UDP_READ_PACKETS => {}
            _ => match resolve(code) {
                Some((level, name)) => {
                    if let Some(fd) = fd {
                        // Deliberately discarded, not swallowed by accident: the C leaves
                        // `propagate = 0` for this option family, so a failed `setsockopt` still
                        // answers `ok` (`inet_drv.c:7923`-`:7942`). `getopts` is how a caller
                        // learns the real value.
                        let _ = set_socket_int_option(fd, level, name, value);
                    }
                }
                None => return Ok(ctl_xerror("einval")),
            },
        }
        rest = &rest[consumed..];
    }
    Ok(ctl_ok())
}

/// The `getopts` verb: reads one `[opt, val_be32]` record per requested option **code**, in
/// request order.
///
/// `options` is the driver-state option store and `stype` is the port's **cached** socket type,
/// which the two read-back gates read: `send_timeout`/`send_timeout_close` only on a stream
/// socket (the C's `TRUNCATE_TO(0, ptr)` on anything else, `inet_drv.c:9237`/`:9247`) and
/// `{read_packets, N}` only on a datagram one (`:9258`-`:9270`). `{mode, …}`/`{deliver, …}` are
/// ungated (`:9168`-`:9175`). `fd` is the port's socket, or `None` while the port has none. With
/// no socket the C's `getsockopt` fails and its entry is dropped, so every **syscall-backed**
/// code contributes nothing and the reply carries only the driver-state records (plus the
/// leading `[1]`); an unserved code — including `line_delim` (`40`), which is set-only — is the
/// whole reply `[0]einval`.
///
/// # Errors
///
/// Only the unserved-code whole reply `[0]einval`. A failed `getsockopt(2)` on a live socket is
/// **not** an error: the C drops that record and keeps going (see the module docs).
pub(super) fn get_options(
    options: &SocketOptions,
    stype: u16,
    fd: Option<RawFd>,
    arg: &[u8],
) -> Result<Vec<u8>> {
    let stream = stype == u16::from(INET_TYPE_STREAM);
    let datagram = stype == u16::from(INET_TYPE_DGRAM);
    let mut reply = ctl_ok();
    for &code in arg {
        match code {
            // `*ptr++ = opt; put_int32(desc->active, ptr); [put_int16(active_count)]`
            // (`inet_drv.c:9176`-`:9184`).
            INET_LOPT_ACTIVE => {
                reply.push(code);
                reply.extend_from_slice(&options.active.to_be_bytes());
                if options.active == INET_MULTI {
                    reply.extend_from_slice(&options.active_count.to_be_bytes());
                }
            }
            INET_LOPT_PACKET => {
                reply.push(code);
                reply.extend_from_slice(&options.packet.to_be_bytes());
            }
            INET_LOPT_PACKET_SIZE => {
                reply.push(code);
                reply.extend_from_slice(&options.packet_size.to_be_bytes());
            }
            // The ungated pair: `*ptr++ = opt; put_int32(desc->mode, ptr);`
            // (`inet_drv.c:9168`-`:9171`) and the same for `deliver` (`:9172`-`:9175`).
            INET_LOPT_MODE => {
                reply.push(code);
                reply.extend_from_slice(&options.mode.to_be_bytes());
            }
            INET_LOPT_DELIVER => {
                reply.push(code);
                reply.extend_from_slice(&options.deliver.to_be_bytes());
            }
            // The C truncates the record away on a non-stream socket (`inet_drv.c:9237`/
            // `:9247`), contributing nothing to the reply.
            INET_LOPT_TCP_SEND_TIMEOUT if stream => {
                reply.push(code);
                reply.extend_from_slice(&options.send_timeout.to_be_bytes());
            }
            INET_LOPT_TCP_SEND_TIMEOUT_CLOSE if stream => {
                reply.push(code);
                reply.extend_from_slice(&options.send_timeout_close.to_be_bytes());
            }
            INET_LOPT_TCP_SEND_TIMEOUT | INET_LOPT_TCP_SEND_TIMEOUT_CLOSE => {}
            // `if (desc->stype == SOCK_DGRAM) { emit } else { TRUNCATE_TO(0, ptr); }`
            // (`inet_drv.c:9258`-`:9270`) — a non-datagram socket contributes nothing, and that
            // includes a port with no socket yet and a closed stream port. Not an error.
            INET_LOPT_UDP_READ_PACKETS if datagram => {
                reply.push(code);
                reply.extend_from_slice(&options.read_packets.to_be_bytes());
            }
            INET_LOPT_UDP_READ_PACKETS => {}
            _ => {
                let Some((level, name)) = resolve(code) else {
                    // The C's `default: RETURN_ERROR()` (`inet_drv.c:9719`-`:9720`) discards the
                    // whole reply buffer, so a code this floor does not serve — and the
                    // set-only `line_delim` (`40`) — answers exactly `[0]einval`.
                    return Ok(ctl_xerror("einval"));
                };
                let Some(fd) = fd else {
                    // The C's `getsockopt` on the invalid descriptor fails, so `TRUNCATE_TO(0,
                    // ptr)` drops this entry (`inet_drv.c:9739`) and the reply keeps the records
                    // gathered so far.
                    continue;
                };
                // The C's `if (sock_getopt(...) < 0) { TRUNCATE_TO(0, ptr); continue; }`
                // (`inet_drv.c:9734`-`:9741`): a failed `getsockopt(2)` drops **this record only**
                // — the records already produced are kept and later codes are still tried.
                match get_socket_int_option(fd, level, name) {
                    Ok(value) => {
                        // The C's `*ptr++ = opt; put_int32(ival, ptr);`
                        // (`inet_drv.c:9743`-`:9746`).
                        reply.push(code);
                        reply.extend_from_slice(&value.to_be_bytes());
                    }
                    Err(_) => continue,
                }
            }
        }
    }
    Ok(reply)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The option vocabulary's own suite: the driver-state store, the wire walk, the `stype` gates,
/// and the **oracle cross-check** of every code this module serves against the real system
/// `tcp_inet` driver. Split into this sibling file (the `stat_tests.rs` pattern) so the source
/// half stays near the ~1000-line target.
#[cfg(test)]
#[path = "opt_tests.rs"]
mod opt_tests;
