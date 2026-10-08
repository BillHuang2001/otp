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
use beam_common::Result;

use crate::error::bad_file;

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
    // names occupy indices `1..=count`.
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
        let bytes = reader.bytes(len)?;
        let name = std::str::from_utf8(bytes).map_err(|_| bad_file("atom name is not UTF-8"))?;
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

    #[test]
    fn zero_atoms_yields_only_the_placeholder() {
        let names = parse(&[0, 0, 0, 0]).unwrap();
        assert_eq!(names, vec!["".to_string()]);
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
