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

//! The socket drivers' **asynchronous-operation queue** — the C's `enq_async` machinery and the
//! `{inet_async, Port, Id, Result}` completions it produces.
//!
//! # Why an operation needs a queue at all
//!
//! A `recv` or an `accept` cannot be answered by its control call: the data (or the connection) is
//! not there yet, and parking the runtime's pump until it is would stall every other process. The C
//! therefore hands the operation an **id** in the control-op reply and delivers the outcome later
//! as a message to the port's owner:
//!
//! * the reply is `[1] ++ <<Id:16/big>>` (`ctl_reply_ok` of a two-byte buffer, `inet_drv.c:12319`
//!   for `TCP_REQ_RECV` and `:15010` for `PACKET_REQ_RECV`), produced by [`async_ok_reply`];
//! * the completion is `{inet_async, Port, Id, Result}` — [`CompletionEvent::InetAsync`]
//!   (`send_async_ok`, `inet_drv.c:2493-2508`; `send_async_error`, `:2539-2562`).
//!
//! Both halves cross this crate's driver→owner completion channel
//! ([`CompletionEvent`](crate::completion::CompletionEvent)): the id rides the *return value* of
//! the control op (which is what `erlang:port_control/3` hands back), and the result is staged on
//! the port with
//! [`DriverContext::deliver_completion`](crate::driver::DriverContext::deliver_completion) — never
//! in the same reply, exactly as the C never returns the data from `port_control`.
//!
//! # The queue's shape — two different disciplines
//!
//! * **The receive slot is single** (`INET_MAX_ASYNC`, `inet_drv.c:1069`): `enq_async` refuses a
//!   second concurrent operation with `EALREADY` (`ctl_error(EALREADY)`, `:12307`), which the
//!   caller sees as `[0]ealready`. Both socket drivers use it for their receive requests
//!   (`TCP_REQ_RECV` 42 on `tcp_inet`, `PACKET_REQ_RECV` 60 on `udp_inet`).
//! * **Accepts are a multi-operation list** (`enq_multi_op`, `:12194`/`:12209`), whose ids this
//!   queue's one counter hands out while the list itself — its per-port timer entries and the
//!   sticky `INET_F_MULTI_CLIENT` bit — is [`super::accept`]'s.
//!
//! # The id counter
//!
//! `NEW_ASYNC_ID(desc)` is `((desc)->op_ref++ & 0xffff)` (`:1549`): a per-descriptor counter that
//! starts at **0** and is taken *before* any capacity check, so a **refused** enqueue still
//! advances it — measured on a fresh listener: `[1,0,0]` … `[1,0,255]`, `[1,1,0]` (the 256th),
//! `[1,1,3]` (the 260th). It wraps at 65536, not at 256; the 255→256 step is the carry into the
//! high byte. [`AsyncQueue::next_id`] reproduces all of it.
//!
//! # Where a completion is produced
//!
//! Never inline in the control call (see the module docs of
//! [`TcpInetDriver`](super::TcpInetDriver)). There are exactly three production points, which are
//! the C's own:
//!
//! 1. **socket readiness** — `Driver::ready_input`, with read readiness armed while an operation is
//!    pending (the C's `sock_select(FD_READ, 1)`);
//! 2. **the port timer** — `Driver::next_timeout` arms the port's `TIMEOUT` task from the earliest
//!    pending deadline (for accepts: the earliest deadline in the accept list) and
//!    `Driver::timeout` then reports `{error, timeout}` for the receive slot's own operation or, for
//!    accepts, for the **head** of the accept list (the C's `driver_set_timer` +
//!    `tcp_inet_recv_timeout`, `:12480`, and `fire_multi_timers` → `tcp_inet_multi_timeout`,
//!    `:15650`/`:12600`);
//! 3. **the round's async drive point** — `Driver::drive_async`, which retries an operation against
//!    data that is already buffered (`desc->i_buf`, a decoded `{packet, N}` frame, a datagram
//!    already sitting in the socket). Without it an operation whose data arrived **before** it was
//!    enqueued would wait for the next readiness edge, which an edge-triggered poller never
//!    re-delivers.
//!
//! A close ([`Driver::stop`](crate::driver::Driver::stop), or the `close` verb) drains every
//! pending operation as `{error, closed}` **before** the descriptor goes away — the C's
//! `tcp_close_check` → `async_error_am_all(am_closed)` (`:11887`) and `packet_inet_stop` (`:14548`,
//! `:14561`).
//!
//! # What the frozen [`Driver`](crate::driver::Driver) seam cannot express
//!
//! The C addresses a completion to `desc->caller` — the process that issued the
//! `port_control`/`port_command` — which is not necessarily the port's owner. The seam has no
//! caller notion, so every completion here is delivered to the **port's owner** (the single-caller
//! model the runtime already has: `beam_rt` dispatches a port's operations from the process that
//! owns the port). That is the one attribution divergence; it is stated again in
//! `src/drivers/inet/CONTEXT.md`.

use std::time::{Duration, Instant};

use beam_common::Result;

use super::accept::AcceptQueue;
use super::frame;
use super::header;
use super::stat;
use super::wire::{ctl_ok, ctl_xerror, echoable_caller_tag, split_command_prefix};
use super::{Interest, ReadReport, TcpInetDriver};
use crate::completion::{CompletionEvent, InetAsyncResult, InetReason, InetReply};
use crate::driver::DriverContext;
use crate::port_failure::DriverFailure;
use crate::error;

/// The C's `INET_MAX_ASYNC` (`erts/emulator/drivers/common/inet_drv.c:1069`): the receive slot
/// holds **one** operation, so a second concurrent receive is `[0]ealready`.
pub(super) const INET_MAX_ASYNC: usize = 1;

/// The C's `TCP_MAX_PACKET_SIZE` (`inet_drv.c:1308`): a receive operand asking for more than this
/// is refused with `[0]enomem`.
const TCP_MAX_PACKET_SIZE: u32 = 0x0400_0000;

/// One queued `recv` operation — the C's `desc->inet.timer` + the `n` (length) of a
/// `TCP_REQ_RECV`, tied to the id its control reply handed out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct RecvOp {
    /// The 16-bit id the control reply carried.
    pub(super) id: u16,
    /// The C's `n`: `0` means "whatever the socket has" (raw) or "one whole frame" (under a
    /// `{packet, N}` code); a non-zero value means "exactly this many bytes" and is legal only
    /// with the raw header type (`:12296`).
    pub(super) length: u32,
    /// The operation's deadline, or `None` for the C's `INET_INFINITY`.
    pub(super) deadline: Option<Instant>,
}

impl RecvOp {
    /// Whether this operation's deadline has passed at `now`.
    pub(super) fn is_due(self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }
}

/// The async-operation queue of one socket descriptor.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(super) struct AsyncQueue {
    /// The C's `desc->op_ref`: the `NEW_ASYNC_ID` counter, `op_ref++ & 0xffff` (`:1549`).
    op_ref: u32,
    /// The C's single receive slot (`INET_MAX_ASYNC`).
    recv: Option<RecvOp>,
    /// The accept half of this queue — the C's `enq_multi_op` accept list, its `desc->mtd` timer
    /// entries and the sticky `INET_F_MULTI_CLIENT` bit. The machinery is [`super::accept`]'s; it
    /// lives beside the receive slot because a descriptor has **one** id counter and **one** port
    /// timer.
    pub(super) accepts: AcceptQueue,
}

impl AsyncQueue {
    /// An empty queue: no operation pending, the id counter at the C's initial `0`
    /// (`inet_init_desc` zeroes `op_ref`, `:10908`).
    pub(super) fn new() -> AsyncQueue {
        AsyncQueue::default()
    }

    /// Takes the next id — the C's `NEW_ASYNC_ID(desc)`, `op_ref++ & 0xffff` (`:1549`).
    ///
    /// Taken **before** any capacity check, so a refused enqueue still advances the counter (the
    /// measured `[1,0,255]` → `[1,1,0]` step is the 256th id, and the 16-bit field wraps at 65536).
    pub(super) fn next_id(&mut self) -> u16 {
        let id = (self.op_ref & 0xffff) as u16;
        self.op_ref = self.op_ref.wrapping_add(1);
        id
    }

    /// The pending receive operation, if the slot is taken.
    pub(super) fn recv(&self) -> Option<RecvOp> {
        self.recv
    }

    /// Empties the receive slot, returning what was in it.
    pub(super) fn take_recv(&mut self) -> Option<RecvOp> {
        self.recv.take()
    }

    /// Enqueues a receive operation, answering `false` when the slot is already taken — the C's
    /// `enq_async` returning `< 0`, which the callers report as `[0]ealready`
    /// (`ctl_error(EALREADY)`, `:12307`, `:15001`).
    pub(super) fn enqueue_recv(&mut self, op: RecvOp) -> bool {
        let pending = usize::from(self.recv.is_some());
        if pending >= INET_MAX_ASYNC {
            return false;
        }
        self.recv = Some(op);
        true
    }

    /// Whether any operation is pending on either queue.
    pub(super) fn has_pending(&self) -> bool {
        self.recv.is_some() || !self.accepts.is_empty()
    }

    /// The earliest deadline among the pending operations — what the driver declares through
    /// `Driver::next_timeout`, which the port mirrors into its single `TIMEOUT` task.
    ///
    /// For accepts it is the **first armed timer entry** (the C's `desc->mtd->when`, see
    /// [`super::accept`]), which is what the port's one timer is armed from — not the accept list
    /// head's own deadline.
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        let accept = self.accepts.deadline();
        let recv = self.recv.and_then(|op| op.deadline);
        accept.into_iter().chain(recv).min()
    }

    /// Every pending operation's id — the receive slot first, then the accepts in enqueue order —
    /// leaving the queue (and its timers) empty.
    pub(super) fn take_all(&mut self) -> Vec<u16> {
        let mut ids = Vec::new();
        if let Some(op) = self.recv.take() {
            ids.push(op.id);
        }
        // The accept half takes the same drain (`async_error_am_all` empties the list, so the
        // sticky multi bit goes with it — see [`super::accept`]).
        ids.extend(self.accepts.take_all());
        ids
    }
}

/// The deadline an operation's `Timeout(4)` operand asks for: `None` for the C's
/// `INET_INFINITY` (`0xffffffff`, i.e. a negative `i32`), otherwise `now + timeout_ms`.
///
/// A **zero** operand is a deadline of "now", so the operation completes with `{error, timeout}`
/// at the next round — the C arms `driver_set_timer(port, 0)`, whose expiry is what its
/// `timeout` callback reports (`tcp_inet_recv_timeout`, `inet_drv.c:12480`).
pub(super) fn deadline_for(timeout_ms: i32) -> Option<Instant> {
    if timeout_ms < 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
    }
}

/// The `[1] ++ <<Id:16/big>>` reply an accepted asynchronous operation answers with
/// (`inet_drv.c:12319`, `:15010`).
pub(super) fn async_ok_reply(id: u16) -> Vec<u8> {
    let mut reply = ctl_ok();
    reply.extend_from_slice(&id.to_be_bytes());
    reply
}

/// The C's `Timeout(4), Length(4)` operand of `TCP_REQ_RECV`(42)/`PACKET_REQ_RECV`(60).
///
/// `None` when the operand is not exactly eight bytes — the callers turn that into the C's
/// `ctl_error(EINVAL)`; the operand itself is read with the C's own `get_int32` layout
/// (`inet_drv.c:12293`).
pub(super) fn parse_recv_operand(arg: &[u8]) -> Option<(i32, u32)> {
    let [t0, t1, t2, t3, n0, n1, n2, n3] = arg else {
        return None;
    };
    Some((
        i32::from_be_bytes([*t0, *t1, *t2, *t3]),
        u32::from_be_bytes([*n0, *n1, *n2, *n3]),
    ))
}

/// Whether an operand's `Length` is one the C refuses with `[0]enomem`
/// (`n > TCP_MAX_PACKET_SIZE`, `inet_drv.c:12301`).
pub(super) fn length_too_large(length: u32) -> bool {
    length > TCP_MAX_PACKET_SIZE
}

/// Completes **every** pending operation of `queue` with `{error, closed}` — the C's
/// `async_error_am_all(desc, am_closed)` (`inet_drv.c:2597-2608`), run by `tcp_close_check`
/// (`:11887`) and `packet_inet_stop` (`:14548`).
///
/// The completions are staged on the port; when this runs from
/// [`Driver::stop`](crate::driver::Driver::stop) they are harvested by the registry's close path,
/// so they still reach the owner exactly once.
///
/// # Errors
///
/// Returns the typed `unsupported` when the driver context carries no completion sink — a driver
/// method called outside a [`Port`](crate::port::Port), which no runtime path does.
pub(super) fn drain_error_all(
    queue: &mut AsyncQueue,
    ctx: &mut DriverContext<'_>,
    reason: InetReason,
) -> Result<()> {
    for id in queue.take_all() {
        ctx.deliver_completion(CompletionEvent::InetAsync {
            id,
            result: InetAsyncResult::Error(reason),
        })?;
    }
    Ok(())
}
/// [`drain_error_all`] with the C's `am_closed` — `async_error_am_all(desc, am_closed)`.
pub(super) fn drain_closed(
    queue: &mut AsyncQueue,
    ctx: &mut DriverContext<'_>,
) -> Result<()> {
    drain_error_all(queue, ctx, InetReason::Closed)
}

impl TcpInetDriver {
    /// The queued operations of this descriptor.
    pub(super) fn queued_ops(&self) -> &AsyncQueue {
        &self.async_ops
    }

    /// `TCP_REQ_RECV`(42): queue one asynchronous receive and answer `[1] ++ id`.
    ///
    /// The guard ladder is the C's, in the C's order (`inet_drv.c:12265-12325`):
    ///
    /// 1. the **delayed-close reply** first — `[0]econnreset`/`[0]closed` once, then
    ///    `[0]enotconn` (`:12279`-`:12288`), then the `!IS_CONNECTED` `[0]enotconn`;
    /// 2. `active || len != 8` ⇒ `[0]einval` (`:12290`);
    /// 3. a framing header type with a non-zero `Length` ⇒ `[0]einval` (`:12296`) — "exactly n
    ///    bytes" only exists for the raw header type;
    /// 4. `Length > TCP_MAX_PACKET_SIZE` ⇒ `[0]enomem` (`:12301`);
    /// 5. the receive slot is taken ⇒ `[0]ealready` (`:12307`).
    ///
    /// Step 1's order is load-bearing: the C's ladder **is** the `tcp_add_flags` check
    /// (`if (desc->tcp_add_flags & TCP_ADDF_DELAYED_CLOSE_RECV)`, `:12279`) and only its `else`
    /// is the `IS_CONNECTED` test (`:12286`). A passive close with the default
    /// `{exit_on_close,true}` ends in `tcp_desc_close` → `erl_inet_close`, which clears
    /// `INET_STATE_CONNECTED`/sets `INET_STATE_CLOSED` (`:5078`, `:5106`) — so a guard that
    /// tested the state first would swallow the armed `closed`/`econnreset` this receive owes
    /// and answer the bare `enotconn` instead.
    ///
    /// The id is taken **before** step 5, so a refused enqueue still advances the counter.
    pub(super) fn async_recv(
        &mut self,
        ctx: &mut DriverContext<'_>,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        if let Some(reason) = self.peer_close.recv_reply() {
            return Ok(ctl_xerror(&reason.atom_name()));
        }
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        // `desc->inet.active` (`:12290`): an active-mode socket has no passive receive.
        if self.options.active != 0 || arg.len() != 8 {
            return Ok(ctl_xerror("einval"));
        }
        let Some((timeout_ms, length)) = parse_recv_operand(arg) else {
            return Ok(ctl_xerror("einval"));
        };
        // `if (htype != TCP_PB_RAW && n != 0) return ctl_error(EINVAL)` (`:12296`).
        if self.framed().is_some() && length != 0 {
            return Ok(ctl_xerror("einval"));
        }
        if length_too_large(length) {
            return Ok(ctl_xerror("enomem"));
        }
        // The C's `enq_async` takes the id first and only then refuses (`NEW_ASYNC_ID` `:1549`,
        // the `enq_async` `< 0` test at `:12307`).
        let id = self.async_ops.next_id();
        let op = RecvOp {
            id,
            length,
            deadline: deadline_for(timeout_ms),
        };
        if !self.async_ops.enqueue_recv(op) {
            return Ok(ctl_xerror("ealready"));
        }
        // The C's `sock_select(FD_READ|FD_CLOSE, 1)` and `driver_set_timer`: read readiness is
        // armed (the port mirrors `next_timeout` into its `TIMEOUT` task right after this control
        // call) and the completion itself is produced at a later round — see the module docs.
        self.set_interest(ctx, Interest::READABLE)?;
        // The enqueue selects the read side even out of a spent `{active, …}` grant's window —
        // the C's `sock_select(INETP(desc), (FD_READ|FD_CLOSE), 1)` on `TCP_REQ_RECV` (`:12315`).
        self.read_armed = true;
        Ok(async_ok_reply(id))
    }

    /// Completes the pending receive operation if the bytes are there.
    ///
    /// This is the C's `tcp_recv(desc, n)` (`inet_drv.c:13130`): it decodes one `{packet, N}`
    /// frame, or takes "exactly `n` bytes", or takes whatever the socket holds, and stages the
    /// result as `{inet_async, Port, Id, {ok, Data}}`. Returns whether an operation completed.
    ///
    /// The bytes live in [`TcpInetDriver::pending_input`], which is also where
    /// `TCP_REQ_UNRECV`'s `tcp_push_buffer` puts them, so a pushed buffer is delivered first.
    ///
    /// # Errors
    ///
    /// The framing decoder's own typed failure (`badarg` for bytes that cannot be a frame,
    /// `notsup` for the HTTP/`ssl_tls` types) — after which the operation is **removed**, so a
    /// poisoned buffer cannot fail every later round — or the read failure
    /// [`TcpInetDriver::read_bytes`] reports.
    fn try_complete_recv(&mut self, ctx: &mut DriverContext<'_>) -> Result<bool> {
        let mut completed = false;
        loop {
            // Re-read the slot every iteration: a teardown that runs **inside** the read below
            // (a peer close latches through `latch_peer_close`, which drains every pending
            // operation with `{error, closed}`) has already answered this id, so reporting it
            // again here would complete one operation twice.
            let Some(op) = self.async_ops.recv() else {
                break;
            };
            let framed = self.framed().is_some();
            // Is the operation satisfiable from what is already buffered?
            //
            // The framed branch also does its accounting: `stat::take_frame` credits the **whole
            // frame** (framing header included), which is `tcp_deliver`'s
            // `inet_input_count(INETP(desc), len)` with `len` = `packet_get_length`'s total packet
            // length (`inet_drv.c:12967`/`:13064`). The raw branches count the delivered bytes
            // below — `tcp_deliver(desc, request_len)` for an exact-length receive (`:13162`) and
            // the whole input buffer for `tcp_deliver(desc, 0)` on a raw socket.
            let payload = if framed {
                match stat::take_frame(
                    &mut self.stat,
                    &mut self.pending_input,
                    &frame::options(&self.options),
                ) {
                    Ok(frame) => frame,
                    Err(err) => {
                        // The buffered bytes can never be a frame for this option set: report the
                        // typed failure once instead of re-failing every round.
                        self.async_ops.take_recv();
                        return Err(err);
                    }
                }
            } else if op.length == 0 {
                if self.pending_input.is_empty() {
                    None
                } else {
                    Some(core::mem::take(&mut self.pending_input))
                }
            } else if self.pending_input.len() >= op.length as usize {
                let rest = self.pending_input.split_off(op.length as usize);
                Some(core::mem::replace(&mut self.pending_input, rest))
            } else {
                None
            };
            if let Some(payload) = payload {
                self.async_ops.take_recv();
                if !framed {
                    // A raw delivery counts exactly the bytes it hands over — no header exists to
                    // add, so this is the C's `len` too.
                    self.stat.input(payload.len());
                }
                // The passive reply splits on `{header, N}` exactly as the active message does:
                // `inet_async_data` (`:3162`) applies `desc->hsz` to the **body**
                // `packet_get_body` left behind, including its flat-list branch when `hsz > len`
                // (`:3175`-`:3190`) — one rule, [`header::split_payload`].
                ctx.deliver_completion(CompletionEvent::InetAsync {
                    id: op.id,
                    result: InetAsyncResult::Data(header::split_payload(
                        self.options.header,
                        payload,
                    )),
                })?;
                return Ok(true);
            }
            if self.peer_close.seen {
                break;
            }
            if self.deferred_close.is_some() {
                // Nothing complete is buffered, so a close the read already reported can never be
                // answered with data: latch it and let the tail report it.
                self.latch_deferred(ctx)?;
                break;
            }
            // Not enough buffered bytes: read what the socket has and try once more.
            let before = self.pending_input.len();
            let mut bytes = Vec::new();
            // The queued operation is the party that consumes a discovery this read makes: it is
            // answered right below from the `peer_close` latch (`async_error_am_all(am_closed)`,
            // `inet_drv.c:12829`), so the receive ladder stays unarmed.
            let close = self.read_bytes(&mut bytes, ReadReport::Answered)?;
            self.pending_input.extend_from_slice(&bytes);
            if let Some(source) = close {
                // The bytes this read collected belong to the parked operation, so a complete unit
                // is delivered **first**: the C's `tcp_recv` answers a complete unit and only
                // reaches its `n == 0` arm on a later call (`inet_drv.c:13216`, `:13206`), which
                // owes `{error, closed}` to the next receive rather than to this one.
                self.deferred_close.get_or_insert(source);
            }
            if self.pending_input.len() == before {
                // `EAGAIN`, or a read that saw only the peer's end of file: a deferred close can
                // no longer be answered with data, so latch it and let the tail report it
                // (`async_error_am_all(am_closed)`, `inet_drv.c:12829`).
                self.latch_deferred(ctx)?;
                break;
            }        }
        if self.async_ops.recv().is_some() && self.peer_close.seen {
            // The peer went away with the operation pending: the C answers the pending operation
            // from the discovery site — `tcp_recv_closed` with `async_error_am(am_closed)`
            // (`inet_drv.c:12829`) for an end-of-file, and `tcp_recv_error` with
            // `async_error_am_all(desc, error_atom(err))` (`:12877`) for a reset under
            // `{show_econnreset,true}`, which is how the oracle reports `{error,econnreset}`.
            let reason = self.peer_close.observed_reason();
            if let Some(op) = self.async_ops.take_recv() {
                ctx.deliver_completion(CompletionEvent::InetAsync {
                    id: op.id,
                    result: InetAsyncResult::Error(reason),
                })?;
                completed = true;
            }
        }
        Ok(completed)
    }

    /// Completes every operation whose deadline has passed — the port's `TIMEOUT` task, i.e. the
    /// C's `tcp_inet_recv_timeout`/accept timeout (`inet_drv.c:12480`).
    ///
    /// Before reporting a receive timeout the operation is given one last chance: the C attempts
    /// the receive at enqueue time, so bytes that are already there are delivered rather than
    /// reported as a timeout (which is exactly what makes a zero timeout see pending data instead
    /// of `{error, timeout}`).
    ///
    /// The **accept** half reports the **head** of the multi-op list, not the operation whose
    /// deadline fired: the port keeps one timer, armed from the earliest entry of the accept-timer
    /// list (`add_multi_timer`, `inet_drv.c:15744`-`:15790`), and its expiry runs
    /// `fire_multi_timers` (`:15650`) → `tcp_inet_multi_timeout` (`:12600`), whose
    /// `remove_multi_op` takes the first entry of the accept list — for a single caller that is its
    /// head (`:2379`-`:2405`) — and answers *that* id with `{error, timeout}`
    /// (`send_async_error(…, am_timeout)`). It then loops while the earliest remaining timer entry
    /// is already due (`next_timeout <= 0`) and otherwise re-arms from the survivors, so one call
    /// may report several heads, always in list order.
    pub(super) fn async_timeout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let now = Instant::now();
        if self.async_ops.recv().is_some_and(|op| op.is_due(now))
            && !self.try_complete_recv(ctx)?
        {
            if let Some(op) = self.async_ops.take_recv() {
                ctx.deliver_completion(CompletionEvent::InetAsync {
                    id: op.id,
                    result: InetAsyncResult::Error(InetReason::Timeout),
                })?;
            }
        }
        // The C's `fire_multi_timers` loop — the accept list's heads, one per due timer entry —
        // is [`super::accept`]'s.
        self.accept_timeouts(ctx, now)
    }

    /// The round's async drive point: completes what already-arrived data allows, answering how
    /// many operations completed.
    ///
    /// It never spins: each call makes at most one non-blocking `accept(2)`/`read(2)` attempt per
    /// pending operation, and once the queue is empty
    /// [`Driver::has_pending_async`](crate::driver::Driver::has_pending_async) turns `false` so the
    /// registry stops calling it.
    pub(super) fn drive_async_ops(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let mut completed = self.accepts_completed(ctx)?;
        if self.try_complete_recv(ctx)? {
            completed += 1;
        }
        Ok(completed)
    }

    /// The read-readiness callback's body: serve the accepts a listener has queued, then either
    /// complete the pending receive or move the bytes into the port's input queue (the passive
    /// floor's `{Port, {data, …}}` path) — that last step only while the descriptor's read side is
    /// selected (see [`TcpInetDriver::read_armed`]), so a spent `{active, …}` grant leaves the
    /// peer's bytes in the kernel for the next arm.
    pub(super) fn async_ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.serve_accepts(ctx)?;
        if !self.state.is_connected() {
            return Ok(());
        }
        if self.async_ops.recv().is_some() {
            self.try_complete_recv(ctx)?;
            return Ok(());
        }
        // No operation is queued, so nothing can be answered here: a close this read discovers is
        // owed to the user's next `recv` (see [`ReadReport`]). The C never reads in this state at
        // all — read interest is armed only by a queued `TCP_REQ_RECV` (`:12315`) — which is why
        // the reply has to be deferred rather than dropped.
        //
        // The reactor's read is additionally gated on the descriptor's read **selection**
        // ([`TcpInetDriver::read_armed`]): a delivery that spent its `{active, …}` grant deselected
        // the socket (`inet_drv.c:13106`-`:13114`), so the peer's later bytes must stay in the
        // kernel for the next arm instead of being moved onto the passive channel here.
        if !self.read_armed {
            return Ok(());
        }
        self.read_available(ctx, ReadReport::OwedToNextRecv)
            .map(|_read| ())
    }
}

// ---------------------------------------------------------------------------
// The named `recv` verbs — this floor's own synchronous vocabulary
// ---------------------------------------------------------------------------
//
// These are **not** the C requests: the numeric `TCP_REQ_RECV`(42) goes through the asynchronous
// queue above and answers `[1] ++ <<Id:16/big>>`. The named verbs are the convenience vocabulary
// this driver has always had for a direct caller (its own tests, and a runtime that wants a
// read-now answer), so they keep their synchronous contract: `recv` reads what the socket holds
// into the port's input queue. The named `accept` verb, which transfers the port to the
// connection, is [`TcpInetDriver::verb_accept`] in [`super::accept`].

impl TcpInetDriver {
    /// The `recv` verb: reads everything available into the port input queue.
    ///
    /// In the `{packet, raw}` default the whole byte stream is moved straight to the port input
    /// queue. With a framing `{packet, N}` the read path instead **decodes exactly one frame**
    /// and delivers its payload (see [`TcpInetDriver::verb_recv_framed`]).
    ///
    /// Once the peer's close is known the verb answers `[0] ++ "closed"` **once** and
    /// `[0] ++ "enotconn"` after that (the C's `TCP_ADDF_DELAYED_CLOSE_RECV`,
    /// `inet_drv.c:12279`); a `recv` whose read is what discovers the close answers `closed`
    /// too, and the bytes that read collected stay in the port input queue for the runtime to
    /// deliver.
    ///
    /// The ladder is consulted **before** the connection state, as the C's `:12279`-`:12286`
    /// order demands: a passive close under the default `{exit_on_close,true}` runs
    /// `tcp_desc_close`, which clears `INET_STATE_CONNECTED` (`:5078`) and sets
    /// `INET_STATE_CLOSED` (`:5106`), so the state alone would answer the bare `enotconn` and
    /// swallow the `closed`/`econnreset` this receive is owed.
    ///
    /// This is the **synchronous** read-now verb; the C's asynchronous `TCP_REQ_RECV` takes a
    /// `Timeout(4), Length(4)` payload, requires a passive socket and a raw header type, and
    /// answers `[1] ++ async_id_be16` — that route is [`TcpInetDriver::async_recv`] (the numeric
    /// opcode), reached with the operand this verb refuses. A non-empty argument here is a
    /// `BadArg` rather than a silently-ignored timeout/length.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` for a non-empty argument, the read failure
    /// [`TcpInetDriver::read_available`] reports, or — under `{packet, N}` — a typed framing
    /// failure ([`self::frame`]: `badarg` for bytes that cannot be a frame, `notsup` for the
    /// HTTP/`ssl_tls` types).
    pub(super) fn verb_recv(
        &mut self,
        ctx: &mut DriverContext<'_>,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        if let Some(reason) = self.peer_close.recv_reply() {
            return Ok(ctl_xerror(&reason.atom_name()));
        }
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        if !arg.is_empty() {
            return Err(error::badarg());
        }
        // An explicit read is never gated by a spent `{active, …}` grant — `sock_select(…, 0)`
        // drops only the *reactor's* read callback (`:13107`) — and the port's eager passive read
        // resumes once it has run, so the read side is selected again (see
        // [`TcpInetDriver::read_armed`]).
        self.read_armed = true;
        if self.framed().is_some() {
            return self.verb_recv_framed(ctx);
        }
        // This caller answers a close its own read discovers (below), so the receive ladder is not
        // armed by it.
        let read = self.read_available(ctx, ReadReport::Answered)?;
        if read == 0 && self.peer_close.seen {
            // This read is what found the peer gone, so it answers the close itself: `closed` for
            // an end-of-file, `econnreset` when the read's error was a reset under
            // `{show_econnreset,true}` (`tcp_recv_closed` vs `tcp_recv_error`,
            // `inet_drv.c:13185`-`:13196`).
            return Ok(ctl_xerror(&self.peer_close.observed_reason().atom_name()));
        }
        Ok(ctl_ok())
    }

    /// The `recv` verb under a framing `{packet, N}`: deliver **one** decoded frame's payload.
    ///
    /// This is the passive, synchronous counterpart of the C's `tcp_recv`/`tcp_remain` pair
    /// (`inet_drv.c:13130`, `:12967`). The driver accumulates socket bytes in
    /// [`TcpInetDriver::pending_input`] (see [`TcpInetDriver::read_available`]) and each call
    /// takes exactly one whole frame off the front of it:
    ///
    /// * a **complete** frame ⇒ its payload (the header stripped, the line delimiter kept) goes
    ///   into the port input queue and the reply is `[1]`; **one frame per `recv`**, so two
    ///   frames back to back are delivered by two calls;
    /// * a **partial** frame — a header the decoder cannot size yet, or a declared body that has
    ///   not all arrived — ⇒ more is read from the socket and the decode is retried; once the
    ///   socket is drained (`EAGAIN`) the partial frame is **retained** and the reply is the
    ///   bare `[1]` with nothing delivered, so the next `recv` continues where this one stopped;
    /// * bytes that **cannot** be a frame (`badarg`) or a type this floor does not decode
    ///   (`notsup`) ⇒ a typed error, never a partial, merged or wrong frame.
    ///
    /// A `recv` whose read is what discovers the peer's close answers `closed` itself, exactly
    /// as the raw path does (a frame already extracted before the close is still delivered).
    ///
    /// # Errors
    ///
    /// The read failure [`TcpInetDriver::read_available`] reports, or a typed framing failure
    /// (`badarg`/`notsup`).
    pub(super) fn verb_recv_framed(&mut self, ctx: &mut DriverContext<'_>) -> Result<Vec<u8>> {
        let framing = frame::options(&self.options);
        loop {
            match stat::take_frame(&mut self.stat, &mut self.pending_input, &framing)? {
                Some(payload) => {
                    // The count is [`stat::take_frame`]'s: `tcp_deliver` charges
                    // `inet_input_count(INETP(desc), len)` with the frame's **total packet
                    // length** (`inet_drv.c:12967`/`:13064`), the framing header included —
                    // not the stripped payload pushed onto the input queue below.
                    ctx.input().push(&payload);
                    return Ok(ctl_ok());
                }
                None => {
                    // Not a whole frame yet: refill the framing buffer from the socket (in a
                    // framing mode `read_available` appends there) and try to decode again.
                    //
                    // A close discovered here is answered by this verb below, so it owes no
                    // receive one-shot.
                    let read = self.read_available(ctx, ReadReport::Answered)?;
                    if read == 0 {
                        if self.peer_close.seen {
                            // This read is what found the peer gone (see [`verb_recv`]):
                            // `closed`, or `econnreset` for a reset under
                            // `{show_econnreset,true}`.
                            return Ok(ctl_xerror(&self.peer_close.observed_reason().atom_name()));
                        }
                        // The socket is drained but the frame is still partial: deliver nothing
                        // and keep the buffered bytes for the next `recv`.
                        return Ok(ctl_ok());
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The send path's caller tag — `init_caller` + `inet_reply_finish`
// ---------------------------------------------------------------------------

impl TcpInetDriver {
    /// The `erlang:port_command/2` write path: strip one caller tag, frame the payload, write, then
    /// answer the caller with the C's `{inet_reply, Port, Reply[, Tag]}`.
    ///
    /// The C's `tcp_inet_commandv` (`inet_drv.c:12656`-`:12705`) runs `init_caller` on the
    /// command's bytes first (`:1820`-`:1890`), writes what is left in `tcp_sendv`
    /// (`:13736`-`:13848`, which is also where the `{packet, 1|2|4}` header is prepended) and
    /// finishes the caller in `inet_reply_finish` (`:2614`-`:2637`). This is that whole sequence.
    ///
    /// Returns the output queue's **net shrinkage**: the tag prefix plus what the socket accepted,
    /// minus the framing header the write path put back on the queue — which is what
    /// [`crate::Port`]'s write-accounting contract compares the queue against.
    ///
    /// # Errors
    ///
    /// Only the typed `unsupported` of a context with no completion sink, or the `BeamError::Io`
    /// of a failed `send(2)`: every other outcome is the C's own — a malformed command charges
    /// itself and latches `driver_failure_posix(EINVAL)`, and an unconnected one charges itself and
    /// stages `{error, enotconn}` instead of failing.
    /// [`TcpInetDriver::write_pending`] can only be reached from here with a socket that **is**
    /// connected (the `!is_connected` arm above returns first), so its own `BadArg` refusal is
    /// unreachable on this path.
    pub(super) fn write_command(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        // A re-driven `OUTPUT` task with nothing queued is not a command: the C is only ever
        // handed one (`tcp_inet_commandv`'s `ev->size`), and `init_caller` on an empty buffer is
        // its `size < 2` FALSE — which would kill the port over a write that already finished.
        if ctx.output().is_empty() {
            return Ok(0);
        }
        // The C's `init_caller` runs first, before the connection state is looked at
        // (`tcp_inet_commandv`, `inet_drv.c:12656`-`:12667`), and its failure kills the port.
        let Some(stripped) = self.strip_command_prefix(ctx) else {
            return self.fail_malformed_command(ctx);
        };
        // The C's ladder is keyed on `!IS_CONNECTED` (`:12681`), and the C **is** in that state
        // once a close has been latched: a passive close with the default
        // `{exit_on_close,true}` ends the socket through `tcp_desc_close` → `erl_inet_close` →
        // `desc_close`, whose `desc->state &= ~INET_STATE_CONNECTED` (`:5079`) is followed by
        // `desc->state = INET_STATE_CLOSED` (`:5106`) — the comment at `:5078`-`:5079` names
        // exactly this reply ("Next write to socket should produce {error, enotconn}").
        //
        // This floor keeps the descriptor for the port's whole life — [`Self::latch_peer_close`]
        // releases the poller registration instead of closing the socket — so `state` alone can no
        // longer answer the C's question: without the latched close in this guard, a command issued
        // *after* the peer's close would be handed to [`Self::write_pending`], which refuses to
        // touch a dead socket (`self.peer_close.seen`, `mod.rs`) and leaves the payload queued with
        // **no** reply at all, where the C answers `{error, closed}`/`{error, enotconn}` from the
        // send ladder below. The two conditions are therefore one arm, not two.
        //
        // The C's **zero-length exemption precedes every closed check**. `tcp_inet_commandv`'s
        // `!IS_CONNECTED` arm replies and never queues, so the output queue is always empty when
        // `tcp_sendv` runs, and its shortcut `if ((h_len == 0) && (len == 0)) return 0;`
        // (`inet_drv.c:13736`, inside the `driver_sizeq(port) == 0` arm) therefore answers
        // `inet_reply_ok` for an empty write **unconditionally and any number of times** — an
        // empty write is exempt from the half-close's closed ladder until a non-empty write has
        // actually failed and the descriptor is closed. Measured on the oracle (OTP 29.1.1):
        // after `gen_tcp:shutdown(S, write)` every `gen_tcp:send(S, <<>>)` / `[]` / `[<<>>,[]]`
        // answers `ok`, and only after a *non-empty* send has answered `{error,closed}` does an
        // empty one answer `{error,enotconn}`.
        //
        // So an empty payload is answered `ok` here — before the ladder — while the socket is
        // still connected and no close has been latched. Under `{packet, N}` the C's `h_len` is
        // not zero (a 1-/2-/4-byte header is still written for an empty payload), so the
        // exemption is keyed on **both** lengths, exactly as the C is.
        if self.state.is_connected() && !self.peer_close.seen {
            let payload_len = ctx.output().len();
            if payload_len == 0 && frame::write_header(&self.options, payload_len).is_none() {
                self.out_framed = false;
                self.caller_stripped = false;
                if let Some(tag) = self.caller_ref.take() {
                    ctx.deliver_completion(CompletionEvent::InetReply {
                        caller_ref: Some(tag),
                        reply: InetReply::Ok,
                    })?;
                }
                return Ok(stripped);
            }
        }
        if !self.state.is_connected() || self.peer_close.seen {
            // The C's `!IS_CONNECTED` arm (`:12674`-`:12683`): the command is accepted (the port
            // charged `prt->bytes_out += size` for it) but never written, and the caller learns
            // the outcome from `inet_reply_error(ENOTCONN)` — a `{inet_reply, Port,
            // {error,enotconn}, Tag}` message, staged only when a tag can be echoed.
            let discarded = ctx.output().len();
            ctx.output().consume(discarded);
            self.caller_stripped = false;
            self.out_framed = false;
            // The C's `!IS_CONNECTED` arm first consults the send ladder
            // (`tcp_inet_commandv`, `inet_drv.c:12681`-`:12692`): a latched `closed`/`econnreset`
            // one-shot is what the caller is answered with, and only an unarmed ladder is the
            // bare `enotconn`.
            let reply = match self.peer_close.send_reply() {
                Some(reason) => InetReply::Error(reason),
                None => InetReply::Error(InetReason::from_errno(libc::ENOTCONN)),
            };
            if let Some(tag) = self.caller_ref.take() {
                ctx.deliver_completion(CompletionEvent::InetReply {
                    caller_ref: Some(tag),
                    reply,
                })?;
            }
            return Ok(stripped + discarded);
        }
        // The command's **payload** length — the C's `len = ev->size` read inside `tcp_sendv`
        // (`inet_drv.c:13736`-`:13743`) **after** `init_caller_iov` has taken the caller tag off
        // (`:1827`-`:1851`, which reduces `ev->size` by the tag). It is therefore the length the
        // framing header announces, and it never includes the tag bytes.
        let payload_len = ctx.output().len();
        // `{packet, 1|2|4}` frames the payload on the **write** side too — `tcp_sendv` switches on
        // `desc->inet.htype` and prepends the 1-/2-/4-byte big-endian payload length
        // (`inet_drv.c:13744`-`:13769`; the non-vector `tcp_send` does the same at `:13861`-`:13885`).
        // Only the **stream** driver frames: `packet_inet_command` (the datagram `port_command`
        // path) has no `h_len` logic at all — `inet_output_count`'s `len+h_len` and the two
        // `tcp_send*` functions are the C's only framing sites — so `udp_inet` writes its
        // datagrams verbatim.
        //
        // The header goes in **front** of the payload, exactly as `ev->iov[0].iov_base = buf`
        // (`:13766`) puts it in the emulator's spare leading iovec slot (`io.c:1639`: the slot
        // arrives with `iov_len = 0`). This floor has no iovec and `ByteQueue` only appends, so
        // the header is prepended by re-queueing the payload behind it; `front()` therefore stays
        // one contiguous slice, and the header bytes are the first thing `write_pending` sends.
        //
        // It runs once per **command**, not once per write attempt (the C decides `h_len` inside
        // the one `tcp_sendv` call it makes per command): a short write leaves header and payload
        // queued together and the remainder must never be framed again, or the peer would read a
        // header announcing the remainder's length in the middle of the stream.
        if !self.out_framed {
            let header = frame::write_header(&self.options, payload_len);
            let h_len = header.as_ref().map_or(0, Vec::len);
            if let Some(header) = header {
                let payload = ctx.output().front().to_vec();
                ctx.output().clear();
                ctx.output().push(&header);
                ctx.output().push(&payload);
            }
            // `inet_output_count(INETP(desc), len+h_len)` (`inet_drv.c:13763`, the `tcp_send`
            // counterpart at `:13862`): the payload **plus** the framing header, counted once per
            // command at issue time — so a partial write, or a write that then discovers the peer
            // gone, still counted it, exactly as the C does before it sends. A refused command
            // never reaches this line (`!IS_CONNECTED` above returns first, where the C returns
            // before `tcp_sendv` too), so `send_oct` is unchanged by it.
            self.stat.output(payload_len + h_len);
            self.out_framed = true;
        }
        let _written = self.write_pending(ctx)?;
        self.settle_command(ctx)?;
        // Report the queue's **net** shrinkage — the length the port measured before this callback
        // (`stripped + payload_len`) minus what is left now — which is exactly what
        // [`crate::Port`]'s write accounting compares against. The strip took `stripped` bytes off
        // the queue and the framing header put `h_len` back on, so the two are not separable: the
        // header is written before the payload and the port only sees the total. A saturated
        // subtraction is the same clamp the port applies, so a command that grew the queue (a
        // zero-length tag under `{packet,4}` that stalled before its header went out) reports 0 on
        // both sides.
        //
        // A write that discovered the peer's close drops the bytes that can never be delivered
        // (`latch_peer_close` → `tcp_clear_output`, `inet_drv.c:12786`) and the C charges the whole
        // command to `prt->bytes_out` there anyway (`:12690`), so the dropped remainder is part of
        // the count too.
        Ok((stripped + payload_len).saturating_sub(ctx.output().len()))
    }

    /// The C's `driver_failure_posix(port, EINVAL)` for a command `init_caller` rejects
    /// (`inet_drv.c:12667`): the whole command is **accounted for** — the port has queued it and
    /// the C's `prt->bytes_out += size` charges every byte — nothing is written, and the port is
    /// closed with the `einval` reason.
    ///
    /// The latch is a request, not a teardown: the registry reaps the port after the round
    /// ([`crate::PortRegistry::reap_failures`]), so this callback still returns normally and
    /// `erlang:port_command/2`'s caller sees the oracle's `true`.
    fn fail_malformed_command(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.caller_ref = None;
        self.caller_stripped = false;
        self.out_framed = false;
        let discarded = ctx.output().len();
        ctx.output().consume(discarded);
        ctx.signal_failure(DriverFailure::posix(libc::EINVAL))?;
        Ok(discarded)
    }

    /// Strips **one** leading caller tag from the head of the output queue, returning how many
    /// bytes it consumed (`0` when this command's prefix is already off).
    ///
    /// The C's `init_caller` (`inet_drv.c:1820`-`:1890`): a `[n:be16][tag][payload]` prefix, with
    /// **no** content inspection — the rule is [`split_command_prefix`]'s and is size-only. The
    /// tag is kept for the reply; the payload is left in the queue to be written.
    ///
    /// It runs **once per command**, not once per write attempt: a short write leaves the
    /// remainder queued and the remainder is *not* re-prefixed, so [`Self::caller_stripped`]
    /// records that this command's prefix is already off. The flag is cleared when the queue
    /// drains ([`TcpInetDriver::write_pending`]), so the next command is examined afresh.
    ///
    /// A **zero-length** tag (`[0,0]`) is still a prefix: the C's `init_caller` consumes the
    /// length word and keeps an empty tag buffer, so the two bytes come off the queue and the
    /// payload reaches the socket unchanged — measured on the oracle, `port_command(P,
    /// <<0:16,"raw">>)` delivers exactly `"raw"` — while **no** completion is staged for it (the
    /// C's `inet_reply_finish` cannot decode an empty EXT, `:2628`).
    ///
    /// `None` is the C's `FALSE` — too short for a length word, or a length that does not fit —
    /// which kills the port ([`TcpInetDriver::fail_malformed_command`]).
    fn strip_command_prefix(&mut self, ctx: &mut DriverContext<'_>) -> Option<usize> {
        if self.caller_stripped {
            return Some(0);
        }
        self.caller_stripped = true;
        let (prefix, tag) = {
            let front = ctx.output().front();
            match split_command_prefix(front) {
                Some((tag, rest)) => (front.len() - rest.len(), tag.to_vec()),
                None => return None,
            }
        };
        self.caller_ref = echoable_caller_tag(&tag);
        if prefix > 0 {
            ctx.output().consume(prefix);
        }
        Some(prefix)
    }

    /// Finishes the command whose bytes have just been written: once nothing of it is left queued
    /// (or the peer's close ended the connection), the caller tag — if there was one — is
    /// answered with the C's `{inet_reply, Port, ok | {error, Reason}}` (`inet_reply_finish`,
    /// `inet_drv.c:2614`-`:2637`) and the strip state is reset for the next command.
    ///
    /// A command that carried **no** tag stages nothing, exactly as the C's `inet_reply_finish`
    /// does with a `NULL` caller.
    fn settle_command(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if !ctx.output().is_empty() {
            return Ok(());
        }
        let Some(tag) = self.caller_ref.take() else {
            return Ok(());
        };
        let reply = match self.peer_close.send_reply() {
            // The command itself (or a previous one) found the peer gone: the C answers the
            // caller with the send ladder's reply — `inet_reply_error`/`inet_reply_error_am`,
            // `inet_drv.c:12689`-`:12692` and `:13664`-`:13669` — and with `{error,enotconn}`
            // once that one-shot is spent.
            Some(reason) => InetReply::Error(reason),
            None => InetReply::Ok,
        };
        ctx.deliver_completion(CompletionEvent::InetReply {
            caller_ref: Some(tag),
            reply,
        })
    }
}

#[cfg(test)]
#[path = "async_tests.rs"]
pub(crate) mod async_tests;
#[cfg(test)]
#[path = "passive_recv_tests.rs"]
mod passive_recv_tests;
