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

//! The baseline JIT's **block prologue**: the one native sequence every compiled
//! block begins with ([`emit_block_prologue`]) plus the position-store helpers it
//! uses ([`emit_position_store`] / [`emit_publish_y`] / [`emit_prologue_helper`]).
//!
//! [`emit_block`](super::block::emit_block) emits this sequence first, then the
//! per-instruction body. The prologue saves the block's own frame registers, reserves
//! the `rt` slot, loads the `machine.x` base, records the block's entry position (or
//! publishes the frame's Y window) and loads the block's live-in cache registers.

use beam_common::Result;

use crate::jit::emitter::{CodeBuffer, Reg};

use super::super::jit_set_position;
use super::super::runtime::{jit_publish_y, PositionHelper, RUNTIME_XREGS_DISP};
use super::operands::x_offset;
#[cfg(test)]
use super::FillSite;
use super::{frame_slot_len, native_frame_regs, touch, Allocation, RUNTIME_SLOT_DISP};

/// Emit the prologue of the native function for `code[start..end]` (a basic block of
/// `func`).
///
/// `highest_touched` accumulates the highest x-register index the emitted code
/// touches natively (a cached load/store, or a direct `machine.x` access); the
/// driver refuses native entry to a machine whose register file does not cover it.
pub(super) fn emit_block_prologue(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
    func: u32,
    start: usize,
) -> Result<()> {
    // Save only the registers this block actually uses: `rbx` (the `machine.x` base)
    // plus the machine registers it holds as cache slots. A block that caches fewer
    // registers pushes fewer (see [`native_frame_regs`]); the `rt`-slot reservation below
    // compensates so `rsp` is 16-byte aligned at every helper call site.
    let frame = native_frame_regs(allocation);
    for reg in &frame {
        buffer.push_r(*reg)?;
    }
    // Reserve the `rt` slot. `lea rsp, [rsp - slot]` is `sub rsp, slot` and keeps `rsp`
    // 16-byte aligned at the helper call site (see [`frame_slot_len`]).
    buffer.lea(Reg::Rsp, Reg::Rsp, -frame_slot_len(frame.len()))?;
    buffer.mov_mr(Reg::Rsp, RUNTIME_SLOT_DISP, Reg::Rdi)?;
    // rbx = rt->x_regs, the base of `machine.x` the cached slots are indexed from.
    // Loaded **before** the position store below: `rbx` is callee-saved, so the helper
    // call preserves it, and the cached x-registers are loaded only *after* the call,
    // so nothing a block keeps live across the call sits in caller-saved scratch.
    buffer.mov_rm(Reg::Rbx, Reg::Rdi, RUNTIME_XREGS_DISP)?;
    // Record this block's own `(func, pc)` as the machine's execution position, so
    // `Machine::current_code` (and the `process_info/2` position items built from it)
    // advances at block granularity even across a **linked** branch, which never
    // returns to the driver. A parked `receive` and a raise stay exact because both go
    // through the interpreted deopt (`jit_step_one`), which records the *instruction*
    // address itself. See `jit_set_position`.
    //
    // A block that accesses a Y slot ([`Allocation::y_last`]) publishes the current
    // frame's Y window through `jit_publish_y` **instead**, which does the position
    // store *and* refreshes the window in the one call — so publishing costs such a
    // block nothing extra, and a Y-free block keeps emitting the plain position store
    // (it pays literally nothing). See `jit_publish_y` for why the prologue is the
    // sound, cheapest publish point.
    if allocation.y_last.is_some() {
        emit_publish_y(buffer, func, start as u32)?;
    } else {
        emit_position_store(buffer, func, start as u32)?;
    }
    // Load a cache register only where the block can still **observe** it at entry
    // ([`Allocation::entry_loads`], the block's live-in set): a register the block writes
    // before it reads is set by that writing instruction, so loading it here would be a
    // redundant load. Every slot stays charged to `highest_touched`, so the driver's
    // register-file bound is unchanged.
    for (x, reg) in &allocation.slots {
        touch(highest_touched, *x);
        if allocation.entry_loads(*x) {
            buffer.mov_rm(*reg, Reg::Rbx, x_offset(*x))?;
        }
    }
    // Structural counter: the prologue loaded the block's live-in set.
    #[cfg(test)]
    super::super::counters::record_fill(FillSite::Prologue, allocation.entry_len() as u64);

    Ok(())
}

/// Emit the prologue's position store: call `jit_set_position(rt, func, pc)` so the
/// machine records this block's entry `(func, pc)` as its current execution position.
///
/// `rt` is read out of the prologue-reserved stack slot ([`RUNTIME_SLOT_DISP`]), so
/// this may be emitted anywhere after that slot is written. The helper clobbers only
/// caller-saved registers, so a block must have loaded nothing it needs into them at
/// the call; every cached x-register and `rbx` live in callee-saved registers, which
/// the helper preserves ([`emit_block_prologue`] emits this before loading the cached
/// x-registers for exactly that reason). `rsp` is already 16-byte aligned at the call
/// (see [`frame_slot_len`]). The helper returns nothing — a position store cannot
/// fail — so `rax` is not consulted.
fn emit_position_store(buffer: &mut CodeBuffer, func: u32, pc: u32) -> Result<()> {
    let helper: PositionHelper = jit_set_position;
    emit_prologue_helper(buffer, func, pc, helper as *const ())
}

/// Emit the prologue's **Y-window publish**: call `jit_publish_y(rt, func, pc)`, which
/// records this block's entry `(func, pc)` as the machine's position *and* re-publishes
/// the current frame's Y window.
///
/// A block whose body accesses a Y slot ([`Allocation::y_last`]) emits this in place of
/// [`emit_position_store`] — same three-argument call shape, so the block pays no extra
/// call for publishing. The window must be live before the block's first Y access
/// because the Erlang stack is a `Vec` a frame push may reallocate; see `jit_publish_y`.
fn emit_publish_y(buffer: &mut CodeBuffer, func: u32, pc: u32) -> Result<()> {
    let helper: PositionHelper = jit_publish_y;
    emit_prologue_helper(buffer, func, pc, helper as *const ())
}

/// Emit the prologue's three-argument helper call — `helper(rt, func, pc)` — shared by
/// [`emit_position_store`] and [`emit_publish_y`] (the two differ only in the helper's
/// body).
///
/// `rt` is read out of the prologue-reserved stack slot ([`RUNTIME_SLOT_DISP`]), so this
/// may be emitted anywhere after that slot is written. The helper clobbers only
/// caller-saved registers, so a block must have loaded nothing it needs into them at
/// the call; every cached x-register and `rbx` live in callee-saved registers, which
/// the helper preserves ([`emit_block_prologue`] emits this before loading the cached
/// x-registers for exactly that reason). `rsp` is already 16-byte aligned at the call
/// (see [`frame_slot_len`]). Neither helper returns a value, so `rax` is not consulted.
fn emit_prologue_helper(
    buffer: &mut CodeBuffer,
    func: u32,
    pc: u32,
    helper: *const (),
) -> Result<()> {
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_ri(Reg::Rsi, i64::from(func))?;
    buffer.mov_ri(Reg::Rdx, i64::from(pc))?;
    buffer.mov_ri(Reg::R11, helper as i64)?;
    buffer.call_r(Reg::R11)
}
