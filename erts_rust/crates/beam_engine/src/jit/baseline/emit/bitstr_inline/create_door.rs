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

//! The **aligned** route's append door: the accumulator's term shape proved **inline**, and
//! only the machine-private capacity lookup left as a call.
//!
//! [`emit_append_cursor`] replaces the whole-door helper the aligned route used to call with
//! exactly this sequence — 25 instructions / **155 B** for a cached x-register accumulator,
//! which [`emit_append_cursor`]'s own byte-length pin records:
//!
//! ```text
//!   mov  rax, <accumulator>         ; cache register, or [rbx + x], or the Y slot  (no store)
//!   test rax, 1                     ; is_boxed              off fallback   (test_ri imm32)
//!   jne  fallback
//!   test rax, 3                     ; not a bare header     off fallback   (test_ri imm32)
//!   je   fallback
//!   and  rax, -8                    ; boxed_val, IN PLACE                  (and_ri imm32)
//!   mov  rcx, [rax + HEADER]
//!   cmp  rcx, HEADER_SUB_BITS       ; not an ErlSubBits     off fallback   (cmp_ri imm32)
//!   jne  fallback
//!   mov  rcx, [rax + START]
//!   test rcx, rcx                   ; start != 0            off fallback
//!   jne  fallback
//!   mov  rsi, [rax + BASE]
//!   and  rsi, !FLAGS                ; the payload pointer                  (and_ri imm32)
//!   test rsi, rsi                   ; null base             off fallback
//!   je   fallback
//!   mov  rdx, [rax + END]
//!   test rdx, 7                     ; end % 8 != 0          off fallback   (test_ri imm32)
//!   jne  fallback
//!   mov  rdi, [rsp + RUNTIME_SLOT]  ; rdi = rt             (the ABI's first argument)
//!   movabs rcx, tail_bits           ; rcx = the plan's tail width
//!   movabs r11, jit_append_cursor
//!   call r11                                                               ; the one call
//!   test rax, rax                   ; the call's refusal    off fallback   ; the door's own
//!   je   fallback
//! ```
//!
//! Every mask and constant guard is a **register-immediate** form (`imm32`), so no `movabs`
//! appears in the guard group; the two `movabs` left are the tail width and the callee's
//! address. `rax` answers the **byte cursor** `base + end / 8` on success and `0` on every
//! refusal, which is exactly what the aligned field phase stores its bytes at.
//!
//! The door emits **no position store**: the block prologue already published this block's
//! entry, a valid resume point whose re-execution re-runs the block prefix, and the call
//! cannot raise, park, pre-empt, allocate or collect (see `## Nothing here publishes a
//! position`).
//!
//! The door also emits **no pre-call spill**: the accumulator is read through [`Loc::load_x`]
//! (a cached x-register comes out of its callee-saved machine register) or [`emit_y_slot_value`]
//! (a `Y` slot), and nothing here needs `machine.x` or `machine.stack` to be written back.
//!
//! The boxed pointer is untagged **in place** (`and rax, -8`) rather than in a second
//! register, and the accumulator's **term word is dead from that point on**: the post-call
//! `test rax, rax` reads the **call's** answer, and the phases after the door re-read the
//! accumulator from its x-register or `Y` slot — once, in phase (5), which keeps that word in
//! `rdx` across the window-end commit and the destination store. No path after the `and` can
//! observe the untagged term word.
//!
//! # Why the proof is here and not in the helper
//!
//! The shape proof is pure term reading — five frozen words of the accumulator's own block,
//! whose layout `beam_term` owns — so it needs no machine state and no allocation. Leaving
//! it to the helper costs a call's `rt` reload, argument marshalling and prologue/epilogue
//! on every accumulator open; the only part that genuinely needs the machine is
//! [`Machine::writable_capacity`](crate::machine::Machine::writable_capacity)'s registry
//! lookup, and that stays in
//! [`jit_append_cursor`](crate::jit::baseline::runtime::jit_append_cursor).
//!
//! The whole sequence reproduces `seam::open_append` condition for condition:
//! `raw::writable_append_target`'s
//! [`is_refc_binary`](beam_term::refc::is_refc_binary) + `start == 0` + non-null base
//! clauses are the guards below, its registry clause and the room rule are the helper's,
//! and the helper's `unit == 8` append-position gate is the `end % 8 == 0` guard. Every
//! guard is therefore a **refusal only**: what it cannot prove takes one interpreter step
//! (the family's own answer), never a wrong result.
//!
//! [`is_refc_binary`](beam_term::refc::is_refc_binary)'s **`is_bin_ref(orig)`** clause is the
//! one condition with no guard of its own here, because the helper's registry gate subsumes
//! it for every accumulator this runtime can produce; section (3) below carries the argument.
//!
//! # Every guard is a refusal and nothing here writes
//!
//! Each test above branches to `fallback` and **stores nothing**, so the whole sequence is
//! re-executable: `fallback` is one interpreter step re-running the instruction with the
//! same operands, and it must see an accumulator the block's prefix has not touched. The one
//! mutation the build performs — the `ErlSubBits.end` advance — is emitted last, by
//! [`super::super::create`]'s phase 5, after every field byte store; the destination store
//! that follows it writes a register or a stack slot, never memory the `fallback` can read as
//! payload or window state.
//!
//! The door's own `rsp` frame is untouched (it reads `rt` out of the block's `rt` slot), and
//! the `fallback` arm's own **spill** — `emit_step_and_continue` → `emit_helper_call`, which
//! writes every dirty cached x-register back into `machine.x` before the step — is what keeps
//! the deopt path sound: `jit_step_one` reads `machine.x`, so the block must not be the only
//! holder of an x-register the interpreter step is about to observe. This door deliberately
//! does **not** spill, because it also never leaves a cached x-register's authoritative value
//! only in its machine register: `Loc::load_x` reads it without writing anything, and the
//! call's answer is consumed from `rax` after the return.
//!
//! # Nothing here publishes a position
//!
//! The sequence emits **no** `Machine::current_code` store, so between the block prologue's
//! publication and the next instruction's the published position names the **block entry**,
//! not this call. That is sound because nothing in that window can observe a wrong position:
//!
//! 1. `jit_append_cursor` cannot raise, park, pre-empt, allocate or collect — it is a
//!    read-only `&Machine` query (see its own docs), so no suspension, stacktrace or
//!    pre-emption address is ever taken against this instruction;
//! 2. the `fallback` arm publishes through `jit_step_one`'s own `(func, pc)` arguments, i.e.
//!    the interpreter step stores this instruction's position itself before it runs the
//!    instruction, exactly as the interpreter would;
//! 3. a block exit or an in-block native transfer republishes — the driver's loop head
//!    re-decodes `(func, pc)`, and `commit_in_block_entry` stores the target;
//! 4. the block prologue already published a valid resume point (the block entry), and a
//!    resume from it re-executes the block prefix, which re-derives everything this door reads
//!    from the machine's own state;
//! 5. no tier-side count (reduction, spill, `Y`-traffic) is charged here, so no counter that
//!    a position store would bracket can disagree.
//!
//! # Registers
//!
//! Only caller-saved scratch: `rax` (the term, untagged in place, then the door's answer),
//! `rcx` (the header / `start` compare scratch, then the ABI's **fourth** argument — the
//! plan's `tail_bits`), `rdx` (the window end, the ABI's **third** argument), `rsi` (the
//! payload base, the ABI's **second** argument), `rdi` (the runtime, the ABI's first) and
//! `r11` (the callee). `rbx` (`machine.x`'s base), `rsp` and every cache register are
//! untouched, so `emit/regalloc.rs` needs no change; `rsi`/`rdx`/`rcx` are argument
//! **registers**, i.e. nothing is kept live across the call.
//!
//! # The frozen layouts it reads
//!
//! `ErlSubBits` is `[thing_word, base_flags, start, end, orig]`
//! (`erts/emulator/beam/erl_bits.h:60-82`); an off-heap payload's first data byte is
//! `base_flags & !ERL_SUB_BITS_FLAG_MASK` (the low two bits are the writable/volatile flags),
//! `start` is the window's bit offset and `end` its bit size. `BinRef` — the one shape a full
//! refc binary's `orig` names — is `[HEADER_BIN_REF, val, next]` and its header is the whole
//! test. Every constant below comes from `beam_term`'s own items or from the layout module
//! two levels up; none is a magic number.

use beam_common::Result;
use beam_term::{ERL_SUB_BITS_FLAG_MASK, SUB_BITS_BASE_WORD, TAG_PRIMARY_HEADER, TAG_PRIMARY_MASK};

use crate::jit::baseline::runtime::jit_append_cursor;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::emit_y_slot_value;
use super::super::{
    HEADER_DISP, NOT_BOXED_MASK, POINTER_MASK, SUB_BITS_END_DISP, SUB_BITS_HEADER,
    SUB_BITS_START_DISP,
};
use super::super::super::operands::Loc;
use super::super::super::{Allocation, RUNTIME_SLOT_DISP};

/// `_TAG_PRIMARY_MASK` — the two-bit primary tag (`beam_term::tag`).
///
/// `beam_term::boxed::is_boxed_term` is `is_boxed(x) && is_term_word(x)`, and
/// `is_term_word` is exactly "the primary tag is not `TAG_PRIMARY_HEADER`". `is_boxed`
/// admits only `TAG_PRIMARY_HEADER` and `TAG_PRIMARY_BOXED`, so a `test`+`je` of this mask
/// is `is_header` **because `TAG_PRIMARY_HEADER` is `0`** — the assertion below keeps that
/// identity from rotting into a wrong branch.
const PRIMARY_MASK: i64 = TAG_PRIMARY_MASK as i64;

const _: () = assert!(
    TAG_PRIMARY_HEADER == 0,
    "`test PRIMARY_MASK`/`je` is `is_header` only for a zero HEADER tag"
);

/// `!ERL_SUB_BITS_FLAG_MASK` — the mask that turns an `ErlSubBits`'s `base_flags` word into
/// the payload pointer (`beam_term::ERL_SUB_BITS_FLAG_MASK` is `0x3`).
const SUB_BITS_POINTER_MASK: i64 = !(ERL_SUB_BITS_FLAG_MASK as i64);

// Every mask and constant guard below is used through one of [`CodeBuffer`]'s
// **register-immediate** encoders (`test_ri`/`cmp_ri`/`and_ri`/`add_ri`), which take the
// constant as a sign-extended `imm32`. The five twins below are that form, each pinned
// against the `i64` item it narrows, so a mask that outgrew an `imm32` would fail the build
// rather than silently drop its high bits. (`POINTER_MASK_I32` is shared with
// [`super::super::create`]'s window-end commit, which reuses it rather than duplicating it.)

/// [`NOT_BOXED_MASK`] as the `imm32` of [`CodeBuffer::test_ri`].
const NOT_BOXED_MASK_I32: i32 = NOT_BOXED_MASK as i32;
const _: () = assert!(NOT_BOXED_MASK == NOT_BOXED_MASK_I32 as i64);
/// [`PRIMARY_MASK`] as the `imm32` of [`CodeBuffer::test_ri`].
const PRIMARY_MASK_I32: i32 = PRIMARY_MASK as i32;
const _: () = assert!(PRIMARY_MASK == PRIMARY_MASK_I32 as i64);
/// [`POINTER_MASK`] as the `imm32` of [`CodeBuffer::and_ri`].
///
/// Shared with [`super::create`]'s commit/destination phase, which masks a *second* loaded
/// accumulator in place: one constant, one narrowing assertion, two sites.
pub(super) const POINTER_MASK_I32: i32 = POINTER_MASK as i32;
const _: () = assert!(POINTER_MASK == POINTER_MASK_I32 as i64);
/// [`SUB_BITS_POINTER_MASK`] as the `imm32` of [`CodeBuffer::and_ri`].
const SUB_BITS_POINTER_MASK_I32: i32 = SUB_BITS_POINTER_MASK as i32;
const _: () = assert!(SUB_BITS_POINTER_MASK == SUB_BITS_POINTER_MASK_I32 as i64);
/// [`SUB_BITS_HEADER`] as the `imm32` of [`CodeBuffer::cmp_ri`].
const SUB_BITS_HEADER_I32: i32 = SUB_BITS_HEADER as i32;
const _: () = assert!(SUB_BITS_HEADER == SUB_BITS_HEADER_I32 as i64);

/// The byte offset of an `ErlSubBits`'s `base_flags` word (word 1) — the payload pointer
/// plus its low flag bits (`erts/emulator/beam/erl_bits.h:60-82`).
///
/// Declared here rather than imported because the layout module two levels up carries the
/// `start`/`end`/`orig` displacements but not this one; it is the same
/// `beam_term::SUB_BITS_BASE_WORD` product those are (`[`SUB_BITS_START_DISP`]` is word 2).
const SUB_BITS_BASE_DISP: i32 = (SUB_BITS_BASE_WORD * core::mem::size_of::<beam_term::Eterm>()) as i32;

/// The divisor of a whole-byte window end: `end % 8 == 0` is C's append **position** gate for
/// `unit == 8` (`append_unit_allows`, `erl_bits.c:1778-1784`), and the aligned route's byte
/// cursor needs it (`beam_term::bitstring::bit_count_to_bytes`'s rounding denominator).
const BITS_PER_BYTE: i32 = 8;

/// Emit the aligned append door for the accumulator at `source`, appending `tail_bits` bits.
///
/// Leaves `rax` = the **byte cursor** the field stores write at (and falls through), or jumps
/// to `fallback` with nothing written. The `test rax, rax ; je fallback` pair is this
/// function's own last two instructions, so the caller must not emit a second one.
///
/// Takes no `(func, pc)`: the door publishes **no** position (see the module docs), so it has
/// nothing to name and the block prologue's own publication stands.
///
/// See the module docs for the proof sequence, the register contract and the frozen layouts.
pub(super) fn emit_append_cursor(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    source: Loc,
    tail_bits: u32,
    fallback: CodeLabel,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    // (1) The accumulator term. A `Y` slot needs no second frame-window guard: the shape's
    // phase (1b) already proved the slot inside the published window, and nothing between
    // there and here (a compare chain) can move the frame.
    //
    // An x-register accumulator is read through [`Loc::load_x`], so a cached one comes out of
    // its **callee-saved** machine register rather than requiring a pre-call spill of
    // `machine.x`. Nothing between this load and the helper call writes that register, and
    // the helper (`jit_append_cursor`) only reads machine state — it cannot allocate, collect,
    // park, raise or pre-empt — so no representation of the term can go stale.
    match source {
        Loc::X(_) => source.load_x(buffer, allocation, Reg::Rax, highest_touched)?,
        Loc::Y(k) => emit_y_slot_value(buffer, Reg::Rax, k)?,
    }

    // (2) `beam_term::refc::is_refc_binary`'s first clause — `is_boxed_term(t)`, i.e. the
    // `is_boxed` bit trick and then "not a bare header word" (which `is_boxed` alone admits,
    // and which would otherwise be dereferenced as a pointer). Both are register-immediate
    // tests, so each mask costs one instruction and no `movabs`.
    buffer.test_ri(Reg::Rax, NOT_BOXED_MASK_I32)?;
    buffer.jcc(Cond::Ne, fallback)?;
    buffer.test_ri(Reg::Rax, PRIMARY_MASK_I32)?;
    buffer.jcc(Cond::E, fallback)?;
    // `boxed_val(t)` — the accumulator's header word address, untagged **in place**. The term
    // word is dead here (see the module docs: every later read of the accumulator re-loads it),
    // so no second register is spent on the pointer.
    buffer.and_ri(Reg::Rax, POINTER_MASK_I32)?;
    // Its header must say `ErlSubBits`: a `BinRef`, a tuple, an `ErlHeapBits` — every other
    // boxed term — is not a refc binary.
    buffer.mov_rm(Reg::Rcx, Reg::Rax, HEADER_DISP)?;
    buffer.cmp_ri(Reg::Rcx, SUB_BITS_HEADER_I32)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // (3) `orig` (word 4) is deliberately **not** tested, and the sequence is therefore one
    // guard group shorter than `raw::writable_append_target`'s clause list. The omitted
    // clause is `is_refc_binary`'s `is_bin_ref(orig)`, and step (7)'s registry gate subsumes
    // it for every accumulator this runtime can produce:
    //
    // * the registry's only writer is `raw::build_writable_bitstring` (the `bs_init_writable`
    //   family, `Machine::mark_writable`), which builds a **0-bit refc binary** — an
    //   `ErlSubBits` over a `BinRef` — and registers *that* payload's first data byte; so an
    //   accepted `base` implies `orig` is a `BinRef`, and no term this test would reject can
    //   reach the registry gate and be accepted;
    // * an `ErlSubBits` that is really a **match context** carries `base_flags == 0`
    //   (`raw::make_ctx` writes the zero word; `set_ctx_start`/`set_ctx_end` only touch words
    //   2 and 3), so step (5) refuses it on the null base;
    // * an `ErlSubBits` windowing an **on-heap** binary carries a process-heap pointer in
    //   `base_flags`, and no heap block's first word is a registry key: the keys are the
    //   `RefcBinary::data_ptr()` of payloads this machine allocated, pruned in lockstep with
    //   `retain_refc_binaries` — so a live key can only name a live payload, whose first byte
    //   is a different address from any live heap block's;
    // * nothing else reads `orig`, so dropping the test removes no dereference-safety guard.
    //
    // A term the registry refuses takes the same one interpreter step the `orig` test took, so
    // the observable surface is unchanged; `bs_create_bin_tests` pins both refused forms (a
    // match context used as the accumulator, and an `ErlSubBits` window over an on-heap
    // binary) at exactly one interpreter step per tier.

    // (4) `start` (word 2) must be `0`: C asserts it for both append forms
    // (`erl_bits.c:1943-1948`), because an in-place append writes at absolute payload offsets.
    buffer.mov_rm(Reg::Rcx, Reg::Rax, SUB_BITS_START_DISP)?;
    buffer.test_rr(Reg::Rcx, Reg::Rcx)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // (5) The payload base (word 1 without its flag bits) — non-null, and the helper's second
    // argument. It is the key the machine's writable-payload registry is asked about.
    buffer.mov_rm(Reg::Rsi, Reg::Rax, SUB_BITS_BASE_DISP)?;
    buffer.and_ri(Reg::Rsi, SUB_BITS_POINTER_MASK_I32)?;
    buffer.test_rr(Reg::Rsi, Reg::Rsi)?;
    buffer.jcc(Cond::E, fallback)?;

    // (6) The window end (word 3) — the helper's third argument — which must be on a byte
    // boundary for this route's cursor (`start == 0`, so the word is the window's bit size).
    buffer.mov_rm(Reg::Rdx, Reg::Rax, SUB_BITS_END_DISP)?;
    buffer.test_ri(Reg::Rdx, BITS_PER_BYTE - 1)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // (7) The one remaining call: the machine's capacity lookup and the room rule. No
    // position store precedes it, and none is needed (see the module docs): the block
    // prologue already published this block's entry — a valid resume point whose
    // re-execution re-runs the block prefix — and `jit_append_cursor` cannot raise, park,
    // pre-empt, allocate or collect, so nothing between it and the next publishing point
    // (the `fallback` arm's `jit_step_one`, a block exit, a native transfer) can observe
    // `Machine::current_code` naming this instruction. The arguments are already in their
    // ABI registers (`rdi` = `rt`, `rsi` = base, `rdx` = end, `rcx` = tail width), so no
    // caller-saved register carries anything across and nothing is kept live.
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_ri(Reg::Rcx, i64::from(tail_bits))?;
    buffer.mov_ri(Reg::R11, jit_append_cursor as *const () as i64)?;
    buffer.call_r(Reg::R11)?;

    // (8) The door's own refusal point — and the only test of `rax` on this path.
    buffer.test_rr(Reg::Rax, Reg::Rax)?;
    buffer.jcc(Cond::E, fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Emit the door for a cached x-register accumulator and return the emitted byte length.
    fn emitted_len() -> usize {
        let mut allocation = Allocation::default();
        // `x(0)` lives in its callee-saved machine register for the whole block.
        allocation.slots.push((0, Reg::R12));
        let mut buffer = CodeBuffer::new().expect("an executable code buffer");
        let fallback = buffer.label();
        let mut highest_touched = None;
        emit_append_cursor(&mut buffer, &allocation, Loc::X(0), 40, fallback, &mut highest_touched)
            .expect("the aligned append door emits");
        buffer.len()
    }

    /// The door's emitted sequence, byte for byte — the invariant that no `movabs` (10 B) crept
    /// back into the guard group and no extra instruction was added anywhere.
    ///
    /// Fixed encodings make the length a closed-form constant, so the pin is deterministic.
    /// Update it **deliberately** whenever the sequence legitimately changes, and re-state the
    /// breakdown below.
    #[test]
    fn the_emitted_sequence_has_the_pinned_length() {
        // 25 instructions / 155 B for a cached x-register accumulator:
        //   3 B    accumulator load             1 instr   `mov rax, r12`
        // 112 B    guard group                 18 instrs `movabs`-free, imm32 forms only
        //  31 B    marshalling + call           4 instrs
        //   9 B    refusal                      2 instrs
        // 155 B   total                       25 instrs
        //
        // Marshalling + call (31 B / 4): `mov rdi, [rsp+RUNTIME_SLOT]` (8 B, base `rsp`),
        //   `movabs rcx, tail_bits` (10 B), `movabs r11, jit_append_cursor` (10 B), `call r11`
        //   (3 B). Refusal (9 B / 2): `test rax, rax` (3 B) + `je fallback` (6 B) — the door's
        //   own last two instructions.
        // The guard group is where a regression would show: a `movabs` (10 B) in place of an
        //   imm32 test/and (6 B) adds 4 B per occurrence, and any extra instruction shows up
        //   here. Guard order (each `jcc` going to `fallback`): `test rax, NOT_BOXED`/`jne`,
        //   `test rax, PRIMARY`/`je`, `and rax, POINTER_MASK`, `mov rcx, [rax+HEADER]`/
        //   `cmp rcx`/`jne`, `mov rcx, [rax+START]`/`test rcx`/`jne`, `mov rsi, [rax+BASE]`/
        //   `and rsi`/`test rsi`/`je`, `mov rdx, [rax+END]`/`test rdx, 7`/`jne`.
        assert_eq!(emitted_len(), 155, "the pinned post-L4..L6 sequence");
    }
}
