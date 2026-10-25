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

//! The UTF-32 shapes: `bs_skip_utf32` and its value-binding twin `bs_get_utf32`.
//!
//! A UTF-32 character is **one 32-bit big-endian field that must be a Unicode scalar**
//! (`decode_utf32`, `families::bitstrings::matching::reads.rs`): read 32 bits, then reject
//! `value > 0x10FFFF` and the surrogate range `0xD800..=0xDFFF`. `emu/ops.tab` rewrites both
//! opcodes into `bs_get_integer2 … 32 1 Flags` plus `i_bs_validate_unicode_retract`, and the
//! skip form is the same read with the value discarded — so the two differ **only** in
//! whether the scalar is bound, never in what they consume or reject. Both therefore share
//! one shape ([`Utf32Shape`], whose `dst` is `None` for the skip form) and one emitted body
//! ([`emit_utf32`], whose [`Utf32Mode`] selects whether the validated scalar is bound).
//!
//! # The endianness flags are refused, not reimplemented
//!
//! `decode_utf32` honours `-little`/`-native` (`field_is_little`) and reads the field
//! byte-swapped. This arm emits the **big-endian** read only, so a flags word asking for
//! little/native endianness is a compile-time refusal — the interpreter's own reader stays
//! the one implementation of that form, exactly as the field-extracting shapes' width and
//! sign rules do (`field_is_unsupported`). Every other flag bit (`BSF_ALIGNED`,
//! `BSF_EXACT`, `BSF_SIGNED`) is irrelevant to the value `decode_utf32` produces — it
//! branches on the endianness alone — so those words are lowered.
//!
//! # The failure is the FAIL label, never a raise and never a fallback
//!
//! Both rejections (`remaining < 32`, an out-of-range or surrogate scalar) are `fail_step`
//! in the family: the resolved fail label, with **nothing written**. So they are the
//! linker's exit to that label, exactly as `bs_test_tail2`'s and `bs_skip_bits2`'s window
//! rejection is. A `NO_FAIL` fail code *raises* the allocating `{badmatch, Subject}`
//! instead, so it is refused at compile time (as it is for every other fail-carrying
//! shape).
//!
//! # The mutation order is the whole correctness argument
//!
//! A `fallback` is a **deopt**: `jit_step_one` re-executes the *whole* instruction in the
//! interpreter with the same operand, so any state a partial execution committed is
//! visible to that re-execution. The one mutation here is the context's `start` word, so
//! the emitted order is: **every** guard (the destination's frame-window guard for the get
//! form, the match-context guard, the remaining-window check, the subject classification,
//! the byte-alignment guard and the readable-bounds guard), then the four byte loads that
//! compose the scalar (which mutate nothing), then the two validity tests (which mutate
//! nothing), then the **single** `start` write. The fail arm writes nothing at all. That is
//! the rule the `bs_match` arm's per-command commit broke once (see
//! `../bs_match_deopt_tests.rs`); here it is trivially satisfiable and both arms' suites pin
//! it with a row that really deopts.
//!
//! The get form's **commit precedes the destination store**, for the same reason
//! `bs_get_integer2`'s does: `beam_asm:filter_essentials/1` compiles `ctx == dst` (the
//! compiler is free to reuse the context's own register for the bound scalar), and the
//! context may therefore only be reached through the pointer the match-context guard left in
//! [`MOVE_SCRATCH`] — never re-read from the now-overwritten operand. Both orders reach the
//! same final state, because the commit writes into the context *block* while the
//! destination store writes the *operand slot*, but the ordering is what makes the sequence
//! independent of the aliasing.
//!
//! # No helper, and no position store
//!
//! The whole shape is arithmetic and comparison, so no helper is called — which is why the
//! arm emits **no position store**: the `machine.current_code` publication
//! (`super::super::prologue::emit_position_store`) is what a helper call or an instruction
//! body re-entered from the driver needs, and the only exits here are the in-block
//! `fast_done` continuation, the linker's fail-label exit (`Linker::emit_exit`, which
//! carries the `pc` it stores) and the `fallback`'s own `emit_step_and_continue`, which
//! stores the position itself. `super::tail`'s two shapes leave the block on exactly the
//! same three edges and likewise emit none. Nothing allocates either — the scalar is at most
//! 21 bits, so the bound value is a bare `make_small` and no moving collection can run
//! part-way through.

use beam_common::Result;
use beam_loader::{Label, Operand, NO_FAIL};

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::link::{ExitSpill, Linker};
use super::super::operands::{Loc, Y_ADDR_SCRATCH};
use super::{
    emit_ctx_guard, emit_step_and_continue, emit_subject_view, Allocation, MOVE_SCRATCH,
    MOVE_SCRATCH_2, SMALL_TAG, SUB_BITS_END_DISP, SUB_BITS_START_DISP,
};

#[cfg(test)]
use super::{record_inlined, SHAPE_GET_UTF32, SHAPE_SKIP_UTF32};

/// The number of bits one UTF-32 character occupies — `decode_utf32`'s fixed field width
/// and the one compile-time constant the emitted sequence compares against.
const UTF32_BITS: u32 = 32;

/// [`UTF32_BITS`] in whole bytes: the width of the field's byte-aligned read, and the
/// `lea` displacement of the over-read guard.
const UTF32_BYTES: i32 = 4;

/// `U+10FFFF` — the largest Unicode scalar. A larger 32-bit field is
/// `i_bs_validate_unicode_retract`'s failure, i.e. the fail label.
const MAX_UNICODE: u32 = 0x10FFFF;

/// `0xD800` — the first UTF-16 surrogate code point. In UTF-32 a scalar in
/// `[SURROGATE_BASE, SURROGATE_BASE + SURROGATE_SPAN]` is the same failure.
const SURROGATE_BASE: u32 = 0xD800;

/// The surrogate range's width: `0xDFFF - 0xD800`, so `value - SURROGATE_BASE` is
/// `<= SURROGATE_SPAN` exactly for a surrogate. One subtract-and-compare rather than two
/// compares, and correct for a `value` below the range because the unsigned subtraction
/// then wraps to a figure far above the span.
const SURROGATE_SPAN: u32 = 0x7FF;

// ---------------------------------------------------------------------------
// The shapes: `bs_skip_utf32 { fail, ctx, live, flags }` and
//             `bs_get_utf32 { fail, ctx, live, flags, dst }`
// ---------------------------------------------------------------------------

/// The operands of a UTF-32 shape the tier lowers natively.
///
/// One type for both opcodes: the field they read, the guards they carry and the fail label
/// they transfer to are identical, and the only difference is whether the validated scalar is
/// bound — which is exactly [`Utf32Shape::dst`] being `Some`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Utf32Shape {
    /// The match-context operand's location.
    ctx: Loc,
    /// The destination the validated scalar is bound to, or `None` for the skip form (which
    /// binds nothing).
    dst: Option<Loc>,
}

impl Utf32Shape {
    /// The [`Utf32Shape`] the instruction's `ctx` names for the **skip** form, or `None` when
    /// the whole instruction must stay on the family.
    ///
    /// Purely syntactic and **label-free** — the fail-label resolution is the caller's — so
    /// the block's Y gate ([`super::super::arith::uses_y`]) derives the same answer as the
    /// emitter without needing a `LabelMap`. The compile-time conditions, each a refusal
    /// rather than an approximation:
    ///
    /// * `ctx` is an in-bound x-register or an addressable `Y` slot ([`Loc::of`]);
    /// * `flags` asks for **big-endian** order — a `-little`/`-native` field is the
    ///   family's byte-swapping reader (see the module docs).
    pub(crate) fn of(ctx: &Operand, flags: u32) -> Option<Utf32Shape> {
        Self::with_dst(ctx, flags, None)
    }

    /// [`Utf32Shape::of`] for the **get** form, whose [`Utf32Shape::dst`] is `dst`.
    ///
    /// The get form adds one compile-time condition: `dst` must be an in-bound x-register or
    /// an addressable `Y` slot, because the scalar is bound with a plain register/heap store
    /// and no allocation (it is at most `21` bits, so a bare `make_small`). Also label-free,
    /// for the same reason as [`Utf32Shape::of`].
    pub(crate) fn of_get(ctx: &Operand, flags: u32, dst: &Operand) -> Option<Utf32Shape> {
        Self::with_dst(ctx, flags, Some(Loc::of(dst)?))
    }

    /// The one body both constructors share, so the two cannot disagree about the endianness
    /// refusal or the context operand's addressability.
    fn with_dst(ctx: &Operand, flags: u32, dst: Option<Loc>) -> Option<Utf32Shape> {
        if super::integer::field_is_little(flags) {
            return None;
        }
        Some(Utf32Shape {
            ctx: Loc::of(ctx)?,
            dst,
        })
    }

    /// The x-register indices the shape touches — the context and, for the get form, the
    /// destination.
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(2);
        registers.extend(self.ctx.register());
        registers.extend(self.dst.and_then(Loc::register));
        registers
    }

    /// The x-register indices the shape **writes**: the get form's destination, and nothing
    /// for the skip form. Charging a bound destination is **mandatory** — the scalar lands in
    /// its cache register, so an uncharged write would let a stale cache register survive a
    /// block exit: a wrong value rather than a redundant store. (The context's `start` word is
    /// a *heap* store, not an x-register write, so it is charged through neither.)
    pub(crate) fn writes(&self) -> Vec<u32> {
        self.dst.and_then(Loc::register).into_iter().collect()
    }

    /// The x-register indices the shape **reads** — the context only. Split from
    /// [`Utf32Shape::registers`] for the get form, whose destination is written and never
    /// read: the prologue's entry-load decision ([`super::super::liveness::fill_sets`]) must
    /// not load a register the instruction only writes.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.ctx.register().into_iter().collect()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.ctx.is_y() || self.dst.is_some_and(Loc::is_y)
    }
}

/// The inlinable shape of a `bs_skip_utf32`, or `None` when the whole instruction must stay
/// on the family.
///
/// A `NO_FAIL` fail code *raises* `{badmatch, Subject}` when the character does not
/// validate, and building that reason allocates — so only a real label is inlined. The
/// label must also resolve in the module's map, because the fail transfer needs its
/// `(function, code)` target; an unresolved label keeps the family's exact `unknown_label`.
pub(crate) fn skip_utf32_shape(
    labels: &LabelMap,
    fail: Label,
    ctx: &Operand,
    flags: u32,
) -> Option<(Utf32Shape, (u32, u32))> {
    let shape = Utf32Shape::of(ctx, flags)?;
    utf32_fail_target(labels, fail).map(|target| (shape, target))
}

/// The inlinable shape of a `bs_get_utf32` — [`skip_utf32_shape`]'s twin, with the bound
/// destination and the same fail half (and therefore the same two refusals plus the
/// destination's addressability).
pub(crate) fn get_utf32_shape(
    labels: &LabelMap,
    fail: Label,
    ctx: &Operand,
    flags: u32,
    dst: &Operand,
) -> Option<(Utf32Shape, (u32, u32))> {
    let shape = Utf32Shape::of_get(ctx, flags, dst)?;
    utf32_fail_target(labels, fail).map(|target| (shape, target))
}

/// The `(function, code)` target a fail-carrying UTF-32 shape transfers to, or `None` when
/// the fail operand is one this tier cannot inline.
///
/// The one definition of the fail half both UTF-32 shapes share, so `bs_get_utf32`'s
/// predicate cannot drift from `bs_skip_utf32`'s.
fn utf32_fail_target(labels: &LabelMap, fail: Label) -> Option<(u32, u32)> {
    if fail == NO_FAIL {
        return None;
    }
    labels.resolve(fail).ok()
}

/// Emit `bs_skip_utf32 Fail Ctx Live Flags` natively, or report that the block must helper
/// it (see [`super::try_inline_bs_get_position`] for the return convention).
///
/// The guarded sequence, in this order — every guard first, then the loads, then the one
/// commit (the get form adds its destination's frame-window guard first and the bound-scalar
/// store last; see [`try_inline_bs_get_utf32`]):
///
/// ```text
///   <the match-context guard>            ; not a context -> fallback; r11 = the block base
///   mov  rsi, [ctx + 16]                 ; start
///   mov  rax, [ctx + 24]                 ; end
///   rcx = end.saturating_sub(start)
///   cmp  rcx, 32 ; jb fail_arm           ; fewer than 32 remaining bits -> the FAIL label
///   <emit_subject_view>                  ; anything but an on-heap ErlHeapBits -> fallback
///                                        ;   r10 = byte base, rax = readable bytes, rsi kept
///   test rsi, 7 ; jne fallback           ; a non-byte-aligned start
///   rdi = start / 8
///   lea  rcx, [rdi + 4] ; cmp rcx, num_bytes ; ja fallback    ; BEFORE the first load
///   add  rdi, r10                         ; the field's first byte's address
///   r8 = the 32-bit BIG-ENDIAN value      ; four movzx bytes, each earlier one shifted up
///   cmp  r8, 0x10FFFF ; ja fail_arm       ; not a Unicode scalar
///   rcx = r8 - 0xD800 ; cmp rcx, 0x7FF ; jbe fail_arm     ; a surrogate code point
///   mov  rcx, 32 ; add rsi, rcx
///   mov  [r11 + 16], rsi                  ; THE COMMIT, after every guard
///   [get only] r8 = (r8 << 4) | SMALL_TAG ; make_small, zero allocation
///   [get only] mov  <Dst>, r8             ; the bind, AFTER the commit
///   jmp  fast_done
/// fail_arm:
///   <the linker's exit to the fail label>     ; a terminator on that path only
/// fallback:
///   <one interpreter step>                    ; the family's own answer, on an untouched context
/// fast_done:
/// ```
///
/// The readable-bounds guard is emitted **before** the first load, exactly as
/// [`super::emit_bit_extract`] emits its own: the four bytes must lie inside the subject's
/// allocated data region, so the guard is `start / 8 + 4 <= num_bytes` against
/// [`super::emit_subject_view`]'s **word-padded** byte count. A field that would need a byte
/// past that bound takes `fallback` — never an out-of-range read, never a fabricated value.
///
/// Nothing allocates (the value is at most 21 bits and the sequence materialises no heap term
/// at all), so no moving collection can run part-way through and the context operand is read
/// exactly once, its block address in [`MOVE_SCRATCH`] reused for the commit.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_skip_utf32(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: Label,
    ctx: &Operand,
    flags: u32,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, target)) = skip_utf32_shape(labels, fail, ctx, flags) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_SKIP_UTF32);

    emit_utf32(
        buffer,
        linker,
        shape,
        Utf32Mode::Skip,
        target,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
    Ok(true)
}

/// Emit `bs_get_utf32 Fail Ctx Live Flags Dst` natively — [`try_inline_bs_skip_utf32`]'s
/// value-binding twin.
///
/// One shape predicate differs (the destination's addressability) and one part of the body,
/// which is why both entries funnel into [`emit_utf32`]: the get form emits its destination's
/// frame-window guard **before** anything else, and binds the validated scalar **after** the
/// context commit. The two orders are not interchangeable in the sequence even though they
/// reach the same state, because `Dst` may **be** `Ctx` (`beam_asm:filter_essentials/1`
/// compiles that): the commit goes through the pointer the match-context guard left in
/// [`MOVE_SCRATCH`], so the context is never re-read from an operand the bind has overwritten.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_get_utf32(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: Label,
    ctx: &Operand,
    flags: u32,
    dst: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, target)) = get_utf32_shape(labels, fail, ctx, flags, dst) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_GET_UTF32);

    emit_utf32(
        buffer,
        linker,
        shape,
        Utf32Mode::Get,
        target,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
    Ok(true)
}

/// Whether [`emit_utf32`] is emitting the skip form or the get form.
///
/// The **only** difference between the two bodies: the get form carries a destination, whose
/// frame-window guard is emitted first and whose bind follows the commit.
#[derive(Clone, Copy, Debug)]
enum Utf32Mode {
    /// `bs_skip_utf32`: the scalar is validated and discarded.
    Skip,
    /// `bs_get_utf32`: the validated scalar is bound to the shape's destination.
    Get,
}

/// The emitted body both UTF-32 entries share: the whole guarded sequence, the single commit,
/// the (get-only) bind, and the three exits.
///
/// `target` is the already-resolved fail label, `mode` names which of the two opcodes this is
/// and the shape's own operands are already `Loc`s, so this body makes no decision the shape
/// predicates did not — which is what keeps the two entries (and the block's Y gate) in
/// lockstep.
#[allow(clippy::too_many_arguments)]
fn emit_utf32(
    buffer: &mut CodeBuffer,
    linker: &Linker<'_>,
    shape: Utf32Shape,
    mode: Utf32Mode,
    target: (u32, u32),
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let fallback: CodeLabel = buffer.label();
    let fail_arm: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();
    let sat: CodeLabel = buffer.label();

    // The get form's destination guard first — the guard phase of a lowering that writes a
    // destination is complete before anything can be written, so a rejection leaves the
    // register/heap untouched. It leaves an addressable `Y` slot's address in
    // [`Y_ADDR_SCRATCH`], which the whole sequence below leaves alone (it works in
    // `rsi`/`rax`/`rcx`/`rdx`/`rdi`/`r8`/`r10`/`r11`), so the bind can store through it
    // without a second guard.
    if let Utf32Mode::Get = mode {
        let dst = shape
            .dst
            .expect("the get form's shape always carries a destination");
        dst.guard_store(buffer, Y_ADDR_SCRATCH, MOVE_SCRATCH_2, fallback)?;
    }

    emit_ctx_guard(buffer, shape.ctx, allocation, fallback, highest_touched)?;

    // `remaining = end.saturating_sub(start)`, then `remaining < 32` is the FAIL label (the
    // family's `decode_utf32` truncation branch → `fail_step`), never a raise and never a
    // fallback. A `start` past `end` saturates to 0 and so fails too, exactly as
    // `end < start + 32` does for the family.
    buffer.mov_rm(Reg::Rsi, MOVE_SCRATCH, SUB_BITS_START_DISP)?;
    buffer.mov_rm(Reg::Rax, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
    buffer.xor_rr(Reg::Rcx, Reg::Rcx)?;
    buffer.cmp_rr(Reg::Rax, Reg::Rsi)?;
    buffer.jcc(Cond::B, sat)?;
    buffer.mov_rr(Reg::Rcx, Reg::Rax)?;
    buffer.sub_rr(Reg::Rcx, Reg::Rsi)?;
    buffer.bind(sat)?;
    buffer.mov_ri(MOVE_SCRATCH_2, i64::from(UTF32_BITS))?;
    buffer.cmp_rr(Reg::Rcx, MOVE_SCRATCH_2)?;
    buffer.jcc(Cond::B, fail_arm)?;

    // Classify the inner subject and derive the readable window — one shared helper, so this
    // shape cannot disagree with `bs_get_integer2`/`bs_match` about what a subject is. A
    // literal-area subject, a refc window, a match context, a bare `BinRef` and every
    // non-bitstring take the fallback: the family's own answer, never a fabricated view.
    let (base, _bit_pos, num_bytes) = emit_subject_view(buffer, MOVE_SCRATCH, Reg::Rsi, fallback)?;

    // A byte-aligned `start` is what makes the four byte loads below exact: an on-heap
    // subject's own bit offset is 0, so the field's first bit is bit `start % 8` of
    // `base[start / 8]`. Anything else is the family's own bit-by-bit read, one interpreter
    // step — never a shifted reimplementation.
    buffer.mov_ri(Reg::Rcx, 7)?;
    buffer.test_rr(Reg::Rsi, Reg::Rcx)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // The over-read guard, emitted BEFORE the first load: `start / 8 + 4 <= num_bytes`,
    // with `num_bytes` the subject's own word-padded data extent.
    buffer.mov_rr(Reg::Rdi, Reg::Rsi)?;
    buffer.shr_ri(Reg::Rdi, 3)?;
    buffer.lea(Reg::Rcx, Reg::Rdi, UTF32_BYTES)?;
    buffer.cmp_rr(Reg::Rcx, num_bytes)?;
    buffer.jcc(Cond::A, fallback)?;
    buffer.add_rr(Reg::Rdi, base)?;

    // The 32-bit **big-endian** value, assembled most-significant byte first in `r8`
    // (one `movzx` per byte) — an x86 multi-byte load is little-endian, so a plain 4-byte
    // load would produce the byte-reversed scalar: a wrong result with no error surfaced.
    buffer.movzx_byte(Reg::R8, Reg::Rdi, 0)?;
    for byte in 1..UTF32_BYTES {
        buffer.shl_ri(Reg::R8, 8)?;
        buffer.movzx_byte(Reg::Rcx, Reg::Rdi, byte)?;
        buffer.or_rr(Reg::R8, Reg::Rcx)?;
    }

    // `value > 0x10FFFF` — not a Unicode scalar.
    buffer.mov_ri(Reg::Rcx, i64::from(MAX_UNICODE))?;
    buffer.cmp_rr(Reg::R8, Reg::Rcx)?;
    buffer.jcc(Cond::A, fail_arm)?;

    // A surrogate code point, as one unsigned range test. `value` is now known to be
    // `<= 0x10FFFF`, so the subtraction below can only wrap for a `value` **below** the
    // surrogate range, and a wrapped figure is far above the span — i.e. "not a surrogate",
    // which is the answer that case needs.
    buffer.mov_rr(Reg::Rcx, Reg::R8)?;
    buffer.mov_ri(Reg::Rdx, i64::from(SURROGATE_BASE))?;
    buffer.sub_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.mov_ri(Reg::Rdx, i64::from(SURROGATE_SPAN))?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::Be, fail_arm)?;

    // The commit: one word store, after every guard and after the loads and validity tests
    // (which mutate nothing), so a `fallback` anywhere above re-executes the instruction
    // against an untouched context. It goes through the pointer `emit_ctx_guard` left in
    // `MOVE_SCRATCH`, never a re-read of the context operand — which is what makes it sound
    // for the get form's `Dst == Ctx` case.
    buffer.mov_ri(Reg::Rcx, i64::from(UTF32_BITS))?;
    buffer.add_rr(Reg::Rsi, Reg::Rcx)?;
    buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rsi)?;

    // The get form's bind, **after** the commit: `make_small(raw)` — `(raw << 4) |
    // TAG_IMMED1_SMALL`, zero allocation, the same two instructions `bs_get_integer2` emits.
    // The value is at most `0x10FFFF`, so the shift cannot overflow the 60-bit immediate.
    if let Utf32Mode::Get = mode {
        let dst = shape
            .dst
            .expect("the get form's shape always carries a destination");
        buffer.shl_ri(Reg::R8, 4)?;
        buffer.mov_ri(Reg::Rcx, SMALL_TAG)?;
        buffer.or_rr(Reg::R8, Reg::Rcx)?;
        dst.store_at(buffer, Y_ADDR_SCRATCH, Reg::R8, allocation, highest_touched)?;
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
    Ok(())
}
