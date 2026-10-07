//! `enif_*` — the C-variadic entries (**not yet populated**).
//!
//! Reserved slots (no implementation yet — the runtime subsystem that owns them is not
//! wired to this crate):
//! `enif_make_tuple`, `enif_make_list`, `enif_fprintf`, `enif_snprintf`, `enif_vfprintf`,
//! `enif_vsnprintf` — a later workstream supplies a C shim for these, since stable Rust cannot
//! define a C-variadic `fn`.
//!
//! See the module docs of [`super`] for why the slot stays `None`.

use super::ErlNifApi;

/// Populate the `ErlNifApi` slots this file owns (none yet — see the module docs).
pub(super) const fn populate(api: ErlNifApi) -> ErlNifApi {
    api
}
