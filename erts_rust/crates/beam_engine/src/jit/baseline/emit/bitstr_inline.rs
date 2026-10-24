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

//! The baseline JIT's native lowerings for the bitstring family's **small, safe**
//! match-context shapes.
//!
//! Every lowering here is allocation-free and raises nothing of its own: it reads the
//! `ErlSubBits` match-context words and (for the mutating shapes) writes one word back.
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
//! # The two invariants these lowerings keep
//!
//! * **M before W.** No shape here both advances the match context and writes a
//!   destination; the two mutating shapes (`bs_set_position`, `bs_skip_bits2`) have no
//!   destination at all, so the "write `start` before `Dst`" rule cannot be violated.
//! * **No allocation, hence no re-read.** Every sequence below allocates nothing, so no
//!   moving collection can run part-way through one, which is why the context operand is
//!   read exactly once and its block address reused.
//!
//! Per-shape ground truth (the interpreter is the in-tree oracle):
//! `crate::families::bitstrings::matching`.

use beam_common::Result;
use beam_loader::{Label, Module, Operand, NO_FAIL};
use beam_term::{
    Eterm, BITSTRING_TAG_MASK, ERL_SUB_BITS_FLAG_MASK, HEADER_BIN_REF, HEADER_SUB_BITS,
    HEAP_BITS_SUBTAG, MAX_SMALL, TAG_IMMED1_MASK, TAG_IMMED1_SMALL, TAG_LITERAL_PTR,
    TAG_PRIMARY_BOXED, TAG_PRIMARY_MASK, TAG_PTR_MASK,
};

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::link::{ExitSpill, Linker};
use super::operands::{Loc, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2};
use super::{emit_step_and_continue, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2};

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

/// The byte offset of an `ErlSubBits`'s `base_flags` word (word index 1) — the low
/// [`ERL_SUB_BITS_FLAG_MASK`] bits are flags and the rest is the subject's byte base
/// pointer (`erl_sub_bits_get_base`, `erts/emulator/beam/erl_bits.h:124-125`).
const SUB_BITS_BASE_DISP: i32 = (core::mem::size_of::<Eterm>()) as i32;

/// `!ERL_SUB_BITS_FLAG_MASK` — the mask an `ErlSubBits::base_flags` word needs before it
/// is usable as a pointer (`beam_term::bitstring::ERL_SUB_BITS_FLAG_MASK`).
const SUB_BITS_BASE_MASK: i64 = !(ERL_SUB_BITS_FLAG_MASK as i64);

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
pub(crate) const SHAPE_COUNT: usize = 8;
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

#[cfg(test)]
thread_local! {
    /// How often each shape's native lowering was **emitted** (a compile-time count, so
    /// it proves the inline arm ran even though the sequence calls no helper).
    static INLINED: [std::cell::Cell<u64>; SHAPE_COUNT] =
        [const { std::cell::Cell::new(0) }; SHAPE_COUNT];
}

/// How many times `shape`'s native lowering has been emitted on this thread.
///
/// A **compile-time** instrument: unlike [`super::super::bitstr::bitstr_calls`] it does
/// not count run-time helper entries (an inlined shape calls no helper at all), it counts
/// the emitter's decision — which is exactly what "the inline arm ran" means for a
/// guard-only lowering. Pair it with a zero `deopts_by_variant` count for the same
/// variant to prove the whole instruction stayed native.
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
pub(super) fn try_inline_bs_get_position(
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
pub(super) fn try_inline_bs_set_position(
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
    /// so the block's Y gate ([`super::arith::uses_y`]) derives the same answer as the
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
pub(super) fn try_inline_bs_test_tail2(
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
pub(super) fn try_inline_bs_skip_bits2(
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

// ---------------------------------------------------------------------------
// Shapes 5/6: `bs_start_match3 { fail, bin, live, dst }` and
//              `bs_start_match4 { fail, live, src, dst }`
// ---------------------------------------------------------------------------

/// How a `bs_start_match3`/`bs_start_match4` fails at run time — the failure half of
/// the family's [`enter_match`](crate::families::bitstrings::matching::enter_match)
/// plus its `fail_step`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MatchFail {
    /// `bs_start_match4 resume`: the source must already be a match context, and any
    /// other value is the family's `NO_FAIL` raise (building the reason allocates), so
    /// a miss is the interpreter step.
    Resume,
    /// A resolvable module-local fail label, already resolved to its `(func, pc)` jump
    /// target — the family's `Step::Goto`.
    Label((u32, u32)),
    /// Everything the family has to decide itself: a `NO_FAIL` fail code (the
    /// allocating `{badmatch, Subject}` raise) or the `no_fail` atom.
    Family,
}

/// The operands of a `bs_start_match3`/`bs_start_match4` the tier lowers natively.
#[derive(Clone, Copy)]
pub(crate) struct StartMatchShape {
    /// The subject operand's location.
    src: Loc,
    /// The destination the match context is written to.
    dst: Loc,
}

impl StartMatchShape {
    /// The [`StartMatchShape`] two subject/destination operands name, or `None` when
    /// either is neither an in-bound x-register nor an addressable `Y` slot.
    ///
    /// Purely syntactic and **label-free** — the fail classification is the caller's — so
    /// the block's Y gate ([`super::arith::uses_y`]) derives the same answer as the
    /// emitter without needing the label map.
    pub(crate) fn of(src: &Operand, dst: &Operand) -> Option<StartMatchShape> {
        Some(StartMatchShape {
            src: Loc::of(src)?,
            dst: Loc::of(dst)?,
        })
    }

    /// The x-register indices the shape touches — the subject and the destination.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(2);
        registers.extend(self.src.register());
        registers.extend(self.dst.register());
        registers
    }

    /// The x-register indices the shape **writes** — the destination only. This is the
    /// one x-register a caller of [`super::regalloc::cacheable_writes`] must never miss:
    /// the reused context word is stored into `Dst`'s cache register, so a block that
    /// did not charge it would let a stale cache register survive an exit.
    pub(crate) fn writes(&self) -> Vec<u32> {
        self.dst.register().into_iter().collect()
    }

    /// The x-register indices the shape **reads through the block register cache** — the
    /// subject only.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.src.register().into_iter().collect()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.src.is_y() || self.dst.is_y()
    }
}

/// The inlinable shape of a `bs_start_match3`, or `None` when the whole instruction must
/// stay on the family.
///
/// A `NO_FAIL` fail code raises `{badmatch, Subject}` on a miss, and building that reason
/// allocates — so it is classified [`MatchFail::Family`], which routes every miss to the
/// interpreter step. A real fail label must resolve in the module's map, because the miss
/// transfer needs its `(function, code)` target; an unresolved one refuses here so the
/// family reports its exact `unknown_label`.
pub(crate) fn start_match3_shape(
    labels: &LabelMap,
    fail: Label,
    bin: &Operand,
    dst: &Operand,
) -> Option<(StartMatchShape, MatchFail)> {
    let shape = StartMatchShape::of(bin, dst)?;
    let fail = if fail == NO_FAIL {
        MatchFail::Family
    } else {
        MatchFail::Label(labels.resolve(fail).ok()?)
    };
    Some((shape, fail))
}

/// The inlinable shape of a `bs_start_match4`, or `None` when the whole instruction must
/// stay on the family.
///
/// The `fail` operand is decoded exactly as
/// [`resolve_match4_fail`](crate::families::bitstrings::matching::resolve_match4_fail)
/// decodes it (`emu/ops.tab:1243-1262`): a label (`NO_FAIL` = "raise on failure"), or the
/// atom `no_fail` / `resume`. Any other atom, an unnameable atom id or a non-label/non-atom
/// operand is refused, so the family keeps its typed `unsupported`.
pub(crate) fn start_match4_shape(
    module: &Module,
    labels: &LabelMap,
    fail: &Operand,
    src: &Operand,
    dst: &Operand,
) -> Option<(StartMatchShape, MatchFail)> {
    let shape = StartMatchShape::of(src, dst)?;
    let fail = match fail {
        Operand::Label(label) => {
            if *label == NO_FAIL {
                MatchFail::Family
            } else {
                MatchFail::Label(labels.resolve(*label).ok()?)
            }
        }
        Operand::Atom(atom) => match module.atom_name(*atom)? {
            "no_fail" => MatchFail::Family,
            "resume" => MatchFail::Resume,
            // `resolve_match4_fail`'s own `unsupported` — refuse so the family reports it.
            _ => return None,
        },
        _ => return None,
    };
    Some((shape, fail))
}

/// Emit the shared `bs_start_match3`/`bs_start_match4` lowering (see
/// [`try_inline_bs_start_match3`] for the return convention and the sequence).
///
/// No path here allocates, so a moving collection cannot run part-way through one: the
/// subject is read once and the reused context word is a **plain move** of the very word
/// the family would re-bind.
#[allow(clippy::too_many_arguments)]
fn emit_start_match(
    buffer: &mut CodeBuffer,
    shape: StartMatchShape,
    fail: MatchFail,
    linker: &Linker<'_>,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();
    // The match-failure arm. Only a resolved label needs its own block: every other
    // failure class is the family's own work, i.e. the interpreter step.
    let (miss, miss_target) = match fail {
        MatchFail::Label(target) => (buffer.label(), Some(target)),
        MatchFail::Resume | MatchFail::Family => (fallback, None),
    };

    // The destination's frame-window guard **first**: a rejection must write nothing.
    shape
        .dst
        .guard_store(buffer, Y_ADDR_SCRATCH, MOVE_SCRATCH_2, fallback)?;
    shape.src.load(
        buffer,
        allocation,
        MOVE_SCRATCH,
        MOVE_SCRATCH_2,
        fallback,
        highest_touched,
    )?;
    // The answer a *hit* stores: the subject word itself (the family's
    // `write_operand(dst, context)` with `context == value`).
    buffer.mov_rr(Reg::Rsi, MOVE_SCRATCH)?;

    // `is_boxed(value)`: a non-boxed subject is neither a bitstring nor a context.
    buffer.mov_ri(MOVE_SCRATCH_2, NOT_BOXED_MASK)?;
    buffer.test_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::Ne, miss)?;
    // `boxed_val(value)` — the header word's address.
    buffer.mov_ri(MOVE_SCRATCH_2, POINTER_MASK)?;
    buffer.and_rr(MOVE_SCRATCH, MOVE_SCRATCH_2)?;
    buffer.mov_rm(Reg::Rdx, MOVE_SCRATCH, HEADER_DISP)?;
    buffer.mov_ri(MOVE_SCRATCH_2, SUB_BITS_HEADER)?;
    buffer.cmp_rr(Reg::Rdx, MOVE_SCRATCH_2)?;
    // `HEADER_SUB_BITS` is the whole test for the `resume` form
    // (`raw::ctx_parts`): the operand already **is** the mutable context.
    if fail == MatchFail::Resume {
        buffer.jcc(Cond::Ne, fallback)?;
        shape
            .dst
            .store_at(buffer, Y_ADDR_SCRATCH, Reg::Rsi, allocation, highest_touched)?;
        buffer.jmp(fast_done)?;
        buffer.bind(fallback)?;
        emit_step_and_continue(buffer, done, allocation, func, pc)?;
        buffer.bind(fast_done)?;
        return Ok(());
    }

    // Not an `ErlSubBits`: a heap-bits bitstring (or a refc window, handled below) is a
    // **fresh subject** the family re-wraps at bit 0 over its whole size — that
    // reservation is the family's `make_ctx`, so it is the interpreter step. Anything
    // else (a bare `BinRef`, a tuple, an atom, …) is the match failure.
    let check_orig: CodeLabel = buffer.label();
    buffer.jcc(Cond::E, check_orig)?;
    buffer.mov_rr(Reg::Rax, Reg::Rdx)?;
    buffer.mov_ri(MOVE_SCRATCH_2, BITSTRING_TAG_MASK_WORD)?;
    buffer.and_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.mov_ri(MOVE_SCRATCH_2, HEAP_BITS_SUBTAG_WORD)?;
    buffer.cmp_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::E, fallback)?;
    buffer.jmp(miss)?;

    // An `ErlSubBits`: a match context **unless** its `orig` is a `BinRef`, i.e. unless it
    // is a refc-binary window — which `raw::heap_bits` reads as a subject (so
    // `enter_match` re-wraps it) before `raw::ctx_parts` ever runs.
    buffer.bind(check_orig)?;
    buffer.mov_rm(Reg::Rax, MOVE_SCRATCH, SUB_BITS_ORIG_DISP)?;
    let hit: CodeLabel = buffer.label();
    buffer.mov_ri(MOVE_SCRATCH_2, NOT_BOXED_MASK)?;
    buffer.test_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::Ne, hit)?;
    buffer.mov_ri(MOVE_SCRATCH_2, POINTER_MASK)?;
    buffer.and_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.mov_rm(Reg::Rax, Reg::Rax, HEADER_DISP)?;
    buffer.mov_ri(MOVE_SCRATCH_2, BIN_REF_HEADER)?;
    buffer.cmp_rr(Reg::Rax, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::E, fallback)?;
    buffer.bind(hit)?;

    shape
        .dst
        .store_at(buffer, Y_ADDR_SCRATCH, Reg::Rsi, allocation, highest_touched)?;
    buffer.jmp(fast_done)?;

    if let Some((target_func, target_pc)) = miss_target {
        buffer.bind(miss)?;
        linker.emit_exit(
            buffer,
            allocation,
            done,
            target_func,
            target_pc,
            pc,
            ExitSpill::Required,
        )?;
    }

    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(())
}

/// Emit `bs_start_match3 Fail Bin Live Dst` natively, or report that the block must helper
/// it (see [`try_inline_bs_get_position`] for the return convention).
///
/// The hit path is the family's **reuse** arm — `write_operand(dst, value)`, a plain move
/// of the subject word with **no** reservation — so the guarded sequence is:
///
/// ```text
///   <Dst's Y-window guard>            ; before any store, so a rejection writes nothing
///   mov  r11, <Bin>                   ; the subject word (also the reused answer)
///   mov  rsi, r11
///   test r11, 1 ; jne miss            ; not boxed -> neither a bitstring nor a context
///   and  r11, !TAG_PTR_MASK           ; boxed_val
///   mov  rdx, [r11 + 0]               ; the header word
///   cmp  rdx, HEADER_SUB_BITS
///   jne  not_a_ctx                    ; a fresh subject, or a match failure
///   mov  rax, [r11 + 32]              ; an ErlSubBits' orig
///   test rax, 1 ; jne hit             ; a non-boxed orig: a real match context
///   and  rax, !TAG_PTR_MASK ; mov rax, [rax + 0]
///   cmp  rax, HEADER_BIN_REF
///   je   fallback                     ; a refc window: the family re-wraps it
/// hit:
///   mov  <Dst>, rsi                   ; the very same context word
///   jmp  fast_done
/// not_a_ctx:
///   (header & _BITSTRING_TAG_MASK) == HEAP_BITS_SUBTAG ? jmp fallback : jmp miss
/// miss:                               ; only when the fail label resolved
///   <the linker's exit to the fail label>
/// fallback:
///   <one interpreter step>            ; the family's `make_ctx` / `badmatch` raise
/// ```
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_bs_start_match3(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: Label,
    bin: &Operand,
    dst: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, fail)) = start_match3_shape(labels, fail, bin, dst) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_START_MATCH3);
    emit_start_match(
        buffer,
        shape,
        fail,
        linker,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
    Ok(true)
}

/// Emit `bs_start_match4 Fail Live Src Dst` natively, or report that the block must
/// helper it (see [`try_inline_bs_get_position`] for the return convention).
///
/// The `resume` form reduces to `move Src Dst` (the family's `ctx_parts` test is the only
/// condition), so its sequence is the same guard with a single `HEADER_SUB_BITS` compare
/// — every other fail class reuses [`emit_start_match`]'s full classification.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_bs_start_match4(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: &Operand,
    src: &Operand,
    dst: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, fail)) = start_match4_shape(module, labels, fail, src, dst) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_START_MATCH4);
    emit_start_match(
        buffer,
        shape,
        fail,
        linker,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
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
    /// so the block's Y gate ([`super::arith::uses_y`]) derives the same answer as the
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
fn field_is_little(flags: u32) -> bool {
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
pub(super) fn try_inline_bs_get_integer2(
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

// ---------------------------------------------------------------------------
// The shared subject view: (base, bit_pos, num_bytes) of a readable window
// ---------------------------------------------------------------------------

/// Emit the subject classification both field-extracting shapes share, and hand back the
/// three registers [`emit_bit_extract`] consumes: the **byte base** of the readable window,
/// the **bit position** of `pos` inside it and that window's **byte count**.
///
/// Exactly `raw::heap_bits`' two readable subject forms: an on-heap `ErlHeapBits` (base =
/// its data region, bit position = `pos`, size = its own bit-size word) and a refc `BinRef`
/// window `ErlSubBits` (base = its byte base plus `pos / 8`, size = `end - pos`, off = `pos
/// & 7`, bit position = `pos + off`). Every other value — a literal-area term, a match
/// context, a bare `BinRef`, a non-boxed word — jumps to `fallback`, i.e. the family's own
/// typed error through one interpreter step; no path here fabricates a view.
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
    let refc_arm: CodeLabel = buffer.label();
    let tail: CodeLabel = buffer.label();
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
    buffer.mov_ri(Reg::Rax, SUB_BITS_HEADER)?;
    buffer.cmp_rr(Reg::Rdi, Reg::Rax)?;
    buffer.jcc(Cond::E, refc_arm)?;
    // Not an `ErlSubBits`: only an on-heap `ErlHeapBits` is a readable subject (a bare
    // `BinRef`, a tuple, an atom, … is the family's typed error).
    buffer.mov_ri(Reg::Rax, BITSTRING_TAG_MASK_WORD)?;
    buffer.and_rr(Reg::Rdi, Reg::Rax)?;
    buffer.mov_ri(Reg::Rax, HEAP_BITS_SUBTAG_WORD)?;
    buffer.cmp_rr(Reg::Rdi, Reg::Rax)?;
    buffer.jcc(Cond::Ne, fallback)?;
    // The on-heap arm: base = the data region (word 2), size = the `size` word, off = 0.
    buffer.lea(MOVE_SCRATCH_2, Reg::Rdx, HEAP_BITS_DATA_DISP)?;
    buffer.mov_rm(Reg::Rax, Reg::Rdx, HEAP_BITS_SIZE_DISP)?;
    buffer.xor_rr(Reg::Rcx, Reg::Rcx)?;
    buffer.jmp(tail)?;

    // The refc-window arm (`raw::heap_bits`' `ErlSubBits` + `BinRef` case): the `orig` must
    // really be a `BinRef`, and the window is the context's own `base_flags`/`start`/`end`.
    buffer.bind(refc_arm)?;
    buffer.mov_rm(Reg::Rdi, Reg::Rdx, HEADER_DISP)?;
    buffer.mov_ri(Reg::Rax, BIN_REF_HEADER)?;
    buffer.cmp_rr(Reg::Rdi, Reg::Rax)?;
    buffer.jcc(Cond::Ne, fallback)?;
    buffer.mov_rm(Reg::Rax, ctx, SUB_BITS_BASE_DISP)?;
    buffer.mov_ri(Reg::Rdi, SUB_BITS_BASE_MASK)?;
    buffer.and_rr(Reg::Rax, Reg::Rdi)?;
    // `base + pos / 8` — the byte holding the window's first bit.
    buffer.mov_rr(MOVE_SCRATCH_2, pos)?;
    buffer.shr_ri(MOVE_SCRATCH_2, 3)?;
    buffer.add_rr(MOVE_SCRATCH_2, Reg::Rax)?;
    // `size = end - pos`, with `end < pos` refused rather than wrapped (`saturating_sub`
    // would make such a window empty, and an empty window can never hold the field).
    buffer.mov_rm(Reg::Rax, ctx, SUB_BITS_END_DISP)?;
    buffer.cmp_rr(Reg::Rax, pos)?;
    buffer.jcc(Cond::B, fallback)?;
    buffer.sub_rr(Reg::Rax, pos)?;
    // `off = pos & 7`.
    buffer.mov_rr(Reg::Rcx, pos)?;
    buffer.mov_ri(Reg::Rdi, 7)?;
    buffer.and_rr(Reg::Rcx, Reg::Rdi)?;

    // `num_bytes = (off + size + 7) >> 3` and `bit_pos = pos + off` — the interpreter's own
    // two figures (`raw::subject_bytes` and `pos + off`).
    buffer.bind(tail)?;
    buffer.add_rr(Reg::Rax, Reg::Rcx)?;
    buffer.mov_ri(Reg::Rdi, 7)?;
    buffer.add_rr(Reg::Rax, Reg::Rdi)?;
    buffer.shr_ri(Reg::Rax, 3)?;
    buffer.mov_rr(Reg::Rdx, pos)?;
    buffer.add_rr(Reg::Rdx, Reg::Rcx)?;
    Ok((MOVE_SCRATCH_2, Reg::Rdx, Reg::Rax))
}

// ---------------------------------------------------------------------------
// Shape 8: `bs_match { fail, ctx, commands }` with a fully static command list
// ---------------------------------------------------------------------------

/// One statically-decoded `bs_match` command — the subset whose whole effect is a word
/// compare and (for `skip`/`integer`) one `start`-word commit.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BsMatchCmd {
    /// `ensure_at_least Size Unit` — `commands::ensure_at_least`'s five branches.
    EnsureAtLeast { size: usize, unit: usize },
    /// `ensure_exactly Stride` — the remaining window must be exactly `stride` bits.
    EnsureExactly { stride: usize },
    /// `skip Stride` — advance the position by `stride` bits without binding anything.
    Skip { stride: usize },
    /// `integer Live Flags Size Unit Dst` — extract `nbits` bits and bind `dst`.
    Integer { nbits: usize, dst: Loc },
}

/// The operands of a `bs_match` the tier lowers natively.
#[derive(Clone, Debug)]
pub(crate) struct BsMatchShape {
    /// The match-context operand's location.
    ctx: Loc,
    /// The decoded command list, in program order.
    commands: Vec<BsMatchCmd>,
}

/// The operand count of each `bs_match` command group, the name word included — a local
/// re-derivation of the family's `prepared::group_len` (which is `pub(super)` inside the
/// private `families::bitstrings` subtree), pinned against it by
/// [`tests::the_command_group_lengths_match_the_familys_own`].
///
/// The steps below are the census' too: `emit/test/refusal_census.rs` walks a command list
/// with them to attribute the commands this shape declines.
pub(crate) fn bs_match_group_len(name: &str) -> Option<usize> {
    match name {
        "ensure_at_least" => Some(3),
        "ensure_exactly" => Some(2),
        "skip" => Some(2),
        "integer" => Some(6),
        "binary" => Some(6),
        "get_tail" => Some(4),
        "=:=" => Some(4),
        _ => None,
    }
}

/// The accept rule of a **static** size/unit/stride operand — a local re-derivation of the
/// family's `prepared::static_size`: a non-negative small integer or a character literal.
pub(crate) fn bs_match_static_size(operand: &Operand) -> Option<usize> {
    match operand {
        Operand::Integer(n) if *n >= 0 && beam_term::try_make_small(*n).is_ok() => Some(*n as usize),
        Operand::Character(c) => Some(*c as usize),
        _ => None,
    }
}

/// Decode an `integer` command's flags operand **statically**, or `None` when it holds a flag
/// this lowering does not model (an unknown atom, a non-list) — a re-derivation of the
/// family's `decode_flags`/`flag_bit` over the two operand forms the compiler emits (`Nil`,
/// or a literal holding the flag-atom list). Only the four bits that change the value are
/// named; `integer`/`unsigned`/`big` are the zero default, exactly as `flag_bit` reads them.
pub(crate) fn bs_match_integer_flags(module: &Module, operand: &Operand) -> Option<u32> {
    fn flags_of(module: &Module, elem: Eterm) -> Option<u32> {
        let named = |name: &str| module.atom_table.lookup(name).map(beam_term::make_atom);
        let eq = |name: &str| named(name) == Some(elem);
        if eq("aligned") {
            Some(BSF_ALIGNED)
        } else if eq("little") {
            Some(BSF_LITTLE)
        } else if eq("signed") {
            Some(BSF_SIGNED)
        } else if eq("exact") {
            Some(BSF_EXACT)
        } else if eq("native") {
            Some(BSF_NATIVE)
        } else if eq("integer") || eq("unsigned") || eq("big") {
            Some(0)
        } else {
            // An unmodelled flag atom: the family decides (the safe direction).
            None
        }
    }
    match operand {
        Operand::Nil => Some(0),
        Operand::Literal(index) => {
            let term = module.literal(*index)?;
            if beam_term::is_nil(term) {
                return Some(0);
            }
            let mut flags = 0;
            for elem in crate::terms::list_elements(term).ok()? {
                flags |= flags_of(module, elem)?;
            }
            Some(flags)
        }
        _ => None,
    }
}

impl BsMatchShape {
    /// The [`BsMatchShape`] the instruction's operands name, or `None` when the whole
    /// instruction must stay on the family.
    ///
    /// Accepted: a **fully static** list of `ensure_at_least` / `ensure_exactly` / `skip` /
    /// `integer`, where every size/unit/stride is a constant, every `ensure_at_least` unit is
    /// a power of two (so `%` is a mask), and every `integer` field is unsigned, big-endian
    /// and of a width [`emit_bit_extract`] covers. Everything else declines the **whole**
    /// instruction:
    ///
    /// * `binary` / `get_tail` / `=:=` build a fresh term on every execution
    ///   (`raw::make_binary` reserves unconditionally), so a moving collection could relocate
    ///   the context block in the middle of the list and the block base this lowering reads
    ///   once would go stale;
    /// * a runtime size/unit/stride is a compare the emitted sequence would not have;
    /// * a signed, little/native-endian or wider-than-`{<= 16, 32}` field is the family's
    ///   general reader;
    /// * more than one `Y` destination needs a second live Y address, which this sequence
    ///   does not keep.
    ///
    /// Purely syntactic and **label-free** (the fail classification is the caller's), so the
    /// block's Y gate ([`super::arith::uses_y`]) derives the same answer as the emitter.
    pub(crate) fn of(module: &Module, ctx: &Operand, commands: &[Operand]) -> Option<BsMatchShape> {
        let ctx = Loc::of(ctx)?;
        let mut parsed = Vec::with_capacity(commands.len() / 2 + 1);
        let mut y_destinations = 0usize;
        let mut i = 0usize;
        while i < commands.len() {
            let Operand::Atom(command) = &commands[i] else {
                return None;
            };
            let name = module.atom_name(*command)?;
            let width = bs_match_group_len(name)?;
            let end = i.checked_add(width)?;
            if end > commands.len() {
                // A group running past the end of the list: the family's own typed
                // "truncated command list".
                return None;
            }
            match name {
                "ensure_at_least" => {
                    let size = bs_match_static_size(commands.get(i + 1)?)?;
                    let unit = bs_match_static_size(commands.get(i + 2)?)?;
                    // `remaining % unit` must lower to a mask, and `unit == 0` is a division
                    // by zero — both are compile-time refusals.
                    if !unit.is_power_of_two() {
                        return None;
                    }
                    parsed.push(BsMatchCmd::EnsureAtLeast { size, unit });
                }
                "ensure_exactly" => parsed.push(BsMatchCmd::EnsureExactly {
                    stride: bs_match_static_size(commands.get(i + 1)?)?,
                }),
                "skip" => parsed.push(BsMatchCmd::Skip {
                    stride: bs_match_static_size(commands.get(i + 1)?)?,
                }),
                // `binary` / `get_tail` / `=:=` are decoded only to find their group's
                // length, so the walk stays aligned — never to be lowered. Each one builds a
                // fresh term on every execution (`raw::make_binary` reserves
                // unconditionally), so a moving collection could relocate the context block
                // in the middle of the list. `binary` matters most: its group is the *same
                // width* as `integer`'s, so a fall-through into the `integer` arm below
                // would read it as an integer field and bind an integer where a sub-binary
                // is due — a wrong value, not a refusal.
                "binary" | "get_tail" | "=:=" => return None,
                "integer" => {
                    let flags = bs_match_integer_flags(module, commands.get(i + 2)?)?;
                    let size = bs_match_static_size(commands.get(i + 3)?)?;
                    let unit = bs_match_static_size(commands.get(i + 4)?)?;
                    let nbits = size.checked_mul(unit)?;
                    if !field_width_supported(nbits) {
                        return None;
                    }
                    if field_is_unsupported(flags) {
                        return None;
                    }
                    let dst = Loc::of(commands.get(i + 5)?)?;
                    if dst.is_y() {
                        y_destinations += 1;
                        if y_destinations > 1 {
                            return None;
                        }
                    }
                    parsed.push(BsMatchCmd::Integer { nbits, dst });
                }
                // Unreachable: `bs_match_group_len` above already declined every name that
                // is not one of the six it knows.
                _ => return None,
            }
            i = end;
        }
        Some(BsMatchShape { ctx, commands: parsed })
    }

    /// The x-register indices the shape touches — the context and every `integer` destination.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(1 + self.commands.len());
        registers.extend(self.ctx.register());
        for command in &self.commands {
            if let BsMatchCmd::Integer { dst, .. } = command {
                registers.extend(dst.register());
            }
        }
        registers
    }

    /// The x-register indices the shape **writes** — every `integer` destination. Charging
    /// them is mandatory: a dropped write lets a stale cache register survive an exit.
    pub(crate) fn writes(&self) -> Vec<u32> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                BsMatchCmd::Integer { dst, .. } => dst.register(),
                _ => None,
            })
            .collect()
    }

    /// The x-register indices the shape **reads** — the context only.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.ctx.register().into_iter().collect()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.ctx.is_y()
            || self.commands.iter().any(|command| match command {
                BsMatchCmd::Integer { dst, .. } => dst.is_y(),
                _ => false,
            })
    }
}

/// The inlinable shape of a `bs_match`, or `None` when the whole instruction must stay on the
/// family.
///
/// A `NO_FAIL` fail code **raises** on a short subject (the family's allocating reason), and
/// only a real label is inlined; it must resolve in the module's map because the fail transfer
/// needs its `(function, code)` target — an unresolved one keeps the family's exact
/// `unknown_label`.
pub(crate) fn bs_match_shape(
    labels: &LabelMap,
    module: &Module,
    fail: Label,
    ctx: &Operand,
    commands: &[Operand],
) -> Option<(BsMatchShape, (u32, u32))> {
    let shape = BsMatchShape::of(module, ctx, commands)?;
    if fail == NO_FAIL {
        return None;
    }
    let target = labels.resolve(fail).ok()?;
    Some((shape, target))
}

/// Emit `remaining = end.saturating_sub(pos)` into [`Reg::Rcx`], reading `end` from the
/// context block and `pos` from [`Reg::Rsi`] — `commands::run_commands`' own figure.
fn emit_bs_match_remaining(buffer: &mut CodeBuffer) -> Result<()> {
    let sat: CodeLabel = buffer.label();
    buffer.mov_rm(Reg::Rax, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
    buffer.xor_rr(Reg::Rcx, Reg::Rcx)?;
    buffer.cmp_rr(Reg::Rax, Reg::Rsi)?;
    buffer.jcc(Cond::B, sat)?;
    buffer.mov_rr(Reg::Rcx, Reg::Rax)?;
    buffer.sub_rr(Reg::Rcx, Reg::Rsi)?;
    buffer.bind(sat)?;
    Ok(())
}

/// Emit `ensure_at_least Size Unit`'s test, branch for branch as
/// `commands::ensure_at_least` decides it, jumping to `fail_arm` on a rejection.
fn emit_bs_match_ensure_at_least(
    buffer: &mut CodeBuffer,
    size: usize,
    unit: usize,
    fail_arm: CodeLabel,
) -> Result<()> {
    if size == 0 && unit == 1 {
        // The family's redundant no-op: no test at all.
        return Ok(());
    }
    emit_bs_match_remaining(buffer)?;
    if size == 0 {
        // `remaining % unit == 0` — `unit` is a power of two, so the modulo is a mask.
        buffer.mov_ri(Reg::Rax, (unit - 1) as i64)?;
        buffer.test_rr(Reg::Rcx, Reg::Rax)?;
        return buffer.jcc(Cond::Ne, fail_arm);
    }
    buffer.mov_ri(Reg::Rax, size as i64)?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rax)?;
    buffer.jcc(Cond::B, fail_arm)?;
    if unit != 1 {
        // `(remaining - size) % unit == 0`.
        buffer.sub_rr(Reg::Rcx, Reg::Rax)?;
        buffer.mov_ri(Reg::Rax, (unit - 1) as i64)?;
        buffer.test_rr(Reg::Rcx, Reg::Rax)?;
        buffer.jcc(Cond::Ne, fail_arm)?;
    }
    Ok(())
}

/// Emit one statically-decoded `bs_match` command.
fn emit_bs_match_command(
    buffer: &mut CodeBuffer,
    command: &BsMatchCmd,
    fail_arm: CodeLabel,
    fallback: CodeLabel,
    allocation: &Allocation,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    match command {
        BsMatchCmd::EnsureAtLeast { size, unit } => {
            emit_bs_match_ensure_at_least(buffer, *size, *unit, fail_arm)
        }
        BsMatchCmd::EnsureExactly { stride } => {
            // `end.saturating_sub(pos) != stride` — the remaining window, exact equality.
            emit_bs_match_remaining(buffer)?;
            buffer.mov_ri(Reg::Rax, *stride as i64)?;
            buffer.cmp_rr(Reg::Rcx, Reg::Rax)?;
            buffer.jcc(Cond::Ne, fail_arm)
        }
        BsMatchCmd::Skip { stride } => {
            // `commands::skip_cmd`: a checked advance, then the commit.
            buffer.mov_ri(Reg::Rcx, *stride as i64)?;
            buffer.add_rr(Reg::Rcx, Reg::Rsi)?;
            buffer.jcc(Cond::B, fail_arm)?;
            buffer.mov_rm(Reg::Rax, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
            buffer.cmp_rr(Reg::Rcx, Reg::Rax)?;
            buffer.jcc(Cond::A, fail_arm)?;
            buffer.mov_rr(Reg::Rsi, Reg::Rcx)?;
            buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rsi)
        }
        BsMatchCmd::Integer { nbits, dst } => {
            // `nbits > remaining` -> the FAIL label.
            emit_bs_match_remaining(buffer)?;
            buffer.mov_ri(Reg::Rax, *nbits as i64)?;
            buffer.cmp_rr(Reg::Rcx, Reg::Rax)?;
            buffer.jcc(Cond::B, fail_arm)?;
            // The subject's readable window, then the shared guarded extraction.
            let (base, bit_pos, num_bytes) =
                emit_subject_view(buffer, MOVE_SCRATCH, Reg::Rsi, fallback)?;
            emit_bit_extract(buffer, base, bit_pos, *nbits, num_bytes, Reg::R8, fallback)?;
            // The commit **before** the destination write (`ctx` may alias `dst`).
            buffer.mov_ri(Reg::Rcx, *nbits as i64)?;
            buffer.add_rr(Reg::Rsi, Reg::Rcx)?;
            buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rsi)?;
            // `make_small(raw)` — `(raw << 4) | TAG_IMMED1_SMALL`, zero allocation.
            buffer.shl_ri(Reg::R8, 4)?;
            buffer.mov_ri(Reg::Rcx, SMALL_TAG)?;
            buffer.or_rr(Reg::R8, Reg::Rcx)?;
            dst.store_at(buffer, Y_ADDR_SCRATCH, Reg::R8, allocation, highest_touched)
        }
    }
}

/// Emit `bs_match Fail Ctx {Commands}` natively, or report that the block must helper it
/// (see [`try_inline_bs_get_position`] for the return convention).
///
/// ```text
///   <each Y destination's frame-window guard>   ; before any store
///   <the match-context guard>                   ; not a context -> fallback
///   mov  rsi, [ctx + 16]                        ; pos
///   <one sequence per command, in order>        ; each mutating one commits as it goes
///   jmp  fast_done
/// fail_arm:
///   <the linker's exit to the fail label>
/// fallback:
///   <one interpreter step>
/// ```
///
/// **Position stores go command by command**, never batched: a rejected guard therefore
/// leaves the context at the last successful command, exactly as the family's own
/// command-by-command loop does. Nothing on the accepted path allocates and nothing raises,
/// so no `emit_position_store` is needed and the context block base read by
/// [`emit_ctx_guard`] stays valid for the whole list.
#[allow(clippy::too_many_arguments)]
pub(super) fn try_inline_bs_match(
    buffer: &mut CodeBuffer,
    module: &Module,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: Label,
    ctx: &Operand,
    commands: &[Operand],
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, target)) = bs_match_shape(labels, module, fail, ctx, commands) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_BS_MATCH);

    let fallback: CodeLabel = buffer.label();
    let fail_arm: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // Every `Y` destination's frame-window guard first, so a rejection writes nothing (at
    // most one is accepted, so the single address scratch is enough).
    for command in &shape.commands {
        if let BsMatchCmd::Integer { dst, .. } = command {
            dst.guard_store(buffer, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)?;
        }
    }
    emit_ctx_guard(buffer, shape.ctx, allocation, fallback, highest_touched)?;
    buffer.mov_rm(Reg::Rsi, MOVE_SCRATCH, SUB_BITS_START_DISP)?;
    for command in &shape.commands {
        emit_bs_match_command(
            buffer,
            command,
            fail_arm,
            fallback,
            allocation,
            highest_touched,
        )?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::baseline::bitstr_tests::{export, fixture, with_both_tiers_arg, Arg};
    use crate::jit::baseline::tests::assert_same_outcome;
    use crate::machine::RunOutcome;

    /// The re-declared `BSF_*` values are the frozen C ones (`erts/emulator/beam/erl_bits.h:461-465`),
    /// which is what makes [`field_is_little`] and the shape predicate faithful rather than a
    /// second policy.
    #[test]
    fn the_flag_bits_match_the_frozen_c_constants() {
        assert_eq!(BSF_LITTLE, 2, "BSF_LITTLE");
        assert_eq!(BSF_SIGNED, 4, "BSF_SIGNED");
        assert_eq!(BSF_NATIVE, 16, "BSF_NATIVE");
        // `native` on this target is little-endian, so both bits ask for a byte-swapping read.
        assert!(field_is_little(BSF_LITTLE));
        assert!(field_is_little(BSF_NATIVE));
        assert!(!field_is_little(0));
        assert!(!field_is_little(8)); // BSF_EXACT
    }

    /// The compile-time predicate: only a constant, unsigned, big-endian field of a width the
    /// extractor covers is lowered.
    #[test]
    fn only_a_constant_unsigned_big_endian_width_is_inlined() {
        let x = |i| Operand::X(i);
        let ok = |size: Operand, unit: u32, flags: u32| {
            GetInteger2Shape::of(&x(3), &size, unit, flags, &x(0)).is_some()
        };
        assert!(ok(Operand::Integer(8), 1, 0));
        assert!(ok(Operand::Integer(16), 1, 0));
        assert!(ok(Operand::Integer(16), 2, 0)); // <<X:16/unit:2>> -> 32 bits
        assert!(ok(Operand::Integer(4), 1, 0)); // granular
        assert!(ok(Operand::Integer(2), 4, 0)); // 8 bits via a non-unit width
        assert!(ok(Operand::Integer(0), 1, 0)); // the zero-bit field
        assert!(ok(Operand::Character(8), 1, 0)); // read_size's other accept arm
        // A runtime `size`, a width above 32 that is not 8/16/32, and > MAX_SMALL all refuse.
        assert!(!ok(x(1), 1, 0));
        assert!(!ok(Operand::Integer(24), 1, 0));
        assert!(!ok(Operand::Integer(64), 1, 0));
        assert!(!ok(Operand::Integer(1000), 1, 0));
        // Signed and little/native endianness are the family's general path.
        assert!(!ok(Operand::Integer(8), 1, BSF_SIGNED));
        assert!(!ok(Operand::Integer(8), 1, BSF_LITTLE));
        assert!(!ok(Operand::Integer(8), 1, BSF_NATIVE));
        // ...but `aligned`/`exact` change nothing about the value the family's reader produces.
        assert!(ok(Operand::Integer(8), 1, 1 | 8));
    }

    /// **Tier agreement on a real `erlc`-compiled shape.** `bitstrings:first8/1` is a committed
    /// `bs_start_match3` + `bs_get_integer2` (`<<X:8, _/binary>>`-shaped): the baseline tier answers
    /// the interpreter's own value and its own error, so the emitted arm never fabricates one.
    ///
    /// The **emitted-arm** proof is not asserted here: `inlined()` counts the emitter's decision,
    /// and the cross-run `CodeCache` is content-keyed and process-wide, so a suite that entered this
    /// module on another tier/warmed it first serves the artifact and re-emits nothing (the counter
    /// reads `0 -> 0`). A cache-free pin compiles through `emit::compile_function` — the stage-1
    /// route (`call_tests.rs::the_frame_preserving_local_call_records_no_static_deopt`) and the
    /// follow-up `bitstr_inline_tests` round.
    #[test]
    fn bs_get_integer2_is_inlined_and_agrees_with_the_interpreter() {
        let module = fixture("bitstrings");
        let atom = export(&module, "bitstrings", "first8");
        with_both_tiers_arg(
            &module,
            atom,
            Arg::Bytes(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]),
            |interpreter, baseline| {
                assert_same_outcome("bitstrings:first8/1", interpreter, baseline, &module.atom_table);
                let returned = baseline.as_ref().unwrap_or_else(|error| panic!("first8/1: {error:?}"));
                let RunOutcome::Returned(value) = returned else {
                    panic!("first8/1: unexpected outcome {returned:?}");
                };
                assert_eq!(beam_term::signed_value(*value), 1, "first8/1 read the wrong byte");
            },
        );
    }

    /// **The overrun reaches the fail label.** `first8/1` on an empty subject has `end - start < 8`,
    /// so the emitted `cmp`/`jb` takes the linker exit to the `fail` label and the function's own
    /// failure clause reports exactly what the interpreter reports — no trap, no wrong class.
    #[test]
    fn an_overrun_field_takes_the_fail_label_and_agrees_with_the_interpreter() {
        let module = fixture("bitstrings");
        let atom = export(&module, "bitstrings", "first8");
        with_both_tiers_arg(&module, atom, Arg::Bytes(&[]), |interpreter, baseline| {
            assert_same_outcome(
                "first8/1 on an empty subject",
                interpreter,
                baseline,
                &module.atom_table,
            );
            assert!(baseline.is_err(), "an empty subject cannot satisfy <<X:8>>");
        });
    }
}
