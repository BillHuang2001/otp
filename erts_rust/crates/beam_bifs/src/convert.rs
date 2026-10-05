//! Pure `erlang` conversions and numeric/term helpers that need no binaries:
//! `integer_to_list/1`, `list_to_integer/1`, `float_to_list/1`, `list_to_float/1`,
//! `atom_to_list/1`, `list_to_atom/1`, `list_to_existing_atom/1`, `min/2`, `max/2`,
//! `round/1`, `trunc/1`, `float/1`, `ceil/1`, `floor/1`, `make_tuple/2,3`,
//! `append_element/2`, `raise/3`, `node/0`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_op.c`,
//! `erl_arith.c`, `erl_bif_atom.c`, `bif.tab`.

use crate::registry::BifRegistry;

/// Registers the pure `erlang` conversion/numeric BIFs.
pub(crate) fn register(_reg: &mut BifRegistry) {}
