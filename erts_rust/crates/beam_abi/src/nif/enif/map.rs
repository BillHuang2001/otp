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

//! `enif_*` — the map accessors and constructors.
//!
//! Owned slots: `enif_get_map_size`, `enif_get_map_value`, `enif_make_new_map`,
//! `enif_make_map_put`, `enif_make_map_update`, `enif_make_map_remove`,
//! `enif_make_map_from_arrays`.
//!
//! These APIs take no `ErlNifEnv` (the C does not pass one either), so no host and no atom table are
//! available; they run against a fresh empty `AtomTable`. They are exact whenever the key terms are
//! not atoms — the overwhelmingly common NIF case, where the key is a term the NIF was handed.
//! `enif_map_iterator_*` needs a dynamic work stack and lives in [`super::mapiter`] (not yet
//! implemented).

use super::*;

use libc::{c_int, size_t};

/// Populate the `ErlNifApi` slots this file owns (see the module docs).
pub(super) const fn populate(mut api: ErlNifApi) -> ErlNifApi {
    api.enif_get_map_size = Some(enif_get_map_size);
    api.enif_get_map_value = Some(enif_get_map_value);
    api.enif_make_new_map = Some(enif_make_new_map);
    api.enif_make_map_from_arrays = Some(enif_make_map_from_arrays);
    api.enif_make_map_put = Some(enif_make_map_put);
    api.enif_make_map_update = Some(enif_make_map_update);
    api.enif_make_map_remove = Some(enif_make_map_remove);
    api
}

/// `int enif_get_map_size(ErlNifEnv *env, ERL_NIF_TERM term, size_t *size);`
///
/// # Safety
///
/// `size` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_map_size(
    _env: *mut crate::nif::types::ErlNifEnv,
    term: ERL_NIF_TERM,
    size: *mut size_t,
) -> c_int {
    if !is_term_word(t(term)) {
        return 0;
    }
    match prelude::map_size(t(term), &no_atoms()) {
        // SAFETY: `size` may be null (handled) or valid for one write.
        Ok(n) => (unsafe { write_out(size, n) }) as c_int,
        Err(_) => 0,
    }
}

/// `int enif_get_map_value(ErlNifEnv *env, ERL_NIF_TERM map, ERL_NIF_TERM key, ERL_NIF_TERM* value);`
///
/// # Safety
///
/// `value` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_get_map_value(
    _env: *mut crate::nif::types::ErlNifEnv,
    map: ERL_NIF_TERM,
    key: ERL_NIF_TERM,
    value: *mut ERL_NIF_TERM,
) -> c_int {
    match prelude::map_get(t(map), t(key), &no_atoms()) {
        Ok(Some(v)) => {
            // SAFETY: `value` may be null (handled) or valid for one write.
            (unsafe { write_out(value, n(v)) }) as c_int
        }
        _ => 0,
    }
}
// ---------------------------------------------------------------------------------------------
// Maps
// ---------------------------------------------------------------------------------------------

/// `ERL_NIF_TERM enif_make_new_map(ErlNifEnv* env);`
///
/// # Safety
///
/// `env` must be a live env.
#[no_mangle]
pub unsafe extern "C" fn enif_make_new_map(env: *mut crate::nif::types::ErlNifEnv) -> ERL_NIF_TERM {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return bare_badarg();
    };
    // SAFETY: `flatmap_size_words(0)` sizes the empty flat map.
    let Some(hp) = (unsafe { alloc(e, prelude::flatmap_size_words(0)) }) else {
        return bare_badarg();
    };
    // SAFETY: `hp` heads the reserved block; an empty pair list leaves it fully written.
    n(unsafe { prelude::write_flatmap(hp, &[]) })
}

/// `int enif_make_map_put(ErlNifEnv* env, ERL_NIF_TERM map_in, ERL_NIF_TERM key, ERL_NIF_TERM value, ERL_NIF_TERM* map_out);`
///
/// # Safety
///
/// `env` must be a live env; `map_out` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_map_put(
    env: *mut crate::nif::types::ErlNifEnv,
    map_in: ERL_NIF_TERM,
    key: ERL_NIF_TERM,
    value: ERL_NIF_TERM,
    map_out: *mut ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let map = t(map_in);
    let Ok(size) = prelude::map_size(map, &no_atoms()) else {
        return 0;
    };
    // SAFETY: `map_words_capacity(size + 1)` sizes the result of a put.
    let Some(hp) = (unsafe { alloc(e, prelude::map_words_capacity(size + 1)) }) else {
        return 0;
    };
    match unsafe { prelude::map_put(hp, map, t(key), t(value), &no_atoms()) } {
        // SAFETY: `map_out` may be null (handled) or valid for one write.
        Ok(out) => (unsafe { write_out(map_out, n(out)) }) as c_int,
        Err(_) => 0,
    }
}

/// `int enif_make_map_update(ErlNifEnv* env, ERL_NIF_TERM map_in, ERL_NIF_TERM key, ERL_NIF_TERM value, ERL_NIF_TERM* map_out);`
///
/// Fails (`0`) when `key` is absent, exactly as the C does.
///
/// # Safety
///
/// `env` must be a live env; `map_out` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_map_update(
    env: *mut crate::nif::types::ErlNifEnv,
    map_in: ERL_NIF_TERM,
    key: ERL_NIF_TERM,
    value: ERL_NIF_TERM,
    map_out: *mut ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let map = t(map_in);
    let Ok(Some(_)) = prelude::map_get(map, t(key), &no_atoms()) else {
        return 0;
    };
    let Ok(size) = prelude::map_size(map, &no_atoms()) else {
        return 0;
    };
    // SAFETY: `map_words_capacity(size + 1)` sizes the result of a put.
    let Some(hp) = (unsafe { alloc(e, prelude::map_words_capacity(size + 1)) }) else {
        return 0;
    };
    match unsafe { prelude::map_put(hp, map, t(key), t(value), &no_atoms()) } {
        // SAFETY: `map_out` may be null (handled) or valid for one write.
        Ok(out) => (unsafe { write_out(map_out, n(out)) }) as c_int,
        Err(_) => 0,
    }
}

/// `int enif_make_map_remove(ErlNifEnv* env, ERL_NIF_TERM map_in, ERL_NIF_TERM key, ERL_NIF_TERM* map_out);`
///
/// Returns `0` **only** when `map_in` is not a map; for any map it returns `1`, leaving `map_out`
/// equal to `map_in` when `key` is absent. This is the C's behaviour (`erl_nif.c:3759`): it ignores
/// `erts_maps_take`'s found/absent result and returns `1` unconditionally after the `is_map` guard.
///
/// # Safety
///
/// `env` must be a live env; `map_out` must be null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_map_remove(
    env: *mut crate::nif::types::ErlNifEnv,
    map_in: ERL_NIF_TERM,
    key: ERL_NIF_TERM,
    map_out: *mut ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    let map = t(map_in);
    let Ok(size) = prelude::map_size(map, &no_atoms()) else {
        return 0;
    };
    // SAFETY: `map_words_capacity(size)` sizes the result of a remove — the pair count only shrinks.
    let Some(hp) = (unsafe { alloc(e, prelude::map_words_capacity(size)) }) else {
        return 0;
    };
    match unsafe { prelude::map_remove(hp, map, t(key), &no_atoms()) } {
        // SAFETY: `map_out` may be null (handled) or valid for one write.
        Ok(out) => (unsafe { write_out(map_out, n(out)) }) as c_int,
        Err(_) => 0,
    }
}

/// `int enif_make_map_from_arrays(ErlNifEnv *env, ERL_NIF_TERM keys[], ERL_NIF_TERM values[], size_t cnt, ERL_NIF_TERM *map_out);`
///
/// # Safety
///
/// `env` must be a live env; `keys`/`values` must point at `cnt` readable terms; `map_out` must be
/// null or writable.
#[no_mangle]
pub unsafe extern "C" fn enif_make_map_from_arrays(
    env: *mut crate::nif::types::ErlNifEnv,
    keys: *mut ERL_NIF_TERM,
    values: *mut ERL_NIF_TERM,
    cnt: size_t,
    map_out: *mut ERL_NIF_TERM,
) -> c_int {
    let Some(e) = (unsafe { env_mut(env) }) else {
        return 0;
    };
    if cnt > 0 && (keys.is_null() || values.is_null()) {
        return 0;
    }
    let mut pairs: Vec<(Eterm, Eterm)> = Vec::with_capacity(cnt);
    for i in 0..cnt {
        // SAFETY: the caller declares `cnt` readable terms in each array.
        let (k, v) = unsafe { (*keys.add(i), *values.add(i)) };
        pairs.push((t(k), t(v)));
    }
    // SAFETY: `map_words_capacity(pairs.len())` sizes the map built from deduplicated pairs.
    let Some(hp) = (unsafe { alloc(e, prelude::map_words_capacity(pairs.len())) }) else {
        return 0;
    };
    match unsafe { prelude::write_map(hp, &pairs, &no_atoms()) } {
        // SAFETY: `map_out` may be null (handled) or valid for one write.
        Ok(map) => (unsafe { write_out(map_out, n(map)) }) as c_int,
        Err(_) => 0,
    }
}
