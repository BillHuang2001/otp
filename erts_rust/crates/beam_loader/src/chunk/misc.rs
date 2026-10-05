//! Data-only chunks captured verbatim: `StrT`, `Attr`, `CInf`, `Meta`, `DbgB`,
//! `Recs`.
//!
//! None of these are interpreted by the loader in this milestone: their payloads
//! are opaque bytes at this layer (`StrT` becomes [`crate::ir::Module::strings`];
//! `Attr`/`CInf` become [`crate::ir::Module::attributes`] /
//! [`crate::ir::Module::compile_info`]). The remaining ids are skipped.

/// Copies a raw chunk payload verbatim.
///
/// Used for the data-only chunks (`StrT`, `Attr`, `CInf`); the copy is deliberate
/// because [`crate::ir::Module`] outlives the borrowed container.
pub fn raw(data: &[u8]) -> Vec<u8> {
    data.to_vec()
}
