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

//! The **driver-state** option family of the `setopts`/`getopts` verbs.
//!
//! These are the codes the C keeps in the `inet`/`tcp`/`udp` descriptor instead of the kernel
//! (`inet_set_opts`, `erts/emulator/drivers/common/inet_drv.c:6931`-`:7210`;
//! `inet_fill_opts`, `:9141`-`:9320`). [`super::opt`] owns the wire walk and the option store
//! ([`SocketOptions`]); this module owns the per-code work.
//!
//! Two record shapes exist. Most codes read and write one `i32` slot. Four read more: the
//! `{active, N}` form carries a trailing 16-bit counter, `{netns, Path}` carries `len` bytes of
//! path, and both report those extra bytes back.
//!
//! # Stored-and-reported only
//!
//! Every option here is **stored and reported** faithfully; none is *enforced* by this floor.
//! Active-mode delivery, packet framing, the send timeouts, the buffer watermarks, `netns` and
//! the busy-message-queue limits are the [`super`] module's documented handoffs. The two that
//! would change an *observable* driver behaviour — `{packet, …}` (framing) and `{mode, …}` (the
//! delivered term shape) — are honoured by the code that consumes them (`super::frame` for the
//! read path, the runtime for the term shape), not here.
//!
//! # The three gates
//!
//! Three gates are reproduced exactly, because each is observable through `getopts`:
//!
//! * `{send_timeout, …}`/`{send_timeout_close, …}`/`{delay_send, …}`/`{high_watermark, …}`/
//!   `{low_watermark, …}` store and report only on a **stream** socket (the C's
//!   `desc->stype == SOCK_STREAM` guards);
//! * `{read_packets, N}` store **and** its `ival <= 0` refusal live inside the **datagram** gate,
//!   so a non-datagram socket silently ignores the value and still answers `[1]`
//!   (`:7138`-`:7146`);
//! * `{show_econnreset, …}`/`{read_ahead, …}` are gated on `desc->sprotocol == IPPROTO_TCP`
//!   (`:7180`-`:7200`, `:9300`-`:9320`), which is what makes them truncate on a `udp_inet` port.
//!
//! `{mode, …}`/`{deliver, …}` carry **no** gate at all, and `getopts` of `{line_delim, …}` is the
//! C's `default: RETURN_ERROR()` — the whole reply `[0]einval` — because that option is
//! set-only; this module simply does not serve it on the read path, so [`super::opt`] refuses it.

use beam_common::Result;

use super::super::wire::{INET_TYPE_DGRAM, INET_TYPE_STREAM};
use super::{
    OptScope, SocketOptions, Step, ACTIVE_COUNTER, INET_LOPT_ACTIVE, INET_LOPT_BUFFER,
    INET_LOPT_DEBUG, INET_LOPT_DELIVER, INET_LOPT_EXITONCLOSE, INET_LOPT_HEADER,
    INET_LOPT_LINE_DELIM, INET_LOPT_MODE, INET_LOPT_MSGQ_HIWTRMRK, INET_LOPT_MSGQ_LOWTRMRK,
    INET_LOPT_NETNS, INET_LOPT_NON_BLOCK_SEND, INET_LOPT_PACKET, INET_LOPT_PACKET_SIZE,
    INET_LOPT_TCP_DELAY_SEND, INET_LOPT_TCP_HIWTRMRK, INET_LOPT_TCP_LOWTRMRK,
    INET_LOPT_TCP_READ_AHEAD, INET_LOPT_TCP_SEND_TIMEOUT, INET_LOPT_TCP_SEND_TIMEOUT_CLOSE,
    INET_LOPT_TCP_SHOW_ECONNRESET, INET_LOPT_UDP_READ_PACKETS, INET_MULTI, INET_PASSIVE, RECORD,
};
use crate::error;

/// The smallest value `{high_watermark, …}`/`{low_watermark, …}` (the busy-message-queue limits)
/// accepts: `ERL_DRV_BUSY_MSGQ_LIM_MIN` (`erts/emulator/beam/erl_driver.h:326`).
const BUSY_MSGQ_LIM_MIN: i32 = 1;

/// Appends one `[opt, val_be32]` record.
fn push_record(reply: &mut Vec<u8>, code: u8, value: i32) {
    reply.push(code);
    reply.extend_from_slice(&value.to_be_bytes());
}

/// Appends one `[opt, val_be32]` record holding a boolean (`0`/`1`), the C's `!!(flag)`.
fn push_flag(reply: &mut Vec<u8>, code: u8, flag: bool) {
    push_record(reply, code, i32::from(flag));
}

/// The `setopts` half of the driver-state family, or `None` when `code` is not one of its codes.
///
/// `rest` is the request cursor **at** the code byte, so a code that consumes extra payload reads
/// it from `rest[RECORD..]` and answers the total it consumed. `value` is the record's big-endian
/// `i32` (`rest[1..5]`).
///
/// # Errors
///
/// `BeamError::BadArg` for a truncated `{active, N}` record: the C reads the counter's two bytes
/// past the payload (`inet_drv.c:6977`), which this floor refuses to reproduce.
pub(super) fn set(
    code: u8,
    value: i32,
    options: &mut SocketOptions,
    scope: OptScope,
    rest: &[u8],
) -> Option<Result<Step>> {
    let step = match code {
        // `desc->hsz = ival` (`inet_drv.c:6931`).
        INET_LOPT_HEADER => {
            options.header = value;
            Step::Done(RECORD)
        }
        // `if (ival < INET_BUFFER_MIN) ival = INET_BUFFER_MIN; desc->bufsz = ival;
        // desc->flags |= INET_FLG_BUFFER_SET;` (`:6957`-`:6966`).
        INET_LOPT_BUFFER => {
            options.buffer_size = value.max(BUFFER_MIN);
            options.buffer_set = true;
            Step::Done(RECORD)
        }
        // `desc->mode = ival` (`:6942`) and `desc->deliver = ival` (`:6954`) — ungated.
        INET_LOPT_MODE => {
            options.mode = value;
            Step::Done(RECORD)
        }
        INET_LOPT_DELIVER => {
            options.deliver = value;
            Step::Done(RECORD)
        }
        INET_LOPT_PACKET => {
            options.packet = value;
            Step::Done(RECORD)
        }
        // `desc->psize = (unsigned int)ival` (`:7013`).
        INET_LOPT_PACKET_SIZE => {
            options.packet_size = value;
            Step::Done(RECORD)
        }
        // `desc->exitf = ival` (`:7026`).
        INET_LOPT_EXITONCLOSE => {
            options.exit_on_close = value;
            Step::Done(RECORD)
        }
        INET_LOPT_ACTIVE => match set_active(value, options, rest) {
            Ok(step) => step,
            Err(err) => return Some(Err(err)),
        },
        // `if (desc->stype == SOCK_STREAM) { if (ival < 0) ival = 0; if (low > ival) low = ival;
        // high = ival; }` (`:7050`-`:7066`) — the two watermarks cross-clamp each other.
        INET_LOPT_TCP_HIWTRMRK => {
            if scope.is_stream() {
                let high = value.max(0);
                if options.low > high {
                    options.low = high;
                }
                options.high = high;
            }
            Step::Done(RECORD)
        }
        INET_LOPT_TCP_LOWTRMRK => {
            if scope.is_stream() {
                let low = value.max(0);
                if options.high < low {
                    options.high = low;
                }
                options.low = low;
            }
            Step::Done(RECORD)
        }
        // `if (ival < ERL_DRV_BUSY_MSGQ_LIM_MIN || ERL_DRV_BUSY_MSGQ_LIM_MAX < ival) return -1;`
        // (`:7090`-`:7103`). `ERL_DRV_BUSY_MSGQ_LIM_MAX` is `SIZE_MAX - 1`, so on a 32-bit `ival`
        // only the lower bound can ever trip.
        //
        // The store itself is the emulator's `erl_drv_busy_msgq_limits`, which **cross-clamps the
        // pair** exactly as the stream watermarks do (`erts/emulator/beam/erl_port_task.c:800`-
        // `:820`): writing `high` pulls `low` down to it (`if (low > high) low = high;`) and writing
        // `low` raises `high` to it (`if (high < low) high = low;`) — so the pair can never invert.
        INET_LOPT_MSGQ_HIWTRMRK => {
            if value < BUSY_MSGQ_LIM_MIN {
                Step::Refused
            } else {
                if options.msgq_low > value {
                    options.msgq_low = value;
                }
                options.msgq_high = value;
                Step::Done(RECORD)
            }
        }
        INET_LOPT_MSGQ_LOWTRMRK => {
            if value < BUSY_MSGQ_LIM_MIN {
                Step::Refused
            } else {
                if options.msgq_high < value {
                    options.msgq_high = value;
                }
                options.msgq_low = value;
                Step::Done(RECORD)
            }
        }
        // Both send-timeout codes store only on a stream socket (`:7104`/`:7115`).
        INET_LOPT_TCP_SEND_TIMEOUT => {
            if scope.is_stream() {
                options.send_timeout = value;
            }
            Step::Done(RECORD)
        }
        INET_LOPT_TCP_SEND_TIMEOUT_CLOSE => {
            if scope.is_stream() {
                options.send_timeout_close = value;
            }
            Step::Done(RECORD)
        }
        // `TCP_ADDF_DELAY_SEND` (`:7122`-`:7136`), stream-gated, reported as a boolean.
        INET_LOPT_TCP_DELAY_SEND => {
            if scope.is_stream() {
                options.delay_send = value != 0;
            }
            Step::Done(RECORD)
        }
        // `if (desc->stype == SOCK_DGRAM) { if (ival <= 0) return -1; read_packets = ival; }`
        // (`:7138`-`:7146`): the refusal sits *inside* the gate.
        INET_LOPT_UDP_READ_PACKETS => {
            if scope.is_datagram() {
                if value <= 0 {
                    Step::Refused
                } else {
                    options.read_packets = value;
                    Step::Done(RECORD)
                }
            } else {
                Step::Done(RECORD)
            }
        }
        // `if (ival < 0) return -1; if (len < ival) return -1;` then the `ival`-byte path is
        // stored (`:7148`-`:7168`).
        INET_LOPT_NETNS => {
            if value < 0 || rest.len() < RECORD || rest.len() - RECORD < value as usize {
                Step::Refused
            } else {
                options.netns = Some(rest[RECORD..RECORD + value as usize].to_vec());
                Step::Done(RECORD + value as usize)
            }
        }
        // `tcp_add_flags |= / &= ~TCP_ADDF_SHOW_ECONNRESET` under `sprotocol == IPPROTO_TCP`
        // (`:7170`-`:7186`).
        INET_LOPT_TCP_SHOW_ECONNRESET => {
            if scope.is_tcp_protocol() {
                options.show_econnreset = value != 0;
            }
            Step::Done(RECORD)
        }
        // `TCP_ADDF_NO_READ_AHEAD` is set on a *false* value (`:7188`-`:7200`).
        INET_LOPT_TCP_READ_AHEAD => {
            if scope.is_tcp_protocol() {
                options.read_ahead = value != 0;
            }
            Step::Done(RECORD)
        }
        // `desc->delimiter = (char)ival` (`:7202`-`:7207`) — the one set-only option.
        INET_LOPT_LINE_DELIM => {
            options.line_delim = value as u8;
            Step::Done(RECORD)
        }
        // Non-SCTP: `continue` — accepted and ignored (`:7870`-`:7896`).
        INET_LOPT_NON_BLOCK_SEND => Step::Done(RECORD),
        // `desc->debug = ival ? TRUE : FALSE` (`:7898`-`:7912`), reported as `0`/`1`.
        INET_LOPT_DEBUG => {
            options.debug = i32::from(value != 0);
            Step::Done(RECORD)
        }
        _ => return None,
    };
    Some(Ok(step))
}

/// The `{active, …}` arm (`inet_drv.c:6968`-`:7000`), including the `{active, N}` counter.
fn set_active(value: i32, options: &mut SocketOptions, rest: &[u8]) -> Result<Step> {
    options.active = value;
    if value == INET_MULTI {
        // The counter's 16-bit `N` follows the five-byte record (`:6977`). This floor cannot
        // mirror the C's over-read of a truncated payload, so it refuses it instead.
        let counter = rest
            .get(RECORD..RECORD + ACTIVE_COUNTER)
            .ok_or_else(error::badarg)?;
        let nval = i16::from_be_bytes([counter[0], counter[1]]);
        // `ac = active_count; ac += nval; if (ac > INT16_MAX || ac < INT16_MIN) return -1;`
        let total = i32::from(options.active_count) + i32::from(nval);
        if total > i32::from(i16::MAX) || total < i32::from(i16::MIN) {
            return Ok(Step::Refused);
        }
        options.active_count = total.max(0) as i16;
        if options.active_count == 0 {
            options.active = INET_PASSIVE;
        }
        Ok(Step::Done(RECORD + ACTIVE_COUNTER))
    } else {
        // Any non-multi `{active, …}` zeroes the running counter (`:6999`-`:7000`).
        options.active_count = 0;
        Ok(Step::Done(RECORD))
    }
}

/// The `getopts` half of the driver-state family, appending this code's record(s) to `reply` and
/// answering how many request bytes it consumed — or `None` when `code` is not one of its codes
/// (which includes the set-only `{line_delim, …}`, so [`super::opt`] refuses that code).
pub(super) fn get(
    code: u8,
    options: &SocketOptions,
    scope: OptScope,
    reply: &mut Vec<u8>,
) -> Option<Step> {
    match code {
        // `*ptr++ = opt; put_int32(desc->bufsz, ptr);` (`:9141`-`:9146`).
        INET_LOPT_BUFFER => push_record(reply, code, options.buffer_size),
        INET_LOPT_HEADER => push_record(reply, code, options.header),
        INET_LOPT_MODE => push_record(reply, code, options.mode),
        INET_LOPT_DELIVER => push_record(reply, code, options.deliver),
        // `put_int32(desc->active, ptr);` plus, for the multi form, a trailing big-endian 16-bit
        // counter (`:9155`-`:9167`).
        INET_LOPT_ACTIVE => {
            push_record(reply, code, options.active);
            if options.active == INET_MULTI {
                reply.extend_from_slice(&options.active_count.to_be_bytes());
            }
        }
        INET_LOPT_PACKET => push_record(reply, code, options.packet),
        INET_LOPT_PACKET_SIZE => push_record(reply, code, options.packet_size),
        INET_LOPT_EXITONCLOSE => push_record(reply, code, options.exit_on_close),
        // The stream-gated watermarks (`:9186`-`:9208`).
        INET_LOPT_TCP_HIWTRMRK if scope.is_stream() => push_record(reply, code, options.high),
        INET_LOPT_TCP_LOWTRMRK if scope.is_stream() => push_record(reply, code, options.low),
        INET_LOPT_TCP_HIWTRMRK | INET_LOPT_TCP_LOWTRMRK => {}
        // `erl_drv_busy_msgq_limits` (`:9209`-`:9228`): stored and reported here, with the C's
        // own defaults (8192/4096). The pair is always stored clamped (`low <= high`), so a read
        // can never report an inverted pair. This floor has no busy-message-queue limit to apply
        // them to.
        INET_LOPT_MSGQ_HIWTRMRK => push_record(reply, code, options.msgq_high),
        INET_LOPT_MSGQ_LOWTRMRK => push_record(reply, code, options.msgq_low),
        // The stream-gated send-timeout pair (`:9237`-`:9255`).
        INET_LOPT_TCP_SEND_TIMEOUT if scope.is_stream() => {
            push_record(reply, code, options.send_timeout)
        }
        INET_LOPT_TCP_SEND_TIMEOUT_CLOSE if scope.is_stream() => {
            push_record(reply, code, options.send_timeout_close)
        }
        INET_LOPT_TCP_SEND_TIMEOUT | INET_LOPT_TCP_SEND_TIMEOUT_CLOSE => {}
        // `ival = !!(tcp_add_flags & TCP_ADDF_DELAY_SEND)` — a boolean (`:9256`-`:9266`).
        INET_LOPT_TCP_DELAY_SEND if scope.is_stream() => {
            push_flag(reply, code, options.delay_send)
        }
        INET_LOPT_TCP_DELAY_SEND => {}
        // The datagram-gated `read_packets` (`:9268`-`:9280`).
        INET_LOPT_UDP_READ_PACKETS if scope.is_datagram() => {
            push_record(reply, code, options.read_packets)
        }
        INET_LOPT_UDP_READ_PACKETS => {}
        // `[opt][len_be32][path bytes]`, or nothing when no path is stored (`:9282`-`:9296`).
        INET_LOPT_NETNS => {
            if let Some(path) = &options.netns {
                reply.push(code);
                reply.extend_from_slice(&(path.len() as u32).to_be_bytes());
                reply.extend_from_slice(path);
            }
        }
        // The protocol-TCP-gated pair (`:9298`-`:9320`).
        INET_LOPT_TCP_SHOW_ECONNRESET if scope.is_tcp_protocol() => {
            push_flag(reply, code, options.show_econnreset)
        }
        INET_LOPT_TCP_SHOW_ECONNRESET => {}
        INET_LOPT_TCP_READ_AHEAD if scope.is_tcp_protocol() => {
            push_flag(reply, code, options.read_ahead)
        }
        INET_LOPT_TCP_READ_AHEAD => {}
        // `*ptr++ = opt; ival = IS_NON_BLOCK_SEND(desc) ...` — always false here, because the
        // flag is SCTP-only (`:9718`-`:9726`).
        INET_LOPT_NON_BLOCK_SEND => push_record(reply, code, 0),
        INET_LOPT_DEBUG => push_record(reply, code, options.debug),
        _ => return None,
    }
    Some(Step::Done(1))
}

/// The smallest buffer size `{buffer, N}` clamps up to (`INET_BUFFER_MIN`, `inet_drv.c:1062`).
const BUFFER_MIN: i32 = 1;

/// The two socket-type gates this module reads, resolved once from the [`OptScope`].
impl OptScope {
    /// Whether the port's cached socket type is a stream (`desc->stype == SOCK_STREAM`).
    pub(super) fn is_stream(self) -> bool {
        self.stype == u16::from(INET_TYPE_STREAM)
    }

    /// Whether the port's cached socket type is a datagram (`desc->stype == SOCK_DGRAM`).
    pub(super) fn is_datagram(self) -> bool {
        self.stype == u16::from(INET_TYPE_DGRAM)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The driver-state family's own suite: every code's store/report round trip, the three gates and
/// the `[0]einval` refusals. The oracle cross-checks live in the sibling `opt_state_tests.rs`.
#[cfg(test)]
#[path = "opt_state_tests.rs"]
mod tests;
