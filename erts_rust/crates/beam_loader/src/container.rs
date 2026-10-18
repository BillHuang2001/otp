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

//! The `FOR1` container: the outer `.beam` file framing and its chunk directory.
//!
//! Wire layout (verified against the OTP 29 / erts-17.1 oracle):
//!
//! ```text
//! u32  "FOR1"                        // iff magic
//! u32  form_length                   // = file_size - 8 ; INCLUDES the 4-byte form type
//! u32  "BEAM"                        // iff FORM TYPE (not a chunk)
//! chunk entry * :  u8[4] chunk_id | u32 size | size bytes | (4 - size%4)%4 zero pad
//! ```
//!
//! `"BEAM"` is the form *type*, not a chunk. The chunk walk is bounded by
//! `form_length` — the C reader views exactly the declared form (`iff_init`,
//! `beam_file.c:2040-2052`) and stops at the end of that view, so bytes trailing the
//! form are ignored (`read_beam_chunks`, `beam_file.c:1536-1576`). Inside the form,
//! unknown ids are skipped, duplicate ids keep the first, and chunk order is not
//! significant.
//!
//! Parsing is safe Rust: the byte cursor is [`beam_codec::Reader`], the ETF
//! codec's bounds-checked big-endian reader (the lowest common ancestor for
//! big-endian reads) — there is no second cursor and no raw pointer in this crate.
use beam_common::Result;

use crate::error::{bad_file, truncated};

/// The four-byte `FOR1` (IFF) magic that opens every `.beam` image.
const FOR1_MAGIC: &[u8; 4] = b"FOR1";
/// The four-byte BEAM form type that follows the `form_length`.
const BEAM_FORM_TYPE: &[u8; 4] = b"BEAM";
/// The size of the fixed `FOR1` + `form_length` + `BEAM` preamble, in bytes.
const PREAMBLE_LEN: usize = 12;

/// A four-byte chunk identifier (e.g. `b"AtU8"`).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct ChunkId(pub [u8; 4]);

impl ChunkId {
    /// The atom table chunk (`AtU8`, UTF-8 atom names).
    ///
    /// The obsolete Latin-1 `Atom` chunk id is not a usable atom source: it is
    /// superseded by `AtU8` and is exposed separately as [`ChunkId::OBSOLETE_ATOM`]
    /// (classified tooling-only; the parser never reads atoms from it).
    pub const ATOM: ChunkId = ChunkId(*b"AtU8");
    /// The instruction stream chunk (`Code`).
    pub const CODE: ChunkId = ChunkId(*b"Code");
    /// The string table chunk (`StrT`).
    pub const STRT: ChunkId = ChunkId(*b"StrT");
    /// The import table chunk (`ImpT`).
    pub const IMPT: ChunkId = ChunkId(*b"ImpT");
    /// The export table chunk (`ExpT`).
    pub const EXPT: ChunkId = ChunkId(*b"ExpT");
    /// The fun/lambda table chunk (`FunT`).
    pub const FUNT: ChunkId = ChunkId(*b"FunT");
    /// The literal table chunk (`LitT`).
    pub const LITT: ChunkId = ChunkId(*b"LitT");
    /// The module attributes chunk (`Attr`).
    pub const ATTR: ChunkId = ChunkId(*b"Attr");
    /// The compile-info chunk (`CInf`).
    pub const CINF: ChunkId = ChunkId(*b"CInf");
    /// The line table chunk (`Line`).
    pub const LINE: ChunkId = ChunkId(*b"Line");
    /// The JIT-only local-function table chunk (`LocT`).
    ///
    /// Identical wire format to [`ChunkId::EXPT`]: the C reader parses it with the
    /// same export-table reader (`parse_locals_chunk` → `parse_export_table`) and
    /// keeps it only for `erts_jit_asm_dump` (`beam_file.c:1684`). It is irrelevant
    /// to interpretation, but the loader recognizes and parses it rather than
    /// silently skipping it (see [`crate::chunk::table::parse_locals`]).
    pub const LOCT: ChunkId = ChunkId(*b"LocT");
    /// The type-annotation chunk (`Type`).
    pub const TYPE: ChunkId = ChunkId(*b"Type");
    /// The metadata chunk (`Meta`).
    pub const META: ChunkId = ChunkId(*b"Meta");
    /// The legacy debug chunk (`DbgB`).
    ///
    /// This is the emulator's legacy debug chunk (`beam_file.c` chunk_iffs id 14),
    /// *not* the compiler's debug-info chunk — that one is [`ChunkId::DBGI`]. Like
    /// `Dbgi`, its payload is deliberately left unparsed.
    pub const DBGB: ChunkId = ChunkId(*b"DbgB");
    /// The compiler's debug-info chunk (`Dbgi`).
    ///
    /// Debug information is tooling-only and irrelevant to the runtime, so
    /// [`crate::chunk::parse`] deliberately does **not** parse its payload. The id
    /// is still recognized so the chunk surfaces as a known id rather than an
    /// "unknown chunk" that merely happened to be skipped.
    pub const DBGI: ChunkId = ChunkId(*b"Dbgi");
    /// The (obsolete) native-record chunk (`Recs`).
    pub const RECS: ChunkId = ChunkId(*b"Recs");
    /// The ExDoc documentation chunk (`Docs`).
    ///
    /// Unlike the other ids this one is not in the C reader's chunk directory
    /// (`beam_file.c`): the compiler emits it as an extra chunk (`compile.erl`), so it
    /// is recognized here for completeness. Its payload is a *compressed*,
    /// deterministic external term consumed by ExDoc and `beam_lib`'s documentation
    /// API; it is tooling-only for the runtime, so [`crate::chunk::parse`] recognizes
    /// the id but deliberately does not parse it (see [`crate::chunk::classify`]).
    pub const DOCS: ChunkId = ChunkId(*b"Docs");
    /// The (obsolete) Latin-1 atom table chunk (`Atom`).
    ///
    /// Superseded by [`ChunkId::ATOM`] (`AtU8`) in OTP 20 and earlier; the C reader
    /// rejects a file whose `AtU8` chunk is missing even when this one is present
    /// (`beam_file.c:1653-1665`). It is classified tooling-only by
    /// [`crate::chunk::classify`] and is never used as an atom source.
    pub const OBSOLETE_ATOM: ChunkId = ChunkId(*b"Atom");

    /// Builds a `ChunkId` from its four raw bytes.
    pub fn new(bytes: [u8; 4]) -> ChunkId {
        ChunkId(bytes)
    }

    /// The four raw bytes of this chunk id.
    pub fn name(self) -> [u8; 4] {
        self.0
    }
}

/// One raw chunk: its id plus a borrowed view of its (unpadded) payload.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawChunk<'a> {
    /// The chunk identifier.
    pub id: ChunkId,
    /// The chunk payload (`size` bytes, padding excluded).
    pub data: &'a [u8],
}

/// The parsed container: the file's chunks in file order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Container<'a> {
    /// The chunks, in the order they appear in the file.
    pub chunks: Vec<RawChunk<'a>>,
}

impl<'a> Container<'a> {
    /// The first chunk with the given id, if any (duplicate ids keep the first).
    pub fn find(&self, id: ChunkId) -> Option<&RawChunk<'a>> {
        self.chunks.iter().find(|chunk| chunk.id == id)
    }
}

/// Parses the `FOR1` header and the chunk directory out of a `.beam` image.
///
/// A missing/bad `FOR1` magic, a bad form type, a chunk size that overruns the form
/// or a truncated directory each yield `BeamError::BadArg` — never a panic.
///
/// The chunk directory is walked **inside the declared `form_length` only**, exactly
/// as the C reader does (`iff_init` hands `read_beam_chunks` a view of `form_length`
/// bytes, `beam_file.c:2040-2058`): bytes past the form are ignored, and a chunk whose
/// padded payload does not fit in what is left of the form is a corrupt image.
pub fn parse(bytes: &[u8]) -> Result<Container<'_>> {
    // The fixed preamble is `FOR1` + `form_length` + `BEAM`; anything shorter is
    // truncated before it can even be inspected.
    if bytes.len() < PREAMBLE_LEN {
        return Err(truncated("FOR1 preamble"));
    }

    let mut reader = beam_codec::Reader::new(bytes);

    let magic = reader.bytes(4)?;
    if magic != FOR1_MAGIC {
        return Err(bad_file("missing FOR1 magic"));
    }

    // `form_length` is the whole form size minus the 8-byte `FOR1`+length prefix;
    // it INCLUDES the 4-byte form type. It bounds the image, so a value that claims
    // more bytes than the file holds is rejected up front, and one that cannot even
    // hold the form type is structurally invalid.
    let form_length = reader.u32be()? as usize;
    if form_length < BEAM_FORM_TYPE.len() {
        return Err(bad_file("form length smaller than the form type"));
    }
    if form_length.saturating_add(8) > bytes.len() {
        return Err(truncated("form length"));
    }

    let form_type = reader.bytes(4)?;
    if form_type != BEAM_FORM_TYPE {
        return Err(bad_file("bad BEAM form type"));
    }

    // Everything after the form type, bounded by the declared form. The C walk
    // (`read_beam_chunks`) runs to the end of this view: trailing bytes outside the
    // form are never examined and a chunk must fit, padding included, inside it.
    let form = reader.bytes(form_length - BEAM_FORM_TYPE.len())?;
    let mut reader = beam_codec::Reader::new(form);

    let mut chunks = Vec::new();
    loop {
        let remaining = reader.remaining();
        if remaining == 0 {
            break;
        }
        // A chunk entry is at least an 8-byte `id`+`size` pair.
        if remaining < 8 {
            return Err(truncated("chunk header"));
        }

        let id_bytes = reader.bytes(4)?;
        let id = ChunkId([id_bytes[0], id_bytes[1], id_bytes[2], id_bytes[3]]);
        let size = reader.u32be()? as usize;

        // Chunk storage is padded to the next 4-byte boundary and that padded size
        // must fit in the rest of the form (`beamreader_test(&reader, storage_size)`,
        // `beam_file.c:1554`). `size` came from a `u32`, so `size + 3` cannot wrap.
        let storage = (size + 3) & !3;
        if storage > reader.remaining() {
            return Err(bad_file("chunk size overruns form"));
        }
        let data = reader.bytes(size)?;

        // Payloads are padded to a 4-byte boundary with zero bytes (skipped, not
        // validated).
        reader.bytes(storage - size)?;

        chunks.push(RawChunk { id, data });
    }

    Ok(Container { chunks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    /// Builds a `FOR1`/`BEAM` image from a sequence of `(id, payload)` chunks,
    /// applying the 4-byte alignment padding the wire format requires.
    fn build_image(chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (id, data) in chunks {
            body.extend_from_slice(*id);
            body.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
            let pad = (4 - (data.len() % 4)) % 4;
            body.extend(std::iter::repeat(0u8).take(pad));
        }

        let mut image = Vec::new();
        image.extend_from_slice(b"FOR1");
        // form_length = whole form minus the 8-byte `FOR1`+length prefix.
        image.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        image.extend_from_slice(b"BEAM");
        image.extend_from_slice(&body);
        image
    }

    #[test]
    fn parses_a_single_chunk() {
        let image = build_image(&[(b"Test", &[1, 2, 3, 4])]);
        let container = parse(&image).unwrap();
        assert_eq!(container.chunks.len(), 1);
        assert_eq!(container.chunks[0].id, ChunkId(*b"Test"));
        assert_eq!(container.chunks[0].data, &[1, 2, 3, 4]);
    }

    #[test]
    fn parses_multiple_chunks_and_skips_padding() {
        // The first payload is 3 bytes, so one zero pad byte follows it.
        let image = build_image(&[(b"Abcd", &[1, 2, 3]), (b"Efgh", &[])]);
        let container = parse(&image).unwrap();
        assert_eq!(container.chunks.len(), 2);
        assert_eq!(container.chunks[0].id, ChunkId(*b"Abcd"));
        assert_eq!(container.chunks[0].data, &[1, 2, 3]);
        assert_eq!(container.chunks[1].id, ChunkId(*b"Efgh"));
        assert_eq!(container.chunks[1].data, &[] as &[u8]);
    }

    #[test]
    fn find_returns_the_first_duplicate() {
        let image = build_image(&[(b"Test", &[1]), (b"Test", &[2, 2, 2, 2])]);
        let container = parse(&image).unwrap();
        assert_eq!(container.chunks.len(), 2);
        let found = container.find(ChunkId(*b"Test")).unwrap();
        assert_eq!(found.data, &[1]);
    }

    #[test]
    fn empty_chunk_directory_is_valid() {
        let image = build_image(&[]);
        let container = parse(&image).unwrap();
        assert!(container.chunks.is_empty());
    }

    #[test]
    fn truncated_preamble_is_rejected() {
        let image = build_image(&[(b"Test", &[1, 2, 3, 4])]);
        for len in 0..PREAMBLE_LEN {
            let err = parse(&image[..len]).unwrap_err();
            assert!(matches!(err, BeamError::BadArg), "len {len}");
        }
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut image = build_image(&[(b"Test", &[1, 2, 3, 4])]);
        image[0] = b'X';
        let err = parse(&image).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn bad_form_type_is_rejected() {
        let mut image = build_image(&[(b"Test", &[1, 2, 3, 4])]);
        image[8..12].copy_from_slice(b"NOPE");
        let err = parse(&image).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn chunk_size_overrunning_the_file_is_rejected() {
        let mut image = build_image(&[(b"Test", &[1, 2, 3, 4])]);
        // Patch the chunk size to claim far more bytes than the file holds.
        let size_off = PREAMBLE_LEN + 4;
        image[size_off..size_off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        let err = parse(&image).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn form_length_larger_than_file_is_rejected() {
        let mut image = build_image(&[(b"Test", &[1, 2, 3, 4])]);
        image[4..8].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        let err = parse(&image).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn form_length_smaller_than_the_form_type_is_rejected() {
        // The declared form must at least hold the 4-byte form type itself
        // (`read_beam_chunks` reads it out of the form view, `beam_file.c:1548`).
        let mut image = build_image(&[(b"Test", &[1, 2, 3, 4])]);
        image[4..8].copy_from_slice(&3u32.to_be_bytes());
        let err = parse(&image).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn incomplete_chunk_header_is_ignored_when_it_lies_past_the_form() {
        // Four trailing bytes cannot form a full id+size pair, but they lie *outside*
        // the declared form: the C walk is bounded by `form_length` and never sees
        // them (`beam_file.c:2040-2058`), so the directory parses unchanged.
        let mut image = build_image(&[(b"Test", &[1, 2, 3, 4])]);
        let form_length = u32::from_be_bytes(image[4..8].try_into().unwrap());
        image.extend_from_slice(b"XXXX");
        let container = parse(&image).unwrap();
        // The form length still describes the same single chunk.
        assert_eq!(u32::from_be_bytes(image[4..8].try_into().unwrap()), form_length);
        assert_eq!(container.chunks.len(), 1);
        assert_eq!(container.chunks[0].id, ChunkId(*b"Test"));
        assert_eq!(container.chunks[0].data, &[1, 2, 3, 4]);
    }

    #[test]
    fn incomplete_chunk_header_inside_the_form_is_rejected() {
        // The very same trailing 4 bytes, but now *inside* the declared form: the walk
        // reaches them and cannot form a chunk entry.
        let mut image = build_image(&[(b"Test", &[1, 2, 3, 4])]);
        image.extend_from_slice(b"XXXX");
        let form_length = u32::from_be_bytes(image[4..8].try_into().unwrap());
        image[4..8].copy_from_slice(&(form_length + 4).to_be_bytes());
        let err = parse(&image).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn a_form_that_ends_inside_a_chunk_is_rejected() {
        // A `form_length` four bytes short of the chunk's padded payload: the C's
        // `beamreader_test(&reader, storage_size)` fails, so the image is corrupt
        // rather than silently truncated.
        let mut image = build_image(&[(b"Test", &[1, 2, 3, 4])]);
        let form_length = u32::from_be_bytes(image[4..8].try_into().unwrap());
        image[4..8].copy_from_slice(&(form_length - 4).to_be_bytes());
        let err = parse(&image).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn a_chunk_whose_padding_does_not_fit_the_form_is_rejected() {
        // A 3-byte payload needs one pad byte; the form ends before it
        // (`storage_size = (size + 3) & ~3`, `beam_file.c:1551-1554`).
        let mut image = build_image(&[(b"Test", &[1, 2, 3])]);
        let form_length = u32::from_be_bytes(image[4..8].try_into().unwrap());
        image[4..8].copy_from_slice(&(form_length - 1).to_be_bytes());
        let err = parse(&image).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }
}
