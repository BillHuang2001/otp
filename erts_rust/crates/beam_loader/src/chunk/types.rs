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
//! `tr` operands collapse to plain registers.
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
/// An unsupported `version`, a zero `count` or a truncated entry yields the
/// fallback table rather than an error where `beam_file.c` does so; genuinely
/// malformed/truncated data yields `BeamError::BadArg`.
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
        // `beam_file.c` requires at least the "any type" entry.
        return Ok(TypeTable::default());
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
        // raw 0x4000 ⇒ upper bound; raw 0x8000 ⇒ unit.
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&2u32.to_be_bytes());
        data.extend_from_slice(&0x4000u16.to_be_bytes());
        data.extend_from_slice(&(-7i64).to_be_bytes());
        data.extend_from_slice(&0x8000u16.to_be_bytes());
        data.push(3); // unit - 1

        let table = parse(&data).unwrap();
        assert_eq!(
            table.entries[0],
            Type {
                raw: 0x4000,
                lower: None,
                upper: Some(-7),
                unit: None
            }
        );
        assert_eq!(
            table.entries[1],
            Type {
                raw: 0x8000,
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

        // Lower bound: b0 bit 4 ⇒ 0x2000, type union 0x0020 ⇒ raw 0x2020.
        data.extend_from_slice(&[0x10, 0x20]);
        data.extend_from_slice(&(-3i64).to_be_bytes());

        // Upper bound: b0 bit 5 ⇒ 0x4000.
        data.extend_from_slice(&[0x20, 0x20]);
        data.extend_from_slice(&9i64.to_be_bytes());

        // Unit: b0 bit 6 ⇒ 0x8000.
        data.extend_from_slice(&[0x40, 0x20]);
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
                    raw: 0x8020,
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
    fn zero_count_falls_back() {
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(parse(&data).unwrap(), TypeTable::default());
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
