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

//! `ErlNifApi` — the frozen `enif_*` function-pointer table of the NIF C ABI.
//!
//! # Generated, header-ordered table — the field order IS the freeze
//!
//! This module mirrors `erts/emulator/beam/erl_nif_api_funcs.h` **entry for entry and in exact
//! header order**: one `#[repr(C)]` field per `enif_*` entry of that file's declaration list. The
//! order is the compatibility contract (the header itself warns that new entries must be added at
//! the bottom "to keep compatibility on Windows"), so a field in this table tracks the header's
//! declaration order exactly.
//!
//! # Deriving the LP64 field set: 185 → 180 → 176
//!
//! The header's `ERL_NIF_API_FUNC_DECL` token *occurs* 185 times, but that is a raw token count:
//! five of those occurrences are the file's own guard/comment lines (header lines 23, 28, 34, 235,
//! 239), not declarations. The **actual declaration list therefore has 180 entries** (every line
//! that begins with `ERL_NIF_API_FUNC_DECL(`).
//!
//! Two further adjustments pin the table to the supported **LP64** target
//! (`SIZEOF_LONG == 8`, `SIZEOF_VOID_P == 8`):
//!
//! * **remove the four `enif_*64` entries** (`enif_get_int64`, `enif_get_uint64`, `enif_make_int64`,
//!   `enif_make_uint64`). They live inside `#if SIZEOF_LONG != 8` (header lines 135–140), so they
//!   do **not exist on LP64** — there the header `#define`s each of them onto its `long`/`ulong`
//!   sibling (header lines 669–674: `enif_get_int64` → `enif_get_long`, …). They are therefore not
//!   separate table slots: **180 − 4 = 176**.
//! * `enif_ioq_enq` is **excluded as well.** It appears only in the `ERL_NIF_API_FUNC_MACRO` list
//!   (header line 398): it has no `ERL_NIF_API_FUNC_DECL` entry, no signature and no definition
//!   anywhere in the tree — a vestigial macro, not a table slot.
//!
//! The result is the header-exact LP64 set of **176** fields, in exact `ERL_NIF_API_FUNC_DECL`
//! order. (The macro list has 181 entries — the 180 declaration names plus the macro-only
//! `enif_ioq_enq` — but it is only the *declaration* list that describes the runtime's actual
//! `enif_*` surface.)
//!
//! Every field is `Option<unsafe extern "C" fn …>` — a C function pointer is null-pointer
//! optimized, so each column is exactly one machine word (8 bytes on LP64) and an all-`None` table
//! can be constructed safely ([`ErlNifApi::empty`]). `size_of::<ErlNifApi>()` is therefore
//! `176 * 8 == 1408`, pinned by the compile-time assertion at the bottom of this file.
//!
//! The declared pointers are **never called** in this crate: the functions they name are
//! implemented by the NIF subsystem (`beam_bifs` / `beam_rt` / `beam_io`) in a later pass. This is
//! the documented deviation of the crate (`beam_abi` `CONTEXT.md`, Design Decisions): the header
//! exposes these as exported free functions on Unix, and modelling them as one `#[repr(C)]` table
//! is how the Rust runtime will export/consume the surface — it makes the frozen signature list
//! explicit and compile-checked without changing any individual function's ABI.
//!
//! # Length
//!
//! This file is a legitimately long, **generated-style table**: one documented field per header
//! entry. It is deliberately not split.

use crate::common::{ErlNifSelectFlags, SysIOVec};
use crate::nif::term::ERL_NIF_TERM;
use crate::nif::types::{
    ErlNifBinary, ErlNifCharEncoding, ErlNifCond, ErlNifEnv, ErlNifEvent, ErlNifHash, ErlNifIOQueue,
    ErlNifIOQueueOpts, ErlNifIOVec, ErlNifMapIterator, ErlNifMapIteratorEntry, ErlNifMonitor,
    ErlNifMutex, ErlNifOption, ErlNifPid, ErlNifPort, ErlNifResourceDtor, ErlNifResourceFlags,
    ErlNifResourceType, ErlNifResourceTypeInit, ErlNifRWLock, ErlNifSysInfo,
    ErlNifTermType, ErlNifThreadOpts, ErlNifTid, ErlNifTime, ErlNifTimeUnit, ErlNifTSDKey,
    ErlNifUInt64, ErlNifUniqueInteger, VaListTag,
};
use libc::{c_char, c_double, c_int, c_long, c_uchar, c_uint, c_ulong, c_void, size_t, FILE};

/// `ErlNifApi` — the frozen table of `enif_*` functions the runtime provides to a NIF library.
///
/// One `#[repr(C)]` field per entry of `erl_nif_api_funcs.h`'s declaration list that exists on LP64,
/// in exact header order; every field is `Option<unsafe extern "C" fn …>` (the C function pointer,
/// null-optimized). The field count is pinned by the compile-time assertion at the bottom of this
/// file ([`ERL_NIF_API_FIELD_COUNT`] `== 176`, i.e. `1408` bytes on LP64).
///
/// Use [`ErlNifApi::empty`] to build an all-`None` table (e.g. for tests, or for a runtime that has
/// not yet wired up a function).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct ErlNifApi {
    /// `void *enif_priv_data(ErlNifEnv* env);` — the library's private data pointer.
    pub enif_priv_data: Option<unsafe extern "C" fn(*mut ErlNifEnv) -> *mut c_void>,
    /// `void *enif_alloc(size_t size);` — allocate `size` bytes.
    pub enif_alloc: Option<unsafe extern "C" fn(size_t) -> *mut c_void>,
    /// `void enif_free(void *ptr);` — free memory obtained from `enif_alloc`/`enif_realloc`.
    pub enif_free: Option<unsafe extern "C" fn(*mut c_void)>,
    /// `int enif_is_atom(ErlNifEnv*, ERL_NIF_TERM term);` — is `term` an atom?
    pub enif_is_atom: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `int enif_is_binary(ErlNifEnv*, ERL_NIF_TERM term);` — is `term` a binary?
    pub enif_is_binary: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `int enif_is_ref(ErlNifEnv*, ERL_NIF_TERM term);` — is `term` a reference?
    pub enif_is_ref: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `int enif_inspect_binary(ErlNifEnv*, ERL_NIF_TERM bin_term, ErlNifBinary* bin);`
    pub enif_inspect_binary:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ErlNifBinary) -> c_int>,
    /// `int enif_alloc_binary(size_t size, ErlNifBinary* bin);`
    pub enif_alloc_binary: Option<unsafe extern "C" fn(size_t, *mut ErlNifBinary) -> c_int>,
    /// `int enif_realloc_binary(ErlNifBinary* bin, size_t size);`
    pub enif_realloc_binary: Option<unsafe extern "C" fn(*mut ErlNifBinary, size_t) -> c_int>,
    /// `void enif_release_binary(ErlNifBinary* bin);`
    pub enif_release_binary: Option<unsafe extern "C" fn(*mut ErlNifBinary)>,
    /// `int enif_get_int(ErlNifEnv*, ERL_NIF_TERM term, int* ip);`
    pub enif_get_int:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_int) -> c_int>,
    /// `int enif_get_ulong(ErlNifEnv*, ERL_NIF_TERM term, unsigned long* ip);`
    pub enif_get_ulong:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_ulong) -> c_int>,
    /// `int enif_get_double(ErlNifEnv*, ERL_NIF_TERM term, double* dp);`
    pub enif_get_double:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_double) -> c_int>,
    /// `int enif_get_list_cell(ErlNifEnv* env, ERL_NIF_TERM term, ERL_NIF_TERM* head, ERL_NIF_TERM* tail);`
    pub enif_get_list_cell: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ERL_NIF_TERM, *mut ERL_NIF_TERM) -> c_int,
    >,
    /// `int enif_get_tuple(ErlNifEnv* env, ERL_NIF_TERM tpl, int* arity, const ERL_NIF_TERM** array);`
    pub enif_get_tuple: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_int, *mut *const ERL_NIF_TERM) -> c_int,
    >,
    /// `int enif_is_identical(ERL_NIF_TERM lhs, ERL_NIF_TERM rhs);` — exact term equality.
    pub enif_is_identical:
        Option<unsafe extern "C" fn(ERL_NIF_TERM, ERL_NIF_TERM) -> c_int>,
    /// `int enif_compare(ERL_NIF_TERM lhs, ERL_NIF_TERM rhs);` — term order comparison.
    pub enif_compare: Option<unsafe extern "C" fn(ERL_NIF_TERM, ERL_NIF_TERM) -> c_int>,
    /// `ERL_NIF_TERM enif_make_binary(ErlNifEnv* env, ErlNifBinary* bin);`
    pub enif_make_binary:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut ErlNifBinary) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_badarg(ErlNifEnv* env);` — the `badarg` exception.
    pub enif_make_badarg: Option<unsafe extern "C" fn(*mut ErlNifEnv) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_int(ErlNifEnv* env, int i);`
    pub enif_make_int: Option<unsafe extern "C" fn(*mut ErlNifEnv, c_int) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_ulong(ErlNifEnv* env, unsigned long i);`
    pub enif_make_ulong: Option<unsafe extern "C" fn(*mut ErlNifEnv, c_ulong) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_double(ErlNifEnv* env, double d);`
    pub enif_make_double: Option<unsafe extern "C" fn(*mut ErlNifEnv, c_double) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_atom(ErlNifEnv* env, const char* name);`
    pub enif_make_atom: Option<unsafe extern "C" fn(*mut ErlNifEnv, *const c_char) -> ERL_NIF_TERM>,
    /// `int enif_make_existing_atom(ErlNifEnv* env, const char* name, ERL_NIF_TERM* atom, ErlNifCharEncoding);`
    pub enif_make_existing_atom: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *const c_char, *mut ERL_NIF_TERM, ErlNifCharEncoding) -> c_int,
    >,
    /// `ERL_NIF_TERM enif_make_tuple(ErlNifEnv* env, unsigned cnt, ...);` — variadic.
    pub enif_make_tuple:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, c_uint, ...) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_list(ErlNifEnv* env, unsigned cnt, ...);` — variadic.
    pub enif_make_list:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, c_uint, ...) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_list_cell(ErlNifEnv* env, ERL_NIF_TERM car, ERL_NIF_TERM cdr);`
    pub enif_make_list_cell:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, ERL_NIF_TERM) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_string(ErlNifEnv* env, const char* string, ErlNifCharEncoding);`
    pub enif_make_string: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *const c_char, ErlNifCharEncoding) -> ERL_NIF_TERM,
    >,
    /// `ERL_NIF_TERM enif_make_ref(ErlNifEnv* env);`
    pub enif_make_ref: Option<unsafe extern "C" fn(*mut ErlNifEnv) -> ERL_NIF_TERM>,
    /// `ErlNifMutex* enif_mutex_create(char *name);`
    pub enif_mutex_create: Option<unsafe extern "C" fn(*mut c_char) -> *mut ErlNifMutex>,
    /// `void enif_mutex_destroy(ErlNifMutex *mtx);`
    pub enif_mutex_destroy: Option<unsafe extern "C" fn(*mut ErlNifMutex)>,
    /// `int enif_mutex_trylock(ErlNifMutex *mtx);`
    pub enif_mutex_trylock: Option<unsafe extern "C" fn(*mut ErlNifMutex) -> c_int>,
    /// `void enif_mutex_lock(ErlNifMutex *mtx);`
    pub enif_mutex_lock: Option<unsafe extern "C" fn(*mut ErlNifMutex)>,
    /// `void enif_mutex_unlock(ErlNifMutex *mtx);`
    pub enif_mutex_unlock: Option<unsafe extern "C" fn(*mut ErlNifMutex)>,
    /// `ErlNifCond* enif_cond_create(char *name);`
    pub enif_cond_create: Option<unsafe extern "C" fn(*mut c_char) -> *mut ErlNifCond>,
    /// `void enif_cond_destroy(ErlNifCond *cnd);`
    pub enif_cond_destroy: Option<unsafe extern "C" fn(*mut ErlNifCond)>,
    /// `void enif_cond_signal(ErlNifCond *cnd);`
    pub enif_cond_signal: Option<unsafe extern "C" fn(*mut ErlNifCond)>,
    /// `void enif_cond_broadcast(ErlNifCond *cnd);`
    pub enif_cond_broadcast: Option<unsafe extern "C" fn(*mut ErlNifCond)>,
    /// `void enif_cond_wait(ErlNifCond *cnd, ErlNifMutex *mtx);`
    pub enif_cond_wait: Option<unsafe extern "C" fn(*mut ErlNifCond, *mut ErlNifMutex)>,
    /// `ErlNifRWLock* enif_rwlock_create(char *name);`
    pub enif_rwlock_create: Option<unsafe extern "C" fn(*mut c_char) -> *mut ErlNifRWLock>,
    /// `void enif_rwlock_destroy(ErlNifRWLock *rwlck);`
    pub enif_rwlock_destroy: Option<unsafe extern "C" fn(*mut ErlNifRWLock)>,
    /// `int enif_rwlock_tryrlock(ErlNifRWLock *rwlck);`
    pub enif_rwlock_tryrlock: Option<unsafe extern "C" fn(*mut ErlNifRWLock) -> c_int>,
    /// `void enif_rwlock_rlock(ErlNifRWLock *rwlck);`
    pub enif_rwlock_rlock: Option<unsafe extern "C" fn(*mut ErlNifRWLock)>,
    /// `void enif_rwlock_runlock(ErlNifRWLock *rwlck);`
    pub enif_rwlock_runlock: Option<unsafe extern "C" fn(*mut ErlNifRWLock)>,
    /// `int enif_rwlock_tryrwlock(ErlNifRWLock *rwlck);`
    pub enif_rwlock_tryrwlock: Option<unsafe extern "C" fn(*mut ErlNifRWLock) -> c_int>,
    /// `void enif_rwlock_rwlock(ErlNifRWLock *rwlck);`
    pub enif_rwlock_rwlock: Option<unsafe extern "C" fn(*mut ErlNifRWLock)>,
    /// `void enif_rwlock_rwunlock(ErlNifRWLock *rwlck);`
    pub enif_rwlock_rwunlock: Option<unsafe extern "C" fn(*mut ErlNifRWLock)>,
    /// `int enif_tsd_key_create(char *name, ErlNifTSDKey *key);`
    pub enif_tsd_key_create:
        Option<unsafe extern "C" fn(*mut c_char, *mut ErlNifTSDKey) -> c_int>,
    /// `void enif_tsd_key_destroy(ErlNifTSDKey key);`
    pub enif_tsd_key_destroy: Option<unsafe extern "C" fn(ErlNifTSDKey)>,
    /// `void enif_tsd_set(ErlNifTSDKey key, void *data);`
    pub enif_tsd_set: Option<unsafe extern "C" fn(ErlNifTSDKey, *mut c_void)>,
    /// `void* enif_tsd_get(ErlNifTSDKey key);`
    pub enif_tsd_get: Option<unsafe extern "C" fn(ErlNifTSDKey) -> *mut c_void>,
    /// `ErlNifThreadOpts* enif_thread_opts_create(char *name);`
    pub enif_thread_opts_create:
        Option<unsafe extern "C" fn(*mut c_char) -> *mut ErlNifThreadOpts>,
    /// `void enif_thread_opts_destroy(ErlNifThreadOpts *opts);`
    pub enif_thread_opts_destroy: Option<unsafe extern "C" fn(*mut ErlNifThreadOpts)>,
    /// `int enif_thread_create(char *name, ErlNifTid *tid, void * (*func)(void *), void *args, ErlNifThreadOpts *opts);`
    ///
    /// The third parameter is the C thread entry point `void *(*)(void *)`.
    pub enif_thread_create: Option<
        unsafe extern "C" fn(
            *mut c_char,
            *mut ErlNifTid,
            Option<unsafe extern "C" fn(*mut c_void) -> *mut c_void>,
            *mut c_void,
            *mut ErlNifThreadOpts,
        ) -> c_int,
    >,
    /// `ErlNifTid enif_thread_self(void);`
    pub enif_thread_self: Option<unsafe extern "C" fn() -> ErlNifTid>,
    /// `int enif_equal_tids(ErlNifTid tid1, ErlNifTid tid2);`
    pub enif_equal_tids: Option<unsafe extern "C" fn(ErlNifTid, ErlNifTid) -> c_int>,
    /// `void enif_thread_exit(void *resp);`
    pub enif_thread_exit: Option<unsafe extern "C" fn(*mut c_void)>,
    /// `int enif_thread_join(ErlNifTid, void **respp);`
    pub enif_thread_join:
        Option<unsafe extern "C" fn(ErlNifTid, *mut *mut c_void) -> c_int>,
    /// `void *enif_realloc(void *ptr, size_t size);`
    pub enif_realloc: Option<unsafe extern "C" fn(*mut c_void, size_t) -> *mut c_void>,
    /// `void enif_system_info(ErlNifSysInfo *sip, size_t si_size);`
    pub enif_system_info: Option<unsafe extern "C" fn(*mut ErlNifSysInfo, size_t)>,
    /// `int enif_fprintf(FILE* filep, const char *format, ...);` — variadic.
    pub enif_fprintf: Option<unsafe extern "C" fn(*mut FILE, *const c_char, ...) -> c_int>,
    /// `int enif_inspect_iolist_as_binary(ErlNifEnv*, ERL_NIF_TERM term, ErlNifBinary* bin);`
    pub enif_inspect_iolist_as_binary:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ErlNifBinary) -> c_int>,
    /// `ERL_NIF_TERM enif_make_sub_binary(ErlNifEnv*, ERL_NIF_TERM bin_term, size_t pos, size_t size);`
    pub enif_make_sub_binary:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, size_t, size_t) -> ERL_NIF_TERM>,
    /// `int enif_get_string(ErlNifEnv*, ERL_NIF_TERM list, char* buf, unsigned len, ErlNifCharEncoding);`
    pub enif_get_string: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_char, c_uint, ErlNifCharEncoding) -> c_int,
    >,
    /// `int enif_get_atom(ErlNifEnv*, ERL_NIF_TERM atom, char* buf, unsigned len, ErlNifCharEncoding);`
    pub enif_get_atom: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_char, c_uint, ErlNifCharEncoding) -> c_int,
    >,
    /// `int enif_is_fun(ErlNifEnv*, ERL_NIF_TERM term);`
    pub enif_is_fun: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `int enif_is_pid(ErlNifEnv*, ERL_NIF_TERM term);`
    pub enif_is_pid: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `int enif_is_port(ErlNifEnv*, ERL_NIF_TERM term);`
    pub enif_is_port: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `int enif_get_uint(ErlNifEnv*, ERL_NIF_TERM term, unsigned* ip);`
    pub enif_get_uint:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_uint) -> c_int>,
    /// `int enif_get_long(ErlNifEnv*, ERL_NIF_TERM term, long* ip);`
    pub enif_get_long:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_long) -> c_int>,
    /// `ERL_NIF_TERM enif_make_uint(ErlNifEnv*, unsigned i);`
    pub enif_make_uint: Option<unsafe extern "C" fn(*mut ErlNifEnv, c_uint) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_long(ErlNifEnv*, long i);`
    pub enif_make_long: Option<unsafe extern "C" fn(*mut ErlNifEnv, c_long) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_tuple_from_array(ErlNifEnv*, const ERL_NIF_TERM arr[], unsigned cnt);`
    pub enif_make_tuple_from_array:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *const ERL_NIF_TERM, c_uint) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_list_from_array(ErlNifEnv*, const ERL_NIF_TERM arr[], unsigned cnt);`
    pub enif_make_list_from_array:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *const ERL_NIF_TERM, c_uint) -> ERL_NIF_TERM>,
    /// `int enif_is_empty_list(ErlNifEnv*, ERL_NIF_TERM term);`
    pub enif_is_empty_list: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `ErlNifResourceType* enif_open_resource_type(ErlNifEnv*, const char* module_str, const char* name_str, void (*dtor)(ErlNifEnv*,void *), ErlNifResourceFlags flags, ErlNifResourceFlags* tried);`
    pub enif_open_resource_type: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            *const c_char,
            *const c_char,
            Option<ErlNifResourceDtor>,
            ErlNifResourceFlags,
            *mut ErlNifResourceFlags,
        ) -> *mut ErlNifResourceType,
    >,
    /// `void *enif_alloc_resource(ErlNifResourceType *type, size_t size);`
    pub enif_alloc_resource:
        Option<unsafe extern "C" fn(*mut ErlNifResourceType, size_t) -> *mut c_void>,
    /// `void enif_release_resource(void* obj);`
    pub enif_release_resource: Option<unsafe extern "C" fn(*mut c_void)>,
    /// `ERL_NIF_TERM enif_make_resource(ErlNifEnv*, void* obj);`
    pub enif_make_resource:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut c_void) -> ERL_NIF_TERM>,
    /// `int enif_get_resource(ErlNifEnv*, ERL_NIF_TERM term, ErlNifResourceType* type, void** objp);`
    pub enif_get_resource: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ErlNifResourceType, *mut *mut c_void) -> c_int,
    >,
    /// `size_t enif_sizeof_resource(void* obj);`
    pub enif_sizeof_resource: Option<unsafe extern "C" fn(*mut c_void) -> size_t>,
    /// `unsigned char* enif_make_new_binary(ErlNifEnv*, size_t size, ERL_NIF_TERM* termp);`
    pub enif_make_new_binary:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, size_t, *mut ERL_NIF_TERM) -> *mut c_uchar>,
    /// `int enif_is_list(ErlNifEnv*, ERL_NIF_TERM term);`
    pub enif_is_list: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `int enif_is_tuple(ErlNifEnv*, ERL_NIF_TERM term);`
    pub enif_is_tuple: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `int enif_get_atom_length(ErlNifEnv*, ERL_NIF_TERM atom, unsigned* len, ErlNifCharEncoding);`
    pub enif_get_atom_length: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_uint, ErlNifCharEncoding) -> c_int,
    >,
    /// `int enif_get_list_length(ErlNifEnv* env, ERL_NIF_TERM term, unsigned* len);`
    pub enif_get_list_length:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_uint) -> c_int>,
    /// `ERL_NIF_TERM enif_make_atom_len(ErlNifEnv* env, const char* name, size_t len);`
    pub enif_make_atom_len:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *const c_char, size_t) -> ERL_NIF_TERM>,
    /// `int enif_make_existing_atom_len(ErlNifEnv* env, const char* name, size_t len, ERL_NIF_TERM* atom, ErlNifCharEncoding);`
    pub enif_make_existing_atom_len: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            *const c_char,
            size_t,
            *mut ERL_NIF_TERM,
            ErlNifCharEncoding,
        ) -> c_int,
    >,
    /// `ERL_NIF_TERM enif_make_string_len(ErlNifEnv* env, const char* string, size_t len, ErlNifCharEncoding);`
    pub enif_make_string_len: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *const c_char, size_t, ErlNifCharEncoding) -> ERL_NIF_TERM,
    >,
    /// `ErlNifEnv* enif_alloc_env(void);`
    pub enif_alloc_env: Option<unsafe extern "C" fn() -> *mut ErlNifEnv>,
    /// `void enif_free_env(ErlNifEnv* env);`
    pub enif_free_env: Option<unsafe extern "C" fn(*mut ErlNifEnv)>,
    /// `void enif_clear_env(ErlNifEnv* env);`
    pub enif_clear_env: Option<unsafe extern "C" fn(*mut ErlNifEnv)>,
    /// `int enif_send(ErlNifEnv* env, const ErlNifPid* to_pid, ErlNifEnv* msg_env, ERL_NIF_TERM msg);`
    pub enif_send: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *const ErlNifPid, *mut ErlNifEnv, ERL_NIF_TERM) -> c_int,
    >,
    /// `ERL_NIF_TERM enif_make_copy(ErlNifEnv* dst_env, ERL_NIF_TERM src_term);`
    pub enif_make_copy:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> ERL_NIF_TERM>,
    /// `ErlNifPid* enif_self(ErlNifEnv* caller_env, ErlNifPid* pid);`
    pub enif_self: Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut ErlNifPid) -> *mut ErlNifPid>,
    /// `int enif_get_local_pid(ErlNifEnv* env, ERL_NIF_TERM, ErlNifPid* pid);`
    pub enif_get_local_pid:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ErlNifPid) -> c_int>,
    /// `void enif_keep_resource(void* obj);`
    pub enif_keep_resource: Option<unsafe extern "C" fn(*mut c_void)>,
    /// `ERL_NIF_TERM enif_make_resource_binary(ErlNifEnv*, void* obj, const void* data, size_t size);`
    pub enif_make_resource_binary: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *mut c_void, *const c_void, size_t) -> ERL_NIF_TERM,
    >,
    /// `int enif_is_exception(ErlNifEnv*, ERL_NIF_TERM term);`
    pub enif_is_exception: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `int enif_make_reverse_list(ErlNifEnv*, ERL_NIF_TERM term, ERL_NIF_TERM *list);`
    pub enif_make_reverse_list:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ERL_NIF_TERM) -> c_int>,
    /// `int enif_is_number(ErlNifEnv*, ERL_NIF_TERM term);`
    pub enif_is_number: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `void *enif_dlopen(const char* lib, void (*err_handler)(void*,const char*), void* err_arg);`
    pub enif_dlopen: Option<
        unsafe extern "C" fn(
            *const c_char,
            Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
            *mut c_void,
        ) -> *mut c_void,
    >,
    /// `void *enif_dlsym(void* handle, const char* symbol, void (*err_handler)(void*,const char*), void* err_arg);`
    pub enif_dlsym: Option<
        unsafe extern "C" fn(
            *mut c_void,
            *const c_char,
            Option<unsafe extern "C" fn(*mut c_void, *const c_char)>,
            *mut c_void,
        ) -> *mut c_void,
    >,
    /// `int enif_consume_timeslice(ErlNifEnv*, int percent);`
    pub enif_consume_timeslice:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, c_int) -> c_int>,
    /// `int enif_is_map(ErlNifEnv* env, ERL_NIF_TERM term);`
    pub enif_is_map: Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> c_int>,
    /// `int enif_get_map_size(ErlNifEnv* env, ERL_NIF_TERM term, size_t *size);`
    pub enif_get_map_size:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut size_t) -> c_int>,
    /// `ERL_NIF_TERM enif_make_new_map(ErlNifEnv* env);`
    pub enif_make_new_map: Option<unsafe extern "C" fn(*mut ErlNifEnv) -> ERL_NIF_TERM>,
    /// `int enif_make_map_put(ErlNifEnv* env, ERL_NIF_TERM map_in, ERL_NIF_TERM key, ERL_NIF_TERM value, ERL_NIF_TERM* map_out);`
    pub enif_make_map_put: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            ERL_NIF_TERM,
            ERL_NIF_TERM,
            ERL_NIF_TERM,
            *mut ERL_NIF_TERM,
        ) -> c_int,
    >,
    /// `int enif_get_map_value(ErlNifEnv* env, ERL_NIF_TERM map, ERL_NIF_TERM key, ERL_NIF_TERM* value);`
    pub enif_get_map_value: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, ERL_NIF_TERM, *mut ERL_NIF_TERM) -> c_int,
    >,
    /// `int enif_make_map_update(ErlNifEnv* env, ERL_NIF_TERM map_in, ERL_NIF_TERM key, ERL_NIF_TERM value, ERL_NIF_TERM* map_out);`
    pub enif_make_map_update: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            ERL_NIF_TERM,
            ERL_NIF_TERM,
            ERL_NIF_TERM,
            *mut ERL_NIF_TERM,
        ) -> c_int,
    >,
    /// `int enif_make_map_remove(ErlNifEnv* env, ERL_NIF_TERM map_in, ERL_NIF_TERM key, ERL_NIF_TERM* map_out);`
    pub enif_make_map_remove: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, ERL_NIF_TERM, *mut ERL_NIF_TERM) -> c_int,
    >,
    /// `int enif_map_iterator_create(ErlNifEnv *env, ERL_NIF_TERM map, ErlNifMapIterator *iter, ErlNifMapIteratorEntry entry);`
    pub enif_map_iterator_create: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ErlNifMapIterator, ErlNifMapIteratorEntry) -> c_int,
    >,
    /// `void enif_map_iterator_destroy(ErlNifEnv *env, ErlNifMapIterator *iter);`
    pub enif_map_iterator_destroy:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut ErlNifMapIterator)>,
    /// `int enif_map_iterator_is_head(ErlNifEnv *env, ErlNifMapIterator *iter);`
    pub enif_map_iterator_is_head:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut ErlNifMapIterator) -> c_int>,
    /// `int enif_map_iterator_is_tail(ErlNifEnv *env, ErlNifMapIterator *iter);`
    pub enif_map_iterator_is_tail:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut ErlNifMapIterator) -> c_int>,
    /// `int enif_map_iterator_next(ErlNifEnv *env, ErlNifMapIterator *iter);`
    pub enif_map_iterator_next:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut ErlNifMapIterator) -> c_int>,
    /// `int enif_map_iterator_prev(ErlNifEnv *env, ErlNifMapIterator *iter);`
    pub enif_map_iterator_prev:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut ErlNifMapIterator) -> c_int>,
    /// `int enif_map_iterator_get_pair(ErlNifEnv *env, ErlNifMapIterator *iter, ERL_NIF_TERM *key, ERL_NIF_TERM *value);`
    pub enif_map_iterator_get_pair: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            *mut ErlNifMapIterator,
            *mut ERL_NIF_TERM,
            *mut ERL_NIF_TERM,
        ) -> c_int,
    >,
    /// `ERL_NIF_TERM enif_schedule_nif(ErlNifEnv*, const char*, int, ERL_NIF_TERM (*)(ErlNifEnv*,int,const ERL_NIF_TERM[]), int, const ERL_NIF_TERM[]);`
    ///
    /// The fourth parameter is the NIF entry point `ERL_NIF_TERM (*)(ErlNifEnv*, int, const ERL_NIF_TERM[])`.
    pub enif_schedule_nif: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            *const c_char,
            c_int,
            Option<unsafe extern "C" fn(*mut ErlNifEnv, c_int, *const ERL_NIF_TERM) -> ERL_NIF_TERM>,
            c_int,
            *const ERL_NIF_TERM,
        ) -> ERL_NIF_TERM,
    >,
    /// `int enif_has_pending_exception(ErlNifEnv *env, ERL_NIF_TERM* reason);`
    pub enif_has_pending_exception:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut ERL_NIF_TERM) -> c_int>,
    /// `ERL_NIF_TERM enif_raise_exception(ErlNifEnv *env, ERL_NIF_TERM reason);`
    pub enif_raise_exception:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> ERL_NIF_TERM>,
    /// `int enif_getenv(const char* key, char* value, size_t* value_size);`
    pub enif_getenv:
        Option<unsafe extern "C" fn(*const c_char, *mut c_char, *mut size_t) -> c_int>,
    /// `ErlNifTime enif_monotonic_time(ErlNifTimeUnit);`
    pub enif_monotonic_time: Option<unsafe extern "C" fn(ErlNifTimeUnit) -> ErlNifTime>,
    /// `ErlNifTime enif_time_offset(ErlNifTimeUnit);`
    pub enif_time_offset: Option<unsafe extern "C" fn(ErlNifTimeUnit) -> ErlNifTime>,
    /// `ErlNifTime enif_convert_time_unit(ErlNifTime, ErlNifTimeUnit, ErlNifTimeUnit);`
    pub enif_convert_time_unit:
        Option<unsafe extern "C" fn(ErlNifTime, ErlNifTimeUnit, ErlNifTimeUnit) -> ErlNifTime>,
    /// `ERL_NIF_TERM enif_now_time(ErlNifEnv *env);`
    pub enif_now_time: Option<unsafe extern "C" fn(*mut ErlNifEnv) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_cpu_time(ErlNifEnv *env);`
    pub enif_cpu_time: Option<unsafe extern "C" fn(*mut ErlNifEnv) -> ERL_NIF_TERM>,
    /// `ERL_NIF_TERM enif_make_unique_integer(ErlNifEnv *env, ErlNifUniqueInteger properties);`
    pub enif_make_unique_integer:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ErlNifUniqueInteger) -> ERL_NIF_TERM>,
    /// `int enif_is_current_process_alive(ErlNifEnv *env);`
    pub enif_is_current_process_alive: Option<unsafe extern "C" fn(*mut ErlNifEnv) -> c_int>,
    /// `int enif_is_process_alive(ErlNifEnv *env, ErlNifPid *pid);`
    pub enif_is_process_alive:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut ErlNifPid) -> c_int>,
    /// `int enif_is_port_alive(ErlNifEnv *env, ErlNifPort *port_id);`
    pub enif_is_port_alive:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut ErlNifPort) -> c_int>,
    /// `int enif_get_local_port(ErlNifEnv* env, ERL_NIF_TERM, ErlNifPort* port_id);`
    pub enif_get_local_port:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ErlNifPort) -> c_int>,
    /// `int enif_term_to_binary(ErlNifEnv *env, ERL_NIF_TERM term, ErlNifBinary *bin);`
    pub enif_term_to_binary:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ErlNifBinary) -> c_int>,
    /// `size_t enif_binary_to_term(ErlNifEnv *env, const unsigned char* data, size_t sz, ERL_NIF_TERM *term, unsigned int opts);`
    pub enif_binary_to_term: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *const c_uchar, size_t, *mut ERL_NIF_TERM, c_uint) -> size_t,
    >,
    /// `int enif_port_command(ErlNifEnv *env, const ErlNifPort* to_port, ErlNifEnv *msg_env, ERL_NIF_TERM msg);`
    pub enif_port_command: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *const ErlNifPort, *mut ErlNifEnv, ERL_NIF_TERM) -> c_int,
    >,
    /// `int enif_thread_type(void);`
    pub enif_thread_type: Option<unsafe extern "C" fn() -> c_int>,
    /// `int enif_snprintf(char * buffer, size_t size, const char *format, ...);` — variadic.
    pub enif_snprintf:
        Option<unsafe extern "C" fn(*mut c_char, size_t, *const c_char, ...) -> c_int>,
    /// `int enif_select(ErlNifEnv* env, ErlNifEvent e, enum ErlNifSelectFlags flags, void* obj, const ErlNifPid* pid, ERL_NIF_TERM ref);`
    pub enif_select: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            ErlNifEvent,
            ErlNifSelectFlags,
            *mut c_void,
            *const ErlNifPid,
            ERL_NIF_TERM,
        ) -> c_int,
    >,
    /// `ErlNifResourceType* enif_open_resource_type_x(ErlNifEnv*, const char* name_str, const ErlNifResourceTypeInit*, ErlNifResourceFlags flags, ErlNifResourceFlags* tried);`
    pub enif_open_resource_type_x: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            *const c_char,
            *const ErlNifResourceTypeInit,
            ErlNifResourceFlags,
            *mut ErlNifResourceFlags,
        ) -> *mut ErlNifResourceType,
    >,
    /// `int enif_monitor_process(ErlNifEnv*, void* obj, const ErlNifPid*, ErlNifMonitor *monitor);`
    pub enif_monitor_process: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *mut c_void, *const ErlNifPid, *mut ErlNifMonitor) -> c_int,
    >,
    /// `int enif_demonitor_process(ErlNifEnv*, void* obj, const ErlNifMonitor *monitor);`
    pub enif_demonitor_process:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *mut c_void, *const ErlNifMonitor) -> c_int>,
    /// `int enif_compare_monitors(const ErlNifMonitor*, const ErlNifMonitor*);`
    pub enif_compare_monitors:
        Option<unsafe extern "C" fn(*const ErlNifMonitor, *const ErlNifMonitor) -> c_int>,
    /// `ErlNifUInt64 enif_hash(ErlNifHash type, ERL_NIF_TERM term, ErlNifUInt64 salt);`
    pub enif_hash:
        Option<unsafe extern "C" fn(ErlNifHash, ERL_NIF_TERM, ErlNifUInt64) -> ErlNifUInt64>,
    /// `int enif_whereis_pid(ErlNifEnv *env, ERL_NIF_TERM name, ErlNifPid *pid);`
    pub enif_whereis_pid:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ErlNifPid) -> c_int>,
    /// `int enif_whereis_port(ErlNifEnv *env, ERL_NIF_TERM name, ErlNifPort *port);`
    pub enif_whereis_port:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut ErlNifPort) -> c_int>,
    /// `ErlNifIOQueue *enif_ioq_create(ErlNifIOQueueOpts opts);`
    pub enif_ioq_create: Option<unsafe extern "C" fn(ErlNifIOQueueOpts) -> *mut ErlNifIOQueue>,
    /// `void enif_ioq_destroy(ErlNifIOQueue *q);`
    pub enif_ioq_destroy: Option<unsafe extern "C" fn(*mut ErlNifIOQueue)>,
    /// `int enif_ioq_enq_binary(ErlNifIOQueue *q, ErlNifBinary *bin, size_t skip);`
    pub enif_ioq_enq_binary:
        Option<unsafe extern "C" fn(*mut ErlNifIOQueue, *mut ErlNifBinary, size_t) -> c_int>,
    /// `int enif_ioq_enqv(ErlNifIOQueue *q, ErlNifIOVec *iov, size_t skip);`
    pub enif_ioq_enqv:
        Option<unsafe extern "C" fn(*mut ErlNifIOQueue, *mut ErlNifIOVec, size_t) -> c_int>,
    /// `size_t enif_ioq_size(ErlNifIOQueue *q);`
    pub enif_ioq_size: Option<unsafe extern "C" fn(*mut ErlNifIOQueue) -> size_t>,
    /// `int enif_ioq_deq(ErlNifIOQueue *q, size_t count, size_t *size);`
    pub enif_ioq_deq:
        Option<unsafe extern "C" fn(*mut ErlNifIOQueue, size_t, *mut size_t) -> c_int>,
    /// `SysIOVec* enif_ioq_peek(ErlNifIOQueue *q, int *iovlen);`
    pub enif_ioq_peek:
        Option<unsafe extern "C" fn(*mut ErlNifIOQueue, *mut c_int) -> *mut SysIOVec>,
    /// `int enif_inspect_iovec(ErlNifEnv *env, size_t max_length, ERL_NIF_TERM iovec_term, ERL_NIF_TERM *tail, ErlNifIOVec **iovec);`
    pub enif_inspect_iovec: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            size_t,
            ERL_NIF_TERM,
            *mut ERL_NIF_TERM,
            *mut *mut ErlNifIOVec,
        ) -> c_int,
    >,
    /// `void enif_free_iovec(ErlNifIOVec *iov);`
    pub enif_free_iovec: Option<unsafe extern "C" fn(*mut ErlNifIOVec)>,
    /// `int enif_ioq_peek_head(ErlNifEnv *env, ErlNifIOQueue *q, size_t *size, ERL_NIF_TERM *head);`
    pub enif_ioq_peek_head: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *mut ErlNifIOQueue, *mut size_t, *mut ERL_NIF_TERM) -> c_int,
    >,
    /// `char* enif_mutex_name(ErlNifMutex*);`
    pub enif_mutex_name: Option<unsafe extern "C" fn(*mut ErlNifMutex) -> *mut c_char>,
    /// `char* enif_cond_name(ErlNifCond*);`
    pub enif_cond_name: Option<unsafe extern "C" fn(*mut ErlNifCond) -> *mut c_char>,
    /// `char* enif_rwlock_name(ErlNifRWLock*);`
    pub enif_rwlock_name: Option<unsafe extern "C" fn(*mut ErlNifRWLock) -> *mut c_char>,
    /// `char* enif_thread_name(ErlNifTid);`
    pub enif_thread_name: Option<unsafe extern "C" fn(ErlNifTid) -> *mut c_char>,
    /// `int enif_vfprintf(FILE*, const char *fmt, va_list);`
    ///
    /// The `va_list` parameter is modelled as `*mut VaListTag` (see [`VaListTag`]).
    pub enif_vfprintf:
        Option<unsafe extern "C" fn(*mut FILE, *const c_char, *mut VaListTag) -> c_int>,
    /// `int enif_vsnprintf(char*, size_t, const char *fmt, va_list);`
    ///
    /// The `va_list` parameter is modelled as `*mut VaListTag` (see [`VaListTag`]).
    pub enif_vsnprintf:
        Option<unsafe extern "C" fn(*mut c_char, size_t, *const c_char, *mut VaListTag) -> c_int>,
    /// `int enif_make_map_from_arrays(ErlNifEnv *env, ERL_NIF_TERM keys[], ERL_NIF_TERM values[], size_t cnt, ERL_NIF_TERM *map_out);`
    pub enif_make_map_from_arrays: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            *mut ERL_NIF_TERM,
            *mut ERL_NIF_TERM,
            size_t,
            *mut ERL_NIF_TERM,
        ) -> c_int,
    >,
    /// `int enif_select_x(ErlNifEnv* env, ErlNifEvent e, enum ErlNifSelectFlags flags, void* obj, const ErlNifPid* pid, ERL_NIF_TERM msg, ErlNifEnv* msg_env);`
    pub enif_select_x: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            ErlNifEvent,
            ErlNifSelectFlags,
            *mut c_void,
            *const ErlNifPid,
            ERL_NIF_TERM,
            *mut ErlNifEnv,
        ) -> c_int,
    >,
    /// `ERL_NIF_TERM enif_make_monitor_term(ErlNifEnv* env, const ErlNifMonitor*);`
    pub enif_make_monitor_term:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, *const ErlNifMonitor) -> ERL_NIF_TERM>,
    /// `void enif_set_pid_undefined(ErlNifPid* pid);`
    pub enif_set_pid_undefined: Option<unsafe extern "C" fn(*mut ErlNifPid)>,
    /// `int enif_is_pid_undefined(const ErlNifPid* pid);`
    pub enif_is_pid_undefined: Option<unsafe extern "C" fn(*const ErlNifPid) -> c_int>,
    /// `ErlNifTermType enif_term_type(ErlNifEnv* env, ERL_NIF_TERM term);`
    pub enif_term_type:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM) -> ErlNifTermType>,
    /// `ErlNifResourceType* enif_init_resource_type(ErlNifEnv*, const char* name_str, const ErlNifResourceTypeInit*, ErlNifResourceFlags flags, ErlNifResourceFlags* tried);`
    pub enif_init_resource_type: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            *const c_char,
            *const ErlNifResourceTypeInit,
            ErlNifResourceFlags,
            *mut ErlNifResourceFlags,
        ) -> *mut ErlNifResourceType,
    >,
    /// `int enif_dynamic_resource_call(ErlNifEnv*, ERL_NIF_TERM mod, ERL_NIF_TERM name, ERL_NIF_TERM rsrc, void* call_data);`
    pub enif_dynamic_resource_call: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, ERL_NIF_TERM, ERL_NIF_TERM, *mut c_void) -> c_int,
    >,
    /// `int enif_get_string_length(ErlNifEnv *env, ERL_NIF_TERM list, unsigned *len, ErlNifCharEncoding encoding);`
    pub enif_get_string_length: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_uint, ErlNifCharEncoding) -> c_int,
    >,
    /// `int enif_make_new_atom(ErlNifEnv *env, const char *name, ERL_NIF_TERM *atom, ErlNifCharEncoding encoding);`
    pub enif_make_new_atom: Option<
        unsafe extern "C" fn(*mut ErlNifEnv, *const c_char, *mut ERL_NIF_TERM, ErlNifCharEncoding) -> c_int,
    >,
    /// `int enif_make_new_atom_len(ErlNifEnv *env, const char *name, size_t len, ERL_NIF_TERM *atom, ErlNifCharEncoding encoding);`
    pub enif_make_new_atom_len: Option<
        unsafe extern "C" fn(
            *mut ErlNifEnv,
            *const c_char,
            size_t,
            *mut ERL_NIF_TERM,
            ErlNifCharEncoding,
        ) -> c_int,
    >,
    /// `int enif_set_option(ErlNifEnv *env, ErlNifOption opt, ...);` — variadic.
    pub enif_set_option:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ErlNifOption, ...) -> c_int>,
    /// `size_t enif_term_size(ERL_NIF_TERM term);`
    pub enif_term_size: Option<unsafe extern "C" fn(ERL_NIF_TERM) -> size_t>,
    /// `int enif_get_atom_cache_index(ErlNifEnv *env, ERL_NIF_TERM atom, unsigned *index);`
    pub enif_get_atom_cache_index:
        Option<unsafe extern "C" fn(*mut ErlNifEnv, ERL_NIF_TERM, *mut c_uint) -> c_int>,
    /// `unsigned enif_max_atom_cache_index(void);`
    pub enif_max_atom_cache_index: Option<unsafe extern "C" fn() -> c_uint>,
}

impl ErlNifApi {
    /// An all-`None` table — every function pointer null.
    ///
    /// Used by tests and by a runtime that has not yet wired up any function; the returned value is
    /// a valid `ErlNifApi` whose `size_of`/alignment match a fully populated one.
    #[allow(clippy::too_many_lines)]
    pub const fn empty() -> Self {
        Self {
            enif_priv_data: None,
            enif_alloc: None,
            enif_free: None,
            enif_is_atom: None,
            enif_is_binary: None,
            enif_is_ref: None,
            enif_inspect_binary: None,
            enif_alloc_binary: None,
            enif_realloc_binary: None,
            enif_release_binary: None,
            enif_get_int: None,
            enif_get_ulong: None,
            enif_get_double: None,
            enif_get_list_cell: None,
            enif_get_tuple: None,
            enif_is_identical: None,
            enif_compare: None,
            enif_make_binary: None,
            enif_make_badarg: None,
            enif_make_int: None,
            enif_make_ulong: None,
            enif_make_double: None,
            enif_make_atom: None,
            enif_make_existing_atom: None,
            enif_make_tuple: None,
            enif_make_list: None,
            enif_make_list_cell: None,
            enif_make_string: None,
            enif_make_ref: None,
            enif_mutex_create: None,
            enif_mutex_destroy: None,
            enif_mutex_trylock: None,
            enif_mutex_lock: None,
            enif_mutex_unlock: None,
            enif_cond_create: None,
            enif_cond_destroy: None,
            enif_cond_signal: None,
            enif_cond_broadcast: None,
            enif_cond_wait: None,
            enif_rwlock_create: None,
            enif_rwlock_destroy: None,
            enif_rwlock_tryrlock: None,
            enif_rwlock_rlock: None,
            enif_rwlock_runlock: None,
            enif_rwlock_tryrwlock: None,
            enif_rwlock_rwlock: None,
            enif_rwlock_rwunlock: None,
            enif_tsd_key_create: None,
            enif_tsd_key_destroy: None,
            enif_tsd_set: None,
            enif_tsd_get: None,
            enif_thread_opts_create: None,
            enif_thread_opts_destroy: None,
            enif_thread_create: None,
            enif_thread_self: None,
            enif_equal_tids: None,
            enif_thread_exit: None,
            enif_thread_join: None,
            enif_realloc: None,
            enif_system_info: None,
            enif_fprintf: None,
            enif_inspect_iolist_as_binary: None,
            enif_make_sub_binary: None,
            enif_get_string: None,
            enif_get_atom: None,
            enif_is_fun: None,
            enif_is_pid: None,
            enif_is_port: None,
            enif_get_uint: None,
            enif_get_long: None,
            enif_make_uint: None,
            enif_make_long: None,
            enif_make_tuple_from_array: None,
            enif_make_list_from_array: None,
            enif_is_empty_list: None,
            enif_open_resource_type: None,
            enif_alloc_resource: None,
            enif_release_resource: None,
            enif_make_resource: None,
            enif_get_resource: None,
            enif_sizeof_resource: None,
            enif_make_new_binary: None,
            enif_is_list: None,
            enif_is_tuple: None,
            enif_get_atom_length: None,
            enif_get_list_length: None,
            enif_make_atom_len: None,
            enif_make_existing_atom_len: None,
            enif_make_string_len: None,
            enif_alloc_env: None,
            enif_free_env: None,
            enif_clear_env: None,
            enif_send: None,
            enif_make_copy: None,
            enif_self: None,
            enif_get_local_pid: None,
            enif_keep_resource: None,
            enif_make_resource_binary: None,
            enif_is_exception: None,
            enif_make_reverse_list: None,
            enif_is_number: None,
            enif_dlopen: None,
            enif_dlsym: None,
            enif_consume_timeslice: None,
            enif_is_map: None,
            enif_get_map_size: None,
            enif_make_new_map: None,
            enif_make_map_put: None,
            enif_get_map_value: None,
            enif_make_map_update: None,
            enif_make_map_remove: None,
            enif_map_iterator_create: None,
            enif_map_iterator_destroy: None,
            enif_map_iterator_is_head: None,
            enif_map_iterator_is_tail: None,
            enif_map_iterator_next: None,
            enif_map_iterator_prev: None,
            enif_map_iterator_get_pair: None,
            enif_schedule_nif: None,
            enif_has_pending_exception: None,
            enif_raise_exception: None,
            enif_getenv: None,
            enif_monotonic_time: None,
            enif_time_offset: None,
            enif_convert_time_unit: None,
            enif_now_time: None,
            enif_cpu_time: None,
            enif_make_unique_integer: None,
            enif_is_current_process_alive: None,
            enif_is_process_alive: None,
            enif_is_port_alive: None,
            enif_get_local_port: None,
            enif_term_to_binary: None,
            enif_binary_to_term: None,
            enif_port_command: None,
            enif_thread_type: None,
            enif_snprintf: None,
            enif_select: None,
            enif_open_resource_type_x: None,
            enif_monitor_process: None,
            enif_demonitor_process: None,
            enif_compare_monitors: None,
            enif_hash: None,
            enif_whereis_pid: None,
            enif_whereis_port: None,
            enif_ioq_create: None,
            enif_ioq_destroy: None,
            enif_ioq_enq_binary: None,
            enif_ioq_enqv: None,
            enif_ioq_size: None,
            enif_ioq_deq: None,
            enif_ioq_peek: None,
            enif_inspect_iovec: None,
            enif_free_iovec: None,
            enif_ioq_peek_head: None,
            enif_mutex_name: None,
            enif_cond_name: None,
            enif_rwlock_name: None,
            enif_thread_name: None,
            enif_vfprintf: None,
            enif_vsnprintf: None,
            enif_make_map_from_arrays: None,
            enif_select_x: None,
            enif_make_monitor_term: None,
            enif_set_pid_undefined: None,
            enif_is_pid_undefined: None,
            enif_term_type: None,
            enif_init_resource_type: None,
            enif_dynamic_resource_call: None,
            enif_get_string_length: None,
            enif_make_new_atom: None,
            enif_make_new_atom_len: None,
            enif_set_option: None,
            enif_term_size: None,
            enif_get_atom_cache_index: None,
            enif_max_atom_cache_index: None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Compile-time layout freeze (LP64)
// ---------------------------------------------------------------------------------------------

/// The exact number of function-pointer fields in [`ErlNifApi`], i.e. the number of `enif_*`
/// entries of `erl_nif_api_funcs.h`'s declaration list that exist on LP64 (180 declaration entries
/// minus the four `#if SIZEOF_LONG != 8` `enif_*64` entries; the macro-only `enif_ioq_enq` is
/// excluded).
///
/// Exposed so dependent code and the sibling `tests/api_surface.rs` can assert the table shape
/// without re-deriving it.
pub const ERL_NIF_API_FIELD_COUNT: usize = 176;

const _: () = {
    // Every column is one pointer, so the table is exactly N machine words on LP64.
    assert!(
        core::mem::size_of::<ErlNifApi>()
            == ERL_NIF_API_FIELD_COUNT * core::mem::size_of::<usize>()
    );
    assert!(core::mem::size_of::<ErlNifApi>() == 176 * 8);
    assert!(core::mem::size_of::<ErlNifApi>() == 1408);
    assert!(core::mem::align_of::<ErlNifApi>() == 8);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_shape_matches_the_field_count() {
        assert_eq!(ERL_NIF_API_FIELD_COUNT, 176);
        assert_eq!(
            core::mem::size_of::<ErlNifApi>(),
            ERL_NIF_API_FIELD_COUNT * core::mem::size_of::<usize>()
        );
        assert_eq!(core::mem::size_of::<ErlNifApi>(), 1408);
        assert_eq!(core::mem::align_of::<ErlNifApi>(), 8);
    }

    #[test]
    fn empty_table_is_all_none() {
        let api = ErlNifApi::empty();
        // Copy + Clone round-trip (the table is a plain aggregate of pointers).
        let copy = api;
        assert!(copy.enif_priv_data.is_none());
        assert!(copy.enif_alloc.is_none());
        assert!(copy.enif_make_tuple.is_none());
        assert!(copy.enif_send.is_none());
        assert!(copy.enif_select.is_none());
        assert!(copy.enif_set_option.is_none());
        assert!(copy.enif_max_atom_cache_index.is_none());
        assert_eq!(core::mem::size_of_val(&api), 1408);
    }

    #[test]
    fn first_and_last_fields_are_the_header_anchors() {
        // The header's first declaration entry is `enif_priv_data` and its last is
        // `enif_max_atom_cache_index`; a reordering or truncation of the table would move them (the
        // field order IS the freeze).
        let api = ErlNifApi::empty();
        assert_eq!(
            core::mem::offset_of!(ErlNifApi, enif_priv_data),
            0
        );
        assert_eq!(
            core::mem::offset_of!(ErlNifApi, enif_max_atom_cache_index),
            175 * core::mem::size_of::<usize>()
        );
        assert!(api.enif_priv_data.is_none());
    }
}
