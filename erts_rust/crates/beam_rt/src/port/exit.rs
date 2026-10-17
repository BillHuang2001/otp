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

//! The signals a **closing port** owes its links and monitors — `{'EXIT', Port, normal}`
//! and `{'DOWN', Ref, port, Port, normal}`.
//!
//! A port is a linkable, monitorable entity: `open_port/2` links it to its owner,
//! `port_connect/2` adds further links, and `erlang:monitor(port, Port)` records an incoming
//! monitor on it. When it closes, every linked process and every watcher is told, exactly as
//! a dying process tells its peers:
//!
//! ```text
//! {'EXIT', Port, normal}             → each link that traps exits
//! {'DOWN', Ref, port, Port, normal}  → each monitor's watcher
//! ```
//!
//! Measured against the oracle (OTP 29.1.1 / erts-17.1), and pinned by
//! `tests/port_exit.rs`:
//!
//! | Who | What it gets on `port_close(P)` |
//! |---|---|
//! | the owner, trapping exits | `{'EXIT', P, normal}` |
//! | the owner, trapping, also watching via `monitor(port, P)` | `{'EXIT', P, normal}` **then** `{'DOWN', Ref, port, P, normal}` |
//! | the owner, **not** trapping | *nothing* — and it survives (`normal` in the link family is ignored, never a termination) |
//! | a watcher that is not linked | just `{'DOWN', Ref, port, P, normal}` |
//! | a `port_connect/2`ed second process, trapping | `{'EXIT', P, normal}` |
//!
//! # Why the policy lives here rather than in `beam_process::ProcessTable::port_closed`
//!
//! `beam_process` already implements exactly this table — `ProcessTable::port_closed(&mut
//! ports, port, reason)` (`ptab.rs`) — but its first step is `ports.close(port)`, which
//! **frees the port number immediately**. `beam_io` deliberately keeps a closed port's number
//! allocated until its [`PortNotification::Closed`](beam_io::PortNotification) report is
//! minted (`beam_io::PortRegistry::take_closed_notifications` → `ids.release`), precisely so a
//! `close` + `open` pair in one slice cannot make a deferred report name a *different, live*
//! port. Freeing the number at close time would reintroduce that aliasing.
//!
//! So the obligation is discharged in two steps instead:
//!
//! 1. **Snapshot at close** ([`PortRegistry::capture_port_exit`], called from
//!    [`PortRegistry::close`](super::PortRegistry) while the port's metadata, link set and
//!    incoming monitors are still readable). Nothing is delivered yet and no table is
//!    mutated, so the port number stays reserved exactly as `beam_io` requires.
//! 2. **Deliver at the close report** — the `PortNotification::Closed` arm of
//!    [`PortRegistry::fold_notifications`](super::PortRegistry) moves the snapshot into the
//!    deliverable queue — which is the one point at which the port really has left the
//!    runtime. The queue is then drained by whichever route owns a process table:
//!    [`Runtime::deliver_port_signals`] (the scheduler `run` route, into the recipient's
//!    mailbox) or [`RuntimeContext::poll_port_signals`] (the `-eval` `receive` route, whose
//!    messages are matched inline).
//!
//! The snapshot is per **recipient**, so a route that can only observe its own mailbox takes
//! its own entries and leaves the rest queued for the route that can deliver them.
//!
//! # What is honest about the reason
//!
//! `port_close/1` is this runtime's only closer and always closes with the reason `normal`
//! (the oracle's own reason for it), so the reason is not carried in the snapshot. A future
//! closer with another reason (`exit_status`-driven death, an owner-death cascade) must add
//! it as a field rather than special-casing the builder.
//!
//! # Known divergence
//!
//! `erlang:monitor/3`'s `{tag, Tag}` user tag for a **port** monitor lives in
//! `beam_process`'s private `ProcessTable::tags` map, which no API of that crate exposes, so
//! a tagged port monitor's `'DOWN'` carries the default `'DOWN'` head instead of the caller's
//! tag. The untagged shape — the overwhelming default and the one the oracle's own library
//! code relies on — is byte-exact. Reference **aliases** on a port monitor are already a
//! typed capability gap in `beam_process` (`unsupported_port_monitor_alias`), so no alias has
//! to be resolved here.

use std::collections::HashMap;
use std::rc::Rc;

use beam_common::Result;
use beam_engine::terms;
use beam_process::{MonitorRef, PortId, ProcessId, ProcessTable};
use beam_term::{make_atom, Eterm};

use crate::context::RuntimeContext;
use crate::error;
use crate::Runtime;

use super::{PortEvent, PortRegistry};

/// Which of the two messages a closing port owes one recipient.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PortSignalKind {
    /// The port's exit, as a **linked** process sees it: `{'EXIT', Port, normal}`.
    Exit,
    /// The port's death, as the holder of `reference` sees it:
    /// `{'DOWN', Reference, port, Port, normal}`.
    Down(MonitorRef),
}

/// One message a closing port owes **one** process.
///
/// Per recipient rather than per port so that a route which can only observe its own mailbox
/// (the `-eval` evaluator) can take exactly its own entries and leave the rest for the route
/// that owns a process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PortSignal {
    /// The port that closed — the source term of the message, and the `'DOWN'` target.
    pub(crate) port: PortId,
    /// The process the message belongs to.
    pub(crate) to: ProcessId,
    /// Which message it is.
    pub(crate) kind: PortSignalKind,
}

/// The two-stage queue of a closing port's signals (see the module docs).
///
/// `unreported` holds the snapshots of ports whose `PortNotification::Closed` report is still
/// outstanding — keyed by **port number**, which `beam_io` guarantees cannot be reused until
/// the report is minted, so a key can never name a live port. `ready` holds the entries whose
/// report has been folded, i.e. whose port has genuinely left the runtime and which are
/// therefore deliverable.
#[derive(Debug, Default)]
pub(crate) struct PendingPortSignals {
    /// Port number → the messages its close owes. Drained as each port's close is reported.
    unreported: HashMap<u64, Vec<PortSignal>>,
    /// The entries of every reported close, in report order, awaiting a delivery route.
    ready: Vec<PortSignal>,
}

impl PendingPortSignals {
    /// Whether every queue is empty — the `has_nothing_to_poll` conjunct.
    pub(crate) fn is_empty(&self) -> bool {
        self.unreported.is_empty() && self.ready.is_empty()
    }

    /// Whether a close report is still outstanding, so a port round must be run to mint it
    /// (see [`PortRegistry::report_pending_closes`]).
    pub(crate) fn has_unreported(&self) -> bool {
        !self.unreported.is_empty()
    }

    /// Records the messages the close of `port` owes.
    fn stash(&mut self, port: PortId, signals: Vec<PortSignal>) {
        self.unreported.insert(port.number(), signals);
    }

    /// Moves one port's snapshot into the deliverable queue — the `Closed` report's half.
    pub(super) fn report(&mut self, port: PortId) {
        if let Some(signals) = self.unreported.remove(&port.number()) {
            self.ready.extend(signals);
        }
    }

    /// Takes every deliverable entry.
    fn take(&mut self) -> Vec<PortSignal> {
        std::mem::take(&mut self.ready)
    }

    /// Takes the deliverable entries addressed to `to`, leaving the rest queued in order.
    fn take_for(&mut self, to: ProcessId) -> Vec<PortSignal> {
        let mut taken = Vec::new();
        let mut rest = Vec::with_capacity(self.ready.len());
        for signal in std::mem::take(&mut self.ready) {
            if signal.to == to {
                taken.push(signal);
            } else {
                rest.push(signal);
            }
        }
        self.ready = rest;
        taken
    }
}

impl PortRegistry {
    /// Snapshots the `'EXIT'`/`'DOWN'` messages the close of `id` owes, **before** the port's
    /// metadata, links and monitors are dropped.
    ///
    /// The recipients come from the two places the relationships live:
    ///
    /// * **links** — [`PortRegistry::extra_links`](super::PortRegistry) holds
    ///   `[original_owner, …connects]` from the first `port_connect/2` on; a port never
    ///   re-connected has exactly one link, its owner (`open_port/2`'s own link, `io.c`
    ///   `set_port_connected`'s non-`!` form), which is what the `None` case supplies;
    /// * **monitors** — the port's incoming set is `beam_process::PortTable::monitors`, the
    ///   authoritative record `erlang:monitor(port, Port)` writes and
    ///   [`Runtime::drain_port_monitor_exits`](crate::Runtime) sweeps.
    ///
    /// Which of them actually produces a message is decided at **delivery** time, because the
    /// answer depends on the recipient's `trap_exit` flag (see
    /// [`Runtime::deliver_port_signals`]) and on whether the recipient is still alive; neither
    /// is readable here (this registry holds no process table).
    pub(super) fn capture_port_exit(&mut self, id: PortId) {
        let Some(owner) = self.meta.get(&id.number()).map(|meta| meta.owner) else {
            return;
        };
        let links = match self.extra_links(id.number()) {
            Some(links) => links.to_vec(),
            None => vec![owner],
        };
        let mut signals = Vec::with_capacity(links.len() + 1);
        for to in links {
            signals.push(PortSignal {
                port: id,
                to,
                kind: PortSignalKind::Exit,
            });
        }
        // The `'EXIT'` entries come first because that is the order the oracle delivers them
        // in: a process that is both linked and watching sees the exit before the `'DOWN'`
        // (pinned by `tests/port_exit.rs`).
        for monitor in self.port_table().monitors(id) {
            signals.push(PortSignal {
                port: id,
                to: monitor.watcher,
                kind: PortSignalKind::Down(monitor.reference),
            });
        }
        if !signals.is_empty() {
            self.pending_port_signals.stash(id, signals);
        }
    }

    /// Mints the close reports of every port whose `Closed` notification is still
    /// outstanding, folding them (so their signals become deliverable) into `events`.
    ///
    /// [`PortRegistry::pump`](super::PortRegistry) calls this on its "nothing live to poll"
    /// path: `beam_io` retires a closed port's number only when its report is minted, and a
    /// port that was the last one open leaves the poller with nothing to wait on, so without
    /// this the report — and the signals it releases — would only ever appear if some *other*
    /// port happened to keep the round running. `beam_io::PortRegistry::handle_events` is the
    /// documented close-report path for exactly this shape (an empty readiness batch).
    ///
    /// Costs nothing when no close is outstanding.
    ///
    /// # Errors
    ///
    /// A round's driver error (which leaves the reports queued for the next round, since
    /// `beam_io` mints them only after every fallible step succeeded), and a typed capability
    /// gap for a close that a completed round still did not report — a `beam_io` invariant
    /// violation, reported rather than looping on a permanently-impossible delivery.
    pub(super) fn report_pending_closes(&mut self, events: &mut Vec<PortEvent>) -> Result<()> {
        if !self.pending_port_signals.has_unreported() {
            return Ok(());
        }
        let outcome = self.registry.handle_events(&mut self.poller, &[])?;
        self.fold_notifications(outcome.notifications, events);
        if self.pending_port_signals.has_unreported() {
            return Err(error::unsupported(
                "a closed port's report was not minted by the port round",
            ));
        }
        Ok(())
    }

    /// Takes every deliverable signal — the scheduler route's half (see
    /// [`Runtime::deliver_port_signals`]).
    pub(crate) fn take_port_signals(&mut self) -> Vec<PortSignal> {
        self.pending_port_signals.take()
    }

    /// Takes the deliverable signals addressed to `to`, leaving the others queued — the
    /// `-eval` route's half (see [`RuntimeContext::poll_port_signals`]).
    pub(crate) fn take_port_signals_for(&mut self, to: ProcessId) -> Vec<PortSignal> {
        self.pending_port_signals.take_for(to)
    }
}

impl Runtime {
    /// Delivers every message a closing port owes into its recipient's mailbox, returning how
    /// many were delivered.
    ///
    /// This is the **scheduler** route's half of the port-exit family (see the module docs):
    /// [`Runtime::deliver_port_messages`] calls it at the same pass boundary, with the same
    /// `table`, so a receiver parked in a compiled `receive` is woken through
    /// `beam_process`'s own wake list exactly as a `{Port, {data, …}}` delivery wakes it.
    ///
    /// The policy applied per entry is the oracle's (module docs):
    ///
    /// * a recipient the table no longer holds is **skipped** — a dead process has no mailbox,
    ///   and its own death already resolved its side of the relationship;
    /// * a **linked** process gets `{'EXIT', Port, normal}` only when it traps exits: the C
    ///   consults `exit_policy` in the link-exit family, where a `normal` reason is *ignored*
    ///   by a non-trapping peer — and, crucially, **ignores** it rather than terminating, so
    ///   the peer survives;
    /// * a **watcher** gets `{'DOWN', Ref, port, Port, normal}` unconditionally, and its
    ///   outgoing `PortWatch` is dropped here (the port's death resolves the monitor on both
    ///   sides, as `ProcessTable::terminate` does for a process monitor), so
    ///   `erlang:process_info(Watcher, monitors)` stops listing it.
    ///
    /// # Errors
    ///
    /// A nested borrow of the port registry (a typed capability gap, never a panic), the term
    /// builders' allocation failure, and the deliver path's own errors.
    pub(crate) fn deliver_port_signals(&mut self, table: &mut ProcessTable) -> Result<usize> {
        // The `Rc` is cloned **before** the borrow so the returned `RefMut` borrows the clone
        // rather than the runtime field — the shape every other port accessor uses.
        let cell = Rc::clone(&self.ports);
        let signals = {
            let mut registry = cell
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry.take_port_signals()
        };
        let mut delivered = 0;
        for signal in signals {
            if deliver_signal(self, table, signal)? {
                delivered += 1;
            }
        }
        Ok(delivered)
    }
}

impl RuntimeContext<'_> {
    /// The messages a closing port owes **this context's own process**, for the `-eval`
    /// `receive` to match inline.
    ///
    /// The `-eval` route can only observe its own mailbox, so it takes exactly its own
    /// entries ([`PortRegistry::take_port_signals_for`]) and leaves every other recipient's
    /// queued for the scheduler route. The policy is
    /// [`Runtime::deliver_port_signals`]'s; the only difference is where the message ends up
    /// (returned to the caller instead of enqueued).
    ///
    /// # Errors
    ///
    /// A nested borrow of the port registry (a typed capability gap) and the message
    /// builders' allocation failure.
    pub(crate) fn poll_port_signals(&mut self) -> Result<Vec<Eterm>> {
        let pid = self.pid();
        let signals = {
            let cell = Rc::clone(&self.runtime.ports);
            let mut registry = cell
                .try_borrow_mut()
                .map_err(|_| error::unsupported("the port table is already in use"))?;
            registry.take_port_signals_for(pid)
        };
        if signals.is_empty() {
            return Ok(Vec::new());
        }
        // The evaluator's own process must exist for its `trap_exit` flag (and its outgoing
        // port monitors) to be readable; a context whose process is already gone has no
        // mailbox, so the entries are dropped exactly as the scheduler route drops them.
        let Some(trapping) = self
            .process_table()
            .get(pid)
            .map(beam_process::Process::is_trapping_exit)
        else {
            return Ok(Vec::new());
        };
        let mut messages = Vec::with_capacity(signals.len());
        for signal in signals {
            match signal.kind {
                // `normal` in the link family is ignored by a non-trapping peer (the C's
                // `exit_policy`); nothing is delivered and nothing is terminated.
                PortSignalKind::Exit if !trapping => continue,
                PortSignalKind::Exit => messages.push(self.port_closed_message(signal.port)?),
                PortSignalKind::Down(reference) => {
                    // The port's death resolves the watcher's outgoing half (see
                    // [`Runtime::deliver_port_signals`]).
                    if let Some(watcher) = self.process_table_mut().get_mut(pid) {
                        watcher
                            .port_monitors_mut()
                            .retain(|watch| watch.reference != reference);
                    }
                    messages.push(self.port_down_message(signal.port, reference)?);
                }
            }
        }
        Ok(messages)
    }

    /// `{'EXIT', Port, normal}` — a closing port's exit as a linked process sees it.
    ///
    /// Built on the runtime's pointer-stable arena through the same
    /// [`beam_engine::terms`] seam every other port message uses, with the `'EXIT'`/`normal`
    /// names resolved in the runtime's **one shared** atom table (both are already part of
    /// the boot seed, and interning is idempotent).
    ///
    /// # Errors
    ///
    /// The arena's allocation failure when the tuple does not fit.
    fn port_closed_message(&mut self, port: PortId) -> Result<Eterm> {
        let tag = intern(self, "EXIT");
        let normal = intern(self, "normal");
        terms::make_tuple(self, &[tag, port.to_eterm(), normal])
    }

    /// `{'DOWN', Reference, port, Port, normal}` — a closing port's death as its watcher sees
    /// it.
    ///
    /// The monitor **kind** slot carries the `port` atom (never `process`), and the target is
    /// the port immediate, exactly as the C's `erl_monitor_link.c` spells a port monitor's
    /// message.
    ///
    /// # Errors
    ///
    /// The arena's allocation failure, the reference thing's encoding refusal (an id that
    /// does not fit), and the pooled BIF heap's allocation failure.
    fn port_down_message(&mut self, port: PortId, reference: MonitorRef) -> Result<Eterm> {
        let tag = intern(self, "DOWN");
        let kind = intern(self, "port");
        let normal = intern(self, "normal");
        let reference = monitor_reference_term(self, reference)?;
        terms::make_tuple(self, &[tag, reference, kind, port.to_eterm(), normal])
    }
}

/// Delivers one queued signal into `table`, answering whether it was delivered.
///
/// The whole policy of the port-exit family in one place (see
/// [`Runtime::deliver_port_signals`]); shared with nothing else because the `-eval` route
/// returns its messages instead of enqueuing them.
fn deliver_signal(
    runtime: &mut Runtime,
    table: &mut ProcessTable,
    signal: PortSignal,
) -> Result<bool> {
    let Some(recipient) = table.get(signal.to) else {
        // A recipient that has already terminated has no mailbox: the message is dropped,
        // exactly as a `{Port, {data, …}}` for a dead owner is.
        return Ok(false);
    };
    let trapping = recipient.is_trapping_exit();
    match signal.kind {
        PortSignalKind::Exit if !trapping => return Ok(false),
        PortSignalKind::Exit => {}
        PortSignalKind::Down(reference) => {
            if let Some(watcher) = table.get_mut(signal.to) {
                watcher
                    .port_monitors_mut()
                    .retain(|watch| watch.reference != reference);
            }
        }
    }
    // The message is built on the runtime's arena while the context is borrowed, then
    // delivered once that borrow is released — the snapshot rule every other port delivery
    // site in this crate follows.
    let message = {
        let mut ctx = RuntimeContext::new(runtime);
        match signal.kind {
            PortSignalKind::Exit => ctx.port_closed_message(signal.port)?,
            PortSignalKind::Down(reference) => ctx.port_down_message(signal.port, reference)?,
        }
    };
    let from = signal.port.to_eterm();
    RuntimeContext::deliver_in_table(runtime, table, signal.to, from, message)?;
    Ok(true)
}

/// Interns `name` in the runtime's **one shared** atom table and returns it as a term.
///
/// The same rule [`RuntimeContext::port_data_message`](crate::port) follows:
/// `beam_rt`'s live table is the single namespace every message, guard and printer reads, so
/// a name interned here is nameable and matchable by a `receive` clause.
fn intern(ctx: &mut RuntimeContext<'_>, name: &str) -> Eterm {
    make_atom(ctx.runtime.code_server_atoms_mut().intern(name))
}

/// `reference` as the reference term a `{'DOWN', …}` carries.
///
/// Built on the pooled BIF heap — the same home [`RuntimeContext::mint_reference`]
/// (`crate::native::process`) gives `erlang:make_ref/0` and a `monitor/2`'s reference — whose
/// words stay valid because the pool is not rewound on this path. A `&mut Heap` is what the
/// frozen `MonitorRef` layout writer needs, and `beam_process` owns that writer, so the heap
/// is taken before the tuple is built and returned right after.
///
/// [`RuntimeContext::mint_reference`]: crate::context::RuntimeContext
fn monitor_reference_term(
    ctx: &mut RuntimeContext<'_>,
    reference: MonitorRef,
) -> Result<Eterm> {
    let mut heap = ctx.runtime.take_bif_heap()?;
    let term = reference.to_eterm(&mut heap);
    ctx.runtime.put_bif_heap(heap);
    term
}
