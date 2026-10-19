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

//! The **in-machine execution-position door**: `erlang:process_info/1,2` answered for the
//! process that is *executing* the query.
//!
//! # Why a door is needed
//!
//! A process's execution-position items (`current_function`/`initial_call`/
//! `current_location`/`current_stacktrace`, plus `catchlevel`) are read out of the
//! [`Machine`] that owns its [`Function`](beam_loader::Function) frame — see
//! [`crate::native::memory`]. The scheduler **takes the executing process's machine out**
//! of [`RunnerState`](crate::runner::RunnerState) for the whole slice to drive it, so the
//! runner-state view that serves *every other* process cannot see the caller's own
//! machine: `erlang:process_info(self(), current_stacktrace)` used to be the typed
//! capability gap `unsupported: process_info: current_stacktrace`. That is
//! `gen_server:handle_common_reply/5`'s `?STACKTRACE()` macro, so it tears down any
//! `gen_server` that reaches a `terminate/2` (and the `permanent` `kernel` supervision
//! tree with it).
//!
//! `beam_engine`'s [`ExecutionContext::wants_bif_in_machine`]/
//! [`ExecutionContext::call_bif_in_machine`] seam closes that: a host that opts in is
//! handed the **live `&Machine`** and the run's entry module on *every* BIF dispatch
//! (the single funnel [`Machine::dispatch_bif_call`](beam_engine::Machine), on both
//! execution tiers). This module is the opt-in, and it is deliberately narrow.
//!
//! # What it does
//!
//! [`RuntimeContext::in_machine_probe`] returns `true` (so the engine takes the in-machine
//! door). [`RuntimeContext::dispatch_in_machine`] then:
//!
//! * rejects everything that is not `erlang:process_info/1,2` with a single string compare
//!   (so a hot BIF costs one `len`+`memcmp` and nothing else);
//! * for a matching call, and only when the queried pid **is the executing process**,
//!   reads the five machine-sourced items **once** and publishes them in
//!   [`RuntimeContext::in_machine`] for the duration of the nested
//!   [`ExecutionContext::call_bif`] — [`crate::native::memory`]'s `position_value`/
//!   `catchlevel_value` consult that slot first;
//! * clears the slot **immediately** after the nested call returns.
//!
//! The same window carries the **`binary` item's engine half**: when (and only when) the
//! query names `binary`, the live machine's off-heap refc-binary reservoir
//! ([`Machine::refc_binaries`]) is read into the published value too, so
//! `erlang:process_info(self(), binary)` lists the payload the process just built —
//! [`crate::native::memory`]'s `snapshot` takes it as the reservoir half of the roster
//! union for the executing pid. A query that does not name `binary` reads no reservoir at
//! all, and every other query keeps the runner-state route.
//!
//! Everything else — another pid, a nested query, a name that is not one of the five —
//! falls straight through to the ordinary path, so a typed gap stays a typed gap and no
//! frame is ever fabricated.//!
//! # Soundness of the published terms
//!
//! The five values are the engine's own builders
//! ([`Machine::current_stacktrace`], [`Machine::current_function`],
//! [`Machine::current_location`], [`Machine::initial_call`]) and are handed back verbatim.
//! They are stable for the whole window they are published: **no collection can run while a
//! BIF call is in flight** (the engine publishes no root set at a BIF dispatch, so the
//! heap-admission point is never reached from inside — `crate::context::gc`), and the
//! window is exactly one nested `call_bif` with no `?` between publication and clearing.
//!
//! The `binary` roster is copied out as plain `usize` triples, never as a term: the payloads
//! stay owned by the machine's own reservoir for the whole window (the machine is alive and
//! nothing prunes it while a BIF runs), so the pointer, length and refcount are live figures,
//! and the reply's integers are built from them afterwards rather than from the payload.
use beam_common::Result;
use beam_engine::{ExecutionContext, Machine, terms};
use beam_loader::Module;
use beam_process::ProcessId;
use beam_term::{atom_val, is_atom, make_small, Eterm};

use super::runtime_context::RuntimeContext;

/// The execution-position answers read from the **live machine** of the running process.
///
/// One instance is published on [`RuntimeContext::in_machine`] for exactly one nested
/// `erlang:process_info/1,2` call, so `position_value`/`catchlevel_value` answer the
/// executing pid from the machine the scheduler has taken out of its own runner state.
///
/// Each field is the item's **value** — the `{M,F,A}` / `{M,F,A,Location}` tuple, the
/// stacktrace list, or the catch-level integer — exactly the shapes the machine's
/// accessors return and `position_value` would otherwise produce. `None` means the machine
/// has no such position (a process that has not interpreted an instruction yet), which
/// falls through to the ordinary (gapping) path rather than inventing a frame.
pub(crate) struct InMachinePosition {
    /// The pid these answers belong to — the executing process.
    pub(crate) pid: ProcessId,
    /// `initial_call`: the entry MFA of the run.
    initial_call: Option<Eterm>,
    /// `current_function`: the `{M,F,A}` the machine is currently in.
    current_function: Option<Eterm>,
    /// `current_location`: the `{M,F,A,Location}` the machine is currently in.
    current_location: Option<Eterm>,
    /// `current_stacktrace`: the cooked frame list, innermost first.
    current_stacktrace: Option<Eterm>,
    /// `catchlevel`: the live catch-frame depth.
    catchlevel: Option<Eterm>,
    /// The `binary` item's engine half: the executing machine's off-heap refc-binary
    /// reservoir ([`Machine::refc_binaries`]) as `(payload pointer, byte length,
    /// refcount)` triples, in build order.
    ///
    /// `None` — the read is **gated** on the query naming the `binary` item (see
    /// [`RuntimeContext::machine_position`]) — means "not read for this query", which is
    /// every query but `process_info(self(), binary)` and a list form containing it, so
    /// nothing else pays for the roster. `Some(vec)` is a real answer, possibly empty (a
    /// process holding no off-heap payload).
    off_heap_binaries: Option<Vec<(usize, usize, usize)>>,
}
impl InMachinePosition {
    /// The published value of the machine-sourced item `name`, or `None` when `name` is not
    /// one of the five machine-sourced items (or the machine has no such position).
    pub(crate) fn value(&self, name: &str) -> Option<Eterm> {
        match name {
            "initial_call" => self.initial_call,
            "current_function" => self.current_function,
            "current_location" => self.current_location,
            "current_stacktrace" => self.current_stacktrace,
            "catchlevel" => self.catchlevel,
            _ => None,
        }
    }
}

impl RuntimeContext<'_> {
    /// The engine's in-machine probe: this host **does** take the door
    /// ([`ExecutionContext::call_bif_in_machine`]).
    ///
    /// The engine latches this **once per run** into `Machine::bif_in_machine_on`, so the
    /// per-dispatch cost of opting in is one `bool` load; the price is the routing check in
    /// [`RuntimeContext::dispatch_in_machine`], which rejects a non-`process_info` BIF with a
    /// single string compare.
    pub(super) fn in_machine_probe(&self) -> bool {
        true
    }

    /// The engine's in-machine BIF dispatch
    /// ([`ExecutionContext::call_bif_in_machine`]): publish the executing process's
    /// execution position when — and only when — this dispatch is a self
    /// `erlang:process_info/1,2`, then delegate to the ordinary
    /// [`ExecutionContext::call_bif`] unchanged.
    ///
    /// # Routing
    ///
    /// The cheapest possible rejection comes first: `function != "process_info"` is one
    /// length check and, for the overwhelming majority of dispatches, the whole answer — so
    /// an ordinary BIF never touches the machine, the process id or the item vocabulary.
    /// Only an `erlang:process_info/1,2` whose queried pid **is the executing process**
    /// (`args[0] == self.pid()`) consults the machine; every other call — another process's
    /// query, a malformed argument, a nested query — is handed to `call_bif` untouched.
    ///
    /// # The published window
    ///
    /// The slot is set immediately before the nested `call_bif` and cleared immediately
    /// after it returns. There is no `?` between the two statements, so the slot cannot
    /// outlive the call that needs it — `crate::native::memory` sees it only while it answers
    /// this very query.
    pub(super) fn dispatch_in_machine(
        &mut self,
        machine: &Machine,
        entry_module: &Module,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if function == "process_info" && module == "erlang" && (arity == 1 || arity == 2) {
            if let Some(position) = self.machine_position(machine, entry_module, args)? {
                self.runtime.in_machine = Some(position);
                let result = self.call_bif(module, function, arity, args);
                self.runtime.in_machine = None;
                return result;
            }
        }
        self.call_bif(module, function, arity, args)
    }

    /// The executing process's machine-sourced position, or `None` when this call must take
    /// the ordinary path.
    ///
    /// `None` — the caller then dispatches through `call_bif` exactly as before — for:
    ///
    /// * a call that arrived while an **outer** door call is in flight (a nested
    ///   `erlang:process_info/1,2`), so the outer answer is never overwritten and the engine
    ///   would be re-read for a frame the outer call already published;
    /// * a first argument that is not a pid (the floor answers the oracle's `badarg`);
    /// * a queried pid that is **not** the executing process: that process's position is
    ///   still in the runner state, so the ordinary path answers it (or reports its own
    ///   honest gap).
    ///
    /// Otherwise the five values are read **once**, from the live machine, against the run's
    /// entry module (the module the machine's context-`0` frames belong to — `beam_engine`
    /// threads it here because a [`Machine`] borrows context `0` rather than owning it).
    ///
    /// # The `binary` roster is read only when the query names it
    ///
    /// The machine's off-heap refc-binary reservoir is the `binary` item's engine half
    /// (`crate::native::memory`), and reading it means one triple per payload. It is
    /// therefore **gated**: [`RuntimeContext::names_binary_item`] is consulted first, and only
    /// a query whose second argument is the atom `binary` — or a list containing it — pays.
    /// Every other self query of `process_info/2` (and every `process_info/1`, whose fixed
    /// key set has no `binary` item) stores `None` and reads nothing.
    fn machine_position(
        &mut self,
        machine: &Machine,
        entry_module: &Module,
        args: &[Eterm],
    ) -> Result<Option<InMachinePosition>> {
        if self.runtime.in_machine.is_some() {
            return Ok(None);
        }
        let Some(pid) = args
            .first()
            .copied()
            .and_then(|term| ProcessId::from_eterm(term).ok())
        else {
            return Ok(None);
        };
        if pid != self.pid() {
            return Ok(None);
        }
        let off_heap_binaries = if self.names_binary_item(args) {
            // The reservoir is the one holder of the payloads this process's own compiled
            // bitstring code built (`bs_create_bin`/`bs_init_writable` above
            // `ERL_ONHEAP_BINARY_LIMIT`), and no collection can run while a BIF call is in
            // flight, so each pointer stays live and `refcount()` is the live count for the
            // whole published window.
            Some(
                machine
                    .refc_binaries()
                    .iter()
                    .map(|binary| {
                        (binary.data_ptr() as usize, binary.len(), binary.refcount())
                    })
                    .collect(),
            )
        } else {
            None
        };
        Ok(Some(InMachinePosition {
            pid,
            initial_call: machine.initial_call(entry_module, self)?,
            current_function: machine.current_function(entry_module, self)?,
            current_location: machine.current_location(entry_module, self)?,
            current_stacktrace: Some(machine.current_stacktrace(entry_module, self)?),
            catchlevel: Some(make_small(machine.catch_frame_depth() as i64)),
            off_heap_binaries,
        }))
    }

    /// Whether this `erlang:process_info/2` call asks for the `binary` item — the atom
    /// `binary` on its own, or a list of items containing it.
    ///
    /// The comparison is by **atom index** against the shared table (`binary` is interned by
    /// the very call that asks for it), so no name is materialized: an atom inside the
    /// process table is one word compare. Anything else — an arity-1 call (whose fixed key
    /// set has no `binary` item), the `{dictionary, Key}` form, a non-atom, a non-list, an
    /// improper list — answers `false`, so the roster is simply not read.
    fn names_binary_item(&self, args: &[Eterm]) -> bool {
        let Some(item) = args.get(1).copied() else {
            return false;
        };
        let Some(binary) = self.runtime.code_server.atoms().lookup("binary") else {
            return false;
        };
        let is_binary_atom =
            |term: Eterm| is_atom(term) && atom_val(term) == binary;
        if is_binary_atom(item) {
            return true;
        }
        match terms::list_elements(item) {
            Ok(items) => items.iter().any(|term| is_binary_atom(*term)),
            Err(_) => false,
        }
    }
    /// The published machine answer for the item `name` of `pid`, when the in-machine door is
    /// open for exactly that pid.
    ///
    /// Consumed by [`crate::native::memory`]'s `position_value`/`catchlevel_value`. `None`
    /// for every other read — the door closed, a different pid, or a name the machine does
    /// not source — so the caller falls through to the runner-state/root-frame path and a
    /// typed gap stays a typed gap.
    pub(crate) fn in_machine_value(&self, name: &str, pid: ProcessId) -> Option<Eterm> {
        let position = self.runtime.in_machine.as_ref()?;
        if position.pid != pid {
            return None;
        }
        position.value(name)
    }

    /// The `binary` item's engine half for `pid`, when the in-machine door published a roster
    /// for exactly that pid.
    ///
    /// Consumed by [`crate::native::memory`]'s `snapshot` as the **reservoir half** of the
    /// roster union: the executing process's own machine is out of the runner state for the
    /// whole slice ([`RunnerState::reservoir_at`](crate::runner::RunnerState::reservoir_at)
    /// answers nothing for it), so this is the only source that can name the payload a process
    /// built earlier in the very slice that asks.
    ///
    /// `None` for every other read, so a caller falls through to
    /// [`RunnerState::reservoir_at`](crate::runner::RunnerState::reservoir_at) unchanged:
    /// * the door closed (a library caller with no
    ///   [`Machine`], the `-eval` route's own tree-walker) — no roster is fabricated;
    /// * a **different** pid (a nested non-self query while an outer door call is in flight);
    /// * a query that does not name the `binary` item (the roster was never read).
    ///
    /// [`Machine`]: beam_engine::Machine
    pub(crate) fn in_machine_off_heap_binaries(
        &self,
        pid: ProcessId,
    ) -> Option<&[(usize, usize, usize)]> {
        let position = self.runtime.in_machine.as_ref()?;
        if position.pid != pid {
            return None;
        }
        position.off_heap_binaries.as_deref()
    }
}