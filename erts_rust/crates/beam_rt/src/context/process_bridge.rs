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
                let previous = process.priority();
                process.set_priority(priority);
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
        Ok(self.self_pid)
    }
}
