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

//! The position shapes: `bs_get_position` and `bs_set_position`.
//!
//! Both are **context-only** lowerings over an `ErlSubBits`' `start` word — `get` makes
//! the position term out of it, `set` overwrites it and writes nothing else — so neither
//! has a destination to order against the commit (see `super`'s "M before W").

use beam_common::Result;
use beam_loader::Operand;
use beam_term::MAX_SMALL;

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::operands::{Loc, Y_ADDR_SCRATCH};
use super::{
    emit_ctx_guard, emit_fallback_and_done, Allocation, HEADER_DISP, IMMED1_MASK, MOVE_SCRATCH,
    MOVE_SCRATCH_2, NOT_BOXED_MASK, POINTER_MASK, SMALL_TAG, SUB_BITS_HEADER, SUB_BITS_START_DISP,
};

#[cfg(test)]
use super::{record_inlined, SHAPE_GET_POSITION, SHAPE_SET_POSITION};

// ---------------------------------------------------------------------------
// Shape 1: `bs_get_position { ctx, dst, live }`
// ---------------------------------------------------------------------------

/// The operands of a `bs_get_position` the tier lowers natively.
#[derive(Clone, Copy)]
pub(crate) struct GetPositionShape {
    /// The match-context operand's location.
    ctx: Loc,
    /// The destination the position term is written to.
    dst: Loc,
}

impl GetPositionShape {
    /// The x-register indices the shape touches — the context and the destination.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(2);
        registers.extend(self.ctx.register());
        registers.extend(self.dst.register());
        registers
    }

    /// The x-register indices the shape **writes** — the destination only.
    pub(crate) fn writes(&self) -> Vec<u32> {
        self.dst.register().into_iter().collect()
    }

    /// The x-register indices the shape **reads through the block register cache** — the
    /// context only.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.ctx.register().into_iter().collect()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.ctx.is_y() || self.dst.is_y()
    }
}

/// The inlinable shape of a `bs_get_position`, or `None` when the whole instruction must
/// stay on the helper.
pub(crate) fn get_position_shape(ctx: &Operand, dst: &Operand) -> Option<GetPositionShape> {
    Some(GetPositionShape {
        ctx: Loc::of(ctx)?,
        dst: Loc::of(dst)?,
    })
}

/// Emit `bs_get_position Ctx Dst Live` natively, or report that the block must helper it.
///
/// Returns `true` when the whole guarded sequence was emitted. Returns `false` **without
/// having emitted a byte** when an operand is not an in-bound x-register or an
/// addressable `Y` slot — the caller then emits exactly today's `jit_step_one` step.
///
/// The guarded sequence:
///
/// ```text
///   <Dst's Y-window guard>            ; before any store, so a rejection writes nothing
///   <the match-context guard>         ; not a context -> fallback
///   mov  rcx, [ctx + 16]              ; start
///   cmp  rcx, MAX_SMALL ; ja fallback ; a position beyond the small range -> the
///                                     ;   family's whole-context snapshot (allocating)
///   shl  rcx, 4                       ; make_small(start)
///   or   rcx, TAG_IMMED1_SMALL
///   mov  <Dst>, rcx
///   jmp  fast_done
/// ```
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_get_position(
    buffer: &mut CodeBuffer,
    ctx: &Operand,
    dst: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = get_position_shape(ctx, dst) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_GET_POSITION);

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // The destination's frame-window guard first: a rejection must leave nothing written.
    shape
        .dst
        .guard_store(buffer, Y_ADDR_SCRATCH, MOVE_SCRATCH_2, fallback)?;
    emit_ctx_guard(
        buffer,
        shape.ctx,
        allocation,
        fallback,
        highest_touched,
    )?;

    // `position_term(start)` = `make_small(start)` when the position fits a small integer;
    // otherwise the family publishes a whole-context snapshot, which allocates — that path
    // stays on the helper.
    buffer.mov_rm(Reg::Rcx, MOVE_SCRATCH, SUB_BITS_START_DISP)?;
    buffer.mov_ri(MOVE_SCRATCH_2, MAX_SMALL)?;
    buffer.cmp_rr(Reg::Rcx, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::A, fallback)?;
    buffer.shl_ri(Reg::Rcx, 4)?;
    buffer.mov_ri(MOVE_SCRATCH_2, SMALL_TAG)?;
    buffer.or_rr(Reg::Rcx, MOVE_SCRATCH_2)?;

    shape
        .dst
        .store_at(buffer, Y_ADDR_SCRATCH, Reg::Rcx, allocation, highest_touched)?;
    emit_fallback_and_done(buffer, allocation, done, func, pc, fallback, fast_done)
}

// ---------------------------------------------------------------------------
// Shape 2: `bs_set_position { ctx, pos }`
// ---------------------------------------------------------------------------

/// Where a `bs_set_position`'s saved position comes from.
#[derive(Clone, Copy)]
enum PosSource {
    /// A register or `Y` slot holding the position term.
    Loc(Loc),
    /// A compile-time constant bit position (already validated to `0..=MAX_SMALL`).
    Constant(usize),
}

impl PosSource {
    /// The x-register this source reads, if any.
    fn register(self) -> Option<u32> {
        match self {
            PosSource::Loc(loc) => loc.register(),
            PosSource::Constant(_) => None,
        }
    }

    /// Whether reading this source touches the frame's Y window.
    fn is_y(self) -> bool {
        matches!(self, PosSource::Loc(loc) if loc.is_y())
    }
}

/// The operands of a `bs_set_position` the tier lowers natively.
#[derive(Clone, Copy)]
pub(crate) struct SetPositionShape {
    /// The match-context operand's location.
    ctx: Loc,
    /// Where the new bit position comes from.
    pos: PosSource,
}

impl SetPositionShape {
    /// The x-register indices the shape touches.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(2);
        registers.extend(self.ctx.register());
        registers.extend(self.pos.register());
        registers
    }

    /// The x-register indices the shape **writes**: none. The new position is written into
    /// the context block's `start` word (a heap word), not into a register.
    pub(crate) fn writes(&self) -> Vec<u32> {
        Vec::new()
    }

    /// The x-register indices the shape **reads** — the context and the position source.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.registers()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.ctx.is_y() || self.pos.is_y()
    }
}

/// The inlinable shape of a `bs_set_position`, or `None` when the whole instruction must
/// stay on the family.
///
/// The position operand must be an addressable `Loc` (so its run-time classification —
/// a match context, or a non-negative small integer — can be done natively) or a
/// **constant** small integer in `0..=MAX_SMALL` (whose value `position_of` accepts
/// outright). A constant outside that range is `position_of`'s typed error, which stays
/// on the family.
pub(crate) fn set_position_shape(ctx: &Operand, pos: &Operand) -> Option<SetPositionShape> {
    let ctx = Loc::of(ctx)?;
    let pos = match pos {
        Operand::Integer(n) => {
            let n = *n;
            if (0..=MAX_SMALL).contains(&n) {
                PosSource::Constant(n as usize)
            } else {
                return None;
            }
        }
        Operand::Character(c) => PosSource::Constant(*c as usize),
        other => PosSource::Loc(Loc::of(other)?),
    };
    Some(SetPositionShape { ctx, pos })
}

/// Emit `bs_set_position Ctx Pos` natively, or report that the block must helper it
/// (see [`try_inline_bs_get_position`] for the return convention).
///
/// The guarded sequence, for a register/`Y` position source:
///
/// ```text
///   <the match-context guard>      ; not a context -> fallback
///   mov  rdx, <Pos>                ; the saved position term
///   test rdx, 1 ; jne not_ctx      ; boxed?
///   <header word == HEADER_SUB_BITS ?> ;  yes -> new_start = [pos + 16]
/// not_ctx:
///   (rdx & TAG_IMMED1_MASK) == TAG_IMMED1_SMALL && (rdx >> 4) >= 0 ?
///                                  ; no -> fallback (the family's typed raise)
///   new_start = rdx >> 4
///   mov  [ctx + 16], new_start     ; ONE word store, no bounds check (the family does none)
/// ```
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_set_position(
    buffer: &mut CodeBuffer,
    ctx: &Operand,
    pos: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = set_position_shape(ctx, pos) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_SET_POSITION);

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    emit_ctx_guard(
        buffer,
        shape.ctx,
        allocation,
        fallback,
        highest_touched,
    )?;

    match shape.pos {
        PosSource::Constant(new_start) => {
            buffer.mov_ri(Reg::Rsi, new_start as i64)?;
        }
        PosSource::Loc(loc) => {
            loc.load(
                buffer,
                allocation,
                Reg::Rdx,
                MOVE_SCRATCH_2,
                fallback,
                highest_touched,
            )?;
            let not_ctx: CodeLabel = buffer.label();
            let store: CodeLabel = buffer.label();
            // A match context: its own `start` word is the position.
            buffer.mov_ri(MOVE_SCRATCH_2, NOT_BOXED_MASK)?;
            buffer.test_rr(Reg::Rdx, MOVE_SCRATCH_2)?;
            buffer.jcc(Cond::Ne, not_ctx)?;
            buffer.mov_rr(Reg::Rsi, Reg::Rdx)?;
            buffer.mov_ri(MOVE_SCRATCH_2, POINTER_MASK)?;
            buffer.and_rr(Reg::Rsi, MOVE_SCRATCH_2)?;
            buffer.mov_rm(Reg::Rax, Reg::Rsi, HEADER_DISP)?;
            buffer.mov_ri(MOVE_SCRATCH_2, SUB_BITS_HEADER)?;
            buffer.cmp_rr(Reg::Rax, MOVE_SCRATCH_2)?;
            buffer.jcc(Cond::Ne, not_ctx)?;
            buffer.mov_rm(Reg::Rsi, Reg::Rsi, SUB_BITS_START_DISP)?;
            buffer.jmp(store)?;
            // A non-negative small integer.
            buffer.bind(not_ctx)?;
            buffer.mov_rr(Reg::Rsi, Reg::Rdx)?;
            buffer.mov_ri(MOVE_SCRATCH_2, IMMED1_MASK)?;
            buffer.and_rr(Reg::Rsi, MOVE_SCRATCH_2)?;
            buffer.mov_ri(MOVE_SCRATCH_2, SMALL_TAG)?;
            buffer.cmp_rr(Reg::Rsi, MOVE_SCRATCH_2)?;
            buffer.jcc(Cond::Ne, fallback)?;
            buffer.mov_rr(Reg::Rsi, Reg::Rdx)?;
            buffer.sar_ri(Reg::Rsi, 4)?;
            buffer.test_rr(Reg::Rsi, Reg::Rsi)?;
            buffer.jcc(Cond::S, fallback)?;
            buffer.bind(store)?;
        }
    }

    // The single commit: word 2 of the context block. `ctx` may alias `pos`, so the write
    // comes after every read — the same order the family uses.
    buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rsi)?;
    emit_fallback_and_done(buffer, allocation, done, func, pc, fallback, fast_done)
}

