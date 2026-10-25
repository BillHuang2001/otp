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

//! One bit-granular field's **payload byte stores**: the straight-line code
//! `Route::BitGranular` emits for a field that fits one 64-bit register, with no helper
//! call and no second consultation of the construction seam.
//!
//! # The rule these stores reproduce
//!
//! [`crate::families::bitstrings::raw::write_bit_field`] is the one definition of the
//! family's bit-granular write, and it writes bit by bit through
//! `raw::write_payload_bit`'s three cases: a write that **enters** a byte (`cur % 8 == 0`)
//! clears it first, a write at the accumulator's window end (`cur == start_bit`) that lands
//! *inside* a byte keeps that byte's high `cur % 8` bits, and each bit is OR-ed in at
//! `7 - cur % 8`.
//!
//! [`crate::families::bitstrings::create::seam::open_append_any`] proves the accumulator's
//! window `start` is `0` and answers its `end`, so a field at compile-time offset `F` starts
//! at `cur = end + F`, `p0 = cur % 8` and `cur == start_bit` exactly when `F == 0` — which
//! turns the rule's first two cases into **compile-time** choices, and the third into a
//! byte store whose bits are this field's.
//!
//! # The word the bytes are extracted from
//!
//! A small integer's word is `(value << 4) | TAG_IMMED1_SMALL` (the family's
//! `small_value(word) as u64` sign-extends that to 64 bits, and only the low `bits` are
//! ever written), so the field's low `bits` bits are the word's bits `4 .. 4 + bits`.
//! Left-aligning them by `64 - bits - p0` gives
//!
//! ```text
//!   W = (low bits of value) << (64 - bits - p0)
//! ```
//!
//! whose bit `63` is the field's MSB and which places byte `j`'s bits (`j = 0` is the byte
//! holding `cur`) in `W >> (56 - 8 * j)`: byte `j` is the low byte of that shift, which is
//! why every store below is a `mov_mr8` of a compile-time right shift.
//! The exponent is a **left** shift by the run-time amount `64 - bits - p0`, which is only
//! non-negative for every `p0` in `0 ..= 7` when `bits <= 57` — that is
//! [`MAX_INLINE_FIELD_BITS`], and the one reason a wider field keeps the per-field helper
//! call.
//!
//! # Why the door is not consulted again
//!
//! [`super::try_inline_bs_create_bin`]'s up-front general door already asked
//! `seam::open_append_any` for the **whole plan**'s room (`end + tail_bits <= capacity`),
//! and every field's `[F, F + bits)` lies inside `[0, tail_bits)`, so the per-field room the
//! helper (`jit_write_bit_field`) re-verifies is implied; its append-unit gate is the
//! plan's own, applied by that same up-front query; and its `is_small` refusal is the
//! arm's up-front per-field guard. Dropping the per-field call therefore changes nothing
//! but the instructions executed.
//!
//! # Registers
//!
//! Everything here runs **after** the door call, so every caller-saved register is dead
//! except the two the arm keeps ([`FieldScratch::base`] = the payload base the door answered
//! in `rax`, [`FieldScratch::end`] = the window end) and the value loaded for this field.
//! None of them is a block-cache register (`r12`–`r15`, `rbp`), the machine base (`rbx`) or
//! the block's `rt` slot (`rsp`), and no instruction after the door call calls a helper, so
//! nothing is spilled by hand and no second instruction position is published.

use beam_common::{BeamError, Result};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

/// The widest field whose `bits` fit left-aligned in one 64-bit register for **every**
/// window offset `p0` in `0 ..= 7`: [`emit_bit_field_store`] shifts the field left by
/// `64 - bits - p0`, which is non-negative for every `p0` exactly when `bits <= 57`.
///
/// A plan with a wider field keeps the per-field `jit_write_bit_field` call, whose
/// `u64` write models a field of up to 64 bits.
pub(super) const MAX_INLINE_FIELD_BITS: u32 = 57;

/// The number of low bits of a small integer's word the tag occupies, i.e. the shift that
/// removes it (`beam_term`'s `signed_value` performs the same transfer in Rust).
const SMALL_WORD_SHIFT: u8 = 4;

/// The scratch registers one field store computes in, beside the value it is handed and
/// the two registers the arm keeps live across its fields.
/// [`PRODUCTION_FIELD_SCRATCH`] is the assignment
/// [`super::try_inline_bs_create_bin`] emits with; a unit test drives the same function
/// with the same assignment behind a hand-written prologue.
#[derive(Clone, Copy, Debug)]
pub(super) struct FieldScratch {
    /// The payload's first data byte (the door call's `rax`), preserved across the plan's
    /// fields.
    pub base: Reg,
    /// The accumulator's window end in bits, preserved across the plan's fields.
    pub end: Reg,
    /// The shift count: every run-time-count shift goes through `cl`, so this is `rcx`.
    pub count: Reg,
    /// `p0 = (end + offset) & 7`.
    pub p0: Reg,
    /// The field's byte address `base + ((end + offset) >> 3)`.
    pub addr: Reg,
    /// The byte being built (byte `0`'s bits, then a copy of `W` for the later bytes).
    pub byte: Reg,
    /// The byte read back from the payload, then `W`'s copies and the small constants.
    pub temp: Reg,
    /// The mask of byte `0`'s kept bits, then the passed-down copy of `W`.
    pub temp2: Reg,
}

/// The register assignment the arm emits with: `r11`/`r10` are the two `move` scratch
/// registers (`MOVE_SCRATCH`/`MOVE_SCRATCH_2`, dead after the door call and reused for the
/// payload base and the window end), `rax` carries the value, and the rest are the
/// caller-saved registers no cache slot, `rbx` or `rsp` occupies.
///
/// `r9`/`r8` are also `Y_ADDR_SCRATCH`/`Y_ADDR_SCRATCH_2`, which the arm uses for the
/// destination's window guard (before this phase) and for its store (after it), never
/// during it; the `Y` accumulator's own guard computes into `r11`/`r10` and deliberately
/// keeps nothing.
pub(super) const PRODUCTION_FIELD_SCRATCH: FieldScratch = FieldScratch {
    base: super::MOVE_SCRATCH,
    end: super::MOVE_SCRATCH_2,
    count: Reg::Rcx,
    p0: Reg::R9,
    addr: Reg::Rdi,
    byte: Reg::Rdx,
    temp: Reg::R8,
    temp2: Reg::Rsi,
};

/// Emit the stores that write **one** bit-granular field: its `bits` bits, MSB-first, at
/// the absolute bit position `end + offset_bits` of the payload [`FieldScratch::base`]
/// names.
///
/// `value` must already hold the field's **raw small-integer word** (a copy of
/// `machine.x[x]`, or a `Y` slot's word — the arm's up-front per-field guard is what proves
/// it is one), and is clobbered. Every register in `scratch` other than
/// [`FieldScratch::base`]/[`FieldScratch::end`] is clobbered. `bits` must be in
/// `1 ..= `[`MAX_INLINE_FIELD_BITS`] (a typed [`BeamError::BadArg`] otherwise, which the
/// caller's own gate makes unreachable).
///
/// The emitted bytes are the family's own rule bit for bit (see the module docs): byte `0`
/// keeps or clears its high `p0` bits exactly as `raw::write_payload_bit` does, and every
/// later byte the field spans is entered at position `0` and so is a plain store of this
/// field's bits.
pub(super) fn emit_bit_field_store(
    buffer: &mut CodeBuffer,
    value: Reg,
    bits: u32,
    offset_bits: u32,
    scratch: &FieldScratch,
) -> Result<()> {
    if bits == 0 || bits > MAX_INLINE_FIELD_BITS {
        return Err(BeamError::BadArg);
    }
    let FieldScratch {
        base,
        end,
        count,
        p0,
        addr,
        byte,
        temp,
        temp2,
    } = *scratch;

    // `p0 = (end + offset) & 7`, then the address register becomes
    // `base + ((end + offset) >> 3)` — the byte holding the window position.
    buffer.lea(addr, end, offset_bits as i32)?;
    buffer.mov_rr(p0, addr)?;
    buffer.mov_ri(temp, 7)?;
    buffer.and_rr(p0, temp)?;
    buffer.shr_ri(addr, 3)?;
    buffer.add_rr(addr, base)?;

    // The field's low `bits` bits, then `W` (see the module docs): the tag goes first, then
    // the two immediate shifts keep exactly the low `bits`, and the run-time left shift
    // aligns them so that bit 63 is the field's MSB at byte `p0`.
    buffer.shr_ri(value, SMALL_WORD_SHIFT as u8)?;
    buffer.shl_ri(value, 64 - bits)?;
    buffer.shr_ri(value, (64 - bits) as u8)?;
    buffer.mov_ri(count, i64::from(64 - bits))?;
    buffer.sub_rr(count, p0)?;
    buffer.shl_cl(value)?;

    // Byte 0: `W`'s top byte, which is exactly the field bits this byte receives (the bits
    // past the field's end are zero, because `W`'s low `64 - bits - p0` bits are).
    buffer.mov_rr(byte, value)?;
    buffer.shr_ri(byte, 56)?;
    buffer.movzx_byte(temp, addr, 0)?;
    if offset_bits == 0 {
        // The field starts at the accumulator's window end, so the rule's **mask** case
        // applies: keep this byte's high `p0` bits and replace the rest.
        // For `p0 == 0` the mask is `0xFF << 8 == 0xFF00`, and `old` is a zero-extended
        // byte, so `old & mask` is `0` — which is exactly the rule's "enters a byte,
        // clear first" case, with no branch.
        buffer.mov_ri(temp2, 0xFF)?;
        buffer.mov_ri(count, 8)?;
        buffer.sub_rr(count, p0)?;
        buffer.shl_cl(temp2)?;
        buffer.and_rr(temp, temp2)?;
        buffer.or_rr(temp, byte)?;
        buffer.mov_mr8(addr, 0, temp)?;
    } else {
        // The field starts past the window's first bit, so the rule clears nothing — except
        // when the write enters the byte at position `0`, where it clears it first. That is
        // a run-time test on `p0` (`offset_bits != 0` still admits `p0 == 0`: the arm's
        // append unit admits a window end the offset is a multiple of).
        let merge: CodeLabel = buffer.label();
        let clear: CodeLabel = buffer.label();
        buffer.test_rr(p0, p0)?;
        buffer.jcc(Cond::E, clear)?;
        buffer.or_rr(temp, byte)?;
        buffer.mov_mr8(addr, 0, temp)?;
        buffer.jmp(merge)?;
        buffer.bind(clear)?;
        buffer.mov_mr8(addr, 0, byte)?;
        buffer.bind(merge)?;
    }

    // Bytes `1 ..`: each is entered at position `0`, so each is cleared and then filled
    // with this field's bits — a plain store of `W >> (56 - 8 * j)`'s low byte, taken from
    // one passed-down copy of `W` (every shift below is a compile-time count, and no shift
    // ever discards a bit this field still needs, because the extraction walks **down** the
    // byte indices).
    let whole = bits.div_ceil(8);
    // `ceil(bits / 8) - 1` bytes are always written; one more is written exactly when the
    // field reaches past them, i.e. when `p0 > 8 * ceil(bits / 8) - bits`. That threshold
    // is `7` only for a one-bit field, whose field is the only one that can never reach a
    // second byte.
    let threshold = whole * 8 - bits;
    if threshold < 7 {
        let skip: CodeLabel = buffer.label();
        buffer.mov_rr(temp2, value)?;
        buffer.shr_ri(temp2, (56 - 8 * whole) as u8)?;
        buffer.mov_ri(temp, i64::from(threshold))?;
        buffer.cmp_rr(p0, temp)?;
        buffer.jcc(Cond::Le, skip)?;
        buffer.mov_mr8(addr, whole as i32, temp2)?;
        buffer.bind(skip)?;
        for j in (1..whole).rev() {
            buffer.shr_ri(temp2, 8)?;
            buffer.mov_mr8(addr, j as i32, temp2)?;
        }
    } else if whole >= 2 {
        buffer.mov_rr(temp2, value)?;
        buffer.shr_ri(temp2, (56 - 8 * (whole - 1)) as u8)?;
        buffer.mov_mr8(addr, (whole - 1) as i32, temp2)?;
        for j in (1..whole - 1).rev() {
            buffer.shr_ri(temp2, 8)?;
            buffer.mov_mr8(addr, j as i32, temp2)?;
        }
    }
    Ok(())
}
