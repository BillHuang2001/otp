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
//!
//! `enif_make_map_put` / `enif_make_map_update` write through `beam_term`'s **incremental
//! (path-copying) writer** `map_put_cow`, the shape the C uses (`erts_maps_put` copies only the
//! nodes on `key`'s hash path and shares every off-path subtree by word; a flat map's present-key
//! replace reuses the source keys tuple verbatim). A single-key update therefore costs Θ(log n)
//! fresh words instead of the Θ(n) a full `write_map` rebuild copies — the reservation comes from
//! `map_put_cow_words_capacity(map_size)`, the writer's size-only bound. Two consequences, both the
//! C's: the result **shares storage with `map_in`** (sound — terms are immutable and never
//! relocated, so sharing a word is a copy-free copy), and a put whose stored value word is already
//! the new value word returns `map_in` itself, writing nothing. `map_out` and `map_in` may alias
//! (a NIF's `enif_make_map_put(env, M, K, V, &M)` idiom): `map_in` is read once, and every write
//! goes to the freshly reserved region. A malformed map is the writer's typed
//! `TermError::BadMap` refusal, answered as the C's `0` — nothing is written to `*map_out` then.
//!
//! `enif_make_map_remove` still takes the full-rebuild route (`map_remove`), whose result borrows
//! nothing from `map_in`.
//!
//! `enif_make_map_from_arrays` **refuses a duplicate key** (answering `0` with `THE_NON_VALUE` in
//! `*map_out`), exactly as `erts_map_from_ks_and_vs` does (`erl_map.c:671-714`); the duplicate basis
//! is the exact (`=:=`) key identity, so `1` and `1.0` — two different map keys — are accepted.
//!
//! `enif_map_iterator_*` needs a dynamic work stack and lives in [`super::mapiter`].

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
    // SAFETY: `map_put_cow` requires a region of `map_put_cow_words_capacity(size)` words
    // (its size-only bound — the words one put *copies*, not the whole map's extent: the
    // off-path subtrees are shared with `map_in` by word, exactly as `erts_maps_put`
    // shares them, which is sound because terms are immutable and never relocated).
    let Some(hp) = (unsafe { alloc(e, prelude::map_put_cow_words_capacity(size)) }) else {
        return 0;
    };
    match unsafe { prelude::map_put_cow(hp, map, t(key), t(value), &no_atoms()) } {
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
    // SAFETY: as `enif_make_map_put` — `map_put_cow`'s size-only capacity, the words the
    // put copies (a present key copies the value sequence / the HAMT path only).
    let Some(hp) = (unsafe { alloc(e, prelude::map_put_cow_words_capacity(size)) }) else {
        return 0;
    };
    match unsafe { prelude::map_put_cow(hp, map, t(key), t(value), &no_atoms()) } {
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

/// Whether any two keys among `pairs` are **identical** in the C's exact (`=:=`) sense.
///
/// This is the predicate `erts_map_from_ks_and_vs` (`erl_map.c:671-714`, "Returns THE_NON_VALUE if
/// duplicate keys found") refuses on, on **both** sides of the flat/hash threshold:
///
/// * the flat arm's `erts_validate_and_sort_flatmap` insertion sort fails the moment
///   `erts_cmp_flatmap_keys(ks[jx], ks[jx-1]) == 0` (`erl_map.c:3186-3216`), and
/// * the hash arm passes `reject_dupkeys = 1` and refuses on an `EQ`-equal key inside an
///   equal-hash region (`erl_map.c:764-800`).
///
/// `erts_cmp_flatmap_keys` is `key_a - key_b` for two atoms and the exact `erts_cmp` otherwise
/// (`erl_utils.h:267-272`), so the basis is [`prelude::eq`] (`=:=`) and **not** `==`: `1` and `1.0`
/// are distinct map keys and a pair of them is not a duplicate.
///
/// `write_map`'s own deduplication is a quadratic scan of the input as well, so scanning here costs
/// the same order as the build it guards and keeps the assertion independent of any hash detail.
fn has_duplicate_keys(pairs: &[(Eterm, Eterm)]) -> bool {
    let atoms = no_atoms();
    pairs.iter().enumerate().any(|(i, (key, _))| {
        pairs[i + 1..]
            .iter()
            .any(|(other, _)| prelude::eq(*key, *other, &atoms))
    })
}

/// `int enif_make_map_from_arrays(ErlNifEnv *env, ERL_NIF_TERM keys[], ERL_NIF_TERM values[], size_t cnt, ERL_NIF_TERM *map_out);`
///
/// Answers `0` — and stores [`Eterm::non_value`] (`THE_NON_VALUE`) in `*map_out`, exactly as
/// `erts/emulator/beam/erl_nif.c:3664-3697` does — when two of the `cnt` keys are identical. A
/// duplicate is **refused, never collapsed**: `erts_map_from_ks_and_vs` is documented "Returns
/// THE_NON_VALUE if duplicate keys found", so the runtime's `write_map` last-value-wins dedup must
/// not be reachable from this slot.
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
    // Duplicate keys are refused *before* anything is allocated, so a refused call leaves no
    // half-built map behind — the C's `erts_factory_undo` in one step.
    if has_duplicate_keys(&pairs) {
        // SAFETY: `map_out` may be null (handled) or valid for one write. The C writes
        // `THE_NON_VALUE` into `*map_out` on this path before answering 0 (`erl_nif.c:3688-3692`).
        unsafe { write_out(map_out, n(Eterm::non_value())) };
        return 0;
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

#[cfg(test)]
mod tests {
    //! Pin the **duplicate-key refusal** of `enif_make_map_from_arrays` (`src/nif/enif/map.rs`).
    //!
    //! `erts_map_from_ks_and_vs` (`erts/emulator/beam/erl_map.c:671-714`) is documented
    //! `/* Returns THE_NON_VALUE if duplicate keys found. */`, and `enif_make_map_from_arrays`
    //! (`erts/emulator/beam/erl_nif.c:3664-3697`) turns that into its `int` answer:
    //!
    //! ```c
    //! (*map_out) = erts_map_from_ks_and_vs(&factory, keys, values, cnt);
    //! succeeded  = (*map_out) != THE_NON_VALUE;
    //! ```
    //!
    //! so a duplicate-key call answers `0` **and leaves `THE_NON_VALUE` in `*map_out`**. The basis
    //! is the *exact* (`=:=`) key identity on **both** sides of the flat/hash threshold: the flat
    //! arm's `erts_validate_and_sort_flatmap` insertion sort fails as soon as
    //! `erts_cmp_flatmap_keys(..) == 0` (`erl_map.c:3186-3216`), and the hash arm passes
    //! `reject_dupkeys = 1` and refuses on an `EQ`-equal key inside an equal-hash region
    //! (`erl_map.c:764-800`). `1` and `1.0` are therefore **not** duplicates.
    //!
    //! Every case below starts from a sentinel in the out slot, so the C's "we do write
    //! `THE_NON_VALUE`" behaviour is observable rather than assumed.

    use super::*;

    use beam_alloc::Heap;
    use beam_term::atom::AtomTable;
    use beam_term::prelude::make_small;

    use crate::nif::host::NifHost;

    /// The minimal host the map constructors need: an arena plus an atom table (`badarg` interning).
    struct MapHost {
        heap: Heap,
        atoms: AtomTable,
    }

    impl MapHost {
        fn new() -> MapHost {
            MapHost {
                heap: Heap::new(4096).expect("a small test heap is allocatable"),
                atoms: AtomTable::new(),
            }
        }

        /// A host whose arena holds `words` words — the 250-pair cases carry a source map plus two
        /// writers' reservations: `map_put_cow_words_capacity` is `293` for a HAMT put, while the
        /// rebuild it is compared against reserves `map_words_capacity(size + 1)` = `5289`.
        fn with_heap(words: usize) -> MapHost {
            MapHost {
                heap: Heap::new(words).expect("a test heap is allocatable"),
                atoms: AtomTable::new(),
            }
        }
    }

    impl NifHost for MapHost {
        fn heap(&mut self) -> &mut Heap {
            &mut self.heap
        }

        fn intern_atom(&mut self, name: &str) -> Option<Eterm> {
            Some(prelude::make_atom(self.atoms.intern(name)))
        }

        fn atom_name(&mut self, atom: Eterm) -> Option<String> {
            if !prelude::is_atom(atom) {
                return None;
            }
            self.atoms.name(prelude::atom_val(atom)).map(str::to_string)
        }
    }

    /// The value pre-written into the out slot, so a refusal's overwrite is observable.
    const SENTINEL: i64 = -999;

    /// Call `enif_make_map_from_arrays` over `keys`/`values` from a sentinel-initialised out slot
    /// and return `(return code, out term)`.
    fn from_arrays(
        env: &NifEnv,
        keys: &[ERL_NIF_TERM],
        values: &[ERL_NIF_TERM],
    ) -> (c_int, Eterm) {
        assert_eq!(keys.len(), values.len(), "one value per key");
        let mut keys = keys.to_vec();
        let mut values = values.to_vec();
        let mut out = n(make_small(SENTINEL));
        // SAFETY: both arrays hold `keys.len()` live term words, and `out` is a live local.
        // `env` is live for the whole call, so the host heap it allocates from is alive.
        let code = unsafe {
            enif_make_map_from_arrays(
                env.as_ptr(),
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                keys.len(),
                &mut out,
            )
        };
        (code, t(out))
    }

    /// The map's key count through `enif_get_map_size`.
    #[track_caller]
    fn size_of(env: &NifEnv, map: Eterm) -> usize {
        let mut size: size_t = 0;
        // SAFETY: `map` is a live map term and `size` is a live local.
        let ok = unsafe { enif_get_map_size(env.as_ptr(), n(map), &mut size) };
        assert_eq!(ok, 1, "the out term is a map");
        size
    }

    /// The value bound to `key` through `enif_get_map_value`.
    #[track_caller]
    fn value_of(env: &NifEnv, map: Eterm, key: Eterm) -> Eterm {
        let mut value = ERL_NIF_TERM::default();
        // SAFETY: `map`/`key` are live terms and `value` is a live local.
        let ok = unsafe { enif_get_map_value(env.as_ptr(), n(map), n(key), &mut value) };
        assert_eq!(ok, 1, "the key is present");
        t(value)
    }

    /// The small-integer terms `lo..hi`, one per element.
    fn smalls(values: impl IntoIterator<Item = i64>) -> Vec<ERL_NIF_TERM> {
        values.into_iter().map(|v| n(make_small(v))).collect()
    }

    // ---------------------------------------------------------------------------------------
    // The no-duplicate path (must stay byte-identical to a plain `write_map`)
    // ---------------------------------------------------------------------------------------

    #[test]
    fn unique_keys_build_the_map_and_answer_one() {
        let mut host = MapHost::new();
        let env = NifEnv::new(&mut host);
        let (code, map) = from_arrays(&env, &smalls([1, 2]), &smalls([100, 200]));
        assert_eq!(code, 1, "two distinct keys are accepted");
        assert!(!prelude::is_non_value(map), "the out term is a real map");
        assert_eq!(size_of(&env, map), 2);
        assert_eq!(
            prelude::signed_value(value_of(&env, map, make_small(1))),
            100
        );
        assert_eq!(
            prelude::signed_value(value_of(&env, map, make_small(2))),
            200
        );
    }

    #[test]
    fn forty_unique_keys_build_the_hash_map_and_answer_one() {
        let mut host = MapHost::new();
        let env = NifEnv::new(&mut host);
        // 40 > MAP_SMALL_MAP_LIMIT: the hash arm, which must not refuse a legitimate map.
        let keys = smalls(0..40);
        let values = smalls((0..40).map(|k| k * 10));
        let (code, map) = from_arrays(&env, &keys, &values);
        assert_eq!(code, 1, "40 distinct keys are accepted");
        assert_eq!(size_of(&env, map), 40);
        assert_eq!(
            prelude::signed_value(value_of(&env, map, make_small(39))),
            390
        );
    }

    #[test]
    fn an_integer_and_a_float_of_the_same_value_are_distinct_keys() {
        let mut host = MapHost::new();
        let env = NifEnv::new(&mut host);
        // SAFETY: `env` is live; `enif_make_double` only allocates on its heap.
        let one_float = t(unsafe { enif_make_double(env.as_ptr(), 1.0) });
        let keys = [n(make_small(1)), n(one_float)];
        let (code, map) = from_arrays(&env, &keys, &smalls([100, 200]));
        assert_eq!(code, 1, "1 and 1.0 are different keys (`=:=`, not `==`)");
        assert_eq!(size_of(&env, map), 2);
        assert_eq!(
            prelude::signed_value(value_of(&env, map, make_small(1))),
            100
        );
        assert_eq!(
            prelude::signed_value(value_of(&env, map, one_float)),
            200
        );
    }

    // ---------------------------------------------------------------------------------------
    // The duplicate-key refusal (the C's `THE_NON_VALUE`)
    // ---------------------------------------------------------------------------------------

    #[track_caller]
    fn assert_refused(env: &NifEnv, keys: &[ERL_NIF_TERM], values: &[ERL_NIF_TERM]) {
        let (code, map) = from_arrays(env, keys, values);
        assert_eq!(code, 0, "a duplicate key is refused (the C answers 0)");
        assert_eq!(
            map,
            Eterm::non_value(),
            "the C writes THE_NON_VALUE into *map_out on a refusal (`erl_nif.c:3688-3692`)"
        );
    }

    #[test]
    fn duplicate_keys_in_the_flat_arm_are_refused() {
        let mut host = MapHost::new();
        let env = NifEnv::new(&mut host);
        assert_refused(&env, &smalls([7, 8, 7]), &smalls([10, 20, 30]));
    }

    #[test]
    fn duplicate_keys_in_the_hash_arm_are_refused() {
        let mut host = MapHost::new();
        let env = NifEnv::new(&mut host);
        // 34 pairs: 0..32 plus a second `5`, i.e. above MAP_SMALL_MAP_LIMIT so the hash arm's
        // `reject_dupkeys` does the refusing.
        let mut keys: Vec<i64> = (0..33).collect();
        keys.push(5);
        let values: Vec<i64> = (0..34).collect();
        assert_refused(&env, &smalls(keys), &smalls(values));
    }

    #[test]
    fn structurally_equal_keys_are_a_duplicate() {
        let mut host = MapHost::new();
        let env = NifEnv::new(&mut host);
        // Two tuples with equal contents but distinct heap addresses: `=:=`-equal, so the C's
        // `erts_cmp_flatmap_keys` says 0 and the call is refused.
        let pair = smalls([1, 2]);
        // SAFETY: `pair` holds two live terms; `enif_make_tuple_from_array` only allocates.
        let tuple_a = t(unsafe {
            enif_make_tuple_from_array(env.as_ptr(), pair.as_ptr(), pair.len() as libc::c_uint)
        });
        // SAFETY: as above.
        let tuple_b = t(unsafe {
            enif_make_tuple_from_array(env.as_ptr(), pair.as_ptr(), pair.len() as libc::c_uint)
        });
        assert_ne!(tuple_a.raw(), tuple_b.raw(), "two distinct blocks");
        assert_refused(&env, &[n(tuple_a), n(tuple_b)], &smalls([10, 20]));
    }

    #[test]
    fn a_refusal_with_a_null_out_slot_writes_nothing_and_still_answers_zero() {
        let mut host = MapHost::new();
        let env = NifEnv::new(&mut host);
        let mut keys = smalls([7, 7]);
        let mut values = smalls([10, 20]);
        // SAFETY: both arrays hold two live term words; `map_out` is null, which the entry point
        // documents as allowed (it then writes nothing).
        let code = unsafe {
            enif_make_map_from_arrays(
                env.as_ptr(),
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                2,
                core::ptr::null_mut(),
            )
        };
        assert_eq!(code, 0, "the refusal does not depend on the out slot");
    }

    // ---------------------------------------------------------------------------------------
    // The incremental (path-copying) writer behind `enif_make_map_put` / `enif_make_map_update`
    // ---------------------------------------------------------------------------------------
    //
    // Both slots write through `beam_term::map::map_put_cow` (the C's `erts_maps_put` shape: only
    // the nodes on `key`'s hash path are copied, every off-path subtree is shared by word) instead
    // of the Θ(n) `map_put` (a full `write_map` rebuild). The two must be **indistinguishable**:
    // the same `=:=` term, the same head word, and the same **interior** — the same pairs in the
    // same storage order (which is what `maps:to_list/1` and every image-preserving serializer
    // read) over the same node/bitmap arrangement.
    //
    // `beam_abi` deliberately has no `beam_codec` dependency (ETF goes through `NifHost`), so the
    // comparison below stands in for a `term_to_binary` byte comparison: `map_pairs_storage_order`
    // (the serializer's order) plus a structural walk of the boxed map/node/leaf/collision words,
    // with block *addresses* excluded — a path-copied node is fresh storage by design, and an
    // off-path subtree is shared by word.

    /// `k => k * 100` for `k` in `0..n`, built by `enif_make_map_from_arrays`, so the source map is
    /// built by the one-shot builder rather than by either writer under test.
    fn map_of(env: &NifEnv, n: i64) -> Eterm {
        let keys = smalls(0..n);
        let values = smalls((0..n).map(|k| k * 100));
        let (code, map) = from_arrays(env, &keys, &values);
        assert_eq!(code, 1, "the source map builds");
        map
    }

    /// The **rebuild** route both slots used to take: `map_put` = `map_pairs` + `write_map`.
    fn rebuild(env: &NifEnv, map: Eterm, key: Eterm, value: Eterm) -> Eterm {
        let size = prelude::map_size(map, &no_atoms()).expect("a well-formed map");
        // SAFETY: `map_words_capacity(size + 1)` is `map_put`'s documented reservation and `alloc`
        // hands out that many words of the host arena, which the live `env` keeps alive.
        let hp = unsafe { alloc(env, prelude::map_words_capacity(size + 1)) }.expect("heap room");
        // SAFETY: as above, and `map` is a well-formed map.
        unsafe { prelude::map_put(hp, map, key, value, &no_atoms()) }.expect("a well-formed map")
    }

    /// The head (first) word of a boxed term.
    ///
    /// # Safety
    ///
    /// `t` must be a live boxed term.
    unsafe fn head_word(t: Eterm) -> Eterm {
        // SAFETY: the caller guarantees `t` is boxed, so its first word is its header.
        unsafe { *prelude::boxed_val(t) }
    }

    /// The number of slots a HAMT node header carries: 16 for the full head array, else one per
    /// set bitmap bit.
    fn slot_count(hdr: Eterm) -> usize {
        if prelude::map_header_type(hdr) as usize == prelude::MAP_HEADER_TAG_HAMT_HEAD_ARRAY {
            16
        } else {
            prelude::map_header_val(hdr).count_ones() as usize
        }
    }

    /// Structural (`=:=` plus interior-shape) equality of two maps — see the section header for
    /// what it covers and why it stands in for a byte comparison.
    fn same_map(a: Eterm, b: Eterm) -> bool {
        let atoms = no_atoms();
        // SAFETY: both are live maps (every caller produced them from a writer that succeeded).
        let (ha, hb) = unsafe { (head_word(a), head_word(b)) };
        if ha.raw() != hb.raw() {
            return false;
        }
        if prelude::is_flatmap_header(ha) {
            // SAFETY: both are live flat maps.
            let (ka, va) = unsafe { prelude::flatmap_parts(a) };
            let (kb, vb) = unsafe { prelude::flatmap_parts(b) };
            return ka.len() == kb.len()
                && ka.iter().zip(kb).all(|(x, y)| prelude::eq(*x, *y, &atoms))
                && va.iter().zip(vb).all(|(x, y)| prelude::eq(*x, *y, &atoms));
        }
        prelude::is_map_header(ha) && same_node(a, b, true)
    }

    /// [`same_map`]'s node arm: a head (`is_head`, whose `size` word precedes the slots) or an
    /// inner node. Both sides must have the same header word (type + bitmap / full array).
    fn same_node(a: Eterm, b: Eterm, is_head: bool) -> bool {
        // SAFETY: both are live HAMT nodes of the same kind.
        let (ha, hb) = unsafe { (head_word(a), head_word(b)) };
        if ha.raw() != hb.raw() {
            return false;
        }
        let (pa, pb) = (prelude::boxed_val(a), prelude::boxed_val(b));
        let slots = if is_head { 2 } else { 1 };
        if is_head {
            // SAFETY: a head node's word 1 is its `size` (the map's key count).
            let (sa, sb) = unsafe { (*pa.add(1), *pb.add(1)) };
            if sa.raw() != sb.raw() {
                return false;
            }
        }
        (0..slot_count(ha)).all(|i| {
            // SAFETY: slot `i` lies inside both nodes' payloads (`slot_count` set bits).
            let (ca, cb) = unsafe { (*pa.add(slots + i), *pb.add(slots + i)) };
            same_slot(ca, cb)
        })
    }

    /// [`same_map`]'s slot arm: a leaf cons cell, a collision tuple or an inner node.
    fn same_slot(a: Eterm, b: Eterm) -> bool {
        let atoms = no_atoms();
        if prelude::is_list(a) {
            if !prelude::is_list(b) {
                return false;
            }
            // SAFETY: a leaf is a header-less two-word `[Key, Value]` cons cell.
            return unsafe {
                prelude::eq(prelude::car(a), prelude::car(b), &atoms)
                    && prelude::eq(prelude::cdr(a), prelude::cdr(b), &atoms)
            };
        }
        if !prelude::is_boxed_term(a) || !prelude::is_boxed_term(b) {
            return false;
        }
        // SAFETY: both are boxed, so word 0 is a header.
        let (ha, hb) = unsafe { (head_word(a), head_word(b)) };
        if ha.raw() != hb.raw() {
            return false;
        }
        if prelude::is_map_header(ha) {
            return same_node(a, b, false);
        }
        if prelude::is_arity_value(ha) {
            // A collision node: a tuple of `[Key, Value]` cons cells in `cmp` order.
            let cells = prelude::arityval(ha);
            return (0..cells).all(|i| {
                // SAFETY: `i < arityval`, so element `i` is inside both tuple payloads.
                let (ca, cb) =
                    unsafe { (prelude::tuple_element(a, i), prelude::tuple_element(b, i)) };
                same_slot(ca, cb)
            });
        }
        false
    }

    /// Assert that `enif_make_map_put` on `(map, key, value)` is indistinguishable from the
    /// `write_map` rebuild it used to produce, and return the put's result.
    #[track_caller]
    fn assert_put_agrees(env: &NifEnv, map: Eterm, key: Eterm, value: Eterm) -> Eterm {
        let mut out = n(make_small(SENTINEL));
        // SAFETY: `env` is live for the whole call and `out` is a live local.
        let code = unsafe { enif_make_map_put(env.as_ptr(), n(map), n(key), n(value), &mut out) };
        assert_eq!(code, 1, "a put on a well-formed map succeeds");
        assert_agrees(env, t(out), rebuild(env, map, key, value));
        t(out)
    }

    /// Assert that `enif_make_map_update` on a **present** key is indistinguishable from the
    /// rebuild; returns the update's result.
    #[track_caller]
    fn assert_update_agrees(env: &NifEnv, map: Eterm, key: Eterm, value: Eterm) -> Eterm {
        let mut out = n(make_small(SENTINEL));
        // SAFETY: `env` is live for the whole call and `out` is a live local.
        let code =
            unsafe { enif_make_map_update(env.as_ptr(), n(map), n(key), n(value), &mut out) };
        assert_eq!(code, 1, "the key is present");
        assert_agrees(env, t(out), rebuild(env, map, key, value));
        t(out)
    }

    /// The one equivalence assertion both writers share.
    #[track_caller]
    fn assert_agrees(env: &NifEnv, cow: Eterm, rebuilt: Eterm) {
        let atoms = no_atoms();
        assert!(!prelude::is_non_value(cow), "the out term is a real map");
        assert_eq!(
            prelude::map_size(cow, &atoms),
            prelude::map_size(rebuilt, &atoms),
            "the same key count"
        );
        assert!(
            prelude::eq(cow, rebuilt, &atoms),
            "the maps are `=:=`-equal"
        );
        assert!(same_map(cow, rebuilt), "the maps have the same interior");
        // The storage order is the serializer's order, so comparing it pair-by-pair is this
        // crate's stand-in for `term_to_binary` byte equality (no `beam_codec` dependency here).
        let from_cow = prelude::map_pairs_storage_order(cow).expect("a map");
        let from_rebuild = prelude::map_pairs_storage_order(rebuilt).expect("a map");
        assert_eq!(from_cow.len(), from_rebuild.len(), "the same pair count");
        for (cow_pair, rebuilt_pair) in from_cow.iter().zip(&from_rebuild) {
            assert!(
                prelude::eq(cow_pair.0, rebuilt_pair.0, &atoms),
                "the same key at the same storage position"
            );
            assert!(
                prelude::eq(cow_pair.1, rebuilt_pair.1, &atoms),
                "the same value at the same storage position"
            );
        }
        // And the count the C's `enif_get_map_size` answers agrees as well.
        assert_eq!(size_of(env, cow), size_of(env, rebuilt));
    }

    #[test]
    fn a_put_agrees_with_the_write_map_rebuild_at_every_size() {
        // 0/1: the empty flat map and a one-pair map; 31: the largest insert below the limit;
        // 32: the flat→HAMT conversion (`FlatConvert`); 33/34: HAMT inserts just above the
        // limit; 250: a wide HAMT (the Θ(log n) claim's regime).
        for pairs in [0i64, 1, 31, 32, 33, 34, 250] {
            let mut host = MapHost::with_heap(1 << 18);
            let env = NifEnv::new(&mut host);
            let map = map_of(&env, pairs);
            assert_eq!(
                size_of(&env, map),
                pairs as usize,
                "the source map has `pairs` pairs"
            );

            // One **absent** key: the insert arm (and, at 32 pairs, the flat→HAMT conversion).
            let inserted = assert_put_agrees(&env, map, make_small(pairs + 1), make_small(999));
            // The comparator is discriminating: the result really is a *different* map.
            assert!(!same_map(map, inserted), "the insert changed the map");
            assert_eq!(
                size_of(&env, inserted),
                pairs as usize + 1,
                "an absent key grows the map by one"
            );
            assert_eq!(
                prelude::signed_value(value_of(&env, inserted, make_small(pairs + 1))),
                999
            );

            // One **present** key: the replace arm (the flat arm reuses the source keys tuple).
            if pairs > 0 {
                let key = pairs / 2;
                let replaced = assert_put_agrees(&env, map, make_small(key), make_small(-7));
                assert!(!same_map(map, replaced), "the replace changed the map");
                assert_eq!(
                    size_of(&env, replaced),
                    pairs as usize,
                    "a present key leaves the size alone"
                );
                assert_eq!(
                    prelude::signed_value(value_of(&env, replaced, make_small(key))),
                    -7
                );
                // The input map is untouched — the write went to the fresh reservation, and the
                // sharing is one-way (the result reads `map`'s off-path nodes).
                assert_eq!(
                    prelude::signed_value(value_of(&env, map, make_small(key))),
                    key * 100,
                    "`map_in` is never mutated"
                );
            }
        }
    }

    #[test]
    fn an_update_agrees_with_the_rebuild_and_writes_nothing_for_an_absent_key() {
        for pairs in [1i64, 31, 32, 33, 34, 250] {
            let mut host = MapHost::with_heap(1 << 18);
            let env = NifEnv::new(&mut host);
            let map = map_of(&env, pairs);

            // A present key: the same answer the rebuild gives.
            let key = pairs / 2;
            let updated = assert_update_agrees(&env, map, make_small(key), make_small(-7));
            assert_eq!(size_of(&env, updated), pairs as usize);
            assert_eq!(
                prelude::signed_value(value_of(&env, updated, make_small(key))),
                -7
            );

            // An absent key: `0`, and neither `*map_out` nor the input map is touched
            // (`erts_maps_update` refuses before anything is built).
            let mut out = n(make_small(SENTINEL));
            // SAFETY: `env` is live and `out` is a live local.
            let code = unsafe {
                enif_make_map_update(
                    env.as_ptr(),
                    n(map),
                    n(make_small(pairs + 1)),
                    n(make_small(9)),
                    &mut out,
                )
            };
            assert_eq!(code, 0, "a missing key is refused");
            assert_eq!(
                t(out),
                make_small(SENTINEL),
                "a refused update writes nothing into `*map_out`"
            );
            assert_eq!(
                size_of(&env, map),
                pairs as usize,
                "the input map is unchanged"
            );
        }
    }

    #[test]
    fn a_malformed_map_is_refused_without_writing_anything() {
        let mut host = MapHost::with_heap(1 << 12);
        let env = NifEnv::new(&mut host);
        let map = map_of(&env, 2);
        // Corrupt the flat map's `size` word so it disagrees with its keys tuple's arity — the
        // one shape `flat_pair_count` refuses, and the shape a forged/corrupt term presents.
        // No builder in this crate produces it, so the writer's typed `TermError::BadMap` is the
        // only guard: it must answer the C's `0` and write nothing, never panic and never fall
        // back to a rebuild.
        let head = prelude::boxed_val(map);
        // SAFETY: `map` is a live 2-pair flat map, whose head block is
        // `[arityval, k0, k1, head, size, keys, v0, v1]`; word 1 of the head is that raw `size`
        // word, and this test owns the whole arena.
        unsafe { *head.add(1) = Eterm::from_raw(3) };

        let mut out = n(make_small(SENTINEL));
        // SAFETY: `env` is live for both calls and `out` is a live local.
        let code = unsafe {
            enif_make_map_put(
                env.as_ptr(),
                n(map),
                n(make_small(0)),
                n(make_small(1)),
                &mut out,
            )
        };
        assert_eq!(code, 0, "a forged map is refused");
        assert_eq!(t(out), make_small(SENTINEL), "and nothing is written");

        // SAFETY: as above.
        let code = unsafe {
            enif_make_map_update(
                env.as_ptr(),
                n(map),
                n(make_small(0)),
                n(make_small(1)),
                &mut out,
            )
        };
        assert_eq!(code, 0, "the update slot refuses it too");
        assert_eq!(t(out), make_small(SENTINEL), "and nothing is written");
    }
}
