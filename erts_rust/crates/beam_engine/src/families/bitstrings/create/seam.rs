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

//! The **construction seam**: the facts a native lowering of `bs_create_bin` cannot
//! derive on its own — one pure compile-time query, two pure run-time queries, and the
//! one bit-granular write the lowering must not re-derive.
//!
//! # Why a seam
//!
//! The compiler's linear accumulator —
//! `bs_init_writable` followed by a `bs_create_bin` whose first segment is a
//! `private_append` of the accumulator itself and whose remaining segments are
//! statically sized fields — is the one bitstring shape a tier can lower to a handful of
//! stores. Three facts it needs are family-private:
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
//! * **the bit-granular byte rule** — how the low `nbits` bits of a value land in a
//!   payload byte the accumulator's window already occupies in part. A lowering that
//!   re-derived it would be a second copy of the family's writer; [`write_bit_field`] is
//!   the single definition, shared with [`BitWriter`](super::super::BitWriter) (both go
//!   through `raw::write_bit_field`, whose per-bit body is also
//!   `raw::InPlace::push_bit`'s).
//!
//! [`append_plan`] answers the first, [`open_append`] / [`open_append_any`] the second and
//! [`write_bit_field`] the third. The queries are pure: they read, they allocate nothing,
//! they mutate nothing, and they are safe to call speculatively (a speculative
//! [`open_append`] on a shape the family never enters is exactly as harmless as a
//! speculative read).
//!
//! # Two-level gating — neither side decides for the other
//!
//! * The **family** owns *what the shape means*: [`append_plan`] answers `Some` only for
//!   a segment list the prepared program resolves to the in-place accumulator form with
//!   every later segment statically sized and side-effect free. Everything else is
//!   `None`, which is not an error — it means "this instruction is not that shape".
//! * The **tier** owns *what it can emit*: a `Some` plan may still be declined (an operand
//!   form it cannot bind — a `y` slot outside the published frame window, say — a plan that
//!   mixes a [`Field::ConstBytes`] with a bit-granular field, a shape past its own
//!   field/tail caps, a payload capacity it cannot check inline). A plan that is merely
//!   **bit-granular** is not declined for that reason: the tier has a bit-granular route
//!   that pairs the general door with [`write_bit_field`]. `None` from a run-time door is
//!   the universal fallback signal: run the helper.
//!
//! The seam never decides the tier's business and the tier never re-derives the family's
//! static facts (a second reading of the flag word or of a `StrT` window is exactly the
//! divergence this seam exists to prevent).
//!
//! # The invariant a lowering relies on
//!
//! **For any shape [`append_plan`] accepts, when a run-time door answers `Some` the
//! family's own build is `Built::Term(source)` — the very `Eterm` it was given, no
//! growth, no payload republish, no allocation of any kind** — so a lowering may write
//! the fields' bytes and store the source register into the destination register, and be
//! exactly right.
//!
//! That is not an assumption; it follows from the three gates a door checks:
//!
//! 1. `raw::writable_append_target` — the source is an off-heap refc binary with
//!    `start == 0` over a payload **this machine** registered as writable. That is the
//!    family's own entry condition (`create/mod.rs`'s `private_append` arm), so the
//!    build takes the in-place route.
//! 2. the **append-position unit gate** ([`append_unit_allows`]) — C's
//!    `erts_bs_append_checked` tests the *position* the build appends at
//!    (`erl_bits.c:1778-1784`): `unit == 8` requires `position % 8 == 0` and any other
//!    `unit > 1` requires `position % unit == 0`; `unit <= 1` is ungated. The family
//!    raises `badarg` otherwise (`create/mod.rs`'s `src_bits % unit` check, which for
//!    `start == 0` is the same position). A door that skipped this gate would report a
//!    shape as writable that the family *raises* on.
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
//! # The two run-time doors
//!
//! The doors are split by **alignment**, not loosened into one, because the tier's
//! straight-line lowering addresses field `i` at `cursor + byte_offset` — sound only
//! while the window end is byte-aligned.
//!
//! * [`open_append`] is the **byte-aligned** door: it refuses unless the accumulator's
//!   window end is on a byte boundary (`append_unit_allows(end_bits, 8)`). The tier's
//!   byte-aligned route — every field a whole number of bytes, addressed at
//!   `cursor + byte_offset_i` — uses it.
//! * [`open_append_any`] is the **general** door: it accepts any alignment (a bit-granular
//!   field leaves the window ending mid-byte) and applies the plan's own unit gate
//!   instead — `end_bits % unit == 0` for `unit > 1`, `unit <= 1` ungated — so the shape it
//!   accepts is exactly the shape the family appends in place at. A byte-aligned route may
//!   use either door (the two agree whenever `unit` divides 8 and the end is byte-aligned).
//!
//! The general door is the one a bit-granular lowering pairs with [`write_bit_field`],
//! since only that writer can place bits at a non-byte-aligned position.
//!
//! # What the plan deliberately does not accept
//!
//! Every field must be a statically sized, **big-endian**, **unsigned** `integer` of
//! 1..=64 bits over an x- or y-register, or a `string` constant. Everything else is
//! `None`: a plain (`private: false`) or later-position append, an append unit that is
//! not one of `1`/`2`/`4`/`8`, a `binary`/`float`/`utf*` segment, an `all` or runtime
//! (`Dynamic`) size, a **literal-area** value operand, a `little`/`signed`/`native` flag,
//! a field wider than a machine word or of zero width, a literal-area index or `StrT`
//! window the module cannot resolve, and a plan with no fields at all (nothing to emit).
//!
//! ## Why a `binary all` segment is not plannable
//!
//! A `binary` segment sized `all` copies a **run-time** number of bits (the source
//! sub-binary's own width), so the plan could carry no static width for it — and with no
//! static width there is no static offset for any field after it, which is the whole
//! product of [`append_plan`]. The only lowering possible would be an out-of-line helper
//! doing a run-time-length copy, i.e. precisely what the family already does
//! (`BitWriter::push_bits_from` → `push_whole_bytes` → `raw::InPlace::copy_bytes` for the
//! byte-aligned case, the per-bit writer otherwise), so the seam declines the shape
//! rather than inventing a second copy of that path. The tier's existing `bs_create_bin`
//! family helper covers it.

use beam_loader::{Module, Operand, Reg};
use beam_term::Eterm;

use crate::families::bitstrings::{decode_flags, raw, BSF_LITTLE, BSF_NATIVE, BSF_SIGNED};
use crate::machine::Machine;

use super::prepared::{prepare, PreparedSeg, Width, SEGMENT_OPERANDS};

/// One write a lowering performs after the accumulator's window end.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Field {
    /// A run-time value from an x- or y-register, written MSB-first as `bits` bits at a
    /// compile-time-known offset. `bits` has the segment's `unit` already folded in (the
    /// width is `size * unit`, validated by `$BS_SAFE_MUL` at prepare time) and need
    /// **not** be a whole number of bytes.
    Bits {
        /// The value operand, exactly as the instruction carries it (`Operand::X`/`Y` or
        /// the compiler's `Operand::Typed { reg: Reg::X|Reg::Y, .. }` spelling).
        value: Operand,
        /// The field width in bits — `1..=64`, MSB-first, unsigned, big-endian.
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
    /// The accumulator operand (an x- or y-register) — the term the build publishes
    /// unchanged.
    pub(crate) source: Operand,
    /// The first segment's append **unit** (`1`, `2`, `4` or `8`). The run-time doors
    /// take it because C gates the append *position* on it
    /// ([`append_unit_allows`]).
    pub(crate) unit: usize,
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
    // Only a unit that divides 8 is accepted here: the run-time doors can apply C's
    // position gate for exactly these, and every one of them is a unit the compiler emits
    // for the accumulator (`unit` 1 on an integer accumulator, 8 on a byte one).
    if !matches!(*unit, 1 | 2 | 4 | 8) {
        return None;
    }
    let unit = *unit;
    let source = locatable_operand(value)?;
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
                    value: locatable_operand(value)?,
                    bits: u32::from(*bytes) * 8,
                }
            }
            // Any other static width (1..=64 bits, whole-byte or bit-granular): accepted
            // with no flag that changes the layout, because a `Bits` field is written
            // MSB-first as the value's low `nbits` bits either way.
            PreparedSeg::Integer {
                flags,
                value,
                width: Width::Imm(nbits),
            } => {
                if *flags & (BSF_LITTLE | BSF_SIGNED | BSF_NATIVE) != 0 {
                    return None;
                }
                if *nbits == 0 || *nbits > 64 {
                    return None;
                }
                Field::Bits {
                    value: locatable_operand(value)?,
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
        unit,
        tail_bits,
        fields,
    })
}

/// The **byte-aligned** run-time door: the payload base address and the window end (in
/// bits) a lowering may write `tail_bits` more bits at, or `None` when the accumulator
/// must take the family's own path.
///
/// `Some` exactly when the source is a writable off-heap refc binary this machine
/// allocated (`raw::writable_append_target`), its window end is **on a byte boundary**
/// (which also makes the append-unit gate vacuous for every unit
/// [`append_plan`] accepts — the module docs carry the argument) and its payload has room
/// for `tail_bits` more bits from that end.
///
/// This is the door a **whole-byte** field lowering uses: it addresses field `i` at
/// `base + (end_bits + offset_i) / 8`, which needs the end on a byte boundary. A shape
/// with a bit-granular field must use [`open_append_any`].
///
/// Allocates nothing, mutates nothing, safe to call speculatively.
///
/// Built for its tests only: the baseline JIT's aligned route proves the accumulator's term
/// shape inline and asks the machine for the capacity alone
/// ([`crate::jit::baseline::runtime::jit_append_cursor`]), so no production path calls this
/// one — it survives as the **reference answer** that door is pinned against.
#[cfg(test)]
pub(crate) fn open_append(
    machine: &Machine,
    src: Eterm,
    tail_bits: u32,
) -> Option<(usize, usize)> {
    let (base, capacity_bits, end_bits) = raw::writable_append_target(machine, src)?;
    if !append_unit_allows(end_bits, 8) {
        return None;
    }
    if end_bits.checked_add(tail_bits as usize)? > capacity_bits {
        return None;
    }
    Some((base as usize, end_bits))
}

/// The **general** run-time door: like [`open_append`], but for a window end at **any**
/// bit alignment, gated on the plan's own append unit ([`append_unit_allows`]) instead of
/// on byte alignment.
///
/// `Some` exactly when the source is a writable off-heap refc binary this machine
/// allocated, the window end satisfies C's append-position gate for `unit`
/// (`erts_bs_append_checked`, `erl_bits.c:1778-1784` — `unit == 8` requires
/// `end_bits % 8 == 0`, any other `unit > 1` requires `end_bits % unit == 0`, and
/// `unit <= 1` is ungated) and the payload has room for `tail_bits` more bits from that
/// end.
///
/// A lowering that inlines a **bit-granular** field pairs this door with
/// [`write_bit_field`]: the field's bits go in at `end_bits + offset` whatever that
/// position's alignment. A whole-byte lowering may use either door (they agree whenever
/// `unit` divides 8 and the end is byte-aligned).
///
/// Allocates nothing, mutates nothing, safe to call speculatively.
pub(crate) fn open_append_any(
    machine: &Machine,
    src: Eterm,
    tail_bits: u32,
    unit: usize,
) -> Option<(usize, usize)> {
    let (base, capacity_bits, end_bits) = raw::writable_append_target(machine, src)?;
    if !append_unit_allows(end_bits, unit) {
        return None;
    }
    if end_bits.checked_add(tail_bits as usize)? > capacity_bits {
        return None;
    }
    Some((base as usize, end_bits))
}

/// Write the low `nbits` bits of `value`, **MSB-first**, into the payload starting at
/// absolute bit `cur_bit` — the family's one bit-granular write rule, for a lowering that
/// inlines a [`Field::Bits`] of a width that is not a whole number of bytes.
///
/// # The rule
///
/// The bits are the value's low `nbits` bits (for a negative value the caller passes its
/// two's-complement `u64`, i.e. `small_value(value) as u64`, so "low `nbits` bits" **is**
/// the mask-and-truncate the family applies). A write that **enters** a payload byte
/// clears it first, a write at the accumulator's window end that lands inside a byte masks
/// the fresh low bits away, and each bit is OR-ed in at `7 - cur_bit % 8`. See
/// `raw::write_bit_field`, which is the same body `raw::InPlace::push_bit` runs — there is
/// exactly one definition of it in this node.
///
/// # Caller contract
///
/// * `base` is the payload base a run-time door returned ([`open_append_any`]'s first
///   element), `start_bit` is that door's second element (the accumulator's window end,
///   i.e. the build's first writable bit) and `cur_bit >= start_bit` is the absolute bit
///   this field starts at.
/// * **Every bit in `[cur_bit, cur_bit + nbits)` must already be verified inside the
///   payload's capacity** — the door's `tail_bits` room check is that verification, which
///   is why the door must be consulted for the whole plan before the first write. `nbits`
///   is at most 64.
/// * `value` is a **small integer**'s `small_value(word) as u64` (sign-extended, so a
///   negative value's low `nbits` bits are its two's complement); a **bignum** field is the
///   caller's own refusal, because the family routes one through
///   `BitWriter::push_wide_int`'s byte model, which this `u64` door does not model.
/// * The write is **re-execution idempotent**: writing the same `value` at the same
///   `cur_bit` produces the same bytes, so a lowering may re-execute a parked instruction.
///   (That holds for the whole plan re-executed from its first field; a field re-executed
///   on its own would rely on the bytes its predecessors wrote, so a lowering re-runs the
///   plan, never a suffix of it.)
/// * It reads and writes only the payload bytes the field spans: no term access, no
///   `Machine`, no allocation, no `Result`.
pub(crate) fn write_bit_field(
    base: usize,
    start_bit: usize,
    cur_bit: usize,
    value: u64,
    nbits: u32,
) {
    raw::write_bit_field(base as *mut u8, start_bit, cur_bit, value, nbits);
}

/// C's append-**position** unit gate (`erts_bs_append_checked`, `erl_bits.c:1778-1784`).
///
/// The writable arm tests the bit position the build appends at: `unit == 8` requires it
/// on a byte boundary and any other `unit > 1` requires it to be a multiple of the unit,
/// while `unit <= 1` is ungated. The family applies the same test on the append's source
/// size (`create/mod.rs`'s `unit != 1 && src_bits % unit != 0`), which for `start == 0` is
/// this very position — a door that skipped it would report a shape as writable that the
/// family raises `badarg` on.
fn append_unit_allows(position_bits: usize, unit: usize) -> bool {
    if unit <= 1 {
        return true;
    }
    if unit == 8 {
        position_bits % 8 == 0
    } else {
        position_bits % unit == 0
    }
}

/// The operand groups **after** the accumulator's own — one
/// `[Type, Index, Unit, Flags, Value, Size]` slice per later segment, aligned one for one
/// with the prepared segments it is paired with.
fn segment_groups(segments: &[Operand]) -> impl Iterator<Item = &[Operand]> {
    segments.chunks_exact(SEGMENT_OPERANDS).skip(1)
}

/// The operand itself when it addresses a register a lowering can bind without a
/// literal-area lookup — an **x** or a **y** register.
///
/// Both spellings the loader produces are accepted for each: the plain
/// [`Operand::X`]/[`Operand::Y`] and the compiler's `z,5` [`Operand::Typed`] form (which
/// is what a real accumulator loop carries, e.g. `{tr,{y,1},{t_bitstring,8,true}}` for an
/// accumulator `move`d into the frame before the loop fun's `call_last`).
///
/// This stays a **pure operand classification** and consults no `Machine`: whether a `y`
/// register is bindable is a run-time question (the caller's frame window must be
/// published for it), and answering it is the **tier's** job, not this static query's.
fn locatable_operand(operand: &Operand) -> Option<Operand> {
    match operand {
        Operand::X(_) | Operand::Y(_) => Some(operand.clone()),
        Operand::Typed {
            reg: Reg::X(_) | Reg::Y(_),
            ..
        } => Some(operand.clone()),
        _ => None,
    }
}
