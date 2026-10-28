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
//! The **bare** `bs_create_bin` build ([`super::Route::Fresh`]): a construction whose segments
//! carry no `private_append` group at all, so its fields *are* the whole result
//! (`<<A:8, B:8>>`, `<<Digit:6>>` — the emqx `emqx_base62` digit extraction).
//!
//! Such an instruction has no accumulator: nothing is appended to, no append door is opened,
//! and the payload is written from scratch. `seam::fresh_plan` is the one predicate that
//! decides which segment sets have that form (statically sized big-endian unsigned integer
//! fields over x/y registers, 1..=64 bits each, at most 64 bits in total), and
//! `seam::make_fresh_binary` is the one materialization rule — so this module owns only the
//! emitted bytes.
//!
//! # The emitted sequence
//!
//! ```text
//!   <dst's Y-window guard>                    ; a rejected destination writes nothing
//!   spill                                     ; machine.x authoritative before any call
//!   per field, in instruction order:
//!     <guarded load of the value into rdx>    ; a Y value's frame-window guard is its load
//!     mov rax, rdx ; mov rcx, SMALL_TAG ; and rax, rcx ; cmp rax, rcx ; jne fallback
//!     sar rdx, 4                              ; untag (arithmetic: negatives sign-extend)
//!     [shl rdx, 64-bits ; shr rdx, 64-bits]   ; mask to the field width (skipped at 64 bits)
//!     first: mov pack, rdx                    ; a COPY — a 64-bit first field must not shift
//!     else:  shl pack, bits ; or pack, rdx
//!   mov rdi, [rsp + rt] ; <inline position store>
//!   mov rsi, pack ; mov rdx, tail_bits ; call jit_make_heap_binary
//!   test rax, rax ; je fallback               ; THE decline point — 0 is THE_NON_VALUE
//!   fill                                      ; post-allocation: cached x-registers are stale
//!   <dst's Y address re-derived> ; mov [dst], rax
//!   <the gated jit_refresh_y>                 ; only when a later in-block instruction uses Y
//!   jmp fast_done
//! fallback:
//!   <one interpreter step>                    ; the family builds the term itself
//! fast_done:
//! ```
//!
//! The pack word is the plan's fields concatenated, **the first field in the most significant
//! position**, each already the field's masked low bits — exactly [`seam::make_fresh_binary`]'s
//! caller contract, which left-aligns the word into a fresh payload and is the family's own
//! value byte for byte.
//!
//! # Why no field needs a range guard
//!
//! The family writes `value & mask(bits)` for a statically sized unsigned integer field: it
//! truncates (`<<300:8>>` is `<<44>>`), and for a negative value it writes the low `bits` of
//! the two's complement (`<<-1:8>>` is `<<255>>`, `<<-1:64>>` is eight `FF` bytes). Masking a
//! small integer reproduces that exactly — `sar` untags with sign extension and the
//! `shl`/`shr` pair keeps the low `bits` — and at 64 bits the mask is the identity, so a
//! **64-bit field needs no instruction at all**. There is therefore nothing to guard beyond
//! "is it a small integer": a bignum or any other non-small value is *not* reproduced by the
//! masking, so it takes `fallback` and the family writes (or refuses with `badarg`) itself.
//! That one tag test is the whole value check, and it is the same `is_small` guard the append
//! route emits.
//!
//! # Deopt idempotency
//!
//! A `fallback` (here: a non-small field value, or the helper's own `0` refusal) is a deopt —
//! `jit_step_one` re-executes the **whole** instruction with the same operands — so nothing may
//! be committed before the last guard that can reach it:
//!
//! * the packed word lives only in a **scratch register** ([`PACK`]); no `machine` or heap word
//!   holds it, so a re-execution re-packs it from the same slots;
//! * `spill` only writes the cached x-registers into `machine.x`, which is where the values it
//!   stores already were (the cache mirrors them), so it is a no-op in terms of observable
//!   state — and it is **mandatory**, because the helper below allocates and a collecting host
//!   relocates exactly the words `machine.x` holds;
//! * every guard (the destination's window guard, each field's load + tag test) precedes the
//!   helper call, and the helper is the only allocation on the path — so no guard can ever run
//!   against a machine the helper has already mutated;
//! * the destination store follows the helper with **no guard after it**, so the sequence has
//!   exactly one commit point — the helper's payload — which is itself re-execution
//!   idempotent for the same `(packed, nbits)` (it writes no machine state and produces an
//!   equal term), and one store, which a re-execution repeats with the equal term.
//!
//! # Registers
//!
//! [`PACK`] (`r8`) is provably free from the spill through the helper call: `rax` carries the
//! tag-test scratch and then the built term (it must survive `fill`, which only writes cache
//! registers — never `rax`), `rdx`/`rcx` are `Loc::load`'s value and window-bound scratches (and
//! `rdx` is the helper's third argument), `rsi`/`rdx`/`rdi`/`r11` are the call's own
//! (`pack` → `rsi`, `tail_bits` → `rdx`, `rt` → `rdi`, the helper address → `r11`), `r10`/`r11`
//! are what the inline position store clobbers (which is why `r10` — [`super::super::MOVE_SCRATCH_2`]
//! — cannot hold the pack word), `r9`/`r8` are the destination guard's address/bound scratches
//! (dead after it, and `r9` — [`super::super::operands::Y_ADDR_SCRATCH`] — is re-derived for the
//! final store), and `rbx` (`machine.x` base), `rsp` (the `rt` slot) and the five cache
//! registers `r12`–`r15`/`rbp` belong to the block ABI.
//!
//! The gated `jit_refresh_y` is emitted **after** the destination store for one reason: it is a
//! call, so `rax` does not survive it. It exists for the block's *later* instructions (a frame
//! change would move the window), which is exactly where `emit_continue_check` puts it, and the
//! store still addresses the window the block published — the helper allocates but pushes no
//! frame, the same argument the append route's post-helper `Y` re-reads rest on.

use beam_common::Result;
use beam_term::TAG_IMMED1_SIZE;

use crate::families::bitstrings::create::seam;
use crate::jit::baseline::runtime::jit_make_heap_binary;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::operands::{Loc, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2};
use super::super::prologue;
use super::super::{
    emit_refresh_y, fill, spill, Allocation, FillSite, SpillSite, RUNTIME_SLOT_DISP,
};
use super::create::emit_y_slot_address;
use super::{emit_fallback_and_done, CreateBinShape, SMALL_TAG};

#[cfg(test)]
use super::{record_inlined, SHAPE_CREATE_FRESH};

/// The register the fields are packed into (see the module docs for why it is free across the
/// whole sequence).
const PACK: Reg = Reg::R8;

/// Emit the bare-build route for `shape` (see the module docs), or report that the block must
/// helper the instruction (`Ok(false)` with **zero bytes emitted**).
pub(super) fn emit_fresh_build(
    buffer: &mut CodeBuffer,
    shape: &CreateBinShape,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    // Every field resolved **before the first emitted byte**: a value the tier cannot address
    // (or a field kind a bare plan cannot carry) emits nothing at all and leaves the whole
    // instruction to the family, exactly like a shape `CreateBinShape::of` refused.
    let mut fields: Vec<(Loc, u32)> = Vec::with_capacity(shape.fields.len());
    for field in &shape.fields {
        // `seam::fresh_plan` accepts integer segments only, so a constant field would be a
        // contract violation rather than a shape this route can emit.
        let seam::Field::Bits { value, bits } = field else {
            return Ok(false);
        };
        let Some(value) = Loc::of(value) else {
            return Ok(false);
        };
        fields.push((value, *bits));
    }
    // A build with no field has nothing to pack (and `tail_bits` would be 0): the family's
    // route.
    if fields.is_empty() {
        return Ok(false);
    }
    // The route is taken: recorded here, before the first emitted byte and after the last
    // refusal, exactly like the append route's own instrument — so a shape this arm declined
    // cannot move the counter.
    #[cfg(test)]
    record_inlined(SHAPE_CREATE_FRESH);

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // (0) The destination's frame-window guard, so a rejected slot writes nothing at all. The
    // address it leaves in `Y_ADDR_SCRATCH` is discarded and re-derived after the helper; the
    // guard is what makes that later unguarded re-derivation sound.
    shape
        .dst
        .guard_store(buffer, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)?;

    // (1) The spill comes early: the helper below allocates (and a collecting host relocates
    // what `machine.x` holds), so the register file must be authoritative before the call.
    spill(buffer, allocation, SpillSite::Helper, pc)?;

    // (2) Pack the fields: one guarded load + one `is_small` test each, then untag, mask to the
    // field's width and shift into the pack word (first field most significant). Every one of
    // those guards runs before the helper, and the pack lives only in `PACK`.
    for (index, (value, bits)) in fields.iter().enumerate() {
        value.load(
            buffer,
            allocation,
            Reg::Rdx,
            Reg::Rcx,
            fallback,
            highest_touched,
        )?;
        // The tag test destroys its input, so the raw word is copied out first.
        buffer.mov_rr(Reg::Rax, Reg::Rdx)?;
        // One tag word per field serves both the mask and the compare: `IMMED1_MASK ==
        // SMALL_TAG` (asserted beside the constants), so `(v & mask) == tag` is
        // `(v & SMALL_TAG) == SMALL_TAG`. It is (re)materialised *after* the load because a
        // `Y` field value's load clobbers the end scratch register.
        buffer.mov_ri(Reg::Rcx, SMALL_TAG)?;
        buffer.and_rr(Reg::Rax, Reg::Rcx)?;
        buffer.cmp_rr(Reg::Rax, Reg::Rcx)?;
        buffer.jcc(Cond::Ne, fallback)?;
        // Untag: `make_small(v) == v * 16 + 15`, so an arithmetic shift is the exact value —
        // and for a negative one it sign-extends to the two's complement the family writes.
        buffer.sar_ri(Reg::Rdx, TAG_IMMED1_SIZE)?;
        // Mask to the field width. A 64-bit field's mask is the identity, and the encoding
        // would turn a count of 64 into a shift by 0, so that width emits nothing at all.
        if *bits < 64 {
            let pad = 64 - *bits;
            buffer.shl_ri(Reg::Rdx, pad)?;
            buffer.shr_ri(Reg::Rdx, pad as u8)?;
        }
        if index == 0 {
            // A copy, not a shift: a lone 64-bit field must not be shifted by 64 (the encoding
            // masks the count to 6 bits, so it would be lost).
            buffer.mov_rr(PACK, Reg::Rdx)?;
        } else {
            buffer.shl_ri(PACK, *bits)?;
            buffer.or_rr(PACK, Reg::Rdx)?;
        }
    }

    // (3) The one call: `jit_make_heap_binary(rt, packed, tail_bits)`. The position is
    // published **before** it runs (exactly what `emit_positioned_helper_call` does), because
    // the helper allocates and may collect; the packed word and the width are the only
    // arguments, and no caller-saved register carries anything across.
    buffer.mov_rm(Reg::Rdi, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    prologue::emit_position_store(buffer, func, pc)?;
    buffer.mov_rr(Reg::Rsi, PACK)?;
    buffer.mov_ri(Reg::Rdx, i64::from(shape.tail_bits))?;
    buffer.mov_ri(Reg::R11, jit_make_heap_binary as *const () as i64)?;
    buffer.call_r(Reg::R11)?;

    // (4) The helper's own refusal point — `0` is `THE_NON_VALUE`, which no build produces —
    // and the sole decline point of the whole sequence.
    buffer.test_rr(Reg::Rax, Reg::Rax)?;
    buffer.jcc(Cond::E, fallback)?;

    // (5) The helper allocated, so the cached x-registers are stale: refill them from
    // `machine.x` (which the spill above made authoritative) **before** the destination store,
    // so a cached destination register ends up holding the built term.
    fill(buffer, allocation, FillSite::Helper, pc)?;

    // (6) The destination. A `Y` slot's address is re-derived behind the up-front guard: the
    // helper allocates but pushes no frame, so the published window still holds the slot (the
    // same argument the append route's post-helper `Y` re-reads rest on). Nothing after this
    // can fail.
    if let Loc::Y(k) = shape.dst {
        emit_y_slot_address(buffer, Y_ADDR_SCRATCH, k)?;
    }
    shape.dst.store_at(
        buffer,
        Y_ADDR_SCRATCH,
        Reg::Rax,
        allocation,
        highest_touched,
    )?;

    // (7) The gated Y-window refresh for the block's **later** instructions, under
    // `emit_continue_check`'s own compile-time gate. It is a call, so it has to come after the
    // store (`rax` carries the built term up to there).
    if allocation.refresh_y_after(pc) {
        emit_refresh_y(buffer)?;
    }

    emit_fallback_and_done(buffer, allocation, done, func, pc, fallback, fast_done)
}
