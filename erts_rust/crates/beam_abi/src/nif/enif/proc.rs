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

//! `enif_*` — pid/port/self accessors and the atom-cache queries.
//!
//! Owned slots: `enif_self`, `enif_get_local_pid`, `enif_get_local_port`,
//! `enif_set_pid_undefined`, `enif_is_pid_undefined`, `enif_get_atom_cache_index`,
//! `enif_max_atom_cache_index`.

use super::*;

use libc::{c_int, c_uint};

use crate::nif::types::{ErlNifPid, ErlNifPort};

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_self = Some(enif_self);
    api.enif_get_local_pid = Some(enif_get_local_pid);
    api.enif_get_local_port = Some(enif_get_local_port);
    api.enif_set_pid_undefined = Some(enif_set_pid_undefined);
    api.enif_is_pid_undefined = Some(enif_is_pid_undefined);
    api.enif_get_atom_cache_index = Some(enif_get_atom_cache_index);
    api.enif_max_atom_cache_index = Some(enif_max_atom_cache_index);
    api
}

// ---------------------------------------------------------------------------------------------
// Pid / port / self accessors and the atom-cache queries
// ---------------------------------------------------------------------------------------------

/// `int enif_get_local_pid(ErlNifEnv* env, ERL_NIF_TERM, ErlNifPid* pid);`
///
/// # Safety
///
/// `pid` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_local_pid(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    pid: *mut ErlNifPid,
) -> c_int {
    let term = t(term);
    if !prelude::is_internal_pid(term) {
        return 0;
    }
    // SAFETY: `pid` may be null (handled) or valid for one write.
    (unsafe { write_out(pid, ErlNifPid { pid: n(term) }) }) as c_int
}

/// `int enif_get_local_port(ErlNifEnv* env, ERL_NIF_TERM, ErlNifPort* port_id);`
///
/// # Safety
///
/// `port_id` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_local_port(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    port_id: *mut ErlNifPort,
) -> c_int {
    let term = t(term);
    if !prelude::is_internal_port(term) {
        return 0;
    }
    // SAFETY: `port_id` may be null (handled) or valid for one write.
    (unsafe { write_out(port_id, ErlNifPort { port_id: n(term) }) }) as c_int
}

/// `ErlNifPid* enif_self(ErlNifEnv* caller_env, ErlNifPid* pid);`
///
/// Returns `pid` on success, `NULL` when the caller has no process (`NifHost::self_pid` = `None`).
///
/// # Safety
///
/// `env` must be a live env; `pid` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_self(
    env: *mut crate::nif::types::ErlNifEnv,
    pid: *mut ErlNifPid,
) -> *mut ErlNifPid {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return core::ptr::null_mut();
    };
    if pid.is_null() {
        return core::ptr::null_mut();
    }
    let Some(self_pid) = e.host().self_pid() else {
        return core::ptr::null_mut();
    };
    // SAFETY: `pid` is non-null and writable (checked).
    unsafe { pid.write(ErlNifPid { pid: n(self_pid) }) };
    pid
}

/// `void enif_set_pid_undefined(ErlNifPid* pid);`
///
/// # Safety
///
/// `pid` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_set_pid_undefined(pid: *mut ErlNifPid) {
    // SAFETY: `pid` may be null (handled) or valid for one write.
    unsafe { write_out(pid, ErlNifPid { pid: UNDEFINED_PID }) };
}

/// `int enif_is_pid_undefined(const ErlNifPid* pid);`
///
/// # Safety
///
/// `pid` must be null or point at a readable `ErlNifPid`.
#[no_mangle]
pub unsafe extern "C" fn enif_is_pid_undefined(pid: *const ErlNifPid) -> c_int {
    if pid.is_null() {
        return 0;
    }
    // SAFETY: `pid` is non-null and readable (caller contract).
    (unsafe { (*pid).pid.raw() } == UNDEFINED_PID.raw()) as c_int
}

/// `int enif_get_atom_cache_index(ErlNifEnv *env, ERL_NIF_TERM atom, unsigned *index);`
///
/// This runtime exposes no cached atom indices, so no atom has one (always `0`).
///
/// # Safety
///
/// `index` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_atom_cache_index(
    _env: *mut crate::nif::types::ErlNifEnv,
    _atom: ERL_NIF_TERM,
    _index: *mut c_uint,
) -> c_int {
    0
}

/// `unsigned enif_max_atom_cache_index(void);` — no cached indices exist (`0`).
///
/// # Safety
///
/// No pointers; always safe.
#[no_mangle]
pub unsafe extern "C" fn enif_max_atom_cache_index() -> c_uint {
    0
}
