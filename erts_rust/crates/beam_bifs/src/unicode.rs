//! `unicode` module BIFs: `characters_to_binary/1,2,3` and
//! `characters_to_list/1,2`, encoding to/from `latin1`, `utf8`, `utf16` and
//! `utf32` (with `{utf16,little|big}` / `{utf32,little|big}` variants).
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_unicode.c` and
//! `../../lib/stdlib/src/unicode.erl`. The heap-bitstring helpers live in
//! [`crate::bits`].

use crate::registry::BifRegistry;

/// Registers the `unicode` module BIFs.
pub(crate) fn register(_reg: &mut BifRegistry) {}
