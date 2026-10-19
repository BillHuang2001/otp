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

//! The `udp_inet` driver's **`{active, …}` half**: a datagram that arrives on an armed datagram
//! port is *delivered to the port owner* instead of waiting for a passive `recv`, and the
//! `{active, …}` state machine advances per delivery.
//!
//! This is the C's active-mode datagram read path, which the passive floor deliberately does not
//! cover: [`UdpInetDriver::async_ready_input`](super::op_queue) serves a queued
//! `PACKET_REQ_RECV`(60) and is a no-op otherwise, because a datagram's *source address* has no
//! home in the byte-only port input queue. The channel that does carry it is
//! [`crate::port_task::ActiveInput`] — `ActiveInputEvent::Data(ReadMessage)` for one delivered
//! datagram (the packed source address as the read's **header**, the payload as its body),
//! `ActiveInputEvent::Passive` for the `{udp_passive, S}` of a spent `{active, N}` counter and
//! `ActiveInputEvent::Error` for the `{udp_error, S, Reason}` of a failed `recvfrom(2)` — with
//! [`DriverContext::deliver_active_input`] as the driver-side seam and
//! [`crate::PortRegistry::take_active_inputs`] as the runtime's drain.
//!
//! Legacy C ground truth (read-only), reproduced arm for arm:
//!
//! * the read loop — `packet_inet_input`'s `while (packet_count--)` burst of at most
//!   `desc->read_packets` datagrams, stopping on `EAGAIN` (`inet_drv.c:15378`, `:15470`-`:15507`);
//! * the per-datagram accounting — `inet_input_count(desc, n)` (`:15513`) — and the delivery
//!   shape — `packet_reply_binary_data` (`:4335`) → `packet_binary_message` (`:4110`), i.e. the
//!   packed source address in front of the payload;
//! * the per-delivery state machine `INET_CHECK_ACTIVE_TO_PASSIVE` (`:1552`-`:1560`, invoked at
//!   `:4352`): `{active, once}` flips to passive **silently**, `{active, N}` decrements and, on
//!   exactly `0`, flips **and** emits `packet_passive_message` (`:4215`);
//! * the arming rule at the tail of `inet_set_opts` (`:7944`-`:7987`) and the passive-message the
//!   option walk itself emits when an `{active, N}` record lands the counter on zero
//!   (`:6993`-`:6998`).
//!
//! # What is deliberately **not** emitted here
//!
//! * `{active, once}` never produces a [`crate::port_task::ActiveInputEvent::Passive`] — the C's
//!   `INET_CHECK_ACTIVE_TO_PASSIVE` flips `INET_ONCE` to `INET_PASSIVE` with **no** message
//!   (`:1554`-`:1555`, measured: a second injection after the flip yields nothing at all);
//! * nothing is delivered *inside* a `setopts` control call. Both
//!   `/* XXX: UDP sockets could also trigger immediate read here NIY */` comments (`:7949`,
//!   `:7969`) are the C's own admission that the inline read `trigger_recv` (`:7945`) performs is
//!   `SOCK_STREAM`-only, so a UDP `setopts` only **arms** (measured: after
//!   `{active,false}`→`{active,true}` with a datagram already buffered, `message_queue_len` is `0`
//!   immediately after the ctl and `1` ~100 ms later);
//! * `{deliver, port}` (the C's `INET_DELIVER_PORT` branch, `:4355`) stays the documented gap it
//!   is today: that mode delivers the raw bytes through `{Port, {data, …}}`, which this channel
//!   cannot spell, so a `{deliver, port}` port is left undelivered rather than handed a
//!   `{udp, S, IP, Port, Data}` its owner never asked for.

use std::os::unix::io::RawFd;

use beam_common::Result;

use super::super::opt::{set_options_tracked, OptScope, INET_MULTI, INET_ONCE, INET_PASSIVE};
use super::super::sys::recv_from;
use super::super::wire::{pack_address, InetAddr};
use super::super::InetState;
use super::{UdpInetDriver, DATAGRAM_CHUNK};
use crate::completion::InetReason;
use crate::driver::DriverContext;
use crate::poller::Interest;
use crate::port_task::{ActiveInputEvent, ActiveSpend, ReadMessage};

/// `{deliver, term}` (`INET_DELIVER_TERM`, `inet_drv.c:783`) — the default, and the only
/// delivery mode this channel can express (see the module docs).
const INET_DELIVER_TERM: i32 = 1;

/// One non-blocking `recvfrom(2)`, retrying an interrupted wait.
///
/// `EINTR` is **not** a burst slot: a signal delivered while the kernel was copying a datagram is
/// the C's `if (err != ERRNO_BLOCK)`-free retry of the `recvfrom` itself (`inet_drv.c:15484`),
/// so retrying here keeps [`UdpInetDriver::active_delivery`]'s budget meaning what the C's
/// `packet_count` means — *datagrams attempted*, not syscalls made.
fn recv_retrying(fd: RawFd, buf: &mut [u8]) -> std::io::Result<(usize, InetAddr)> {
    loop {
        match recv_from(fd, buf) {
            Err(err) if err.raw_os_error() == Some(libc::EINTR) => continue,
            other => return other,
        }
    }
}

impl UdpInetDriver {
    /// The active-mode half of the read-readiness callback: deliver up to
    /// [`SocketOptions::read_packets`](super::super::opt::SocketOptions) datagrams to the port
    /// owner as `{udp, S, IP, Port, Data}` deliveries, advancing the `{active, …}` state machine
    /// after each one.
    ///
    /// The burst is the C's `packet_inet_input` loop (`inet_drv.c:15378`): one non-blocking
    /// `recvfrom(2)` per iteration, at most `desc->read_packets` of them (`:15513`'s
    /// `inet_input_count(desc, n)` per datagram), stopping on `EAGAIN` — the C's
    /// `ERRNO_BLOCK` (`:15490`) — and stopping **immediately** when the mode has flipped to
    /// passive (`:15566`-`:15573`). It never parks, sleeps or spins: the readiness edge is the
    /// whole trigger, and the next edge (or nothing at all) brings the rest.
    ///
    /// A burst that ends because it spent the whole `read_packets` allowance — rather than because
    /// the socket ran dry — re-arms read readiness, so the datagrams still buffered arrive on the
    /// next round. That is this floor's substitute for the C's level-triggered
    /// `sock_select(desc, FD_READ, 1)`, which re-reports the socket on the next loop iteration by
    /// itself; here the registration is edge-triggered, so an `EPOLL_CTL_MOD` re-arm is what
    /// re-reports a descriptor that is ready *now*.
    ///
    /// Nothing happens, exactly as before this module existed, when:
    ///
    /// * an asynchronous receive owns the socket — `PACKET_REQ_RECV`(60) and an armed port are
    ///   mutually exclusive by its own guard (`active || len != 8 ⇒ [0]einval`,
    ///   `:14988`), so this is only the belt-and-braces ordering: the queued operation, whose
    ///   completion is the `{inet_async, Port, Id, {ok, D}}` reply, stays authoritative;
    /// * the port is passive (`options.active == INET_PASSIVE`) — the passive floor: a datagram's
    ///   source has no path into the byte-only port input queue, so the runtime serves a passive
    ///   socket with the `recv` verb;
    /// * the port has no socket (`!IS_OPEN`) or `{deliver, port}` is set (see the module docs).
    ///
    /// # A real read error
    ///
    /// A non-blocking, non-`EAGAIN` `recvfrom(2)` failure on an **active** socket is the C's
    /// `if (err != ERRNO_BLOCK)` "real error" arm (`inet_drv.c:15479`-`:15492`): an active
    /// descriptor runs `packet_error_message` (`:15486`) — the `{udp_error, S, Reason}` of `:4245`,
    /// whose `Reason` is `error_atom(err)` — and the read loop returns. A datagram port has no
    /// close message of its own (the C has no `{udp_closed, S}` anywhere), so the port stays live
    /// and the burst simply ends on the delivery; the next readiness edge reads again. This is a
    /// genuine socket failure (`ECONNREFUSED` on a *connected* datagram socket is the realistic
    /// one), never a normal datagram arrival.
    ///
    /// # Errors
    ///
    /// The typed `unsupported` of a context with no active-input sink (a driver driven outside a
    /// port), and — through [`DriverContext::deliver_active_input`] — a refusal after a failure
    /// latched in the same callback.
    pub(super) fn active_delivery(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // The queued passive operation owns the socket; it was already served by
        // `async_ready_input` (the caller runs it first).
        if self.async_ops.recv().is_some() {
            return Ok(());
        }
        // The passive floor, and the one delivery mode this channel cannot spell.
        if self.options.active == INET_PASSIVE || self.options.deliver != INET_DELIVER_TERM {
            return Ok(());
        }
        let Some(fd) = self.socket else {
            return Ok(());
        };
        // One buffer for the whole burst: it is re-filled by every `recvfrom(2)` and consumed
        // before the next attempt, so a datagram is never truncated by a previous one.
        let mut buf = [0u8; DATAGRAM_CHUNK];
        let mut budget = self.options.read_packets.max(0);
        let mut delivered = 0usize;
        let mut spent_the_budget = false;
        while budget > 0 {
            // The C's passive re-check (`:15566`): `{active, once}`/`{active, N}` may have
            // flipped on the *previous* delivery, in which case the remaining datagrams stay in
            // the kernel for a later passive `recv` (measured: two `{udp,…}` then one
            // `{udp_passive,S}` for `{active,2}` + three datagrams — the third is not delivered).
            if self.options.active == INET_PASSIVE || self.socket != Some(fd) {
                break;
            }
            budget -= 1;
            match recv_retrying(fd, &mut buf) {
                Ok((len, source)) => {
                    // `inet_input_count(desc, n)` (`:15513`): the received **payload** length,
                    // never the packed source address in front of it.
                    self.stat.input(len);
                    let read = ReadMessage {
                        header: pack_address(source.port, source.addr),
                        body: Some(buf[..len].to_vec()),
                    };
                    ctx.deliver_active_input(ActiveInputEvent::Data(read))?;
                    delivered += 1;
                    // The C runs `INET_CHECK_ACTIVE_TO_PASSIVE` **after** the delivery
                    // (`packet_reply_binary_data`, `:4352`), so the datagram that spent the last
                    // credit is still delivered and the passive message follows it.
                    if let Some(event) = self.after_delivery() {
                        ctx.deliver_active_input(event)?;
                    }
                }
                // `ERRNO_BLOCK` — nothing more is buffered right now (`:15490`) — ends the
                // burst; any other failure is the C's `if (err != ERRNO_BLOCK)` arm (`:15479`-
                // `:15492`): a **real error** on an active descriptor runs
                // `packet_error_message(udesc, err)` (`:15486`, the `{udp_error, S, Reason}` of
                // `:4245`) and `return count`s out of the read loop. The datagram port has no
                // close message of its own (there is no `{udp_closed, S}` anywhere in the C), so
                // it stays live and this delivery is the whole report.
                Err(err) => match err.raw_os_error() {
                    Some(libc::EAGAIN) => break,
                    errno => {
                        ctx.deliver_active_input(ActiveInputEvent::Error(
                            InetReason::from_errno(errno.unwrap_or(libc::EIO)),
                        ))?;
                        return Ok(());
                    }
                },
            }
            spent_the_budget = budget == 0;
        }
        // The burst ended because it spent its whole `read_packets` allowance, not because the
        // socket ran dry: datagrams are most likely still buffered, and the C's level-triggered
        // `sock_select(desc, FD_READ, 1)` would simply report the socket readable again on its
        // next loop iteration. This crate's poller registration is edge-triggered
        // ([`crate::poller::Interest`] has no empty mask and `mio`'s epoll backend uses
        // `EPOLLET`), so the equivalent is a re-arm: `EPOLL_CTL_MOD` re-reports readiness for a
        // descriptor that is ready *now*, which is what brings the rest in on the next round.
        //
        // `delivered > 0` keeps a misconfigured `{read_packets, 0}` from re-arming forever (it
        // never delivers, and would otherwise re-edge on every callback — a busy loop).
        if spent_the_budget && delivered > 0 && self.socket == Some(fd) {
            self.set_interest(ctx, Interest::READABLE)?;
        }
        Ok(())
    }

    /// The `setopts` control's post-walk effects: the passive message an `{active, N}` record that
    /// landed on zero owes (`inet_drv.c:6993`-`:6998`), and the arming rule of the walk's tail
    /// (`:7944`-`:7987`).
    ///
    /// Both the named `"setopts"` verb and the numeric `INET_REQ_SETOPTS`(7) opcode reach it
    /// through [`UdpInetDriver::setopts_with_effects`], so the two routes cannot drift apart.
    ///
    /// * `landed_at_zero` ⇒ one [`ActiveInputEvent::Passive`], emitted **unconditionally**: the C
    ///   calls `packet_passive_message(desc)` from inside the option walk itself, before the
    ///   descriptor state is looked at (`:6996`), so neither a socket-less port nor a closed one
    ///   gates it. This is the only place a `{active, N}` that reaches 0 **without** a datagram
    ///   is reported — a delivery-driven zero is [`UdpInetDriver::after_delivery`]'s.
    /// * passive→armed ([`INET_PASSIVE`] before, `> 0` after) on an open datagram socket ⇒
    ///   `sock_select(desc, FD_READ|FD_CLOSE, 1)` (`:7971`), reproduced as a re-arm of read
    ///   readiness. mio's registration is edge-triggered and this crate has no empty interest
    ///   mask, so the registration never actually goes away: the arm is a `reregister`, which
    ///   re-reports readiness for a datagram already buffered (the measured oracle behaviour: `0`
    ///   messages inside the ctl, one on the next readiness round). A `setopts` that changes
    ///   nothing about `active` re-arms nothing.
    /// * armed→passive ⇒ nothing beyond keeping the registration. The C calls
    ///   `sock_select(…, 0)` and cancels its `read_packet` timer (`:7959`-`:7964`); here the gate
    ///   is [`UdpInetDriver::active_delivery`]'s `options.active` test, and this floor has no
    ///   `read_packet` timer to cancel ([`crate::Driver::next_timeout`] carries only the
    ///   asynchronous-operation deadlines).
    ///
    /// # Errors
    ///
    /// The typed `unsupported` of a context with no active-input sink, and whatever the poller
    /// reports when read readiness cannot be re-armed.
    pub(super) fn after_setopts(
        &mut self,
        ctx: &mut DriverContext<'_>,
        before_active: i32,
        before_socket: bool,
        landed_at_zero: bool,
    ) -> Result<()> {
        if landed_at_zero {
            ctx.deliver_active_input(ActiveInputEvent::Passive)?;
        }
        let open = before_socket && self.has_socket() && self.state != InetState::Closed;
        if before_active == INET_PASSIVE && self.options.active > 0 && open {
            self.set_interest(ctx, Interest::READABLE)?;
        }
        Ok(())
    }

    /// The `setopts` control verb **with** its post-walk effects: the option walk plus
    /// [`UdpInetDriver::after_setopts`].
    ///
    /// The walk itself is [`set_options_tracked`], which needs no context; this is the pair the
    /// two control routes (`Opcode::Setopts` and the named `"setopts"` verb) both call, so the
    /// numeric and named replies stay byte-identical.
    ///
    /// # Errors
    ///
    /// The walk's own argument faults (the typed `BadArg` of a truncated `{active, N}` record),
    /// plus [`UdpInetDriver::after_setopts`]'s.
    pub(super) fn setopts_with_effects(
        &mut self,
        ctx: &mut DriverContext<'_>,
        arg: &[u8],
    ) -> Result<Vec<u8>> {
        let scope = OptScope::new(self.stype, self.family, libc::IPPROTO_UDP);
        let before_active = self.options.active;
        let before_socket = self.has_socket();
        let mut landed_at_zero = false;
        let reply = set_options_tracked(
            &mut self.options,
            scope,
            self.socket,
            arg,
            &mut landed_at_zero,
        )?;
        self.after_setopts(ctx, before_active, before_socket, landed_at_zero)?;
        Ok(reply)
    }

    /// Advances the `{active, …}` state machine after one **delivered** datagram — the C's
    /// `INET_CHECK_ACTIVE_TO_PASSIVE` (`inet_drv.c:1552`-`:1560`), invoked from
    /// `packet_reply_binary_data` (`:4352`).
    ///
    /// * [`INET_ONCE`] ⇒ [`INET_PASSIVE`], **silently** — no event (measured: the second
    ///   injection after an `{active, once}` delivery yields nothing at all, and no
    ///   `{udp_passive, S}` is ever sent for it);
    /// * [`INET_MULTI`] ⇒ `--active_count`; on **exactly** `0` the mode flips to
    ///   [`INET_PASSIVE`] **and** the `{udp_passive, S}` of `packet_passive_message` (`:4215`)
    ///   is returned for the caller to deliver — after the datagram that spent the credit;
    /// * `{active, true}` does **nothing**, so such a port never reports a passive message.
    ///
    /// The decrement is a wrapping `i16` subtraction, as the C's `--((inet)->active_count)` on a
    /// `Sint16` is, and the flip tests `== 0` rather than `<= 0` — the C's own condition.
    ///
    /// The two spending arms also **record** the spend (`ActiveSpend::Once` / `ActiveSpend::Multi`)
    /// on the driver's `active_spends`, which [`crate::Driver::take_active_spends`] drains — an
    /// `{active, once}` transition carries no message at all, so without it the runtime could not
    /// tell it happened. This is the **only** spend site: [`UdpInetDriver::after_setopts`]'s
    /// `landed_at_zero` is the option walk's own zero (`inet_drv.c:6993`-`:6998`), which delivers
    /// no datagram and therefore spends nothing.
    fn after_delivery(&mut self) -> Option<ActiveInputEvent> {
        match self.options.active {
            INET_ONCE => {
                self.options.active = INET_PASSIVE;
                self.active_spends.push(ActiveSpend::Once);
                None
            }
            INET_MULTI => {
                self.options.active_count = self.options.active_count.wrapping_sub(1);
                if self.options.active_count == 0 {
                    self.options.active = INET_PASSIVE;
                    self.active_spends.push(ActiveSpend::Multi);
                    Some(ActiveInputEvent::Passive)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}
