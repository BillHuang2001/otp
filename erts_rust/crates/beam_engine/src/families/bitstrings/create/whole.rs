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

//! The **whole-byte `integer` accumulator** fast path of `bs_create_bin`.
//!
//! # The shape it serves
//!
//! Every binary comprehension and every `<<Acc/binary, …>>` accumulator loop compiles to
//! the *same* two-to-four group segment list
//! (`beam_ssa_codegen`'s `bs_args`), executed once per element:
//!
//! ```text
//! [private_append, 1, 8, nil, {tr,{x,1},{t_bs_matchable,1}}, all]
//! [integer,        2, 1, nil, {tr,{x,0},{t_integer,{0,255}}},  {integer,8}]
//! ```
//!
//! so [`super::assemble`]'s generic walk re-derived, on **every** execution, the segment
//! type atom's name (`&str` match), the unit ([`super::super::read_size`]), the flags
//! ([`super::super::decode_flags`]), the `all` test, the size
//! ([`super::size::segment_size`]), the `size * unit` product
//! ([`super::size::seg_nbits`]) and the integer classification
//! (`predicates::is_integer` + [`super::super::small_value`]) — for what is a
//! **compile-time-constant** segment descriptor.
//!
//! [`try_assemble`] resolves that descriptor **once per execution** with a handful of
//! operand/immediate compares ([`plan`]), then reads each field's value once and writes
//! the whole segment list with **one** byte copy: the fields are contiguous whole bytes,
//! so their concatenation is byte-identical to writing them one at a time.
//!
//! # The guard (all of it must hold, else the generic walk runs unchanged)
//!
//! * the segment list is a whole number of 6-operand `[Type, Index, Unit, Flags, Value,
//!   Size]` groups, at least two of them, with at most [`MAX_FIELDS`] `integer` groups
//!   after the first;
//! * the first group is `private_append`, names the size `all`, has no flags and a static
//!   unit of 1 or 8 bits;
//! * every following group is `integer` (the same type atom in all of them), has unit 1,
//!   no flags and a **static** `Operand::Integer` size that is a whole number of bytes in
//!   `8..=64` bits;
//! * the build is at its first segment ([`super::BitWriter::is_pristine`]);
//! * the accumulator operand reads as a bitstring whose bit count is a multiple of the
//!   append unit (`erts_bs_append_checked`, `erl_bits.c:1877`) that
//!   [`super::raw::writable_append_target`] resolves to an in-place payload;
//! * the in-place position is byte-aligned ([`super::BitWriter::bit_len`]).
//!
//! # The fallback
//!
//! Every deviation — a shape the guard rejects, a `read_value` decline, a **bignum** or
//! non-integer field value, a non-writable accumulator, a non-byte-aligned position —
//! answers `Ok(None)` *before anything is written*, and [`super::assemble`] runs its
//! generic loop from a pristine writer. So the fast path cannot change a single result:
//! the same bits, the same `fail` branch, and the generic walk's own typed errors.
//!
//! The one failure the fast path *does* own is the non-bitstring accumulator, because the
//! generic walk reports it (`BADARG`, `erl_bits.c:1744`) from the same
//! [`super::raw::heap_bits`] read, so the reason and the branch are identical.

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::Step;

use crate::families::bitstrings::{is_all, raw, small_value, BitWriter, Built};

use super::read_value;
use super::size::{fail_seg, SegReason};

/// One segment group's operand count — `[Type, Index, Unit, Flags, Value, Size]`.
const GROUP: usize = 6;

/// How many whole-byte `integer` groups one fast build handles. A longer segment list
/// (never emitted for an accumulator loop) takes the generic walk.
const MAX_FIELDS: usize = 8;

/// The resolved shape of a whole-byte accumulator build.
struct Plan<'s> {
    /// The accumulator group's **value** operand (the `bs_init_writable` term).
    accumulator: &'s Operand,
    /// The accumulator group's unit in bits (1 or 8): the source's whole bit count must be
    /// a multiple of it (`erts_bs_append_checked`, `erl_bits.c:1877`).
    unit: usize,
    /// `(value operand, field width in bits)` per `integer` group, in segment order.
    fields: [Option<(&'s Operand, usize)>; MAX_FIELDS],
    /// How many `fields` entries are live.
    count: usize,
}

/// Build the whole segment list in place when it is the compiler's whole-byte
/// `integer` accumulator shape; `Ok(None)` hands the build back to [`super::assemble`]'s
/// generic walk (having written nothing).
///
/// `master` is the walk's writer: the fast path requires it to be **pristine** (the build
/// is at its first segment), which is what makes the in-place attach equivalent to the
/// generic loop's `i == 0` arm.
#[allow(clippy::type_complexity)]
pub(super) fn try_assemble(
    master: &BitWriter,
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    segments: &[Operand],
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<std::result::Result<Built, Step>>> {
    if !master.is_pristine() {
        return Ok(None);
    }
    let Some(plan) = plan(module, segments) else {
        return Ok(None);
    };
    // The accumulator: a bitstring whose bit count is a multiple of the group's unit over
    // a payload this machine allocated for appending. `erts_bs_append_checked`
    // (`erl_bits.c:1744`) rejects a non-bitstring accumulator with `BADARG` — the same
    // reason and the same `fail` branch the generic walk reports from this very read.
    let source = read_value(machine, module, ctx, plan.accumulator)?;
    let Some((_, src_bits, _)) = raw::heap_bits(source) else {
        return fail_seg(machine, module, labels, fail, SegReason::BadArg).map(Some);
    };
    if plan.unit != 1 && src_bits % plan.unit != 0 {
        return Ok(None);
    }
    let Some((base, capacity_bits, end_bits)) = raw::writable_append_target(machine, source) else {
        return Ok(None);
    };
    let mut writer = BitWriter::in_place(raw::InPlace::new(base, capacity_bits, end_bits, source));
    if writer.bit_len() % 8 != 0 {
        // The build would start inside a byte: the generic walk's bit-exact writers are
        // the only sound route (this is the `<<Acc/binary, 1:3>>` shape).
        return Ok(None);
    }
    // Read every value BEFORE writing: a value the fast path cannot encode exactly (a
    // bignum, a non-integer) hands the build back to the generic walk, which re-reads the
    // same (immutable) terms — so no bit is ever written twice.
    let mut bytes = [0u8; MAX_FIELDS * 8];
    let mut written = 0usize;
    for field in &plan.fields[..plan.count] {
        let Some((operand, nbits)) = *field else {
            // `plan` fills `0..count`; refuse rather than guess if that ever changes.
            return Ok(None);
        };
        let value = read_value(machine, module, ctx, operand)?;
        let Some(raw_value) = small_value(value) else {
            return Ok(None);
        };
        let width = nbits / 8;
        let (start, end) = (written, written + width);
        // The field is the value's low `nbits` bits, MSB-first — exactly what
        // `BitWriter::push_uint` writes, and exactly the value's trailing big-endian bytes
        // (a negative small's `as u64` is its two's complement, so the low bits agree).
        bytes[start..end].copy_from_slice(&(raw_value as u64).to_be_bytes()[8 - width..]);
        written = end;
    }
    // ONE reservation (a shortfall grows the payload, never promotes on this path) and one
    // whole-byte copy for the entire segment list.
    writer.push_bits_from(&bytes[..written], 0, written * 8, machine)?;
    #[cfg(test)]
    FAST_BUILDS.with(|built| built.set(built.get() + 1));
    Ok(Some(Ok(writer.finish_built())))
}

// A **test-only** per-thread tally of the builds this fast path completed. The only way a
// test can tell the specialised route apart from the generic walk, whose bytes, term
// identity and failure modes are identical by construction. Thread-local so each test's
// count is exact even when the suite runs in parallel.
#[cfg(test)]
std::thread_local! {
    static FAST_BUILDS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// How many builds this thread's fast path has completed so far (see [`FAST_BUILDS`]).
#[cfg(test)]
pub(super) fn fast_builds() -> usize {
    FAST_BUILDS.with(core::cell::Cell::get)
}

/// Resolve the segment list into a [`Plan`], or `None` when it is not the whole-byte
/// `integer` accumulator shape.
///
/// The field groups must all name the **same** type atom, so the atom name is resolved
/// twice per execution in total (`private_append` for the accumulator group, `integer` for
/// the fields) instead of once per group — the group-to-group check is an integer compare
/// on the operand.
fn plan<'s>(module: &Module, segments: &'s [Operand]) -> Option<Plan<'s>> {
    if segments.len() % GROUP != 0 {
        return None;
    }
    let groups = segments.len() / GROUP;
    if groups < 2 || groups - 1 > MAX_FIELDS {
        return None;
    }
    // Group 0: the compiler's accumulator append.
    let Operand::Atom(append_atom) = &segments[0] else {
        return None;
    };
    if module.atom_name(*append_atom) != Some("private_append") {
        return None;
    }
    if !matches!(segments[3], Operand::Nil) || !is_all(module, &segments[5]) {
        return None;
    }
    let unit = match segments[2] {
        Operand::Integer(1) => 1,
        Operand::Integer(8) => 8,
        _ => return None,
    };
    // Groups 1..: whole-byte `integer` fields, all naming the same type atom.
    let Operand::Atom(field_atom) = &segments[GROUP] else {
        return None;
    };
    if module.atom_name(*field_atom) != Some("integer") {
        return None;
    }
    let mut plan = Plan {
        accumulator: &segments[4],
        unit,
        fields: [None; MAX_FIELDS],
        count: 0,
    };
    for group in 1..groups {
        let at = group * GROUP;
        if segments[at] != Operand::Atom(*field_atom) {
            return None;
        }
        if segments[at + 2] != Operand::Integer(1) || !matches!(segments[at + 3], Operand::Nil) {
            return None;
        }
        plan.fields[plan.count] = Some((&segments[at + 4], whole_byte_width(&segments[at + 5])?));
        plan.count += 1;
    }
    Some(plan)
}

/// A static segment size that is a whole number of bytes in `8..=64` bits.
///
/// Anything else (a register, `all`, a 4-bit width, a literal) declines: the generic walk
/// reads those, and its own `bs_create_bin: integer segment size 'all'` / `system_limit`
/// errors stay exactly as they were.
fn whole_byte_width(operand: &Operand) -> Option<usize> {
    match operand {
        Operand::Integer(nbits) if (8..=64).contains(nbits) && nbits % 8 == 0 => {
            Some(*nbits as usize)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
