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

//! The aggregate *shape* reads the tier inlines: the exact-value switch
//! (`select_val`) and the guarded tuple (`get_tuple_element`,
//! `select_tuple_arity`) and list (`get_list`/`get_hd`/`get_tl`) reads.
//!
//! Like the sibling [`super::inline`] module, every one of these is a shape test
//! first: a shape above the subset deopts the whole instruction to the helper,
//! and a run-time guard rejection (a non-tuple, a non-cons, an out-of-range
//! element) deopts to the helper, whose interpreter step is the family's exact
//! semantics (including its exact typed error / `{badmatch, V}` raise).

use crate::interpreter::LabelMap;
use std::mem;

use beam_loader::{Label, Operand, NO_FAIL};
use beam_term::{
    Eterm, ARITYVAL_SUBTAG, HEADER_ARITY_OFFS, TAG_HEADER_MASK, TAG_PRIMARY_BOXED,
    TAG_PRIMARY_MASK, TAG_PTR_MASK,
};
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{types, FuncRef, InstBuilder, MemFlags, Value};
use cranelift_frontend::FunctionBuilder;

use crate::machine::MAX_REGISTER;

use super::codegen::{
    emit_helper_dispatch, emit_target_branch, goto_or_leave, x_offset, Cfg, XREGS_DISP,
};
use super::inline::{cmp_src, constant_word, load_cmp_src, x_register, CmpSrc, LIST_MASK};
use super::report::CodegenReport;

// ---------------------------------------------------------------------------
// The exact-value switch (`select_val`)
// ---------------------------------------------------------------------------
//
// `select_val Value Fail {Value,Label}*` decides every compiled `case`/`receive`
// arm whose patterns are literal constants — one of the most frequently executed
// branching instructions after `test`. The interpreter (`crate::interpreter`'s
// `Instr::SelectVal` arm) scans the alternatives **in order** and takes the first
// whose candidate is **exactly** (`=:=`) equal to the scrutinee
// (`cmp(a, b, ..).is_eq()`), falling through to `Fail` when none match (`NO_FAIL`
// raises).
//
// The subset below mirrors that scan branch for branch and — like the baseline
// tier's `select_val_plan` (`crate::jit::baseline::emit`) — needs **no** run-time
// guard, because `=:=` over immediates *is* a plain raw-word compare: distinct
// immediate classes have disjoint words (a small int's low nibble is `0xF`, an
// atom's low six bits are `0xB`, `nil` is `0x3B`), and a boxed or list pointer —
// whose low bits are even / `0b01` — can never collide with an immediate word, so
// a float/atom/bignum scrutinee is not `=:=` to an immediate unless the words
// coincide (the same argument the baseline tier documents).
//
// * the scrutinee must be a value the tier can read word-exactly — an x-register
//   inside `MAX_REGISTER`, or an immediate constant;
// * **every** alternative must be an immediate constant (a small int in the 60-bit
//   range, an atom, or `nil`) with a **resolved** target — a float, bignum, literal
//   or register alternative would need a term compare, not a word compare, so the
//   whole instruction deopts;
// * the fail label must resolve (the interpreter's exact `unknown_label` otherwise).
//
// A `NO_FAIL` switch raises on the no-match path (the helper does that exactly) and
// deopts, as does every shape above the subset.

/// The inlinable shape of a `select_val`, or `None` when the whole instruction must
/// stay on the helper.
pub(super) struct SelectValPlan {
    /// The scrutinee's native source (an x-register or an immediate constant).
    scrutinee: CmpSrc,
    /// The `(constant word, resolved target)` alternatives, in source order.
    cases: Vec<(i64, (u32, u32))>,
    /// The resolved fail label's `(func, pc)`.
    fail: (u32, u32),
}

/// The inlinable shape of a `select_val`, or `None` for any shape above the subset
/// (a `NO_FAIL` switch, a non-immediate or unresolved alternative, a scrutinee the
/// tier cannot read word-exactly).
pub(super) fn inline_select_val(
    value: &Operand,
    fail: Label,
    pairs: &[(Operand, Label)],
    labels: &LabelMap,
) -> Option<SelectValPlan> {
    // A `NO_FAIL` switch *raises* when no alternative matches; the helper is exact.
    if fail == NO_FAIL {
        return None;
    }
    // An unresolved label keeps the interpreter's exact `unknown_label`.
    let fail = labels.resolve(fail).ok()?;
    // The scrutinee must be an x-register in bound, or an immediate constant.
    let scrutinee = cmp_src(value)?;
    // Every alternative must be an immediate constant with a resolved target.
    let mut cases = Vec::with_capacity(pairs.len());
    for (candidate, label) in pairs {
        let word = constant_word(candidate)?;
        let target = labels.resolve(*label).ok()?;
        cases.push((word, target));
    }
    Some(SelectValPlan {
        scrutinee,
        cases,
        fail,
    })
}

/// Emit an inlined `select_val`: load the scrutinee once, then one word compare per
/// alternative — each match branching to its resolved target — and, when none match,
/// the resolved fail target. Exactly the interpreter's ordered scan.
pub(super) fn emit_inline_select_val(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    code_len: usize,
    func_index: u32,
    rt: Value,
    plan: &SelectValPlan,
    report: &mut CodegenReport,
) {
    let x_regs = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), rt, XREGS_DISP);
    let scrutinee = load_cmp_src(builder, x_regs, &plan.scrutinee, report);

    // One compare-and-branch per alternative, in source order, so the first match
    // wins — exactly as the interpreter's `break` does.
    let mut cursor = builder.create_block();
    builder.ins().jump(cursor, &[]);
    for (word, target) in &plan.cases {
        builder.switch_to_block(cursor);
        let candidate = builder.ins().iconst(types::I64, *word);
        let hit = builder.ins().icmp(IntCC::Equal, scrutinee, candidate);
        let matched = builder.create_block();
        let miss = builder.create_block();
        builder.ins().brif(hit, matched, &[], miss, &[]);

        builder.switch_to_block(matched);
        emit_target_branch(builder, cfg, code_len, func_index, *target);

        cursor = miss;
    }

    // No alternative matched (or there are none): the fail target.
    builder.switch_to_block(cursor);
    emit_target_branch(builder, cfg, code_len, func_index, plan.fail);
}
// ---------------------------------------------------------------------------
// The guarded tuple reads (`get_tuple_element` / `select_tuple_arity`)
// ---------------------------------------------------------------------------
//
// Both read a tuple's shape: `get_tuple_element` fetches element `i`, and
// `select_tuple_arity` switches on the arity. The interpreter's family
// (`crate::families::tuples`) is the *safety net*: it does `terms::tuple_arity`
// (an `is_boxed` test, then `is_arity_value(header)`, then `header >> 6`) and only
// then reads, raising `{badmatch, Source}` (built by
// `terms::badmatch_reason_rooted`) when the source is not a tuple or the element is
// out of range. `select_tuple_arity` treats a non-tuple exactly like an arity that
// matches no alternative — it jumps to the instruction's `fail` label — and only a
// `NO_FAIL` switch raises `{badmatch, Source}` on that path.
//
// The native path reproduces exactly that: a run-time guard accepts only a tuple
// whose arity satisfies the read, and every rejection — a non-tuple, a too-short
// tuple, an out-of-bound x-register, a `Y` destination — branches to the helper,
// whose interpreter step is the family's exact semantics. The header load is only
// ever issued **behind** the boxed guard, so a non-tuple source is never
// dereferenced.

/// The byte size of one term word (an [`Eterm`] is one machine word).
const WORD_BYTES: i32 = mem::size_of::<Eterm>() as i32;

/// `beam_term::is_boxed`'s mask — the C bit trick `TAG_PRIMARY_MASK -
/// TAG_PRIMARY_BOXED` (`0x1`), so `(word & BOXED_MASK) == 0` is exactly
/// `is_boxed(word)`. The same mask covers a literal-area pointer (whose low bit is
/// still clear), which is what lets the header load be guarded by it.
const BOXED_MASK: i64 = (TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED) as i64;

/// `beam_term::boxed_val`/`list_val`'s pointer mask: `!TAG_PTR_MASK` (`0x7`).
const PTR_MASK: i64 = !(TAG_PTR_MASK as i64);

/// `beam_term::HEADER_ARITY_OFFS` as an `i64` — the shift that turns an arityval
/// header into its arity.
const ARITY_SHIFT: i64 = HEADER_ARITY_OFFS as i64;

/// `beam_term::TAG_HEADER_MASK` as an `i64` — the low-six-bits mask
/// `is_arity_value` tests with.
const HEADER_MASK: i64 = TAG_HEADER_MASK as i64;

/// The byte offset of a tuple's first element (word 0 is the arityval header).
const TUPLE_ELEM_BASE: i32 = WORD_BYTES;

/// The byte offset of a cons cell's `CDR` (its `CAR` is at offset 0).
const CDR_OFFSET: i32 = WORD_BYTES;

/// The x-register an in-bound x-register operand names, or `None` — a `Y`/float
/// register, a literal, or an index past the engine's [`MAX_REGISTER`] bound keeps
/// the instruction on the helper (whose interpreter step surfaces the exact
/// `bad_register` / operand error).
///
/// Serves both a **source** (a tuple/list read must come from a register: a constant
/// immediate is never a tuple or a cons cell, and a literal/`Y`/float operand is not
/// word-readable here) and a **destination**.
pub(super) fn in_bound_x(operand: &Operand) -> Option<u32> {
    let x = x_register(operand)?;
    (x < MAX_REGISTER).then_some(x)
}

/// Load the word at `[word & !TAG_PTR_MASK + offset]` — the `boxed_val`/
/// `list_val` accessor for a boxed term or a cons cell.
pub(super) fn load_at(builder: &mut FunctionBuilder, word: Value, offset: i32) -> Value {
    let ptr = builder.ins().band_imm(word, PTR_MASK);
    builder
        .ins()
        .load(types::I64, MemFlags::trusted(), ptr, offset)
}

/// The inlinable shape of a `get_tuple_element`, or `None` when it stays on the
/// helper.
pub(super) struct TupleElementPlan {
    /// The source x-register.
    src_x: u32,
    /// The 0-based element index.
    element: u32,
    /// The destination x-register.
    dst_x: u32,
}

/// The inlinable shape of a `get_tuple_element`, or `None` for a non-x source or a
/// non-x destination (the helper is exact for those).
pub(super) fn inline_get_tuple_element(source: &Operand, element: u32, dst: &Operand) -> Option<TupleElementPlan> {
    Some(TupleElementPlan {
        src_x: in_bound_x(source)?,
        element,
        dst_x: in_bound_x(dst)?,
    })
}

/// Emit the inlined `get_tuple_element`: guard the source as a tuple with
/// `arity > element`, then read `[ptr + 8*(1+element)]` into the destination; a
/// rejected source deopts to the helper (whose `{badmatch, Source}` is the
/// interpreter's exact raise).
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_inline_get_tuple_element(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    pc: u32,
    code_len: usize,
    func_index: u32,
    rt: Value,
    plan: &TupleElementPlan,
    helper: FuncRef,
    this_func: Value,
    report: &mut CodegenReport,
) {
    let x_regs = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), rt, XREGS_DISP);
    report.touch(plan.src_x);
    let word = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), x_regs, x_offset(plan.src_x));

    // `is_boxed(word)`: `(word & 0x1) == 0`. A non-boxed source is never a tuple, so
    // it deopts before the header is ever dereferenced.
    let boxed = builder.ins().band_imm(word, BOXED_MASK);
    let is_boxed = builder.ins().icmp_imm(IntCC::Equal, boxed, 0);
    let shape = builder.create_block();
    let slow = builder.create_block();
    builder.ins().brif(is_boxed, shape, &[], slow, &[]);

    // `is_arity_value(header) && (header >> 6) > element`.
    builder.switch_to_block(shape);
    let header = load_at(builder, word, 0);
    let subtag = builder.ins().band_imm(header, HEADER_MASK);
    let is_arity = builder
        .ins()
        .icmp_imm(IntCC::Equal, subtag, ARITYVAL_SUBTAG as i64);
    let arity = builder.ins().ushr_imm(header, ARITY_SHIFT);
    let long_enough = builder
        .ins()
        .icmp_imm(IntCC::UnsignedGreaterThan, arity, plan.element as i64);
    let ok = builder.ins().band(is_arity, long_enough);
    let read = builder.create_block();
    builder.ins().brif(ok, read, &[], slow, &[]);

    // Success: read the element (the guard proved `element < arity`) and continue.
    builder.switch_to_block(read);
    let element = load_at(
        builder,
        word,
        TUPLE_ELEM_BASE + (plan.element as i32) * WORD_BYTES,
    );
    builder
        .ins()
        .store(MemFlags::trusted(), element, x_regs, x_offset(plan.dst_x));
    report.touch(plan.dst_x);
    goto_or_leave(builder, cfg, code_len, func_index, pc + 1);

    // Rejected: one interpreter step, the family's exact semantics.
    builder.switch_to_block(slow);
    emit_helper_dispatch(builder, cfg, pc, code_len, helper, rt, this_func);
}

/// The inlinable shape of a `select_tuple_arity`, or `None` when it stays on the
/// helper.
pub(super) struct SelectTupleArityPlan {
    /// The source x-register.
    src_x: u32,
    /// The `(arity value, resolved target)` alternatives, in source order.
    cases: Vec<(i64, (u32, u32))>,
    /// The resolved fail label's `(func, pc)`.
    fail: (u32, u32),
}

/// The inlinable shape of a `select_tuple_arity`, or `None` for any shape above the
/// subset: a `NO_FAIL` switch (whose no-match path *raises* `{badmatch, Tuple}`), an
/// unresolved fail label or integer alternative, or a non-register source.
///
/// A non-integer or negative alternative can never equal an arity (the interpreter
/// skips it) and contributes nothing; every other `Integer(n)` alternative must have
/// a resolved target, because a run-time match would take it.
pub(super) fn inline_select_tuple_arity(
    tuple: &Operand,
    fail: Label,
    destinations: &[(Operand, Label)],
    labels: &LabelMap,
) -> Option<SelectTupleArityPlan> {
    // A `NO_FAIL` switch raises `{badmatch, Tuple}` when no arity matches, which the
    // engine cannot express inline; the helper is exact.
    if fail == NO_FAIL {
        return None;
    }
    let fail = labels.resolve(fail).ok()?;
    let src_x = in_bound_x(tuple)?;
    let mut cases = Vec::new();
    for (candidate, label) in destinations {
        let Operand::Integer(n) = candidate else {
            continue;
        };
        if *n < 0 {
            continue;
        }
        let target = labels.resolve(*label).ok()?;
        cases.push((*n, target));
    }
    Some(SelectTupleArityPlan {
        src_x,
        cases,
        fail,
    })
}

/// Emit the inlined `select_tuple_arity`: guard the source as a tuple, then one
/// arity compare per alternative — each match branching to its resolved target — and,
/// when none match, the resolved fail target; a rejected source (a non-tuple) deopts to
/// the helper, whose interpreter step is the family's exact semantics (the `fail`
/// jump).
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_inline_select_tuple_arity(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    pc: u32,
    code_len: usize,
    func_index: u32,
    rt: Value,
    plan: &SelectTupleArityPlan,
    helper: FuncRef,
    this_func: Value,
    report: &mut CodegenReport,
) {
    let x_regs = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), rt, XREGS_DISP);
    report.touch(plan.src_x);
    let word = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), x_regs, x_offset(plan.src_x));

    let boxed = builder.ins().band_imm(word, BOXED_MASK);
    let is_boxed = builder.ins().icmp_imm(IntCC::Equal, boxed, 0);
    let shape = builder.create_block();
    let slow = builder.create_block();
    builder.ins().brif(is_boxed, shape, &[], slow, &[]);

    builder.switch_to_block(shape);
    let header = load_at(builder, word, 0);
    let subtag = builder.ins().band_imm(header, HEADER_MASK);
    let is_arity = builder
        .ins()
        .icmp_imm(IntCC::Equal, subtag, ARITYVAL_SUBTAG as i64);
    let switch = builder.create_block();
    builder.ins().brif(is_arity, switch, &[], slow, &[]);

    // The arity switch, in source order (the first matching alternative wins).
    builder.switch_to_block(switch);
    let arity = builder.ins().ushr_imm(header, ARITY_SHIFT);
    for (n, target) in &plan.cases {
        let hit = builder.ins().icmp_imm(IntCC::Equal, arity, *n);
        let matched = builder.create_block();
        let miss = builder.create_block();
        builder.ins().brif(hit, matched, &[], miss, &[]);
        builder.switch_to_block(matched);
        emit_target_branch(builder, cfg, code_len, func_index, *target);
        builder.switch_to_block(miss);
    }

    // No arity matched (or there are no alternatives): the fail target.
    emit_target_branch(builder, cfg, code_len, func_index, plan.fail);

    // Rejected: one interpreter step, the family's exact semantics.
    builder.switch_to_block(slow);
    emit_helper_dispatch(builder, cfg, pc, code_len, helper, rt, this_func);
}

// ---------------------------------------------------------------------------
// The guarded list reads (`get_list` / `get_hd` / `get_tl`)
// ---------------------------------------------------------------------------
//
// All three read a cons cell's `CAR`/`CDR`. The interpreter's family
// (`crate::families::lists`) is the *safety net*: it requires `terms::cons_head`/
// `cons_tail` — with `beam_term::is_list` (the C bit trick `(word & 0x2) == 0`;
// **`NIL` is not a list**) as the guard — and raises `{badmatch, Source}` for
// anything else (`[]`, an integer, a tuple, …).
//
// The native path reproduces exactly that: a run-time guard accepts only a cons
// cell, and a rejected source branches to the helper, whose interpreter step raises
// the family's exact `{badmatch, Source}`. The cell loads are only issued behind the
// guard, so a non-cons source is never dereferenced.

/// The inlinable shape of a list read, or `None` when it stays on the helper.
pub(super) struct ListReadPlan {
    /// The source x-register.
    src_x: u32,
    /// The destination x-register for the head (`CAR`), when the read produces one.
    head: Option<u32>,
    /// The destination x-register for the tail (`CDR`), when the read produces one.
    tail: Option<u32>,
}

/// The inlinable shape of a `get_list`/`get_hd`/`get_tl`, or `None` for a non-x
/// source or a non-x destination (the helper is exact for those).
pub(super) fn inline_list_read(
    source: &Operand,
    head: Option<&Operand>,
    tail: Option<&Operand>,
) -> Option<ListReadPlan> {
    let head = match head {
        Some(operand) => Some(in_bound_x(operand)?),
        None => None,
    };
    let tail = match tail {
        Some(operand) => Some(in_bound_x(operand)?),
        None => None,
    };
    Some(ListReadPlan {
        src_x: in_bound_x(source)?,
        head,
        tail,
    })
}

/// Emit an inlined list read: guard the source as a cons cell, then read its
/// `CAR`/`CDR` into the requested destinations **in the interpreter's order** (head
/// before tail, so an aliasing `Head == Tail` destination keeps the interpreter's
/// "tail wins" result); a rejected source deopts to the helper (whose
/// `{badmatch, Source}` is the interpreter's exact raise).
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_inline_list_read(
    builder: &mut FunctionBuilder,
    cfg: &mut Cfg,
    pc: u32,
    code_len: usize,
    func_index: u32,
    rt: Value,
    plan: &ListReadPlan,
    helper: FuncRef,
    this_func: Value,
    report: &mut CodegenReport,
) {
    let x_regs = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), rt, XREGS_DISP);
    report.touch(plan.src_x);
    let word = builder
        .ins()
        .load(types::I64, MemFlags::trusted(), x_regs, x_offset(plan.src_x));

    // `is_list(word)`: `(word & 0x2) == 0` (and `NIL`, whose low two bits are `0b11`,
    // fails it).
    let not_list = builder.ins().band_imm(word, LIST_MASK);
    let is_list = builder.ins().icmp_imm(IntCC::Equal, not_list, 0);
    let read = builder.create_block();
    let slow = builder.create_block();
    builder.ins().brif(is_list, read, &[], slow, &[]);

    builder.switch_to_block(read);
    if let Some(head) = plan.head {
        let car = load_at(builder, word, 0);
        builder
            .ins()
            .store(MemFlags::trusted(), car, x_regs, x_offset(head));
        report.touch(head);
    }
    if let Some(tail) = plan.tail {
        let cdr = load_at(builder, word, CDR_OFFSET);
        builder
            .ins()
            .store(MemFlags::trusted(), cdr, x_regs, x_offset(tail));
        report.touch(tail);
    }
    goto_or_leave(builder, cfg, code_len, func_index, pc + 1);

    // Rejected: one interpreter step, the family's exact semantics.
    builder.switch_to_block(slow);
    emit_helper_dispatch(builder, cfg, pc, code_len, helper, rt, this_func);
}
