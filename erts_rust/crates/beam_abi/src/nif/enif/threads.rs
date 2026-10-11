// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! `enif_*` — threads, mutexes, conditions, reader/writer locks and thread-specific data
//! (compatibility contract **A.3**).
//!
//! Owned slots (34):
//!
//! | family | slots |
//! |---|---|
//! | mutex | `enif_mutex_create`, `enif_mutex_destroy`, `enif_mutex_trylock`, `enif_mutex_lock`, `enif_mutex_unlock`, `enif_mutex_name` |
//! | condition | `enif_cond_create`, `enif_cond_destroy`, `enif_cond_signal`, `enif_cond_broadcast`, `enif_cond_wait`, `enif_cond_name` |
//! | rwlock | `enif_rwlock_create`, `enif_rwlock_destroy`, `enif_rwlock_tryrlock`, `enif_rwlock_rlock`, `enif_rwlock_runlock`, `enif_rwlock_tryrwlock`, `enif_rwlock_rwlock`, `enif_rwlock_rwunlock`, `enif_rwlock_name` |
//! | tsd | `enif_tsd_key_create`, `enif_tsd_key_destroy`, `enif_tsd_set`, `enif_tsd_get` |
//! | thread | `enif_thread_opts_create`, `enif_thread_opts_destroy`, `enif_thread_create`, `enif_thread_self`, `enif_equal_tids`, `enif_thread_exit`, `enif_thread_join`, `enif_thread_type`, `enif_thread_name` |
//!
//! # Shape — a thin ABI facade over [`crate::sync`]
//!
//! These slots call the **frozen ABI types** directly: `enif_mutex_*`/`enif_cond_*`/`enif_rwlock_*` take
//! `*mut ErlNifMutex`/`_Cond`/`_RWLock` (the opaque `ErlDrvMutex_`-family structs), `enif_tsd_*` take
//! the signed `ErlNifTSDKey`, and `enif_thread_*` take `ErlNifTid` (`*mut ErlDrvTid_`) — the same
//! layout-identical handles the driver facet of the NIF/driver ABI uses. Each handle is passed
//! straight through to [`crate::sync`] as its Rust counterpart (`*mut sync::Mutex`, `sync::TsdKey`,
//! …), which is where the behaviour lives; nothing here re-implements locking, waiting or per-thread
//! storage.
//!
//! # Ground truth
//!
//! * `erts/emulator/beam/erl_nif.c:2405-2441` wraps every `enif_*` here as a one-line forward to its
//!   `erl_drv_*` sibling (with the NIF/TSD-key `int` spelling preserved), which is exactly the shape
//!   reproduced below.
//! * The behaviour itself is `erts/emulator/beam/erl_drv_thread.c` (mutex/cond/rwlock/tsd/threads).
//! * `enif_thread_type` is the one function with no `erl_drv_*` twin; its C body is
//!   `erl_nif.c:3606-3625` and it only ever reports the scheduler a NIF call is running on.
//! * The NIF-side documentation is `erts/doc/references/erl_nif.md` (§`enif_thread_create`,
//!   §`enif_thread_type`, …), delegating to `erl_driver.md`.
//!
//! # Documented deviations (inherited from [`crate::sync`] — never a fabricated success)
//!
//! * **Names.** A **null handle** yields `NULL` from its `*_name` accessor — as does the C
//!   (`dmtx ? dmtx->name : NULL`, `erl_drv_thread.c:198`, `:283`, `:365`, `:656`). A **live handle
//!   created with a NULL name** yields the C's static string `"unknown"` (`no_name`,
//!   `erl_drv_thread.c:99,135`, stored in the handle at `:170`/`:264`/`:341`/`:639`): [`crate::sync`]
//!   keeps that state as a *null* name and this facade renders it ([`rendered_name`], [`sync::no_name`]).
//!   `enif_thread_name` additionally answers `NULL` for a **tid this layer did not create** or one
//!   already joined; the C mints a `drv_thr = 0` record with `name = "unknown"` for any thread it did
//!   not create (`erl_drv_thread.c:664-684`, the `erl_drv_thread_self` path) and reads freed memory
//!   after a join (`:728`). That residual is the one place this facade and its `erl_drv_*` sibling
//!   diverge from the oracle (the foreign tid carries no record here; reproducing the C's per-thread
//!   record needs state this layer does not keep).
//! * **Mutex/cond/rwlock misuse never aborts.** A null handle is a no-op (`EINVAL` from the
//!   `try*` forms); the C calls `erts_exit` (`erl_drv_thread.c:207`). Unlocking a mutex from a
//!   *different* thread is permitted (the handle is a default, non-error-checking pthread mutex,
//!   matching the C's `posix_compliant` request, `erl_drv_thread.c:160-161`).
//! * **`enif_thread_create` failure returns a *positive* `errno`.** `erl_drv_thread_create` returns
//!   the raw `pthread_create` result (`ethread.c:403`), and `erl_driver.md` states it: "*Returns `0`
//!   on success, otherwise an `errno` value*". A null `enif_tid` or null `func` is refused with
//!   `EINVAL` where the C would fault (a non-`XCHK` build dereferences it, `ethread.c:327-330`).
//! * **TSD keys are real pthread keys**; an invalid/destroyed key makes `set`/`get` a safe no-op
//!   (`NULL`) where the C aborts. A null out-key is `EINVAL`; a failing create leaves the C's `-1`
//!   sentinel in the out-slot.
//! * **`enif_thread_join` retires the tid**: afterwards `enif_thread_name` returns `NULL` rather
//!   than a dangling pointer (the C frees its `ErlDrvTid_` record, `erl_drv_thread.c:728`).
//! * **`enif_thread_exit` always exits the calling thread** (via `pthread_exit`); the C aborts when
//!   called from a thread the interface did not create (`erl_drv_thread.c:704-713`).
//! * **`enif_equal_tids` compares the tid *values*** (pthread identity), where the C compares its
//!   per-thread record pointers (`erl_drv_thread.c:686-702`) — for the tids this layer hands out
//!   (each thread's own bit-cast `pthread_t`) the two agree. A null operand answers `0` where the C
//!   aborts.
//!
//! # `enif_thread_type` — the honest answer for this runtime
//!
//! `erl_nif.h:328-337` documents the return as a scheduler type `0..3`
//! (`ERL_NIF_THR_UNDEFINED` = 0, `ERL_NIF_THR_NORMAL_SCHEDULER` = 1,
//! `ERL_NIF_THR_DIRTY_CPU_SCHEDULER` = 2, `ERL_NIF_THR_DIRTY_IO_SCHEDULER` = 3; negative values are
//! reserved "for specific types of non-scheduler threads"). This runtime has **no scheduler
//! threads** — a NIF is entered on whatever thread calls it — so the C's answer for such a thread,
//! `ERL_NIF_THR_UNDEFINED`, is the only truthful one here. [`crate::sync::thread_type`] additionally
//! marks a thread it created with its own extension value `ERL_NIF_THR_OTHER` (`-1`, never produced
//! by the C — `erl_nif.c:3606-3625` knows only schedulers); this facade collapses that to
//! `ERL_NIF_THR_UNDEFINED` so the slot stays inside the header's documented set. A later scheduler
//! that starts running NIFs on real scheduler threads extends [`crate::sync::thread_type`], and this
//! mapping passes those values through unchanged.
//!
//! # Safety
//!
//! Every function takes raw handles with the C's lifetime contract: a handle must be null or come
//! from the matching create call and not be used after its destroy/join. Each function documents its
//! own contract; a null handle is always safe (it is a no-op or a typed error, never a dereference).

use super::*;

use libc::{c_char, c_int, c_void};

use crate::nif::types::{
    ErlNifCond, ErlNifMutex, ErlNifRWLock, ErlNifTSDKey, ErlNifThreadOpts, ErlNifTid,
};
use crate::sync;

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_mutex_create = Some(enif_mutex_create);
    api.enif_mutex_destroy = Some(enif_mutex_destroy);
    api.enif_mutex_trylock = Some(enif_mutex_trylock);
    api.enif_mutex_lock = Some(enif_mutex_lock);
    api.enif_mutex_unlock = Some(enif_mutex_unlock);
    api.enif_cond_create = Some(enif_cond_create);
    api.enif_cond_destroy = Some(enif_cond_destroy);
    api.enif_cond_signal = Some(enif_cond_signal);
    api.enif_cond_broadcast = Some(enif_cond_broadcast);
    api.enif_cond_wait = Some(enif_cond_wait);
    api.enif_rwlock_create = Some(enif_rwlock_create);
    api.enif_rwlock_destroy = Some(enif_rwlock_destroy);
    api.enif_rwlock_tryrlock = Some(enif_rwlock_tryrlock);
    api.enif_rwlock_rlock = Some(enif_rwlock_rlock);
    api.enif_rwlock_runlock = Some(enif_rwlock_runlock);
    api.enif_rwlock_tryrwlock = Some(enif_rwlock_tryrwlock);
    api.enif_rwlock_rwlock = Some(enif_rwlock_rwlock);
    api.enif_rwlock_rwunlock = Some(enif_rwlock_rwunlock);
    api.enif_tsd_key_create = Some(enif_tsd_key_create);
    api.enif_tsd_key_destroy = Some(enif_tsd_key_destroy);
    api.enif_tsd_set = Some(enif_tsd_set);
    api.enif_tsd_get = Some(enif_tsd_get);
    api.enif_thread_opts_create = Some(enif_thread_opts_create);
    api.enif_thread_opts_destroy = Some(enif_thread_opts_destroy);
    api.enif_thread_create = Some(enif_thread_create);
    api.enif_thread_self = Some(enif_thread_self);
    api.enif_equal_tids = Some(enif_equal_tids);
    api.enif_thread_exit = Some(enif_thread_exit);
    api.enif_thread_join = Some(enif_thread_join);
    api.enif_thread_type = Some(enif_thread_type);
    api.enif_mutex_name = Some(enif_mutex_name);
    api.enif_cond_name = Some(enif_cond_name);
    api.enif_rwlock_name = Some(enif_rwlock_name);
    api.enif_thread_name = Some(enif_thread_name);
    api
}

// ---------------------------------------------------------------------------------------------
// Mutex (`enif_mutex_*`, `erl_nif.c:2405-2409`)
// ---------------------------------------------------------------------------------------------

/// `ErlNifMutex *enif_mutex_create(char *name);` — `NULL` when the mutex cannot be created.
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string; the returned handle is owned by the
/// caller and must be released with [`enif_mutex_destroy`].
#[no_mangle]
pub unsafe extern "C" fn enif_mutex_create(name: *mut c_char) -> *mut ErlNifMutex {
    // SAFETY: `name` is null or NUL-terminated (the caller contract); the freshly boxed handle is
    // handed back as the opaque `*mut ErlNifMutex` the ABI declares (same pointer, opaque type).
    unsafe { sync::mutex_create(name) as *mut ErlNifMutex }
}

/// `void enif_mutex_destroy(ErlNifMutex *mtx);` — a null handle is a no-op.
///
/// # Safety
/// `mtx` must be null or a live handle from [`enif_mutex_create`] that is not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn enif_mutex_destroy(mtx: *mut ErlNifMutex) {
    // SAFETY: `mtx` is null or a live `enif_mutex_create` handle (the caller contract).
    unsafe { sync::mutex_destroy(mtx as *mut sync::Mutex) };
}

/// `int enif_mutex_trylock(ErlNifMutex *mtx);` — `0` when acquired, `EBUSY` when held, `EINVAL` for
/// a null handle (the C aborts there).
///
/// # Safety
/// `mtx` must be null or a live handle from [`enif_mutex_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_mutex_trylock(mtx: *mut ErlNifMutex) -> c_int {
    // SAFETY: `mtx` is null or a live handle; both are handled by the callee.
    unsafe { sync::mutex_try_lock(mtx as *mut sync::Mutex) }
}

/// `void enif_mutex_lock(ErlNifMutex *mtx);` — blocks until the mutex is acquired; a null handle is
/// a no-op.
///
/// # Safety
/// `mtx` must be null or a live handle from [`enif_mutex_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_mutex_lock(mtx: *mut ErlNifMutex) {
    // SAFETY: `mtx` is null or a live handle.
    unsafe { sync::mutex_lock(mtx as *mut sync::Mutex) };
}

/// `void enif_mutex_unlock(ErlNifMutex *mtx);` — a null handle is a no-op; unlocking from a
/// different thread than the locker is permitted (see the module docs).
///
/// # Safety
/// `mtx` must be null or a live handle from [`enif_mutex_create`] locked by some thread.
#[no_mangle]
pub unsafe extern "C" fn enif_mutex_unlock(mtx: *mut ErlNifMutex) {
    // SAFETY: `mtx` is null or a live handle.
    unsafe { sync::mutex_unlock(mtx as *mut sync::Mutex) };
}

/// Render a shared-layer `*_name` result the way the C's accessors do
/// (`erl_drv_thread.c:198`, `:283`, `:365`, `:656`: `handle ? handle->name : NULL`).
///
/// A **null handle** answers `NULL`. A **live handle** never answers `NULL` in the C: a handle created
/// with a NULL name carries the static `no_name` string (`"unknown"`, `erl_drv_thread.c:99,135`,
/// stored at `:170`/`:264`/`:341`). [`crate::sync`] represents that state as a *null* name, so it is
/// rendered as the C's `no_name` here ([`sync::no_name`]).
///
/// `live` must be `true` only for a non-null handle that really was created through this family;
/// `name` is the shared layer's name for it ([`sync::mutex_name`] &c.).
fn rendered_name(live: bool, name: *mut c_char) -> *mut c_char {
    if !live {
        core::ptr::null_mut()
    } else if name.is_null() {
        sync::no_name()
    } else {
        name
    }
}

/// `char *enif_mutex_name(ErlNifMutex *mtx);` — the debug name: the name [`enif_mutex_create`] was
/// handed, the static string `"unknown"` for a mutex created with a NULL name (`no_name`,
/// `erl_drv_thread.c:99,170`), or `NULL` for a null handle (`:198`).
///
/// # Safety
/// `mtx` must be null or a live handle from [`enif_mutex_create`]; the name stays valid until
/// [`enif_mutex_destroy`].
#[no_mangle]
pub unsafe extern "C" fn enif_mutex_name(mtx: *mut ErlNifMutex) -> *mut c_char {
    // SAFETY: `mtx` is null or a live handle; `mutex_name` returns a pointer into the handle's own
    // NUL-terminated name buffer (or null for a null/unnamed handle).
    let handle = mtx as *mut sync::Mutex;
    let name = unsafe { sync::mutex_name(handle) };
    rendered_name(!handle.is_null(), name)
}

// ---------------------------------------------------------------------------------------------
// Condition variable (`enif_cond_*`, `erl_nif.c:2410-2414`)
// ---------------------------------------------------------------------------------------------

/// `ErlNifCond *enif_cond_create(char *name);` — `NULL` when the condition cannot be created.
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string; the returned handle is owned by the
/// caller and must be released with [`enif_cond_destroy`].
#[no_mangle]
pub unsafe extern "C" fn enif_cond_create(name: *mut c_char) -> *mut ErlNifCond {
    // SAFETY: as `enif_mutex_create`.
    unsafe { sync::cond_create(name) as *mut ErlNifCond }
}

/// `void enif_cond_destroy(ErlNifCond *cnd);` — a null handle is a no-op.
///
/// # Safety
/// `cnd` must be null or a live handle from [`enif_cond_create`] with no waiters, not used
/// afterwards.
#[no_mangle]
pub unsafe extern "C" fn enif_cond_destroy(cnd: *mut ErlNifCond) {
    // SAFETY: `cnd` is null or a live `enif_cond_create` handle (the caller contract).
    unsafe { sync::cond_destroy(cnd as *mut sync::Cond) };
}

/// `void enif_cond_signal(ErlNifCond *cnd);` — wake one waiter; a null handle is a no-op.
///
/// # Safety
/// `cnd` must be null or a live handle from [`enif_cond_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_cond_signal(cnd: *mut ErlNifCond) {
    // SAFETY: `cnd` is null or a live handle.
    unsafe { sync::cond_signal(cnd as *mut sync::Cond) };
}

/// `void enif_cond_broadcast(ErlNifCond *cnd);` — wake every waiter; a null handle is a no-op.
///
/// # Safety
/// `cnd` must be null or a live handle from [`enif_cond_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_cond_broadcast(cnd: *mut ErlNifCond) {
    // SAFETY: `cnd` is null or a live handle.
    unsafe { sync::cond_broadcast(cnd as *mut sync::Cond) };
}

/// `void enif_cond_wait(ErlNifCond *cnd, ErlNifMutex *mtx);` — atomically release `mtx` and block
/// until signalled, then re-acquire `mtx` before returning (pthread's contract, which the C states
/// as "the calling thread must hold `mtx`").
///
/// A null handle of either kind is a no-op.
///
/// # Safety
/// `cnd` must be null or a live handle from [`enif_cond_create`], and `mtx` must be null or a live
/// handle from [`enif_mutex_create`] **currently held by the calling thread**.
#[no_mangle]
pub unsafe extern "C" fn enif_cond_wait(cnd: *mut ErlNifCond, mtx: *mut ErlNifMutex) {
    // SAFETY: both operands are null or live handles of the required kinds, and `mtx` is held by
    // this thread (the caller contract) — exactly `pthread_cond_wait`'s own precondition.
    unsafe { sync::cond_wait(cnd as *mut sync::Cond, mtx as *mut sync::Mutex) };
}

/// `char *enif_cond_name(ErlNifCond *cnd);` — the debug name: the name [`enif_cond_create`] was
/// handed, the static string `"unknown"` for a condition variable created with a NULL name
/// (`no_name`, `erl_drv_thread.c:99,264`), or `NULL` for a null handle (`:283`).
///
/// # Safety
/// `cnd` must be null or a live handle from [`enif_cond_create`]; the name stays valid until
/// [`enif_cond_destroy`].
#[no_mangle]
pub unsafe extern "C" fn enif_cond_name(cnd: *mut ErlNifCond) -> *mut c_char {
    // SAFETY: `cnd` is null or a live handle; `cond_name` returns a pointer into the handle's own
    // NUL-terminated name buffer (or null for a null/unnamed handle).
    let handle = cnd as *mut sync::Cond;
    let name = unsafe { sync::cond_name(handle) };
    rendered_name(!handle.is_null(), name)
}

// ---------------------------------------------------------------------------------------------
// Reader/writer lock (`enif_rwlock_*`, `erl_nif.c:2415-2422`)
// ---------------------------------------------------------------------------------------------

/// `ErlNifRWLock *enif_rwlock_create(char *name);` — `NULL` when the lock cannot be created.
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string; the returned handle is owned by the
/// caller and must be released with [`enif_rwlock_destroy`].
#[no_mangle]
pub unsafe extern "C" fn enif_rwlock_create(name: *mut c_char) -> *mut ErlNifRWLock {
    // SAFETY: as `enif_mutex_create`.
    unsafe { sync::rwlock_create(name) as *mut ErlNifRWLock }
}

/// `void enif_rwlock_destroy(ErlNifRWLock *rwlck);` — a null handle is a no-op.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`enif_rwlock_create`] that is not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn enif_rwlock_destroy(rwlck: *mut ErlNifRWLock) {
    // SAFETY: `rwlck` is null or a live `enif_rwlock_create` handle (the caller contract).
    unsafe { sync::rwlock_destroy(rwlck as *mut sync::RwLock) };
}

/// `int enif_rwlock_tryrlock(ErlNifRWLock *rwlck);` — `0` when the read lock was taken, `EBUSY`
/// when a writer holds it, `EINVAL` for a null handle (the C aborts there).
///
/// # Safety
/// `rwlck` must be null or a live handle from [`enif_rwlock_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_rwlock_tryrlock(rwlck: *mut ErlNifRWLock) -> c_int {
    // SAFETY: `rwlck` is null or a live handle; both are handled by the callee.
    unsafe { sync::rwlock_try_read_lock(rwlck as *mut sync::RwLock) }
}

/// `void enif_rwlock_rlock(ErlNifRWLock *rwlck);` — take the read lock, blocking while a writer
/// holds it; a null handle is a no-op.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`enif_rwlock_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_rwlock_rlock(rwlck: *mut ErlNifRWLock) {
    // SAFETY: `rwlck` is null or a live handle.
    unsafe { sync::rwlock_read_lock(rwlck as *mut sync::RwLock) };
}

/// `void enif_rwlock_runlock(ErlNifRWLock *rwlck);` — release the read lock; a null handle is a
/// no-op.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`enif_rwlock_create`] held for reading by this
/// thread.
#[no_mangle]
pub unsafe extern "C" fn enif_rwlock_runlock(rwlck: *mut ErlNifRWLock) {
    // SAFETY: `rwlck` is null or a live handle with a read lock held by this thread.
    unsafe { sync::rwlock_read_unlock(rwlck as *mut sync::RwLock) };
}

/// `int enif_rwlock_tryrwlock(ErlNifRWLock *rwlck);` — `0` when the write lock was taken, `EBUSY`
/// when any reader or writer holds it, `EINVAL` for a null handle (the C aborts there).
///
/// # Safety
/// `rwlck` must be null or a live handle from [`enif_rwlock_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_rwlock_tryrwlock(rwlck: *mut ErlNifRWLock) -> c_int {
    // SAFETY: `rwlck` is null or a live handle; both are handled by the callee.
    unsafe { sync::rwlock_try_write_lock(rwlck as *mut sync::RwLock) }
}

/// `void enif_rwlock_rwlock(ErlNifRWLock *rwlck);` — take the write lock, blocking while any reader
/// or writer holds it; a null handle is a no-op.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`enif_rwlock_create`].
#[no_mangle]
pub unsafe extern "C" fn enif_rwlock_rwlock(rwlck: *mut ErlNifRWLock) {
    // SAFETY: `rwlck` is null or a live handle.
    unsafe { sync::rwlock_write_lock(rwlck as *mut sync::RwLock) };
}

/// `void enif_rwlock_rwunlock(ErlNifRWLock *rwlck);` — release the write lock; a null handle is a
/// no-op.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`enif_rwlock_create`] held for writing by this
/// thread.
#[no_mangle]
pub unsafe extern "C" fn enif_rwlock_rwunlock(rwlck: *mut ErlNifRWLock) {
    // SAFETY: `rwlck` is null or a live handle with a write lock held by this thread.
    unsafe { sync::rwlock_write_unlock(rwlck as *mut sync::RwLock) };
}

/// `char *enif_rwlock_name(ErlNifRWLock *rwlck);` — the debug name: the name [`enif_rwlock_create`]
/// was handed, the static string `"unknown"` for a lock created with a NULL name (`no_name`,
/// `erl_drv_thread.c:99,341`), or `NULL` for a null handle (`:365`).
///
/// # Safety
/// `rwlck` must be null or a live handle from [`enif_rwlock_create`]; the name stays valid until
/// [`enif_rwlock_destroy`].
#[no_mangle]
pub unsafe extern "C" fn enif_rwlock_name(rwlck: *mut ErlNifRWLock) -> *mut c_char {
    // SAFETY: `rwlck` is null or a live handle; `rwlock_name` returns a pointer into the handle's own
    // NUL-terminated name buffer (or null for a null/unnamed handle).
    let handle = rwlck as *mut sync::RwLock;
    let name = unsafe { sync::rwlock_name(handle) };
    rendered_name(!handle.is_null(), name)
}

// ---------------------------------------------------------------------------------------------
// Thread-specific data (`enif_tsd_*`, `erl_nif.c:2423-2426`)
// ---------------------------------------------------------------------------------------------

/// `int enif_tsd_key_create(char *name, ErlNifTSDKey *key);` — `0` on success, `EINVAL` for a null
/// out-key, otherwise the platform's error code. The C's `-1` sentinel is written to `*key` on
/// failure (`erl_drv_thread.c:462,472`) so a caller must check the return value first.
///
/// # Safety
/// `name` must be null or NUL-terminated; `key` must be a valid writable `ErlNifTSDKey` slot (or
/// null, which returns `EINVAL`).
#[no_mangle]
pub unsafe extern "C" fn enif_tsd_key_create(name: *mut c_char, key: *mut ErlNifTSDKey) -> c_int {
    // SAFETY: `name` is null or NUL-terminated; `key` is null or a valid writable slot.
    // `ErlNifTSDKey` is the header's `int` and `sync::TsdKey` its `unsigned` twin — the same 4 bytes
    // — so the out-write is bit-identical (in particular a failed create leaves `-1` behind).
    unsafe { sync::tsd_key_create(name, key as *mut sync::TsdKey) }
}

/// `void enif_tsd_key_destroy(ErlNifTSDKey key);` — invalidates `key` for every thread. A key that
/// was never created (or was already destroyed) is rejected by the platform and is a no-op.
///
/// # Safety
/// `key` must be an `ErlNifTSDKey` value; it is only ever used as a value, never dereferenced.
#[no_mangle]
pub unsafe extern "C" fn enif_tsd_key_destroy(key: ErlNifTSDKey) {
    // SAFETY: `pthread_key_delete` takes the key by value and dereferences nothing.
    sync::tsd_key_destroy(key as sync::TsdKey);
}

/// `void enif_tsd_set(ErlNifTSDKey key, void *data);` — this thread's value for `key`. An invalid or
/// destroyed key is a no-op (the C aborts).
///
/// # Safety
/// `key` must be a value from [`enif_tsd_key_create`]; `data` is stored as a value and never
/// dereferenced by this layer.
#[no_mangle]
pub unsafe extern "C" fn enif_tsd_set(key: ErlNifTSDKey, data: *mut c_void) {
    // SAFETY: `pthread_setspecific` stores the pointer value in the calling thread's slot for `key`
    // and validates a destroyed/out-of-range key itself.
    sync::tsd_set(key as sync::TsdKey, data);
}

/// `void *enif_tsd_get(ErlNifTSDKey key);` — this thread's value for `key`, or `NULL` when unset (or
/// for an invalid/destroyed key).
///
/// # Safety
/// `key` must be a value from [`enif_tsd_key_create`] (a destroyed key yields `NULL`).
#[no_mangle]
pub unsafe extern "C" fn enif_tsd_get(key: ErlNifTSDKey) -> *mut c_void {
    // SAFETY: `pthread_getspecific` reads the calling thread's slot for `key`; a destroyed key
    // yields null.
    sync::tsd_get(key as sync::TsdKey)
}

// ---------------------------------------------------------------------------------------------
// Thread options (`enif_thread_opts_*`, `erl_nif.c:2427-2428`)
// ---------------------------------------------------------------------------------------------

/// `ErlNifThreadOpts *enif_thread_opts_create(char *name);` — `NULL` on allocation failure. `name`
/// is accepted for signature parity and ignored: the C never reads it (`erl_drv_thread.c:584-593`);
/// the thread's own name is a separate argument of [`enif_thread_create`]. The record carries the C
/// default `suggested_stack_size = -1` ("platform default").
///
/// # Safety
/// `name` must be null or a NUL-terminated C string; the returned record is owned by the caller and
/// must be released with [`enif_thread_opts_destroy`].
#[no_mangle]
pub unsafe extern "C" fn enif_thread_opts_create(name: *mut c_char) -> *mut ErlNifThreadOpts {
    // SAFETY: `name` is null or NUL-terminated and is only read for its bytes.
    sync::thread_opts_create(name)
}

/// `void enif_thread_opts_destroy(ErlNifThreadOpts *opts);` — a null handle is a no-op.
///
/// # Safety
/// `opts` must be null or a live pointer from [`enif_thread_opts_create`] that is not used
/// afterwards.
#[no_mangle]
pub unsafe extern "C" fn enif_thread_opts_destroy(opts: *mut ErlNifThreadOpts) {
    // SAFETY: `opts` is null or a live `enif_thread_opts_create` allocation (the caller contract).
    unsafe { sync::thread_opts_destroy(opts) };
}

// ---------------------------------------------------------------------------------------------
// Threads (`enif_thread_*`, `erl_nif.c:2429-2436,2441`)
// ---------------------------------------------------------------------------------------------

/// `int enif_thread_create(char *name, ErlNifTid *tid, void *(*func)(void *), void *args,
/// ErlNifThreadOpts *opts);`
///
/// Returns `0` on success, otherwise a **positive** `errno` (`erl_driver.md` §`erl_drv_thread_create`;
/// the underlying `pthread_create` result, `ethread.c:403`) — never a fabricated `0`. A null `tid`
/// or null `func` is refused with `EINVAL` where the C would fault (see the module docs). `name` is
/// the thread's debug name ([`enif_thread_name`]); `opts` may be null (platform default stack) or a
/// record from [`enif_thread_opts_create`].
///
/// # Safety
/// `name` must be null or NUL-terminated; `tid` must be a valid writable `ErlNifTid` slot (or null,
/// which returns `EINVAL`); `func` must be null or safe to call on a fresh thread; `args` is passed
/// through to `func` untouched; `opts` must be null or a live pointer from
/// [`enif_thread_opts_create`]. The caller owns `tid` until it joins the thread.
#[no_mangle]
pub unsafe extern "C" fn enif_thread_create(
    name: *mut c_char,
    tid: *mut ErlNifTid,
    func: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    args: *mut c_void,
    opts: *mut ErlNifThreadOpts,
) -> c_int {
    let Some(func) = func else {
        // The C has no null-`func` path (it would call through the null pointer); refusing it is the
        // `EINVAL` a checked build reports (`ethread.c:327-330`).
        return libc::EINVAL;
    };
    // The C entry point is `unsafe extern "C" fn`; `sync::thread_create` takes the equivalent *safe*
    // `extern "C" fn`. Both are function pointers with the same ABI and representation — `unsafe` is
    // a caller-side obligation, not part of the calling convention — so this is a bit-preserving
    // reinterpretation of one function pointer as the other (a plain `as` cast is rejected between
    // the `unsafe` and safe forms).
    // SAFETY: both types are `extern "C" fn(*mut c_void) -> *mut c_void` function pointers, i.e.
    // the same size and the same ABI; only the caller-side `unsafe` obligation differs.
    let func: extern "C" fn(*mut c_void) -> *mut c_void = unsafe {
        core::mem::transmute::<
            unsafe extern "C" fn(*mut c_void) -> *mut c_void,
            extern "C" fn(*mut c_void) -> *mut c_void,
        >(func)
    };
    // SAFETY: `name` is null or NUL-terminated, `tid` is a valid out-slot (a null one is reported as
    // `EINVAL` by the callee), `func` is a live entry point and `opts` is null or a live record —
    // `thread_create`'s own contract.
    unsafe { sync::thread_create(name, tid as *mut sync::Tid, func, args, opts) }
}

/// `ErlNifTid enif_thread_self(void);` — the calling thread's tid handle.
///
/// The handle is the bit pattern of the platform `pthread_t`, so it compares equal (under
/// [`enif_equal_tids`]) to the tid [`enif_thread_create`] wrote for this thread.
///
/// # Safety
/// No preconditions: no pointers are taken and the function never panics.
#[no_mangle]
pub unsafe extern "C" fn enif_thread_self() -> ErlNifTid {
    sync::thread_self_tid() as ErlNifTid
}

/// `int enif_equal_tids(ErlNifTid tid1, ErlNifTid tid2);` — `1` when both refer to the same thread,
/// else `0`. A null operand answers `0` where the C aborts (`erl_drv_thread.c:692`).
///
/// # Safety
/// Both operands must be null or tid handles (they are only compared, never dereferenced).
#[no_mangle]
pub unsafe extern "C" fn enif_equal_tids(tid1: ErlNifTid, tid2: ErlNifTid) -> c_int {
    sync::thread_equal_tids(tid1 as sync::Tid, tid2 as sync::Tid) as c_int
}

/// `void enif_thread_exit(void *resp);` — terminate the calling thread, returning `resp` to whoever
/// joins it. **Never returns.**
///
/// The C aborts when it is called from a thread the thread interface did not create
/// (`erl_drv_thread.c:704-713`); this implementation always exits the calling thread.
///
/// # Safety
/// `resp` is passed through to `pthread_exit` as a value and is never dereferenced.
#[no_mangle]
pub unsafe extern "C" fn enif_thread_exit(resp: *mut c_void) {
    // SAFETY: `pthread_exit` runs the platform's thread-exit sequence for the calling thread; it
    // treats `resp` as an opaque value. It does not return, so the signature's `()` is honest.
    sync::thread_exit(resp)
}

/// `int enif_thread_join(ErlNifTid tid, void **respp);` — wait for the thread and write its
/// `enif_thread_exit`/return value to `*respp` (which may be null). Returns `0` on success,
/// otherwise the platform's error code (`EINVAL` for a null tid, where the C aborts).
///
/// # Safety
/// `tid` must be null or the tid of a joinable thread created by [`enif_thread_create`] and not
/// already joined; `respp` must be null or a valid writable `void *` slot.
#[no_mangle]
pub unsafe extern "C" fn enif_thread_join(tid: ErlNifTid, respp: *mut *mut c_void) -> c_int {
    // SAFETY: `tid` is a joinable tid and `respp` a null or valid out-slot — `pthread_join`'s own
    // contract, which the callee also validates (null tid => `EINVAL`).
    unsafe { sync::thread_join(tid as sync::Tid, respp) }
}

/// `char *enif_thread_name(ErlNifTid tid);` — the debug name of a thread created by
/// [`enif_thread_create`]: the name it was handed, or the static string `"unknown"` for a thread
/// created with a NULL name (`no_name`, `erl_drv_thread.c:99,638-639`). `NULL` for a null tid
/// (`:656`) — and for a tid this layer did not create or that has already been joined (the
/// deviation below, identical to the driver facade's `erl_drv_thread_name`).
///
/// # Safety
/// `tid` must be null or a tid handle; the returned buffer stays valid until that thread is joined.
#[no_mangle]
pub unsafe extern "C" fn enif_thread_name(tid: ErlNifTid) -> *mut c_char {
    let handle = tid as sync::Tid;
    // `sync::thread_name` is safe: it only reads the registry entry keyed by the tid (a raw tid is
    // never dereferenced).
    let name = sync::thread_name(handle);
    if name.is_null() && sync::thread_created_unnamed(handle) {
        // A thread this layer created with a NULL name is the C's `no_name` state (`:638-639`). Any
        // other null — an unknown tid, a joined tid, a null handle — stays `NULL`.
        sync::no_name()
    } else {
        name
    }
}

/// `int enif_thread_type(void);` — the type of the calling thread (see the module docs for why this
/// runtime reports `ERL_NIF_THR_UNDEFINED`).
///
/// # Safety
/// No preconditions: no pointers are taken and the function never panics.
#[no_mangle]
pub unsafe extern "C" fn enif_thread_type() -> c_int {
    let ty = sync::thread_type();
    if ty == sync::ERL_NIF_THR_OTHER {
        // `ERL_NIF_THR_OTHER` is this layer's own extension for "a thread it created" and is never
        // produced by the C (`erl_nif.c:3606-3625`); the C's answer for such a thread is the
        // undefined-scheduler type, which is what the header's `0..3` set provides.
        sync::ERL_NIF_THR_UNDEFINED
    } else {
        ty
    }
}

// ---------------------------------------------------------------------------------------------
// Tests — the null/error paths and the handle semantics, driven through the public `enif_*` names
// (the C fixture in `tests/fixtures/nif_threads.c` covers the end-to-end `.so` path).
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr;
    use std::ffi::CStr;

    /// A NUL-terminated debug name for the create calls.
    const NAME: &[u8] = b"abi-threads\0";

    /// The `name` argument (a `char *` to a NUL-terminated literal).
    fn name() -> *mut c_char {
        NAME.as_ptr() as *mut c_char
    }

    /// A `void *` with the given bit pattern (the C idiom for small payloads).
    fn ptr_of(value: usize) -> *mut c_void {
        value as *mut c_void
    }

    /// The `&str` a `char *` returned by a `*_name` accessor points at.
    ///
    /// # Safety
    /// `p` must be null or NUL-terminated and valid for the call.
    unsafe fn text(p: *mut c_char) -> &'static str {
        assert!(!p.is_null(), "expected a name");
        // SAFETY: `p` is NUL-terminated and owned by a live handle for the duration of the test.
        unsafe { CStr::from_ptr(p) }.to_str().expect("ASCII name")
    }

    // -- mutex ---------------------------------------------------------------------------------

    #[test]
    fn mutex_lock_trylock_unlock_and_name() {
        let m = unsafe { enif_mutex_create(name()) };
        assert!(!m.is_null());

        // A fresh mutex is free, then busy once this thread holds it.
        assert_eq!(unsafe { enif_mutex_trylock(m) }, 0);
        assert_eq!(unsafe { enif_mutex_trylock(m) }, libc::EBUSY);
        unsafe { enif_mutex_unlock(m) };
        assert_eq!(unsafe { enif_mutex_trylock(m) }, 0);
        unsafe { enif_mutex_unlock(m) };

        // The blocking form is a no-op pair on a free mutex.
        unsafe { enif_mutex_lock(m) };
        unsafe { enif_mutex_unlock(m) };

        // SAFETY: `m` is a live handle; the returned pointer is valid until `enif_mutex_destroy`.
        let mn = unsafe { enif_mutex_name(m) };
        assert_eq!(unsafe { text(mn) }, "abi-threads");

        // A null handle is safe everywhere: `EINVAL` from trylock, no-ops elsewhere, NULL name.
        assert_eq!(unsafe { enif_mutex_trylock(ptr::null_mut()) }, libc::EINVAL);
        unsafe { enif_mutex_lock(ptr::null_mut()) };
        unsafe { enif_mutex_unlock(ptr::null_mut()) };
        unsafe { enif_mutex_destroy(ptr::null_mut()) };
        assert!(unsafe { enif_mutex_name(ptr::null_mut()) }.is_null());

        unsafe { enif_mutex_destroy(m) };
    }

    #[test]
    fn unnamed_mutex_reports_no_name() {
        let m = unsafe { enif_mutex_create(ptr::null_mut()) };
        assert!(!m.is_null());
        // A live handle never answers NULL: the C stores its static `no_name` ("unknown",
        // `erl_drv_thread.c:99,135,170`) for a mutex created with a NULL name.
        let mn = unsafe { enif_mutex_name(m) };
        assert_eq!(unsafe { text(mn) }, "unknown");
        unsafe { enif_mutex_destroy(m) };
    }

    // -- condition variable --------------------------------------------------------------------

    #[test]
    fn cond_signal_broadcast_and_name_on_a_null_handle() {
        let c = unsafe { enif_cond_create(name()) };
        assert!(!c.is_null());
        // No waiters: signal/broadcast are harmless and return.
        unsafe { enif_cond_signal(c) };
        unsafe { enif_cond_broadcast(c) };
        // SAFETY: `c` is live and its name buffer is stable until destroy.
        let cn = unsafe { enif_cond_name(c) };
        assert_eq!(unsafe { text(cn) }, "abi-threads");

        unsafe { enif_cond_signal(ptr::null_mut()) };
        unsafe { enif_cond_broadcast(ptr::null_mut()) };
        unsafe { enif_cond_wait(ptr::null_mut(), ptr::null_mut()) };
        unsafe { enif_cond_destroy(ptr::null_mut()) };
        assert!(unsafe { enif_cond_name(ptr::null_mut()) }.is_null());

        unsafe { enif_cond_destroy(c) };
    }

    #[test]
    fn unnamed_cond_reports_no_name() {
        let c = unsafe { enif_cond_create(ptr::null_mut()) };
        assert!(!c.is_null());
        // The C stores `no_name` ("unknown") for an unnamed condition variable
        // (`erl_drv_thread.c:99,264`).
        let cn = unsafe { enif_cond_name(c) };
        assert_eq!(unsafe { text(cn) }, "unknown");
        unsafe { enif_cond_destroy(c) };
    }

    #[test]
    fn cond_wait_is_woken_by_another_threads_signal() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let cnd = unsafe { enif_cond_create(name()) };
        let mtx = unsafe { enif_mutex_create(name()) };
        assert!(!cnd.is_null() && !mtx.is_null());

        // The waiter holds the mutex while it announces itself and enters `cond_wait`, which
        // releases it atomically; the signaller therefore cannot acquire the mutex (and hence cannot
        // signal) before the waiter is genuinely blocked — no lost wake-up, no hang.
        let armed = Arc::new(AtomicBool::new(false));
        let waiter_armed = Arc::clone(&armed);
        let (cnd_addr, mtx_addr) = (cnd as usize, mtx as usize);
        let waiter = std::thread::spawn(move || {
            let (cnd, mtx) = (cnd_addr as *mut ErlNifCond, mtx_addr as *mut ErlNifMutex);
            unsafe { enif_mutex_lock(mtx) };
            waiter_armed.store(true, Ordering::SeqCst);
            unsafe { enif_cond_wait(cnd, mtx) };
            // `cond_wait` re-acquired the mutex, exactly as the C contract requires.
            assert_eq!(unsafe { enif_mutex_trylock(mtx) }, libc::EBUSY);
            unsafe { enif_mutex_unlock(mtx) };
        });

        while !armed.load(Ordering::SeqCst) {
            std::hint::spin_loop();
        }
        // Blocks until the waiter released the mutex inside `cond_wait`.
        unsafe { enif_mutex_lock(mtx) };
        unsafe { enif_cond_signal(cnd) };
        unsafe { enif_mutex_unlock(mtx) };
        waiter.join().expect("the waiter must be woken");

        unsafe { enif_cond_destroy(cnd) };
        unsafe { enif_mutex_destroy(mtx) };
    }

    #[test]
    fn cond_broadcast_wakes_several_waiters() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let cnd = unsafe { enif_cond_create(name()) };
        let mtx = unsafe { enif_mutex_create(name()) };
        let awake = Arc::new(AtomicUsize::new(0));
        let (cnd_addr, mtx_addr) = (cnd as usize, mtx as usize);

        let mut waiters = Vec::new();
        for _ in 0..3 {
            let awake = Arc::clone(&awake);
            waiters.push(std::thread::spawn(move || {
                let (cnd, mtx) = (cnd_addr as *mut ErlNifCond, mtx_addr as *mut ErlNifMutex);
                unsafe { enif_mutex_lock(mtx) };
                awake.fetch_add(1, Ordering::SeqCst);
                unsafe { enif_cond_wait(cnd, mtx) };
                unsafe { enif_mutex_unlock(mtx) };
            }));
        }
        while awake.load(Ordering::SeqCst) < 3 {
            std::hint::spin_loop();
        }
        unsafe { enif_mutex_lock(mtx) };
        unsafe { enif_cond_broadcast(cnd) };
        unsafe { enif_mutex_unlock(mtx) };
        for w in waiters {
            w.join().expect("every waiter must be woken by the broadcast");
        }

        unsafe { enif_cond_destroy(cnd) };
        unsafe { enif_mutex_destroy(mtx) };
    }

    // -- reader/writer lock --------------------------------------------------------------------

    #[test]
    fn rwlock_read_write_and_try_forms() {
        let l = unsafe { enif_rwlock_create(name()) };
        assert!(!l.is_null());

        // A read lock excludes a write lock.
        unsafe { enif_rwlock_rlock(l) };
        assert_eq!(unsafe { enif_rwlock_tryrwlock(l) }, libc::EBUSY);
        unsafe { enif_rwlock_runlock(l) };

        // With nothing held, both try-forms succeed.
        assert_eq!(unsafe { enif_rwlock_tryrwlock(l) }, 0);
        unsafe { enif_rwlock_rwunlock(l) };
        assert_eq!(unsafe { enif_rwlock_tryrlock(l) }, 0);
        unsafe { enif_rwlock_runlock(l) };

        // A write lock excludes a read lock, and the blocking forms round-trip.
        unsafe { enif_rwlock_rwlock(l) };
        assert_eq!(unsafe { enif_rwlock_tryrlock(l) }, libc::EBUSY);
        unsafe { enif_rwlock_rwunlock(l) };
        unsafe { enif_rwlock_rlock(l) };
        unsafe { enif_rwlock_runlock(l) };

        // SAFETY: `l` is live and its name buffer is stable until destroy.
        let ln = unsafe { enif_rwlock_name(l) };
        assert_eq!(unsafe { text(ln) }, "abi-threads");

        // A null handle: `EINVAL` from the try forms, no-ops elsewhere, NULL name.
        assert_eq!(unsafe { enif_rwlock_tryrlock(ptr::null_mut()) }, libc::EINVAL);
        assert_eq!(unsafe { enif_rwlock_tryrwlock(ptr::null_mut()) }, libc::EINVAL);
        unsafe { enif_rwlock_rlock(ptr::null_mut()) };
        unsafe { enif_rwlock_runlock(ptr::null_mut()) };
        unsafe { enif_rwlock_rwlock(ptr::null_mut()) };
        unsafe { enif_rwlock_rwunlock(ptr::null_mut()) };
        unsafe { enif_rwlock_destroy(ptr::null_mut()) };
        assert!(unsafe { enif_rwlock_name(ptr::null_mut()) }.is_null());

        unsafe { enif_rwlock_destroy(l) };
    }

    #[test]
    fn unnamed_rwlock_reports_no_name() {
        let l = unsafe { enif_rwlock_create(ptr::null_mut()) };
        assert!(!l.is_null());
        // The C stores `no_name` ("unknown") for an unnamed reader/writer lock
        // (`erl_drv_thread.c:99,341`).
        let ln = unsafe { enif_rwlock_name(l) };
        assert_eq!(unsafe { text(ln) }, "unknown");
        unsafe { enif_rwlock_destroy(l) };
    }

    // -- thread-specific data ------------------------------------------------------------------

    #[test]
    fn tsd_key_rejects_a_null_out_slot() {
        // The out-slot still receives the C's `-1` sentinel when the create fails.
        let mut key: ErlNifTSDKey = 7;
        assert_eq!(
            unsafe { enif_tsd_key_create(name(), ptr::null_mut()) },
            libc::EINVAL
        );
        assert_eq!(unsafe { enif_tsd_key_create(name(), &mut key) }, 0);
        assert!(key >= 0, "a real pthread key is non-negative, got {key}");
        unsafe { enif_tsd_key_destroy(key) };
    }

    #[test]
    fn tsd_is_per_thread() {
        let mut key: ErlNifTSDKey = -1;
        assert_eq!(unsafe { enif_tsd_key_create(name(), &mut key) }, 0);

        let here = ptr_of(0x11);
        unsafe { enif_tsd_set(key, here) };
        assert_eq!(unsafe { enif_tsd_get(key) }, here);

        // A fresh thread starts with the slot unset and its write is invisible here. (The closure
        // returns a `usize` — a raw pointer is not `Send`, and the child's payload is what is being
        // asserted.)
        let child = std::thread::spawn(move || {
            assert!(unsafe { enif_tsd_get(key) }.is_null(), "TSD must be per-thread");
            let there = ptr_of(0x22);
            unsafe { enif_tsd_set(key, there) };
            unsafe { enif_tsd_get(key) as usize }
        });
        assert_eq!(child.join().expect("the child cannot panic"), 0x22);
        assert_eq!(unsafe { enif_tsd_get(key) }, here, "the parent's slot is intact");

        unsafe { enif_tsd_key_destroy(key) };
        // A destroyed key is a no-op/null path, never a dereference.
        assert!(unsafe { enif_tsd_get(key) }.is_null());
        unsafe { enif_tsd_set(key, here) };
        unsafe { enif_tsd_key_destroy(u32::MAX as ErlNifTSDKey) };
    }

    // -- threads -------------------------------------------------------------------------------

    /// Returns `0x2a` — the join payload the tests assert.
    unsafe extern "C" fn answer(_: *mut c_void) -> *mut c_void {
        ptr_of(0x2a)
    }

    /// Reports its own `enif_thread_type()` as the join payload.
    unsafe extern "C" fn report_type(_: *mut c_void) -> *mut c_void {
        ptr_of(unsafe { enif_thread_type() } as usize)
    }

    /// Exits through [`enif_thread_exit`] instead of returning.
    unsafe extern "C" fn exits(_: *mut c_void) -> *mut c_void {
        unsafe { enif_thread_exit(ptr_of(0x99)) };
        ptr_of(0xdead)
    }

    #[test]
    fn thread_create_join_self_and_equal_type() {
        let opts = unsafe { enif_thread_opts_create(name()) };
        assert!(!opts.is_null());

        let mut tid: ErlNifTid = ptr::null_mut();
        assert_eq!(
            unsafe { enif_thread_create(name(), &mut tid, Some(answer), ptr::null_mut(), opts) },
            0
        );
        // The created thread is named until it is joined.
        let nm = unsafe { enif_thread_name(tid) };
        assert_eq!(unsafe { text(nm) }, "abi-threads");

        let mut resp = ptr_of(0);
        assert_eq!(unsafe { enif_thread_join(tid, &mut resp) }, 0);
        assert_eq!(resp, ptr_of(0x2a));
        // Joining retires the registry entry (documented deviation vs a dangling C pointer).
        assert!(unsafe { enif_thread_name(tid) }.is_null());

        unsafe { enif_thread_opts_destroy(opts) };
        unsafe { enif_thread_opts_destroy(ptr::null_mut()) };

        // `enif_thread_self` yields a tid equal to itself, and to the calling thread's own.
        let me = unsafe { enif_thread_self() };
        let again = unsafe { enif_thread_self() };
        assert!(!me.is_null());
        assert_eq!(unsafe { enif_equal_tids(me, me) }, 1);
        assert_eq!(unsafe { enif_equal_tids(me, again) }, 1);
        assert_eq!(unsafe { enif_equal_tids(ptr::null_mut(), ptr::null_mut()) }, 0);
        assert_eq!(unsafe { enif_equal_tids(me, ptr::null_mut()) }, 0);

        // A failure is reported as a positive `errno` — never a fabricated success.
        assert_eq!(
            unsafe { enif_thread_create(name(), &mut tid, None, ptr::null_mut(), ptr::null_mut()) },
            libc::EINVAL,
            "a null entry point is refused"
        );
        assert_eq!(
            unsafe { enif_thread_create(name(), ptr::null_mut(), Some(answer), ptr::null_mut(), ptr::null_mut()) },
            libc::EINVAL,
            "a null tid out-slot is refused"
        );
    }

    #[test]
    fn unnamed_thread_reports_no_name_and_a_foreign_tid_is_null() {
        // A thread created with a NULL name carries the C's static `no_name` ("unknown",
        // `erl_drv_thread.c:99,638-639`), not NULL.
        let mut tid: ErlNifTid = ptr::null_mut();
        assert_eq!(
            unsafe { enif_thread_create(ptr::null_mut(), &mut tid, Some(answer), ptr::null_mut(), ptr::null_mut()) },
            0
        );
        let nm = unsafe { enif_thread_name(tid) };
        assert_eq!(unsafe { text(nm) }, "unknown");
        let mut resp = ptr_of(0);
        assert_eq!(unsafe { enif_thread_join(tid, &mut resp) }, 0);
        assert_eq!(resp, ptr_of(0x2a));
        // The entry is retired by the join, so the name is gone (the C frees its record).
        assert!(unsafe { enif_thread_name(tid) }.is_null());

        // A tid this layer did not create — the calling thread's own `enif_thread_self()` — answers
        // NULL here; the C returns "unknown" through the `drv_thr = 0` record `erl_drv_thread_self`
        // mints (`erl_drv_thread.c:664-684`). Documented deviation (see the module docs): a foreign
        // tid carries no record in this layer.
        let me = unsafe { enif_thread_self() };
        assert!(!me.is_null());
        assert!(unsafe { enif_thread_name(me) }.is_null());
        assert!(unsafe { enif_thread_name(ptr::null_mut()) }.is_null());
    }

    #[test]
    fn thread_type_is_undefined_for_this_runtime() {
        // The calling (test) thread was not created by the thread layer.
        assert_eq!(
            unsafe { enif_thread_type() },
            crate::nif::types::ERL_NIF_THR_UNDEFINED
        );

        // …and so is a thread this layer *did* create: `sync::thread_type`'s extension value
        // `ERL_NIF_THR_OTHER` (-1) is mapped to the header's `ERL_NIF_THR_UNDEFINED`, because the C
        // never reports a negative type for a NIF thread.
        let mut tid: ErlNifTid = ptr::null_mut();
        assert_eq!(
            unsafe {
                enif_thread_create(name(), &mut tid, Some(report_type), ptr::null_mut(), ptr::null_mut())
            },
            0
        );
        let mut resp = ptr_of(0xffff);
        assert_eq!(unsafe { enif_thread_join(tid, &mut resp) }, 0);
        assert_eq!(resp as usize, 0);
    }

    #[test]
    fn thread_exit_hands_its_argument_to_the_joiner() {
        let mut tid: ErlNifTid = ptr::null_mut();
        assert_eq!(
            unsafe {
                enif_thread_create(name(), &mut tid, Some(exits), ptr::null_mut(), ptr::null_mut())
            },
            0
        );
        let mut resp = ptr::null_mut();
        assert_eq!(unsafe { enif_thread_join(tid, &mut resp) }, 0);
        assert_eq!(resp, ptr_of(0x99), "the exit argument is the join result");
        // A null tid is `EINVAL` (the C aborts on it).
        assert_eq!(
            unsafe { enif_thread_join(ptr::null_mut(), &mut resp) },
            libc::EINVAL
        );
    }
}
