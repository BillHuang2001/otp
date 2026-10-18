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

//! The `AtU8` atom chunk: UTF-8 atom names in wire-index order.
//!
//! Layout: `i32 count` (negative ⇒ "long" varint lengths, real count = `-count`);
//! then per atom a length (varint if long, else `u8`) followed by the UTF-8 bytes.
//! Names are **UTF-8, not Latin-1** (`beam_file.c:291`). Wire **index 0 is reserved
//! for NIL**: index 1 is the module name and real atoms are `1..=count`
//! (`beam_file.c:262-297`), so the total number of entries is `count + 1`.
//!
//! Two structural bounds accompany the parse: `CHECK_ITEM_COUNT(count, 1, …)` requires
//! **at least one** real atom (`beam_file.c:260`), and `erts_atom_put` rejects a name
//! that is too large to be an atom (`atom.c:283-323`, non-truncating form) — see
//! [`MAX_ATOM_SZ_LIMIT`] and [`MAX_ATOM_CHARACTERS`].
use beam_common::{BeamError, Result};

use crate::error::bad_file;

/// The largest atom name in **characters** (`MAX_ATOM_CHARACTERS`, `atom.c:29`).
///
/// `erts_atom_put` runs the UTF-8 analyzer with this cap and, with `trunc == 0` (the
/// loader's form), reports `ATOM_MAX_CHARS_ERROR` past it (`atom.c:313-323`).
const MAX_ATOM_CHARACTERS: usize = 255;

/// The largest atom name in **bytes** (`MAX_ATOM_SZ_LIMIT`, `atom.c:31`).
///
/// Checked before the UTF-8 analysis (`atom.c:284-286`), so an over-long name is rejected
/// as a size limit rather than as an encoding error.
const MAX_ATOM_SZ_LIMIT: usize = 4 * MAX_ATOM_CHARACTERS;

/// The decoded atom names, in wire-index order.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Atoms {
    /// The names; `names[0]` is the reserved NIL placeholder (the empty string).
    pub names: Vec<String>,
}

/// Parses an `AtU8` chunk payload into its names, in wire-index order.
///
/// The returned vector is prefixed with the reserved NIL placeholder, so
/// `names[wire_index]` is the atom for that index and `names.len() == count + 1`.
pub fn parse(data: &[u8]) -> Result<Vec<String>> {
    let mut reader = beam_codec::Reader::new(data);

    // A negative count selects the "long" (varint) length encoding; its magnitude
    // is the real atom count. `i32::MIN` has no representable magnitude.
    let raw_count = reader.u32be()? as i32;
    let (count, long) = if raw_count < 0 {
        let magnitude = raw_count
            .checked_neg()
            .ok_or_else(|| bad_file("atom count overflow"))?;
        (magnitude as usize, true)
    } else {
        (raw_count as usize, false)
    };

    // Wire index 0 is the reserved NIL placeholder (`beam_file.c:262-297`); real
    // names occupy indices `1..=count`. `CHECK_ITEM_COUNT(count, 1, …)` requires at
    // least one real atom (`beam_file.c:260`): a module always names itself, so an
    // empty table is a corrupt/missing one.
    if count == 0 {
        return Err(bad_file("empty atom table"));
    }
    let mut names = Vec::new();
    names.push(String::new());

    for _ in 0..count {
        let len = if long {
            // The length is itself a packed tag/length number (tag `u`), which the
            // C asserts fits a word (`len.size == 0`).
            crate::decode::read_tagged(&mut reader)?.word_or_system_limit()? as usize
        } else {
            reader.u8()? as usize
        };
        // `atom.c:284-286`: a name longer than `MAX_ATOM_SZ_LIMIT` bytes is rejected
        // before the encoding is even examined.
        if len > MAX_ATOM_SZ_LIMIT {
            return Err(BeamError::SystemLimit);
        }
        let bytes = reader.bytes(len)?;
        let name = std::str::from_utf8(bytes).map_err(|_| bad_file("atom name is not UTF-8"))?;
        // `atom.c:313-323`: `erts_analyze_utf8_x` caps the *character* count at
        // `MAX_ATOM_CHARACTERS` and reports `ATOM_MAX_CHARS_ERROR` past it (the
        // loader passes `trunc == 0`, so the cap is an error, not a truncation).
        if name.chars().count() > MAX_ATOM_CHARACTERS {
            return Err(BeamError::SystemLimit);
        }
        names.push(name.to_string());
    }

    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    #[test]
    fn parses_short_form_lengths() {
        // count = 2, then a 1-byte length + bytes per atom.
        let data = [0, 0, 0, 2, 5, b'h', b'e', b'l', b'l', b'o', 3, b'a', b'b', b'c'];
        let names = parse(&data).unwrap();
        assert_eq!(names, vec!["".to_string(), "hello".into(), "abc".into()]);
    }

    #[test]
    fn parses_long_form_lengths() {
        // count = -2 (long form), then a packed tag/length per atom:
        // 0x50 ⇒ tag 0, value 5; 0x30 ⇒ tag 0, value 3.
        let data = [
            0xff, 0xff, 0xff, 0xfe, 0x50, b'h', b'e', b'l', b'l', b'o', 0x30, b'a', b'b', b'c',
        ];
        let names = parse(&data).unwrap();
        assert_eq!(names, vec!["".to_string(), "hello".into(), "abc".into()]);
    }

    /// Packs a length as the long (varint) form's `u`-tagged number
    /// (`beamreader_read_tagged`, `beam_file.c:167-240`).
    fn long_length(value: u64) -> Vec<u8> {
        if value <= 15 {
            return vec![(value as u8) << 4];
        }
        if value < 0x800 {
            return vec![0x08 | (((value >> 8) as u8) << 5), value as u8];
        }
        let count = ((64 - value.leading_zeros()) as usize + 7) / 8;
        let mut out = vec![0x18 | (((count - 2) as u8) << 5)];
        out.extend_from_slice(&value.to_be_bytes()[8 - count..]);
        out
    }

    #[test]
    fn an_empty_atom_table_is_rejected() {
        // `CHECK_ITEM_COUNT(count, 1, …)` (`beam_file.c:260`): a module always carries
        // its own name, so a table with no real atoms (only the reserved NIL slot) is
        // corrupt rather than a usable empty table.
        let err = parse(&[0, 0, 0, 0]).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn an_atom_name_longer_than_the_byte_limit_is_rejected() {
        // `MAX_ATOM_SZ_LIMIT` (1020 bytes, `atom.c:31,284-286`). Only the long (varint
        // length) form can express a length above 255.
        let len = MAX_ATOM_SZ_LIMIT + 1;
        let mut data = Vec::new();
        data.extend_from_slice(&(-1i32).to_be_bytes()); // count = -1 (long form)
        data.extend_from_slice(&long_length(len as u64));
        data.extend(std::iter::repeat_n(b'a', len));
        assert!(matches!(parse(&data).unwrap_err(), BeamError::SystemLimit));

        // At the limit it is accepted (a 255-character name of 4-byte characters).
        let name = "😀".repeat(MAX_ATOM_CHARACTERS);
        assert_eq!(name.len(), MAX_ATOM_SZ_LIMIT);
        let mut data = Vec::new();
        data.extend_from_slice(&(-1i32).to_be_bytes());
        data.extend_from_slice(&long_length(name.len() as u64));
        data.extend_from_slice(name.as_bytes());
        let names = parse(&data).unwrap();
        assert_eq!(names[1], name);
    }

    #[test]
    fn an_atom_name_longer_than_the_character_limit_is_rejected() {
        // `MAX_ATOM_CHARACTERS` (255 characters, `atom.c:29,313-323`): 300 ASCII bytes
        // are only 300 of the 1020 allowed *bytes* but 45 characters too many.
        let name = "a".repeat(MAX_ATOM_CHARACTERS + 45);
        let mut data = Vec::new();
        data.extend_from_slice(&(-1i32).to_be_bytes()); // count = -1 (long form)
        data.extend_from_slice(&long_length(name.len() as u64));
        data.extend_from_slice(name.as_bytes());
        assert!(matches!(parse(&data).unwrap_err(), BeamError::SystemLimit));

        // Exactly 255 characters is the largest legal name.
        let name = "a".repeat(MAX_ATOM_CHARACTERS);
        let mut data = Vec::new();
        data.extend_from_slice(&(-1i32).to_be_bytes());
        data.extend_from_slice(&long_length(name.len() as u64));
        data.extend_from_slice(name.as_bytes());
        assert_eq!(parse(&data).unwrap()[1], name);
    }

    #[test]
    fn empty_atom_name_is_allowed() {
        // count = 1, length 0 ⇒ an empty name at wire index 1.
        let names = parse(&[0, 0, 0, 1, 0]).unwrap();
        assert_eq!(names, vec!["".to_string(), "".into()]);
    }

    #[test]
    fn non_utf8_atom_is_rejected() {
        // count = 1, length 1, byte 0xFF is not valid UTF-8.
        let err = parse(&[0, 0, 0, 1, 1, 0xFF]).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn truncated_atom_is_rejected() {
        // count = 2 but only one atom's worth of bytes follows.
        let err = parse(&[0, 0, 0, 2, 1, b'a']).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn missing_count_is_rejected() {
        let err = parse(&[0, 0, 0]).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn count_overflow_is_rejected() {
        // count = i32::MIN has no representable magnitude.
        let err = parse(&[0x80, 0x00, 0x00, 0x00]).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn truncated_long_form_length_is_rejected() {
        // count = -1 but the packed length byte is missing.
        let err = parse(&[0xff, 0xff, 0xff, 0xff]).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }
}
