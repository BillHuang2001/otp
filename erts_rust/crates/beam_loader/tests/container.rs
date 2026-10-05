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

use beam_loader::chunk::{self, Chunks};
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

    // Ids and payload sizes, in file order. `Dbgi` (debug info) is *not* `DbgB`;
    // it is not a known chunk id and is skipped (but still surfaces as a raw chunk).
    let expected: Vec<(ChunkId, usize)> = vec![
        (ChunkId::ATOM, 64),          // AtU8
        (ChunkId::CODE, 163),         // Code
        (ChunkId::STRT, 0),           // StrT
        (ChunkId::IMPT, 52),          // ImpT
        (ChunkId::EXPT, 64),          // ExpT
        (ChunkId::META, 45),          // Meta
        (ChunkId::LOCT, 16),          // LocT
        (ChunkId::ATTR, 39),          // Attr
        (ChunkId::CINF, 27),          // CInf
        (ChunkId::new(*b"Dbgi"), 66), // Dbgi (unknown id)
        (ChunkId::LINE, 25),          // Line
        (ChunkId::TYPE, 30),          // Type
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
