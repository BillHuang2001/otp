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

//! The `tcp_inet` driver's **`{active, …}` delivery path** — the C's active-mode read machine
//! (`tcp_deliver`/`tcp_recv`, `erts/emulator/drivers/common/inet_drv.c:13045`-`:13130`,
//! `:13130`-`:13260`).
//!
//! A passive socket's bytes wait in the port's **input queue** for the owner's next read — the
//! `{Port, {data, Data}}` path
//! ([`PortNotification::Data`](crate::PortNotification::Data)), which
//! [`TcpInetDriver::async_ready_input`](super::TcpInetDriver) drives. An **active** socket
//! (`{active, true}`, `{active, once}`, `{active, N}`) instead sends its owner a **message** per
//! delivered unit — `tcp_message` (`:3979`), `{tcp, S, [H1..Hsz | Data]}` — which the runtime
//! renders from an [`ActiveInputEvent::Data`] on the active-input channel
//! ([`crate::PortTaskOutcome`](crate::port_task::PortTaskOutcome)'s sibling
//! [`crate::PortRegistry::take_active_inputs`]). That channel is deliberately *not*
//! [`PortNotification`](crate::PortNotification) (a message to the owner is not
//! `{Port, {data, …}}`) and *not* a completion (inbound data and a counter transition are not the
//! answer to a call).
//!
//! # What a delivered unit is
//!
//! One unit is one [`ActiveInputEvent::Data`]:
//!
//! * under a framing `{packet, N}` it is **one frame**, and the framing header is stripped before
//!   delivery — `packet_get_length`/`packet_get_body` (`packet_parser.c:248`, called from
//!   `tcp_reply_data`, `inet_drv.c:4285`), which this driver already implements in
//!   [`super::frame::extract`];
//! * under `{packet, raw}` (the default) it is **one read burst**, because `packet_get_length`
//!   answers "the whole buffer" for the raw type.
//!
//! The read's `header` is **not** the framing header (that one is already gone): it is the
//! `{header, N}` option — the C's `desc->hsz` (`inet_drv.c:6931`) — split off the front of the
//! payload, exactly as `tcp_message` (`:3979`) renders `[H1..Hsz | Data]`. In `[list]` mode the
//! C flattens the pair into one string (and reports the whole payload as a flat string when
//! `hsz > len`); flattening is the **runtime's** job, so this module always hands up the split
//! [`ReadMessage`] and never a mode-dependent shape.
//!
//! # The `{active, N}` bookkeeping
//!
//! `INET_CHECK_ACTIVE_TO_PASSIVE` (`:1552`-`:1561`) runs after **each** delivered packet: an
//! `{active, once}` grant is spent silently (no message), an `{active, N}` counter is decremented
//! and, at zero, the socket goes passive **and** sends `packet_passive_message` (`:4215`) — the
//! one place a `{tcp_passive, S}` comes from. `{active, true}` is never spent.
//!
//! # Where the burst is triggered
//!
//! Three sites, all in the C:
//!
//! 1. **read readiness** (`tcp_inet_input` → `tcp_deliver`/`tcp_recv`) — [`Driver::ready_input`]
//!    ([`crate::Driver::ready_input`]), the ordinary delivery;
//! 2. **an `{active, …}` `setopts`** — `inet_set_opts` returns `2` when `trigger_recv` holds
//!    (`:7950`-`:7958`) and its `INET_REQ_SETOPTS` consumer then runs `tcp_recv(desc, 0)` **inline**
//!    (`:11127`-`:11150`), which is why `{active, once}` with a byte already waiting delivers
//!    *inside* the control call (measured: `message_queue_len` 1 right after the `setopts`).
//!    `{active, true}` instead returns `1`, whose `tcp_deliver(desc, 0)` only delivers what is
//!    already in `desc->i_buf` — a buffer this driver never pre-fills — so it arms and delivers
//!    nothing (measured: `message_queue_len` 0). See
//!    [`TcpInetDriver::after_setopts`](super::TcpInetDriver);
//! 3. the **write path** (`tcp_send`'s delayed-send flush) — out of scope here, it belongs with
//!    `{delay_send, …}`.
//!
//! `timeout` and `drive_async` stay **async-only**: they serve the queued `TCP_REQ_RECV`/
//! `INET_REQ_ACCEPT` operations, and an active socket cannot have a receive queued at all (the C's
//! ladder refuses one, `active || len != 8 ⇒ einval`, `:12273`).
//!
//! # Arming
//!
//! The C arms read readiness only while `active > 0` (`sock_select(desc, FD_READ|FD_CLOSE,
//! desc->active > 0)`, `:7944`-`:7986`) — going passive de-selects the socket. This crate's
//! poller has no empty-interest mask (`crate::poller::Interest` is `READABLE`/`WRITABLE`/`BOTH`)
//! and `mio`'s epoll is **edge-triggered**, so the registration is kept armed for the socket's
//! life and the driver gates on `options.active` instead; a **passive → active** transition
//! re-registers so already-buffered bytes produce a fresh edge
//! ([`TcpInetDriver::after_setopts`](super::TcpInetDriver)). The consequence of keeping the
//! registration armed is that bytes arriving **after** an `{active, N}` counter is spent are read
//! into the port's input queue by the passive half of
//! [`Driver::ready_input`](crate::Driver::ready_input) rather than staying in the kernel buffer —
//! they are still there for the owner's next passive read, and nothing is delivered on the active
//! channel.
//!
//! Legacy C ground truth (read-only): `erts/emulator/drivers/common/inet_drv.c`,
//! `erts/emulator/beam/packet_parser.{c,h}`.
use beam_common::Result;

use crate::driver::DriverContext;
use crate::poller::Interest;
use crate::port_task::{ActiveInputEvent, ReadMessage};

use super::frame;
use super::opt::{set_options_tracked, OptScope};
use super::{INET_MULTI, INET_ONCE, INET_PASSIVE, INET_REP_OK, TcpInetDriver};

impl TcpInetDriver {
    /// Whether the port is in one of the active modes (`active > INET_PASSIVE`).
    ///
    /// The C's test is the plain integer comparison `desc->active > 0` (`:7957`, `:7964`), which is
    /// what tells `{active, false}` from `{active, true|once|N}`.
    fn active_mode(&self) -> bool {
        self.options.active > INET_PASSIVE
    }

    /// Whether **this driver owns the read readiness** of its socket — the one predicate that
    /// tells [`Driver::ready_input`](crate::Driver::ready_input) which of its two bodies to run.
    ///
    /// The C dispatches `tcp_inet_input` (`inet_drv.c:13461`) on the descriptor's **state**, never
    /// on the mode: an `INET_STATE_ACCEPTING`/`INET_STATE_MULTI_ACCEPTING` listener takes the
    /// accept ladder (`:13469`/`:13529`), an `IS_CONNECTED(INETP(desc))` socket goes to exactly
    /// one call — `tcp_recv(desc, 0)` (`:13598`-`:13600`) — and only the *delivery* inside it
    /// splits on the mode: `tcp_reply_data` sends the bytes to the port's input queue when
    /// `desc->inet.active == INET_PASSIVE` (`inet_async_data`, `:4294`-`:4295`) and to the owner
    /// as a message otherwise (`tcp_message`, `:4296`-`:4297`).
    ///
    /// This crate splits that single read path into two bodies —
    /// [`TcpInetDriver::active_delivery`] (the active channel) and
    /// [`TcpInetDriver::async_ready_input`](super::TcpInetDriver) (the accept ladder, the queued
    /// `TCP_REQ_RECV` completion and the passive port-input read) — so the callback has to pick
    /// the one the C's state arm would have reached. Running both, or the passive one first, would
    /// let `read_available` drain the socket into the port input queue
    /// ([`TcpInetDriver::pending_input`](super::TcpInetDriver) under a framing `{packet, N}`)
    /// before the active half could read a single byte.
    ///
    /// `true` exactly in the state [`TcpInetDriver::active_delivery`] can serve: a **connected**
    /// socket, an **active** mode, and **no** queued `TCP_REQ_RECV` operation (which owns the
    /// socket and reports through the completion channel).
    ///
    /// **Divergence, recorded:** that third condition has no C counterpart — `tcp_recv` never
    /// consults `desc->inet.opt`, so a receive queued *before* the port went active is simply
    /// ignored there and the owner gets a `tcp_message` while the operation runs into its own
    /// `{error, timeout}` (measured: `active_tests.rs`'s
    /// `the_oracle_lets_the_active_mode_win_over_a_queued_receive`). This floor hands the data to
    /// the operation the owner queued instead, which loses nothing and cannot deliver the same
    /// bytes twice; the state is only reachable at all because this driver — unlike the C — does
    /// not refuse the enqueue's successor, and the C's own ladder refuses a receive on an already
    /// active port (`:12273`).
    pub(super) fn owns_readiness_for_active(&self) -> bool {
        self.state.is_connected() && self.active_mode() && self.async_ops.recv().is_none()
    }

    /// Delivers everything the socket holds on the **active-input channel** — the C's
    /// `tcp_deliver`/`tcp_recv` pair.
    ///
    /// One call delivers one unit at a time and stops the moment the mode flips passive (the C's
    /// `if (!desc->inet.active)` at `:13108`-`:13114`), so the packets after an `{active, once}`
    /// grant — or after an `{active, N}` counter runs out — stay buffered for the next arm.
    ///
    /// The burst is bounded by the socket: [`TcpInetDriver::read_bytes`](super::TcpInetDriver)
    /// drains to `EAGAIN` (the edge-triggered poller's requirement) and the loop ends as soon as a
    /// read makes no progress, so a callback can neither park, sleep nor spin.
    ///
    /// A no-op — nothing is delivered *and* nothing is read — unless
    ///
    /// * the socket is connected (a listener's input is `accept`, and a socket-less port has
    ///   none),
    /// * the port is in an active mode, and
    /// * no `TCP_REQ_RECV` operation is queued: that operation owns the socket and reports through
    ///   the completion channel ([`TcpInetDriver::try_complete_recv`](super::TcpInetDriver)). The
    ///   C's read ladder refuses an async receive on an active socket
    ///   (`active || len != 8 ⇒ einval`, `:12273`), so this state is only reachable when the
    ///   option changed *after* the enqueue; keeping the queued operation authoritative matches
    ///   what the owner asked for.
    ///
    /// # Errors
    ///
    /// A genuine `read(2)` failure ([`beam_common::BeamError::Io`]), a framing failure
    /// ([`super::frame::extract`]'s typed `badarg`/`notsup`), or a delivery the port refuses
    /// ([`DriverContext::deliver_active_input`]).
    pub(super) fn active_delivery(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if !self.owns_readiness_for_active() {
            return Ok(());
        }
        if self.framed().is_some() {
            self.deliver_framed(ctx)
        } else {
            self.deliver_raw(ctx)
        }
    }

    /// The framing (`{packet, N}`) half of [`TcpInetDriver::active_delivery`]: one message per
    /// whole frame, buffered bytes first.
    ///
    /// This is `tcp_deliver`'s loop (`:13045`-`:13130`): as long as a whole frame sits in the
    /// driver's buffer it is delivered (`tcp_remain` answers "a complete packet"), and only when
    /// the buffer falls short is the socket read again. Bytes that are not yet a whole frame stay
    /// in [`TcpInetDriver::pending_input`](super::TcpInetDriver) for the next callback — never a
    /// partial or merged frame, and nothing is dropped.
    fn deliver_framed(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        loop {
            let framing = frame::options(&self.options);
            match frame::extract(&mut self.pending_input, &framing)? {
                Some(payload) => {
                    // `inet_input_count(INETP(desc), len)` (`:13064`): the bytes **delivered**,
                    // i.e. the payload with the framing header already stripped.
                    self.stat.input(payload.len());
                    self.deliver_data(ctx, payload)?;
                    // `if (! desc->inet.active)` (`:13108`): the mode the last delivery spent
                    // ends the burst here.
                    if !self.active_mode() {
                        return Ok(());
                    }
                }
                None => {
                    let mut bytes = Vec::new();
                    let read = self.read_bytes(ctx, &mut bytes)?;
                    if read == 0 {
                        // `EAGAIN`, or a peer close the read latched: the socket is drained and
                        // the retained bytes are not a whole frame yet.
                        return Ok(());
                    }
                    self.pending_input.extend_from_slice(&bytes);
                }
            }
        }
    }

    /// The `{packet, raw}` half of [`TcpInetDriver::active_delivery`]: **one** message per read
    /// burst.
    ///
    /// `packet_get_length` answers "the whole buffer is one unit" for the raw type, so
    /// `tcp_deliver` sends exactly one `{tcp, S, Data}` per read callback and its loop ends once
    /// the input buffer is cleared (`:13099`-`:13123`). Bytes retained from an earlier framing
    /// mode are delivered **first** and stay authoritative, the same rule the passive path
    /// follows (see the `pending_input` field's note).
    fn deliver_raw(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let mut bytes = core::mem::take(&mut self.pending_input);
        self.read_bytes(ctx, &mut bytes)?;
        if bytes.is_empty() {
            return Ok(());
        }
        self.stat.input(bytes.len());
        self.deliver_data(ctx, bytes)
    }

    /// Splits one payload by the `{header, N}` option and delivers it, then applies the C's
    /// per-delivery `{active, …}` accounting.
    ///
    /// The order is the C's: `tcp_message` sends the term (`:3979`) and
    /// `INET_CHECK_ACTIVE_TO_PASSIVE` runs **after** it (`tcp_reply_data`, `:4302`), so a
    /// `{tcp_passive, S}` always follows the data message it followed from, never precedes it.
    fn deliver_data(
        &mut self,
        ctx: &mut DriverContext<'_>,
        payload: Vec<u8>,
    ) -> Result<()> {
        let read = self.split_payload(payload);
        ctx.deliver_active_input(ActiveInputEvent::Data(read))?;
        self.count_delivery(ctx)
    }

    /// The `{tcp, S, [H1..Hsz | Data]}` split of one delivered payload
    /// (`tcp_message`, `:3979`-`:4005`).
    ///
    /// `hsz` is the `{header, N}` option (the C's `desc->hsz`, `:6931`) — **not** the `{packet, N}`
    /// framing header, which was stripped before this point. The body is always present, even
    /// when empty: the C's `LOAD_BUF2BINARY(buf+hsz, 0)` still builds `<<>>`, so a `[binary]` port
    /// renders `[H1..Hsz|<<>>]` where a header-less read renders the bare binary.
    ///
    /// **Divergence, recorded:** when `hsz > payload.len()` the C takes its list branch
    /// (`(desc->mode == INET_MODE_LIST) || (hsz > len)`, `:3985`) and reports the whole payload as
    /// one flat string (a *proper* list, no binary tail). This floor always hands up
    /// `header = payload`, `body = Some(vec![])` there, which a `[list]` consumer flattens to the
    /// same bytes; only a `[binary]` consumer would see an empty binary tail instead of none.
    /// Nothing is lost or mis-ordered either way, and the split stays the runtime's to render.
    fn split_payload(&self, mut payload: Vec<u8>) -> ReadMessage {
        // The C stores `{header, N}` as an `unsigned int` (`desc->hsz = ival`, `:6931`), so a
        // negative option is the huge unsigned that makes `hsz > len` true.
        let hsz = self.options.header as u32 as usize;
        let split = hsz.min(payload.len());
        let body = payload.split_off(split);
        ReadMessage {
            header: payload,
            body: Some(body),
        }
    }

    /// `INET_CHECK_ACTIVE_TO_PASSIVE` (`:1552`-`:1561`): spends one `{active, …}` grant after
    /// each delivered packet.
    ///
    /// * `{active, once}` — spent **silently**: the packet that spent it is the one just
    ///   delivered, and the owner gets no `{tcp_passive, S}`;
    /// * `{active, N}` — the counter is decremented and, at zero, the socket goes passive **and**
    ///   `packet_passive_message` (`:4215`) is sent as [`ActiveInputEvent::Passive`];
    /// * `{active, true}` — never spent: no counter, no passive message.
    fn count_delivery(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        match self.options.active {
            INET_ONCE => self.options.active = INET_PASSIVE,
            INET_MULTI => {
                // The C's `--active_count == 0`. `wrapping_sub` keeps the C's arithmetic for the
                // (unreachable) already-zero case instead of panicking.
                self.options.active_count = self.options.active_count.wrapping_sub(1);
                if self.options.active_count == 0 {
                    self.options.active = INET_PASSIVE;
                    ctx.deliver_active_input(ActiveInputEvent::Passive)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The `setopts` request's **whole** effect, including the two active-mode side effects the C
    /// produces from inside `inet_set_opts` — the one body behind both wire forms (`Opcode::Setopts`
    /// and the named `"setopts"` verb), so the numeric and named routes stay identical.
    ///
    /// The reply is byte-for-byte what [`TcpInetDriver::verb_setopts`](super::TcpInetDriver)
    /// produces (`[1]`, the C's `[0]einval`, or the typed `BadArg` of a truncated `{active, N}`
    /// record): this function only observes the walk through
    /// [`set_options_tracked`], which reports the one fact a caller cannot re-derive (whether an
    /// `{active, N}` record drove the counter to zero, because the record lengths are
    /// code-dependent).
    pub(super) fn setopts_with_delivery(
        &mut self,
        ctx: &mut DriverContext<'_>,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        let before_active = self.options.active;
        // The C's `old_htype` — the `{packet, …}` type **before** the walk (`:6896`).
        let before_htype = self.options.packet;
        let scope = OptScope::new(self.stype, self.family, libc::IPPROTO_TCP);
        let mut landed_at_zero = false;
        let reply = set_options_tracked(
            &mut self.options,
            scope,
            self.socket,
            arg,
            &mut landed_at_zero,
        );
        // The C's `case -1: return ctl_error(EINVAL)` skips the tail of `inet_set_opts`
        // (`:7944`-`:7986`), so a refused walk arms nothing and triggers no read — but the passive
        // message an earlier `{active, N}` record emitted from *inside* the walk still stands.
        let accepted = matches!(&reply, Ok(bytes) if bytes.first() == Some(&INET_REP_OK));
        self.after_setopts(ctx, before_active, before_htype, landed_at_zero, accepted)?;
        reply
    }

    /// The tail of the C's `inet_set_opts` plus its `INET_REQ_SETOPTS` consumer, applied to the
    /// state a `setopts` walk just produced.
    ///
    /// Three independent effects, in the C's order:
    ///
    /// 1. **The passive message** — `packet_passive_message` (`:6995`-`:6998`) is emitted from
    ///    inside the option walk, so it precedes the arming and any inline read and carries **no**
    ///    state gate (a socket-less port emits it too). [`ActiveInputEvent::Passive`] is the
    ///    term-free half; the runtime picks `{tcp_passive, S}` from the port's driver name.
    /// 2. **The arming** — `sock_select(desc, FD_READ|FD_CLOSE, desc->active > 0)` for a
    ///    `(stream && connected)` socket whose `active` changed (`:7944`-`:7986`). This crate keeps
    ///    the registration armed (no empty interest mask exists, and epoll is edge-triggered), so
    ///    only the **passive → active** half does anything: it re-registers, which re-arms the
    ///    edge and so delivers bytes that arrived while the port was passive.
    /// 3. **The inline burst** — the C's `trigger_recv` (`:7950`-`:7958`):
    ///    `stype == SOCK_STREAM && !old_active && (active == INET_ONCE || active == INET_MULTI) &&
    ///    htype == old_htype`, answered as `sres == 2` and consumed by `tcp_recv(desc, 0)`
    ///    **inline** (`:11127`-`:11150`). `{active, true}` returns `1` instead (a `tcp_deliver`
    ///    over an empty input buffer ⇒ nothing), and a listener has no read to trigger.
    ///
    /// # Errors
    ///
    /// A delivery the port refuses, the poller's re-registration failure, or the inline burst's own
    /// read/framing error.
    fn after_setopts(
        &mut self,
        ctx: &mut DriverContext<'_>,
        before_active: i32,
        before_htype: i32,
        landed_at_zero: bool,
        accepted: bool,
    ) -> Result<()> {
        if landed_at_zero {
            ctx.deliver_active_input(ActiveInputEvent::Passive)?;
        }
        if !accepted {
            return Ok(());
        }
        // `set_interest` answers `BadArg` without a socket, so the C's own
        // `(stream && connected) || (dgram && open)` gate is the guard here too.
        if self.state.is_connected() && self.has_socket() && before_active == INET_PASSIVE {
            if self.active_mode() {
                self.set_interest(ctx, Interest::READABLE)?;
            }
        }
        if self.inline_burst(before_active, before_htype) {
            self.active_delivery(ctx)?;
        }
        Ok(())
    }

    /// The C's `trigger_recv` predicate (`:7950`-`:7958`) — whether the `setopts` must deliver
    /// **inside** the control call.
    ///
    /// Everything the C tests, in the C's order, and nothing more: a connected stream socket, a
    /// port that was **passive** before the walk, an outcome of `{active, once}` or `{active, N}`,
    /// and an unchanged `{packet, …}` type (`htype != old_htype` invalidates the calculated packet
    /// remainder, so the C takes its read-readiness path instead).
    fn inline_burst(&self, before_active: i32, before_htype: i32) -> bool {
        self.state.is_connected()
            && before_active == INET_PASSIVE
            && (self.options.active == INET_ONCE || self.options.active == INET_MULTI)
            && before_htype == self.options.packet
    }
}
