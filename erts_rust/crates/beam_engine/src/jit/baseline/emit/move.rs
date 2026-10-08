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

//! The baseline JIT's inline `move`/`swap`: shuffling an x-register value between its
//! machine register (when the allocator caches it) and its `machine.x` slot.

use beam_common::Result;
use beam_loader::Operand;

use crate::jit::emitter::{CodeBuffer, Reg};
use crate::machine::MAX_REGISTER;

use super::operands::{constant_word, x_offset, x_register};
use super::{touch, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2};

/// Where a `move`'s source value comes from.
enum MoveSource {
    /// An x-register the block holds in a machine register.
    Cached(Reg),
    /// An x-register the block does **not** cache — read straight from its
    /// `machine.x` slot (`[rbx + x*8]`), which is where the value is at a block
    /// boundary.
    Memory(u32),
    /// A constant operand, materialised as its raw `Eterm` word.
    Constant(i64),
}

/// Emit `move Src Dst` natively, or report that the block must helper it.
///
/// Returns `true` when the move was emitted. **Any** destination that names an
/// x-register inside the engine's [`MAX_REGISTER`] bound is inlined, whether or not
/// the register allocator could cache it:
///
/// * a **cached** destination is written in its machine register;
/// * a destination **beyond the cache budget** is written straight into `machine.x`
///   (`mov [rbx + dst*8], …`) — the very slot [`super::spill`] would have written, so the
///   tier's liveness model is unchanged (cached values live in registers *within* a
///   block and are always back in `machine.x` at a block boundary).
///
/// The source is read from its cached register, from its `machine.x` slot, or from
/// a constant word. A `move` with a Y destination (or any other operand shape), or
/// one naming an x-register no machine can hold, still deopts to the helper — which
/// reports the interpreter's exact result (including its faithful `bad_register`
/// error).
pub(super) fn try_inline_move(
    buffer: &mut CodeBuffer,
    src: &Operand,
    dst: &Operand,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(dst_x) = x_register(dst) else {
        return Ok(false);
    };
    if dst_x >= MAX_REGISTER {
        // `machine.x` is at most `MAX_REGISTER` long, so addressing `dst_x` directly
        // would be out of bounds; the helper reports the faithful `bad_register`.
        return Ok(false);
    }
    let source = match x_register(src) {
        Some(src_x) if src_x < MAX_REGISTER => match allocation.slot(src_x) {
            Some(reg) => MoveSource::Cached(reg),
            None => MoveSource::Memory(src_x),
        },
        // A register index outside the engine's bound (same rule as the
        // destination): the interpreter step reports the exact error.
        Some(_) => return Ok(false),
        None => match constant_word(src) {
            Some(word) => MoveSource::Constant(word),
            None => return Ok(false),
        },
    };

    match (allocation.slot(dst_x), source) {
        // Cached destination: write its machine register.
        (Some(dst_reg), MoveSource::Cached(src_reg)) => buffer.mov_rr(dst_reg, src_reg)?,
        (Some(dst_reg), MoveSource::Memory(src_x)) => {
            buffer.mov_rm(dst_reg, Reg::Rbx, x_offset(src_x))?;
            touch(highest_touched, src_x);
        }
        (Some(dst_reg), MoveSource::Constant(word)) => buffer.mov_ri(dst_reg, word)?,
        // Uncached destination: write its `machine.x` slot directly.
        (None, MoveSource::Cached(src_reg)) => {
            buffer.mov_mr(Reg::Rbx, x_offset(dst_x), src_reg)?;
        }
        (None, MoveSource::Memory(src_x)) => {
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(src_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(dst_x), MOVE_SCRATCH)?;
            touch(highest_touched, src_x);
        }
        (None, MoveSource::Constant(word)) => {
            buffer.mov_ri(MOVE_SCRATCH, word)?;
            buffer.mov_mr(Reg::Rbx, x_offset(dst_x), MOVE_SCRATCH)?;
        }
    }
    touch(highest_touched, dst_x);
    Ok(true)
}

/// Emit `swap A B` natively, or report that the block must helper it.
///
/// Returns `true` when the exchange was emitted. Both operands must name
/// x-registers inside the engine's [`MAX_REGISTER`] bound; each is then read and
/// written through its own representation — a cached operand through its machine
/// register, a non-cached one through its `machine.x` slot — so the swap is exact
/// for every combination of the four. A `Y` (or other) operand deopts, and the
/// helper performs the interpreter's exact `write(b, read(a))` / `write(a, read(b))`
/// exchange.
pub(super) fn try_inline_swap(
    buffer: &mut CodeBuffer,
    a: &Operand,
    b: &Operand,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let (Some(a_x), Some(b_x)) = (x_register(a), x_register(b)) else {
        return Ok(false);
    };
    if a_x >= MAX_REGISTER || b_x >= MAX_REGISTER {
        return Ok(false);
    }
    match (allocation.slot(a_x), allocation.slot(b_x)) {
        (Some(a_reg), Some(b_reg)) => {
            // `xchg` spelled as three moves through the scratch register (the
            // encoder has no `xchg`, and a register pair that overlaps the
            // destination is not expressible with `mov` alone otherwise).
            buffer.mov_rr(MOVE_SCRATCH, a_reg)?;
            buffer.mov_rr(a_reg, b_reg)?;
            buffer.mov_rr(b_reg, MOVE_SCRATCH)?;
        }
        (Some(a_reg), None) => {
            // a is cached, b lives in `machine.x`.
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(b_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(b_x), a_reg)?;
            buffer.mov_rr(a_reg, MOVE_SCRATCH)?;
            touch(highest_touched, b_x);
        }
        (None, Some(b_reg)) => {
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(a_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(a_x), b_reg)?;
            buffer.mov_rr(b_reg, MOVE_SCRATCH)?;
            touch(highest_touched, a_x);
        }
        (None, None) => {
            buffer.mov_rm(MOVE_SCRATCH, Reg::Rbx, x_offset(a_x))?;
            buffer.mov_rm(MOVE_SCRATCH_2, Reg::Rbx, x_offset(b_x))?;
            buffer.mov_mr(Reg::Rbx, x_offset(a_x), MOVE_SCRATCH_2)?;
            buffer.mov_mr(Reg::Rbx, x_offset(b_x), MOVE_SCRATCH)?;
            touch(highest_touched, a_x);
            touch(highest_touched, b_x);
        }
    }
    Ok(true)
}
