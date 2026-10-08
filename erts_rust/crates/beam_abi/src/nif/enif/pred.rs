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

//! `enif_*` — the type predicates, comparison, hashing and sizing.
//!
//! Owned slots: `enif_is_atom`, `enif_is_binary`, `enif_is_ref`, `enif_is_fun`, `enif_is_pid`,
//! `enif_is_port`, `enif_is_empty_list`, `enif_is_list`, `enif_is_tuple`, `enif_is_map`,
//! `enif_is_number`, `enif_is_exception`, `enif_is_identical`, `enif_compare`, `enif_hash`,
//! `enif_term_type`, `enif_term_size`.
//!
//! [`enif_compare`], [`enif_is_identical`] and [`enif_hash`] take no env parameter, hence no host and
//! no atom-name table: bit-identical words take the raw-word fast path and distinct atoms fall back
//! to `beam_term`'s index ordering (exact for non-atom terms).

use super::*;

use libc::{c_int, size_t};

use crate::nif::types::{ErlNifHash, ErlNifTermType};

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_is_atom = Some(enif_is_atom);
    api.enif_is_binary = Some(enif_is_binary);
    api.enif_is_ref = Some(enif_is_ref);
    api.enif_is_fun = Some(enif_is_fun);
    api.enif_is_pid = Some(enif_is_pid);
    api.enif_is_port = Some(enif_is_port);
    api.enif_is_empty_list = Some(enif_is_empty_list);
    api.enif_is_list = Some(enif_is_list);
    api.enif_is_tuple = Some(enif_is_tuple);
    api.enif_is_map = Some(enif_is_map);
    api.enif_is_number = Some(enif_is_number);
    api.enif_is_exception = Some(enif_is_exception);
    api.enif_is_identical = Some(enif_is_identical);
    api.enif_compare = Some(enif_compare);
    api.enif_hash = Some(enif_hash);
    api.enif_term_type = Some(enif_term_type);
    api.enif_term_size = Some(enif_term_size);
    api
}

// ---------------------------------------------------------------------------------------------
// Predicates
// ---------------------------------------------------------------------------------------------

/// `int enif_is_atom(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term; no pointer out-parameters.
#[no_mangle]
pub unsafe extern "C" fn enif_is_atom(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    prelude::is_atom(t(term)) as c_int
}

/// `int enif_is_binary(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_binary(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `is_binary` only dereferences the header of a boxed term.
    if !is_term_word(t(term)) {
        return 0;
    }
    (unsafe { prelude::is_binary(t(term)) }) as c_int
}

/// `int enif_is_ref(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_ref(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    is_ref(t(term)) as c_int
}

/// `int enif_is_fun(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_fun(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `is_any_fun` only dereferences the header of a boxed term.
    if !is_term_word(t(term)) {
        return 0;
    }
    (unsafe { prelude::is_any_fun(t(term)) }) as c_int
}

/// `int enif_is_pid(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_pid(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    is_pid(t(term)) as c_int
}

/// `int enif_is_port(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_port(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    is_port(t(term)) as c_int
}

/// `int enif_is_empty_list(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_empty_list(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    prelude::is_nil(t(term)) as c_int
}

/// `int enif_is_list(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// True for a cons cell **or** `[]` (the C `is_list(term) || is_nil(term)`).
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_list(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    let term = t(term);
    (prelude::is_list(term) || prelude::is_nil(term)) as c_int
}

/// `int enif_is_tuple(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_tuple(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `is_tuple` only dereferences the header of a boxed term.
    if !is_term_word(t(term)) {
        return 0;
    }
    (unsafe { prelude::is_tuple(t(term)) }) as c_int
}

/// `int enif_is_map(ErlNifEnv* env, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_map(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `is_map` only dereferences the header of a boxed term.
    if !is_term_word(t(term)) {
        return 0;
    }
    (unsafe { prelude::is_map(t(term)) }) as c_int
}

/// `int enif_is_number(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_is_number(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    // SAFETY: `is_number` only inspects tags/headers, never panics.
    if !is_term_word(t(term)) {
        return 0;
    }
    (unsafe { prelude::is_number(t(term)) }) as c_int
}

/// `int enif_is_exception(ErlNifEnv*, ERL_NIF_TERM term);`
///
/// # Safety
///
/// `env` must be null or a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_is_exception(
    env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    (e.pending_exception().is_some() && prelude::is_non_value(t(term))) as c_int
}

/// `int enif_is_identical(ERL_NIF_TERM lhs, ERL_NIF_TERM rhs);`
///
/// # Safety
///
/// Both arguments are any terms; no pointers.
#[no_mangle]
pub unsafe extern "C" fn enif_is_identical(lhs: ERL_NIF_TERM, rhs: ERL_NIF_TERM) -> c_int {
    let (a, b) = (t(lhs), t(rhs));
    if a.raw() == b.raw() {
        // The C `EQ` macro's fast path: bit-identical words are equal terms.
        return 1;
    }
    prelude::eq(a, b, &no_atoms()) as c_int
}

/// `int enif_compare(ERL_NIF_TERM lhs, ERL_NIF_TERM rhs);` — `-1`, `0` or `1`.
///
/// # Safety
///
/// Both arguments are any terms; no pointers.
#[no_mangle]
pub unsafe extern "C" fn enif_compare(lhs: ERL_NIF_TERM, rhs: ERL_NIF_TERM) -> c_int {
    let (a, b) = (t(lhs), t(rhs));
    if a.raw() == b.raw() {
        return 0;
    }
    match prelude::cmp(a, b, &no_atoms()) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

/// `ErlNifUInt64 enif_hash(ErlNifHash type, ERL_NIF_TERM term, ErlNifUInt64 salt);`
///
/// # Safety
///
/// `term` is any term; no pointers.
#[no_mangle]
pub unsafe extern "C" fn enif_hash(
    hash_type: ErlNifHash,
    term: ERL_NIF_TERM,
    salt: u64,
) -> u64 {
    match hash_type {
        // An implementation-defined internal hash; salted, like the C.
        ErlNifHash::InternalHash => prelude::term_hash(t(term), &no_atoms()) ^ salt,
        // `erlang:phash2/1` — the low 27 bits, unsalted (as the C ignores the salt here).
        ErlNifHash::Phash2 => prelude::term_hash(t(term), &no_atoms()) & ((1 << 27) - 1),
    }
}

/// `size_t enif_term_size(ERL_NIF_TERM term);` — the term's heap size in bytes.
///
/// # Safety
///
/// `term` is any term; no pointers.
#[no_mangle]
pub unsafe extern "C" fn enif_term_size(term: ERL_NIF_TERM) -> size_t {
    if !is_term_word(t(term)) {
        return 0;
    }
    prelude::size_object(t(term)) * core::mem::size_of::<Eterm>()
}

/// `ErlNifTermType enif_term_type(ErlNifEnv* env, ERL_NIF_TERM term);`
///
/// Total: it never aborts. `[]` reports `List` (consistent with [`enif_is_list`]) and a reference
/// reports `Reference`; the C's `default` arm is exactly `MissingDefaultCaseReadTheManual`.
///
/// # Safety
///
/// `term` is any term.
#[no_mangle]
pub unsafe extern "C" fn enif_term_type(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
) -> ErlNifTermType {
    let term = t(term);
    if !is_term_word(term) {
        return ErlNifTermType::MissingDefaultCaseReadTheManual;
    }
    if prelude::is_atom(term) {
        return ErlNifTermType::Atom;
    }
    if prelude::is_nil(term) {
        return ErlNifTermType::List;
    }
    if prelude::is_small(term) || unsafe { prelude::is_big(term) } {
        return ErlNifTermType::Integer;
    }
    // SAFETY: the remaining predicates only dereference the header of a boxed term (and each checks
    // `is_boxed` first), so they are total over arbitrary terms.
    unsafe {
        if prelude::is_float(term) {
            return ErlNifTermType::Float;
        }
        if prelude::is_bitstring(term) {
            return ErlNifTermType::Bitstring;
        }
        if prelude::is_list(term) {
            return ErlNifTermType::List;
        }
        if prelude::is_tuple(term) {
            return ErlNifTermType::Tuple;
        }
        if prelude::is_map(term) {
            return ErlNifTermType::Map;
        }
        if prelude::is_any_fun(term) {
            return ErlNifTermType::Fun;
        }
    }
    if prelude::is_internal_pid(term) || unsafe { prelude::is_external_pid(term) } {
        return ErlNifTermType::Pid;
    }
    if prelude::is_internal_port(term) || unsafe { prelude::is_external_port(term) } {
        return ErlNifTermType::Port;
    }
    if is_ref(term) {
        return ErlNifTermType::Reference;
    }
    ErlNifTermType::MissingDefaultCaseReadTheManual
}
