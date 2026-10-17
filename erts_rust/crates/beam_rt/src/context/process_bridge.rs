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
//! * `whereis/1` / `register/2` / `unregister/1` / `registered/0`: the oracle's registry holds
//!   a **pid or a port** id ([`beam_process::ProcessTable`] registers pids, the port registry
//!   [`PortRegistry`] registers ports), so this bridge consults the process table first and
//!   the port registry second — one name can only ever be held by one entry. A name a
//!   *process* holds shadows nothing: `register(init, Port)` is the oracle's `badarg`.
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

use crate::config::MaxHeapSetting;
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
        // The name registry holds a **pid or a port** (the C's register table, `register.c:175-300`),
        // so the process table is consulted first and the port registry second — one name can
        // only ever be held by one of them (see [`Self::register`]).
        if let Some(pid) = self.table_mut().whereis(name) {
            return Ok(Some(pid.to_eterm()));
        }
        let ports = Rc::clone(&self.ports);
        let registered = match ports.try_borrow() {
            Ok(registry) => registry.port_of_name(name),
            Err(_) => return Err(error::unsupported("the port table is already in use")),
        };
        Ok(registered.map(PortId::to_eterm))
    }

    fn register(&mut self, name: Atom, id: Eterm) -> Result<bool> {
        // The BIF layer already restricted `id` to an internal pid or an internal port, and the
        // oracle's registry holds either — so both arms register for real.
        if let Ok(pid) = ProcessId::from_eterm(id) {
            return Ok(self.table_mut().register(name, pid).is_ok());
        }
        let port = PortId::from_eterm(id)?;
        // A name another *process* holds is the oracle's `badarg` (`register(init, Port)`), which
        // the port registry cannot see; a name another *port* holds, a port that already has a
        // name and a port that is not alive are all [`PortRegistry::name_port`]'s `false`.
        if self.table_mut().whereis(name).is_some() {
            return Ok(false);
        }
        let ports = Rc::clone(&self.ports);
        let inserted = match ports.try_borrow_mut() {
            Ok(mut registry) => registry.name_port(name, port),
            Err(_) => return Err(error::unsupported("the port table is already in use")),
        };
        Ok(inserted)
    }

    fn unregister(&mut self, name: Atom) -> Result<bool> {
        if self.table_mut().unregister(name).is_ok() {
            return Ok(true);
        }
        let ports = Rc::clone(&self.ports);
        let removed = match ports.try_borrow_mut() {
            Ok(mut registry) => registry.unname_port(name),
            Err(_) => return Err(error::unsupported("the port table is already in use")),
        };
        Ok(removed)
    }

    fn registered(&mut self) -> Result<Vec<Atom>> {
        let mut names: Vec<Atom> = self
            .table_mut()
            .registered()
            .map(|(name, _)| name)
            .collect();
        let ports = Rc::clone(&self.ports);
        let port_names = match ports.try_borrow() {
            Ok(registry) => registry.port_names(),
            Err(_) => return Err(error::unsupported("the port table is already in use")),
        };
        names.extend(port_names);
        Ok(names)
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
            // `async_dist` is the one flag with **no PCB field**: `beam_process::ProcessFlags`
            // carries no such bit, so the runtime keeps it in its own per-pid store
            // ([`Runtime::async_dist`]) — the same pattern as `group_leaders`. The `process`
            // borrow is unused in this arm, so the runtime can be read and written here; the
            // store is the one source of truth the `process_info/2` item also reads, so the two
            // can never disagree (the C: `bif.c:2032-2043`).
            "async_dist" => {
                let Some(new) = bool_value(atoms, value) else {
                    return Ok(None);
                };
                let previous = self.runtime.is_async_dist(pid);
                self.runtime.set_async_dist(pid, new);
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
            // `message_queue_data` (`on_heap`/`off_heap`) is the message-queue placement flag
            // (`bif.c:2150-2163`). `off_heap`/`on_heap` are both **accepted** and the real
            // current placement is answered: this runtime keeps every process's message queue
            // heap-resident (`beam_message::Queue` on the process's own heap allocation), which
            // is exactly the `on_heap` the `message_queue_data` `process_info/2` item reports —
            // honouring `off_heap` would need per-process queue placement the runtime does not
            // model. A value that is neither atom is the oracle's `badarg`.
            "message_queue_data" => {
                match atom_name(atoms, value).as_deref() {
                    Some("on_heap") | Some("off_heap") => {}
                    _ => return Ok(None),
                }
                let atom = atoms.lookup("on_heap").ok_or(BeamError::TermNotFound)?;
                Ok(Some(make_atom(atom)))
            }
            // `max_heap_size` — the per-process heap bound and its three flags
            // (`MAX_HEAP_SIZE_GET(p)`/`MAX_HEAP_SIZE_FLAGS_GET(p)`, `bif.c:2148-2166`).
            //
            // The value is accepted in the C's two shapes — a non-negative integer, or a map
            // with a mandatory `size` and optional `kill`/`error_logger`/
            // `include_shared_binaries` booleans — and the parse is
            // [`RuntimeContext::max_heap_setting`]'s, the very same one `spawn_opt`'s
            // `{max_heap_size, …}` option uses (no second acceptance set). A nonzero `size`
            // below the **process's own** `min_heap_size` is the C's `badarg`
            // (`bif.c:2155`, `MIN_HEAP_SIZE(BIF_P)`), and a rejected value leaves the current
            // setting untouched (probed).
            //
            // The answer is the **previous** setting as the oracle's four-key map, built by the
            // same `max_heap_size_map` builder `process_info(Pid, max_heap_size)` renders
            // through. The new setting is recorded in the runtime's per-pid store
            // ([`Runtime::max_heap`](crate::Runtime::max_heap)), which is what the
            // `process_info` items read.
            //
            // Residuum: the recorded bound is **not enforced** — no `error_logger` report and no
            // kill at the limit — because neither `beam_process::Process` nor
            // `beam_gc::AnyCollector` exposes a policy setter, so the `beam_gc` ceiling a
            // process was born with cannot be replaced (see `M1_GAPS.md` *Heap switches*).
            "max_heap_size" => {
                // The floor is the process's own stored `min_heap_size` (the C's
                // `MIN_HEAP_SIZE(BIF_P)`), and the current setting is what it reports right
                // now; both are read off the PCB we already hold, then the borrow ends.
                let floor = process.collector().policy().min_heap_words;
                let born_with = process.collector().policy().max_heap_words;
                let current = self.runtime.max_heap_setting(pid).unwrap_or_else(|| {
                    MaxHeapSetting {
                        size: born_with,
                        flags: self.runtime.config().heap_options().max_heap_flags(),
                    }
                });
                let Ok(setting) = self.with_ctx(|ctx| ctx.max_heap_setting(value)) else {
                    return Ok(None);
                };
                if setting.size != 0 && setting.size < floor {
                    return Ok(None);
                }
                self.runtime.set_max_heap_setting(pid, setting);
                let previous = self.with_ctx(|ctx| ctx.max_heap_size_map(current))?;
                Ok(Some(previous))
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
    use beam_term::{make_small, NIL};

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

    /// A live `{spawn_driver, "ram_file_drv"}` port opened on the runtime's **own** registry, so
    /// the bridge sees exactly the port a program opened.
    ///
    /// The name/option terms are built on a scratch [`Heap`] (the `open_port/2` shape) and the
    /// atoms it needs are interned in the runtime's shared table first — `open_port/2` resolves
    /// them **by name**, so a missing atom would be `badarg`.
    fn live_port(rt: &mut Runtime) -> PortId {
        use beam_bifs::PortHook;
        for name in ["spawn_driver", "ram_file_drv"] {
            atom(rt, name);
        }
        let mut heap = Heap::new(64).expect("a scratch heap");
        let driver = char_list(&mut heap, "ram_file_drv");
        let tag = make_atom(rt.code_server.atoms().lookup("spawn_driver").expect("interned"));
        let name = heap.tuple(&[tag, driver]).expect("a name tuple");
        let owner = rt.self_pid();
        let mut hook = crate::port::RuntimePorts::new(
            Rc::clone(&rt.ports),
            rt.code_server.atoms_handle(),
            owner,
        );
        hook.open_port(name, NIL)
            .expect("open_port({spawn_driver, \"ram_file_drv\"}, [])")
    }

    /// `text` as an Erlang char list on `heap` (a `String`-shaped `open_port/2` operand).
    fn char_list(heap: &mut Heap, text: &str) -> Eterm {
        let mut list = NIL;
        for byte in text.bytes().rev() {
            list = heap
                .cons(make_small(i64::from(byte)), list)
                .expect("a cons cell");
        }
        list
    }

    #[test]
    fn a_port_registers_like_a_pid_and_leaves_the_registry_on_close() {
        let mut rt = runtime();
        let port = live_port(&mut rt);
        let closed = live_port(&mut rt);
        rt.ports
            .borrow_mut()
            .close(closed)
            .expect("the port closes");
        let name = atom(&mut rt, "zz_bridge_port_name");
        let second = atom(&mut rt, "zz_bridge_port_name_2");
        let dead_name = atom(&mut rt, "zz_bridge_dead_port_name");
        let taken_by_process = atom(&mut rt, "init");
        let ports = Rc::clone(&rt.ports);
        let self_pid = rt.self_pid();
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, self_pid);

        // Registering a live port succeeds, and `whereis/1` answers the **port term** (the
        // oracle's `#Port<0.3>`), not a pid.
        assert!(bridge.register(name, port.to_eterm()).unwrap());
        assert_eq!(bridge.whereis(name).unwrap(), Some(port.to_eterm()));
        // `registered/0` lists it alongside the process names.
        assert!(bridge.registered().unwrap().contains(&name));
        // A second name for the **same** port is the oracle's `badarg` (`false` here) ...
        assert!(!bridge.register(second, port.to_eterm()).unwrap());
        // ... as is a name a **process** already holds (`register(init, Port)`).
        assert!(!bridge.register(taken_by_process, port.to_eterm()).unwrap());
        // A **closed** port cannot be registered at all, and no name appears for it.
        assert!(!bridge.register(dead_name, closed.to_eterm()).unwrap());
        assert_eq!(bridge.whereis(dead_name).unwrap(), None);
        // `unregister/1` removes the name — once.
        assert!(bridge.unregister(name).unwrap());
        assert_eq!(bridge.whereis(name).unwrap(), None);
        assert!(!bridge.unregister(name).unwrap());
        // A name a **port** holds is not a process name: registering it again and unregistering
        // it goes through the port registry and leaves `registered/0` without it.
        assert!(bridge.register(name, port.to_eterm()).unwrap());
        assert!(bridge.unregister(name).unwrap());
        assert!(!bridge.registered().unwrap().contains(&name));
    }    #[test]
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
        let async_dist = atom(&mut rt, "async_dist");
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
        // `async_dist`: the default is `false`, and the flag round-trips through the runtime's
        // own per-pid store (the one place it lives — `beam_process::ProcessFlags` has no such
        // bit), so the second set really reports the `true` it replaced.
        assert_eq!(
            previous(
                &atoms,
                bridge.process_flag(&mut heap, &atoms, async_dist, make_atom(v_true))
            ),
            "false"
        );
        assert_eq!(
            previous(
                &atoms,
                bridge.process_flag(&mut heap, &atoms, async_dist, make_atom(v_false))
            ),
            "true"
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
        // ... and a non-boolean value for `async_dist`.
        assert!(bridge
            .process_flag(&mut heap, &atoms, async_dist, make_atom(not_a_bool))
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
    // -----------------------------------------------------------------------------------
    // process_flag(max_heap_size, …)
    // -----------------------------------------------------------------------------------
    /// The `max_heap_size` map `[("size", 0), ("kill", true), ("error_logger", true),
    /// ("include_shared_binaries", false)]` as the **term** the flag takes, built by the
    /// runtime's own map builder (the atoms are interned through the shared table first).
    ///
    /// Each entry is `(key, value)` where a `None` value omits the key — the shape an
    /// omitted flag has (its value is then the emulator default).
    fn max_heap_arg(rt: &mut Runtime, entries: &[(&str, Option<Eterm>)]) -> Eterm {
        for (name, _) in entries {
            atom(rt, name);
        }
        let pairs: Vec<(Eterm, Eterm)> = entries
            .iter()
            .filter_map(|(name, value)| {
                let key = rt.code_server.atoms().lookup(name).expect("interned");
                value.map(|value| (make_atom(key), value))
            })
            .collect();
        let atoms = rt.code_server.atoms_handle();
        let mut sink = beam_bifs::NullSink;
        let mut context = RuntimeContext::with_output(rt, &mut sink);
        beam_engine::terms::make_map(&mut context, &pairs, &atoms).expect("a map")
    }
    /// The `size` entry of a `max_heap_size` map term, and every flag it names.
    ///
    /// Read through `beam_term::map::map_get` — the same accessor the flag's own parser uses —
    /// so a map this bridge built is read back exactly as a caller's map would be.
    fn max_heap_parts(rt: &mut Runtime, map: Eterm) -> (Option<i64>, Vec<(String, bool)>) {
        let atoms = rt.code_server.atoms_handle();
        let mut size = None;
        if let Some(key) = atoms.lookup("size") {
            if let Ok(Some(value)) = beam_term::map::map_get(map, make_atom(key), &atoms) {
                size = is_small(value).then(|| signed_value(value));
            }
        }
        let mut flags = Vec::new();
        for name in ["kill", "error_logger", "include_shared_binaries"] {
            let Some(key) = atoms.lookup(name) else {
                continue;
            };
            match beam_term::map::map_get(map, make_atom(key), &atoms) {
                Ok(Some(value)) => flags.push((
                    name.to_string(),
                    atom_name(&atoms, value).as_deref() == Some("true"),
                )),
                _ => {}
            }
        }
        (size, flags)
    }
    /// The `max_heap_size` map's `size` entry and its three flags, as the tuple the tests
    /// compare: `(size, kill, error_logger, include_shared_binaries)`.
    fn max_heap_summary(rt: &mut Runtime, map: Eterm) -> (Option<i64>, Option<bool>, Option<bool>, Option<bool>) {
        let (size, flags) = max_heap_parts(rt, map);
        let flag = |name: &str| {
            flags
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| *value)
        };
        (
            size,
            flag("kill"),
            flag("error_logger"),
            flag("include_shared_binaries"),
        )
    }
    /// `process_info(Pid, max_heap_size)`'s map, read through the bridge's own item builder.
    fn info_max_heap(rt: &mut Runtime, pid: ProcessId) -> Eterm {
        let item = atom(rt, "max_heap_size");
        let atoms = rt.code_server.atoms_handle();
        let ports = Rc::clone(&rt.ports);
        let mut bridge = ProcessBridge::new(rt, None, ports, None, pid);
        let mut pool = heap();
        match bridge.process_info_item(&mut pool, &atoms, pid, item).unwrap() {
            ProcessInfoItem::Value(term) => term,
            other => panic!("expected the max_heap_size item, got {other:?}"),
        }
    }
    /// The oracle probe behind these expectations (OTP 29.1.1 / erts-17.1):
    ///
    /// ```text
    /// erl -noshell -eval '
    ///     P1 = process_flag(max_heap_size, #{size => 1000}),
    ///     I1 = process_info(self(), max_heap_size),
    ///     P2 = process_flag(max_heap_size, #{size => 1300, kill => false,
    ///                                        error_logger => false,
    ///                                        include_shared_binaries => true}),
    ///     I2 = process_info(self(), max_heap_size),
    ///     io:format("~p~n", [{P1, P2, I1, I2}]), halt(0).'
    /// ```
    ///
    /// `P1` is the fresh-process default `#{error_logger => true, include_shared_binaries =>
    /// false, kill => true, size => 0}`; `I1` reports `size => 1000` with those same flags; `P2`
    /// is the state `P1` set (all four keys); `I2` reports the new setting.
    #[test]
    fn max_heap_size_stores_the_setting_and_answers_the_previous_map() {
        let mut rt = runtime();
        let pid = rt.self_pid();
        let flag = atom(&mut rt, "max_heap_size");
        let v_true = atom(&mut rt, "true");
        let v_false = atom(&mut rt, "false");

        // The fresh process has no explicit bound: every flag is the emulator default.
        let fresh = info_max_heap(&mut rt, pid);
        assert_eq!(
            max_heap_summary(&mut rt, fresh),
            (Some(0), Some(true), Some(true), Some(false))
        );

        // A first set answers that default and records the new bound.
        let size = max_heap_arg(&mut rt, &[("size", Some(make_small(1000)))]);
        let atoms = rt.code_server.atoms_handle();
        let ports = Rc::clone(&rt.ports);
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, pid);
        let mut pool = heap();
        let previous = bridge
            .process_flag(&mut pool, &atoms, flag, size)
            .unwrap()
            .expect("a legal value has a previous value");
        let previous = max_heap_summary(&mut rt, previous);
        assert_eq!(
            previous,
            (Some(0), Some(true), Some(true), Some(false)),
            "the previous value of a fresh process is the default map"
        );
        let stored = info_max_heap(&mut rt, pid);
        assert_eq!(
            max_heap_summary(&mut rt, stored),
            (Some(1000), Some(true), Some(true), Some(false)),
            "process_info reports the new bound"
        );

        // A second set with all four keys answers the *previous* setting and replaces every
        // flag, which `process_info` then reports.
        let size = max_heap_arg(
            &mut rt,
            &[
                ("size", Some(make_small(1300))),
                ("kill", Some(make_atom(v_false))),
                ("error_logger", Some(make_atom(v_false))),
                ("include_shared_binaries", Some(make_atom(v_true))),
            ],
        );
        let atoms = rt.code_server.atoms_handle();
        let ports = Rc::clone(&rt.ports);
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, pid);
        let mut pool = heap();
        let previous = bridge
            .process_flag(&mut pool, &atoms, flag, size)
            .unwrap()
            .expect("a legal value has a previous value");
        assert_eq!(
            max_heap_summary(&mut rt, previous),
            (Some(1000), Some(true), Some(true), Some(false))
        );
        let stored = info_max_heap(&mut rt, pid);
        assert_eq!(
            max_heap_summary(&mut rt, stored),
            (Some(1300), Some(false), Some(false), Some(true))
        );

        // The legacy bare-integer form is accepted and answers the same map shape.
        let atoms = rt.code_server.atoms_handle();
        let ports = Rc::clone(&rt.ports);
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, pid);
        let mut pool = heap();
        let previous = bridge
            .process_flag(&mut pool, &atoms, flag, make_small(4000))
            .unwrap()
            .expect("a bare integer is a legal value");
        assert_eq!(
            max_heap_summary(&mut rt, previous),
            (Some(1300), Some(false), Some(false), Some(true))
        );
        let stored = info_max_heap(&mut rt, pid);
        assert_eq!(
            max_heap_summary(&mut rt, stored),
            (Some(4000), Some(true), Some(true), Some(false)),
            "the integer form resets the flags to the emulator defaults"
        );
    }
    /// Every `badarg` the oracle raises, and every value it **accepts**, at the bridge
    /// boundary.
    ///
    /// Oracle (OTP 29.1.1 / erts-17.1), each probed in a fresh process:
    ///
    /// ```text
    /// badarg: #{}, #{kill => true}, #{size => 232}, #{size => -1}, #{size => 1.0},
    ///         #{size => foo}, #{size => 1 bsl 70}, #{size => 1000, kill => 1},
    ///         #{size => 1000, kill => foo}, #{size => 1000, error_logger => yes},
    ///         #{size => 1000, include_shared_binaries => no}, #{size => 1000, kill => #{}},
    ///         232, -1, 1.0, foo, [], {size, 1000}
    /// ok:     #{size => 0}, #{size => 233}, #{size => 1000, zzz => 1},
    ///         #{size => 1000, 1 => 2}, #{size => 1000, min_bin_vheap_size => 5000},
    ///         #{size => 1000, include_shared_binary => true}, 233, 0, 4000
    /// ```
    ///
    /// `232` is below the fresh process's `min_heap_size` (233) and `233` is not, which is the
    /// C's `max_heap_size < MIN_HEAP_SIZE(BIF_P)` check (`bif.c:2155`).
    #[test]
    fn max_heap_size_rejections_and_acceptances_match_the_oracle() {
        let mut rt = runtime();
        let flag = atom(&mut rt, "max_heap_size");
        let v_true = atom(&mut rt, "true");
        let foo = atom(&mut rt, "foo");
        let one = make_small(1);
        // A boxed term that is not a small integer — the `1.0` / `1 bsl 70` probes' branch
        // (`!is_small`, so `spawn_words` rejects it); a float term needs the crate-forbidden
        // `unsafe` writer, and a list takes the very same path.
        let not_a_small = {
            let mut scratch = Heap::new(16).expect("a scratch heap");
            scratch.cons(make_small(1), NIL).expect("a cons cell")
        };

        // The rejected values: `Ok(None)` is the hook's `badarg` contract.
        let rejected: Vec<(&str, Eterm)> = vec![
            ("empty map", max_heap_arg(&mut rt, &[])),
            (
                "no size, flags only",
                max_heap_arg(&mut rt, &[("kill", Some(make_atom(v_true)))]),
            ),
            (
                "size 232 (below the min heap)",
                max_heap_arg(&mut rt, &[("size", Some(make_small(232)))]),
            ),
            (
                "negative size",
                max_heap_arg(&mut rt, &[("size", Some(make_small(-1)))]),
            ),
            (
                "a boxed (non-small) size",
                max_heap_arg(&mut rt, &[("size", Some(not_a_small))]),
            ),
            (
                "atom size",
                max_heap_arg(&mut rt, &[("size", Some(make_atom(foo)))]),
            ),
            (
                "kill => 1",
                max_heap_arg(
                    &mut rt,
                    &[("size", Some(make_small(1000))), ("kill", Some(one))],
                ),
            ),
            (
                "kill => foo",
                max_heap_arg(
                    &mut rt,
                    &[("size", Some(make_small(1000))), ("kill", Some(make_atom(foo)))],
                ),
            ),
            (
                "error_logger => foo",
                max_heap_arg(
                    &mut rt,
                    &[
                        ("size", Some(make_small(1000))),
                        ("error_logger", Some(make_atom(foo))),
                    ],
                ),
            ),
            (
                "include_shared_binaries => foo",
                max_heap_arg(
                    &mut rt,
                    &[
                        ("size", Some(make_small(1000))),
                        ("include_shared_binaries", Some(make_atom(foo))),
                    ],
                ),
            ),
            ("bare atom", make_atom(foo)),
            ("bare negative integer", make_small(-1)),
            ("bare boxed term", not_a_small),
            ("bare nil", NIL),
        ];
        for (label, value) in rejected {
            let atoms = rt.code_server.atoms_handle();
            let ports = Rc::clone(&rt.ports);
            let pid = rt.self_pid();
            let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, pid);
            let mut pool = heap();
            assert!(
                bridge
                    .process_flag(&mut pool, &atoms, flag, value)
                    .unwrap()
                    .is_none(),
                "{label} must be the oracle's badarg"
            );
        }

        // The accepted values, each in its own fresh runtime so the floor stays 233.
        let accepted: Vec<(&str, Eterm)> = {
            let mut rt = runtime();
            vec![
                ("size 0", max_heap_arg(&mut rt, &[("size", Some(make_small(0)))])),
                (
                    "size 233",
                    max_heap_arg(&mut rt, &[("size", Some(make_small(233)))]),
                ),
                (
                    "an unknown key",
                    max_heap_arg(
                        &mut rt,
                        &[("size", Some(make_small(1000))), ("zzz", Some(one))],
                    ),
                ),
                (
                    "a misspelt flag",
                    max_heap_arg(
                        &mut rt,
                        &[
                            ("size", Some(make_small(1000))),
                            ("include_shared_binary", Some(make_atom(v_true))),
                        ],
                    ),
                ),
                (
                    "min_bin_vheap_size",
                    max_heap_arg(
                        &mut rt,
                        &[("size", Some(make_small(1000))), ("min_bin_vheap_size", Some(one))],
                    ),
                ),
                ("bare 0", make_small(0)),
                ("bare 233", make_small(233)),
                ("bare 4000", make_small(4000)),
            ]
        };
        for (label, value) in accepted {
            let atoms = rt.code_server.atoms_handle();
            let ports = Rc::clone(&rt.ports);
            let pid = rt.self_pid();
            let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, pid);
            let mut pool = heap();
            assert!(
                bridge
                    .process_flag(&mut pool, &atoms, flag, value)
                    .unwrap()
                    .is_some(),
                "{label} must be accepted, as the oracle accepts it"
            );
        }
    }
    /// A rejected value leaves the setting **untouched** (oracle: set 2000, a `#{size => 232}`
    /// `badarg`, then `process_info` still reports 2000), and the flag applies to the caller
    /// only — a sibling process keeps the setting it was born with.
    #[test]
    fn a_rejected_max_heap_size_changes_nothing_and_the_flag_is_per_process() {
        let mut rt = runtime();
        let pid = rt.self_pid();
        let flag = atom(&mut rt, "max_heap_size");
        let other = rt.processes.spawn_default().expect("a sibling process");

        let value = max_heap_arg(&mut rt, &[("size", Some(make_small(2000)))]);
        let atoms = rt.code_server.atoms_handle();
        let ports = Rc::clone(&rt.ports);
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, pid);
        let mut pool = heap();
        assert!(bridge
            .process_flag(&mut pool, &atoms, flag, value)
            .unwrap()
            .is_some());
        // The sibling is untouched by the caller's setting.
        let sibling = info_max_heap(&mut rt, other);
        assert_eq!(
            max_heap_summary(&mut rt, sibling),
            (Some(0), Some(true), Some(true), Some(false))
        );

        // A rejected value answers `badarg` and leaves the current setting in place.
        let bad = max_heap_arg(&mut rt, &[("size", Some(make_small(232)))]);
        let atoms = rt.code_server.atoms_handle();
        let ports = Rc::clone(&rt.ports);
        let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, pid);
        let mut pool = heap();
        assert!(bridge
            .process_flag(&mut pool, &atoms, flag, bad)
            .unwrap()
            .is_none());
        let stored = info_max_heap(&mut rt, pid);
        assert_eq!(
            max_heap_summary(&mut rt, stored),
            (Some(2000), Some(true), Some(true), Some(false))
        );
    }
    /// The bound's floor is the **process's own** `min_heap_size`, not the emulator default:
    /// a child spawned with `{min_heap_size, 1000}` has the rounded 1598, so `1597` is
    /// `badarg` and `1598` is accepted — the oracle's exact boundary (probed).
    #[test]
    fn the_max_heap_size_floor_follows_the_process_own_min_heap_size() {
        let mut rt = runtime();
        let flag = atom(&mut rt, "max_heap_size");
        let policy = beam_gc::HeapPolicy::from_spawn_opts(1000, 0, 65535);
        assert_eq!(policy.min_heap_words, 1598, "the growth table's rounding");
        let child = rt.processes.spawn(policy).expect("a child");

        for (size, accepted) in [(1597_i64, false), (1598, true)] {
            let value = max_heap_arg(&mut rt, &[("size", Some(make_small(size)))]);
            let atoms = rt.code_server.atoms_handle();
            let ports = Rc::clone(&rt.ports);
            let mut bridge = ProcessBridge::new(&mut rt, None, ports, None, child);
            let mut pool = heap();
            let result = bridge
                .process_flag(&mut pool, &atoms, flag, value)
                .unwrap();
            assert_eq!(
                result.is_some(),
                accepted,
                "size {size} against a 1598-word min_heap_size"
            );
        }
    }
}
