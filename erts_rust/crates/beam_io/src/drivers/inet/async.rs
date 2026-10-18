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
//! * **Accepts are a multi-operation list**: `INET_REQ_ACCEPT` (26) uses `enq_multi_op`
//!   (`:12194`/`:12209`), so N pending accepts each get their own id, all answer `[1] ++ id` and
//!   complete **in order** as connections arrive. The descriptor state carries the C's
//!   `INET_F_ACC` (one pending accept) / `INET_F_MULTI_CLIENT` (two or more) bits while that list
//!   is non-empty (`INET_STATE_ACCEPTING`/`INET_STATE_MULTI_ACCEPTING`, `:1031`-`:1032`), which is
//!   what `INET_REQ_GETSTATUS` reports ([`AsyncQueue::accept_state_bits`]).
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
//! 2. **the port timer** — `Driver::next_timeout` arms the port's `TIMEOUT` task from the
//!    operation's deadline and `Driver::timeout` then reports `{error, timeout}` (the C's
//!    `driver_set_timer` + `tcp_inet_recv_timeout`, `:12480`);
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

use super::frame;
use super::sys::{accept_socket, close_fd};
use super::wire::{ctl_ok, ctl_xerror};
use super::{Interest, TcpInetDriver};
use crate::completion::{CompletionEvent, InetAsyncResult, InetReason};
use crate::driver::DriverContext;
use crate::drivers::fd::set_nonblocking;
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
    fn is_due(self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }
}

/// One queued `accept` operation (the C's `enq_multi_op` list).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct AcceptOp {
    /// The 16-bit id the control reply carried.
    pub(super) id: u16,
    /// The operation's deadline, or `None` for the C's `INET_INFINITY`.
    pub(super) deadline: Option<Instant>,
}

impl AcceptOp {
    /// Whether this operation's deadline has passed at `now`.
    fn is_due(self, now: Instant) -> bool {
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
    /// The C's `enq_multi_op` accept list, in enqueue order.
    accepts: Vec<AcceptOp>,
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
        if self.recv.is_some() {
            return false;
        }
        self.recv = Some(op);
        true
    }

    /// The queued accept operations, oldest first.
    pub(super) fn accepts(&self) -> &[AcceptOp] {
        &self.accepts
    }

    /// Appends an accept operation — the C's `enq_multi_op`, which never refuses (`:12194`).
    pub(super) fn enqueue_accept(&mut self, op: AcceptOp) {
        self.accepts.push(op);
    }

    /// Takes the oldest queued accept operation.
    pub(super) fn take_accept(&mut self) -> Option<AcceptOp> {
        if self.accepts.is_empty() {
            None
        } else {
            Some(self.accepts.remove(0))
        }
    }

    /// Whether any operation is pending on either queue.
    pub(super) fn has_pending(&self) -> bool {
        self.recv.is_some() || !self.accepts.is_empty()
    }

    /// The descriptor-state bits the queued accepts add to `INET_REQ_GETSTATUS`
    /// (`inet_drv.c:1031`-`:1032`, `:11160`).
    ///
    /// One pending accept is `INET_STATE_ACCEPTING` (`INET_F_LISTEN | INET_F_ACC`) and two or more
    /// are `INET_STATE_MULTI_ACCEPTING` (adding `INET_F_MULTI_CLIENT`), so a listener with two
    /// accepts queued reports `[1,0,0,1,41]` — measured on the oracle.
    pub(super) fn accept_state_bits(&self) -> u32 {
        /// `INET_F_ACC` (`inet_drv.c:797`).
        const INET_F_ACC: u32 = 0x0020;
        /// `INET_F_MULTI_CLIENT` (`:800`).
        const INET_F_MULTI_CLIENT: u32 = 0x0100;
        match self.accepts.len() {
            0 => 0,
            1 => INET_F_ACC,
            _ => INET_F_ACC | INET_F_MULTI_CLIENT,
        }
    }

    /// The earliest deadline among the pending operations — what the driver declares through
    /// `Driver::next_timeout`, which the port mirrors into its single `TIMEOUT` task.
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        let recv = self.recv.and_then(|op| op.deadline);
        self.accepts
            .iter()
            .filter_map(|op| op.deadline)
            .chain(recv)
            .min()
    }

    /// Every pending operation's id — the receive slot first, then the accepts in enqueue order —
    /// leaving the queue empty.
    pub(super) fn take_all(&mut self) -> Vec<u16> {
        let mut ids = Vec::new();
        if let Some(op) = self.recv.take() {
            ids.push(op.id);
        }
        ids.extend(self.accepts.drain(..).map(|op| op.id));
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

/// The C's `Timeout(4)` operand of `INET_REQ_ACCEPT`(26), or `None` when the operand is not
/// exactly four bytes.
pub(super) fn parse_timeout_operand(arg: &[u8]) -> Option<i32> {
    let [t0, t1, t2, t3] = arg else {
        return None;
    };
    Some(i32::from_be_bytes([*t0, *t1, *t2, *t3]))
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

    /// The descriptor-state bits the queued accepts add to `INET_REQ_GETSTATUS`.
    pub(super) fn accept_state_bits(&self) -> u32 {
        self.async_ops.accept_state_bits()
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

    /// `INET_REQ_ACCEPT`(26): queue one asynchronous accept and answer `[1] ++ id`.
    ///
    /// The C's guard is one combined test — the socket must be listening **and** the operand must
    /// be the four-byte timeout, both answering `[0]einval` (`inet_drv.c:12160`-`:12170`) — and the
    /// operation joins the multi-op accept list (`enq_multi_op`, `:12194`/`:12209`), so repeats
    /// never answer `ealready`.
    pub(super) fn async_accept(&mut self, arg: &[u8]) -> Result<Vec<u8>> {
        if !self.state.is_listening() || arg.len() != 4 {
            return Ok(ctl_xerror("einval"));
        }
        let Some(timeout_ms) = parse_timeout_operand(arg) else {
            return Ok(ctl_xerror("einval"));
        };
        let id = self.async_ops.next_id();
        self.async_ops.enqueue_accept(AcceptOp {
            id,
            deadline: deadline_for(timeout_ms),
        });
        Ok(async_ok_reply(id))
    }

    /// Accepts one connection if the listener has one, completing the oldest queued accept.
    ///
    /// Returns whether an operation completed. The accepted descriptor is put into non-blocking
    /// mode and handed to the runtime **inside the completion**
    /// ([`InetAsyncResult::Accepted`]): this driver never stores it, so no teardown path can close
    /// it twice.
    ///
    /// # Errors
    ///
    /// Returns the typed I/O failure of a descriptor that cannot be made non-blocking — after
    /// completing the operation with that errno, so the descriptor is still closed here and the
    /// caller learns what happened.
    fn accept_ready_once(&mut self, ctx: &mut DriverContext<'_>) -> Result<bool> {
        let Some(op) = self.async_ops.accepts().first().copied() else {
            return Ok(false);
        };
        let listener = self.socket.ok_or_else(error::badarg)?;
        match accept_socket(listener) {
            Ok(fd) => {
                self.async_ops.take_accept();
                if let Err(err) = set_nonblocking(fd) {
                    // Nothing can be handed over: close what we took and report the errno, the
                    // C's `inet_async_error` shape (`:2539`).
                    close_fd(fd);
                    let reason = InetReason::from_errno(err.raw_os_error().unwrap_or(libc::EINVAL));
                    ctx.deliver_completion(CompletionEvent::InetAsync {
                        id: op.id,
                        result: InetAsyncResult::Error(reason),
                    })?;
                    return Ok(true);
                }
                ctx.deliver_completion(CompletionEvent::InetAsync {
                    id: op.id,
                    result: InetAsyncResult::Accepted { fd },
                })?;
                Ok(true)
            }
            Err(err) if err.raw_os_error() == Some(libc::EAGAIN) => Ok(false),
            Err(err) => {
                // A real accept failure: the C completes the operation with the errno
                // (`async_error(desc, err)`) and leaves the descriptor listening.
                self.async_ops.take_accept();
                let reason = InetReason::from_errno(err.raw_os_error().unwrap_or(libc::EINVAL));
                ctx.deliver_completion(CompletionEvent::InetAsync {
                    id: op.id,
                    result: InetAsyncResult::Error(reason),
                })?;
                Ok(true)
            }
        }
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
        let Some(op) = self.async_ops.recv() else {
            return Ok(false);
        };
        let framed = self.framed().is_some();
        loop {
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
            if self.peer_close.seen || self.async_ops.recv().is_none() {
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
        if self.peer_close.seen && self.async_ops.recv().is_some() {
            // The peer went away with the operation pending: the C's `tcp_recv_closed` answers
            // `async_error_am(am_closed)` (`inet_drv.c:13180`).
            if let Some(op) = self.async_ops.take_recv() {
                ctx.deliver_completion(CompletionEvent::InetAsync {
                    id: op.id,
                    result: InetAsyncResult::Error(InetReason::Closed),
                })?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Completes every operation whose deadline has passed — the port's `TIMEOUT` task, i.e. the
    /// C's `tcp_inet_recv_timeout`/accept timeout (`inet_drv.c:12480`).
    ///
    /// Before reporting a receive timeout the operation is given one last chance: the C attempts
    /// the receive at enqueue time, so bytes that are already there are delivered rather than
    /// reported as a timeout (which is exactly what makes a zero timeout see pending data instead
    /// of `{error, timeout}`).
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
        loop {
            let due = self
                .async_ops
                .accepts()
                .first()
                .copied()
                .filter(|op| op.is_due(now));
            let Some(op) = due else { break };
            self.async_ops.take_accept();
            ctx.deliver_completion(CompletionEvent::InetAsync {
                id: op.id,
                result: InetAsyncResult::Error(InetReason::Timeout),
            })?;
        }
        Ok(())
    }

    /// The round's async drive point: completes what already-arrived data allows, answering how
    /// many operations completed.
    ///
    /// It never spins: each call makes at most one non-blocking `accept(2)`/`read(2)` attempt per
    /// pending operation, and once the queue is empty
    /// [`Driver::has_pending_async`](crate::driver::Driver::has_pending_async) turns `false` so the
    /// registry stops calling it.
    pub(super) fn drive_async_ops(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let mut completed = 0;
        while self.accept_ready_once(ctx)? {
            completed += 1;
        }
        if self.try_complete_recv(ctx)? {
            completed += 1;
        }
        Ok(completed)
    }

    /// The read-readiness callback's body: serve the accepts a listener has queued, then either
    /// complete the pending receive or move the bytes into the port's input queue (the passive
    /// floor's `{Port, {data, …}}` path).
    pub(super) fn async_ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        while self.accept_ready_once(ctx)? {}
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



#[cfg(test)]
#[path = "async_tests.rs"]
mod async_tests;
