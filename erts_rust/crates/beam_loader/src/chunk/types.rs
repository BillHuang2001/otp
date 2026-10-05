//! The `Type` chunk: compile-time register type annotations.
//!
//! Layout: `u32 version` (`4` current, `3` OTP 27/28; anything else ⇒ fallback),
//! `i32 count` (≥ 1, `entries[0]` is the "any type" entry), then per entry a
//! `u16 type_word` plus an optional `i64 lower`, `i64 upper` and `u8 unit-1`,
//! gated by `type_word` bits 13-15.
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
        // (`beam_types_decode_extra`, `beam_types.c:84-105`).
        let raw = reader.u16be()?;
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

    #[test]
    fn parses_the_oracle_type_table() {
        // version 4, count 3; entries `{0x1FFF}`, `{0x2020, lower 0}`,
        // `{0x2020, lower 1}` (the "any" entry plus the two `fib/3` annotations).
        let mut data = Vec::new();
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&3u32.to_be_bytes());
        data.extend_from_slice(&0x1FFFu16.to_be_bytes());
        data.extend_from_slice(&0x2020u16.to_be_bytes());
        data.extend_from_slice(&0i64.to_be_bytes());
        data.extend_from_slice(&0x2020u16.to_be_bytes());
        data.extend_from_slice(&1i64.to_be_bytes());

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
        data.extend_from_slice(&0x1FFFu16.to_be_bytes());
        let table = parse(&data).unwrap();
        assert!(!table.fallback);
        assert_eq!(table.entries.len(), 1);
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
