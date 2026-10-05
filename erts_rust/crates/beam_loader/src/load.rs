//! The two public entry points: [`load`] and [`load_file`].
//!
//! `load` is the whole pipeline — container → chunks → instruction decode →
//! lowering — and `load_file` is the convenience `std::fs::read` wrapper. These are
//! the *only* entry points; every intermediate type is public for inspection and
//! testing (see the crate `CONTEXT.md`, "API Surface").
use std::path::Path;

use beam_common::Result;

use crate::ir::Module;

/// Loads a `.beam` image from memory into the IR [`Module`].
///
/// Parses the `FOR1` container, decodes every known chunk, decodes the `Code`
/// instruction stream and lowers the result into the typed block-structured IR.
///
/// A malformed or truncated image yields `BeamError::BadArg`; an over-large
/// size/arity/count bound yields `BeamError::SystemLimit`. Never panics.
// TODO(beam_loader-impl): wire the pipeline together — `container::parse` →
// TODO(beam_loader-impl): `chunk::parse` → `decode::decode_stream` →
// TODO(beam_loader-impl): `lower::build_module` — using the interning atom table.
pub fn load(bytes: &[u8]) -> Result<Module> {
    let _ = bytes;
    Err(beam_common::BeamError::BadArg)
}

/// Loads a `.beam` image from a file path into the IR [`Module`].
///
/// Reads the whole file and delegates to [`load`]; an operating-system read error
/// is surfaced as [`beam_common::BeamError::Io`].
pub fn load_file(path: impl AsRef<Path>) -> Result<Module> {
    let bytes = std::fs::read(path)?;
    load(&bytes)
}
