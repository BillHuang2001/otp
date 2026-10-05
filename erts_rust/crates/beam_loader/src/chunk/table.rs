//! The `ExpT` / `ImpT` / `FunT` tables: exports, imports and lambda descriptors.
//!
//! * `ExpT` — `i32 count` × `{ i32 function_atom, i32 arity, i32 label }`; the
//!   label is the function **entry** label.
//! * `ImpT` — `i32 count` × `{ i32 module_atom, i32 function_atom, i32 arity }`.
//! * `FunT` — `i32 count` × `{ i32 function_atom, i32 arity, i32 label, i32 index,
//!   i32 num_free, i32 old_uniq }` (24-byte entries).
//!
//! These are the *raw* wire forms (still atom **indices**, not interned atoms);
//! [`crate::lower`] maps them onto the IR using the atom table.
use beam_common::Result;

/// A raw `ExpT` entry (atom indices, not interned atoms).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawExport {
    /// The function-name atom index.
    pub function: u32,
    /// The exported arity.
    pub arity: u32,
    /// The function entry label.
    pub label: u32,
}

/// A raw `ImpT` entry (atom indices, not interned atoms).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawImport {
    /// The module-name atom index.
    pub module: u32,
    /// The function-name atom index.
    pub function: u32,
    /// The imported arity.
    pub arity: u32,
}

/// A raw `FunT` (lambda) descriptor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawFun {
    /// The function-name atom index.
    pub function: u32,
    /// The lambda arity.
    pub arity: u32,
    /// The lambda code label.
    pub label: u32,
    /// The index within the enclosing function's fun list.
    pub index: u32,
    /// The number of free variables captured.
    pub num_free: u32,
    /// The old unique-id field (`beam_file.c`).
    pub old_uniq: u32,
}

/// Parses an `ExpT` chunk payload into its raw entries.
// TODO(beam_loader-impl): read `count` then `count` × 12-byte `{function, arity,
// TODO(beam_loader-impl): label}` entries; a short chunk ⇒ `BeamError::BadArg`.
pub fn parse_exports(data: &[u8]) -> Result<Vec<RawExport>> {
    let _ = data;
    Err(beam_common::BeamError::BadArg)
}

/// Parses an `ImpT` chunk payload into its raw entries.
// TODO(beam_loader-impl): read `count` then `count` × 12-byte `{module, function,
// TODO(beam_loader-impl): arity}` entries; a short chunk ⇒ `BeamError::BadArg`.
pub fn parse_imports(data: &[u8]) -> Result<Vec<RawImport>> {
    let _ = data;
    Err(beam_common::BeamError::BadArg)
}

/// Parses a `FunT` chunk payload into its raw lambda descriptors.
///
/// M1 validates the framing only; the descriptors are not surfaced in the IR.
// TODO(beam_loader-impl): read `count` then `count` × 24-byte entries; a short
// TODO(beam_loader-impl): chunk ⇒ `BeamError::BadArg`.
pub fn parse_funs(data: &[u8]) -> Result<Vec<RawFun>> {
    let _ = data;
    Err(beam_common::BeamError::BadArg)
}
