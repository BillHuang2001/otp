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

//! The baseline JIT's guarded aggregate accessors: the cons-cell readers
//! (`get_list`/`get_hd`/`get_tl`), the tuple readers (`get_tuple_element`,
//! `select_tuple_arity`) and the tuple writer (`set_tuple_element`) behind guards
//! that can only ever *reject* (a non-cons or non-tuple source — or, for the writer, a
//! shared-literal-area target — falls back to `jit_step_one`, which raises the
//! interpreter's exact `{badmatch, Source}` or reports its typed `unsupported`).

use crate::interpreter::LabelMap;
use std::mem::size_of;

use beam_common::Result;
use beam_loader::{Label, Operand, NO_FAIL};
use beam_term::{
    Eterm, ARITYVAL_SUBTAG, CAR_OFFSET, CDR_OFFSET, HEADER_ARITY_OFFS, TAG_HEADER_MASK,
    TAG_LITERAL_PTR, TAG_PRIMARY_BOXED, TAG_PRIMARY_LIST, TAG_PRIMARY_MASK, TAG_PTR_MASK,
};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};
use crate::machine::MAX_REGISTER;

use super::link::Linker;
use super::operands::{constant_word, x_offset, x_register};
use super::{emit_step_and_continue, touch, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2};

// ---------------------------------------------------------------------------
// The cons-cell readers
// ---------------------------------------------------------------------------
//
// `get_list`/`get_hd`/`get_tl` split a cons cell (`emu/instrs.tab` `get_list`
// `:389`, `get_hd` `:398`, `get_tl` `:403`). The interpreter
// (`crate::families::lists`) reads the source, gates on `beam_term::is_list`
// (via `terms::cons_head`/`cons_tail`) and, on success, writes the CAR — then
// the CDR — to the destination operands; a non-cons source *raises*
// `{badmatch, Source}` through `terms::badmatch_reason`.
//
// The subset below is lowered natively and mirrors that branch for branch:
//
// * the source and **every** destination must be an x-register inside the
//   engine's [`MAX_REGISTER`] bound — a `Y` operand is not something the tier
//   can address, and a register index past the bound would read out of
//   `machine.x` (the interpreter step reports the faithful `bad_register`);
// * the guard is `is_list` itself, the C bit trick
//   `(word & (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST)) == 0`: the emitted `and`
//   leaves `0` for a cons cell (primary tag `LIST`) and the mask itself for
//   everything else — `NIL`, a small int, an atom, a boxed term (`[]`, `1`,
//   `foo`, a tuple, …) — so the comparison against the mask is exactly
//   "not a cons cell" and branches to the helper, whose interpreter step raises
//   the exact `{badmatch, Source}`;
// * on success the cell's address is `list_val(word)` (`word & !TAG_PTR_MASK`),
//   the CAR is the word at offset 0 and the CDR the word at the cons cell's
//   second word (`beam_term::{CAR_OFFSET, CDR_OFFSET}`).
//
// The destinations are written **CAR first, then CDR** — the interpreter's write
// order — so a list read whose destinations alias each other (or the source)
// ends with the CDR value, exactly as the interpreter's does.

/// `TAG_PRIMARY_MASK - TAG_PRIMARY_LIST` as an `i64` — the mask of the
/// `is_not_list` bit trick (`beam_term::list::is_not_list`), which is `0x2`.
const NOT_LIST_MASK: i64 = (TAG_PRIMARY_MASK - TAG_PRIMARY_LIST) as i64;

/// `!TAG_PTR_MASK` as an `i64` — the mask `list_val` clears the pointer tag with.
const POINTER_MASK: i64 = !(TAG_PTR_MASK as i64);

/// The byte offset of the CAR (head) word inside a cons cell.
const CAR_DISP: i32 = (size_of::<Eterm>() * CAR_OFFSET) as i32;

/// The byte offset of the CDR (tail) word inside a cons cell.
const CDR_DISP: i32 = (size_of::<Eterm>() * CDR_OFFSET) as i32;

/// Which half of a cons cell a destination receives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Half {
    /// The CAR (head) word — `get_list`'s head destination, `get_hd`.
    Head,
    /// The CDR (tail) word — `get_list`'s tail destination, `get_tl`.
    Tail,
}

/// The x-register operands of a list read the tier lowers natively: the source cons
/// cell and the destinations it writes, in emission order.
#[derive(Clone)]
pub(super) struct ListShape {
    /// The `source` operand's x-register.
    source: u32,
    /// The `(half, destination)` writes, in emission order: the CAR before the CDR
    /// wherever both are written (`get_list`), so aliasing destinations end with the
    /// CDR value exactly as the interpreter's write order does.
    destinations: Vec<(Half, u32)>,
}

impl ListShape {
    /// The x-register indices the shape's native lowering touches, in emission
    /// order (the source first, then each destination).
    pub(super) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(self.destinations.len() + 1);
        registers.push(self.source);
        registers.extend(self.destinations.iter().map(|(_, x)| *x));
        registers
    }

    /// The x-register indices the shape's native lowering **writes** — its
    /// destinations, in emission order (CAR before CDR wherever both are written).
    /// The source is only read.
    ///
    /// This is the block register cache's write set ([`super::Allocation::written`]).
    pub(super) fn writes(&self) -> Vec<u32> {
        self.destinations.iter().map(|(_, x)| *x).collect()
    }
}

/// The inlinable shape of a `get_list Source Head Tail`, or `None` when the whole
/// instruction must stay on the helper.
pub(super) fn get_list_shape(
    source: &Operand,
    head: &Operand,
    tail: &Operand,
) -> Option<ListShape> {
    shape(source, &[(Half::Head, head), (Half::Tail, tail)])
}

/// The inlinable shape of a `get_hd Source Head`, or `None` when the whole
/// instruction must stay on the helper.
pub(super) fn get_hd_shape(source: &Operand, head: &Operand) -> Option<ListShape> {
    shape(source, &[(Half::Head, head)])
}

/// The inlinable shape of a `get_tl Source Tail`, or `None` when the whole
/// instruction must stay on the helper.
pub(super) fn get_tl_shape(source: &Operand, tail: &Operand) -> Option<ListShape> {
    shape(source, &[(Half::Tail, tail)])
}

/// The inlinable shape of a list read that writes `writes` (in that order), or
/// `None` for a shape above the subset.
///
/// The source and every destination must be an x-register inside
/// [`MAX_REGISTER`] — see the section comment above.
fn shape(source: &Operand, writes: &[(Half, &Operand)]) -> Option<ListShape> {
    let source = in_bound_x(source)?;
    let destinations = writes
        .iter()
        .map(|(half, operand)| in_bound_x(operand).map(|x| (*half, x)))
        .collect::<Option<Vec<_>>>()?;
    Some(ListShape {
        source,
        destinations,
    })
}

/// The x-register index `operand` names, if it lies inside [`MAX_REGISTER`].
///
/// An out-of-bound index makes the instruction deopt, so the helper reports the
/// interpreter's exact `bad_register` instead of the emitted code reading past
/// the end of `machine.x`.
fn in_bound_x(operand: &Operand) -> Option<u32> {
    x_register(operand).filter(|x| *x < MAX_REGISTER)
}

/// Emit `get_list Source Head Tail` natively, or report that the block must
/// helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving the CAR in
/// `head` and the CDR in `tail`. Returns `false` **without having emitted a byte**
/// when the operands are not three in-bound x-registers ([`get_list_shape`]) — the
/// caller then emits exactly today's `jit_step_one` step.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_get_list(
    buffer: &mut CodeBuffer,
    source: &Operand,
    head: &Operand,
    tail: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_list_shape(source, head, tail) else {
        return Ok(false);
    };
    emit_list_read(buffer, &shape, allocation, done, func, pc, highest_touched)?;
    Ok(true)
}

/// Emit `get_hd Source Head` natively, or report that the block must helper it
/// (see [`try_inline_get_list`] for the return convention).
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_get_hd(
    buffer: &mut CodeBuffer,
    source: &Operand,
    head: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_hd_shape(source, head) else {
        return Ok(false);
    };
    emit_list_read(buffer, &shape, allocation, done, func, pc, highest_touched)?;
    Ok(true)
}

/// Emit `get_tl Source Tail` natively, or report that the block must helper it
/// (see [`try_inline_get_list`] for the return convention).
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_get_tl(
    buffer: &mut CodeBuffer,
    source: &Operand,
    tail: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_tl_shape(source, tail) else {
        return Ok(false);
    };
    emit_list_read(buffer, &shape, allocation, done, func, pc, highest_touched)?;
    Ok(true)
}

/// Lower an inlinable list read (see the section comment above for why it is
/// exact). The emitted shape:
///
/// ```text
///   mov  r11, <source>      ; the source word: a cached register, or `[rbx + x*8]`
///   mov  r10, NOT_LIST_MASK ; the `is_not_list` bit
///   and  r11, r10
///   cmp  r11, r10           ; the masked word is 0 (a cons cell) or the mask itself
///   je   fallback           ; not a cons cell: the helper raises `{badmatch,Source}`
///   mov  r11, <source>      ; reload (the mask consumed the word)
///   mov  r10, POINTER_MASK
///   and  r11, r10           ; r11 = the cons cell's address, `list_val(word)`
///   mov  r10, [r11 + 0]     ; the CAR           — for a `get_list`, first ...
///   mov  <head>, r10
///   mov  r10, [r11 + 8]     ; the CDR           — ... then the tail
///   mov  <tail>, r10
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spend one `jit_step_one` on the
///    instruction — which either advances and lets the block carry on at
///    `pc + 1`, or exits to the driver with the helper's word>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// A `get_hd` writes only the CAR and a `get_tl` only the CDR (the half each
/// destination receives is [`ListShape::destinations`]' `Half`). Every destination
/// is written through its own representation — a cached register, or its
/// `machine.x` slot (the very slot the exit spill would write) — and the source is
/// read the same way, so the tier's liveness model is unchanged. The fast path
/// never spills.
fn emit_list_read(
    buffer: &mut CodeBuffer,
    shape: &ListShape,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // The guard: `is_not_list(source)` — a non-cons source (`[]`, a small int, an
    // atom, a tuple) branches to the helper, which raises `{badmatch, Source}`.
    load_x(
        buffer,
        shape.source,
        allocation,
        MOVE_SCRATCH,
        highest_touched,
    )?;
    buffer.mov_ri(MOVE_SCRATCH_2, NOT_LIST_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.cmp_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::E, fallback)?;

    // The cell's address: reload the source word (the mask above consumed it) and
    // clear the pointer tag — `list_val(word)`.
    load_x(
        buffer,
        shape.source,
        allocation,
        MOVE_SCRATCH,
        highest_touched,
    )?;
    buffer.mov_ri(MOVE_SCRATCH_2, POINTER_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;

    // The destinations, in emission order — CAR first, then CDR — the interpreter's
    // write order, so aliasing destinations (`head == tail`, or a destination that
    // is the source) end with the CDR value.
    for (half, destination) in &shape.destinations {
        let disp = match half {
            Half::Head => CAR_DISP,
            Half::Tail => CDR_DISP,
        };
        buffer.mov_rm(MOVE_SCRATCH_2, MOVE_SCRATCH, disp)?;
        store_x(
            buffer,
            *destination,
            MOVE_SCRATCH_2,
            allocation,
            highest_touched,
        )?;
    }
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(())
}

/// Load x-register `x` into scratch register `reg` — from its cache register, or
/// straight from its `machine.x` slot, recording the direct access.
fn load_x(
    buffer: &mut CodeBuffer,
    x: u32,
    allocation: &Allocation,
    reg: Reg,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match allocation.slot(x) {
        Some(src) => buffer.mov_rr(reg, src),
        None => {
            buffer.mov_rm(reg, Reg::Rbx, x_offset(x))?;
            touch(highest_touched, x);
            Ok(())
        }
    }
}

/// Store scratch register `src` into x-register `x` — into its cache register, or
/// straight into its `machine.x` slot (the slot the exit spill writes), recording
/// the direct access.
fn store_x(
    buffer: &mut CodeBuffer,
    x: u32,
    src: Reg,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match allocation.slot(x) {
        Some(dst) => buffer.mov_rr(dst, src),
        None => {
            buffer.mov_mr(Reg::Rbx, x_offset(x), src)?;
            touch(highest_touched, x);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// The tuple readers
// ---------------------------------------------------------------------------
//
// `get_tuple_element` (`emu/instrs.tab` `i_get_tuple_element` `:416`) and
// `select_tuple_arity` (`emu/select_instrs.tab` `i_select_tuple_arity` `:111`) read
// a tuple's arityval header. The interpreter (`crate::families::tuples`) gates
// `get_tuple_element` on `terms::tuple_arity(value)` with `element < arity` and
// otherwise *raises* `{badmatch, Source}` through `terms::badmatch_reason`; an
// `arity` switch requires a tuple (a non-tuple is the documented typed
// `unsupported("select_tuple_arity: non-tuple")`), then takes the **first**
// `(Integer(n), Label)` alternative with `n == arity`, else `Fail` — a `NO_FAIL`
// switch raising `{badmatch, Tuple}`.
//
// The subset below is lowered natively and mirrors that branch for branch:
//
// * the source (and, for a tuple read, the destination) must be an x-register
//   inside the engine's [`MAX_REGISTER`] bound — a `Y` operand is not something the
//   tier can address, and a register index past the bound would read out of
//   `machine.x` (the interpreter step reports the faithful `bad_register`);
// * the guard is `beam_term::is_tuple`: the `is_boxed` bit trick
//   `(word & (TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED)) == 0`, the header pointer
//   `word & !TAG_PTR_MASK` (`boxed_val`, the literal-area bit included), and
//   `is_arity_value(hdr)` — `(hdr & TAG_HEADER_MASK) == ARITYVAL_SUBTAG`. That mask
//   is six bits wide, so the guard is a `test`/`jne` on zero rather than the
//   single-bit `cmp` the cons-cell readers use;
// * the arity bound is a plain word compare: a valid arityval has its low
//   `HEADER_ARITY_OFFS` (six) bits zero, so `element < arity` is exactly
//   `hdr >= (element + 1) << HEADER_ARITY_OFFS` — a `mov_ri`/`cmp`/`jl`, with no
//   shift encoder needed;
// * element `i` is the word at `8 * (1 + i)` past the header word.
//
// ANY guard failure branches to the helper, whose interpreter step raises the exact
// `{badmatch, Source}` (a non-tuple or an out-of-range element) or reports the typed
// `unsupported` — the guard can only ever *reject*.

/// `TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED` as an `i64` — the mask of the
/// `is_not_boxed` bit trick (`beam_term::boxed::is_not_boxed`), which is `0x1`.
const NOT_BOXED_MASK: i64 = (TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED) as i64;

/// `TAG_HEADER_MASK` as an `i64` — the six-bit mask `is_arity_value` tests a header
/// word with.
const HEADER_MASK: i64 = TAG_HEADER_MASK as i64;

/// The byte offset of a tuple's header word (a boxed term points *at* it).
const HEADER_DISP: i32 = 0;

/// The third scratch register the tuple guards need, beside [`MOVE_SCRATCH`] and
/// [`MOVE_SCRATCH_2`]: `is_arity_value` must keep the header word and its mask live
/// at once, and — unlike the single-bit primary-tag tests — the six-bit mask cannot
/// be folded into the word being tested.
///
/// It is caller-saved and used strictly inside one instruction's guard, so it never
/// survives a helper call; the block's cache registers are untouched.
const GUARD_SCRATCH: Reg = Reg::R8;

/// The arityval header word of a tuple of arity `n` — `beam_term::make_arityval`
/// (`(n << HEADER_ARITY_OFFS) + ARITYVAL_SUBTAG`). It is both the bound of a tuple
/// read's range guard (`n = element + 1`) and the word an `arity` alternative
/// compares against.
fn arity_header(n: i64) -> i64 {
    (n << HEADER_ARITY_OFFS) + ARITYVAL_SUBTAG as i64
}

/// The byte offset of element `element` inside a tuple, or `None` when it does not
/// fit the emitter's `disp32` addressing (an arity that large cannot exist in a live
/// tuple, so the whole instruction deopts rather than truncating the displacement).
fn element_disp(element: u32) -> Option<i32> {
    i32::try_from(size_of::<Eterm>() as i64 * (i64::from(element) + 1)).ok()
}

/// The x-register operands of a tuple read the tier lowers natively: the source
/// tuple and the destination it writes.
#[derive(Clone, Copy)]
pub(super) struct TupleReadShape {
    /// The `source` operand's x-register.
    source: u32,
    /// The `dst` operand's x-register.
    dst: u32,
    /// The byte offset of the read element from the header word.
    element_disp: i32,
}

impl TupleReadShape {
    /// The x-register indices the shape's native lowering touches, in emission
    /// order (the source first, then the destination).
    pub(super) fn registers(&self) -> Vec<u32> {
        vec![self.source, self.dst]
    }

    /// The x-register index the shape's native lowering **writes** — its
    /// destination. The source tuple is only read.
    ///
    /// This is the block register cache's write set ([`super::Allocation::written`]).
    pub(super) fn writes(&self) -> Vec<u32> {
        vec![self.dst]
    }
}

/// The inlinable shape of a `get_tuple_element Source Element Dst`, or `None` when
/// the whole instruction must stay on the helper.
///
/// The source and the destination must be x-registers inside [`MAX_REGISTER`], and
/// the element's word offset must be addressable — see the section comment above.
/// The element index itself is an immediate, so an out-of-range one is a guard
/// rejection rather than a shape the tier refuses.
pub(super) fn get_tuple_element_shape(
    source: &Operand,
    element: u32,
    dst: &Operand,
) -> Option<TupleReadShape> {
    Some(TupleReadShape {
        source: in_bound_x(source)?,
        dst: in_bound_x(dst)?,
        element_disp: element_disp(element)?,
    })
}

/// Emit `get_tuple_element Source Element Dst` natively, or report that the block
/// must helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, leaving element
/// `element` of the source tuple in `dst`. Returns `false` **without having emitted
/// a byte** when the operands are not two in-bound x-registers
/// ([`get_tuple_element_shape`]) — the caller then emits exactly today's
/// `jit_step_one` step.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_get_tuple_element(
    buffer: &mut CodeBuffer,
    source: &Operand,
    element: u32,
    dst: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_tuple_element_shape(source, element, dst) else {
        return Ok(false);
    };
    emit_tuple_read(
        buffer,
        &shape,
        element,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
    Ok(true)
}

/// The inlinable shape of a `select_tuple_arity` the tier lowers natively.
pub(super) struct AritySwitchPlan {
    /// The scrutinee tuple's x-register.
    tuple: u32,
    /// The `(arityval header word, resolved target)` alternatives, in source order.
    cases: Vec<(i64, (u32, u32))>,
    /// The resolved fail label's `(func, pc)`.
    fail: (u32, u32),
}

impl AritySwitchPlan {
    /// The x-register indices the plan's native lowering touches — the scrutinee is
    /// its only runtime operand (every alternative is an immediate).
    pub(super) fn registers(&self) -> Vec<u32> {
        vec![self.tuple]
    }

    /// An arity switch writes **no** x-register: the scrutinee is only read, every
    /// alternative is an immediate, and every path transfers control.
    pub(super) fn writes(&self) -> Vec<u32> {
        Vec::new()
    }
}

/// The inlinable shape of a `select_tuple_arity`, or `None` for any shape above the
/// subset (see the section comment above): a `NO_FAIL` switch — which *raises* on
/// the no-match path — an unresolved label, a non-x-register scrutinee, or an
/// alternative that is not a non-negative `Integer` with a resolved label.
pub(super) fn select_tuple_arity_plan(
    labels: &LabelMap,
    tuple: &Operand,
    fail: Label,
    destinations: &[(Operand, Label)],
) -> Option<AritySwitchPlan> {
    // A `NO_FAIL` switch *raises* when no alternative matches; the helper is exact.
    if fail == NO_FAIL {
        return None;
    }
    // An unresolved label keeps the interpreter's exact `unknown_label`.
    let fail = labels.resolve(fail).ok()?;
    let tuple = in_bound_x(tuple)?;
    let mut cases = Vec::with_capacity(destinations.len());
    for (candidate, label) in destinations {
        let Operand::Integer(n) = candidate else {
            return None;
        };
        if *n < 0 {
            return None;
        }
        let target = labels.resolve(*label).ok()?;
        cases.push((arity_header(*n), target));
    }
    Some(AritySwitchPlan { tuple, cases, fail })
}

/// Emit `select_tuple_arity Tuple Fail {Arity,Label}*` natively, or report that the
/// whole instruction must be handed to the helper.
///
/// Returns `false` **without having emitted a byte** for any shape
/// [`select_tuple_arity_plan`] refuses, so the caller falls back to exactly today's
/// `jit_step_one`. On success the emitted code **always** transfers control (every
/// path returns a `(func, pc)` word), so the caller ends the block there.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_select_tuple_arity(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    tuple: &Operand,
    fail: Label,
    destinations: &[(Operand, Label)],
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(plan) = select_tuple_arity_plan(labels, tuple, fail, destinations) else {
        return Ok(false);
    };

    let fallback: CodeLabel = buffer.label();
    emit_tuple_header_guard(buffer, plan.tuple, allocation, fallback, highest_touched)?;

    // One `cmp`/`jcc` per alternative, in source order — the interpreter takes the
    // **first** alternative whose arity equals the tuple's, and a word compare on
    // the arityval header is exactly that equality (a valid header's low six bits
    // are zero, so two arities share a header word only when they are equal).
    let case_labels: Vec<CodeLabel> = (0..plan.cases.len()).map(|_| buffer.label()).collect();
    for ((word, _), case_label) in plan.cases.iter().zip(&case_labels) {
        buffer.mov_ri(MOVE_SCRATCH_2, *word)?;
        buffer.cmp_rr(GUARD_SCRATCH, MOVE_SCRATCH_2)?;
        buffer.jcc(Cond::E, *case_label)?;
    }

    // --- no alternative matched: branch to the fail label -------------------
    linker.emit_exit(buffer, allocation, done, plan.fail.0, plan.fail.1)?;

    // --- each alternative: branch to its target ----------------------------
    for ((_, target), case_label) in plan.cases.iter().zip(&case_labels) {
        buffer.bind(*case_label)?;
        linker.emit_exit(buffer, allocation, done, target.0, target.1)?;
    }

    // Rejected (a non-tuple source): one interpreter step, whose typed
    // `unsupported`/raise is exact.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    Ok(true)
}

/// Emit the `is_tuple` guard for x-register `source`: branch to `fallback` unless it
/// holds a boxed term whose header word is an arityval, leaving that header word in
/// [`GUARD_SCRATCH`].
///
/// The guard is `beam_term::is_tuple` exactly — `is_boxed`, the header pointer
/// `word & !TAG_PTR_MASK`, then `is_arity_value(hdr)` (see the section comment
/// above). It can only ever *reject*: `fallback` runs one interpreter step, whose
/// typed error is the ground truth.
fn emit_tuple_header_guard(
    buffer: &mut CodeBuffer,
    source: u32,
    allocation: &Allocation,
    fallback: CodeLabel,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    // `is_boxed(source)`: the masked word is `0` for a boxed term and the mask itself
    // for everything else — `NIL`, a small int, an atom, a cons cell, … .
    load_x(buffer, source, allocation, MOVE_SCRATCH, highest_touched)?;
    buffer.mov_ri(MOVE_SCRATCH_2, NOT_BOXED_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.cmp_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::E, fallback)?;

    // The header pointer: reload the source word (the mask consumed it) and clear
    // the pointer tag — `boxed_val(word)`.
    load_x(buffer, source, allocation, MOVE_SCRATCH, highest_touched)?;
    buffer.mov_ri(MOVE_SCRATCH_2, POINTER_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;

    // `is_arity_value(*ptr)`: the header's low `HEADER_ARITY_OFFS` bits must be the
    // arityval subtag. `test` sets `ZF` for `(hdr & mask) == 0`, which is exactly
    // "the subtag equals `ARITYVAL_SUBTAG`".
    buffer.mov_rm(GUARD_SCRATCH, MOVE_SCRATCH, HEADER_DISP)?;
    buffer.mov_ri(MOVE_SCRATCH_2, HEADER_MASK)?;
    buffer.test_rr(GUARD_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::Ne, fallback)?;
    Ok(())
}

/// Lower an inlinable tuple read (see the section comment above for why it is
/// exact). The emitted shape:
///
/// ```text
///   mov  r11, <source>      ; the source word: a cached register, or `[rbx + x*8]`
///   mov  r10, NOT_BOXED_MASK; the `is_boxed` bit
///   and  r11, r10
///   cmp  r11, r10           ; the masked word is 0 (boxed) or the mask itself
///   je   fallback           ; not boxed: the helper raises `{badmatch, Source}`
///   mov  r11, <source>      ; reload (the mask consumed the word)
///   mov  r10, POINTER_MASK
///   and  r11, r10           ; r11 = the header word's address, `boxed_val(word)`
///   mov  r8,  [r11 + 0]     ; the header word
///   mov  r10, HEADER_MASK
///   test r8,  r10           ; the low six bits must be the arityval subtag
///   jne  fallback           ; not a tuple: the helper raises `{badmatch, Source}`
///   mov  r10, <bound>       ; (element + 1) << HEADER_ARITY_OFFS
///   cmp  r8,  r10           ; the tuple's arity minus `element + 1`
///   jl   fallback           ; element >= arity: the helper raises `{badmatch, Source}`
///   mov  r10, [r11 + 8*(1+element)]   ; the element word
///   mov  <dst>, r10
///   jmp  fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spend one `jit_step_one` on the
///    instruction — which either advances and lets the block carry on at
///    `pc + 1`, or exits to the driver with the helper's word>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// The destination is written through its own representation — a cached register,
/// or its `machine.x` slot (the very slot the exit spill would write) — and the
/// source is read the same way, so the tier's liveness model is unchanged. The fast
/// path never spills.
fn emit_tuple_read(
    buffer: &mut CodeBuffer,
    shape: &TupleReadShape,
    element: u32,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    emit_tuple_header_guard(buffer, shape.source, allocation, fallback, highest_touched)?;

    // `element < arity`: the header word carries the arity above its subtag bits, so
    // the bound is the arityval of `element + 1` and the test is a plain `cmp`. Both
    // operands are non-negative, so the signed `jl` is the word order.
    buffer.mov_ri(MOVE_SCRATCH_2, arity_header(i64::from(element) + 1))?;
    buffer.cmp_rr(GUARD_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::L, fallback)?;

    // The element word, `8 * (1 + element)` past the header word.
    buffer.mov_rm(MOVE_SCRATCH_2, MOVE_SCRATCH, shape.element_disp)?;
    store_x(
        buffer,
        shape.dst,
        MOVE_SCRATCH_2,
        allocation,
        highest_touched,
    )?;
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The tuple writer
// ---------------------------------------------------------------------------
//
// `set_tuple_element` (`emu/instrs.tab:845-851`, `i_set_tuple_element`) overwrites
// one element word of a tuple **in place** — the destructive store the compiler emits
// only for a tuple it has proved uniquely owned. The interpreter
// (`crate::families::tuples::exec`) reads the tuple and the element value, gates the
// tuple on `terms::tuple_arity` with `offset < arity` (otherwise it *raises*
// `{badmatch, Source}` through `terms::badmatch_reason`), refuses a term in a shared
// literal area with the typed `error::unsupported`, and otherwise writes the value
// through `terms::set_tuple_element` at word `1 + offset` past the header — the very
// word `get_tuple_element` with the same index reads.
//
// The subset below is lowered natively and mirrors that branch for branch:
//
// * the tuple must be an x-register inside the engine's [`MAX_REGISTER`] bound — a `Y`
//   operand, a literal operand or a register past the bound deopts (the interpreter
//   step reports the faithful `bad_register`);
// * the element operand must be an immediate constant word
//   ([`super::operands::constant_word`]) or an in-bound x-register: a value that needs
//   heap materialisation (a bignum/float/literal) is not a shape the tier lowers, so
//   it deopts to the interpreter, which materialises it exactly;
// * the guard is the same `is_tuple` bit trick the readers use (`emit_tuple_header_guard`);
//   its rejection branches to the helper, whose interpreter step raises the exact
//   `{badmatch, Source}` — a non-tuple target or an out-of-range `offset`;
// * the literal-area bit ([`TAG_LITERAL_PTR`]) is **also** tested on the source word and
//   *rejects* to the helper: the interpreter refuses a shared-literal-area target with
//   the typed `unsupported`, so the tier must not write it either. Both guards can only
//   ever reject, never corrupt a module's constants.
//
// The store writes the element word at `8 * (1 + offset)` past the header — exactly the
// word the read path loads. There is no write barrier: the frozen term layout is
// immutable except for this destructive update (workspace `CONTEXT.md` §2), so the
// tier's root-set/liveness model is unchanged and no cached register is clobbered.

/// `TAG_LITERAL_PTR` as an `i64` — the literal-area bit `is_literal` tests a boxed/list
/// word with (`beam_term::literal::is_literal`), which a write must reject.
const LITERAL_MASK: i64 = TAG_LITERAL_PTR as i64;

/// The element value a `set_tuple_element` stores.
#[derive(Clone, Copy)]
pub(super) enum TupleWriteValue {
    /// A constant operand, materialised as its raw `Eterm` word.
    Immediate(i64),
    /// An x-register whose value is read into scratch at the store.
    Register(u32),
}

/// The x-register operands of a tuple write the tier lowers natively: the target
/// tuple and the element value it stores.
#[derive(Clone, Copy)]
pub(super) struct TupleWriteShape {
    /// The `tuple` operand's x-register.
    tuple: u32,
    /// The element value to store.
    value: TupleWriteValue,
    /// The byte offset of the written element from the header word.
    element_disp: i32,
}

impl TupleWriteShape {
    /// The x-register indices the shape's native lowering touches, in emission
    /// order (the target tuple first, then the value register when there is one).
    pub(super) fn registers(&self) -> Vec<u32> {
        let mut registers = vec![self.tuple];
        if let TupleWriteValue::Register(x) = self.value {
            registers.push(x);
        }
        registers
    }

    /// A tuple write writes **no** x-register: it stores one element word
    /// destructively into the heap tuple (behind the header/literal guards), and the
    /// target tuple and element value are only read. A cached register holding the
    /// tuple keeps its `machine.x` value, so the write set is empty.
    pub(super) fn writes(&self) -> Vec<u32> {
        Vec::new()
    }
}

/// The inlinable shape of a `set_tuple_element Element Tuple Offset`, or `None` when
/// the whole instruction must stay on the helper.
///
/// The target tuple must be an x-register inside [`MAX_REGISTER`]; the element value
/// must be an immediate constant word or an in-bound x-register (a value needing heap
/// materialisation deopts); the offset's word must be addressable — see the section
/// comment above.
pub(super) fn set_tuple_element_shape(
    element: &Operand,
    tuple: &Operand,
    offset: u32,
) -> Option<TupleWriteShape> {
    Some(TupleWriteShape {
        tuple: in_bound_x(tuple)?,
        value: match x_register(element) {
            Some(x) if x < MAX_REGISTER => TupleWriteValue::Register(x),
            Some(_) => return None,
            None => TupleWriteValue::Immediate(constant_word(element)?),
        },
        element_disp: element_disp(offset)?,
    })
}

/// Emit `set_tuple_element Element Tuple Offset` natively, or report that the block
/// must helper it.
///
/// Returns `true` when the whole guarded sequence was emitted, having stored the
/// element value into the tuple's word `offset`. Returns `false` **without having
/// emitted a byte** when the operands are not an in-bound x-register tuple plus an
/// immediate-or-in-bound-register value ([`set_tuple_element_shape`]) — the caller
/// then emits exactly today's `jit_step_one` step.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_set_tuple_element(
    buffer: &mut CodeBuffer,
    element: &Operand,
    tuple: &Operand,
    offset: u32,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = set_tuple_element_shape(element, tuple, offset) else {
        return Ok(false);
    };
    emit_tuple_write(
        buffer,
        &shape,
        offset,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
    Ok(true)
}

/// Lower an inlinable tuple write (see the section comment above for why it is exact).
/// The emitted shape:
///
/// ```text
///   <the `emit_tuple_header_guard` sequence: the `is_tuple` guard, leaving the
///    header word in GUARD_SCRATCH and the header pointer in MOVE_SCRATCH>
///   mov   r10, <bound>      ; (offset + 1) << HEADER_ARITY_OFFS
///   cmp   r8,  r10          ; the tuple's arity minus `offset + 1`
///   jl    fallback          ; offset >= arity: the helper raises `{badmatch, Source}`
///   mov   r10, <Tuple>      ; reload (the header guard consumed the source word)
///   mov   r8,  LITERAL_MASK
///   test  r10, r8           ; the literal-area bit
///   jne   fallback          ; literal: the helper reports the typed `unsupported`
///   mov   r10, <value>      ; the element value: an immediate word, or a register read
///   mov   [r11 + 8*(1+offset)], r10   ; the destructive store
///   jmp   fast_done
/// fallback:
///   <the `emit_step_and_continue` sequence: spend one `jit_step_one` on the
///    instruction — which either advances and lets the block carry on at
///    `pc + 1`, or exits to the driver with the helper's word>
/// fast_done:
///   <emit the rest of the block>
/// ```
///
/// The value is read into scratch — never into a cache register or `machine.x` — so the
/// tier's liveness model is unchanged: the instruction produces no destination
/// register, and the only x-registers the lowering touches are the target tuple (read)
/// and, for a register value, the element register (read). The fast path never spills.
fn emit_tuple_write(
    buffer: &mut CodeBuffer,
    shape: &TupleWriteShape,
    offset: u32,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    emit_tuple_header_guard(buffer, shape.tuple, allocation, fallback, highest_touched)?;

    // `offset < arity`: the header word carries the arity above its subtag bits, so the
    // bound is the arityval of `offset + 1` and the test is a plain `cmp`. Both operands
    // are non-negative, so the signed `jl` is the word order. (The interpreter checks the
    // arity bound before the literal refusal, and this mirrors that order.)
    buffer.mov_ri(MOVE_SCRATCH_2, arity_header(i64::from(offset) + 1))?;
    buffer.cmp_rr(GUARD_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::L, fallback)?;

    // The literal-area guard: reload the source word — the header guard's `and` cleared
    // the pointer tag *and* the literal bit from `MOVE_SCRATCH`, which must keep the
    // header pointer for the store — and reject `TAG_LITERAL_PTR`. The interpreter
    // refuses a shared-literal-area target rather than writing it, so the tier must too.
    load_x(
        buffer,
        shape.tuple,
        allocation,
        MOVE_SCRATCH_2,
        highest_touched,
    )?;
    buffer.mov_ri(GUARD_SCRATCH, LITERAL_MASK)?;
    buffer.test_rr(MOVE_SCRATCH_2, GUARD_SCRATCH)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // The element value into the scratch register the store reads.
    match shape.value {
        TupleWriteValue::Immediate(word) => buffer.mov_ri(MOVE_SCRATCH_2, word)?,
        TupleWriteValue::Register(x) => {
            load_x(buffer, x, allocation, MOVE_SCRATCH_2, highest_touched)?
        }
    }
    // The destructive store, `8 * (1 + offset)` past the header pointer.
    buffer.mov_mr(MOVE_SCRATCH, shape.element_disp, MOVE_SCRATCH_2)?;
    buffer.jmp(fast_done)?;

    // Rejected (or not lowered): one interpreter step, then continue at `pc + 1`.
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(())
}
