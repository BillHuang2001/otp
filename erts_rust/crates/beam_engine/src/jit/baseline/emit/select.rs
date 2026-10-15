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

//! The baseline JIT's exact-value switch (`select_val`) subset: the plan shape test
//! and its lowering (the section comment below documents exactly which switches the
//! tier may lower natively).

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Operand, NO_FAIL};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel};

use super::link::Linker;
use super::operands::constant_word;
use super::test::{load_test_src, test_src, TestSrc, TEST_A, TEST_C, TEST_T};
use super::{emit_step_and_continue, Allocation};

// ---------------------------------------------------------------------------
// The exact-value switch (`select_val`)
// ---------------------------------------------------------------------------
//
// `select_val Value Fail {Value,Label}*` decides every compiled `case`/`receive`
// arm whose patterns are literal constants — one of the most frequently executed
// branching instructions after `test`. The interpreter
// (`crate::interpreter`'s `Instr::SelectVal` arm) scans the alternatives in order
// and takes the first whose candidate is **exactly** (`=:=`) equal to the scrutinee
// (`cmp(a, b, ..).is_eq()`), falling through to `Fail` when none match (`NO_FAIL`
// raises).
//
// The subset below is lowered natively and mirrors that scan branch for branch:
//
// * the scrutinee must be a value the tier can read word-exactly — an x-register
//   inside `MAX_REGISTER`, an **addressable `Y` slot** (read through the guarded
//   current-frame window, exactly like the arithmetic fast path's `Y` operand), or an
//   immediate constant ([`TestSrc`]);
// * **every** alternative must be an immediate constant — a small int (in the
//   60-bit range), an atom, or `nil` — because `=:=` over immediates *is* a plain
//   raw-word compare: distinct immediate classes have disjoint words (a small int's
//   low nibble is `0xF`, an atom's low six bits are `0xB`, `nil` is `0x3B`), a boxed
//   or list pointer can never collide with an immediate (its low bits are even), and
//   `cmp` is the *exact* ordering (so a float/atom/bignum scrutinee is not `=:=` to
//   an immediate unless the words coincide). A float, bignum, literal or register
//   alternative deopts — a word compare would be wrong for those;
// * the scrutinee and every alternative's target — the fail label included — must
//   resolve (an unresolved label keeps the interpreter's exact `unknown_label`).
//
// A `NO_FAIL` switch raises on the no-match path (the helper does that exactly) and
// deopts, as does any shape above the subset. The emitted code is a chain of
// `cmp`/`jcc` — each match jumps to a small stub that spills the cached x-registers
// and returns that alternative's `(func, pc)` — with the no-match path falling
// through to the fail target's `(func, pc)`.

/// The inlinable shape of a `select_val`, or `None` when the whole instruction must
/// stay on the helper.
pub(super) struct SelectPlan {
    /// The scrutinee's source.
    pub(super) scrutinee: TestSrc,
    /// The `(constant word, resolved target)` alternatives, in source order.
    cases: Vec<(i64, (u32, u32))>,
    /// The resolved fail label's `(func, pc)`.
    fail: (u32, u32),
}

impl SelectPlan {
    /// Whether the scrutinee is read from an addressable `Y` slot.
    pub(super) fn uses_y(&self) -> bool {
        self.scrutinee.is_y()
    }
}

/// The inlinable shape of a `select_val`, or `None` for any shape above the subset
/// (a `NO_FAIL` switch, a non-immediate alternative, an unresolved label, a
/// scrutinee the tier cannot read word-exactly). See the section comment above.
pub(super) fn select_val_plan(
    labels: &LabelMap,
    value: &Operand,
    fail: Label,
    pairs: &[(Operand, Label)],
) -> Option<SelectPlan> {
    // A `NO_FAIL` switch *raises* when no alternative matches; the helper is exact.
    if fail == NO_FAIL {
        return None;
    }
    // An unresolved label keeps the interpreter's exact `unknown_label`.
    let fail = labels.resolve(fail).ok()?;
    // The scrutinee must be an x-register in bound, or an immediate constant.
    let scrutinee = test_src(value)?;
    // Every alternative must be an immediate constant with a resolved target.
    let mut cases = Vec::with_capacity(pairs.len());
    for (candidate, label) in pairs {
        let word = constant_word(candidate)?;
        let target = labels.resolve(*label).ok()?;
        cases.push((word, target));
    }
    Some(SelectPlan {
        scrutinee,
        cases,
        fail,
    })
}

/// Lower an inlinable `select_val` natively, or report that the whole instruction
/// must be handed to the helper.
///
/// Returns `false` **without having emitted a byte** for any shape
/// [`select_val_plan`] refuses, so the caller falls back to exactly today's
/// `jit_step_one`. On success the emitted code **always** transfers control (every
/// match and the no-match path return a `(func, pc)` word), except on the `Y`
/// scrutinee's guard-rejection path, which runs one interpreter step — and since the
/// interpreted instruction itself always transfers control, that step too leaves the
/// block with a word in `rax` (never an in-block continuation), so the caller ends the
/// block there.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_select_val(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    instr: &Instr,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Instr::SelectVal { value, fail, pairs } = instr else {
        return Ok(false);
    };
    let Some(plan) = select_val_plan(labels, value, *fail, pairs) else {
        return Ok(false);
    };

    // The `Y` scrutinee's frame-window rejection label: a slot outside the published
    // window (or no live frame) jumps here **before anything is read**, and the block
    // spends exactly one interpreter step on the instruction at `(func, pc)`.
    let deopt: Option<CodeLabel> = plan.uses_y().then(|| buffer.label());

    // --- the scrutinee ------------------------------------------------------
    load_test_src(
        buffer,
        plan.scrutinee,
        TEST_A,
        TEST_C,
        deopt,
        allocation,
        highest_touched,
    )?;

    // --- one `cmp`/`jcc` per alternative (jump to the match's stub) ---------
    let case_labels: Vec<CodeLabel> = (0..plan.cases.len()).map(|_| buffer.label()).collect();
    for ((word, _), case_label) in plan.cases.iter().zip(&case_labels) {
        buffer.mov_ri(TEST_T, *word)?;
        buffer.cmp_rr(TEST_A, TEST_T)?;
        buffer.jcc(Cond::E, *case_label)?;
    }

    // --- no alternative matched: branch to the fail label -------------------
    linker.emit_exit(buffer, allocation, done, plan.fail.0, plan.fail.1)?;

    // --- each alternative: branch to its target ----------------------------
    for ((_, target), case_label) in plan.cases.iter().zip(&case_labels) {
        buffer.bind(*case_label)?;
        linker.emit_exit(buffer, allocation, done, target.0, target.1)?;
    }

    // --- the `Y` guard rejected the scrutinee: run the real instruction -----
    if let Some(deopt) = deopt {
        buffer.bind(deopt)?;
        emit_step_and_continue(buffer, done, allocation, func, pc)?;
    }
    Ok(true)
}
