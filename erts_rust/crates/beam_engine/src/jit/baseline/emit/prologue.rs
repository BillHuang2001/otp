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
//! uses ([`emit_position_store`] / [`emit_publish_y`]).
//!
//! [`emit_block`](super::block::emit_block) emits this sequence first, then the
//! per-instruction body. A block that must publish the frame's Y window
//! ([`Allocation::prologue_publishes_y`]) opens with that publish, and the prologue
//! then reports a **second entry point** — the offset just past the publish — which
//! `jit_deallocate_return` answers so the publish never runs twice on the up leg
//! (`emit/compile.rs` records it per `pc` and `runtime::EntryTables` publishes it).
//! After the publish the prologue saves the block's own frame registers, reserves the
//! `rt` slot, loads the `machine.x` base, **stores the block's entry position inline**
//! (three instructions — no helper call; see [`emit_position_store`]), and loads the
//! block's live-in cache registers.

use beam_common::Result;

use crate::jit::emitter::{CodeBuffer, Reg};
use crate::machine::pack_current_code;
use super::super::runtime::{
    jit_publish_y, PositionHelper, RUNTIME_CURRENT_CODE_DISP, RUNTIME_XREGS_DISP,
};
use super::operands::x_offset;
#[cfg(test)]
use super::FillSite;
use super::{frame_slot_len, native_frame_regs, touch, Allocation, RUNTIME_SLOT_DISP};

/// Emit the prologue of the native function for `code[start..end]` (a basic block of
/// `func`), answering the block's **post-publish entry offset** when it has one.
///
/// `highest_touched` accumulates the highest x-register index the emitted code
/// touches natively (a cached load/store, or a direct `machine.x` access); the
/// driver refuses native entry to a machine whose register file does not cover it.
///
/// The answer is `Some(offset)` for a publishing block ([`Allocation::prologue_publishes_y`]):
/// the buffer offset at which the block's *second* entry point begins, i.e. just past
/// the publish, where `rdi` again holds `rt` and `rsp` is again `≡ 8 (mod 16)` — the
/// same machine state a normal entry reaches after its own publish. Entering there
/// skips only the publish, so every push this prologue emits still runs exactly once
/// and the shared `done` epilogue's teardown stays balanced. The offset is
/// **recorded**, never assumed: its length is whatever [`emit_publish_y`] emitted.
pub(super) fn emit_block_prologue(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
    func: u32,
    start: usize,
) -> Result<Option<usize>> {
    // The Y-window publish runs **before** the frame pushes, so the block's second
    // entry point (below the pushes, at the point the publish is complete) is a valid
    // block entry: `jit_deallocate_return` publishes the popped frame's window itself
    // and answers this offset, so the up leg of a `deallocate_return` into a
    // Y-accessing continuation sub-block pays one helper call instead of two.
    let alternate_entry = if allocation.prologue_publishes_y() {
        emit_publish_y(buffer, func, start as u32)?;
        Some(buffer.len())
    } else {
        None
    };
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
    // Loaded **after** the publish (whose `pop rdi` restores the `rt` argument) and
    // **before** the position store below: `rbx` is callee-saved and the cached
    // x-registers are loaded only *after* the store, so nothing a block keeps live sits
    // in caller-saved scratch.
    buffer.mov_rm(Reg::Rbx, Reg::Rdi, RUNTIME_XREGS_DISP)?;
    // Record this block's own `(func, pc)` as the machine's execution position, so
    // `Machine::current_code` (and the `process_info/2` position items built from it)
    // advances at block granularity even across a **linked** branch, which never
    // returns to the driver. A parked `receive` and a raise stay exact because both go
    // through the interpreted deopt (`jit_step_one`), which records the *instruction*
    // address itself. See `emit_position_store`.
    //
    // **Every** block emits it, publishing or not: the publish no longer carries the
    // position store (it runs before `rdi` is re-secured past a `pop`, and a block
    // entered through the post-publish alternate skips it entirely), so the store is the
    // prologue's own unconditional step. A Y-accessing block publishes
    // ([`Allocation::prologue_publishes_y`]) unless every one of its Y accesses follows an
    // in-block `allocate`/`allocate_heap`/`trim`, whose helper re-published the window
    // itself on its in-block continue arm: then the prologue's window is overwritten
    // before anything reads it, and a publish would be pure cost. A Y-free block never
    // publishes (it pays nothing but the three instructions). See `jit_publish_y` for why
    // the prologue is the sound, cheapest publish point.
    emit_position_store(buffer, func, start as u32)?;
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

    Ok(alternate_entry)
}

/// The emitted length of one [`emit_publish_y`] sequence, measured by emitting it into a
/// throwaway buffer.
///
/// A pin on a block's post-publish alternate entry must assert the **relation**
/// `alternate = normal entry + the publish sequence's length`, never a byte count copied
/// from the encoder's arithmetic, so the length is derived here, from the emitter itself.
#[cfg(test)]
pub(crate) fn publish_y_len() -> Result<usize> {
    let mut buffer = CodeBuffer::new()?;
    emit_publish_y(&mut buffer, 0, 0)?;
    Ok(buffer.len())
}
/// Emit an **inline position store**: write `(func, pc)` — two compile-time constants —
/// into `machine.current_code`.
///
/// The one encoder, used at both sites that must record an instruction position
/// without a helper call:
///
/// * the block prologue ([`emit_block_prologue`]), for the block's entry `(func, start)`
///   — **unconditionally**, publishing block or not, because the publish no longer
///   carries the store: a linked branch never returns to the driver, so the position
///   must still advance, and a block entered through the post-publish alternate entry
///   skips the publish entirely and would otherwise leave the position at the previous
///   transfer's target;
/// * the bitstring arm ([`emit_bitstr`](super::family::emit_bitstr)), for each `Bs*`
///   instruction, **before** its helper call: `jit_bitstr` used to store the position
///   itself, re-deriving the packed word from its two run-time arguments on every
///   execution. Sharing this function is what keeps one encoder (and
///   [`crate::machine::pack_current_code`] underneath it) the only definition of the
///   position word.
///
/// The destination is the one `u64` word `Machine::set_current_code` writes, reached
/// through the `rt` argument the block was entered with (`rdi` at both sites: the
/// prologue receives it — and re-secures it from the publish's `pop` — the bitstring arm
/// reloads it from the block's `rt` slot —
/// [`emit_positioned_helper_call`](super::emit_positioned_helper_call) does that load
/// before calling this):
///
/// ```text
/// mov r10, [rdi + RUNTIME_CURRENT_CODE_DISP]   ; r10 = &machine.current_code
/// mov r11, movabs pack_current_code(func, pc)  ; the packed position word
/// mov [r10], r11
/// ```
///
/// Three instructions, no helper call: there is nothing for a helper to compute (both
/// operands are known at compile time), so the call's `rt` slot load, two `movabs`
/// argument loads, its own `movabs` of the helper address, the call and the helper's
/// `rt`/`machine` derefs were all pure overhead. The packed word comes from
/// [`crate::machine::pack_current_code`] — the single definition of the encoding
/// (`CURRENT_CODE_NONE`/`unpack_current_code` are its other halves) — so the emitted
/// store and every Rust-side store stay the same word by construction.
///
/// `r10`/`r11` are the caller-saved move scratch registers ([`super::MOVE_SCRATCH_2`]/
/// [`super::MOVE_SCRATCH`]), never cache registers, and they are free here: a block's
/// cache registers are loaded only *after* this point and no value survives into the
/// next instruction. `rdi` must hold the block's `rt` argument (the prologue received it
/// and never modifies it; the bitstring arm reloads it from the block's `rt` slot), so
/// the load needs no stack slot. A position store cannot fail, so nothing is checked.
pub(super) fn emit_position_store(buffer: &mut CodeBuffer, func: u32, pc: u32) -> Result<()> {
    // Structural counter: one emitted inline position store (a Y-free block prologue's
    // entry, or one bitstring arm's instruction). A **compile-time** count — the store is
    // generated code now, so there is no helper call left at run time to record it; the
    // count is the number of sites that emit this sequence (the emission site is the only
    // place that knows).
    #[cfg(test)]
    super::super::counters::record_set_position();
    buffer.mov_rm(Reg::R10, Reg::Rdi, RUNTIME_CURRENT_CODE_DISP)?;
    buffer.mov_ri(Reg::R11, pack_current_code(func, pc) as i64)?;
    buffer.mov_mr(Reg::R10, 0, Reg::R11)
}

/// Emit the prologue's **Y-window publish**: call `jit_publish_y(rt, func, pc)`, which
/// re-publishes the current frame's Y window.
///
/// A block that must publish the window ([`Allocation::prologue_publishes_y`]: it
/// accesses a Y slot, and not every one of those accesses follows a frame helper that
/// re-published the window itself) emits this **at the very head of the prologue**,
/// before its frame pushes, and reports the offset just past it as the block's second
/// entry point. The window must be live before the block's first Y access because the
/// Erlang stack is a `Vec` a frame push may reallocate; see `jit_publish_y`.
///
/// This half stays a helper, unlike the position store: it must re-derive the window
/// from **live** `Machine` state (`Machine::frames`/`Machine::stack`), which generated
/// code cannot read.
///
/// The call's argument register is `rdi`, which at a block entry already holds `rt`, so
/// the sequence saves it with a `push` instead of reloading it from a stack slot: one
/// byte and no load, and the push also supplies the 16-byte `rsp` alignment the C ABI
/// requires at a call (a block entry has `rsp ≡ 8 (mod 16)`). The `pop` restores `rt`
/// and the entry invariant, so everything the prologue emits after this point sees the
/// `rdi` it would have seen without it.
///
/// ```text
/// push rdi                                     ; save `rt`, align rsp for the call
/// mov rsi, movabs func
/// mov rdx, movabs pc
/// mov r11, movabs jit_publish_y
/// call r11
/// pop rdi                                      ; restore `rt`; the alternate entry is here
/// ```
///
/// The helper clobbers only caller-saved registers, and nothing is live in them here
/// (no cache register is loaded until after the position store).
fn emit_publish_y(buffer: &mut CodeBuffer, func: u32, pc: u32) -> Result<()> {
    let helper: PositionHelper = jit_publish_y;
    buffer.push_r(Reg::Rdi)?;
    buffer.mov_ri(Reg::Rsi, i64::from(func))?;
    buffer.mov_ri(Reg::Rdx, i64::from(pc))?;
    buffer.mov_ri(Reg::R11, helper as *const () as i64)?;
    buffer.call_r(Reg::R11)?;
    buffer.pop_r(Reg::Rdi)
}
