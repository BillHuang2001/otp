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

//! Floating-point instruction family.
//!
//! Owns the float-register (FR) instructions (per `../beam_loader/CONTEXT.md`,
//! "The instruction vocabulary"):
//! [`Instr::Fmove`], [`Instr::Fconv`], [`Instr::Fadd`], [`Instr::Fsub`],
//! [`Instr::Fmul`], [`Instr::Fdiv`] and [`Instr::Fnegate`].
//!
//! The work is split by responsibility: this file holds [`exec`] (the family's
//! dispatch), the shared operand seam ([`read_value`]) and the shared
//! failure/raise semantics ([`arithmetic_failure`], [`raise_badarith`]);
//! `arith.rs` holds the four binary arithmetic bodies plus `fnegate`;
//! `convert.rs` holds the `fmove`/`fconv` conversions; `tests` is the
//! per-variant unit suite.
//!
//! # Ground truth (`../../erts/emulator/beam/emu/float_instrs.tab`)
//!
//! | IR variant | C body | semantics |
//! |---|---|---|
//! | `Fmove`   | `fload`/`fstore` (`emu/ops.tab:1337-1338`) | copy a float between an FR slot and a value register / literal |
//! | `Fconv`   | `i_fconv` (`:38-52`) | small int / float / bignum (faithful `big_to_double`) → FR; otherwise `badarith` (`$BADARITH0`) |
//! | `Fadd`/`Fsub`/`Fmul`/`Fdiv` | `i_f{add,sub,mul,div}` (`:62-76`) | `Dst = Src1 op Src2`; a **non-finite** result is `badarith` |
//! | `Fnegate` | `i_fnegate` (`:78-84`) | `Dst = -Src`; flipping a finite float's sign stays finite, so no finiteness check |
//!
//! # Representation (the M1 FR-file substitution)
//!
//! The C float-register file is raw doubles (`FloatDef f_reg_array[MAX_REG]`). At
//! Milestone 1 the engine's FR file stores **`Eterm` term slots** (see
//! [`Machine::get_fr`]/[`Machine::set_fr`] and `crate::machine`), so an FR slot
//! holds a *boxed float term*. Every read therefore converts the slot to `f64`
//! with [`crate::terms::float_value`] and every write stores a boxed float built by
//! [`crate::terms::make_float`]. For a genuine float this is observably identical to
//! the raw-double model: `fload`ing then `fstore`ing a value yields a float term the
//! C `fstore` would `HAlloc` the same way.
//!
//! # Errors (never a panic, never a wrong result)
//!
//! * A **non-float operand** or a **non-finite result** (including division by
//!   zero — `x/0.0` is `inf` or, for `0.0/0.0`, `NaN`) is the instruction's
//!   *arithmetic failure*: with a live `fail` label it becomes a guard branch
//!   ([`Step::Goto`]); with `NO_FAIL` it is the `badarith` **raise**. C's
//!   `$BADARITH0` (`emu/macros.tab:285-288`) sets `c_p->freason = BADARITH` and
//!   goes `goto find_func_info` → `handle_error` → `next_catch`
//!   (`../../erts/emulator/beam/beam_common.c`), so a live `try`/`catch`
//!   **catches it**. The engine routes it through [`Machine::throw_error`] (see
//!   [`raise_badarith`]), landing in a live catch frame or propagating a typed
//!   [`BeamError::BadArith`]. The compiler only ever emits `NO_FAIL` for these
//!   opcodes (verified with `beam_disasm` against the oracle), so the live-label
//!   branch is engine-side robustness, exactly like `GcBif`.
//! * `Fconv` carries **no** fail label: a non-numeric source, and a bignum whose
//!   magnitude is too large for a finite double, both raise `badarith` through the
//!   same catch machinery.
//! * `Fconv` of a *bignum* source is the faithful `big_to_double` conversion
//!   (`i_fconv` → `../../erts/emulator/beam/big.c:2412`, via
//!   [`beam_term::number::big_to_f64`]): the nearest double, ties to even, exactly
//!   as the oracle computes it. A bignum that is not a finite double
//!   (`bitlen > 1024`, or one rounding up to `2^1024`) is C's `< 0` failure, so it
//!   is the `badarith` raise — never a silently wrong float.
//! * An operand the shared seam cannot evaluate (out-of-range register, unsupported
//!   operand kind) propagates unchanged — that is a malformed/unsupported program,
//!   not a value error, and must not be masked as `badarith`.

mod arith;
mod convert;
#[cfg(test)]
mod tests;

use crate::interpreter::LabelMap;

use beam_common::{BeamError, ExceptionClass, Result};
use beam_loader::{Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{make_atom, Eterm};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::read_operand;
use crate::Step;

/// Execute a float-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a float opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    // No instruction here stores a new (func, pc) of its own: control flow moves
    // only through the returned `Step` (the `fail` jump), so the loop's current
    // position is unused.
    let _ = (func, pc);

    match instr {
        // `fmove Src Dst` — FR ⇄ value register (and a float *literal* source).
        // Both directions are a single copy in the term-slot FR model.
        Instr::Fmove { src, dst } => convert::fmove(machine, module, ctx, src, dst),
        // `fconv Src Dst` — small int / float / bignum → float register. No fail label.
        Instr::Fconv { src, dst } => convert::fconv(machine, module, ctx, src, dst),
        Instr::Fadd {
            fail,
            lhs,
            rhs,
            dst,
        } => arith::binary(
            machine,
            module,
            labels,
            *fail,
            lhs,
            rhs,
            dst,
            ctx,
            |a, b| a + b,
        ),
        Instr::Fsub {
            fail,
            lhs,
            rhs,
            dst,
        } => arith::binary(
            machine,
            module,
            labels,
            *fail,
            lhs,
            rhs,
            dst,
            ctx,
            |a, b| a - b,
        ),
        Instr::Fmul {
            fail,
            lhs,
            rhs,
            dst,
        } => arith::binary(
            machine,
            module,
            labels,
            *fail,
            lhs,
            rhs,
            dst,
            ctx,
            |a, b| a * b,
        ),
        Instr::Fdiv {
            fail,
            lhs,
            rhs,
            dst,
        } => arith::binary(
            machine,
            module,
            labels,
            *fail,
            lhs,
            rhs,
            dst,
            ctx,
            |a, b| a / b,
        ),
        // `fnegate Fail Src Dst` — `Dst = -Src` (see [`arith::negate`]).
        Instr::Fnegate { fail, src, dst } => {
            arith::negate(machine, module, labels, *fail, src, dst, ctx)
        }
        _ => Ok(None),
    }
}

/// The arithmetic-failure outcome: the `fail`-label branch, or the `badarith`
/// raise.
///
/// Mirrors `FAIL_HEAD_OR_BODY`/`$BADARITH0` (`emu/macros.tab:266-288`): a live label
/// makes the failure a guard failure (`Step::Goto`); `NO_FAIL` makes it the
/// `badarith` **raise**, which C reaches through `goto find_func_info` →
/// `handle_error` → `next_catch` and so a live `try`/`catch` catches. The engine's
/// equivalent entry point is [`raise_badarith`].
fn arithmetic_failure(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
) -> Result<Option<Step>> {
    if fail == NO_FAIL {
        return raise_badarith(machine, module).map(Some);
    }
    let (func, pc) = labels.resolve(fail)?;
    Ok(Some(Step::Goto { func, pc }))
}

/// Raise the `badarith` semantic error through the machine, landing it in a live
/// catch frame (or a typed [`BeamError::BadArith`] when there is none).
///
/// C's `$BADARITH0()` (`emu/macros.tab:285-288`) sets `c_p->freason = BADARITH`
/// and goes `goto find_func_info`, i.e. through `handle_error` → `next_catch`
/// (`../../erts/emulator/beam/beam_common.c:580-679`), so the raise **is
/// catchable** by an enclosing `try`/`catch`. The engine's raise/unwind entry
/// point is [`Machine::throw_error`], with the reason atom `badarith` resolved
/// from the running module's atom table (the engine never invents an atom the
/// module cannot name).
///
/// A module whose atom table lacks `badarith` cannot name the reason, so the typed
/// [`BeamError::BadArith`] is returned instead — never a substituted, wrong atom.
/// With **no live catch frame** the raise cannot land, so it likewise propagates
/// unchanged as the typed [`BeamError::BadArith`] (the same uncaught-error contract
/// the interpreter's `raise_bif_error` follows). The `error` class atom is installed
/// when the module has it, which is what a landing places in `x(3)`.
fn raise_badarith(machine: &mut Machine, module: &Module) -> Result<Step> {
    if machine.catch_frame_depth() == 0 {
        return Err(BeamError::BadArith);
    }
    let atom = module
        .atom_table
        .lookup("badarith")
        .ok_or(BeamError::BadArith)?;
    if let Some(class_atom) = module.atom_table.lookup(ExceptionClass::Error.as_str()) {
        machine.set_class_atom(ExceptionClass::Error, make_atom(class_atom));
    }
    machine.throw_error(make_atom(atom))
}

/// Evaluate a float instruction's operand.
///
/// Everything but a static literal goes through the shared operand seam
/// ([`read_operand`]). A bare `Operand::Literal(index)` is the float family's own
/// `fmove {float,K} {fr,N}` source — the loader's IR documents `Fmove`'s source as
/// "a float register, value register **or float literal**", and the value lives in
/// the module's decoded literal area, which the shared seam does not resolve.
fn read_value(
    machine: &Machine,
    module: &Module,
    ctx: &mut dyn ExecutionContext,
    operand: &Operand,
) -> Result<Eterm> {
    match operand {
        Operand::Literal(index) => module
            .literal(*index)
            .ok_or_else(|| error::unsupported("float: literal index out of range")),
        other => read_operand(machine, ctx, other),
    }
}
