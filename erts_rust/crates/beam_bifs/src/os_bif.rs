//! Operating-system and filename BIFs: the `os`, `file` and `prim_file`
//! surfaces (`getenv/1`, `putenv/2`, `unsetenv/1`, `env/0`, `getpid/0`,
//! `system_time/0,1`, `timestamp/0`, `perf_counter/0`;
//! `file:native_name_encoding/0`; the `prim_file` name-conversion helpers).
//!
//! Ground truth (read-only): the `os`, `file` and `prim_file` entries in
//! `../../erts/emulator/beam/bif.tab` and the corresponding `erl_bif_*.c` /
//! `prim_file` sources.
//!
//! # Roster
//!
//! `os:getenv/1`, `os:putenv/2`, `os:unsetenv/1`, `os:env/0`, `os:getpid/0`,
//! `os:system_time/0`, `os:system_time/1`, `os:timestamp/0`,
//! `os:perf_counter/0`; `file:native_name_encoding/0`;
//! `prim_file:internal_name2native/1`, `prim_file:internal_native2name/1`,
//! `prim_file:internal_normalize_utf8/1` and `prim_file:is_translatable/1`.
//!
//! The implementations land here in a follow-up change; this module currently
//! declares only its `register` stub so the module tree and the
//! `with_core_bifs` wiring are in place.

/// Registers this module's BIFs (none yet).
pub(crate) fn register(reg: &mut crate::registry::BifRegistry) {
    let _ = reg;
}
