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

//! Read-only **execution-position introspection**: the engine's view of a
//! running (or paused) process.
//!
//! These accessors are the engine half of `beam_rt`'s
//! `erlang:process_info/2` items that describe *where a process is*:
//! `current_function`, `current_location`, `current_stacktrace` and
//! `initial_call`. ERTS reads them from the process's saved instruction pointer
//! and its `$initial_call`; the engine keeps the same two facts —
//! [`Machine::current_code`] (the last interpreted `(func, pc)`) and
//! [`Machine::entry_call`] (the run's entry function) — and turns them into the
//! exact term shapes the oracle returns.
//!
//! ## Oracle shapes (verified against the system `erl`, OTP 29 / erts-17.1)
//!
//! ```text
//! erlang:process_info(P, current_function)    -> {current_function, {M,F,A}}
//! erlang:process_info(P, initial_call)        -> {initial_call,     {M,F,A}}
//! erlang:process_info(P, current_location)    -> {current_location, {M,F,A,[{file,File},{line,Line}]}}
//! erlang:process_info(P, current_stacktrace)  -> {current_stacktrace,
//!                                                  [{M,F,A,Loc}, {M,F,A,Loc}, ...]}
//! ```
//!
//! A `Location` list is `[{file,File},{line,Line}]` when the function has a source
//! location in effect (the `build_stacktrace` shape) and [`NIL`] (the empty list,
//! rendered `[]`) when it has none. The accessors here return the **value** of each
//! item — the `{M,F,A}` / `{M,F,A,Location}` tuple or the stacktrace list — so the
//! runtime adds only the `{current_function, …}` wrapper.
//!
//! The term builders are shared with the exception family's `build_stacktrace`
//! (`crate::families::exceptions::stacktrace`) so the `{M,F,A,Location}` encoding —
//! including which atoms are resolved from the module and how a file name becomes a
//! char list — has exactly one definition.
//!
//! ## Honesty
//!
//! Nothing here invents a value. Before the machine has interpreted anything there
//! is no current position, so [`Machine::current_function`]/
//! [`Machine::current_location`] return `Ok(None)` and
//! [`Machine::current_stacktrace`] is the empty list; a `func` index outside the
//! module is a typed error; and a `Location` the module cannot name (a missing
//! `file`/`line` atom, a file index outside the `Line` chunk) degrades to the
//! empty list exactly as `build_stacktrace` does. The stacktrace is the engine's
//! **cooked** (`build_stacktrace`-style) trace over its **live interpreted
//! frames** — the same approximation the exception family documents — so it omits
//! host frames outside the run rather than fabricating them.

use beam_common::Result;
use beam_loader::{Function, Module};
use beam_term::{make_atom, make_small, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::families::exceptions::stacktrace::{stacktrace_item, symbolic_stacktrace};
use crate::terms;

use super::Machine;

impl Machine {
    /// The code address the machine is currently executing, or was most recently
    /// suspended at, as `(function index, code offset)` — the engine's read-only
    /// "saved instruction pointer".
    ///
    /// `None` before the machine has interpreted an instruction (and after
    /// [`Machine::reset`]). This is the raw input to every derived accessor here;
    /// like them it is exact at a suspension point and is the *last interpreted*
    /// instruction while a native JIT tier runs a block mid-flight.
    pub fn current_code(&self) -> Option<(u32, u32)> {
        self.current_code
    }

    /// The `(function index, arity)` the run was entered at — the engine's
    /// analogue of ERTS's `$initial_call`.
    ///
    /// `None` before any run. This is the raw form of [`Machine::initial_call`].
    pub fn entry_call(&self) -> Option<(u32, u32)> {
        self.entry_call
    }

    /// The value of `process_info(P, current_function)`: `{Module, Function,
    /// Arity}` for the function the machine is currently in.
    ///
    /// `Ok(None)` when no instruction has been interpreted yet (there is no current
    /// function), and a typed error when the current `func` index is outside
    /// `module`.
    pub fn current_function(
        &self,
        module: &Module,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Option<Eterm>> {
        match self.current_code {
            Some((func, _pc)) => mfa_term(module, ctx, func).map(Some),
            None => Ok(None),
        }
    }

    /// The value of `process_info(P, initial_call)`: `{Module, Function, Arity}`
    /// for the function the run was entered at (the `$initial_call` analogue).
    ///
    /// `Ok(None)` when the machine has not begun a run, and a typed error when the
    /// recorded `func` index is outside `module`.
    pub fn initial_call(
        &self,
        module: &Module,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Option<Eterm>> {
        match self.entry_call {
            Some((func, _arity)) => mfa_term(module, ctx, func).map(Some),
            None => Ok(None),
        }
    }

    /// The value of `process_info(P, current_location)`: `{Module, Function, Arity,
    /// Location}` where `Location` is `[{file,File},{line,Line}]` (or `[]` when the
    /// function has none) — the same `{M,F,A,Location}` shape a stacktrace item has.
    ///
    /// `Ok(None)` when no instruction has been interpreted yet, and a typed error
    /// when the current `func` index is outside `module`.
    pub fn current_location(
        &self,
        module: &Module,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Option<Eterm>> {
        match self.current_code {
            Some((func, pc)) => stacktrace_item(module, ctx, func, pc).map(Some),
            None => Ok(None),
        }
    }

    /// The value of `process_info(P, current_stacktrace)`: the **cooked** stacktrace
    /// `[{Module, Function, Arity, Location}, …]`, innermost frame first.
    ///
    /// Built from the machine's current position and its live interpreted frames by
    /// the exception family's `build_stacktrace` (`symbolic_stacktrace`), so it
    /// carries that builder's documented approximation: it describes the engine's
    /// live call frames and cannot name host frames outside the run. Before any
    /// instruction has been interpreted the stacktrace is the empty list ([`NIL`]).
    pub fn current_stacktrace(
        &self,
        module: &Module,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        match self.current_code {
            Some((func, pc)) => symbolic_stacktrace(self, module, ctx, func, pc),
            None => Ok(NIL),
        }
    }
}

/// Resolve the function at index `func` in `module`, or the crate's typed
/// `unsupported` error when the index is out of range (never a panic).
fn function_at(module: &Module, func: u32) -> Result<&Function> {
    module
        .functions
        .get(func as usize)
        .ok_or_else(|| error::unsupported("process_info: function index out of range"))
}

/// The `{Module, Function, Arity}` tuple for the function at index `func` — the
/// `current_function`/`initial_call` value (C's MFA term).
fn mfa_term(module: &Module, ctx: &mut dyn ExecutionContext, func: u32) -> Result<Eterm> {
    let function = function_at(module, func)?;
    terms::make_tuple(
        ctx,
        &[
            make_atom(function.module),
            make_atom(function.name),
            make_small(function.arity as i64),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::Machine;
    use beam_common::BeamError;
    use beam_loader::Module;
    use beam_term::NIL;

    use crate::context::MinimalContext;

    #[test]
    fn a_fresh_machine_reports_no_position() {
        let machine = Machine::new();
        assert_eq!(machine.current_code(), None);
        assert_eq!(machine.entry_call(), None);
    }

    #[test]
    fn an_out_of_range_position_is_a_typed_error_never_a_panic() {
        // `func` outside the module's function table: the accessors must report the
        // crate's typed error rather than indexing out of bounds or inventing a name.
        let module = Module::default();
        let mut machine = Machine::new();
        machine.current_code = Some((3, 0));
        machine.entry_call = Some((7, 2));
        let mut ctx = MinimalContext::new();

        let err = machine.current_function(&module, &mut ctx).unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
        let err = machine.current_location(&module, &mut ctx).unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
        let err = machine.initial_call(&module, &mut ctx).unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)), "got {err:?}");
    }

    #[test]
    fn no_recorded_position_yields_none_and_an_empty_stacktrace() {
        let module = Module::default();
        let machine = Machine::new();
        let mut ctx = MinimalContext::new();
        assert_eq!(machine.current_function(&module, &mut ctx).unwrap(), None);
        assert_eq!(machine.initial_call(&module, &mut ctx).unwrap(), None);
        assert_eq!(machine.current_location(&module, &mut ctx).unwrap(), None);
        assert_eq!(machine.current_stacktrace(&module, &mut ctx).unwrap(), NIL);
    }
}
