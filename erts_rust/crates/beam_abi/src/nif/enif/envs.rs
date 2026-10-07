//! `enif_*` — extra environments and private data (**not yet populated**).
//!
//! Reserved slots (no implementation yet — the runtime subsystem that owns them is not
//! wired to this crate):
//! `enif_alloc_env`, `enif_free_env`, `enif_make_copy`, `enif_priv_data`, `enif_set_option`.
//!
//! See the module docs of [`super`] for why the slot stays `None`.

use super::ErlNifApi;

/// Populate the `ErlNifApi` slots this file owns (none yet — see the module docs).
pub(super) const fn populate(api: ErlNifApi) -> ErlNifApi {
    api
}
