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
//! makes `demonitor/1`'s argument check exact: a term with no entry is not a reference
//! this runtime minted, and the oracle's reason for that is `badarg`.
//!
//! # The targets the oracle accepts, and the two capability gaps
//!
//! `monitor(process, Pid)`, `monitor(process, Name)` and `monitor(process, {Name, Node})`
//! are all resolved (the last only when `Node` is **this** node; a remote node needs
//! distribution, contract A.5). `monitor(port, Port)` and `monitor(process, Target, [{tag,
//! _}])` are typed capability gaps: a port monitor's table (`beam_process::PortTable`) is
//! not reachable from this crate, and `beam_process::Monitor`/`MonitoredBy` carry no tag
//! field, so neither can be recorded faithfully — a fabricated monitor would deliver a
//! wrong `'DOWN'`, which the workspace error strategy forbids.

use beam_common::{BeamError, Result};
use beam_engine::terms;
use beam_process::{MonitorKind, PortId, ProcessId};
use beam_term::{atom_val, is_atom, Atom, Eterm};

use crate::context::RuntimeContext;
use crate::error;

/// The target `erlang:monitor(process, Target)` watches, once resolved.
enum MonitorTarget {
    /// A pid: `ProcessTable::monitor` resolves it itself, so an unknown pid is born down.
    Pid(ProcessId),
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
    /// `Target` may be a pid, a registered **name**, or a `{Name, Node}` pair on this node.
    /// A pid or name the table does not hold still yields a reference, exactly as the
    /// oracle's does: the monitor is born down and the caller is sent the matching
    /// `{'DOWN', …}` with reason `noproc` (delivered by `beam_process`).
    ///
    /// A `port` kind, a remote `{Name, Node}` target and any other kind are rejected: the
    /// first two are Milestone-1 capability gaps (no port monitors, no distribution — the
    /// oracle answers `badarg` and a reference respectively), the last is the oracle's
    /// `badarg`.
    pub(in crate::native) fn monitor_2(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let kind = *args.first().ok_or(BeamError::BadArg)?;
        let target = *args.get(1).ok_or(BeamError::BadArg)?;
        let kind = self.atom_name(kind).ok_or(BeamError::BadArg)?;
        match kind.as_str() {
            "process" => self.monitor_process(target),
            // A port monitor needs `beam_io`'s port machinery (the oracle's own `port`
            // monitors live in the emulator's port table), and the target of the oracle's
            // `monitor(port, X)` must be a port — anything else is `badarg` there too.
            "port" => match PortId::from_eterm(target) {
                Ok(_port) => Err(error::unsupported("erlang:monitor/2 on a port")),
                Err(_) => Err(BeamError::BadArg),
            },
            _ => Err(BeamError::BadArg),
        }
    }

    /// `erlang:monitor(process, Target, Options)`: `monitor/2` plus the option list.
    ///
    /// The only option the oracle accepts here is `{tag, Tag}` (OTP 24+), which gives the
    /// monitor's `{'DOWN', …}` a caller-chosen tag. `beam_process`'s `Monitor`/`MonitoredBy`
    /// pair carries no tag field, so that single form is the typed capability gap; every
    /// other element — an unknown atom, a 3-tuple — is the oracle's `badarg`, as is a
    /// non-list. The empty list is `monitor/2` exactly.
    pub(in crate::native) fn monitor_3(&mut self, args: &[Eterm]) -> Result<Eterm> {
        let options = terms::list_elements(*args.get(2).ok_or(BeamError::BadArg)?)
            .map_err(|_| BeamError::BadArg)?;
        let mut tag = false;
        for option in options {
            if self.is_tag_option(option) {
                tag = true;
            } else {
                return Err(BeamError::BadArg);
            }
        }
        if tag {
            return Err(error::unsupported("erlang:monitor/3 option {tag, _}"));
        }
        self.monitor_2(args)
    }

    // -- erlang:demonitor/1,2 -------------------------------------------------

    /// `erlang:demonitor(Ref)`: removes the monitor and answers `true`.
    ///
    /// A reference this runtime minted always answers `true` — including a bare
    /// `make_ref/0` reference and an already-demonitored one, both of which the oracle also
    /// answers `true` for. Anything that is not such a reference is `badarg` (the oracle's
    /// reason for `demonitor(not_a_ref)`).
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
    fn monitor_process(&mut self, target: Eterm) -> Result<Eterm> {
        // The target is resolved **first**, so a malformed target keeps the oracle's
        // `badarg` (and a remote `{Name, Node}` its capability gap) even when the caller
        // itself has no process.
        let target = self.monitor_target(target)?;
        let watcher = self.pid();
        if !self.process_table_mut().contains(watcher) {
            return Err(error::no_process(watcher));
        }
        let reference = match target {
            MonitorTarget::Pid(pid) => {
                self.process_table_mut().monitor(watcher, pid, MonitorKind::Process)?
            }
            MonitorTarget::Named { name, node } => self
                .process_table_mut()
                .monitor_named(watcher, name, node, MonitorKind::Process)?,
        };
        let term = self.reference_term(reference)?;
        // Recorded whatever happened, so `demonitor/1,2` recognises the reference: a
        // monitor on a pid or name the table does not hold is born down, and the oracle
        // still lets it be demonitored.
        self.runtime.references.insert(term, Some(reference));
        Ok(term)
    }

    /// The target `erlang:monitor(process, Target)` watches.
    ///
    /// A pid term always resolves (the oracle returns a reference even for a pid that does
    /// not exist, and the monitor then fires `noproc`); an atom is a name registered on
    /// **this** node; a `{Name, Node}` pair needs `Node` to be this node (see
    /// [`Self::named_node_target`]); anything else is `badarg`, the oracle's reason.
    fn monitor_target(&self, target: Eterm) -> Result<MonitorTarget> {
        if let Ok(pid) = ProcessId::from_eterm(target) {
            return Ok(MonitorTarget::Pid(pid));
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
    /// boot); any other node needs distribution (contract A.5), which Milestone-1 does not
    /// implement, so it is a typed capability gap rather than a fabricated local monitor.
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

    /// Whether `option` is the `{tag, Tag}` form of `erlang:monitor/3`.
    fn is_tag_option(&self, option: Eterm) -> bool {
        if !matches!(terms::tuple_arity(option), Ok(2)) {
            return false;
        }
        match terms::tuple_element(option, 0) {
            Ok(first) => self.atom_name(first).as_deref() == Some("tag"),
            Err(_) => false,
        }
    }

    /// Removes the monitor `term` stands for and reports whether one was on record.
    ///
    /// # Errors
    ///
    /// `badarg` for a term that is not a reference this runtime minted (see the module
    /// docs) — the oracle's reason for `demonitor(not_a_ref)`. A propagation failure from
    /// the process table is reported rather than swallowed.
    fn remove_monitor(&mut self, term: Eterm, flush: bool) -> Result<bool> {
        let Some(entry) = self.runtime.references.get(&term).copied() else {
            return Err(BeamError::BadArg);
        };
        let Some(reference) = entry else {
            // A bare `make_ref/0` reference backs no monitor; the oracle still answers
            // `true` for `demonitor/1` on it.
            return Ok(false);
        };
        let watcher = self.pid();
        // The reference is left on record, so demonitoring twice answers the same way as
        // the oracle's (never `badarg`).
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
    use beam_term::{make_small, NIL};

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
        // Anything that is not a reference this runtime minted is `badarg`.
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
    fn monitor_three_tag_option_is_a_capability_gap_and_other_options_are_badarg() {
        let mut runtime = runtime();
        let process = atom(&mut runtime, "process");
        let pid = runtime.self_pid().to_eterm();
        let tag = atom(&mut runtime, "tag");
        let x = atom(&mut runtime, "x");

        // `{tag, Tag}` is the oracle's only accepted option here, and the one this
        // runtime cannot express: a typed capability gap, never a `'DOWN'` the caller did
        // not ask for.
        let tag_option = make_tuple(&mut runtime, &[tag, x]);
        let options = make_list(&mut runtime, &[tag_option]);
        let error = call(&mut runtime, "erlang", "monitor", &[process, pid, options])
            .expect_err("a per-monitor tag is not expressible");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "the `{{tag, _}}` option is a capability gap: {error}"
        );

        // An unknown atom and a 3-tuple are the oracle's `badarg`; so is a non-list.
        let foo = atom(&mut runtime, "foo");
        let bogus = make_list(&mut runtime, &[foo]);
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[process, pid, bogus]),
            Err(BeamError::BadArg)
        ));
        let three = make_tuple(&mut runtime, &[tag, x, x]);
        let bogus = make_list(&mut runtime, &[three]);
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[process, pid, bogus]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            call(&mut runtime, "erlang", "monitor", &[process, pid, make_small(1)]),
            Err(BeamError::BadArg)
        ));
    }
}
