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

//! The **peer-close channel** — the driver's latched "the other end has closed" observation
//! turned into the `{tcp_closed, S}` message the port's connected process is owed.
//!
//! # What it is for
//!
//! When the peer of a connected socket closes, the driver is the only component that sees it: the
//! C's `tcp_recv_closed` (`erts/emulator/drivers/common/inet_drv.c:12783-12845`) runs from the
//! driver's read path and, for an **active** socket, sends the owner one `{tcp_closed, S}`
//! (2-tuple) before tearing the port down. [`beam_io`] models the observation (a *latched, sticky*
//! per-port answer, [`beam_io::PortRegistry::port_closed`] — only
//! [`TcpInetDriver`](beam_io::drivers::inet::TcpInetDriver) overrides the drivers'
//! `Driver::port_closed`) and nothing in this crate read it, so the observation was swallowed and
//! an `{active, …}` owner never learned its socket had closed. This module is the reading half.
//!
//! | the C | here |
//! |---|---|
//! | `tcp_recv_closed`'s `if (!desc->inet.active)` passive arm — *no message*, **no** port death from the message path (`:12819-12832`) | [`PeerCloseReport`] is **not** produced while the port's mirrored `{active, …}` is `INET_PASSIVE` |
//! | `tcp_recv_closed`'s active arm → `tcp_closed_message` (`:12830`) | one [`PeerCloseReport`] per descriptor |
//! | `tcp_closed_message`'s `TCP_ADDF_CLOSE_SENT` guard (`:4064-4071`) — **at most once per descriptor** | [`PeerCloseLatches::is_reported`] |
//! | the sticky re-fire in `inet_set_opts`'s `INET_LOPT_ACTIVE` arm: a *passive* descriptor already in `INET_STATE_CLOSED` sends the message the moment an active mode is armed (`:7013-7021`) | a passive observation keeps the latch and the **later** active `setopts` makes the report due — the once-only guard is keyed on *reported*, never on *observed* |
//! | `if (desc->inet.exitf) driver_exit(...)` (`:12835-12837`) | [`PeerCloseReport::exit_on_close`], so the caller can retire the port exactly as that call does |
//!
//! # Why the active mode comes from the runtime's own mirror
//!
//! `beam_io`'s socket driver holds the `{active, …}` state machine but exposes **no accessor** for
//! it (its `SocketOptions` is private to the driver), while the runtime *does* observe every
//! accepted `{active, …}` record: `prim_inet:setopts/2` is an `erlang:port_control/3` call and
//! [`PortRegistry::note_inet_setopts`] records it ([`super::inet_opts`]). So the gate reads
//! [`PortInetOptions::active_mode`], and the *fallback* is the C's own default: a descriptor with
//! no `{active, …}` record has never been armed, and `inet_start` leaves `desc->active =
//! INET_PASSIVE` (`inet_drv.c:10932`). An unknown mode is therefore **passive**, never an invented
//! "active" — a fabricated `{tcp_closed, S}` would be a wrong message, which is worse than none.
//!
//! # No `{udp_closed, _}`
//!
//! The C has no such atom (`inet_drv` sends `{udp_error, S, econnrefused}` for a datagram socket's
//! ICMP failure, a different family), so [`PeerCloseKind`] has exactly one member and a
//! `udp_inet` port never reports. Synthesizing a message the oracle cannot send is a defect; the
//! honest answer is that this channel has nothing to say about a datagram port.
//!
//! # Where the drain happens
//!
//! The latch is read **once per port-task round**, immediately beside the other two driver→owner
//! channels ([`PortRegistry::drain_io_completions`] and
//! [`PortRegistry::drain_active_inputs`]), because the observation is made by the very same
//! `ready_input`/`read` callbacks those channels are staged from: every site that owes one owes
//! this one. Reading it there also means the once-only latch is one hash probe per live port per
//! round, and no callback can miss a close by draining late — `beam_io`'s answer is sticky.
//!
//! [`TcpInetDriver`]: beam_io::drivers::inet::TcpInetDriver
//! [`PortInetOptions::active_mode`]: super::inet_opts::PortInetOptions::active_mode
//!
//! # The three halves of the channel
//!
//! Reading the latch is only the first half: the message has to reach its recipient, and the
//! port has to die afterwards exactly as the C makes it die. Both are here, so the consuming
//! sites in `src/port.rs` are one line each.
//!
//! | half | entry point | route |
//! |---|---|---|
//! | read | [`PortRegistry::drain_peer_closes`] / [`PortRegistry::drain_peer_closes_for`] | per port round |
//! | deliver | [`Runtime::deliver_peer_closes`] | the scheduler `run` route (a mailbox write) |
//! | deliver | [`RuntimeContext::poll_peer_closes`] | the `-eval` `receive` route (returned inline) |
//! | retire | [`PortRegistry::retire_peer_closed`] | the C's `driver_exit`, called by both deliver halves |
//!
//! ## The retirement is `driver_exit`, not `port_close/1`
//!
//! `tcp_recv_closed`'s `if (desc->inet.exitf) driver_exit(desc->inet.port, 0)`
//! (`erts/emulator/drivers/common/inet_drv.c:12835-12837`) is a *driver* exit, and `driver_exit`
//! runs `unlink_proc` **before** it delivers (`io.c:7244`). The port therefore leaves the runtime
//! with the `'EXIT'` half of the link family **suppressed** while every monitor still gets its
//! `{'DOWN', Ref, port, S, normal}` — measured on the oracle (OTP 29.1.1 / erts-17.1): a trapping
//! owner linked to the socket receives **nothing** when its peer closes, and after that the port
//! is fully gone (`erlang:port_info(S, connected)` ⇒ `undefined`). [`PortRegistry::close`] is the
//! ordinary `port_close/1` closer and *does* signal the links, so the suppression is recorded on
//! the port's extras immediately before that one call — never at delivery, so the two messages of
//! one close can never disagree.

use std::collections::HashSet;
use std::rc::Rc;

use beam_common::Result;
use beam_process::{PortId, ProcessId, ProcessTable};
use beam_term::{make_atom, Eterm};

use crate::error;
use crate::port::completion::terms_tuple;
use crate::port::{driver_path_name, PortMeta, PortRegistry, RuntimeContext};
use crate::Runtime;

use super::inet_opts::PortInetOptions;

/// `INET_PASSIVE` (0) — the value of the C's `INET_LOPT_ACTIVE` operand that means "no `{active,
/// …}` grant is armed" (`inet_drv.c:786`), and the state `inet_start` leaves a descriptor in
/// (`inet_drv.c:10932`).
const INET_PASSIVE: i32 = 0;

/// The one driver whose peer-close observation this channel reports — `beam_io`'s
/// [`beam_io::drivers::inet::DRIVER_NAME`], matched literally so the module never has to hold a
/// driver in hand (the same rule [`super::active_input`] applies to its own kind).
const TCP_INET: &str = "tcp_inet";

/// Which socket family a peer close belongs to.
///
/// Deliberately a **single** variant: `inet_drv` has no `{udp_closed, _}` message (a datagram
/// socket's ICMP failure is `{udp_error, S, econnrefused}`, a different channel), so a `udp_inet`
/// port produces no report at all rather than a fabricated one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerCloseKind {
    /// A `tcp_inet` port: `{tcp_closed, S}` (`tcp_closed_message`,
    /// `erts/emulator/drivers/common/inet_drv.c:4064-4071`).
    Tcp,
}

impl PeerCloseKind {
    /// The Erlang atom this kind's message is tagged with.
    fn tag(self) -> &'static str {
        match self {
            PeerCloseKind::Tcp => "tcp_closed",
        }
    }
}

/// One due peer-close notification, bound to the port that produced it **and** to everything the
/// caller needs once the port's metadata may be gone.
///
/// Shaped exactly like [`ActiveInputPacket`](super::active_input::ActiveInputPacket) and
/// [`CompletionPacket`]: the `owner`/`kind`/`exit_on_close` triple is resolved at **drain** time,
/// so nothing is read from a port table the delivering route may no longer hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PeerCloseReport {
    /// The port whose peer closed — the `S` element of `{tcp_closed, S}` (the C's
    /// `desc->inet.dport`).
    pub(crate) port: PortId,
    /// The process the message belongs to: the port's **connected** process, which is where the C
    /// delivers (`erl_drv_output_term(desc->inet.dport, …)`). No link and no monitor is needed for
    /// this message — the owner is the recipient by definition.
    pub(crate) owner: ProcessId,
    /// Which socket family the close belongs to (and therefore which message tag it carries).
    pub(crate) kind: PeerCloseKind,
    /// Whether the port must be **retired** once the message has been delivered — the C's
    /// `desc->inet.exitf`, which `tcp_recv_closed` tests right after sending the message
    /// (`:12835-12837`) and honours with `driver_exit(desc->inet.port, 0)`. Defaults to `true`
    /// (`inet_drv.c:10929`), and is `false` only after an accepted `{exit_on_close, false}`.
    pub(crate) exit_on_close: bool,
}

/// The peer-close channel's **once-per-descriptor** latch: the port numbers whose
/// `{tcp_closed, S}` has already been reported.
///
/// A set of numbers rather than a per-port flag because only the *reported* fact needs to be kept:
/// the observation itself is `beam_io`'s (its `port_closed` answer is latched and sticky for the
/// rest of the port's life), so a port that has observed a close but not yet been allowed to report
/// it — a **passive** socket, which the C sends nothing for (`tcp_recv_closed`'s passive arm,
/// `inet_drv.c:12819-12832`) — simply is not in this set. Keying the guard on *reported* rather
/// than on *observed* is what makes the C's sticky rule work: a later `{active, …}` `setopts`
/// re-fires the message on a descriptor that is already closed (`inet_set_opts`,
/// `inet_drv.c:7013-7021`), and it must fire exactly once.
///
/// Held in [`PortExtras`](super::ops::PortExtras), so a closed port's entry goes with the rest of
/// its extras ([`PortExtras::forget_port`](super::ops::PortExtras::forget_port)) — a recycled port
/// number inherits nothing.
#[derive(Debug, Default)]
pub(crate) struct PeerCloseLatches {
    /// The port numbers whose `{tcp_closed, S}` has been reported.
    reported: HashSet<u64>,
}

impl PeerCloseLatches {
    /// Whether `number`'s peer-close message has already been reported.
    pub(crate) fn is_reported(&self, number: u64) -> bool {
        self.reported.contains(&number)
    }

    /// Records that `number`'s peer-close message has been reported — the C's
    /// `TCP_ADDF_CLOSE_SENT` bit.
    pub(crate) fn mark_reported(&mut self, number: u64) {
        self.reported.insert(number);
    }

    /// Forgets everything about `number` — the C's flags dying with the descriptor.
    pub(crate) fn forget(&mut self, number: u64) {
        self.reported.remove(&number);
    }

    /// Drops every latch whose port the caller no longer holds, answering how many were dropped.
    ///
    /// A latch can only outlive its port if a port left the table without going through
    /// [`PortExtras::forget_port`](super::ops::PortExtras::forget_port) — the closure is that
    /// safety net, and it keeps the set bounded by the number of live ports on every route.
    pub(crate) fn prune(&mut self, live: impl Fn(u64) -> bool) -> usize {
        let before = self.reported.len();
        self.reported.retain(|number| live(*number));
        before - self.reported.len()
    }

    /// How many ports this latch currently holds a report for (the suite's witness).
    pub(crate) fn len(&self) -> usize {
        self.reported.len()
    }
}

/// Whether `meta` is a `tcp_inet` port — the only kind this channel can report for.
///
/// The driver name is the first blank-separated word of the port's command (the C's own rule,
/// `io.c`, and this crate's [`driver_path_name`]), so a `{spawn_driver, "tcp_inet"}` port and one
/// opened through a spelled-out object path both resolve.
fn is_tcp_inet(meta: &PortMeta) -> bool {
    driver_path_name(&meta.command) == TCP_INET
}

impl PortRegistry {
    /// Reads every live `tcp_inet` port's **latched** driver peer-close observation once per
    /// port-task round and answers the reports whose delivery is **due now**: the first
    /// observation on an active socket, or a sticky re-fire on a passive→active transition.
    ///
    /// Call this beside the two sibling channels — immediately after
    /// [`drain_io_completions`](PortRegistry::drain_io_completions) and
    /// [`drain_active_inputs`](PortRegistry::drain_active_inputs) — at every point a driver
    /// callback can run, because the close is observed by the same `ready_input`/read path that
    /// stages them.
    ///
    /// Per port the decision is the C's:
    ///
    /// 1. only a `tcp_inet` port is consulted ([`is_tcp_inet`]); every other driver's answer is the
    ///    trait default `false` and this channel has no message family for it;
    /// 2. the driver's answer is [`beam_io::PortRegistry::port_closed`] — latched and sticky, so a
    ///    round that drains late cannot miss it;
    /// 3. the port's mirrored `{active, …}` decides **whether a message exists at all**
    ///    ([`PortInetOptions::active_mode`], passive on `None` — the C's `inet_start` default). A
    ///    passive socket reports nothing **now** and its latch is *not* consumed, so the active
    ///    `setopts` that arrives later makes the report due (the sticky rule);
    /// 4. the once-only guard ([`PeerCloseLatches::is_reported`]) is set **before** the report is
    ///    handed out, so one descriptor yields one `{tcp_closed, S}` however many rounds drain it.
    ///
    /// A report is **term-free** on purpose: the caller wraps it in its own event and delivers it
    /// to [`PeerCloseReport::owner`], and builds the message with
    /// [`RuntimeContext::peer_close_message`] once this registry's borrow has been released — the
    /// same snapshot rule the completion and active-input channels follow.
    ///
    /// Reports are answered in **port-number order**, so two closings in one round have a stable
    /// sequence rather than the hash map's.
    pub(crate) fn drain_peer_closes(&mut self) -> Vec<PeerCloseReport> {
        self.collect_peer_closes(None)
    }

    /// [`PortRegistry::drain_peer_closes`] restricted to the reports addressed to `to` — the
    /// `-eval` route's half (see [`RuntimeContext::poll_peer_closes`]).
    ///
    /// A route that can only observe **its own** mailbox (the `-eval` evaluator) must not consume
    /// another process's report: the message would be lost, because the once-only latch has no
    /// second chance. So a report addressed elsewhere is **not** marked as reported here, and the
    /// next full drain — the scheduler route's [`PortRegistry::drain_peer_closes`], at the same
    /// pass boundary — still produces it. That is strictly better than the sibling channels'
    /// "drop it and let the other route handle it", whose pumps are destructive; here nothing is
    /// destroyed, the latch simply stays undrained.
    pub(crate) fn drain_peer_closes_for(&mut self, to: ProcessId) -> Vec<PeerCloseReport> {
        self.collect_peer_closes(Some(to))
    }

    /// The one drain: `wanted = None` reports every due close, `wanted = Some(to)` only that
    /// recipient's (see the two public wrappers).
    fn collect_peer_closes(&mut self, wanted: Option<ProcessId>) -> Vec<PeerCloseReport> {
        // A latch can only outlive its port on a route that bypassed `PortExtras::forget_port`;
        // this keeps the set bounded by the live ports on every path (`PendingPortSignals::prune`
        // is the same safety net for the close signals).
        self.extras
            .peer_closes
            .prune(|number| self.meta.contains_key(&number));
        let mut numbers: Vec<u64> = self.meta.keys().copied().collect();
        numbers.sort_unstable();
        let mut reports = Vec::new();
        for number in numbers {
            // The recipient and the driver kind are resolved first, so the port metadata's borrow
            // ends before the latch table is touched mutably.
            let Some((owner, is_tcp)) = self
                .meta
                .get(&number)
                .map(|meta| (meta.owner, is_tcp_inet(meta)))
            else {
                continue;
            };
            if !is_tcp {
                continue;
            }
            // A recipient this route cannot deliver to: leave the latch untouched so the route
            // that owns a process table still reports it (see `drain_peer_closes_for`).
            if let Some(wanted) = wanted {
                if owner != wanted {
                    continue;
                }
            }
            let id = PortId::new(number);
            let closed = match self.registry.port_closed(id) {
                Ok(closed) => closed,
                // `beam_io` does not hold this port any more — it closed between this registry's
                // bookkeeping and the drain (a driver that failed on its own port, whose failure
                // channel this runtime has not processed yet). There is nothing left to observe,
                // and the port is about to leave the table with its latch.
                Err(_) => {
                    self.extras.peer_closes.forget(number);
                    continue;
                }
            };
            if !closed {
                // A port that has observed nothing starts clean: a number `beam_io` recycled after
                // a close this runtime never saw must not inherit the previous port's report.
                self.extras.peer_closes.forget(number);
                continue;
            }
            if self.extras.peer_closes.is_reported(number) {
                continue;
            }
            // The active-mode gate: a passive socket's close is answered with no message at all
            // (the C's passive arm), and the latch is *kept* — a later active `setopts` re-fires it.
            if !self.inet_active(id) {
                continue;
            }
            let exit_on_close = self.inet_exit_on_close(id);
            self.extras.peer_closes.mark_reported(number);
            reports.push(PeerCloseReport {
                port: id,
                owner,
                kind: PeerCloseKind::Tcp,
                exit_on_close,
            });
        }
        reports
    }

    /// Retires a peer-closed port exactly as the C's
    /// `driver_exit(desc->inet.port, 0)` does — the second half of
    /// `tcp_recv_closed`'s active arm (`inet_drv.c:12835-12837`).
    ///
    /// [`PortRegistry::close`] is the *Erlang* closer (`port_close/1`) and owes a trapping linked
    /// owner its `{'EXIT', Port, normal}`. `driver_exit` does not: it runs `unlink_proc` first
    /// (`io.c:7244`), so the link family is dropped and only the monitors are told. The mark that
    /// reproduces that order is recorded on the port's extras **before** the close, because
    /// [`PortRegistry::capture_port_exit`](super::PortRegistry) is what snapshots the signals and
    /// it runs inside [`PortRegistry::close`]:
    ///
    /// * `no_owner_exit` is set for this port number;
    /// * [`PortRegistry::close`] snapshots the `'DOWN'`-only set
    ///   (`signals_owner = false`);
    /// * the port's extras — the mark included — are dropped by the close's own
    ///   `forget_port`, so the mark never outlives the close it was made for.
    ///
    /// The `'DOWN'` messages become deliverable at the port's close **report**, like every other
    /// close's signals (see [`super::exit`]), so the caller's delivery route reports them one
    /// round later.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::BadArg`] for a port the runtime does not hold — the same answer
    /// [`PortRegistry::close`] gives, tested **before** the mark is set so a refused retirement
    /// cannot leave one behind — and whatever the teardown or the driver's exit notification
    /// reports.
    pub(crate) fn retire_peer_closed(&mut self, id: PortId) -> Result<()> {
        if !self.is_live(id) {
            return Err(beam_common::BeamError::BadArg);
        }
        self.extras.suppress_owner_exit(id.number());
        self.close(id)
    }

    /// Forgets port `number`'s peer-close latch — the per-port half of
    /// [`PeerCloseLatches::forget`], for a caller that retires a port number outside
    /// [`PortRegistry::close`](PortRegistry::close).
    ///
    /// [`PortRegistry::close`](PortRegistry::close) already drops it with the port's extras, so
    /// this is idempotent: a second call for the same number is a no-op.
    pub(crate) fn forget_peer_close(&mut self, number: u64) {
        self.extras.peer_closes.forget(number);
    }

    /// Whether `id`'s mirrored `{active, …}` mode is anything but `INET_PASSIVE` — the C's
    /// `desc->inet.active` test.
    ///
    /// An unknown mode is **passive**: no `{active, …}` record has ever been accepted for this
    /// port, and the driver starts at `INET_PASSIVE` (`inet_drv.c:10932`). Never an invented
    /// "active" — an unknown mode must not produce a message the oracle would not send.
    ///
    /// Residual: a **spent** grant (`{active, once}` after its one delivery, or `{active, N}` whose
    /// counter reached zero) is still reported as active, because the transition happens inside the
    /// driver and no `{active, …}` record follows it. See the module report.
    fn inet_active(&self, id: PortId) -> bool {
        let mode = self
            .inet_options(id)
            .and_then(PortInetOptions::active_mode);
        matches!(mode, Some(value) if value != INET_PASSIVE)
    }

    /// `id`'s mirrored `{exit_on_close, …}` flag, defaulting to the C's `true`
    /// (`desc->inet.exitf = 1`, `inet_drv.c:10929`).
    fn inet_exit_on_close(&self, id: PortId) -> bool {
        self.inet_options(id)
            .and_then(PortInetOptions::exit_on_close)
            .unwrap_or(true)
    }
}

impl Runtime {
    /// Delivers every due `{tcp_closed, S}` into its **connected process's** mailbox and retires
    /// every port whose `{exit_on_close, …}` is true, answering how many messages were delivered.
    ///
    /// This is the **scheduler** route's half of the channel, shaped exactly like
    /// [`Runtime::deliver_port_signals`](crate::port::Runtime) (which `src/port.rs`'s
    /// `deliver_port_messages` calls at the same pass boundary, with the same `table`): the
    /// message is copied into the recipient's own heap through
    /// [`RuntimeContext::deliver_in_table`], so a receiver parked in a compiled `receive` is woken
    /// through `beam_process`'s wake list exactly as a `{Port, {data, …}}` delivery wakes it.
    ///
    /// Per report the steps are the C's `tcp_recv_closed`:
    ///
    /// 1. a report whose owner the table no longer holds is **skipped** — a dead process has no
    ///    mailbox, and its own death already resolved its side of the relationship;
    /// 2. the message is built on the runtime's arena (`peer_close_message`) **after** the port
    ///    registry's borrow has been released — the snapshot rule every other port delivery site
    ///    follows — and delivered with the port as its sender;
    /// 3. when [`PeerCloseReport::exit_on_close`] is set the port is retired through
    ///    [`PortRegistry::retire_peer_closed`], which is the C's following
    ///    `driver_exit(desc->inet.port, 0)` — `'DOWN'` to the monitors, **no** `'EXIT'` to the
    ///    owner, port fully gone. A port that has already left the table (a concurrent
    ///    `port_close/1`, its own failure channel) is not retired twice.
    ///
    /// Retirement does **not** depend on the delivery: the C sends the message and then kills the
    /// port, so an owner that is gone still takes its port with it.
    ///
    /// # Errors
    ///
    /// A nested borrow of the port registry (a typed capability gap, never a panic), the message
    /// builder's allocation failure, the deliver path's own errors, and the retirement's teardown
    /// errors.
    pub(crate) fn deliver_peer_closes(&mut self, table: &mut ProcessTable) -> Result<usize> {
        // The `Rc` is cloned **before** the borrow so the returned `RefMut` borrows the clone
        // rather than the runtime field — the shape every other port accessor uses.
        let cell = Rc::clone(&self.ports);
        let reports = {
            let mut registry = cell
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry.drain_peer_closes()
        };
        let mut delivered = 0;
        for report in reports {
            if table.contains(report.owner) {
                // The message is built while the context is borrowed, then delivered once that
                // borrow is released.
                let message = {
                    let mut ctx = RuntimeContext::new(&mut *self);
                    ctx.peer_close_message(&report)?
                };
                RuntimeContext::deliver_in_table(
                    self,
                    table,
                    report.owner,
                    report.port.to_eterm(),
                    message,
                )?;
                delivered += 1;
            }
            if report.exit_on_close {
                self.retire_peer_closed_port(report.port)?;
            }
        }
        Ok(delivered)
    }

    /// Retires `id` as a peer close must, skipping a port that has already left the table.
    ///
    /// The liveness test is the whole point: [`PortRegistry::retire_peer_closed`] answers
    /// `badarg` for a port the runtime no longer holds, and that is the *expected* answer when the
    /// port died between the drain and this call (a `port_close/1` from the owner, the driver's
    /// own failure channel). Folding it into a delivery error would fail a scheduler pass for a
    /// port that is already in exactly the state the retirement would have put it in, so it is
    /// skipped here instead of surfaced.
    ///
    /// # Errors
    ///
    /// A nested borrow of the port registry (a typed capability gap) and the teardown's own
    /// failures, which are never swallowed.
    pub(crate) fn retire_peer_closed_port(&mut self, id: PortId) -> Result<()> {
        let cell = Rc::clone(&self.ports);
        let mut registry = cell
            .try_borrow_mut()
            .map_err(|_| error::unsupported("the port table is already in use"))?;
        if !registry.is_live(id) {
            return Ok(());
        }
        registry.retire_peer_closed(id)
    }
}

impl RuntimeContext<'_> {
    /// The messages the peer-close channel owes **this context's own process**, for the `-eval`
    /// `receive` to match inline.
    ///
    /// The `-eval` route can only observe its own mailbox, so it takes exactly the reports
    /// addressed to itself ([`PortRegistry::drain_peer_closes_for`]) and **leaves every other
    /// recipient's latch undrained**, so the scheduler route still reports it — the rule the
    /// completion/active-input/driver-term arms follow, made exact here because the once-only
    /// latch has no second chance to give.
    ///
    /// A report with [`PeerCloseReport::exit_on_close`] set retires its port here, exactly as
    /// [`Runtime::deliver_peer_closes`] does: the C's `tcp_recv_closed` sends the message and then
    /// calls `driver_exit`, so the evaluator's next `port_info/2` on that port finds it gone. The
    /// close's `'DOWN'` messages need the port's close **report**, which a later port round mints
    /// (see [`super::exit`]), so a watcher's `{'DOWN', …}` arrives on the round after — the same
    /// shape `port_close/1` has on this route.
    ///
    /// # Errors
    ///
    /// A nested borrow of the port registry (a typed capability gap), the message builder's
    /// allocation failure and the retirement's teardown errors.
    pub(crate) fn poll_peer_closes(&mut self) -> Result<Vec<Eterm>> {
        let pid = self.pid();
        let cell = Rc::clone(&self.runtime.ports);
        let reports = {
            let mut registry = cell
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry.drain_peer_closes_for(pid)
        };
        let mut messages = Vec::with_capacity(reports.len());
        for report in reports {
            messages.push(self.peer_close_message(&report)?);
            if report.exit_on_close {
                self.runtime.retire_peer_closed_port(report.port)?;
            }
        }
        Ok(messages)
    }

    /// The message one [`PeerCloseReport`] delivers: the 2-tuple `{tcp_closed, S}`.
    ///
    /// Built on the runtime's pointer-stable `-eval` arena (like every other port message) after the
    /// port registry's borrow has been released, with the tag atom interned in the runtime's **one
    /// shared** table so the printer can name it and a `receive` clause can match it. `S` is the
    /// port's own term ([`PortId::to_eterm`]) — never a fabricated atom, and never `S` as it was
    /// spelled at open time.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the tuple does not fit.
    pub(crate) fn peer_close_message(
        &mut self,
        report: &PeerCloseReport,
    ) -> beam_common::Result<Eterm> {
        let tag = report.kind.tag();
        let tag = make_atom(self.runtime.code_server.atoms_mut().intern(tag));
        terms_tuple(self, &[tag, report.port.to_eterm()])
    }
}

/// The channel's suites — the active-mode gate, the once-only latch and its recycle rule, the
/// `exit_on_close` mirror, and one real loopback peer close end to end (see
/// `src/port/peer_closed_tests.rs`).
#[cfg(test)]
#[path = "peer_closed_tests.rs"]
mod tests;
