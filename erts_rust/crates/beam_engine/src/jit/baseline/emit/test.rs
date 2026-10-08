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

//! The baseline JIT's word-level `test` subset: the guard/plan shape test and its
//! lowering (the section comment below documents exactly which predicates the tier
//! may lower natively).

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module, Operand, TestKind, NO_FAIL};
use beam_term::{
    make_atom, NIL, TAG_IMMED2_ATOM, TAG_IMMED2_MASK, TAG_PRIMARY_LIST, TAG_PRIMARY_MASK,
};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};
use crate::machine::MAX_REGISTER;

use super::arith::{FAST_C, SMALL_MASK};
use super::link::Linker;
use super::operands::{small_word, x_offset, x_register};
use super::{emit_step_and_continue, touch, Allocation};

// ---------------------------------------------------------------------------
// The word-level `test` subset
// ---------------------------------------------------------------------------
//
// `test` decides every clause head, guard and `case` arm, so leaving it to
// `jit_step_one` costs a full helper round-trip for the most frequently executed
// non-arithmetic instruction. The subset below is lowered natively — and only where
// a word-level check is **provably** the interpreter's result (`eval_test` in
// `crate::interpreter`), mirroring the rule set the Cranelift tier uses
// (`../cranelift.rs::inline_test`) so the two native tiers agree:
//
// * the pure word tests `is_nil`/`is_atom`/`is_list`/`is_nonempty_list`/`is_boolean`,
//   which never dereference a term and mirror `beam_term`'s const predicates:
//   `is_nil` is `word == NIL`; `is_atom` is `(word & 0x3F) == 0xB`; and the list
//   tests are the C bit trick `(word & (_TAG_PRIMARY_MASK - TAG_PRIMARY_LIST)) == 0`
//   (`is_list` additionally accepts `NIL`);
// * `is_eq_exact`/`is_ne_exact` with a constant-immediate operand — an immediate is
//   `=:=` only to the identical word, so a plain word compare is exact. Two
//   registers are **not** inlined: two distinct boxed terms (equal floats, bignums,
//   binaries, tuples, …) are `=:=` with different words;
// * `is_eq`/`is_ne` — `==` is the one comparison where immediates are not
//   self-sufficient (it equates `1` and `1.0`), so a **small-int** operand's register
//   counterpart is guarded as a small int first. An atom/`nil` operand is never
//   arithmetically equal to anything but itself, so it needs no guard;
// * `is_lt`/`is_ge` — only two small-int words are safe, because the tagged-word
//   *signed* order is the value order (`make_small(v) == v*16 + 15` is monotonic)
//   while `cmp_arith` otherwise falls back to the term classes; an atom/`nil`
//   constant is therefore never inlined for these two.
//
// A guard rejection branches to **one** `jit_step_one` call that then continues
// ([`emit_step_and_continue`]), so a float/bignum/atom operand costs exactly what it
// did before. A test failure branches to the resolved `fail` label's `(func, pc)` —
// a `(spill, return-word, exit)` — while a `NO_FAIL` test (which *raises*) and an
// unresolved label (whose exact `unknown_label` must surface) keep the whole
// instruction on the helper.

/// The scratch register holding a `test`'s first operand while its predicate is
/// evaluated.
///
/// All three are caller-saved (System V AMD64), so clobbering them within one
/// instruction's lowering is free, and none is a cache register ([`super::CACHE_REGISTERS`])
/// or the block's reserved `rbx` (the `machine.x` base) / `rsp` (the `rt` slot).
pub(super) const TEST_A: Reg = Reg::Rax;
/// The second operand's scratch register.
const TEST_B: Reg = Reg::R8;
/// The predicate's staging temporary (`mov`/`and` operand of a word test).
pub(super) const TEST_T: Reg = Reg::R10;

/// `beam_term::TAG_IMMED2_MASK` as an `i64` — the low-six-bits mask `is_atom` tests.
const ATOM_MASK: i64 = TAG_IMMED2_MASK as i64;
/// `beam_term::TAG_IMMED2_ATOM` as an `i64` — the atom immediate tag.
const ATOM_TAG: i64 = TAG_IMMED2_ATOM as i64;
/// `TAG_PRIMARY_MASK - TAG_PRIMARY_LIST` as an `i64`: `(word & LIST_MASK) == 0` is
/// exactly `beam_term::is_not_nil_list` (the C bit trick; the literal-pointer bit is
/// handled by the mask, as in the C).
const LIST_MASK: i64 = (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST) as i64;
/// The raw word of `NIL`.
const TEST_NIL: i64 = NIL.raw() as i64;

/// Which `test` operands need the small-int guard.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guard {
    /// No operand is a runtime value (both are constants), or the comparison is
    /// exact over immediate words.
    None,
    /// Guard the first operand.
    Lhs,
    /// Guard the second operand.
    Rhs,
    /// Guard both operands.
    Both,
}

/// One `test` operand the tier can read without the helper.
#[derive(Clone, Copy)]
pub(super) enum TestSrc {
    /// An x-register inside the engine's [`MAX_REGISTER`] bound.
    Reg(u32),
    /// A small-int constant (`{integer,V}`/`{char,C}` in the 60-bit range) — the only
    /// numeric constant that is word-order exact.
    Small(i64),
    /// An atom or `nil` constant — a **non-numeric** immediate.
    Immediate(i64),
}

/// The word-level predicate of an inlinable `test`; a jump to the success label is
/// taken when it holds.
enum TestPred {
    /// `is_nil` — the word is `NIL`.
    IsNil,
    /// `is_atom` — the word carries the atom immediate tag.
    IsAtom,
    /// `is_list` — `is_nil(v) || is_not_nil_list(v)`.
    IsList,
    /// `is_nonempty_list` — a cons cell.
    IsNonemptyList,
    /// `is_boolean` — the word is the module's `true` or `false` atom word; a name the
    /// module never interned contributes nothing.
    IsBoolean {
        /// The raw word of the module's `true` atom, when interned.
        truthy: Option<i64>,
        /// The raw word of the module's `false` atom, when interned.
        falsy: Option<i64>,
    },
    /// A word compare: success when `A <cond> B` holds.
    Cmp(Cond),
}

/// An inlinable `test`.
pub(super) struct TestPlan {
    /// The first operand's source.
    pub(super) lhs: TestSrc,
    /// The second operand's source (a comparison only).
    pub(super) rhs: Option<TestSrc>,
    /// The guard the comparison needs.
    guard: Guard,
    /// The predicate to evaluate over the loaded operands.
    pred: TestPred,
    /// The resolved fail label's `(func, pc)`.
    fail: (u32, u32),
}

/// The inlinable shape of `instr`, or `None` when the whole instruction must stay on
/// the helper.
pub(super) fn test_plan(
    module: &Module,
    kind: TestKind,
    fail: Label,
    args: &[Operand],
    labels: &LabelMap,
) -> Option<TestPlan> {
    // A `NO_FAIL` test *raises* on failure; the helper reproduces that exactly.
    if fail == NO_FAIL {
        return None;
    }
    // An unresolved label keeps the interpreter's exact `unknown_label`.
    let target = labels.resolve(fail).ok()?;

    let pred = match kind {
        TestKind::IsNil => TestPred::IsNil,
        TestKind::IsAtom => TestPred::IsAtom,
        TestKind::IsList => TestPred::IsList,
        TestKind::IsNonemptyList => TestPred::IsNonemptyList,
        TestKind::IsBoolean => {
            let truthy = module
                .atom_table
                .lookup("true")
                .map(|atom| make_atom(atom).raw() as i64);
            let falsy = module
                .atom_table
                .lookup("false")
                .map(|atom| make_atom(atom).raw() as i64);
            if truthy.is_none() && falsy.is_none() {
                // `is_boolean` can never succeed in this module; the helper is exact.
                return None;
            }
            TestPred::IsBoolean { truthy, falsy }
        }
        _ => {
            let lhs = test_src(args.first()?)?;
            let rhs = test_src(args.get(1)?)?;
            let (guard, cond) = compare_shape(kind, lhs, rhs)?;
            return Some(TestPlan {
                lhs,
                rhs: Some(rhs),
                guard,
                pred: TestPred::Cmp(cond),
                fail: target,
            });
        }
    };

    Some(TestPlan {
        lhs: test_src(args.first()?)?,
        rhs: None,
        guard: Guard::None,
        pred,
        fail: target,
    })
}

/// The native source of one operand, or `None` when it needs the helper: a `Y`
/// register, a float register, a literal, a bignum constant, or an x-register past
/// the engine's [`MAX_REGISTER`] bound (whose exact `bad_register` must surface).
pub(super) fn test_src(operand: &Operand) -> Option<TestSrc> {
    match x_register(operand) {
        Some(x) if x < MAX_REGISTER => Some(TestSrc::Reg(x)),
        Some(_) => None,
        None => match operand {
            Operand::Integer(value) => small_word(*value).map(TestSrc::Small),
            Operand::Character(c) => small_word(i64::from(*c)).map(TestSrc::Small),
            Operand::Atom(atom) => Some(TestSrc::Immediate(make_atom(*atom).raw() as i64)),
            Operand::Nil => Some(TestSrc::Immediate(TEST_NIL)),
            _ => None,
        },
    }
}

/// The `(guard, success condition)` of an inlinable comparison, or `None` when the
/// shape is not provably word-exact.
fn compare_shape(kind: TestKind, lhs: TestSrc, rhs: TestSrc) -> Option<(Guard, Cond)> {
    let is_const =
        |src: TestSrc| matches!(src, TestSrc::Small(_) | TestSrc::Immediate(_));
    match kind {
        TestKind::IsEqExact | TestKind::IsNeExact => {
            let cond = if kind == TestKind::IsEqExact {
                Cond::E
            } else {
                Cond::Ne
            };
            match (lhs, rhs) {
                // Two runtime values: guard **both** as small ints, after which a plain
                // word compare *is* `=:=` — `make_small(v) == v*16 + 15` is injective
                // and `cmp`'s raw-word fast path is exact. Two distinct boxed terms
                // (equal floats, bignums, tuples, …) share no word, but neither is a
                // small, so the guard sends them to the helper (which compares `cmp`'s
                // own way) rather than fabricating a word-compare answer.
                (TestSrc::Reg(_), TestSrc::Reg(_)) => Some((Guard::Both, cond)),
                // At least one operand is a constant immediate, which is `=:=` only to
                // the identical word (see the module comment): a plain word compare is
                // already exact.
                _ if is_const(lhs) || is_const(rhs) => Some((Guard::None, cond)),
                _ => None,
            }
        }
        TestKind::IsEq | TestKind::IsNe => {
            let cond = if kind == TestKind::IsEq { Cond::E } else { Cond::Ne };
            Some((arith_guard(lhs, rhs)?, cond))
        }
        TestKind::IsLt | TestKind::IsGe => {
            let cond = if kind == TestKind::IsLt { Cond::L } else { Cond::Ge };
            Some((order_guard(lhs, rhs)?, cond))
        }
        _ => None,
    }
}

/// The guard an **arithmetic** equality (`is_eq`/`is_ne`) needs for the word compare
/// to reproduce `cmp_arith` exactly.
///
/// `==` is the only comparison where an immediate is not self-sufficient: it treats
/// `1` and `1.0` as equal, so a small-int operand needs its register counterpart
/// guarded. An atom/`nil` immediate is `==` only to the identical word, so it needs
/// no guard; two small-int constants need none either.
fn arith_guard(lhs: TestSrc, rhs: TestSrc) -> Option<Guard> {
    match (lhs, rhs) {
        (TestSrc::Immediate(_), _) | (_, TestSrc::Immediate(_)) => Some(Guard::None),
        (TestSrc::Small(_), TestSrc::Small(_)) => Some(Guard::None),
        (TestSrc::Small(_), TestSrc::Reg(_)) => Some(Guard::Rhs),
        (TestSrc::Reg(_), TestSrc::Small(_)) => Some(Guard::Lhs),
        (TestSrc::Reg(_), TestSrc::Reg(_)) => Some(Guard::Both),
    }
}

/// The guard an **ordering** compare (`is_lt`/`is_ge`) needs: only two small-int
/// words are word-order exact, so an atom/`nil` (or bignum/literal) constant keeps
/// the instruction on the helper.
fn order_guard(lhs: TestSrc, rhs: TestSrc) -> Option<Guard> {
    match (lhs, rhs) {
        (TestSrc::Small(_), TestSrc::Small(_)) => Some(Guard::None),
        (TestSrc::Small(_), TestSrc::Reg(_)) => Some(Guard::Rhs),
        (TestSrc::Reg(_), TestSrc::Small(_)) => Some(Guard::Lhs),
        (TestSrc::Reg(_), TestSrc::Reg(_)) => Some(Guard::Both),
        _ => None,
    }
}

/// Load a `test` operand into scratch `reg`.
pub(super) fn load_test_src(
    buffer: &mut CodeBuffer,
    src: TestSrc,
    reg: Reg,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match src {
        TestSrc::Reg(x) => match allocation.slot(x) {
            Some(cached) => buffer.mov_rr(reg, cached),
            None => {
                touch(highest_touched, x);
                buffer.mov_rm(reg, Reg::Rbx, x_offset(x))
            }
        },
        TestSrc::Small(word) | TestSrc::Immediate(word) => buffer.mov_ri(reg, word),
    }
}

/// Branch to `deopt` unless `reg` holds a small integer (`(word & 0xF) == 0xF`).
///
/// `FAST_C` must already hold [`SMALL_MASK`] — the tag and the mask are the same
/// nibble (asserted below), exactly as in [`super::arith::try_inline_gc_bif`].
fn guard_small(buffer: &mut CodeBuffer, reg: Reg, deopt: CodeLabel) -> Result<()> {
    buffer.mov_rr(TEST_T, reg)?;
    buffer.and_rr(TEST_T, FAST_C)?;
    buffer.cmp_rr(TEST_T, FAST_C)?;
    buffer.jcc(Cond::Ne, deopt)
}

/// Lower an inlinable `test` natively, or report that the whole instruction must be
/// handed to the helper.
///
/// Returns `false` **without having emitted a byte** for any shape
/// [`test_plan`] refuses (a `NO_FAIL` test, an unresolved fail label, a boxed-term
/// type test, an inexact comparison), so the caller falls back to exactly today's
/// `jit_step_one`.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_test(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    linker: &Linker<'_>,
    instr: &Instr,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Instr::Test { kind, fail, args } = instr else {
        return Ok(false);
    };
    let Some(plan) = test_plan(module, *kind, *fail, args, labels) else {
        return Ok(false);
    };

    let after: CodeLabel = buffer.label();
    let deopt: Option<CodeLabel> = (plan.guard != Guard::None).then(|| buffer.label());

    // --- operands -----------------------------------------------------------
    load_test_src(buffer, plan.lhs, TEST_A, allocation, highest_touched)?;
    if let Some(rhs) = plan.rhs {
        load_test_src(buffer, rhs, TEST_B, allocation, highest_touched)?;
    }

    // --- the small-int guard ------------------------------------------------
    if let Some(deopt) = deopt {
        buffer.mov_ri(FAST_C, SMALL_MASK)?;
        if matches!(plan.guard, Guard::Lhs | Guard::Both) {
            guard_small(buffer, TEST_A, deopt)?;
        }
        if matches!(plan.guard, Guard::Rhs | Guard::Both) {
            guard_small(buffer, TEST_B, deopt)?;
        }
    }

    // --- the predicate (jump to `after` when the test SUCCEEDS) -------------
    match &plan.pred {
        TestPred::IsNil => {
            buffer.mov_ri(TEST_T, TEST_NIL)?;
            buffer.cmp_rr(TEST_A, TEST_T)?;
            buffer.jcc(Cond::E, after)?;
        }
        TestPred::IsAtom => {
            buffer.mov_ri(FAST_C, ATOM_MASK)?;
            buffer.mov_rr(TEST_T, TEST_A)?;
            buffer.and_rr(TEST_T, FAST_C)?;
            buffer.mov_ri(FAST_C, ATOM_TAG)?;
            buffer.cmp_rr(TEST_T, FAST_C)?;
            buffer.jcc(Cond::E, after)?;
        }
        TestPred::IsNonemptyList => {
            buffer.mov_ri(FAST_C, LIST_MASK)?;
            buffer.mov_rr(TEST_T, TEST_A)?;
            buffer.and_rr(TEST_T, FAST_C)?;
            buffer.jcc(Cond::E, after)?;
        }
        TestPred::IsList => {
            buffer.mov_ri(TEST_T, TEST_NIL)?;
            buffer.cmp_rr(TEST_A, TEST_T)?;
            buffer.jcc(Cond::E, after)?;
            buffer.mov_ri(FAST_C, LIST_MASK)?;
            buffer.mov_rr(TEST_T, TEST_A)?;
            buffer.and_rr(TEST_T, FAST_C)?;
            buffer.jcc(Cond::E, after)?;
        }
        TestPred::IsBoolean { truthy, falsy } => {
            for word in [*truthy, *falsy].into_iter().flatten() {
                buffer.mov_ri(TEST_T, word)?;
                buffer.cmp_rr(TEST_A, TEST_T)?;
                buffer.jcc(Cond::E, after)?;
            }
        }
        TestPred::Cmp(cond) => {
            buffer.cmp_rr(TEST_A, TEST_B)?;
            buffer.jcc(*cond, after)?;
        }
    }

    // --- the test failed: branch to the fail label --------------------------
    // A compiled fail block is entered directly (the linker spills the cached
    // x-registers, leaves this frame and jumps to its entry); any other fail
    // target returns its `(func, pc)` word to the driver exactly as before.
    linker.emit_exit(buffer, allocation, done, plan.fail.0, plan.fail.1)?;

    // --- the guard rejected an operand: run the real instruction ------------
    if let Some(deopt) = deopt {
        buffer.bind(deopt)?;
        emit_step_and_continue(buffer, done, allocation, func, pc)?;
    }
    buffer.bind(after)?;
    Ok(true)
}
