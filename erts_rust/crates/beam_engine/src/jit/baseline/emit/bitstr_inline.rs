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
    Eterm, BITSTRING_TAG_MASK, HEADER_BIN_REF, HEADER_SUB_BITS, HEAP_BITS_SUBTAG, MAX_SMALL,
    TAG_IMMED1_MASK, TAG_IMMED1_SMALL, TAG_PRIMARY_BOXED, TAG_PRIMARY_MASK, TAG_PTR_MASK,
};

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::link::{ExitSpill, Linker};
use super::operands::{Loc, Y_ADDR_SCRATCH};
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
pub(crate) const SHAPE_COUNT: usize = 6;
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
///
/// No caller in the tree yet: the per-shape pins that consume it are the emitter
/// suite's, which is a separate round — [`record_inlined`] already charges every
/// emission, so the counter is live and this is its reader.
#[cfg(test)]
#[allow(dead_code)]
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
pub(crate) fn try_inline_bs_get_position(
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
pub(crate) fn try_inline_bs_set_position(
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
pub(crate) fn try_inline_bs_test_tail2(
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
pub(crate) fn try_inline_bs_skip_bits2(
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
pub(crate) fn try_inline_bs_start_match3(
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
pub(crate) fn try_inline_bs_start_match4(
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
