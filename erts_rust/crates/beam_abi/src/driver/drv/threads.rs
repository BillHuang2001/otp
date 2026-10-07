//! **Threads & synchronisation** family of the driver host layer (part of the `driver_*` /
//! `erl_drv_*` C ABI, workspace contract **A.4**).
//!
//! This file owns the slot list (and, in a later workstream, the implementations) for the
//! `erl_drv_*` functions that build the driver-facing threading/synchronisation surface. It
//! contributes no slots yet: [`populate`] is a no-op today, so every one of them stays `None` in
//! the frozen `ErlDrvApi` table — a driver that reaches one gets the C's failure behaviour, never
//! a crash.
//!
//! # Slots owned by this file
//!
//! * mutexes — `erl_drv_mutex_create`, `erl_drv_mutex_destroy`, `erl_drv_mutex_trylock`,
//!   `erl_drv_mutex_lock`, `erl_drv_mutex_unlock`, `erl_drv_mutex_name`
//! * condition variables — `erl_drv_cond_create`, `erl_drv_cond_destroy`, `erl_drv_cond_signal`,
//!   `erl_drv_cond_broadcast`, `erl_drv_cond_wait`, `erl_drv_cond_name`
//! * read/write locks — `erl_drv_rwlock_create`, `erl_drv_rwlock_destroy`,
//!   `erl_drv_rwlock_tryrlock`, `erl_drv_rwlock_rlock`, `erl_drv_rwlock_runlock`,
//!   `erl_drv_rwlock_tryrwlock`, `erl_drv_rwlock_rwlock`, `erl_drv_rwlock_rwunlock`,
//!   `erl_drv_rwlock_name`
//! * thread-specific data — `erl_drv_tsd_key_create`, `erl_drv_tsd_key_destroy`,
//!   `erl_drv_tsd_set`, `erl_drv_tsd_get`
//! * thread options & lifecycle — `erl_drv_thread_opts_create`, `erl_drv_thread_opts_destroy`,
//!   `erl_drv_thread_create`, `erl_drv_thread_self`, `erl_drv_thread_exit`,
//!   `erl_drv_thread_join`, `erl_drv_thread_name`, `erl_drv_equal_tids`
//!
//! # Modelling notes (for the implementing workstream)
//!
//! The C functions take/return raw `ErlDrvMutex *`/`ErlDrvCond *`/`ErlDrvRWLock *`/`ErlDrvTid`
//! handles; the runtime creates those objects itself, so — unlike the port family — they need no
//! [`DriverHost`](crate::driver::host::DriverHost) seam method: the handles are the runtime's own
//! (a mutex is a real OS/runtime lock, not a per-port service). Only `erl_drv_tsd_*` and the debug
//! `*_name` accessors may need host state, which the implementing pass can add to the seam then.

use crate::driver::api::ErlDrvApi;

/// Populate this family's slots of the frozen [`ErlDrvApi`].
///
/// No-op stub: the thread/synchronisation family is not implemented yet, so every slot it owns
/// stays `None`. Wiring a slot is a local edit to this file (the `populate` chain in
/// [`super`] already calls this).
pub(super) const fn populate(api: ErlDrvApi) -> ErlDrvApi {
    api
}
