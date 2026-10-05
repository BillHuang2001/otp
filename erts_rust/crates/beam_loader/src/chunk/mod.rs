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

use crate::container::Container;
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
// TODO(beam_loader-impl): dispatch on each chunk id, calling `atom::parse`,
// TODO(beam_loader-impl): `code::parse_header`, `table::parse_exports/imports/funs`,
// TODO(beam_loader-impl): `line::parse`, `types::parse`, `literal::parse` and
// TODO(beam_loader-impl): `misc::raw`, then assemble the `Chunks` value (deriving
// TODO(beam_loader-impl): `code_version` from the `Code` header).
pub fn parse<'a>(chunks: &Container<'a>) -> Result<Chunks<'a>> {
    let _ = chunks;
    Err(beam_common::BeamError::BadArg)
}
