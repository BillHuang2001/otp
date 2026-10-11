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

//! Per-chunk parsers and the assembled raw chunk set.
//!
//! Each child module owns one chunk family's wire layout and exposes a small
//! typed parser; [`parse`] drives them from a parsed [`Container`] and assembles a
//! [`Chunks`] value that [`crate::lower`] turns into the IR. See the crate
//! `CONTEXT.md` ("Per-chunk layouts") for every field's wire format.
//!
//! The chunks are parsed in file order and unknown/obsolete chunks are skipped;
//! the `Code` instruction stream is retained as a borrowed slice
//! ([`Chunks::code_stream`]) so instruction decoding is allocation-light.
//!
//! [`classify`] is the single in-code table saying how *every* chunk id is treated
//! (parsed+exposed / parsed-as-raw / tooling-only) so a runtime-relevant chunk cannot
//! go unnoticed; `tests/chunks.rs` enforces it over the installed OTP corpus.
pub mod atom;
pub mod classify;
pub mod code;
pub mod line;
pub mod literal;
pub mod misc;
pub mod record;
pub mod table;
pub mod types;

use beam_common::Result;

use crate::container::{ChunkId, Container};
use crate::error::bad_file;
use crate::ir::{CodeVersion, LineTable, TypeTable};

/// The parsed raw pieces of a `.beam` image, before lowering to the IR.
///
/// The named fields are the decoded chunk contents; `code_stream` borrows the
/// `Code` chunk's instruction bytes so [`crate::decode`] can walk them without a
/// copy.
#[derive(Debug)]
pub struct Chunks<'a> {
    /// The `AtU8` atom names in wire-index order (`names[0]` is the NIL placeholder).
    pub atoms: Vec<String>,
    /// The `Code` chunk header, if a `Code` chunk was present.
    pub code: Option<code::CodeHeader>,
    /// The `ExpT` export table.
    pub exports: Vec<table::RawExport>,
    /// The `LocT` local-function table (JIT-only; byte-for-byte an `ExpT` layout).
    ///
    /// Irrelevant to interpretation, but parsed so the chunk is *recognized* rather
    /// than silently skipped; see [`table::parse_locals`]. Empty when absent.
    pub locals: Vec<table::RawExport>,
    /// The `ImpT` import table.
    pub imports: Vec<table::RawImport>,
    /// The `FunT` lambda descriptors, in file (fun-table index) order.
    pub funs: Vec<table::RawFun>,
    /// The `Recs` native-record definitions, in file order (empty when absent).
    pub records: Vec<record::RawRecord>,
    /// The decoded `Line` table, if a usable `Line` chunk was present.
    pub line: Option<LineTable>,
    /// The decoded `Type` table (or its fallback).
    pub types: TypeTable,
    /// The `LitT` literal payloads (raw external-term bytes).
    pub literals: Vec<Vec<u8>>,
    /// The `StrT` string table bytes.
    pub strings: Vec<u8>,
    /// The raw `Attr` chunk bytes, if present.
    pub attributes: Option<Vec<u8>>,
    /// The raw `CInf` chunk bytes, if present.
    ///
    /// `CInf` is `term_to_binary(CompileInfo)` (`beam_asm.erl:236,309`); the decoded term
    /// is materialized by [`crate::lower::build_module`] into
    /// [`crate::ir::Module::compile_info_term`].
    pub compile_info: Option<Vec<u8>>,
    /// The raw `Meta` chunk bytes, if present.
    ///
    /// `Meta` is the compiler's feature/CPU metadata, an external term written by
    /// `compile.erl` (`?META_CHUNK_NAME`) and passed through `beam_asm.erl:216-224`. It is
    /// also part of the module checksum (`beam_file.c:1811-1815`), so it is captured raw;
    /// the decoded term is materialized by [`crate::lower::build_module`] into
    /// [`crate::ir::Module::meta_term`].
    pub meta: Option<Vec<u8>>,
    /// The `Code` header as the IR's [`CodeVersion`].
    pub code_version: CodeVersion,
    /// The `Code` chunk instruction stream (the bytes after the header).
    pub code_stream: &'a [u8],
    /// Every chunk of the source container, in file order, with its **raw unpadded
    /// payload**.
    ///
    /// Retained so the module checksum ([`crate::checksum::module_md5`]) can digest the
    /// whole chunk directory — including ids that are otherwise left unparsed (`DbgB`)
    /// or ignored — without re-reading the file. [`crate::container::RawChunk`] is
    /// `Copy`, so this is a cheap clone of the container's directory.
    pub raw: Vec<crate::container::RawChunk<'a>>,
}

/// Parses every known chunk out of a parsed container into a [`Chunks`] value.
///
/// Missing optional chunks yield their defaults (`None`/empty); a malformed or
/// truncated known chunk yields `BeamError::BadArg`.
pub fn parse<'a>(chunks: &Container<'a>) -> Result<Chunks<'a>> {
    // `AtU8` and `Code` are structurally required; everything else is optional.
    // Duplicate ids keep the first occurrence (`Container::find`).
    let atom_data = chunks
        .find(ChunkId::ATOM)
        .ok_or_else(|| bad_file("missing AtU8 chunk"))?
        .data;
    let atoms = atom::parse(atom_data)?;
    let module_name = atoms.get(1).cloned().unwrap_or_default();

    let code_data = chunks
        .find(ChunkId::CODE)
        .ok_or_else(|| bad_file("missing Code chunk"))?
        .data;
    let (header, code_stream) = code::parse_header(code_data)?;
    let code_version = CodeVersion {
        format_number: header.format_number,
        max_opcode: header.max_opcode,
        label_count: header.label_count,
        function_count: header.function_count,
    };

    let imports = match chunks.find(ChunkId::IMPT) {
        Some(chunk) => table::parse_imports(chunk.data)?,
        None => Vec::new(),
    };
    let exports = match chunks.find(ChunkId::EXPT) {
        Some(chunk) => table::parse_exports(chunk.data)?,
        None => Vec::new(),
    };
    // `LocT` shares the `ExpT` layout; the C parses it with the same reader. It is
    // the JIT-only local-function table, so the interpreter never consults it, but it
    // is a *recognized* chunk (not silently skipped).
    let locals = match chunks.find(ChunkId::LOCT) {
        Some(chunk) => table::parse_locals(chunk.data)?,
        None => Vec::new(),
    };
    let funs = match chunks.find(ChunkId::FUNT) {
        Some(chunk) => table::parse_funs(chunk.data)?,
        None => Vec::new(),
    };
    // `Recs` carries the module's native-record definitions. It is parsed with the
    // atom *names* because the export flag is the atom `true`/`false`.
    let records = match chunks.find(ChunkId::RECS) {
        Some(chunk) => record::parse(chunk.data, &atoms)?,
        None => Vec::new(),
    };
    let literals = match chunks.find(ChunkId::LITT) {
        Some(chunk) => literal::parse(chunk.data)?,
        None => Vec::new(),
    };
    let line = match chunks.find(ChunkId::LINE) {
        Some(chunk) => Some(line::parse(chunk.data, &module_name)?),
        None => None,
    };
    let types = match chunks.find(ChunkId::TYPE) {
        Some(chunk) => types::parse(chunk.data)?,
        None => TypeTable::default(),
    };

    let strings = chunks
        .find(ChunkId::STRT)
        .map(|chunk| chunk.data.to_vec())
        .unwrap_or_default();
    let attributes = chunks.find(ChunkId::ATTR).map(|chunk| misc::raw(chunk.data));
    let compile_info = chunks.find(ChunkId::CINF).map(|chunk| misc::raw(chunk.data));
    // `Meta` is captured raw for the module checksum (`src/checksum.rs`); it is
    // otherwise tooling-only (see `classify`). `Docs` carries no raw copy: it is a
    // large, compressed ExDoc payload the runtime never reads.
    let meta = chunks.find(ChunkId::META).map(|chunk| misc::raw(chunk.data));

    Ok(Chunks {
        atoms,
        code: Some(header),
        exports,
        locals,
        imports,
        funs,
        records,
        line,
        types,
        literals,
        strings,
        attributes,
        compile_info,
        meta,
        code_version,
        code_stream,
        raw: chunks.chunks.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{LineEntry, Type};
    use beam_common::BeamError;

    /// The committed oracle fixture (produced by the system `erlc`, OTP 29 /
    /// `erts-17.1`); see `tests/fixtures/README.md` for the full wire summary.
    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/slice.beam");

    #[test]
    fn parses_the_oracle_fixture() {
        let container = crate::container::parse(FIXTURE).unwrap();

        // Chunk directory: ids and payload sizes, in file order.
        let ids: Vec<([u8; 4], usize)> =
            container.chunks.iter().map(|c| (c.id.name(), c.data.len())).collect();
        assert_eq!(
            ids,
            vec![
                (*b"AtU8", 64),
                (*b"Code", 163),
                (*b"StrT", 0),
                (*b"ImpT", 52),
                (*b"ExpT", 64),
                (*b"Meta", 45),
                (*b"LocT", 16),
                (*b"Attr", 39),
                (*b"CInf", 27),
                (*b"Dbgi", 66),
                (*b"Line", 25),
                (*b"Type", 30),
            ]
        );

        let chunks = parse(&container).unwrap();

        // Atoms: wire index 0 is the NIL placeholder, index 1 the module name.
        assert_eq!(
            chunks.atoms,
            vec![
                "",
                "slice",
                "add",
                "erlang",
                "+",
                "fib",
                "-",
                "pick",
                "a",
                "module_info",
                "get_module_info",
            ]
        );

        // Imports (file order), as wire atom indices.
        let imports: Vec<(u32, u32, u32)> = chunks
            .imports
            .iter()
            .map(|i| (i.module, i.function, i.arity))
            .collect();
        assert_eq!(imports, vec![(3, 4, 2), (3, 6, 2), (3, 10, 1), (3, 10, 2)]);

        // Exports (file order, preserved/not sorted), as wire atom indices.
        let exports: Vec<(u32, u32, u32)> = chunks
            .exports
            .iter()
            .map(|e| (e.function, e.arity, e.label))
            .collect();
        assert_eq!(
            exports,
            vec![(9, 1, 14), (9, 0, 12), (7, 1, 9), (5, 1, 4), (2, 2, 2)]
        );

        // `LocT` (the JIT-only local-function table): the single non-exported local
        // `fib/3` (wire atom index 5) at entry label 6, as an `ExpT`-shaped triple.
        assert_eq!(
            chunks.locals,
            vec![table::RawExport {
                function: 5,
                arity: 3,
                label: 6
            }]
        );

        // Code header.
        let cv = chunks.code_version;
        assert_eq!(
            (cv.format_number, cv.max_opcode, cv.label_count, cv.function_count),
            (0, 181, 15, 6)
        );
        assert_eq!(
            chunks.code,
            Some(code::CodeHeader {
                format_number: 0,
                max_opcode: 181,
                label_count: 15,
                function_count: 6,
            })
        );
        assert!(!chunks.code_stream.is_empty());

        // Line table: five TAG_i items after the implicit undefined location.
        let line = chunks.line.as_ref().unwrap();
        assert_eq!(line.flags, 0);
        assert_eq!(line.instruction_count, 7);
        assert_eq!(line.items.len(), 6);
        assert_eq!(line.items[0], LineEntry::default());
        assert_eq!(line.files, vec!["slice.erl".to_string()]);

        // Type table.
        assert!(!chunks.types.fallback);
        assert_eq!(
            chunks.types.entries,
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

        // Raw data chunks.
        assert_eq!(chunks.strings, Vec::<u8>::new());
        assert_eq!(chunks.literals, Vec::<Vec<u8>>::new());
        assert!(chunks.attributes.is_some());
        assert!(chunks.compile_info.is_some());
        // The fixture carries a 45-byte `Meta` chunk, captured raw (and decoded into a
        // term one layer up — `Module::meta_term`).
        assert_eq!(chunks.meta.as_deref().map(<[u8]>::len), Some(45));
    }

    #[test]
    fn missing_atom_chunk_is_rejected() {
        // A container with only a `Code` chunk (with a *valid* header, so the rejection
        // can only come from the missing `AtU8`).
        let mut body = Vec::new();
        body.extend_from_slice(b"Code");
        body.extend_from_slice(&20u32.to_be_bytes());
        body.extend_from_slice(&16u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(&181u32.to_be_bytes());
        body.extend_from_slice(&[0u8; 8]);
        let mut image = Vec::new();
        image.extend_from_slice(b"FOR1");
        image.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        image.extend_from_slice(b"BEAM");
        image.extend_from_slice(&body);

        let container = crate::container::parse(&image).unwrap();
        let err = parse(&container).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn missing_locals_chunk_yields_an_empty_table() {
        // A minimal valid container: `AtU8` with the single module-name atom plus a
        // bare `Code` header, and no `LocT` chunk at all.
        let mut body = Vec::new();
        body.extend_from_slice(b"AtU8");
        body.extend_from_slice(&6u32.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes()); // count = 1
        body.extend_from_slice(&[1u8, b'm', 0, 0]); // len 1, "m", 2 pad bytes
        body.extend_from_slice(b"Code");
        body.extend_from_slice(&20u32.to_be_bytes());
        body.extend_from_slice(&16u32.to_be_bytes()); // head_size
        body.extend_from_slice(&0u32.to_be_bytes()); // format_number
        body.extend_from_slice(&181u32.to_be_bytes()); // max_opcode
        body.extend_from_slice(&0u32.to_be_bytes()); // label_count
        body.extend_from_slice(&0u32.to_be_bytes()); // function_count
        let mut image = Vec::new();
        image.extend_from_slice(b"FOR1");
        image.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        image.extend_from_slice(b"BEAM");
        image.extend_from_slice(&body);

        let container = crate::container::parse(&image).unwrap();
        assert!(container.find(crate::container::ChunkId::LOCT).is_none());
        let chunks = parse(&container).unwrap();
        assert!(chunks.locals.is_empty());
    }

    #[test]
    fn missing_code_chunk_is_rejected() {
        // A container with only an `AtU8` chunk holding zero atoms.
        let mut body = Vec::new();
        body.extend_from_slice(b"AtU8");
        body.extend_from_slice(&4u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes());
        let mut image = Vec::new();
        image.extend_from_slice(b"FOR1");
        image.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        image.extend_from_slice(b"BEAM");
        image.extend_from_slice(&body);

        let container = crate::container::parse(&image).unwrap();
        let err = parse(&container).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
    }
}
