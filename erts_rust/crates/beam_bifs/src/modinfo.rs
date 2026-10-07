//! Module-introspection BIFs for `erlang`: `get_module_info/1,2`.
//!
//! Ground truth (read-only): the entries in
//! `../../erts/emulator/beam/bif.tab` and the `get_module_info` implementation
//! in `../../erts/emulator/beam/erl_bif_info.c`.
//!
//! # Roster
//!
//! `erlang:get_module_info/1` and `erlang:get_module_info/2`.
//!
//! # A loaded module is runtime state, not a BIF
//!
//! The information a module exposes (`exports`, `attributes`, `compile`,
//! `md5`, …) lives with the *loaded* `beam_loader::Module` owned by the
//! runtime's code server, so both arities resolve it through the optional
//! [`ModuleInfo`](crate::context::ModuleInfo) seam that `beam_rt` supplies. An
//! unknown module or item — or no hook at all — is a typed capability gap, never
//! a fabricated value.
//!
//! The implementations land here in a follow-up change; this module currently
//! declares only its `register` stub so the module tree and the
//! `with_core_bifs` wiring are in place.

/// Registers this module's BIFs (none yet).
pub(crate) fn register(reg: &mut crate::registry::BifRegistry) {
    let _ = reg;
}
