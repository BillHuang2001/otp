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

//! The `Recs` chunk: compile-time native-record definitions.
//!
//! `Recs` carries the definitions of a module's **native records** (OTP 29's
//! `-record #name{…}` syntax). It is written by `lib/compiler/src/beam_asm.erl`
//! (`build_record_chunk`, `:577-639`) and read by the C loader
//! (`parse_record_chunk` / `parse_record_chunk_data`, `beam_file.c:1077-1348`).
//! The definitions are the source the `records:get_definition/2` BIF serves; this
//! module parses them into the raw [`RawRecord`] form, which [`crate::lower`] maps
//! onto [`crate::ir::RecordDefinition`].
//!
//! # Wire layout (BIG-ENDIAN, verified against the oracle)
//!
//! ```text
//! u32 version             // must be 0; a non-zero version means "no records"
//! i32 record_count        // >= 0
//! i32 total_field_count   // sum of the records' field counts (informational)
//! record entry *          // record_count of them, each encoded as one instruction:
//!   u8  opcode            // == Opcode::call_last (5), reused only for its 3 operands
//!   <operand> name        // an atom operand (the record name)
//!   <operand> exported    // an atom operand: `true` or `false`
//!   <operand> fields      // a `z,1` argument list: `u` count N (even) then N
//!                         // operands = N/2 (`atom name`, `default`) pairs
//! ```
//!
//! A `z,1` list operand decodes to the C's flattened `TAG_u count` + elements form
//! (`beamcodereader_next`), which is what the C reads here.
//!
//! The record's own `module` is the module being loaded — the chunk carries no module
//! field (the C sets `tmp_def->module = beam->module`).
//!
//! # Field defaults, and the `u`/`i` ambiguity
//!
//! A field's default operand is one of (`beam_file.c:1200-1216`):
//!
//! | operand | meaning |
//! |---------|---------|
//! | `u` (`u,0`) | **no default** — the C's absent sentinel `make_catch(0)` |
//! | `i` | a signed small-integer default |
//! | `i`, wider than a machine word | a signed **bignum** default (raw two's-complement field) |
//! | `a` | an atom default; wire index `0` means `nil`/`[]` |
//! | `z,4` | an index into the module's static `LitT` literals |
//!
//! The instruction decoder ([`crate::decode::decode_operand`]) collapses both the `u`
//! and `i` tags to `Operand::Integer`, so `u,0` ("no default") and `i,0` ("default is
//! the integer `0`") would be indistinguishable. Defaults are therefore decoded here
//! from the raw tag via the shared [`crate::decode::read_tagged`], never through
//! `decode_operand`.
//!
//! A malformed or truncated chunk yields [`beam_common::BeamError::BadArg`] (through
//! [`crate::error::bad_file`]); the parser never panics and never `unwrap()`s file
//! data.
use beam_common::{BeamError, Result};

use crate::decode::{bignum_sign_magnitude, read_tagged, read_unsigned};
use crate::error::bad_file;
use crate::opcode::Opcode;

/// The `u` (unsigned) operand tag.
const TAG_U: u8 = 0;
/// The `i` (signed) operand tag.
const TAG_I: u8 = 1;
/// The `a` (atom) operand tag.
const TAG_A: u8 = 2;
/// The `z` (extended) operand tag.
const TAG_Z: u8 = 7;

/// The `z,1` argument-list sub-tag (the record's field list).
const Z_LIST: i64 = 1;
/// The `z,4` static-literal sub-tag (a field default held as a literal).
const Z_LITERAL: i64 = 4;

/// The `Recs` chunk version this loader understands.
const RECORD_VERSION: u32 = 0;

/// One field of a raw `Recs` record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RawRecordField {
    /// The field-name atom's **wire** index (not an interned atom).
    pub name: u32,
    /// The field's default value (or its absence).
    pub default: RawRecordDefault,
}

/// A record field's default, in the exact form the file encodes it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RawRecordDefault {
    /// The field has no default (the `u,0` absent sentinel).
    Absent,
    /// A signed small-integer default.
    Integer(i64),
    /// An atom default; wire index `0` means `nil`/`[]`.
    Atom(u32),
    /// An index into the module's static `LitT` literals.
    Literal(u32),
    /// A signed bignum default: `magnitude` is **big-endian** (as in
    /// [`crate::operand::Operand::BigInteger`]), `negative` its sign.
    BigInteger {
        /// Whether the value is negative.
        negative: bool,
        /// The big-endian, normalized magnitude (the sign is carried separately).
        magnitude: Vec<u8>,
    },
}

/// One raw `Recs` record definition (wire atom indices, not interned atoms).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RawRecord {
    /// The record-name atom's **wire** index.
    pub name: u32,
    /// Whether the record is exported (`-export_record/1`).
    pub is_exported: bool,
    /// The fields, in **declaration order** (the file order).
    pub fields: Vec<RawRecordField>,
}

/// Parses a `Recs` chunk payload into its raw record definitions.
///
/// `atoms` maps a wire atom index to its name (`atoms[0]` is the NIL placeholder);
/// it is needed to recognize the `true`/`false` export flag, exactly as the C compares
/// against the interned `am_true`/`am_false`.
///
/// A non-zero chunk version yields an empty vector (the C treats it as "no record
/// table", `beam_file.c:1343-1347`). A negative count, a wrong opcode byte, a
/// non-atom name, an odd field-list length, an unsupported operand tag or a truncated
/// payload yields [`BeamError::BadArg`]. Never panics.
pub fn parse(data: &[u8], atoms: &[String]) -> Result<Vec<RawRecord>> {
    let mut r = beam_codec::Reader::new(data);

    let version = r.u32be()?;
    if version != RECORD_VERSION {
        // The C treats a non-zero version as chunk-absent, *not* as an error
        // (`parse_record_chunk`, `beam_file.c:1335-1348`); mirror it.
        return Ok(Vec::new());
    }

    let record_count = r.u32be()? as i32;
    if record_count < 0 {
        return Err(bad_file("negative record count"));
    }
    let total_field_count = r.u32be()? as i32;
    if total_field_count < 0 {
        return Err(bad_file("negative record field count"));
    }

    // The count is file-supplied: do *not* pre-allocate with it, so a bogus count
    // fails on the first short read instead of reserving an attacker-chosen amount.
    let mut records = Vec::new();
    for _ in 0..record_count {
        records.push(parse_record(&mut r, atoms)?);
    }
    Ok(records)
}

/// Parses one record entry (one `call_last`-shaped instruction).
fn parse_record(r: &mut beam_codec::Reader<'_>, atoms: &[String]) -> Result<RawRecord> {
    let opcode = r.u8()?;
    if opcode != Opcode::call_last.as_u8() {
        return Err(bad_file("`Recs` entry is not a `call_last` instruction"));
    }

    let name = read_atom_index(r, "record name is not an atom")?;

    let flag = read_atom_index(r, "record export flag is not an atom")?;
    let is_exported = match atoms.get(flag as usize).map(String::as_str) {
        Some("true") => true,
        Some("false") => false,
        _ => return Err(bad_file("record export flag is neither `true` nor `false`")),
    };

    // Operand 3: the `z,1` argument list holding the `(name, default)` pairs.
    let list = read_tagged(r)?;
    if list.tag != TAG_Z || !list.bytes.is_empty() || list.word != Z_LIST {
        return Err(bad_file("record fields are not a `z,1` argument list"));
    }
    let count = read_unsigned(r, "record field count is not unsigned")?;
    if count % 2 != 0 {
        return Err(bad_file("record field list has an odd length"));
    }

    let mut fields = Vec::new();
    for _ in 0..count / 2 {
        let name = read_atom_index(r, "record field name is not an atom")?;
        let default = read_default(r)?;
        fields.push(RawRecordField { name, default });
    }

    Ok(RawRecord {
        name,
        is_exported,
        fields,
    })
}

/// Reads an `a`-tagged operand and returns its **wire** atom index.
///
/// Index `0` — the NIL placeholder — is not an atom, so it is rejected (the C's
/// `is_atom(arg->val)` test fails for it too).
fn read_atom_index(r: &mut beam_codec::Reader<'_>, msg: &str) -> Result<u32> {
    let tagged = read_tagged(r)?;
    if tagged.tag != TAG_A || !tagged.bytes.is_empty() || tagged.word < 1 {
        return Err(bad_file(msg));
    }
    u32::try_from(tagged.word).map_err(|_| bad_file(msg))
}

/// Reads and classifies one field-default operand from its raw tag.
fn read_default(r: &mut beam_codec::Reader<'_>) -> Result<RawRecordDefault> {
    let tagged = read_tagged(r)?;
    match tagged.tag {
        // `u` — the absent sentinel. The C maps *any* `u` operand to `make_catch(0)`
        // (the compiler only ever emits `u,0`), so `u` means "no default".
        TAG_U => Ok(RawRecordDefault::Absent),
        // `i` — a signed integer: an inline small, or a wide two's-complement field
        // (a bignum) when it did not fit a machine word.
        TAG_I => {
            if tagged.bytes.is_empty() {
                Ok(RawRecordDefault::Integer(tagged.word))
            } else {
                let (negative, magnitude) = bignum_sign_magnitude(tagged.bytes)?;
                Ok(RawRecordDefault::BigInteger {
                    negative,
                    magnitude,
                })
            }
        }
        // `a` — an atom; wire index 0 is `nil`.
        TAG_A => {
            if !tagged.bytes.is_empty() {
                return Err(bad_file("record default atom index is too wide"));
            }
            let index = u32::try_from(tagged.word)
                .map_err(|_| bad_file("record default atom index is out of range"))?;
            Ok(RawRecordDefault::Atom(index))
        }
        // `z,4` — an index into the module's static `LitT` literals.
        TAG_Z => {
            if !tagged.bytes.is_empty() || tagged.word != Z_LITERAL {
                return Err(bad_file("record default uses an unsupported `z` sub-tag"));
            }
            let index = read_unsigned(r, "record default literal index is not unsigned")?;
            if index > u64::from(u32::MAX) {
                return Err(BeamError::SystemLimit);
            }
            Ok(RawRecordDefault::Literal(index as u32))
        }
        _ => Err(bad_file("record default has an unsupported operand tag")),
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests over synthetic `Recs` payloads: the field/default encodings, the
    //! version-0 rule, and every structural rejection. Tests are allowed to `unwrap`
    //! (a panic there is a test failure).

    use super::*;

    /// Packs one `(tag, value)` operand into the file's tag/length byte plus
    /// magnitude bytes (`beam_file.c:167-240`); mirrors the encoder in
    /// [`crate::decode`]'s tests.
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

    /// An `a`-tagged atom operand at wire index `index`.
    fn atom(index: u32) -> Vec<u8> {
        tagged(TAG_A, u64::from(index))
    }

    /// Packs a signed `i`-tagged small integer. Non-negative values use the same
    /// encodings as [`tagged`]; a negative one uses the explicit-count form the
    /// compiler emits (the 1-byte-tail form is unsigned).
    fn signed(value: i64) -> Vec<u8> {
        if value >= 0 {
            return tagged(TAG_I, value as u64);
        }
        for count in 2..=8usize {
            let mut field = value.to_be_bytes().to_vec();
            let bytes = field.split_off(8 - count);
            let mut raw: u64 = 0;
            for byte in &bytes {
                raw = (raw << 8) | u64::from(*byte);
            }
            let shift = 64 - 8 * count as u32;
            if ((raw << shift) as i64) >> shift == value {
                let mut out = vec![0x18 | (((count - 2) as u8) << 5) | TAG_I];
                out.extend(bytes);
                return out;
            }
        }
        panic!("{value} does not fit an 8-byte `i` field");
    }

    /// An explicit-count `i` field wide enough to be a bignum: the tag byte
    /// `0x18 | (7 << 5)` (both length bits set and the `7` count selector), the
    /// `u`-tagged sub-length (`field.len() - 9`) and the raw big-endian field. This is
    /// exactly how the compiler encodes a bignum default.
    fn wide_i(field: &[u8]) -> Vec<u8> {
        assert!(field.len() >= 9, "a wide field is at least 9 bytes");
        let mut out = vec![0x18 | (7 << 5) | TAG_I];
        out.extend(tagged(TAG_U, (field.len() - 9) as u64));
        out.extend_from_slice(field);
        out
    }

    /// The atom-name table for a list of names (`index 0` is the NIL placeholder).
    fn names(list: &[&str]) -> Vec<String> {
        let mut table = vec![String::new()];
        table.extend(list.iter().map(|name| (*name).to_string()));
        table
    }

    /// One record entry: `call_last name flag (z,1 (count) field… )`.
    fn record(name: u32, flag: u32, fields: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = vec![Opcode::call_last.as_u8()];
        out.extend(atom(name));
        out.extend(atom(flag));
        out.extend(tagged(TAG_Z, Z_LIST as u64));
        out.extend(tagged(TAG_U, (fields.len() * 2) as u64));
        for (field_name, default) in fields {
            out.extend(atom(*field_name));
            out.extend(default.iter().copied());
        }
        out
    }

    /// A whole `Recs` payload from its header and record entries.
    fn chunk(version: u32, record_count: i32, total: i32, records: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&version.to_be_bytes());
        out.extend_from_slice(&record_count.to_be_bytes());
        out.extend_from_slice(&total.to_be_bytes());
        for entry in records {
            out.extend(entry.iter().copied());
        }
        out
    }

    /// Atom names for the common test table:
    /// `1 m, 2 user, 3 false, 4 true, 5 id, 6 name, 7 city`.
    fn test_names() -> Vec<String> {
        names(&["m", "user", "false", "true", "id", "name", "city"])
    }

    #[test]
    fn parses_a_record_with_every_default_encoding() {
        // `10^25` as the compiler encodes it: a positive 11-byte two's-complement
        // field (`test/../fixtures/records_fixture.beam`'s
        // `#mixed{f = 10000000000000000000000000}`).
        let bignum = [0x08, 0x45, 0x95, 0x16, 0x14, 0x01, 0x48, 0x4a, 0x00, 0x00, 0x00];
        let payload = chunk(
            0,
            1,
            6,
            &[record(
                2,
                4,
                &[
                    // (id, i,-1)
                    (5, signed(-1)),
                    // (name, u,0 — absent)
                    (6, tagged(TAG_U, 0)),
                    // (city, a,7 — the `city` atom)
                    (7, atom(7)),
                    // (opt, z,4 — static literal 3)
                    (5, [tagged(TAG_Z, Z_LITERAL as u64), tagged(TAG_U, 3)].concat()),
                    // (big, a wide `i` field)
                    (6, wide_i(&bignum)),
                    // (nil, a,0 — `nil`/`[]`)
                    (7, atom(0)),
                ],
            )],
        );

        let records = parse(&payload, &test_names()).unwrap();
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.name, 2);
        assert!(record.is_exported);
        assert_eq!(record.fields.len(), 6);
        assert_eq!(record.fields[0].name, 5);
        // `i,-1` — the explicit two-byte two's-complement form.
        assert_eq!(record.fields[0].default, RawRecordDefault::Integer(-1));
        assert_eq!(record.fields[1].default, RawRecordDefault::Absent);
        assert_eq!(record.fields[2].default, RawRecordDefault::Atom(7));
        assert_eq!(record.fields[3].default, RawRecordDefault::Literal(3));
        assert_eq!(
            record.fields[4].default,
            RawRecordDefault::BigInteger {
                negative: false,
                magnitude: bignum.to_vec(),
            }
        );
        assert_eq!(record.fields[5].default, RawRecordDefault::Atom(0));
    }

    #[test]
    fn distinguishes_an_integer_zero_default_from_an_absent_default() {
        // `i,0` (default = 0) and `u,0` (no default) share a magnitude but differ by
        // tag — the whole reason defaults are decoded from the raw tag.
        let payload = chunk(
            0,
            1,
            2,
            &[record(2, 3, &[(5, tagged(TAG_I, 0)), (6, tagged(TAG_U, 0))])],
        );
        let records = parse(&payload, &test_names()).unwrap();
        assert_eq!(records[0].fields[0].default, RawRecordDefault::Integer(0));
        assert_eq!(records[0].fields[1].default, RawRecordDefault::Absent);
    }

    #[test]
    fn parses_an_empty_record_table() {
        assert!(parse(&chunk(0, 0, 0, &[]), &test_names()).unwrap().is_empty());
    }

    #[test]
    fn parses_a_record_with_no_fields() {
        let payload = chunk(0, 1, 0, &[record(2, 3, &[])]); // flag atom `false`
        let records = parse(&payload, &test_names()).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].fields.is_empty());
        assert!(!records[0].is_exported);
    }

    #[test]
    fn a_non_zero_version_yields_no_records() {
        // Mirror the C: a non-zero version is chunk-absent, not an error.
        let payload = chunk(1, 1, 1, &[record(2, 3, &[(5, tagged(TAG_U, 0))])]);
        assert!(parse(&payload, &test_names()).unwrap().is_empty());
        assert!(parse(&chunk(7, 0, 0, &[]), &test_names()).unwrap().is_empty());
    }

    #[test]
    fn a_negative_record_count_is_rejected() {
        let payload = chunk(0, -1, 0, &[]);
        assert!(matches!(
            parse(&payload, &test_names()).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn a_negative_field_count_is_rejected() {
        let payload = chunk(0, 0, -1, &[]);
        assert!(matches!(
            parse(&payload, &test_names()).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn a_wrong_opcode_is_rejected() {
        let mut entry = record(2, 3, &[]);
        entry[0] = Opcode::call_only.as_u8();
        let payload = chunk(0, 1, 0, &[entry]);
        assert!(matches!(
            parse(&payload, &test_names()).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn an_odd_field_list_length_is_rejected() {
        // Hand-build an entry whose list count is odd (3).
        let mut entry = vec![Opcode::call_last.as_u8()];
        entry.extend(atom(2));
        entry.extend(atom(3));
        entry.extend(tagged(TAG_Z, Z_LIST as u64));
        entry.extend(tagged(TAG_U, 3));
        entry.extend(atom(5));
        entry.extend(tagged(TAG_U, 0));
        entry.extend(atom(6));
        let payload = chunk(0, 1, 0, &[entry]);
        assert!(matches!(
            parse(&payload, &test_names()).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn a_non_atom_record_name_is_rejected() {
        let mut entry = vec![Opcode::call_last.as_u8()];
        entry.extend(tagged(TAG_I, 2)); // an integer, not an atom
        entry.extend(atom(3));
        entry.extend(tagged(TAG_Z, Z_LIST as u64));
        entry.extend(tagged(TAG_U, 0));
        let payload = chunk(0, 1, 0, &[entry]);
        assert!(matches!(
            parse(&payload, &test_names()).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn an_export_flag_that_is_not_true_or_false_is_rejected() {
        let payload = chunk(0, 1, 0, &[record(2, 5, &[])]); // flag atom `id`
        assert!(matches!(
            parse(&payload, &test_names()).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn the_records_are_true_or_false() {
        // Flag `3` is `false`, flag `4` is `true` in the test atom table.
        let payload = chunk(0, 2, 0, &[record(2, 3, &[]), record(2, 4, &[])]);
        let records = parse(&payload, &test_names()).unwrap();
        assert_eq!([records[0].is_exported, records[1].is_exported], [false, true]);
    }

    #[test]
    fn an_unknown_z_sub_tag_is_rejected() {
        let mut entry = vec![Opcode::call_last.as_u8()];
        entry.extend(atom(2));
        entry.extend(atom(3));
        entry.extend(tagged(TAG_Z, 5)); // `tr`, not a list
        let payload = chunk(0, 1, 0, &[entry]);
        assert!(matches!(
            parse(&payload, &test_names()).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn an_unsupported_default_tag_is_rejected() {
        // A `y`-register default (tag 4) is not a valid record default.
        let payload = chunk(0, 1, 0, &[record(2, 3, &[(5, tagged(4, 0))])]);
        assert!(matches!(
            parse(&payload, &test_names()).unwrap_err(),
            BeamError::BadArg
        ));
    }

    #[test]
    fn a_truncated_payload_is_rejected() {
        let full = chunk(0, 2, 0, &[record(2, 3, &[(5, tagged(TAG_U, 0))]), record(2, 4, &[])]);
        // Chunks the header only, and chops bytes off the end.
        for len in 0..full.len() {
            assert!(
                matches!(parse(&full[..len], &test_names()).unwrap_err(), BeamError::BadArg),
                "len {len} must be rejected"
            );
        }
        // The full payload parses.
        assert_eq!(parse(&full, &test_names()).unwrap().len(), 2);
    }
}
