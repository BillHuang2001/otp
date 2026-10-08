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

//! The `Line` chunk: the source-location table.
//!
//! Layout: `u32 version` (must be 0, else the whole chunk is ignored), `u32 flags`,
//! `u32 instr_count`, `u32 item_count`, `u32 name_count`; then a tagged item stream
//! (`TAG_a <name_index>` / `TAG_i <line>`); then `name_count` ×
//! `{ i16 len, len bytes UTF-8 }` file names (`beam_file.c:443-602`).
//!
//! The wire `item_count` counts **only** the `TAG_i` (line) entries: a `TAG_a` sets
//! the *current file index* and is consumed in addition, so the stream is read until
//! `item_count` `TAG_i` entries have been emitted (`beam_file.c:506-536`).
//!
//! `items[0]` is the undefined location `{0, 0}` and `files[0]` is synthesized as
//! `<module>.erl`, so the returned table always has at least those entries when the
//! chunk is usable.
use beam_common::Result;

use crate::error::bad_file;
use crate::ir::{LineEntry, LineTable};

/// The `TAG_i` line-item tag (`beam_file.c`): the item carries a line number.
const TAG_I: u8 = 1;
/// The `TAG_a` line-item tag (`beam_file.c`): the item carries a file-name index.
const TAG_A: u8 = 2;

/// Parses a `Line` chunk payload into the IR [`LineTable`].
///
/// `module_name` is used to synthesize `files[0]` (`<module>.erl`). A `version`
/// other than 0 means the chunk is ignored and a default (empty) table is
/// returned; a truncated or otherwise malformed chunk yields
/// `BeamError::BadArg`.
pub fn parse(data: &[u8], module_name: &str) -> Result<LineTable> {
    let mut reader = beam_codec::Reader::new(data);

    let version = reader.u32be()?;
    if version != 0 {
        // An unknown line-chunk version means "no line information".
        return Ok(LineTable::default());
    }

    let flags = reader.u32be()?;
    let instruction_count = reader.u32be()?;
    let item_count = reader.u32be()?;
    let name_count = reader.u32be()?;

    // The zeroth item is always the implicit "undefined location". The wire stream
    // then carries a sequence of tagged items that runs until `item_count` `TAG_i`
    // entries have been emitted: a `TAG_a` updates the current file-name index and is
    // consumed in addition (it does not count toward `item_count`), a `TAG_i` records
    // the line of the next instruction (`beam_file.c:506-536`).
    let mut items = vec![LineEntry::default()];
    let mut current_name: u32 = 0;
    // `items.len() - 1` is the number of `TAG_i` entries emitted so far.
    while items.len() as u32 <= item_count {
        let item = crate::decode::read_tagged(&mut reader)?;
        let tag = item.tag;
        let magnitude = item.word_or_system_limit()?;
        match tag {
            TAG_A => {
                // The name table is off by one (index 0 is the module name), so the
                // bound is inclusive (`beam_file.c:517`).
                if magnitude > name_count as u64 {
                    return Err(bad_file("line name index out of range"));
                }
                current_name = magnitude as u32;
            }
            TAG_I => {
                items.push(LineEntry {
                    file: current_name,
                    line: magnitude as u32,
                });
            }
            _ => return Err(bad_file("illegal line item tag")),
        }
    }

    // `files[0]` is the implicit `<module>.erl`; the chunk then carries `name_count`
    // `{ u16be len, len bytes UTF-8 }` table entries.
    let mut files = vec![format!("{module_name}.erl")];
    for _ in 0..name_count {
        let len = reader.u16be()? as usize;
        let bytes = reader.bytes(len)?;
        let name = std::str::from_utf8(bytes).map_err(|_| bad_file("line file name is not UTF-8"))?;
        files.push(name.to_string());
    }

    Ok(LineTable {
        flags,
        instruction_count,
        items,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    /// Builds a `Line` chunk from the header fields and a raw item stream.
    fn line_bytes(
        version: u32,
        flags: u32,
        instr_count: u32,
        item_count: u32,
        name_count: u32,
        items: &[u8],
        names: &[&str],
    ) -> Vec<u8> {
        let mut data = Vec::new();
        for v in [version, flags, instr_count, item_count, name_count] {
            data.extend_from_slice(&v.to_be_bytes());
        }
        data.extend_from_slice(items);
        for name in names {
            data.extend_from_slice(&(name.len() as u16).to_be_bytes());
            data.extend_from_slice(name.as_bytes());
        }
        data
    }

    #[test]
    fn parses_a_line_table_without_file_names() {
        // Five `TAG_i` items with values 4, 6, 8, 9, 11 (nibble-packed: 0x?1).
        let items = [0x41u8, 0x61, 0x81, 0x91, 0xb1];
        let data = line_bytes(0, 0, 7, 5, 0, &items, &[]);
        let table = parse(&data, "slice").unwrap();

        assert_eq!(table.flags, 0);
        assert_eq!(table.instruction_count, 7);
        assert_eq!(table.items.len(), 6);
        assert_eq!(table.items[0], LineEntry::default());
        assert_eq!(
            table.items[1..],
            [
                LineEntry { file: 0, line: 4 },
                LineEntry { file: 0, line: 6 },
                LineEntry { file: 0, line: 8 },
                LineEntry { file: 0, line: 9 },
                LineEntry { file: 0, line: 11 },
            ]
        );
        assert_eq!(table.files, vec!["slice.erl".to_string()]);
    }

    #[test]
    fn tag_a_sets_the_current_file_index() {
        // 0x12 ⇒ TAG_a with magnitude 1 (file index 1); 0x41 ⇒ TAG_i line 4. The
        // `TAG_a` is consumed *in addition* to the single counted `TAG_i` item.
        let items = [0x12u8, 0x41];
        let data = line_bytes(0, 0, 1, 1, 1, &items, &["other.erl"]);
        let table = parse(&data, "mod").unwrap();
        assert_eq!(table.items.len(), 2);
        assert_eq!(table.items[1], LineEntry { file: 1, line: 4 });
        assert_eq!(
            table.files,
            vec!["mod.erl".to_string(), "other.erl".to_string()]
        );
    }

    #[test]
    fn tag_a_items_do_not_count_toward_item_count() {
        // A module whose functions span two source files: the item stream interleaves
        // `TAG_a` (file switch) with `TAG_i` (line) entries, and only the `TAG_i`
        // entries count toward `item_count` (`beam_file.c:506-536`).
        //
        // 0x41 ⇒ TAG_i line 4 (still file index 0), 0x12 ⇒ TAG_a name index 1,
        // 0x91 ⇒ TAG_i line 9 (now file index 1).
        let items = [0x41u8, 0x12, 0x91];
        let data = line_bytes(0, 0, 2, 2, 1, &items, &["two_file.erl"]);
        let table = parse(&data, "m").unwrap();

        assert_eq!(table.flags, 0);
        assert_eq!(table.instruction_count, 2);
        assert_eq!(table.items.len(), 3);
        assert_eq!(table.items[0], LineEntry { file: 0, line: 0 });
        assert_eq!(table.items[1], LineEntry { file: 0, line: 4 });
        assert_eq!(table.items[2], LineEntry { file: 1, line: 9 });
        assert_eq!(
            table.files,
            vec!["m.erl".to_string(), "two_file.erl".to_string()]
        );
    }

    #[test]
    fn stream_with_fewer_tag_i_items_than_item_count_is_rejected() {
        // `item_count` counts only `TAG_i` entries: a stream that supplies just one
        // `TAG_i` for a declared `item_count` of 2 must fail rather than silently
        // returning a short table.
        let items = [0x41u8];
        let data = line_bytes(0, 0, 2, 2, 0, &items, &[]);
        let err = parse(&data, "m").unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn zero_items_yields_only_the_undefined_location() {
        let data = line_bytes(0, 0, 0, 0, 0, &[], &[]);
        let table = parse(&data, "mod").unwrap();
        assert_eq!(table.items, vec![LineEntry::default()]);
    }

    #[test]
    fn nonzero_version_is_ignored() {
        let data = line_bytes(1, 0, 7, 5, 0, &[0x41], &[]);
        let table = parse(&data, "slice").unwrap();
        assert_eq!(table, LineTable::default());
    }

    #[test]
    fn illegal_item_tag_is_rejected() {
        // 0x03 ⇒ tag 3, which is neither TAG_a nor TAG_i.
        let data = line_bytes(0, 0, 1, 1, 0, &[0x03], &[]);
        let err = parse(&data, "mod").unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn out_of_range_name_index_is_rejected() {
        // TAG_a magnitude 1 with name_count 0.
        let data = line_bytes(0, 0, 1, 1, 0, &[0x12], &[]);
        let err = parse(&data, "mod").unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn truncated_item_stream_is_rejected() {
        // item_count 5 but only two item bytes are present.
        let data = line_bytes(0, 0, 7, 5, 0, &[0x41, 0x61], &[]);
        let err = parse(&data, "mod").unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn truncated_header_is_rejected() {
        let err = parse(&[0, 0, 0, 0], "mod").unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn non_utf8_file_name_is_rejected() {
        let mut data = line_bytes(0, 0, 0, 0, 1, &[], &[]);
        data.extend_from_slice(&1u16.to_be_bytes());
        data.push(0xFF);
        let err = parse(&data, "mod").unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }
}
