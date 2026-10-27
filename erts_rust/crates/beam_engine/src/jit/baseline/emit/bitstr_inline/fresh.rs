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
//! The **fresh whole-byte** `bs_create_bin` arm: `Dst = <<Value:N>>` for one static
//! whole-byte field of `N` (`1..=8`) bytes.
//!
//! This is the compiler's *temp binary* — `emqx_base62:decode/2` builds `<<DecodeChar>>`
//! three times per call, with no accumulator anywhere in the instruction — and the
//! second (and last) shape the construction family lowers natively. The append arm
//! ([`super::create`]) covers the accumulator build; this one covers a build whose whole
//! payload is a single value, so the emitted sequence is two frame-window guards and
//! **one** call to
//! [`jit_fresh_heap_binary`](crate::jit::baseline::runtime::jit_fresh_heap_binary).
//!
//! # Why this shape is one rooted helper call, not inline stores
//!
//! Unlike every match-context shape in the roster, and unlike the append arm (which
//! writes into a payload that already exists), a fresh build must **allocate** the
//! resulting term. The inline set's one hard rule is that an arm which would allocate or
//! root *inside* the instruction declines rather than allocate mid-instruction
//! (`../../CONTEXT.md` §Constraints), because a collection relocates terms a raw
//! `*mut Eterm` still names. So the reservation, the term write and the destination
//! store all live in the helper, behind the same rooted door every other construction
//! arm uses, and the block keeps only its guards.
//!
//! # The emitted sequence
//!
//! ```text
//!   <dst slot guard>            ; jump fallback on an out-of-frame Y destination
//!   <value slot guard>          ; jump fallback on an out-of-frame Y value
//!   call jit_fresh_heap_binary(rt, func, pc, nbytes)
//!   cmp rax, encode_target(func, pc + 1) ; the block's usual continue test
//!   jne done
//!   <refill + Y refresh as the allocation's liveness requires>
//!   jmp fast_done
//! fallback:
//!   <one interpreter step and continue>
//! fast_done:
//! ```
//!
//! Both guards are needed for correctness of *placement*, not of value: a `Y` slot the
//! frame window does not cover must write nothing at all, exactly as the append arm's
//! first guard requires. Every other refusal — a value that is not a small integer, a
//! negative or field-wider value, a term the rooted door rejects, an instruction that is
//! no longer this shape — is the **helper's** decline: it hands the instruction back to
//! [`jit_step_one`](crate::jit::baseline::runtime::jit_step_one) and the interpreter's own
//! step produces the family's value, fail transfer or raise.
//!
//! # Deopt idempotency
//!
//! A `fallback` is a deopt (`jit_step_one` re-executes the whole instruction), and this
//! arm never deopts at run time: the two guards are frame-window tests on operands the
//! plan already proved addressable, and nothing between them and the helper call mutates
//! any machine state. The helper commits exactly one thing — the destination write — and
//! only after the built term exists, so a declined attempt leaves the machine untouched
//! and the re-execution re-reads the same operands. Pinned by the arm's own suite
//! (`../../bs_create_bin_fresh_tests.rs`), which asserts a **zero** run-time deopt count.
use beam_common::Result;
use beam_loader::{Module, Operand};
use crate::families::bitstrings::create::seam;
use crate::jit::emitter::CodeBuffer;
use super::super::super::runtime::jit_fresh_heap_binary;
use super::super::operands::{Loc, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2};
use super::super::{emit_positioned_helper_call, Allocation};
use super::emit_fallback_and_done;
#[cfg(test)]
use super::{record_inlined, SHAPE_FRESH_BIN};
/// The operands of a `bs_create_bin` the fresh arm lowers natively: the single field's
/// value, the destination, and the field width already reduced to whole **bytes**.
#[derive(Clone, Debug)]
pub(crate) struct FreshBinShape {
    /// The field's value: an in-bound x-register or an addressable `Y` slot. The helper
    /// reads the **operand**, not this `Loc` — the location exists so the guards and the
    /// register accounting can name it.
    value: Loc,
    /// The destination: an x-register or an addressable `Y` slot.
    dst: Loc,
    /// The field width in bytes (`1..=8`), the helper's third argument and the figure it
    /// re-derives from the module to refuse a plan that moved under the sealed code.
    nbytes: usize,
}
impl FreshBinShape {
    /// The inlinable fresh shape of a `bs_create_bin`, or `None` when the instruction is
    /// not one this arm lowers.
    ///
    /// Purely syntactic and label-free, like [`super::CreateBinShape::of`], so the
    /// register-allocation mirrors, the `Y` analysis, the emitter and the refusal census
    /// all reach the same answer from this one call. The shape itself (one segment, a
    /// static whole-byte width `1..=8`) is [`seam::fresh_plan`]'s; this function only
    /// adds the two operands' addressability.
    pub(crate) fn of(module: &Module, dst: &Operand, segments: &[Operand]) -> Option<FreshBinShape> {
        let plan = seam::fresh_plan(module, segments)?;
        Some(FreshBinShape {
            value: Loc::of(&plan.value)?,
            dst: Loc::of(dst)?,
            nbytes: plan.nbytes,
        })
    }
    /// The x-register indices the shape touches — the value and the destination (a `Y`
    /// slot contributes nothing: it is a stack slot, never a cached x-register).
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(2);
        registers.extend(self.value.register());
        registers.extend(self.dst.register());
        registers
    }
    /// The x-register indices the shape **reads** — the value operand, which the helper
    /// reads out of `machine.x` after the block spilled it.
    ///
    /// A `Y` value contributes nothing here, but its slot is still read: that is what the
    /// second guard proves is in frame.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.value.register().into_iter().collect()
    }
    /// The x-register indices the shape **writes** — the destination, and nothing else (a
    /// `Y` destination contributes nothing).
    ///
    /// Charging it is mandatory: a dropped write lets a stale cache register survive a
    /// block exit, the one unsound failure mode of the register cache.
    pub(crate) fn writes(&self) -> Vec<u32> {
        self.dst.register().into_iter().collect()
    }
    /// Whether the lowering accesses the current frame's Y window — a `Y` destination or a
    /// `Y` value.
    pub(crate) fn uses_y(&self) -> bool {
        self.dst.is_y() || self.value.is_y()
    }
    /// The field width in bytes, for the arm's own tests (the emitter bakes it into the
    /// helper call).
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn nbytes(&self) -> usize {
        self.nbytes
    }
}
/// Emit the fresh-build sequence for `shape`, or report that the block must helper the
/// instruction (`Ok(false)` with **zero bytes emitted**).
///
/// [`FreshBinShape::of`] has already proved the shape; a `false` here means the sequence
/// itself could not be emitted (an instruction block that runs out of scratch), which
/// `emit/block.rs` handles exactly like a refusal.
pub(in crate::jit::baseline::emit) fn try_inline_bs_fresh_bin(
    buffer: &mut CodeBuffer,
    shape: &FreshBinShape,
    allocation: &Allocation,
    done: crate::jit::emitter::Label,
    func: u32,
    pc: u32,
) -> Result<bool> {
    #[cfg(test)]
    record_inlined(SHAPE_FRESH_BIN);
    let fallback = buffer.label();
    let fast_done = buffer.label();
    // (1) The destination's frame-window guard, so a rejected slot writes nothing at all.
    shape
        .dst
        .guard_store(buffer, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)?;
    // (2) The value's frame-window guard, on the same terms: guard **only**, because the
    // helper re-reads the operand through `read_operand` itself. The frame cannot move
    // between this guard and the helper call — the helper runs no interpreter step before
    // it reads the operand.
    shape
        .value
        .guard_load(buffer, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)?;
    // (3) The build: one rooted helper call carrying this instruction's identity and the
    // field width, with an inline position store (what a pre-emption or a raise from the
    // built term's own path needs to name the instruction).
    emit_positioned_helper_call(
        buffer,
        allocation,
        jit_fresh_heap_binary as *const (),
        &[i64::from(func), i64::from(pc), shape.nbytes as i64],
        func,
        pc,
    )?;
    // (4) The block's usual continue arm; a declined helper hands back the interpreter
    // step's own word, which leaves the block exactly as any other deopt does.
    super::super::emit_continue_check(buffer, done, allocation, func, pc)?;
    emit_fallback_and_done(buffer, allocation, done, func, pc, fallback, fast_done)?;
    Ok(true)
}

/// Which of the two `bs_create_bin` inline arms lowers an instruction.
///
/// The construction family has **two** shapes and they share one instruction, so the
/// tier's register-allocation mirrors, its `Y` analysis, its refusal census and its
/// emitter must all agree on *one* answer: the append arm first
/// ([`CreateBinShape::of`](super::CreateBinShape::of), the in-place accumulator), then this
/// arm (the fresh whole-byte build). Routing through one predicate is what keeps "the
/// emitter lowered it" and "the register file accounted for it" the same statement.
#[derive(Clone, Debug)]
pub(crate) enum CreateArm {
    /// `<<Acc/binary, F…>>`: write the fields where the accumulator's window ends and
    /// commit the window advance (see [`super::create`]).
    Append(super::CreateBinShape),
    /// `<<Value:N>>`: build a new binary whose whole payload is the value's low `N * 8`
    /// bits (see [`FreshBinShape`]).
    Fresh(FreshBinShape),
}
impl CreateArm {
    /// The arm that lowers this instruction, or `None` when neither does.
    pub(crate) fn of(module: &Module, dst: &Operand, segments: &[Operand]) -> Option<CreateArm> {
        if let Some(shape) = super::CreateBinShape::of(module, dst, segments) {
            return Some(CreateArm::Append(shape));
        }
        FreshBinShape::of(module, dst, segments).map(CreateArm::Fresh)
    }
    /// The x-register indices the arm touches.
    pub(crate) fn registers(&self) -> Vec<u32> {
        match self {
            CreateArm::Append(shape) => shape.registers(),
            CreateArm::Fresh(shape) => shape.registers(),
        }
    }
    /// The x-register indices the arm **reads**.
    pub(crate) fn reads(&self) -> Vec<u32> {
        match self {
            CreateArm::Append(shape) => shape.reads(),
            CreateArm::Fresh(shape) => shape.reads(),
        }
    }
    /// The x-register indices the arm **writes** — the destination, on both arms.
    pub(crate) fn writes(&self) -> Vec<u32> {
        match self {
            CreateArm::Append(shape) => shape.writes(),
            CreateArm::Fresh(shape) => shape.writes(),
        }
    }
    /// Whether the arm accesses the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        match self {
            CreateArm::Append(shape) => shape.uses_y(),
            CreateArm::Fresh(shape) => shape.uses_y(),
        }
    }
    /// Which arm this is, for the refusal census's three-way split.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn arm_name(&self) -> &'static str {
        match self {
            CreateArm::Append(_) => "append",
            CreateArm::Fresh(_) => "fresh",
        }
    }
}
