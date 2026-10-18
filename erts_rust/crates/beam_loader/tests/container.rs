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

//! Container-level integration test for `beam_loader`.
//!
//! Exercises the crate's public wire-layer API from *outside* the crate: the
//! `FOR1`/`BEAM` container walk ([`beam_loader::container`]) and the per-chunk
//! decoders ([`beam_loader::chunk`]) over the committed oracle fixture
//! `tests/fixtures/slice.beam`.
//!
//! Every expected value here is the **verified** ground truth recorded in
//! `tests/fixtures/README.md` (captured from the system Erlang toolchain,
//! OTP 29 / erts-17.1, with `beam_disasm:file/1` plus a byte decode). See the
//! crate `CONTEXT.md` for the authoritative container/chunk wire formats.

use beam_loader::chunk::{self, table, Chunks};
use beam_loader::container::{self, ChunkId};
use beam_loader::ir::{CodeVersion, LineEntry, Type, TypeTable};

/// The committed oracle fixture (a real `.beam` produced by the system `erlc`).
fn fixture() -> Vec<u8> {
    std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/slice.beam"
    ))
    .expect("read tests/fixtures/slice.beam")
}

/// Parses the fixture container and returns the decoded chunk set, borrowing
/// `bytes` (the instruction stream is a zero-copy slice of the `Code` chunk).
fn parse_chunks(bytes: &[u8]) -> Chunks<'_> {
    let container = container::parse(bytes).expect("container::parse");
    chunk::parse(&container).expect("chunk::parse")
}

#[test]
fn container_chunk_directory_is_the_oracle_sequence() {
    let bytes = fixture();
    let container = container::parse(&bytes).expect("container::parse");

    // Ids and payload sizes, in file order. `Dbgi` (the compiler's debug-info chunk)
    // is a *recognized* id now (`ChunkId::DBGI`), distinct from the legacy `DbgB`
    // debug chunk; its payload is deliberately left unparsed.
    let expected: Vec<(ChunkId, usize)> = vec![
        (ChunkId::ATOM, 64),         // AtU8
        (ChunkId::CODE, 163),        // Code
        (ChunkId::STRT, 0),          // StrT
        (ChunkId::IMPT, 52),         // ImpT
        (ChunkId::EXPT, 64),         // ExpT
        (ChunkId::META, 45),         // Meta
        (ChunkId::LOCT, 16),         // LocT
        (ChunkId::ATTR, 39),         // Attr
        (ChunkId::CINF, 27),         // CInf
        (ChunkId::DBGI, 66),         // Dbgi (recognized, payload unparsed)
        (ChunkId::LINE, 25),         // Line
        (ChunkId::TYPE, 30),         // Type
    ];
    let actual: Vec<(ChunkId, usize)> = container
        .chunks
        .iter()
        .map(|chunk| (chunk.id, chunk.data.len()))
        .collect();

    assert_eq!(actual, expected);
    assert_eq!(container.chunks.len(), 12);
}

#[test]
fn container_find_locates_the_atom_chunk() {
    let bytes = fixture();
    let container = container::parse(&bytes).expect("container::parse");

    let atom = container.find(ChunkId::ATOM).expect("AtU8 chunk present");
    assert_eq!(atom.id, ChunkId::ATOM);
    assert_eq!(atom.id.name(), *b"AtU8");
    assert_eq!(atom.data.len(), 64);

    // `AtU8` is the first chunk in the file; `find` returns that same view.
    assert_eq!(atom.data, container.chunks[0].data);
}

#[test]
fn atoms_are_parsed_in_wire_order() {
    let bytes = fixture();
    let chunks = parse_chunks(&bytes);

    // Wire index 0 is the NIL placeholder (`""`), index 1 the module name.
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
}

#[test]
fn code_header_matches_the_oracle() {
    let bytes = fixture();
    let chunks = parse_chunks(&bytes);

    assert_eq!(
        chunks.code_version,
        CodeVersion {
            format_number: 0,
            max_opcode: 181,
            label_count: 15,
            function_count: 6,
        }
    );

    // The `Code` header field mirrors `code_version`.
    let header = chunks.code.expect("Code chunk present");
    assert_eq!(header.format_number, 0);
    assert_eq!(header.max_opcode, 181);
    assert_eq!(header.label_count, 15);
    assert_eq!(header.function_count, 6);

    // The instruction stream borrows the bytes after the `Code` header.
    assert!(!chunks.code_stream.is_empty());
}

#[test]
fn imports_are_parsed_in_file_order() {
    let bytes = fixture();
    let chunks = parse_chunks(&bytes);

    let imports: Vec<(u32, u32, u32)> = chunks
        .imports
        .iter()
        .map(|import| (import.module, import.function, import.arity))
        .collect();

    // `{erlang,'+'/2}`, `{erlang,'-'/2}`, `{erlang,get_module_info/1,2}`.
    assert_eq!(imports, vec![(3, 4, 2), (3, 6, 2), (3, 10, 1), (3, 10, 2)]);
}

#[test]
fn exports_are_parsed_in_file_order() {
    let bytes = fixture();
    let chunks = parse_chunks(&bytes);

    let exports: Vec<(u32, u32, u32)> = chunks
        .exports
        .iter()
        .map(|export| (export.function, export.arity, export.label))
        .collect();

    // File order is preserved (not sorted); `label` is the function entry label.
    assert_eq!(
        exports,
        vec![(9, 1, 14), (9, 0, 12), (7, 1, 9), (5, 1, 4), (2, 2, 2)]
    );
}

#[test]
fn line_table_matches_the_oracle() {
    let bytes = fixture();
    let chunks = parse_chunks(&bytes);

    let line = chunks.line.expect("Line chunk present");
    assert_eq!(line.flags, 0);
    assert_eq!(line.instruction_count, 7);

    // Five tagged items plus the implicit undefined location at index 0.
    assert_eq!(line.items.len(), 6);
    assert_eq!(line.items[0], LineEntry { file: 0, line: 0 });
    assert_eq!(line.items[0], LineEntry::default());
    assert_eq!(line.files, vec!["slice.erl".to_string()]);
}

#[test]
fn type_table_matches_the_oracle() {
    let bytes = fixture();
    let chunks = parse_chunks(&bytes);

    assert!(!chunks.types.fallback);
    assert_eq!(
        chunks.types,
        TypeTable {
            entries: vec![
                Type {
                    raw: 0x1FFF,
                    lower: None,
                    upper: None,
                    unit: None,
                },
                Type {
                    raw: 0x2020,
                    lower: Some(0),
                    upper: None,
                    unit: None,
                },
                Type {
                    raw: 0x2020,
                    lower: Some(1),
                    upper: None,
                    unit: None,
                },
            ],
            fallback: false,
        }
    );
}

#[test]
fn locals_table_is_parsed_from_the_loct_chunk() {
    let bytes = fixture();
    let chunks = parse_chunks(&bytes);

    // `LocT` is byte-for-byte an `ExpT` payload: the single non-exported local
    // `fib/3` (wire atom index 5) at entry label 6.
    assert_eq!(
        chunks.locals,
        vec![table::RawExport {
            function: 5,
            arity: 3,
            label: 6,
        }]
    );
}

#[test]
fn a_container_without_a_loct_chunk_has_no_locals() {
    // A minimal valid container: `AtU8` (one module-name atom), a bare `Code` header and
    // the two mandatory tables (`ImpT`/`ExpT`, both empty — `beam_load.c:155-169`), and
    // no `LocT` chunk — `chunks.locals` must be empty, not an error.
    let mut body = Vec::new();
    body.extend_from_slice(b"AtU8");
    body.extend_from_slice(&6u32.to_be_bytes());
    body.extend_from_slice(&1u32.to_be_bytes()); // count = 1
    body.extend_from_slice(&[1u8, b'm', 0, 0]); // len 1, "m", 2 pad bytes
    body.extend_from_slice(b"Code");
    body.extend_from_slice(&20u32.to_be_bytes());
    body.extend_from_slice(&16u32.to_be_bytes()); // head_size
    // The four header fields: `format_number` 0, a `max_opcode` inside the accepted
    // window ([169, 193], `beam_load.c:189-225`), `label_count` and `function_count`.
    body.extend_from_slice(&0u32.to_be_bytes()); // format_number
    body.extend_from_slice(&181u32.to_be_bytes()); // max_opcode
    body.extend_from_slice(&0u32.to_be_bytes()); // label_count
    body.extend_from_slice(&0u32.to_be_bytes()); // function_count
    for id in [b"ImpT", b"ExpT"] {
        body.extend_from_slice(id);
        body.extend_from_slice(&4u32.to_be_bytes());
        body.extend_from_slice(&0u32.to_be_bytes()); // count = 0
    }
    let mut image = Vec::new();
    image.extend_from_slice(b"FOR1");
    image.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    image.extend_from_slice(b"BEAM");
    image.extend_from_slice(&body);

    let container = container::parse(&image).expect("container::parse");
    assert!(container.find(ChunkId::LOCT).is_none());
    let chunks = chunk::parse(&container).expect("chunk::parse");
    assert!(chunks.locals.is_empty());
    assert!(chunks.imports.is_empty());
    assert!(chunks.exports.is_empty());
}

#[test]
fn the_debug_chunk_ids_are_distinct_and_recognized() {
    // `DbgB` is the emulator's legacy debug chunk; `Dbgi` is the compiler's
    // debug-info chunk. They are two different ids.
    assert_eq!(ChunkId::DBGB.name(), *b"DbgB");
    assert_eq!(ChunkId::DBGI.name(), *b"Dbgi");
    assert_ne!(ChunkId::DBGB, ChunkId::DBGI);

    let bytes = fixture();
    let container = container::parse(&bytes).expect("container::parse");

    // `Dbgi` is present in the fixture and is now matched by its *recognized* id...
    let dbgi = container
        .find(ChunkId::DBGI)
        .expect("the fixture carries a Dbgi chunk");
    assert_eq!(dbgi.data.len(), 66);
    assert_eq!(
        container
            .chunks
            .iter()
            .filter(|c| c.id == ChunkId::DBGI)
            .count(),
        1
    );
    // ... while there is no legacy `DbgB` chunk in this file.
    assert!(container.find(ChunkId::DBGB).is_none());

    // The debug payload is deliberately not parsed: `chunk::parse` succeeds (the
    // recognized id is simply ignored, exactly like `Meta`).
    chunk::parse(&container).expect("chunk::parse ignores the Dbgi payload");
}

#[test]
fn data_chunks_are_captured() {
    let bytes = fixture();
    let chunks = parse_chunks(&bytes);

    // `StrT` is empty, there is no `LitT`, and no `FunT` descriptors.
    assert!(chunks.strings.is_empty());
    assert!(chunks.literals.is_empty());
    assert!(chunks.funs.is_empty());

    // `Attr`/`CInf` are captured raw (not parsed).
    assert!(chunks.attributes.is_some());
    assert!(chunks.compile_info.is_some());
}
