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

//! The Cranelift hot tier's comparison fast paths: the eight `erlang` comparison
//! BIFs (`=:=`/`=/=`/`==`/`/=`/`<`/`=<`/`>`/`>=`) lowered natively for small-int and
//! immediate operands (the section comment below proves each rule is
//! interpreter-identical).
//!
//! This mirrors the baseline tier's `jit/baseline/emit/compare.rs` rule for rule.
//! The two native tiers must agree **exactly** on the mirrored set, so every shape
//! this module refuses, and every run-time guard rejection inside it, deopts to the
//! *same* shared helper — [`jit_step_one`](crate::jit::baseline::jit_step_one), the
//! interpreter's own step — rather than computing a second answer.
//!
//! Like the sibling [`super::inline`] module this is a **shape** test (never a value
//! test). The aggregate *shape* reads (`select_val`, the tuple reads, the list reads)
//! live in [`super::shape`].

use beam_loader::{Instr, Module};
use beam_term::make_atom;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, FuncRef, InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;

use crate::machine::MAX_REGISTER;

use super::codegen::{
    emit_helper_dispatch, goto_or_leave, x_offset, Cfg, XREGS_DISP,
};
use super::inline::{cmp_src, load_cmp_src, small_int_guard, x_register, CmpSrc, Guard, TestConst};
use super::report::CodegenReport;

// ---------------------------------------------------------------------------
// The comparison fast paths
// ---------------------------------------------------------------------------
//
// The eight comparison operators are the compiler's clause guards and boolean
// bodies (the settled corpus emits every one of them as a **non-collecting**
// `{bif,Op,{f,0},Args,Dst}`, never as a `gc_bif`), so leaving them to
// `jit_step_one` costs a helper round-trip per comparison. They are total
// functions — `beam_bifs` returns `am_true`/`am_false` for any two terms
// (`erts/emulator/beam/erl_bif_op.c:103-131`) — so the fast path below produces a
// boolean *without* consulting `ctx.call_bif`, and any shape it cannot prove exact
// deopts to the interpreter step (which reproduces the fail/raise split, `Fail = 0`
// included).
//
// Each operator is lowered as `load a; load b; guard(s); cmp a, b; brif(cond,
// true); false; true`. The rules, and why each is the interpreter's own result
// (`cmp` / `cmp_arith` over the module's atom table), are the baseline tier's,
// reproduced verbatim:
//
// * `=:=`/`=/=` use the **exact** order (`cmp`). Two registers are guarded as
//   smalls first: `make_small(v) == v*16 + 15` is injective and `cmp`'s
//   `a.raw() == b.raw()` fast path is exact, so a plain word compare of two small
//   words is exactly `=:=`. With at least one **constant** operand a word compare
//   is already exact (an immediate atom/`nil` word has low bits `0xB`/`0x3B` and a
//   small word has low nibble `0xF`, while a boxed pointer is even — no cross-class
//   collision — and `cmp`'s exact mode never equates two different classes), which
//   is the shape [`super::inline`] already inlines for `is_eq_exact`/`is_ne_exact`.
// * `==`/`/=` use the **arithmetic** order (`cmp_arith`). It is the one comparison
//   where an immediate is not self-sufficient (`1 == 1.0`, `-0.0 == +0.0`), so a
//   small-int operand's *register* counterpart is guarded as a small. An atom/`nil`
//   constant is `==` only to the identical word, so it needs no guard — the same
//   conservative `Guard::None` rule [`super::inline`]'s `arith_guard` uses.
// * `<`/`=<`/`>`/`>=` use the **arithmetic** order too, and only two *numbers* are
//   word-order exact: `make_small(v)` is monotonic, so the tagged-word **signed**
//   compare is the value order for two smalls (a guarded register included). An
//   atom/`nil` constant is not word-order exact (`cmp_arith` orders the term
//   classes, not their words), so those shapes stay on the helper.
//
// The result is the module's own `true`/`false` atom word; the whole instruction
// deopts when either name is absent from the module's atom table, so a wrong atom is
// never fabricated. `beam_rt` loads every module into one append-only shared atom
// table, so these are the very words `beam_bifs`' `ctx.boolean(..)` would build —
// the interpreter's result term and the fast path's are the same `Eterm`.
//
// `>`/`=<` are lowered directly (an `icmp` over the loaded words), never by swapping
// the operands: `cmp_arith` is **not** antisymmetric for the boxed pairs the guard
// rejects, so a swapped-operand form could flip which operand order a deopt would
// have compared.

/// The eight `erlang` comparison operators, split by the order they use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ComparisonOp {
    /// `erlang:'=:='/2` — the exact order.
    ExactEq,
    /// `erlang:'=/='/2` — the exact order.
    ExactNe,
    /// `erlang:'=='/2` — the arithmetic order.
    ArithEq,
    /// `erlang:'/='/2` — the arithmetic order.
    ArithNe,
    /// `erlang:'<'/2` — the arithmetic order.
    Lt,
    /// `erlang:'=<'/2` — the arithmetic order.
    Le,
    /// `erlang:'>'/2` — the arithmetic order.
    Gt,
    /// `erlang:'>='/2` — the arithmetic order.
    Ge,
}

impl ComparisonOp {
    /// The operator `name` denotes, or `None` for any other BIF.
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "=:=" => Self::ExactEq,
            "=/=" => Self::ExactNe,
            "==" => Self::ArithEq,
            "/=" => Self::ArithNe,
            "<" => Self::Lt,
            "=<" => Self::Le,
            ">" => Self::Gt,
            ">=" => Self::Ge,
            _ => return None,
        })
    }

    /// The operator's BIF spelling — the `module:function` name
    /// [`ExecutionContext::call_bif`](crate::ExecutionContext::call_bif) is called
    /// with. Reported by [`super::report::CodegenReport`] so a test can prove the
    /// fast path was **compiled** rather than silently deopted.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::ExactEq => "=:=",
            Self::ExactNe => "=/=",
            Self::ArithEq => "==",
            Self::ArithNe => "/=",
            Self::Lt => "<",
            Self::Le => "=<",
            Self::Gt => ">",
            Self::Ge => ">=",
        }
    }

    /// The condition on `cmp a, b` (the two loaded operand words, in instruction
    /// order) that means the operator **succeeded**.
    fn success(self) -> IntCC {
        match self {
            Self::ExactEq | Self::ArithEq => IntCC::Equal,
            Self::ExactNe | Self::ArithNe => IntCC::NotEqual,
            Self::Lt => IntCC::SignedLessThan,
            Self::Le => IntCC::SignedLessThanOrEqual,
            Self::Gt => IntCC::SignedGreaterThan,
            Self::Ge => IntCC::SignedGreaterThanOrEqual,
        }
    }

    /// Whether the operator uses the exact (`cmp`) rather than arithmetic
    /// (`cmp_arith`) order.
    fn is_exact(self) -> bool {
        matches!(self, Self::ExactEq | Self::ExactNe)
    }

    /// Whether the operator is an ordering (`<`/`=<`/`>`/`>=`).
    fn is_order(self) -> bool {
        matches!(self, Self::Lt | Self::Le | Self::Gt | Self::Ge)
    }
}

/// A comparison-BIF shape the fast path targets — a *purely syntactic* check that
/// never consults a run-time value (so the compile-time decision is stable).
pub(super) struct ComparisonPlan {
    /// Which operator is called.
    pub(super) op: ComparisonOp,
    /// The first operand's native source.
    a: CmpSrc,
    /// The second operand's native source.
    b: CmpSrc,
    /// The operands the small-int guard covers.
    guard: Guard,
    /// The x-register index the result is written to.
    dst_x: u32,
    /// The raw `Eterm` word of the module's `true` atom.
    truthy: i64,
    /// The raw `Eterm` word of the module's `false` atom.
    falsy: i64,
}

/// The shape test for `instr` (`Instr::Bif` or `Instr::GcBif`), or `None` when the
/// instruction is not one of the eight comparison operators over addressable
/// x-registers / immediate constants.
///
/// The `Fail` label is deliberately **not** inspected: the fast path never produces
/// an error and never branches to `fail`, and a guard rejection deopts to the full
/// interpreter step, which reproduces the fail/raise split for a live label and a
/// `NO_FAIL` (`{f,0}`) alike. Requiring a live fail label would inline nothing —
/// the settled corpus emits every comparison with `Fail = 0`.
pub(super) fn inline_comparison(module: &Module, instr: &Instr) -> Option<ComparisonPlan> {
    let (arity, bif, args, dst) = match instr {
        Instr::Bif {
            arity, bif, args, dst, ..
        } => (*arity, *bif, args, dst),
        Instr::GcBif {
            arity, bif, args, dst, ..
        } => (*arity, *bif, args, dst),
        _ => return None,
    };
    // Resolve the import exactly as the interpreter's `Bif`/`GcBif` arms do: by the
    // strings `call_bif` receives, so the fast path can never lower a call the
    // interpreter routes somewhere else.
    let import = module.imports.get(bif.0 as usize)?;
    if module.atom_name(import.module)? != "erlang" || import.arity != 2 {
        return None;
    }
    if arity != 2 || args.len() != 2 {
        return None;
    }
    let op = ComparisonOp::from_name(module.atom_name(import.function)?)?;
    let dst_x = x_register(dst)?;
    if dst_x >= MAX_REGISTER {
        return None;
    }
    let a = cmp_src(&args[0])?;
    let b = cmp_src(&args[1])?;
    let guard = comparison_guard(op, a, b)?;
    // The result words must be interned: an absent `true`/`false` would have to be
    // fabricated, and the whole instruction stays on the helper instead.
    let (truthy, falsy) = result_words(module)?;
    Some(ComparisonPlan {
        op,
        a,
        b,
        guard,
        dst_x,
        truthy,
        falsy,
    })
}

/// The raw `Eterm` words of the module's `true` and `false` atoms, or `None` when
/// either is absent from its atom table.
fn result_words(module: &Module) -> Option<(i64, i64)> {
    let truthy = module.atom_table.lookup("true")?;
    let falsy = module.atom_table.lookup("false")?;
    Some((
        make_atom(truthy).raw() as i64,
        make_atom(falsy).raw() as i64,
    ))
}

/// Whether `source` is a constant (never a runtime value).
fn is_const(source: CmpSrc) -> bool {
    matches!(source, CmpSrc::Const(_))
}

/// The guard `op` needs over `lhs`/`rhs` for the word compare to be exact, or
/// `None` when the shape is not provably word-exact and must stay on the helper.
fn comparison_guard(op: ComparisonOp, lhs: CmpSrc, rhs: CmpSrc) -> Option<Guard> {
    if op.is_order() {
        // Only two smalls are word-order exact; an atom/`nil` constant (or any
        // bignum/literal operand) keeps the instruction on the helper.
        return Some(match (lhs, rhs) {
            (CmpSrc::Const(TestConst::Immediate(_)), _) | (_, CmpSrc::Const(TestConst::Immediate(_))) => {
                return None
            }
            (CmpSrc::Const(TestConst::Small(_)), CmpSrc::Const(TestConst::Small(_))) => Guard::None,
            (CmpSrc::Const(TestConst::Small(_)), CmpSrc::Reg(_)) => Guard::Rhs,
            (CmpSrc::Reg(_), CmpSrc::Const(TestConst::Small(_))) => Guard::Lhs,
            (CmpSrc::Reg(_), CmpSrc::Reg(_)) => Guard::Both,
        });
    }
    if op.is_exact() {
        // `=:=`/`=/=`: a word compare is exact when at least one operand is a
        // constant immediate; two registers must both be guarded as smalls.
        return Some(match (lhs, rhs) {
            (CmpSrc::Reg(_), CmpSrc::Reg(_)) => Guard::Both,
            _ if is_const(lhs) || is_const(rhs) => Guard::None,
            _ => return None,
        });
    }
    // `==`/`/=` (`cmp_arith`): an atom/`nil` immediate is `==` only to the identical
    // word, so it needs no guard; a small-int constant's register counterpart does.
    Some(match (lhs, rhs) {
        (CmpSrc::Const(TestConst::Immediate(_)), _) | (_, CmpSrc::Const(TestConst::Immediate(_))) => {
            Guard::None
        }
        (CmpSrc::Const(TestConst::Small(_)), CmpSrc::Const(TestConst::Small(_))) => Guard::None,
        (CmpSrc::Const(TestConst::Small(_)), CmpSrc::Reg(_)) => Guard::Rhs,
        (CmpSrc::Reg(_), CmpSrc::Const(TestConst::Small(_))) => Guard::Lhs,
        (CmpSrc::Reg(_), CmpSrc::Reg(_)) => Guard::Both,
    })
}

/// Emit an inlined comparison BIF at `pc`.
///
/// The emitted shape (see the section comment above for why each rule is exact):
///
/// ```text
///   load a                   ; a cached x-register, or an iconst constant
///   load b
///   <small-int guard(s)>     ; only for the operands the word compare needs guarded
///   cmp  a, b -> condition
///   brif condition, true, false
/// true:  store <dst> = true  ; the module's own `true` atom word
///        continue at pc + 1
/// false: store <dst> = false ; the module's own `false` atom word
///        continue at pc + 1
/// slow:  <jit_step_one + continuation, the interpreter's exact semantics>
/// ```
///
/// Every operand is loaded and every guard applied **before** the destination is
/// written, so a destination aliasing an operand reads the original value (matching
/// the interpreter, which reads all arguments before writing the result); the slow
/// path leaves `machine.x` untouched, so a deopt sees exactly the interpreter's
/// pre-instruction state.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_inline_comparison(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    pc: u32,
    code_len: usize,
    func_index: u32,
    rt: Value,
    plan: &ComparisonPlan,
    helper: FuncRef,
    this_func: Value,
    report: &mut CodegenReport,
) {
    let x_regs = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), rt, XREGS_DISP);
    let a = load_cmp_src(builder, x_regs, &plan.a, report);
    let b = load_cmp_src(builder, x_regs, &plan.b, report);

    // A guarded operand that is not an untagged small int makes the word compare
    // inexact (a float, bignum, atom, …), so the whole instruction deopts to the
    // helper rather than fabricating a result from the tagged words.
    let guarded: [Option<Value>; 2] = match plan.guard {
        Guard::None => [None, None],
        Guard::Lhs => [Some(a), None],
        Guard::Rhs => [None, Some(b)],
        Guard::Both => [Some(a), Some(b)],
    };
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

    // The comparison, then the operator's own `true`/`false` word.
    builder.switch_to_block(compare);
    let condition = builder.ins().icmp(plan.op.success(), a, b);
    let taken = builder.create_block();
    let falsy = builder.create_block();
    builder.ins().brif(condition, taken, &[], falsy, &[]);

    builder.switch_to_block(taken);
    store_word(builder, x_regs, plan.dst_x, plan.truthy);
    report.touch(plan.dst_x);
    goto_or_leave(builder, cfg, code_len, func_index, pc + 1);

    builder.switch_to_block(falsy);
    store_word(builder, x_regs, plan.dst_x, plan.falsy);
    report.touch(plan.dst_x);
    goto_or_leave(builder, cfg, code_len, func_index, pc + 1);

    // Rejected (or not lowered): one interpreter step, the BIF's exact semantics.
    if let Some(slow) = slow {
        builder.switch_to_block(slow);
        emit_helper_dispatch(builder, cfg, pc, code_len, helper, rt, this_func);
    }
}

/// Store the raw `Eterm` word `word` into x-register `dst_x`'s `machine.x` slot.
fn store_word(builder: &mut FunctionBuilder, x_regs: Value, dst_x: u32, word: i64) {
    let value = builder.ins().iconst(types::I64, word);
    builder
        .ins()
        .store(MemFlags::trusted(), value, x_regs, x_offset(dst_x));
}
