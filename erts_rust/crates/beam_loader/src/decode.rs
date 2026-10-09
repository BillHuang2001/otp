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

//! The instruction-stream decoder: `Code` bytes → [`RawInstr`]s.
//!
//! Each instruction is one opcode byte (`1..=max_opcode`) followed by
//! [`Opcode::arity`] operands. Each operand is one *tag/length* byte plus an
//! optional payload (`beam_file.c:167-240`); the tag is `byte & 0x07` (a file only
//! ever carries tags 0–7):
//!
//! | tag | meaning |
//! |-----|---------|
//! | `0` | `u` untagged integer |
//! | `1` | `i` integer (may be negative) |
//! | `2` | `a` atom index (0 ⇒ NIL) |
//! | `3` | `x` X register |
//! | `4` | `y` Y register |
//! | `5` | `f` label (0 ⇒ [`crate::operand::NO_FAIL`]) |
//! | `6` | `h` character |
//! | `7` | `z` extended sub-tag (see the crate `CONTEXT.md`) |
//!
//! The tag byte also encodes the operand's variable-length integer: `byte & 0x08 ==
//! 0` ⇒ value `byte >> 4`; else `byte & 0x10 == 0` ⇒ a 1-byte big-endian tail
//! (0–0x7FF); else `(byte >> 5) + 2` big-endian magnitude bytes, with
//! `(byte >> 5) == 7` meaning a `u`-tagged sub-varint gives `count = S + 9` bytes.
//!
//! All reads go through [`beam_codec::Reader`] (the shared bounds-checked
//! big-endian cursor); a malformed stream yields `BeamError::BadArg`, never a panic.
use beam_common::Result;
use beam_term::{Atom, MAX_SMALL, MIN_SMALL};
use std::sync::OnceLock;

use crate::error::bad_file;
use crate::ir::{CodeVersion, TypeTable};
use crate::opcode::Opcode;
use crate::operand::{AllocKind, Label, Operand, Reg, TypeIndex};

/// The `u` (unsigned) operand tag: the low three bits of the tag byte.
const TAG_U: u8 = 0;
/// The `i` (signed) operand tag. It is the **only** tag allowed to hold a negative
/// value or one wider than a machine word (`beam_file.c:228-231`).
const TAG_I: u8 = 1;

/// The largest register index a `.beam` operand may name.
///
/// `x(N)`/`y(N)` must be below this (`beam_file.c:2335-2338`) and so must a `z,2` float
/// register (`beam_file.c:2381`); it is the C's `MAX_REG` (`erl_vm.h:49`, "max number of
/// x(N) registers used").
const MAX_REG: u64 = 1024;

/// The largest number of `(kind, number)` pairs a `z,3` allocation list may carry.
///
/// One entry per allocation kind and there are only four kinds, so the C asserts
/// `count.word_value <= 4` (`beam_file.c:2241`).
const MAX_ALLOC_ENTRIES: u64 = 4;

/// The largest `z,3` allocation-list *number*.
///
/// The C bounds it by `ERTS_SINT32_MAX / MAX(FLOAT_SIZE_OBJECT, ERL_FUN_SIZE + 1)`
/// (`beam_file.c:2255-2256`) — `i32::MAX / 3` on the 64-bit emulator — so that the word
/// total accumulates in a 32-bit signed `Sint` without overflowing.
const MAX_ALLOC_NUMBER: u64 = (i32::MAX as u64) / 3;

/// The largest `z,3` word total (`sum`) as the C accumulates it: `Sint` heap words, so it
/// must stay within `ERTS_SINT32_MAX` (`beam_file.c:2261-2274`).
const MAX_ALLOC_WORDS: u64 = i32::MAX as u64;
/// One decoded tag/length number: the tag and its magnitude
/// (`beam_file.c:167-240`, `beamreader_read_tagged`).
///
/// The C reader first tries to unpack the magnitude into one machine word
/// (`unpack_varint`); when that succeeds — and, for an `i` value, when the
/// sign-extended result is still a small integer (`IS_SSMALL`) — the value is
/// *inline* and [`TaggedNumber::bytes`] is empty, with [`TaggedNumber::word`]
/// holding it. Otherwise the value stays as its raw big-endian field and the C keeps
/// the `count` bytes in `ptr_value` with `size = count`; that is
/// [`TaggedNumber::bytes`] here (and `word` is then meaningless, mirroring the C's
/// union, which the overflow path overwrites).
pub(crate) struct TaggedNumber<'a> {
    /// The low three bits of the tag byte (`0 u`, `1 i`, `2 a`, …).
    pub(crate) tag: u8,
    /// The value when it fits one machine word: the raw unpacked value for every tag
    /// but `i`, the sign-extended value for a small `i`. Meaningless when `bytes` is
    /// non-empty.
    pub(crate) word: i64,
    /// The raw **big-endian** magnitude bytes when the value did not fit a machine
    /// word (or an `i` value did not stay a small); empty otherwise.
    pub(crate) bytes: &'a [u8],
}

impl TaggedNumber<'_> {
    /// The inline value as an unsigned word.
    ///
    /// The `AtU8`/`Line` chunk parsers need a plain length/index and assert the
    /// number fit a word (`len.size == 0`, `beam_file.c:282`), so a wider one is the
    /// loader's documented [`beam_common::BeamError::SystemLimit`].
    pub(crate) fn word_or_system_limit(&self) -> Result<u64> {
        if self.bytes.is_empty() {
            Ok(self.word as u64)
        } else {
            Err(beam_common::BeamError::SystemLimit)
        }
    }
}

/// Reads one packed "tagged number" from a stream (`beam_file.c:167-240`,
/// `beamreader_read_tagged`).
///
/// This is the single lowest-common-ancestor decoder for the tag/length byte used by
/// the instruction operand decoder ([`decode_operand`]) *and* by the `AtU8`/`Line`
/// chunk parsers ([`crate::chunk::atom`], [`crate::chunk::line`]); it is exposed at
/// crate scope so those callers share one implementation rather than re-deriving the
/// encoding.
///
/// The first byte is `tag = byte & 0x07` and encodes the value's length:
/// `byte & 0x08 == 0` ⇒ value `byte >> 4` (0–15); else `byte & 0x10 == 0` ⇒ a 1-byte
/// big-endian tail with value `((byte >> 5) << 8) | tail`; else `(byte >> 5) + 2`
/// big-endian magnitude bytes, where `(byte >> 5) == 7` means a `u`-tagged sub-length
/// `S` precedes `S + 9` magnitude bytes.
///
/// The value is inlined into [`TaggedNumber::word`] when it fits one machine word
/// — for an `i` value only while its sign extension is still `IS_SSMALL`
/// (`beam_file.c:209-225`) — and otherwise returned as the raw big-endian
/// [`TaggedNumber::bytes`]. A sub-length whose tag is not `u`, or one at/above the
/// C's `ERTS_SINT32_MAX - 9` bound (`beam_file.c:201`), yields
/// `BeamError::BadArg`. Never panics.
pub(crate) fn read_tagged<'a>(r: &mut beam_codec::Reader<'a>) -> Result<TaggedNumber<'a>> {
    let b = r.u8()?;
    let tag = b & 0x07;
    if (b & 0x08) == 0 {
        return Ok(TaggedNumber {
            tag,
            word: i64::from(b >> 4),
            bytes: &[],
        });
    }
    if (b & 0x10) == 0 {
        let tail = r.u8()?;
        let value = (u64::from(b >> 5) << 8) | u64::from(tail);
        return Ok(TaggedNumber {
            tag,
            word: value as i64,
            bytes: &[],
        });
    }
    let count = if (b >> 5) < 7 {
        usize::from(b >> 5) + 2
    } else {
        let prefix = read_tagged(r)?;
        // `LoadAssert(size_prefix.tag == TAG_u)` (`beam_file.c:198`).
        if prefix.tag != TAG_U {
            return Err(bad_file("tagged sub-length is not unsigned"));
        }
        // `LoadAssert(unpacked_size < (ERTS_SINT32_MAX - size_base))`
        // (`beam_file.c:201`): the bound keeps `count` far away from overflowing.
        let length = prefix.word_or_system_limit()?;
        if length >= u64::from(i32::MAX as u32) - 9 {
            return Err(bad_file("tagged sub-length is too large"));
        }
        length as usize + 9
    };
    let bytes = r.bytes(count)?;

    // `unpack_varint` (`beam_file.c:150-165`): a value packed in at most one machine
    // word is unpacked here. `count` is never zero (`>= 2` above).
    if count <= 8 {
        let mut raw: u64 = 0;
        for &byte in bytes {
            raw = (raw << 8) | u64::from(byte);
        }
        if tag == TAG_I {
            // The C sign-extends an `i` value from `count * 8` bits and keeps it
            // inline only while it is *still* a small (`beam_file.c:209-221`); a
            // wider value keeps its raw bytes and is turned into a bignum literal by
            // `marshal_integer`. An `i` operand that is neither would be handed to
            // `make_small` out of range, i.e. a wrong term.
            let signed = sign_extend(raw, count);
            if (MIN_SMALL..=MAX_SMALL).contains(&signed) {
                return Ok(TaggedNumber {
                    tag,
                    word: signed,
                    bytes: &[],
                });
            }
        } else if (raw as i64) >= 0 {
            // For every other tag the C keeps the value only while it is
            // non-negative as a *signed* word (`beam_file.c:222-225`); anything else
            // overflows to the valueless `TAG_o`, which the callers reject.
            return Ok(TaggedNumber {
                tag,
                word: raw as i64,
                bytes: &[],
            });
        }
    }

    Ok(TaggedNumber {
        tag,
        word: 0,
        bytes,
    })
}

/// The largest instruction count [`decode_stream`] pre-reserves.
///
/// The reservation is `stream.len() / 4` — the sampled real OTP code streams average
/// ~4.25 bytes per instruction (`tests/load_cost.rs`) — but a *file-supplied* stream
/// length must not decide an unbounded up-front allocation, so it is capped here. The
/// biggest module in the installed OTP carries ~31 000 instructions (same measurement),
/// so `1 << 18` is far above any real module while bounding the reservation to
/// `1 << 18` × `size_of::<RawInstr>()`. A stream that outgrows the cap simply grows the
/// vector as before.
const MAX_RESERVED_INSTRUCTIONS: usize = 1 << 18;

/// One decoded instruction: opcode, operands and its byte offset in the stream.
///
/// The offset lets [`crate::lower`] attach line/backtrace information and lets the
/// engine report a source location for an exception.
#[derive(Clone, Debug)]
pub struct RawInstr {
    /// The instruction's opcode.
    pub opcode: Opcode,
    /// The decoded operands, in the order the opcode declares them.
    pub operands: Vec<Operand>,
    /// The byte offset of the opcode in the `Code` instruction stream.
    pub offset: u32,
}

/// The per-opcode-byte operand count, in O(1).
///
/// The decoder asks for the arity of **every** instruction it decodes, and
/// [`Opcode::arity`] resolves through a linear scan of the ~193-row
/// [`crate::opcode::OPCODE_TABLE`] (measured at ~33 ns per call — the single hottest
/// lookup on the load path; `tests/load_cost.rs` is the measurement that justifies
/// this table). This function answers the same question with one array index.
///
/// The table is **derived once per process** from the roster itself
/// ([`Opcode::from_u8`] + [`Opcode::arity`]), so it duplicates no generated data: the
/// roster in [`crate::opcode`] stays the single source of truth and this is only a
/// cache of it. Byte `0` and every byte outside the roster hold `0`, but a caller never
/// reads those entries — it looks the arity up only after [`Opcode::from_u8`] has
/// accepted the byte (see [`decode_stream`]).
fn opcode_arity(byte: u8) -> u8 {
    static ARITY: OnceLock<[u8; 256]> = OnceLock::new();
    ARITY
        .get_or_init(|| {
            let mut table = [0u8; 256];
            for value in 0u16..=255 {
                if let Some(opcode) = Opcode::from_u8(value as u8) {
                    table[value as usize] = opcode.arity();
                }
            }
            table
        })[usize::from(byte)]
}
/// Decodes a `Code` instruction stream into [`RawInstr`]s.
///
/// `atoms` maps a *wire* atom index — the magnitude of an `a` operand — to the
/// interned [`Atom`] (`atoms[0]` is the NIL placeholder and is never looked up,
/// because `a,0` decodes to [`Operand::Nil`]). `types` is the module's
/// [`TypeTable`]: it decides whether a `z,5` `tr` operand keeps its type index or
/// collapses to a plain register. `version` supplies `max_opcode`, the valid
/// opcode-byte bound.
///
/// The stream ends at the `int_code_end/0` instruction, **not** at size exhaustion:
/// a stream that ends before `int_code_end` is malformed (the `Code` chunk's size
/// includes trailing padding, which the reader must never try to interpret). A zero
/// opcode byte, an opcode byte above `max_opcode`, an opcode with no roster entry, a
/// truncated instruction, an out-of-range atom index or an unknown/obsolete `z`
/// sub-tag yields [`beam_common::BeamError::BadArg`]. Never panics.
pub fn decode_stream(
    stream: &[u8],
    atoms: &[Atom],
    version: &CodeVersion,
    types: &TypeTable,
) -> Result<Vec<RawInstr>> {
    let mut r = beam_codec::Reader::new(stream);
    // Pre-size the instruction vector from the stream length (capped — see
    // [`MAX_RESERVED_INSTRUCTIONS`]): a sampled real OTP code stream averages ~4.25 bytes
    // per instruction, so `len / 4` is a close, cheap estimate that removes the repeated
    // regrows — and their memmoves — of a multi-thousand-instruction module. It is only
    // a hint: a stream that outgrows it still grows correctly.
    let mut instrs =
        Vec::with_capacity((stream.len() / 4).min(MAX_RESERVED_INSTRUCTIONS));

    loop {
        // The stream must be terminated by `int_code_end`; running out of bytes
        // first means the opcode/operand framing did not line up with the chunk.
        if r.is_empty() {
            return Err(bad_file("instruction stream ended before int_code_end"));
        }

        // Recorded before the opcode byte is consumed, so it is the instruction's
        // own offset in the `Code` stream (what line/backtrace mapping needs).
        let offset = r.position() as u32;

        let op_byte = r.u8()?;
        if op_byte == 0 {
            return Err(bad_file("zero opcode byte"));
        }
        if u32::from(op_byte) > version.max_opcode {
            return Err(bad_file("opcode above the chunk's max_opcode"));
        }
        let op = Opcode::from_u8(op_byte).ok_or_else(|| bad_file("opcode is not in the roster"))?;

        // The operand count comes from the derived table rather than through
        // `Opcode::arity` (a linear scan of the roster) — see [`opcode_arity`]. The
        // `derived_arity_table_must_match_the_roster` unit test pins the two together.
        let arity = usize::from(opcode_arity(op_byte));
        let mut operands = Vec::with_capacity(arity);
        for index in 0..arity {
            let operand = decode_operand(&mut r, atoms, types)?;
            // `LoadAssert((i + 1) == arity)` (`beam_file.c:2352`): a `z,1` argument list is
            // always the instruction's *last* operand (the compiler emits the variadic
            // operand last, and the C expands it into the trailing argument slots).
            if index + 1 < arity && matches!(operand, Operand::List(_)) {
                return Err(bad_file("argument list is not the last operand"));
            }
            check_label(&operand, version.label_count)?;
            operands.push(operand);
        }
        instrs.push(RawInstr {
            opcode: op,
            operands,
            offset,
        });

        if op == Opcode::int_code_end {
            return Ok(instrs);
        }
    }
}

/// Decodes one operand from the instruction stream.
///
/// Reads the tag/length byte and its payload from `r`, returning the typed
/// [`Operand`]. `atoms` maps a wire atom index to the interned [`Atom`] (used by the
/// `a` tag; an index outside `atoms` is a malformed image). `types` is the module's
/// [`TypeTable`]: when it is a fallback the `z,5` `tr` operand collapses to a plain
/// X/Y register (the type index is discarded), and a `z,1` argument list recurses
/// into [`decode_operand`].
///
/// An `i` operand whose value does not fit a small integer decodes to
/// [`Operand::BigInteger`] (the C's bignum-literal operand, `beam_file.c:2134-2232`);
/// every *other* tag whose value did not fit a machine word overflowed to the C's
/// valueless `TAG_o` (`beam_file.c:228-233`), which is a malformed image here.
///
/// A short read, an out-of-range atom index, an over-wide non-`i` operand or an
/// unknown/obsolete `z` sub-tag (notably sub-tag 0, the obsolete float form) yields
/// [`beam_common::BeamError::BadArg`]. Never panics.
pub fn decode_operand(
    r: &mut beam_codec::Reader<'_>,
    atoms: &[Atom],
    types: &TypeTable,
) -> Result<Operand> {
    // One tag/length byte plus its magnitude, decoded by the shared primitive.
    let tagged = read_tagged(r)?;
    let tag = tagged.tag;
    if !tagged.bytes.is_empty() {
        // The value did not fit a machine word: only `i` is allowed to be wider than
        // a word (it becomes a bignum literal); any other tag is the C's valueless
        // `TAG_o`, which fails the load (`beam_file.c:228-233`).
        if tag != TAG_I {
            return Err(bad_file("operand is wider than a machine word"));
        }
        return big_integer_operand(tagged.bytes);
    }
    let magnitude = tagged.word as u64;

    match tag {
        // `u` — untagged integer. The file's encoding is unsigned; the value is
        // carried as the IR's `i64` (the compiler only emits tagged integers that
        // fit, and `i` is the tag for a negative value). A `u` value above
        // `i64::MAX` is rejected above as an overflow (`beam_file.c:222-225`).
        0 => Ok(Operand::Integer(magnitude as i64)),
        // `i` — signed integer, already sign-extended (and small-checked) by
        // `read_tagged`; a value too wide for a small took the [`Operand::BigInteger`]
        // path above.
        1 => Ok(Operand::Integer(magnitude as i64)),
        // `a` — atom index (0 ⇒ NIL).
        2 => {
            if magnitude == 0 {
                return Ok(Operand::Nil);
            }
            let atom = atoms
                .get(magnitude as usize)
                .copied()
                .ok_or_else(|| bad_file("atom index out of range"))?;
            Ok(Operand::Atom(atom))
        }
        // `x` — X register.
        //
        // `raw_arg.word_value < MAX_REG` (`beam_file.c:2335-2338`): a register index at or
        // above the runtime's register file cannot exist, so it is a malformed image.
        3 => {
            if magnitude >= MAX_REG {
                return Err(bad_file("x register index out of range"));
            }
            Ok(Operand::X(magnitude as u32))
        }
        // `y` — Y register (stack slot). Same bound as `x` (`beam_file.c:2335-2338`).
        4 => {
            if magnitude >= MAX_REG {
                return Err(bad_file("y register index out of range"));
            }
            Ok(Operand::Y(magnitude as u32))
        }
        // `f` — label; `f,0` is the failure sentinel `NO_FAIL`, kept verbatim.
        //
        // The index bound against the chunk's `label_count` needs the `Code` header, so it
        // is enforced by [`decode_stream`] on the decoded operand.
        5 => Ok(Operand::Label(Label(magnitude as u32))),
        // `h` — character (defined but never emitted by the compiler). The C applies no
        // bound, but the IR field is 32-bit, so a wider value would silently truncate into
        // a *different* character; reject it instead.
        6 => Ok(Operand::Character(narrow_u32(magnitude)?)),
        // `z` — extended form; the magnitude is the sub-tag.
        7 => decode_extended(r, magnitude, atoms, types),
        // Tags 8–14 exist only inside the loader and must never appear in a file.
        _ => Err(bad_file("unrecognized operand tag")),
    }
}

/// Builds the IR operand for an `i`-tagged integer that does not fit a machine word
/// (or no longer fits a small after sign extension).
///
/// `bytes` is the raw **big-endian** `count`-byte field, which the C reads as a
/// signed `count * 8`-bit two's-complement integer: it takes the sign from the top
/// bit, negates the field when negative and normalizes at most one leading zero
/// byte; a field with two leading zero bytes is "not normalized" and a corrupt file
/// (`marshal_integer`, `beam_file.c:2155-2208`). The result is the value's sign and
/// magnitude, which [`crate::lower`] materializes as a bignum static literal.
fn big_integer_operand(bytes: &[u8]) -> Result<Operand> {
    let (negative, magnitude) = bignum_sign_magnitude(bytes)?;
    Ok(Operand::big_integer(negative, magnitude))
}

/// Extracts the sign and normalized little-endian magnitude of a wide `i`-tagged
/// integer field.
///
/// `bytes` is the raw **big-endian** `count`-byte field, which the C reads as a
/// signed `count * 8`-bit two's-complement integer: it takes the sign from the top
/// bit, negates the field when negative and normalizes at most one leading zero
/// byte; a field with two leading zero bytes is "not normalized" and a corrupt file
/// (`marshal_integer`, `beam_file.c:2155-2208`). The result is the value's sign and
/// magnitude, which callers materialize as a bignum.
///
/// This is the single lowest-common-ancestor decoder for a wide `i` field: the
/// instruction-stream operand decoder ([`decode_operand`]) and the `Recs` chunk's
/// field-default decoder ([`crate::chunk::record`]) both share it.
pub(crate) fn bignum_sign_magnitude(bytes: &[u8]) -> Result<(bool, Vec<u8>)> {
    // `bytes` is non-empty (the explicit-count form packs at least two bytes).
    debug_assert!(!bytes.is_empty(), "an operand field is never empty");
    let negative = bytes[0] & 0x80 != 0;

    // The magnitude, little-endian first (the order the C builds in `conv_buf`).
    let mut magnitude: Vec<u8> = bytes.iter().rev().copied().collect();
    if negative {
        // Two's-complement negation, exactly `beam_file.c:2175-2184`. The field is a
        // two's-complement encoding of a negative value, so the carry is consumed
        // within its width.
        let mut carry = 1u16;
        for byte in &mut magnitude {
            let negated = u16::from(!*byte) + carry;
            *byte = negated as u8;
            carry = negated >> 8;
        }
    }

    // Strip at most one leading zero byte; two mean the field was not normalized.
    if magnitude.last() == Some(&0) {
        magnitude.pop();
        if magnitude.last().is_none_or(|byte| *byte == 0) {
            return Err(bad_file("wide integer operand is not normalized"));
        }
    }

    magnitude.reverse();
    Ok((negative, magnitude))
}

/// Sign-extends an `i`-tagged magnitude packed in `byte_count` magnitude bytes.
///
/// Only the explicit-count form (`2..=8` bytes — the `count <= 8` branch of
/// [`read_tagged`], the only place an `i` value is sign-extended) reaches here, so
/// the value's sign bit is the top bit of that width and the left shift is `16..=48`,
/// which never overflows (`beam_file.c:205-215`).
fn sign_extend(magnitude: u64, byte_count: usize) -> i64 {
    let shift = 64 - 8 * byte_count;
    ((magnitude << shift) as i64) >> shift
}

/// Decodes the payload of a `z` (extended) operand, whose magnitude is the sub-tag
/// (`beam_file.c:2339-2447`).
///
/// Sub-tag `0` (the pre-R16B float form) and any unassigned sub-tag are rejected:
/// only 1–5 are ever emitted.
fn decode_extended(
    r: &mut beam_codec::Reader<'_>,
    sub_tag: u64,
    atoms: &[Atom],
    types: &TypeTable,
) -> Result<Operand> {
    match sub_tag {
        // 1 — argument list: a `u` count then that many sub-operands.
        //
        // The count is file-supplied, so the vector is *not* pre-allocated with it:
        // a bogus count fails on the first short read instead of trying to reserve
        // an attacker-chosen amount of memory.
        1 => {
            let count = read_unsigned(r, "argument-list length is not unsigned")?;
            let mut args = Vec::new();
            for _ in 0..count {
                args.push(decode_operand(r, atoms, types)?);
            }
            Ok(Operand::list(args))
        }
        // 2 — float register index (`beam_file.c:2388-2395`), bounded by `MAX_REG` exactly
        // like an `x`/`y` register (`beam_file.c:2381`).
        2 => {
            let index = read_unsigned(r, "float-register index is not unsigned")?;
            if index >= MAX_REG {
                return Err(bad_file("float register index out of range"));
            }
            Ok(Operand::FloatReg(index as u32))
        }
        // 3 — allocation list: a `u` count then that many `(kind, number)` pairs.
        3 => {
            let count = read_unsigned(r, "allocation-list length is not unsigned")?;
            // `LoadAssert(count.word_value <= 4)` (`beam_file.c:2241`): there are only four
            // allocation kinds, so a longer list is malformed framing, not a big request.
            if count > MAX_ALLOC_ENTRIES {
                return Err(bad_file("allocation list is longer than the four kinds"));
            }
            let mut entries = Vec::new();
            // The C accumulates the word total in a `Sint` and refuses to exceed
            // `ERTS_SINT32_MAX` (`beam_file.c:2243,2261-2274`); mirror that so the sum the
            // engine reserves cannot wrap.
            let mut sum: u64 = 0;
            for _ in 0..count {
                let kind = read_unsigned(r, "allocation kind is not unsigned")?;
                let number = read_unsigned(r, "allocation count is not unsigned")?;
                let entry = allocation_entry(kind, number)?;
                sum += u64::from(entry.words);
                if sum > MAX_ALLOC_WORDS {
                    return Err(beam_common::BeamError::SystemLimit);
                }
                entries.push(entry);
            }
            Ok(Operand::alloc(entries))
        }
        // 4 — static literal index into `Module.literals` (`beam_file.c:2400-2417`).
        //
        // The C asserts `index.word_value < literals->count` (the file's *static* literal
        // table, `beam_file.c:2412`). The table size is not known to the decoder, so
        // [`decode_stream`]'s caller-facing bound lives in `crate::lower::build_module`
        // (which owns the `LitT` table); here the index is only narrowed to the IR's `u32`
        // so a wider one cannot silently address a *different* literal.
        4 => {
            let index = read_unsigned(r, "literal index is not unsigned")?;
            Ok(Operand::Literal(narrow_u32(index)?))
        }
        // 5 — `tr`, a register carrying a `Type` chunk index (`beam_file.c:2419-2443`).
        5 => {
            let reg = match decode_operand(r, atoms, types)? {
                Operand::X(n) => Reg::X(n),
                Operand::Y(n) => Reg::Y(n),
                // `tr` annotates a register, nothing else.
                _ => return Err(bad_file("`tr` does not annotate an x/y register")),
            };
            let index = read_unsigned(r, "type index is not unsigned")?;
            if types.fallback {
                // The `Type` chunk was stripped after compilation (or is
                // incompatible): the annotation is dropped and `tr` becomes an
                // ordinary register, exactly as the C loader does.
                Ok(match reg {
                    Reg::X(n) => Operand::X(n),
                    Reg::Y(n) => Operand::Y(n),
                })
            } else {
                // `LoadAssert(index.word_value < types->count)` (`beam_file.c:2437`): a
                // `tr` annotation must name a real `Type` chunk entry.
                if index >= types.entries.len() as u64 {
                    return Err(bad_file("type index out of range"));
                }
                Ok(Operand::Typed {
                    reg,
                    ty: TypeIndex(narrow_u32(index)?),
                })
            }
        }        // 0 — the pre-R16B float byte form; anything else is unassigned.
        _ => Err(bad_file("unrecognized `z` sub-tag")),
    }
}

/// Reads one `u`-tagged (unsigned) sub-operand, rejecting every other tag.
///
/// Every `z` sub-tag payload is an unsigned count or index (`beam_file.c` asserts
/// `TAG_u` at each of them), so any other tag means the stream is malformed — as does
/// a value that did not fit a machine word. Shared with the `Recs` chunk reader.
pub(crate) fn read_unsigned(r: &mut beam_codec::Reader<'_>, msg: &str) -> Result<u64> {
    let tagged = read_tagged(r)?;
    if tagged.tag != TAG_U {
        return Err(bad_file(msg));
    }
    tagged
        .word_or_system_limit()
        .map_err(|_| bad_file("unsigned sub-operand is wider than a machine word"))
}

/// One `z,3` allocation-list entry, with its heap-word contribution.
///
/// Mirrors `marshal_allocation_list` (`beam_file.c:2259-2284`): kind `0` counts raw
/// heap words, kind `1` counts floats (`FLOAT_SIZE_OBJECT` words per element), kind
/// `2` counts funs (`ERL_FUN_SIZE` words per element) and kind `3` counts native
/// records (`RECORD_INST_SIZE(0)` words per element). On the 64-bit emulator all
/// three multipliers are `2`: `FLOAT_SIZE_OBJECT = DOUBLE_DATA_WORDS+1`
/// (`erl_term.h:525-526`), `ERL_FUN_SIZE = sizeof(ErlFunThing)/sizeof(Eterm)`
/// (`erl_fun.h:99`; `ErlFunThing` is `{thing_word, entry, env[]}`) and
/// `RECORD_INST_SIZE(0) = sizeof(ErtsRecordInstance)/sizeof(Eterm)`
/// (`erl_record.h:73-74`; `ErtsRecordInstance` is `{thing_word, record_definition,
/// values[]}`). Any other kind is malformed.
///
/// `number` is bounded by [`MAX_ALLOC_NUMBER`], the C's `ERTS_SINT32_MAX /
/// MAX(FLOAT_SIZE_OBJECT, ERL_FUN_SIZE + 1)` (`beam_file.c:2255-2256`); a wider one is a
/// size limit, not a `badarg`, exactly as before.
fn allocation_entry(kind: u64, number: u64) -> Result<AllocKind> {
    let words = match kind {
        0 => number,
        1 | 2 | 3 => number.saturating_mul(2),
        _ => return Err(bad_file("invalid allocation-list kind")),
    };
    // The C keeps its running total in a 32-bit signed `Sint`, so a single element that
    // already exceeds `i32::MAX / 3` (its per-element bound) is rejected.
    if number > MAX_ALLOC_NUMBER {
        return Err(beam_common::BeamError::SystemLimit);
    }
    Ok(AllocKind {
        kind: kind as u32,
        number: number as u32,
        words: words as u32,
    })
}

/// Narrows an operand magnitude to the IR's 32-bit field, rejecting a wider one.
///
/// Several `Operand` fields are `u32` while the wire magnitude is a machine word;
/// truncating with `as u32` would silently turn an out-of-range index into a *different*
/// valid one (a wrong result rather than a typed error). The value is a size the IR cannot
/// represent, so the loader reports [`beam_common::BeamError::SystemLimit`] — the same
/// class it uses for its other representational bounds.
fn narrow_u32(value: u64) -> Result<u32> {
    u32::try_from(value).map_err(|_| beam_common::BeamError::SystemLimit)
}

/// Rejects a decoded `f` operand that names a label the `Code` chunk does not define.
///
/// The C asserts `raw_arg.word_value < code->label_count` for every non-zero `f` operand
/// (`beam_file.c:2324-2331`); `f,0` is the `NO_FAIL` sentinel and is kept verbatim. A label
/// operand may also appear inside a `z,1` argument list (`Operand::List`), so the walk
/// descends into it — the C applies the same check to those slots, because a list's
/// elements *are* the instruction's trailing operands.
fn check_label(operand: &Operand, label_count: u32) -> Result<()> {
    match operand {
        Operand::Label(label) => {
            if label.0 != 0 && label.0 >= label_count {
                return Err(bad_file("label index out of range"));
            }
            Ok(())
        }
        Operand::List(items) => {
            for item in items.iter() {
                check_label(item, label_count)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{TestKind, Type};
    use crate::operand::{HeapNeed, NO_FAIL};
    use beam_term::AtomTable;

    /// The `Code` header of the committed `slice.beam` fixture: `max_opcode 181`,
    /// `label_count 15`, `function_count 6` (`tests/fixtures/README.md`).
    const VERSION: CodeVersion = CodeVersion {
        format_number: 0,
        max_opcode: 181,
        label_count: 15,
        function_count: 6,
    };

    /// Opcode numbers used by the stream tests (from `genop.tab` via
    /// [`crate::opcode::OPCODE_TABLE`]).
    const OP_MOVE: u8 = 64;
    const OP_IS_EQ_EXACT: u8 = 43;
    const OP_GC_BIF2: u8 = 125;
    const OP_INT_CODE_END: u8 = 3;
    /// `allocate_heap/3` (`genop.tab:103`).
    const OP_ALLOCATE_HEAP: u8 = 13;
    /// `test_heap/2` (`genop.tab:129`).
    const OP_TEST_HEAP: u8 = 16;

    /// Packs one `(tag, value)` operand into the file's tag/length byte plus
    /// magnitude bytes (`beam_file.c:167-240`), using the same three encodings the
    /// compiler emits:
    /// * value 0–15 ⇒ `value << 4 | tag` (in-nibble form);
    /// * value 16–0x7FF ⇒ `0x08 | ((value >> 8) << 5) | tag` + one magnitude byte;
    /// * wider ⇒ `0x18 | ((count - 2) << 5) | tag` + `count` big-endian bytes
    ///   (`0x18` = `0x08 | 0x10`: both length bits set means "explicit count").
    fn tagged(tag: u8, value: u64) -> Vec<u8> {
        assert!(tag <= 7, "a `.beam` file only ever carries tags 0-7");
        if value <= 15 {
            vec![((value as u8) << 4) | tag]
        } else if value < 0x800 {
            vec![0x08 | (((value >> 8) as u8) << 5) | tag, value as u8]
        } else {
            let count = ((64 - value.leading_zeros()) as usize + 7) / 8; // 2..=8
            let mut out = vec![0x18 | (((count - 2) as u8) << 5) | tag];
            out.extend_from_slice(&value.to_be_bytes()[8 - count..]);
            out
        }
    }

    /// The bytes of a synthetic instruction stream.
    fn stream(parts: &[&[u8]]) -> Vec<u8> {
        parts.iter().flat_map(|p| p.iter().copied()).collect()
    }

    /// Asserts that a decode step failed with the loader's `badarg`.
    ///
    /// [`beam_common::BeamError`] deliberately does not implement `PartialEq`, so
    /// the variant is matched rather than compared.
    #[track_caller]
    fn assert_bad_arg<T>(result: Result<T>) {
        match result {
            Err(beam_common::BeamError::BadArg) => {}
            Err(other) => panic!("expected BadArg, got {other:?}"),
            Ok(_) => panic!("expected BadArg, got a successful decode"),
        }
    }

    /// The fallback `TypeTable` (no usable `Type` chunk): `tr` collapses to a plain
    /// register.
    fn fallback_types() -> TypeTable {
        TypeTable::default()
    }

    /// A real (non-fallback) `TypeTable` with `count` entries.
    fn types_with(count: usize) -> TypeTable {
        TypeTable {
            entries: vec![Type::default(); count],
            fallback: false,
        }
    }

    // ---- decode_stream ----------------------------------------------------

    #[test]
    fn int_code_end_terminates_an_empty_stream() {
        let instrs = decode_stream(&[OP_INT_CODE_END], &[], &VERSION, &fallback_types()).unwrap();
        assert_eq!(instrs.len(), 1);
        assert_eq!(instrs[0].opcode, Opcode::int_code_end);
        assert!(instrs[0].operands.is_empty());
        assert_eq!(instrs[0].offset, 0);
    }

    #[test]
    fn decodes_move_x0_to_x1() {
        // move/2 = opcode 64, operands `x0 x1`: x0 = tag 3 value 0 = 0x03,
        // x1 = tag 3 value 1 = 0x13; then int_code_end (3).
        let bytes = stream(&[&[OP_MOVE], &tagged(3, 0), &tagged(3, 1), &[OP_INT_CODE_END]]);
        let instrs = decode_stream(&bytes, &[], &VERSION, &fallback_types()).unwrap();

        assert_eq!(instrs.len(), 2);
        assert_eq!(instrs[0].opcode, Opcode::r#move);
        assert_eq!(instrs[0].operands, vec![Operand::X(0), Operand::X(1)]);
        assert_eq!(instrs[0].offset, 0);
        assert_eq!(instrs[1].opcode, Opcode::int_code_end);
        assert_eq!(instrs[1].offset, 3);
    }

    #[test]
    fn decodes_gc_bif2_operands_in_order() {
        // gc_bif2/6 = opcode 125: `fail live bif arg1 arg2 dst`, i.e.
        // `f,0` (0x05) `u,2` (0x20) `u,1` (0x10) `x1` (0x13) `x2` (0x23) `x1`.
        let bytes = stream(&[
            &[OP_GC_BIF2],
            &tagged(5, 0),
            &tagged(0, 2),
            &tagged(0, 1),
            &tagged(3, 1),
            &tagged(3, 2),
            &tagged(3, 1),
            &[OP_INT_CODE_END],
        ]);
        let instrs = decode_stream(&bytes, &[], &VERSION, &fallback_types()).unwrap();

        assert_eq!(instrs[0].opcode, Opcode::gc_bif2);
        assert_eq!(
            instrs[0].operands,
            vec![
                Operand::Label(Label(0)),
                Operand::Integer(2),
                Operand::Integer(1),
                Operand::X(1),
                Operand::X(2),
                Operand::X(1),
            ]
        );
    }

    #[test]
    fn decodes_is_eq_exact_and_classifies_it() {
        // is_eq_exact/3 = opcode 43: `f,7` (0x75) `x0` (0x03) `a,1` (0x12).
        let mut atoms = AtomTable::new();
        let atom = atoms.intern("slice");
        let bytes = stream(&[
            &[OP_IS_EQ_EXACT],
            &tagged(5, 7),
            &tagged(3, 0),
            &tagged(2, 1),
            &[OP_INT_CODE_END],
        ]);
        let instrs =
            decode_stream(&bytes, &[Atom::MAX, atom], &VERSION, &fallback_types()).unwrap();

        assert_eq!(instrs[0].opcode, Opcode::is_eq_exact);
        assert_eq!(
            instrs[0].operands,
            vec![Operand::Label(Label(7)), Operand::X(0), Operand::Atom(atom),]
        );
        assert_eq!(
            TestKind::from_opcode(instrs[0].opcode),
            Some(TestKind::IsEqExact)
        );
    }

    #[test]
    fn rejects_a_stream_that_never_reaches_int_code_end() {
        // A complete `move` followed by EOF: the stream is unterminated.
        let bytes = stream(&[&[OP_MOVE], &tagged(3, 0), &tagged(3, 1)]);
        assert_bad_arg(decode_stream(&bytes, &[], &VERSION, &fallback_types()));

        // And an empty stream is unterminated too.
        assert_bad_arg(decode_stream(&[], &[], &VERSION, &fallback_types()));
    }

    #[test]
    fn rejects_a_zero_opcode_byte() {
        let bytes = &[0u8, OP_INT_CODE_END];
        assert_bad_arg(decode_stream(bytes, &[], &VERSION, &fallback_types()));
    }

    #[test]
    fn rejects_an_opcode_above_max_opcode() {
        // 182 > the chunk's `max_opcode` of 181.
        let bytes = &[182u8, OP_INT_CODE_END];
        assert_bad_arg(decode_stream(bytes, &[], &VERSION, &fallback_types()));
    }

    #[test]
    fn rejects_an_opcode_with_no_roster_entry() {
        // The roster is contiguous over 1..=193, so 194 has no `Opcode`; widen
        // `max_opcode` so the failure is the roster lookup, not the bound.
        let version = CodeVersion {
            max_opcode: 255,
            ..VERSION
        };
        let bytes = &[194u8, OP_INT_CODE_END];
        assert_bad_arg(decode_stream(bytes, &[], &version, &fallback_types()));
    }

    #[test]
    fn rejects_a_truncated_operand() {
        // `move` with only the first operand byte present.
        let bytes = stream(&[&[OP_MOVE], &tagged(3, 0)]);
        assert_bad_arg(decode_stream(&bytes, &[], &VERSION, &fallback_types()));
    }

    // ---- scalar operand tags ---------------------------------------------

    #[test]
    fn decodes_untagged_and_signed_integers() {
        let cases: &[(&[u8], i64)] = &[
            (&tagged(0, 0), 0),         // in-nibble `u,0`
            (&tagged(0, 15), 15),       // in-nibble `u,15`
            (&tagged(0, 16), 16),       // 1-byte tail `u,16`
            (&tagged(0, 0x7FF), 0x7FF), // 1-byte tail maximum
            (&tagged(0, 0x800), 0x800), // explicit 2-byte magnitude
            (&tagged(1, 0), 0),         // in-nibble `i,0`
            (&tagged(1, 15), 15),       // in-nibble `i,15`
            // The 1-byte-tail `i` form holds 16–0x7FF and is *positive*: the
            // compiler writes positive literals of that size this way
            // (`beam_asm.erl:858-860`) and `beamreader_read_tagged` sign-extends
            // only the explicit-count form (`beam_file.c:205-215`).
            (&tagged(1, 200), 200),
            (&tagged(1, 0x7FF), 0x7FF),
        ];
        for &(bytes, expected) in cases {
            assert_eq!(
                decode_operand(&mut beam_codec::Reader::new(bytes), &[], &fallback_types())
                    .unwrap(),
                Operand::Integer(expected),
                "bytes {bytes:02x?}"
            );
        }

        // Negatives are packed in the explicit-count form (`0x19` = tag `i`, two
        // magnitude bytes; `0x39` = three) and sign-extended from that width.
        let negative_cases: &[(&[u8], i64)] = &[
            (&[0x19, 0xFF, 0xFF], -1),         // 0xFFFF as i16
            (&[0x19, 0xFF, 0xFE], -2),         // 0xFFFE as i16
            (&[0x19, 0xFF, 0x38], -200),       // 0xFF38 as i16
            (&[0x19, 0x80, 0x00], -32_768),    // i16::MIN
            (&[0x39, 0xFF, 0xFF, 0x38], -200), // 0xFFFF38 as i24
        ];
        for &(bytes, expected) in negative_cases {
            assert_eq!(
                decode_operand(&mut beam_codec::Reader::new(bytes), &[], &fallback_types())
                    .unwrap(),
                Operand::Integer(expected),
                "bytes {bytes:02x?}"
            );
        }

        // The widest form: eight magnitude bytes, sign-extended from all 64 bits.
        let widest = tagged(1, u64::MAX);
        assert_eq!(widest.len(), 9, "tag byte + eight magnitude bytes");
        assert_eq!(
            decode_operand(
                &mut beam_codec::Reader::new(&widest),
                &[],
                &fallback_types()
            )
            .unwrap(),
            Operand::Integer(-1)
        );
    }

    /// Packs a `>= 9`-byte magnitude into the file's *sub-length* form: the tag byte
    /// `0xF8 | tag` (`(byte >> 5) == 7`) announces a `u`-tagged sub-length `S`, after
    /// which `S + 9` magnitude bytes follow (`beam_file.c:192-204`). This is the only
    /// encoding that carries a field wider than one machine word.
    fn tagged_wide(tag: u8, magnitude_be: &[u8]) -> Vec<u8> {
        assert!(tag <= 7, "a `.beam` file only ever carries tags 0-7");
        assert!(
            magnitude_be.len() >= 9,
            "the direct-count form covers 2..=8 bytes"
        );
        let mut out = vec![0xF8 | tag];
        out.extend(tagged(0, (magnitude_be.len() - 9) as u64));
        out.extend_from_slice(magnitude_be);
        out
    }

    #[test]
    fn decodes_a_wide_signed_integer_operand() {
        // A 9-byte negative field: `FF 00 … 00` is `-2^64` as a 72-bit
        // two's-complement integer (`marshal_integer`, `beam_file.c:2168-2208`).
        let mut field = vec![0xFFu8];
        field.extend_from_slice(&[0u8; 8]);
        let bytes = tagged_wide(1, &field);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&bytes), &[], &fallback_types()).unwrap(),
            Operand::big_integer(true, vec![1, 0, 0, 0, 0, 0, 0, 0, 0])
        );

        // A 9-byte positive field `00 FF … FF` is `2^64 - 1`; the single leading zero
        // byte is normalized away, exactly as the C does (`beam_file.c:2186-2203`).
        let mut field = vec![0x00u8];
        field.extend_from_slice(&[0xFFu8; 8]);
        let bytes = tagged_wide(1, &field);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&bytes), &[], &fallback_types()).unwrap(),
            Operand::big_integer(false, vec![0xFF; 8])
        );

        // The raw field is reported as the overflow field, never unpacked into a word
        // (`unpack_varint` refuses `count > sizeof(SWord)`).
        let mut field = vec![0x00u8];
        field.extend_from_slice(&[0xFFu8; 8]);
        let bytes = tagged_wide(1, &field);
        let tagged = read_tagged(&mut beam_codec::Reader::new(&bytes)).unwrap();
        assert_eq!(tagged.tag, TAG_I);
        assert_eq!(tagged.bytes.len(), 9);
    }

    #[test]
    fn rejects_a_non_i_operand_wider_than_a_machine_word() {
        // Only `i` may be negative or wider than a word; every other tag overflows to
        // the C's valueless `TAG_o` (`beam_file.c:228-233`), which is a malformed image
        // here (the parser rejects it before any tag-specific work).
        for tag in [0u8, 2, 3, 4, 5, 6, 7] {
            let bytes = tagged_wide(tag, &[0xFF; 9]);
            assert_bad_arg(decode_operand(
                &mut beam_codec::Reader::new(&bytes),
                &[],
                &fallback_types(),
            ));
        }
    }

    #[test]
    fn rejects_a_non_normalized_wide_integer_operand() {
        // Two leading zero bytes after negation mean the field was not normalized (a
        // corrupt file); `marshal_integer` returns 0 (`beam_file.c:2190-2202`).
        let bytes = tagged_wide(1, &[0u8; 9]);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&bytes),
            &[],
            &fallback_types(),
        ));
    }

    #[test]
    fn decodes_atoms_and_the_nil_placeholder() {
        let mut table = AtomTable::new();
        let slice = table.intern("slice");
        let add = table.intern("add");
        let atoms = [Atom::MAX, slice, add];

        // `a,0` is NIL, not `atoms[0]`.
        assert_eq!(
            decode_operand(
                &mut beam_codec::Reader::new(&tagged(2, 0)),
                &atoms,
                &fallback_types()
            )
            .unwrap(),
            Operand::Nil
        );
        assert_eq!(
            decode_operand(
                &mut beam_codec::Reader::new(&tagged(2, 2)),
                &atoms,
                &fallback_types()
            )
            .unwrap(),
            Operand::Atom(add)
        );
    }

    #[test]
    fn rejects_an_out_of_range_atom_index() {
        // `a,1` with an empty atom table.
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&tagged(2, 1)),
            &[],
            &fallback_types(),
        ));
    }

    #[test]
    fn decodes_register_label_and_character_tags() {
        let cases: &[(&[u8], Operand)] = &[
            (&tagged(3, 0), Operand::X(0)),
            (&tagged(3, 12), Operand::X(12)),
            (&tagged(4, 1), Operand::Y(1)),
            (&tagged(5, 0), Operand::Label(NO_FAIL)),
            (&tagged(5, 8), Operand::Label(Label(8))),
            (&tagged(6, 0), Operand::Character(0)),
            (&tagged(6, 65), Operand::Character(65)),
        ];
        for (bytes, expected) in cases {
            assert_eq!(
                &decode_operand(&mut beam_codec::Reader::new(bytes), &[], &fallback_types())
                    .unwrap(),
                expected,
                "bytes {bytes:02x?}"
            );
        }
    }

    // ---- `z` extended sub-tags -------------------------------------------

    #[test]
    fn decodes_an_argument_list() {
        // `z,1` (0x17) `u,2` (0x20) `x0` (0x03) `y1` (0x14).
        let bytes = stream(&[&tagged(7, 1), &tagged(0, 2), &tagged(3, 0), &tagged(4, 1)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&bytes), &[], &fallback_types()).unwrap(),
            Operand::list(vec![Operand::X(0), Operand::Y(1)])
        );

        // An empty list is legal framing-wise.
        let empty = stream(&[&tagged(7, 1), &tagged(0, 0)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&empty), &[], &fallback_types()).unwrap(),
            Operand::list(Vec::new())
        );
    }

    #[test]
    fn rejects_a_truncated_argument_list() {
        // `z,1` announcing 2 arguments but carrying one: the nested decode runs
        // out of bytes.
        let bytes = stream(&[&tagged(7, 1), &tagged(0, 2), &tagged(3, 0)]);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&bytes),
            &[],
            &fallback_types(),
        ));
    }

    #[test]
    fn decodes_a_float_register() {
        // `z,2` (0x27) `u,1` (0x10).
        let bytes = stream(&[&tagged(7, 2), &tagged(0, 1)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&bytes), &[], &fallback_types()).unwrap(),
            Operand::FloatReg(1)
        );
    }

    #[test]
    fn decodes_an_allocation_list_with_heap_words() {
        // `z,3` (0x37), count `u,4` (0x40 = in-nibble form), then the
        // `(kind, number)` pairs (0,3) (1,2) (2,5) (3,7).
        let bytes = stream(&[
            &tagged(7, 3),
            &tagged(0, 4),
            &tagged(0, 0),
            &tagged(0, 3),
            &tagged(0, 1),
            &tagged(0, 2),
            &tagged(0, 2),
            &tagged(0, 5),
            &tagged(0, 3),
            &tagged(0, 7),
        ]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&bytes), &[], &fallback_types()).unwrap(),
            Operand::alloc(vec![
                // kind 0: raw heap words.
                AllocKind {
                    kind: 0,
                    number: 3,
                    words: 3
                },
                // kind 1: floats, `FLOAT_SIZE_OBJECT` = 2 words each.
                AllocKind {
                    kind: 1,
                    number: 2,
                    words: 4
                },
                // kind 2: funs, `ERL_FUN_SIZE` = 2 words each.
                AllocKind {
                    kind: 2,
                    number: 5,
                    words: 10
                },
                // kind 3: native records, `RECORD_INST_SIZE(0)` = 2 words each.
                AllocKind {
                    kind: 3,
                    number: 7,
                    words: 14
                },
            ])
        );
    }

    #[test]
    fn rejects_an_invalid_allocation_kind() {
        // `z,3` count 1, then kind 4 (unknown).
        let bytes = stream(&[&tagged(7, 3), &tagged(0, 1), &tagged(0, 4), &tagged(0, 1)]);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&bytes),
            &[],
            &fallback_types(),
        ));
    }

    #[test]
    fn allocation_entry_counts_funs_and_records_at_two_words_each() {
        // `ERL_FUN_SIZE = sizeof(ErlFunThing)/sizeof(Eterm)` (`erl_fun.h:99`) and
        // `RECORD_INST_SIZE(0) = sizeof(ErtsRecordInstance)/sizeof(Eterm)`
        // (`erl_record.h:73-74`) are both 2 words on the 64-bit emulator, exactly like
        // `FLOAT_SIZE_OBJECT`, and `marshal_allocation_list` multiplies `number` by
        // them (`beam_file.c:2264-2275`).
        for kind in 0..=3 {
            let entry = allocation_entry(kind, 5).unwrap();
            assert_eq!(entry.kind, kind as u32, "the raw kind is preserved");
            assert_eq!(entry.number, 5, "the raw count is preserved");
            assert_eq!(entry.words, if kind == 0 { 5 } else { 10 }, "kind {kind}");
        }
        // A zero count contributes nothing, whatever the kind.
        for kind in 0..=3 {
            assert_eq!(allocation_entry(kind, 0).unwrap().words, 0, "kind {kind}");
        }
    }

    #[test]
    fn allocation_entry_bounds_the_count_and_rejects_a_bad_kind() {
        // The per-entry bound is the C's `ERTS_SINT32_MAX / MAX(FLOAT_SIZE_OBJECT,
        // ERL_FUN_SIZE + 1)` (`beam_file.c:2255-2256`) — `i32::MAX / 3` on the 64-bit
        // emulator, the factor `3` being the largest multiplier (`ERL_FUN_SIZE + 1`).
        // It exists so the running word total stays inside a signed `Sint`.
        const MAX: u64 = (i32::MAX as u64) / 3;
        assert_eq!(MAX, MAX_ALLOC_NUMBER, "the pinned bound agrees with the decoder");

        // Kind `0` counts raw words: the bound itself is accepted and the word count is
        // the raw number.
        assert_eq!(allocation_entry(0, MAX).unwrap().words, MAX as u32);
        // Kinds `1`/`2`/`3` multiply by 2 (`FLOAT_SIZE_OBJECT` / `ERL_FUN_SIZE` /
        // `RECORD_INST_SIZE(0)`), exactly and without overflow at the bound.
        for kind in 1..=3u64 {
            let entry = allocation_entry(kind, MAX).unwrap();
            assert_eq!(u64::from(entry.kind), kind, "kind {kind}: the raw kind is preserved");
            assert_eq!(
                u64::from(entry.number),
                MAX,
                "kind {kind}: the raw count is preserved"
            );
            assert_eq!(
                u64::from(entry.words),
                MAX * 2,
                "kind {kind}: two words per element"
            );
        }
        // One past the per-element bound is a system limit (not a `badarg`), whatever the
        // kind — the C's `LoadAssert(number <= ERTS_SINT32_MAX / MAX(...))`.
        for kind in 0..=3 {
            assert!(
                matches!(
                    allocation_entry(kind, MAX + 1),
                    Err(beam_common::BeamError::SystemLimit)
                ),
                "kind {kind}: one past the per-element bound is a system limit"
            );
        }
        // Kind 4 (and anything else) is malformed.
        assert_bad_arg(allocation_entry(4, 1));
    }

    #[test]
    fn recovers_the_c_word_total_from_an_allocation_list() {
        // `test_heap/2` (16) with a `z,3` list of `{funs,3}` and native
        // `{records,4}`, `live 1`; then `allocate_heap/3` (13) with a `z,3`
        // stack-need list of `{funs,2}`, heap need `u,1`, `live 1`.
        let bytes = stream(&[
            &[OP_TEST_HEAP],
            &tagged(7, 3),
            &tagged(0, 2),
            &tagged(0, 2),
            &tagged(0, 3),
            &tagged(0, 3),
            &tagged(0, 4),
            &tagged(0, 1),
            &[OP_ALLOCATE_HEAP],
            &tagged(7, 3),
            &tagged(0, 1),
            &tagged(0, 2),
            &tagged(0, 2),
            &tagged(0, 1),
            &tagged(0, 1),
            &[OP_INT_CODE_END],
        ]);
        let instrs = decode_stream(&bytes, &[], &VERSION, &fallback_types()).unwrap();

        // `ERL_FUN_SIZE×3 + RECORD_INST_SIZE(0)×4` = 2×3 + 2×4 = 14 — the word total
        // `marshal_allocation_list` reserves for the same list (`beam_file.c:2259-2284`).
        assert_eq!(instrs[0].opcode, Opcode::test_heap);
        assert_eq!(
            HeapNeed::from_operand(&instrs[0].operands[0]).unwrap(),
            HeapNeed::Alloc(vec![
                AllocKind {
                    kind: 2,
                    number: 3,
                    words: 6,
                },
                AllocKind {
                    kind: 3,
                    number: 4,
                    words: 8,
                },
            ])
        );
        assert_eq!(
            HeapNeed::from_operand(&instrs[0].operands[0])
                .unwrap()
                .words(),
            14,
            "the reserved word count is the C total, not the raw element count"
        );
        assert_eq!(instrs[0].operands[1], Operand::Integer(1));

        // `allocate_heap`'s stack need: `ERL_FUN_SIZE×2` = 4 words.
        assert_eq!(instrs[1].opcode, Opcode::allocate_heap);
        let stack_need = HeapNeed::from_operand(&instrs[1].operands[0]).unwrap();
        assert_eq!(stack_need.words(), 4);
        assert_eq!(
            stack_need,
            HeapNeed::Alloc(vec![AllocKind {
                kind: 2,
                number: 2,
                words: 4,
            }])
        );
        assert_eq!(instrs[1].operands[1], Operand::Integer(1));
        assert_eq!(instrs[1].operands[2], Operand::Integer(1));
    }

    #[test]
    fn decodes_a_static_literal() {
        // `z,4` (0x47) `u,7` (0x70).
        let bytes = stream(&[&tagged(7, 4), &tagged(0, 7)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&bytes), &[], &fallback_types()).unwrap(),
            Operand::Literal(7)
        );
    }

    #[test]
    fn collapses_a_typed_register_when_types_are_a_fallback() {
        // `z,5` (0x57) `x1` (0x13) type index `u,1` (0x10).
        let x = stream(&[&tagged(7, 5), &tagged(3, 1), &tagged(0, 1)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&x), &[], &fallback_types()).unwrap(),
            Operand::X(1)
        );

        let y = stream(&[&tagged(7, 5), &tagged(4, 2), &tagged(0, 0)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&y), &[], &fallback_types()).unwrap(),
            Operand::Y(2)
        );
    }

    #[test]
    fn keeps_the_type_index_of_a_typed_register() {
        let types = types_with(3);
        // `z,5` `x1` type index `u,2`.
        let x = stream(&[&tagged(7, 5), &tagged(3, 1), &tagged(0, 2)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&x), &[], &types).unwrap(),
            Operand::Typed {
                reg: Reg::X(1),
                ty: TypeIndex(2)
            }
        );

        // The "any type" index 0.
        let any = stream(&[&tagged(7, 5), &tagged(4, 0), &tagged(0, 0)]);
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&any), &[], &types).unwrap(),
            Operand::Typed {
                reg: Reg::Y(0),
                ty: TypeIndex(0)
            }
        );
    }

    #[test]
    fn rejects_a_typed_register_that_is_not_a_register() {
        // `z,5` `a,1` type index `u,0` — `tr` may only annotate x/y.
        let mut table = AtomTable::new();
        let atom = table.intern("slice");
        let bytes = stream(&[&tagged(7, 5), &tagged(2, 1), &tagged(0, 0)]);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&bytes),
            &[Atom::MAX, atom],
            &fallback_types(),
        ));
    }

    #[test]
    fn rejects_the_obsolete_and_unknown_z_sub_tags() {
        // Sub-tag 0: the pre-R16B float byte form.
        let obsolete = tagged(7, 0);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&obsolete),
            &[],
            &fallback_types(),
        ));

        // Sub-tag 6 and beyond are unassigned.
        for sub_tag in 6..=8u64 {
            let bytes = tagged(7, sub_tag);
            assert_bad_arg(decode_operand(
                &mut beam_codec::Reader::new(&bytes),
                &[],
                &fallback_types(),
            ));
        }
    }

    // ---- operand range checks (the C's `LoadAssert`s) ----------------------
    //
    // Every bound below mirrors an assertion the C reader makes while it fills in one
    // operand (`beam_file.c:2239-2443`). Each negative case sits next to a *control*
    // stream that decodes successfully, so a rejection can only come from the bound
    // under test.

    /// A `z,1` argument list operand carrying `items`.
    fn arg_list(items: &[&[u8]]) -> Vec<u8> {
        let mut out = tagged(7, 1);
        out.extend(tagged(0, items.len() as u64));
        for item in items {
            out.extend_from_slice(item);
        }
        out
    }

    /// Decodes `bytes` with the module-level [`VERSION`], panicking (with the typed
    /// error) if it fails — the control half of a range-check case.
    #[track_caller]
    fn decode_ok(bytes: &[u8]) -> Vec<RawInstr> {
        decode_stream(bytes, &[], &VERSION, &fallback_types())
            .unwrap_or_else(|error| panic!("the control stream must decode, got {error:?}"))
    }

    /// Asserts the [`VERSION`]-headed decode of `bytes` fails with the loader's `badarg`.
    #[track_caller]
    fn assert_decode_rejects(bytes: &[u8]) {
        assert_bad_arg(decode_stream(bytes, &[], &VERSION, &fallback_types()));
    }

    #[test]
    fn rejects_a_label_index_at_or_past_label_count() {
        // `LoadAssert(raw_arg.word_value < code->label_count)` for every non-zero `f`
        // operand (`beam_file.c:2324-2331`); `f,0` is the `NO_FAIL` sentinel.
        // `is_eq_exact/3` (43) `Fail Arg1 Arg2`, `VERSION.label_count` is 15.
        let build = |label: u64| {
            stream(&[
                &[OP_IS_EQ_EXACT],
                &tagged(5, label),
                &tagged(3, 0),
                &tagged(3, 0),
                &[OP_INT_CODE_END],
            ])
        };
        for label in [15u64, 16, 1000] {
            assert_decode_rejects(&build(label));
        }
        // Control: the sentinel (`f,0`) and the largest legal index (`label_count - 1`)
        // both decode.
        for label in [0u64, 1, 14] {
            let instrs = decode_ok(&build(label));
            assert_eq!(instrs[0].operands[0], Operand::Label(Label(label as u32)));
        }
    }

    #[test]
    fn rejects_a_label_inside_an_argument_list_that_is_out_of_range() {
        // The bound applies to a label in a `z,1` argument list too — a list's elements
        // *are* the instruction's trailing operands. `select_val/3` (59) carries its
        // `Value,Label` alternatives as one such list.
        let build = |label: u64| {
            let mut bytes = vec![Opcode::select_val.as_u8()];
            bytes.extend(tagged(3, 0)); // Value = x0
            bytes.extend(tagged(5, 0)); // Fail = `f,0`
            bytes.extend(arg_list(&[&tagged(0, 1), &tagged(5, label)]));
            bytes.push(OP_INT_CODE_END);
            bytes
        };
        assert_decode_rejects(&build(15));
        // Control: the same instruction with an in-range alternative label decodes.
        let instrs = decode_ok(&build(14));
        assert_eq!(
            instrs[0].operands[2],
            Operand::list(vec![Operand::Integer(1), Operand::Label(Label(14))])
        );
    }

    #[test]
    fn rejects_a_register_index_at_or_past_max_reg() {
        // `raw_arg.word_value < MAX_REG` for `x`/`y` (`beam_file.c:2335-2338`,
        // `erl_vm.h:49`). `move/2` (64) `Src Dst`.
        let build = |tag: u8, index: u64| {
            stream(&[
                &[OP_MOVE],
                &tagged(tag, index),
                &tagged(3, 0),
                &[OP_INT_CODE_END],
            ])
        };
        for tag in [3u8, 4] {
            assert_decode_rejects(&build(tag, MAX_REG));
        }
        // Control: `MAX_REG - 1` is the highest legal register index.
        for tag in [3u8, 4] {
            decode_ok(&build(tag, MAX_REG - 1));
        }
    }

    #[test]
    fn rejects_a_float_register_index_at_or_past_max_reg() {
        // `z,2` shares the `MAX_REG` bound (`beam_file.c:2388-2395`).
        let build = |index: u64| stream(&[&tagged(7, 2), &tagged(0, index)]);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&build(MAX_REG)),
            &[],
            &fallback_types(),
        ));
        // Control: `MAX_REG - 1` decodes.
        assert_eq!(
            decode_operand(
                &mut beam_codec::Reader::new(&build(MAX_REG - 1)),
                &[],
                &fallback_types()
            )
            .unwrap(),
            Operand::FloatReg(1023)
        );
    }

    #[test]
    fn rejects_a_type_index_at_or_past_the_type_count() {
        // `LoadAssert(index.word_value < types->count)` while the `Type` chunk is
        // present (`beam_file.c:2436-2437`); `z,5` `x0` plus the index.
        let types = types_with(3);
        let build = |index: u64| stream(&[&tagged(7, 5), &tagged(3, 0), &tagged(0, index)]);
        assert_bad_arg(decode_operand(
            &mut beam_codec::Reader::new(&build(3)),
            &[],
            &types,
        ));
        // Control: the table's last entry decodes to a typed register …
        assert_eq!(
            decode_operand(&mut beam_codec::Reader::new(&build(2)), &[], &types).unwrap(),
            Operand::Typed {
                reg: Reg::X(0),
                ty: TypeIndex(2)
            }
        );
        // … and with a *fallback* table the annotation is dropped, so no index can be
        // out of range (`beam_file.c:2433-2435`).
        assert_eq!(
            decode_operand(
                &mut beam_codec::Reader::new(&build(9999)),
                &[],
                &fallback_types()
            )
            .unwrap(),
            Operand::X(0)
        );
    }

    #[test]
    fn rejects_an_allocation_list_longer_than_the_four_kinds() {
        // `LoadAssert(count.word_value <= 4)` (`beam_file.c:2241`): there are only four
        // allocation kinds, so a longer list is malformed framing. `test_heap/2` (16)
        // `Need Live`.
        let build = |entries: usize| {
            let mut bytes = vec![OP_TEST_HEAP];
            bytes.extend(tagged(7, 3));
            bytes.extend(tagged(0, entries as u64));
            for _ in 0..entries {
                bytes.extend(tagged(0, 0)); // kind 0 (raw words)
                bytes.extend(tagged(0, 1)); // one word
            }
            bytes.extend(tagged(0, 1)); // Live
            bytes.push(OP_INT_CODE_END);
            bytes
        };
        assert_decode_rejects(&build(5));
        // Control: exactly four entries (one per kind) decode.
        decode_ok(&build(4));
    }

    #[test]
    fn rejects_an_argument_list_that_is_not_the_last_operand() {
        // `LoadAssert((i + 1) == arity)` (`beam_file.c:2352`): a `z,1` argument list is
        // always the instruction's *last* operand. `put_tuple2/2` (164) `Dst Elements`.
        let mut bad = vec![Opcode::put_tuple2.as_u8()];
        bad.extend(arg_list(&[&tagged(3, 0)])); // the list in the FIRST of two slots
        bad.extend(tagged(3, 1));
        bad.push(OP_INT_CODE_END);
        assert_decode_rejects(&bad);

        // Control: the same list in the last slot (`Elements`) decodes.
        let mut ok = vec![Opcode::put_tuple2.as_u8()];
        ok.extend(tagged(3, 0));
        ok.extend(arg_list(&[&tagged(3, 1)]));
        ok.push(OP_INT_CODE_END);
        decode_ok(&ok);
    }

    // ---- TestKind::from_opcode -------------------------------------------

    #[test]
    fn test_kind_maps_the_is_family() {
        let cases: &[(Opcode, TestKind)] = &[
            (Opcode::is_lt, TestKind::IsLt),
            (Opcode::is_ge, TestKind::IsGe),
            (Opcode::is_eq, TestKind::IsEq),
            (Opcode::is_ne, TestKind::IsNe),
            (Opcode::is_eq_exact, TestKind::IsEqExact),
            (Opcode::is_ne_exact, TestKind::IsNeExact),
            (Opcode::is_integer, TestKind::IsInteger),
            (Opcode::is_float, TestKind::IsFloat),
            (Opcode::is_number, TestKind::IsNumber),
            (Opcode::is_atom, TestKind::IsAtom),
            (Opcode::is_pid, TestKind::IsPid),
            (Opcode::is_reference, TestKind::IsReference),
            (Opcode::is_port, TestKind::IsPort),
            (Opcode::is_nil, TestKind::IsNil),
            (Opcode::is_binary, TestKind::IsBinary),
            (Opcode::is_bitstr, TestKind::IsBitstr),
            (Opcode::is_list, TestKind::IsList),
            (Opcode::is_nonempty_list, TestKind::IsNonemptyList),
            (Opcode::is_tuple, TestKind::IsTuple),
            (Opcode::is_map, TestKind::IsMap),
            (Opcode::is_boolean, TestKind::IsBoolean),
            (Opcode::is_function, TestKind::IsFunction),
            (Opcode::is_function2, TestKind::IsFunction2),
            (Opcode::is_tagged_tuple, TestKind::IsTaggedTuple),
            (Opcode::test_arity, TestKind::TestArity),
        ];
        for (op, kind) in cases {
            assert_eq!(&TestKind::from_opcode(*op), &Some(*kind), "{}", op.name());
        }
    }

    #[test]
    fn test_kind_is_none_for_every_other_opcode() {
        // A handful of neighbours and non-test opcodes must not be classified.
        for op in [
            Opcode::r#move,
            Opcode::label,
            Opcode::int_code_end,
            Opcode::func_info,
            Opcode::is_constant,
            Opcode::is_record_accessible,
            Opcode::gc_bif2,
            Opcode::select_val,
        ] {
            assert_eq!(TestKind::from_opcode(op), None, "{}", op.name());
        }

        // Exhaustively: `from_opcode` is `Some` for exactly the 25 test opcodes.
        let classified = crate::opcode::OPCODE_TABLE
            .iter()
            .filter(|(_, op, _, _)| TestKind::from_opcode(*op).is_some())
            .count();
        assert_eq!(classified, 25);
    }

    // ---- the derived arity cache -----------------------------------------

    /// The derived O(1) arity table is a *cache* of the roster, so it must agree with
    /// `Opcode::arity` for every byte a `.beam` file can carry — and answer `0` (never
    /// a stale value) for the bytes outside the roster, which [`decode_stream`]
    /// rejects before it ever consults the table.
    #[test]
    fn derived_arity_table_matches_the_roster() {
        for value in 0u16..=255 {
            let byte = value as u8;
            let expected = Opcode::from_u8(byte).map_or(0, |opcode| opcode.arity());
            assert_eq!(
                opcode_arity(byte),
                expected,
                "the derived arity of byte {byte} disagrees with the roster"
            );
        }
        // A byte the roster does not know must never look like a real arity: byte 0 is
        // not an opcode (`decode_stream` rejects it as a zero opcode byte).
        assert_eq!(opcode_arity(0), 0);
        assert!(Opcode::from_u8(0).is_none());
    }
}
