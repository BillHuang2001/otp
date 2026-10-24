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

//! The field shape: `bs_get_integer2`, and the tier's one width/flag rule.
//!
//! [`field_width_supported`] and [`field_is_unsupported`] are the **single** definition of
//! which fields this tier reads natively — the shape predicate and the refusal census
//! (`emit/test/refusal_census.rs`) both consult them, so the two cannot drift apart.

use beam_common::Result;
use beam_loader::{Label, Operand, NO_FAIL};
use beam_term::MAX_SMALL;

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::link::{ExitSpill, Linker};
use super::super::operands::{Loc, Y_ADDR_SCRATCH};
use super::{
    emit_bit_extract, emit_ctx_guard, emit_step_and_continue, emit_subject_view, Allocation,
    BSF_LITTLE, BSF_NATIVE, BSF_SIGNED, MOVE_SCRATCH, MOVE_SCRATCH_2, SMALL_TAG,
    SUB_BITS_END_DISP, SUB_BITS_START_DISP,
};

#[cfg(test)]
use super::{record_inlined, SHAPE_GET_INTEGER2};

// ---------------------------------------------------------------------------
// Shape 7: `bs_get_integer2 { fail, ctx, live, size, unit, flags, dst }`
// ---------------------------------------------------------------------------

/// The operands of a `bs_get_integer2` the tier lowers natively.
#[derive(Clone, Copy)]
pub(crate) struct GetInteger2Shape {
    /// The match-context operand's location.
    ctx: Loc,
    /// The destination the extracted integer is written to.
    dst: Loc,
    /// `size * unit`, the compile-time field width in bits.
    nbits: usize,
}

impl GetInteger2Shape {
    /// The [`GetInteger2Shape`] the four operands name, or `None` when the whole
    /// instruction must stay on the family.
    ///
    /// Purely syntactic and **label-free** — the fail-label resolution is the caller's —
    /// so the block's Y gate ([`super::super::arith::uses_y`]) derives the same answer as the
    /// emitter without needing the label map. The compile-time conditions are:
    ///
    /// * `ctx` and `dst` are an in-bound x-register or an addressable `Y` slot;
    /// * `size` is a **constant** non-negative small integer — `Operand::Integer` in
    ///   `0..=MAX_SMALL` or `Operand::Character`, exactly `read_size`'s accept rule — so
    ///   the field width is known at emit time (a runtime `size` is a register compare the
    ///   emitted sequence would not have);
    /// * `nbits = size * unit` does not overflow and is at most `59` (so the extracted
    ///   value is always `<= MAX_SMALL` and the result is a bare `make_small`, with no
    ///   allocation) **and** is one of the widths [`emit_bit_extract`] covers —
    ///   `nbits <= 16` or `nbits == 32`;
    /// * `flags` asks for neither `signed` nor little/native endianness: a signed field
    ///   needs a sign extension and a little-endian one a byte-swapping read, and both are
    ///   the family's `erts_bs_get_integer_2` general path.
    ///
    /// `flags`' remaining bits (`BSF_ALIGNED`, `BSF_EXACT`) do not change the value this
    /// family's reader produces — `read_integer_field` branches on `signed`/`little` only
    /// — so they are ignored here exactly as the family ignores them.
    pub(crate) fn of(
        ctx: &Operand,
        size: &Operand,
        unit: u32,
        flags: u32,
        dst: &Operand,
    ) -> Option<GetInteger2Shape> {
        let size = match size {
            Operand::Integer(n) if (0..=MAX_SMALL).contains(n) => *n as usize,
            Operand::Character(c) => *c as usize,
            _ => return None,
        };
        let nbits = size.checked_mul(unit as usize)?;
        if !field_width_supported(nbits) {
            return None;
        }
        if field_is_unsupported(flags) {
            return None;
        }
        Some(GetInteger2Shape {
            ctx: Loc::of(ctx)?,
            dst: Loc::of(dst)?,
            nbits,
        })
    }

    /// The x-register indices the shape touches — the context and the destination.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(2);
        registers.extend(self.ctx.register());
        registers.extend(self.dst.register());
        registers
    }

    /// The x-register indices the shape **writes** — the destination only. Charging it is
    /// **mandatory**: the extracted integer lands in `Dst`'s cache register, so a block
    /// that did not charge it would let a stale cache register survive a block exit — a
    /// wrong value, not a redundant store. (The context's `start` word is a heap store,
    /// not an x-register write.)
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

/// Whether a field of `nbits` bits is one [`emit_bit_extract`] can read directly.
///
/// The single definition of the tier's **width rule**: the extracted value must fit a
/// small integer (`<= MAX_SMALL`, so the result is a bare `make_small` with no allocation)
/// **and** be one of the widths the extraction sequence covers (`<= 16`, or exactly `32`).
/// Every field-extracting shape consults it, and so does the refusal census that attributes
/// the fields the tier declines — one rule, so the two cannot drift apart.
pub(crate) fn field_width_supported(nbits: usize) -> bool {
    nbits <= MAX_SMALL as usize && (nbits <= 16 || nbits == 32)
}

/// Whether `flags` asks for a field form this family's inline readers do not model: a
/// **signed** field (needs a sign extension) or a little/native-endian one (a byte-swapping
/// read). Both are the family's `erts_bs_get_integer_2` general path, so every
/// field-extracting shape declines them; the refusal census attributes them through this
/// same test.
pub(crate) fn field_is_unsupported(flags: u32) -> bool {
    flags & BSF_SIGNED != 0 || field_is_little(flags)
}

/// Whether `flags` asks for **little-endian** byte order.
///
/// The family's `field_is_little`
/// (`families::bitstrings::matching::reads::field_is_little`), re-derived: `BSF_LITTLE`
/// directly, or `BSF_NATIVE` on a little-endian target (the C loader collapses `native`
/// to the target's own order, `erts/emulator/beam/emu/generators.tab:43-55`).
pub(crate) fn field_is_little(flags: u32) -> bool {
    flags & BSF_LITTLE != 0 || (flags & BSF_NATIVE != 0 && cfg!(target_endian = "little"))
}

/// The inlinable shape of a `bs_get_integer2`, or `None` when the whole instruction must
/// stay on the family.
///
/// A `NO_FAIL` fail code *raises* on a short subject, and by then the destination is
/// unread — the raise is the family's `fail_step`, which is an interpreter step anyway
/// ([`MatchFail::Family`]'s reasoning); only a real label is inlined, and it must resolve
/// in the module's map because the fail transfer needs its `(function, code)` target. An
/// unresolved label keeps the family's exact `unknown_label`.
pub(crate) fn get_integer2_shape(
    labels: &LabelMap,
    fail: Label,
    ctx: &Operand,
    size: &Operand,
    unit: u32,
    flags: u32,
    dst: &Operand,
) -> Option<(GetInteger2Shape, (u32, u32))> {
    let shape = GetInteger2Shape::of(ctx, size, unit, flags, dst)?;
    if fail == NO_FAIL {
        return None;
    }
    let target = labels.resolve(fail).ok()?;
    Some((shape, target))
}

/// Emit `bs_get_integer2 Fail Ctx Live Size Unit Flags Dst` natively, or report that the
/// block must helper it (see [`try_inline_bs_get_position`] for the return convention).
///
/// The guarded sequence, in this order:
///
/// ```text
///   <Dst's Y-window guard>              ; before any store, so a rejection writes nothing
///   <the match-context guard>           ; not a context -> fallback; r11 = the block base
///   mov  rsi, [ctx + 16]                ; start
///   mov  rax, [ctx + 24]                ; end
///   rcx = end.saturating_sub(start)
///   cmp  rcx, nbits ; jb fail_arm       ; end - start < nbits -> the FAIL label
///   mov  rdx, [ctx + 32]                ; orig
///   test rdx, 1 ; jne fallback          ;    not boxed
///   test rdx, TAG_LITERAL_PTR ; jne fallback   ; a literal-area subject
///   and  rdx, !TAG_PTR_MASK ; mov rdi, [rdx]   ; the subject's header
///   cmp  rdi, HEADER_SUB_BITS ; je refc_arm
///   (rdi & _BITSTRING_TAG_MASK) == HEAP_BITS_SUBTAG ?  on-heap : fallback
/// on-heap:  r10 = rdx + 16 ; rax = [rdx + 8] ; rcx = 0
/// refc_arm: the orig must be a BinRef, else fallback
///           r10 = ([ctx + 8] & !ERL_SUB_BITS_FLAG_MASK) + (start / 8)
///           rax = [ctx + 24] - start   (end < start -> fallback)
///           rcx = start & 7
///   rax = (rcx + rax + 7) >> 3          ; num_bytes
///   rdx = start + rcx                   ; bit_pos
///   <emit_bit_extract: the guarded load + shift/mask, or fallback>
///   mov  [ctx + 16], start + nbits      ; the commit, BEFORE `Dst`
///   r8 = (r8 << 4) | TAG_IMMED1_SMALL   ; make_small, zero allocation
///   mov  <Dst>, r8
///   jmp  fast_done
/// fail_arm:
///   <the linker's exit to the fail label>     ; a terminator on that path only
/// fallback:
///   <one interpreter step>              ; the family's own typed error
/// ```
///
/// Two invariants hold by construction. **Nothing allocates** — the field is at most 32
/// bits and always fits `MAX_SMALL` — so no moving collection can run part-way through,
/// which is why the context operand is read exactly once and its block address reused. And
/// the **commit precedes the destination write**: `beam_asm:filter_essentials/1` compiles
/// `ctx == dst == x(3)`, so the family writes the context's `start` word before it binds
/// `Dst`, and this sequence does the same.
///
/// The subject forms lowered natively are exactly `raw::heap_bits`' two readable ones: an
/// on-heap `ErlHeapBits` and a refc-binary `ErlSubBits` window. A **match context**
/// (an `ErlSubBits` whose `orig` is not a `BinRef`), a bare `BinRef`, a literal-area
/// subject and every non-bitstring take `fallback`, i.e. the family's own typed error —
/// never a fabricated value.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_get_integer2(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: Label,
    ctx: &Operand,
    size: &Operand,
    unit: u32,
    flags: u32,
    dst: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, target)) = get_integer2_shape(labels, fail, ctx, size, unit, flags, dst)
    else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_GET_INTEGER2);

    let fallback: CodeLabel = buffer.label();
    let fail_arm: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();
    let sat: CodeLabel = buffer.label();
    // The destination's frame-window guard first: a rejection must leave nothing written.
    shape
        .dst
        .guard_store(buffer, Y_ADDR_SCRATCH, MOVE_SCRATCH_2, fallback)?;
    emit_ctx_guard(buffer, shape.ctx, allocation, fallback, highest_touched)?;

    // `remaining = end.saturating_sub(start)`; a field wider than what is left is the
    // FAIL label (the family's `fail_step`), never a raise and never a fallback.
    buffer.mov_rm(Reg::Rsi, MOVE_SCRATCH, SUB_BITS_START_DISP)?;
    buffer.mov_rm(Reg::Rax, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
    buffer.xor_rr(Reg::Rcx, Reg::Rcx)?;
    buffer.cmp_rr(Reg::Rax, Reg::Rsi)?;
    buffer.jcc(Cond::B, sat)?;
    buffer.mov_rr(Reg::Rcx, Reg::Rax)?;
    buffer.sub_rr(Reg::Rcx, Reg::Rsi)?;
    buffer.bind(sat)?;
    buffer.mov_ri(MOVE_SCRATCH_2, shape.nbits as i64)?;
    buffer.cmp_rr(Reg::Rcx, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::B, fail_arm)?;

    // Classify the inner subject (`orig`, word 4) exactly as `raw::heap_bits` does and
    // derive the byte base, the bit position and the byte count of the readable window —
    // one shared helper, so `bs_match`'s `integer` command cannot drift from this shape.
    let (base, bit_pos, num_bytes) = emit_subject_view(buffer, MOVE_SCRATCH, Reg::Rsi, fallback)?;
    emit_bit_extract(
        buffer,
        base,
        bit_pos,
        shape.nbits,
        num_bytes,
        Reg::R8,
        fallback,
    )?;
    // The commit **before** the destination write (`ctx` may alias `dst`).
    buffer.mov_ri(Reg::Rcx, shape.nbits as i64)?;
    buffer.add_rr(Reg::Rsi, Reg::Rcx)?;
    buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rsi)?;
    // `make_small(raw)` — `(raw << 4) | TAG_IMMED1_SMALL`, zero allocation.
    buffer.shl_ri(Reg::R8, 4)?;
    buffer.mov_ri(Reg::Rcx, SMALL_TAG)?;
    buffer.or_rr(Reg::R8, Reg::Rcx)?;
    shape
        .dst
        .store_at(buffer, Y_ADDR_SCRATCH, Reg::R8, allocation, highest_touched)?;
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

