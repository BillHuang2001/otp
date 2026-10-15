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

//! The baseline tier's **comparison-`test`** helper.
//!
//! A `test` whose predicate is a **comparison** (`is_eq_exact`/`is_ne_exact`/
//! `is_eq`/`is_ne`/`is_lt`/`is_ge`) cannot be lowered by the inlined word/header
//! subset (`emit/test.rs`) whenever either operand is a `Y` slot, a float register or
//! a literal-area term, or whenever both operands are boxed terms (a structural
//! comparison must not word-compare two pointers). Those are exactly the shapes a
//! `receive`/`case` clause head produces — a bound-variable match against a `Y` slot, or
//! against the matching literal (`{tr,{y,0},{t_map,…}}`) — so left to the generic
//! per-instruction deopt they each paid a whole interpreter step ([`jit_step_one`] →
//! [`Machine::step_instruction`](crate::Machine)) on every round of a message loop.
//!
//! [`jit_test`] evaluates the instruction **without** the interpreter's generic
//! dispatch: it resolves the instruction at `(func, pc)` in the active module and runs
//! the same two-line body `Machine::eval_test` runs for a comparison — read both
//! arguments through the shared [`read_operand`] and apply `beam_term`'s canonical
//! `cmp`/`cmp_arith` — so the semantics are the interpreter's **by construction**. It
//! reports the outcome as a boolean ([`TEST_TRUE`]/[`TEST_FALSE`]) and the emitted code
//! branches to the instruction's **fail label** in-block (a compiled fail block is
//! jumped to directly — see `emit/test.rs::emit_test_helper`); a propagating read error
//! becomes the block-ABI [`ERROR`] word.
//!
//! # Why a boolean and not a `Step` word
//!
//! A `Step`-shaped helper could only report the fail target as a `(func, pc)` word for
//! the **driver** to decode — that is the `jit_step_one` round trip this helper exists to
//! remove. Because a comparison `test`'s fail label is a compile-time constant of the
//! block's own module, the emitter resolves it and jumps to it natively, so the helper
//! only has to answer "did the test succeed?". Every other `test` shape — a type test,
//! a `NO_FAIL` form (which *raises* `function_clause`) — is deliberately **not** this
//! helper's: it falls back to [`jit_step_one`], which reproduces the interpreter's exact
//! behaviour (including the raise).
//!
//! # Exactness rules (binding)
//!
//! `is_eq_exact` is **exact** equality, never a word compare: an immediate is never
//! equal to a boxed term, two word-equal immediates *are* equal, and two structurally
//! equal **boxed** terms (maps, tuples, bignums, floats, interna pid/port/ref) may live
//! at different addresses and must compare by structure, not by pointer. The helper
//! therefore routes **every** case through `beam_term`'s comparison; it never
//! word-compares two boxed pointers. `is_eq`/`is_ne`/`is_lt`/`is_ge` are the arithmetic
//! (inexact) comparisons `cmp_arith` defines, mirroring `Machine::eval_test`.

use beam_common::Result;
use beam_loader::{Instr, Module, Operand, TestKind, NO_FAIL};
use beam_term::{cmp, cmp_arith, Eterm};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::read_operand;

use super::jit_step_one;
use super::runtime::JitRuntime;
use super::ERROR;

/// The helper's "the test **succeeded**" word: the emitted code refills the cached
/// x-registers and continues in-block at the next instruction.
pub(crate) const TEST_TRUE: i64 = 1;

/// The helper's "the test **failed**" word: the emitted code branches to the
/// instruction's resolved fail label (a compiled fail block is jumped to directly).
pub(crate) const TEST_FALSE: i64 = 0;

/// Evaluate the comparison `test` at `(func, pc)` natively and report its outcome as
/// [`TEST_TRUE`]/[`TEST_FALSE`] (or the block-ABI [`ERROR`] word of a propagating
/// raise).
///
/// The helper has the [`StepHelper`](super::runtime::StepHelper) signature, so the
/// emitter's `emit_test_helper` arm drives it through the shared
/// [`emit_helper_call`](super::emit::emit_helper_call) sequence — which spills the
/// cached x-registers first, so the shared [`read_operand`] reads the interpreter's
/// exact live `machine.x`.
///
/// Anything this helper does not own — the instruction at `(func, pc)` is not a
/// comparison `test`, carries a `NO_FAIL` fail code, or has an operand count other than
/// two — is handed to [`jit_step_one`], which reproduces the interpreter's exact
/// behaviour (including a `NO_FAIL` test's `function_clause` raise). The emitter only
/// emits the boolean protocol for the forms this helper handles, so that fallback is
/// unreachable for a lowered instruction; the emitted code nevertheless treats any
/// word other than the two booleans as a driver word (see `emit_test_helper`).
pub(crate) extern "C" fn jit_test(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer
    // it holds points at an object the driver owns and keeps alive for the whole
    // loop (`machine`, `module`), or at a local that outlives the loop.
    let runtime = unsafe { &mut *rt };
    // SAFETY: `runtime.machine` is a non-null pointer to the driver's live `Machine`;
    // no other `&mut Machine` exists while the helper runs (the driver is blocked in
    // the block call that reached us).
    let machine = unsafe { &mut *runtime.machine };
    // SAFETY: `runtime.module` points at the module the driver is running, which
    // outlives the loop.
    let module = unsafe { &*runtime.module };
    // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as
    // a raw pointer; the driver holds no `&mut` to it while the helper runs.
    let ctx = unsafe { &mut *runtime.ctx };

    // The same position store `Machine::step_instruction` makes at entry: a comparison
    // `test` is an exact raise position, so the machine's saved instruction pointer
    // must name this instruction, not the block.
    machine.set_current_code(func, pc);

    // The instruction this block emitted the helper for. An address outside the module
    // cannot be here (the block was compiled from this very code), so it is handed to
    // the interpreter's own step rather than invented — it reports the interpreter's
    // exact diagnostic for a bad `(func, pc)`.
    let instr = match module
        .functions
        .get(func as usize)
        .and_then(|function| function.code.get(pc as usize))
    {
        Some(instr) => instr,
        None => return jit_step_one(rt, func, pc),
    };

    // `None` = "not this helper's instruction": the interpreter's own step, unchanged.
    let Some(result) = evaluate_comparison(machine, module, instr, ctx) else {
        return jit_step_one(rt, func, pc);
    };

    match result {
        Ok(true) => TEST_TRUE,
        Ok(false) => TEST_FALSE,
        Err(err) => {
            // The uniform raise funnel's context-`0` half, exactly as
            // `Machine::step_instruction` runs it: in context `0` the active module IS
            // the entry module, so a propagating raise is decorated here; a non-zero
            // context is left to the driver, which knows the true entry module.
            let err = if machine.current_context() == 0 {
                machine.with_uncaught_frames(err, Some(module), func, pc)
            } else {
                err
            };
            runtime.error = Some(err);
            ERROR
        }
    }
}

/// Evaluate the two-operand comparison `test` `instr` names, or `None` when `instr` is
/// not a comparison `test` this helper owns.
///
/// The body is `Machine::eval_test`'s, for the six comparison kinds only: read the two
/// arguments in order through the shared [`read_operand`] (an argument read may
/// materialize a non-small `Integer`/`Character` as an exact bignum) and apply
/// `beam_term`'s canonical comparison against the host's atom namespace
/// ([`ExecutionContext::atoms`], the runtime's live shared table).
fn evaluate_comparison(
    machine: &Machine,
    module: &Module,
    instr: &Instr,
    ctx: &mut dyn ExecutionContext,
) -> Option<Result<bool>> {
    let Instr::Test { kind, fail, args } = instr else {
        return None;
    };
    // A `NO_FAIL` test *raises* `function_clause` (never a fail branch) and a shape with
    // operands other than two is not a comparison — both stay the interpreter's step.
    if *fail == NO_FAIL || args.len() != 2 {
        return None;
    }
    let result = match kind {
        TestKind::IsEqExact => {
            read_pair(machine, ctx, args).map(|(a, b)| cmp(a, b, ctx.atoms(module)).is_eq())
        }
        TestKind::IsNeExact => {
            read_pair(machine, ctx, args).map(|(a, b)| !cmp(a, b, ctx.atoms(module)).is_eq())
        }
        TestKind::IsEq => {
            read_pair(machine, ctx, args).map(|(a, b)| cmp_arith(a, b, ctx.atoms(module)).is_eq())
        }
        TestKind::IsNe => {
            read_pair(machine, ctx, args).map(|(a, b)| !cmp_arith(a, b, ctx.atoms(module)).is_eq())
        }
        TestKind::IsLt => {
            read_pair(machine, ctx, args).map(|(a, b)| cmp_arith(a, b, ctx.atoms(module)).is_lt())
        }
        TestKind::IsGe => {
            read_pair(machine, ctx, args).map(|(a, b)| cmp_arith(a, b, ctx.atoms(module)).is_ge())
        }
        _ => return None,
    };
    Some(result)
}

/// Read both operands of a comparison `test`, in order, through the shared
/// [`read_operand`] — the two reads `Machine::eval_test`'s `test_pair` performs, so a
/// failing first read is reported exactly as the interpreter reports it.
fn read_pair(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    args: &[Operand],
) -> Result<(Eterm, Eterm)> {
    let a = read_operand(machine, ctx, &args[0])?;
    let b = read_operand(machine, ctx, &args[1])?;
    Ok((a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two boolean words are distinct and neither is a block-ABI sentinel, so the
    /// emitted `TEST_TRUE`/`TEST_FALSE` compares can never be confused with an error.
    #[test]
    fn the_boolean_protocol_words_are_distinct() {
        assert_ne!(TEST_TRUE, TEST_FALSE);
        assert_ne!(TEST_TRUE, ERROR);
        assert_ne!(TEST_FALSE, ERROR);
    }

    /// Only the six comparison kinds with two operands and a real fail label are the
    /// helper's; a type test, a `NO_FAIL` form and a one-operand shape are declined
    /// (so `jit_step_one` keeps the interpreter's exact behaviour for them).
    #[test]
    fn only_the_comparison_kinds_are_owned() {
        let module = Module::default();
        let machine = Machine::new();
        let mut ctx = crate::context::MinimalContext::new();
        let mut own = |kind: TestKind, fail: beam_loader::Label, args: Vec<Operand>| {
            let instr = Instr::Test { kind, fail, args };
            evaluate_comparison(
                &machine,
                &module,
                &instr,
                &mut ctx as &mut dyn ExecutionContext,
            )
            .is_some()
        };
        let two = || vec![Operand::X(0), Operand::X(1)];
        // NB `NO_FAIL == Label(0)`, so a resolvable fail label must be a non-zero one.
        let fail = beam_loader::Label(1);
        assert!(own(TestKind::IsEqExact, fail, two()));
        assert!(own(TestKind::IsLt, fail, two()));
        assert!(!own(TestKind::IsEqExact, NO_FAIL, two()), "a NO_FAIL test raises");
        assert!(
            !own(TestKind::IsTuple, fail, vec![Operand::X(0)]),
            "a type test is not a comparison"
        );
        assert!(
            !own(TestKind::IsEqExact, fail, vec![Operand::X(0)]),
            "a one-operand shape is not a comparison"
        );
    }
}
