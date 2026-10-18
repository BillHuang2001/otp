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

//! The `Type` chunk: compile-time register type annotations.
//!
//! Layout: `u32 version` (`4` current, `3` OTP 27/28; anything else ⇒ fallback),
//! `i32 count` (≥ 1, `entries[0]` is the "any type" entry), then per entry a
//! `u16 type_word` plus an optional `i64 lower`, `i64 upper` and `u8 unit-1`,
//! gated by `type_word` bits 13-15.
//!
//! Version 3 packs the *same* information with the metadata bits shifted one
//! position down, because its type union needs 12 bits and the top three bits of
//! the high byte carry lower/upper/unit instead of lower/upper/unit *and* a type
//! bit:
//!
//! ```text
//!    0mmmtttt_tttttttt   (version 3)
//!           |
//!           v
//!    mmm0tttt_tttttttt   (version 4)
//! ```
//!
//! Every version-3 entry is remapped to the version-4 convention
//! ([`version3_word`]) before its metadata bits are consulted, so both versions
//! decode into the same [`Type`] values and `Type::raw` always follows
//! version 4 (`raw & 0x1FFF` = type union, `raw & 0xE000` = metadata flags).
//!
//! When the chunk is absent or its version is incompatible the loader synthesizes
//! a single "any type" fallback entry ([`TypeTable::default`]), which makes `z,5`
//! `tr` operands collapse to plain registers. A chunk that *is* in a supported version
//! is validated as the C reader validates it (`beam_types_decode_type`,
//! `beam_types.c:35-82`; `parse_type_chunk_data`, `beam_file.c:625-698`): a zero word,
//! a lower/upper bound on something that is neither `INTEGER` nor `FLOAT`, a unit on
//! something that is not a `BITSTRING`, a `count` below 1, and a first entry that is
//! not the unbounded "any type" are all corrupt.
use beam_common::Result;
use crate::error::bad_file;
use crate::ir::{Type, TypeTable};

/// `Type` chunk version 4 — the current format (`BEAM_TYPES_VERSION`).
const BEAM_TYPES_VERSION: u32 = 4;
/// `Type` chunk version 3 — the OTP 27/28 format.
const BEAM_TYPES_VERSION_OLD: u32 = 3;

/// `type_word` bit 13: an `i64` lower bound follows (`BEAM_TYPE_HAS_LOWER_BOUND`).
const HAS_LOWER_BOUND: u16 = 1 << 13;
/// `type_word` bit 14: an `i64` upper bound follows (`BEAM_TYPE_HAS_UPPER_BOUND`).
const HAS_UPPER_BOUND: u16 = 1 << 14;
/// `type_word` bit 15: a `u8` `unit - 1` follows (`BEAM_TYPE_HAS_UNIT`).
const HAS_UNIT: u16 = 1 << 15;

/// The low 13 bits of a `type_word`: the type union (`BEAM_TYPE_ANY`, `beam_types.h:60`).
///
/// The remaining top three bits are the metadata flags (`BEAM_TYPE_METADATA_MASK`,
/// `beam_types.h:68`), tested individually below.
const TYPE_UNION: u16 = BEAM_TYPE_ANY;
/// The "any type" union (`BEAM_TYPE_ANY`, `beam_types.h:60`).
const BEAM_TYPE_ANY: u16 = (1 << 13) - 1;
/// The `BEAM_TYPE_NONE` word (`beam_types.h:44`): a word the C rejects outright.
const BEAM_TYPE_NONE: u16 = 0;
/// `BEAM_TYPE_BITSTRING` (`beam_types.h:47`): the only union a unit may annotate.
const BEAM_TYPE_BITSTRING: u16 = 1 << 1;
/// `BEAM_TYPE_FLOAT | BEAM_TYPE_INTEGER` (`beam_types.h:49,51`): the only unions a
/// bound may annotate.
const BEAM_TYPE_NUMERIC: u16 = (1 << 3) | (1 << 5);

/// The default (unbounded) lower bound of a decoded entry: `MAX_SMALL + 1`
/// (`beam_types_decode_extra`, `beam_types.c:84-92`).
const UNBOUNDED_MIN: i64 = 1 << 59;
/// The default (unbounded) upper bound of a decoded entry: `MIN_SMALL - 1`.
const UNBOUNDED_MAX: i64 = -(1 << 59) - 1;

/// Applies `beam_types_decode_type`'s structural rejections to one `type_word`
/// (`beam_types.c:44-82`) and returns how many extra bytes the entry carries.
///
/// `raw` is always in the **version-4** convention ([`version3_word`] remaps a
/// version-3 entry first), so the metadata bits are 13/14/15 on both paths.
fn validate(raw: u16) -> Result<u8> {
    if raw == BEAM_TYPE_NONE {
        return Err(bad_file("type entry with an empty type word"));
    }
    let union = raw & TYPE_UNION;
    let mut extra = 0u8;
    if raw & HAS_LOWER_BOUND != 0 {
        if union & BEAM_TYPE_NUMERIC == 0 {
            return Err(bad_file("type lower bound on a non-numeric union"));
        }
        extra += 8;
    }
    if raw & HAS_UPPER_BOUND != 0 {
        if union & BEAM_TYPE_NUMERIC == 0 {
            return Err(bad_file("type upper bound on a non-numeric union"));
        }
        extra += 8;
    }
    if raw & HAS_UNIT != 0 {
        if union & BEAM_TYPE_BITSTRING == 0 {
            return Err(bad_file("type unit on a non-bitstring union"));
        }
        extra += 1;
    }
    Ok(extra)
}

/// Checks the "first entry must be the any type" rule of `parse_type_chunk_data`
/// (`beam_file.c:689-691`).
///
/// The C asserts `entries[0].type_union == BEAM_TYPE_ANY` and, with the extra fields
/// decoded, `entries[0].min > entries[0].max` — i.e. the first entry is the *unbounded*
/// `-inf..inf` entry every `tr` index 0 refers to.
fn validate_first_entry(entry: &Type) -> Result<()> {
    if entry.raw & TYPE_UNION != BEAM_TYPE_ANY {
        return Err(bad_file("type table does not start with the any type"));
    }
    let min = entry.lower.unwrap_or(UNBOUNDED_MIN);
    let max = entry.upper.unwrap_or(UNBOUNDED_MAX);
    if min <= max {
        return Err(bad_file("type table's any entry is bounded"));
    }
    Ok(())
}

/// Reconstructs the version-4-convention `type_word` of a version-3 (OTP 27/28)
/// entry from its two wire bytes (`beam_file.c:664-681`).
///
/// Version 3 shifts the metadata bits (`lower`/`upper`/`unit`, version-4 bits
/// 13/14/15) down to bits 4/5/6 of the high byte and widens the type union's
/// high part to four bits (`tttt`), because one type bit was later reclaimed for
/// the native-record type. The remap therefore moves the metadata bits back up
/// (`(b0 << 1) & 0xE0`) and keeps the four type bits in the low nibble — except
/// that an all-ones four-bit union (`0x0F`) is forced to `0x1F` so that the
/// version-3 "any type" (`0xFFF`) becomes the version-4 `BEAM_TYPE_ANY`
/// (`0x1FFF`) instead of the truncated `0x0FFF`.
fn version3_word(b0: u8, b1: u8) -> u16 {
    let extra_bits = ((b0 as u16) << 1) & 0x00E0;
    let mut upper_type_bits = (b0 & 0x0F) as u16;
    if upper_type_bits == 0x0F {
        /* Force to ANY. */
        upper_type_bits = 0x1F;
    }
    ((upper_type_bits | extra_bits) << 8) | b1 as u16
}

/// Parses a `Type` chunk payload into the IR [`TypeTable`].
///
/// An unsupported `version` yields the fallback table (the C's
/// `init_fallback_type_table`, `beam_file.c:700-710`); a supported version is validated
/// and any structural violation — a `count` below 1, a rejected `type_word`, a
/// truncated entry, a first entry that is not the unbounded "any type" — is
/// `BeamError::BadArg` (`parse_type_chunk_data`, `beam_file.c:625-698`).
pub fn parse(data: &[u8]) -> Result<TypeTable> {
    let mut reader = beam_codec::Reader::new(data);

    let version = reader.u32be()?;
    if version != BEAM_TYPES_VERSION && version != BEAM_TYPES_VERSION_OLD {
        // An incompatible type format: fall back to the single "any type" entry.
        return Ok(TypeTable::default());
    }

    let raw_count = reader.u32be()? as i32;
    if raw_count < 0 {
        return Err(bad_file("negative type count"));
    }
    if raw_count < 1 {
        // `LoadAssert(count >= 1)` (`beam_file.c:636`): a present chunk in a supported
        // version must carry at least the "any type" entry — the C errors here rather
        // than falling back (`init_fallback_type_table` is only for an *absent* or
        // version-incompatible chunk, `beam_file.c:700-710`).
        return Err(bad_file("empty type table"));
    }
    let count = raw_count as usize;

    let mut entries = Vec::new();
    for _ in 0..count {
        // Each entry is a `u16 type_word` followed by the optional fields its
        // metadata bits (13/14/15) select, in the order lower, upper, unit
        // (`beam_types_decode_extra`, `beam_types.c:84-105`). A version-3 entry
        // is remapped to the version-4 convention first (`version3_word`).
        let raw = if version == BEAM_TYPES_VERSION {
            reader.u16be()?
        } else {
            version3_word(reader.u8()?, reader.u8()?)
        };
        // `beam_types_decode_type` (`beam_types.c:44-82`) rejects the word *before* the
        // extra bytes it announces are read, so a rejected entry never consumes them.
        validate(raw)?;
        let lower = if raw & HAS_LOWER_BOUND != 0 {
            Some(reader.u64be()? as i64)
        } else {
            None
        };
        let upper = if raw & HAS_UPPER_BOUND != 0 {
            Some(reader.u64be()? as i64)
        } else {
            None
        };
        let unit = if raw & HAS_UNIT != 0 {
            // The wire stores `unit - 1`; retain the raw byte.
            Some(reader.u8()?)
        } else {
            None
        };
        entries.push(Type {
            raw,
            lower,
            upper,
            unit,
        });
    }

    // `LoadAssert(types->entries[0].type_union == BEAM_TYPE_ANY)` and
    // `LoadAssert(types->entries[0].min > types->entries[0].max)`
    // (`beam_file.c:689-691`). Every `tr` index 0 means "any", so a table whose first
    // entry is anything else is corrupt.
    if let Some(first) = entries.first() {
        validate_first_entry(first)?;
    }

    Ok(TypeTable {
        entries,
        fallback: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    /// The version-4 oracle `Type` chunk: count 3, entries `{0x1FFF}` (any),
    /// `{0x2020, lower 0}` and `{0x2020, lower 1}`.
    fn oracle_type_chunk_v4() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend_from_slice(&0x1FFFu16.to_be_bytes());
        data.extend_from_slice(&0x2020u16.to_be_bytes());
        data.extend_from_slice(&0i64.to_be_bytes());
        data.extend_from_slice(&0x2020u16.to_be_bytes());
        data.extend_from_slice(&1i64.to_be_bytes());
        data
    }

    /// The version-3 (OTP 27/28) encoding of the very same three entries:
    /// `0x1FFF` (`0x0F 0xFF`), `0x2020` + lower 0 (`0x10 0x20`) and `0x2020` +
    /// lower 1.
    ///
    /// The metadata bits sit one position lower in version 3 (`lower` on b0 bit
    /// 4 rather than version-4 bit 13) and the type union is 12 bits wide, so the
    /// type union's high byte is the b0 low nibble.
    fn oracle_type_chunk_v3() -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend_from_slice(&[0x0F, 0xFF]); // any type
        data.extend_from_slice(&[0x10, 0x20]); // 0x2020: `lower` + type union 0x20
        data.extend_from_slice(&0i64.to_be_bytes());
        data.extend_from_slice(&[0x10, 0x20]);
        data.extend_from_slice(&1i64.to_be_bytes());
        data
    }

    #[test]
    fn parses_the_oracle_type_table() {
        // version 4, count 3; entries `{0x1FFF}`, `{0x2020, lower 0}`,
        // `{0x2020, lower 1}` (the "any" entry plus the two `fib/3` annotations).
        let data = oracle_type_chunk_v4();

        let table = parse(&data).unwrap();
        assert!(!table.fallback);
        assert_eq!(
            table.entries,
            vec![
                Type {
                    raw: 0x1FFF,
                    lower: None,
                    upper: None,
                    unit: None
                },
                Type {
                    raw: 0x2020,
                    lower: Some(0),
                    upper: None,
                    unit: None
                },
                Type {
                    raw: 0x2020,
                    lower: Some(1),
                    upper: None,
                    unit: None
                },
            ]
        );
    }

    #[test]
    fn parses_upper_bound_and_unit() {
        // `raw 0x4020` ⇒ upper bound on the `INTEGER` union (bit 5); `raw 0x8002` ⇒
        // unit on the `BITSTRING` union (bit 1) — the only unions those metadata bits
        // may annotate (`beam_types.c:52-74`). The first entry is the any type
        // (`beam_file.c:689`).
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend_from_slice(&0x1FFFu16.to_be_bytes());
        data.extend_from_slice(&0x4020u16.to_be_bytes());
        data.extend_from_slice(&(-7i64).to_be_bytes());
        data.extend_from_slice(&0x8002u16.to_be_bytes());
        data.push(3); // unit - 1

        let table = parse(&data).unwrap();
        assert_eq!(
            table.entries[1],
            Type {
                raw: 0x4020,
                lower: None,
                upper: Some(-7),
                unit: None
            }
        );
        assert_eq!(
            table.entries[2],
            Type {
                raw: 0x8002,
                lower: None,
                upper: None,
                unit: Some(3)
            }
        );
    }

    #[test]
    fn version_3_is_accepted() {
        let mut data = Vec::new();
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&[0x0F, 0xFF]);
        let table = parse(&data).unwrap();
        assert!(!table.fallback);
        assert_eq!(table.entries.len(), 1);
        // The version-3 "any type" (`0xFFF`) is reconstructed as the
        // version-4 `BEAM_TYPE_ANY` (`0x1FFF`), not as the truncated `0x0FFF`.
        assert_eq!(
            table.entries[0],
            Type {
                raw: 0x1FFF,
                lower: None,
                upper: None,
                unit: None
            }
        );
    }

    #[test]
    fn version_3_bit_layout() {
        // One hand-built version-3 chunk (the installed OTP 29 `erlc` only emits
        // version 4) pinning the remap for each metadata bit and for "any type":
        //   b0 bit 4 = lower, b0 bit 5 = upper, b0 bit 6 = unit,
        //   b0 low nibble = the type union's high part.
        let mut data = Vec::new();
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend_from_slice(&4u32.to_be_bytes());

        // `0x0F 0xFF` ⇒ any type.
        data.extend_from_slice(&[0x0F, 0xFF]);

        // Lower bound: b0 bit 4 ⇒ 0x2000, type union 0x0020 (`INTEGER`) ⇒ raw 0x2020.
        data.extend_from_slice(&[0x10, 0x20]);
        data.extend_from_slice(&(-3i64).to_be_bytes());

        // Upper bound: b0 bit 5 ⇒ 0x4000, type union `INTEGER`.
        data.extend_from_slice(&[0x20, 0x20]);
        data.extend_from_slice(&9i64.to_be_bytes());

        // Unit: b0 bit 6 ⇒ 0x8000, type union 0x0002 (`BITSTRING`) ⇒ raw 0x8002.
        data.extend_from_slice(&[0x40, 0x02]);
        data.push(7); // unit - 1

        let table = parse(&data).unwrap();
        assert!(!table.fallback);
        assert_eq!(
            table.entries,
            vec![
                Type {
                    raw: 0x1FFF,
                    lower: None,
                    upper: None,
                    unit: None
                },
                Type {
                    raw: 0x2020,
                    lower: Some(-3),
                    upper: None,
                    unit: None
                },
                Type {
                    raw: 0x4020,
                    lower: None,
                    upper: Some(9),
                    unit: None
                },
                Type {
                    raw: 0x8002,
                    lower: None,
                    upper: None,
                    unit: Some(7)
                },
            ]
        );
    }

    #[test]
    fn version_3_decodes_like_version_4() {
        // The same three semantic entries encoded both ways must decode to
        // identical IR: the IR always stores the version-4-convention word.
        let v4 = parse(&oracle_type_chunk_v4()).unwrap();
        let v3 = parse(&oracle_type_chunk_v3()).unwrap();
        assert_eq!(v3, v4);
        assert!(!v3.fallback);
    }

    #[test]
    fn unsupported_version_falls_back() {
        let mut data = Vec::new();
        data.extend_from_slice(&5u32.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&0x1FFFu16.to_be_bytes());
        let table = parse(&data).unwrap();
        assert_eq!(table, TypeTable::default());
    }

    #[test]
    fn zero_count_is_rejected() {
        // `LoadAssert(count >= 1)` (`beam_file.c:636`): a *present* chunk in a supported
        // version must carry the any-type entry — only an absent or version-incompatible
        // chunk falls back to the synthesized table (`beam_file.c:700-710`).
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        assert!(parse(&data).is_err());
    }

    /// One-entry version-4 `Type` payload with the given `type_word` and no extra
    /// fields (used for the words whose metadata bits are unset or rejected before the
    /// extra bytes are read).
    fn single_entry_chunk(raw: u16) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&raw.to_be_bytes());
        data
    }

    #[test]
    fn an_empty_type_word_is_rejected() {
        // `beam_types_decode_type` returns -1 for `flags == BEAM_TYPE_NONE`
        // (`beam_types.c:46-48`), and `parse_type_chunk_data` asserts `extra >= 0`.
        let err = parse(&single_entry_chunk(0x0000)).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
        // Control: the any type word is accepted.
        assert!(parse(&single_entry_chunk(0x1FFF)).is_ok());
    }

    #[test]
    fn a_bound_without_a_numeric_union_is_rejected() {
        // `beam_types.c:52-62`: a lower/upper bound is only meaningful on
        // `INTEGER`/`FLOAT`. `0x2000`/`0x4000` carry the flag but an empty union; `0x2001`
        // carries it on the `ATOM` union.
        for raw in [0x2000u16, 0x4000, 0x2001, 0x4001] {
            let err = parse(&single_entry_chunk(raw)).unwrap_err();
            assert!(matches!(err, BeamError::BadArg), "raw {raw:#06x}");
        }
        // Control: the same flags on the `INTEGER` union are accepted.
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&0x1FFFu16.to_be_bytes());
        data.extend_from_slice(&0x2020u16.to_be_bytes());
        data.extend_from_slice(&0i64.to_be_bytes());
        assert!(parse(&data).is_ok());
    }

    #[test]
    fn a_unit_without_a_bitstring_union_is_rejected() {
        // `beam_types.c:63-69`: the unit is only meaningful on `BITSTRING`. `0x8000` is
        // a unit with an empty union, `0x8020` a unit on `INTEGER`.
        for raw in [0x8000u16, 0x8020] {
            let err = parse(&single_entry_chunk(raw)).unwrap_err();
            assert!(matches!(err, BeamError::BadArg), "raw {raw:#06x}");
        }
        // Control: a unit on `BITSTRING` is accepted.
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&0x1FFFu16.to_be_bytes());
        data.extend_from_slice(&0x8002u16.to_be_bytes());
        data.push(7);
        assert!(parse(&data).is_ok());
    }

    #[test]
    fn a_first_entry_that_is_not_the_any_type_is_rejected() {
        // `LoadAssert(types->entries[0].type_union == BEAM_TYPE_ANY)`
        // (`beam_file.c:689`): entry 0 is what every `tr` index 0 resolves to.
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&0x0020u16.to_be_bytes()); // INTEGER, not ANY
        data.extend_from_slice(&0x1FFFu16.to_be_bytes());
        assert!(parse(&data).is_err());

        // Control: the same table with the any type first loads.
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&0x1FFFu16.to_be_bytes());
        data.extend_from_slice(&0x0020u16.to_be_bytes());
        assert!(parse(&data).is_ok());
    }

    #[test]
    fn a_bounded_any_entry_is_rejected() {
        // `LoadAssert(types->entries[0].min > types->entries[0].max)`
        // (`beam_file.c:690`): the any entry must stay unbounded (`MAX_SMALL + 1` ..
        // `MIN_SMALL - 1`), which a fully specified lower *and* upper bound inverts.
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&0x7FFFu16.to_be_bytes()); // ANY | lower | upper
        data.extend_from_slice(&0i64.to_be_bytes()); // min = 0
        data.extend_from_slice(&10i64.to_be_bytes()); // max = 10 ⇒ min <= max
        assert!(parse(&data).is_err());

        // Control: the any type with only a lower bound is still "unbounded above".
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&0x3FFFu16.to_be_bytes()); // ANY | lower
        data.extend_from_slice(&0i64.to_be_bytes());
        assert!(parse(&data).is_ok());
    }

    #[test]
    fn negative_count_is_rejected() {
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&(-1i32).to_be_bytes());
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn truncated_entry_is_rejected() {
        // raw 0x2020 promises a lower bound, but the chunk ends first.
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&0x2020u16.to_be_bytes());
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn missing_version_is_rejected() {
        let err = parse(&[0, 0]).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }
}
