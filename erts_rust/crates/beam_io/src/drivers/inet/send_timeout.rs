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

//! The `{send_timeout, …}` / `{send_timeout_close, …}` timer — the C's `desc->busy_on_send`
//! one-shot (`tcp_inet_send_timeout`, `erts/emulator/drivers/common/inet_drv.c:12458`-`:12520`).
//!
//! # What the two options do in the C
//!
//! A command whose bytes the socket could not take stays on the driver's output queue. The C
//! marks the port **busy** and arms a multi-timer for `desc->inet.send_timeout`
//! (`tcp_sendv`/`tcp_send`, `:13775`-`:13790`/`:13887`-`:13902`); when it fires,
//! `tcp_inet_send_timeout` clears the busy flag and answers the **command's own caller** with
//! `{inet_reply, Port, {error, timeout}}` (`inet_reply_error_am(…, am_timeout)`, `:12484`), then —
//! only under `{send_timeout_close, true}` — takes the descriptor down. The timer is cancelled at
//! the low watermark when the queue has drained enough (`cancel_multi_timer(…,
//! &tcp_inet_send_timeout)`, `:12795`/`:12856`/`:13623`), which is also where the caller is
//! answered `ok` (`inet_reply_ok`).
//!
//! Two values are special. `{send_timeout, 0}` never arms a timer: the C answers the stalled
//! command `{error, timeout}` **at once** (the `if (desc->send_timeout == 0)` shortcut, `:13776`,
//! `:13888`), which is why it reads as "no timer, immediate answer" on the oracle. An infinite
//! timeout (`INET_INFINITY`, `super::opt::INET_INFINITY`) arms nothing at all, so the caller waits for the
//! queue to drain however long that takes.
//!
//! # What is reproduced here
//!
//! This floor has exactly **one** command in flight: the port queues a command's bytes before it
//! calls the driver, and [`super::TcpInetDriver::write_command`] keeps the command's caller tag
//! until the queue drains ([`super::TcpInetDriver::caller_ref`]). A stall therefore *is* "the
//! queue is still non-empty after the write attempt", and that is exactly when
//! [`super::TcpInetDriver::note_send_stall`] arms [`SendTimeout`] — with that command's tag as the
//! addressee. The one-shot discipline is the C's: [`SendTimeout`] is armed once per stall
//! (`Idle` → `Armed`), fires once (`Armed` → `Fired`), and only a drained queue resets it to
//! `Idle` — so a re-driven `OUTPUT` task cannot push the deadline out, and a fired stall cannot
//! answer twice.
//!
//! On expiry ([`super::TcpInetDriver::send_timeout_fired`]):
//!
//! 1. the issuing caller is answered `{inet_reply, Port, {error, timeout}}` **first** — a
//!    `{send_timeout_close,true}` may latch the port's failure below, and every delivery seam
//!    refuses afterwards ([`crate::port_failure`]);
//! 2. `{send_timeout_close, false}` keeps the socket: the queued remainder goes on draining and
//!    nobody is answered for it any more (the caller's tag is spent by the timeout reply);
//! 3. `{send_timeout_close, true}` mirrors the C's own branch on the socket's mode:
//!    a **passive** socket takes `tcp_desc_close` (`:12521`) — the descriptor and the queued output
//!    go, the **port stays** and its next command is the `enotconn` ladder (`:12681`) — while an
//!    **active** socket under the default `{exit_on_close,true}` takes `driver_exit(port, 0)`
//!    (`:12507`), i.e. [`DriverContext::signal_failure`] with
//!    [`DriverFailure::exit`] (the owner is unlinked first, so the port closes with no signal).
//!
//! Measured on the oracle (OTP 29.1.2 / erts-17.1.1, a loopback pair whose client never reads,
//! `{sndbuf, 4096}` and a 4 MiB send): with `{send_timeout, 200}` the second send answers
//! `{error, timeout}` after ~201 ms and `inet:getopts/2` still reports the pair; with
//! `{send_timeout_close, true}` the second send answers `{error, timeout}` after ~205 ms,
//! `inet:peername/1` then answers `{error, enotconn}`, `erlang:port_info/1` still lists the port
//! (`connected`) and the next send answers `{error, enotconn}` — the passive `tcp_desc_close`
//! shape. `{send_timeout, 0}` answers the stalled send `{error, timeout}` in 0 ms, and
//! `{send_timeout, infinity}` never answers (the caller stays parked, as the C's missing timer
//! leaves it).
//!
//! # The one residual
//!
//! The C arms the timer for a command issued **while the output queue is already non-empty** (the
//! `driver_sizeq(ix) > 0` branch), and answers the *first* command `ok` at issue even when it
//! queues a remainder (`inet_reply_ok`, `tcp_inet_commandv`'s `tcp_sendv(desc, ev) == 0`). This
//! floor answers a command only when its bytes are **off** the queue
//! ([`super::TcpInetDriver::settle_command`]), so the first stalled command is the one that gets
//! the timeout where the oracle answers it `ok` and times out the second. The difference is the
//! port-busy/high-watermark machinery this floor does not model; the elapsed time and the shape of
//! the reply are the C's.

use std::time::{Duration, Instant};

use beam_common::Result;

use super::op_queue::drain_closed;
use super::TcpInetDriver;
use crate::completion::{CompletionEvent, InetReason, InetReply};
use crate::driver::DriverContext;
use crate::port_failure::DriverFailure;

/// One command the socket could not take in full — what the timer is armed for.
///
/// `caller_ref` is the `prim_inet` caller tag captured by `init_caller` (the C's
/// `desc->caller_ref.tag_buf`), i.e. the process the `{inet_reply, …}` is addressed to. It is
/// `None` for a command whose tag cannot be echoed ([`super::wire::echoable_caller_tag`]); such a
/// command still gets the `{send_timeout_close,true}` teardown, it just has nobody to answer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct PendingSend {
    /// The caller to answer, as the bytes of the `{inet_reply, …}` message's trailing tag.
    pub(super) caller_ref: Option<Vec<u8>>,
    /// When the timer fires.
    pub(super) deadline: Instant,
}

/// The three states of the C's `desc->busy_on_send` one-shot.
#[derive(Clone, PartialEq, Eq, Debug)]
enum SendTimeoutState {
    /// No command is waiting on the socket.
    Idle,
    /// A stalled command is waiting, and the timer is armed for it.
    Armed(PendingSend),
    /// The timer fired for the current stall. Nothing re-arms until the queue drains, so one
    /// stall answers its caller exactly once (the C's `desc->busy_on_send = 0`).
    Fired,
}

/// The C's `{send_timeout, …}` one-shot: armed when a command stalls, cancelled when the output
/// queue drains, fired at most once per stall.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct SendTimeout {
    state: SendTimeoutState,
}

impl SendTimeout {
    /// Nothing pending, no timer — the state of a fresh descriptor (`inet_init_desc` leaves
    /// `desc->busy_on_send = 0`, `inet_drv.c:11770`).
    pub(super) const fn new() -> SendTimeout {
        SendTimeout {
            state: SendTimeoutState::Idle,
        }
    }

    /// Arms the one-shot for `caller_ref` after `timeout_ms` milliseconds.
    ///
    /// * `timeout_ms < 0` is the C's `INET_INFINITY` (`0xffffffff`): nothing is armed, so the
    ///   stalled caller waits for the queue to drain (`tcp_sendv`'s
    ///   `if (desc->send_timeout != INET_INFINITY)`, `inet_drv.c:13785`);
    /// * `timeout_ms == 0` is the C's immediate-answer shortcut (`:13776`): the deadline is
    ///   *now*, so the very next port round runs the `timeout` callback;
    /// * anything already `Armed`/`Fired` is left alone: a re-driven write attempt must not push
    ///   the deadline out, and a fired stall must not answer twice.
    pub(super) fn arm(&mut self, caller_ref: Option<Vec<u8>>, timeout_ms: i32) {
        if self.is_armed() || self.has_fired() {
            return;
        }
        if timeout_ms < 0 {
            return;
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        self.state = SendTimeoutState::Armed(PendingSend {
            caller_ref,
            deadline,
        });
    }

    /// Drops the one-shot — the command it was armed for has been answered, or the descriptor is
    /// going away. An `Idle` timer is unaffected.
    pub(super) fn cancel(&mut self) {
        self.state = SendTimeoutState::Idle;
    }

    /// A **new** command has been handed to the driver.
    ///
    /// A stall that already fired belongs to the previous command, so a fresh command starts a
    /// fresh one-shot: the C re-arms `desc->busy_on_send` for every command that stalls
    /// (`tcp_sendv`, `inet_drv.c:13785`), and its `tcp_inet_send_timeout` clears the flag when it
    /// fires (`:12488`). An **armed** timer is left alone: its command's remainder may still be on
    /// the queue, and the deadline belongs to that command's first failure.
    pub(super) fn begin_command(&mut self) {
        if self.has_fired() {
            self.state = SendTimeoutState::Idle;
        }
    }

    /// The deadline the port's single `TIMEOUT` task is armed from, if the timer is armed.
    pub(super) fn next_deadline(&self) -> Option<Instant> {
        match &self.state {
            SendTimeoutState::Armed(pending) => Some(pending.deadline),
            SendTimeoutState::Idle | SendTimeoutState::Fired => None,
        }
    }

    /// Whether a stalled command is waiting for its deadline.
    pub(super) fn is_armed(&self) -> bool {
        matches!(self.state, SendTimeoutState::Armed(_))
    }

    /// Whether a stall has already been timed out and the queue has not drained since.
    pub(super) fn has_fired(&self) -> bool {
        matches!(self.state, SendTimeoutState::Fired)
    }

    /// Takes the stalled command if its deadline has passed at `now`, leaving the timer fired.
    ///
    /// A deadline in the future leaves the timer armed and answers `None`, so the port's
    /// `timeout` callback can be reached by the *other* deadline (a queued `recv`/`accept`) in the
    /// same call without spending this one.
    pub(super) fn take_expired(&mut self, now: Instant) -> Option<PendingSend> {
        match &self.state {
            SendTimeoutState::Armed(pending) if pending.deadline <= now => {
                let expired = pending.clone();
                self.state = SendTimeoutState::Fired;
                Some(expired)
            }
            SendTimeoutState::Idle | SendTimeoutState::Armed(_) | SendTimeoutState::Fired => None,
        }
    }
}

impl TcpInetDriver {
    /// Arms the send timer for the command still on the output queue, or cancels it once the
    /// queue is empty — the C's `busy_on_send` arm (`tcp_sendv`/`tcp_send`,
    /// `inet_drv.c:13775`-`:13790`/`:13887`-`:13902`) and its low-watermark cancel
    /// (`:14006`-`:14018`).
    ///
    /// Called at the end of every [`super::TcpInetDriver::write_command`], i.e. once per write
    /// attempt: a drained queue cancels (nothing is owed a timeout), a remaining queue arms with
    /// the in-flight command's caller tag as the addressee. Arming is a one-shot for the whole
    /// stall ([`SendTimeout::arm`]), so the per-attempt call cannot push the deadline out.
    pub(super) fn note_send_stall(&mut self, ctx: &mut DriverContext<'_>) {
        if ctx.output().is_empty() {
            self.send_timeout.cancel();
            return;
        }
        let caller_ref = self.caller_ref.clone();
        self.send_timeout.arm(caller_ref, self.options.send_timeout);
    }

    /// The C's `tcp_inet_send_timeout` (`inet_drv.c:12458`-`:12520`): answers a stalled command's
    /// caller `{error, timeout}` and, under `{send_timeout_close, true}`, takes the socket down.
    ///
    /// Returns whether the port's **failure was latched** (the C's `driver_exit` arm), in which
    /// case the caller — [`Driver::timeout`](crate::Driver::timeout) — must not run the rest of
    /// the timer work: the completion channel refuses everything after a latch.
    ///
    /// The reply is staged **before** any teardown, so a latched failure cannot swallow it; the
    /// command's caller tag is spent by it either way, so the drain that follows (or the next
    /// command's ladder) cannot answer the same caller twice.
    pub(super) fn send_timeout_fired(&mut self, ctx: &mut DriverContext<'_>) -> Result<bool> {
        let Some(pending) = self.send_timeout.take_expired(Instant::now()) else {
            return Ok(false);
        };
        // The command is answered now: its tag must not be reused by the `ok` a later drain would
        // stage, nor by the next command's `enotconn` ladder.
        self.caller_ref = None;
        if let Some(tag) = pending.caller_ref {
            ctx.deliver_completion(CompletionEvent::InetReply {
                caller_ref: Some(tag),
                reply: InetReply::Error(InetReason::Timeout),
            })?;
        }
        if self.options.send_timeout_close == 0 {
            // `{send_timeout_close, false}`: the port is un-busied and the caller answered; the
            // queued remainder keeps draining on write readiness (the C's `:12488`-`:12520` note —
            // the port is deliberately not kept busy, or the following send would hang).
            return Ok(false);
        }
        if self.options.active > super::opt::INET_PASSIVE && self.options.exit_on_close != 0 {
            // The C's active arm (`:12500`-`:12509`): `tcp_closed_message` then `driver_exit(port,
            // 0)` — the owner is unlinked first, so the port closes with no signal. (The
            // `{tcp_closed, Port}` message itself is the runtime's render; this floor reports the
            // close through the failure channel only.)
            ctx.signal_failure(DriverFailure::exit(0))?;
            return Ok(true);
        }
        // The passive arm (`:12510`-`:12522`, and the active `{exit_on_close,false}` one):
        // `tcp_desc_close` — the queued output and the descriptor go, the **port stays**, and its
        // next command is answered from the `!IS_CONNECTED` ladder (`{error, enotconn}`).
        // Every queued async operation is answered `{error, closed}` first: the descriptor it
        // waits on is about to go (`tcp_close_check`'s `async_error_am_all(am_closed)`, `:11887`,
        // which the C's send-timeout close leaves out — an operation would then wait on a dead
        // descriptor, which is the one place this floor answers instead of leaking it).
        drain_closed(&mut self.async_ops, ctx)?;
        self.desc_close(ctx)?;
        Ok(false)
    }
}
