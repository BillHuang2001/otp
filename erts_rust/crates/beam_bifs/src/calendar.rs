//! Calendar and clock BIFs for `erlang`: `date/0`, `time/0`, `now/0`,
//! `universaltime/0`, `localtime/0`, `posixtime_to_universaltime/1`,
//! `universaltime_to_posixtime/1`, `universaltime_to_localtime/1` and
//! `localtime_to_universaltime/2`.
//!
//! Ground truth (read-only): the entries in
//! `../../erts/emulator/beam/bif.tab` and
//! `../../erts/emulator/beam/erl_time_sup.c`.
//!
//! # Roster
//!
//! `erlang:date/0`, `erlang:time/0`, `erlang:now/0`, `erlang:universaltime/0`,
//! `erlang:localtime/0`, `erlang:posixtime_to_universaltime/1`,
//! `erlang:universaltime_to_posixtime/1`, `erlang:universaltime_to_localtime/1`
//! and `erlang:localtime_to_universaltime/2`.
//!
//! The implementations land here in a follow-up change; this module currently
//! declares only its `register` stub so the module tree and the
//! `with_core_bifs` wiring are in place.

/// Registers this module's BIFs (none yet).
pub(crate) fn register(reg: &mut crate::registry::BifRegistry) {
    let _ = reg;
}
