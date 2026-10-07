//! `enif_*` — the resource family (**not yet populated**).
//!
//! Reserved slots (no implementation yet — the runtime subsystem that owns them is not
//! wired to this crate):
//! `enif_open_resource_type`, `enif_open_resource_type_x`, `enif_init_resource_type`,
//! `enif_alloc_resource`, `enif_release_resource`, `enif_keep_resource`, `enif_make_resource`,
//! `enif_make_resource_binary`, `enif_get_resource`, `enif_sizeof_resource`,
//! `enif_dynamic_resource_call`.
//!
//! See the module docs of [`super`] for why the slot stays `None`.

use super::ErlNifApi;

/// Populate the `ErlNifApi` slots this file owns (none yet — see the module docs).
pub(super) const fn populate(api: ErlNifApi) -> ErlNifApi {
    api
}
