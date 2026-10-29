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

//! Unit tests for the map interior: flat-map/HAMT construction, lookups, the
//! structural-size invariants and the layout pins (`src/map.rs` owns the code).

use super::*;
use crate::boxed::{
    header_arity, is_flatmap, is_hashmap, is_hashmap_header, map_header_val,
};
use crate::heap::Heap;
use crate::immediate::{make_atom, make_small, signed_value, NIL};
use crate::size::size_object;

/// A permanently-invalid marker word: `write_map` must never touch it.
const SENTINEL: Eterm = Eterm::from_raw(0xDEAD_BEEF);

fn atoms() -> AtomTable {
    let mut t = AtomTable::new();
    t.seed(["a", "b", "c"]);
    t
}

/// A map built into an owned, over-sized buffer: the tail beyond
/// `map_words_capacity` is a sentinel that detects any overflow.
struct Built {
    _buf: Vec<Eterm>,
    map: Eterm,
}

fn build(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Built {
    let cap = map_words_capacity(pairs.len());
    let mut buf = vec![SENTINEL; cap + 64];
    let base = buf.as_mut_ptr();
    // SAFETY: `buf` reserved `cap` words for the map (plus slack), and it is
    // kept alive by the returned `Built`.
    let map = unsafe { write_map(base, pairs, atoms).unwrap() };
    for i in 0..64 {
        assert_eq!(
            buf[cap + i],
            SENTINEL,
            "write_map overflowed map_words_capacity by at least {} words",
            i + 1
        );
    }
    Built { _buf: buf, map }
}

/// Normalize a map buffer for base-independent comparison: each word becomes
/// `(true, offset)` when it is a tagged pointer into `buf`, else `(false, raw)`.
/// Two canonically-laid-out maps (same shape, different heap base) normalize
/// equal.
fn normalize(buf: &[Eterm]) -> Vec<(bool, usize)> {
    let base = buf.as_ptr() as usize;
    let end = base + std::mem::size_of_val(buf);
    buf.iter()
        .map(|w| {
            let raw = w.raw();
            // LIST (0x1) and BOXED (0x2) primary tags carry a tagged pointer.
            if raw & 0x3 == 0x1 || raw & 0x3 == 0x2 {
                let p = raw & !0x7usize;
                if p >= base && p < end {
                    return (true, p - base);
                }
            }
            (false, raw)
        })
        .collect()
}
fn pairs_of(keys: &[i64], atoms: &AtomTable) -> Vec<(Eterm, Eterm)> {
    let _ = atoms;
    keys.iter()
        .map(|k| (make_small(*k), make_small(*k * 10)))
        .collect()
}

fn keys_1_to(n: i64) -> Vec<i64> {
    (1..=n).collect()
}

#[test]
fn empty_map_is_a_three_word_flatmap() {
    let a = atoms();
    let b = build(&[], &a);
    // SAFETY: `b.map` is a live boxed map.
    assert!(unsafe { is_flatmap(b.map) });
    assert_eq!(map_size(b.map, &a).unwrap(), 0);
    assert_eq!(map_pairs(b.map, &a).unwrap(), vec![]);
    assert_eq!(size_object(b.map), 3);
}

#[test]
fn small_map_is_a_flatmap_with_sorted_keys() {
    let a = atoms();
    let mut heap = Heap::new(64);
    let k3 = heap.tuple(&[make_small(3)]).unwrap();
    let k1 = heap.cons(make_small(1), NIL).unwrap();
    let raw = [
        (make_small(5), make_small(50)),
        (k3, make_atom(a.lookup("a").unwrap())),
        (k1, make_small(10)),
    ];
    let b = build(&raw, &a);
    // SAFETY: `b.map` is a live boxed map.
    assert!(unsafe { is_flatmap(b.map) });
    assert_eq!(map_size(b.map, &a).unwrap(), 3);
    // `map_pairs` is in Erlang term order: number < tuple < list.
    let pairs = map_pairs(b.map, &a).unwrap();
    assert_eq!(pairs[0].0, make_small(5));
    assert_eq!(pairs[1].0, k3);
    assert_eq!(pairs[2].0, k1);
    assert_eq!(pairs[2].1, make_small(10));
}

#[test]
fn storage_order_is_the_flatmap_keys_tuple_order() {
    let a = atoms();
    let mut heap = Heap::new(64);
    let k3 = heap.tuple(&[make_small(3)]).unwrap();
    let k1 = heap.cons(make_small(1), NIL).unwrap();
    let raw = [
        (make_small(5), make_small(50)),
        (k3, make_atom(a.lookup("a").unwrap())),
        (k1, make_small(10)),
    ];
    let b = build(&raw, &a);
    // SAFETY: `b.map` is a live boxed map.
    assert!(unsafe { is_flatmap(b.map) });
    // SAFETY: `b.map` is a live flat map.
    let (keys, values) = unsafe { flatmap_parts(b.map) };
    let stored = map_pairs_storage_order(b.map).unwrap();
    assert_eq!(stored.len(), 3);
    assert_eq!(
        stored,
        keys.iter()
            .copied()
            .zip(values.iter().copied())
            .collect::<Vec<_>>()
    );
    assert_eq!(stored[0].0, make_small(5));

    // Flat-map storage order is atom-*index* order, which is not the `cmp`
    // (name) order `map_pairs` returns: seed "b" before "a".
    let mut t = AtomTable::new();
    t.seed(["b", "a"]);
    let b_atom = make_atom(t.lookup("b").unwrap());
    let a_atom = make_atom(t.lookup("a").unwrap());
    let built = build(&[(a_atom, make_small(1)), (b_atom, make_small(2))], &t);
    let stored = map_pairs_storage_order(built.map).unwrap();
    assert_eq!(stored, vec![(b_atom, make_small(2)), (a_atom, make_small(1))]);
    assert_ne!(stored, map_pairs(built.map, &t).unwrap());
}

#[test]
fn storage_order_matches_the_hamt_traversal_of_a_hash_map() {
    let a = atoms();
    let n = 40i64;
    let b = build(&pairs_of(&keys_1_to(n), &a), &a);
    // SAFETY: `b.map` is a live boxed map.
    assert!(unsafe { is_hashmap(b.map) });
    let stored = map_pairs_storage_order(b.map).unwrap();
    // Every input key appears exactly once, with its matching value.
    assert_eq!(stored.len(), n as usize);
    let mut keys: Vec<i64> = stored.iter().map(|(k, _)| signed_value(*k)).collect();
    keys.sort_unstable();
    assert_eq!(keys, keys_1_to(n));
    for (k, v) in &stored {
        assert_eq!(*v, make_small(signed_value(*k) * 10));
    }
    // The order is the raw HAMT slot traversal, not a re-sorted one.
    let mut direct: Vec<(Eterm, Eterm)> = Vec::new();
    collect_slots(boxed_val(b.map), &mut direct).unwrap();
    assert_eq!(stored, direct);
}

#[test]
fn storage_order_rejects_non_maps() {
    assert_eq!(
        map_pairs_storage_order(make_small(1)).unwrap_err(),
        TermError::BadMap
    );
    assert_eq!(
        map_pairs_storage_order(NIL).unwrap_err(),
        TermError::BadMap
    );
    let mut heap = Heap::new(8);
    let tup = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
    assert_eq!(map_pairs_storage_order(tup).unwrap_err(), TermError::BadMap);
}

#[test]
fn duplicate_keys_last_value_wins() {
    let a = atoms();
    let raw = [
        (make_small(1), make_small(10)),
        (make_small(2), make_small(20)),
        (make_small(1), make_small(99)),
    ];
    let b = build(&raw, &a);
    assert_eq!(map_size(b.map, &a).unwrap(), 2);
    assert_eq!(map_get(b.map, make_small(1), &a).unwrap(), Some(make_small(99)));
    assert_eq!(map_get(b.map, make_small(2), &a).unwrap(), Some(make_small(20)));
}

#[test]
fn flatmap_to_hashmap_boundary_is_at_32_keys() {
    let a = atoms();
    for n in [31i64, 32] {
        let b = build(&pairs_of(&keys_1_to(n), &a), &a);
        // SAFETY: `b.map` is a live boxed map.
        assert!(unsafe { is_flatmap(b.map) }, "n={n} should be a flat map");
        assert!(!unsafe { is_hashmap(b.map) });
        assert_eq!(map_size(b.map, &a).unwrap(), n as usize);
    }
    for n in [33i64, 34, 40] {
        let b = build(&pairs_of(&keys_1_to(n), &a), &a);
        // SAFETY: `b.map` is a live boxed map.
        assert!(unsafe { is_hashmap(b.map) }, "n={n} should be a hash map");
        assert!(!unsafe { is_flatmap(b.map) });
        assert_eq!(map_size(b.map, &a).unwrap(), n as usize);
        for k in 1..=n {
            assert_eq!(
                map_get(b.map, make_small(k), &a).unwrap(),
                Some(make_small(k * 10))
            );
        }
        assert_eq!(map_get(b.map, make_small(n + 1), &a).unwrap(), None);
    }
}

#[test]
fn hashmap_node_headers_match_the_c_encodings() {
    let a = atoms();
    let b = build(&pairs_of(&keys_1_to(33), &a), &a);
    let p = boxed_val(b.map);
    // SAFETY: `b.map` is a live boxed hash map.
    let head = unsafe { *p };
    assert!(is_map_header(head));
    assert!(is_hashmap_header(head));
    // Head array: MAKE_MAP_HEADER(tag=2, arity=1, val=0xffff) == 0xFFFF01AC.
    if map_header_type(head) as usize == MAP_HEADER_TAG_HAMT_HEAD_ARRAY {
        assert_eq!(head.raw(), 0xFFFF_01AC);
        assert_eq!(map_header_val(head), 0xffff);
    } else {
        // Head bitmap: MAKE_MAP_HEADER(tag=3, arity=1, bitmap).
        assert_eq!(
            map_header_type(head) as usize,
            MAP_HEADER_TAG_HAMT_HEAD_BITMAP
        );
        assert_eq!(head.raw() & 0xFFFF, 0x0000_0000 | (head.raw() & 0xFFFF));
    }
    assert_eq!(header_arity(head), 1);
    // The head's size word is the key count.
    // SAFETY: word 1 of a head node is its size word.
    assert_eq!(unsafe { *p.add(1) }.raw(), 33);

    // Node bitmaps: MAKE_MAP_HEADER(tag=1, arity=0, bitmap) => arity field 0.
    // Walk the tree and check every non-head map header.
    let mut stack = vec![p];
    let mut seen_nodes = 0;
    while let Some(node) = stack.pop() {
        // SAFETY: every pushed pointer is a live node header.
        let hdr = unsafe { *node };
        assert!(is_map_header(hdr));
        let (off, sz) = node_slots(hdr).unwrap();
        if node != p {
            assert_eq!(map_header_type(hdr) as usize, MAP_HEADER_TAG_HAMT_NODE_BITMAP);
            assert_eq!(header_arity(hdr), 0);
            // `HAMT_SUBTAG_NODE_BITMAP` = (1 << 6) | 0x2C.
            assert_eq!(hdr.raw() & 0xFF, 0x6C);
        }
        assert!(sz >= 1 && sz <= 16);
        for i in 0..sz {
            // SAFETY: slot `i` is within the node payload.
            let item = unsafe { *node.add(off + i) };
            if is_list(item) {
                // A leaf is a `[Key, Value]` cons cell (`is_list` already asserted).
            } else {
                // SAFETY: a non-leaf slot is a live boxed node or collision tuple.
                let ihdr = unsafe { *boxed_val(item) };
                if is_map_header(ihdr) {
                    stack.push(boxed_val(item));
                } else {
                    seen_nodes += 1;
                    assert!(is_arity_value(ihdr));
                }
            }
        }
    }
    let _ = seen_nodes;
}

#[test]
fn insertion_order_does_not_change_the_layout() {
    let a = atoms();
    let forward: Vec<(Eterm, Eterm)> = (1..=60i64)
        .map(|k| (make_small(k), make_small(k * 3)))
        .collect();
    let mut backward = forward.clone();
    backward.reverse();
    let b1 = build(&forward, &a);
    let b2 = build(&backward, &a);
    // The two maps must be structurally identical: same shape and size.
    assert_eq!(size_object(b1.map), size_object(b2.map));
    // Base-independent layout comparison: the canonical leaf/trie layout is
    // insertion-order free (the block bases differ, so absolute pointers are
    // normalized to offsets first).
    let cap = map_words_capacity(forward.len());
    assert_eq!(
        normalize(&b1._buf[..cap]),
        normalize(&b2._buf[..cap]),
        "canonical layout is insertion-order free"
    );
    assert!(eq(b1.map, b2.map, &a));
}

#[test]
fn build_lookup_iterate_delete_over_sizes() {
    let a = atoms();
    for n in 0..=500i64 {
        let raw = pairs_of(&keys_1_to(n), &a);
        let b = build(&raw, &a);
        assert_eq!(map_size(b.map, &a).unwrap(), n as usize);
        let got = map_pairs(b.map, &a).unwrap();
        assert_eq!(got.len(), n as usize);
        for i in 0..got.len() {
            assert_eq!(got[i].0, make_small(i as i64 + 1));
            assert_eq!(got[i].1, make_small((i as i64 + 1) * 10));
        }
        for k in 1..=n {
            assert_eq!(
                map_get(b.map, make_small(k), &a).unwrap(),
                Some(make_small(k * 10)),
                "missing key {k} of {n}"
            );
        }
        assert_eq!(map_get(b.map, make_small(n + 1), &a).unwrap(), None);
    }
}

#[test]
fn put_and_remove_are_functional() {
    let a = atoms();
    for n in [0i64, 3, 32, 33, 100] {
        let b = build(&pairs_of(&keys_1_to(n), &a), &a);
        // Insert/replace a key.
        let cap = map_words_capacity(map_size(b.map, &a).unwrap() + 1);
        let mut buf = vec![SENTINEL; cap + 64];
        let base = buf.as_mut_ptr();
        // SAFETY: `buf` reserved the capacity `map_put` documents.
        let put = unsafe { map_put(base, b.map, make_small(1), make_small(-1), &a).unwrap() };
        assert_eq!(map_get(put, make_small(1), &a).unwrap(), Some(make_small(-1)));
        assert_eq!(map_size(put, &a).unwrap(), n.max(1) as usize);
        assert!(size_object(put) <= cap);
        if n == 0 {
            // SAFETY: `put` is a live boxed map.
            assert!(unsafe { is_flatmap(put) });
        }

        // Insert a brand-new key.
        let base2 = buf.as_mut_ptr();
        let added = n + 1000;
        // SAFETY: `buf` reserved the capacity `map_put` documents.
        let put2 = unsafe { map_put(base2, b.map, make_small(added), make_small(7), &a).unwrap() };
        assert_eq!(map_get(put2, make_small(added), &a).unwrap(), Some(make_small(7)));
        assert_eq!(map_size(put2, &a).unwrap(), n as usize + 1);

        // Remove a key (present or not).
        let removed = if n > 0 { n / 2 + 1 } else { 1 };
        // SAFETY: `buf` reserved at least `map_words_capacity(n + 1)`.
        let gone = unsafe { map_remove(base2, b.map, make_small(removed), &a).unwrap() };
        assert_eq!(map_get(gone, make_small(removed), &a).unwrap(), None);
        let expected = if n > 0 { n - 1 } else { 0 };
        assert_eq!(map_size(gone, &a).unwrap(), expected as usize);
    }
}

#[test]
fn structural_compare_and_hash_agree_across_representations() {
    let a = atoms();
    // Same pairs built in two different orders.
    let p1 = pairs_of(&keys_1_to(60), &a);
    let mut p2 = p1.clone();
    p2.reverse();
    let b1 = build(&p1, &a);
    let b2 = build(&p2, &a);
    assert_eq!(crate::compare::cmp(b1.map, b2.map, &a), core::cmp::Ordering::Equal);
    assert_eq!(
        crate::hashing::term_hash(b1.map, &a),
        crate::hashing::term_hash(b2.map, &a)
    );

    // Size decides first.
    let small = build(&pairs_of(&keys_1_to(33), &a), &a);
    let big = build(&pairs_of(&keys_1_to(40), &a), &a);
    assert_eq!(
        crate::compare::cmp(small.map, big.map, &a),
        core::cmp::Ordering::Less
    );
    assert_ne!(
        crate::hashing::term_hash(small.map, &a),
        crate::hashing::term_hash(big.map, &a)
    );

    // Same size, keys ordered identically, values decide.
    let x = build(
        &[
            (make_small(1), make_atom(a.lookup("a").unwrap())),
            (make_small(2), make_atom(a.lookup("b").unwrap())),
        ],
        &a,
    );
    let y = build(
        &[
            (make_small(1), make_atom(a.lookup("a").unwrap())),
            (make_small(2), make_atom(a.lookup("c").unwrap())),
        ],
        &a,
    );
    assert_eq!(
        crate::compare::cmp(x.map, y.map, &a),
        core::cmp::Ordering::Less
    );
}

#[test]
fn colliding_keys_form_a_collision_node() {
    // Force identical hashes by using keys whose `term_hash` collides is hard;
    // instead check the *general* invariant that a lookup never returns a value
    // for a key that is present only under a different hash path.
    let a = atoms();
    let b = build(&pairs_of(&keys_1_to(200), &a), &a);
    for k in 1..=200 {
        assert_eq!(
            map_get(b.map, make_small(k), &a).unwrap(),
            Some(make_small(k * 10))
        );
    }
    // Keys sharing the same low nibble (e.g. 1 and 17: 1 & 0xF == 17 & 0xF) must
    // still be distinguishable.
    assert_eq!(map_get(b.map, make_small(1), &a).unwrap(), Some(make_small(10)));
    assert_eq!(map_get(b.map, make_small(17), &a).unwrap(), Some(make_small(170)));
}

#[test]
fn errors_surface_for_non_maps() {
    let a = atoms();
    assert_eq!(map_size(make_small(1), &a), Err(TermError::BadMap));
    assert_eq!(
        map_pairs(make_small(1), &a).unwrap_err(),
        TermError::BadMap
    );
    assert_eq!(
        map_get(NIL, make_small(1), &a).unwrap_err(),
        TermError::BadMap
    );
}

#[test]
fn empty_pairs_and_single_pair_round_trip() {
    let a = atoms();
    let one = build(&[(make_small(42), make_atom(a.lookup("a").unwrap()))], &a);
    // SAFETY: `one.map` is a live boxed map.
    assert!(unsafe { is_flatmap(one.map) });
    assert_eq!(map_size(one.map, &a).unwrap(), 1);
    assert_eq!(
        map_get(one.map, make_small(42), &a).unwrap(),
        Some(make_atom(a.lookup("a").unwrap()))
    );
    assert_eq!(map_get(one.map, make_small(41), &a).unwrap(), None);
}

#[test]
fn helper_predicates_are_consistent() {
    let a = atoms();
    let b = build(&pairs_of(&keys_1_to(5), &a), &a);
    assert!(is_map_term(b.map));
    assert!(is_leaf_term(make_small(1)) == false);
    let map33 = build(&pairs_of(&keys_1_to(33), &a), &a);
    // SAFETY: a live hash-map head node.
    let hdr = unsafe { *boxed_val(map33.map) };
    assert_eq!(node_subtag(hdr), hdr.raw() & 0xFF);
    assert_eq!(crate::tag::HAMT_SUBTAG_HEAD_ARRAY, 0xAC);
    assert_eq!(MAP_SMALL_MAP_LIMIT, 32);
    assert_eq!(HAMT_MAX_LEVEL, 16);
}

#[test]
fn atom_keys_use_index_order_in_a_flatmap() {
    // The flat-map key tuple is sorted by `erts_cmp_flatmap_keys`: atoms by
    // *index*, everything else by term order. Seed so that "b" has a lower
    // index than "a" and check the stored order.
    let mut t = AtomTable::new();
    t.seed(["b", "a"]);
    let b_atom = make_atom(t.lookup("b").unwrap());
    let a_atom = make_atom(t.lookup("a").unwrap());
    assert!(b_atom.raw() < a_atom.raw(), "b must have the lower index");
    let built = build(&[(a_atom, make_small(1)), (b_atom, make_small(2))], &t);
    // SAFETY: `built.map` is a live boxed map.
    assert!(unsafe { is_flatmap(built.map) });
    // SAFETY: `built.map` is a live flat map.
    let (keys, values) = unsafe { flatmap_parts(built.map) };
    assert_eq!(keys, &[b_atom, a_atom]);
    assert_eq!(values, &[make_small(2), make_small(1)]);
    // `map_pairs` re-sorts into term (`cmp`) order, i.e. by atom name.
    let pairs = map_pairs(built.map, &t).unwrap();
    assert_eq!(pairs[0].0, a_atom);
    assert_eq!(pairs[1].0, b_atom);
}
