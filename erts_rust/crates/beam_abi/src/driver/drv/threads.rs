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

//! **Threads & synchronisation** family of the driver host layer — part of the `driver_*` /
//! `erl_drv_*` C ABI (workspace contract **A.4**).
//!
//! This file owns the `erl_drv_*` functions a driver uses to build **threads and the
//! synchronisation objects they share**: mutexes, condition variables, read/write locks,
//! thread-specific data, thread options and the thread lifecycle. Every one is a
//! `#[no_mangle] pub unsafe extern "C" fn` matching one field of the frozen
//! [`ErlDrvApi`](crate::driver::api::ErlDrvApi) table **exactly**, and every one is a thin,
//! cast-and-forward wrapper over the shared primitive layer [`crate::sync`] — the Rust equivalent of
//! the C's single implementation in `erts/emulator/beam/erl_drv_thread.c`.
//!
//! # Why there is no `DriverHost` seam here
//!
//! Unlike the port families, **none of these functions take an `ErlDrvPort`**: the C mints the
//! handles itself (`erl_drv_mutex_create` `erts_alloc`s an `ErlDrvMutex` and returns it), so a
//! mutex/cond/rwlock/tid/tsd-key is an object this runtime owns outright — not a per-port service.
//! The family therefore needs no host hook and no port-pointer recovery; it is the one driver family
//! that is fully self-contained.
//!
//! # Slots owned by this file (33)
//!
//! | slot | C | state |
//! |---|---|---|
//! | `erl_drv_mutex_create` | `erl_drv_thread.c:154` | implemented |
//! | `erl_drv_mutex_destroy` | `erl_drv_thread.c:184` | implemented |
//! | `erl_drv_mutex_trylock` | `erl_drv_thread.c:204` | implemented |
//! | `erl_drv_mutex_lock` | `erl_drv_thread.c:220` | implemented |
//! | `erl_drv_mutex_unlock` | `erl_drv_thread.c:237` | implemented |
//! | `erl_drv_mutex_name` | `erl_drv_thread.c:198` | implemented |
//! | `erl_drv_cond_create` | `erl_drv_thread.c:251` | implemented |
//! | `erl_drv_cond_destroy` | `erl_drv_thread.c:274` | implemented |
//! | `erl_drv_cond_signal` | `erl_drv_thread.c:289` | implemented |
//! | `erl_drv_cond_broadcast` | `erl_drv_thread.c:297` | implemented |
//! | `erl_drv_cond_wait` | `erl_drv_thread.c:306` | implemented |
//! | `erl_drv_cond_name` | `erl_drv_thread.c:283` | implemented |
//! | `erl_drv_rwlock_create` | `erl_drv_thread.c:327` | implemented |
//! | `erl_drv_rwlock_destroy` | `erl_drv_thread.c:352` | implemented |
//! | `erl_drv_rwlock_tryrlock` | `erl_drv_thread.c:371` | implemented |
//! | `erl_drv_rwlock_rlock` | `erl_drv_thread.c:384` | implemented |
//! | `erl_drv_rwlock_runlock` | `erl_drv_thread.c:398` | implemented |
//! | `erl_drv_rwlock_tryrwlock` | `erl_drv_thread.c:409` | implemented |
//! | `erl_drv_rwlock_rwlock` | `erl_drv_thread.c:422` | implemented |
//! | `erl_drv_rwlock_rwunlock` | `erl_drv_thread.c:436` | implemented |
//! | `erl_drv_rwlock_name` | `erl_drv_thread.c:365` | implemented |
//! | `erl_drv_tsd_key_create` | `erl_drv_thread.c:447` | implemented |
//! | `erl_drv_tsd_key_destroy` | `erl_drv_thread.c:519` | implemented |
//! | `erl_drv_tsd_set` | `erl_drv_thread.c:541` | implemented |
//! | `erl_drv_tsd_get` | `erl_drv_thread.c:569` | implemented |
//! | `erl_drv_thread_opts_create` | `erl_drv_thread.c:585` | implemented |
//! | `erl_drv_thread_opts_destroy` | `erl_drv_thread.c:596` | implemented |
//! | `erl_drv_thread_create` | `erl_drv_thread.c:604` | implemented |
//! | `erl_drv_thread_self` | `erl_drv_thread.c:664` | implemented |
//! | `erl_drv_equal_tids` | `erl_drv_thread.c:687` | implemented |
//! | `erl_drv_thread_exit` | `erl_drv_thread.c:705` | implemented |
//! | `erl_drv_thread_join` | `erl_drv_thread.c:716` | implemented |
//! | `erl_drv_thread_name` | `erl_drv_thread.c:656` | implemented |
//!
//! **Every slot this family owns is populated** — there is no `None` left in the group.
//!
//! `erl_drv_thread_type` is **not** a slot: the OTP-29 headers of this release declare no such
//! symbol (`erl_driver.h` has the thread/mutex/cond/rwlock/tsd surface only; `enif_thread_type` in
//! `erl_nif.h` has no driver ABI counterpart), so declaring one would be an invented ABI. Nothing is
//! added for it. (The shared layer still exposes `sync::thread_type` for the NIF side.)
//!
//! # Faithfulness to the C entry points
//!
//! * `erl_drv_mutex_create`/`erl_drv_cond_create`/`erl_drv_rwlock_create`/`erl_drv_thread_opts_create`
//!   return `NULL` on failure; the handle they hand out is the runtime's own object
//!   (`erl_drv_thread.c:154-181`, `:251-272`, `:327-349`, `:585-593`).
//! * `erl_drv_mutex_trylock`/`erl_drv_rwlock_tryrlock`/`erl_drv_rwlock_tryrwlock` return the C's
//!   convention: `0` = acquired, `EBUSY` = not acquired (`erl_drv_thread.c:209-211`), and the
//!   `ethr_*` codes are the platform pthreads codes this layer forwards.
//! * `erl_drv_cond_wait` atomically releases the mutex and re-acquires it before returning.
//! * `erl_drv_thread_create` writes the new tid to its out-parameter and returns `0`, or a positive
//!   errno on failure (`erl_drv_thread.c:646-652`).
//! * `erl_drv_thread_join` returns `0` and writes the thread's response value through `respp`
//!   (`erl_drv_thread.c:716-730`); `erl_drv_thread_exit` never returns.
//! * `erl_drv_equal_tids` returns `1` for two handles of the same thread and `0` otherwise
//!   (`erl_drv_thread.c:687-702`).
//! * `erl_drv_tsd_key_create` writes the new key and returns `0`, or writes the C's `-1` sentinel and
//!   returns a positive errno (`erl_drv_thread.c:447-516`).
//! * the `*_name` accessors return the object's own NUL-terminated name buffer, so a driver may
//!   `strcmp` it against the name it passed to `*_create`.
//!
//! # Documented deviations from the oracle (mostly inherited from [`crate::sync`]; never a wrong
//! value)
//!
//! The C makes every `enif_*` thread function a one-line forward to its `erl_drv_*` sibling
//! (`erl_nif.c:2405-2441`), so both facades share one implementation and one deviation list — the
//! `*_name` accessors are the one place where the *facades* differ (see that bullet):
//!
//! * **A tid handle is the `pthread_t` bit pattern** (the C returns a per-thread `struct ErlDrvTid_ *`
//!   record). `erl_drv_thread_self` therefore returns a handle that compares *equal* under
//!   `erl_drv_equal_tids` to the tid a `erl_drv_thread_create` produced for the same thread, exactly
//!   as the C guarantees; what differs is only the pointer's identity.
//! * **The `*_name` accessors answer the C's value for every live handle**: the name handed to
//!   `*_create`, or the C's static string `"unknown"` (`no_name`, `erl_drv_thread.c:99,135`, stored in
//!   the handle at `:170`/`:264`/`:341`/`:639`/`:678`) for a handle created with a NULL name — `NULL`
//!   only for a **null handle** (`erl_drv_thread.c:198,283,365,656`). [`crate::sync`] keeps the unnamed
//!   state as a *null* name and this file renders it ([`sync::no_name`], [`rendered_name`]). The
//!   **NIF** facade over that same layer (`crate::nif::enif`, a sibling node) still answers `NULL`
//!   there: it is the one remaining divergence in this family.
//! * **`erl_drv_thread_name` is `NULL` for a thread this layer did not create**; the C mints a
//!   `drv_thr = 0` record with `name = "unknown"` for any other thread
//!   (`erl_drv_thread.c:664-684`). After a `erl_drv_thread_join` the entry is retired and the name
//!   becomes `NULL` rather than a dangling pointer.
//! * **Handle misuse never aborts.** The C `erts_exit`s (killing the whole VM) on a `NULL`
//!   mutex/cond/rwlock/tid, on an unused/destroyed TSD key, and on `erl_drv_thread_exit` from a
//!   thread it did not create; this runtime turns each into the documented failure code
//!   (`EINVAL`/`NULL`/a safe no-op) instead — a library function must not abort its host process
//!   (workspace `CONTEXT.md` §3, never panic-as-control-flow).
//! * **TSD keys are real pthread keys** (`pthread_key_create`/`key_delete`/`setspecific`/
//!   `getspecific`), not the C's own key counter table; the observable contract (one `void *` slot
//!   per thread per key) is identical, and the `name` argument is accepted and dropped as the C drops
//!   it (`erl_drv_thread.c:449-466` stores it only for its own key validation).
//! * **`erl_drv_tsd_key_create` reports the platform's errno** (pthreads' `EAGAIN`/`EINVAL`) where the
//!   C manufactures `ENOMEM` from its own table growth (`erl_drv_thread.c:463,473`) — both are
//!   positive errno values, and `*key` is the C's `-1` sentinel in both.
//! * **`erl_drv_cond_wait` makes a single `pthread_cond_wait` call**; the C retries until it returns
//!   `0`, a loop that cannot terminate on a persistent error.
//! * **`suggested_stack_size` is kilowords** (the C's `ETHR_KW2B` unit, `erl_driver.h:637-638`), the
//!   converted size is clamped into the C's own window and a size the platform rejects falls back to
//!   the default stack — exactly as the C ignores `pthread_attr_setstacksize`'s result.
//!
//! # `unsafe`
//!
//! The whole file is FFI. Every function carries a `# Safety` section stating its caller contract
//! (a handle must be null or live, a name must be null or NUL-terminated, an out-parameter must be
//! writable) and every `unsafe` block a preceding `// SAFETY:` rationale (workspace `CONTEXT.md` §3).
//! The only pointer arithmetic is the handle cast between the C opaque type and the
//! [`crate::sync`] object behind it, done through one documented helper per kind; no runtime data is
//! `unwrap()`ed and no failure path panics.

use libc::{c_char, c_int, c_void};

use crate::driver::api::ErlDrvApi;
use crate::driver::types::{
    ErlDrvCond, ErlDrvMutex, ErlDrvRWLock, ErlDrvTSDKey, ErlDrvThreadOpts, ErlDrvTid,
    ErlDrvTid_,
};
use crate::sync;

// ---------------------------------------------------------------------------------------------
// Handle and key conversions (the one place each C opaque type meets its `crate::sync` object)
// ---------------------------------------------------------------------------------------------
//
// Each helper is a pure pointer/value cast: the C's opaque `struct ErlDrv*_` is a *name* for the
// `crate::sync` object allocation (`erl_driver.h` never defines its body, so no layout claim is
// made), and `ErlDrvTSDKey` is `int` where the layer's key type is `unsigned int`.

/// The `crate::sync` mutex behind a C `ErlDrvMutex *`.
///
/// # Safety
/// `mtx` must be null or a live handle from [`erl_drv_mutex_create`].
unsafe fn mutex_handle(mtx: *mut ErlDrvMutex) -> *mut sync::Mutex {
    mtx.cast()
}

/// The `crate::sync` condition variable behind a C `ErlDrvCond *`.
///
/// # Safety
/// `cnd` must be null or a live handle from [`erl_drv_cond_create`].
unsafe fn cond_handle(cnd: *mut ErlDrvCond) -> *mut sync::Cond {
    cnd.cast()
}

/// The `crate::sync` read/write lock behind a C `ErlDrvRWLock *`.
///
/// # Safety
/// `lck` must be null or a live handle from [`erl_drv_rwlock_create`].
unsafe fn rwlock_handle(lck: *mut ErlDrvRWLock) -> *mut sync::RwLock {
    lck.cast()
}

/// The `crate::sync` thread handle behind a C `ErlDrvTid`.
///
/// # Safety
/// `tid` must be null or a handle from [`erl_drv_thread_create`]/[`erl_drv_thread_self`].
unsafe fn tid_handle(tid: ErlDrvTid) -> sync::Tid {
    tid.cast()
}

/// The `crate::sync` TSD key for a driver `ErlDrvTSDKey` (`int`, `erl_driver.h:208`).
///
/// The C facade's `int` and the layer's `unsigned int` (the platform `pthread_key_t`) are the same
/// 32 bits. A live key is a small non-negative value, and the failure sentinel the layer writes is
/// `u32::MAX` — i.e. `-1` read as an `int`, exactly the C's sentinel. The cast is therefore
/// value-preserving in both directions.
const fn tsd_key_from_driver(key: ErlDrvTSDKey) -> sync::TsdKey {
    key as sync::TsdKey
}

/// The driver `ErlDrvTSDKey` for a `crate::sync` TSD key (the inverse of
/// [`tsd_key_from_driver`]).
const fn tsd_key_to_driver(key: sync::TsdKey) -> ErlDrvTSDKey {
    key as ErlDrvTSDKey
}

// ---------------------------------------------------------------------------------------------
// Mutex family (`erl_drv_thread.c:153-248`)
// ---------------------------------------------------------------------------------------------

/// `ErlDrvMutex *erl_drv_mutex_create(char *name)` (`erl_driver.h:460`) — create a driver mutex,
/// `NULL` on failure. The name is copied into the handle for the debug `erl_drv_mutex_name`
/// accessor.
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_mutex_create(name: *mut c_char) -> *mut ErlDrvMutex {
    // SAFETY: the caller contract above is `sync::mutex_create`'s own (`name` is null or a valid
    // NUL-terminated C string). The returned box is the handle; only the `erl_drv_mutex_*` entry
    // points below ever reinterpret it back.
    unsafe { sync::mutex_create(name).cast::<ErlDrvMutex>() }
}

/// `void erl_drv_mutex_destroy(ErlDrvMutex *mtx)` (`erl_driver.h:459`) — release the handle.
///
/// # Safety
/// `mtx` must be null or a live handle from [`erl_drv_mutex_create`] that is not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_mutex_destroy(mtx: *mut ErlDrvMutex) {
    // SAFETY: `mtx` is null or a live handle (the caller contract), which is `sync::mutex_destroy`'s.
    unsafe { sync::mutex_destroy(mutex_handle(mtx)) }
}

/// `int erl_drv_mutex_trylock(ErlDrvMutex *mtx)` (`erl_driver.h:461`) — `0` when the mutex was
/// acquired, non-zero (`EBUSY`) when it is held by someone else.
///
/// # Safety
/// `mtx` must be null or a live handle from [`erl_drv_mutex_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_mutex_trylock(mtx: *mut ErlDrvMutex) -> c_int {
    // SAFETY: `mtx` is null or a live handle (the caller contract).
    unsafe { sync::mutex_try_lock(mutex_handle(mtx)) }
}

/// `void erl_drv_mutex_lock(ErlDrvMutex *mtx)` (`erl_driver.h:462`) — take the mutex, blocking.
///
/// # Safety
/// `mtx` must be null or a live handle from [`erl_drv_mutex_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_mutex_lock(mtx: *mut ErlDrvMutex) {
    // SAFETY: `mtx` is null or a live handle (the caller contract).
    unsafe { sync::mutex_lock(mutex_handle(mtx)) }
}

/// `void erl_drv_mutex_unlock(ErlDrvMutex *mtx)` (`erl_driver.h:463`) — release the mutex.
///
/// As in the C, the mutex is not error-checking, so it may be released by a thread other than the
/// one that took it (`erl_drv_thread.c:160-161`).
///
/// # Safety
/// `mtx` must be null or a live handle from [`erl_drv_mutex_create`] held by some thread.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_mutex_unlock(mtx: *mut ErlDrvMutex) {
    // SAFETY: `mtx` is null or a live handle (the caller contract).
    unsafe { sync::mutex_unlock(mutex_handle(mtx)) }
}

/// Render a shared-layer `*_name` result the way the C's accessors do
/// (`erl_drv_thread.c:198`, `:283`, `:365`, `:656`: `handle ? handle->name : NULL`).
///
/// A **null handle** answers `NULL`. A **live handle** never answers `NULL` in the C: a handle created
/// with a NULL name carries the static `no_name` string (`"unknown"`, `erl_drv_thread.c:99,135`,
/// stored at `:170`/`:264`/`:341`/`:639`). [`crate::sync`] represents that state as a *null* name, so
/// it is rendered as the C's `no_name` here ([`sync::no_name`]).
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

/// `char *erl_drv_mutex_name(ErlDrvMutex *mtx)` (`erl_driver.h:493`) — the handle's debug name.
///
/// The name [`erl_drv_mutex_create`] was handed, the static string `"unknown"` for a mutex created
/// with a NULL name (`no_name`, `erl_drv_thread.c:99,170`), or `NULL` for a null handle (`:198`).
///
/// The pointer is valid until the handle is destroyed.
///
/// # Safety
/// `mtx` must be null or a live handle from [`erl_drv_mutex_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_mutex_name(mtx: *mut ErlDrvMutex) -> *mut c_char {
    // SAFETY: `mtx` is null or a live handle (the caller contract).
    let handle = unsafe { mutex_handle(mtx) };
    // SAFETY: `handle` is null or a live mutex handle (as above); the accessor only reads its name.
    rendered_name(!handle.is_null(), unsafe { sync::mutex_name(handle) })
}

// ---------------------------------------------------------------------------------------------
// Condition-variable family (`erl_drv_thread.c:250-324`)
// ---------------------------------------------------------------------------------------------

/// `ErlDrvCond *erl_drv_cond_create(char *name)` (`erl_driver.h:465`) — create a condition
/// variable, `NULL` on failure.
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_cond_create(name: *mut c_char) -> *mut ErlDrvCond {
    // SAFETY: the caller contract above is `sync::cond_create`'s own.
    unsafe { sync::cond_create(name).cast::<ErlDrvCond>() }
}

/// `void erl_drv_cond_destroy(ErlDrvCond *cnd)` (`erl_driver.h:464`) — release the handle.
///
/// # Safety
/// `cnd` must be null or a live handle from [`erl_drv_cond_create`] that is not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_cond_destroy(cnd: *mut ErlDrvCond) {
    // SAFETY: `cnd` is null or a live handle (the caller contract).
    unsafe { sync::cond_destroy(cond_handle(cnd)) }
}

/// `void erl_drv_cond_signal(ErlDrvCond *cnd)` (`erl_driver.h:466`) — wake one waiter.
///
/// # Safety
/// `cnd` must be null or a live handle from [`erl_drv_cond_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_cond_signal(cnd: *mut ErlDrvCond) {
    // SAFETY: `cnd` is null or a live handle (the caller contract).
    unsafe { sync::cond_signal(cond_handle(cnd)) }
}

/// `void erl_drv_cond_broadcast(ErlDrvCond *cnd)` (`erl_driver.h:467`) — wake every waiter.
///
/// # Safety
/// `cnd` must be null or a live handle from [`erl_drv_cond_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_cond_broadcast(cnd: *mut ErlDrvCond) {
    // SAFETY: `cnd` is null or a live handle (the caller contract).
    unsafe { sync::cond_broadcast(cond_handle(cnd)) }
}

/// `void erl_drv_cond_wait(ErlDrvCond *cnd, ErlDrvMutex *mtx)` (`erl_driver.h:468`) — wait on the
/// condition variable, atomically releasing `mtx` and re-acquiring it before returning.
///
/// There is no timed driver-cond wait: the header declares one only for the NIF facade
/// (`enif_cond_wait_timed`), so no such slot exists in [`ErlDrvApi`].
///
/// # Safety
/// `cnd` must be null or a live handle from [`erl_drv_cond_create`], and `mtx` must be null or a
/// live handle from [`erl_drv_mutex_create`] **currently held by the calling thread** (pthread's
/// contract for `pthread_cond_wait`).
#[no_mangle]
pub unsafe extern "C" fn erl_drv_cond_wait(cnd: *mut ErlDrvCond, mtx: *mut ErlDrvMutex) {
    // SAFETY: both operands are null or live handles of the required kinds held as the caller
    // promises; `pthread_cond_wait` releases `mtx` while waiting and re-acquires it before returning.
    unsafe { sync::cond_wait(cond_handle(cnd), mutex_handle(mtx)) }
}

/// `char *erl_drv_cond_name(ErlDrvCond *cnd)` (`erl_driver.h:494`) — the handle's debug name.
///
/// The name [`erl_drv_cond_create`] was handed, the static string `"unknown"` for a condition variable
/// created with a NULL name (`no_name`, `erl_drv_thread.c:99,264`), or `NULL` for a null handle
/// (`:283`).
///
/// # Safety
/// `cnd` must be null or a live handle from [`erl_drv_cond_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_cond_name(cnd: *mut ErlDrvCond) -> *mut c_char {
    // SAFETY: `cnd` is null or a live handle (the caller contract).
    let handle = unsafe { cond_handle(cnd) };
    // SAFETY: `handle` is null or a live condition-variable handle (as above).
    rendered_name(!handle.is_null(), unsafe { sync::cond_name(handle) })
}

// ---------------------------------------------------------------------------------------------
// Read/write lock family (`erl_drv_thread.c:326-444`)
//
// The header spells the write operations `*rwlock`/`*rwunlock` (not `*wlock`/`*wunlock`), and the
// `ErlDrvApi` field names follow it verbatim.
// ---------------------------------------------------------------------------------------------

/// `ErlDrvRWLock *erl_drv_rwlock_create(char *name)` (`erl_driver.h:470`) — create a read/write
/// lock, `NULL` on failure.
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_rwlock_create(name: *mut c_char) -> *mut ErlDrvRWLock {
    // SAFETY: the caller contract above is `sync::rwlock_create`'s own.
    unsafe { sync::rwlock_create(name).cast::<ErlDrvRWLock>() }
}

/// `void erl_drv_rwlock_destroy(ErlDrvRWLock *rwlck)` (`erl_driver.h:469`) — release the handle.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`erl_drv_rwlock_create`] that is not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_rwlock_destroy(rwlck: *mut ErlDrvRWLock) {
    // SAFETY: `rwlck` is null or a live handle (the caller contract).
    unsafe { sync::rwlock_destroy(rwlock_handle(rwlck)) }
}

/// `int erl_drv_rwlock_tryrlock(ErlDrvRWLock *rwlck)` (`erl_driver.h:471`) — `0` when the read lock
/// was taken, non-zero (`EBUSY`) when a writer holds it.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`erl_drv_rwlock_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_rwlock_tryrlock(rwlck: *mut ErlDrvRWLock) -> c_int {
    // SAFETY: `rwlck` is null or a live handle (the caller contract).
    unsafe { sync::rwlock_try_read_lock(rwlock_handle(rwlck)) }
}

/// `void erl_drv_rwlock_rlock(ErlDrvRWLock *rwlck)` (`erl_driver.h:472`) — take the read lock,
/// blocking while a writer holds it.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`erl_drv_rwlock_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_rwlock_rlock(rwlck: *mut ErlDrvRWLock) {
    // SAFETY: `rwlck` is null or a live handle (the caller contract).
    unsafe { sync::rwlock_read_lock(rwlock_handle(rwlck)) }
}

/// `void erl_drv_rwlock_runlock(ErlDrvRWLock *rwlck)` (`erl_driver.h:473`) — release the read lock.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`erl_drv_rwlock_create`] held for reading by the
/// calling thread.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_rwlock_runlock(rwlck: *mut ErlDrvRWLock) {
    // SAFETY: `rwlck` is null or a live handle held for reading (the caller contract).
    unsafe { sync::rwlock_read_unlock(rwlock_handle(rwlck)) }
}

/// `int erl_drv_rwlock_tryrwlock(ErlDrvRWLock *rwlck)` (`erl_driver.h:474`) — `0` when the write
/// lock was taken, non-zero (`EBUSY`) when any reader or writer holds the lock.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`erl_drv_rwlock_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_rwlock_tryrwlock(rwlck: *mut ErlDrvRWLock) -> c_int {
    // SAFETY: `rwlck` is null or a live handle (the caller contract).
    unsafe { sync::rwlock_try_write_lock(rwlock_handle(rwlck)) }
}

/// `void erl_drv_rwlock_rwlock(ErlDrvRWLock *rwlck)` (`erl_driver.h:475`) — take the write lock,
/// blocking while any reader or writer holds the lock.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`erl_drv_rwlock_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_rwlock_rwlock(rwlck: *mut ErlDrvRWLock) {
    // SAFETY: `rwlck` is null or a live handle (the caller contract).
    unsafe { sync::rwlock_write_lock(rwlock_handle(rwlck)) }
}

/// `void erl_drv_rwlock_rwunlock(ErlDrvRWLock *rwlck)` (`erl_driver.h:476`) — release the write
/// lock.
///
/// # Safety
/// `rwlck` must be null or a live handle from [`erl_drv_rwlock_create`] held for writing by the
/// calling thread.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_rwlock_rwunlock(rwlck: *mut ErlDrvRWLock) {
    // SAFETY: `rwlck` is null or a live handle held for writing (the caller contract).
    unsafe { sync::rwlock_write_unlock(rwlock_handle(rwlck)) }
}

/// `char *erl_drv_rwlock_name(ErlDrvRWLock *rwlck)` (`erl_driver.h:495`) — the handle's debug name.
///
/// The name [`erl_drv_rwlock_create`] was handed, the static string `"unknown"` for a lock created
/// with a NULL name (`no_name`, `erl_drv_thread.c:99,341`), or `NULL` for a null handle (`:365`).
///
/// # Safety
/// `rwlck` must be null or a live handle from [`erl_drv_rwlock_create`].
#[no_mangle]
pub unsafe extern "C" fn erl_drv_rwlock_name(rwlck: *mut ErlDrvRWLock) -> *mut c_char {
    // SAFETY: `rwlck` is null or a live handle (the caller contract).
    let handle = unsafe { rwlock_handle(rwlck) };
    // SAFETY: `handle` is null or a live read/write-lock handle (as above).
    rendered_name(!handle.is_null(), unsafe { sync::rwlock_name(handle) })
}

// ---------------------------------------------------------------------------------------------
// Thread-specific data (`erl_drv_thread.c:446-579`)
// ---------------------------------------------------------------------------------------------

/// `int erl_drv_tsd_key_create(char *name, ErlDrvTSDKey *key)` (`erl_driver.h:477`) — reserve a
/// thread-specific-data key: `0` on success with the new key written through `key`, else a positive
/// errno with the C's `-1` sentinel written through `key`
/// (`erl_drv_thread.c:462,470,515`).
///
/// The caller's `name` is only ever the C's own key bookkeeping, so it is accepted and dropped (see
/// the deviations above).
///
/// # Safety
/// `key` must be a valid writable `ErlDrvTSDKey` slot; `name` must be null or a valid
/// NUL-terminated C string. A null `key` is refused with `EINVAL` (the C aborts).
#[no_mangle]
pub unsafe extern "C" fn erl_drv_tsd_key_create(
    name: *mut c_char,
    key: *mut ErlDrvTSDKey,
) -> c_int {
    if key.is_null() {
        return libc::EINVAL;
    }
    let mut slot: sync::TsdKey = 0;
    // SAFETY: the caller's contract is `sync::tsd_key_create`'s (`name` is null or NUL-terminated,
    // and `&mut slot` is a valid writable out-slot), and its failure sentinel is `TsdKey::MAX`.
    let res = unsafe { sync::tsd_key_create(name, &mut slot) };
    // SAFETY: `key` is a valid writable slot (checked non-null above and promised by the caller).
    unsafe { *key = tsd_key_to_driver(slot) };
    res
}

/// `void erl_drv_tsd_key_destroy(ErlDrvTSDKey key)` (`erl_driver.h:478`) — release a key.
///
/// The key becomes invalid for every thread; a key that was never created (or was already
/// destroyed) is a safe no-op (the C aborts, `erl_drv_thread.c:523`).
#[no_mangle]
pub extern "C" fn erl_drv_tsd_key_destroy(key: ErlDrvTSDKey) {
    sync::tsd_key_destroy(tsd_key_from_driver(key));
}

/// `void erl_drv_tsd_set(ErlDrvTSDKey key, void *data)` (`erl_driver.h:479`) — set the calling
/// thread's value for `key`, dropping the previous one.
///
/// An invalid or destroyed key is a safe no-op (the C aborts, `erl_drv_thread.c:545`).
#[no_mangle]
pub extern "C" fn erl_drv_tsd_set(key: ErlDrvTSDKey, data: *mut c_void) {
    sync::tsd_set(tsd_key_from_driver(key), data);
}

/// `void *erl_drv_tsd_get(ErlDrvTSDKey key)` (`erl_driver.h:480`) — the calling thread's value for
/// `key`, or `NULL` when it is unset (or the key is invalid/destroyed).
#[no_mangle]
pub extern "C" fn erl_drv_tsd_get(key: ErlDrvTSDKey) -> *mut c_void {
    sync::tsd_get(tsd_key_from_driver(key))
}

// ---------------------------------------------------------------------------------------------
// Thread options and the thread lifecycle (`erl_drv_thread.c:583-730`)
// ---------------------------------------------------------------------------------------------

/// `ErlDrvThreadOpts *erl_drv_thread_opts_create(char *name)` (`erl_driver.h:482`) — allocate a
/// thread-options record, `NULL` on failure.
///
/// The C ignores `name` (`erl_drv_thread.c:585-593`); so does this layer. The record starts with
/// `suggested_stack_size = -1` ("use the platform default").
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string (it is never read).
#[no_mangle]
pub unsafe extern "C" fn erl_drv_thread_opts_create(name: *mut c_char) -> *mut ErlDrvThreadOpts {
    // SAFETY: `name` is accepted for signature parity only; `sync::thread_opts_create` never reads
    // it (neither does the C, `erl_drv_thread.c:585-593`).
    sync::thread_opts_create(name)
}

/// `void erl_drv_thread_opts_destroy(ErlDrvThreadOpts *opts)` (`erl_driver.h:481`) — free a record
/// from [`erl_drv_thread_opts_create`]; a null pointer is a no-op (the C aborts).
///
/// # Safety
/// `opts` must be null or a live pointer from [`erl_drv_thread_opts_create`] that is not used
/// afterwards.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_thread_opts_destroy(opts: *mut ErlDrvThreadOpts) {
    // SAFETY: `opts` is null or a live record (the caller contract).
    unsafe { sync::thread_opts_destroy(opts) }
}

/// `int erl_drv_thread_create(char *name, ErlDrvTid *tid, void *(*func)(void *), void *args,
/// ErlDrvThreadOpts *opts)` (`erl_driver.h:483-487`) — start a driver thread running `func(args)`,
/// writing its tid through `tid`. `0` on success, a positive errno on failure
/// (`erl_drv_thread.c:646-652`).
///
/// `name` is the thread's debug name ([`erl_drv_thread_name`]); `opts` may be null (the platform
/// default stack) or a record from [`erl_drv_thread_opts_create`].
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string; `tid` must be a valid writable tid slot;
/// `opts` must be null or a live pointer from [`erl_drv_thread_opts_create`]; `func` must be an
/// entry point that is safe to call on a fresh thread, and must not unwind across the FFI boundary.
/// A null `func` is refused with `EINVAL` (the C would call the null pointer and fault).
#[no_mangle]
pub unsafe extern "C" fn erl_drv_thread_create(
    name: *mut c_char,
    tid: *mut ErlDrvTid,
    func: Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
    args: *mut c_void,
    opts: *mut ErlDrvThreadOpts,
) -> c_int {
    if tid.is_null() {
        return libc::EINVAL;
    }
    // A null entry point is a caller error the C turns into a call through a null function pointer
    // (`erl_drv_thread.c:634,115-121`); refuse it instead of faulting.
    let Some(func) = func else {
        return libc::EINVAL;
    };
    // The layer's trampoline takes a *safe* `extern "C" fn`; the C ABI's slot is an `unsafe` one
    // because a foreign driver may not uphold Rust's safety contract. A Rust function pointer is a
    // bare code address and the `unsafe` qualifier is a *type-level* restriction with no ABI
    // consequence, so the two types are representation-identical (same size, same align, same
    // calling convention); the caller contract above carries over the requirement that the entry
    // point is valid. This is the same conversion `beam_abi::nif` does for its callbacks.
    // SAFETY: both types are `extern "C" fn(*mut c_void) -> *mut c_void` function pointers, which
    // differ only in the safety qualifier and are therefore layout-compatible.
    let entry: extern "C" fn(*mut c_void) -> *mut c_void =
        unsafe { core::mem::transmute::<_, extern "C" fn(*mut c_void) -> *mut c_void>(func) };
    // SAFETY: `name`/`opts` are null or valid per the caller contract; `tid` was checked non-null
    // and is a valid out-slot; `entry` is the caller's entry point.
    unsafe { sync::thread_create(name, tid.cast::<sync::Tid>(), entry, args, opts) }
}

/// `ErlDrvTid erl_drv_thread_self(void)` (`erl_driver.h:488`) — the calling thread's driver tid.
///
/// The handle is the `pthread_t` bit pattern (see the deviations above); it compares equal under
/// [`erl_drv_equal_tids`] to the tid [`erl_drv_thread_create`] produced for the same thread.
#[no_mangle]
pub extern "C" fn erl_drv_thread_self() -> ErlDrvTid {
    sync::thread_self_tid().cast::<ErlDrvTid_>()
}

/// `int erl_drv_equal_tids(ErlDrvTid tid1, ErlDrvTid tid2)` (`erl_driver.h:489`) — `1` when the two
/// handles refer to the same thread, `0` otherwise.
///
/// A null operand yields `0` (the C aborts, `erl_drv_thread.c:692`).
#[no_mangle]
pub extern "C" fn erl_drv_equal_tids(tid1: ErlDrvTid, tid2: ErlDrvTid) -> c_int {
    sync::thread_equal_tids(
        // SAFETY: both operands are null or handles from this family (the caller contract); the
        // conversion is a pure pointer cast and `thread_equal_tids` tolerates nulls.
        unsafe { tid_handle(tid1) },
        // SAFETY: as above.
        unsafe { tid_handle(tid2) },
    ) as c_int
}

/// `void erl_drv_thread_exit(void *resp)` (`erl_driver.h:490`) — terminate the calling thread,
/// returning `resp` to whoever joins it. **Never returns.**
///
/// # Safety
/// Must be called on a thread started by [`erl_drv_thread_create`] (the C aborts otherwise,
/// `erl_drv_thread.c:708-712`); `resp` is an arbitrary value handed to the joiner.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_thread_exit(resp: *mut c_void) {
    // The caller contract above is `sync::thread_exit`'s; the function runs the platform's
    // thread-exit sequence and never returns. `thread_exit` itself is a safe function (it reads no
    // pointer), so no `unsafe` block is needed here.
    sync::thread_exit(resp)
}

/// `int erl_drv_thread_join(ErlDrvTid tid, void **respp)` (`erl_driver.h:491`) — wait for a driver
/// thread to exit, writing its response through `respp` (which may be null) and returning `0`, or a
/// positive errno (`EINVAL` for a null tid).
///
/// # Safety
/// `tid` must be null or the tid of a joinable thread from [`erl_drv_thread_create`] that is not
/// already joined; `respp` must be null or a valid writable `void *` slot.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_thread_join(
    tid: ErlDrvTid,
    respp: *mut *mut c_void,
) -> c_int {
    // SAFETY: the caller contract above is `sync::thread_join`'s own.
    unsafe { sync::thread_join(tid_handle(tid), respp) }
}

/// `char *erl_drv_thread_name(ErlDrvTid tid)` (`erl_driver.h:496`) — the thread's debug name.
///
/// The name [`erl_drv_thread_create`] was handed, the static string `"unknown"` for a thread created
/// with a NULL name (`no_name`, `erl_drv_thread.c:99,638-639`), or `NULL` for a null handle (`:656`) —
/// and for a tid this layer did not create or that has already been joined (the deviation below).
///
/// The pointer is a NUL-terminated copy of the name passed to [`erl_drv_thread_create`], valid until
/// that thread is joined.
///
/// # Safety
/// `tid` must be null or a handle from this family.
#[no_mangle]
pub unsafe extern "C" fn erl_drv_thread_name(tid: ErlDrvTid) -> *mut c_char {
    // SAFETY: `tid` is null or a handle from this family (the caller contract).
    let handle = unsafe { tid_handle(tid) };
    // `sync::thread_name` is safe: it only reads the registry entry keyed by the tid (a raw tid is
    // never dereferenced).
    let name = sync::thread_name(handle);
    if name.is_null() && sync::thread_created_unnamed(handle) {
        // A thread this layer created with a NULL name is the C's `no_name` state (`:638-639`). Any other
        // null — an unknown tid, a joined tid, a null handle — stays `NULL`.
        sync::no_name()
    } else {
        name
    }
}

// ---------------------------------------------------------------------------------------------
// Table assembly
// ---------------------------------------------------------------------------------------------

/// Populate this family's slots of the frozen [`ErlDrvApi`].
///
/// Every slot the family owns is implemented, so the whole group becomes `Some`; the `const fn`
/// chain in [`super`] calls this, so the assignment folds at compile time and the table's *shape*
/// is unchanged.
pub(super) const fn populate(mut api: ErlDrvApi) -> ErlDrvApi {
    // Mutexes.
    api.erl_drv_mutex_destroy = Some(erl_drv_mutex_destroy);
    api.erl_drv_mutex_create = Some(erl_drv_mutex_create);
    api.erl_drv_mutex_trylock = Some(erl_drv_mutex_trylock);
    api.erl_drv_mutex_lock = Some(erl_drv_mutex_lock);
    api.erl_drv_mutex_unlock = Some(erl_drv_mutex_unlock);
    // Condition variables.
    api.erl_drv_cond_destroy = Some(erl_drv_cond_destroy);
    api.erl_drv_cond_create = Some(erl_drv_cond_create);
    api.erl_drv_cond_signal = Some(erl_drv_cond_signal);
    api.erl_drv_cond_broadcast = Some(erl_drv_cond_broadcast);
    api.erl_drv_cond_wait = Some(erl_drv_cond_wait);
    // Read/write locks.
    api.erl_drv_rwlock_destroy = Some(erl_drv_rwlock_destroy);
    api.erl_drv_rwlock_create = Some(erl_drv_rwlock_create);
    api.erl_drv_rwlock_tryrlock = Some(erl_drv_rwlock_tryrlock);
    api.erl_drv_rwlock_rlock = Some(erl_drv_rwlock_rlock);
    api.erl_drv_rwlock_runlock = Some(erl_drv_rwlock_runlock);
    api.erl_drv_rwlock_tryrwlock = Some(erl_drv_rwlock_tryrwlock);
    api.erl_drv_rwlock_rwlock = Some(erl_drv_rwlock_rwlock);
    api.erl_drv_rwlock_rwunlock = Some(erl_drv_rwlock_rwunlock);
    // Thread-specific data.
    api.erl_drv_tsd_key_create = Some(erl_drv_tsd_key_create);
    api.erl_drv_tsd_key_destroy = Some(erl_drv_tsd_key_destroy);
    api.erl_drv_tsd_set = Some(erl_drv_tsd_set);
    api.erl_drv_tsd_get = Some(erl_drv_tsd_get);
    // Thread options and lifecycle.
    api.erl_drv_thread_opts_destroy = Some(erl_drv_thread_opts_destroy);
    api.erl_drv_thread_opts_create = Some(erl_drv_thread_opts_create);
    api.erl_drv_thread_create = Some(erl_drv_thread_create);
    api.erl_drv_thread_self = Some(erl_drv_thread_self);
    api.erl_drv_equal_tids = Some(erl_drv_equal_tids);
    api.erl_drv_thread_exit = Some(erl_drv_thread_exit);
    api.erl_drv_thread_join = Some(erl_drv_thread_join);
    // Debug names.
    api.erl_drv_mutex_name = Some(erl_drv_mutex_name);
    api.erl_drv_cond_name = Some(erl_drv_cond_name);
    api.erl_drv_rwlock_name = Some(erl_drv_rwlock_name);
    api.erl_drv_thread_name = Some(erl_drv_thread_name);
    api
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A value the worker threads hand back so a join can be checked.
    const MAGIC: usize = 0x5eed_1234;

    /// A NUL-terminated C literal for the `*_create` calls.
    macro_rules! cstr {
        ($s:literal) => {
            concat!($s, "\0").as_ptr() as *mut c_char
        };
    }

    /// Read a `*_name` accessor's result as a Rust string (`None` for null).
    ///
    /// # Safety
    /// `name` must be null or a valid NUL-terminated C string.
    unsafe fn name_str(name: *mut c_char) -> Option<String> {
        if name.is_null() {
            return None;
        }
        // SAFETY: the caller guarantees `name` is null or NUL-terminated; the accessors hand out
        // NUL-terminated buffers owned by the object.
        Some(unsafe { core::ffi::CStr::from_ptr(name) }.to_string_lossy().into_owned())
    }

    // -----------------------------------------------------------------------------------------
    // Mutex family
    // -----------------------------------------------------------------------------------------

    #[test]
    fn mutex_round_trip_and_the_c_s_failure_values() {
        let name = cstr!("drv_mutex");
        // SAFETY: `name` is a NUL-terminated literal.
        let m = unsafe { erl_drv_mutex_create(name) };
        assert!(!m.is_null(), "mutex_create returned NULL");

        // SAFETY: `m` is the live handle just created.
        let reported = unsafe { name_str(erl_drv_mutex_name(m)) };
        assert_eq!(reported.as_deref(), Some("drv_mutex"), "the name must round-trip");

        // `0` = acquired (`erl_drv_thread.c:209`); a second trylock while held is EBUSY.
        // SAFETY: `m` is live.
        assert_eq!(unsafe { erl_drv_mutex_trylock(m) }, 0);
        // SAFETY: `m` is live and held by this thread.
        assert_eq!(unsafe { erl_drv_mutex_trylock(m) }, libc::EBUSY);
        // SAFETY: `m` is live and held.
        unsafe { erl_drv_mutex_unlock(m) };

        // The blocking form.
        // SAFETY: `m` is live.
        unsafe { erl_drv_mutex_lock(m) };
        // SAFETY: `m` is live and held.
        assert_eq!(unsafe { erl_drv_mutex_trylock(m) }, libc::EBUSY);
        // SAFETY: `m` is live and held.
        unsafe { erl_drv_mutex_unlock(m) };
        // SAFETY: `m` is live; ownership transfers to `destroy`.
        unsafe { erl_drv_mutex_destroy(m) };

        // A null handle is the documented safe no-op / failure value (the C aborts).
        // SAFETY: null is explicitly allowed by this layer.
        assert_eq!(unsafe { erl_drv_mutex_trylock(ptr::null_mut()) }, libc::EINVAL);
        // SAFETY: as above.
        assert!(unsafe { erl_drv_mutex_name(ptr::null_mut()) }.is_null());
        // SAFETY: as above.
        unsafe { erl_drv_mutex_lock(ptr::null_mut()) };
        // SAFETY: as above.
        unsafe { erl_drv_mutex_unlock(ptr::null_mut()) };
        // SAFETY: as above.
        unsafe { erl_drv_mutex_destroy(ptr::null_mut()) };
    }

    #[test]
    fn an_unnamed_mutex_reports_the_unknown_name() {
        // SAFETY: a null name is the C's "unnamed" case.
        let m = unsafe { erl_drv_mutex_create(ptr::null_mut()) };
        assert!(!m.is_null());
        // SAFETY: `m` is live. The C stores its static `no_name` in the handle
        // (`erl_drv_thread.c:99,135,170`) and answers it here (`:198`).
        assert_eq!(unsafe { name_str(erl_drv_mutex_name(m)) }.as_deref(), Some("unknown"));
        // SAFETY: `m` is live; ownership transfers to `destroy`.
        unsafe { erl_drv_mutex_destroy(m) };
    }

    #[test]
    fn unnamed_cond_rwlock_and_thread_report_the_unknown_name() {
        // SAFETY: a null name is the C's "unnamed" case for each create call.
        let cnd = unsafe { erl_drv_cond_create(ptr::null_mut()) };
        // SAFETY: as above.
        let lck = unsafe { erl_drv_rwlock_create(ptr::null_mut()) };
        assert!(!cnd.is_null() && !lck.is_null());

        // `erl_drv_thread_create` stores `no_name` for a NULL name too (`erl_drv_thread.c:638-639`).
        let mut tid: ErlDrvTid = ptr::null_mut();
        let magic = MAGIC as *mut c_void;
        // SAFETY: a null name/opts means "unnamed, platform default stack"; `&mut tid` is a valid
        // out-slot and `value_worker` is a plain entry point.
        let res =
            unsafe { erl_drv_thread_create(ptr::null_mut(), &mut tid, Some(value_worker), magic, ptr::null_mut()) };
        assert_eq!(res, 0, "thread_create failed with {res}");
        assert!(!tid.is_null());

        // SAFETY: all three handles are live; each accessor reads only the object's name.
        unsafe {
            assert_eq!(name_str(erl_drv_cond_name(cnd)).as_deref(), Some("unknown"));
            assert_eq!(name_str(erl_drv_rwlock_name(lck)).as_deref(), Some("unknown"));
            assert_eq!(name_str(erl_drv_thread_name(tid)).as_deref(), Some("unknown"));
        }

        // SAFETY: `tid` is the tid from the create above, not yet joined.
        let mut resp: *mut c_void = ptr::null_mut();
        assert_eq!(unsafe { erl_drv_thread_join(tid, &mut resp) }, 0);
        assert_eq!(resp as usize, MAGIC);
        // SAFETY: the entry was retired by the join; the lookup just finds nothing.
        assert!(unsafe { erl_drv_thread_name(tid) }.is_null());

        // SAFETY: both handles are live; ownership transfers to their `destroy` calls.
        unsafe {
            erl_drv_cond_destroy(cnd);
            erl_drv_rwlock_destroy(lck);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Condition variables
    // -----------------------------------------------------------------------------------------

    /// Shared state for the cond handshake: the lock, the condition and the phase flag.
    struct CondState {
        mtx: *mut ErlDrvMutex,
        cnd: *mut ErlDrvCond,
        phase: AtomicUsize,
    }

    /// 0 = idle, 1 = the waiter is inside `wait`, 2 = the signaller released it.
    const PHASE_READY: usize = 1;
    const PHASE_RELEASED: usize = 2;

    /// The waiter: publish readiness under the lock, then wait for the release.
    extern "C" fn cond_waiter(ctx: *mut c_void) -> *mut c_void {
        // SAFETY: `ctx` is the `&CondState` the test handed to `thread_create`, which outlives this
        // thread because the test joins it before dropping the state.
        let st = unsafe { &*(ctx as *const CondState) };
        // SAFETY: both handles are live for the whole handshake.
        unsafe {
            erl_drv_mutex_lock(st.mtx);
            st.phase.store(PHASE_READY, Ordering::SeqCst);
            erl_drv_cond_signal(st.cnd);
            while st.phase.load(Ordering::SeqCst) != PHASE_RELEASED {
                erl_drv_cond_wait(st.cnd, st.mtx);
            }
            erl_drv_mutex_unlock(st.mtx);
        }
        MAGIC as *mut c_void
    }

    #[test]
    fn cond_wait_and_signal_hand_off_across_threads() {
        let mname = cstr!("drv_cond_mtx");
        let cname = cstr!("drv_cond");
        // SAFETY: both names are NUL-terminated literals.
        let (mtx, cnd) = unsafe { (erl_drv_mutex_create(mname), erl_drv_cond_create(cname)) };
        assert!(!mtx.is_null() && !cnd.is_null());

        // SAFETY: `cnd` is live; the name round-trips as for a mutex.
        assert_eq!(unsafe { name_str(erl_drv_cond_name(cnd)) }.as_deref(), Some("drv_cond"));

        // Signalling with no waiter is harmless (`erl_drv_thread.c:289-302`).
        // SAFETY: `cnd` is live.
        unsafe { erl_drv_cond_signal(cnd) };
        // SAFETY: `cnd` is live.
        unsafe { erl_drv_cond_broadcast(cnd) };

        let state = CondState { mtx, cnd, phase: AtomicUsize::new(0) };
        let mut tid: ErlDrvTid = ptr::null_mut();
        let tname = cstr!("drv_cond_worker");
        // SAFETY: all handles are live, `&mut tid` is a valid out-slot and `cond_waiter` is the
        // documented entry point shape.
        let res = unsafe {
            erl_drv_thread_create(
                tname,
                &mut tid,
                Some(cond_waiter),
                &state as *const CondState as *mut c_void,
                ptr::null_mut(),
            )
        };
        assert_eq!(res, 0, "thread_create failed with {res}");

        // The signaller half of the same handshake: wait for readiness, release, wake the waiter.
        // SAFETY: both handles are live; the predicate is checked under the lock, so no wakeup can
        // be lost in either order.
        unsafe {
            erl_drv_mutex_lock(mtx);
            while state.phase.load(Ordering::SeqCst) != PHASE_READY {
                erl_drv_cond_wait(cnd, mtx);
            }
            state.phase.store(PHASE_RELEASED, Ordering::SeqCst);
            erl_drv_cond_broadcast(cnd);
            erl_drv_mutex_unlock(mtx);
        }

        let mut resp: *mut c_void = ptr::null_mut();
        // SAFETY: `tid` comes from the successful `thread_create` above and is not joined yet.
        assert_eq!(unsafe { erl_drv_thread_join(tid, &mut resp) }, 0);
        assert_eq!(resp as usize, MAGIC, "the waiter's response must reach the joiner");

        // SAFETY: both handles are live and no thread holds them; ownership transfers to destroy.
        unsafe {
            erl_drv_cond_destroy(cnd);
            erl_drv_mutex_destroy(mtx);
        }

        // Null handles are no-ops for the void entry points.
        // SAFETY: null is explicitly allowed by this layer.
        unsafe { erl_drv_cond_signal(ptr::null_mut()) };
        // SAFETY: as above.
        unsafe { erl_drv_cond_broadcast(ptr::null_mut()) };
        // SAFETY: as above.
        unsafe { erl_drv_cond_wait(ptr::null_mut(), ptr::null_mut()) };
        // SAFETY: as above.
        unsafe { erl_drv_cond_destroy(ptr::null_mut()) };
        // SAFETY: as above.
        assert!(unsafe { erl_drv_cond_name(ptr::null_mut()) }.is_null());
    }

    // -----------------------------------------------------------------------------------------
    // Read/write locks
    // -----------------------------------------------------------------------------------------

    #[test]
    fn rwlock_read_and_write_round_trip() {
        let name = cstr!("drv_rwlock");
        // SAFETY: `name` is a NUL-terminated literal.
        let lck = unsafe { erl_drv_rwlock_create(name) };
        assert!(!lck.is_null());
        // SAFETY: `lck` is live.
        assert_eq!(unsafe { name_str(erl_drv_rwlock_name(lck)) }.as_deref(), Some("drv_rwlock"));

        // SAFETY: every call below operates on the live, unlocked handle.
        unsafe {
            assert_eq!(erl_drv_rwlock_tryrlock(lck), 0, "an idle lock is try-readable");
            erl_drv_rwlock_runlock(lck);

            erl_drv_rwlock_rlock(lck);
            erl_drv_rwlock_runlock(lck);

            erl_drv_rwlock_rwlock(lck);
            // A held write lock refuses a read lock (EBUSY, `erl_drv_thread.c:371-381`).
            assert_eq!(erl_drv_rwlock_tryrlock(lck), libc::EBUSY);
            erl_drv_rwlock_rwunlock(lck);

            assert_eq!(erl_drv_rwlock_tryrwlock(lck), 0, "an idle lock is try-writable");
            erl_drv_rwlock_rwunlock(lck);

            erl_drv_rwlock_destroy(lck);
        }

        // Null handles: the C's failure values for the `int` returns, no-ops for the rest.
        // SAFETY: null is explicitly allowed by this layer.
        assert_eq!(unsafe { erl_drv_rwlock_tryrlock(ptr::null_mut()) }, libc::EINVAL);
        // SAFETY: as above.
        assert_eq!(unsafe { erl_drv_rwlock_tryrwlock(ptr::null_mut()) }, libc::EINVAL);
        // SAFETY: as above.
        assert!(unsafe { erl_drv_rwlock_name(ptr::null_mut()) }.is_null());
        // SAFETY: as above.
        unsafe {
            erl_drv_rwlock_rlock(ptr::null_mut());
            erl_drv_rwlock_runlock(ptr::null_mut());
            erl_drv_rwlock_rwlock(ptr::null_mut());
            erl_drv_rwlock_rwunlock(ptr::null_mut());
            erl_drv_rwlock_destroy(ptr::null_mut());
        }
    }

    // -----------------------------------------------------------------------------------------
    // Thread-specific data
    // -----------------------------------------------------------------------------------------

    /// The value the TSD worker stores in its own thread-local slot.
    const WORKER_TSD: usize = 0x77aa_bbcc;

    /// Reads the shared key from a *different* thread, where it must be unset.
    extern "C" fn tsd_worker(ctx: *mut c_void) -> *mut c_void {
        let key = ctx as usize as ErlDrvTSDKey;
        if erl_drv_tsd_get(key).is_null() {
            erl_drv_tsd_set(key, WORKER_TSD as *mut c_void);
        }
        erl_drv_tsd_get(key)
    }

    #[test]
    fn tsd_is_per_thread_and_the_key_round_trips() {
        let mut key: ErlDrvTSDKey = -1;
        let name = cstr!("drv_tsd");
        // SAFETY: `name` is a NUL-terminated literal and `&mut key` is a valid out-slot.
        assert_eq!(unsafe { erl_drv_tsd_key_create(name, &mut key) }, 0);
        assert!(key >= 0, "a created key is non-negative, got {key}");

        // Unset for this thread, set after `tsd_set` — the C's observable contract.
        assert!(erl_drv_tsd_get(key).is_null());
        let mine = MAGIC as *mut c_void;
        erl_drv_tsd_set(key, mine);
        assert_eq!(erl_drv_tsd_get(key), mine);

        // A second thread sees no value for the same key and can set its own.
        let mut tid: ErlDrvTid = ptr::null_mut();
        let tname = cstr!("drv_tsd_worker");
        // SAFETY: all handles are live, `&mut tid` is a valid out-slot and `tsd_worker` is the
        // documented entry point shape; the key value travels through the `void *` argument.
        let res = unsafe {
            erl_drv_thread_create(
                tname,
                &mut tid,
                Some(tsd_worker),
                (key as usize) as *mut c_void,
                ptr::null_mut(),
            )
        };
        assert_eq!(res, 0, "thread_create failed with {res}");
        let mut resp: *mut c_void = ptr::null_mut();
        // SAFETY: `tid` is the tid of the thread just started, not yet joined.
        assert_eq!(unsafe { erl_drv_thread_join(tid, &mut resp) }, 0);
        assert_eq!(resp as usize, WORKER_TSD, "the worker's own TSD value");
        assert_eq!(erl_drv_tsd_get(key), mine, "the caller's slot is untouched");

        erl_drv_tsd_key_destroy(key);
        // A destroyed key is a safe no-op (the C aborts, `erl_drv_thread.c:523,545`).
        erl_drv_tsd_set(key, ptr::null_mut());
        assert!(erl_drv_tsd_get(key).is_null());

        // A null out-slot is refused with the C's failure code and no key is written.
        // SAFETY: null is explicitly refused by this layer (the C aborts).
        assert_eq!(
            unsafe { erl_drv_tsd_key_create(name, ptr::null_mut()) },
            libc::EINVAL
        );
    }

    // -----------------------------------------------------------------------------------------
    // Thread lifecycle
    // -----------------------------------------------------------------------------------------

    /// A worker that simply returns its argument, so a join can check the response.
    extern "C" fn value_worker(ctx: *mut c_void) -> *mut c_void {
        ctx
    }

    /// A worker that terminates through `erl_drv_thread_exit` instead of returning.
    extern "C" fn exiting_worker(ctx: *mut c_void) -> *mut c_void {
        // SAFETY: this thread was started by `thread_create` (the test's own contract), and `ctx`
        // is the response value the joiner expects.
        unsafe { erl_drv_thread_exit(ctx) };
        // Unreachable: `erl_drv_thread_exit` runs the platform's thread-exit sequence and never
        // returns; this tail expression only satisfies the entry-point signature (the joiner sees
        // the value passed to `erl_drv_thread_exit`).
        ctx
    }

    /// A worker that reports its own tid, obtained from `erl_drv_thread_self`.
    extern "C" fn self_worker(ctx: *mut c_void) -> *mut c_void {
        let out = ctx as *mut ErlDrvTid;
        // SAFETY: `out` is the `&mut ErlDrvTid` the test handed over, and this thread runs inside
        // the test's join, so the borrow is alive.
        unsafe { *out = erl_drv_thread_self() };
        ptr::null_mut()
    }

    #[test]
    fn thread_create_join_self_and_equal() {
        // `erl_drv_thread_self` is stable for the calling thread and compares equal to itself.
        let t1 = erl_drv_thread_self();
        let t2 = erl_drv_thread_self();
        assert!(!t1.is_null() && !t2.is_null());
        assert_eq!(erl_drv_equal_tids(t1, t2), 1, "self must equal self");
        assert_eq!(
            erl_drv_equal_tids(ptr::null_mut(), t1),
            0,
            "a null tid is never equal to a real one (the C aborts)"
        );

        // A created thread reports the *same* tid through `thread_self` as the creator got.
        let mut reported: ErlDrvTid = ptr::null_mut();
        let mut tid: ErlDrvTid = ptr::null_mut();
        let tname = cstr!("drv_self_worker");
        // SAFETY: all handles are live, `&mut tid` is a valid out-slot and `self_worker` is the
        // documented entry point shape.
        let res = unsafe {
            erl_drv_thread_create(
                tname,
                &mut tid,
                Some(self_worker),
                &mut reported as *mut ErlDrvTid as *mut c_void,
                ptr::null_mut(),
            )
        };
        assert_eq!(res, 0, "thread_create failed with {res}");
        assert!(!tid.is_null(), "thread_create must write the tid");
        assert_eq!(erl_drv_equal_tids(t1, tid), 0, "a worker is a different thread");

        // The debug name is the one requested, and it is readable before the join.
        // SAFETY: `tid` is live until it is joined below.
        assert_eq!(
            unsafe { name_str(erl_drv_thread_name(tid)) }.as_deref(),
            Some("drv_self_worker")
        );

        let mut resp: *mut c_void = ptr::null_mut();
        // SAFETY: `tid` is the tid from the successful create above, not yet joined.
        assert_eq!(unsafe { erl_drv_thread_join(tid, &mut resp) }, 0);
        assert!(resp.is_null(), "the worker returns null");
        assert_eq!(
            erl_drv_equal_tids(reported, tid),
            1,
            "the tid the thread reported for itself is the created tid"
        );

        // The registry entry is retired by the join, so the name is gone (the C frees its record).
        // SAFETY: `tid` is still a handle value; the lookup just finds nothing.
        assert!(unsafe { erl_drv_thread_name(tid) }.is_null());
        // SAFETY: null is explicitly allowed by this layer.
        assert!(unsafe { erl_drv_thread_name(ptr::null_mut()) }.is_null());

        // A thread returning a value through `join`.
        let magic = MAGIC as *mut c_void;
        let mut tid2: ErlDrvTid = ptr::null_mut();
        // SAFETY: as above, with `value_worker` echoing its argument.
        let res = unsafe {
            erl_drv_thread_create(
                ptr::null_mut(),
                &mut tid2,
                Some(value_worker),
                magic,
                ptr::null_mut(),
            )
        };
        assert_eq!(res, 0, "thread_create failed with {res}");
        let mut resp2: *mut c_void = ptr::null_mut();
        // SAFETY: `tid2` is the tid from the create above, not yet joined.
        assert_eq!(unsafe { erl_drv_thread_join(tid2, &mut resp2) }, 0);
        assert_eq!(resp2 as usize, MAGIC, "join delivers the thread's return value");
        // A null `respp` is allowed (the C's `erl_drv_thread_join(tid, NULL)` case).
        // SAFETY: `tid2` has already been joined here only in the sense that the entry is retired;
        // to keep the contract clean a fresh thread is used instead.
        let mut tid3: ErlDrvTid = ptr::null_mut();
        // SAFETY: as above.
        let res = unsafe {
            erl_drv_thread_create(
                ptr::null_mut(),
                &mut tid3,
                Some(value_worker),
                magic,
                ptr::null_mut(),
            )
        };
        assert_eq!(res, 0, "thread_create failed with {res}");
        // SAFETY: `tid3` is a live joinable tid and a null `respp` is explicitly allowed.
        assert_eq!(unsafe { erl_drv_thread_join(tid3, ptr::null_mut()) }, 0);

        // A null tid is refused with `EINVAL` (the C asserts).
        // SAFETY: null is explicitly allowed by this layer.
        assert_eq!(
            unsafe { erl_drv_thread_join(ptr::null_mut(), ptr::null_mut()) },
            libc::EINVAL
        );
    }

    #[test]
    fn thread_exit_delivers_its_response_to_the_joiner() {
        let magic = MAGIC as *mut c_void;
        let mut tid: ErlDrvTid = ptr::null_mut();
        let tname = cstr!("drv_exiting_worker");
        // SAFETY: all handles are live, `&mut tid` is a valid out-slot and `exiting_worker` is the
        // documented entry point shape.
        let res = unsafe {
            erl_drv_thread_create(tname, &mut tid, Some(exiting_worker), magic, ptr::null_mut())
        };
        assert_eq!(res, 0, "thread_create failed with {res}");
        let mut resp: *mut c_void = ptr::null_mut();
        // SAFETY: `tid` is the tid from the create above, not yet joined.
        assert_eq!(
            unsafe { erl_drv_thread_join(tid, &mut resp) },
            0,
            "a thread that exits through `erl_drv_thread_exit` is still joinable"
        );
        assert_eq!(resp as usize, MAGIC, "`thread_exit`'s response reaches the joiner");
    }

    #[test]
    fn a_null_entry_point_or_out_slot_is_refused_not_called() {
        let mut tid: ErlDrvTid = ptr::null_mut();
        // SAFETY: the null `func` is explicitly refused by this layer (the C would call through it
        // and fault), and a null `tid` likewise.
        unsafe {
            assert_eq!(
                erl_drv_thread_create(
                    ptr::null_mut(),
                    &mut tid,
                    None,
                    ptr::null_mut(),
                    ptr::null_mut()
                ),
                libc::EINVAL
            );
            assert!(tid.is_null(), "a refused create must not write a tid");
            assert_eq!(
                erl_drv_thread_create(
                    ptr::null_mut(),
                    ptr::null_mut(),
                    Some(value_worker),
                    ptr::null_mut(),
                    ptr::null_mut()
                ),
                libc::EINVAL
            );
        }
    }

    #[test]
    fn thread_opts_round_trip_and_the_stack_size_is_kilowords() {
        // SAFETY: a null name is the C's "the name is ignored" case.
        let opts = unsafe { erl_drv_thread_opts_create(ptr::null_mut()) };
        assert!(!opts.is_null());
        // SAFETY: `opts` is live and `suggested_stack_size` is the record's public field.
        assert_eq!(unsafe { (*opts).suggested_stack_size }, -1, "the C's default");

        // A small-but-legal request exercised end-to-end: the thread must still start and join.
        // SAFETY: `opts` is live; 64 kilowords = 512 KiB on LP64.
        unsafe { (*opts).suggested_stack_size = 64 };
        let mut tid: ErlDrvTid = ptr::null_mut();
        let magic = MAGIC as *mut c_void;
        // SAFETY: `opts` is live, `&mut tid` is a valid out-slot and `value_worker` is a plain
        // entry point.
        let res = unsafe {
            erl_drv_thread_create(ptr::null_mut(), &mut tid, Some(value_worker), magic, opts)
        };
        assert_eq!(res, 0, "thread_create with options failed with {res}");
        let mut resp: *mut c_void = ptr::null_mut();
        // SAFETY: `tid` is the tid from the create above, not yet joined.
        assert_eq!(unsafe { erl_drv_thread_join(tid, &mut resp) }, 0);
        assert_eq!(resp as usize, MAGIC);
        // SAFETY: `opts` is no longer needed and is not used again.
        unsafe { erl_drv_thread_opts_destroy(opts) };

        // A null record is a safe no-op (the C aborts, `erl_drv_thread.c:598`).
        // SAFETY: null is explicitly allowed by this layer.
        unsafe { erl_drv_thread_opts_destroy(ptr::null_mut()) };
    }

    // -----------------------------------------------------------------------------------------
    // Table assembly
    // -----------------------------------------------------------------------------------------

    #[test]
    fn the_frozen_table_populates_every_slot_this_family_implements() {
        let table = crate::driver::drv::driver_api_table();
        let populated = [
            // Mutexes.
            table.erl_drv_mutex_destroy.is_some(),
            table.erl_drv_mutex_create.is_some(),
            table.erl_drv_mutex_trylock.is_some(),
            table.erl_drv_mutex_lock.is_some(),
            table.erl_drv_mutex_unlock.is_some(),
            // Condition variables.
            table.erl_drv_cond_destroy.is_some(),
            table.erl_drv_cond_create.is_some(),
            table.erl_drv_cond_signal.is_some(),
            table.erl_drv_cond_broadcast.is_some(),
            table.erl_drv_cond_wait.is_some(),
            // Read/write locks.
            table.erl_drv_rwlock_destroy.is_some(),
            table.erl_drv_rwlock_create.is_some(),
            table.erl_drv_rwlock_tryrlock.is_some(),
            table.erl_drv_rwlock_rlock.is_some(),
            table.erl_drv_rwlock_runlock.is_some(),
            table.erl_drv_rwlock_tryrwlock.is_some(),
            table.erl_drv_rwlock_rwlock.is_some(),
            table.erl_drv_rwlock_rwunlock.is_some(),
            // Thread-specific data.
            table.erl_drv_tsd_key_create.is_some(),
            table.erl_drv_tsd_key_destroy.is_some(),
            table.erl_drv_tsd_set.is_some(),
            table.erl_drv_tsd_get.is_some(),
            // Thread options and lifecycle.
            table.erl_drv_thread_opts_destroy.is_some(),
            table.erl_drv_thread_opts_create.is_some(),
            table.erl_drv_thread_create.is_some(),
            table.erl_drv_thread_self.is_some(),
            table.erl_drv_equal_tids.is_some(),
            table.erl_drv_thread_exit.is_some(),
            table.erl_drv_thread_join.is_some(),
            // Debug names.
            table.erl_drv_mutex_name.is_some(),
            table.erl_drv_cond_name.is_some(),
            table.erl_drv_rwlock_name.is_some(),
            table.erl_drv_thread_name.is_some(),
        ];
        assert!(populated.iter().all(|&p| p), "every implemented slot must be populated: {populated:?}");
    }
}
