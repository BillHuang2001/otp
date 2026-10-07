//! `enif_*` — the I/O queue and I/O vector family (**not yet populated**).
//!
//! Reserved slots (no implementation yet — the runtime subsystem that owns them is not
//! wired to this crate):
//! `enif_ioq_create`, `enif_ioq_destroy`, `enif_ioq_enq_binary`, `enif_ioq_enqv`, `enif_ioq_size`,
//! `enif_ioq_deq`, `enif_ioq_peek`, `enif_ioq_peek_head`, `enif_inspect_iovec`, `enif_free_iovec`.
//!
//! See the module docs of [`super`] for why the slot stays `None`.

use super::ErlNifApi;

/// Populate the `ErlNifApi` slots this file owns (none yet — see the module docs).
pub(super) const fn populate(api: ErlNifApi) -> ErlNifApi {
    api
}
