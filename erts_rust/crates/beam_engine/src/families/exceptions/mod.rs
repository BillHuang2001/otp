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

//! Exception / raise instruction family.
//!
//! Owns the in-VM exception instructions (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - [`Instr::Badmatch`] — raise `{badmatch,Value}`
//! - [`Instr::CaseEnd`] — raise `{case_clause,Value}`
//! - [`Instr::IfEnd`] — raise `if_clause`
//! - [`Instr::TryCaseEnd`] — raise `{try_clause,Value}`
//! - [`Instr::Badrecord`] — raise `{badrecord,Value}`
//! - [`Instr::Raise`] — re-raise with the class of the exception being handled
//! - [`Instr::Try`] / [`Instr::Catch`] — push a catch frame
//! - [`Instr::TryCase`] / [`Instr::TryEnd`] / [`Instr::CatchEnd`] — the handler
//!   opcodes (each consumes exactly one catch frame)
//! - [`Instr::BuildStacktrace`] — cook the raw stacktrace in `x(0)` into the
//!   symbolic `[{M,F,A,Location}]` form
//! - [`Instr::RawRaise`] — raise from `x(0)`=class, `x(1)`=value, `x(2)`=trace
//!
//! ## Semantics (C ground truth, `emu/instrs.tab:1137-1263`, `beam_common.c:735-778`)
//!
//! The raising opcodes set a pending exception and **expand the reason at raise
//! time** (`expand_error_value`): `badmatch` ⇒ `{badmatch,V}`, `case_end` ⇒
//! `{case_clause,V}`, `try_case_end` ⇒ `{try_clause,V}`, `badrecord` ⇒
//! `{badrecord,V}`, `if_end` ⇒ the bare atom `if_clause`. All are class `error`.
//! The raise then goes through the one uniform machine entry point
//! [`Machine::throw`]: it consults the innermost catch frame, unwinds the call
//! frames to the frame owning the handler and lands the exception in the handler
//! registers (`x(0) = THE_NON_VALUE`, `x(1) = reason`, `x(2) = trace`,
//! `x(3) = class atom`), yielding a `Step::Goto` to the handler. With no live
//! catch frame it propagates as `Err(BeamError::Raise(..))` — never a panic.
//!
//! The catch frame a `try`/`catch` pushed is **not** popped by the raise; the
//! handler opcode that runs next pops exactly one entry (the normal or the
//! exceptional path — never both). `try_case` blanks its frame slot, moves the
//! class atom from `x(3)` into `x(0)` and requires the `THE_NON_VALUE` marker in
//! `x(0)`; `try_end` just blonks its slot; `catch_end` leaves a normal result
//! untouched but folds a landed exception into the caught term — the thrown value
//! for class `throw`, `{'EXIT',Reason}` otherwise.
//!
//! The **class atom** (`error`/`exit`/`throw`) is installed from the running
//! module's atom table before a raise ([`install_class_atom`]) so a landing can
//! place it in `x(3)`. An atom absent from the module atom table is never
//! substituted by a *different* atom: an uncaught raise still propagates as a
//! typed error, and a caught raise fails with a typed `unsupported`.
//!
//! **Milestone-1 limits (all documented, never a wrong result):**
//! - No stacktrace is captured, so `catch_end` builds `{'EXIT',Reason}` without
//!   the stacktrace element the real runtime appends to the reason (C's
//!   `add_stacktrace`); a `catch` whose handler would need a stacktrace only ever
//!   observes a shorter (but otherwise identical) reason term.
//! - `catch_end` cannot build `{'EXIT',V}` in a module whose atom table lacks
//!   `'EXIT'` (the loader exposes only the module's atoms); it returns a typed
//!   `unsupported` rather than substituting a wrong atom.
//! - [`Instr::BuildStacktrace`] cooks a **NIL** raw stacktrace to `NIL` exactly as
//!   C does (a caught exception lands with `x(2) = NIL` because the machine
//!   captures no stacktrace), and approximates a non-NIL one from the machine's
//!   **live call frames** — the engine keeps no raw `StackTrace` object. See the
//!   `symbolic_stacktrace` body for the precise residuals.
//! - [`Instr::RawRaise`] reproduces C exactly for the three real classes and for
//!   the "unknown class" fall-through; the fall-through needs the atom `badarg` in
//!   the module's atom table (a module whose atom table lacks it is a typed
//!   error, never a substituted atom).
//! - The re-raise opcode [`Instr::Raise`] requires a class to be currently
//!   handled ([`Machine::pending_class`]); the compiler emits it only as the
//!   "no catch clause matched" tail of a handler, where that always holds
//!   (`lib/compiler/src/beam_ssa_*.erl` `resume` ⇒ `{bif,raise,..}`).

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Instr, Module, Operand};
use beam_term::{is_nil, make_atom, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::{CatchFrame, Machine};
use crate::operand::{read_operand, write_operand};
use crate::terms;
use crate::Step;

use raise::{classify_class, install_class_atom, raise_error_atom, raise_error_tuple};
use stacktrace::symbolic_stacktrace;

mod raise;
pub(crate) mod stacktrace;

#[cfg(test)]
mod tests;

/// Execute an exception-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is an exception opcode this family
/// executes, `Ok(None)` when `instr` is not this family's variant, and `Err` on a
/// raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    match instr {
        // `try Frame Fail` / `catch Frame Fail` (`catch` is a macro for `try`):
        // push the handler's catch frame, blank the frame slot (`catches++`, `Y =
        // Fail` in C) and fall through.
        Instr::Try { frame, fail } | Instr::Catch { frame, fail } => {
            let (handler_func, handler_pc) = labels.resolve(*fail)?;
            // The handler lives in the frame that is current *now* (an unwind in
            // `Machine::throw` drops every deeper frame). `saturating_sub` keeps a
            // frame-less machine a typed error rather than a panic.
            let frame_index = machine.frame_depth().saturating_sub(1);
            machine.push_catch_frame(CatchFrame::new(handler_func, handler_pc, frame_index));
            write_operand(machine, frame, NIL)?;
            Ok(Some(Step::Next))
        }

        // `badmatch Src` ⇒ `{badmatch,Src}` (class error).
        Instr::Badmatch { value } => raise_error_tuple(machine, module, ctx, "badmatch", value),
        // `case_end Src` ⇒ `{case_clause,Src}` (class error).
        Instr::CaseEnd { value } => raise_error_tuple(machine, module, ctx, "case_clause", value),
        // `try_case_end Src` ⇒ `{try_clause,Src}` (class error).
        Instr::TryCaseEnd { value } => raise_error_tuple(machine, module, ctx, "try_clause", value),
        // `badrecord Src` ⇒ `{badrecord,Src}` (class error).
        Instr::Badrecord { value } => raise_error_tuple(machine, module, ctx, "badrecord", value),
        // `if_end` ⇒ the bare atom `if_clause` (class error).
        Instr::IfEnd => raise_error_atom(machine, module, "if_clause"),

        // `raise Trace Value` — the re-raise tail of a handler (`{bif,raise,...}`).
        // C's `i_raise` copies the reason + raw stacktrace but never touches
        // `freason`, so the class of the exception currently being handled is kept.
        Instr::Raise { trace, value } => {
            let value = read_operand(machine, ctx, value)?;
            let trace = read_operand(machine, ctx, trace)?;
            let class = machine.pending_class().ok_or_else(|| {
                error::unsupported("exceptions: raise with no exception being handled")
            })?;
            let _ = install_class_atom(machine, module, class);
            machine.throw_with_trace(class, value, trace).map(Some)
        }

        // `try_case Frame` — the handler entry of a `try`. C:
        // `catch--; make_blank(Y); assert(is_non_value(x(0))); x(0) = x(3)`.
        Instr::TryCase { frame } => {
            machine.pop_catch_frame_checked("try_case: unbalanced catch frames")?;
            write_operand(machine, frame, NIL)?;
            if machine.x(0) != Some(Eterm::non_value()) {
                return Err(error::unsupported(
                    "try_case: x(0) is not THE_NON_VALUE (no exception landed)",
                ));
            }
            let class = machine
                .x(3)
                .filter(|term| !is_nil(*term))
                .ok_or_else(|| error::unsupported("try_case: x(3) holds no exception class"))?;
            machine.set_x(0, class)?;
            Ok(Some(Step::Next))
        }

        // `try_end Frame` — the normal-exit of a `try`: `catch--; make_blank(Y)`.
        Instr::TryEnd { frame } => {
            machine.pop_catch_frame_checked("try_end: unbalanced catch frames")?;
            write_operand(machine, frame, NIL)?;
            Ok(Some(Step::Next))
        }

        // `catch_end Frame` — fold the old-style `catch`: a normal result passes
        // through unchanged; a landed exception becomes the thrown value (class
        // `throw`) or `{'EXIT',Reason}` (classes `error`/`exit`).
        Instr::CatchEnd { frame } => {
            machine.pop_catch_frame_checked("catch_end: unbalanced catch frames")?;
            write_operand(machine, frame, NIL)?;
            if machine.x(0) == Some(Eterm::non_value()) {
                let class = machine
                    .x(3)
                    .ok_or_else(|| error::unsupported("catch_end: x(3) out of range"))?;
                let reason = machine
                    .x(1)
                    .ok_or_else(|| error::unsupported("catch_end: x(1) out of range"))?;
                let throw_atom = module.atom_table.lookup("throw").map(make_atom);
                if throw_atom == Some(class) {
                    // Class `throw`: the caught value is the thrown term itself.
                    machine.set_x(0, reason)?;
                } else {
                    // Classes `error`/`exit` ⇒ `{'EXIT',Reason}`. M1 appends no
                    // stacktrace (C's `add_stacktrace`).
                    let exit_atom = module.atom_table.lookup("EXIT").ok_or_else(|| {
                        error::unsupported("catch_end: module atom table lacks the 'EXIT' atom")
                    })?;
                    let tuple = terms::make_tuple(ctx, &[make_atom(exit_atom), reason])?;
                    machine.set_x(0, tuple)?;
                }
            }
            Ok(Some(Step::Next))
        }

        // `build_stacktrace` — cook the raw stacktrace in `x(0)` into the
        // symbolic `[{M,F,A,Location}]` list (C:
        // `x(0) = build_stacktrace(c_p, x(0))`, `emu/instrs.tab:1247-1251`).
        Instr::BuildStacktrace => {
            let raw = read_operand(machine, ctx, &Operand::X(0))?;
            // C's `get_trace_from_exc/1` returns NULL for a NIL exception, and
            // `build_stacktrace` then yields NIL (`beam_common.c:1219-1223`). The
            // M1 machine captures no stacktrace, so every caught exception lands
            // with `x(2) = NIL` (`Machine::land_exception`) and the compiler's
            // handler moves that into `x(0)` — the engine therefore reproduces the
            // oracle's `[]` exactly on the compiled path. A non-NIL raw stacktrace
            // (which no compiler-emitted path produces at M1) is approximated from
            // the machine's live call frames.
            let cooked = if is_nil(raw) {
                NIL
            } else {
                symbolic_stacktrace(machine, module, ctx, func, pc)?
            };
            machine.set_x(0, cooked)?;
            Ok(Some(Step::Next))
        }

        // `raw_raise` — raise from the register protocol (`emu/instrs.tab:1253-1263`,
        // `beam_common.c:1148-1179`): x(0) = the class atom, x(1) = the value,
        // x(2) = the raw stacktrace. `raw_raise` is what `sys_core_fold` folds the
        // `catch C:R:S -> erlang:raise(C, R, S)` shape into
        // (`lib/compiler/src/sys_core_fold.erl:2441-2465`).
        Instr::RawRaise => {
            let class_term = read_operand(machine, ctx, &Operand::X(0))?;
            let value = read_operand(machine, ctx, &Operand::X(1))?;
            let trace = read_operand(machine, ctx, &Operand::X(2))?;
            match classify_class(module, class_term) {
                Some(class) => {
                    // The class atom is exactly the one we matched, so it is
                    // present in the module atom table and the landing can install
                    // it in `x(3)` (C stores the class in the `StackTrace` struct
                    // *and* in `c_p->freason`, `beam_common.c:1151-1175`).
                    install_class_atom(machine, module, class)?;
                    machine.throw_with_trace(class, value, trace).map(Some)
                }
                None => {
                    // C: an unknown class is not an error — `raw_raise` returns 1,
                    // the instruction stores `am_badarg` in `x(0)` and execution
                    // continues with the next instruction (no raise,
                    // `beam_common.c:1176-1178`).
                    let badarg = module.atom_table.lookup("badarg").ok_or_else(|| {
                        error::unsupported("raw_raise: module atom table lacks the 'badarg' atom")
                    })?;
                    machine.set_x(0, make_atom(badarg))?;
                    Ok(Some(Step::Next))
                }
            }
        }

        // Not this family's variant.
        _ => Ok(None),
    }
}
