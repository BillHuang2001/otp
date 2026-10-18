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

//! The `tcp_inet`/`udp_inet` **socket-option vocabulary** (`INET_REQ_SETOPTS`/`INET_REQ_GETOPTS`).
//!
//! The `setopts`/`getopts` control verbs speak the C's option wire verbatim, so every reply
//! byte can be cross-checked against the oracle:
//!
//! * `setopts` receives a sequence of **five-byte** records `[opt, val_be32]` — the C's
//!   `opt = *ptr++; ival = get_int32(ptr);` (`inet_set_opts`, `inet_drv.c:6911`-`:6918`, which
//!   walks the payload with `while (len >= 5)`, so a trailing partial record is ignored) — and
//!   answers the C's `ctl_reply_ok` `[1]`. Four codes consume more than five bytes: `{active, N}`
//!   carries a trailing big-endian 16-bit counter, `{linger, …}` four more value bytes, `{netns,
//!   Path}` its path, `raw` a `(type, size, bytes)` triple, `bind_to_device` a device name and the
//!   two multicast-membership codes three more words;
//! * `getopts` receives a sequence of **one-byte** option codes (the C's `while(len--)`, which is
//!   why an empty payload answers a bare `[1]`) and answers `[1]` followed by one
//!   `[opt, val_be32]` record per requested code, **in request order** (the C's
//!   `*ptr++ = opt; put_int32(ival, ptr);`, `inet_fill_opts`, `inet_drv.c:9141`-`:9751`), with the
//!   few variable-length records (`{active, N}`, `{netns, _}`, `linger`, `raw`, `bind_to_device`,
//!   `pktoptions`) carrying their extra payload after the four-byte slot.
//!
//! This module owns the wire walk, the option **store** ([`SocketOptions`]) and the split between
//! the two families:
//!
//! * [`opt_state`] — the **driver-state** family: the codes the C keeps in the `inet`/`tcp`/`udp`
//!   descriptor and reports back verbatim;
//! * [`opt_sys`] — the **syscall-backed** family: every code that reaches the kernel through
//!   `setsockopt(2)`/`getsockopt(2)`, including the non-`int` shapes (`linger`, `bind_to_device`,
//!   `raw`, `pktoptions`, `multicast_if`).
//!
//! # The option codes
//!
//! The code→name mapping is the C's own table (`inet_drv.c:874`-`:930`); [`opt_state`] and
//! [`opt_sys`] carry the per-code semantics.
//!
//! # The gates
//!
//! Five gates are reproduced exactly, because each is observable through `getopts`:
//!
//! 1. `{send_timeout, …}`, `{send_timeout_close, …}`, `{delay_send, …}`,
//!    `{high_watermark, …}` and `{low_watermark, …}` store and report only on a **stream** socket
//!    (the C's `desc->stype == SOCK_STREAM` guards, `inet_drv.c:7050`-`:7136`, `:9186`-`:9266`).
//!    `inet.stype` is set by `open` and is **not** cleared by `close`, so a closed stream port still
//!    reports them.
//! 2. `{read_packets, N}` store **and** its `ival <= 0` refusal live inside the **datagram** gate
//!    (`:7138`-`:7146`, `:9268`-`:9280`), so a non-datagram socket silently ignores the value (still
//!    answering `[1]`) and contributes no record.
//! 3. `{show_econnreset, …}` and `{read_ahead, …}` are gated on `desc->sprotocol == IPPROTO_TCP`
//!    (`:7170`-`:7200`, `:9298`-`:9320`), which is why they truncate on a `udp_inet` port.
//! 4. `multicast_if` (`11`) and `pktoptions` (`45`) are gated on the **cached socket family**
//!    (`desc->sfamily`): a port that has never opened a socket answers `[0]einval`
//!    (`:9494`-`:9520`, `:9602`-`:9620`) rather than truncating.
//! 5. `{mode, …}`/`{deliver, …}` carry **no** gate at all (`:6939`-`:6956`, `:9168`-`:9175`).
//!
//! # Faithfully mirrored quirks
//!
//! 1. **A configured buffer reports the kernel's value, not the requested one.** Linux doubles a
//!    `SO_RCVBUF`/`SO_SNDBUF` request, so `setopts` of 4096 reads back as 8192 (and the default
//!    `recbuf` of a fresh AF_INET stream socket reads back as 131072). Reporting the kernel's own
//!    `getsockopt(2)` value reproduces that exactly, with nothing hard-coded.
//! 2. **`setopts` answers `[1]` even when the kernel rejects the option** — the C's
//!    `propagate = 0` (`:7923`-`:7942`), except for the nine codes that set `propagate = 1`
//!    (`priority`, `tos`, `ipv6_v6only`, `tclass`, `bind_to_device`, `recvtos`, `recvtclass`,
//!    `ttl`, `recvttl`), where a failed `setsockopt` is the whole reply `[0]einval`. That is also
//!    what makes `setopts` of one of those nine on a **socket-less** port `[0]einval`.
//! 3. **Neither verb has an `IS_OPEN` guard.** The C dispatches `INET_REQ_SETOPTS`/
//!    `INET_REQ_GETOPTS` (`:11111`/`:11046`) without one, so on a port with no socket at all
//!    `setopts` answers `[1]` and `getopts` answers the stored options, contributing no record for
//!    a syscall-backed code (the `getsockopt` on the invalid descriptor fails and the C's
//!    `TRUNCATE_TO(0, ptr)` drops the entry, `:9739`) — but an unserved code is still `[0]einval`.
//! 4. **A failed `getsockopt(2)` drops that record only** (`:9734`-`:9741`): the records already
//!    produced are kept and the remaining codes are still tried.
//! 5. **`getopts` of `line_delim` (`40`) is the C's `[0]einval` — the option is set-only**
//!    (`:9719`-`:9720`'s `default: RETURN_ERROR()`), replacing the **whole** reply, so a multi-code
//!    request that includes `40` answers only `[0]einval`. The multicast-membership pair (`14`/`15`)
//!    is set-only for the same reason.
//! 6. **`{active, N}` accumulates its counter** (`:6974`-`:7000`): the C adds `N` to the running
//!    total, clamps it at zero, falls back to `{active, false}` at zero, and answers the
//!    whole-reply error when the sum leaves the signed 16-bit range.
//! 7. **`{recbuf, N}` raises the reported `{buffer, N}`** unless `{buffer, N}` was set explicitly,
//!    capped at `INET_UDP_THEORETICAL_MAX_MTU` (65536) for a datagram socket (`:7321`-`:7360`).
//!
//! Two payloads are **not** mirrored, each a documented non-mirror rather than an invented value:
//! a **truncated `{active, N}` record** (the C reads the counter past the payload, `:6977`) is the
//! crate's `BeamError::BadArg`, and a **short multicast-membership** payload (which the C reads past
//! the request) is the C's own `[0]einval` instead. `multicast_if`'s read-back carries an
//! uninitialized value slot in the C; this floor answers the deterministic zero there.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`.

mod opt_state;
mod opt_sys;

use std::os::unix::io::RawFd;

use beam_common::Result;

use super::wire::{ctl_ok, ctl_xerror, INET_TYPE_DGRAM, INET_TYPE_STREAM};

/// One step of the wire walk: how many request bytes the code consumed, or the C's
/// `return -1`, which replaces the **whole** reply with `[0]einval`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Step {
    /// The code was served and consumed this many request bytes.
    Done(usize),
    /// The code resolved to the C's `return -1` — the whole reply becomes `[0]einval`.
    Refused,
}

/// The port state a gate reads: the C's `desc->stype` (the **cached** socket type),
/// `desc->sfamily` and `desc->sprotocol`.
///
/// `stype`/`family` are the wire tags with `0` meaning "unset" — a port that has never opened a
/// socket — which is what makes the family-selected arms (`multicast_if`, `pktoptions`) refuse on
/// a bare port, exactly as the C's `-1` sentinels do (`inet_drv.c:10925`-`:10932`).
#[derive(Clone, Copy, Debug)]
pub(super) struct OptScope {
    /// The cached socket type ([`INET_TYPE_STREAM`]/[`INET_TYPE_DGRAM`], `0` = none).
    pub(super) stype: u16,
    /// The cached address family (`1` = `INET_AF_INET`, `0` = none).
    pub(super) family: u16,
    /// The cached protocol (`libc::IPPROTO_TCP`/`IPPROTO_UDP`) — the C's `desc->sprotocol`.
    pub(super) protocol: i32,
}

impl OptScope {
    /// The scope of a port whose cached socket state is `stype`/`family`/`protocol`.
    pub(super) fn new(stype: u16, family: u16, protocol: i32) -> OptScope {
        OptScope {
            stype,
            family,
            protocol,
        }
    }

    /// Whether the cached protocol is TCP (`desc->sprotocol == IPPROTO_TCP`).
    pub(super) fn is_tcp_protocol(self) -> bool {
        self.protocol == libc::IPPROTO_TCP
    }
}

impl From<u16> for OptScope {
    /// A scope carrying only a socket type — the shape the driver-state gates need, and what the
    /// module's own unit tests drive.
    fn from(stype: u16) -> OptScope {
        OptScope {
            stype,
            family: 0,
            protocol: 0,
        }
    }
}

/// The `keepalive` option code (`INET_OPT_KEEPALIVE`, `inet_drv.c:875`) — `SO_KEEPALIVE`.
pub const INET_OPT_KEEPALIVE: u8 = 1;

/// The `sndbuf` option code (`INET_OPT_SNDBUF`, `inet_drv.c:880`) — `SO_SNDBUF`.
pub const INET_OPT_SNDBUF: u8 = 6;

/// The `recbuf` option code (`INET_OPT_RCVBUF`, `inet_drv.c:881`) — `SO_RCVBUF`.
pub const INET_OPT_RCVBUF: u8 = 7;

/// The `nodelay` option code (`TCP_OPT_NODELAY`, `inet_drv.c:884`) — `TCP_NODELAY`.
pub const TCP_OPT_NODELAY: u8 = 10;

/// The `{active, …}` option code (`INET_LOPT_ACTIVE`, `inet_drv.c:897`) — driver state.
pub const INET_LOPT_ACTIVE: u8 = 22;

/// The `{packet, …}` option code (`INET_LOPT_PACKET`, `inet_drv.c:898`) — driver state.
pub const INET_LOPT_PACKET: u8 = 23;

/// The `{mode, …}` option code (`INET_LOPT_MODE`, `inet_drv.c:899`) — driver state.
pub const INET_LOPT_MODE: u8 = 24;

/// The `{deliver, …}` option code (`INET_LOPT_DELIVER`, `inet_drv.c:900`) — driver state.
pub const INET_LOPT_DELIVER: u8 = 25;

/// The `{send_timeout, …}` option code (`INET_LOPT_TCP_SEND_TIMEOUT`, `inet_drv.c:905`).
pub const INET_LOPT_TCP_SEND_TIMEOUT: u8 = 30;

/// The `{packet_size, …}` option code (`INET_LOPT_PACKET_SIZE`, `inet_drv.c:907`).
pub const INET_LOPT_PACKET_SIZE: u8 = 32;

/// The `{send_timeout_close, …}` option code (`INET_LOPT_TCP_SEND_TIMEOUT_CLOSE`,
/// `inet_drv.c:910`).
pub const INET_LOPT_TCP_SEND_TIMEOUT_CLOSE: u8 = 35;

/// The `{line_delim, …}` option code (`INET_LOPT_LINE_DELIM`, `inet_drv.c:915`) — **set-only**
/// (`getopts` of this code is the C's `[0]einval`).
pub const INET_LOPT_LINE_DELIM: u8 = 40;

/// The `reuseaddr` option code (`INET_OPT_REUSEADDR`, `inet_drv.c:874`) — `SO_REUSEADDR`.
pub const INET_OPT_REUSEADDR: u8 = 0;

/// The `dontroute` option code (`INET_OPT_DONTROUTE`, `inet_drv.c:876`) — `SO_DONTROUTE`.
pub const INET_OPT_DONTROUTE: u8 = 2;

/// The `linger` option code (`INET_OPT_LINGER`, `inet_drv.c:877`) — `SO_LINGER`.
pub const INET_OPT_LINGER: u8 = 3;

/// The `broadcast` option code (`INET_OPT_BROADCAST`, `inet_drv.c:878`) — `SO_BROADCAST`.
pub const INET_OPT_BROADCAST: u8 = 4;

/// The `oobinline` option code (`INET_OPT_OOBINLINE`, `inet_drv.c:879`) — `SO_OOBINLINE`.
pub const INET_OPT_OOBINLINE: u8 = 5;

/// The `priority` option code (`INET_OPT_PRIORITY`, `inet_drv.c:882`) — `SO_PRIORITY`.
pub const INET_OPT_PRIORITY: u8 = 8;

/// The `tos` option code (`INET_OPT_TOS`, `inet_drv.c:883`) — `IP_TOS`.
pub const INET_OPT_TOS: u8 = 9;

/// The `multicast_if` option code (`UDP_OPT_MULTICAST_IF`, `inet_drv.c:887`) — `IP_MULTICAST_IF`.
pub const INET_OPT_MULTICAST_IF: u8 = 11;

/// The `multicast_ttl` option code (`UDP_OPT_MULTICAST_TTL`, `inet_drv.c:888`).
pub const INET_OPT_MULTICAST_TTL: u8 = 12;

/// The `multicast_loop` option code (`UDP_OPT_MULTICAST_LOOP`, `inet_drv.c:889`).
pub const INET_OPT_MULTICAST_LOOP: u8 = 13;

/// The `add_membership` option code (`UDP_OPT_ADD_MEMBERSHIP`, `inet_drv.c:890`) — **set-only**.
pub const INET_OPT_ADD_MEMBERSHIP: u8 = 14;

/// The `drop_membership` option code (`UDP_OPT_DROP_MEMBERSHIP`, `inet_drv.c:891`) — **set-only**.
pub const INET_OPT_DROP_MEMBERSHIP: u8 = 15;

/// The `ipv6_v6only` option code (`INET_OPT_IPV6_V6ONLY`, `inet_drv.c:892`).
pub const INET_OPT_IPV6_V6ONLY: u8 = 16;

/// The `reuseport` option code (`INET_OPT_REUSEPORT`, `inet_drv.c:894`).
pub const INET_OPT_REUSEPORT: u8 = 17;

/// The `reuseport_lb` option code (`INET_OPT_REUSEPORT_LB`, `inet_drv.c:895`).
pub const INET_OPT_REUSEPORT_LB: u8 = 18;

/// The `exclusiveaddruse` option code (`INET_OPT_EXCLUSIVEADDRUSE`, `inet_drv.c:896`).
pub const INET_OPT_EXCLUSIVEADDRUSE: u8 = 19;

/// The `raw` option code (`INET_OPT_RAW`, `inet_drv.c:901`) — a caller-nominated syscall option.
pub const INET_OPT_RAW: u8 = 34;

/// The `tclass` option code (`INET_OPT_TCLASS`, `inet_drv.c:909`) — `IPV6_TCLASS`.
pub const INET_OPT_TCLASS: u8 = 41;

/// The `bind_to_device` option code (`INET_OPT_BIND_TO_DEVICE`, `inet_drv.c:912`).
pub const INET_OPT_BIND_TO_DEVICE: u8 = 42;

/// The `recvtos` option code (`INET_OPT_RECVTOS`, `inet_drv.c:918`) — `IP_RECVTOS`.
pub const INET_OPT_RECVTOS: u8 = 43;

/// The `recvtclass` option code (`INET_OPT_RECVTCLASS`, `inet_drv.c:919`) — `IPV6_RECVTCLASS`.
pub const INET_OPT_RECVTCLASS: u8 = 44;

/// The `pktoptions` option code (`INET_OPT_PKTOPTIONS`, `inet_drv.c:920`) — `IP_PKTOPTIONS`.
pub const INET_OPT_PKTOPTIONS: u8 = 45;

/// The `{ttl, …}` option code (`INET_OPT_TTL`, `inet_drv.c:921`) — `IP_TTL`.
pub const INET_OPT_TTL: u8 = 46;

/// The `recvttl` option code (`INET_OPT_RECVTTL`, `inet_drv.c:922`) — `IP_RECVTTL`.
pub const INET_OPT_RECVTTL: u8 = 47;

/// The `nopush` option code (`TCP_OPT_NOPUSH`, `inet_drv.c:923`) — `TCP_CORK` on Linux.
pub const INET_OPT_NOPUSH: u8 = 48;

/// The `keepcnt` option code (`TCP_OPT_KEEPCNT`, `inet_drv.c:925`) — `TCP_KEEPCNT`.
pub const INET_OPT_KEEPCNT: u8 = 51;

/// The `keepidle` option code (`TCP_OPT_KEEPIDLE`, `inet_drv.c:926`) — `TCP_KEEPIDLE`.
pub const INET_OPT_KEEPIDLE: u8 = 52;

/// The `keepintvl` option code (`TCP_OPT_KEEPINTVL`, `inet_drv.c:927`) — `TCP_KEEPINTVL`.
pub const INET_OPT_KEEPINTVL: u8 = 53;

/// The `user_timeout` option code (`TCP_OPT_USER_TIMEOUT`, `inet_drv.c:929`) — `TCP_USER_TIMEOUT`.
pub const INET_OPT_USER_TIMEOUT: u8 = 54;

/// The `{header, …}` option code (`INET_LOPT_HEADER`, `inet_drv.c:886`) — the packet header size.
pub const INET_LOPT_HEADER: u8 = 21;

/// The `{buffer, …}` option code (`INET_LOPT_BUFFER`, `inet_drv.c:885`) — the driver's read buffer.
pub const INET_LOPT_BUFFER: u8 = 20;

/// The `{exit_on_close, …}` option code (`INET_LOPT_EXITONCLOSE`, `inet_drv.c:902`).
pub const INET_LOPT_EXITONCLOSE: u8 = 26;

/// The `{high_watermark, …}` option code (`INET_LOPT_TCP_HIWTRMRK`, `inet_drv.c:903`).
pub const INET_LOPT_TCP_HIWTRMRK: u8 = 27;

/// The `{low_watermark, …}` option code (`INET_LOPT_TCP_LOWTRMRK`, `inet_drv.c:904`).
pub const INET_LOPT_TCP_LOWTRMRK: u8 = 28;

/// The `{delay_send, …}` option code (`INET_LOPT_TCP_DELAY_SEND`, `inet_drv.c:906`).
pub const INET_LOPT_TCP_DELAY_SEND: u8 = 31;

/// The `{read_packets, …}` option code (`INET_LOPT_UDP_READ_PACKETS`, `inet_drv.c:908`).
pub const INET_LOPT_UDP_READ_PACKETS: u8 = 33;

/// The `{high_msgq_watermark, …}` option code (`INET_LOPT_MSGQ_HIWTRMRK`, `inet_drv.c:911`).
pub const INET_LOPT_MSGQ_HIWTRMRK: u8 = 36;

/// The `{low_msgq_watermark, …}` option code (`INET_LOPT_MSGQ_LOWTRMRK`, `inet_drv.c:913`).
pub const INET_LOPT_MSGQ_LOWTRMRK: u8 = 37;

/// The `{netns, …}` option code (`INET_LOPT_NETNS`, `inet_drv.c:914`).
pub const INET_LOPT_NETNS: u8 = 38;

/// The `{show_econnreset, …}` option code (`INET_LOPT_TCP_SHOW_ECONNRESET`, `inet_drv.c:916`).
pub const INET_LOPT_TCP_SHOW_ECONNRESET: u8 = 39;

/// The `{read_ahead, …}` option code (`INET_LOPT_TCP_READ_AHEAD`, `inet_drv.c:917`).
pub const INET_LOPT_TCP_READ_AHEAD: u8 = 49;

/// The `non_block_send` option code (`INET_LOPT_NON_BLOCK_SEND`, `inet_drv.c:924`) — SCTP-only.
pub const INET_LOPT_NON_BLOCK_SEND: u8 = 50;

/// The `{debug, …}` option code (`INET_LOPT_DEBUG`, `inet_drv.c:928`).
pub const INET_LOPT_DEBUG: u8 = 99;

/// A `{mode, …}`/`{deliver, …}` **set** record is five bytes; the rest of the store's defaults
/// follow the C descriptor's initializers (`inet_drv.c:10923`-`:10976`, `:11755`-`:11783`).
///
/// Every field here is **stored and reported** faithfully; acting on one is the [`super`] node's
/// documented handoff (active-mode delivery, the send timeouts, the watermarks, `netns`).
#[derive(Clone, Debug)]
pub(super) struct SocketOptions {
    /// `{active, …}` (code [`INET_LOPT_ACTIVE`]) — the C's `inet.active`.
    pub(super) active: i32,
    /// The `{active, N}` counter (`inet.active_count`), a signed 16-bit running total.
    pub(super) active_count: i16,
    /// `{packet, …}` (code [`INET_LOPT_PACKET`]) — the C's `inet.htype`.
    pub(super) packet: i32,
    /// `{packet_size, …}` (code [`INET_LOPT_PACKET_SIZE`]) — the C's `inet.psize`.
    pub(super) packet_size: i32,
    /// `{mode, …}` (code [`INET_LOPT_MODE`]) — the C's `inet.mode`, ungated.
    pub(super) mode: i32,
    /// `{deliver, …}` (code [`INET_LOPT_DELIVER`]) — the C's `inet.deliver`, ungated.
    pub(super) deliver: i32,
    /// `{header, …}` (code [`INET_LOPT_HEADER`]) — the C's `inet.hsz`.
    pub(super) header: i32,
    /// `{buffer, …}` (code [`INET_LOPT_BUFFER`]) — the C's `inet.bufsz`
    /// (`INET_BUFFER_TCP_DEFAULT`/`_UDP_DEFAULT` = 9216).
    pub(super) buffer_size: i32,
    /// The C's `INET_FLG_BUFFER_SET`: once `{buffer, N}` has been set explicitly,
    /// `{recbuf, N}` stops raising [`Self::buffer_size`] (`inet_drv.c:7329`).
    pub(super) buffer_set: bool,
    /// `{exit_on_close, …}` (code [`INET_LOPT_EXITONCLOSE`]) — the C's `inet.exitf`.
    pub(super) exit_on_close: i32,
    /// `{high_watermark, …}` (code [`INET_LOPT_TCP_HIWTRMRK`]) — the C's `tcp.high`.
    pub(super) high: i32,
    /// `{low_watermark, …}` (code [`INET_LOPT_TCP_LOWTRMRK`]) — the C's `tcp.low`.
    pub(super) low: i32,
    /// `{high_msgq_watermark, …}` (code [`INET_LOPT_MSGQ_HIWTRMRK`]) — the C's port limit.
    pub(super) msgq_high: i32,
    /// `{low_msgq_watermark, …}` (code [`INET_LOPT_MSGQ_LOWTRMRK`]) — the C's port limit.
    pub(super) msgq_low: i32,
    /// `{netns, Path}` (code [`INET_LOPT_NETNS`]) — the C's `inet.netns`, a NUL-terminated path.
    pub(super) netns: Option<Vec<u8>>,
    /// `{debug, …}` (code [`INET_LOPT_DEBUG`]) — the C's `inet.debug` (`DDBG_DEFAULT` is false).
    pub(super) debug: i32,
    /// `{delay_send, …}` (code [`INET_LOPT_TCP_DELAY_SEND`]) — the C's `TCP_ADDF_DELAY_SEND`.
    pub(super) delay_send: bool,
    /// `{show_econnreset, …}` (code [`INET_LOPT_TCP_SHOW_ECONNRESET`]).
    pub(super) show_econnreset: bool,
    /// `{read_ahead, …}` (code [`INET_LOPT_TCP_READ_AHEAD`]) — `!TCP_ADDF_NO_READ_AHEAD`.
    pub(super) read_ahead: bool,
    /// `{send_timeout, …}` (code [`INET_LOPT_TCP_SEND_TIMEOUT`]) in milliseconds, or
    /// [`INET_INFINITY`].
    pub(super) send_timeout: i32,
    /// `{send_timeout_close, …}` (code [`INET_LOPT_TCP_SEND_TIMEOUT_CLOSE`]).
    pub(super) send_timeout_close: i32,
    /// `{line_delim, …}` (code [`INET_LOPT_LINE_DELIM`]) — **stored, never reported**.
    pub(super) line_delim: u8,
    /// `{read_packets, …}` (code [`INET_LOPT_UDP_READ_PACKETS`]) — datagram-gated.
    pub(super) read_packets: i32,
}

impl Default for SocketOptions {
    fn default() -> SocketOptions {
        SocketOptions {
            active: INET_PASSIVE,
            active_count: 0,
            packet: TCP_PB_RAW,
            packet_size: 0,
            mode: INET_MODE_LIST,
            deliver: INET_DELIVER_TERM,
            header: 0,
            buffer_size: INET_BUFFER_DEFAULT,
            buffer_set: false,
            exit_on_close: 1,
            high: INET_HIGH_WATERMARK,
            low: INET_LOW_WATERMARK,
            msgq_high: INET_HIGH_MSGQ_WATERMARK,
            msgq_low: INET_LOW_MSGQ_WATERMARK,
            netns: None,
            debug: 0,
            delay_send: false,
            show_econnreset: false,
            read_ahead: true,
            send_timeout: INET_INFINITY,
            send_timeout_close: 0,
            line_delim: DEFAULT_LINE_DELIM,
            read_packets: INET_PACKET_POLL,
        }
    }
}

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

/// `infinity` for `{send_timeout, …}` (`INET_INFINITY`, `inet_drv.c:1067`, `0xffffffff`).
pub const INET_INFINITY: i32 = -1;

/// The `{packet, …}` default (`TCP_PB_RAW`, `packet_parser.h:35`): the raw byte stream.
const TCP_PB_RAW: i32 = 0;

/// The `{line_delim, …}` default (`desc->delimiter = '\n'`, `inet_drv.c:10934`).
const DEFAULT_LINE_DELIM: u8 = b'\n';

/// The read-buffer default (`INET_BUFFER_TCP_DEFAULT`/`INET_BUFFER_UDP_DEFAULT`, `inet_drv.c:1046`).
const INET_BUFFER_DEFAULT: i32 = 9216;

/// The stream `{high_watermark, …}` default (`INET_HIGH_WATERMARK`, `inet_drv.c:1050`).
const INET_HIGH_WATERMARK: i32 = 8192;

/// The stream `{low_watermark, …}` default (`INET_LOW_WATERMARK`, `inet_drv.c:1051`).
const INET_LOW_WATERMARK: i32 = 4096;

/// The `{high_msgq_watermark, …}` default (`INET_HIGH_MSGQ_WATERMARK`, `inet_drv.c:1064`).
const INET_HIGH_MSGQ_WATERMARK: i32 = 8192;

/// The `{low_msgq_watermark, …}` default (`INET_LOW_MSGQ_WATERMARK`, `inet_drv.c:1065`).
const INET_LOW_MSGQ_WATERMARK: i32 = 4096;

/// The bytes of one `[opt, val_be32]` record, the C's `while (len >= 5)` unit.
pub(super) const RECORD: usize = 5;

/// The extra bytes an `{active, N}` record carries after `[opt, val_be32]`: the big-endian
/// 16-bit counter (`Sint16`, `inet_drv.c:1256`, `:6977`).
pub(super) const ACTIVE_COUNTER: usize = 2;

/// The `setopts` verb: applies a sequence of option records.
///
/// `options` carries the driver-state options ([`SocketOptions`]) and `scope` the port's **cached**
/// socket state (type, family, protocol) the C reads off the descriptor; `fd` is the port's socket,
/// or `None` while the port has none. The option codes are validated either way — an unserved one
/// answers `[0]einval` exactly as the oracle does — but no syscall is attempted without a socket,
/// and the reply is the C's `[1]` (except for the nine `propagate = 1` codes, whose failed
/// `setsockopt` — including the socket-less case — is `[0]einval`).
///
/// # Errors
///
/// `BeamError::BadArg` for a truncated `{active, N}` record (a multi form without its trailing
/// counter — the C reads it past the payload, see the module docs). Never for a served option.
pub(super) fn set_options(
    options: &mut SocketOptions,
    scope: impl Into<OptScope>,
    fd: Option<RawFd>,
    arg: &[u8],
) -> Result<Vec<u8>> {
    let scope = scope.into();
    // The C's `while (len >= 5)` (`inet_set_opts`, `inet_drv.c:6908`): a trailing partial record
    // is the remainder and is ignored.
    let mut rest = arg;
    while rest.len() >= RECORD {
        let code = rest[0];
        let value = i32::from_be_bytes([rest[1], rest[2], rest[3], rest[4]]);
        let step = if let Some(step) = opt_state::set(code, value, options, scope, rest) {
            step?
        } else if let Some(step) = opt_sys::set(code, value, options, scope, rest, fd) {
            step?
        } else {
            // The C's `default: return -1` (`:7914`) — the whole reply, never a partial one.
            Step::Refused
        };
        match step {
            Step::Refused => return Ok(ctl_xerror("einval")),
            // `get` is total over the served codes, so a wrong count can only mean a bug here;
            // draining the cursor is the safe answer rather than a panic.
            Step::Done(consumed) => rest = rest.get(consumed..).unwrap_or(&[]),
        }
    }
    Ok(ctl_ok())
}

/// The `getopts` verb: reads one record per requested option **code**, in request order.
///
/// With no socket the C's `getsockopt` fails and its entry is dropped, so every **syscall-backed**
/// code contributes nothing and the reply carries only the driver-state records (plus the leading
/// `[1]`); an unserved code — including the set-only `line_delim` (`40`) and the set-only
/// multicast-membership pair — is the whole reply `[0]einval`.
///
/// # Errors
///
/// Only the unserved-code whole reply `[0]einval`. A failed `getsockopt(2)` on a live socket is
/// **not** an error: the C drops that record and keeps going (see the module docs).
pub(super) fn get_options(
    options: &SocketOptions,
    scope: impl Into<OptScope>,
    fd: Option<RawFd>,
    arg: &[u8],
) -> Result<Vec<u8>> {
    let scope = scope.into();
    let mut reply = ctl_ok();
    let mut rest = arg;
    while let Some(&code) = rest.first() {
        let step = if let Some(step) = opt_state::get(code, options, scope, &mut reply) {
            step
        } else if let Some(step) = opt_sys::get(code, scope, fd, rest, &mut reply)? {
            step
        } else {
            // The C's `default: RETURN_ERROR()` (`:9719`-`:9720`) discards the whole reply buffer,
            // so a code this floor does not serve answers exactly `[0]einval` — dropping every
            // code before it, as the oracle does.
            Step::Refused
        };
        match step {
            Step::Refused => return Ok(ctl_xerror("einval")),
            Step::Done(consumed) => rest = rest.get(consumed..).unwrap_or(&[]),
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

/// The shared oracle machinery for the two option-family suites and the `accept_opts` replay.
#[cfg(test)]
#[path = "opt_oracle.rs"]
pub(super) mod oracle;

/// The `prim_inet:accept_opts/3` replay: the exact record sequence `gen_tcp:accept/1` sends.
#[cfg(test)]
#[path = "accept_opts_tests.rs"]
mod accept_opts_tests;
