//! `enif_*` — map iteration (**not yet populated**).
//!
//! Reserved slots (no implementation yet — the runtime subsystem that owns them is not
//! wired to this crate):
//! `enif_map_iterator_create`, `enif_map_iterator_destroy`, `enif_map_iterator_is_head`,
//! `enif_map_iterator_is_tail`, `enif_map_iterator_next`, `enif_map_iterator_prev`,
//! `enif_map_iterator_get_pair`.
//!
//! See the module docs of [`super`] for why the slot stays `None`.

use super::ErlNifApi;

/// Populate the `ErlNifApi` slots this file owns (none yet — see the module docs).
pub(super) const fn populate(api: ErlNifApi) -> ErlNifApi {
    api
}
