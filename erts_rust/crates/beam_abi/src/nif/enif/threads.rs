//! `enif_*` — threads, mutexes, conditions, rwlocks and TSD (**not yet populated**).
//!
//! Reserved slots (no implementation yet — the runtime subsystem that owns them is not
//! wired to this crate):
//! `enif_mutex_create`, `enif_mutex_destroy`, `enif_mutex_trylock`, `enif_mutex_lock`,
//! `enif_mutex_unlock`, `enif_cond_create`, `enif_cond_destroy`, `enif_cond_signal`,
//! `enif_cond_broadcast`, `enif_cond_wait`, `enif_rwlock_create`, `enif_rwlock_destroy`,
//! `enif_rwlock_tryrlock`, `enif_rwlock_rlock`, `enif_rwlock_runlock`, `enif_rwlock_tryrwlock`,
//! `enif_rwlock_rwlock`, `enif_rwlock_rwunlock`, `enif_tsd_key_create`, `enif_tsd_key_destroy`,
//! `enif_tsd_set`, `enif_tsd_get`, `enif_thread_opts_create`, `enif_thread_opts_destroy`,
//! `enif_thread_create`, `enif_thread_self`, `enif_thread_exit`, `enif_thread_join`,
//! `enif_equal_tids`, `enif_thread_type`, `enif_mutex_name`, `enif_cond_name`, `enif_rwlock_name`,
//! `enif_thread_name`.
//!
//! See the module docs of [`super`] for why the slot stays `None`.

use super::ErlNifApi;

/// Populate the `ErlNifApi` slots this file owns (none yet — see the module docs).
pub(super) const fn populate(api: ErlNifApi) -> ErlNifApi {
    api
}
