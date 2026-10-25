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

//! The `bs_create_bin` shape: the compiler's **linear accumulator** build, lowered in
//! place, on one of **three routes** — aligned, bit-granular, or the `binary all` tail
//! ([`create_binary`](create_binary)).
//!
//! `bs_init_writable` followed by a `bs_create_bin` whose first segment is a
//! `private_append` of the accumulator itself and whose remaining segments are
//! statically sized fields is the binary-construction hot loop of every encoder (a
//! base62/UTF-8/JSON writer, `io_lib`, `erl_prettypr`, …): one instruction per appended
//! field, each executed once per output byte. The family's own route reserves a room
//! check, walks the prepared program, writes the field bytes and advances the
//! accumulator's window end; the two facts a tier cannot derive itself are the
//! [construction seam](crate::families::bitstrings::create::seam)'s —
//! [`seam::append_plan`] (this instruction *is* that shape, and here are its fields) and
//! the append doors (the accumulator has room, and here is where its window starts).
//!
//! # Three routes, one predicate
//!
//! [`CreateBinShape::route`] — derived from the same [`CreateBinShape::of`] predicate every
//! other consumer already calls, so no second predicate exists — splits the accepted shapes
//! that carry static fields by **bit alignment**, and [`CreateBinShape::of`] reports the
//! third route ahead of that split: a build whose only segment is a whole `binary all` tail
//! has no field at all, so it is branched on **before** [`CreateBinShape::route`] (which
//! answers `Route::Aligned` for it vacuously). The three emitted sequences are therefore:
//!
//! * [`Route::Aligned`] — every field is a whole number of bytes. Field `i` is addressed as
//!   `cursor + byte_offset` from the byte cursor [`create_door::emit_append_cursor`] leaves
//!   in `rax` — the accumulator's shape proved **inline**, then one
//!   [`jit_append_cursor`](crate::jit::baseline::runtime::jit_append_cursor) call for the
//!   machine-private half — and its bytes are extracted with an arithmetic shift (see *The
//!   byte extraction is exact* below).
//! * [`Route::BitGranular`] — at least one field is **not** a whole number of bytes (the
//!   `integer 6` of a base62 decoder's per-character append, say). Each field is written by
//!   one call to [`jit_write_bit_field`], which re-derives the payload base and the window
//!   end from the general door ([`seam::open_append_any`]) and applies the family's one
//!   bit-granular write rule ([`seam::write_bit_field`]) at an **absolute bit** position.
//! * the **`binary all` tail** — one [`create_binary::emit_binary_all`] call that performs
//!   the room check, the copy and the build's single `end` commit itself (the advance is the
//!   source's run-time width, so there is no compile-time tail to add).
//!
//! The two field-write routes share the destination/accumulator/`is_small` guards and the
//! closing `end` commit + destination store, so a shape that moves from one of them to the
//! other changes only the field-write phase; the `binary all` tail keeps the destination
//! and source guards and folds its commit into its one helper call.
//!
//! # The mutation order is the whole correctness argument
//!
//! The family's build performs exactly one mutation beyond the field bytes: C's
//! `sb->end = new_position` (`erl_bits.c:1975`), i.e. `ErlSubBits.end += tail_bits`. A
//! lowering that committed that word **before** the field stores would make the
//! `fallback` (one whole interpreter step re-executing *this* instruction) append the
//! tail a second time — the window would already cover the bytes. So the emitted
//! sequence is:
//!
//! ```text
//!   <the destination's Y-window guard>             ; a rejected destination writes nothing
//!   <the accumulator's Y-window guard, if Y>       ; guard only, still before every write
//!   <one guarded load + is_small guard per Field::Bits>  ; a non-integer field is the family's badarg
//!   <the append door, by route>                     ; a refusal is the family's path
//!     Aligned:     <the inline refusal-only shape proof, then one jit_append_cursor call>
//!                  <test rax, rax ; je fallback>     ; the LAST guard, still before every write
//!     BitGranular: <one positioned jit_open_append_bin_any[_y] call>
//!                  <test rax, rax ; je fallback>     ; the LAST guard, still before every write
//!   <the field writes>                             ; the only payload writes
//!   <the accumulator re-read>                       ; re-read: the helper clobbered every scratch
//!   mov rcx, [boxed_val(rdx) + SUB_BITS_END_DISP]   ; the old window end
//!   add rcx, tail_bits ; mov [block + END], rcx     ; THE ONLY MUTATION, last
//!   <the accumulator re-read into Dst>              ; Dst's Y address re-derived too
//!   jmp fast_done
//! fallback:
//!   <one interpreter step>                          ; re-reads an untouched accumulator
//! fast_done:
//! ```
//!
//! `<the field writes>` is the route's own phase:
//!
//! ```text
//!   Route::Aligned      <a byte store per field byte at cursor + byte_offset>
//!   Route::BitGranular  <per field, in order>
//!                         jit_write_bit_field(src_slot, bit_offset, value_slot, nbits, flags)
//!                         test rax, rax ; je fallback      ; a refusal is the family's path
//! ```
//!
//! [`jit_append_cursor`](crate::jit::baseline::runtime::jit_append_cursor) (the aligned
//! route's machine-private half, a read-only `&Machine` query) and the general doors
//! `jit_open_append_bin_any` / `_any_y` themselves **mutate nothing** (they are `&Machine`
//! throughout), and the aligned route's inline proof stores nothing either, so every guard
//! that can reach `fallback` runs against the machine state the interpreter step will see.
//! The payload bytes are the one
//! thing written before the `end` commit, and they are written at the same offsets with the
//! same values on every attempt, so even a deopt after them re-produces them — never a
//! duplicated field.
//!
//! # The `Y` forms: the accumulator's slot, and why the re-reads need no second guard
//!
//! Where a helper needs an operand the block cannot compute, the block ABI makes it a slot
//! **index**: `emit_helper_invoke` adds every argument as a `movabs` immediate, so the
//! bit-granular door takes a `Y` accumulator as its slot index and reads it through
//! `Machine::y`'s bounds check, and `jit_write_bit_field` takes a `Y` field value the same
//! way. The aligned route needs no index at all — [`create_door::emit_append_cursor`] loads
//! the term out of the slot itself (`emit_y_slot_value`) and passes the door the payload base
//! and the window end it read out of that term, with only the compile-time `tail_bits` as an
//! immediate.
//! The emitted code re-reads a `Y` slot **directly** (`y_base + k*8`) after the
//! call, because the helper clobbers every caller-saved register and the window-end commit
//! and the `Dst` store both need the term again. That direct read is unguarded, and the
//! **once-up-front guard** is what makes it sound: the frame cannot change between the
//! guard and the read (the helper only reads machine state and runs no interpreter step),
//! so a slot inside the published window at the guard is still inside it later — exactly
//! the argument [`super::Loc::store_at`] already relies on for a `Y` destination.
//!
//! Every post-helper re-read is therefore four instructions into a caller-saved scratch
//! (`mov scratch,[rsp+RUNTIME_SLOT_DISP]` ; `mov scratch,[scratch+RUNTIME_YBASE_DISP]` ;
//! `lea scratch,[scratch+y_offset(k)]` ; `mov dst,[scratch]`) and nothing more.
//!
//! # The argument channel: slot indices where a helper needs one
//!
//! Neither route can hand a helper a **word** it must interpret as a term.
//! [`emit_positioned_helper_call`] admits at
//! most five arguments and materialises each as a `movabs` immediate, which is why the
//! helpers here take either slot indices or a value the block has already proved: the
//! bit-granular door takes the accumulator as an x-register or `Y`-slot
//! index, and `jit_write_bit_field` takes the field value the same way plus one flags word
//! ([`WRITE_ACC_IN_Y`] / [`WRITE_VALUE_IN_Y`]) naming which machine file holds each. The
//! block's spill before every helper call makes `machine.x` authoritative, and a `Y` slot is
//! reached through `Machine::y`, so an index is all such a helper needs.
//! The aligned route's call is the exception the block can afford: it passes the payload base
//! and the window end it has just loaded from the accumulator's own block, in the ABI's own
//! argument registers (`rsi`/`rdx`), and writes only the plan's `tail_bits` as an immediate —
//! a payload pointer the block has already proved is not a term the helper must re-derive.
//!
//! # The general route re-derives the door per field, and why that is inexpensive
//!
//! A bit-granular field is written at an **absolute bit**, so there is no byte cursor to
//! carry: [`jit_write_bit_field`] asks the general door for this field's own
//! `field_offset_bits + nbits`, which both checks the field's room inside the payload (the
//! seam's caller contract for `write_bit_field`) and answers the `(base, end_bits)` pair.
//! The door is pure and allocation-free, so re-consulting it per field costs a few compares
//! on the **write** path only — and it keeps the emitted code free of any run-time word,
//! which the block ABI could not carry anyway.
//!
//! The accumulator's window start is always 0 (`raw::writable_append_target` rejects a
//! non-zero `start`), so the door's `end_bits` **is** the build's first writable bit: passing
//! it as `write_bit_field`'s `start_bit` reproduces `raw::InPlace::new`'s `start = end_bits`,
//! which is what makes the write's `cur == start` mask rule preserve the high bits of a byte
//! a **previous** build already partly filled. `cur_bit` is the absolute bit
//! `end_bits + field_offset_bits`.
//!
//! # The byte extraction is exact, not approximate (the aligned route)
//!
//! A whole-byte big-endian integer field of `nbytes` bytes is the value's low
//! `8 * nbytes` bits, most significant byte first: `BitWriter::push_uint`'s whole-byte
//! path writes `(value as u64).to_be_bytes()[8 - nbytes..]` and `push_byte_field` writes
//! the same bytes through `to_be_bytes`. A small int's word is
//! `((value as u64) << 4) | TAG_IMMED1_SMALL`, so byte `i` (MSB first) is bit
//! `4 + 8 * (nbytes - 1 - i)` of that word — but the family never hands `push_byte_field`
//! that word: it hands it `small_value(field) as u64` (the 60-bit two's-complement value
//! **sign-extended** to the whole 64 bits), so the bytes to reproduce are the sign-extended
//! value's, not the zero-extended one's.
//!
//! The extraction is therefore an **arithmetic** (`sar`) shift right by that count, then a
//! **byte** store: the raw word's bit 63 *is* the small value's sign bit (its bit 59), so
//! `sar` fills the vacated high bits with that sign and the stored low byte is byte `i` of
//! `(value as i64 as u64).to_be_bytes()` — `<<-1:64>>` is `FF`×8, an 8-byte field of the
//! small `-0x0123_4567_89AB_CDEF` is `FE DC BA 98 76 54 32 11`, `<<-1:16>>` is `FF FF` and
//! an 8-bit field of `0x1FF` truncates to `FF` (`push_uint` masks nothing).
//! A **logical** (`shr`) shift would be right only for widths of ≤ 7 bytes: an 8-byte
//! field's top byte has the shift count `60`, so the zero fill reaches the stored low byte
//! and `-1` would be written as `0F FF FF FF FF FF FF FF`. For a **non-negative** field the
//! raw word's bit 63 is 0, so `sar` and `shr` agree byte for byte — which is why the
//! arithmetic shift is unconditionally the right instruction. The shift count peaks at
//! `4 + 8 * 7 = 60`, inside the encoding's one-byte immediate and below the 64-bit width.
//!
//! The general route needs none of that: [`seam::write_bit_field`] is the family's own
//! rule, i.e. bit-for-bit what `BitWriter` writes, so a bit-granular field cannot disagree
//! with the interpreter about a byte.
//!
//! # What is declined at compile time
//!
//! [`CreateBinShape::of`] refuses a shape either sequence cannot reproduce exactly, and
//! `emit_block`'s `_ if bitstrings::claimed(…)` arm keeps handling it:
//!
//! * anything [`seam::append_plan`] answers `None` for — a plain or later-position
//!   append, an append `unit` outside `{1, 2, 4, 8}` (the units the run-time doors can
//!   position-gate), a `binary`/`float`/`utf*` segment, an `all` or runtime (`Dynamic`)
//!   size, a `little`/`signed`/`native` flag, a field of zero width or wider than a
//!   machine word, a `StrT` window the module cannot resolve, a plan with no fields;
//! * an accumulator or a field **value** operand that is neither an in-bound x-register nor
//!   an addressable `Y` slot ([`super::Loc::of`] — a literal-area operand, an out-of-bound
//!   register index, a `Y` index the emitted `lea`'s `disp32` could not address); both
//!   kinds the tier *can* address are lowered, and [`super::Loc::of`] is the one place that
//!   decision lives;
//! * a field width outside `1..=64` bits — the aligned route emits whole bytes, and the
//!   general route's write rule models at most a machine word;
//! * a [`seam::Field::ConstBytes`] field on the **general** route — a constant copy at a
//!   non-byte-aligned position has no straight-line form here (its bytes would have to be
//!   shifted into the neighbouring ones), and inventing a per-byte helper-call form for it
//!   would put a call on a path this lowering exists to keep call-free. Such a mixed plan
//!   is the family's own loop, unchanged;
//! * more than [`MAX_FIELDS`] fields or more than [`MAX_TAIL_BYTES`] tail bytes — the
//!   emitted sequence is straight-line code, so an unbounded segment list would be an
//!   unbounded instruction (the family's own loop is the right home for a very wide one);
//! * a field list whose bit offsets do not sum to the plan's `tail_bits` — the
//!   `end` commit would then publish a window over bits that were never written.
//!
//! Only the **compile-time** half of the append-unit rule is settled above. The other half
//! is the run-time door's, on whichever route the plan's widths picked: the door applies C's
//! append-**position** gate (`seam::append_unit_allows`) to the accumulator's window end —
//! `unit == 8` needs `end_bits % 8 == 0`, any other `unit > 1` needs `end_bits % unit == 0`,
//! and `unit <= 1` is ungated — so a window end the plan's own unit does not accept is that
//! door's `0` refusal, i.e. the same one interpreter step every compile-time refusal above
//! takes. The general door [`jit_open_append_bin_any`] / [`jit_open_append_bin_any_y`] is
//! handed the plan's `unit` for exactly this test; the aligned route
//! ([`create_door::emit_append_cursor`]) refuses unless `end_bits % 8 == 0` — the `unit == 8`
//! form of the same gate, because a byte cursor needs a byte-aligned window end — and
//! [`jit_append_cursor`](crate::jit::baseline::runtime::jit_append_cursor) re-checks it so the
//! machine-private half agrees with the seam on every input.
//!
//! The `fail` arm is deliberately **not** part of this shape: `bs_create_bin`'s
//! `NO_FAIL`-vs-label discrimination stays in the family, and every refusal above (and
//! the helpers') is one interpreter step, so the family's own value, fail transfer or
//! raise is what the process observes.

use beam_common::Result;
use beam_loader::{Module, Operand};

use crate::families::bitstrings::create::seam;
use crate::jit::emitter::{CodeBuffer, Cond, Label as CodeLabel, Reg};

use super::super::super::runtime::{
    jit_open_append_bin_any, jit_open_append_bin_any_y, jit_write_bit_field, OpenAppendAnyHelper,
    WriteBitFieldHelper, RUNTIME_YBASE_DISP, WRITE_ACC_IN_Y, WRITE_VALUE_IN_Y,
};
use super::super::operands::{x_offset, y_offset, Loc, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2};
use super::super::{
    emit_positioned_helper_call, touch, Allocation, MOVE_SCRATCH, MOVE_SCRATCH_2, RUNTIME_SLOT_DISP,
};
use super::{emit_fallback_and_done, IMMED1_MASK, POINTER_MASK, SMALL_TAG, SUB_BITS_END_DISP};

#[cfg(test)]
use super::{record_inlined, SHAPE_CREATE_BIN};

#[path = "create_field.rs"]
mod create_field;
#[path = "create_door.rs"]
mod create_door;
#[path = "create_binary.rs"]
mod create_binary;

use create_binary::emit_binary_all;
use create_field::{
    emit_bit_field_store, FieldScratch, MAX_INLINE_FIELD_BITS, PRODUCTION_FIELD_SCRATCH,
};
use create_door::emit_append_cursor;

// The gold-standard oracle for `emit_bit_field_store`: the sequence is executed for every
// width, window offset and window end over a byte-pre-filled payload and compared with the
// family's own bit-granular write rule (`create_field_store_tests.rs`).
#[cfg(test)]
#[path = "create_field_store_tests.rs"]
mod field_store_tests;
// How many field-write plans the general route has emitted through the **inline** store
// path (see `emit_bit_fields_inline`).
//
// A `#[cfg(test)]` non-vacuity probe: a row that only compares values cannot tell an
// inline plan from one that silently stayed on the per-field helper, so the end-to-end
// tier-parity tests assert this counter moved.
// Thread-local for the same reason `super::INLINED` is: the test harness runs the crate's
// tests on parallel threads of one process, and a process-global counter would let a
// neighbouring test's compile account for this one's.
#[cfg(test)]
thread_local! {
    static INLINE_FIELD_PLANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Count one plan emitted through the inline store path (see [`INLINE_FIELD_PLANS`]).
#[cfg(test)]
fn note_inline_field_plan() {
    INLINE_FIELD_PLANS.with(|plans| plans.set(plans.get() + 1));
}

/// The most fields one natively lowered build writes.
///
/// The emitted sequence is straight-line code (one guarded byte store per field byte on the
/// aligned route, one helper call per field on the general one), so the cap is what bounds
/// both the block's size and the `CREATE_BIN_PATH_BYTES` budget in `emit/compile.rs`. A wider
/// segment list is the family's own loop, which is exactly the shape an unbounded list wants.
const MAX_FIELDS: usize = 16;

/// The widest tail one natively lowered build appends, in **bytes** — the cap bounds
/// `ceil(tail_bits / 8)`, so a build whose fields sum to 6 bits costs one of these, not zero.
///
/// A `StrT` string segment can be arbitrarily long, and each byte of it is one emitted
/// store (aligned route) or one bit-granular write, so the tail is capped here; a longer
/// constant is the family's own copy.
const MAX_TAIL_BYTES: usize = 256;

/// [`MAX_TAIL_BYTES`] in bits, the form the plan's `tail_bits` is compared against.
const MAX_TAIL_BITS: u32 = MAX_TAIL_BYTES as u32 * 8;

/// The scratch the `Y` accumulator's up-front guard computes its slot address in, and the
/// scratch its window bound goes to.
///
/// Both must be distinct from every other scratch the sequence holds live across the guard
/// phase — [`Y_ADDR_SCRATCH`]/[`Y_ADDR_SCRATCH_2`] carry the *destination*'s address and
/// bound — and neither is a cache register nor the block's reserved `rbx`/`rsp`. The value
/// the guard computes is deliberately not kept: the helper re-reads the accumulator through
/// `Machine::y`, and every later use re-derives the address behind this guard.
const SOURCE_GUARD_SCRATCH: Reg = MOVE_SCRATCH;
/// The `Y` accumulator guard's window-bound scratch (see [`SOURCE_GUARD_SCRATCH`]).
const SOURCE_GUARD_END: Reg = MOVE_SCRATCH_2;

/// Which of the two straight-line sequences a shape is emitted with (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// Every field is a whole number of bytes: field bytes are stored at
    /// `cursor + byte_offset` from the byte cursor the append door returns.
    Aligned,
    /// At least one field is not a whole number of bytes: every field goes through the
    /// seam's bit-granular write rule at an absolute bit position.
    BitGranular,
}

/// One field of a lowered build, fully resolved for emission: the value's location and the
/// compile-time **bit** offset from the accumulator's window end.
#[derive(Clone, Debug)]
enum PreparedField<'a> {
    /// A run-time value, written MSB-first as `bits` bits.
    Bits {
        /// The value's location — an in-bound x-register or an addressable `Y` slot (see
        /// [`CreateBinShape::of`]), used for the `is_small` guard's read and, on the
        /// aligned route, for the post-helper re-read every byte store needs.
        value: Loc,
        /// The field width in bits — `1..=64`, not necessarily a whole number of bytes.
        bits: u32,
        /// The field's **bit** offset from the accumulator's window end.
        offset_bits: u32,
    },
    /// Compile-time constant bytes (a `string` segment's `StrT` contents, already copied
    /// out of the module — a cached module's string area may be a different allocation, so
    /// no table offset is ever baked into code). Only ever emitted on the aligned route.
    Const {
        /// The literal's bytes, in order.
        bytes: &'a [u8],
        /// The field's **bit** offset from the accumulator's window end (a whole number of
        /// bytes by construction).
        offset_bits: u32,
    },
}

/// The operands of a `bs_create_bin` the tier lowers natively: the in-place accumulator,
/// the destination, and the statically resolved fields with their bit offsets from the
/// accumulator's window end.
#[derive(Clone, Debug)]
pub(crate) struct CreateBinShape {
    /// The accumulator operand's location: an in-bound x-register or an addressable `Y`
    /// slot (the emitted call's door follows from which — see the module docs).
    source: Loc,
    /// The destination: an x-register or an addressable `Y` slot.
    dst: Loc,
    /// The plan's append **unit** (`1`, `2`, `4` or `8`) — the general door's position
    /// gate (`seam::open_append_any`'s fourth argument). Carried as a compile-time
    /// constant because it is the plan's, not the instruction's.
    unit: usize,
    /// The summed width of the fields, in bits: exactly the figure the accumulator's
    /// window end advances by (`seam::AppendPlan::tail_bits`).
    tail_bits: u32,
    /// The fields, in instruction order.
    fields: Vec<seam::Field>,
    /// The **bit** offset of each field from the write start, parallel to [`Self::fields`]
    /// — the one offset figure both routes derive from (the aligned route divides by 8).
    field_bit_offsets: Vec<u32>,
    /// The plan's `binary all` source, when the build is an accumulator with one
    /// whole-binary tail (`seam::AppendPlan::binary_all`). Such a shape has **no** static
    /// fields (the seam accepts the segment only as the last and only one), so its whole
    /// emitted sequence is [`create_binary::emit_binary_all`]'s one helper call; it is
    /// branched on **before** [`Self::route`], which reports `Route::Aligned` for it
    /// vacuously (`tail_bits == 0`, no fields).
    binary_all: Option<Loc>,
}

/// The x-register a [`seam::Field::Bits`] value operand names; `None` for a constant
/// string field or a `Y`-slot value (a stack slot is never a cached x-register).
///
/// [`CreateBinShape::of`] accepts a shape only when every field value resolves to a
/// [`Loc`], so a `None` for an *addressable* `Bits` field means a `Y` slot.
fn bits_register(field: &seam::Field) -> Option<u32> {
    match field {
        seam::Field::Bits { value, .. } => Loc::of(value).and_then(Loc::register),
        seam::Field::ConstBytes { .. } => None,
    }
}

/// The plan's fields, resolved for emission, or `None` when a read site does not resolve.
///
/// Runs **before** the first emitted byte: a shape whose field value stopped resolving
/// must emit nothing at all, because a partial sequence could not be abandoned without
/// having written into the accumulator's payload.
fn prepare_fields(shape: &CreateBinShape) -> Option<Vec<PreparedField<'_>>> {
    let mut prepared = Vec::with_capacity(shape.fields.len());
    for (field, offset_bits) in shape.fields.iter().zip(&shape.field_bit_offsets) {
        prepared.push(match field {
            seam::Field::Bits { value, bits } => PreparedField::Bits {
                value: Loc::of(value)?,
                bits: *bits,
                offset_bits: *offset_bits,
            },
            seam::Field::ConstBytes { bytes } => PreparedField::Const {
                bytes,
                offset_bits: *offset_bits,
            },
        });
    }
    Some(prepared)
}

impl CreateBinShape {
    /// The inlinable shape of a `bs_create_bin`, or `None` when the whole instruction must
    /// stay on the family.
    ///
    /// Purely syntactic and **label-free** (it takes no `LabelMap` and no fail label), so
    /// [`super::super::regalloc`]'s four mirrors, [`super::super::arith::uses_y`], the
    /// emitter and the refusal census all derive the same answer from this one call — which
    /// is also why the route ([`Self::route`]) is a function of the shape rather than a
    /// second predicate.
    pub(crate) fn of(
        module: &Module,
        dst: &Operand,
        segments: &[Operand],
    ) -> Option<CreateBinShape> {
        let plan = seam::append_plan(module, segments)?;
        // An in-bound x-register or an addressable `Y` slot: the `Y` forms carry the
        // accumulator to the helper as a slot index and re-derive the slot's address
        // post-helper behind the one up-front guard (see the module docs). Every other
        // operand kind — a literal-area operand, an out-of-bound index, a `Y` index the
        // `disp32` cannot address — is refused here, by the one shared predicate.
        let source = Loc::of(&plan.source)?;
        let dst = Loc::of(dst)?;
        // The plan's `binary all` tail, if it has one: the source is read by the one append
        // helper through its slot index, so it must resolve exactly like the accumulator — a
        // literal-area or out-of-bound operand refuses the shape here, before a byte is
        // emitted. A `binary_all` plan carries no static fields (the seam accepts the segment
        // only as the last and only one), so the field caps below are vacuous for it.
        let binary_all = match &plan.binary_all {
            Some(value) => Some(Loc::of(value)?),
            None => None,
        };
        // The two shape caps (see [`MAX_FIELDS`] / [`MAX_TAIL_BYTES`]).
        if plan.fields.len() > MAX_FIELDS {
            return None;
        }
        if plan.tail_bits > MAX_TAIL_BITS {
            return None;
        }
        let mut field_bit_offsets = Vec::with_capacity(plan.fields.len());
        let mut offset_bits: u32 = 0;
        for field in &plan.fields {
            let bits = match field {
                seam::Field::Bits { value, bits } => {
                    // A bit-granular width is accepted (`1..=64`); only a zero width and a
                    // width past a machine word are refused — `write_bit_field` models a
                    // `u64`, and the aligned route emits whole bytes only.
                    if *bits == 0 || *bits > 64 {
                        return None;
                    }
                    // An addressable `Y` value is lowered too: its guard is the `Loc::load`
                    // below, and its (byte-store or bit-field) write re-reads the slot.
                    Loc::of(value)?;
                    *bits
                }
                // A `StrT` window is a whole number of bytes (`prepare` resolves it from a
                // byte offset and a byte count).
                seam::Field::ConstBytes { bytes } => {
                    u32::try_from(bytes.len()).ok()?.checked_mul(8)?
                }
            };
            field_bit_offsets.push(offset_bits);
            // `offset_bits` stays inside [`MAX_TAIL_BITS`] (`2048`), so the aligned route's
            // `disp32` byte displacement (`offset_bits / 8 <= 256`) is addressable and no
            // further bound is needed.
            offset_bits = offset_bits.checked_add(bits)?;
        }
        // The running offset must land exactly on the window advance: a mismatch means the
        // field list and the tail width disagree (see the module docs).
        if offset_bits != plan.tail_bits {
            return None;
        }
        let shape = CreateBinShape {
            source,
            dst,
            unit: plan.unit,
            tail_bits: plan.tail_bits,
            fields: plan.fields,
            field_bit_offsets,
            binary_all,
        };
        // The general route's one structural refusal: a constant copy at a non-byte-aligned
        // position has no straight-line form (see the module docs), so a mixed plan stays on
        // the family rather than inventing a per-byte helper-call form.
        if shape.route() == Route::BitGranular
            && shape
                .fields
                .iter()
                .any(|field| matches!(field, seam::Field::ConstBytes { .. }))
        {
            return None;
        }
        Some(shape)
    }

    /// Which of the two straight-line sequences this shape is emitted with (see the module
    /// docs).
    ///
    /// `Route::Aligned` exactly when every field is a whole number of bytes, i.e. when the
    /// aligned route's `cursor + byte_offset` addressing is sound. The `tail_bits % 8 == 0`
    /// half of the test follows from the fields' own widths (the plan's `tail_bits` is their
    /// sum, and [`Self::of`] verifies the offsets sum to it); it is kept as belt and braces,
    /// because a plan whose tail width disagreed with its fields would otherwise publish a
    /// window over bits nobody wrote.
    pub(crate) fn route(&self) -> Route {
        let aligned = self.tail_bits % 8 == 0
            && self.fields.iter().all(|field| match field {
                seam::Field::Bits { bits, .. } => bits % 8 == 0,
                // A `StrT` window is whole bytes by construction.
                seam::Field::ConstBytes { .. } => true,
            });
        if aligned {
            Route::Aligned
        } else {
            Route::BitGranular
        }
    }

    /// The accumulator's **slot index**, which is how it crosses to a door helper (an
    /// x-register index or a `Y`-slot index — see the module docs).
    fn source_slot(&self) -> u32 {
        match self.source {
            Loc::X(x) => x,
            Loc::Y(k) => k,
        }
    }

    /// The x-register indices the shape touches — the accumulator, every field value and the
    /// `binary all` source (a `Y` slot contributes nothing: it is a stack slot, never a
    /// cached x-register).
    pub(crate) fn registers(&self) -> Vec<u32> {
        let mut registers = Vec::with_capacity(2 + self.fields.len());
        registers.extend(self.source.register());
        registers.extend(self.binary_all.and_then(Loc::register));
        registers.extend(self.fields.iter().filter_map(bits_register));
        registers
    }

    /// The x-register indices the shape **reads** — the accumulator, every field value and the
    /// `binary all` source.
    ///
    /// The whole read set: the accumulator is read again for the window commit and every
    /// field value again for its own byte stores, so a missed entry load here would be a
    /// stale read, not merely a redundant one.
    pub(crate) fn reads(&self) -> Vec<u32> {
        self.registers()
    }

    /// The x-register indices the shape **writes** — the destination, and nothing else (a
    /// `Y` destination contributes nothing).
    ///
    /// Charging it is mandatory: a dropped write lets a stale cache register survive a
    /// block exit, the one unsound failure mode of the register cache.
    pub(crate) fn writes(&self) -> Vec<u32> {
        self.dst.register().into_iter().collect()
    }

    /// Whether the lowering accesses the current frame's Y window — a `Y` destination, a
    /// `Y` accumulator, a `Y` field value or a `Y` `binary all` source.
    pub(crate) fn uses_y(&self) -> bool {
        self.source.is_y()
            || self.dst.is_y()
            || self.binary_all.is_some_and(Loc::is_y)
            || self.fields.iter().any(|field| match field {
                seam::Field::Bits { value, .. } => Loc::of(value).is_some_and(Loc::is_y),
                seam::Field::ConstBytes { .. } => false,
            })
    }

    /// How many field-write plans this thread's compiles have emitted through the
    /// **inline** general-route store path (see [`INLINE_FIELD_PLANS`]).
    ///
    /// The non-vacuity probe the end-to-end tests read: it is reachable from a sibling test
    /// module through this type because `create` is private to `bitstr_inline` while
    /// `CreateBinShape` is re-exported from it.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn inline_field_plans() -> usize {
        INLINE_FIELD_PLANS.with(std::cell::Cell::get)
    }
}

/// Emit `bs_create_bin Dst Segments` natively, or report that the block must helper it
/// (`Ok(false)` with **zero bytes emitted**).
///
/// See the module docs for the three emitted sequences (the aligned and bit-granular
/// field-write routes and the `binary all` tail), the mutation order they are built on
/// and the shapes [`CreateBinShape::of`] declines.
#[allow(clippy::too_many_arguments)]
pub(in crate::jit::baseline::emit) fn try_inline_bs_create_bin(
    buffer: &mut CodeBuffer,
    module: &Module,
    dst: &Operand,
    segments: &[Operand],
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    highest_touched: &mut Option<u32>,
) -> Result<bool> {
    let Some(shape) = CreateBinShape::of(module, dst, segments) else {
        return Ok(false);
    };
    let Some(prepared) = prepare_fields(&shape) else {
        return Ok(false);
    };
    let route = shape.route();
    // The general route's structural precondition, re-checked **before** the first emitted
    // byte: [`CreateBinShape::of`] refuses a `ConstBytes` field there, so the sequence below
    // sees `Bits` fields only. A future field kind that slipped through would emit zero bytes
    // and stay on the family rather than be silently dropped from the field list.
    if route == Route::BitGranular
        && prepared
            .iter()
            .any(|field| !matches!(field, PreparedField::Bits { .. }))
    {
        return Ok(false);
    }
    #[cfg(test)]
    record_inlined(SHAPE_CREATE_BIN);

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // (1) The destination's frame-window guard, so a rejected slot writes nothing at all.
    shape
        .dst
        .guard_store(buffer, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)?;

    // (1b) The accumulator's frame-window guard, for the `Y` form: guard **only** — the
    // helper reaches the term through `Machine::y` itself — emitted here so the post-helper
    // unguarded re-reads of that slot (sections 4-6) are behind a proven in-frame address.
    // The frame cannot move between this guard and those reads: the helper only reads
    // machine state and runs no interpreter step.
    shape
        .source
        .guard_load(buffer, SOURCE_GUARD_SCRATCH, SOURCE_GUARD_END, fallback)?;

    // (1c) The `Y` **`binary all` source**'s frame-window guard, on the same terms as (1b):
    // guard only, because the helper reaches the term through `Machine::y` itself. The frame
    // cannot move between this guard and the helper call (the helper only reads machine state
    // and runs no interpreter step).
    if let Some(src) = shape.binary_all {
        src.guard_load(buffer, SOURCE_GUARD_SCRATCH, SOURCE_GUARD_END, fallback)?;
    }

    // (2) One `is_small` guard per integer field, still before any write: a non-integer
    // value is the family's `badarg` (`erts_bs_put_integer_*` returns 0 for a non-integer,
    // `erl_bits.c:952`), reported by the `fallback` interpreter step. A `Y` value's guard is
    // [`Loc::load`]'s guarded read, in the same position.
    //
    // The `binary all` route has no static field at all, so its guard phase is the (1c)
    // source guard and nothing else.
    for field in &prepared {
        let PreparedField::Bits { value, .. } = field else {
            continue;
        };
        value.load(
            buffer,
            allocation,
            Reg::Rdx,
            Reg::Rcx,
            fallback,
            highest_touched,
        )?;
        buffer.mov_ri(Reg::Rax, IMMED1_MASK)?;
        buffer.and_rr(Reg::Rdx, Reg::Rax)?;
        buffer.mov_ri(Reg::Rax, SMALL_TAG)?;
        buffer.cmp_rr(Reg::Rdx, Reg::Rax)?;
        buffer.jcc(Cond::Ne, fallback)?;
    }

    // (3) The construction seam's run-time half — the whole plan's room check, and the last
    // guard before every write. `rax` answers a non-zero word on success (the byte cursor on
    // the aligned route, the payload base on the general one) and `0` for every refusal. The
    // door mutates nothing, so this costs exactly one test — and the aligned route's own
    // emitted sequence already carries it (see [`create_door::emit_append_cursor`]).
    //
    // The `binary all` route's (3) is its one helper call, which is also its (4) and its (5):
    // the copy is run-time-length, so there is no static field phase, and the helper performs
    // the build's single `ErlSubBits.end` commit itself — which is why no window-end commit is
    // emitted for it below. Its `0` answer is the same `fallback`.
    let source_slot = shape.source_slot();
    if let Some(src) = shape.binary_all {
        emit_binary_all(buffer, &shape, src, allocation, fallback, func, pc)?;
    } else {
        match route {
            Route::Aligned => emit_append_cursor(
                buffer,
                allocation,
                shape.source,
                shape.tail_bits,
                func,
                pc,
                fallback,
                highest_touched,
            )?,
            Route::BitGranular => {
                let helper: OpenAppendAnyHelper = match shape.source {
                    Loc::X(_) => jit_open_append_bin_any,
                    Loc::Y(_) => jit_open_append_bin_any_y,
                };
                emit_positioned_helper_call(
                    buffer,
                    allocation,
                    helper as *const (),
                    &[
                        i64::from(source_slot),
                        i64::from(shape.tail_bits),
                        i64::from(shape.unit as u32),
                    ],
                    func,
                    pc,
                )?;
                buffer.test_rr(Reg::Rax, Reg::Rax)?;
                buffer.jcc(Cond::E, fallback)?;
            }
        }

        // (4) The field writes — the route's own phase (see the module docs).
        match route {
            Route::Aligned => emit_aligned_fields(buffer, &prepared, highest_touched)?,
            Route::BitGranular => {
                emit_bit_fields(
                    buffer,
                    &shape,
                    &prepared,
                    allocation,
                    func,
                    pc,
                    fallback,
                    highest_touched,
                )?
            }
        }

        // (5) The window-end commit — `sb->end = new_position` (`erl_bits.c:1975`), the
        // build's result, and **the only mutation** the sequence performs beyond the payload
        // bytes. (The `binary all` route's commit is inside its helper: its advance is the
        // source's run-time width, not a compile-time constant.)
        match shape.source {
            Loc::X(x) => {
                buffer.mov_rm(Reg::Rdx, Reg::Rbx, x_offset(x))?;
                touch(highest_touched, x);
            }
            Loc::Y(k) => emit_y_slot_value(buffer, Reg::Rdx, k)?,
        }
        buffer.mov_ri(Reg::Rax, POINTER_MASK)?;
        buffer.and_rr(Reg::Rdx, Reg::Rax)?;
        buffer.mov_rm(Reg::Rcx, Reg::Rdx, SUB_BITS_END_DISP)?;
        buffer.mov_ri(Reg::Rax, i64::from(shape.tail_bits))?;
        buffer.add_rr(Reg::Rcx, Reg::Rax)?;
        buffer.mov_mr(Reg::Rdx, SUB_BITS_END_DISP, Reg::Rcx)?;
    }

    // (6) The accumulator term into the destination — the very word the instruction was
    // given, because the seam proved the build takes the in-place route (`grow`, republish
    // and promotion to the copy-rebuild form all decline). Nothing after this may fail.
    if let Loc::Y(k) = shape.dst {
        // The window guard already ran and nothing between it and here moved the frame (the
        // helper only reads machine state), so the slot address is re-derived without a
        // second guard.
        emit_y_slot_address(buffer, Y_ADDR_SCRATCH, k)?;
    }
    match shape.source {
        Loc::X(x) => {
            buffer.mov_rm(Reg::Rdx, Reg::Rbx, x_offset(x))?;
            touch(highest_touched, x);
        }
        Loc::Y(k) => emit_y_slot_value(buffer, Reg::Rdx, k)?,
    }
    shape
        .dst
        .store_at(buffer, Y_ADDR_SCRATCH, Reg::Rdx, allocation, highest_touched)?;

    emit_fallback_and_done(buffer, allocation, done, func, pc, fallback, fast_done)
}

/// The **aligned** route's field-write phase: the byte stores at the write cursor the append
/// door returned in `rax`.
///
/// Every value is re-read after the helper: an x-register from `machine.x` (`rbx` is
/// callee-saved, and the spill before the call made that array authoritative for a cached
/// register too), a `Y` slot through its re-derived address — once per field into `rcx`,
/// since its four-instruction re-derivation should not be paid per byte.
///
/// `rax` (the cursor) is never clobbered: `rdx` carries the extracted byte and `rcx` a `Y`
/// value, and the displacement is `cursor + offset_bits / 8`.
fn emit_aligned_fields(
    buffer: &mut CodeBuffer,
    prepared: &[PreparedField<'_>],
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    for field in prepared {
        match field {
            PreparedField::Bits {
                value,
                bits,
                offset_bits,
            } => {
                // A whole number of bytes on this route, and the whole tail is capped at
                // [`MAX_TAIL_BYTES`], so the displacement is a small `disp32`.
                let offset = (*offset_bits / 8) as i32;
                let nbytes = (*bits / 8) as usize;
                match *value {
                    Loc::X(_) => {}
                    Loc::Y(k) => emit_y_slot_value(buffer, Reg::Rcx, k)?,
                }
                for i in 0..nbytes {
                    match *value {
                        Loc::X(x) => {
                            buffer.mov_rm(Reg::Rdx, Reg::Rbx, x_offset(x))?;
                            touch(highest_touched, x);
                        }
                        Loc::Y(_) => buffer.mov_rr(Reg::Rdx, Reg::Rcx)?,
                    }
                    // Byte `i` (MSB first) of the big-endian field is word bits
                    // `4 + 8 * (nbytes - 1 - i)` .. +8 (see the module docs), and the shift
                    // is ARITHMETIC precisely because the caller wants
                    // `(value as i64 as u64).to_be_bytes()`: the raw word's bit 63 is the
                    // small value's sign bit, so `sar` sign-extends the 60-bit value to the
                    // 64 bits `push_byte_field` receives, where a logical shift's zero fill
                    // would corrupt a negative 8-byte field's top byte (`-1` as `0F FF …`).
                    buffer.sar_ri(Reg::Rdx, (4 + 8 * (nbytes - 1 - i)) as u32)?;
                    buffer.mov_mr8(Reg::Rax, offset + i as i32, Reg::Rdx)?;
                }
            }
            PreparedField::Const { bytes, offset_bits } => {
                let offset = (*offset_bits / 8) as i32;
                // Eight bytes per `movabs` + 64-bit store (the bytes are baked as an
                // immediate — never as a `StrT` pointer, which a cached module's string area
                // would invalidate), then the remaining tail one byte at a time.
                let mut written = 0usize;
                while written + 8 <= bytes.len() {
                    let mut chunk = [0u8; 8];
                    chunk.copy_from_slice(&bytes[written..written + 8]);
                    buffer.mov_ri(Reg::Rdx, i64::from_le_bytes(chunk))?;
                    buffer.mov_mr(Reg::Rax, offset + written as i32, Reg::Rdx)?;
                    written += 8;
                }
                while written < bytes.len() {
                    buffer.mov_ri(Reg::Rdx, i64::from(bytes[written]))?;
                    buffer.mov_mr8(Reg::Rax, offset + written as i32, Reg::Rdx)?;
                    written += 1;
                }
            }
        }
    }
    Ok(())
}

/// The **general** (bit-granular) route's field-write phase.
///
/// It has two forms, chosen per **plan** by the field widths, and both run behind the
/// up-front general door and its `test rax, rax ; je fallback`:
///
/// * [`emit_bit_fields_inline`] — every field is at most [`MAX_INLINE_FIELD_BITS`] bits, so
///   the payload base the door answered stays in a register, the window end is derived from
///   the accumulator term once, and each field's bytes are straight-line stores.
/// * the per-field `jit_write_bit_field` call below — a field wider than that cannot be
///   left-aligned in one 64-bit register for every window offset, so the whole plan keeps
///   the helper sequence (one call per field, each re-deriving the base and the window end
///   from the seam).
///
/// Every argument of the helper is a compile-time constant (`movabs`): the accumulator's
/// slot index, the field's **bit** offset, the value's slot index, the field width in bits
/// and the [`WRITE_ACC_IN_Y`]/[`WRITE_VALUE_IN_Y`] flags naming which machine file holds
/// each term. The helper re-derives the payload base and the window end from the general
/// door, applies the family's bit-granular write rule, and answers `1`/`0`; a refusal is
/// the family's own path through the `fallback`, and the payload bytes it may already have
/// written are the same bytes a re-execution writes.
fn emit_bit_fields(
    buffer: &mut CodeBuffer,
    shape: &CreateBinShape,
    prepared: &[PreparedField<'_>],
    allocation: &Allocation,
    func: u32,
    pc: u32,
    fallback: CodeLabel,
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    if prepared.iter().all(|field| {
        matches!(field, PreparedField::Bits { bits, .. } if *bits <= MAX_INLINE_FIELD_BITS)
    }) {
        return emit_bit_fields_inline(buffer, shape, prepared, highest_touched);
    }
    let acc_in_y = shape.source.is_y();
    let src_slot = i64::from(shape.source_slot());
    let helper: WriteBitFieldHelper = jit_write_bit_field;
    for field in prepared {
        // `try_inline_bs_create_bin` gated this route on `Bits`-only field lists (see the
        // module docs), so a `ConstBytes` field cannot reach here.
        let PreparedField::Bits {
            value,
            bits,
            offset_bits,
        } = field
        else {
            continue;
        };
        let value_slot = match *value {
            Loc::X(x) => x,
            Loc::Y(k) => k,
        };
        let flags = if acc_in_y { WRITE_ACC_IN_Y } else { 0 }
            | if value.is_y() { WRITE_VALUE_IN_Y } else { 0 };
        emit_positioned_helper_call(
            buffer,
            allocation,
            helper as *const (),
            &[
                src_slot,
                i64::from(*offset_bits),
                i64::from(value_slot),
                i64::from(*bits),
                flags,
            ],
            func,
            pc,
        )?;
        buffer.test_rr(Reg::Rax, Reg::Rax)?;
        buffer.jcc(Cond::E, fallback)?;
    }
    Ok(())
}

/// The **inline** general-route field-write phase: one wide field store per field, no call,
/// for a plan whose every field is at most [`MAX_INLINE_FIELD_BITS`] bits.
///
/// `rax` holds the payload base the up-front general door answered, so it moves to the
/// base register; the window end comes from the accumulator term itself — the door proved
/// it is an `ErlSubBits` over a `BinRef` with `start == 0`, so
/// `beam_term::bitstring_parts`' size for it **is** the `ErlSubBits.end` word, which **is**
/// the door's `end_bits` (`ErlSubBits` words 3 and 4 are `end` and `orig`, exactly the
/// `SUB_BITS_END_DISP` the window commit already reads). One mask-and-load derives it, and
/// every field of the plan reuses it.
///
/// What that drops is only the helper's own re-verification: the whole-plan room check the
/// up-front door made implies every field's room, the append-unit gate was the plan's, and
/// the `is_small` refusal is this arm's up-front per-field guard (see
/// [`super::create_field`]).
///
/// Every value is re-read from the machine after the helper: a cached x-register was
/// spilled before the call, so `machine.x` (via `rbx`) is authoritative, and a `Y` slot is
/// reached through its own re-derived address behind the up-front guard.
fn emit_bit_fields_inline(
    buffer: &mut CodeBuffer,
    shape: &CreateBinShape,
    prepared: &[PreparedField<'_>],
    highest_touched: &mut Option<u32>,
) -> Result<()> {
    let FieldScratch {
        base, end, ..
    } = PRODUCTION_FIELD_SCRATCH;
    // The door's answer (the payload base) into the register the whole phase keeps it in.
    buffer.mov_rr(base, Reg::Rax)?;
    match shape.source {
        Loc::X(x) => {
            buffer.mov_rm(end, Reg::Rbx, x_offset(x))?;
            touch(highest_touched, x);
        }
        Loc::Y(k) => emit_y_slot_value(buffer, end, k)?,
    }
    buffer.mov_ri(Reg::Rax, POINTER_MASK)?;
    buffer.and_rr(end, Reg::Rax)?;
    buffer.mov_rm(end, end, SUB_BITS_END_DISP)?;
    for field in prepared {
        let PreparedField::Bits {
            value,
            bits,
            offset_bits,
        } = field
        else {
            continue;
        };
        match *value {
            Loc::X(x) => {
                buffer.mov_rm(Reg::Rax, Reg::Rbx, x_offset(x))?;
                touch(highest_touched, x);
            }
            Loc::Y(k) => emit_y_slot_value(buffer, Reg::Rax, k)?,
        }
        emit_bit_field_store(buffer, Reg::Rax, *bits, *offset_bits, &PRODUCTION_FIELD_SCRATCH)?;
    }
    #[cfg(test)]
    note_inline_field_plan();
    Ok(())
}

/// Re-derive `Y` slot `k`'s **address** into `dst` from the block's own `rt` slot and the
/// current frame's published `y_base`, with **no** frame-window guard.
///
/// Sound only behind the up-front [`Loc::guard_load`] of the same slot (see the module
/// docs): the frame cannot change between them, so the guard's proof still holds here.
fn emit_y_slot_address(buffer: &mut CodeBuffer, dst: Reg, k: u32) -> Result<()> {
    buffer.mov_rm(dst, Reg::Rsp, RUNTIME_SLOT_DISP)?;
    buffer.mov_rm(dst, dst, RUNTIME_YBASE_DISP)?;
    buffer.lea(dst, dst, y_offset(k))
}

/// Re-derive `Y` slot `k`'s **word** into `dst`, with no frame-window guard (see
/// [`emit_y_slot_address`]).
fn emit_y_slot_value(buffer: &mut CodeBuffer, dst: Reg, k: u32) -> Result<()> {
    emit_y_slot_address(buffer, dst, k)?;
    buffer.mov_rm(dst, dst, 0)
}
