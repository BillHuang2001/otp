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
