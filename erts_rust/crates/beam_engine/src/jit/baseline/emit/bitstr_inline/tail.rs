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

//! The tail shapes: `bs_test_tail2` and `bs_skip_bits2`.
//!
//! One reads the remaining window without touching it, the other advances it by a
//! compile-time bit count; both take the caller's resolved **fail label** for a rejected
//! window, because only a real label (never `NO_FAIL`, whose raise allocates) is inlined.

use beam_common::Result;
use beam_loader::{Label, Operand, NO_FAIL};
use beam_term::MAX_SMALL;

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::link::{ExitSpill, Linker};
use super::super::operands::Loc;
use super::{
    emit_ctx_guard, emit_step_and_continue, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2,
    SUB_BITS_END_DISP, SUB_BITS_START_DISP,
};

#[cfg(test)]
use super::{record_inlined, SHAPE_SKIP_BITS2, SHAPE_TEST_TAIL2};

// ---------------------------------------------------------------------------
// Shape 3: `bs_test_tail2 { fail, ctx, bits }`
// ---------------------------------------------------------------------------

/// The operands of a `bs_test_tail2` the tier lowers natively.
#[derive(Clone, Copy)]
pub(crate) struct TestTail2Shape {
    /// The match-context operand's location.
    ctx: Loc,
    /// The exact number of remaining bits the test requires.
    bits: u32,
}

impl TestTail2Shape {
    /// The [`TestTail2Shape`] two operands name, or `None` when the context operand is
    /// neither an in-bound x-register nor an addressable `Y` slot.
    ///
    /// Purely syntactic and **label-free** — the fail-label resolution is the caller's —
    /// so the block's Y gate ([`super::super::arith::uses_y`]) derives the same answer as the
    /// emitter without needing the label map.
    pub(crate) fn of(ctx: &Operand, bits: u32) -> Option<TestTail2Shape> {
        Some(TestTail2Shape {
            ctx: Loc::of(ctx)?,
            bits,
        })
    }

    /// The x-register indices the shape touches — the context only.
    pub(crate) fn registers(&self) -> Vec<u32> {
        self.ctx.register().into_iter().collect()
    }

    /// The x-register indices the shape **writes**: none (`bs_test_tail2` only branches).
    pub(crate) fn writes(&self) -> Vec<u32> {
        Vec::new()
    }

    /// The x-register indices the shape **reads** — the context.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.registers()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.ctx.is_y()
    }
}

/// The inlinable shape of a `bs_test_tail2`, or `None` when the whole instruction must
/// stay on the family.
///
/// A `NO_FAIL` fail code *raises* `{badmatch, Subject}` when the tail does not match, and
/// building that reason allocates — so only a real label is inlined. The label must also
/// resolve in the module's map, because the fail transfer needs its `(function, code)`
/// target; an unresolved label keeps the family's exact `unknown_label`.
pub(crate) fn test_tail2_shape(
    labels: &LabelMap,
    fail: Label,
    ctx: &Operand,
    bits: u32,
) -> Option<(TestTail2Shape, (u32, u32))> {
    let shape = TestTail2Shape::of(ctx, bits)?;
    if fail == NO_FAIL {
        return None;
    }
    let target = labels.resolve(fail).ok()?;
    Some((shape, target))
}

/// Emit `bs_test_tail2 Fail Ctx Bits` natively, or report that the block must helper it
/// (see [`try_inline_bs_get_position`] for the return convention).
///
/// The guarded sequence:
///
/// ```text
///   <the match-context guard>          ; not a context -> fallback
///   mov  rsi, [ctx + 16]               ; start
///   mov  rcx, [ctx + 24]               ; end
///   xor  rdx, rdx                      ; the saturating difference
///   cmp  rcx, rsi ; jb after
///   mov  rdx, rcx ; sub rdx, rsi       ; end - start  (only when end >= start)
/// after:
///   cmp  rdx, Bits ; jne fail_arm
///   jmp  fast_done
/// fail_arm:
///   <the linker's exit to the fail label>   ; a terminator on that path only
/// ```
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_test_tail2(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: Label,
    ctx: &Operand,
    bits: u32,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, target)) = test_tail2_shape(labels, fail, ctx, bits) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_TEST_TAIL2);

    let fallback: CodeLabel = buffer.label();
    let fail_arm: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();
    let after: CodeLabel = buffer.label();

    emit_ctx_guard(
        buffer,
        shape.ctx,
        allocation,
        fallback,
        highest_touched,
    )?;

    // `end.saturating_sub(start) == bits` — the family's exact test.
    buffer.mov_rm(Reg::Rsi, MOVE_SCRATCH, SUB_BITS_START_DISP)?;
    buffer.mov_rm(Reg::Rcx, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
    buffer.xor_rr(Reg::Rdx, Reg::Rdx)?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rsi)?;
    buffer.jcc(Cond::B, after)?;
    buffer.mov_rr(Reg::Rdx, Reg::Rcx)?;
    buffer.sub_rr(Reg::Rdx, Reg::Rsi)?;
    buffer.bind(after)?;
    buffer.mov_ri(MOVE_SCRATCH_2, i64::from(shape.bits))?;
    buffer.cmp_rr(Reg::Rdx, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::Ne, fail_arm)?;
    buffer.jmp(fast_done)?;

    buffer.bind(fail_arm)?;
    linker.emit_exit(
        buffer,
        allocation,
        done,
        target.0,
        target.1,
        pc,
        ExitSpill::Required,
    )?;

    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Shape 4: `bs_skip_bits2 { fail, ctx, size, unit, flags }`
// ---------------------------------------------------------------------------

/// The operands of a `bs_skip_bits2` the tier lowers natively.
#[derive(Clone, Copy)]
pub(crate) struct SkipBits2Shape {
    /// The match-context operand's location.
    ctx: Loc,
    /// `size * unit`, the compile-time number of bits to skip.
    nbits: usize,
}

impl SkipBits2Shape {
    /// The [`SkipBits2Shape`] three operands name, or `None` when the whole instruction
    /// must stay on the family.
    ///
    /// Only a **constant** segment size is inlined (`Operand::Integer`/`Character`, the
    /// form the compiler emits for `<<X:N>>`), because the emitted compare is against a
    /// compile-time `nbits`. The size must be a non-negative small integer —
    /// `read_size`'s accept rule — and `size * unit` must fit both `usize` and an `i64`
    /// immediate. Purely syntactic and **label-free** (see
    /// [`StartMatchShape::of`] for why that matters to the block's Y gate).
    pub(crate) fn of(ctx: &Operand, size: &Operand, unit: u32) -> Option<SkipBits2Shape> {
        let size = match size {
            Operand::Integer(n) if (0..=MAX_SMALL).contains(n) => *n as usize,
            Operand::Character(c) => *c as usize,
            _ => return None,
        };
        let nbits = size.checked_mul(unit as usize)?;
        if nbits > i64::MAX as usize {
            return None;
        }
        Some(SkipBits2Shape {
            ctx: Loc::of(ctx)?,
            nbits,
        })
    }

    /// The x-register indices the shape touches — the context only.
    pub(crate) fn registers(&self) -> Vec<u32> {
        self.ctx.register().into_iter().collect()
    }

    /// The x-register indices the shape **writes**: none (the commit stores into the
    /// context block's `start` word).
    pub(crate) fn writes(&self) -> Vec<u32> {
        Vec::new()
    }

    /// The x-register indices the shape **reads** — the context.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.registers()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.ctx.is_y()
    }
}

/// The inlinable shape of a `bs_skip_bits2`, or `None` when the whole instruction must
/// stay on the family.
///
/// Only a **constant** segment size is inlined (`Operand::Integer`/`Character`, the form
/// the compiler emits for `<<X:N>>`), because the emitted compare is against a compile-time
/// `nbits`. The size must be a non-negative small integer — `read_size`'s accept rule — and
/// `size * unit` must fit both `usize` and an `i64` immediate. A `NO_FAIL` fail code raises,
/// so only a resolvable real label is inlined; `flags` is ignored by the family and so is
/// ignored here.
pub(crate) fn skip_bits2_shape(
    labels: &LabelMap,
    fail: Label,
    ctx: &Operand,
    size: &Operand,
    unit: u32,
) -> Option<(SkipBits2Shape, (u32, u32))> {
    let shape = SkipBits2Shape::of(ctx, size, unit)?;
    if fail == NO_FAIL {
        return None;
    }
    let target = labels.resolve(fail).ok()?;
    Some((shape, target))
}

/// Emit `bs_skip_bits2 Fail Ctx Size Unit Flags` natively, or report that the block must
/// helper it (see [`try_inline_bs_get_position`] for the return convention).
///
/// The guarded sequence:
///
/// ```text
///   <the match-context guard>       ; not a context -> fallback
///   mov  rsi, [ctx + 16]            ; start
///   mov  rdx, [ctx + 24]            ; end
///   mov  rcx, Nbits
///   add  rcx, rsi ; jb fail_arm     ; `start.checked_add(nbits)` overflowed
///   cmp  rcx, rdx ; ja fail_arm     ; `next > end`  (note: > , not >=)
///   mov  [ctx + 16], rcx            ; the commit, one word store
///   jmp  fast_done
/// ```
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_skip_bits2(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: Label,
    ctx: &Operand,
    size: &Operand,
    unit: u32,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, target)) = skip_bits2_shape(labels, fail, ctx, size, unit) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_SKIP_BITS2);

    let fallback: CodeLabel = buffer.label();
    let fail_arm: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    emit_ctx_guard(
        buffer,
        shape.ctx,
        allocation,
        fallback,
        highest_touched,
    )?;

    buffer.mov_rm(Reg::Rsi, MOVE_SCRATCH, SUB_BITS_START_DISP)?;
    buffer.mov_rm(Reg::Rdx, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
    buffer.mov_ri(Reg::Rcx, shape.nbits as i64)?;
    // `start.checked_add(nbits)` — the carry flag is the overflow.
    buffer.add_rr(Reg::Rcx, Reg::Rsi)?;
    buffer.jcc(Cond::B, fail_arm)?;
    // `next > end` — a match failure, **not** a fallback: the family takes the fail label.
    buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::A, fail_arm)?;
    buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rcx)?;
    buffer.jmp(fast_done)?;

    buffer.bind(fail_arm)?;
    linker.emit_exit(
        buffer,
        allocation,
        done,
        target.0,
        target.1,
        pc,
        ExitSpill::Required,
    )?;

    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}

