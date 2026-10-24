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

//! The **construction seam**: the two facts a native lowering of `bs_create_bin`
//! cannot derive on its own, exposed as one pure compile-time query and one pure
//! run-time query.
//!
//! # Why a seam
//!
//! The compiler's linear accumulator —
//! `bs_init_writable` followed by a `bs_create_bin` whose first segment is a
//! `private_append` of the accumulator itself and whose remaining segments are
//! statically sized fields — is the one bitstring shape a tier can lower to a handful of
//! stores. Two things it needs are family-private:
//!
//! * **static decodability** — whether this instruction's segment list is that shape,
//!   which fields it writes in which order and at which widths, and what the constant
//!   bytes of a `string` segment are. All of it is decided by
//!   [`prepare`](super::prepared::prepare) from the module's atoms, `StrT` table and the
//!   instruction's own operands, and every answer lives in a private [program
//!   type](super::prepared::CreateProgram).
//! * **the writable payload's capacity** — whether, and where, the accumulator can
//!   still be written. That figure is machine state
//!   (`Machine::writable_payloads`); the term carries only its window end.
//!
//! [`append_plan`] answers the first, [`open_append`] the second. Both are pure: they
//! read, they allocate nothing, they mutate nothing, and they are safe to call
//! speculatively (a speculative [`open_append`] on a shape the family never enters is
//! exactly as harmless as a speculative read).
//!
//! # Two-level gating — neither side decides for the other
//!
//! * The **family** owns *what the shape means*: [`append_plan`] answers `Some` only for
//!   a segment list the prepared program resolves to the in-place accumulator form with
//!   every later segment statically sized and side-effect free. Everything else is
//!   `None`, which is not an error — it means "this instruction is not that shape".
//! * The **tier** owns *what it can emit*: a `Some` plan may still be declined (a
//!   register-operand form it does not lower, a payload capacity it cannot check
//!   inline). `None` from [`open_append`] is the universal fallback signal: run the
//!   helper.
//!
//! The seam never decides the tier's business and the tier never re-derives the family's
//! static facts (a second reading of the flag word or of a `StrT` window is exactly the
//! divergence this seam exists to prevent).
//!
//! # The invariant a lowering relies on
//!
//! **For any shape [`append_plan`] accepts, when [`open_append`] answers `Some` the
//! family's own build is `Built::Term(source)` — the very `Eterm` it was given, no
//! growth, no payload republish, no allocation of any kind** — so a lowering may store
//! the source register into the destination register and be exactly right.
//!
//! That is not an assumption; it follows from the three gates [`open_append`] checks:
//!
//! 1. `raw::writable_append_target` — the source is an off-heap refc binary with
//!    `start == 0` over a payload **this machine** registered as writable. That is the
//!    family's own entry condition (`create/mod.rs`'s `private_append` arm), so the
//!    build takes the in-place route.
//! 2. `end_bits % 8 == 0` — the accumulator's window is whole-byte aligned. The
//!    family's `erts_bs_append_checked` unit gate (`src_bits % unit != 0` ⇒ `badarg`)
//!    becomes vacuous for every unit [`append_plan`] accepts (`1`, `2`, `4`, `8`: every
//!    unit that divides 8), because `src_bits == end_bits` at `start == 0`.
//! 3. `end_bits + tail_bits <= capacity_bits` — the payload already has room for every
//!    field this instruction writes. [`AppendPlan::tail_bits`] is exactly the figure the
//!    prepared program arms its per-instruction reservation with
//!    (`BitWriter::arm_room`), and [`BitWriter`](super::super::BitWriter) **only ever
//!    writes into room it has verified** — the reservation is consulted before every
//!    write, and its single growth door (`raw::InPlace::grow`) is reached only on a
//!    shortfall. With the room pre-verified there is no shortfall, so no
//!    `raw::InPlace::grow`, no relocation, no `republish_over` and no promotion to the
//!    copy-rebuild form.
//!
//! The one mutation the build does perform is C's `sb->end = new_position`
//! (`erl_bits.c:1975`) — `ErlSubBits.end` advanced by exactly `tail_bits`. That is the
//! build's **result**, not a side effect on the accumulator's identity: the term word is
//! unchanged, and a lowering storing it needs no republish step.
//!
//! A field value that is not an integer is still the family's `badarg`
//! (`erts_bs_put_integer_*` returns 0 for a non-integer, `erl_bits.c:952`), so a
//! lowering reproduces that raise rather than treating the shape as writable — the
//! plan's static decidability says nothing about a field's run-time *type*, only about
//! its width and position.
//!
//! # What the plan deliberately does not accept
//!
//! Every field must be a statically sized, **big-endian**, **unsigned**,
//! whole-byte-wide `integer` of at most 64 bits over an x-register, or a `string`
//! constant. Everything else is `None`: a plain (`private: false`) or later-position
//! append, an append unit that does not divide 8, a `binary`/`float`/`utf*` segment, an
//! `all` or runtime (`Dynamic`) size, a `Y` or literal-area value operand, a
//! `little`/`signed`/`native` flag, a field that is not a whole number of bytes or is
//! wider than a machine word, a literal-area index or `StrT` window the module cannot
//! resolve, and a plan with no fields at all (nothing to emit).

use beam_loader::{Module, Operand, Reg};
use beam_term::Eterm;

use crate::families::bitstrings::{decode_flags, raw, BSF_LITTLE, BSF_NATIVE, BSF_SIGNED};
use crate::machine::Machine;

use super::prepared::{prepare, PreparedSeg, Width, SEGMENT_OPERANDS};

/// One write a lowering performs after the accumulator's window end.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Field {
    /// A run-time value from an x-register, written MSB-first as `bits` bits at a
    /// compile-time-known offset. `bits` is a whole number of bytes with the segment's
    /// `unit` already folded in (the width is `size * unit`, validated by `$BS_SAFE_MUL`
    /// at prepare time).
    Bits {
        /// The value operand, exactly as the instruction carries it (`Operand::X` or the
        /// compiler's `Operand::Typed { reg: Reg::X, .. }` spelling).
        value: Operand,
        /// The field width in bits — a whole number of bytes, `8..=64`.
        bits: u32,
    },
    /// Compile-time constant bytes (a `string` segment's `StrT` contents), copied out of
    /// the module's string table here so a lowering never bakes a table offset into code
    /// — a cached module's string area may be a different allocation.
    ConstBytes {
        /// The literal's bytes, in order.
        bytes: Vec<u8>,
    },
}

/// The compile-time half of a recognised in-place accumulator build.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AppendPlan {
    /// The accumulator operand (an x-register) — the term the build publishes unchanged.
    pub(crate) source: Operand,
    /// The summed width of [`AppendPlan::fields`]: exactly the bits the accumulator's
    /// window end advances by.
    pub(crate) tail_bits: u32,
    /// The fields, in instruction order.
    pub(crate) fields: Vec<Field>,
}

/// The compile-time half: recognise an in-place accumulator `bs_create_bin` and resolve
/// its fields, or `None` when the shape is not one a lowering may emit bytes for.
///
/// Pure over the module image and the instruction's operands (see the module docs for
/// the accepted shapes and the two-level gating).
pub(crate) fn append_plan(module: &Module, segments: &[Operand]) -> Option<AppendPlan> {
    let program = prepare(module, segments)?;
    let mut prepared = program.segments.iter();
    // The first segment must be the writable accumulator itself.
    let PreparedSeg::Append {
        unit,
        value,
        private: true,
    } = prepared.next()?
    else {
        return None;
    };
    // Only a unit that divides 8 keeps the family's `src_bits % unit` gate vacuous for
    // the whole-byte window `open_append` requires (see the module docs).
    if !matches!(*unit, 1 | 2 | 4 | 8) {
        return None;
    }
    let source = x_register(value)?;
    let mut fields = Vec::new();
    let mut tail_bits: u32 = 0;
    // `prepare` walks the same fixed-width groups in the same order, so the raw operands
    // line up with the prepared segments one for one. They are needed because the
    // direct-store form (`ByteInt`) keeps only the endianness — a `signed` whole-byte
    // field is indistinguishable from an unsigned one there, and a `Bits` field a
    // lowering writes MSB-first as an unsigned value must never be signed.
    for (segment, group) in prepared.zip(segment_groups(segments)) {
        let field = match segment {
            // A 1/2/4/8-byte field: prepare settled the width and the store shape.
            PreparedSeg::ByteInt {
                value,
                bytes,
                little: false,
            } => {
                let flags = decode_flags(module, &group[3]).ok()?;
                if flags & (BSF_LITTLE | BSF_SIGNED | BSF_NATIVE) != 0 {
                    return None;
                }
                Field::Bits {
                    value: x_register(value)?,
                    bits: u32::from(*bytes) * 8,
                }
            }
            // Any other static width (24/40/48/56 bits, which the compiler emits for
            // sub-word fields): accepted only whole-byte and within a machine word, with
            // no flag that changes the layout.
            PreparedSeg::Integer {
                flags,
                value,
                width: Width::Imm(nbits),
            } => {
                if *flags & (BSF_LITTLE | BSF_SIGNED | BSF_NATIVE) != 0 {
                    return None;
                }
                if *nbits == 0 || *nbits % 8 != 0 || *nbits > 64 {
                    return None;
                }
                Field::Bits {
                    value: x_register(value)?,
                    bits: *nbits as u32,
                }
            }
            // A `string` constant: its window is already resolved and range-checked, so
            // the bytes are a compile-time fact.
            PreparedSeg::String { start, end } => Field::ConstBytes {
                bytes: module.strings.get(*start..*end)?.to_vec(),
            },
            // Everything else: an `append`, `binary`, `float`, `utf*`, a dynamic or
            // `all` width, a second `private_append` — not this shape.
            _ => return None,
        };
        let field_bits = match &field {
            Field::Bits { bits, .. } => *bits,
            // A `StrT` window is a whole number of bytes (`prepare` resolves it from a
            // byte offset and a byte count).
            Field::ConstBytes { bytes } => {
                u32::try_from(bytes.len()).ok()?.checked_mul(8)?
            }
        };
        tail_bits = tail_bits.checked_add(field_bits)?;
        fields.push(field);
    }
    // A plan with no fields writes nothing; there is nothing for a lowering to emit.
    if fields.is_empty() {
        return None;
    }
    Some(AppendPlan {
        source,
        tail_bits,
        fields,
    })
}

/// The run-time half: the payload base address and the window end (in bits) a lowering
/// may write `tail_bits` more bits at, or `None` when the accumulator must take the
/// family's own path.
///
/// `Some` exactly when the source is a writable off-heap refc binary this machine
/// allocated (`raw::writable_append_target`), its window is whole-byte aligned (which
/// makes the family's append-unit gate vacuous — the module docs carry the argument) and
/// its payload has room for `tail_bits` more bits from that end.
///
/// Allocates nothing, mutates nothing, safe to call speculatively.
pub(crate) fn open_append(
    machine: &Machine,
    src: Eterm,
    tail_bits: u32,
) -> Option<(usize, usize)> {
    let (base, capacity_bits, end_bits) = raw::writable_append_target(machine, src)?;
    if end_bits % 8 != 0 {
        return None;
    }
    if end_bits.checked_add(tail_bits as usize)? > capacity_bits {
        return None;
    }
    Some((base as usize, end_bits))
}

/// The operand groups **after** the accumulator's own — one
/// `[Type, Index, Unit, Flags, Value, Size]` slice per later segment, aligned one for one
/// with the prepared segments it is paired with.
fn segment_groups(segments: &[Operand]) -> impl Iterator<Item = &[Operand]> {
    segments.chunks_exact(SEGMENT_OPERANDS).skip(1)
}

/// The operand itself when it addresses an **x** register — the only operand kind a
/// lowering can bind without a literal-area or stack read.
///
/// Both spellings the loader produces are accepted: the plain [`Operand::X`] and the
/// compiler's `z,5` [`Operand::Typed`] form (which is what a real accumulator loop
/// carries, e.g. `{tr,{x,1},{t_bitstring,8,true}}`).
fn x_register(operand: &Operand) -> Option<Operand> {
    match operand {
        Operand::X(_) | Operand::Typed { reg: Reg::X(_), .. } => Some(operand.clone()),
        _ => None,
    }
}
