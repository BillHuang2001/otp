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

//! The `bs_create_bin` shape: the compiler's **linear accumulator** build, lowered in
//! place.
//!
//! `bs_init_writable` followed by a `bs_create_bin` whose first segment is a
//! `private_append` of the accumulator itself and whose remaining segments are
//! statically sized fields is the binary-construction hot loop of every encoder (a
//! base62/UTF-8/JSON writer, `io_lib`, `erl_prettypr`, …): one instruction per appended
//! field, each executed once per output byte. The family's own route reserves a room
//! check, walks the prepared program, writes the field bytes and advances the
//! accumulator's window end; the two facts a tier cannot derive itself are the
//! [construction seam](crate::families::bitstrings::create::seam)'s —
//! [`seam::append_plan`] (this instruction *is* that shape, and here are its fields) and
//! [`seam::open_append`] (the accumulator has room, and here is where its window's bytes
//! start).
//!
//! # The mutation order is the whole correctness argument
//!
//! The family's build performs exactly one mutation beyond the field bytes: C's
//! `sb->end = new_position` (`erl_bits.c:1975`), i.e. `ErlSubBits.end += tail_bits`. A
//! lowering that committed that word **before** the field stores would make the
//! `fallback` (one whole interpreter step re-executing *this* instruction) append the
//! tail a second time — the window would already cover the bytes. So the emitted
//! sequence is:
//!
//! ```text
//!   <the destination's Y-window guard>             ; a rejected destination writes nothing
//!   <the accumulator's Y-window guard, if Y>       ; guard only, still before every write
//!   <one guarded load + is_small guard per Field::Bits>  ; a non-integer field is the family's badarg
//!   <the append-door helper call>                  ; x-register | Y-slot door; a refusal is the family's path
//!   test rax, rax ; je fallback                     ; the LAST guard, still before every write
//!   <the field byte stores at the returned cursor>  ; the only payload writes
//!   <the accumulator re-read>                       ; re-read: the helper clobbered every scratch
//!   mov rcx, [boxed_val(rdx) + SUB_BITS_END_DISP]   ; the old window end
//!   add rcx, tail_bits ; mov [block + END], rcx     ; THE ONLY MUTATION, last
//!   <the accumulator re-read into Dst>              ; Dst's Y address re-derived too
//!   jmp fast_done
//! fallback:
//!   <one interpreter step>                          ; re-reads an untouched accumulator
//! fast_done:
//! ```
//!
//! `jit_open_append_bin` (the x-register door) and `jit_open_append_bin_y` (the `Y`-slot
//! door) themselves **mutate nothing** (they are `&Machine` throughout), so every guard
//! that can reach `fallback` runs against the machine state the interpreter step will see.
//! The payload bytes are the one thing written before the `end` commit, and they are
//! written at the same offsets with the same values on every attempt, so even a deopt
//! after them re-produces them — never a duplicated field.
//!
//! # The `Y` forms: two doors, and why the re-reads need no second guard
//!
//! The block ABI cannot carry a run-time *word* into a helper — `emit_helper_invoke` adds
//! every argument as a `movabs` immediate — so a `Y` accumulator crosses as its **slot
//! index** and [`jit_open_append_bin_y`] re-reads it through `Machine::y`'s bounds check.
//! The emitted code, however, re-reads that slot **directly** (`y_base + k*8`) after the
//! call, because the helper clobbers every caller-saved register and the window-end commit
//! and the `Dst` store both need the term again. That direct read is unguarded, and the
//! **once-up-front guard** is what makes it sound: the frame cannot change between the
//! guard and the read (the helper only reads machine state and runs no interpreter step),
//! so a slot inside the published window at the guard is still inside it later — exactly
//! the argument [`super::Loc::store_at`] already relies on for a `Y` destination.
//!
//! Every post-helper re-read is therefore four instructions into a caller-saved scratch
//! (`mov scratch,[rsp+RUNTIME_SLOT_DISP]` ; `mov scratch,[scratch+RUNTIME_YBASE_DISP]` ;
//! `lea scratch,[scratch+y_offset(k)]` ; `mov dst,[scratch]`) and nothing more.//!
//! # The byte extraction is exact, not approximate
//!
//! A whole-byte big-endian integer field of `nbytes` bytes is the value's low
//! `8 * nbytes` bits, most significant byte first: `BitWriter::push_uint`'s whole-byte
//! path writes `(value as u64).to_be_bytes()[8 - nbytes..]` and `push_byte_field` writes
//! the same bytes through `to_be_bytes`. A small int's word is
//! `((value as u64) << 4) | TAG_IMMED1_SMALL`, so byte `i` (MSB first) is bit
//! `4 + 8 * (nbytes - 1 - i)` of that word — but the family never hands `push_byte_field`
//! that word: it hands it `small_value(field) as u64` (the 60-bit two's-complement value
//! **sign-extended** to the whole 64 bits), so the bytes to reproduce are the sign-extended
//! value's, not the zero-extended one's.
//!
//! The extraction is therefore an **arithmetic** (`sar`) shift right by that count, then a
//! **byte** store: the raw word's bit 63 *is* the small value's sign bit (its bit 59), so
//! `sar` fills the vacated high bits with that sign and the stored low byte is byte `i` of
//! `(value as i64 as u64).to_be_bytes()` — `<<-1:64>>` is `FF`×8, an 8-byte field of the
//! small `-0x0123_4567_89AB_CDEF` is `FE DC BA 98 76 54 32 11`, `<<-1:16>>` is `FF FF` and
//! an 8-bit field of `0x1FF` truncates to `FF` (`push_uint` masks nothing).
//! A **logical** (`shr`) shift would be right only for widths of ≤ 7 bytes: an 8-byte
//! field's top byte has the shift count `60`, so the zero fill reaches the stored low byte
//! and `-1` would be written as `0F FF FF FF FF FF FF FF`. For a **non-negative** field the
//! raw word's bit 63 is 0, so `sar` and `shr` agree byte for byte — which is why the
//! arithmetic shift is unconditionally the right instruction. The shift count peaks at
//! `4 + 8 * 7 = 60`, inside the encoding's one-byte immediate and below the 64-bit width.
//!
//! # What is declined at compile time
//!
//! [`CreateBinShape::of`] refuses a shape the sequence above cannot reproduce exactly,
//! and `emit_block`'s `_ if bitstrings::claimed(…)` arm keeps handling it:
//!
//! * anything [`seam::append_plan`] answers `None` for — a plain or later-position
//!   append, an append unit that does not divide 8, a `binary`/`float`/`utf*` segment, an
//!   `all` or runtime (`Dynamic`) size, a `little`/`signed`/`native` flag, a field that is
//!   not a whole number of bytes or is wider than a machine word, a `StrT` window the
//!   module cannot resolve, a plan with no fields;
//! * an accumulator or a field **value** operand that is neither an in-bound x-register nor
//!   an addressable `Y` slot ([`super::Loc::of`] — a literal-area operand, an out-of-bound
//!   register index, a `Y` index the emitted `lea`'s `disp32` could not address); both
//!   kinds the tier *can* address are lowered, and [`super::Loc::of`] is the one place that
//!   decision lives;
//! * a field width outside `8..=64` whole bytes — the exact form above;
//! * more than [`MAX_FIELDS`] fields or more than [`MAX_TAIL_BYTES`] tail bytes — the
//!   emitted sequence is straight-line code, so an unbounded segment list would be an
//!   unbounded instruction (the family's own loop is the right home for a very wide one);
//! * a field list whose byte offsets do not sum to the plan's `tail_bits / 8` — the
//!   `end` commit would then publish a window over bytes that were never written.
//!
//! The `fail` arm is deliberately **not** part of this shape: `bs_create_bin`'s
//! `NO_FAIL`-vs-label discrimination stays in the family, and every refusal above (and
//! the helper's) is one interpreter step, so the family's own value, fail transfer or
//! raise is what the process observes.

use beam_common::Result;
use beam_loader::{Module, Operand};

use crate::families::bitstrings::create::seam;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::super::runtime::{
    jit_open_append_bin, jit_open_append_bin_y, OpenAppendHelper, RUNTIME_YBASE_DISP,
};
use super::super::operands::{x_offset, y_offset, Loc, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2};
use super::super::{
    emit_positioned_helper_call, touch, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2, RUNTIME_SLOT_DISP,
};
use super::{emit_fallback_and_done, IMMED1_MASK, POINTER_MASK, SMALL_TAG, SUB_BITS_END_DISP};

#[cfg(test)]
use super::{record_inlined, SHAPE_CREATE_BIN};

/// The most fields one natively lowered build writes.
///
/// The emitted sequence is straight-line code (one guarded byte store per field byte), so
/// the cap is what bounds both the block's size and the `CREATE_BIN_PATH_BYTES` budget in
/// `emit/compile.rs`. A wider segment list is the family's own loop, which is exactly the
/// shape an unbounded list wants.
const MAX_FIELDS: usize = 16;

/// The widest tail (in bytes) one natively lowered build appends.
///
/// A `StrT` string segment can be arbitrarily long, and each byte of it is one emitted
/// store, so the tail is capped here; a longer constant is the family's own copy.
const MAX_TAIL_BYTES: usize = 256;

/// The scratch the `Y` accumulator's up-front guard computes its slot address in, and the
/// scratch its window bound goes to.
///
/// Both must be distinct from every other scratch the sequence holds live across the guard
/// phase — [`Y_ADDR_SCRATCH`]/[`Y_ADDR_SCRATCH_2`] carry the *destination*'s address and
/// bound — and neither is a cache register nor the block's reserved `rbx`/`rsp`. The value
/// the guard computes is deliberately not kept: the helper re-reads the accumulator through
/// `Machine::y`, and every later use re-derives the address behind this guard.
const SOURCE_GUARD_SCRATCH: Reg = MOVE_SCRATCH;
/// The `Y` accumulator guard's window-bound scratch (see [`SOURCE_GUARD_SCRATCH`]).
const SOURCE_GUARD_END: Reg = MOVE_SCRATCH_2;

/// One field of a lowered build, fully resolved for emission: the value's location and the
/// compile-time byte offset from the accumulator's window end.
#[derive(Clone, Debug)]
enum PreparedField<'a> {
    /// A run-time value, written MSB-first as `bits` bits.
    Bits {
        /// The value's location — an in-bound x-register or an addressable `Y` slot (see
        /// [`CreateBinShape::of`]), used for the `is_small` guard's read and for the
        /// post-helper re-read every byte store needs.
        value: Loc,
        /// The field width in bits — a whole number of bytes, `8..=64`.
        bits: u32,
        /// The field's byte offset from the write cursor.
        offset: i32,
    },
    /// Compile-time constant bytes (a `string` segment's `StrT` contents, already copied
    /// out of the module — a cached module's string area may be a different allocation, so
    /// no table offset is ever baked into code).
    Const {
        /// The literal's bytes, in order.
        bytes: &'a [u8],
        /// The field's byte offset from the write cursor.
        offset: i32,
    },
}

/// The operands of a `bs_create_bin` the tier lowers natively: the in-place accumulator,
/// the destination, and the statically resolved fields with their byte offsets from the
/// accumulator's window end.
#[derive(Clone, Debug)]
pub(crate) struct CreateBinShape {
    /// The accumulator operand's location: an in-bound x-register or an addressable `Y`
    /// slot (the emitted call's door follows from which — see the module docs).
    source: Loc,
    /// The destination: an x-register or an addressable `Y` slot.
    dst: Loc,
    /// The summed width of the fields, in bits: exactly the figure the accumulator's
    /// window end advances by (`seam::AppendPlan::tail_bits`).
    tail_bits: u32,
    /// The fields, in instruction order.
    fields: Vec<seam::Field>,
    /// The byte offset of each field from the write cursor, parallel to [`Self::fields`].
    field_offsets: Vec<u32>,
}

/// The x-register a [`seam::Field::Bits`] value operand names; `None` for a constant
/// string field or a `Y`-slot value (a stack slot is never a cached x-register).
///
/// [`CreateBinShape::of`] accepts a shape only when every field value resolves to a
/// [`Loc`], so a `None` for an *addressable* `Bits` field means a `Y` slot.
fn bits_register(field: &seam::Field) -> Option<u32> {
    match field {
        seam::Field::Bits { value, .. } => Loc::of(value).and_then(Loc::register),
        seam::Field::ConstBytes { .. } => None,
    }
}

/// The plan's fields, resolved for emission, or `None` when a read site does not resolve.
///
/// Runs **before** the first emitted byte: a shape whose field value stopped resolving
/// must emit nothing at all, because a partial sequence could not be abandoned without
/// having written into the accumulator's payload.
fn prepare_fields(shape: &CreateBinShape) -> Option<Vec<PreparedField<'_>>> {
    let mut prepared = Vec::with_capacity(shape.fields.len());
    for (field, offset) in shape.fields.iter().zip(&shape.field_offsets) {
        // `of` bounded every offset well inside `i32` (the whole tail is ≤ [`MAX_TAIL_BYTES`]).
        let offset = i32::try_from(*offset).ok()?;
        prepared.push(match field {
            seam::Field::Bits { value, bits } => PreparedField::Bits {
                value: Loc::of(value)?,
                bits: *bits,
                offset,
            },
            seam::Field::ConstBytes { bytes } => PreparedField::Const { bytes, offset },
        });
    }
    Some(prepared)
}
impl CreateBinShape {
    /// The inlinable shape of a `bs_create_bin`, or `None` when the whole instruction must
    /// stay on the family.
    ///
    /// Purely syntactic and **label-free** (it takes no `LabelMap` and no fail label), so
    /// [`super::super::regalloc`]'s four mirrors, [`super::super::arith::uses_y`], the
    /// emitter and the refusal census all derive the same answer from this one call.
    pub(crate) fn of(
        module: &Module,
        dst: &Operand,
        segments: &[Operand],
    ) -> Option<CreateBinShape> {
        let plan = seam::append_plan(module, segments)?;
        // An in-bound x-register or an addressable `Y` slot: the `Y` forms carry the
        // accumulator to the helper as a slot index and re-derive the slot's address
        // post-helper behind the one up-front guard (see the module docs). Every other
        // operand kind — a literal-area operand, an out-of-bound index, a `Y` index the
        // `disp32` cannot address — is refused here, by the one shared predicate.
        let source = Loc::of(&plan.source)?;
        let dst = Loc::of(dst)?;
        // The two shape caps (see [`MAX_FIELDS`] / [`MAX_TAIL_BYTES`]).
        if plan.fields.len() > MAX_FIELDS {
            return None;
        }
        if plan.tail_bits % 8 != 0 {
            // Unreachable for the seam's own plans (every field it accepts is whole-byte),
            // but the window commit would otherwise advance past bytes this sequence did
            // not write — the one failure mode that must never be emitted.
            return None;
        }
        if usize::try_from(plan.tail_bits).ok()? > MAX_TAIL_BYTES * 8 {
            return None;
        }
        let mut field_offsets = Vec::with_capacity(plan.fields.len());
        let mut offset: u32 = 0;
        for field in &plan.fields {
            let bytes = match field {
                seam::Field::Bits { value, bits } => {
                    if *bits == 0 || *bits % 8 != 0 || *bits > 64 {
                        return None;
                    }
                    // An addressable `Y` value is lowered too: its guard is the `Loc::load`
                    // above, and its byte stores re-read the slot unguarded post-helper.
                    Loc::of(value)?;
                    *bits / 8
                }
                seam::Field::ConstBytes { bytes } => u32::try_from(bytes.len()).ok()?,
            };
            // Every store's displacement is `cursor + offset`, a `disp32`; the whole tail is
            // capped at [`MAX_TAIL_BYTES`], so this is belt and braces.
            if i32::try_from(offset).is_err() {
                return None;
            }
            field_offsets.push(offset);
            offset = offset.checked_add(bytes)?;
        }
        // The running offset must land exactly on the window advance: a mismatch means the
        // field list and the tail width disagree (see the module docs).
        if offset != plan.tail_bits / 8 {
            return None;
        }
        Some(CreateBinShape {
            source,
            dst,
            tail_bits: plan.tail_bits,
            fields: plan.fields,
            field_offsets,
        })
    }

    /// The x-register indices the shape touches — the accumulator and every field value
    /// (a `Y` slot contributes nothing: it is a stack slot, never a cached x-register).
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(1 + self.fields.len());
        registers.extend(self.source.register());
        registers.extend(self.fields.iter().filter_map(bits_register));
        registers
    }

    /// The x-register indices the shape **reads** — the accumulator and every field value.
    ///
    /// The whole read set: the accumulator is read again for the window commit and every
    /// field value again for its own byte stores, so a missed entry load here would be a
    /// stale read, not merely a redundant one.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.registers()
    }

    /// The x-register indices the shape **writes** — the destination, and nothing else (a
    /// `Y` destination contributes nothing).
    ///
    /// Charging it is mandatory: a dropped write lets a stale cache register survive a
    /// block exit, the one unsound failure mode of the register cache.
    pub(crate) fn writes(&self) -> Vec<u32> {
        self.dst.register().into_iter().collect()
    }

    /// Whether the lowering accesses the current frame's Y window — a `Y` destination, a
    /// `Y` accumulator or a `Y` field value.
    pub(crate) fn uses_y(&self) -> bool {
        self.source.is_y()
            || self.dst.is_y()
            || self.fields.iter().any(|field| match field {
                seam::Field::Bits { value, .. } => Loc::of(value).is_some_and(Loc::is_y),
                seam::Field::ConstBytes { .. } => false,
            })
    }
}

/// Emit `bs_create_bin Dst Segments` natively, or report that the block must helper it
/// (`Ok(false)` with **zero bytes emitted**).
///
/// See the module docs for the emitted sequence, the mutation order it is built on and the
/// shapes [`CreateBinShape::of`] declines.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_create_bin(
    buffer: &mut CodeBuffer,
    module: &Module,
    dst: &Operand,
    segments: &[Operand],
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = CreateBinShape::of(module, dst, segments) else {
        return Ok(false);
    };
    let Some(prepared) = prepare_fields(&shape) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_CREATE_BIN);

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // (1) The destination's frame-window guard, so a rejected slot writes nothing at all.
    shape
        .dst
        .guard_store(buffer, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)?;

    // (1b) The accumulator's frame-window guard, for the `Y` form: guard **only** — the
    // helper reaches the term through `Machine::y` itself — emitted here so the post-helper
    // unguarded re-reads of that slot (sections 4-6) are behind a proven in-frame address.
    // The frame cannot move between this guard and those reads: the helper only reads
    // machine state and runs no interpreter step.
    shape
        .source
        .guard_load(buffer, SOURCE_GUARD_SCRATCH, SOURCE_GUARD_END, fallback)?;

    // (2) One `is_small` guard per integer field, still before any write: a non-integer
    // value is the family's `badarg` (`erts_bs_put_integer_*` returns 0 for a non-integer,
    // `erl_bits.c:952`), reported by the `fallback` interpreter step. A `Y` value's guard is
    // [`Loc::load`]'s guarded read, in the same position.
    for field in &prepared {
        let PreparedField::Bits { value, .. } = field else {
            continue;
        };
        value.load(
            buffer,
            allocation,
            Reg::Rdx,
            Reg::Rcx,
            fallback,
            highest_touched,
        )?;
        buffer.mov_ri(Reg::Rax, IMMED1_MASK)?;
        buffer.and_rr(Reg::Rdx, Reg::Rax)?;
        buffer.mov_ri(Reg::Rax, SMALL_TAG)?;
        buffer.cmp_rr(Reg::Rdx, Reg::Rax)?;
        buffer.jcc(Cond::Ne, fallback)?;
    }

    // (3) The construction seam's run-time half: `rax` returns the write cursor, or `0` for
    // every refusal — the last guard, and it costs exactly one test because the helper
    // mutates nothing. The accumulator crosses as a **slot index** (the block ABI carries
    // `movabs` immediates only), and which door resolves it follows from the `Loc`: the
    // x-register door reads `machine.x`, the `Y` door `Machine::y`.
    let (helper, source_slot): (OpenAppendHelper, u32) = match shape.source {
        Loc::X(x) => (jit_open_append_bin, x),
        Loc::Y(k) => (jit_open_append_bin_y, k),
    };
    emit_positioned_helper_call(
        buffer,
        allocation,
        helper as *const (),
        &[i64::from(source_slot), i64::from(shape.tail_bits)],
        func,
        pc,
    )?;
    buffer.test_rr(Reg::Rax, Reg::Rax)?;
    buffer.jcc(Cond::E, fallback)?;

    // (4) The field byte stores at the returned cursor. Every value is re-read after the
    // helper: an x-register from `machine.x` (`rbx` is callee-saved, and the spill before
    // the call made that array authoritative for a cached register too), a `Y` slot through
    // its re-derived address — once per field into `rcx`, since its four-instruction
    // re-derivation should not be paid per byte.
    for field in &prepared {
        match field {
            PreparedField::Bits {
                value, bits, offset, ..
            } => {
                let nbytes = (*bits / 8) as usize;
                match *value {
                    Loc::X(_) => {}
                    Loc::Y(k) => emit_y_slot_value(buffer, Reg::Rcx, k)?,
                }
                for i in 0..nbytes {
                    match *value {
                        Loc::X(x) => {
                            buffer.mov_rm(Reg::Rdx, Reg::Rbx, x_offset(x))?;
                            touch(highest_touched, x);
                        }
                        Loc::Y(_) => buffer.mov_rr(Reg::Rdx, Reg::Rcx)?,
                    }
                    // Byte `i` (MSB first) of the big-endian field is word bits
                    // `4 + 8 * (nbytes - 1 - i)` .. +8 (see the module docs), and the shift
                    // is ARITHMETIC precisely because the caller wants
                    // `(value as i64 as u64).to_be_bytes()`: the raw word's bit 63 is the
                    // small value's sign bit, so `sar` sign-extends the 60-bit value to the
                    // 64 bits `push_byte_field` receives, where a logical shift's zero fill
                    // would corrupt a negative 8-byte field's top byte (`-1` as `0F FF …`).
                    buffer.sar_ri(Reg::Rdx, (4 + 8 * (nbytes - 1 - i)) as u32)?;
                    buffer.mov_mr8(Reg::Rax, offset + i as i32, Reg::Rdx)?;
                }
            }
            PreparedField::Const { bytes, offset } => {
                // Eight bytes per `movabs` + 64-bit store (the bytes are baked as an
                // immediate — never as a `StrT` pointer, which a cached module's string area
                // would invalidate), then the remaining tail one byte at a time.
                let mut written = 0usize;
                while written + 8 <= bytes.len() {
                    let mut chunk = [0u8; 8];
                    chunk.copy_from_slice(&bytes[written..written + 8]);
                    buffer.mov_ri(Reg::Rdx, i64::from_le_bytes(chunk))?;
                    buffer.mov_mr(Reg::Rax, offset + written as i32, Reg::Rdx)?;
                    written += 8;
                }
                while written < bytes.len() {
                    buffer.mov_ri(Reg::Rdx, i64::from(bytes[written]))?;
                    buffer.mov_mr8(Reg::Rax, offset + written as i32, Reg::Rdx)?;
                    written += 1;
                }
            }
        }
    }

    // (5) The window-end commit — `sb->end = new_position` (`erl_bits.c:1975`), the build's
    // result, and **the only mutation** the sequence performs beyond the payload bytes.
    match shape.source {
        Loc::X(x) => {
            buffer.mov_rm(Reg::Rdx, Reg::Rbx, x_offset(x))?;
            touch(highest_touched, x);
        }
        Loc::Y(k) => emit_y_slot_value(buffer, Reg::Rdx, k)?,
    }
    buffer.mov_ri(Reg::Rax, POINTER_MASK)?;
    buffer.and_rr(Reg::Rdx, Reg::Rax)?;
    buffer.mov_rm(Reg::Rcx, Reg::Rdx, SUB_BITS_END_DISP)?;
    buffer.mov_ri(Reg::Rax, i64::from(shape.tail_bits))?;
    buffer.add_rr(Reg::Rcx, Reg::Rax)?;
    buffer.mov_mr(Reg::Rdx, SUB_BITS_END_DISP, Reg::Rcx)?;

    // (6) The accumulator term into the destination — the very word the instruction was
    // given, because the seam proved the build takes the in-place route (`grow`, republish
    // and promotion to the copy-rebuild form all decline). Nothing after this may fail.
    if let Loc::Y(k) = shape.dst {
        // The window guard already ran and nothing between it and here moved the frame (the
        // helper only reads machine state), so the slot address is re-derived without a
        // second guard.
        emit_y_slot_address(buffer, Y_ADDR_SCRATCH, k)?;
    }
    match shape.source {
        Loc::X(x) => {
            buffer.mov_rm(Reg::Rdx, Reg::Rbx, x_offset(x))?;
            touch(highest_touched, x);
        }
        Loc::Y(k) => emit_y_slot_value(buffer, Reg::Rdx, k)?,
    }
    shape
        .dst
        .store_at(buffer, Y_ADDR_SCRATCH, Reg::Rdx, allocation, highest_touched)?;

    emit_fallback_and_done(buffer, allocation, done, func, pc, fallback, fast_done)
}

/// Re-derive `Y` slot `k`'s **address** into `dst` from the block's own `rt` slot and the
/// current frame's published `y_base`, with **no** frame-window guard.
///
/// Sound only behind the up-front [`Loc::guard_load`] of the same slot (see the module
/// docs): the frame cannot change between them, so the guard's proof still holds here.
fn emit_y_slot_address(buffer: &mut CodeBuffer, dst: Reg, k: u32) -> Result<()> {
    buffer.mov_rm(dst, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_rm(dst, dst, RUNTIME_YBASE_DISP)?;
    buffer.lea(dst, dst, y_offset(k))
}

/// Re-derive `Y` slot `k`'s **word** into `dst`, with no frame-window guard (see
/// [`emit_y_slot_address`]).
fn emit_y_slot_value(buffer: &mut CodeBuffer, dst: Reg, k: u32) -> Result<()> {
    emit_y_slot_address(buffer, dst, k)?;
    buffer.mov_rm(dst, dst, 0)
}
