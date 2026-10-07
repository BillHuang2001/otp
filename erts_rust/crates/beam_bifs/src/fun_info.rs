//! Fun-introspection BIFs for `erlang`: `fun_info/2`, `fun_info_mfa/1`,
//! `fun_to_list/1` and `make_fun/3`.
//!
//! Ground truth (read-only): the entries in
//! `../../erts/emulator/beam/bif.tab` and
//! `../../erts/emulator/beam/erl_bif_info.c`.
//!
//! # Roster
//!
//! `erlang:fun_info/2`, `erlang:fun_info_mfa/1`, `erlang:fun_to_list/1` and
//! `erlang:make_fun/3`.
//!
//! The implementations land here in a follow-up change; this module currently
//! declares only its `register` stub so the module tree and the
//! `with_core_bifs` wiring are in place.

/// Registers this module's BIFs (none yet).
pub(crate) fn register(reg: &mut crate::registry::BifRegistry) {
    let _ = reg;
}
