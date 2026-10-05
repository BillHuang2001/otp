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
pub mod atom;
pub mod code;
pub mod line;
pub mod literal;
pub mod misc;
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
    /// The `ImpT` import table.
    pub imports: Vec<table::RawImport>,
    /// The `FunT` lambda descriptors (framing only; not modeled in M1).
    pub funs: Vec<table::RawFun>,
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
    pub compile_info: Option<Vec<u8>>,
    /// The `Code` header as the IR's [`CodeVersion`].
    pub code_version: CodeVersion,
    /// The `Code` chunk instruction stream (the bytes after the header).
    pub code_stream: &'a [u8],
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
    let funs = match chunks.find(ChunkId::FUNT) {
        Some(chunk) => table::parse_funs(chunk.data)?,
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

    Ok(Chunks {
        atoms,
        code: Some(header),
        exports,
        imports,
        funs,
        line,
        types,
        literals,
        strings,
        attributes,
        compile_info,
        code_version,
        code_stream,
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
                (*b"CInf", 105),
                (*b"Dbgi", 104),
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
    }

    #[test]
    fn missing_atom_chunk_is_rejected() {
        // A container with only a `Code` chunk.
        let mut body = Vec::new();
        body.extend_from_slice(b"Code");
        body.extend_from_slice(&20u32.to_be_bytes());
        body.extend_from_slice(&16u32.to_be_bytes());
        body.extend_from_slice(&[0u8; 16]);
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
