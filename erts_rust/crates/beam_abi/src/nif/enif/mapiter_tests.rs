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

//! Unit tests for the map-iterator family, driven through the `#[no_mangle]` functions directly.
//!
//! The end-to-end evidence (a real C NIF `.so` loaded through `crate::nif::loader`) lives in
//! `tests/nif_mapiter.rs`; these tests pin the two things that are awkward to observe from C: the
//! **exact index arithmetic** (including the C's empty-flat-map wrap-around) and the fact that the
//! walk order equals `beam_term`'s storage order for **both** map representations.

use super::*;

use beam_term::atom::AtomTable;
use beam_term::map::{map_pairs_storage_order, map_words_capacity, write_map, MAP_SMALL_MAP_LIMIT};
use beam_term::prelude::{make_small, Eterm};

/// A heap that outlives the returned terms (never reallocates, per `beam_alloc::Heap`).
struct Arena {
    heap: beam_alloc::Heap,
}

impl Arena {
    fn new(words: usize) -> Arena {
        Arena {
            heap: beam_alloc::Heap::new(words).expect("a test heap is allocatable"),
        }
    }

    /// Build a map from `n` distinct small-integer keys (`K -> K*10`).
    fn map(&mut self, n: usize) -> Eterm {
        let pairs: Vec<(Eterm, Eterm)> = (0..n)
            .map(|i| (make_small(i as i64), make_small(i as i64 * 10)))
            .collect();
        let atoms = AtomTable::new();
        // SAFETY: `map_words_capacity(n)` fresh words, exclusively owned by this arena, which
        // outlives every term derived from it.
        unsafe {
            let hp = self
                .heap
                .alloc_words(map_words_capacity(pairs.len()))
                .expect("enough words for the map");
            write_map(hp, &pairs, &atoms).expect("a well-formed map")
        }
    }
}

/// A zeroed iterator, as a NIF's stack local would be before `create`.
fn fresh_iter() -> ErlNifMapIterator {
    ErlNifMapIterator::default()
}

/// Walk the whole map forwards from `FIRST`, collecting `get_pair` results.
fn forward_walk(iter: &mut ErlNifMapIterator, term: ERL_NIF_TERM) -> Vec<(Eterm, Eterm)> {
    let mut out = Vec::new();
    // SAFETY: `iter` is a live, unaliased iterator owned by the caller.
    unsafe {
        assert_eq!(
            enif_map_iterator_create(null_mut(), term, iter, ErlNifMapIteratorEntry::First),
            1
        );
        loop {
            let mut key = ERL_NIF_TERM::default();
            let mut value = ERL_NIF_TERM::default();
            if enif_map_iterator_get_pair(null_mut(), iter, &mut key, &mut value) == 0 {
                break;
            }
            out.push((eterm_from_nif(key), eterm_from_nif(value)));
            if enif_map_iterator_next(null_mut(), iter) == 0 {
                // The last pair was read; `next` reported the tail.
                break;
            }
        }
        enif_map_iterator_destroy(null_mut(), iter);
    }
    out
}

/// Walk the whole map backwards from `TAIL` (i.e. after `create(LAST)`), collecting `get_pair`.
fn backward_walk(iter: &mut ErlNifMapIterator, term: ERL_NIF_TERM) -> Vec<(Eterm, Eterm)> {
    let mut out = Vec::new();
    // SAFETY: as `forward_walk`.
    unsafe {
        assert_eq!(
            enif_map_iterator_create(null_mut(), term, iter, ErlNifMapIteratorEntry::Last),
            1
        );
        loop {
            let mut key = ERL_NIF_TERM::default();
            let mut value = ERL_NIF_TERM::default();
            if enif_map_iterator_get_pair(null_mut(), iter, &mut key, &mut value) == 0 {
                break;
            }
            out.push((eterm_from_nif(key), eterm_from_nif(value)));
            if enif_map_iterator_prev(null_mut(), iter) == 0 {
                break;
            }
        }
        enif_map_iterator_destroy(null_mut(), iter);
    }
    out
}

/// The expected walk = `beam_term`'s storage order (the C's own iterator order).
fn expected(map: Eterm) -> Vec<(Eterm, Eterm)> {
    map_pairs_storage_order(map).expect("a well-formed map")
}

#[test]
fn flat_map_walks_in_storage_order_both_ways() {
    let mut arena = Arena::new(4096);
    let map = arena.map(5);
    let mut iter = fresh_iter();
    let term = nif_from_eterm(map);
    let want = expected(map);
    assert_eq!(forward_walk(&mut iter, term), want);
    assert_eq!(
        backward_walk(&mut iter, term),
        want.into_iter().rev().collect::<Vec<_>>()
    );
}

#[test]
fn empty_flat_map_is_head_and_tail_and_yields_no_pair() {
    let mut arena = Arena::new(1024);
    let map = arena.map(0);
    let mut iter = fresh_iter();
    // SAFETY: `iter` is a live, unaliased iterator.
    unsafe {
        assert_eq!(
            enif_map_iterator_create(
                null_mut(),
                nif_from_eterm(map),
                &mut iter,
                ErlNifMapIteratorEntry::First
            ),
            1
        );
        // `FIRST` on an empty map is `offset = 0`, hence `idx = 1` (`erl_nif.c:3783/3796`).
        assert_eq!(iter.idx.raw(), 1);
        assert_eq!(iter.size.raw(), 0);
        assert_eq!(enif_map_iterator_is_head(null_mut(), &mut iter), 1);
        assert_eq!(enif_map_iterator_is_tail(null_mut(), &mut iter), 1);
        let mut key = ERL_NIF_TERM::default();
        let mut value = ERL_NIF_TERM::default();
        assert_eq!(enif_map_iterator_get_pair(null_mut(), &mut iter, &mut key, &mut value), 0);
        // `FIRST` and `LAST` both leave the iterator in the same "bad shape".
        assert_eq!(
            enif_map_iterator_create(
                null_mut(),
                nif_from_eterm(map),
                &mut iter,
                ErlNifMapIteratorEntry::Last
            ),
            1
        );
        assert_eq!(iter.idx.raw(), 0);
        assert_eq!(enif_map_iterator_is_head(null_mut(), &mut iter), 1);
        assert_eq!(enif_map_iterator_is_tail(null_mut(), &mut iter), 1);
        enif_map_iterator_destroy(null_mut(), &mut iter);
        // `destroy` is idempotent for the flat arm too (nothing is owned).
        enif_map_iterator_destroy(null_mut(), &mut iter);
    }
}

#[test]
fn hash_map_walks_in_storage_order_both_ways() {
    let mut arena = Arena::new(1 << 20);
    let map = arena.map(MAP_SMALL_MAP_LIMIT + 8);
    // The interior really is a hash map (the >32-key path).
    // SAFETY: `map` is a live boxed term.
    assert!(unsafe { prelude::is_hashmap(map) });
    let mut iter = fresh_iter();
    let term = nif_from_eterm(map);
    let want = expected(map);
    assert_eq!(want.len(), MAP_SMALL_MAP_LIMIT + 8);
    assert_eq!(forward_walk(&mut iter, term), want);
    assert_eq!(
        backward_walk(&mut iter, term),
        want.into_iter().rev().collect::<Vec<_>>()
    );
}

#[test]
fn create_rejects_a_non_map_and_destroy_is_idempotent_on_a_hash_iterator() {
    let mut arena = Arena::new(1 << 20);
    let map = arena.map(MAP_SMALL_MAP_LIMIT + 8);
    let mut iter = fresh_iter();
    // SAFETY: `iter` is live and unaliased; the non-map term is a small integer.
    unsafe {
        assert_eq!(
            enif_map_iterator_create(
                null_mut(),
                nif_from_eterm(make_small(7)),
                &mut iter,
                ErlNifMapIteratorEntry::First
            ),
            0
        );
        assert_eq!(
            enif_map_iterator_create(
                null_mut(),
                nif_from_eterm(map),
                &mut iter,
                ErlNifMapIteratorEntry::Last
            ),
            1
        );
        assert_eq!(enif_map_iterator_is_tail(null_mut(), &mut iter), 0);
        assert_eq!(enif_map_iterator_is_head(null_mut(), &mut iter), 0);
        enif_map_iterator_destroy(null_mut(), &mut iter);
        // The cache handle is nulled, so the second `destroy` cannot double free.
        assert!(iter.u.hash.wstack.is_null());
        assert_eq!(enif_map_iterator_get_pair(null_mut(), &mut iter, &mut key(), &mut key()), 0);
    }
}

/// A throwaway out-parameter slot.
fn key() -> ERL_NIF_TERM {
    ERL_NIF_TERM::default()
}

/// An **all-zero** iterator — exactly the state `memset(&it, 0, sizeof(it))` leaves the C fixture's
/// stack local in (and the state a never-touched NIF stack local is in). Note that
/// `ErlNifMapIterator::default()` is *not* this: `ERL_NIF_TERM::default()` is `NIL` (an immediate, so
/// the pre-fix `destroy` took the flat arm and only the `map == 0` word provoked the null read).
fn zeroed_iter() -> ErlNifMapIterator {
    // SAFETY: all-bits-zero is a valid `ErlNifMapIterator` — every field is an integer or a raw
    // pointer (map/size/idx are `ERL_NIF_TERM` = one machine word; both union arms are pairs of raw
    // pointers; `__spare__` is raw pointers). Nothing is dereferenced by the `memcpy`.
    unsafe { core::mem::zeroed() }
}

// ---------------------------------------------------------------------------------------------
// Regression: `destroy` on a never-created (or already destroyed) iterator — the null-deref bug
// ---------------------------------------------------------------------------------------------
//
// `enif_map_iterator_destroy` used to test `is_hashmap(it.map)` *without* the `is_term_word` guard
// the sibling read paths apply (`iter_is_flat`, `create`). The C's `is_boxed` bit trick (`erl_term.h`)
// classifies the primary tag `0` as *boxed*, so an all-zero iterator (`map == 0`, what `memset`/a NIF
// stack local starts as) made `is_hashmap` read the header through a **null pointer**: an abort under
// `debug_assertions` and UB in release. `create` on a non-map leaves `it.map` untouched, so the same
// path was reachable from a NIF that created the iterator on a non-map too. The module docs promise
// `destroy` is idempotent, so all of these must be safe no-ops.

#[test]
fn destroy_on_a_never_created_iterator_is_a_safe_noop() {
    let mut iter = zeroed_iter();
    assert_eq!(
        iter.map.raw(),
        0,
        "the all-zero struct a NIF stack local starts as (`memset` in the C fixture)"
    );
    // SAFETY: `iter` is a live, unaliased, zeroed iterator; `destroy` tolerates a non-term `map`
    // (nothing was created, so there is no cache to release).
    unsafe {
        enif_map_iterator_destroy(null_mut(), &mut iter);
        // ...and it stays a no-op on every subsequent call (the documented idempotency).
        enif_map_iterator_destroy(null_mut(), &mut iter);
        enif_map_iterator_destroy(null_mut(), &mut iter);
        // The rest of the family keeps working on the same iterator: the read paths guard the map
        // too (`iter_is_flat` rejects the non-term word), so it falls to the hash arm, where the C's
        // own predicates are `idx == 0` for head and `idx > size` for tail.
        assert_eq!(enif_map_iterator_is_head(null_mut(), &mut iter), 1);
        assert_eq!(enif_map_iterator_is_tail(null_mut(), &mut iter), 0);
        assert_eq!(
            enif_map_iterator_get_pair(null_mut(), &mut iter, &mut key(), &mut key()),
            0
        );
        // A `create` that *refuses* (a non-map) leaves `it.map` untouched, so the very same
        // never-created state is what a refused `create` hands back to `destroy`.
        assert_eq!(
            enif_map_iterator_create(
                null_mut(),
                nif_from_eterm(make_small(7)),
                &mut iter,
                ErlNifMapIteratorEntry::First
            ),
            0
        );
        assert_eq!(iter.map.raw(), 0);
        enif_map_iterator_destroy(null_mut(), &mut iter);
        // A null iterator is the other tolerated operand (`iter_mut` returns `None`).
        enif_map_iterator_destroy(null_mut(), null_mut());
        // A default-constructed (`map == NIL`) iterator is a further never-created shape.
        let mut nil_iter = fresh_iter();
        enif_map_iterator_destroy(null_mut(), &mut nil_iter);
        enif_map_iterator_destroy(null_mut(), &mut nil_iter);
    }
}

#[test]
fn destroy_tolerates_a_hand_built_non_term_map_word() {
    // The C's `DEBUG` `destroy` writes `iter->map = THE_NON_VALUE` and leaves the cache dangling; a
    // reader of such a struct (or merely an uninitialised one) reaches `destroy` with a non-term map
    // word that is *not* zero. `is_term_word` rejects it — pre-fix its primary tag `HEADER` was still
    // classified as boxed and dereferenced at that garbage address.
    //
    // `make_header(0, FLOAT_SUBTAG)` is exactly `THE_NON_VALUE` in a debug build
    // (`beam_term::ops::THE_NON_VALUE` is zero in release), and is spelled out here so the case is
    // a non-zero non-term word under *both* profiles.
    let non_term = nif_from_eterm(prelude::make_header(0, prelude::FLOAT_SUBTAG));
    let mut iter = fresh_iter();
    iter.map = non_term;
    assert_ne!(iter.map.raw(), 0, "the hand-built word is a non-zero non-term");
    // SAFETY: `iter` is live and unaliased; `destroy` tolerates a non-term `map`.
    unsafe {
        enif_map_iterator_destroy(null_mut(), &mut iter);
        enif_map_iterator_destroy(null_mut(), &mut iter);
    }
    // The word is left alone (there is nothing to release, exactly as for the flat arm).
    assert_eq!(iter.map, non_term);
}

#[test]
fn destroy_releases_the_hash_cache_exactly_once() {
    let mut arena = Arena::new(1 << 20);
    let hash = arena.map(MAP_SMALL_MAP_LIMIT + 8);
    let flat = arena.map(4);
    let mut iter = fresh_iter();
    // SAFETY: `iter` is live and unaliased.
    unsafe {
        // The hash arm owns the `Box<HashPairs>` cache: the first `destroy` releases it (and nulls
        // the handle *first*), the second must not free it again (a double free in the C).
        assert_eq!(
            enif_map_iterator_create(
                null_mut(),
                nif_from_eterm(hash),
                &mut iter,
                ErlNifMapIteratorEntry::First
            ),
            1
        );
        assert!(!iter.u.hash.wstack.is_null());
        enif_map_iterator_destroy(null_mut(), &mut iter);
        assert!(
            iter.u.hash.wstack.is_null(),
            "the handle is nulled before the cache is released"
        );
        enif_map_iterator_destroy(null_mut(), &mut iter);
        enif_map_iterator_destroy(null_mut(), &mut iter);
        // The flat arm owns nothing at all: every `destroy` is a no-op.
        assert_eq!(
            enif_map_iterator_create(
                null_mut(),
                nif_from_eterm(flat),
                &mut iter,
                ErlNifMapIteratorEntry::First
            ),
            1
        );
        enif_map_iterator_destroy(null_mut(), &mut iter);
        enif_map_iterator_destroy(null_mut(), &mut iter);
    }
}
#[test]
fn next_and_prev_report_the_c_counters() {
    let mut arena = Arena::new(1024);
    let map = arena.map(3);
    let mut iter = fresh_iter();
    // SAFETY: `iter` is live and unaliased.
    unsafe {
        assert_eq!(
            enif_map_iterator_create(
                null_mut(),
                nif_from_eterm(map),
                &mut iter,
                ErlNifMapIteratorEntry::First
            ),
            1
        );
        // `create(FIRST)` is at pair 1 — neither head nor tail.
        assert_eq!(enif_map_iterator_is_head(null_mut(), &mut iter), 0);
        assert_eq!(enif_map_iterator_is_tail(null_mut(), &mut iter), 0);
        // Two `next`s reach the last pair (idx 3) and report success; the third reports the tail.
        assert_eq!(enif_map_iterator_next(null_mut(), &mut iter), 1);
        assert_eq!(enif_map_iterator_next(null_mut(), &mut iter), 1);
        assert_eq!(iter.idx.raw(), 3);
        assert_eq!(enif_map_iterator_is_tail(null_mut(), &mut iter), 0);
        assert_eq!(enif_map_iterator_next(null_mut(), &mut iter), 0);
        assert_eq!(enif_map_iterator_is_tail(null_mut(), &mut iter), 1);
        // Past the tail `next` keeps reporting failure.
        assert_eq!(enif_map_iterator_next(null_mut(), &mut iter), 0);
        // One `prev` comes back to the last pair; three more reach HEAD (`idx == 0`).
        assert_eq!(enif_map_iterator_prev(null_mut(), &mut iter), 1);
        assert_eq!(iter.idx.raw(), 3);
        assert_eq!(enif_map_iterator_prev(null_mut(), &mut iter), 1);
        assert_eq!(enif_map_iterator_prev(null_mut(), &mut iter), 1);
        assert_eq!(enif_map_iterator_prev(null_mut(), &mut iter), 0);
        assert_eq!(iter.idx.raw(), 0);
        assert_eq!(enif_map_iterator_is_head(null_mut(), &mut iter), 1);
        assert_eq!(enif_map_iterator_get_pair(null_mut(), &mut iter, &mut key(), &mut key()), 0);
        enif_map_iterator_destroy(null_mut(), &mut iter);
    }
}
