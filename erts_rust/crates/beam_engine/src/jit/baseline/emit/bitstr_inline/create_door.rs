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
//!
//! ```text
//!   <the accumulator term, from its x-register or its Y slot>    ; no store
//!   is_boxed        off fallback                                 ; guard
//!   not a bare header word  off fallback                         ; guard
//!   header == HEADER_SUB_BITS       off fallback                 ; guard
//!   is_bin_ref([p + ORIG])          off fallback                 ; 3 guards
//!   start == 0                      off fallback                 ; guard
//!   base = [p + BASE] & !FLAGS      off fallback / zero          ; 2 guards
//!   end = [p + END], end % 8 == 0   off fallback                 ; guard
//!   mov rdi,rt ; <position store> ; movabs rcx,tail_bits
//!   movabs r11,jit_append_cursor ; call r11                      ; the one remaining call
//!   test rax, rax ; je fallback                                  ; the door's own refusal
//! ```
//!
//! `rax` answers the **byte cursor** `base + end / 8` on success and `0` on every refusal,
//! which is exactly what the aligned field phase stores its bytes at.
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
//! # Every guard is a refusal and nothing here writes
//!
//! Each test above branches to `fallback` and **stores nothing**, so the whole sequence is
//! re-executable: `fallback` is one interpreter step re-running the instruction with the
//! same operands, and it must see an accumulator the block's prefix has not touched. The one
//! mutation the build performs — the `ErlSubBits.end` advance — is emitted last, by
//! [`super::super::create`]'s phase 5, after every field byte store.
//!
//! # Registers
//!
//! Only caller-saved scratch: `rax` (the term, then the door's answer), `rcx`/`rdx` (constant
//! and compare scratch, then the ABI's third and fourth arguments), `rsi` (the payload base,
//! the ABI's second argument), `r8` (the accumulator's block pointer, dead before the call)
//! and `rdi` (the runtime). `rbx` (`machine.x`'s base), `rsp` and every cache register are
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
    BIN_REF_HEADER, HEADER_DISP, NOT_BOXED_MASK, POINTER_MASK, SUB_BITS_END_DISP, SUB_BITS_HEADER,
    SUB_BITS_ORIG_DISP, SUB_BITS_START_DISP,
};
use super::super::super::operands::{x_offset, Loc};
use super::super::super::prologue;
use super::super::super::{spill, touch, Allocation, SpillSite, RUNTIME_SLOT_DISP};

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
const BITS_PER_BYTE: i64 = 8;

/// Emit the aligned append door for the accumulator at `source`, appending `tail_bits` bits.
///
/// Leaves `rax` = the **byte cursor** the field stores write at (and falls through), or jumps
/// to `fallback` with nothing written. The `test rax, rax ; je fallback` pair is this
/// function's own last two instructions, so the caller must not emit a second one.
///
/// See the module docs for the proof sequence, the register contract and the frozen layouts.
pub(super) fn emit_append_cursor(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    source: Loc,
    tail_bits: u32,
    func: u32,
    pc: u32,
    fallback: CodeLabel,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    // The cached x-registers go to `machine.x` first: the accumulator load below reads the
    // block's authoritative view of the term from there, and every post-call re-read of a
    // field value (the write phase) relies on that store having happened.
    spill(buffer, allocation, SpillSite::Helper, pc)?;

    // (1) The accumulator term. A `Y` slot needs no second frame-window guard: the shape's
    // phase (1b) already proved the slot inside the published window, and nothing between
    // there and here (a spill, a compare chain) can move the frame.
    match source {
        Loc::X(x) => {
            buffer.mov_rm(Reg::Rax, Reg::Rbx, x_offset(x))?;
            touch(highest_touched, x);
        }
        Loc::Y(k) => emit_y_slot_value(buffer, Reg::Rax, k)?,
    }

    // (2) `beam_term::refc::is_refc_binary`'s first clause — `is_boxed_term(t)`, i.e. the
    // `is_boxed` bit trick and then "not a bare header word" (which `is_boxed` alone admits,
    // and which would otherwise be dereferenced as a pointer).
    buffer.mov_ri(Reg::Rcx, NOT_BOXED_MASK)?;
    buffer.test_rr(Reg::Rax, Reg::Rcx)?;
    buffer.jcc(Cond::Ne, fallback)?;
    buffer.mov_ri(Reg::Rcx, PRIMARY_MASK)?;
    buffer.test_rr(Reg::Rax, Reg::Rcx)?;
    buffer.jcc(Cond::E, fallback)?;
    // `boxed_val(t)` — the accumulator's header word address, kept in `r8`.
    buffer.mov_ri(Reg::R8, POINTER_MASK)?;
    buffer.and_rr(Reg::R8, Reg::Rax)?;
    // Its header must say `ErlSubBits`: a `BinRef`, a tuple, an `ErlHeapBits` — every other
    // boxed term — is not a refc binary.
    buffer.mov_rm(Reg::Rcx, Reg::R8, HEADER_DISP)?;
    buffer.mov_ri(Reg::Rdx, SUB_BITS_HEADER)?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // (3) `orig` (word 4) must be a `BinRef` — `is_bin_ref(orig)`, i.e. `is_boxed_term(orig)`
    // and `*boxed_val(orig) == HEADER_BIN_REF`. An `ErlSubBits` whose `orig` is anything else
    // is a **match context** (or an on-heap-bitstring window), which has no writable payload.
    buffer.mov_rm(Reg::Rcx, Reg::R8, SUB_BITS_ORIG_DISP)?;
    buffer.mov_ri(Reg::Rdx, NOT_BOXED_MASK)?;
    buffer.test_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::Ne, fallback)?;
    buffer.mov_ri(Reg::Rdx, PRIMARY_MASK)?;
    buffer.test_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::E, fallback)?;
    buffer.mov_ri(Reg::Rdx, POINTER_MASK)?;
    buffer.and_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.mov_rm(Reg::Rcx, Reg::Rcx, HEADER_DISP)?;
    buffer.mov_ri(Reg::Rdx, BIN_REF_HEADER)?;
    buffer.cmp_rr(Reg::Rcx, Reg::Rdx)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // (4) `start` (word 2) must be `0`: C asserts it for both append forms
    // (`erl_bits.c:1943-1948`), because an in-place append writes at absolute payload offsets.
    buffer.mov_rm(Reg::Rcx, Reg::R8, SUB_BITS_START_DISP)?;
    buffer.test_rr(Reg::Rcx, Reg::Rcx)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // (5) The payload base (word 1 without its flag bits) — non-null, and the helper's second
    // argument. It is the key the machine's writable-payload registry is asked about.
    buffer.mov_rm(Reg::Rsi, Reg::R8, SUB_BITS_BASE_DISP)?;
    buffer.mov_ri(Reg::Rcx, SUB_BITS_POINTER_MASK)?;
    buffer.and_rr(Reg::Rsi, Reg::Rcx)?;
    buffer.test_rr(Reg::Rsi, Reg::Rsi)?;
    buffer.jcc(Cond::E, fallback)?;

    // (6) The window end (word 3) — the helper's third argument — which must be on a byte
    // boundary for this route's cursor (`start == 0`, so the word is the window's bit size).
    buffer.mov_rm(Reg::Rdx, Reg::R8, SUB_BITS_END_DISP)?;
    buffer.mov_ri(Reg::Rcx, BITS_PER_BYTE - 1)?;
    buffer.test_rr(Reg::Rdx, Reg::Rcx)?;
    buffer.jcc(Cond::Ne, fallback)?;

    // (7) The one remaining call: the machine's capacity lookup and the room rule. The
    // position is published **before** it runs, exactly as `emit_positioned_helper_call`
    // does, and the arguments are already in their ABI registers (`rdi` = `rt`, `rsi` = base,
    // `rdx` = end, `rcx` = tail width), so no caller-saved register carries anything across.
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    prologue::emit_position_store(buffer, func, pc)?;
    buffer.mov_ri(Reg::Rcx, i64::from(tail_bits))?;
    buffer.mov_ri(Reg::R11, jit_append_cursor as *const () as i64)?;
    buffer.call_r(Reg::R11)?;

    // (8) The door's own refusal point — and the only test of `rax` on this path.
    buffer.test_rr(Reg::Rax, Reg::Rax)?;
    buffer.jcc(Cond::E, fallback)
}
