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

//! The `bs_skip_utf8` shape.
//!
//! A UTF-8 character is **one to four bytes**, and the lead byte says how many
//! (`decode_utf8`, `families::bitstrings::matching::reads.rs`): `0x00..=0x7F` is ASCII
//! and stands alone, `0xC0..=0xDF` announces one continuation byte, `0xE0..=0xEF` two and
//! `0xF0..=0xF4` three, while `0x80..=0xBF` (a continuation byte) and `0xF5..=0xFF` can
//! never lead a sequence at all. So unlike the fixed-width [`super::utf32`] and the
//! two-width [`super::utf16`], the character's **length is decided by the first byte** and
//! every later decision — the window check, the readable-bounds guard and the commit —
//! depends on it.
//!
//! # The shape is four static paths and one dynamic `total`
//!
//! The emitted sequence reads the lead byte once, classifies it into one of four
//! **static** paths (0/1/2/3 continuation bytes) with a compare chain, and each path then
//! runs its own straight-line body: its own window check, its own bounds guard, its own
//! fixed count of continuation reads with **static** shift amounts and masks, and its own
//! validity tests. The one dynamic figure — the character's bit length — is materialised
//! into `rcx` at the end of the path taken, so it drives both the window check comparisons
//! and the commit without a second classification.
//!
//! # The ten malformed characters are the FAIL label, never a trap and never a fallback
//!
//! `decode_utf8` answers `None` — the family's `fail_step`, i.e. the resolved fail label
//! with **nothing written** — for each of:
//!
//! * fewer than 8 bits remaining (the lead byte is truncated);
//! * a **continuation byte as the lead** (`0x80..=0xBF`) or a byte above `0xF4`;
//! * a sequence shorter than its lead byte's announcement (a truncated 2/3/4-byte form);
//! * a **non-continuation byte** where one is required (`b & 0xC0 != 0x80`);
//! * an **overlong** form: a 2/3/4-byte encoding below `0x80`/`0x800`/`0x10000`
//!   (`0xC0 0x80`, `0xE0 0x80 0x80`, `0xF0 0x80 0x80 0x80`);
//! * a **surrogate** code point (`0xD800..=0xDFFF`, e.g. `0xED 0xA0 0x80`);
//! * a value above `0x10FFFF` (`0xF4 0x90 0x80 0x80`).
//!
//! Each is the linker's exit to that label, exactly as [`super::utf16`]'s four are. A
//! `NO_FAIL` fail code *raises* the allocating `{badmatch, Subject}` instead, so it is
//! refused at compile time (as it is for every other fail-carrying shape).
//!
//! # The flags word is accepted, because the family ignores it
//!
//! `decode_utf8` takes **no** flags and both UTF-8 family arms destructure `flags: _`
//! (`matching/mod.rs`), because a UTF-8 sequence has no endianness to select. There is
//! therefore nothing for this arm to refuse: every flags word the instruction carries
//! is lowered, and the interpreter's reader stays the one implementation of the decode
//! itself. (This is the one place the UTF-8 pair differs from the [`super::utf16`] and
//! [`super::utf32`] pairs, which refuse `-little`/`-native` because *their* readers
//! branch on it.)
//!
//! # The reads are pre-load-guarded, and only where they are needed
//!
//! Every read is a `movzx` byte load against the subject's own data region, so each is
//! preceded by a readable-bounds guard — `start / 8 + 1 <= num_bytes` for the lead byte
//! and `start / 8 + W <= num_bytes` for the `W`-byte form — which jumps to `fallback`
//! (one interpreter step, on an untouched context) rather than reading past the buffer.
//! The lead byte's guard is unconditional (every path reads it); a wider form's guard
//! lives **only** on the path that reads those bytes, so an ASCII character at the very
//! end of a subject is not forced to deopt because a four-byte sequence would not fit.
//!
//! # The mutation order is the whole correctness argument
//!
//! A `fallback` is a **deopt**: `jit_step_one` re-executes the *whole* instruction in the
//! interpreter with the same operand, so any state a partial execution committed is
//! visible to that re-execution. The one mutation here is the context's `start` word, so
//! the emitted order is: **every** guard (the match-context guard, the remaining-window
//! check, the subject classification, the byte-alignment guard, the lead byte's
//! bounds guard and the taken path's own window and bounds guards), then the loads and
//! the validity tests (which mutate nothing), then the **single** `start` write. The fail
//! arm writes nothing at all.
//!
//! All four paths converge on **one** commit block, so "commits exactly once" is
//! structural rather than an argument about four sequences happening to be exclusive:
//! each path materialises its own bit length into `rcx` and jumps there, and no path can
//! reach another's arithmetic. Because the commit goes through the block pointer the
//! match-context guard left in [`MOVE_SCRATCH`], it is already sound for a
//! `bs_get_utf8` whose destination **is** the context operand.
//!
//! # No helper, and no position store
//!
//! The whole shape is arithmetic and comparison, so no helper is called — which is why the
//! arm emits **no position store**: the `machine.current_code` publication
//! (`super::super::prologue::emit_position_store`) is what a helper call or an instruction
//! body re-entered from the driver needs, and the only exits here are the in-block
//! `fast_done` continuation, the linker's fail-label exit (`Linker::emit_exit`, which
//! carries the `pc` it stores) and the `fallback`'s own `emit_step_and_continue`, which
//! stores the position itself. Nothing allocates either — the shape materialises no term
//! at all — so no moving collection can run part-way through.

use beam_common::Result;
use beam_loader::{Label, Operand, NO_FAIL};

use crate::interpreter::LabelMap;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::link::{ExitSpill, Linker};
use super::super::operands::Loc;
use super::{
    emit_ctx_guard, emit_step_and_continue, emit_subject_view, Allocation, MOVE_SCRATCH,
    SUB_BITS_END_DISP, SUB_BITS_START_DISP,
};

/// The number of bits a UTF-8 character's **lead** byte occupies — `decode_utf8`'s
/// `end < start + 8` truncation test, and the width of the one read every path makes.
const UTF8_LEAD_BITS: u32 = 8;

/// [`UTF8_LEAD_BITS`] in whole bytes: the `lea` displacement of the lead byte's
/// over-read guard.
const UTF8_LEAD_BYTES: i32 = 1;

/// The bit length of a one-byte (ASCII) character — one lead byte and no continuation, so
/// [`UTF8_LEAD_BITS`] again, named for the fact it states.
const ASCII_BITS: u32 = UTF8_LEAD_BITS;

/// The last lead byte that stands alone (`0x00..=0x7F`).
const ASCII_LEAD_MAX: u32 = 0x7F;

/// The last byte that can **never** lead a sequence (`0x80..=0xBF` is a continuation
/// byte, so a sequence cannot start there).
const CONTINUATION_LEAD_MAX: u32 = 0xBF;

/// The last lead byte announcing **one** continuation byte (`0xC0..=0xDF`).
const TWO_BYTE_LEAD_MAX: u32 = 0xDF;

/// The last lead byte announcing **two** continuation bytes (`0xE0..=0xEF`).
const THREE_BYTE_LEAD_MAX: u32 = 0xEF;

/// The last lead byte announcing **three** continuation bytes (`0xF0..=0xF4`); above it
/// the encoding would exceed `U+10FFFF` whatever follows.
const FOUR_BYTE_LEAD_MAX: u32 = 0xF4;

/// The bits the lead byte's tag occupies in a continuation byte — the mask
/// `decode_utf8` tests `b & 0xC0 != 0x80` with.
const CONTINUATION_TAG_MASK: u32 = 0xC0;

/// The value [`CONTINUATION_TAG_MASK`] must select in a continuation byte.
const CONTINUATION_TAG: u32 = 0x80;

/// The six payload bits a continuation byte carries.
const CONTINUATION_PAYLOAD_MASK: u32 = 0x3F;

/// `0xD800` — the first UTF-16 surrogate code point, which UTF-8 may not encode.
const SURROGATE_BASE: u32 = 0xD800;

/// The surrogate range's width: `0xDFFF - 0xD800`, so `result - SURROGATE_BASE` is
/// `<= SURROGATE_SPAN` exactly for a surrogate. One subtract-and-compare rather than two
/// compares, and correct for a `result` below the range because the unsigned subtraction
/// then wraps to a figure far above the span.
const SURROGATE_SPAN: u32 = 0x7FF;

/// `0x10FFFF` — the last Unicode scalar value, the ceiling `decode_utf8` rejects above.
const UNICODE_MAX: u32 = 0x10_FFFF;

// ---------------------------------------------------------------------------
// The shape: `bs_skip_utf8 { fail, ctx, live, flags }`
// ---------------------------------------------------------------------------

/// The operands of a UTF-8 shape the tier lowers natively.
///
/// Only the match-context operand: the skip form binds nothing, so there is no second
/// operand to record, and `flags` is not part of the shape at all (the family ignores it
/// — see the module docs).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Utf8Shape {
    /// The match-context operand's location.
    ctx: Loc,
}

impl Utf8Shape {
    /// The [`Utf8Shape`] the instruction's `ctx` names, or `None` when the whole
    /// instruction must stay on the family.
    ///
    /// Purely syntactic and **label-free** — the fail-label resolution is the caller's — so
    /// the block's Y gate ([`super::super::arith::uses_y`]) derives the same answer as the
    /// emitter without needing a `LabelMap`. The one compile-time condition is a refusal
    /// rather than an approximation: `ctx` is an in-bound x-register or an addressable `Y`
    /// slot ([`Loc::of`]).
    pub(crate) fn of(ctx: &Operand) -> Option<Utf8Shape> {
        Some(Utf8Shape { ctx: Loc::of(ctx)? })
    }

    /// The x-register indices the shape touches — the context only.
    pub(crate) fn registers(&self) -> Vec<u32> {
        self.ctx.register().into_iter().collect()
    }

    /// The x-register indices the shape **writes**: none. Its only mutation is the
    /// context's `start` **heap** word, not an x-register store, so a block that lowers it
    /// leaves `machine.x`'s cached registers untouched. (Charged anyway — an empty set —
    /// so the four regalloc mirrors stay one decision.)
    pub(crate) fn writes(&self) -> Vec<u32> {
        Vec::new()
    }

    /// The x-register indices the shape **reads** — the context only.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.ctx.register().into_iter().collect()
    }

    /// Whether the lowering reads the current frame's Y window.
    pub(crate) fn uses_y(&self) -> bool {
        self.ctx.is_y()
    }
}

/// The inlinable shape of a `bs_skip_utf8`, or `None` when the whole instruction must stay
/// on the family.
///
/// A `NO_FAIL` fail code *raises* `{badmatch, Subject}` when the character does not decode,
/// and building that reason allocates — so only a real label is inlined. The label must also
/// resolve in the module's map, because the fail transfer needs its `(function, code)`
/// target; an unresolved label keeps the family's exact `unknown_label`.
pub(crate) fn skip_utf8_shape(
    labels: &LabelMap,
    fail: Label,
    ctx: &Operand,
) -> Option<(Utf8Shape, (u32, u32))> {
    let shape = Utf8Shape::of(ctx)?;
    utf8_fail_target(labels, fail).map(|target| (shape, target))
}

/// The `(function, code)` target a fail-carrying UTF-8 shape transfers to, or `None` when
/// the fail operand is one this tier cannot inline.
///
/// The one definition of the fail half, so the eventual `bs_get_utf8` predicate cannot
/// drift from `bs_skip_utf8`'s.
fn utf8_fail_target(labels: &LabelMap, fail: Label) -> Option<(u32, u32)> {
    if fail == NO_FAIL {
        return None;
    }
    labels.resolve(fail).ok()
}

/// Emit `bs_skip_utf8 Fail Ctx Live Flags` natively, or report that the block must helper
/// it (see [`super::try_inline_bs_get_position`] for the return convention).
///
/// The guarded sequence, in this order — every guard first, then the loads and the validity
/// tests, then the one commit:
///
/// ```text
///   <the match-context guard>            ; not a context -> fallback; r11 = the block base
///   mov  rsi, [ctx + 16]                 ; start
///   mov  r8,  [ctx + 24]                 ; end
///   rcx = end.saturating_sub(start)
///   cmp  rcx, 8 ; jb fail_arm            ; fewer than 8 remaining bits -> the FAIL label
///   <emit_subject_view>                  ; anything but an on-heap ErlHeapBits -> fallback
///                                        ;   r10 = byte base, rax = readable bytes, rsi kept
///   test rsi, 7 ; jne fallback           ; a non-byte-aligned start
///   rdi = start / 8
///   lea  rcx, [rdi + 1] ; cmp rcx, rax ; ja fallback   ; BEFORE the lead-byte load
///   add  rdi, r10                        ; the lead byte's address
///   add  rax, r10                        ; the readable bound, as an address
///   movzx rdx, byte [rdi]                ; the lead byte
///   ; --- classify it into one of four static paths ---
///   cmp rdx, 0x7F ; jbe ascii
///   cmp rdx, 0xBF ; jbe fail_arm         ; a continuation byte as the lead
///   cmp rdx, 0xDF ; jbe two
///   cmp rdx, 0xEF ; jbe three
///   cmp rdx, 0xF4 ; jbe four
///   jmp fail_arm                          ; above 0xF4
/// ascii:                                  ; total = 8: its window check IS the `cmp rcx, 8` above
///   r8 = (rdx & 0x7F)                    ; the decoded scalar (the get form's bind value)
///   rcx = 8 ; jmp commit
/// two / three / four:                     ; total = 16 / 24 / 32, W = 2 / 3 / 4
///   rcx = end - start ; r8 = total ; cmp rcx, r8 ; jb fail_arm       ; the window check
///   lea  rcx, [rdi + W] ; cmp rcx, rax ; ja fallback                 ; BEFORE the loads
///   r8 = (rdx & mask) << 6K                                          ; the lead's payload
///   ; K continuation bytes, each: b & 0xC0 == 0x80 else fail_arm,
///   ;                            then r8 |= (b & 0x3F) << 6(K - k)
///   cmp  r8, min ; jb fail_arm                                       ; an overlong form
///   rcx = r8 - 0xD800 ; cmp rcx, 0x7FF ; jbe fail_arm                ; a surrogate
///   cmp  r8, 0x10FFFF ; ja fail_arm                                  ; above Unicode
///   rcx = total ; jmp commit
/// commit:
///   add  rsi, rcx
///   mov  [r11 + 16], rsi                 ; THE COMMIT, after every guard, reached ONCE
///   jmp  fast_done
/// fail_arm:
///   <the linker's exit to the fail label>     ; a terminator on that path only
/// fallback:
///   <one interpreter step>                    ; the family's own answer, on an untouched context
/// fast_done:
/// ```
///
/// The lead byte's bounds guard is emitted **before** the load it protects, exactly as
/// [`super::emit_bit_extract`] emits its own: the byte must lie inside the subject's
/// allocated data region, so the guard compares against [`super::emit_subject_view`]'s
/// **word-padded** byte count. A form that would need a byte past that bound takes
/// `fallback` — never an out-of-range read, never a fabricated value.
///
/// Nothing is written on any path before the commit and nothing allocates, so no moving
/// collection can run part-way through and the context operand is read exactly once, its
/// block address in [`MOVE_SCRATCH`] reused for the commit.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_skip_utf8(
    buffer: &mut CodeBuffer,
    labels: &LabelMap,
    linker: &Linker<'_>,
    fail: Label,
    ctx: &Operand,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some((shape, target)) = skip_utf8_shape(labels, fail, ctx) else {
        return Ok(false);
    };

    emit_utf8(
        buffer,
        linker,
        shape,
        target,
        allocation,
        done,
        func,
        pc,
        highest_touched,
    )?;
    Ok(true)
}

/// One of [`emit_utf8`]'s four static paths: everything about the character that the lead
/// byte's classification decides, as compile-time constants.
///
/// The four are the only shapes `decode_utf8` accepts, so a path is not a specialisation —
/// it is the whole table, and each body is emitted exactly once.
#[derive(Clone, Copy, Debug)]
struct Utf8Path {
    /// The label this path's body is bound to.
    label: CodeLabel,
    /// How many continuation bytes the lead byte announces (`0..=3`) — the number of
    /// reads, and the shift the lead byte's own payload is placed at.
    continuations: usize,
    /// The character's total bit length (`(continuations + 1) * 8`), the figure the commit
    /// advances `start` by.
    total_bits: u32,
    /// [`Utf8Path::total_bits`] in whole bytes — the `lea` displacement of the path's
    /// over-read guard.
    total_bytes: i32,
    /// The mask the lead byte's payload is kept with (`0x7F`/`0x1F`/`0x0F`/`0x07`).
    payload_mask: u32,
    /// The smallest value this length may encode (`0x80`/`0x800`/`0x10000`; `0` for ASCII,
    /// which has no overlong form). A result below it is an overlong encoding.
    minimum: u32,
}

/// The emitted body: the classification, the four static paths, the single commit and the
/// three exits.
///
/// `target` is the already-resolved fail label and the shape's own operand is already a
/// [`Loc`], so this body makes no decision the shape predicate did not — which is what keeps
/// the predicate (and the block's Y gate) in lockstep with what is emitted.
#[allow(clippy::too_many_arguments)]
fn emit_utf8(
    buffer: &mut CodeBuffer,
    linker: &Linker<'_>,
    shape: Utf8Shape,
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
    let commit: CodeLabel = buffer.label();

    emit_ctx_guard(buffer, shape.ctx, allocation, fallback, highest_touched)?;

    // `remaining = end.saturating_sub(start)`, then `remaining < 8` is the FAIL label (the
    // family's `decode_utf8` truncation branch → `fail_step`), never a raise and never a
    // fallback. A `start` past `end` saturates to 0 and so fails too. This is also the
    // ASCII path's window check: its `total` is exactly these 8 bits, so the comparison is
    // emitted once here rather than again inside that path.
    buffer.mov_rm(Reg::Rsi, MOVE_SCRATCH, SUB_BITS_START_DISP)?;
    buffer.mov_rm(Reg::R8, MOVE_SCRATCH, SUB_BITS_END_DISP)?;
    buffer.xor_rr(Reg::Rcx, Reg::Rcx)?;
    buffer.cmp_rr(Reg::R8, Reg::Rsi)?;
    buffer.jcc(Cond::B, sat)?;
    buffer.mov_rr(Reg::Rcx, Reg::R8)?;
    buffer.sub_rr(Reg::Rcx, Reg::Rsi)?;
    buffer.bind(sat)?;
    buffer.mov_ri(Reg::Rdx, i64::from(UTF8_LEAD_BITS))?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::B, fail_arm)?;

    // Classify the inner subject and derive the readable window — one shared helper, so this
    // shape cannot disagree with `bs_get_integer2`/`bs_match`/`bs_skip_utf16` about what a
    // subject is. A literal-area subject, a refc window, a match context, a bare `BinRef` and
    // every non-bitstring take the fallback: the family's own answer, never a fabricated view.
    let (base, _bit_pos, num_bytes) = emit_subject_view(buffer, MOVE_SCRATCH, Reg::Rsi, fallback)?;

    // A byte-aligned `start` is what makes the byte loads below exact: an on-heap subject's
    // own bit offset is 0, so the lead byte's first bit is bit `start % 8` of
    // `base[start / 8]`. A character straddling a byte boundary is the family's own
    // bit-by-bit read, one interpreter step — never a shifted reimplementation.
    buffer.mov_ri(Reg::Rcx, 7)?;
    buffer.test_rr(Reg::Rsi, Reg::Rcx)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // The LEAD byte's over-read guard, emitted BEFORE the load: `start / 8 + 1 <=
    // num_bytes`, with `num_bytes` the subject's own word-padded data extent. `rdi` becomes
    // the lead byte's address and stays there for every continuation byte, and `rax` becomes
    // the read limit **as an address** — so a wider path's guard is one `lea` + `cmp` with no
    // recomputation of `start / 8`.
    buffer.mov_rr(Reg::Rdi, Reg::Rsi)?;
    buffer.shr_ri(Reg::Rdi, 3)?;
    buffer.lea(Reg::Rcx, Reg::Rdi, UTF8_LEAD_BYTES)?;
    buffer.cmp_rr(Reg::Rcx, num_bytes)?;
    buffer.jcc(Cond::A, fallback)?;
    buffer.add_rr(Reg::Rdi, base)?;
    // `num_bytes` is dead after the lead guard, so the register it lives in becomes the read
    // limit **as an address** — one `add`, after which a wider path's guard is a single `lea`
    // plus a compare with no recomputation of `start / 8`.
    let limit = num_bytes;
    buffer.add_rr(limit, base)?;

    // The lead byte, in `rdx` — a load only, so nothing has been mutated yet. `r8` still
    // holds `end` (each path's window check consumes it) and `rcx` serves as the compare
    // constant.
    buffer.movzx_byte(Reg::Rdx, Reg::Rdi, 0)?;

    let paths = [
        Utf8Path {
            label: buffer.label(),
            continuations: 0,
            total_bits: ASCII_BITS,
            total_bytes: 1,
            payload_mask: 0x7F,
            minimum: 0,
        },
        Utf8Path {
            label: buffer.label(),
            continuations: 1,
            total_bits: 2 * UTF8_LEAD_BITS,
            total_bytes: 2,
            payload_mask: 0x1F,
            minimum: 0x80,
        },
        Utf8Path {
            label: buffer.label(),
            continuations: 2,
            total_bits: 3 * UTF8_LEAD_BITS,
            total_bytes: 3,
            payload_mask: 0x0F,
            minimum: 0x800,
        },
        Utf8Path {
            label: buffer.label(),
            continuations: 3,
            total_bits: 4 * UTF8_LEAD_BITS,
            total_bytes: 4,
            payload_mask: 0x07,
            minimum: 0x1_0000,
        },
    ];

    // The classification — `trailing_bytes_for_utf8`'s five bands, branch for branch, with
    // the two invalid ones (a continuation byte as the lead, and anything above `0xF4`)
    // taking the FAIL label.
    for (bound, target_path) in [
        (ASCII_LEAD_MAX, paths[0].label),
        (CONTINUATION_LEAD_MAX, fail_arm),
        (TWO_BYTE_LEAD_MAX, paths[1].label),
        (THREE_BYTE_LEAD_MAX, paths[2].label),
        (FOUR_BYTE_LEAD_MAX, paths[3].label),
    ] {
        buffer.mov_ri(Reg::Rcx, i64::from(bound))?;
        buffer.cmp_rr(Reg::Rdx, Reg::Rcx)?;
        buffer.jcc(Cond::Be, target_path)?;
    }
    buffer.jmp(fail_arm)?;

    for path in paths {
        emit_utf8_path(buffer, path, limit, fail_arm, fallback, commit)?;
    }

    // THE COMMIT — one word store, reached by all four mutually exclusive successful paths,
    // after every guard and after the loads and validity tests (which mutate nothing), so a
    // `fallback` anywhere above re-executes the instruction against an untouched context. It
    // goes through the pointer `emit_ctx_guard` left in `MOVE_SCRATCH`, never a re-read of
    // the context operand — which is what makes it sound for a `bs_get_utf8`'s `Dst == Ctx`
    // case, whose bind will follow it.
    buffer.bind(commit)?;
    buffer.add_rr(Reg::Rsi, Reg::Rcx)?;
    buffer.mov_mr(MOVE_SCRATCH, SUB_BITS_START_DISP, Reg::Rsi)?;
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

/// Emit one static path: its window check, its over-read guard, its continuation reads and
/// its validity tests, ending in the shared commit.
///
/// `rsi` (start), `rdi` (the lead byte's address), `rdx` (the lead byte's value), `limit`
/// (the read limit, as an address) and `r8` (`end`) are read here; `r8` receives the decoded
/// scalar, `rcx` the character's bit length, and `r10` is used as the continuation byte's
/// temporary (the subject's byte base — [`super::MOVE_SCRATCH_2`] — is dead once `limit`
/// carries the absolute bound). Nothing is written to the context — that is [`emit_utf8`]'s
/// single commit — so a `fallback` taken anywhere in here leaves the instruction un-applied.
fn emit_utf8_path(
    buffer: &mut CodeBuffer,
    path: Utf8Path,
    limit: Reg,
    fail_arm: CodeLabel,
    fallback: CodeLabel,
    commit: CodeLabel,
) -> Result<()> {
    buffer.bind(path.label)?;

    // The ASCII path's window check is the top-level `remaining < 8` test, which is exactly
    // `remaining >= total` here, so this path does none of the work below.
    if path.continuations > 0 {
        // (a) the character's own window: `end - start >= total` is the family's `end - start
        // < total_bits` rejection. `r8` carries `end` and is dead after this, which is why it
        // becomes the accumulator below.
        buffer.mov_rr(Reg::Rcx, Reg::R8)?;
        buffer.sub_rr(Reg::Rcx, Reg::Rsi)?;
        buffer.mov_ri(Reg::R8, i64::from(path.total_bits))?;
        buffer.cmp_rr(Reg::Rcx, Reg::R8)?;
        buffer.jcc(Cond::B, fail_arm)?;

        // (b) the continuation bytes' over-read guard, BEFORE any of them is read: the same
        // `j + W <= num_bytes` test as the lead byte's, against the absolute limit in `limit`.
        buffer.lea(Reg::Rcx, Reg::Rdi, path.total_bytes)?;
        buffer.cmp_rr(Reg::Rcx, limit)?;
        buffer.jcc(Cond::A, fallback)?;
    }

    // (c) the lead byte's own payload, placed at its final position: `(a & mask) << 6K`.
    buffer.mov_rr(Reg::R8, Reg::Rdx)?;
    buffer.mov_ri(Reg::Rcx, i64::from(path.payload_mask))?;
    buffer.and_rr(Reg::R8, Reg::Rcx)?;
    if path.continuations > 0 {
        buffer.shl_ri(Reg::R8, 6 * path.continuations as u32)?;
    }

    // (d) the continuation bytes: each must carry the `10` tag, and each contributes six
    // bits at a **static** position (`6 * (K - k)`), so the sequence has no variable shift at
    // all. A non-continuation byte is the FAIL label — the family's own `None`.
    for k in 1..=path.continuations {
        buffer.movzx_byte(Reg::R10, Reg::Rdi, k as i32)?;
        buffer.mov_rr(Reg::Rcx, Reg::R10)?;
        buffer.mov_ri(Reg::Rdx, i64::from(CONTINUATION_TAG_MASK))?;
        buffer.and_rr(Reg::Rcx, Reg::Rdx)?;
        buffer.mov_ri(Reg::Rdx, i64::from(CONTINUATION_TAG))?;
        buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
        buffer.jcc(Cond::Ne, fail_arm)?;
        buffer.mov_ri(Reg::Rdx, i64::from(CONTINUATION_PAYLOAD_MASK))?;
        buffer.and_rr(Reg::R10, Reg::Rdx)?;
        let shift = 6 * (path.continuations - k) as u32;
        if shift > 0 {
            buffer.shl_ri(Reg::R10, shift)?;
        }
        buffer.or_rr(Reg::R8, Reg::R10)?;
    }

    // (e) the three scalar-validity tests a multi-byte form needs. An ASCII character is at
    // most `0x7F`, so it has no overlong form, is never a surrogate and is never above
    // `U+10FFFF` — none of them is emitted for it.
    if path.continuations > 0 {
        // An overlong encoding: below the smallest value this length may represent.
        buffer.mov_ri(Reg::Rdx, i64::from(path.minimum))?;
        buffer.cmp_rr(Reg::R8, Reg::Rdx)?;
        buffer.jcc(Cond::B, fail_arm)?;
        // A surrogate code point, as one unsigned range test.
        buffer.mov_rr(Reg::Rcx, Reg::R8)?;
        buffer.mov_ri(Reg::Rdx, i64::from(SURROGATE_BASE))?;
        buffer.sub_rr(Reg::Rcx, Reg::Rdx)?;
        buffer.mov_ri(Reg::Rdx, i64::from(SURROGATE_SPAN))?;
        buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
        buffer.jcc(Cond::Be, fail_arm)?;
        // Beyond Unicode.
        buffer.mov_ri(Reg::Rdx, i64::from(UNICODE_MAX))?;
        buffer.cmp_rr(Reg::R8, Reg::Rdx)?;
        buffer.jcc(Cond::A, fail_arm)?;
    }

    // (f) the character's length, then the jump into the one shared commit.
    buffer.mov_ri(Reg::Rcx, i64::from(path.total_bits))?;
    buffer.jmp(commit)?;
    Ok(())
}
