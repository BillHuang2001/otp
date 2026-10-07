//! `enif_*` — poll-select registration (**not yet populated**).
//!
//! Reserved slots (no implementation yet — the runtime subsystem that owns them is not
//! wired to this crate):
//! `enif_select`, `enif_select_x`.
//!
//! See the module docs of [`super`] for why the slot stays `None`.

use super::ErlNifApi;

/// Populate the `ErlNifApi` slots this file owns (none yet — see the module docs).
pub(super) const fn populate(api: ErlNifApi) -> ErlNifApi {
    api
}
