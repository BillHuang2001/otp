//! `erlang` binary/atom/integer conversions and term (de)serialization:
//! `list_to_binary/1`, `binary_to_list/1,3`, `byte_size/1`, `bit_size/1`,
//! `binary_part/2,3`, `integer_to_binary/1,2`, `binary_to_integer/1,2`,
//! `float_to_binary/1,2,3`, `binary_to_float/1`, `atom_to_binary/1,2`,
//! `binary_to_atom/1,2`, `binary_to_existing_atom/1,2`, `term_to_binary/1,2`,
//! `binary_to_term/1,2`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_binary.c`,
//! `erl_bif_atom.c`, `bif.tab`. Term (de)serialization routes through
//! `beam_codec`; the shared heap-bitstring helpers live in [`crate::bits`].

use crate::registry::BifRegistry;

/// Registers the `erlang` binary-conversion BIFs.
pub(crate) fn register(_reg: &mut BifRegistry) {}
