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
//! Everything else — another pid, a nested query, a name that is not one of the five —
//! falls straight through to the ordinary path, so a typed gap stays a typed gap and no
//! frame is ever fabricated.
//!
//! # Soundness of the published terms
//!
//! The five values are the engine's own builders
//! ([`Machine::current_stacktrace`], [`Machine::current_function`],
//! [`Machine::current_location`], [`Machine::initial_call`]) and are handed back verbatim.
//! They are stable for the whole window they are published: **no collection can run while a
//! BIF call is in flight** (the engine publishes no root set at a BIF dispatch, so the
//! heap-admission point is never reached from inside — `crate::context::gc`), and the
//! window is exactly one nested `call_bif` with no `?` between publication and clearing.

use beam_common::Result;
use beam_engine::{ExecutionContext, Machine};
use beam_loader::Module;
use beam_process::ProcessId;
use beam_term::{make_small, Eterm};

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
        Ok(Some(InMachinePosition {
            pid,
            initial_call: machine.initial_call(entry_module, self)?,
            current_function: machine.current_function(entry_module, self)?,
            current_location: machine.current_location(entry_module, self)?,
            current_stacktrace: Some(machine.current_stacktrace(entry_module, self)?),
            catchlevel: Some(make_small(machine.catch_frame_depth() as i64)),
        }))
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
}
