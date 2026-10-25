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

//! The **gold-standard oracle** for [`super::create_field::emit_bit_field_store`]: the
//! straight-line sequence `Route::BitGranular` emits for one field is executed for **every**
//! width, **every** window offset and **every** window end, over a byte-pre-filled payload,
//! and its bytes are compared against the family's own bit-granular write rule —
//! [`crate::families::bitstrings::create::seam::write_bit_field`], i.e.
//! `raw::write_payload_bit`'s three cases, applied bit by bit to a second copy of the same
//! pre-filled payload.
//!
//! The reference is applied to the **same starting bytes**, never to a zeroed buffer: the
//! rule's first two cases are exactly about the residue a byte already holds (a write that
//! enters a byte clears it, and a write at the accumulator's window end keeps that byte's
//! high `p0` bits), so a zeroed reference could not tell a masking store from a clearing one.
//! Every payload here is filled with `0xA5` for that reason.
//!
//! The sweep is exhaustive rather than sampled because the emitted sequence is a *closed-form*
//! straight-line encoding: its byte-count, its shift counts, its mask and its "one byte more
//! when the field reaches past its whole bytes" test are all functions of
//! `(bits, offset_bits)` alone. A boundary mistake (the `p0 == 0` clear case, the `8 == 8`
//! mask, the last whole byte at a one-bit width) shows up in exactly one
//! `(bits, offset_bits, window end, value)` corner, which is the corner a hand-picked table
//! would miss.
//!
//! One native function is emitted per `(bits, offset_bits)` pair — everything else is a
//! run-time argument — and the payload, the window end and the value are then looped inside,
//! so the sweep costs one compile per pair rather than one per case.

use beam_common::BeamError;
use beam_term::{make_small, signed_value, Eterm, MAX_SMALL};

use crate::families::bitstrings::create::seam;
use crate::jit::emitter::{CodeBuffer, Reg};

use super::create_field::{
    emit_bit_field_store, MAX_INLINE_FIELD_BITS, PRODUCTION_FIELD_SCRATCH,
};

/// The payload every row writes into, in bytes.
///
/// Wide enough for the whole sweep with room to spare: the deepest bit any row writes is
/// `end_bits + offset_bits + bits - 1 = 40 + 64 + 57 - 1 = 160`, i.e. payload byte `20`, and
/// the emitted sequence writes the byte holding the field's **last** bit plus (at a
/// non-zero offset) the following one, so 48 bytes is comfortably in bounds. The extra
/// bytes are also what makes a *stray* store visible: they stay `0xA5` on both sides.
const PAYLOAD_BYTES: usize = 48;

/// The payload's pre-fill. A non-zero, non-trivial pattern: a fresh payload byte in the real
/// build holds whatever an earlier attempt left, and the rule's mask case exists precisely to
/// preserve the bits above a window end.
const FILL: u8 = 0xA5;

/// The form every emitted sequence is called as: `(payload base, window end in bits, the
/// field's raw small-integer word)`.
type StoreFn = extern "C" fn(*mut u8, usize, u64) -> i64;

/// Emit one callable function that performs `emit_bit_field_store` for `(bits, offset_bits)`
/// over the arguments above and answers `1`.
///
/// The three argument registers are moved to the registers [`PRODUCTION_FIELD_SCRATCH`]
/// assigns — `r11` (the payload base) and `r10` (the window end) are preserved across the
/// fields, and the value is the caller's `rax` — which is exactly what the arm's own emitted
/// prologue leaves behind, so the sequence under test runs with the register assignment it
/// runs with in production.
fn emit_store(bits: u32, offset_bits: u32) -> CodeBuffer {
    let mut buffer = CodeBuffer::new().expect("a code buffer");
    let assigned = |register: Reg, expected: Reg, what: &str| {
        assert_eq!(
            register, expected,
            "the scratch assignment must keep the {what} in {expected:?}"
        );
    };
    assigned(
        PRODUCTION_FIELD_SCRATCH.base,
        Reg::R11,
        "payload base (the door call's answer)",
    );
    assigned(
        PRODUCTION_FIELD_SCRATCH.end,
        Reg::R10,
        "accumulator window end",
    );
    buffer
        .mov_rr(PRODUCTION_FIELD_SCRATCH.base, Reg::Rdi)
        .expect("the prologue moves the base");
    buffer
        .mov_rr(PRODUCTION_FIELD_SCRATCH.end, Reg::Rsi)
        .expect("the prologue moves the window end");
    buffer
        .mov_rr(Reg::Rax, Reg::Rdx)
        .expect("the prologue moves the value");
    emit_bit_field_store(
        &mut buffer,
        Reg::Rax,
        bits,
        offset_bits,
        &PRODUCTION_FIELD_SCRATCH,
    )
    .unwrap_or_else(|error| panic!("({bits}, {offset_bits}): the sequence must emit: {error:?}"));
    buffer.mov_ri(Reg::Rax, 1).expect("the epilogue answers 1");
    buffer.ret().expect("the epilogue returns");
    buffer.seal().expect("the buffer must seal");
    buffer
}

/// The values every `(bits, offset_bits, end_bits)` row is run with: the two sign corners, the
/// three small magnitudes, and one value whose low bits are only reachable through a shift
/// (a `2^40` magnitude needs a 41-bit window, so at a narrow width it is the *low* bits that
/// must be written and at a wide one the high ones).
fn values() -> [Eterm; 6] {
    [
        make_small(-1),
        make_small(0),
        make_small(1),
        make_small(0x2A),
        make_small(-(1i64 << 40)),
        make_small(MAX_SMALL),
    ]
}

/// **The semantic proof: for every width `1 ..= 57`, every window offset `0 ..= 64` and every
/// window end `0 ..= 40`, the emitted stores reproduce the family's bit-granular rule byte for
/// byte — over a payload that already holds `0xA5`.**
///
/// The oracle is
/// [`seam::write_bit_field`](crate::families::bitstrings::create::seam::write_bit_field)
/// applied to a second copy of the same pre-filled payload, i.e. the rule every interpreter
/// construction runs, so the two cannot agree by sharing an implementation detail. Both sides
/// start from identical bytes and each case asserts all [`PAYLOAD_BYTES`] of them, which is
/// what makes a store that writes **too much** (a cleared byte it should have merged into, an
/// extra byte past the field) as visible as one that writes too little.
#[test]
fn every_width_offset_and_window_end_matches_the_familys_write_rule() {
    let values = values();
    let mut cases = 0usize;
    for bits in 1..=MAX_INLINE_FIELD_BITS {
        for offset_bits in 0..=64u32 {
            let buffer = emit_store(bits, offset_bits);
            let store: StoreFn = buffer.as_fn_ptr();
            for end_bits in 0..=40usize {
                for word in values {
                    let mut native = [FILL; PAYLOAD_BYTES];
                    let mut reference = [FILL; PAYLOAD_BYTES];
                    // The value argument is the term's **raw word** (the tagged small-integer
                    // immediate), which is exactly what the arm hands the sequence after its
                    // guarded `machine.x` read: the sequence strips the tag itself.
                    let answer = store(native.as_mut_ptr(), end_bits, word.raw() as u64);
                    seam::write_bit_field(
                        reference.as_mut_ptr() as usize,
                        end_bits,
                        end_bits + offset_bits as usize,
                        signed_value(word) as u64,
                        bits,
                    );
                    let case = format!(
                        "bits={bits} offset_bits={offset_bits} end_bits={end_bits} value={}",
                        signed_value(word)
                    );
                    assert_eq!(answer, 1, "{case}: the emitted function must answer 1");
                    assert_eq!(
                        native, reference,
                        "{case}: the emitted stores must reproduce the family's \
                         bit-granular rule over a 0x{FILL:02X}-pre-filled payload"
                    );
                    cases += 1;
                }
            }
        }
    }
    // The sweep's own floor: a loop that silently stopped iterating (a range that became
    // empty) must not pass as a green oracle.
    assert_eq!(
        cases,
        MAX_INLINE_FIELD_BITS as usize * 65 * 41 * values.len(),
        "the sweep must cover every width, offset, window end and value"
    );
}

/// **The gate's own boundary: a width outside `1 ..= `[`MAX_INLINE_FIELD_BITS`] is refused
/// with a typed error and emits nothing.**
///
/// The refusal is unreachable from [`super::emit_bit_fields_inline`], whose per-plan gate is
/// the same constant — which is exactly why it is pinned here: the two must stay the same
/// number, and a silent widening (or a zero-width field slipping through to a shift of 64)
/// would be a wrong result rather than a slower one.
#[test]
fn a_width_outside_the_inline_range_is_a_typed_error_and_emits_nothing() {
    for bits in [0u32, MAX_INLINE_FIELD_BITS + 1, 64, u32::MAX] {
        let mut buffer = CodeBuffer::new().expect("a code buffer");
        let outcome =
            emit_bit_field_store(&mut buffer, Reg::Rax, bits, 3, &PRODUCTION_FIELD_SCRATCH);
        assert!(
            matches!(outcome, Err(BeamError::BadArg)),
            "bits={bits} must be a typed `BadArg`, got {outcome:?}"
        );
        assert!(
            buffer.is_empty(),
            "bits={bits} must emit no byte at all (the refusal precedes the first store)"
        );
    }
    // The widest accepted width still emits, so the boundary above is the real one rather
    // than an artefact of the refusal being unconditional.
    let mut buffer = CodeBuffer::new().expect("a code buffer");
    emit_bit_field_store(
        &mut buffer,
        Reg::Rax,
        MAX_INLINE_FIELD_BITS,
        0,
        &PRODUCTION_FIELD_SCRATCH,
    )
    .expect("the widest accepted width must emit");
    assert!(!buffer.is_empty(), "the widest accepted width emitted nothing");
}
