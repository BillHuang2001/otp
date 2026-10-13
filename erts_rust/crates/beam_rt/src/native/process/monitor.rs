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

//! `erlang:monitor/2,3` and `erlang:demonitor/1,2`: the monitor half of the runtime-native
//! process/registry/reference floor.
//!
//! The machinery is `beam_process`'s — [`ProcessTable::monitor`] for a pid target,
//! [`ProcessTable::monitor_named`] for a registered name, [`ProcessTable::demonitor`] and
//! [`ProcessTable::demonitor_flush`] for the removal. Those four own the whole monitor
//! lifecycle (the caller's outgoing `Monitor`, the target's incoming `MonitoredBy`, the
//! named-monitor descriptor a `{'DOWN', …}` is spelled from, and the immediate `noproc`
//! `'DOWN'` a dead pid or an unregistered name is born with); this module only decodes the
//! BIF's terms onto them (call down, never duplicate).
//!
//! # Why `demonitor` needs a registry
//!
//! `demonitor(Ref)` must recover the `MonitorRef` a term stands for. Reading a reference
//! thing's payload (its id word) needs `beam_term::boxed_val`, which is `unsafe`, and this
//! crate is `#![deny(unsafe_code)]`, so the runtime instead **records every reference it
//! mints**, keyed by the term itself ([`Runtime::references`](crate::Runtime::references)).
//! A reference term is the address of a freshly allocated 4-word thing, so the term is a
//! unique key; the entry is `Some(MonitorRef)` for a monitor reference and `None` for a
//! bare `make_ref/0` ([`Runtime::refs_mut`](crate::Runtime::refs_mut)). That is also what
//! makes `demonitor/1`'s argument check exact: a term with no entry that is **not a
//! reference** is the oracle's `badarg`, while a *reference* with no entry — an
//! `erlang:send_after/3` timer reference, which the timer heap owns — backs no monitor and
//! so has nothing to remove (`demonitor/1` answers `true`; `demonitor/2`'s `[info]` boolean
//! is `false`), exactly as the oracle's `demonitor(make_ref())` does. The C's own test is the
//! purely structural `is_not_internal_ref` (`bif.c:443-450`).
//!
//! # The targets the oracle accepts
//!
//! `monitor(process, Pid)`, `monitor(process, RemotePid)`, `monitor(process, Name)` and
//! `monitor(process, {Name, Node})` are all resolved (the last only when `Node` is **this**
//! node; a **remote** `{Name, Node}` is the one target that stays a typed capability gap —
//! see below), and `monitor(port, Port)` is recorded on the runtime's **port table**
//! (`beam_io`'s `PortRegistry`, reached through
//! [`crate::port::PortRegistry::port_table_mut`]) — the same table `open_port/2` allocates
//! from and `port_close/1` releases, so the monitor's `{'DOWN', Ref, port, Port, Reason}`
//! fires when the port closes.
//!
//! A **remote** (external) pid is a *remote monitor*: `ProcessTable::monitor_term_with_options`
//! records it in the watcher's outgoing remote-monitor set and routes the `DOP_MONITOR_P`
//! control signal; `demonitor/1,2` shares that path, because `ProcessTable::demonitor`
//! falls back to its outgoing remote monitors and routes `DOP_DEMONITOR_P`
//! (`beam_process/src/ptab.rs:1580-1588`, `demonitor_remote`). Oracle-verified against a real
//! `erl -sname` peer: `monitor(process, RemotePid)` ⇒ a reference and
//! `demonitor(Ref, [info])` ⇒ `true`. The wire signal only reaches a socket when this
//! runtime **is** a node with an established link to the peer's node; otherwise the router
//! reports its own typed error, which is propagated (never a silently dropped monitor).
//!
//! The **remote `{Name, Node}`** target remains the typed gap it always was:
//! `ProcessTable::monitor_named_with_options` resolves the name against **this** node's
//! registration table and has no node argument that would re-address the wire signal (a
//! named monitor's `DOP_MONITOR_P` is a *name* signal), so passing a foreign node there
//! would install a monitor on a *local* process the caller never asked about — a wrong
//! result. It is therefore refused precisely (`named_node_target`).
//!
//! # `erlang:monitor/3`'s option list
//!
//! [`RuntimeContext::monitor_options`] is the `beam_rt` side of the C's `erts_monitor_opts`
//! (`bif.c:701-756`): it walks the list and builds the [`MonitorOptions`] the
//! `beam_process` `*_with_options` entry points consume.
//!
//! * the bare atom `priority` is accepted and **ignored** (the C sets `ERTS_ML_FLG_PRIO_ML`,
//!   which has no observable effect here),
//! * `{tag, Tag}` supplies the `{'DOWN', …}` head atom, honoured for an
//!   **immediately-representable** tag — one resident in a per-process heap stays the typed
//!   capability gap (`beam_process`'s `error::unsupported_tag`), because a monitor record is
//!   a plain value the collector never scans — and
//! * `{alias, Mode}` makes the monitor's own reference double as a **reference alias**
//!   (`explicit_unalias` / `demonitor` / `reply_demonitor`), so a message sent to it is
//!   delivered to the watcher.
//!
//! Anything else is the oracle's `badarg`, as is a non-list. An empty list — or any list
//! yielding [`MonitorOptions::NONE`] — is `monitor/2` byte for byte. The option list is
//! parsed **first**, so a malformed option outranks a malformed target, exactly as in the C.
//!
//! # The remaining capability gaps
//!
//! A **named** port monitor (`monitor(port, Name)`) is the typed
//! `beam_process`-owned `error::named_port_monitor()`: the process registry resolves names to
//! *processes* only, so recording the process the name resolves to would be a wrong (process)
//! monitor. An `{alias, Mode}` option on a **port** monitor is likewise
//! `beam_process`'s typed `error::unsupported_port_monitor_alias()` (a port alias's reply
//! site has no port table), and one on a remote target is the Milestone-1 distribution gap.

use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_process::{
    MonitorKind, MonitorOptions, MonitorRef, PidTarget, PortTable, ProcessId, ProcessTable,
    UnaliasMode,
};
use beam_term::{atom_val, is_atom, Atom, Eterm};

use crate::context::RuntimeContext;
use crate::error;

/// The target `erlang:monitor(process, Target)` watches, once resolved.
enum MonitorTarget {
    /// A local pid: `ProcessTable::monitor` resolves it itself, so an unknown pid is born down.
    Pid(ProcessId),
    /// A **remote** (external) pid: a remote monitor, installed by
    /// `ProcessTable::monitor_term_with_options` (it is recorded in the watcher's outgoing
    /// remote-monitor set *and* routed as a `DOP_MONITOR_P` control signal). The term is
    /// carried through unchanged so that entry point decomposes it into its stable parts.
    Remote(Eterm),
    /// A registered name on `node`.
    Named {
        /// The registered name.
        name: Atom,
        /// The node the registration is resolved on.
        node: Atom,
    },
}

impl RuntimeContext<'_> {
    // -- erlang:monitor/2,3 ---------------------------------------------------

    /// `erlang:monitor(process, Target)`: a fresh reference, with the monitor recorded on
    /// both sides (the caller's outgoing `Monitors` and the target's `monitored_by`).
    ///
    /// `Target` may be a pid, a **remote** pid, a registered **name**, or a `{Name, Node}`
    /// pair on this node. A pid — local or remote — or a local name the table does not hold
    /// still yields a reference, exactly as the oracle's does: the monitor is born down and
    /// the caller is sent the matching `{'DOWN', …}` with reason `noproc` (delivered by
    /// `beam_process`; a *remote* pid is recorded and routed instead — see the module docs).
    ///
    /// A `port` kind is served (the monitor is recorded on the runtime's **port table**, the
    /// one `open_port/2` allocates from — a non-port target is `badarg`, a registered *name*
    /// the typed named-port-monitor gap). A remote `{Name, Node}` target is the one target
    /// rejected as the typed capability gap, and any other kind is the oracle's `badarg`.
    pub(in crate::native) fn monitor_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let kind = *args.first().ok_or(BeamError::BadArg)?;
        let target = *args.get(1).ok_or(BeamError::BadArg)?;
        let kind = self.atom_name(kind).ok_or(BeamError::BadArg)?;
        match kind.as_str() {
            "process" => self.monitor_process(target, MonitorOptions::NONE),
            // A port monitor is recorded on the runtime's **port table** — the same one
            // `open_port/2` allocates from, so the `{'DOWN', …}` fires when the port closes.
            // The target of the oracle's `monitor(port, X)` must be a port (a non-port is
            // `badarg`, a registered *name* the typed named-port-monitor gap).
            "port" => self.monitor_port(target, MonitorOptions::NONE),
            _ => Err(BeamError::BadArg),
        }
    }

    /// `erlang:monitor(process, Target, Options)`: `monitor/2` plus the option list.
    ///
    /// The options are parsed (see [`Self::monitor_options`], the C's `erts_monitor_opts`)
    /// and forwarded to the matching `beam_process` `*_with_options` entry point: a `{tag,
    /// Tag}` gives the monitor's `{'DOWN', …}` a caller-chosen tag and an `{alias, Mode}`
    /// makes its reference a send destination for the watcher. Every other element — an
    /// unknown atom, a 3-tuple, an `{alias, _}` with an unknown mode — is the oracle's
    /// `badarg`, as is a non-list. The empty list is `monitor/2` exactly, and so is any list
    /// yielding [`MonitorOptions::NONE`], byte for byte.
    pub(in crate::native) fn monitor_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        // Parsed **first**, so a malformed option list outranks a malformed target — the C's
        // `erts_monitor_opts` runs before `monitor()` looks at `type`/`target` at all.
        let options = self.monitor_options(*args.get(2).ok_or(BeamError::BadArg)?)?;
        let kind = *args.first().ok_or(BeamError::BadArg)?;
        let target = *args.get(1).ok_or(BeamError::BadArg)?;
        let kind = self.atom_name(kind).ok_or(BeamError::BadArg)?;
        match kind.as_str() {
            "process" => self.monitor_process(target, options),
            "port" => self.monitor_port(target, options),
            _ => Err(BeamError::BadArg),
        }
    }

    // -- erlang:demonitor/1,2 -------------------------------------------------

    /// `erlang:demonitor(Ref)`: removes the monitor and answers `true`.
    ///
    /// A reference always answers `true` — a monitor's (whatever the removal did), a bare
    /// `make_ref/0` reference, an `erlang:send_after/3` timer reference, and an
    /// already-demonitored one. Only a term that is **not a reference at all** is `badarg`
    /// (the oracle's reason for `demonitor(not_a_ref)`).
    pub(in crate::native) fn demonitor_1(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        // The removal's own report is discarded: `demonitor/1` answers `true` whatever it
        // removed, exactly as the oracle does.
        let _removed = self.remove_monitor(term, false)?;
        self.atom("true")
    }

    /// `erlang:demonitor(Ref, Options)`: `demonitor/1` plus the option list.
    ///
    /// The options must be a list of the atoms `info`/`flush` (anything else is `badarg`,
    /// as the oracle's is). Oracle-verified (OTP 29/erts-17.1): `[info]` answers the
    /// **real** boolean — `true` when a monitor was actually on record and got removed,
    /// `false` when there was none — and every other option list answers `true`.
    pub(in crate::native) fn demonitor_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let term = *args.first().ok_or(BeamError::BadArg)?;
        let options = terms::list_elements(*args.get(1).ok_or(BeamError::BadArg)?)
            .map_err(|_| BeamError::BadArg)?;
        let mut info = false;
        let mut flush = false;
        for option in options {
            match self.atom_name(option).as_deref() {
                Some("info") => info = true,
                Some("flush") => flush = true,
                _ => return Err(BeamError::BadArg),
            }
        }
        let removed = self.remove_monitor(term, flush)?;
        if info {
            self.atom(if removed { "true" } else { "false" })
        } else {
            self.atom("true")
        }
    }

    // -- helpers --------------------------------------------------------------

    /// `erlang:monitor(process, Target)` for the resolved target.
    ///
    /// `options` is the parsed `erlang:monitor/3` option set ([`MonitorOptions::NONE`] for
    /// `monitor/2`), forwarded to the option-aware `beam_process` entry point so the eventual
    /// `{'DOWN', …}` carries the user tag and the reference doubles as an alias.
    fn monitor_process(&mut self, target: Eterm, options: MonitorOptions) -> Result<Eterm> {
        // The target is resolved **first**, so a malformed target keeps the oracle's
        // `badarg` (and a remote `{Name, Node}` its capability gap) even when the caller
        // itself has no process.
        let target = self.monitor_target(target)?;
        let watcher = self.pid();
        if !self.process_table_mut().contains(watcher) {
            return Err(error::no_process(watcher));
        }
        let reference = match target {
            MonitorTarget::Pid(pid) => self.process_table_mut().monitor_with_options(
                watcher,
                pid,
                MonitorKind::Process,
                options,
            )?,
            // A remote pid is a **remote monitor**: the single `beam_process` entry point for
            // an arbitrary pid term records the outgoing monitor in the watcher's remote set
            // and routes the `DOP_MONITOR_P` control signal (the same entry point
            // `erlang:monitor/2` on a remote pid is *supposed* to reach — the front door used
            // to answer `badarg` for one). A `{tag, Tag}`/`{alias, Mode}` option on a remote
            // target is that crate's typed capability gap (the wire signal carries neither),
            // propagated rather than silently dropped.
            MonitorTarget::Remote(term) => self.process_table_mut().monitor_term_with_options(
                watcher,
                term,
                MonitorKind::Process,
                options,
            )?,
            MonitorTarget::Named { name, node } => self
                .process_table_mut()
                .monitor_named_with_options(watcher, name, node, MonitorKind::Process, options)?,
        };
        let term = self.reference_term(reference)?;
        // Recorded whatever happened, so `demonitor/1,2` recognises the reference: a
        // monitor on a pid or name the table does not hold is born down, and the oracle
        // still lets it be demonitored. `Some` marks it as a **monitor** (a real `Monitor`
        // entry, or its alias-carrying sibling), which is what `demonitor/2`'s `[info]`
        // boolean reads.
        self.runtime.references.insert(term, Some(reference));
        Ok(term)
    }

    /// `erlang:monitor(port, Target)` for the port target — recorded on the runtime's
    /// **port table** ([`crate::port::PortRegistry::port_table_mut`]).
    ///
    /// `Target` must be a port term: a non-port is [`BeamError::BadArg`] (the oracle's own
    /// reason) and a registered **name** is `beam_process`'s typed named-port-monitor gap —
    /// this never records a process monitor for a port-typed request. A port that is already
    /// closed still yields a reference (the monitor is born down with reason `noproc`), exactly
    /// as the oracle's does, and the reference is recorded so `demonitor/1` recognises it.
    ///
    /// `options` is the parsed `erlang:monitor/3` option set; an `{alias, Mode}` in it is the
    /// typed `beam_process` port-alias gap, propagated rather than swallowed.
    fn monitor_port(&mut self, target: Eterm, options: MonitorOptions) -> Result<Eterm> {
        let watcher = self.pid();
        let reference = self.with_port_table(|table, ports| {
            if !table.contains(watcher) {
                return Err(error::no_process(watcher));
            }
            table.monitor_port_term_with_options(watcher, target, ports, options)
        })?;
        let term = self.reference_term(reference)?;
        self.runtime.references.insert(term, Some(reference));
        Ok(term)
    }

    /// Runs `f` with the process table this context resolves against **and** the runtime's
    /// live **port table**, both borrowed for the one call.
    ///
    /// The port table lives behind the runtime's shared `Rc<RefCell<PortRegistry>>` (see
    /// [`crate::port`]). The `Rc` is cloned **first**, so the returned [`RefMut`] borrows the
    /// clone rather than the runtime — which is what lets the process table be borrowed at the
    /// same time (the `#![deny(unsafe_code)]` runtime may not alias). A **nested** borrow (a
    /// port operation re-entered from inside one) stays the typed capability gap, never a
    /// panic.
    ///
    /// [`RefMut`]: std::cell::RefMut
    fn with_port_table<R>(
        &mut self,
        f: impl FnOnce(&mut ProcessTable, &mut PortTable) -> Result<R>,
    ) -> Result<R> {
        let cell = Rc::clone(&self.runtime.ports);
        let mut registry = cell
            .try_borrow_mut()
            .map_err(|_| error::unsupported("the port table is already in use"))?;
        f(self.process_table_mut(), registry.port_table_mut())
    }

    /// The target `erlang:monitor(process, Target)` watches.
    ///
    /// A **local** pid always resolves (the oracle returns a reference even for a pid that
    /// does not exist, and the monitor then fires `noproc`); a **remote** (external) pid is a
    /// *remote monitor* ([`MonitorTarget::Remote`]) — the oracle answers a reference there
    /// too (measured against a real peer: `erlang:monitor(process, RemotePid)` ⇒ `#Ref<>`),
    /// so the front door must not reject it; an atom is a name registered on **this** node; a
    /// `{Name, Node}` pair needs `Node` to be this node (see
    /// [`Self::named_node_target`]); anything else is `badarg`, the oracle's reason.
    fn monitor_target(&self, target: Eterm) -> Result<MonitorTarget> {
        match PidTarget::from_eterm(target) {
            Ok(PidTarget::Local(pid)) => return Ok(MonitorTarget::Pid(pid)),
            Ok(PidTarget::Remote(_)) => return Ok(MonitorTarget::Remote(target)),
            Err(_) => {}
        }
        if is_atom(target) {
            return Ok(MonitorTarget::Named {
                name: atom_val(target),
                node: self.runtime.local_node(),
            });
        }
        if terms::tuple_arity(target).is_ok() {
            return self.named_node_target(target);
        }
        Err(BeamError::BadArg)
    }

    /// `{Name, Node}`: the registration `Name` on the node `Node`.
    ///
    /// Both elements must be atoms — anything else is the oracle's `badarg`, as is a tuple
    /// that is not a 2-tuple. A `Node` naming this runtime's own node resolves the
    /// registration locally (the `{Name, Node}` monitor the local node was seeded for at
    /// boot); any **other** node stays the typed capability gap — it is the one remote shape
    /// the monitor entry points cannot express: `ProcessTable::monitor_named_with_options`
    /// resolves `Name` against **this** node's registration table (its `node` argument only
    /// spells the `{'DOWN', …}` target), so a foreign `Node` here would watch a *local*
    /// process the caller never asked about — a wrong result, not a gap. A remote **pid**
    /// needs no such treatment and is served (see [`MonitorTarget::Remote`]).
    fn named_node_target(&self, target: Eterm) -> Result<MonitorTarget> {
        if !matches!(terms::tuple_arity(target), Ok(2)) {
            return Err(BeamError::BadArg);
        }
        let name = terms::tuple_element(target, 0).map_err(|_| BeamError::BadArg)?;
        let node = terms::tuple_element(target, 1).map_err(|_| BeamError::BadArg)?;
        if !is_atom(name) || !is_atom(node) {
            return Err(BeamError::BadArg);
        }
        if !self.is_local_node(node) {
            return Err(error::unsupported(
                "erlang:monitor/2 on a {Name, Node} of another node",
            ));
        }
        Ok(MonitorTarget::Named {
            name: atom_val(name),
            node: atom_val(node),
        })
    }

    /// Whether `node` names this runtime's own node.
    ///
    /// The comparison is by atom **name** against
    /// [`Runtime::local_node`](crate::Runtime::local_node) — the `node@host` string the
    /// node identity was interned from (`nonode@nohost` when the runtime is not a
    /// distributed node), never a guessed name.
    ///
    /// `pub(super)` so the sibling spawn family in [`super`] resolves the `Node` argument
    /// of `erlang:spawn_monitor/2,4` through the *same* definition (reuse, not duplicate).
    pub(super) fn is_local_node(&self, node: Eterm) -> bool {
        let Some(name) = self.atom_name(node) else {
            return false;
        };
        matches!(
            self.runtime.atom_name_of(self.runtime.local_node_term()),
            Some(local) if local == name.as_str()
        )
    }

    /// The parsed `erlang:monitor/3` option list — the `beam_rt` side of the C's
    /// `erts_monitor_opts` (`bif.c:701-756`).
    ///
    /// * the bare atom `priority` is accepted and **ignored** (the C sets
    ///   `ERTS_ML_FLG_PRIO_ML`, whose only effect is on the priority signal queue, which this
    ///   runtime does not model);
    /// * `{tag, Tag}` supplies the user tag (its *immediacy* is checked downstream, by
    ///   `beam_process`'s `check_tag`, so a heap-resident tag stays that crate's typed
    ///   `error::unsupported_tag`);
    /// * `{alias, Mode}` selects the reference alias's expiry rule from the atoms
    ///   `explicit_unalias` / `demonitor` / `reply_demonitor`;
    /// * **everything else** — another atom, a tuple of another arity, a non-tuple element, a
    ///   2-tuple whose first element is not `tag`/`alias`, an `{alias, _}` with an unknown
    ///   mode — is the oracle's `badarg`, as is a non-list (an improper tail included).
    ///
    /// A repeated option follows the C's last-wins assignment: `{tag, A}, {tag, B}` keeps `B`,
    /// and `[reply, explicit_unalias]` keeps `explicit_unalias`.
    fn monitor_options(&self, options: Eterm) -> Result<MonitorOptions> {
        let elements = terms::list_elements(options).map_err(|_| BeamError::BadArg)?;
        let mut parsed = MonitorOptions::NONE;
        for option in elements {
            match terms::tuple_arity(option) {
                Ok(2) => {
                    let first = terms::tuple_element(option, 0).map_err(|_| BeamError::BadArg)?;
                    let second = terms::tuple_element(option, 1).map_err(|_| BeamError::BadArg)?;
                    match self.atom_name(first).as_deref() {
                        Some("tag") => parsed.tag = Some(second),
                        Some("alias") => {
                            parsed.alias =
                                Some(self.unalias_mode(second).ok_or(BeamError::BadArg)?);
                        }
                        _ => return Err(BeamError::BadArg),
                    }
                }
                // The C's `case am_priority` precedes its `is_not_tuple(opt)` test, so the
                // bare atom is the one non-tuple element that is accepted.
                _ if self.atom_name(option).as_deref() == Some("priority") => {}
                _ => return Err(BeamError::BadArg),
            }
        }
        Ok(parsed)
    }

    /// The `erlang:monitor/3` `{alias, Mode}` mode atom as a [`UnaliasMode`], or `None` for
    /// any other word (which the caller answers with the oracle's `badarg`).
    ///
    /// The three names are the C's `am_explicit_unalias` / `am_demonitor` /
    /// `am_reply_demonitor` (`bif.c:724-737`).
    fn unalias_mode(&self, mode: Eterm) -> Option<UnaliasMode> {
        match self.atom_name(mode).as_deref() {
            Some("explicit_unalias") => Some(UnaliasMode::ExplicitUnalias),
            Some("demonitor") => Some(UnaliasMode::Demonitor),
            Some("reply_demonitor") => Some(UnaliasMode::ReplyDemonitor),
            _ => None,
        }
    }

    /// Removes the monitor `term` stands for and reports whether one was on record.
    ///
    /// # Errors
    ///
    /// `badarg` for a term that is **not a reference at all** (see the module docs) — the
    /// oracle's reason for `demonitor(not_a_ref)`. A *reference* that backs no recorded
    /// monitor is not an error: it is the "nothing was removed" answer (`Ok(false)`), which
    /// `demonitor/1` reports as `true` and `demonitor/2`'s `[info]` as `false`. A propagation
    /// failure from either table is reported rather than swallowed.
    fn remove_monitor(&mut self, term: Eterm, flush: bool) -> Result<bool> {
        // Only a term that is **not a reference** is the oracle's `badarg` — the C's
        // `is_not_internal_ref` test (`bif.c:443-450`). The crate-local `TermError` that
        // `from_eterm` reports for one is deliberately not surfaced.
        let _reference = MonitorRef::from_eterm(term).map_err(|_| BeamError::BadArg)?;
        let Some(entry) = self.runtime.references.get(&term).copied() else {
            // A reference this runtime minted for something **other** than a monitor — an
            // `erlang:send_after/3` timer reference, which the timer heap owns and never
            // records here — has no monitor to remove, so the oracle's answer is "nothing
            // removed": `demonitor/1` answers `true` and `demonitor/2`'s `[info]` boolean is
            // `false` (verified on OTP 29/erts-17.1).
            return Ok(false);
        };
        let Some(reference) = entry else {
            // A bare `make_ref/0` reference backs no monitor either; the oracle still answers
            // `true` for `demonitor/1` on it.
            return Ok(false);
        };
        let watcher = self.pid();
        // The reference is left on record, so demonitoring twice answers the same way as
        // the oracle's (never `badarg`).
        //
        // A **port** monitor lives on the port table and a process monitor on the process
        // table; `demonitor_port` reports `false` for a reference it does not hold, so the
        // port half is tried first and the process half is its documented fallback
        // (`ProcessTable::demonitor_port`). The port borrow is scoped to that one call.
        let port_removed =
            self.with_port_table(|table, ports| table.demonitor_port(watcher, reference, ports))?;
        if port_removed {
            return Ok(true);
        }
        let table = self.process_table_mut();
        if flush {
            table.demonitor_flush(watcher, reference)
        } else {
            table.demonitor(watcher, reference)
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the monitor/demonitor BIF layer: the reference and both monitor
    //! halves, the type/target dispatch (the oracle's shapes and reasons), the named and
    //! `{Name, Node}` targets, the `monitor/3` option list and `demonitor/2`'s real `info`
    //! boolean. Every answer was verified against OTP 29/erts-17.1.

    use super::*;
    use beam_bifs::NullSink;
    use beam_term::{make_small, signed_value, NIL};

    use crate::native::test_support::{atom, call, runtime};

    /// Whether `term` is a reference, asked through the registry's own `is_reference/1`.
    fn is_reference(runtime: &mut crate::Runtime, term: Eterm) -> bool {
        let answer = call(runtime, "erlang", "is_reference", &[term]).expect("is_reference/1");
        runtime.atom_name_of(answer) == Some("true")
    }

    /// The atom `name` of the floor's answer, resolved through the shared table.
    fn atom_name(runtime: &crate::Runtime, term: Eterm) -> Option<&str> {
        runtime.atom_name_of(term)
    }

    /// A list built on the process heap by the runtime's own term constructors.
    fn make_list(runtime: &mut crate::Runtime, elements: &[Eterm]) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_list(&mut context, elements).expect("a list")
    }

    /// A tuple built on the process heap by the runtime's own term constructors.
    fn make_tuple(runtime: &mut crate::Runtime, elements: &[Eterm]) -> Eterm {
        let mut sink = NullSink;
        let mut context = RuntimeContext::with_output(runtime, &mut sink);
        terms::make_tuple(&mut context, elements).expect("a tuple")
    }

    /// The number of outgoing monitors the pid holds.
    fn monitor_count(runtime: &crate::Runtime, pid: ProcessId) -> usize {
        runtime.processes().get(pid).expect("the process").monitors().len()
    }

    /// A fresh monitor on `target` from the runtime's own process.
    fn monitor_target(runtime: &mut crate::Runtime, target: Eterm) -> Eterm {
        let process = atom(runtime, "process");
        call(runtime, "erlang", "monitor", &[process, target]).expect("erlang:monitor/2")
    }

    #[test]
    fn monitor_of_self_returns_a_reference_and_records_both_halves() {
        let mut runtime = runtime();
        let pid = runtime.self_pid();
        let reference = monitor_target(&mut runtime, pid.to_eterm());
        assert!(
            is_reference(&mut runtime, reference),
            "monitor/2 answers a reference"
        );
        assert_ne!(
            reference,
            call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0"),
            "a monitor reference is freshly minted"
        );

        // Both halves of the monitor are recorded on the running process (it monitors
        // itself), which is exactly what a later `{'DOWN', …}` delivery reads.
        let recorded = runtime.processes().get(pid).expect("the runtime's process");
        assert_eq!(recorded.monitors().len(), 1, "the outgoing monitor");
        assert_eq!(recorded.monitored_by().len(), 1, "the incoming monitor");
        assert_eq!(
            recorded.monitored_by()[0].watcher,
            pid,
            "the watcher is the running pid"
        );
    }

    #[test]
    fn demonitor_removes_the_monitor_and_answers_true() {
        let mut runtime = runtime();
        let pid = runtime.self_pid();
        let reference = monitor_target(&mut runtime, pid.to_eterm());
        let answer = call(&mut runtime, "erlang", "demonitor", &[reference]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
        assert_eq!(monitor_count(&runtime, pid), 0, "the outgoing monitor is gone");
        assert_eq!(
            runtime
                .processes()
                .get(pid)
                .expect("the process")
                .monitored_by()
                .len(),
            0,
            "the incoming monitor is gone"
        );
        // Demonitoring again is still `true`, as the oracle answers.
        let again = call(&mut runtime, "erlang", "demonitor", &[reference]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, again), Some("true"));
    }

    #[test]
    fn demonitor_of_a_bare_reference_is_true_and_of_a_non_reference_is_badarg() {
        let mut runtime = runtime();
        // A plain `make_ref/0` reference backs no monitor, and the oracle answers `true`.
        let plain = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");
        let answer = call(&mut runtime, "erlang", "demonitor", &[plain]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
        // Anything that is not a reference at all is `badarg`.
        for argument in [make_small(42), atom(&mut runtime, "notaref"), NIL] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "demonitor", &[argument]),
                    Err(BeamError::BadArg)
                ),
                "demonitor({argument:?}) is badarg"
            );
        }
    }

    /// Oracle-verified (OTP 29/erts-17.1): a timer reference is **not** a monitor, so
    /// `demonitor(T)` answers `true`, `demonitor(T, [info])` answers `false`, and the timer
    /// itself is untouched (`cancel_timer(T)` still reports the remaining time).
    #[test]
    fn demonitor_of_a_timer_reference_answers_true_and_leaves_the_timer_pending() {
        let mut runtime = runtime();
        let me = runtime.self_pid().to_eterm();
        let message = atom(&mut runtime, "x");
        let timer = call(
            &mut runtime,
            "erlang",
            "send_after",
            &[make_small(10_000), me, message],
        )
        .expect("erlang:send_after/3");
        assert!(is_reference(&mut runtime, timer), "a timer answers a reference");

        // `demonitor/1` answers `true`, and the timer really is still pending: a timer
        // reference lives in the timer heap, never in `Runtime::references`.
        let answer = call(&mut runtime, "erlang", "demonitor", &[timer]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
        assert_eq!(
            runtime.timers.pending_count(),
            1,
            "demonitor does not cancel the timer"
        );

        // `[info]` reports the truthful "nothing was removed"; `[flush]` answers `true`.
        let info = atom(&mut runtime, "info");
        let info_list = make_list(&mut runtime, &[info]);
        let answer =
            call(&mut runtime, "erlang", "demonitor", &[timer, info_list]).expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("false"));
        let flush = atom(&mut runtime, "flush");
        let flush_list = make_list(&mut runtime, &[flush]);
        let answer =
            call(&mut runtime, "erlang", "demonitor", &[timer, flush_list]).expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("true"));

        // The timer the handle names is still cancellable for its full delay.
        let remaining =
            call(&mut runtime, "erlang", "cancel_timer", &[timer]).expect("cancel_timer/1");
        let millis = signed_value(remaining);
        assert!(
            (0..=10_000).contains(&millis),
            "the timer survived the demonitor: {millis}"
        );
    }

    #[test]
    fn demonitor_with_options_matches_the_oracle() {
        let mut runtime = runtime();
        let pid = runtime.self_pid();
        let empty = make_list(&mut runtime, &[]);
        let info = atom(&mut runtime, "info");
        let flush = atom(&mut runtime, "flush");
        let info_list = make_list(&mut runtime, &[info]);
        let both = make_list(&mut runtime, &[info, flush]);
        let flush_list = make_list(&mut runtime, &[flush]);

        // `[]` answers `true`, and the monitor really is removed.
        let reference = monitor_target(&mut runtime, pid.to_eterm());
        let answer =
            call(&mut runtime, "erlang", "demonitor", &[reference, empty]).expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("true"), "[] answers true");
        assert_eq!(monitor_count(&runtime, pid), 0, "[] removes the monitor");

        // `[info]` answers the **real** boolean (oracle-verified): `true` while a monitor
        // was on record and got removed, `false` once there is none left.
        let reference = monitor_target(&mut runtime, pid.to_eterm());
        let answer = call(&mut runtime, "erlang", "demonitor", &[reference, info_list])
            .expect("demonitor/2");
        assert_eq!(
            atom_name(&runtime, answer),
            Some("true"),
            "[info] on a live monitor answers true"
        );
        assert_eq!(monitor_count(&runtime, pid), 0, "[info] removes the monitor");
        let answer = call(&mut runtime, "erlang", "demonitor", &[reference, info_list])
            .expect("demonitor/2");
        assert_eq!(
            atom_name(&runtime, answer),
            Some("false"),
            "[info] on a monitor that is already gone answers false"
        );

        // `[info, flush]` answers the same real boolean.
        let reference = monitor_target(&mut runtime, pid.to_eterm());
        let answer =
            call(&mut runtime, "erlang", "demonitor", &[reference, both]).expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("true"), "[info, flush] answers true");
        let answer =
            call(&mut runtime, "erlang", "demonitor", &[reference, both]).expect("demonitor/2");
        assert_eq!(
            atom_name(&runtime, answer),
            Some("false"),
            "[info, flush] answers false once the monitor is gone"
        );

        // `[flush]` answers `true`; a malformed option list or a non-list is `badarg`.
        let reference = monitor_target(&mut runtime, pid.to_eterm());
        let answer = call(&mut runtime, "erlang", "demonitor", &[reference, flush_list])
            .expect("demonitor/2");
        assert_eq!(atom_name(&runtime, answer), Some("true"), "[flush] answers true");

        let nonsense = atom(&mut runtime, "nope");
        let bad_options = make_list(&mut runtime, &[nonsense]);
        assert!(matches!(
            call(&mut runtime, "erlang", "demonitor", &[reference, bad_options]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            call(&mut runtime, "erlang", "demonitor", &[reference, make_small(1)]),
            Err(BeamError::BadArg)
        ));
        // A non-reference is `badarg` whatever the options are.
        assert!(matches!(
            call(&mut runtime, "erlang", "demonitor", &[make_small(1), empty]),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn monitor_of_a_registered_name_resolves_through_the_register() {
        let mut runtime = runtime();
        let name = atom(&mut runtime, "myreg");
        let pid = runtime.self_pid();
        let registered = call(&mut runtime, "erlang", "register", &[name, pid.to_eterm()])
            .expect("erlang:register/2");
        assert_eq!(atom_name(&runtime, registered), Some("true"));

        let reference = monitor_target(&mut runtime, name);
        assert!(is_reference(&mut runtime, reference));
        // The monitor is on the *registered* pid, so the running process sees it.
        assert_eq!(monitor_count(&runtime, pid), 1);

        // An unregistered name still yields a reference (the oracle does the same, with an
        // immediate `noproc` `'DOWN'`), but nothing is recorded on the watcher.
        let missing = atom(&mut runtime, "no_such_registered_name");
        let reference = monitor_target(&mut runtime, missing);
        assert!(is_reference(&mut runtime, reference));
        assert_eq!(
            monitor_count(&runtime, pid),
            1,
            "only the resolvable name produced a recorded monitor"
        );
        // The born-down monitor is still demonitorable, as the oracle's is.
        let answer = call(&mut runtime, "erlang", "demonitor", &[reference]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
    }

    #[test]
    fn monitor_of_a_name_on_the_local_node_is_recorded_and_another_node_is_a_gap() {
        let mut runtime = runtime();
        let name = atom(&mut runtime, "myreg");
        let pid = runtime.self_pid();
        call(&mut runtime, "erlang", "register", &[name, pid.to_eterm()]).expect("register/2");

        // `{Name, LocalNode}` resolves the registration locally and installs the monitor.
        let node = runtime.local_node_term();
        let target = make_tuple(&mut runtime, &[name, node]);
        let reference = monitor_target(&mut runtime, target);
        assert!(is_reference(&mut runtime, reference));
        assert_eq!(monitor_count(&runtime, pid), 1, "the local registration is watched");
        let answer = call(&mut runtime, "erlang", "demonitor", &[reference]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));

        // A node that is not this one needs distribution (contract A.5).
        let other = atom(&mut runtime, "other@host");
        let remote = make_tuple(&mut runtime, &[name, other]);
        let process = atom(&mut runtime, "process");
        let error = call(&mut runtime, "erlang", "monitor", &[process, remote])
            .expect_err("a remote monitor is not implemented");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "a remote monitor is a capability gap: {error}"
        );

        // A non-atom name, a non-atom node and a tuple that is not a 2-tuple are `badarg`.
        for target in [
            make_tuple(&mut runtime, &[make_small(1), node]),
            make_tuple(&mut runtime, &[name, make_small(1)]),
            make_tuple(&mut runtime, &[name, node, node]),
            make_tuple(&mut runtime, &[name]),
        ] {
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "monitor", &[process, target]),
                    Err(BeamError::BadArg)
                ),
                "monitor(process, {target:?}) is badarg"
            );
        }
    }

    #[test]
    fn monitor_rejects_bad_kinds_and_bad_targets() {
        let mut runtime = runtime();
        let pid = runtime.self_pid().to_eterm();
        let bad = atom(&mut runtime, "bad");
        let port = atom(&mut runtime, "port");
        let process = atom(&mut runtime, "process");
        let reference = call(&mut runtime, "erlang", "make_ref", &[]).expect("make_ref/0");

        // A kind that is not an atom, and a kind that is not `process`/`port`.
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[make_small(1), pid]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[bad, pid]),
            Err(BeamError::BadArg)
        ));
        // `port` needs a port target: a pid is `badarg`, as the oracle answers.
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[port, pid]),
            Err(BeamError::BadArg)
        ));
        // `process` with a target that is neither a pid, nor a name, nor `{Name, Node}`.
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[process, reference]),
            Err(BeamError::BadArg)
        ));
        // A `{Name, Node}` target on another node needs distribution — a Milestone-1
        // capability gap.
        let tuple = make_tuple(&mut runtime, &[bad, bad]);
        let error = call(&mut runtime, "erlang", "monitor", &[process, tuple])
            .expect_err("distribution is not implemented");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "a remote monitor is a capability gap: {error}"
        );
    }

    #[test]
    fn monitor_three_without_options_is_monitor_two_and_records_the_monitor() {
        let mut runtime = runtime();
        let process = atom(&mut runtime, "process");
        let pid = runtime.self_pid();
        let empty = make_list(&mut runtime, &[]);
        let reference = call(&mut runtime, "erlang", "monitor", &[process, pid.to_eterm(), empty])
            .expect("erlang:monitor/3");
        assert!(is_reference(&mut runtime, reference));
        assert_eq!(monitor_count(&runtime, pid), 1, "the monitor is recorded");
        // The answered reference is the very one `demonitor/1` recognises.
        let answer = call(&mut runtime, "erlang", "demonitor", &[reference]).expect("demonitor/1");
        assert_eq!(atom_name(&runtime, answer), Some("true"));
        assert_eq!(monitor_count(&runtime, pid), 0, "the monitor is removed");
    }

    #[test]
    fn monitor_three_parses_tag_and_alias_options_and_rejects_the_rest() {
        let mut runtime = runtime();
        let process = atom(&mut runtime, "process");
        let pid = runtime.self_pid();
        let tag = atom(&mut runtime, "tag");
        let alias = atom(&mut runtime, "alias");
        let x = atom(&mut runtime, "x");

        // `{tag, Tag}` with an immediate tag is honoured: a reference, with the monitor on
        // record.
        let tag_option = make_tuple(&mut runtime, &[tag, x]);
        let options = make_list(&mut runtime, &[tag_option]);
        let reference = call(&mut runtime, "erlang", "monitor", &[process, pid.to_eterm(), options])
            .expect("erlang:monitor/3 with {tag, Tag}");
        assert!(is_reference(&mut runtime, reference));
        assert_eq!(monitor_count(&runtime, pid), 1, "the monitor is recorded");
        assert_eq!(
            call(&mut runtime, "erlang", "demonitor", &[reference]).expect("demonitor/1"),
            atom(&mut runtime, "true")
        );

        // `{alias, Mode}` is now adopted for all three modes: a reference is answered and the
        // monitor is recorded (the alias itself lives in `beam_process`'s alias table, reached
        // by the send path).
        for mode in ["explicit_unalias", "demonitor", "reply_demonitor"] {
            let mode_atom = atom(&mut runtime, mode);
            let alias_option = make_tuple(&mut runtime, &[alias, mode_atom]);
            let options = make_list(&mut runtime, &[alias_option]);
            let reference = call(
                &mut runtime,
                "erlang",
                "monitor",
                &[process, pid.to_eterm(), options],
            )
            .unwrap_or_else(|error| panic!("erlang:monitor/3 with {{alias, {mode}}}: {error}"));
            assert!(
                is_reference(&mut runtime, reference),
                "{{alias, {mode}}} answers a reference"
            );
            assert_eq!(monitor_count(&runtime, pid), 1, "the monitor is recorded");
            assert_eq!(
                call(&mut runtime, "erlang", "demonitor", &[reference]).expect("demonitor/1"),
                atom(&mut runtime, "true")
            );
            assert_eq!(monitor_count(&runtime, pid), 0, "the monitor is removed");
        }

        // `priority` is accepted and ignored; the combination with a tag still works.
        let priority = atom(&mut runtime, "priority");
        let options = make_list(&mut runtime, &[priority, tag_option]);
        let reference = call(&mut runtime, "erlang", "monitor", &[process, pid.to_eterm(), options])
            .expect("erlang:monitor/3 with priority");
        assert!(is_reference(&mut runtime, reference));
        call(&mut runtime, "erlang", "demonitor", &[reference]).expect("demonitor/1");

        // A **non-immediate** tag (a heap term) is the typed `beam_process` capability gap:
        // a monitor record is a plain value the collector never scans.
        let heap_tag = make_tuple(&mut runtime, &[x, x]);
        let tag_option = make_tuple(&mut runtime, &[tag, heap_tag]);
        let options = make_list(&mut runtime, &[tag_option]);
        let error = call(&mut runtime, "erlang", "monitor", &[process, pid.to_eterm(), options])
            .expect_err("a heap-resident tag is not expressible");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "a non-immediate tag is a capability gap: {error}"
        );

        // An `{alias, Mode}` whose `Mode` is not one of the three atoms — an unknown atom, or a
        // plain (monitor) reference — is the oracle's `badarg`.
        for mode in [atom(&mut runtime, "nope"), call(&mut runtime, "erlang", "make_ref", &[])
            .expect("make_ref/0")]
        {
            let bogus_alias = make_tuple(&mut runtime, &[alias, mode]);
            let bogus = make_list(&mut runtime, &[bogus_alias]);
            assert!(
                matches!(
                    call(&mut runtime, "erlang", "monitor", &[process, pid.to_eterm(), bogus]),
                    Err(BeamError::BadArg)
                ),
                "{{alias, {mode:?}}} is badarg"
            );
        }

        // An unknown atom and a 3-tuple are the oracle's `badarg`; so is a non-list.
        let foo = atom(&mut runtime, "foo");
        let bogus = make_list(&mut runtime, &[foo]);
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[process, pid.to_eterm(), bogus]),
            Err(BeamError::BadArg)
        ));
        let three = make_tuple(&mut runtime, &[tag, x, x]);
        let bogus = make_list(&mut runtime, &[three]);
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[process, pid.to_eterm(), bogus]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[process, pid.to_eterm(), make_small(1)]),
            Err(BeamError::BadArg)
        ));
    }
}
