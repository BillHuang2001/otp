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
use super::wire::{ctl_ok, ctl_xerror, echoable_caller_tag, split_command_prefix};
use super::{Interest, TcpInetDriver};
use crate::completion::{CompletionEvent, InetAsyncResult, InetReason, InetReply};
use crate::driver::DriverContext;
use crate::port_failure::DriverFailure;
use crate::error;
use crate::port_task::ReadMessage;

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
pub(super) fn drain_closed(
    queue: &mut AsyncQueue,
    ctx: &mut DriverContext<'_>,
) -> Result<()> {
    for id in queue.take_all() {
        ctx.deliver_completion(CompletionEvent::InetAsync {
            id,
            result: InetAsyncResult::Error(InetReason::Closed),
        })?;
    }
    Ok(())
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
    /// 1. not connected ⇒ the C's delayed-close reply (`[0]econnreset`/`[0]closed` once, then
    ///    `[0]enotconn`) or `[0]enotconn` (`:12279`-`:12286`);
    /// 2. `active || len != 8` ⇒ `[0]einval` (`:12290`);
    /// 3. a framing header type with a non-zero `Length` ⇒ `[0]einval` (`:12296`) — "exactly n
    ///    bytes" only exists for the raw header type;
    /// 4. `Length > TCP_MAX_PACKET_SIZE` ⇒ `[0]enomem` (`:12301`);
    /// 5. the receive slot is taken ⇒ `[0]ealready` (`:12307`).
    ///
    /// The id is taken **before** step 5, so a refused enqueue still advances the counter.
    pub(super) fn async_recv(
        &mut self,
        ctx: &mut DriverContext<'_>,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        if let Some(name) = self.peer_close.recv_reply() {
            return Ok(ctl_xerror(name));
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
            let payload = if framed {
                match frame::extract(&mut self.pending_input, &frame::options(&self.options)) {
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
                // `inet_input_count(desc, len)` (`inet_drv.c:13064`, `tcp_deliver`): the delivered
                // payload, the framing header already stripped.
                self.stat.input(payload.len());
                ctx.deliver_completion(CompletionEvent::InetAsync {
                    id: op.id,
                    result: InetAsyncResult::Data(ReadMessage::body_only(payload)),
                })?;
                return Ok(true);
            }
            if self.peer_close.seen {
                break;
            }
            // Not enough buffered bytes: read what the socket has and try once more.
            let before = self.pending_input.len();
            let mut bytes = Vec::new();
            self.read_bytes(ctx, &mut bytes)?;
            self.pending_input.extend_from_slice(&bytes);
            if self.pending_input.len() == before {
                // `EAGAIN`: no progress, so the operation waits for the next readiness edge.
                break;
            }
        }
        if self.async_ops.recv().is_some() && self.peer_close.seen {
            // The peer went away with the operation pending: the C's `tcp_recv_closed` answers
            // `async_error_am(am_closed)` (`inet_drv.c:13180`).
            if let Some(op) = self.async_ops.take_recv() {
                ctx.deliver_completion(CompletionEvent::InetAsync {
                    id: op.id,
                    result: InetAsyncResult::Error(InetReason::Closed),
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
    /// floor's `{Port, {data, …}}` path).
    pub(super) fn async_ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        self.serve_accepts(ctx)?;
        if !self.state.is_connected() {
            return Ok(());
        }
        if self.async_ops.recv().is_some() {
            self.try_complete_recv(ctx)?;
            return Ok(());
        }
        self.read_available(ctx).map(|_read| ())
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
        if !self.state.is_connected() {
            return Ok(ctl_xerror("enotconn"));
        }
        if let Some(name) = self.peer_close.recv_reply() {
            return Ok(ctl_xerror(name));
        }
        if !arg.is_empty() {
            return Err(error::badarg());
        }
        if self.framed().is_some() {
            return self.verb_recv_framed(ctx);
        }
        let read = self.read_available(ctx)?;
        if read == 0 && self.peer_close.seen {
            // This read is what found the peer gone, so it answers `closed` itself.
            self.peer_close.recv_reported = true;
            return Ok(ctl_xerror("closed"));
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
            match frame::extract(&mut self.pending_input, &framing)? {
                Some(payload) => {
                    // `inet_input_count(desc, len)` at `inet_drv.c:13064` (`tcp_deliver`): the
                    // **delivered payload** length — the framing header already stripped.
                    self.stat.input(payload.len());
                    ctx.input().push(&payload);
                    return Ok(ctl_ok());
                }
                None => {
                    // Not a whole frame yet: refill the framing buffer from the socket (in a
                    // framing mode `read_available` appends there) and try to decode again.
                    let read = self.read_available(ctx)?;
                    if read == 0 {
                        if self.peer_close.seen {
                            // This read is what found the peer gone, so it answers `closed`.
                            self.peer_close.recv_reported = true;
                            return Ok(ctl_xerror("closed"));
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
    /// The `erlang:port_command/2` write path: strip one caller tag, write, then answer the
    /// caller with the C's `{inet_reply, Port, Reply[, Tag]}`.
    ///
    /// The C's `tcp_inet_commandv` (`inet_drv.c:12656`-`:12705`) runs `init_caller` on the
    /// command's bytes first (`:1820`-`:1890`), writes what is left and finishes the caller in
    /// `inet_reply_finish` (`:2614`-`:2637`). This is that whole sequence.
    ///
    /// Returns the number of bytes **consumed from the port's output queue** — the tag prefix
    /// plus what the socket accepted — which is what [`crate::Port`]'s write-accounting contract
    /// compares against the queue's shrinkage.
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
        if !self.state.is_connected() {
            // The C's `!IS_CONNECTED` arm (`:12674`-`:12683`): the command is accepted (the port
            // charged `prt->bytes_out += size` for it) but never written, and the caller learns
            // the outcome from `inet_reply_error(ENOTCONN)` — a `{inet_reply, Port,
            // {error,enotconn}, Tag}` message, staged only when a tag can be echoed.
            let discarded = ctx.output().len();
            ctx.output().consume(discarded);
            self.caller_stripped = false;
            if let Some(tag) = self.caller_ref.take() {
                ctx.deliver_completion(CompletionEvent::InetReply {
                    caller_ref: Some(tag),
                    reply: InetReply::Error(InetReason::from_errno(libc::ENOTCONN)),
                })?;
            }
            return Ok(stripped + discarded);
        }
        let written = self.write_pending(ctx)?;
        self.settle_command(ctx)?;
        Ok(stripped + written)
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
        let reply = if self.peer_close.seen {
            // `tcp_send_error`/the delayed-close arm: the peer is gone, so the command that was
            // accepted into the queue never reached it (`inet_reply_error_am(…, am_closed)`).
            InetReply::Error(InetReason::Closed)
        } else {
            InetReply::Ok
        };
        ctx.deliver_completion(CompletionEvent::InetReply {
            caller_ref: Some(tag),
            reply,
        })
    }
}

#[cfg(test)]
#[path = "async_tests.rs"]
mod async_tests;
