//! `enif_*` — time, environment, system information and dynamic symbols (**not yet populated**).
//!
//! Reserved slots (no implementation yet — the runtime subsystem that owns them is not
//! wired to this crate):
//! `enif_monotonic_time`, `enif_time_offset`, `enif_convert_time_unit`, `enif_now_time`,
//! `enif_cpu_time`, `enif_system_info`, `enif_getenv`, `enif_make_unique_integer`,
//! `enif_consume_timeslice`, `enif_dlopen`, `enif_dlsym`.
//!
//! See the module docs of [`super`] for why the slot stays `None`.

use super::ErlNifApi;

/// Populate the `ErlNifApi` slots this file owns (none yet — see the module docs).
pub(super) const fn populate(api: ErlNifApi) -> ErlNifApi {
    api
}
