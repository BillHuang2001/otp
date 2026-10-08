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

//! `enif_*` — the exception slot of an env.
//!
//! Owned slots: `enif_make_badarg`, `enif_raise_exception`, `enif_has_pending_exception`,
//! `enif_clear_env`.
//!
//! The pending exception lives on [`NifEnv`] (the C runtime's `enif_environment_t` exception slot);
//! a raise records the reason there and the loader turns a pending exception into
//! `NifCallError::Raised`.

use super::*;

use libc::c_int;

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_make_badarg = Some(enif_make_badarg);
    api.enif_raise_exception = Some(enif_raise_exception);
    api.enif_has_pending_exception = Some(enif_has_pending_exception);
    api.enif_clear_env = Some(enif_clear_env);
    api
}

// ---------------------------------------------------------------------------------------------
// Exceptions / misc
// ---------------------------------------------------------------------------------------------

/// `ERL_NIF_TERM enif_make_badarg(ErlNifEnv* env);`
///
/// Raises `badarg` and returns the `badarg` atom.
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_make_badarg(env: *mut crate::nif::types::ErlNifEnv) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    let reason = badarg_atom(e);
    e.set_exception(reason);
    n(reason)
}

/// `ERL_NIF_TERM enif_raise_exception(ErlNifEnv *env, ERL_NIF_TERM reason);`
///
/// Records `reason` as the pending exception and returns it.
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_raise_exception(
    env: *mut crate::nif::types::ErlNifEnv,
    reason: ERL_NIF_TERM,
) -> ERL_NIF_TERM {
    if let Some(e) = unsafe { env_mut(env) } {
        e.set_exception(t(reason));
    }
    reason
}

/// `int enif_has_pending_exception(ErlNifEnv *env, ERL_NIF_TERM* reason);`
///
/// # Safety
///
/// `env` must be null or a live env; `reason` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_has_pending_exception(
    env: *mut crate::nif::types::ErlNifEnv,
    reason: *mut ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    match e.pending_exception() {
        Some(pending) => {
            if !reason.is_null() {
                // SAFETY: `reason` is non-null and writable (checked).
                unsafe { reason.write(n(pending)) };
            }
            1
        }
        None => 0,
    }
}

/// `void enif_clear_env(ErlNifEnv* env);`
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_clear_env(env: *mut crate::nif::types::ErlNifEnv) {
    if let Some(e) = unsafe { env_mut(env) } {
        e.clear();
    }
}
