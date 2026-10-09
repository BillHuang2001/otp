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
//! ## Which module the position resolves in
//!
//! Every accessor here is **module-relative**: the `(func, pc)` it reads is an index
//! into a *specific* module's tables, so naming it in the wrong module invents an
//! `M`/`F`/`A`. The machine executes a cross-module callee **in place** (see
//! [`crate::machine::context`]), so the module that owns the current address is not
//! always the entry module the caller passed:
//!
//! * [`Machine::current_function`]/[`Machine::current_location`]/
//!   [`Machine::current_stacktrace`] follow the **module context** the machine is
//!   executing in — the callee's own module when a `call_ext*` the host resolved
//!   through [`ExecutionContext::resolve_external_call`] is running, else the
//!   `module` handed in (module context `0`, the run's entry module). That is what
//!   makes `process_info(Pid, current_function)` name the callee rather than the
//!   caller.
//! * [`Machine::initial_call`] is the one exception: its recorded `(func, arity)` is
//!   the **entry** call (the `$initial_call` analogue), which by definition belongs
//!   to the entry module, so it resolves against the passed `module` unconditionally.
//!
//! [`Machine::current_module`] exposes the resolved callee handle directly, and
//! [`Machine::wait_deadline`] uses the same resolution to read a parked
//! `receive … after T`'s delay out of the callee's own frames.
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
//! host frames outside the run rather than fabricating them, and each frame is
//! resolved in its **own** module context (see [`Machine::current_stacktrace`]).

use std::sync::Arc;

use beam_common::Result;
use beam_loader::{Function, Instr, Module, Operand};
use beam_term::{is_small, make_atom, make_small, signed_value, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::families::exceptions::stacktrace::stacktrace_item;
use crate::terms;

use super::{Machine, ModuleContext};

impl Machine {
    /// The **registered** module context the machine is executing in, or `None`
    /// when that is the run's entry module (context `0`).
    ///
    /// Context `0`'s module is deliberately *not* owned by the machine — the drive
    /// loop borrows it — so a caller resolving a position must supply it. Every
    /// other context is a `call_ext*` the host resolved through
    /// [`ExecutionContext::resolve_external_call`], and its module handle lives in
    /// the machine's own registry, which is what makes the module identity of a
    /// **suspended** cross-module frame survive until the host resumes it.
    fn active_context(&self) -> Option<Arc<ModuleContext>> {
        match self.current_context() {
            0 => None,
            id => self.module_context(id),
        }
    }

    /// The module whose instructions this machine is currently executing — the
    /// callee handle when it is inside a cross-module callee, `None` when that is
    /// the run's **entry** module (which the machine borrows rather than owns, so
    /// only the caller can name it).
    ///
    /// This is the machine's module-identity state: it follows the module context,
    /// so it is already the callee's while a cross-module callee runs — including
    /// while the process is *suspended* inside one
    /// ([`RunOutcome::Wait`](crate::RunOutcome::Wait) does not disturb it, and a
    /// resume does not reset the registry). `None` therefore means "the `module` you
    /// pass to [`Machine::current_function`] is the right one", not "unknown".
    pub fn current_module(&self) -> Option<Arc<Module>> {
        self.active_context().map(|context| context.module.clone())
    }

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
    /// `module` is the module the run was **entered** in (module context `0`); when
    /// the machine is executing a cross-module callee the callee's own module is
    /// resolved from its context registry instead, so the reported `M`/`F`/`A` names
    /// the module the address actually belongs to. A machine still executing the
    /// entry module — the common case — is unchanged.
    ///
    /// `Ok(None)` when no instruction has been interpreted yet (there is no current
    /// function), and a typed error when the current `func` index is outside the
    /// resolved module.
    pub fn current_function(
        &self,
        module: &Module,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Option<Eterm>> {
        let (func, _pc) = match self.current_code {
            Some(code) => code,
            None => return Ok(None),
        };
        match self.active_context() {
            Some(context) => mfa_term(&context.module, ctx, func).map(Some),
            None => mfa_term(module, ctx, func).map(Some),
        }
    }

    /// The value of `process_info(P, initial_call)`: `{Module, Function, Arity}`
    /// for the function the run was entered at (the `$initial_call` analogue).
    ///
    /// Unlike the other position accessors this one always resolves against the
    /// passed `module`: the recorded `(func, arity)` is the **entry** call, recorded
    /// once per run in the entry module, and it stays that call for the whole run —
    /// including while a cross-module callee runs, exactly as ERTS reports a
    /// process's `$initial_call` regardless of where it currently is.
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
    /// The module is resolved exactly as [`Machine::current_function`]'s is, so a
    /// location inside a cross-module callee is named in that callee's module.
    ///
    /// `Ok(None)` when no instruction has been interpreted yet, and a typed error
    /// when the current `func` index is outside the resolved module.
    pub fn current_location(
        &self,
        module: &Module,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Option<Eterm>> {
        let (func, pc) = match self.current_code {
            Some(code) => code,
            None => return Ok(None),
        };
        match self.active_context() {
            Some(context) => stacktrace_item(&context.module, ctx, func, pc).map(Some),
            None => stacktrace_item(module, ctx, func, pc).map(Some),
        }
    }

    /// The value of `process_info(P, current_stacktrace)`: the **cooked** stacktrace
    /// `[{Module, Function, Arity, Location}, …]`, innermost frame first.
    ///
    /// Built here — not by the exception family's single-module builder — from the
    /// machine's current position and its live interpreted frames, so it carries the
    /// builder's documented approximation: it describes the engine's live call frames
    /// and cannot name host frames outside the run. Unlike
    /// `symbolic_stacktrace` (which the `build_stacktrace` opcode uses and which
    /// resolves every frame against the single module it is handed), this assembles
    /// the list itself and resolves **each** frame in the module its own context
    /// names: the innermost item is the current position in the module the machine is
    /// executing in, and every outer frame ([`Machine::resumption_contexts`]) is
    /// resolved in the module its recorded context is, so a stacktrace taken inside a
    /// cross-module callee names the callee and the caller's module separately rather
    /// than resolving the caller's `(func, pc)` — which belongs to another module —
    /// against the callee (which would fabricate a frame). Before any instruction has
    /// been interpreted the stacktrace is the empty list ([`NIL`]).
    pub fn current_stacktrace(
        &self,
        module: &Module,
        ctx: &mut dyn ExecutionContext,
    ) -> Result<Eterm> {
        let (func, pc) = match self.current_code {
            Some(code) => code,
            None => return Ok(NIL),
        };
        // The current position resolves in the module the machine is executing in.
        let mut frames = Vec::new();
        match self.active_context() {
            Some(context) => frames.push(stacktrace_item(&context.module, ctx, func, pc)?),
            None => frames.push(stacktrace_item(module, ctx, func, pc)?),
        }
        // Then every frame suspended in a call, innermost first, each resolved in its
        // own module. Context `0` is the run's entry module, which the machine borrows;
        // any other id is a callee handle in the machine's own registry. A missing
        // handle for a non-zero id is practically unreachable — the registry only grows
        // within a run — so falling back to the passed entry module keeps the accessor
        // total instead of inventing an error.
        for (func, pc, context) in self.resumption_contexts() {
            let owner = match context {
                0 => None,
                id => self.module_context(id).map(|context| context.module.clone()),
            };
            match owner {
                Some(owner) => frames.push(stacktrace_item(&owner, ctx, func, pc)?),
                None => frames.push(stacktrace_item(module, ctx, func, pc)?),
            }
        }
        terms::make_list(ctx, &frames)
    }

    /// The finite `after T` deadline of the `receive` this machine is parked at — the
    /// engine half of the runtime's timeout rearming, which
    /// [`RunOutcome::Wait`](crate::RunOutcome::Wait) deliberately does not carry.
    ///
    /// A suspended process is parked at one of two instructions, and both are
    /// *interpreted* (a blocking instruction is never inlined by a native tier), so
    /// [`Machine::current_code`] is exact at the suspension:
    ///
    /// ```text
    /// ScanLabel:  loop_rec Fail, Dst
    ///             … match tests, loop_rec_end ScanLabel …
    /// Fail:       wait_timeout ScanLabel, Time   ; <-- current_code while parked
    ///             timeout                         ; <-- the timed-out continuation
    /// ```
    ///
    /// `Some(WaitDeadline { delay_ms, resume })` when the parked instruction **is** a
    /// `wait_timeout`; `delay_ms` is [`Some`] exactly when the delay is statically
    /// readable (see [`Machine::timeout_delay`]) and `resume` is the `timeout`
    /// continuation (`pc + 1`), which is where the host resumes a fired timer — the
    /// C rule (`emu/msg_instrs.tab:356-363`: "In case of timeout, control will be
    /// transferred to the `timeout` instruction following the `wait_timeout`").
    ///
    /// `None` when the machine is parked at a bare `wait` — the `after infinity` the
    /// compiler emits — or is not parked at all. A `wait_timeout` whose delay is not
    /// statically readable still yields `Some` (with `delay_ms: None` and a usable
    /// `resume`): the *absence* of a deadline is the honest answer, never a guessed
    /// one.
    ///
    /// `module` is the run's **entry** module; a machine parked inside a cross-module
    /// callee reads the callee's own instruction stream instead (see
    /// [`Machine::current_module`]).
    pub fn wait_deadline(&self, module: &Module) -> Option<WaitDeadline> {
        let (func, pc) = self.current_code?;
        match self.active_context() {
            Some(context) => deadline_at(self, &context.module, func, pc),
            None => deadline_at(self, module, func, pc),
        }
    }

    /// The positive millisecond delay a `wait_timeout`'s `Time` operand denotes, when
    /// it is statically readable — the delay half of [`Machine::wait_deadline`].
    ///
    /// The operand is a **decoded IR operand**, not a runtime term: the compiler emits
    /// [`Operand::Integer`] for a literal `after 20` (or [`Operand::Literal`] for a
    /// value the loader materialized), and an [`Operand::X`]/[`Operand::Y`] register
    /// for a runtime variable — the registers and every Y slot of the parked frame are
    /// untouched by the suspension, so the value the `wait_timeout` tested is still
    /// there. The Y case is why [`Machine::y`] is public.
    ///
    /// `None` for everything else — the atom `infinity`, a zero/negative or
    /// non-integer value, a float register, and any operand a `wait_timeout` cannot
    /// carry. `None` means "no deadline": the process parks until a message arrives,
    /// which is what an unreadable delay really is, rather than an invented one.
    pub fn timeout_delay(&self, module: &Module, time: &Operand) -> Option<u64> {
        match time {
            Operand::Integer(millis) => positive_millis(*millis),
            Operand::Literal(index) => module.literal(*index).and_then(millis_of),
            Operand::X(register) => self.x(*register).and_then(millis_of),
            Operand::Y(register) => self.y(*register).and_then(millis_of),
            _ => None,
        }
    }
}

/// The finite `after T` deadline of the `receive` a machine is parked at.
///
/// Produced by [`Machine::wait_deadline`]; see its docs for the parked shape this is
/// recovered from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitDeadline {
    /// The positive delay of the parked `wait_timeout` in milliseconds, or [`None`]
    /// when the delay is not statically readable (an `infinity`, zero/negative or
    /// non-integer value) — the absence of a deadline, never a guess.
    pub delay_ms: Option<u64>,
    /// The `(func, pc)` a **timeout** resumes at: the `timeout` opcode directly after
    /// the parked `wait_timeout`, which is where the host re-enters
    /// [`Machine::resume_outcome`] when its timer fires.
    pub resume: (u32, u32),
}

/// The deadline of the `wait_timeout` at `(func, pc)` of `module`, if that is really
/// what is there.
///
/// Separated from [`Machine::wait_deadline`] so the module the position belongs to can
/// be bound in a local: a cross-module callee's handle is an [`Arc`] the caller only
/// borrows for the call.
fn deadline_at(machine: &Machine, module: &Module, func: u32, pc: u32) -> Option<WaitDeadline> {
    let function = module.functions.get(func as usize)?;
    let Instr::WaitTimeout { time, .. } = function.code.get(pc as usize)? else {
        // A bare `wait` (`after infinity`) or any other parked instruction: the
        // process has no deadline at all.
        return None;
    };
    Some(WaitDeadline {
        delay_ms: machine.timeout_delay(module, time),
        resume: (func, pc + 1),
    })
}

/// `millis` as a delay, or [`None`] when it is not a positive one.
fn positive_millis(millis: i64) -> Option<u64> {
    (millis > 0).then_some(millis as u64)
}

/// The positive millisecond delay a timeout **term** denotes, or [`None`].
///
/// A small integer greater than zero is a delay; the atom `infinity`, a zero/negative
/// integer, and anything else are not (`infinity` means "never", which is the absence
/// of a deadline — the same `None`).
fn millis_of(term: Eterm) -> Option<u64> {
    if is_small(term) {
        return positive_millis(signed_value(term));
    }
    None
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
