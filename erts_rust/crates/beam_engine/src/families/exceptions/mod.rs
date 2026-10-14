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
//! - No raw `StackTrace` object is **landed** as `x(2)` (the engine keeps no such
//!   object — C's raw trace is an opaque array of saved code pointers, `c_p->ftrace`),
//!   so a landed exception carries `x(2) = NIL`. A caught `try ... catch C:R:S -> S`
//!   nevertheless cooks `S` from the machine's **raise-time snapshot** — the frames
//!   the raise unwound out of, the raising function first then every live caller
//!   innermost-first (see [`Instr::BuildStacktrace`] below). The oracle's own cooked
//!   `S` additionally holds host `init:*`/`erl_eval` frames that live *outside* this
//!   engine's run, so the engine's list is always a strictly-**shorter prefix** of
//!   the oracle's — the engine's frames first, never a different value. `catch_end`
//!   is unaffected: it builds `{'EXIT',Reason}` from `x(1)` with no stacktrace
//!   element (C's `add_stacktrace`), a shorter but otherwise identical reason term.
//! - `catch_end` cannot build `{'EXIT',V}` in a module whose atom table lacks
//!   `'EXIT'` (the loader exposes only the module's atoms); it returns a typed
//!   `unsupported` rather than substituting a wrong atom.
//! - [`Instr::BuildStacktrace`] cooks the machine's **raise-time snapshot**
//!   ([`Machine::raise_snapshot`]) — the pre-unwind frame chain, each frame
//!   resolved in its **own** module context, with a frame whose context cannot be
//!   resolved skipped (and an empty/wholly-skipped snapshot cooking to the empty
//!   list) — whenever a snapshot is live. A `NIL` raw trace with **no** recorded
//!   raise is the one shape that cooks to `NIL`, exactly C's
//!   `get_trace_from_exc(NIL) == NULL`. See the `symbolic_stacktrace` body for the
//!   precise residuals.
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

use crate::context::{ExecutionContext, RootSet};
use crate::error;
use crate::machine::{CatchFrame, Machine};
use crate::operand::{read_operand, write_operand};
use crate::terms;
use crate::Step;

use raise::{classify_class, install_class_atom, raise_error_atom, raise_error_tuple};
use stacktrace::symbolic_stacktrace;

mod raise;
pub(crate) mod callee;
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
    // The family's opcodes are position-independent: each handler opcode is
    // addressed by its own operand/register state, and `build_stacktrace` cooks the
    // machine's **raise-time snapshot** (not the dispatch loop's current `(func,
    // pc)`). The `(func, pc)` parameters stay in the signature because it is the
    // frozen family-router contract.
    let _ = (func, pc);
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
                    let exit_term = make_atom(exit_atom);
                    // Reserve → read → write: the reason is re-read from the machine
                    // root `x(1)` *inside* the reader closure, so a moving collector
                    // running under the one rooted reservation cannot leave this build
                    // holding the stale copy read above.
                    let tuple = terms::make_tuple_rooted(
                        machine,
                        ctx,
                        RootSet::ALL_LIVE,
                        2,
                        0,
                        |m, _slack| {
                            let reason = m.x(1).ok_or_else(|| {
                                error::unsupported("catch_end: x(1) out of range")
                            })?;
                            Ok(vec![exit_term, reason])
                        },
                    )?;
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
            // C's `get_trace_from_exc/1` returns NULL only for a NIL exception
            // (`beam_common.c:1219-1223`), and the compiled path lands every caught
            // exception with `x(2) = NIL` (the engine keeps no raw `StackTrace`
            // object — see the module doc) which the compiler's handler moves into
            // `x(0)`. The engine nevertheless DID capture the exception's frames: the
            // raise rebuilt `Machine::raise_snapshot` (the pre-unwind chain, the
            // raising frame first then every live caller innermost-first) before it
            // unwound. The compiler reaches `build_stacktrace` **only** from a
            // `try`/`catch` handler clause that just landed an exception (it is
            // emitted at the head of a handler clause whose `S` is used,
            // `v3_core.erl:1515-1532`), so a **live** snapshot is exactly this
            // exception's own chain — cook it instead of the oracle-visible `[]`.
            // Only a machine with **no** recorded raise (an empty snapshot, e.g. a
            // handler whose snapshot the run never set) still cooks to NIL, C's
            // `get_trace_from_exc(NIL) == NULL` answer; a non-NIL raw is cooked as
            // before.
            let has_snapshot = !machine.raise_snapshot().is_empty();
            // An `undef` raised by an unresolvable `call_ext*`/`apply` target carries
            // a synthesized **callee head item** as its raw trace (see
            // [`callee`]): C's innermost `build_stacktrace` item, the callee named
            // with its **argument list**. Consume the latch here — it describes
            // exactly this one cooking — and hand it (with the operand the head word
            // must be re-read through, never a pre-computed term that could have
            // moved) to the trace builder.
            let head = machine.take_raise_head_item();
            let cooked = if is_nil(raw) && !has_snapshot && !head {
                NIL
            } else {
                symbolic_stacktrace(machine, module, ctx, &Operand::X(0), head)?
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
