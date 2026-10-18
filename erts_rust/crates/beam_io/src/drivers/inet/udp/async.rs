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

//! The `udp_inet` driver's **asynchronous-operation** half — `PACKET_REQ_RECV`(60) and the
//! `{inet_async, Port, Id, Result}` completion it produces, plus the send path's
//! `{inet_reply, Port, Reply, Tag}`.
//!
//! The queue machinery itself is **not** re-implemented here: it is the driver-agnostic
//! [`AsyncQueue`](super::super::op_queue::AsyncQueue) the `tcp_inet` half uses (the C keeps both
//! drivers in one file and shares `enq_async`/`NEW_ASYNC_ID` between them, `inet_drv.c:1549`).
//! What this module owns is the *datagram* body of each production point:
//!
//! * the guard ladder of `packet_inet_ctl`'s `PACKET_REQ_RECV` arm (`inet_drv.c:14968`-`:15010`),
//!   in the C's own order;
//! * a completed receive's **shape** — the datagram plus its *packed source address*
//!   ([`ReadMessage::header`]) as one read message, which is what makes an `[list]` port's
//!   `Data` the C's single flat list `[1, PortHi, PortLo, A, B, C, D, …data]`
//!   (`inet_async_binary_data`'s `INET_MODE_LIST` arm, `:3900`) and a `[binary]` port's the
//!   improper `[1, PortHi, PortLo, A, B, C, D | <<Payload>>]` (its `LOAD_STRING_CONS` arm);
//! * the three production points (readiness, the port timer, the round's async drive), with
//!   `packet_inet_recv`'s passive-mode "one datagram per operation" discipline
//!   (`packet_inet_input`, `:15500`-`:15530`);
//! * the teardown drain — `packet_inet_stop`'s `async_error_am_all(desc, am_closed)`
//!   (`:14548`);
//! * the send path's `init_caller` strip and `inet_reply_finish` reply
//!   (`:1820`-`:1890`, `:2614`-`:2637`).
//!
//! # Why the completion can never ride the control reply
//!
//! The control reply is `[1] ++ <<Id:16/big>>` and **nothing else** — the datagram is not there
//! yet, and when it is already there the C still answers the id first and delivers the data as a
//! message. So [`UdpInetDriver::async_packet_recv`] answers the id, arms read readiness, and lets
//! a *later* round ([`crate::Driver::drive_async`], a readiness event, or the timer) produce the
//! completion. That also means a `Timeout` of `0` reports `{error, timeout}` at the next round,
//! after the reply, exactly as the C's `driver_set_timer(port, 0)` does.

use std::time::Instant;

use beam_common::Result;

use super::super::op_queue::{
    async_ok_reply, deadline_for, drain_closed, length_too_large, parse_recv_operand, AsyncQueue,
    RecvOp,
};
use super::super::wire::{ctl_xerror, pack_address, split_caller_ref_parts};
use super::{UdpInetDriver, DATAGRAM_CHUNK};
use crate::completion::{CompletionEvent, InetAsyncResult, InetReason, InetReply};
use crate::driver::DriverContext;
use crate::poller::Interest;
use crate::port_task::ReadMessage;

impl UdpInetDriver {
    /// `PACKET_REQ_RECV`(60): queue one asynchronous datagram receive and answer `[1] ++ id`.
    ///
    /// The guard ladder is the C's, in the C's order (`inet_drv.c:14968`-`:15010`):
    ///
    /// 1. no socket ⇒ `[0]exbadport` (`!IS_OPEN`, `:14980`);
    /// 2. `active || len != 8` ⇒ `[0]einval` (`:14988`) — an active-mode socket has no passive
    ///    receive, and the operand is the eight-byte `Timeout(4), Length(4)` record;
    /// 3. a `Length` above the C's `TCP_MAX_PACKET_SIZE` ⇒ `[0]enomem` (`:12301`, the bound the
    ///    stream arm applies to the same operand);
    /// 4. the single receive slot is taken ⇒ `[0]ealready` (`enq_async` returning `< 0`,
    ///    `:15003`).
    ///
    /// The id is taken **before** step 4, so a refused enqueue still advances
    /// `NEW_ASYNC_ID`'s counter — measured on the oracle: `[1,0,0]`, then `[0]ealready`, then
    /// `[1,0,2]` once the first operation completes.
    ///
    /// # Errors
    ///
    /// The poller's own failure when read readiness cannot be armed (`BeamError::Io`).
    pub(super) fn async_packet_recv(
        &mut self,
        ctx: &mut DriverContext<'_>,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        if self.socket.is_none() {
            return Ok(ctl_xerror("exbadport"));
        }
        if self.options.active != 0 || arg.len() != 8 {
            return Ok(ctl_xerror("einval"));
        }
        let Some((timeout_ms, length)) = parse_recv_operand(arg) else {
            return Ok(ctl_xerror("einval"));
        };
        // The C's datagram arm never reads `Length` (the comment at `:14993` says so), but the
        // same operand's bound is the one the stream arm enforces, and an over-long request is
        // never satisfiable — report the C's own refusal instead of queueing it.
        if length_too_large(length) {
            return Ok(ctl_xerror("enomem"));
        }
        let id = self.async_ops.next_id();
        let op = RecvOp {
            id,
            length,
            deadline: deadline_for(timeout_ms),
        };
        if !self.async_ops.enqueue_recv(op) {
            return Ok(ctl_xerror("ealready"));
        }
        // The C's `sock_select(desc, FD_READ, 1)` (`:15006`): keep read readiness armed so the
        // datagram's arrival re-drives the port. The completion itself is produced at a later
        // round — never here (see the module docs).
        self.set_interest(ctx, Interest::READABLE)?;
        Ok(async_ok_reply(id))
    }

    /// Completes the pending receive operation if a datagram has arrived.
    ///
    /// This is the C's passive `packet_inet_input` body (`inet_drv.c:15500`-`:15530`): **one**
    /// `recvfrom(2)`, and on success `inet_input_count(desc, n)` followed by the
    /// `{inet_async, Port, Id, {ok, Address ++ Payload}}` answer. Returns whether an operation
    /// completed.
    ///
    /// A receive that finds nothing (`EAGAIN`) leaves the operation queued; a real syscall error
    /// completes it with `{error, Reason}` — the C's `async_error(desc, err)` (`:15516`) — and
    /// removes it, so a poisoned socket cannot fail every later round.
    ///
    /// # Errors
    ///
    /// The typed `unsupported` of a context with no completion sink, plus the raw read failure
    /// when it carries no `errno` to report (never fabricated into a reason).
    pub(super) fn try_complete_recv(&mut self, ctx: &mut DriverContext<'_>) -> Result<bool> {
        let Some(op) = self.async_ops.recv() else {
            return Ok(false);
        };
        let Some(fd) = self.socket else {
            // The socket went away with the operation pending: the C's `async_error_am_all`
            // answer (`:14548`), never a caller left waiting.
            self.async_ops.take_recv();
            ctx.deliver_completion(CompletionEvent::InetAsync {
                id: op.id,
                result: InetAsyncResult::Error(InetReason::Closed),
            })?;
            return Ok(true);
        };
        let mut buf = [0u8; DATAGRAM_CHUNK];
        loop {
            match super::super::sys::recv_from(fd, &mut buf) {
                Ok((len, source)) => {
                    self.async_ops.take_recv();
                    // `inet_input_count(desc, n)` at `inet_drv.c:15513`/`:15526`
                    // (`udp_deliver`): the received **datagram payload** length.
                    self.stat.input(len);
                    // The packed source address is the read's *header*, so a `[list]` port's
                    // `Data` is one flat list and a `[binary]` port's the C's improper
                    // `[Address | <<Payload>>]` (`inet_async_binary_data`, `:3870`-`:3960`).
                    let header = pack_address(source.port, source.addr);
                    let body = buf[..len].to_vec();
                    ctx.deliver_completion(CompletionEvent::InetAsync {
                        id: op.id,
                        result: InetAsyncResult::Data(ReadMessage {
                            header,
                            body: Some(body),
                        }),
                    })?;
                    return Ok(true);
                }
                Err(err) => match err.raw_os_error() {
                    Some(libc::EINTR) => continue,
                    // Nothing pending: the operation waits for the next readiness edge.
                    Some(libc::EAGAIN) => return Ok(false),
                    other => {
                        self.async_ops.take_recv();
                        let Some(errno) = other else {
                            return Err(crate::error::io(err));
                        };
                        let reason = InetReason::from_errno(errno);
                        ctx.deliver_completion(CompletionEvent::InetAsync {
                            id: op.id,
                            result: InetAsyncResult::Error(reason),
                        })?;
                        return Ok(true);
                    }
                },
            }
        }
    }

    /// Completes every operation whose deadline has passed — the port's `TIMEOUT` task, i.e. the
    /// C's `packet_inet_timeout` (`inet_drv.c:15012`-`:15024`).
    ///
    /// Before reporting the timeout the operation gets one last non-blocking attempt: the C runs
    /// `packet_inet_input` at enqueue time, so a datagram that is already there is delivered
    /// rather than reported as a timeout. That is what makes a zero timeout see pending data
    /// instead of `{error, timeout}`.
    ///
    /// # Errors
    ///
    /// Whatever [`UdpInetDriver::try_complete_recv`] reports.
    pub(super) fn async_timeout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let now = Instant::now();
        if !self.async_ops.recv().is_some_and(|op| op.is_due(now)) {
            return Ok(());
        }
        if self.try_complete_recv(ctx)? {
            return Ok(());
        }
        let Some(op) = self.async_ops.take_recv() else {
            return Ok(());
        };
        ctx.deliver_completion(CompletionEvent::InetAsync {
            id: op.id,
            result: InetAsyncResult::Error(InetReason::Timeout),
        })
    }

    /// The round's async drive point: one non-blocking attempt per pending operation, answering
    /// how many completed.
    ///
    /// It never spins — the single attempt per call is the whole work — and once the queue is
    /// empty [`crate::Driver::has_pending_async`] turns `false`, so the registry stops driving a
    /// finished port.
    ///
    /// # Errors
    ///
    /// Whatever [`UdpInetDriver::try_complete_recv`] reports.
    pub(super) fn drive_async_ops(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        if self.try_complete_recv(ctx)? {
            return Ok(1);
        }
        Ok(0)
    }

    /// The read-readiness callback's body: complete the pending receive, if any.
    ///
    /// With **no** operation queued this stays the documented no-op the passive driver has always
    /// had: a datagram's source address has no channel in the port input queue, so a passive port
    /// is served by the runtime issuing the `recv` verb, not by the driver pushing bytes.
    ///
    /// # Errors
    ///
    /// Whatever [`UdpInetDriver::try_complete_recv`] reports.
    pub(super) fn async_ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if self.async_ops.recv().is_some() {
            self.try_complete_recv(ctx)?;
        }
        Ok(())
    }

    /// Completes **every** pending operation with `{error, closed}` — the C's `async_error_am_all`
    /// in `packet_inet_stop` (`inet_drv.c:14548`), which runs **before** the descriptor goes away.
    ///
    /// # Errors
    ///
    /// The typed `unsupported` of a context with no completion sink.
    pub(super) fn drain_pending_closed(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        drain_closed(&mut self.async_ops, ctx)
    }

    /// The `erlang:port_command/2` write path: strip one caller tag, write the datagram, then
    /// answer the caller with the C's `{inet_reply, Port, Reply, Tag}`.
    ///
    /// Returns the number of bytes **consumed from the port's output queue** — the tag prefix plus
    /// the command the socket accepted — which is what [`crate::Port`]'s write-accounting contract
    /// compares against the queue's shrinkage.
    ///
    /// # Errors
    ///
    /// Whatever [`UdpInetDriver::write_pending`] reports (the socket-less refusal is unchanged:
    /// nothing is stripped and the command's bytes stay queued), or the typed `unsupported` of a
    /// context with no completion sink.
    pub(super) fn write_command(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let stripped = self.strip_caller_prefix(ctx);
        let written = self.write_pending(ctx)?;
        self.settle_command(ctx)?;
        Ok(stripped + written)
    }

    /// Strips **one** leading caller tag from the head of the output queue, returning how many
    /// bytes it consumed (`0` when there was nothing to strip).
    ///
    /// The C's `init_caller`/`init_caller_iov` (`inet_drv.c:1820`-`:1890`): the wire form is
    /// `[n:be16][tag][command]`, the tag being `term_to_binary(Mref, [local])`. The tag is kept so
    /// the later `{inet_reply, S, Reply, Tag}` can echo it **verbatim** — this crate never decodes
    /// the ETF image — and the command is left in the queue to be written.
    ///
    /// It runs **once per command**, not once per write attempt, so a short write's remainder is
    /// never re-prefixed; [`UdpInetDriver::caller_stripped`] records that, and
    /// [`UdpInetDriver::settle_command`] clears it when the command is done with.
    ///
    /// A payload carrying no prefix — including the `[0,0]` zero-length-tag form — is written
    /// verbatim and produces **no** completion (the C's `inet_reply_finish` sends nothing without a
    /// caller, `:2628`), and a socket-less port is not inspected at all, so its refusal keeps every
    /// byte queued.
    fn strip_caller_prefix(&mut self, ctx: &mut DriverContext<'_>) -> usize {
        if self.caller_stripped || self.socket.is_none() {
            return 0;
        }
        self.caller_stripped = true;
        let (prefix, tag) = {
            let front = ctx.output().front();
            match split_caller_ref_parts(front) {
                Some((tag, rest)) => (front.len() - rest.len(), tag.to_vec()),
                None => return 0,
            }
        };
        if prefix == 0 {
            return 0;
        }
        self.caller_ref = Some(tag);
        ctx.output().consume(prefix);
        prefix
    }

    /// Finishes the command whose bytes have just been written: once nothing of it is left queued,
    /// the caller tag — if there was one — is answered with the C's
    /// `{inet_reply, Port, ok | {error, Reason}, Tag}` (`inet_reply_finish`, `inet_drv.c:2614`),
    /// and the strip state is reset for the next command.
    ///
    /// A command that carried **no** tag stages nothing, exactly as the C's `inet_reply_finish`
    /// does with a `NULL` caller.
    ///
    /// # Errors
    ///
    /// The typed `unsupported` of a context with no completion sink.
    fn settle_command(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if !ctx.output().is_empty() {
            return Ok(());
        }
        self.caller_stripped = false;
        let Some(tag) = self.caller_ref.take() else {
            // Nothing to answer, but a send failure still has to be forgotten so it cannot be
            // attributed to the *next* tagged command.
            self.send_error = None;
            return Ok(());
        };
        let reply = match self.send_error.take() {
            // `inet_reply_error(desc, err)` — the C's datagram send failure (`:15170`).
            Some(errno) => InetReply::Error(InetReason::from_errno(errno)),
            None => InetReply::Ok,
        };
        ctx.deliver_completion(CompletionEvent::InetReply {
            caller_ref: Some(tag),
            reply,
        })
    }
}
