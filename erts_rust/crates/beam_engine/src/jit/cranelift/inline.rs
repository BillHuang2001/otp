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

//! The inlined instruction set: `move`, the small-int arithmetic fast path and
//! the word-level `test` subset.
//!
//! Everything here is a **shape** test (never a value test): a compile-time
//! shape rejection deopts the whole instruction to the helper, and a run-time
//! guard rejection (`jit_step_one` via [`super::codegen::emit_helper_dispatch`])
//! is the instruction's exact interpreter semantics. The aggregate *shape*
//! reads (`select_val`, the tuple reads, the list reads) live in the sibling
//! [`super::shape`] module.

use crate::interpreter::LabelMap;

use beam_loader::{Instr, Label, Module, Operand, Reg as RegRef, TestKind, NO_FAIL};
use beam_term::{
    make_atom, make_small, MAX_SMALL, MIN_SMALL, NIL, TAG_IMMED1_MASK, TAG_IMMED1_SIZE,
    TAG_IMMED1_SMALL, TAG_IMMED2_ATOM, TAG_IMMED2_MASK, TAG_PRIMARY_LIST, TAG_PRIMARY_MASK,
};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, FuncRef, InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;

use crate::machine::MAX_REGISTER;

use super::codegen::{
    emit_helper_dispatch, emit_target_branch, goto_or_leave, x_offset, Cfg, XREGS_DISP,
};
use super::report::CodegenReport;

// ---------------------------------------------------------------------------
// The inlined instruction set
// ---------------------------------------------------------------------------

/// Where an inlined `move`'s value comes from.
pub(super) enum MoveSource {
    /// An x-register (`machine.x[n]`).
    X(u32),
    /// A constant operand's raw `Eterm` word.
    Constant(i64),
}

/// The `(destination x-register, source)` of a `move` the tier can inline, or
/// `None` when the block must deopt to the helper.
///
/// Only x-register destinations and x-register / constant sources are inlinable; a
/// `Y` (or other) operand, or an x-register index past the engine's
/// [`MAX_REGISTER`] bound, deopts so the interpreter's exact `bad_register` error
/// surfaces.
pub(super) fn inline_move(src: &Operand, dst: &Operand) -> Option<(u32, MoveSource)> {
    let dst_x = x_register(dst)?;
    if dst_x >= MAX_REGISTER {
        return None;
    }
    let source = match x_register(src) {
        Some(src_x) if src_x < MAX_REGISTER => MoveSource::X(src_x),
        Some(_) => return None,
        None => MoveSource::Constant(constant_word(src)?),
    };
    Some((dst_x, source))
}

/// Emit an inlined `move`: read the source (an `machine.x` slot or a constant raw
/// word) and write the destination x-register slot, both in `machine.x`.
pub(super) fn emit_inline_move(
    builder: &mut FunctionBuilder,
    rt: Value,
    dst_x: u32,
    source: &MoveSource,
    report: &mut CodegenReport,
) {
    let x_regs = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), rt, XREGS_DISP);
    let value = match source {
        MoveSource::X(src_x) => {
            report.touch(*src_x);
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), x_regs, x_offset(*src_x))
        }
        MoveSource::Constant(word) => builder.ins().iconst(types::I64, *word),
    };
    builder
        .ins()
        .store(MemFlags::trusted(), value, x_regs, x_offset(dst_x));
    report.touch(dst_x);
}

// ---------------------------------------------------------------------------
// The inlined small-int arithmetic fast path
// ---------------------------------------------------------------------------

/// The two BIFs the inlined fast path lowers — `erlang:'+'/2`, `erlang:'-'/2` and
/// `erlang:'*'/2`, and nothing else.
///
/// A [`super::CompiledFunctions`] artifact reports the ops it inlined through
/// [`super::CompiledFunctions::inlined_gc_bif_ops`], so a test can prove that the fast
/// path — rather than a fall-through to `jit_step_one` — was actually **compiled**
/// into the artifact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SmallArith {
    /// `erlang:'+'/2`.
    Add,
    /// `erlang:'-'/2`.
    Sub,
    /// `erlang:'*'/2`.
    Mul,
}

impl SmallArith {
    /// The BIF this op lowers, spelled `erlang:'+'/2`, `erlang:'-'/2` or
    /// `erlang:'*'/2` — exactly the `module`/`function`/`arity` triple the
    /// interpreter's `GcBif` arm hands to
    /// [`ExecutionContext::call_bif`](crate::ExecutionContext::call_bif).
    pub fn name(&self) -> &'static str {
        match self {
            SmallArith::Add => "erlang:'+'/2",
            SmallArith::Sub => "erlang:'-'/2",
            SmallArith::Mul => "erlang:'*'/2",
        }
    }
}

/// `beam_term::TAG_IMMED1_MASK` as an `i64` — the low-nibble mask `is_small` tests
/// with (`(word & 0xF) == 0xF`).
const SMALL_MASK: i64 = TAG_IMMED1_MASK as i64;

/// `beam_term::TAG_IMMED1_SMALL` as an `i64` — the immediate-1 small tag, which is
/// also the low nibble of [`SMALL_MASK`] (so one mask serves as the guard's tag).
const SMALL_TAG: i64 = TAG_IMMED1_SMALL as i64;

/// The shift `make_small` applies (`beam_term::TAG_IMMED1_SIZE`).
const SMALL_SHIFT: i64 = TAG_IMMED1_SIZE as i64;

/// Where an inlined `gc_bif` reads one operand from.
#[derive(Clone, Copy)]
enum FastOperand {
    /// An x-register (`machine.x[n]`).
    X(u32),
    /// A small-int constant's raw `Eterm` word.
    Constant(i64),
}

/// A `gc_bif` shape the fast path lowers, resolved at compile time.
pub(super) struct FastArith {
    /// Which of the two BIFs is called.
    pub(super) op: SmallArith,
    /// The first operand.
    a: FastOperand,
    /// The second operand.
    b: FastOperand,
    /// The x-register index the result is written to.
    dst_x: u32,
}

/// The `gc_bif` shape the fast path lowers, or `None`.
///
/// The shape is the small-int `erlang:'+'/2`/`'-'/2`/`'*'/2` of arity 2 whose every
/// operand is an x-register (below the engine's [`MAX_REGISTER`] bound) or a
/// **small-int constant**, and whose destination is such an x-register. The BIF is
/// classified by exactly the strings the interpreter's `GcBif` arm hands to
/// [`ExecutionContext::call_bif`](crate::ExecutionContext::call_bif), so the fast
/// path can never lower a call the interpreter routes somewhere else.
pub(super) fn fast_arith(module: &Module, instr: &Instr) -> Option<FastArith> {
    let Instr::GcBif {
        arity, bif, args, dst, ..
    } = instr
    else {
        return None;
    };
    if *arity != 2 || args.len() != 2 {
        return None;
    }
    let import = module.imports.get(bif.0 as usize)?;
    if module.atom_name(import.module)? != "erlang" || import.arity != 2 {
        return None;
    }
    let op = match module.atom_name(import.function)? {
        "+" => SmallArith::Add,
        "-" => SmallArith::Sub,
        "*" => SmallArith::Mul,
        _ => return None,
    };
    let dst_x = x_register(dst)?;
    if dst_x >= MAX_REGISTER {
        return None;
    }
    Some(FastArith {
        op,
        a: fast_operand(&args[0])?,
        b: fast_operand(&args[1])?,
        dst_x,
    })
}

/// The operand the fast path can load: an x-register, or a constant that **is** a
/// small integer so the run-time guard cannot reject it.
fn fast_operand(operand: &Operand) -> Option<FastOperand> {
    match x_register(operand) {
        Some(x) if x < MAX_REGISTER => Some(FastOperand::X(x)),
        Some(_) => None,
        None => {
            let word = constant_word(operand)?;
            ((word & SMALL_MASK) == SMALL_TAG).then_some(FastOperand::Constant(word))
        }
    }
}

/// Emit the inlined small-int arithmetic fast path for the `gc_bif` at `pc`.
///
/// The fast path is taken only when **both** operands are tagged small integers and
/// the exact result is a representable small integer; every other case — either
/// operand not a small int, or a sum/difference outside
/// `[MIN_SMALL, MAX_SMALL]` — branches to the helper, whose interpreter step is the
/// BIF's exact semantics (including the `fail`-label guard / raise split and the
/// bignum / float / `badarith` paths). So the fast path can only ever *replace* a
/// successful BIF call with the identical tagged word; it never fabricates a result.
///
/// The arithmetic is done on the **untagged** values, which is what makes it exact
/// without an overflow flag: a tagged small word is `v*16 + 15`, so `(word - 15) >> 4`
/// recovers `v` exactly (the shift is arithmetic and the operand is an exact multiple
/// of 16). Two recovered values are each within `±2^59`, so their sum/difference
/// always fits an `i64` — unlike the tagged words, whose sum can overflow — and the
/// range test against `[MIN_SMALL, MAX_SMALL]` is then exactly `make_small`'s domain.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_inline_arith(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    pc: u32,
    code_len: usize,
    func_index: u32,
    rt: Value,
    fast: &FastArith,
    helper: FuncRef,
    this_func: Value,
    report: &mut CodegenReport,
) {
    let x_regs = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), rt, XREGS_DISP);
    let a = load_fast_operand(builder, x_regs, &fast.a, report);
    let b = load_fast_operand(builder, x_regs, &fast.b, report);

    let tagged_a = builder.ins().band_imm(a, SMALL_MASK);
    let tagged_b = builder.ins().band_imm(b, SMALL_MASK);
    let a_is_small = builder.ins().icmp_imm(IntCC::Equal, tagged_a, SMALL_TAG);
    let b_is_small = builder.ins().icmp_imm(IntCC::Equal, tagged_b, SMALL_TAG);
    let both_small = builder.ins().band(a_is_small, b_is_small);

    let slow = builder.create_block();
    let check = builder.create_block();
    builder.ins().brif(both_small, check, &[], slow, &[]);

    // Both operands are small: recover the untagged values and do the arithmetic.
    builder.switch_to_block(check);
    let untagged_a = builder.ins().iadd_imm(a, -SMALL_TAG);
    let untagged_b = builder.ins().iadd_imm(b, -SMALL_TAG);
    let value_a = builder.ins().sshr_imm(untagged_a, SMALL_SHIFT);
    let value_b = builder.ins().sshr_imm(untagged_b, SMALL_SHIFT);
    // `+`/`-` of two 60-bit values always fits an `i64`, so the range test alone is
    // the guard. `*` can overflow a 64-bit word, so the product is computed with an
    // explicit overflow check: `smulhi` yields the high half of the signed
    // double-width product, and "no overflow" is exactly
    // `high_half == (low_half >> 63)` (the sign-extension of the low half). A
    // forgotten check would let a wrapped product slip past the range test, so the
    // overflow flag is ANDed into the guard.
    let (product, overflow_ok) = match fast.op {
        SmallArith::Add => (builder.ins().iadd(value_a, value_b), None),
        SmallArith::Sub => (builder.ins().isub(value_a, value_b), None),
        SmallArith::Mul => {
            let low = builder.ins().imul(value_a, value_b);
            let high = builder.ins().smulhi(value_a, value_b);
            let sign = builder.ins().sshr_imm(low, 63);
            let no_overflow = builder.ins().icmp(IntCC::Equal, high, sign);
            (low, Some(no_overflow))
        }
    };
    let low_ok = builder
        .ins()
        .icmp_imm(IntCC::SignedGreaterThanOrEqual, product, MIN_SMALL);
    let high_ok = builder
        .ins()
        .icmp_imm(IntCC::SignedLessThanOrEqual, product, MAX_SMALL);
    let mut in_range = builder.ins().band(low_ok, high_ok);
    if let Some(no_overflow) = overflow_ok {
        in_range = builder.ins().band(in_range, no_overflow);
    }

    let store = builder.create_block();
    builder.ins().brif(in_range, store, &[], slow, &[]);

    // Success: store `make_small(product)` — `product * 16 + 15` — into the
    // destination slot and continue at `pc + 1`.
    builder.switch_to_block(store);
    let shifted = builder.ins().ishl_imm(product, SMALL_SHIFT);
    let result = builder.ins().iadd_imm(shifted, SMALL_TAG);
    builder
        .ins()
        .store(MemFlags::trusted(), result, x_regs, x_offset(fast.dst_x));
    report.touch(fast.dst_x);
    goto_or_leave(builder, cfg, code_len, func_index, pc + 1);

    // Rejected: one interpreter step, the BIF's exact semantics.
    builder.switch_to_block(slow);
    emit_helper_dispatch(builder, cfg, pc, code_len, helper, rt, this_func);
}
/// Read one fast-path operand: an `machine.x` slot (recording it as natively
/// addressed) or a constant's raw word.
fn load_fast_operand(
    builder: &mut FunctionBuilder,
    x_regs: Value,
    operand: &FastOperand,
    report: &mut CodegenReport,
) -> Value {
    match operand {
        FastOperand::X(x) => {
            report.touch(*x);
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), x_regs, x_offset(*x))
        }
        FastOperand::Constant(word) => builder.ins().iconst(types::I64, *word),
    }
}
/// The x-register index an operand names, if any (`X(n)` or the `tr`-annotated
/// `Typed { reg: X(n), .. }` — the `Type` annotation is irrelevant to the value).
pub(super) fn x_register(operand: &Operand) -> Option<u32> {
    match operand {        Operand::X(n) => Some(*n),
        Operand::Typed {
            reg: RegRef::X(n),
            ..
        } => Some(*n),
        _ => None,
    }
}

/// The raw `Eterm` word of a constant operand, as the interpreter's
/// `read_operand` evaluates it (`make_small`/`make_atom`/`NIL`).
///
/// An `Integer`/`Character` outside the small-int range is `None`: the native tier
/// only inlines immediates, so such a constant makes the block **deopt** to
/// `jit_step_one`, whose interpreter step materialises the exact bignum.
pub(super) fn constant_word(operand: &Operand) -> Option<i64> {
    match operand {
        Operand::Integer(value) => small_word(*value),
        Operand::Character(c) => small_word(i64::from(*c)),
        Operand::Atom(atom) => Some(make_atom(*atom).raw() as i64),
        Operand::Nil => Some(NIL.raw() as i64),
        _ => None,
    }
}

/// The raw `Eterm` word of a small-int constant, or `None` when `value` is outside
/// the 60-bit immediate range (`beam_term::{MIN_SMALL, MAX_SMALL}`).
fn small_word(value: i64) -> Option<i64> {
    if (MIN_SMALL..=MAX_SMALL).contains(&value) {
        Some(make_small(value).raw() as i64)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// The inlined `test` subset
// ---------------------------------------------------------------------------

/// `beam_term::TAG_IMMED2_MASK` as an `i64` — the low-six-bits mask `is_atom` tests.
const ATOM_MASK: i64 = TAG_IMMED2_MASK as i64;

/// `beam_term::TAG_IMMED2_ATOM` as an `i64` — the atom immediate tag.
const ATOM_TAG: i64 = TAG_IMMED2_ATOM as i64;

/// The `is_not_list` mask `beam_term::list::is_not_list` uses —
/// `TAG_PRIMARY_MASK - TAG_PRIMARY_LIST` (the C bit trick `x & 0x2`), so
/// `(word & LIST_MASK) == 0` is exactly `is_list(word)`.
pub(super) const LIST_MASK: i64 = (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST) as i64;

/// The `NIL` raw word as an `i64` (`is_nil` is a whole-word equality test).
const NIL_WORD: i64 = NIL.raw() as i64;

/// A constant-immediate operand an inlined `test` compares against.
#[derive(Clone, Copy)]
pub(super) enum TestConst {
    /// A small-int constant's raw tagged `Eterm` word (`{integer,V}` / `{char,C}`
    /// in the 60-bit range) — a *numeric* immediate.
    Small(i64),
    /// A non-numeric immediate constant's raw word — `{atom,A}` or `nil`.
    Immediate(i64),
}

/// A `test` operand the inline path can read natively: an x-register slot or a
/// constant-immediate word.
#[derive(Clone, Copy)]
pub(super) enum CmpSrc {
    /// An x-register (`machine.x[n]`).
    Reg(u32),
    /// A constant-immediate operand's raw word.
    Const(TestConst),
}

/// Which of an inlined comparison's operands must be guarded as untagged small
/// integers for the tagged-**word** compare to reproduce the interpreter's
/// exact / arithmetic ordering exactly.
#[derive(Clone, Copy)]
pub(super) enum Guard {
    /// No guard: the compare is already exact (at least one operand is a
    /// constant-immediate, or both are small-int constants).
    None,
    /// The first operand must be an untagged small int.
    Lhs,
    /// The second operand must be an untagged small int.
    Rhs,
    /// Both operands must be untagged small ints.
    Both,
}

/// A comparison `test` the tier inlines.
pub(super) struct ComparePlan {
    /// The first operand's native source.
    lhs: CmpSrc,
    /// The second operand's native source.
    rhs: CmpSrc,
    /// The operands that must be guarded as small ints.
    guard: Guard,
    /// The condition on the two loaded words that means the test **succeeds**.
    success: IntCC,
    /// The resolved `(func, pc)` of the instruction's `fail` label.
    fail: (u32, u32),
}

/// A word-level type `test` the tier inlines.
pub(super) struct TypePlan {
    /// The tested operand's native source.
    src: CmpSrc,
    /// The predicate.
    kind: TypeTest,
    /// The resolved `(func, pc)` of the instruction's `fail` label.
    fail: (u32, u32),
}

/// The word-level `is_*` tests the tier inlines (each a single-word predicate that
/// never dereferences).
#[derive(Clone, Copy)]
enum TypeTest {
    /// `is_nil` — the word is `NIL`.
    Nil,
    /// `is_atom` — the word carries the atom immediate tag.
    Atom,
    /// `is_list` — `is_nil(v) || is_not_nil_list(v)`.
    List,
    /// `is_nonempty_list` — a cons cell.
    NonemptyList,
    /// `is_boolean` — the word is the module's `true` or `false` atom.
    Boolean {
        /// The raw word of the module's `true` atom, when it is interned.
        truthy: Option<i64>,
        /// The raw word of the module's `false` atom, when it is interned.
        falsy: Option<i64>,
    },
}

/// A `test` the tier inlines natively.
pub(super) enum TestPlan {
    /// A two-operand comparison.
    Compare(ComparePlan),
    /// A one-operand word-level type test.
    Type(TypePlan),
}

/// The plan for an inlinable `test`, or `None` when the test stays on the helper.
///
/// Only **provably-exact** shapes are inlined (see the module docs); everything
/// else — a `NO_FAIL` (`fail,0`) test, an unresolved fail label, a `Y`/float/literal
/// operand, an out-of-range x-register, a bignum constant, a boxed-term type test,
/// `is_function2`/`is_tagged_tuple`/`test_arity` — returns `None`, so the helper runs
/// the instruction with the interpreter's exact semantics (and the interpreter's
/// exact typed error).
pub(super) fn inline_test(
    module: &Module,
    kind: TestKind,
    fail: Label,
    args: &[Operand],
    labels: &LabelMap,
) -> Option<TestPlan> {
    // A `NO_FAIL` test *raises* on failure (the interpreter's typed `test/no_fail`),
    // so it is never inlined: the helper reproduces that exactly.
    if fail == NO_FAIL {
        return None;
    }
    // An unresolved fail label keeps the interpreter's exact `unknown_label` error,
    // so the whole test stays on the helper.
    let target = labels.resolve(fail).ok()?;

    let type_kind = match kind {
        TestKind::IsNil => Some(TypeTest::Nil),
        TestKind::IsAtom => Some(TypeTest::Atom),
        TestKind::IsList => Some(TypeTest::List),
        TestKind::IsNonemptyList => Some(TypeTest::NonemptyList),
        // `is_boolean` compares against the module's own `true`/`false` atom words; a
        // name the module never interned can never be the value, and contributes
        // nothing (the interpreter's `.unwrap_or(false)`).
        TestKind::IsBoolean => Some(TypeTest::Boolean {
            truthy: module
                .atom_table
                .lookup("true")
                .map(|atom| make_atom(atom).raw() as i64),
            falsy: module
                .atom_table
                .lookup("false")
                .map(|atom| make_atom(atom).raw() as i64),
        }),
        _ => None,
    };
    if let Some(kind) = type_kind {
        let src = cmp_src(args.first()?)?;
        return Some(TestPlan::Type(TypePlan {
            src,
            kind,
            fail: target,
        }));
    }

    let lhs = cmp_src(args.first()?)?;
    let rhs = cmp_src(args.get(1)?)?;
    let (guard, success) = compare_shape(kind, lhs, rhs)?;
    Some(TestPlan::Compare(ComparePlan {
        lhs,
        rhs,
        guard,
        success,
        fail: target,
    }))
}

/// The native source of one `test` operand, or `None` when the operand needs the
/// helper: a `Y` register, a float register, a literal, a bignum constant, an
/// `Atom::MAX` placeholder, or an x-register index past the engine's
/// [`MAX_REGISTER`] bound (whose exact `bad_register` error must surface).
pub(super) fn cmp_src(operand: &Operand) -> Option<CmpSrc> {
    match x_register(operand) {
        Some(x) if x < MAX_REGISTER => Some(CmpSrc::Reg(x)),
        Some(_) => None,
        None => test_const(operand).map(CmpSrc::Const),
    }
}

/// The constant-immediate form of an operand, with its raw `Eterm` word: a
/// small-int (`{integer,V}`/`{char,C}` in the 60-bit range), an atom, or `nil`.
///
/// A bignum `{integer,V}` constant (out of the small range), a literal, and every
/// other operand kind return `None`, so they stay on the helper.
fn test_const(operand: &Operand) -> Option<TestConst> {
    match operand {
        Operand::Integer(value) => small_word(*value).map(TestConst::Small),
        Operand::Character(c) => small_word(i64::from(*c)).map(TestConst::Small),
        Operand::Atom(atom) => Some(TestConst::Immediate(make_atom(*atom).raw() as i64)),
        Operand::Nil => Some(TestConst::Immediate(NIL_WORD)),
        _ => None,
    }
}

/// The `(guard, success)` of an inlinable comparison, or `None` when the shape is
/// not provably word-exact.
fn compare_shape(kind: TestKind, lhs: CmpSrc, rhs: CmpSrc) -> Option<(Guard, IntCC)> {
    match kind {
        TestKind::IsEqExact | TestKind::IsNeExact => {
            let success = if kind == TestKind::IsEqExact {
                IntCC::Equal
            } else {
                IntCC::NotEqual
            };
            match (lhs, rhs) {
                // Two runtime values: guard **both** as untagged small ints, after
                // which a plain word compare *is* `=:=` — `make_small(v) == v*16 + 15`
                // is injective and `cmp`'s raw-word fast path is exact. Two distinct
                // boxed terms (equal floats, bignums, binaries, tuples, …) share no
                // word, but neither is a small, so the guard sends them to the helper
                // (which compares `cmp`'s own way) rather than fabricating a
                // word-compare answer. This is exactly the baseline tier's
                // `emit/test.rs::compare_shape` rule.
                (CmpSrc::Reg(_), CmpSrc::Reg(_)) => Some((Guard::Both, success)),
                // At least one operand is a constant-immediate (a small int, an atom
                // or `nil`), which is `=:=` only to the identical word, so a plain
                // word compare is already exact.
                _ => Some((Guard::None, success)),
            }
        }
        TestKind::IsEq | TestKind::IsNe => {
            let success = if kind == TestKind::IsEq {
                IntCC::Equal
            } else {
                IntCC::NotEqual
            };
            Some((arith_guard(lhs, rhs)?, success))
        }
        TestKind::IsLt | TestKind::IsGe => {
            let success = if kind == TestKind::IsLt {
                IntCC::SignedLessThan
            } else {
                IntCC::SignedGreaterThanOrEqual
            };
            Some((order_guard(lhs, rhs)?, success))
        }
        _ => None,
    }
}

/// The guard an **arithmetic** equality (`==`: `is_eq`/`is_ne`) needs for the word
/// compare to reproduce `cmp_arith` exactly.
///
/// `==` is the only comparison where immediates are not self-sufficient: it treats
/// `1` and `1.0` as equal (and `-0.0 == +0.0`), so a small-int constant needs its
/// register counterpart guarded as a small int. An atom/`nil` immediate is never
/// arithmetically equal to any other term, so it needs no guard; two constants need
/// none either (a small-int and an atom/`nil` constant are never `==`).
fn arith_guard(lhs: CmpSrc, rhs: CmpSrc) -> Option<Guard> {
    match (lhs, rhs) {
        // An atom/`nil` immediate is `==` only to the identical word.
        (CmpSrc::Const(TestConst::Immediate(_)), _) | (_, CmpSrc::Const(TestConst::Immediate(_))) => {
            Some(Guard::None)
        }
        (CmpSrc::Const(TestConst::Small(_)), CmpSrc::Const(TestConst::Small(_))) => Some(Guard::None),
        (CmpSrc::Const(TestConst::Small(_)), CmpSrc::Reg(_)) => Some(Guard::Rhs),
        (CmpSrc::Reg(_), CmpSrc::Const(TestConst::Small(_))) => Some(Guard::Lhs),
        (CmpSrc::Reg(_), CmpSrc::Reg(_)) => Some(Guard::Both),
    }
}

/// The guard an **ordering** compare (`<`, `>=`: `is_lt`/`is_ge`) needs.
///
/// `cmp_arith` orders numbers against floats by value and places numbers below the
/// non-numeric classes, so only two small-int words are safe: the tagged-word
/// *signed* order is the value order because `make_small(v) == v*16 + 15` is
/// monotonic. A constant that is an atom/`nil` (or a bignum/literal operand) is not
/// word-order exact, so those shapes stay on the helper.
fn order_guard(lhs: CmpSrc, rhs: CmpSrc) -> Option<Guard> {
    match (lhs, rhs) {
        (CmpSrc::Const(TestConst::Small(_)), CmpSrc::Const(TestConst::Small(_))) => Some(Guard::None),
        (CmpSrc::Const(TestConst::Small(_)), CmpSrc::Reg(_)) => Some(Guard::Rhs),
        (CmpSrc::Reg(_), CmpSrc::Const(TestConst::Small(_))) => Some(Guard::Lhs),
        (CmpSrc::Reg(_), CmpSrc::Reg(_)) => Some(Guard::Both),
        _ => None,
    }
}

/// Emit an inlined `test` at `pc`: load its operand word(s), (optionally) guard
/// them as untagged small ints, branch on the resulting condition — falling through
/// to `pc + 1` on success and to the resolved `fail` label otherwise — and deopt to
/// the helper when the guard rejects an operand.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_inline_test(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    pc: u32,
    code_len: usize,
    func_index: u32,
    rt: Value,
    plan: &TestPlan,
    helper: FuncRef,
    this_func: Value,
    report: &mut CodegenReport,
) {
    let x_regs = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), rt, XREGS_DISP);

    // The success condition on the loaded word(s), plus the operands whose
    // "is an untagged small int" guard the condition depends on.
    let mut guarded: [Option<Value>; 2] = [None, None];
    let (condition, fail) = match plan {
        TestPlan::Type(plan) => {
            let word = load_cmp_src(builder, x_regs, &plan.src, report);
            (type_condition(builder, word, &plan.kind), plan.fail)
        }
        TestPlan::Compare(plan) => {
            let lhs = load_cmp_src(builder, x_regs, &plan.lhs, report);
            let rhs = load_cmp_src(builder, x_regs, &plan.rhs, report);
            guarded = match plan.guard {
                Guard::None => [None, None],
                Guard::Lhs => [Some(lhs), None],
                Guard::Rhs => [None, Some(rhs)],
                Guard::Both => [Some(lhs), Some(rhs)],
            };
            (builder.ins().icmp(plan.success, lhs, rhs), plan.fail)
        }
    };

    // A guarded operand that is not an untagged small int makes the word compare
    // inexact (a float, bignum, atom, …), so the whole instruction deopts to the
    // helper rather than fabricating a result from the tagged words.
    let guard = guarded.into_iter().flatten().fold(None, |acc, value| {
        let is_small = small_int_guard(builder, value);
        Some(match acc {
            Some(previous) => builder.ins().band(previous, is_small),
            None => is_small,
        })
    });
    let compare = builder.create_block();
    let slow = guard.map(|_| builder.create_block());
    match (guard, slow) {
        (Some(guard), Some(slow)) => {
            builder.ins().brif(guard, compare, &[], slow, &[]);
        }
        _ => {
            builder.ins().jump(compare, &[]);
        }
    }

    // The test's outcome: success falls through to `pc + 1`, failure jumps to the
    // `fail` label (natively, or through the driver for a cross-function target).
    builder.switch_to_block(compare);
    let success = builder.create_block();
    let fail_block = builder.create_block();
    builder
        .ins()
        .brif(condition, success, &[], fail_block, &[]);
    builder.switch_to_block(success);
    goto_or_leave(builder, cfg, code_len, func_index, pc + 1);
    builder.switch_to_block(fail_block);
    emit_target_branch(builder, cfg, code_len, func_index, fail);

    // A rejected guard: one interpreter step, the test's exact semantics.
    if let Some(slow) = slow {
        builder.switch_to_block(slow);
        emit_helper_dispatch(builder, cfg, pc, code_len, helper, rt, this_func);
    }
}

/// The success condition of a word-level type test, as a single boolean value.
fn type_condition(builder: &mut FunctionBuilder, word: Value, kind: &TypeTest) -> Value {
    match kind {
        TypeTest::Nil => builder.ins().icmp_imm(IntCC::Equal, word, NIL_WORD),
        TypeTest::Atom => {
            let tag = builder.ins().band_imm(word, ATOM_MASK);
            builder.ins().icmp_imm(IntCC::Equal, tag, ATOM_TAG)
        }
        TypeTest::NonemptyList => {
            let tag = builder.ins().band_imm(word, LIST_MASK);
            builder.ins().icmp_imm(IntCC::Equal, tag, 0)
        }
        TypeTest::List => {
            // `is_nil(v) || is_not_nil_list(v)`: `NIL` is an immediate, not a cons
            // cell, so the two halves are disjoint and both are needed.
            let tag = builder.ins().band_imm(word, LIST_MASK);
            let is_list = builder.ins().icmp_imm(IntCC::Equal, tag, 0);
            let is_nil = builder.ins().icmp_imm(IntCC::Equal, word, NIL_WORD);
            builder.ins().bor(is_list, is_nil)
        }
        TypeTest::Boolean { truthy, falsy } => {
            // The interpreter's `is_atom(v) && (is_true || is_false)`. The `is_atom`
            // half is implied by `v == atom_word` (an atom word carries the atom
            // tag) and an absent name contributes nothing, so the OR alone is exact —
            // with neither name interned the whole condition is the interpreter's
            // `false`.
            let mut condition: Option<Value> = None;
            for &atom in [truthy, falsy].into_iter().flatten() {
                let equal = builder.ins().icmp_imm(IntCC::Equal, word, atom);
                condition = Some(match condition {
                    Some(previous) => builder.ins().bor(previous, equal),
                    None => equal,
                });
            }
            match condition {
                Some(condition) => condition,
                None => builder.ins().iconst(types::I8, 0),
            }
        }
    }
}

/// Load one `test` operand word: an `machine.x` slot (recording it as natively
/// addressed) or a constant-immediate's raw word.
pub(super) fn load_cmp_src(
    builder: &mut FunctionBuilder,
    x_regs: Value,
    src: &CmpSrc,
    report: &mut CodegenReport,
) -> Value {
    match src {
        CmpSrc::Reg(x) => {
            report.touch(*x);
            builder
                .ins()
                .load(types::I64, MemFlags::trusted(), x_regs, x_offset(*x))
        }
        CmpSrc::Const(TestConst::Small(word) | TestConst::Immediate(word)) => {
            builder.ins().iconst(types::I64, *word)
        }
    }
}

/// The guard's "is an untagged small int" test for one operand —
/// `(word & 0xF) == 0xF`, the same test the arithmetic fast path uses.
pub(super) fn small_int_guard(builder: &mut FunctionBuilder, value: Value) -> Value {
    let masked = builder.ins().band_imm(value, SMALL_MASK);
    builder.ins().icmp_imm(IntCC::Equal, masked, SMALL_TAG)
}
