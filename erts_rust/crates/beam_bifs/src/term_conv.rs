//! Term-conversion and inspection BIFs for `erlang` and `binary`:
//! `bitstring_to_list/1`, `iolist_to_iovec/1`, `delete_element/2`,
//! `insert_element/3`, `display/1`, `display_string/2`, `ref_to_list/1`,
//! `list_to_ref/1`, `pid_to_list/1`, `list_to_pid/1`, `external_size/1`,
//! `is_integer/3` and `binary:referenced_byte_size/1`.
//!
//! Ground truth (read-only): the entries in
//! `../../erts/emulator/beam/bif.tab` and the corresponding `erl_bif_*.c`
//! sources.
//!
//! # Roster
//!
//! `erlang:bitstring_to_list/1`, `erlang:iolist_to_iovec/1`,
//! `erlang:delete_element/2`, `erlang:insert_element/3`, `erlang:display/1`,
//! `erlang:display_string/2`, `erlang:ref_to_list/1`, `erlang:list_to_ref/1`,
//! `erlang:pid_to_list/1`, `erlang:list_to_pid/1`, `erlang:external_size/1`,
//! `erlang:is_integer/3` and `binary:referenced_byte_size/1`.
//!
//! The implementations land here in a follow-up change; this module currently
//! declares only its `register` stub so the module tree and the
//! `with_core_bifs` wiring are in place.

/// Registers this module's BIFs (none yet).
pub(crate) fn register(reg: &mut crate::registry::BifRegistry) {
    let _ = reg;
}
