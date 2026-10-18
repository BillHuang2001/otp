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

//! The `tcp_inet` **accept state machine** — the C's `INET_REQ_ACCEPT`(26) multi-operation list,
//! its per-port timers and the listener's readiness arming.
//!
//! # The accept list
//!
//! An accept cannot be answered by its control call: the connection is not there yet. The C
//! enqueues with `enq_multi_op` (`inet_drv.c:12194`/`:12209`), so N pending accepts each get their
//! own id — the shared descriptor counter (`AsyncQueue::next_id`) is taken by the caller — every
//! one answers `[1] ++ <<Id:16/big>>` (never the single receive slot's `[0]ealready`) and they
//! complete **in enqueue order**. The C completes one as `{ok, Port2}` after making a second port
//! (`tcp_inet_copy` + `driver_create_port` + `async_ok_port`, `:12236`-`:12252`); a
//! [`Driver`](crate::Driver) cannot create a port, so the accepted descriptor rides
//! [`InetAsyncResult::Accepted`] and the runtime opens that port itself.
//!
//! # The sticky multi-accept bit
//!
//! `desc->inet.state` carries `INET_F_ACC` (0x20 — an accept is pending) and `INET_F_MULTI_CLIENT`
//! (0x100) — `INET_STATE_ACCEPTING`/`INET_STATE_MULTI_ACCEPTING` (`:1031`-`:1032`) — which is what
//! `INET_REQ_GETSTATUS`(16) reports. The multi bit latches when the **second** accept joins the
//! list (the C's `enq_multi_op(...); desc->inet.state = INET_STATE_MULTI_ACCEPTING;`,
//! `:12189`-`:12194`) and is restored to `INET_STATE_LISTENING` only where the list is seen to have
//! **drained** (`tcp_inet_multi_timeout`, `:12607`-`:12611`, and `tcp_inet_input`'s multi arm,
//! `:13536`-`:13540`), so a survivor keeps reporting `297` after its sibling timed out or was
//! served ([`AcceptQueue::state_bits`], measured in `accept_tests.rs`).
//!
//! # The timers are per port, not per operation
//!
//! Each **finite** `Timeout(4)` operand becomes one sorted `MultiTimerData`
//! (`add_multi_timer`, `:15744`-`:15790`) — an `INET_INFINITY` operand adds none, and the entries
//! are kept beside the operations rather than on them because a firing consumes an **entry** and
//! answers the **head** operation (`fire_multi_timers` `:15650` → `tcp_inet_multi_timeout`
//! `:12600`), leaving the head's own entry armed for the next head: `[500, 50]` reports id 0 at
//! ~51 ms and id 1 at ~501 ms. Only a **successful** completion cancels its own entry
//! ([`AcceptQueue::take`], the C's `deq_multi_op` + `remove_multi_timer`, `:13553`-`:13559`).
//!
//! # Readiness
//!
//! `FD_ACCEPT` is `ERL_DRV_READ` (`:631`), i.e. the listener's *read* readiness:
//! [`TcpInetDriver::async_accept`] re-arms it on **every** enqueue (the C's
//! `sock_select(INETP(desc), FD_ACCEPT, 1)`, `:12224`) and [`TcpInetDriver::disarm_accepts`] drops
//! it once the list has drained. A ghost `EAGAIN` changes nothing (the C's `goto done`).
//!
//! The **named** `accept` verb is the older read-now shape, a separate contract:
//! [`TcpInetDriver::verb_accept`] transfers this port to the connection and answers
//! `[1] ++ fd_be32` instead of an id.

use std::time::Instant;

use beam_common::Result;

use super::op_queue::{async_ok_reply, deadline_for};
use super::sys::{accept_socket, close_fd};
use super::wire::{ctl_error, ctl_ok, ctl_xerror};
use super::{Interest, TcpInetDriver};
use crate::completion::{CompletionEvent, InetAsyncResult, InetReason};
use crate::driver::DriverContext;
use crate::drivers::fd::set_nonblocking;
use crate::error;

/// One queued `accept` operation (an entry of the C's `enq_multi_op` list).
///
/// A queued accept's own deadline decides **when** the port's single timer fires, never **which**
/// operation is reported: the C keeps one `MultiTimerData` per finite deadline in a list sorted by
/// `when` ([`AcceptQueue::timers`], `add_multi_timer`, `inet_drv.c:15744`), and a firing reports
/// the head of the accept list whatever deadline woke it (`tcp_inet_multi_timeout` →
/// `remove_multi_op`, `:12600`). See [`AcceptQueue::timeout_due`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct AcceptOp {
    /// The 16-bit id the control reply carried.
    pub(super) id: u16,
    /// The operation's deadline, or `None` for the C's `INET_INFINITY`.
    ///
    /// This is the operation's **own** timer entry (the C's `opp->op.tmo.mtd`), which only a
    /// *successful* completion cancels (`deq_multi_op` + `remove_multi_timer`, `:13553`-`:13559`);
    /// a firing reports the list head and leaves the head's own entry armed
    /// (`remove_multi_op` with `timeout_p == NULL`, `:12603`). The timer list itself is
    /// [`AcceptQueue::timers`].
    pub(super) deadline: Option<Instant>,
}

/// The accept half of one socket descriptor's async-operation queue: the C's `enq_multi_op`
/// accept list, its `desc->mtd` timer entries and its sticky state bit.
///
/// Owned by the shared `AsyncQueue` beside the receive slot — the descriptor has **one**
/// `NEW_ASYNC_ID` counter and **one** port timer, so the two halves cannot be separated further.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(super) struct AcceptQueue {
    /// The C's `enq_multi_op` accept list, in enqueue order.
    accepts: Vec<AcceptOp>,
    /// The C's `desc->mtd`: **one entry per finite enqueued accept deadline**, sorted by `when`
    /// (`add_multi_timer`, `inet_drv.c:15744`-`:15790`).
    ///
    /// Deliberately separate from the operations' own [`AcceptOp::deadline`]s, because the C's
    /// bookkeeping is: a firing pops the **earliest entry** and removes the list **head**
    /// (`fire_multi_timers` `:15650` → `tcp_inet_multi_timeout` `:12600`, whose
    /// `remove_multi_op(…, NULL, …)` leaves the head's own entry in `mtd`), so an entry whose
    /// operation was removed by *another* entry's firing stays armed and expires the next head.
    timers: Vec<Instant>,
    /// The C's `INET_F_MULTI_CLIENT` bit of `desc->inet.state` (`inet_drv.c:800`, the
    /// `INET_STATE_MULTI_ACCEPTING` state at `:1032`), kept **sticky** here beside the list it
    /// describes so the two cannot desync.
    ///
    /// It is latched by [`AcceptQueue::enqueue`] and cleared only by [`AcceptQueue::take`]/
    /// [`AcceptQueue::expire`] when the list is left empty and by [`AcceptQueue::take_all`] with
    /// the teardown drain — never by the list length alone.
    multi: bool,
}

impl AcceptQueue {
    /// Whether any accept is queued.
    pub(super) fn is_empty(&self) -> bool {
        self.accepts.is_empty()
    }

    /// The oldest queued accept, if any.
    fn first(&self) -> Option<AcceptOp> {
        self.accepts.first().copied()
    }

    /// Appends an accept operation — the C's `enq_multi_op`, which never refuses (`:12194`) — and
    /// registers its timer: an accept whose operand is **not** `INET_INFINITY` gets one sorted
    /// `MultiTimerData` (`add_multi_timer`, `:15744`-`:15790`), an infinite one gets none at all
    /// (`mtd` stays `NULL` at the enqueue sites, `:12198`-`:12210`).
    fn enqueue(&mut self, op: AcceptOp) {
        self.accepts.push(op);
        // The C's `INET_STATE_ACCEPTING` arm, which enqueues into the multi list and then sets
        // `desc->inet.state = INET_STATE_MULTI_ACCEPTING` (`inet_drv.c:12189`-`:12194`): the multi
        // bit appears with the **second** queued accept.
        if self.accepts.len() >= 2 {
            self.multi = true;
        }
        if let Some(deadline) = op.deadline {
            let at = self
                .timers
                .iter()
                .position(|when| *when > deadline)
                .unwrap_or(self.timers.len());
            self.timers.insert(at, deadline);
        }
    }

    /// Takes the oldest queued accept operation **and cancels its own timer** — the C's successful
    /// completion path: `deq_multi_op(…, &timeout, …)` followed by `remove_multi_timer`
    /// (`inet_drv.c:13553`-`:13559`).
    fn take(&mut self) -> Option<AcceptOp> {
        if self.accepts.is_empty() {
            return None;
        }
        let op = self.accepts.remove(0);
        if let Some(deadline) = op.deadline {
            if let Some(at) = self.timers.iter().position(|when| *when == deadline) {
                self.timers.remove(at);
            }
        }
        // `tcp_inet_input`'s multi arm restores the state only once the list is empty:
        // `if (desc->multi_first == NULL) { sock_select(…,FD_ACCEPT,0); desc->inet.state =
        // INET_STATE_LISTENING; }` (`inet_drv.c:13536`-`:13540`). A survivor keeps the bit.
        if self.accepts.is_empty() {
            self.multi = false;
        }
        Some(op)
    }

    /// The earliest armed accept timer — the C's `desc->mtd->when`, i.e. what the port's single
    /// timer is armed from (the accept half of `AsyncQueue::next_deadline`).
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.timers.first().copied()
    }

    /// The descriptor-state bits the queued accepts add to `INET_REQ_GETSTATUS`
    /// (`inet_drv.c:1031`-`:1032`, `:11160`).
    ///
    /// `INET_STATE_ACCEPTING` (`INET_F_LISTEN | INET_F_ACC`) while an accept is pending, plus
    /// `INET_F_MULTI_CLIENT` (`INET_STATE_MULTI_ACCEPTING`) once the multi bit has been latched —
    /// i.e. from the second enqueue until the list **drains**, so a listener with two accepts
    /// queued reports `[1,0,0,1,41]` (297) and so does one whose sibling timed out or was served.
    /// Measured on the oracle; see the `multi` field.
    fn state_bits(&self) -> u32 {
        /// `INET_F_ACC` (`inet_drv.c:797`).
        const INET_F_ACC: u32 = 0x0020;
        /// `INET_F_MULTI_CLIENT` (`:800`).
        const INET_F_MULTI_CLIENT: u32 = 0x0100;
        match (self.accepts.is_empty(), self.multi) {
            (true, _) => 0,
            (false, false) => INET_F_ACC,
            (false, true) => INET_F_ACC | INET_F_MULTI_CLIENT,
        }
    }

    /// Whether the port's accept timer has expired at `now` — the C's `fire_multi_timers`
    /// condition (`desc->mtd->when - erl_drv_monotonic_time(...) <= 0`) over the **earliest** armed
    /// accept timer, not over the accept list head's own deadline.
    ///
    /// The C arms the port's one timer from the **first** entry of `desc->mtd`, an accept-timer
    /// list kept sorted by `when` (`add_multi_timer`, `inet_drv.c:15744`-`:15790`), where an
    /// `INET_INFINITY` accept contributes **no** entry at all. The timer's expiry therefore means
    /// "the earliest armed accept deadline is due" — and `tcp_inet_multi_timeout` then reports the
    /// **head** of the accept list, not the operation whose deadline fired (`:12600`-`:12615`).
    fn timeout_due(&self, now: Instant) -> bool {
        !self.accepts.is_empty() && self.deadline().is_some_and(|deadline| now >= deadline)
    }

    /// One `fire_multi_timers` iteration: consumes the earliest armed accept timer, removes the
    /// accept list's **head** and answers that operation's id (the caller reports it as
    /// `{error, timeout}`).
    ///
    /// The C's exact bookkeeping: `fire_multi_timers` (`inet_drv.c:15650`) pops the head of
    /// `desc->mtd` and calls `tcp_inet_multi_timeout` (`:12600`), whose
    /// `remove_multi_op(desc, &id, &req, caller, NULL, &monitor)` walks the multi-op list for the
    /// first entry of that caller (`:2379`-`:2405`) — the head — *without* removing that
    /// operation's own timer entry, which stays armed and expires the next head. `None` when the
    /// timer has not expired at `now` or no accept is queued.
    fn expire(&mut self, now: Instant) -> Option<u16> {
        if !self.timeout_due(now) {
            return None;
        }
        self.timers.remove(0);
        let head = self.accepts.remove(0);
        // The C restores the state only when the list drains — `if (desc->multi_first == NULL) {
        // sock_select(INETP(desc),FD_ACCEPT,0); desc->inet.state = INET_STATE_LISTENING; /* restore
        // state */ }` (`tcp_inet_multi_timeout`, `inet_drv.c:12607`-`:12611`). With a survivor
        // still queued the multi bit stays, which is the oracle's measured row.
        if self.accepts.is_empty() {
            self.multi = false;
        }
        Some(head.id)
    }

    /// Every queued accept's id, in enqueue order, leaving the list (and its timers) empty — the
    /// accept half of the teardown drain.
    pub(super) fn take_all(&mut self) -> Vec<u16> {
        self.timers.clear();
        // The teardown drain (`async_error_am_all`, `inet_drv.c:2597`-`:2608`) empties the list, so
        // the sticky multi bit goes with it.
        self.multi = false;
        self.accepts.drain(..).map(|op| op.id).collect()
    }
}

/// The C's `Timeout(4)` operand of `INET_REQ_ACCEPT`(26), or `None` when the operand is not
/// exactly four bytes.
pub(super) fn parse_timeout_operand(arg: &[u8]) -> Option<i32> {
    let [t0, t1, t2, t3] = arg else {
        return None;
    };
    Some(i32::from_be_bytes([*t0, *t1, *t2, *t3]))
}

impl TcpInetDriver {
    /// The descriptor-state bits the queued accepts add to `INET_REQ_GETSTATUS`.
    pub(super) fn accept_state_bits(&self) -> u32 {
        self.async_ops.accepts.state_bits()
    }

    /// `INET_REQ_ACCEPT`(26): queue one asynchronous accept and answer `[1] ++ id`.
    ///
    /// The C's guard is one combined test — the socket must be listening **and** the operand must
    /// be the four-byte timeout, both answering `[0]einval` (`inet_drv.c:12160`-`:12170`) — and the
    /// operation joins the multi-op accept list (`enq_multi_op`, `:12194`/`:12209`), so repeats
    /// never answer `ealready`.
    ///
    /// Queueing an accept also **arms accept readiness** on the listener: the C's
    /// `sock_select(INETP(desc), FD_ACCEPT, 1)` on the accept whose `sock_accept` answered
    /// `ERRNO_BLOCK` (`inet_drv.c:12224`), and `FD_ACCEPT` is `ERL_DRV_READ` (`:631`), i.e. the
    /// listener's *read* readiness. It is issued on **every** enqueue (an `epoll_ctl(MOD)` re-arm
    /// of an already-`READABLE` registration), which re-reports a connection that arrived while no
    /// accept was queued; the last completion/timeout disarms it
    /// ([`TcpInetDriver::disarm_accepts`]).
    ///
    /// A **finite** `Timeout(4)` operand also joins the descriptor's timer set: the C stores one
    /// `MultiTimerData` per finite deadline, sorted by `when`, and arms the port's single timer
    /// from the earliest (`add_multi_timer`, `inet_drv.c:15744`-`:15790`); an `INET_INFINITY`
    /// operand adds **no** entry (`mtd` stays `NULL`, `:12198`-`:12210`). What the port declares
    /// through `Driver::next_timeout` is therefore the **earliest** deadline of the list, and its
    /// expiry reports the list's **head** ([`AcceptQueue::timeout_due`] and
    /// [`TcpInetDriver::accept_timeouts`]).
    ///
    /// # Errors
    ///
    /// Whatever the poller answers when the readiness arm fails.
    pub(super) fn async_accept(
        &mut self,
        ctx: &mut DriverContext<'_>,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        if !self.state.is_listening() || arg.len() != 4 {
            return Ok(ctl_xerror("einval"));
        }
        let Some(timeout_ms) = parse_timeout_operand(arg) else {
            return Ok(ctl_xerror("einval"));
        };
        let id = self.async_ops.next_id();
        self.async_ops.accepts.enqueue(AcceptOp {
            id,
            deadline: deadline_for(timeout_ms),
        });
        self.set_interest(ctx, Interest::READABLE)?;
        Ok(async_ok_reply(id))
    }

    /// Disarms accept readiness once the multi-accept list has drained — the C's
    /// `if (desc->multi_first == NULL) { sock_select(INETP(desc), FD_ACCEPT, 0);
    /// desc->inet.state = INET_STATE_LISTENING; }` after a successful accept
    /// (`inet_drv.c:13559`-`:13562`, single-accept twin `:13538`-`:13545`) and after a due accept
    /// timeout (`tcp_inet_multi_timeout`, `:12607`-`:12611`).
    ///
    /// Guarded on `InetState::is_listening` so it can never disturb a connected socket's read
    /// interest or a `InetState::Connecting` socket's write interest. While accepts remain
    /// queued nothing is issued: the registration is still `READABLE` for the survivors.
    ///
    /// # Errors
    ///
    /// Whatever the poller answers when the de-registration fails.
    fn disarm_accepts(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if self.async_ops.accepts.is_empty() && self.state.is_listening() {
            self.deregister(ctx)?;
        }
        Ok(())
    }

    /// Accepts one connection if the listener has one, completing the oldest queued accept.
    ///
    /// Returns whether an operation completed. The accepted descriptor is put into non-blocking
    /// mode and handed to the runtime **inside the completion**
    /// ([`InetAsyncResult::Accepted`]): this driver never stores it, so no teardown path can close
    /// it twice.
    ///
    /// Every completion dequeues its operation and then re-checks the listener's arming
    /// ([`TcpInetDriver::disarm_accepts`]); an `EAGAIN` (the C's "ghost trigger") changes nothing,
    /// exactly like the `goto done` in `tcp_inet_input`.
    ///
    /// # Errors
    ///
    /// Returns the typed I/O failure of a descriptor that cannot be made non-blocking — after
    /// completing the operation with that errno, so the descriptor is still closed here and the
    /// caller learns what happened.
    fn accept_ready_once(&mut self, ctx: &mut DriverContext<'_>) -> Result<bool> {
        let Some(op) = self.async_ops.accepts.first() else {
            return Ok(false);
        };
        let listener = self.socket.ok_or_else(error::badarg)?;
        match accept_socket(listener) {
            Ok(fd) => {
                self.async_ops.accepts.take();
                self.disarm_accepts(ctx)?;
                if let Err(err) = set_nonblocking(fd) {
                    // Nothing can be handed over: close what we took and report the errno, the
                    // C's `inet_async_error` shape (`:2539`).
                    close_fd(fd);
                    let Some(errno) = io_errno(&err) else {
                        // Not an OS failure: no errno to report, so the operation's caller learns
                        // about it through the typed error instead of a fabricated reason.
                        return Err(err);
                    };
                    let reason = InetReason::from_errno(errno);
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
                self.async_ops.accepts.take();
                self.disarm_accepts(ctx)?;
                let reason = InetReason::from_errno(err.raw_os_error().unwrap_or(libc::EINVAL));
                ctx.deliver_completion(CompletionEvent::InetAsync {
                    id: op.id,
                    result: InetAsyncResult::Error(reason),
                })?;
                Ok(true)
            }
        }
    }

    /// Serves every accept the listener has ready, answering how many operations completed — the
    /// accept half of the round's drive point
    /// ([`TcpInetDriver::drive_async_ops`]).
    pub(super) fn accepts_completed(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let mut completed = 0;
        while self.accept_ready_once(ctx)? {
            completed += 1;
        }
        Ok(completed)
    }

    /// Serves every accept the listener has ready, without counting — the accept ladder a read
    /// readiness event runs before the connected-socket paths
    /// ([`TcpInetDriver::async_ready_input`]).
    pub(super) fn serve_accepts(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        while self.accept_ready_once(ctx)? {}
        Ok(())
    }

    /// Reports every accept whose deadline has passed — the accept half of the port's `TIMEOUT`
    /// task.
    ///
    /// The C's `fire_multi_timers` (`inet_drv.c:15650`) → `tcp_inet_multi_timeout` (`:12600`)
    /// answers the **head** of the multi-op list, not the operation whose deadline fired: the port
    /// keeps one timer, armed from the earliest entry of the accept-timer list, and
    /// `remove_multi_op` takes the first entry of the accept list — for a single caller its head
    /// (`:2379`-`:2405`) — answering *that* id with `{error, timeout}`. The loop then continues
    /// while the earliest remaining timer entry is already due (`next_timeout <= 0`) and otherwise
    /// re-arms from the survivors, so one call may report several heads, always in list order.
    ///
    /// # Errors
    ///
    /// The poller's failure from the re-arming check, or the typed `unsupported` of a context with
    /// no completion sink.
    pub(super) fn accept_timeouts(
        &mut self,
        ctx: &mut DriverContext<'_>,
        now: Instant,
    ) -> Result<()> {
        while let Some(id) = self.async_ops.accepts.expire(now) {
            // The C's `tcp_inet_multi_timeout` disarms only once the list is empty
            // (`inet_drv.c:12607-12611`): survivors keep the socket armed.
            self.disarm_accepts(ctx)?;
            ctx.deliver_completion(CompletionEvent::InetAsync {
                id,
                result: InetAsyncResult::Error(InetReason::Timeout),
            })?;
        }
        Ok(())
    }

    /// The `accept` verb: accepts one connection and **transfers the port** to it.
    ///
    /// The listener is released and the accepted descriptor adopted as the port's own socket
    /// (registered read-ready), so the reply carries the **new descriptor** (`[1] ++ fd_be32`)
    /// where the C's asynchronous accept carries an id. With nothing pending the reply is the bare
    /// `[1]` and the caller retries when the port reports readability.
    ///
    /// # Errors
    ///
    /// A typed `einval` reply when the socket is not listening, and the poller's/adoption's own
    /// failure when the transfer cannot be made.
    pub(super) fn verb_accept(&mut self, ctx: &mut DriverContext<'_>) -> Result<Vec<u8>> {
        if !self.state.is_listening() {
            return Ok(ctl_xerror("einval"));
        }
        let listener = self.socket.ok_or_else(error::badarg)?;
        let accepted = match accept_socket(listener) {
            Ok(fd) => fd,
            // Nothing to accept yet: the caller retries when the port reports readability.
            Err(err) if err.raw_os_error() == Some(libc::EAGAIN) => return Ok(ctl_ok()),
            Err(err) => return Ok(ctl_error(err)),
        };
        // The port follows its connection: release the listener, adopt the accepted socket.
        self.release_socket(ctx)?;
        if let Err(err) = self.adopt(ctx, accepted) {
            self.close_socket();
            return Err(err);
        }
        self.mark_connected();
        let mut reply = ctl_ok();
        reply.extend_from_slice(&(accepted as i32).to_be_bytes());
        Ok(reply)
    }
}

/// The `errno` an OS-level [`BeamError`](beam_common::BeamError) carries, or `None` for a failure
/// that has no errno at all (an argument fault, a capability gap).
///
/// A driver has to report an operation's failure as the C's `{error, Reason}` atom, and the reason
/// vocabulary is errno-based, so this is the one place the accept completion makes the conversion —
/// never a fabricated `EINVAL` for a failure that is not an OS error.
fn io_errno(err: &beam_common::BeamError) -> Option<i32> {
    match err {
        beam_common::BeamError::Io(io) => io.raw_os_error(),
        _ => None,
    }
}
