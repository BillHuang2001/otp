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

//! The baseline JIT's native lowerings for the bitstring family: eleven **small, safe**
//! match-context shapes (the seven position/tail/start-match/integer extractors, the static
//! `bs_match` and `bs_match_string` compares, and the `bs_skip_utf32`/`bs_get_utf32`
//! character pair) plus the variable-width character duo `bs_skip_utf16`/`bs_get_utf16` and
//! the `bs_create_bin` private-append construction shape.
//!
//! Every lowering here is allocation-free and raises nothing of its own: the match-context
//! shapes read the `ErlSubBits` words and (for the mutating ones) write one word back, while
//! the construction shape calls its route's append door — plus one `jit_write_bit_field` per
//! field on the bit-granular route — every one of those helpers **mutating nothing**, and
//! then writes the accumulator's payload bytes and its window end.
//! A shape the module cannot prove safe is **refused at compile time** by its `*_shape`
//! predicate — returning `Ok(false)` with **zero bytes emitted** — so `emit_block`'s
//! existing `_ if bitstrings::claimed(instr) => emit_bitstr(...)` arm keeps handling it
//! exactly as before. A shape whose operands are addressable but whose *run-time* value
//! is not a match context branches to a local `fallback` label that runs exactly one
//! interpreter step ([`emit_step_and_continue`]), so the interpreter's own typed error
//! (`bs_get_position: operand is not a match context`, …) is what the process sees.
//!
//! # Frozen layouts read here
//!
//! `ErlSubBits` is `[thing_word, base_flags, start, end, orig]`
//! (`erts/emulator/beam/erl_bits.h:60-82`), so word 2 is the current **read position**
//! in bits and word 3 the subject size in bits. A term is a match context iff
//! `beam_term::is_boxed` holds and its header word is `HEADER_SUB_BITS` — exactly
//! `raw::ctx_parts`'s test.
//!
//! # The invariants these lowerings keep
//!
//! * **M before W where both happen.** Three shapes both advance the match context **and**
//!   write a destination — `bs_get_integer2`, `bs_get_utf32` and `bs_get_utf16` — and all
//!   three store the advanced `start` word **before** the destination write, because `Dst`
//!   may be the very same location as `Ctx` (`beam_asm:filter_essentials/1` compiles that).
//!   The mutating shapes without a destination (`bs_set_position`, `bs_skip_bits2`,
//!   `bs_skip_utf32`, `bs_skip_utf16`) have nothing to order against.
//! * **No allocation, hence no re-read.** Every sequence below allocates nothing, so no
//!   moving collection can run part-way through one, which is why the context operand is
//!   read exactly once and its block address reused. The construction shape re-reads its
//!   accumulator from `machine.x` after the helper call — not for a collection, but because
//!   the call clobbers every scratch register.
//! * **A `fallback` is a deopt, so every sequence is re-execution idempotent.** The
//!   construction shape therefore commits the accumulator's window end **last**, after every
//!   payload byte store, and its helper mutates nothing (see [`create`]).
//!
//! Per-shape ground truth (the interpreter is the in-tree oracle):
//! `crate::families::bitstrings::matching`.
//!
//! # Layout
//!
//! One file per shape group, plus the machinery they share (this file):
//!
//! * [`position`] — `bs_get_position` + `bs_set_position`
//! * [`tail`] — `bs_test_tail2` + `bs_skip_bits2`
//! * [`start_match`] — `bs_start_match3` + `bs_start_match4`
//! * [`integer`] — `bs_get_integer2`, and the tier's one width/flag rule
//! * [`bs_match`] — `bs_match` over a fully static command list
//! * [`match_string`] — `bs_match_string`
//! * [`utf32`] — `bs_skip_utf32` + `bs_get_utf32`
//! * [`utf16`] — `bs_skip_utf16` + `bs_get_utf16`
//! * [`create`] — `bs_create_bin` over the private-append accumulator
//! * [`tests`] — the in-crate unit suite
//!
//! This file owns the frozen layout constants ([`SUB_BITS_START_DISP`] and friends), the
//! per-shape `#[cfg(test)]` counters ([`inlined`]), the two shared emitters
//! ([`emit_ctx_guard`], [`emit_fallback_and_done`]) and the two shared field-extraction
//! pieces ([`emit_bit_extract`], [`emit_subject_view`]) that the two field-extracting
//! shapes (`bs_get_integer2`, `bs_match`) both build on.

use beam_common::Result;
use beam_term::{
    Eterm, BITSTRING_TAG_MASK, HEADER_BIN_REF, HEADER_SUB_BITS,
    HEAP_BITS_SUBTAG, TAG_IMMED1_MASK, TAG_IMMED1_SMALL, TAG_LITERAL_PTR, TAG_PRIMARY_BOXED,
    TAG_PRIMARY_MASK, TAG_PTR_MASK,
};

use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::{emit_step_and_continue, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2};
use super::operands::Loc;

mod bs_match;
mod create;
mod integer;
mod match_string;
mod position;
mod start_match;
mod tail;
#[cfg(test)]
mod tests;
mod utf16;
mod utf32;

/// The `bs_match_string` arm's own suite (boundaries, the legitimate-deopt idempotency pin,
/// the compile-time refusals and the live `erl` oracle cross-check).
#[cfg(test)]
mod match_string_tests;
/// The `bs_skip_utf32`/`bs_get_utf32` arm's own suite (the BMP/astral/boundary rows, the
/// `dst == ctx` hazard row, the legitimate-deopt idempotency pin and the live `erl` oracle
/// cross-check).
#[cfg(test)]
mod utf32_tests;
/// The `bs_skip_utf16` arm's own suite (the BMP and surrogate-pair rows, the four malformed
/// characters, the exactly-once character-consumption pin and the live `erl` oracle
/// cross-check).
#[cfg(test)]
mod utf16_tests;
pub(super) use bs_match::try_inline_bs_match;
pub(crate) use bs_match::{bs_match_shape, BsMatchShape};
#[cfg(test)]
pub(crate) use bs_match::{bs_match_group_len, bs_match_integer_flags, bs_match_static_size};
pub(super) use create::try_inline_bs_create_bin;
pub(crate) use create::CreateBinShape;
/// The route split, re-exported for the arm's own tests (the emitter reaches it through
/// [`CreateBinShape::route`]).
#[cfg(test)]
pub(crate) use create::Route;
pub(super) use integer::try_inline_bs_get_integer2;
pub(crate) use integer::{
    field_is_unsupported, field_width_supported, get_integer2_shape, GetInteger2Shape,
};
#[cfg(test)]
pub(crate) use integer::field_is_little;
pub(super) use match_string::try_inline_bs_match_string;
pub(crate) use match_string::{match_string_shape, MatchStringShape};
/// The shape's own code-size cap, re-exported for the arm's tests: the refusal row is
/// "a pattern **longer** than this", so a test must derive the bound from the shape rather
/// than hard-code 64 (a widened cap would otherwise leave the row asserting nothing).
#[cfg(test)]
pub(crate) use match_string::MAX_MATCH_BYTES;
pub(super) use position::{try_inline_bs_get_position, try_inline_bs_set_position};
pub(crate) use position::{get_position_shape, set_position_shape};
pub(super) use start_match::{try_inline_bs_start_match3, try_inline_bs_start_match4};
pub(crate) use start_match::{start_match3_shape, start_match4_shape, StartMatchShape};
#[cfg(test)]
pub(crate) use start_match::MatchFail;
pub(super) use tail::{try_inline_bs_skip_bits2, try_inline_bs_test_tail2};
pub(crate) use tail::{skip_bits2_shape, test_tail2_shape, SkipBits2Shape, TestTail2Shape};
pub(super) use utf32::{try_inline_bs_get_utf32, try_inline_bs_skip_utf32};
pub(crate) use utf32::{get_utf32_shape, skip_utf32_shape, Utf32Shape};
pub(super) use utf16::{try_inline_bs_get_utf16, try_inline_bs_skip_utf16};
pub(crate) use utf16::{get_utf16_shape, skip_utf16_shape, Utf16Shape};

// ---------------------------------------------------------------------------
// Constants (derived from the frozen layout owners, never re-derived)
// ---------------------------------------------------------------------------

/// `TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED` — the mask of the `is_boxed` bit trick
/// (`beam_term::boxed::is_boxed`), which is `0x1`.
const NOT_BOXED_MASK: i64 = (TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED) as i64;

/// `!TAG_PTR_MASK` — the mask `boxed_val` clears the pointer tag (and the literal-area
/// bit) with.
const POINTER_MASK: i64 = !(TAG_PTR_MASK as i64);

/// The `HEADER_SUB_BITS` header word, as the word compare materialises it.
const SUB_BITS_HEADER: i64 = HEADER_SUB_BITS.raw() as i64;

/// The byte offset of an `ErlSubBits`'s `start` word (word index 2).
const SUB_BITS_START_DISP: i32 = (2 * core::mem::size_of::<Eterm>()) as i32;

/// The byte offset of an `ErlSubBits`'s `end` word (word index 3).
const SUB_BITS_END_DISP: i32 = (3 * core::mem::size_of::<Eterm>()) as i32;

/// The byte offset of an `ErlSubBits`'s `orig` word (word index 4) — the inner subject
/// the context was created over (`erts/emulator/beam/erl_bits.h:60-82`).
const SUB_BITS_ORIG_DISP: i32 = (4 * core::mem::size_of::<Eterm>()) as i32;

/// The byte offset of an `ErlHeapBits`'s bit-`size` word (word index 1): the layout is
/// `[thing_word, size_in_bits, data…]` (`erts/emulator/beam/erl_bits.h:150-164`).
const HEAP_BITS_SIZE_DISP: i32 = (core::mem::size_of::<Eterm>()) as i32;

/// The byte offset of an `ErlHeapBits`'s first data byte (word index
/// `ERL_HEAP_BITS_SIZE` = 2) — `beam_term::bitstring_parts`' `p.add(ERL_HEAP_BITS_SIZE)`.
const HEAP_BITS_DATA_DISP: i32 = (2 * core::mem::size_of::<Eterm>()) as i32;

/// `TAG_LITERAL_PTR` — the literal-area marker ORed into a `BOXED` word's pointer tag.
/// A subject term carrying it points into a module's **literal area**, which this tier
/// deliberately leaves to the family (see [`try_inline_bs_get_integer2`]).
const LITERAL_PTR_BIT: i64 = TAG_LITERAL_PTR as i64;

/// `BSF_LITTLE` (`erts/emulator/beam/erl_bits.h:462`): the field is little-endian.
///
/// Re-declared here because the family's copy is `pub(super)` inside the private
/// `families::bitstrings` subtree — a **derivation** of the frozen C constants, not a
/// second policy: the values are pinned against the family's by
/// `super::bitstr_inline::tests::the_flag_bits_match_the_familys_own_constants`.
const BSF_LITTLE: u32 = 2;
/// `BSF_SIGNED` (`erts/emulator/beam/erl_bits.h:463`): the field is signed (see
/// [`BSF_LITTLE`] for why the value is re-declared).
const BSF_SIGNED: u32 = 4;
/// `BSF_ALIGNED` (`erts/emulator/beam/erl_bits.h:461`): irrelevant to the value (see
/// [`BSF_LITTLE`] for why the value is re-declared).
const BSF_ALIGNED: u32 = 1;
/// `BSF_EXACT` (`erts/emulator/beam/erl_bits.h:464`): irrelevant to the value (see
/// [`BSF_LITTLE`]).
const BSF_EXACT: u32 = 8;
/// `BSF_NATIVE` (`erts/emulator/beam/erl_bits.h:465`): the field is in native
/// endianness, which on this target means little-endian (see [`BSF_LITTLE`]).
const BSF_NATIVE: u32 = 16;

/// `_BITSTRING_TAG_MASK` — the header mask `beam_term::boxed::is_bitstring_header` tests,
/// which both bitstring headers (`HEAP_BITS_SUBTAG`/`SUB_BITS_SUBTAG`) share.
const BITSTRING_TAG_MASK_WORD: i64 = BITSTRING_TAG_MASK as i64;

/// `HEAP_BITS_SUBTAG` — the header subtag a bitstring header carries.
const HEAP_BITS_SUBTAG_WORD: i64 = HEAP_BITS_SUBTAG as i64;

/// `HEADER_BIN_REF` — an off-heap `BinRef`'s header word
/// (`beam_term::boxed::is_bin_ref`). An `ErlSubBits` whose `orig` is one is a **refc
/// window** over an off-heap payload, i.e. a fresh *subject*, not a match context.
const BIN_REF_HEADER: i64 = HEADER_BIN_REF.raw() as i64;

/// The byte offset of a term's header word (a boxed term points *at* it).
const HEADER_DISP: i32 = 0;

/// `_TAG_IMMED1_MASK` — the four-bit level-1 immediate tag mask.
const IMMED1_MASK: i64 = TAG_IMMED1_MASK as i64;

/// `_TAG_IMMED1_SMALL` — the small-integer level-1 immediate tag.
const SMALL_TAG: i64 = TAG_IMMED1_SMALL as i64;

/// The scan of the number of the last inlined shape, for the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_COUNT: usize = 14;
/// The `bs_get_position` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_GET_POSITION: usize = 0;
/// The `bs_set_position` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_SET_POSITION: usize = 1;
/// The `bs_test_tail2` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_TEST_TAIL2: usize = 2;
/// The `bs_skip_bits2` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_SKIP_BITS2: usize = 3;
/// The `bs_start_match3` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_START_MATCH3: usize = 4;
/// The `bs_start_match4` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_START_MATCH4: usize = 5;
/// The `bs_get_integer2` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_GET_INTEGER2: usize = 6;
/// The `bs_match` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_BS_MATCH: usize = 7;
/// The `bs_create_bin` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_CREATE_BIN: usize = 8;
/// The `bs_match_string` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_BS_MATCH_STRING: usize = 9;
/// The `bs_skip_utf32` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_SKIP_UTF32: usize = 10;
/// The `bs_get_utf32` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_GET_UTF32: usize = 11;
/// The `bs_skip_utf16` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_SKIP_UTF16: usize = 12;
/// The `bs_get_utf16` shape index in the test-build instrument.
#[cfg(test)]
pub(crate) const SHAPE_GET_UTF16: usize = 13;

#[cfg(test)]
thread_local! {
    /// How often each shape's native lowering was **emitted** (a compile-time count, so it
    /// proves the inline arm ran even where no run-time instrument could observe it).
    static INLINED: [std::cell::Cell<u64>; SHAPE_COUNT] =
        [const { std::cell::Cell::new(0) }; SHAPE_COUNT];
}

/// How many times `shape`'s native lowering has been emitted on this thread.
///
/// A **compile-time** instrument: it counts the emitter's decision, never run-time helper
/// entries. The two kinds of shape here need that count for different reasons.
///
/// For the **thirteen helper-free** shapes — the eleven small match-context shapes plus the
/// variable-width `bs_skip_utf16`/`bs_get_utf16` pair — the emission is the only observable
/// there can be: such a shape calls no helper at all, so
/// [`super::super::bitstr::bitstr_calls`] never moves for it and "the inline arm ran" *is*
/// the decision to emit.
///
/// For [`SHAPE_CREATE_BIN`] the sequence **does** call helpers — one append door, plus one
/// `jit_write_bit_field` per field on the bit-granular route — so a run-time count would
/// observe *a* helper but not the arm: which door it is, and how many calls follow it,
/// depends on the route the plan's widths pick. The emission is therefore what proves the
/// arm ran for **either** route, and it is also the proof that a plan
/// `CreateBinShape::of` **refused** emitted zero bytes (the refusal happens before the
/// first byte, so the counter cannot move).
///
/// Either way, pair it with a zero [`super::super::bitstr_inline_tests::static_deopts`]
/// count for the same variant to prove the whole instruction stayed native.
#[cfg(test)]
pub(crate) fn inlined(shape: usize) -> u64 {
    INLINED.with(|counts| counts[shape].get())
}

#[cfg(test)]
fn record_inlined(shape: usize) {
    INLINED.with(|counts| counts[shape].set(counts[shape].get() + 1));
}

// ---------------------------------------------------------------------------
// The shared match-context guard
// ---------------------------------------------------------------------------

/// Emit the guard that `ctx` holds a live `ErlSubBits` **match context**, leaving the
/// block's base address (header word) in [`MOVE_SCRATCH`] and jumping to `fallback` on
/// any other value.
///
/// The test is `raw::ctx_parts`'s, branch for branch: `beam_term::is_boxed` (the
/// `(word & 1) == 0` bit trick), then `*boxed_val(word) == HEADER_SUB_BITS`. A
/// non-context is *not* an error here — the `fallback` interpreter step reports the
/// family's own typed `unsupported`, which is why the guard can only ever reject.
fn emit_ctx_guard(
    buffer: &mut CodeBuffer,
    ctx: Loc,
    allocation: &Allocation,
    fallback: CodeLabel,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    ctx.load(
        buffer,
        allocation,
        MOVE_SCRATCH,
        MOVE_SCRATCH_2,
        fallback,
        highest_touched,
    )?;
    // `is_boxed(word)` — the low bit clear.
    buffer.mov_ri(MOVE_SCRATCH_2, NOT_BOXED_MASK)?;
    buffer.test_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::Ne, fallback)?;
    // `boxed_val(word)` — the header-word pointer.
    buffer.mov_ri(MOVE_SCRATCH_2, POINTER_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    // The header word must be `HEADER_SUB_BITS`.
    buffer.mov_rm(Reg::Rax, MOVE_SCRATCH, HEADER_DISP)?;
    buffer.mov_ri(MOVE_SCRATCH_2, SUB_BITS_HEADER)?;
    buffer.cmp_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::Ne, fallback)
}

/// Emit the shared tail of every shape: the `fast_done` jump, the `fallback` interpreter
/// step and the `fast_done` binding the rest of the block continues after.
fn emit_fallback_and_done(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    fallback: CodeLabel,
    fast_done: CodeLabel,
) -> Result<bool> {
    buffer.jmp(fast_done)?;
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// The shared bit-extraction emitter
// ---------------------------------------------------------------------------

/// Emit the extraction of a big-endian bit field of `nbits` bits at `bit_pos` into
/// `scratch`, or jump to `fallback` when the field's shape is not one this sequence can
/// read without leaving the subject's own bytes.
///
/// `base` holds the subject's byte base pointer, `bit_pos` the absolute bit position of
/// the field's first bit **within that byte view**, `num_bytes` the number of readable
/// bytes from `base` (the subject's own `ceil((off + size) / 8)` figure) and `nbits` the
/// field width, a compile-time constant. Bit `i` of the view is bit
/// `i % 8` (MSB first) of `base[i / 8]`, exactly as
/// [`raw::subject_bytes`](crate::families::bitstrings::raw::subject_bytes) hands the
/// interpreter's readers a `(bytes, off)` pair.
///
/// Two forms, chosen at run time from `bit_pos % 8`:
///
/// ```text
///   test bit_pos, 7 ; jne granular        ; only when nbits is 8/16/32
///   ; the byte-aligned form: W byte loads, each shifted up and or-ed in
///   mov  rdi, bit_pos ; shr rdi, 3        ; j = bit_pos / 8
///   lea  rcx, [rdi + W]                   ; the read's exclusive byte bound
///   cmp  rcx, num_bytes ; ja fallback     ; j + W <= num_bytes, checked FIRST
///   add  rdi, base
///   movzx  scratch, byte [rdi]            ; the field's most significant byte
///   ; ... W - 1 times:
///   shl  scratch, 8 ; movzx rcx, byte [rdi + i] ; or scratch, rcx
///   jmp  done
/// granular:
///   mov  rdi, bit_pos ; shr rdi, 3        ; j
///   lea  rcx, [rdi + 2]                   ; the two-byte window's exclusive bound
///   cmp  rcx, num_bytes ; ja fallback     ; j + 2 <= num_bytes, checked FIRST
///   add  rdi, base
///   movzx  scratch, byte [rdi]            ; the window's high byte
///   shl  scratch, 8
///   movzx  rcx, byte [rdi + 1]            ; and its low byte
///   or   scratch, rcx                     ; the 16-bit BIG-ENDIAN window
///   mov  rdi, bit_pos ; and rdi, 7        ; rem = bit_pos % 8
///   cmp  rdi, 16 - nbits ; ja fallback    ; rem + nbits <= 16
///   mov  rcx, 16 - nbits ; sub rcx, rdi   ; the shift count 16 - rem - nbits
///   shr  scratch, cl                      ; drop the low 16 - rem - nbits
///   and  scratch, (1 << nbits) - 1        ; keep exactly nbits   (nbits < 16 only)
/// done:
/// ```
///
/// Both loads are **byte-wise and assembled most-significant byte first**, because the
/// field is big-endian (bit `i` of the view is bit `i % 8`, MSB first, of `base[i / 8]`)
/// while an x86 multi-byte load is little-endian: a plain 2-/4-byte load would produce the
/// byte-reversed value — a wrong result with no error surfaced, which is exactly what
/// `../../bitstr_inline_tests.rs`'s 16-/32-bit rows pin.
///
/// The **over-read guard is always emitted before the load**, so the sequence never reads
/// a byte outside the subject's own buffer, and a field that would need one is the
/// `fallback` interpreter step rather than a fabricated value. The `fallback` labels are
/// the caller's, so its shared interpreter step is the single place the family's typed
/// error comes from.
///
/// A width no form covers (`nbits > 16` that is not 8/16/32, or a 16/32-bit field seen
/// off an 8-bit boundary) is an unconditional jump to `fallback` — a run-time rejection,
/// never a wrong value; the caller's shape predicate refuses those widths at **compile**
/// time anyway, so the emitted jump is only reachable through a caller that skipped it.
///
/// # Register contract
///
/// `base`, `bit_pos` and `num_bytes` are **read only** (the sequence works in
/// [`Reg::Rcx`] and [`Reg::Rdi`]), `scratch` receives the value and may be used as one
/// more work register before it does, and `scratch` must not be `base`, `bit_pos`,
/// `num_bytes`, [`Reg::Rcx`] or [`Reg::Rdi`]. `cl` carries the granular form's shift
/// count, exactly as [`CodeBuffer::shr_cl`] requires.
pub(crate) fn emit_bit_extract(
    buffer: &mut CodeBuffer,
    base: Reg,
    bit_pos: Reg,
    nbits: usize,
    num_bytes: Reg,
    scratch: Reg,
    fallback: CodeLabel,
) -> Result<()> {
    /// The one byte-aligned load width, in bytes, the aligned form implements.
    fn aligned_width(nbits: usize) -> Option<i32> {
        match nbits {
            8 => Some(1),
            16 => Some(2),
            32 => Some(4),
            _ => None,
        }
    }

    // The compile-time shift/limit magnitudes. `nbits <= 16` is the granular form's
    // precondition and `nbits <= 32` bounds every mask the aligned form's clears need.
    if nbits > 32 {
        // No form covers a wider field: an unconditional rejection (the caller's shape
        // predicate already refuses it at compile time).
        return buffer.jmp(fallback);
    }

    // The form choice's two labels. Both are always bound (a buffer with an unbound label
    // cannot be sealed), even on the paths that never jump to one of them.
    let granular: CodeLabel = buffer.label();
    let done: CodeLabel = buffer.label();

    if let Some(width) = aligned_width(nbits) {
        // `bit_pos % 8 == 0` -> the byte-aligned form; otherwise the granular one.
        buffer.mov_ri(Reg::Rcx, 7)?;
        buffer.test_rr(bit_pos, Reg::Rcx)?;
        buffer.jcc(Cond::Ne, granular)?;

        // `j = bit_pos / 8`; the over-read guard `j + W <= num_bytes` comes FIRST.
        buffer.mov_rr(Reg::Rdi, bit_pos)?;
        buffer.shr_ri(Reg::Rdi, 3)?;
        buffer.lea(Reg::Rcx, Reg::Rdi, width)?;
        buffer.cmp_rr(Reg::Rcx, num_bytes)?;
        buffer.jcc(Cond::A, fallback)?;
        buffer.add_rr(Reg::Rdi, base)?;
        // The field is **big-endian** (bit `i` of the view is bit `i % 8`, MSB first, of
        // `base[i / 8]`) while an x86 load is little-endian, so the value is assembled
        // most-significant byte first in `scratch` — one `movzx` per byte, each earlier
        // byte shifted up by eight. For `W == 1` this is the single load the aligned form
        // is for; the bound check above left `Reg::Rcx` dead, which is why it serves as the
        // byte temporary without disturbing the register contract.
        buffer.movzx_byte(scratch, Reg::Rdi, 0)?;
        for byte in 1..width {
            buffer.shl_ri(scratch, 8)?;
            buffer.movzx_byte(Reg::Rcx, Reg::Rdi, byte)?;
            buffer.or_rr(scratch, Reg::Rcx)?;
        }
        buffer.jmp(done)?;
    }

    // The granular form: a 16-bit big-endian window assembled from the two bytes at `j`,
    // then a variable shift. It covers `bit_pos % 8 + nbits <= 16`, so a 16- or 32-bit
    // field reaching this label can never satisfy it (its `rem` is non-zero here, and
    // `16 - nbits <= 0`), which is why such a width is a plain rejection instead.
    buffer.bind(granular)?;
    if nbits >= 16 {
        buffer.jmp(fallback)?;
    } else {
        // `j = bit_pos / 8`; the over-read guard `j + 2 <= num_bytes` comes FIRST, before
        // either byte of the window is read.
        buffer.mov_rr(Reg::Rdi, bit_pos)?;
        buffer.shr_ri(Reg::Rdi, 3)?;
        buffer.lea(Reg::Rcx, Reg::Rdi, 2)?;
        buffer.cmp_rr(Reg::Rcx, num_bytes)?;
        buffer.jcc(Cond::A, fallback)?;
        buffer.add_rr(Reg::Rdi, base)?;
        // The window's own big-endian value: `base[j] << 8 | base[j + 1]` (`Reg::Rcx` is
        // dead after the bound check, so it carries the low byte).
        buffer.movzx_byte(scratch, Reg::Rdi, 0)?;
        buffer.shl_ri(scratch, 8)?;
        buffer.movzx_byte(Reg::Rcx, Reg::Rdi, 1)?;
        buffer.or_rr(scratch, Reg::Rcx)?;
        // `rem = bit_pos % 8`, then `rem + nbits <= 16`, i.e. `rem <= 16 - nbits` — the
        // window's own reach. Both are recomputed here because the byte temporary above
        // consumed `Reg::Rcx`, whose `cl` carries the shift count only from here on.
        buffer.mov_rr(Reg::Rdi, bit_pos)?;
        buffer.mov_ri(Reg::Rcx, 7)?;
        buffer.and_rr(Reg::Rdi, Reg::Rcx)?;
        buffer.mov_ri(Reg::Rcx, (16 - nbits) as i64)?;
        buffer.cmp_rr(Reg::Rdi, Reg::Rcx)?;
        buffer.jcc(Cond::A, fallback)?;
        // The shift count `16 - rem - nbits` into `cl`.
        buffer.sub_rr(Reg::Rcx, Reg::Rdi)?;
        buffer.shr_cl(scratch)?;
        // Keep exactly `nbits` bits. A 16-bit window already bounds the field, so the mask
        // is only needed below that (`nbits == 0` masks everything away, which is exactly
        // the zero-bit field's value).
        let mask = ((1u64 << nbits) - 1) as i64;
        buffer.mov_ri(Reg::Rcx, mask)?;
        buffer.and_rr(scratch, Reg::Rcx)?;
    }
    // The granular form (and, when no aligned form exists, the whole sequence) falls
    // through to `done`.
    buffer.bind(done)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The shared subject view: (base, bit_pos, num_bytes) of a readable window
// ---------------------------------------------------------------------------

/// Emit the subject classification both field-extracting shapes share, and hand back the
/// three registers [`emit_bit_extract`] consumes: the **byte base** of the readable window,
/// the **bit position** of `pos` inside it and that window's **readable byte count**.
///
/// Only `raw::heap_bits`' **on-heap** readable form is lowered: an `ErlHeapBits` (base = its
/// data region, bit position = `pos`, since the form's bit offset is always `0`). Every other
/// value jumps to `fallback`, i.e. one interpreter step — the family then answers the true
/// value or its own typed error. That covers a non-boxed word, a literal-area term, a
/// **refc window** (an `ErlSubBits` over a `BinRef`, whose payload byte base is not the
/// context's `base_flags` — see below), a match context used as a subject, a bare `BinRef`
/// and every non-bitstring; no path here fabricates a view.
///
/// **`num_bytes` is the window's readable byte count rounded up to whole words**, not the
/// strict `ceil(bits / 8)`: an `ErlHeapBits` block is `[header, size, data…]` with its data
/// region padded to a whole word (`heap_bits_size_words`), so the bytes between the last
/// logical byte and the end of that final word are allocated and readable. The wider bound
/// only ever admits a read of such a padding byte, and [`emit_bit_extract`]'s granular form
/// reads a second byte **only** for a field whose bits reach past the first one — every
/// field bit lies inside the remaining window, hence inside the logical subject — so the
/// extra byte can never contribute a bit of the result. The narrow bound was a real
/// performance bug: a `<<_:2, Field:6>>` match over a **1-byte** subject (a `base62`-style
/// per-character binary) needs no second byte at all, yet `ceil(8 / 8) = 1` refused the
/// two-byte window and forced a deopt on every character.
///
/// # Register contract
///
/// Returns `base = `[`MOVE_SCRATCH_2`], `bit_pos = `[`Reg::Rdx`], `num_bytes = `[`Reg::Rax`].
/// The sequence also works in [`Reg::Rdi`] and [`Reg::Rcx`] (which [`emit_bit_extract`] owns
/// itself), so `ctx` and `pos` must be distinct from all five and are only ever read.
fn emit_subject_view(
    buffer: &mut CodeBuffer,
    ctx: Reg,
    pos: Reg,
    fallback: CodeLabel,
) -> Result<(Reg, Reg, Reg)> {
    buffer.mov_rm(Reg::Rdx, ctx, SUB_BITS_ORIG_DISP)?;
    buffer.mov_ri(Reg::Rax, NOT_BOXED_MASK)?;
    buffer.test_rr(Reg::Rdx, Reg::Rax)?;
    buffer.jcc(Cond::Ne, fallback)?;
    // A literal-area subject: the tier leaves it to the family (a documented choice, not a
    // gap — the words are read-only and the family reads them the same way, but a literal
    // subject never needs the copy-avoidance this sequence buys).
    buffer.mov_ri(Reg::Rax, LITERAL_PTR_BIT)?;
    buffer.test_rr(Reg::Rdx, Reg::Rax)?;
    buffer.jcc(Cond::Ne, fallback)?;
    // `boxed_val(orig)`, then its header word.
    buffer.mov_ri(Reg::Rax, POINTER_MASK)?;
    buffer.and_rr(Reg::Rdx, Reg::Rax)?;
    buffer.mov_rm(Reg::Rdi, Reg::Rdx, HEADER_DISP)?;
    // An `ErlSubBits` as the inner subject is **not** lowered: a refc window's byte base is
    // its own payload pointer plus its window bit offset (`raw::heap_bits`), never the
    // context's `base_flags`, and a match context is not a readable subject at all. Both are
    // one interpreter step — the family's answer, correct by construction.
    buffer.mov_ri(Reg::Rax, SUB_BITS_HEADER)?;
    buffer.cmp_rr(Reg::Rdi, Reg::Rax)?;
    buffer.jcc(Cond::E, fallback)?;
    // Only an on-heap `ErlHeapBits` is a readable subject (a bare `BinRef`, a tuple, an
    // atom, … is the family's typed error).
    buffer.mov_ri(Reg::Rax, BITSTRING_TAG_MASK_WORD)?;
    buffer.and_rr(Reg::Rdi, Reg::Rax)?;
    buffer.mov_ri(Reg::Rax, HEAP_BITS_SUBTAG_WORD)?;
    buffer.cmp_rr(Reg::Rdi, Reg::Rax)?;
    buffer.jcc(Cond::Ne, fallback)?;
    // The on-heap arm: base = the data region (word 2), bit position = `pos` (offset 0).
    buffer.lea(MOVE_SCRATCH_2, Reg::Rdx, HEAP_BITS_DATA_DISP)?;
    buffer.mov_rm(Reg::Rax, Reg::Rdx, HEAP_BITS_SIZE_DISP)?;
    // `num_bytes = round_up_8(ceil(size / 8)) = ((size + 63) >> 6) << 3` — the block's own
    // data-region extent, which is word-padded (see the doc comment).
    buffer.mov_ri(Reg::Rdi, 63)?;
    buffer.add_rr(Reg::Rax, Reg::Rdi)?;
    buffer.shr_ri(Reg::Rax, 6)?;
    buffer.shl_ri(Reg::Rax, 3)?;
    buffer.mov_rr(Reg::Rdx, pos)?;
    Ok((MOVE_SCRATCH_2, Reg::Rdx, Reg::Rax))
}
