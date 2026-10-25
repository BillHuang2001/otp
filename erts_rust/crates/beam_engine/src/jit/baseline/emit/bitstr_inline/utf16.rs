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

//! The UTF-16 shapes: `bs_skip_utf16` and its value-binding twin `bs_get_utf16`.
//!
//! A UTF-16 character is **one or two 16-bit big-endian words** (`decode_utf16`,
//! `families::bitstrings::matching::reads.rs`), which is what separates it from the
//! fixed-width [`super::utf32`] pair: a word outside `0xD800..=0xDFFF` is a BMP character
//! and consumes 16 bits on its own, while a **high** surrogate (`0xD800..=0xDBFF`) consumes
//! 16 more by requiring a following **low** surrogate (`0xDC00..=0xDFFF`) and decodes to the
//! astral scalar `((w1 & 0x3FF) << 10 | (w2 & 0x3FF)) + 0x10000`. Everything else is a
//! malformed character.
//!
//! Both opcodes read the same words, carry the same guards and transfer to the same fail
//! label; they differ **only** in whether the decoded scalar is bound. Both therefore share
//! one shape ([`Utf16Shape`], whose `dst` is `None` for the skip form) and one emitted body
//! ([`emit_utf16`], whose [`Utf16Mode`] selects whether the scalar is bound).
//!
//! # The four malformed characters are the FAIL label, never a trap and never a fallback
//!
//! `decode_utf16` answers `None` — the family's `fail_step`, i.e. the resolved fail label
//! with **nothing written** — for each of:
//!
//! * fewer than 16 bits remaining (`end < start + 16`);
//! * a **lone low surrogate** (`w1` in `0xDC00..=0xDFFF`, i.e. a surrogate that is not a
//!   high one);
//! * a high surrogate with fewer than 32 bits remaining;
//! * a high surrogate **not followed by a low one** (`w2` outside `0xDC00..=0xDFFF`).
//!
//! Each is the linker's exit to that label, exactly as `bs_test_tail2`'s and
//! `bs_skip_utf32`'s window rejection is. A `NO_FAIL` fail code *raises* the allocating
//! `{badmatch, Subject}` instead, so it is refused at compile time (as it is for every
//! other fail-carrying shape).
//!
//! # The two reads are pre-load-guarded, and only on the path that needs them
//!
//! Both reads are `movzx` byte loads against the subject's own data region, so each is
//! preceded by a readable-bounds guard — `start / 8 + 2 <= num_bytes` for the first word
//! and `start / 8 + 4 <= num_bytes` for the second — which jumps to `fallback` (one
//! interpreter step, on an untouched context) rather than reading past the buffer. The
//! second guard lives **only** on the surrogate path: a BMP character at the very end of a
//! subject must not be forced to deopt merely because a *pair* would not have fit.
//!
//! # The endianness flags are refused, not reimplemented
//!
//! `decode_utf16` honours `-little`/`-native` (`field_is_little`) and reads each 16-bit word
//! byte-swapped. This arm emits the **big-endian** read only, so a flags word asking for
//! little/native endianness is a compile-time refusal — the interpreter's own reader stays
//! the one implementation of that form, exactly as [`super::utf32`] and the
//! field-extracting shapes' width/sign rules do. Every other flag bit (`BSF_ALIGNED`,
//! `BSF_EXACT`, `BSF_SIGNED`) is irrelevant to what `decode_utf16` produces — it branches on
//! the endianness alone — so those words are lowered.
//!
//! # The mutation order is the whole correctness argument
//!
//! A `fallback` is a **deopt**: `jit_step_one` re-executes the *whole* instruction in the
//! interpreter with the same operand, so any state a partial execution committed is visible
//! to that re-execution. The one mutation here is the context's `start` word, so the emitted
//! order is: **every** guard (the get form's destination frame-window guard, the match-context
//! guard, the remaining-window check, the subject classification, the byte-alignment guard and
//! the two pre-load bounds guards), then the loads and the validity tests (which mutate
//! nothing), then the **single** `start` write, and — get form only — the bind. The fail arm
//! writes nothing at all.
//!
//! Both mutually exclusive successful paths — the BMP one and the surrogate pair one —
//! converge on **one** commit block, so "commits exactly once" is structural rather than an
//! argument about two sequences happening to be exclusive: the BMP path falls through into
//! it with `len = 16` and the surrogate path jumps to it with `len = 32`, and neither can
//! reach the other's arithmetic. The get form's bind lives **inside** that one block, after
//! the commit, so there is still exactly one commit site and one bind site per execution.
//!
//! # The get form's commit precedes the destination store
//!
//! For the same reason `bs_get_integer2`'s does: `beam_asm:filter_essentials/1` is free to
//! compile `ctx == dst`, and the commit may therefore only be reached through the pointer the
//! match-context guard left in [`MOVE_SCRATCH`] — never a re-read of the now-overwritten
//! operand. Both orders reach the same final state, because the commit writes into the
//! context *block* while the destination store writes the *operand slot*, but the ordering is
//! what makes the sequence independent of the aliasing.
//!
//! # No helper, and no position store
//!
//! The whole shape is arithmetic and comparison, so no helper is called — which is why the
//! arm emits **no position store**: the `machine.current_code` publication
//! (`super::super::prologue::emit_position_store`) is what a helper call or an instruction
//! body re-entered from the driver needs, and the only exits here are the in-block
//! `fast_done` continuation, the linker's fail-label exit (`Linker::emit_exit`, which
//! carries the `pc` it stores) and the `fallback`'s own `emit_step_and_continue`, which
//! stores the position itself. Nothing allocates either — the skip form materialises no term
//! at all and the get form's bound value is a bare `make_small` (at most `21` bits), so no
//! moving collection can run part-way through.

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
use super::{record_inlined, SHAPE_GET_UTF16, SHAPE_SKIP_UTF16};
/// The number of bits one **BMP** UTF-16 character occupies — `decode_utf16`'s minimum
/// field width and the first compile-time constant the emitted sequence compares against.
const UTF16_BITS: u32 = 16;

/// [`UTF16_BITS`] in whole bytes: the width of the first word's byte-aligned read, and the
/// `lea` displacement of its over-read guard.
const UTF16_BYTES: i32 = 2;

/// The number of bits a **surrogate pair** occupies — the second compile-time constant, and
/// the figure the second over-read guard's displacement is derived from.
const UTF16_PAIR_BITS: u32 = 32;

/// [`UTF16_PAIR_BITS`] in whole bytes: the `lea` displacement of the pair's over-read guard
/// (bytes 3 and 4 of the field).
const UTF16_PAIR_BYTES: i32 = 4;

/// `0xD800` — the first UTF-16 surrogate code point. A first word below it is a BMP
/// character.
const SURROGATE_BASE: u32 = 0xD800;

/// `0xDFFF` — the last UTF-16 surrogate code point. A first word above it is a BMP
/// character.
const SURROGATE_MAX: u32 = 0xDFFF;

/// `0xDBFF` — the last **high** surrogate. A first word in the surrogate range but above it
/// is a **lone low** surrogate, which `decode_utf16` rejects.
const HIGH_SURROGATE_MAX: u32 = 0xDBFF;

/// `0xDC00` — the first **low** surrogate, i.e. the first value a pair's second word may
/// take.
const LOW_SURROGATE_BASE: u32 = 0xDC00;

/// The low-surrogate range's width: `0xDFFF - 0xDC00`, so `w2 - LOW_SURROGATE_BASE` is
/// `<= LOW_SURROGATE_SPAN` exactly for a low surrogate. One subtract-and-compare rather than
/// two compares, and correct for a `w2` below the range because the unsigned subtraction then
/// wraps to a figure far above the span.
const LOW_SURROGATE_SPAN: u32 = 0x3FF;

/// The 10-bit payload mask both halves of a surrogate pair are reduced with — the pair
/// encoding's field width (`decode_utf16`), numerically the same figure as
/// [`LOW_SURROGATE_SPAN`] but a different fact.
const SURROGATE_PAYLOAD_MASK: u32 = 0x3FF;

/// `0x10000` — the offset a surrogate pair's `(w1 & 0x3FF) << 10 | (w2 & 0x3FF)` is added
/// to, i.e. the first astral plane's base code point (`decode_utf16`).
const ASTRAL_BASE: u32 = 0x1_0000;

// ---------------------------------------------------------------------------
// The shapes: `bs_skip_utf16 { fail, ctx, live, flags }` and
// `bs_get_utf16 { fail, ctx, live, flags, dst }`
// ---------------------------------------------------------------------------

/// The operands of a UTF-16 shape the tier lowers natively.
///
/// One type for both opcodes: the words they read, the guards they carry and the fail label
/// they transfer to are identical, and the only difference is whether the decoded scalar is
/// bound — which is exactly what [`Utf16Shape::dst`] records (`None` for the skip form).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Utf16Shape {
    /// The match-context operand's location.
    ctx: Loc,
    /// The get form's destination, or `None` for the skip form.
    dst: Option<Loc>,
}

impl Utf16Shape {
    /// The [`Utf16Shape`] the instruction's `ctx` names for the **skip** form, or `None` when
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
    pub(crate) fn of(ctx: &Operand, flags: u32) -> Option<Utf16Shape> {
        Self::with_dst(ctx, flags, None)
    }

    /// [`Utf16Shape::of`] for the **get** form, whose [`Utf16Shape::dst`] is `dst`.
    ///
    /// The get form adds one compile-time condition: `dst` must be an in-bound x-register or
    /// an addressable `Y` slot, because the scalar is bound with a plain register/heap store
    /// and no allocation (it is at most `21` bits, so a bare `make_small`). Also label-free,
    /// for the same reason as [`Utf16Shape::of`].
    pub(crate) fn of_get(ctx: &Operand, flags: u32, dst: &Operand) -> Option<Utf16Shape> {
        Self::with_dst(ctx, flags, Some(Loc::of(dst)?))
    }

    /// The one body both constructors share, so the two cannot disagree about the endianness
    /// refusal or the context operand's addressability.
    fn with_dst(ctx: &Operand, flags: u32, dst: Option<Loc>) -> Option<Utf16Shape> {
        if super::integer::field_is_little(flags) {
            return None;
        }
        Some(Utf16Shape {
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
    /// [`Utf16Shape::registers`] for the get form, whose destination is written and never
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

/// The inlinable shape of a `bs_skip_utf16`, or `None` when the whole instruction must stay
/// on the family.
///
/// A `NO_FAIL` fail code *raises* `{badmatch, Subject}` when the character does not decode,
/// and building that reason allocates — so only a real label is inlined. The label must also
/// resolve in the module's map, because the fail transfer needs its `(function, code)`
/// target; an unresolved label keeps the family's exact `unknown_label`.
pub(crate) fn skip_utf16_shape(
    labels: &LabelMap,
    fail: Label,
    ctx: &Operand,
    flags: u32,
) -> Option<(Utf16Shape, (u32, u32))> {
    let shape = Utf16Shape::of(ctx, flags)?;
    utf16_fail_target(labels, fail).map(|target| (shape, target))
}

/// The inlinable shape of a `bs_get_utf16` — [`skip_utf16_shape`]'s twin, with the bound
/// destination and the same fail half (and therefore the same two refusals plus the
/// destination's addressability).
pub(crate) fn get_utf16_shape(
    labels: &LabelMap,
    fail: Label,
    ctx: &Operand,
    flags: u32,
    dst: &Operand,
) -> Option<(Utf16Shape, (u32, u32))> {
    let shape = Utf16Shape::of_get(ctx, flags, dst)?;
    utf16_fail_target(labels, fail).map(|target| (shape, target))
}

/// The `(function, code)` target a fail-carrying UTF-16 shape transfers to, or `None` when
/// the fail operand is one this tier cannot inline.
///
/// The one definition of the fail half every UTF-16 shape shares, so `bs_get_utf16`'s
/// predicate cannot drift from `bs_skip_utf16`'s.
fn utf16_fail_target(labels: &LabelMap, fail: Label) -> Option<(u32, u32)> {
    if fail == NO_FAIL {
        return None;
    }
    labels.resolve(fail).ok()
}

/// Emit `bs_skip_utf16 Fail Ctx Live Flags` natively, or report that the block must helper
/// it (see [`super::try_inline_bs_get_position`] for the return convention).
///
/// The guarded sequence, in this order — every guard first, then the loads and the validity
/// tests, then the one commit (the get form adds its destination's frame-window guard first
/// and the bound-scalar store last; see [`try_inline_bs_get_utf16`]):
///
/// ```text
///   <the match-context guard>            ; not a context -> fallback; r11 = the block base
///   mov  rsi, [ctx + 16]                 ; start
///   mov  r8,  [ctx + 24]                 ; end        (consumed by the remaining test)
///   rcx = end.saturating_sub(start)
///   cmp  rcx, 16 ; jb fail_arm           ; fewer than 16 remaining bits -> the FAIL label
///   <emit_subject_view>                  ; anything but an on-heap ErlHeapBits -> fallback
///                                        ;   r10 = byte base, rax = readable bytes, rsi kept
///   test rsi, 7 ; jne fallback           ; a non-byte-aligned start
///   rdi = start / 8
///   lea  rcx, [rdi + 2] ; cmp rcx, rax ; ja fallback   ; BEFORE the first load
///   add  rdi, r10                        ; the first word's first byte address
///   rcx = the first 16-bit BIG-ENDIAN word
///   cmp  rcx, 0xD800 ; jb bmp            ; below the surrogate range -> a BMP character
///   cmp  rcx, 0xDFFF ; ja bmp            ; above it -> a BMP character
///   ; --- a surrogate: only a HIGH one may continue ---
///   cmp  rcx, 0xDBFF ; ja fail_arm       ; a lone LOW surrogate
///   rdx = end - start ; cmp rdx, 32 ; jb fail_arm       ; no room for the second word
///   rdx = start / 8 ; lea r8, [rdx + 4] ; cmp r8, rax ; ja fallback  ; BEFORE the load
///   r8  = the second 16-bit BIG-ENDIAN word
///   rdx = r8 - 0xDC00 ; cmp rdx, 0x3FF ; ja fail_arm    ; not a LOW surrogate
///   [get only] r8 = ((rcx & 0x3FF) << 10 | (r8 & 0x3FF)) + 0x10000   ; the decoded scalar
///   rcx = 32
///   jmp  commit
/// bmp:
///   [get only] r8 = rcx                   ; a BMP character's scalar IS its word
///   rcx = 16
/// commit:
///   add  rsi, rcx
///   mov  [r11 + 16], rsi                  ; THE COMMIT, after every guard, ONCE
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
/// Both load guards are emitted **before** the load they protect, exactly as
/// [`super::emit_bit_extract`] emits its own: the bytes must lie inside the subject's
/// allocated data region, so each guard compares against [`super::emit_subject_view`]'s
/// **word-padded** byte count. A field that would need a byte past that bound takes
/// `fallback` — never an out-of-range read, never a fabricated value.
///
/// Nothing is written on any path before the commit and nothing allocates, so no moving
/// collection can run part-way through and the context operand is read exactly once, its
/// block address in [`MOVE_SCRATCH`] reused for the commit.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_skip_utf16(
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
    let Some((shape, target)) = skip_utf16_shape(labels, fail, ctx, flags) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_SKIP_UTF16);

    emit_utf16(
        buffer,
        linker,
        shape,
        Utf16Mode::Skip,
        target,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
    Ok(true)
}

/// Emit `bs_get_utf16 Fail Ctx Live Flags Dst` natively — [`try_inline_bs_skip_utf16`]'s
/// value-binding twin.
///
/// One shape predicate differs (the destination's addressability) and one part of the body,
/// which is why both entries funnel into [`emit_utf16`]: the get form emits its destination's
/// frame-window guard **before** anything else, and binds the decoded scalar **after** the
/// context commit. The two orders are not interchangeable in the sequence even though they
/// reach the same state, because `Dst` may **be** `Ctx` (`beam_asm:filter_essentials/1`
/// compiles that): the commit goes through the pointer the match-context guard left in
/// [`MOVE_SCRATCH`], so the context is never re-read from an operand the bind has overwritten.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_get_utf16(
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
    let Some((shape, target)) = get_utf16_shape(labels, fail, ctx, flags, dst) else {
        return Ok(false);
    };
    #[cfg(test)]
    record_inlined(SHAPE_GET_UTF16);

    emit_utf16(
        buffer,
        linker,
        shape,
        Utf16Mode::Get,
        target,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
    Ok(true)
}

/// Whether [`emit_utf16`] is emitting the skip form or the get form.
///
/// The **only** difference between the two bodies: the get form carries a destination, whose
/// frame-window guard is emitted first, whose decoded scalar is materialised in `r8` on each
/// successful path, and whose bind follows the commit.
#[derive(Clone, Copy, Debug)]
enum Utf16Mode {
    /// `bs_skip_utf16`: the character is validated and discarded.
    Skip,
    /// `bs_get_utf16`: the decoded scalar is bound to the shape's destination.
    Get,
}

/// The emitted body both UTF-16 entries share: the whole guarded sequence, the single commit,
/// the (get-only) bind, and the three exits.
///
/// `target` is the already-resolved fail label, `mode` names which of the two opcodes this is
/// and the shape's own operands are already `Loc`s, so this body makes no decision the shape
/// predicates did not — which is what keeps the two entries (and the block's Y gate) in
/// lockstep.
#[allow(clippy::too_many_arguments)]
fn emit_utf16(
    buffer: &mut CodeBuffer,
    linker: &Linker<'_>,
    shape: Utf16Shape,
    mode: Utf16Mode,
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
    let bmp: CodeLabel = buffer.label();
    let commit: CodeLabel = buffer.label();

    // The get form's destination guard first — the guard phase of a lowering that writes a
    // destination is complete before anything can be written, so a rejection leaves the
    // register/heap untouched. It leaves an addressable `Y` slot's address in
    // [`Y_ADDR_SCRATCH`], which the whole sequence below leaves alone (it works in
    // `rsi`/`r8`/`rcx`/`rdx`/`rdi`/`rax`/`r10`/`r11`), so the bind can store through it
    // without a second guard — and, for an x-register destination, it emits nothing.
    if let Utf16Mode::Get = mode {
        let dst = shape
            .dst
            .expect("the get form's shape always carries a destination");
        dst.guard_store(buffer, Y_ADDR_SCRATCH, MOVE_SCRATCH_2, fallback)?;
    }

    emit_ctx_guard(buffer, shape.ctx, allocation, fallback, highest_touched)?;

    // `remaining = end.saturating_sub(start)`, then `remaining < 16` is the FAIL label (the
    // family's `decode_utf16` truncation branch → `fail_step`), never a raise and never a
    // fallback. A `start` past `end` saturates to 0 and so fails too. `end` is kept in `r8`
    // because the *pair* path re-tests it (`remaining < 32`), and a BMP character must not
    // pay for a window it does not need — which is also why the pair test is deliberately
    // not hoisted here.
    buffer.mov_rm(Reg::Rsi, MOVE_SCRATCH, SUB_BITS_START_DISP)?;
    buffer.mov_rm(Reg::R8, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
    buffer.xor_rr(Reg::Rcx, Reg::Rcx)?;
    buffer.cmp_rr(Reg::R8, Reg::Rsi)?;
    buffer.jcc(Cond::B, sat)?;
    buffer.mov_rr(Reg::Rcx, Reg::R8)?;
    buffer.sub_rr(Reg::Rcx, Reg::Rsi)?;
    buffer.bind(sat)?;
    buffer.mov_ri(Reg::Rdx, i64::from(UTF16_BITS))?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::B, fail_arm)?;

    // Classify the inner subject and derive the readable window — one shared helper, so this
    // shape cannot disagree with `bs_get_integer2`/`bs_match`/`bs_skip_utf32` about what a
    // subject is. A literal-area subject, a refc window, a match context, a bare `BinRef` and
    // every non-bitstring take the fallback: the family's own answer, never a fabricated view.
    let (base, _bit_pos, num_bytes) = emit_subject_view(buffer, MOVE_SCRATCH, Reg::Rsi, fallback)?;

    // A byte-aligned `start` is what makes the byte loads below exact: an on-heap subject's
    // own bit offset is 0, so the field's first bit is bit `start % 8` of `base[start / 8]`.
    // A 16-bit word straddling a byte boundary is the family's own bit-by-bit read, one
    // interpreter step — never a shifted reimplementation.
    buffer.mov_ri(Reg::Rcx, 7)?;
    buffer.test_rr(Reg::Rsi, Reg::Rcx)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // The FIRST word's over-read guard, emitted BEFORE the load: `start / 8 + 2 <=
    // num_bytes`, with `num_bytes` the subject's own word-padded data extent. `rdi` becomes
    // the first word's first byte address and stays there for the pair's bytes 3-4.
    buffer.mov_rr(Reg::Rdi, Reg::Rsi)?;
    buffer.shr_ri(Reg::Rdi, 3)?;
    buffer.lea(Reg::Rcx, Reg::Rdi, UTF16_BYTES)?;
    buffer.cmp_rr(Reg::Rcx, num_bytes)?;
    buffer.jcc(Cond::A, fallback)?;
    buffer.add_rr(Reg::Rdi, base)?;

    // The first 16-bit **big-endian** word in `rcx` (`base[j] << 8 | base[j + 1]`): an x86
    // multi-byte load is little-endian, so a plain 2-byte load would produce the
    // byte-reversed word — a wrong result with no error surfaced.
    buffer.movzx_byte(Reg::Rcx, Reg::Rdi, 0)?;
    buffer.shl_ri(Reg::Rcx, 8)?;
    buffer.movzx_byte(Reg::Rdx, Reg::Rdi, 1)?;
    buffer.or_rr(Reg::Rcx, Reg::Rdx)?;

    // `w1` outside `0xD800..=0xDFFF` is a **BMP** character: it consumes 16 bits on its own
    // and no second word exists. Two compares (`< base`, `> max`) rather than a
    // subtract-and-compare, because the pair path below needs `rdx` for its immediates and a
    // wrapped subtraction would need a third register to hold the constant.
    buffer.mov_ri(Reg::Rdx, i64::from(SURROGATE_BASE))?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::B, bmp)?;
    buffer.mov_ri(Reg::Rdx, i64::from(SURROGATE_MAX))?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::A, bmp)?;

    // `w1` is a surrogate. Only a **high** one may start a pair: a first word above
    // `0xDBFF` is a lone LOW surrogate, which `decode_utf16` rejects outright.
    buffer.mov_ri(Reg::Rdx, i64::from(HIGH_SURROGATE_MAX))?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::A, fail_arm)?;

    // The pair needs a second word: `end - start < 32` is the FAIL label, and this is the
    // only place it is tested (a BMP character is never asked for 32 bits). `r8` holds `end`
    // and is dead after this, which is why it serves as the constant's register.
    buffer.mov_rr(Reg::Rdx, Reg::R8)?;
    buffer.sub_rr(Reg::Rdx, Reg::Rsi)?;
    buffer.mov_ri(Reg::R8, i64::from(UTF16_PAIR_BITS))?;
    buffer.cmp_rr(Reg::Rdx, Reg::R8)?;
    buffer.jcc(Cond::B, fail_arm)?;

    // The SECOND word's over-read guard, again BEFORE the load, and again derived from the
    // subject's word-padded extent. `num_bytes` is dead after this, so `r8` carries the
    // bound; `rdx` recomputes `start / 8` from `rsi` (the sequence's read-only start).
    buffer.mov_rr(Reg::Rdx, Reg::Rsi)?;
    buffer.shr_ri(Reg::Rdx, 3)?;
    buffer.lea(Reg::R8, Reg::Rdx, UTF16_PAIR_BYTES)?;
    buffer.cmp_rr(Reg::R8, num_bytes)?;
    buffer.jcc(Cond::A, fallback)?;

    // The second 16-bit big-endian word in `r8`.
    buffer.movzx_byte(Reg::R8, Reg::Rdi, 2)?;
    buffer.shl_ri(Reg::R8, 8)?;
    buffer.movzx_byte(Reg::Rdx, Reg::Rdi, 3)?;
    buffer.or_rr(Reg::R8, Reg::Rdx)?;

    // A second word outside `0xDC00..=0xDFFF` is not a low surrogate, so the pair is
    // malformed: the FAIL label, as one unsigned range test (`rdx` carries the figure, and
    // `rax` — `num_bytes`, dead above — carries the two constants).
    buffer.mov_rr(Reg::Rdx, Reg::R8)?;
    buffer.mov_ri(Reg::Rax, i64::from(LOW_SURROGATE_BASE))?;
    buffer.sub_rr(Reg::Rdx, Reg::Rax)?;
    buffer.mov_ri(Reg::Rax, i64::from(LOW_SURROGATE_SPAN))?;
    buffer.cmp_rr(Reg::Rdx, Reg::Rax)?;
    buffer.jcc(Cond::A, fail_arm)?;

    // The pair's decoded scalar, materialised in `r8` — the register both successful paths
    // leave the code point in — for the get form only. `decode_utf16`'s pair branch is
    // `((w1 & 0x3FF) << 10 | (w2 & 0x3FF)) + 0x10000`: `r8` holds `w2`, `rcx` holds `w1`
    // (`rdx` and `rcx` are dead after this — the length below reloads `rcx`), and the result
    // is at most `0x10FFFF`, so the `make_small` in the commit block cannot overflow the
    // 60-bit immediate. The skip form computes nothing: emitted work no path would consume.
    if let Utf16Mode::Get = mode {
        buffer.mov_ri(Reg::Rdx, i64::from(SURROGATE_PAYLOAD_MASK))?;
        buffer.and_rr(Reg::Rcx, Reg::Rdx)?;
        buffer.and_rr(Reg::R8, Reg::Rdx)?;
        buffer.shl_ri(Reg::Rcx, 10)?;
        buffer.or_rr(Reg::R8, Reg::Rcx)?;
        buffer.mov_ri(Reg::Rcx, i64::from(ASTRAL_BASE))?;
        buffer.add_rr(Reg::R8, Reg::Rcx)?;
    }

    // The pair's length, then the jump into the shared commit.
    buffer.mov_ri(Reg::Rcx, i64::from(UTF16_PAIR_BITS))?;
    buffer.jmp(commit)?;

    // The BMP arm: 16 bits, and this is the only place the two paths differ. For the get form
    // a BMP character's scalar **is** the word just read (`decode_utf16`'s first branch), so
    // it moves into `r8` — the register the pair path left the code point in — before `rcx` is
    // reused for the length.
    buffer.bind(bmp)?;
    if let Utf16Mode::Get = mode {
        buffer.mov_rr(Reg::R8, Reg::Rcx)?;
    }
    buffer.mov_ri(Reg::Rcx, i64::from(UTF16_BITS))?;

    // THE COMMIT — one word store, reached by both mutually exclusive successful paths
    // (BMP falls through, the pair jumps here), after every guard and after the loads and
    // validity tests (which mutate nothing), so a `fallback` anywhere above re-executes the
    // instruction against an untouched context. It goes through the pointer `emit_ctx_guard`
    // left in `MOVE_SCRATCH`, never a re-read of the context operand — which is what makes it
    // sound for the get form's `Dst == Ctx` case.
    buffer.bind(commit)?;
    buffer.add_rr(Reg::Rsi, Reg::Rcx)?;
    buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rsi)?;

    // The get form's bind, **after** the commit: `make_small(raw)` — `(raw << 4) |
    // TAG_IMMED1_SMALL`, zero allocation, the same two instructions `bs_get_integer2` and
    // `bs_get_utf32` emit. Inside this one block, so there is still exactly ONE commit site
    // and one bind site per execution, and the skip form branches around nothing.
    if let Utf16Mode::Get = mode {
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
