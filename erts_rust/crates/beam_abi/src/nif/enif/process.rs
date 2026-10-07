//! `enif_*` — monitors, liveness, messaging and NIF scheduling (**not yet populated**).
//!
//! Reserved slots (no implementation yet — the runtime subsystem that owns them is not
//! wired to this crate):
//! `enif_monitor_process`, `enif_demonitor_process`, `enif_compare_monitors`,
//! `enif_make_monitor_term`, `enif_is_process_alive`, `enif_is_current_process_alive`,
//! `enif_is_port_alive`, `enif_send`, `enif_port_command`, `enif_whereis_pid`,
//! `enif_whereis_port`, `enif_schedule_nif`.
//!
//! See the module docs of [`super`] for why the slot stays `None`.

use super::ErlNifApi;

/// Populate the `ErlNifApi` slots this file owns (none yet — see the module docs).
pub(super) const fn populate(api: ErlNifApi) -> ErlNifApi {
    api
}
