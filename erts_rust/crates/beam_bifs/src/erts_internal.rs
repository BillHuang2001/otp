//! `erts_internal`/`erts_debug` BIFs: the term/map helpers and the timers'
//! unit queries — `term_type/1`, `map_to_tuple_keys/1`,
//! `map_hashmap_children/1`, `map_next/3`, `time_unit/0`,
//! `perf_counter_unit/0`, `list_to_integer/2`, `binary_to_integer/2`;
//! `erts_debug:same/2` and `erts_debug:flat_size/1`.
//!
//! Ground truth (read-only): the `erts_internal`/`erts_debug` entries in
//! `../../erts/emulator/beam/bif.tab` and the corresponding `erl_bif_*.c`
//! sources.
//!
//! # Roster
//!
//! `erts_internal:term_type/1`, `erts_internal:map_to_tuple_keys/1`,
//! `erts_internal:map_hashmap_children/1`, `erts_internal:map_next/3`,
//! `erts_internal:time_unit/0`, `erts_internal:perf_counter_unit/0`,
//! `erts_internal:list_to_integer/2`, `erts_internal:binary_to_integer/2`,
//! `erts_debug:same/2` and `erts_debug:flat_size/1`.
//!
//! The implementations land here in a follow-up change; this module currently
//! declares only its `register` stub so the module tree and the
//! `with_core_bifs` wiring are in place.

/// Registers this module's BIFs (none yet).
pub(crate) fn register(reg: &mut crate::registry::BifRegistry) {
    let _ = reg;
}
