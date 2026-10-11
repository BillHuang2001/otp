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

//! Shared threading / synchronisation primitive layer for **both** C thread facades.
//!
//! The NIF thread family (`enif_mutex_*`, `enif_cond_*`, `enif_rwlock_*`, `enif_tsd_*`,
//! `enif_thread_*`, `enif_equal_tids`, `enif_thread_opts_*`, `enif_mutex_name`/`enif_cond_name`/
//! `enif_rwlock_name`/`enif_thread_name`, `enif_thread_type`) and the driver thread family
//! (`erl_drv_mutex_*`, `erl_drv_cond_*`, `erl_drv_rwlock_*`, `erl_drv_tsd_*`, `erl_drv_thread_*`,
//! `erl_drv_equal_tids`) are the **same primitives** behind different opaque handle types. The C
//! makes every `enif_*` thread function a one-line forward to its `erl_drv_*` sibling
//! (`erts/emulator/beam/erl_nif.c:2405-2441`), which in turn is implemented once in
//! `erts/emulator/beam/erl_drv_thread.c`. This module is the Rust equivalent of that shared
//! implementation: both C facades are thin cast-and-forward wrappers over the free functions here.
//!
//! Ground truth (read-only): `erts/emulator/beam/erl_drv_thread.c`,
//! `erts/emulator/beam/erl_nif_api_funcs.h:65-93,178,205-208` and
//! `erts/emulator/beam/erl_driver.h:459-496`.
//!
//! # Implementation
//! The primitives are **raw libc pthreads** (`libc::pthread_mutex_t`, `pthread_cond_t`,
//! `pthread_rwlock_t`, `pthread_key_t`, `pthread_create`/`join`/`exit`/`self`/`equal`). `std::sync`
//! cannot express the C contract: a NIF/driver mutex is locked by one thread and unlocked by
//! another (the C asks for a `posix_compliant` mutex, `erl_drv_thread.c:160-161`), and
//! `cond_wait(cond, mutex)` must *atomically* release and re-acquire the caller's mutex.
//! `std::sync` is used for this module's own private bookkeeping only (the created-thread
//! registry), never for a primitive the C sees.
//!
//! Handle representations match the headers: a thread handle is the C `void *`
//! (`ErlNifTid`/`ErlDrvTid`) holding the **value** of the platform `pthread_t` bit-cast to a
//! pointer, so [`thread_self_tid`] and a [`thread_create`]d tid compare equal under
//! `pthread_equal`; a TSD key is the C `unsigned int` (`ErlNifTSDKey`/`ErlDrvTSDKey`) holding the
//! `pthread_key_t` value (also `unsigned int` on this platform).
//!
//! # Deviations from the oracle (all deliberate; none returns a wrong value)
//! * **`thread_type` never reports a scheduler.** This runtime has no scheduler threads, so
//!   [`thread_type`] returns [`ERL_NIF_THR_UNDEFINED`] (`0`) on any thread this layer did not
//!   create, and [`ERL_NIF_THR_OTHER`] (`-1`) on a thread created through [`thread_create`]
//!   (tracked in this module's in-process registry). It never returns
//!   [`ERL_NIF_THR_NORMAL_SCHEDULER`]/[`ERL_NIF_THR_DIRTY_CPU_SCHEDULER`]/
//!   [`ERL_NIF_THR_DIRTY_IO_SCHEDULER`]. **The C returns `ERL_NIF_THR_UNDEFINED` for a NIF-created
//!   thread** (`erl_nif.c:3606-3625` knows only scheduler threads); `ERL_NIF_THR_OTHER` is an
//!   extension of this layer, chosen negative per the header's rule that negative values are
//!   "reserved for specific types of non-scheduler threads" (`erl_nif.h:328-337`). A facade that
//!   wants byte-exact oracle parity must map `OTHER -> UNDEFINED`.
//! * **A created thread's debug name lives in an in-process registry**, not in the OS
//!   (`pthread_setname_np` is not called): the registry maps a tid bit pattern to the
//!   NUL-terminated copy of the name, and that buffer is what [`thread_name`] hands out. The C
//!   appends the name to its `struct ErlDrvTid_` record instead.
//! * **`try_lock`'s busy code is pthread's `EBUSY`** (as the C's `ethread`). A *null* handle yields
//!   `EINVAL` where the C calls `erts_exit` (`erl_drv_thread.c:208,400-412`).
//! * **An unnamed object has no *stored* name here**, so this layer's `*_name` accessors report it
//!   as null. The C instead stores the static string `no_name` = `"unknown"` in every handle it
//!   creates with a NULL name (`erl_drv_thread.c:99,135`; assigned at `:170`, `:264`, `:341`, `:639`,
//!   `:678`) and answers `handle ? handle->name : NULL` (`:198`, `:283`, `:365`, `:656`), so a
//!   **live** handle never answers null. [`no_name`] is that string; the **driver** facade
//!   (`crate::driver::drv::threads`) renders the unnamed state with it, so contract A.4 is
//!   byte-exact. The **NIF** facade over this layer (`crate::nif::enif`) still reports null — a
//!   documented deviation of that facade, even though the C's `enif_*_name` is a one-line forward to
//!   the same implementation (`erl_nif.c:2405-2441`).
//! * **Handle misuse never aborts.** A null handle is a no-op (destroy/lock/signal) or the
//!   documented failure code (`EINVAL`); the C calls `erts_exit(ERTS_ABORT_EXIT, ...)`.
//! * **`cond_wait` makes a single `pthread_cond_wait` call.** The C retries until it returns `0`
//!   (`erl_drv_thread.c:314-323`); that loop cannot terminate on a persistent error, so the retry
//!   is dropped rather than reproduced.
//! * **TSD keys are real pthread keys** (`pthread_key_create`/`key_delete`/`setspecific`/
//!   `getspecific`), not the C's own key counter table. The observable contract is identical (a
//!   per-thread `void *` slot per key; the returned/observable code is the platform's). The `name`
//!   argument of [`tsd_key_create`] is accepted and dropped: the C stores it in `used_tsd_keys[]`
//!   purely for its own key validation and never exposes it. An invalid or destroyed key makes
//!   `set`/`get` a safe no-op (`EINVAL`/null) where the C aborts.
//! * **`thread_opts_create` ignores `name`**, exactly as the C does (`erl_drv_thread.c:584-593`
//!   never reads it), and returns a boxed [`ErlDrvThreadOpts`] with `suggested_stack_size = -1`.
//!   `suggested_stack_size` is interpreted as **kilowords**, as the C documentation says and as the
//!   C computes it (`ETHR_KW2B` = `KW * sizeof(void *) * 1024`, `ethr_internal.h`;
//!   `pthread/ethread.c:365`), and the converted size is clamped into
//!   `[PTHREAD_STACK_MIN, 64 MiB]` (the C's `ethr_min_stack_size__`/`ethr_max_stack_size__` window,
//!   `ethr_aux.c:215-251`) before it is applied with `pthread_attr_setstacksize`. A rejected size is
//!   ignored, exactly as the C ignores it (`(void) pthread_attr_setstacksize(...)`,
//!   `pthread/ethread.c:377`) — the thread then gets the platform default stack.
//! * **`thread_exit` unconditionally calls `pthread_exit`** (the C aborts when it is called from a
//!   thread the interface did not create, `erl_drv_thread.c:704-713`), and **`thread_equal_tids`
//!   returns `false` for a null operand** (the C aborts, `erl_drv_thread.c:686-702`).
//! * **`thread_join` retires the registry entry** on success (the C frees its `ErlDrvTid_` record,
//!   `erl_drv_thread.c:715-730`): afterwards [`thread_name`] returns null rather than a dangling
//!   pointer. A created thread that is never joined keeps its (small) entry, exactly as the C keeps
//!   its `ErlDrvTid_`.
//!
//! # Thread-safety
//! Every function is safe to call from any thread. A `*_name` accessor returns a pointer into
//! storage owned by the object (locks) or by the registry entry (thread names): it stays valid
//! until the object is destroyed or the thread is joined.

use core::mem::MaybeUninit;
use core::ptr;
use libc::{c_char, c_int, c_uint, c_void};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex as StdMutex, MutexGuard};

use crate::common::ErlDrvThreadOpts;

/// The frozen NIF thread-type constants (`erl_nif.h:334-337`), re-exported from
/// [`crate::nif::types`] so a facade resolves them directly from this layer.
pub use crate::nif::types::{
    ERL_NIF_THR_DIRTY_CPU_SCHEDULER, ERL_NIF_THR_DIRTY_IO_SCHEDULER, ERL_NIF_THR_NORMAL_SCHEDULER,
    ERL_NIF_THR_UNDEFINED,
};

/// A thread created through this layer — this layer's extension for
/// `enif_thread_type`/`erl_drv_thread_type`.
///
/// **Not defined by the OTP headers of this release** (which only define `ERL_NIF_THR_UNDEFINED`
/// 0, `ERL_NIF_THR_NORMAL_SCHEDULER` 1, `ERL_NIF_THR_DIRTY_CPU_SCHEDULER` 2,
/// `ERL_NIF_THR_DIRTY_IO_SCHEDULER` 3). The value is negative because the header reserves
/// negative values for "specific types of non-scheduler threads" (`erl_nif.h:328-337`), and this
/// runtime has no scheduler threads to report. See the module docs: a facade that needs byte-exact
/// oracle parity must map this to [`ERL_NIF_THR_UNDEFINED`].
pub const ERL_NIF_THR_OTHER: c_int = -1;

/// A thread handle — the C `ErlNifTid`/`ErlDrvTid` (both `void *`).
///
/// Holds the **bit pattern of the platform `pthread_t`**, so two tids referring to the same thread
/// are equal under `pthread_equal` (`erl_drv_thread.c:686-702`).
pub type Tid = *mut c_void;

/// A thread-specific-data key — the C `ErlNifTSDKey`/`ErlDrvTSDKey` (both `unsigned int`),
/// holding a `pthread_key_t` value.
pub type TsdKey = c_uint;

// ---------------------------------------------------------------------------------------------
// Internal helpers: names, tid conversion, the created-thread registry
// ---------------------------------------------------------------------------------------------

/// Copy a C debug name into a stable, NUL-terminated, heap-owned buffer.
///
/// `None` means "unnamed": the C `name == NULL` case (`erl_drv_thread.c:166-171`). The returned
/// `Vec` keeps the terminating NUL, so its allocation *is* the `char *` a `*_name` accessor hands
/// out (`copy_name` copies, so later writes to the caller's buffer are not visible — as in the C).
fn copy_name(name: *const c_char) -> Option<Vec<u8>> {
    if name.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `name` is a valid NUL-terminated C string for the duration of
    // the call; `CStr::from_ptr` scans to the first NUL and `to_bytes_with_nul` copies it,
    // including the NUL, into a fresh allocation.
    let bytes = unsafe { core::ffi::CStr::from_ptr(name) }.to_bytes_with_nul();
    Some(bytes.to_vec())
}

/// The `char *` a `*_name` accessor returns for a stored name: a pointer into the object's own
/// NUL-terminated buffer (stable until the object is destroyed), or null when unnamed.
///
/// A frozen ABI whose C stores `no_name` for a handle created with a NULL name renders that null with
/// [`no_name`] instead (see the module docs).
fn name_ptr(name: &Option<Vec<u8>>) -> *mut c_char {
    match name {
        Some(bytes) => bytes.as_ptr() as *mut c_char,
        None => ptr::null_mut(),
    }
}

/// The C's `no_name` string (`erl_drv_thread.c:99`, assigned `"unknown"` at `:135`).
///
/// The C stores a pointer to it in every handle it creates with a NULL name
/// (`erl_drv_thread.c:170`, `:264`, `:341`, `:639`, `:678`, `:772`), so its `*_name` accessors — all
/// shaped `handle ? handle->name : NULL` (`:198`, `:283`, `:365`, `:656`) — answer it for every
/// **live** handle and reserve null for a **null handle**. NUL-terminated and `'static`, so a pointer
/// to it is never dangling.
static NO_NAME: [u8; 8] = *b"unknown\0";

/// The C's `no_name` as the `char *` a frozen ABI's `*_name` accessor reports for a **live** handle
/// created with a NULL name: the string `"unknown"` (never null, never dangling).
///
/// This layer keeps the unnamed state as a null name ([`name_ptr`]); a facade that must reproduce the
/// C's `*_name` contract renders it with this function (the driver facade does — see the module docs).
pub fn no_name() -> *mut c_char {
    NO_NAME.as_ptr() as *mut c_char
}

/// Bit-cast a platform `pthread_t` into the C `void *` tid handle.
///
/// One `as` cast covers both platform spellings of `pthread_t` (`c_ulong` on glibc, a pointer on
/// musl); the supported targets are LP64 (crate `CONTEXT.md`, "Target platform").
fn tid_from_pthread(tid: libc::pthread_t) -> Tid {
    tid as Tid
}

/// Recover the platform `pthread_t` from a C tid handle (the inverse of [`tid_from_pthread`]).
fn pthread_from_tid(tid: Tid) -> libc::pthread_t {
    tid as libc::pthread_t
}

/// The registry key for a tid handle: its bit pattern.
fn tid_key(tid: Tid) -> usize {
    tid as usize
}

/// The debug names of the threads created through this layer, keyed by tid bit pattern.
///
/// In-process only (see the module docs); an entry is created by [`thread_create`] (and by the new
/// thread itself, so its own `thread_type`/`thread_name` are correct immediately) and retired by
/// [`thread_join`].
static CREATED_THREADS: LazyLock<StdMutex<HashMap<usize, Option<Vec<u8>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// The created-thread registry.
///
/// Lock poisoning (a panicking thread) is *recovered* rather than propagated: these lookups run
/// inside NIF/driver code, which must not inherit a panic from unrelated bookkeeping.
fn created_threads() -> MutexGuard<'static, HashMap<usize, Option<Vec<u8>>>> {
    CREATED_THREADS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Register a created thread's debug name under `key`, **without** replacing an existing entry.
///
/// The name is registered twice for the same tid: by [`thread_create`] on the new thread's behalf
/// (before it publishes the tid, so a caller's `thread_name(tid)` is correct as soon as the call
/// returns) and by [`thread_start`] from inside the new thread. Those two registrations race — the
/// new thread can reach its entry point before the creator's `pthread_create` returns — so this
/// keeps **whichever lands first** and makes the loser a no-op. Plainly inserting in both places
/// would instead *replace* (and free) the winning `Vec<u8>`, dangling a pointer that a concurrent
/// [`thread_name`] caller — or the new thread's own entry point — already holds; the buffer is
/// documented to stay valid until the thread is joined.
fn register_thread_name(key: usize, name: Option<Vec<u8>>) {
    created_threads().entry(key).or_insert(name);
}

// ---------------------------------------------------------------------------------------------
// Mutex (`enif_mutex_*` / `erl_drv_mutex_*`)
// ---------------------------------------------------------------------------------------------

/// An opaque, pthread-backed mutex handle — the C `ErlNifMutex`/`ErlDrvMutex`.
///
/// Only ever reached through the `*mut Mutex` handed out by [`mutex_create`]; the inner
/// `pthread_mutex_t` is mutated by libc, so it is only ever touched through `addr_of_mut!` (never
/// through a Rust reference).
pub struct Mutex {
    inner: libc::pthread_mutex_t,
    name: Option<Vec<u8>>,
}

/// Create a mutex, `NULL` on failure (the C `erl_drv_mutex_create` contract).
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string.
pub unsafe fn mutex_create(name: *const c_char) -> *mut Mutex {
    let mut inner = MaybeUninit::<libc::pthread_mutex_t>::uninit();
    // SAFETY: `inner` is a valid, uninitialised `pthread_mutex_t` slot and a null attribute asks
    // for the platform default type — a non-error-checking mutex, like the C's `posix_compliant`
    // driver mutex (`erl_drv_thread.c:160-161`), so it may be unlocked by a *different* thread.
    if unsafe { libc::pthread_mutex_init(inner.as_mut_ptr(), ptr::null()) } != 0 {
        return ptr::null_mut();
    }
    // SAFETY: `pthread_mutex_init` returned 0, so the mutex is fully initialised.
    let inner = unsafe { inner.assume_init() };
    Box::into_raw(Box::new(Mutex {
        inner,
        name: copy_name(name),
    }))
}

/// Destroy a mutex handle; a null handle is a no-op.
///
/// # Safety
/// `mtx` must be null or a live handle from [`mutex_create`] that is not used afterwards.
pub unsafe fn mutex_destroy(mtx: *mut Mutex) {
    if mtx.is_null() {
        return;
    }
    // SAFETY: `mtx` is a live handle (the caller contract), so the inner mutex is initialised and
    // not yet destroyed. A failure (e.g. destroying a locked mutex) is a caller error the C turns
    // into `erts_exit` (`erl_drv_thread.c:184-194`); it is ignored here rather than aborting the
    // runtime.
    unsafe { libc::pthread_mutex_destroy(ptr::addr_of_mut!((*mtx).inner)) };
    // SAFETY: the caller transferred ownership of the `mutex_create` allocation and must not use
    // the handle again; dropping the box frees the handle and its name buffer.
    drop(unsafe { Box::from_raw(mtx) });
}

/// Lock a mutex, blocking until it can be acquired; a null handle is a no-op.
///
/// # Safety
/// `mtx` must be null or a live handle from [`mutex_create`].
pub unsafe fn mutex_lock(mtx: *mut Mutex) {
    if mtx.is_null() {
        return;
    }
    // SAFETY: `mtx` is a live handle, so its inner mutex is initialised.
    unsafe { libc::pthread_mutex_lock(ptr::addr_of_mut!((*mtx).inner)) };
}

/// Try to lock a mutex without blocking: `0` = acquired, non-zero = not acquired.
///
/// The busy code is pthread's `EBUSY` (the C `erl_drv_mutex_trylock` returns its `ethread`
/// equivalent). A null handle returns `EINVAL` (the C aborts).
///
/// # Safety
/// `mtx` must be null or a live handle from [`mutex_create`].
pub unsafe fn mutex_try_lock(mtx: *mut Mutex) -> c_int {
    if mtx.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: `mtx` is a live handle, so its inner mutex is initialised.
    unsafe { libc::pthread_mutex_trylock(ptr::addr_of_mut!((*mtx).inner)) }
}

/// Unlock a mutex; a null handle is a no-op.
///
/// # Safety
/// `mtx` must be null or a live handle from [`mutex_create`], locked by some thread.
pub unsafe fn mutex_unlock(mtx: *mut Mutex) {
    if mtx.is_null() {
        return;
    }
    // SAFETY: `mtx` is a live handle, so its inner mutex is initialised. Unlocking from a thread
    // other than the locker is permitted because the mutex is not error-checking (see
    // `mutex_create`).
    unsafe { libc::pthread_mutex_unlock(ptr::addr_of_mut!((*mtx).inner)) };
}

/// The mutex's debug name (`NULL` when it was created unnamed or for a null handle).
///
/// # Safety
/// `mtx` must be null or a live handle from [`mutex_create`].
pub unsafe fn mutex_name(mtx: *mut Mutex) -> *mut c_char {
    if mtx.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `mtx` is a live handle and `name` is never mutated after creation, so a shared
    // borrow of the field is sound; the returned pointer is valid until `mutex_destroy`.
    name_ptr(unsafe { &(*mtx).name })
}

// ---------------------------------------------------------------------------------------------
// Condition variable (`enif_cond_*` / `erl_drv_cond_*`)
// ---------------------------------------------------------------------------------------------

/// An opaque, pthread-backed condition variable — the C `ErlNifCond`/`ErlDrvCond`.
///
/// Only ever reached through the `*mut Cond` handed out by [`cond_create`].
pub struct Cond {
    inner: libc::pthread_cond_t,
    name: Option<Vec<u8>>,
}

/// Create a condition variable, `NULL` on failure (the C `erl_drv_cond_create` contract).
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string.
pub unsafe fn cond_create(name: *const c_char) -> *mut Cond {
    let mut inner = MaybeUninit::<libc::pthread_cond_t>::uninit();
    // SAFETY: `inner` is a valid, uninitialised `pthread_cond_t` slot and a null attribute asks for
    // the platform default (process-private) condition variable, as in the C.
    if unsafe { libc::pthread_cond_init(inner.as_mut_ptr(), ptr::null()) } != 0 {
        return ptr::null_mut();
    }
    // SAFETY: `pthread_cond_init` returned 0, so the condition variable is fully initialised.
    let inner = unsafe { inner.assume_init() };
    Box::into_raw(Box::new(Cond {
        inner,
        name: copy_name(name),
    }))
}

/// Destroy a condition variable; a null handle is a no-op.
///
/// # Safety
/// `cnd` must be null or a live handle from [`cond_create`] that is not used afterwards.
pub unsafe fn cond_destroy(cnd: *mut Cond) {
    if cnd.is_null() {
        return;
    }
    // SAFETY: `cnd` is a live handle, so its inner condition variable is initialised and not yet
    // destroyed. A failure (e.g. waiters present) is a caller error the C turns into `erts_exit`
    // (`erl_drv_thread.c:273-280`); it is ignored here rather than aborting the runtime.
    unsafe { libc::pthread_cond_destroy(ptr::addr_of_mut!((*cnd).inner)) };
    // SAFETY: the caller transferred ownership of the `cond_create` allocation and must not use the
    // handle again.
    drop(unsafe { Box::from_raw(cnd) });
}

/// Wake one thread waiting on a condition variable; a null handle is a no-op.
///
/// # Safety
/// `cnd` must be null or a live handle from [`cond_create`].
pub unsafe fn cond_signal(cnd: *mut Cond) {
    if cnd.is_null() {
        return;
    }
    // SAFETY: `cnd` is a live handle, so its inner condition variable is initialised.
    unsafe { libc::pthread_cond_signal(ptr::addr_of_mut!((*cnd).inner)) };
}

/// Wake every thread waiting on a condition variable; a null handle is a no-op.
///
/// # Safety
/// `cnd` must be null or a live handle from [`cond_create`].
pub unsafe fn cond_broadcast(cnd: *mut Cond) {
    if cnd.is_null() {
        return;
    }
    // SAFETY: `cnd` is a live handle, so its inner condition variable is initialised.
    unsafe { libc::pthread_cond_broadcast(ptr::addr_of_mut!((*cnd).inner)) };
}

/// Wait on a condition variable, atomically releasing `mtx` and re-acquiring it before returning
/// (the C `erl_drv_cond_wait` contract, `erl_drv_thread.c:305-324`).
///
/// A null handle of either kind is a no-op. Unlike the C, a failing wait is *not* retried in a
/// loop (see the module docs' deviation list).
///
/// # Safety
/// `cnd` must be null or a live handle from [`cond_create`], and `mtx` must be null or a live
/// handle from [`mutex_create`] that the calling thread currently holds (pthread's contract).
pub unsafe fn cond_wait(cnd: *mut Cond, mtx: *mut Mutex) {
    if cnd.is_null() || mtx.is_null() {
        return;
    }
    // SAFETY: both are live handles of the required kinds; `pthread_cond_wait` releases `mtx`
    // while waiting and re-acquires it before returning.
    unsafe {
        libc::pthread_cond_wait(
            ptr::addr_of_mut!((*cnd).inner),
            ptr::addr_of_mut!((*mtx).inner),
        )
    };
}

/// The condition variable's debug name (`NULL` when unnamed or for a null handle).
///
/// # Safety
/// `cnd` must be null or a live handle from [`cond_create`].
pub unsafe fn cond_name(cnd: *mut Cond) -> *mut c_char {
    if cnd.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `cnd` is a live handle and `name` is never mutated after creation.
    name_ptr(unsafe { &(*cnd).name })
}

// ---------------------------------------------------------------------------------------------
// Reader/writer lock (`enif_rwlock_*` / `erl_drv_rwlock_*`)
// ---------------------------------------------------------------------------------------------

/// An opaque, pthread-backed reader/writer lock — the C `ErlNifRWLock`/`ErlDrvRWLock`.
///
/// Only ever reached through the `*mut RwLock` handed out by [`rwlock_create`].
pub struct RwLock {
    inner: libc::pthread_rwlock_t,
    name: Option<Vec<u8>>,
}

/// Create a reader/writer lock, `NULL` on failure (the C `erl_drv_rwlock_create` contract).
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string.
pub unsafe fn rwlock_create(name: *const c_char) -> *mut RwLock {
    let mut inner = MaybeUninit::<libc::pthread_rwlock_t>::uninit();
    // SAFETY: `inner` is a valid, uninitialised `pthread_rwlock_t` slot and a null attribute asks
    // for the platform default (reader-preferring) reader/writer lock, as in the C.
    if unsafe { libc::pthread_rwlock_init(inner.as_mut_ptr(), ptr::null()) } != 0 {
        return ptr::null_mut();
    }
    // SAFETY: `pthread_rwlock_init` returned 0, so the lock is fully initialised.
    let inner = unsafe { inner.assume_init() };
    Box::into_raw(Box::new(RwLock {
        inner,
        name: copy_name(name),
    }))
}

/// Destroy a reader/writer lock; a null handle is a no-op.
///
/// # Safety
/// `lck` must be null or a live handle from [`rwlock_create`] that is not used afterwards.
pub unsafe fn rwlock_destroy(lck: *mut RwLock) {
    if lck.is_null() {
        return;
    }
    // SAFETY: `lck` is a live handle, so its inner lock is initialised and not yet destroyed. A
    // failure is a caller error the C turns into `erts_exit` (`erl_drv_thread.c:351-362`); it is
    // ignored here rather than aborting the runtime.
    unsafe { libc::pthread_rwlock_destroy(ptr::addr_of_mut!((*lck).inner)) };
    // SAFETY: the caller transferred ownership of the `rwlock_create` allocation and must not use
    // the handle again.
    drop(unsafe { Box::from_raw(lck) });
}

/// Try to take a read lock without blocking: `0` = acquired, non-zero (`EBUSY`) = not acquired.
/// A null handle returns `EINVAL` (the C aborts, `erl_drv_thread.c:370-381`).
///
/// # Safety
/// `lck` must be null or a live handle from [`rwlock_create`].
pub unsafe fn rwlock_try_read_lock(lck: *mut RwLock) -> c_int {
    if lck.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: `lck` is a live handle, so its inner lock is initialised.
    unsafe { libc::pthread_rwlock_tryrdlock(ptr::addr_of_mut!((*lck).inner)) }
}

/// Take a read lock, blocking while a writer holds the lock; a null handle is a no-op.
///
/// # Safety
/// `lck` must be null or a live handle from [`rwlock_create`].
pub unsafe fn rwlock_read_lock(lck: *mut RwLock) {
    if lck.is_null() {
        return;
    }
    // SAFETY: `lck` is a live handle, so its inner lock is initialised.
    unsafe { libc::pthread_rwlock_rdlock(ptr::addr_of_mut!((*lck).inner)) };
}

/// Release a read lock; a null handle is a no-op.
///
/// # Safety
/// `lck` must be null or a live handle from [`rwlock_create`] that the calling thread holds for
/// reading.
pub unsafe fn rwlock_read_unlock(lck: *mut RwLock) {
    if lck.is_null() {
        return;
    }
    // SAFETY: `lck` is a live handle with a read lock held by this thread.
    unsafe { libc::pthread_rwlock_unlock(ptr::addr_of_mut!((*lck).inner)) };
}

/// Try to take a write lock without blocking: `0` = acquired, non-zero (`EBUSY`) = not acquired.
/// A null handle returns `EINVAL` (the C aborts, `erl_drv_thread.c:408-419`).
///
/// # Safety
/// `lck` must be null or a live handle from [`rwlock_create`].
pub unsafe fn rwlock_try_write_lock(lck: *mut RwLock) -> c_int {
    if lck.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: `lck` is a live handle, so its inner lock is initialised.
    unsafe { libc::pthread_rwlock_trywrlock(ptr::addr_of_mut!((*lck).inner)) }
}

/// Take a write lock, blocking while any reader or writer holds the lock; a null handle is a no-op.
///
/// # Safety
/// `lck` must be null or a live handle from [`rwlock_create`].
pub unsafe fn rwlock_write_lock(lck: *mut RwLock) {
    if lck.is_null() {
        return;
    }
    // SAFETY: `lck` is a live handle, so its inner lock is initialised.
    unsafe { libc::pthread_rwlock_wrlock(ptr::addr_of_mut!((*lck).inner)) };
}

/// Release a write lock; a null handle is a no-op.
///
/// # Safety
/// `lck` must be null or a live handle from [`rwlock_create`] that the calling thread holds for
/// writing.
pub unsafe fn rwlock_write_unlock(lck: *mut RwLock) {
    if lck.is_null() {
        return;
    }
    // SAFETY: `lck` is a live handle with a write lock held by this thread.
    unsafe { libc::pthread_rwlock_unlock(ptr::addr_of_mut!((*lck).inner)) };
}

/// The reader/writer lock's debug name (`NULL` when unnamed or for a null handle).
///
/// # Safety
/// `lck` must be null or a live handle from [`rwlock_create`].
pub unsafe fn rwlock_name(lck: *mut RwLock) -> *mut c_char {
    if lck.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `lck` is a live handle and `name` is never mutated after creation.
    name_ptr(unsafe { &(*lck).name })
}

// ---------------------------------------------------------------------------------------------
// Thread-specific data (`enif_tsd_*` / `erl_drv_tsd_*`)
// ---------------------------------------------------------------------------------------------

/// Create a thread-specific-data key, writing the new key to `out`.
///
/// Returns `0` on success, or the platform/pthreads error code (`EINVAL` for a null `out`). On
/// failure the C's `-1` sentinel is written to `*out` (`erl_drv_thread.c:447-516`); a caller must
/// therefore check the return value before using the key.
///
/// # Safety
/// `out` must be a valid writable `unsigned int` slot (or null, which returns `EINVAL`).
pub unsafe fn tsd_key_create(_name: *const c_char, out: *mut TsdKey) -> c_int {
    if out.is_null() {
        return libc::EINVAL;
    }
    let mut key: libc::pthread_key_t = 0;
    // SAFETY: `key` is a valid out-slot for `pthread_key_create`; `None` installs no destructor
    // (the C attaches none either — a key's data lives until the thread dies).
    let res = unsafe { libc::pthread_key_create(&mut key, None) };
    // SAFETY: `out` is a valid writable slot (checked non-null above and guaranteed by the caller).
    unsafe {
        *out = if res == 0 { key } else { TsdKey::MAX };
    }
    res
}

/// Destroy a thread-specific-data key.
///
/// The key becomes invalid for every thread; a key that was never created (or was already
/// destroyed) is rejected by pthreads with `EINVAL` and is otherwise a no-op (the C aborts).
pub fn tsd_key_destroy(key: TsdKey) {
    // SAFETY: `pthread_key_delete` only needs a key value; it never dereferences `key`.
    unsafe { libc::pthread_key_delete(key) };
}

/// Set this thread's value for `key`.
///
/// An invalid or destroyed key is rejected with `EINVAL` and is a no-op (the C aborts,
/// `erl_drv_thread.c:540-566`).
pub fn tsd_set(key: TsdKey, data: *mut c_void) {
    // SAFETY: `pthread_setspecific` only associates the pointer *value* `data` with the calling
    // thread's slot for `key`; an out-of-range/destroyed key is validated by pthreads, never used to
    // index memory.
    unsafe { libc::pthread_setspecific(key, data.cast_const()) };
}

/// This thread's value for `key`, or null when unset (or for an invalid/destroyed key).
pub fn tsd_get(key: TsdKey) -> *mut c_void {
    // SAFETY: `pthread_getspecific` reads the calling thread's slot for `key` and returns null for
    // an out-of-range/destroyed key.
    unsafe { libc::pthread_getspecific(key) }
}

// ---------------------------------------------------------------------------------------------
// Thread options (`enif_thread_opts_*` / `erl_drv_thread_opts_*`)
// ---------------------------------------------------------------------------------------------

/// Create a thread-options record, `NULL` on allocation failure.
///
/// `_name` is accepted for signature parity with `erl_drv_thread_opts_create(char *name)` and
/// ignored — the C ignores it too (`erl_drv_thread.c:584-593`); the thread's name is a separate
/// argument of [`thread_create`]. The record is the frozen 4-byte [`ErlDrvThreadOpts`] with the
/// C's default `suggested_stack_size = -1` ("platform default").
pub fn thread_opts_create(_name: *const c_char) -> *mut ErlDrvThreadOpts {
    Box::into_raw(Box::new(ErlDrvThreadOpts {
        suggested_stack_size: -1,
    }))
}

/// Destroy a thread-options record from [`thread_opts_create`]; a null handle is a no-op.
///
/// # Safety
/// `opts` must be null or a live pointer from [`thread_opts_create`] that is not used afterwards.
pub unsafe fn thread_opts_destroy(opts: *mut ErlDrvThreadOpts) {
    if opts.is_null() {
        return;
    }
    // SAFETY: the caller transferred ownership of the `thread_opts_create` allocation and must not
    // use the pointer again.
    drop(unsafe { Box::from_raw(opts) });
}

// ---------------------------------------------------------------------------------------------
// Threads (`enif_thread_*` / `erl_drv_thread_*`)
// ---------------------------------------------------------------------------------------------

/// Bytes per requested "kiloword" of stack — the multiplier of the C's `ETHR_KW2B`
/// (`erts/include/internal/ethr_internal.h:48`: `KW * sizeof(void *) * 1024`). A NIF/driver's
/// `suggested_stack_size` is counted in **kilowords** (`erl_driver.md:637-638`, "in kilowords"),
/// which is 8 KiB per unit on LP64 — so the naive byte interpretation would undersize the request
/// by three orders of magnitude.
const STACK_WORD_BYTES: usize = core::mem::size_of::<*mut c_void>() * 1024;

/// Assumed page size for `ETHR_PAGE_ALIGN`; the C reads it from the OS
/// (`ethr_aux.c:204-211`, `sysconf(_SC_PAGESIZE)`), but only the *rounding* of an explicitly
/// requested stack size depends on it (documented deviation: the layer assumes 4 KiB pages).
const PAGE_BYTES: usize = 4 * 1024;

/// Upper end of the accepted stack window, `ethr_max_stack_size__`
/// (`ethr_aux.c:247-251`: 32 MiB, doubled to 64 MiB on 64-bit, page-aligned).
const MAX_STACK: usize = 64 * 1024 * 1024;

/// Lower end of the accepted stack window, `ethr_min_stack_size__`
/// (`ethr_aux.c:216-245`: 4 KiB, doubled on 64-bit, + 4 KiB "for possible system usage",
/// + the signal-handler stack, then raised to `PTHREAD_STACK_MIN` if still smaller and
/// page-aligned). On this target (`SIGSTKSZ` = 8 KiB, `PTHREAD_STACK_MIN` = 16 KiB) that floor is
/// 20 KiB; a request below it is raised to it, exactly as the C raises it
/// (`pthread/ethread.c:373-376`).
const MIN_STACK: usize = {
    let floor = 4 * 1024 * 2 + 4 * 1024 + 8 * 1024;
    if floor < libc::PTHREAD_STACK_MIN {
        libc::PTHREAD_STACK_MIN
    } else {
        floor
    }
};

/// The byte size this layer applies for a requested `suggested_stack_size` in kilowords: the C's
/// `ETHR_KW2B` conversion, page alignment, the extra page "for possible system usage", then the
/// `[MIN_STACK, MAX_STACK]` clamp (`erts/lib_src/pthread/ethread.c:360-379`). Saturating, so a
/// pathological request cannot overflow.
fn stack_bytes(kilowords: c_int) -> usize {
    let bytes = (kilowords.max(0) as usize).saturating_mul(STACK_WORD_BYTES);
    let bytes = bytes.saturating_add(PAGE_BYTES - 1) & !(PAGE_BYTES - 1);
    bytes.saturating_add(PAGE_BYTES).clamp(MIN_STACK, MAX_STACK)
}

/// What [`thread_create`] hands to the new thread: the user's entry point plus its debug name.
///
/// Ownership moves to the new thread through the `pthread_create` argument, so the C-style `void *`
/// trampoline argument carries no borrowed data.
struct Start {
    func: extern "C" fn(*mut c_void) -> *mut c_void,
    arg: *mut c_void,
    name: Option<Vec<u8>>,
}

/// The entry point every thread created here runs: register the thread (so its own
/// [`thread_type`]/[`thread_name`] are correct from its first instruction), then call the user's
/// function.
extern "C" fn thread_start(payload: *mut c_void) -> *mut c_void {
    // SAFETY: `payload` is the `Box<Start>` that `thread_create` handed to `pthread_create`; the
    // ownership of that allocation passes to exactly this thread, which frees it here by moving the
    // fields out of the box. Freeing it before running `func` also means a `thread_exit` from
    // `func` cannot leak it.
    let Start { func, arg, name } = *unsafe { Box::from_raw(payload as *mut Start) };
    register_thread_name(tid_key(thread_self_tid()), name);
    func(arg)
}

/// Create a thread running `func(args)`, writing its tid to `out_tid`.
///
/// Returns `0` on success or the platform's pthreads error code (`EINVAL` for a null `out_tid`).
/// `name` is the thread's debug name ([`thread_name`]); `opts` may be null (platform default stack)
/// or a record from [`thread_opts_create`], whose `suggested_stack_size` — **in kilowords**, as the
/// C documents it — is converted, clamped into this layer's stack window and applied when `>= 0`;
/// a size the platform rejects falls back to the default stack (the C ignores the rejection too,
/// `pthread/ethread.c:377`).
///
/// # Safety
/// `name` must be null or a valid NUL-terminated C string; `out_tid` must be a valid writable tid
/// slot (or null, which returns `EINVAL`); `opts` must be null or a live pointer from
/// [`thread_opts_create`]; `func` must be safe to call on a fresh thread.
pub unsafe fn thread_create(
    name: *const c_char,
    out_tid: *mut Tid,
    func: extern "C" fn(*mut c_void) -> *mut c_void,
    args: *mut c_void,
    opts: *mut ErlDrvThreadOpts,
) -> c_int {
    if out_tid.is_null() {
        return libc::EINVAL;
    }
    let name = copy_name(name);
    // SAFETY: `opts` is null or a live `thread_opts_create` record (the caller contract).
    let suggested = if opts.is_null() {
        None
    } else {
        Some(unsafe { (*opts).suggested_stack_size })
    };

    let mut attr = MaybeUninit::<libc::pthread_attr_t>::uninit();
    // SAFETY: `attr` is a valid, uninitialised `pthread_attr_t` slot.
    let attr_inited = unsafe { libc::pthread_attr_init(attr.as_mut_ptr()) } == 0;
    if attr_inited {
        if let Some(stack) = suggested.filter(|size| *size >= 0).map(stack_bytes) {
            // SAFETY: `attr` was initialised above; a rejected size leaves the default in place and
            // is not fatal (pthread_create then uses whatever the attribute holds).
            unsafe { libc::pthread_attr_setstacksize(attr.as_mut_ptr(), stack) };
        }
    }
    let attr_ptr = if attr_inited {
        attr.as_ptr()
    } else {
        ptr::null()
    };

    let start = Box::into_raw(Box::new(Start {
        func,
        arg: args,
        name: name.clone(),
    }));
    let mut tid = MaybeUninit::<libc::pthread_t>::uninit();
    // SAFETY: `tid` is a valid out-slot, `attr_ptr` is a live or null attribute and `start` is an
    // owned `Box<Start>` whose ownership moves to the new thread on success.
    let res = unsafe { libc::pthread_create(tid.as_mut_ptr(), attr_ptr, thread_start, start.cast()) };
    if attr_inited {
        // SAFETY: `attr` was successfully initialised above and is not used afterwards.
        unsafe { libc::pthread_attr_destroy(attr.as_mut_ptr()) };
    }
    if res != 0 {
        // SAFETY: `pthread_create` failed, so no thread took ownership of `start` and this thread
        // still owns it exclusively.
        drop(unsafe { Box::from_raw(start) });
        return res;
    }
    // SAFETY: `pthread_create` returned 0, so it initialised `tid`.
    let tid = unsafe { tid.assume_init() };

    // Publish the debug name *before* the tid, so a joiner (which can only learn the tid from
    // `out_tid` below) always observes an entry that exists, and so the creator's own
    // `thread_name(tid)` is correct as soon as this call returns. The new thread registers the same
    // name at its entry (`thread_start`), which makes its own `thread_type`/`thread_name` correct
    // without waiting for this thread to resume; that registration races this one, so both go
    // through `register_thread_name`, which keeps the first entry and never replaces it.
    register_thread_name(tid_key(tid_from_pthread(tid)), name);
    // SAFETY: `out_tid` is a valid writable tid slot (checked non-null above and guaranteed by the
    // caller).
    unsafe { *out_tid = tid_from_pthread(tid) };
    0
}

/// The calling thread's tid.
pub fn thread_self_tid() -> Tid {
    // SAFETY: `pthread_self` takes no arguments and always returns the caller's tid.
    tid_from_pthread(unsafe { libc::pthread_self() })
}

/// Whether two tid handles refer to the same thread (the C `enif_equal_tids`/`erl_drv_equal_tids`).
///
/// A null operand yields `false` (the C aborts, `erl_drv_thread.c:686-702`).
pub fn thread_equal_tids(a: Tid, b: Tid) -> bool {
    if a.is_null() || b.is_null() {
        return false;
    }
    // SAFETY: `pthread_equal` only compares the two tid values; it dereferences nothing.
    unsafe { libc::pthread_equal(pthread_from_tid(a), pthread_from_tid(b)) != 0 }
}

/// Terminate the calling thread, returning `resp` to whoever joins it — never returns.
///
/// Reproduces `erl_drv_thread_exit` (`erl_drv_thread.c:704-713`) minus its "only from a thread
/// created by this interface" check, which the C enforces with `erts_exit`.
// `resp` is the opaque response value `pthread_exit` merely forwards to a joiner; it is never
// dereferenced (or even read) here, so the pointer-validity contract stays the caller's, exactly as
// in the C. An `unsafe fn` would break the C-mirroring public API for no safety gain.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn thread_exit(resp: *mut c_void) -> ! {
    // SAFETY: `pthread_exit` runs the platform's thread-exit sequence for the calling thread; it
    // has no other precondition and does not return.
    unsafe { libc::pthread_exit(resp) }
}

/// Join a thread created by [`thread_create`], writing its response value to `out_resp` (which may
/// be null).
///
/// Returns `0` on success or the platform's pthreads error code (`EINVAL` for a null tid). On
/// success the thread's registry entry is retired, so a later [`thread_name`] for that tid returns
/// null rather than a dangling pointer (the C frees its `ErlDrvTid_` record,
/// `erl_drv_thread.c:715-730`).
///
/// # Safety
/// `tid` must be null or a tid of a joinable thread created by [`thread_create`] and not already
/// joined; `out_resp` must be null or a valid writable `void *` slot.
pub unsafe fn thread_join(tid: Tid, out_resp: *mut *mut c_void) -> c_int {
    if tid.is_null() {
        return libc::EINVAL;
    }
    // SAFETY: `tid` is a live joinable tid (the caller contract) and `out_resp` is null or a valid
    // out-slot, which is exactly `pthread_join`'s own contract.
    let res = unsafe { libc::pthread_join(pthread_from_tid(tid), out_resp) };
    if res == 0 {
        created_threads().remove(&tid_key(tid));
    }
    res
}

/// The debug name of a thread created by [`thread_create`], or null.
///
/// Null is returned for an unnamed thread, for an unknown tid, and for a thread that has already
/// been joined (its entry is retired). The returned buffer stays valid until that thread is joined.
///
/// [`thread_created_unnamed`] tells the two null cases that matter apart: a thread this layer created
/// with a NULL name (which the C reports as [`no_name`], `erl_drv_thread.c:638-639`) versus a tid this
/// layer does not know.
pub fn thread_name(tid: Tid) -> *mut c_char {
    let threads = created_threads();
    match threads.get(&tid_key(tid)).and_then(|name| name.as_deref()) {
        Some(bytes) => bytes.as_ptr() as *mut c_char,
        None => ptr::null_mut(),
    }
}

/// Whether `tid` is a thread created through this layer **with a NULL name** — the state the C stores
/// as its [`no_name`] pointer in the tid record (`erl_drv_thread.c:638-639`).
///
/// `false` for a named created thread, for a tid this layer did not create, and for a null tid — the
/// last two are indistinguishable here, since no handle of a foreign thread is ever registered.
pub fn thread_created_unnamed(tid: Tid) -> bool {
    !tid.is_null() && matches!(created_threads().get(&tid_key(tid)), Some(None))
}

/// The type of the calling thread — `ERL_NIF_THR_*` (`enif_thread_type`, `erl_nif.c:3606-3625`).
///
/// Returns [`ERL_NIF_THR_OTHER`] for a thread created through [`thread_create`] and
/// [`ERL_NIF_THR_UNDEFINED`] otherwise. This runtime has no scheduler threads, so a scheduler type
/// is never reported (see the module docs).
pub fn thread_type() -> c_int {
    if created_threads().contains_key(&tid_key(thread_self_tid())) {
        ERL_NIF_THR_OTHER
    } else {
        ERL_NIF_THR_UNDEFINED
    }
}

#[cfg(test)]
mod tests;
