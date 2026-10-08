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

//! The [`ProcessHook`] implementation over the runtime's own state.
//!
//! `beam_bifs`'s process/name/global-state BIFs (`erlang:whereis/1`, `register/2`,
//! `unregister/1`, `registered/0`, `is_process_alive/1`, `processes/0`, `ports/0`,
//! `process_info/1,2`, `process_flag/2`, `group_leader/0`) answer questions about the
//! *running* node, which no BIF can reach: the name registry and the process table live in
//! [`beam_process`], the port table in [`crate::port`], and the per-process state (flags,
//! group leader, heap counters) in the processes themselves. This module adapts those onto
//! `beam_bifs`'s [`ProcessHook`] seam (call down, never duplicate).
//!
//! # One implementation, shared with the runtime-native floor
//!
//! The `process_info` item vocabulary and its value builders live in
//! [`crate::native::memory`]. This bridge does **not** re-implement them: it builds a
//! [`RuntimeContext`] over the same runtime state (see [`ProcessBridge::with_ctx`]) and
//! calls the very same helpers the native floor arms used to call, so both paths can never
//! drift. The bridge is the *only* path now — `try_native` no longer claims these eleven
//! names — so the runtime-native floor stops duplicating registry-owned BIFs.
//!
//! # Documented divergences (honest, by construction)
//!
//! * `whereis/1` / `register/2`: the oracle's registry can hold a **port** id as well as a
//!   pid term. [`beam_process::ProcessTable`] registers pids only, so a faithful pid answer
//!   is given and a port id is reported as the typed capability gap (never a fabricated
//!   `undefined`/`false`).
//! * `error_handler`: this runtime holds no `error_handler` state, so the flag accepts a new
//!   atom value and answers the unchanging runtime default atom `error_handler` as the
//!   previous value (the oracle's default for a fresh process).
//! * `group_leader/0` answers this runtime's root pid — the same real, live pid the
//!   runtime-native `group_leader/0` reported (M1 has no `init` process to lead everything).

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_bifs::{ProcessHook, ProcessInfoItem};
use beam_common::{BeamError, Result};
use beam_process::{PortId, Priority, ProcessId, ProcessTable};
use beam_term::{is_atom, is_small, make_atom, signed_value, Atom, AtomTable, Eterm};

use crate::error;
use crate::port::PortRegistry;
use crate::runner::RunnerState;
use crate::runtime::Runtime;

use super::RuntimeContext;

/// The `beam_bifs::ProcessHook` over the real runtime tables.
pub(crate) struct ProcessBridge<'a> {
    /// The runtime: the fallback process table and the arena the `process_info` builders
    /// allocate their (pointer-stable) terms on.
    runtime: &'a mut Runtime,
    /// A caller-supplied process table (a scheduler pass whenever the table has been moved
    /// out of the runtime), else the runtime's own.
    table: Option<&'a mut ProcessTable>,
    /// The shared port registry (`erlang:ports/0`), a cheap `Rc` clone — the runtime stays
    /// borrowable (the ETS pattern).
    ports: Rc<RefCell<PortRegistry>>,
    /// The engines of the scheduler pass this call is a slice of, when it is one — the
    /// `process_info` execution-position items read the parked machines through it.
    engines: Option<&'a RunnerState>,
    /// The pid this call runs as (the `group_leader/0` answer and the `process_flag/2`
    /// subject).
    self_pid: ProcessId,
}

impl<'a> ProcessBridge<'a> {
    /// A bridge over `runtime`, resolving processes against `table` when one is supplied.
    pub(crate) fn new(
        runtime: &'a mut Runtime,
        table: Option<&'a mut ProcessTable>,
        ports: Rc<RefCell<PortRegistry>>,
        engines: Option<&'a RunnerState>,
        self_pid: ProcessId,
    ) -> ProcessBridge<'a> {
        ProcessBridge {
            runtime,
            table,
            ports,
            engines,
            self_pid,
        }
    }

    /// The process table a pid resolves against: the caller-supplied one when there is one,
    /// else the runtime's own.
    fn table_mut(&mut self) -> &mut ProcessTable {
        match self.table {
            Some(ref mut table) => &mut **table,
            None => &mut self.runtime.processes,
        }
    }

    /// Runs `f` against a [`RuntimeContext`] over this bridge's state.
    ///
    /// The context resolves its process table exactly as [`Self::table_mut`] does, so the
    /// `process_info` helpers (and their execution-position reads) see the same processes
    /// the bridge does — and their reply terms land on the same pointer-stable arena the
    /// runtime-native floor used.
    fn with_ctx<R>(&mut self, f: impl FnOnce(&mut RuntimeContext<'_>) -> R) -> R {
        match (self.table.as_deref_mut(), self.engines) {
            (Some(table), Some(engines)) => {
                let mut ctx = RuntimeContext::for_process_with_engines(
                    self.runtime,
                    table,
                    self.self_pid,
                    engines,
                );
                f(&mut ctx)
            }
            (Some(table), None) => {
                let mut ctx = RuntimeContext::for_process(self.runtime, table, self.self_pid);
                f(&mut ctx)
            }
            // No caller-supplied table: the runtime's own (the `-eval` context). Its
            // `self_pid` is the process the context runs as, so the two agree.
            (None, _) => {
                let mut ctx = RuntimeContext::new(self.runtime);
                f(&mut ctx)
            }
        }
    }
}

/// The atom `true`/`false` of the shared table, or the crate's usual `TermNotFound`.
fn bool_term(atoms: &AtomTable, value: bool) -> Result<Eterm> {
    let name = if value { "true" } else { "false" };
    Ok(make_atom(atoms.lookup(name).ok_or(BeamError::TermNotFound)?))
}

/// The boolean the term `value` names (`true`/`false`), or `None` for anything else — the
/// oracle's `badarg` for a non-boolean flag value.
fn bool_value(atoms: &AtomTable, value: Eterm) -> Option<bool> {
    if !is_atom(value) {
        return None;
    }
    let atom = beam_term::atom_val(value);
    if atoms.lookup("true") == Some(atom) {
        Some(true)
    } else if atoms.lookup("false") == Some(atom) {
        Some(false)
    } else {
        None
    }
}

/// The atom name of `value`, or `None` when it is not an atom.
fn atom_name(atoms: &AtomTable, value: Eterm) -> Option<String> {
    if !is_atom(value) {
        return None;
    }
    atoms.name(beam_term::atom_val(value)).map(str::to_string)
}

impl ProcessHook for ProcessBridge<'_> {
    fn whereis(&mut self, name: Atom) -> Result<Option<Eterm>> {
        Ok(self.table_mut().whereis(name).map(ProcessId::to_eterm))
    }

    fn register(&mut self, name: Atom, id: Eterm) -> Result<bool> {
        // The BIF layer already restricted `id` to an internal pid or an internal port.
        // The oracle's registry holds either; this runtime's holds pids only, so the port
        // case is the honest typed capability gap, never a fabricated `true`/`false`.
        let Ok(pid) = ProcessId::from_eterm(id) else {
            return Err(error::unsupported(
                "erlang:register/2: registering a port under a name (this runtime's name \
                 registry holds pids only)",
            ));
        };
        Ok(self.table_mut().register(name, pid).is_ok())
    }

    fn unregister(&mut self, name: Atom) -> Result<bool> {
        Ok(self.table_mut().unregister(name).is_ok())
    }

    fn registered(&mut self) -> Result<Vec<Atom>> {
        Ok(self
            .table_mut()
            .registered()
            .map(|(name, _)| name)
            .collect())
    }

    fn is_process_alive(&mut self, pid: ProcessId) -> Result<bool> {
        Ok(self.table_mut().contains(pid))
    }

    fn processes(&mut self) -> Result<Vec<ProcessId>> {
        Ok(self.table_mut().pids().collect())
    }

    fn ports(&mut self) -> Result<Vec<PortId>> {
        match self.ports.try_borrow() {
            Ok(registry) => Ok(registry.ids()),
            Err(_) => Err(error::unsupported(
                "erlang:ports/0: the port registry is already borrowed",
            )),
        }
    }

    fn process_info_keys(&mut self, pid: ProcessId) -> Result<Option<Vec<Atom>>> {
        self.with_ctx(|ctx| ctx.process_info_keys_pid(pid))
    }

    fn process_info_item(
        &mut self,
        _heap: &mut Heap,
        atoms: &AtomTable,
        pid: ProcessId,
        item: Atom,
    ) -> Result<ProcessInfoItem> {
        // The item is validated **first** (the C's `pi_arg2ix`): an item this runtime does
        // not report is `badarg` even for a pid that names no live process.
        let Some(name) = atoms.name(item).map(str::to_string) else {
            return Ok(ProcessInfoItem::Unknown);
        };
        if !self.with_ctx(|ctx| ctx.is_known_process_info_item(&name)) {
            return Ok(ProcessInfoItem::Unknown);
        }
        // Only now inspect the process: a valid item with a pid that names no live process
        // is the oracle's `undefined`.
        if !self.table_mut().contains(pid) {
            return Ok(ProcessInfoItem::Dead);
        }
        match self.with_ctx(|ctx| ctx.process_info_item_value(pid, &name))? {
            Some(value) => Ok(ProcessInfoItem::Value(value)),
            // A valid oracle item this runtime cannot answer: the typed capability gap,
            // never a fabricated value or a false `badarg`.
            None => Err(self.with_ctx(|ctx| ctx.unsupported_process_info_item(&name))),
        }
    }

    fn process_flag(
        &mut self,
        _heap: &mut Heap,
        atoms: &AtomTable,
        flag: Atom,
        value: Eterm,
    ) -> Result<Option<Eterm>> {
        // The BIF layer has already established that `flag` is an atom; a flag this runtime
        // does not implement is the oracle's `badarg`.
        let Some(name) = atoms.name(flag).map(str::to_string) else {
            return Ok(None);
        };
        let pid = self.self_pid;
        let Some(process) = self.table_mut().get_mut(pid) else {
            return Ok(None);
        };
        match name.as_str() {
            "trap_exit" => {
                let Some(new) = bool_value(atoms, value) else {
                    return Ok(None);
                };
                let previous = process.is_trapping_exit();
                process.set_trap_exit(new);
                Ok(Some(bool_term(atoms, previous)?))
            }
            "sensitive" => {
                let Some(new) = bool_value(atoms, value) else {
                    return Ok(None);
                };
                let previous = process.is_sensitive();
                process.set_sensitive(new);
                Ok(Some(bool_term(atoms, previous)?))
            }
            "priority" => {
                let Some(priority) = atom_name(atoms, value)
                    .and_then(|name| Priority::from_name(&name))
                else {
                    return Ok(None);
                };
                // The previous priority is read off the PCB we already hold, then the change
                // is applied through the table's own setter (the table-level entry point),
                // rather than the PCB directly. The borrow ends at the read, so the table can
                // be re-borrowed here.
                let previous = process.priority();
                self.table_mut().set_priority(pid, priority)?;
                let atom = atoms.lookup(previous.name()).ok_or(BeamError::TermNotFound)?;
                Ok(Some(make_atom(atom)))
            }
            "save_calls" => {
                let previous = u32::from(process.save_calls());
                let next = if is_small(value) {
                    let value = signed_value(value);
                    if !(0..=10_000).contains(&value) {
                        return Ok(None);
                    }
                    value as u16
                } else {
                    match bool_value(atoms, value) {
                        Some(true) => 1,
                        Some(false) => 0,
                        // The oracle answers the previous value for a value it cannot use
                        // (probed: `process_flag(save_calls, <non-integer>)` ⇒ `0`), so the
                        // flag is left untouched and the real current value returned.
                        None => process.save_calls(),
                    }
                };
                process.set_save_calls(next);
                Ok(Some(beam_term::make_small(i64::from(previous))))
            }
            "error_handler" => {
                // This runtime holds no error-handler state: an atom value is accepted and
                // the unchanging runtime default is the previous value.
                if !is_atom(value) {
                    return Ok(None);
                }
                let atom = atoms.lookup("error_handler").ok_or(BeamError::TermNotFound)?;
                Ok(Some(make_atom(atom)))
            }
            // Any other flag (including the ones this milestone has no state for) is the
            // oracle's `badarg` through the hook's `Ok(None)` contract.
            _ => Ok(None),
        }
    }

    fn group_leader(&mut self) -> Result<ProcessId> {
        // The one store every reader shares: `group_leader/0` reports whatever
        // `erlang:group_leader/2` last set for the running process, and the runtime's root
        // pid when nothing has been set (see [`Runtime::group_leader_of`]).
        Ok(self.runtime.group_leader_of(self.self_pid))
    }
}

#[cfg(test)]
mod tests {
    //! Direct unit tests for the [`ProcessBridge`] over a real [`Runtime`].
    //!
    //! The bridge is the concrete [`ProcessHook`] the BIF layer dispatches to, so these
    //! tests drive the trait **methods** (not the BIFs) against the runtime's own process
    //! table, port registry and shared atom table. The end-to-end path through the
    //! registered BIFs lives in `tests/proc_seams.rs`; here the aim is the bridge's own
    //! contract — which failures are `false` (the oracle's `badarg`), which are the typed
    //! capability gap, and how `process_flag/2` reports an *unchanged* flag.
    //!
    //! Oracle ground truth (OTP 29.1.1 / erts-17.1), pinned by the probes quoted at each
    //! test: `whereis(init)` is a pid, `whereis(<absent>)` is `undefined`,
    //! `process_flag(trap_exit, true)` returns the previous `false`,
    //! `process_flag(priority, high)` the previous `normal`, and `process_flag/2` of an
    //! unknown flag or a non-boolean `trap_exit` value raises `badarg`.

    use super::*;
    use beam_term::make_small;

    /// A runtime with the root `init` process registered (the default runtime).
    fn runtime() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }

    /// A small heap for the `process_flag`/`process_info` builders (they ignore it, but the
    /// trait takes one).
    fn heap() -> Heap {
        Heap::new(64).expect("a test heap")
    }

    /// The atom `name`, interned in the runtime's shared table.
    fn atom(rt: &mut Runtime, name: &str) -> Atom {
        rt.code_server_atoms_mut().intern(name)
    }

    /// The previous-value atom a `process_flag` reply names, as a string.
    fn previous(atoms: &AtomTable, result: Result<Option<Eterm>>) -> String {
        let value = result
            .expect("process_flag/2 does not fail at the bridge level")
            .expect("a known flag with a legal value has a previous value");
        atom_name(atoms, value).expect("the previous value is an atom")
    }

    #[test]
    fn whereis_resolves_the_root_and_reports_an_absent_name_as_none() {
        let mut rt = runtime();
        let init = atom(&mut rt, "init");
        let absent = atom(&mut rt, "zz_bridge_absent_name");
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);

        // The runtime registers its root process as `init` (the oracle's `<0.0.0>`).
        assert_eq!(bridge.whereis(init).unwrap(), Some(self_pid.to_eterm()));
        // An unregistered name is `None` (the BIF layer renders the atom `undefined`).
        assert_eq!(bridge.whereis(absent).unwrap(), None);
    }

    #[test]
    fn register_and_unregister_round_trip_with_the_false_failures() {
        let mut rt = runtime();
        let name = atom(&mut rt, "zz_bridge_registered_name");
        let taken = atom(&mut rt, "init");
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);

        assert!(bridge.register(name, self_pid.to_eterm()).unwrap());
        assert_eq!(bridge.whereis(name).unwrap(), Some(self_pid.to_eterm()));
        // A name that is already registered is the oracle's `badarg` — `false` here.
        assert!(!bridge.register(taken, self_pid.to_eterm()).unwrap());
        // `unregister` of a registered name succeeds; of an unregistered one is `false`.
        assert!(bridge.unregister(name).unwrap());
        assert_eq!(bridge.whereis(name).unwrap(), None);
        assert!(!bridge.unregister(name).unwrap());
    }

    #[test]
    fn register_of_a_port_id_is_the_typed_capability_gap() {
        let mut rt = runtime();
        let name = atom(&mut rt, "zz_bridge_port_name");
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);

        // The oracle's registry holds a port id as well as a pid; this runtime's holds pids
        // only, so registering a port is the honest typed gap — never a fabricated
        // `true`/`false` (see the module docs).
        let error = bridge
            .register(name, PortId::new(7).to_eterm())
            .expect_err("a port id is not registrable");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "expected a typed capability gap, got {error:?}"
        );
    }

    #[test]
    fn registered_lists_the_root_init_name() {
        let mut rt = runtime();
        let init = atom(&mut rt, "init");
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);

        let names = bridge.registered().unwrap();
        assert!(names.contains(&init), "`init` is registered: {names:?}");
    }

    #[test]
    fn is_process_alive_reports_table_membership() {
        let mut rt = runtime();
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);

        assert!(bridge.is_process_alive(self_pid).unwrap());
        // A pid the table never allocated names no live process.
        assert!(!bridge.is_process_alive(ProcessId::new(999_999, 0)).unwrap());
    }

    #[test]
    fn processes_lists_the_live_pids() {
        let mut rt = runtime();
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);

        let pids = bridge.processes().unwrap();
        assert!(pids.contains(&self_pid), "the root is live: {pids:?}");
    }

    #[test]
    fn ports_come_from_the_shared_registry_and_a_held_borrow_is_the_typed_gap() {
        let mut rt = runtime();
        let ports = Rc::clone(&rt.ports);
        let handle = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);

        // A fresh runtime has opened no ports.
        assert_eq!(bridge.ports().unwrap(), Vec::<PortId>::new());
        // With the registry already mutably borrowed the bridge reports the typed
        // capability gap (`try_borrow`, never `borrow`), never a panic or a fabricated list.
        let _guard = handle.borrow_mut();
        let error = bridge.ports().expect_err("the registry is already borrowed");
        assert!(
            crate::error::unsupported_reason(&error).is_some(),
            "expected a typed capability gap, got {error:?}"
        );
    }

    #[test]
    fn group_leader_is_the_running_process() {
        let mut rt = runtime();
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);

        assert_eq!(bridge.group_leader().unwrap(), self_pid);
    }

    #[test]
    fn process_flag_reports_the_previous_value_of_every_flavour() {
        let mut rt = runtime();
        let trap = atom(&mut rt, "trap_exit");
        let sensitive = atom(&mut rt, "sensitive");
        let priority = atom(&mut rt, "priority");
        let save_calls = atom(&mut rt, "save_calls");
        let unknown = atom(&mut rt, "zz_bridge_frobnicate");
        let high = atom(&mut rt, "high");
        let frobnicate = atom(&mut rt, "zz_bridge_not_a_priority");
        let not_a_bool = atom(&mut rt, "zz_bridge_not_a_bool");
        let v_true = atom(&mut rt, "true");
        let v_false = atom(&mut rt, "false");
        let atoms = rt.code_server.atoms_handle();
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);
        let mut heap = heap();

        // `trap_exit`: a fresh process is `false`, so the first set returns `false` and the
        // reset returns the `true` it replaced.
        assert_eq!(
            previous(
                &atoms,
                bridge.process_flag(&mut heap, &atoms, trap, make_atom(v_true))
            ),
            "false"
        );
        assert_eq!(
            previous(
                &atoms,
                bridge.process_flag(&mut heap, &atoms, trap, make_atom(v_false))
            ),
            "true"
        );
        // `sensitive`: the default is `false`.
        assert_eq!(
            previous(
                &atoms,
                bridge.process_flag(&mut heap, &atoms, sensitive, make_atom(v_true))
            ),
            "false"
        );
        // `priority`: the default is the atom `normal`.
        assert_eq!(
            previous(
                &atoms,
                bridge.process_flag(&mut heap, &atoms, priority, make_atom(high))
            ),
            "normal"
        );
        // `save_calls`: the default is `0`, and the previous value comes back as a small int.
        let first = bridge
            .process_flag(&mut heap, &atoms, save_calls, make_small(100))
            .unwrap()
            .expect("a legal save_calls value");
        assert!(is_small(first));
        assert_eq!(signed_value(first), 0);
        let second = bridge
            .process_flag(&mut heap, &atoms, save_calls, make_small(7))
            .unwrap()
            .expect("a legal save_calls value");
        assert_eq!(signed_value(second), 100);

        // An unknown flag is the oracle's `badarg` (the hook's `Ok(None)` contract) ...
        assert!(bridge
            .process_flag(&mut heap, &atoms, unknown, make_small(1))
            .unwrap()
            .is_none());
        // ... as is `trap_exit` with a non-boolean value ...
        assert!(bridge
            .process_flag(&mut heap, &atoms, trap, make_atom(not_a_bool))
            .unwrap()
            .is_none());
        // ... an out-of-range `save_calls` ...
        assert!(bridge
            .process_flag(&mut heap, &atoms, save_calls, make_small(-1))
            .unwrap()
            .is_none());
        assert!(bridge
            .process_flag(&mut heap, &atoms, save_calls, make_small(10_001))
            .unwrap()
            .is_none());
        // ... and a non-priority atom for `priority`.
        assert!(bridge
            .process_flag(&mut heap, &atoms, priority, make_atom(frobnicate))
            .unwrap()
            .is_none());
    }

    /// **Documented divergence** (reported, not hidden): the oracle raises `badarg` for a
    /// non-integer `save_calls` value (`goto error`, `../../erts/emulator/beam/bif.c:1911`,
    /// for any non-small value; probed `process_flag(save_calls, true)` ⇒ `badarg` on OTP
    /// 29.1.1). This bridge instead treats `true`/`false` as `1`/`0` and leaves the flag
    /// untouched for any other non-boolean, returning the **previous value**. The test pins
    /// the current behaviour so the branch is covered and the divergence is visible; fixing
    /// it (returning `Ok(None)` for a non-small value) would make this test fail.
    #[test]
    fn save_calls_with_a_non_integer_returns_the_previous_value_a_documented_divergence() {
        let mut rt = runtime();
        let save_calls = atom(&mut rt, "save_calls");
        let v_true = atom(&mut rt, "true");
        let not_a_number = atom(&mut rt, "zz_bridge_not_a_number");
        let atoms = rt.code_server.atoms_handle();
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);
        let mut heap = heap();

        // A fresh process reports `0` for both shapes (the oracle would raise `badarg`).
        let boolean = bridge
            .process_flag(&mut heap, &atoms, save_calls, make_atom(v_true))
            .unwrap()
            .expect("the bridge answers rather than raising");
        assert_eq!(signed_value(boolean), 0);
        let other = bridge
            .process_flag(&mut heap, &atoms, save_calls, make_atom(not_a_number))
            .unwrap()
            .expect("the bridge leaves the flag untouched");
        // `true` set the counter to `1`, so the following call reports that `1` back.
        assert_eq!(signed_value(other), 1);
    }

    #[test]
    fn process_info_keys_and_items_read_the_runtime_state() {
        let mut rt = runtime();
        // The key list resolves each item name through the shared table; in production the
        // caller's module has interned them, so the test interns the very same vocabulary
        // (the single source of truth, `native::memory::PI_1_KEYS`) up front.
        for name in crate::native::memory::PI_1_KEYS {
            atom(&mut rt, name);
        }
        let item = atom(&mut rt, "message_queue_len");
        let unknown = atom(&mut rt, "zz_bridge_not_an_item");
        let atoms = rt.code_server.atoms_handle();
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);
        let mut heap = heap();

        // The key list of a live process names a real item, and is `None` for a dead pid
        // (the BIF layer renders the oracle's `undefined`).
        let keys = bridge
            .process_info_keys(self_pid)
            .unwrap()
            .expect("the root is live");
        assert!(keys.contains(&item), "the item list names it: {keys:?}");
        assert!(bridge
            .process_info_keys(ProcessId::new(999_999, 0))
            .unwrap()
            .is_none());

        // A known item's value is built into the reply, an unknown item is `Unknown` (the
        // oracle's `badarg`, decided before the process is inspected) and a known item on a
        // pid that names no live process is `Dead` (the oracle's `undefined`).
        let value = bridge
            .process_info_item(&mut heap, &atoms, self_pid, item)
            .unwrap();
        match value {
            ProcessInfoItem::Value(term) => {
                assert!(is_small(term), "an empty mailbox: {term:?}");
                assert_eq!(signed_value(term), 0);
            }
            other => panic!("expected the item's value, got {other:?}"),
        }
        assert_eq!(
            bridge
                .process_info_item(&mut heap, &atoms, self_pid, unknown)
                .unwrap(),
            ProcessInfoItem::Unknown
        );
        assert_eq!(
            bridge
                .process_info_item(&mut heap, &atoms, ProcessId::new(999_999, 0), item)
                .unwrap(),
            ProcessInfoItem::Dead
        );
    }
}
