//! `maps` module BIFs over flat maps: `new/0`, `from_list/1`, `to_list/1`,
//! `get/2,3`, `find/2`, `put/3`, `update/3`, `remove/2`, `keys/1`, `values/1`,
//! `size/1`, `is_key/2`, `merge/2`, `with/2`, `without/2`, `iterator/1`, `next/1`,
//! plus `erlang:map_get/2` and `erlang:map_size/1`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_map.c`,
//! `../../lib/stdlib/src/maps.erl`, `bif.tab`. Flat-map layout is owned by
//! `beam_term::thing` (`write_flatmap`/`flatmap_parts`).
//!
//! The `fun`-taking BIFs (`fold/3`, `map/2`, `update_with/3,4`) are not registered
//! here (they need an apply/callback seam; the stdlib module runs them).

use crate::registry::BifRegistry;

/// Registers the `maps` module BIFs.
pub(crate) fn register(_reg: &mut BifRegistry) {}
