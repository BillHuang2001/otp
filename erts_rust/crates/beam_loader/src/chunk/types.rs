//! The `Type` chunk: compile-time register type annotations.
//!
//! Layout: `u32 version` (`4` current, `3` OTP 27/28; anything else ⇒ fallback),
//! `i32 count` (≥ 1, `entries[0]` is the "any type" entry), then per entry a
//! `u16 type_word` plus an optional `i64 lower`, `i64 upper` and `u8 unit-1`,
//! gated by `type_word` bits 13-15.
//!
//! When the chunk is absent or its version is incompatible the loader synthesizes
//! a single "any type" fallback entry ([`TypeTable::default`]), which makes `z,5`
//! `tr` operands collapse to plain registers.
use beam_common::Result;

use crate::ir::TypeTable;

/// Parses a `Type` chunk payload into the IR [`TypeTable`].
///
/// An unsupported `version`, a zero `count` or a truncated entry yields the
/// fallback table rather than an error where `beam_file.c` does so; genuinely
/// malformed/truncated data yields `BeamError::BadArg`.
// TODO(beam_loader-impl): read the version and count, decode each `type_word` and
// TODO(beam_loader-impl): its optional bounds/unit, and fall back to
// TODO(beam_loader-impl): `TypeTable::default()` when the version is incompatible.
pub fn parse(data: &[u8]) -> Result<TypeTable> {
    let _ = data;
    Err(beam_common::BeamError::BadArg)
}
