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

//! Unit tests for the path-copying map writers (`src/map/cow/`): every arm of
//! the C's put/delete state machine, cross-checked against a `write_map` rebuild
//! of the same pair set — element for element, in canonical *and* storage order —
//! and pinned on the word figures the capacity/required pair reports.

use super::*;
use crate::boxed::{
    boxed_val, is_flatmap, is_hashmap, is_map_header, make_arityval, make_boxed, map_header_type,
    map_header_val,
};
use crate::compare::{cmp, eq};
use crate::error::TermError;
use crate::ihash::{erts_map_hash, hashmap_nibble};
use crate::immediate::make_small;
use crate::list::car;
use crate::map::{
    make_map_header, map_get, map_pairs, map_pairs_storage_order, map_size, map_words_capacity,
    write_map, HAMT_MAX_LEVEL, MAP_SMALL_MAP_LIMIT,
};
use crate::tag::{
    MAP_HEADER_TAG_HAMT_HEAD_ARRAY, MAP_HEADER_TAG_HAMT_HEAD_BITMAP,
    MAP_HEADER_TAG_HAMT_NODE_BITMAP,
};
use crate::tuple::{tuple_element, tuple_size};

/// A permanently-invalid marker word: nothing may write past the reservation.
const SENTINEL: Eterm = Eterm::from_raw(0xDEAD_BEEF);

/// How many sentinel words follow every reservation.
const TAIL: usize = 64;

fn atoms() -> AtomTable {
    let mut t = AtomTable::new();
    t.seed(["a", "b", "c"]);
    t
}

fn small(n: i64) -> Eterm {
    make_small(n)
}

/// `n` pairs `{1 => 10, 2 => 20, ...}`.
fn pairs_1_to(n: i64) -> Vec<(Eterm, Eterm)> {
    (1..=n).map(|i| (small(i), small(i * 10))).collect()
}

/// An owned region of `cap` map words followed by [`TAIL`] sentinel words, so any
/// over-write past the reservation is caught.
struct Buf {
    words: Vec<Eterm>,
    cap: usize,
}

impl Buf {
    fn new(cap: usize) -> Buf {
        Buf {
            words: vec![SENTINEL; cap + TAIL],
            cap,
        }
    }

    fn base(&mut self) -> *mut Eterm {
        self.words.as_mut_ptr()
    }

    /// The whole reservation is still untouched (a "writes nothing" case).
    fn is_pristine(&self) -> bool {
        self.words[..self.cap].iter().all(|w| *w == SENTINEL)
    }

    fn check_tail(&self) {
        for i in 0..TAIL {
            assert_eq!(
                self.words[self.cap + i],
                SENTINEL,
                "wrote past the reservation by at least {} words",
                i + 1
            );
        }
    }
}

/// Builds `pairs` with `write_map` into an owned, over-sized buffer.
fn build_map(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> (Buf, Eterm) {
    let mut buf = Buf::new(map_words_capacity(pairs.len()));
    // SAFETY: `buf` reserved `map_words_capacity` words plus a sentinel tail and
    // outlives every use of the returned map.
    let map = unsafe { write_map(buf.base(), pairs, atoms).unwrap() };
    buf.check_tail();
    (buf, map)
}

/// Applies [`map_put_cow`] into a fresh buffer sized by the **capacity** bound,
/// asserting the exact figure fits and nothing was written past it.
fn cow_put(src: Eterm, key: Eterm, value: Eterm, atoms: &AtomTable) -> (Buf, Eterm) {
    let size = map_size(src, atoms).unwrap();
    let required = map_put_cow_words_required(src, key).unwrap();
    let cap = map_put_cow_words_capacity(size);
    assert!(
        required <= cap,
        "required {required} words exceed the capacity bound {cap} at size {size}"
    );
    let mut out = Buf::new(cap);
    // SAFETY: the buffer reserved the capacity bound in a heap that outlives the
    // source map, and the result is dropped before either buffer.
    let map = unsafe { map_put_cow(out.base(), src, key, value, atoms).unwrap() };
    out.check_tail();
    (out, map)
}

/// Applies [`map_remove_cow`] into a fresh buffer sized by the capacity bound.
fn cow_remove(src: Eterm, key: Eterm, atoms: &AtomTable) -> (Buf, Option<Eterm>) {
    let size = map_size(src, atoms).unwrap();
    let required = map_remove_cow_words_required(src, key).unwrap();
    let cap = map_remove_cow_words_capacity(size);
    assert!(
        required <= cap,
        "required {required} words exceed the capacity bound {cap} at size {size}"
    );
    let mut out = Buf::new(cap);
    // SAFETY: as `cow_put`, plus `hp` is untouched for an absent key.
    let map = unsafe { map_remove_cow(out.base(), src, key, atoms).unwrap() };
    out.check_tail();
    (out, map)
}

/// Asserts `got` is exactly the map `pairs` describes: the same size, the same
/// value for every key, the same canonical `map_pairs` list *and* the same stored
/// (trie) order — all against a `write_map` rebuild of the same pair set.
fn assert_map(got: Eterm, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) {
    let (rebuf, rebuilt) = build_map(pairs, atoms);
    let want = map_pairs(rebuilt, atoms).unwrap();
    let have = map_pairs(got, atoms).unwrap();
    assert_eq!(have, want, "map_pairs differs from the rebuild");
    assert_eq!(
        map_size(got, atoms).unwrap(),
        want.len(),
        "map_size differs"
    );
    for (k, v) in &want {
        assert_eq!(map_get(got, *k, atoms).unwrap(), Some(*v), "map_get differs");
    }
    assert_eq!(
        map_pairs_storage_order(got).unwrap(),
        map_pairs_storage_order(rebuilt).unwrap(),
        "the stored trie order differs from the rebuild's"
    );
    let _ = rebuf;
}

/// Replaces `key` in `pairs` (insert-or-replace) for the expected-set helper.
fn with(pairs: &[(Eterm, Eterm)], key: Eterm, value: Eterm, atoms: &AtomTable) -> Vec<(Eterm, Eterm)> {
    let mut out: Vec<(Eterm, Eterm)> = pairs
        .iter()
        .copied()
        .filter(|(k, _)| !eq(*k, key, atoms))
        .collect();
    out.push((key, value));
    out
}

/// Removes `key` from `pairs`.
fn without(pairs: &[(Eterm, Eterm)], key: Eterm, atoms: &AtomTable) -> Vec<(Eterm, Eterm)> {
    pairs
        .iter()
        .copied()
        .filter(|(k, _)| !eq(*k, key, atoms))
        .collect()
}

/// The head node's words, read from the source map (used to derive the expected
/// divergence-chain cost exactly).
fn head_words(map: Eterm) -> usize {
    let p = boxed_val(map) as *const Eterm;
    // SAFETY: `map` is a live hash map, so `p` is its head word.
    let header = unsafe { *p };
    match map_header_type(header) as usize {
        MAP_HEADER_TAG_HAMT_HEAD_ARRAY => 18,
        MAP_HEADER_TAG_HAMT_HEAD_BITMAP => 2 + map_header_val(header).count_ones() as usize,
        _ => panic!("not a hash map head"),
    }
}

/// The number of leading nibbles two small-int keys' `erts_map_hash` agree on.
fn agreement(a: i64, b: i64) -> usize {
    let (ha, hb) = (erts_map_hash(small(a)), erts_map_hash(small(b)));
    let mut n = 0;
    while n < HAMT_MAX_LEVEL && hashmap_nibble(ha, n) == hashmap_nibble(hb, n) {
        n += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// Flat map arms
// ---------------------------------------------------------------------------

#[test]
fn empty_map_takes_one_pair() {
    let atoms = atoms();
    let (_src, map) = build_map(&[], &atoms);
    assert_eq!(map_size(map, &atoms).unwrap(), 0);

    let (_o, got) = cow_put(map, small(1), small(10), &atoms);
    assert!(unsafe { is_flatmap(got) });
    assert_map(got, &[(small(1), small(10))], &atoms);
    // Six words: `[hdr][n][keys]` plus the pair's two words.
    assert_eq!(map_put_cow_words_required(map, small(1)).unwrap(), 6);
}

#[test]
fn one_pair_map_put_and_replace() {
    let atoms = atoms();
    let base = vec![(small(1), small(10))];
    let (_src, map) = build_map(&base, &atoms);

    // A second key: still flat.
    let (_o, got) = cow_put(map, small(2), small(20), &atoms);
    assert!(unsafe { is_flatmap(got) });
    assert_map(got, &with(&base, small(2), small(20), &atoms), &atoms);

    // The same key, a new value: `3 + n` words.
    let (_o2, replaced) = cow_put(map, small(1), small(11), &atoms);
    assert_map(replaced, &[(small(1), small(11))], &atoms);
    assert_eq!(map_put_cow_words_required(map, small(1)).unwrap(), 3 + 1);
}

#[test]
fn flat_arms_write_the_canonical_flatmap_header() {
    // `MAP_HEADER_FLATMAP` is `MAKE_MAP_HEADER(TAG_FLATMAP_HEAD, 1, 0)` =
    // `0x12C` (`erl_map.h:181-186`) — a header word, not a word count, so both the
    // replace arm (which writes the prefix itself) and the insert arm (through
    // `write_flatmap`) must emit exactly it. A structural comparison alone cannot
    // see this: the flat map's arity/val field is dead, so a wrong header word
    // reads identically through every reader.
    let atoms = atoms();
    let (r, reference) = build_map(&[(small(1), small(10))], &atoms);
    // SAFETY: `reference` is a live flat map, so this is its head word.
    let canonical = unsafe { *boxed_val(reference) };
    assert_eq!(canonical.raw(), 0x12C, "the reference flatmap header");
    let _ = r;
    let base = pairs_1_to(5);
    let (_src, map) = build_map(&base, &atoms);
    // Reuse `map`'s own head word as the reference (same provenance, one buffer).
    // SAFETY: `map` is a live flat map.
    let canonical = unsafe { *boxed_val(map) };
    for pairs in [
        with(&base, small(3), small(999), &atoms), // replace: the `FlatReplace` arm
        with(&base, small(9), small(99), &atoms),  // insert: `write_flatmap`
    ] {
        let key = if pairs.len() == base.len() {
            small(3)
        } else {
            small(9)
        };
        let value = map_get_in(&pairs, key, &atoms);
        let (_o, got) = cow_put(map, key, value, &atoms);
        assert!(unsafe { is_flatmap(got) });
        // SAFETY: `got` is a live flat map.
        assert_eq!(
            unsafe { *boxed_val(got) },
            canonical,
            "the flat map's head word must be the canonical `0x12C`"
        );
    }
}
/// The value `pairs` gives `key` (the expected-set helper's lookup half).
fn map_get_in(pairs: &[(Eterm, Eterm)], key: Eterm, atoms: &AtomTable) -> Eterm {
    pairs
        .iter()
        .find(|(k, _)| eq(*k, key, atoms))
        .map(|(_, v)| *v)
        .expect("the key is in the expected pair set")
}

#[test]
fn flat_replace_reuses_the_source_keys_tuple() {
    let atoms = atoms();
    let base = pairs_1_to(5);
    let (src, map) = build_map(&base, &atoms);
    // The keys tuple is flat-map word 2.
    // SAFETY: `map` is a live flat map.
    let before = unsafe { *boxed_val(map).add(2) };
    let (_o, got) = cow_put(map, small(3), small(999), &atoms);
    // SAFETY: `got` is a live flat map (same shape as the source).
    let after = unsafe { *boxed_val(got).add(2) };
    assert_eq!(
        before, after,
        "the flat arm must REUSE the source keys-tuple word, not rebuild it"
    );
    // A different buffer, so the shared word really is the source's storage.
    assert_ne!(src.words.as_ptr(), before.raw() as *const Eterm as *const Eterm);
    assert_map(got, &with(&base, small(3), small(999), &atoms), &atoms);
}

#[test]
fn flat_same_value_writes_nothing_and_returns_the_input() {
    let atoms = atoms();
    let base = pairs_1_to(5);
    let (_src, map) = build_map(&base, &atoms);
    let mut out = Buf::new(map_put_cow_words_capacity(base.len()));
    // SAFETY: the buffer reserved the capacity bound; nothing is written.
    let got = unsafe { map_put_cow(out.base(), map, small(3), small(30), &atoms) }.unwrap();
    assert_eq!(got, map, "an identical value word must return the input map");
    assert!(out.is_pristine(), "an identical value must write nothing");
}

#[test]
fn flat_insert_at_the_front_middle_and_end() {
    let atoms = atoms();
    let base = vec![
        (small(10), small(100)),
        (small(20), small(200)),
        (small(30), small(300)),
    ];
    let (_src, map) = build_map(&base, &atoms);
    for key in [5i64, 25, 40] {
        let (_o, got) = cow_put(map, small(key), small(key * 100), &atoms);
        assert!(unsafe { is_flatmap(got) });
        assert_map(got, &with(&base, small(key), small(key * 100), &atoms), &atoms);
    }
}

#[test]
fn flat_31_to_32_stays_flat() {
    let atoms = atoms();
    let base = pairs_1_to(31);
    let (_src, map) = build_map(&base, &atoms);
    assert!(unsafe { is_flatmap(map) });
    let (_o, got) = cow_put(map, small(32), small(320), &atoms);
    assert!(unsafe { is_flatmap(got) }, "32 pairs still fit in a flat map");
    assert_map(got, &with(&base, small(32), small(320), &atoms), &atoms);
}

#[test]
fn flat_32_to_33_converts_to_a_hash_map() {
    let atoms = atoms();
    let base = pairs_1_to(32);
    let (_src, map) = build_map(&base, &atoms);
    assert!(unsafe { is_flatmap(map) });
    let (_o, got) = cow_put(map, small(33), small(330), &atoms);
    assert!(unsafe { is_hashmap(got) }, "33 pairs must be a hash map");
    assert_map(got, &with(&base, small(33), small(330), &atoms), &atoms);
    assert_eq!(map_size(got, &atoms).unwrap(), 33);
    // `map_words_capacity(33)` is the conversion arm's reservation.
    assert_eq!(map_put_cow_words_capacity(32), map_words_capacity(33));
}

// ---------------------------------------------------------------------------
// Hash map arms
// ---------------------------------------------------------------------------

#[test]
fn hamt_put_replace_and_delete_across_sizes() {
    let atoms = atoms();
    for n in [33i64, 40, 64, 250] {
        let base = pairs_1_to(n);
        let (src, map) = build_map(&base, &atoms);
        assert!(unsafe { is_hashmap(map) }, "n={n} should be a hash map");
        assert_eq!(map_size(map, &atoms).unwrap(), n as usize);

        // Insert a key that is not there yet: one more pair, `size + 1` words at most.
        let added = small(n + 1000);
        let (src2, inserted) = cow_put(map, added, small(7), &atoms);
        assert_map(inserted, &with(&base, added, small(7), &atoms), &atoms);
        assert_eq!(map_size(inserted, &atoms).unwrap(), n as usize + 1);

        // Replace a value: the size is unchanged (`delta == 0`).
        let existing = small(1);
        let (_o, replaced) = cow_put(inserted, existing, small(77), &atoms);
        assert_map(
            replaced,
            &with(&with(&base, added, small(7), &atoms), existing, small(77), &atoms),
            &atoms,
        );
        assert_eq!(map_size(replaced, &atoms).unwrap(), n as usize + 1);

        // Delete it again: the 34-key map comes back as the 33-key one.
        let (_o, back) = cow_remove(inserted, added, &atoms);
        let back = back.expect("the key is present");
        assert_map(back, &base, &atoms);
        assert_eq!(map_size(back, &atoms).unwrap(), n as usize);

        // Delete one of the original keys.
        let (src3, deleted) = cow_remove(inserted, existing, &atoms);
        let deleted = deleted.expect("the key is present");
        assert_map(
            deleted,
            &without(&with(&base, added, small(7), &atoms), existing, &atoms),
            &atoms,
        );
        let _ = (src, src2, src3);
    }
}

#[test]
fn hamt_identical_value_word_writes_nothing() {
    let atoms = atoms();
    let base = pairs_1_to(40);
    let (_src, map) = build_map(&base, &atoms);
    let mut out = Buf::new(map_put_cow_words_capacity(base.len()));
    // SAFETY: the buffer reserved the capacity bound; the arm writes nothing.
    let got = unsafe { map_put_cow(out.base(), map, small(7), small(70), &atoms) }.unwrap();
    assert_eq!(got, map, "an identical value word must return the input map");
    assert!(out.is_pristine(), "an identical value must write nothing");
}

#[test]
fn remove_absent_key_answers_none_and_writes_nothing() {
    let atoms = atoms();
    for n in [0i64, 5, 33, 64] {
        let base = pairs_1_to(n);
        let (_src, map) = build_map(&base, &atoms);
        let mut out = Buf::new(map_remove_cow_words_capacity(base.len()));
        // SAFETY: the buffer reserved the capacity bound.
        let got = unsafe { map_remove_cow(out.base(), map, small(n + 1000), &atoms) }.unwrap();
        assert_eq!(got, None, "n={n}: an absent key answers None");
        assert!(out.is_pristine(), "n={n}: an absent key writes nothing");
        assert_eq!(
            map_remove_cow_words_required(map, small(n + 1000)).unwrap(),
            0
        );
    }
}

#[test]
fn remove_down_to_32_is_a_flat_map_again() {
    let atoms = atoms();
    let base = pairs_1_to(33);
    let (_src, map) = build_map(&base, &atoms);
    assert!(unsafe { is_hashmap(map) });
    let (_o, got) = cow_remove(map, small(17), &atoms);
    let got = got.expect("the key is present");
    assert!(unsafe { is_flatmap(got) }, "32 pairs must be flat again");
    assert_map(got, &without(&base, small(17), &atoms), &atoms);
    assert_eq!(map_size(got, &atoms).unwrap(), 32);
}

#[test]
fn remove_every_key_down_to_zero() {
    let atoms = atoms();
    let mut pairs = pairs_1_to(34);
    let (src, map) = build_map(&pairs, &atoms);
    // Every result shares storage with its source by word, so the whole chain of
    // (buffer, map) pairs is kept alive until the test ends.
    let mut chain: Vec<(Buf, Eterm)> = vec![(src, map)];
    for key in 1..=34i64 {
        let current = chain[chain.len() - 1].1;
        let (out, next) = cow_remove(current, small(key), &atoms);
        let next = next.expect("the key is present");
        pairs = without(&pairs, small(key), &atoms);
        assert_map(next, &pairs, &atoms);
        assert_eq!(map_size(next, &atoms).unwrap(), pairs.len());
        if pairs.len() <= MAP_SMALL_MAP_LIMIT {
            assert!(
                unsafe { is_flatmap(next) },
                "size {} must be flat",
                pairs.len()
            );
        } else {
            assert!(unsafe { is_hashmap(next) });
        }
        chain.push((out, next));
    }
    let last = chain[chain.len() - 1].1;
    assert_eq!(map_size(last, &atoms).unwrap(), 0);
    assert!(unsafe { is_flatmap(last) });
}

#[test]
fn reinsert_past_the_flat_limit_again() {
    let atoms = atoms();
    let mut pairs = pairs_1_to(32);
    let (src, map) = build_map(&pairs, &atoms);
    assert!(unsafe { is_flatmap(map) });
    let mut chain: Vec<(Buf, Eterm)> = vec![(src, map)];

    // 32 -> 33 (the conversion arm).
    let (o, up) = cow_put(map, small(33), small(330), &atoms);
    pairs.push((small(33), small(330)));
    assert!(unsafe { is_hashmap(up) });
    assert_map(up, &pairs, &atoms);
    chain.push((o, up));

    // 33 -> 32 (back to a flat map).
    let (o, down) = cow_remove(up, small(33), &atoms);
    let down = down.expect("the key is present");
    pairs = without(&pairs, small(33), &atoms);
    assert!(unsafe { is_flatmap(down) });
    assert_map(down, &pairs, &atoms);
    chain.push((o, down));

    // 32 -> 33 again, with a different key.
    let (o, up2) = cow_put(down, small(34), small(340), &atoms);
    pairs.push((small(34), small(340)));
    assert!(unsafe { is_hashmap(up2) });
    assert_map(up2, &pairs, &atoms);
    chain.push((o, up2));
    assert_eq!(map_size(up2, &atoms).unwrap(), 33);
}

#[test]
fn inner_node_divergence_chain() {
    let atoms = atoms();
    // Find two keys whose hashes agree on the first two nibbles but differ later.
    let mut found = None;
    'search: for a in 1..600i64 {
        for b in (a + 1)..600i64 {
            if agreement(a, b) >= 2 {
                found = Some((a, b));
                break 'search;
            }
        }
    }
    let (a, b) = found.expect("some small-int pair agrees on two leading nibbles");
    let agree = agreement(a, b);
    assert!(agree >= 2 && agree < HAMT_MAX_LEVEL);

    // A 33-key hash map in which `a` is ALONE under its level-0 nibble, so the
    // insert of `b` meets `a`'s leaf at level 1 and builds the chain from there.
    let a_nib = hashmap_nibble(erts_map_hash(small(a)), 0);
    let mut base: Vec<(Eterm, Eterm)> = vec![(small(a), small(0))];
    let mut k = 1_000i64;
    while base.len() < 33 {
        if hashmap_nibble(erts_map_hash(small(k)), 0) != a_nib {
            base.push((small(k), small(k * 10)));
        }
        k += 1;
    }
    let (_src, map) = build_map(&base, &atoms);
    assert!(unsafe { is_hashmap(map) });
    assert_eq!(head_words(map).min(18), head_words(map));

    // The exact figure: the head, `agree - 1` single-bit inner nodes, the two-slot
    // node and the new leaf's cons cell.
    let expected = head_words(map) + (agree - 1) * 2 + 3 + 2;
    assert_eq!(
        map_put_cow_words_required(map, small(b)).unwrap(),
        expected,
        "the divergence chain for agree={agree} costs the C's insert_subnodes figure"
    );

    let (_o, got) = cow_put(map, small(b), small(b * 10), &atoms);
    let expected_pairs = with(&base, small(b), small(b * 10), &atoms);
    assert_map(got, &expected_pairs, &atoms);
    assert_eq!(map_size(got, &atoms).unwrap(), 34);
    // Everything is still reachable after the chain was spliced in.
    for (k, v) in &expected_pairs {
        assert_eq!(map_get(got, *k, &atoms).unwrap(), Some(*v));
    }
}

#[test]
fn every_key_of_many_sizes_is_replaced_and_removed() {
    let atoms = atoms();
    // A wide sweep over the deep arms: each size's every key takes one put
    // (value replacement) and one remove, both checked against a rebuild — which
    // exercises the bitmap change matrix (keep/drop/collapse), the head
    // array/bitmap downgrade and the flat conversion from every leaf position.
    for n in [33i64, 34, 35, 40, 48, 64, 100, 250] {
        let base = pairs_1_to(n);
        let (_src, map) = build_map(&base, &atoms);
        for key in 1..=n {
            let fresh = small(key + 7_000);
            let (o, replaced) = cow_put(map, small(key), fresh, &atoms);
            assert_map(replaced, &with(&base, small(key), fresh, &atoms), &atoms);
            let (o2, deleted) = cow_remove(map, small(key), &atoms);
            let deleted = deleted.expect("the key is present");
            assert_map(deleted, &without(&base, small(key), &atoms), &atoms);
            // Every result shares the source's storage, so the source stays alive;
            // the two staging buffers are only needed while `replaced`/`deleted`
            // are inspected.
            drop((o, o2));
        }
    }
}

// ---------------------------------------------------------------------------
// Word figures and typed rejections
// ---------------------------------------------------------------------------

#[test]
fn capacity_figures() {
    // Flat arms: `flatmap_size_words(n + 1)` / `flatmap_size_words(n - 1)`.
    assert_eq!(map_put_cow_words_capacity(0), 6);
    assert_eq!(map_put_cow_words_capacity(31), 68);
    // The conversion arm.
    assert_eq!(map_put_cow_words_capacity(32), 711);
    // The hash-map spines.
    assert_eq!(map_put_cow_words_capacity(33), 293);
    assert_eq!(map_put_cow_words_capacity(10_000), 293);
    assert_eq!(map_remove_cow_words_capacity(0), 3);
    assert_eq!(map_remove_cow_words_capacity(1), 3);
    assert_eq!(map_remove_cow_words_capacity(32), 66);
    assert_eq!(map_remove_cow_words_capacity(33), 288);
    assert_eq!(map_remove_cow_words_capacity(10_000), 288);
}

#[test]
fn exact_reservation_leaves_the_sentinel_tail_intact() {
    let atoms = atoms();
    for n in [0i64, 3, 32, 33, 64] {
        let base = pairs_1_to(n);
        let (_src, map) = build_map(&base, &atoms);
        let required = map_put_cow_words_required(map, small(n + 1)).unwrap();
        let mut out = Buf::new(required);
        // SAFETY: exactly the figure `map_put_cow_words_required` reported.
        let got = unsafe { map_put_cow(out.base(), map, small(n + 1), small(1), &atoms) }.unwrap();
        out.check_tail();
        assert_map(
            got,
            &with(&base, small(n + 1), small(1), &atoms),
            &atoms,
        );

        let required = map_remove_cow_words_required(map, small(1)).unwrap();
        let mut out = Buf::new(required);
        // SAFETY: exactly the figure `map_remove_cow_words_required` reported.
        let got = unsafe { map_remove_cow(out.base(), map, small(1), &atoms) }.unwrap();
        out.check_tail();
        if let Some(got) = got {
            assert_map(got, &without(&base, small(1), &atoms), &atoms);
        }
    }
}

#[test]
fn a_non_map_is_a_typed_bad_map() {
    let atoms = atoms();
    let mut buf = Buf::new(64);
    let raw = buf.base();
    let non_maps = [small(1), Eterm::nil(), crate::tuple::tuple0()];
    for t in non_maps {
        // SAFETY: `buf` has room for the small writes these paths make (none).
        unsafe {
            assert_eq!(map_put_cow(raw, t, small(1), small(2), &atoms), Err(TermError::BadMap));
            assert_eq!(map_remove_cow(raw, t, small(1), &atoms), Err(TermError::BadMap));
        }
        assert_eq!(map_put_cow_words_required(t, small(1)), Err(TermError::BadMap));
        assert_eq!(map_remove_cow_words_required(t, small(1)), Err(TermError::BadMap));
    }
    assert!(buf.is_pristine());
    buf.check_tail();
}

#[test]
fn a_flat_map_chain_of_puts_matches_one_rebuild() {
    let atoms = atoms();
    // Fold 300 single-key puts into one growing map and compare against a rebuild.
    let mut pairs: Vec<(Eterm, Eterm)> = Vec::new();
    let (_root, mut map) = build_map(&[], &atoms);
    let mut keep: Vec<(Buf, Eterm)> = Vec::new();
    for i in 1..=300i64 {
        let (o, next) = cow_put(map, small(i), small(i * 3), &atoms);
        pairs.push((small(i), small(i * 3)));
        if i == 300 {
            assert_map(next, &pairs, &atoms);
        }
        keep.push((o, next));
        map = next;
    }
    assert_eq!(map_size(map, &atoms).unwrap(), 300);
    assert_map(map, &pairs, &atoms);
    let _ = keep;
}

// ---------------------------------------------------------------------------
// Hand-built hash maps: the collision-node arms
//
// A collision node exists only for keys that share **all** 64 bits of
// `erts_map_hash` (`erl_map.c:2642-2646`), and `ihash_mix64` is a bijection over an
// immediate's raw word (`src/ihash.rs`), so no two ordinary keys — small integers,
// atoms, tuples — can be made to collide. These shapes are therefore **built by
// hand**, word for word, in the frozen node layout of `src/map.rs`: a head bitmap
// node whose slots hold a leaf cons cell, a nested bitmap node, or a collision node.
// That is the only way to reach the writer's collision insert / replace / shrink /
// collapse arms at all.
// ---------------------------------------------------------------------------

use super::words::{HAMT_PUT_COLLISION_CELLS_MAX, HAMT_REMOVE_COLLISION_CELLS_MAX};

/// One subtree of a hand-built hash map.
enum Hand {
    /// A `[Key, Value]` cons cell.
    Leaf(Eterm, Eterm),
    /// A collision node: `[arityval(n)][cell term]…` plus the `n` two-word
    /// `[Key, Value]` cons cells those slots point at, the cells in `cmp` order.
    Collision(Vec<(Eterm, Eterm)>),
    /// A bitmap node one level down: `(nibble, subtree)` per slot.
    Node(Vec<(u32, Hand)>),
}

/// The words a hand-built subtree occupies (`level == 0` is the head node, which
/// also carries the `size` word).
fn hand_words(node: &Hand, level: usize) -> usize {
    match node {
        Hand::Leaf(..) => 2,
        // The `n + 1` words of the tuple *plus* its `n` two-word cons cells — a
        // collision node stores cell **terms**, not raw key/value pairs
        // (`src/map.rs::serialize_node`, `TrieChild::Collision`).
        Hand::Collision(cells) => 1 + 3 * cells.len(),
        Hand::Node(slots) => {
            let prefix = if level == 0 { 2 } else { 1 };
            prefix
                + slots.len()
                + slots
                    .iter()
                    .map(|(_, child)| hand_words(child, level + 1))
                    .sum::<usize>()
        }
    }
}

/// The number of keys a hand-built subtree holds (the head's `size` word).
fn hand_size(node: &Hand) -> usize {
    match node {
        Hand::Leaf(..) => 1,
        Hand::Collision(cells) => cells.len(),
        Hand::Node(slots) => slots.iter().map(|(_, child)| hand_size(child)).sum(),
    }
}

/// Every pair a hand-built subtree holds, in no meaningful order.
fn hand_pairs(node: &Hand) -> Vec<(Eterm, Eterm)> {
    match node {
        Hand::Leaf(k, v) => vec![(*k, *v)],
        Hand::Collision(cells) => cells.clone(),
        Hand::Node(slots) => slots
            .iter()
            .flat_map(|(_, child)| hand_pairs(child))
            .collect(),
    }
}

/// The stored pair sequence of a hand-built subtree — slot order, and a collision
/// node's cells in the order they sit in it (what `map_pairs_storage_order` walks).
fn hand_storage_order(node: &Hand) -> Vec<(Eterm, Eterm)> {
    match node {
        Hand::Leaf(k, v) => vec![(*k, *v)],
        Hand::Collision(cells) => cells.clone(),
        Hand::Node(slots) => {
            let mut order: Vec<&(u32, Hand)> = slots.iter().collect();
            order.sort_by_key(|(nib, _)| *nib);
            order
                .into_iter()
                .flat_map(|(_, child)| hand_storage_order(child))
                .collect()
        }
    }
}

/// Writes the hand-built subtree `node` at `base + at` (node level `level`) and
/// returns the term addressing it.
///
/// # Safety
///
/// `base + at` must address [`hand_words`]`(node, level)` exclusively-owned writable
/// words, and the region must stay live and unmoved while the term is used.
unsafe fn hand_write(base: *mut Eterm, at: usize, node: &Hand, level: usize) -> Eterm {
    let here = base.wrapping_add(at);
    match node {
        Hand::Leaf(k, v) => {
            // SAFETY: a leaf is two reserved words; both are written before the term
            // escapes.
            unsafe {
                *here = *k;
                *here.add(1) = *v;
            }
            make_list(here as *const Eterm)
        }
        Hand::Collision(cells) => {
            // SAFETY: a collision node reserves `1 + 3 * cells.len()` words — the
            // tuple, then its cell terms, then each cell's two cons-cell words. All
            // of them are written before the term escapes.
            unsafe {
                *here = make_arityval(cells.len());
                for (i, (k, v)) in cells.iter().enumerate() {
                    let cell = here.add(1 + cells.len() + 2 * i);
                    *cell = *k;
                    *cell.add(1) = *v;
                    *here.add(1 + i) = make_list(cell as *const Eterm);
                }
            }
            make_boxed(here as *const Eterm)
        }
        Hand::Node(slots) => {
            let mut order: Vec<&(u32, Hand)> = slots.iter().collect();
            order.sort_by_key(|(nib, _)| *nib);
            let prefix = if level == 0 { 2 } else { 1 };
            let mut bitmap = 0u16;
            // The children's payloads follow this node's own words, each child taking
            // exactly `hand_words(child, level + 1)` of them.
            let mut cur = at + prefix + order.len();
            for (i, (nib, child)) in order.iter().enumerate() {
                assert!(
                    bitmap & (1 << nib) == 0,
                    "hand-built slots need distinct nibbles"
                );
                bitmap |= 1u16 << nib;
                // SAFETY: `cur` addresses the child's own reserved words, and the slot
                // word at `at + prefix + i` lies below `cur`.
                unsafe {
                    let child_term = hand_write(base, cur, child, level + 1);
                    *here.add(prefix + i) = child_term;
                }
                cur += hand_words(child, level + 1);
            }
            // SAFETY: the header (and the head's `size` word) are reserved words.
            unsafe {
                if level == 0 {
                    *here =
                        make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, bitmap as usize);
                    *here.add(1) = Eterm::from_raw(hand_size(node));
                } else {
                    *here = make_map_header(MAP_HEADER_TAG_HAMT_NODE_BITMAP, 0, bitmap as usize);
                }
            }
            make_boxed(here as *const Eterm)
        }
    }
}

/// Builds the hand-built map `root` (a [`Hand::Node`] at level `0`) in an owned buffer.
fn build_hand_map(root: &Hand) -> (Buf, Eterm) {
    let mut buf = Buf::new(hand_words(root, 0));
    let base = buf.base();
    // SAFETY: the buffer reserved `hand_words(root, 0)` words plus a sentinel tail and
    // outlives every use of the returned map.
    let map = unsafe { hand_write(base, 0, root, 0) };
    buf.check_tail();
    (buf, map)
}

/// `count` distinct small ints, one per level-1 nibble `0..count`, whose level-0
/// nibble is `nib0` — the level-1 fan-out of a hand-built trie node.
fn fan_out(nib0: u32, count: usize) -> Vec<i64> {
    let mut out: Vec<Option<i64>> = vec![None; count];
    let mut found = 0usize;
    for k in 1..200_000i64 {
        let h = erts_map_hash(small(k));
        if hashmap_nibble(h, 0) != nib0 {
            continue;
        }
        let l1 = hashmap_nibble(h, 1) as usize;
        if l1 < out.len() && out[l1].is_none() {
            out[l1] = Some(k);
            found += 1;
            if found == out.len() {
                break;
            }
        }
    }
    assert_eq!(found, out.len(), "a level-1 fan-out for head nibble {nib0}");
    out.into_iter().map(|k| k.expect("filled")).collect()
}

/// `count` distinct small ints at or above `from`, all in head slot `nib0` (the cells
/// of a hand-built collision node).
fn slot_keys(nib0: u32, count: usize, from: i64) -> Vec<i64> {
    (from..)
        .filter(|k| hashmap_nibble(erts_map_hash(small(*k)), 0) == nib0)
        .take(count)
        .collect()
}

/// `pairs` with a 16-leaf level-1 fan-out in head slot 0 (`k => k * 10`), a second one
/// in head slot 1, and whatever else the caller adds.
fn fan_root(extra: Vec<(u32, Hand)>) -> Hand {
    let mut slots = vec![(0u32, fan_node(fan_out(0, 16))), (1, fan_node(fan_out(1, 16)))];
    slots.extend(extra);
    Hand::Node(slots)
}

/// A level-1 bitmap node holding one leaf per level-1 nibble, `k => k * 10`.
fn fan_node(keys: Vec<i64>) -> Hand {
    Hand::Node(
        keys.iter()
            .enumerate()
            .map(|(i, k)| (i as u32, Hand::Leaf(small(*k), small(k * 10))))
            .collect(),
    )
}

/// `cells` with `(key, value)` placed at its `compare::cmp` position — the order a
/// collision node's cells are kept in.
fn sorted_cells(
    cells: &[(Eterm, Eterm)],
    key: Eterm,
    value: Eterm,
    atoms: &AtomTable,
) -> Vec<(Eterm, Eterm)> {
    let mut out = cells.to_vec();
    let at = out
        .iter()
        .position(|(k, _)| cmp(key, *k, atoms) == core::cmp::Ordering::Less)
        .unwrap_or(out.len());
    out.insert(at, (key, value));
    out
}

/// The keys of the collision node in `map`'s head slot `nib`, in stored order.
fn collision_cell_keys(map: Eterm, nib: u32) -> Vec<Eterm> {
    let p = boxed_val(map) as *const Eterm;
    // SAFETY: `map` is a live hash map whose head is a bitmap node, so `p` is its
    // header word, `p + 2 …` its slot words, and the slot's child is a collision node.
    unsafe {
        assert!(is_map_header(*p), "the hand-built root is a hash-map head");
        let bitmap = map_header_val(*p) as u16;
        let slot = (bitmap & ((1u16 << nib) - 1)).count_ones() as usize;
        let child = *p.add(2 + slot);
        let cells = tuple_size(child);
        (0..cells).map(|i| car(tuple_element(child, i))).collect()
    }
}

/// Asserts `got` holds exactly `pairs` on the reader surface every consumer uses: the
/// canonical `map_pairs` order, the size and every key's value.
///
/// The **storage** order is deliberately not compared with a `write_map` rebuild of
/// `pairs`: a hand-built map holds a collision node where `write_map` builds a deeper
/// trie (the cells' keys do not really collide), so the two differ in shape while
/// agreeing on content. The collision cases pin the storage order against the
/// hand-built expectation instead.
fn assert_content(got: Eterm, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) {
    let (rebuf, rebuilt) = build_map(pairs, atoms);
    let want = map_pairs(rebuilt, atoms).unwrap();
    assert_eq!(map_pairs(got, atoms).unwrap(), want, "map_pairs differs");
    assert_eq!(map_size(got, atoms).unwrap(), want.len(), "map_size differs");
    for (k, v) in &want {
        assert_eq!(map_get(got, *k, atoms).unwrap(), Some(*v), "map_get differs");
    }
    let _ = rebuf;
}

#[test]
fn collision_node_put_inserts_a_cell() {
    let atoms = atoms();
    let cells: Vec<(Eterm, Eterm)> = slot_keys(2, 3, 300_000)
        .into_iter()
        .map(|k| (small(k), small(k * 10)))
        .collect();
    let root = fan_root(vec![(2, Hand::Collision(cells.clone()))]);
    let (_src, map) = build_hand_map(&root);
    assert_eq!(map_size(map, &atoms).unwrap(), 35);

    // A key of head slot 2 reaches the collision node (its level-0 nibble matches and
    // the slot's child *is* the collision node).
    let new_key = slot_keys(2, 1, 400_000)[0];
    let added = (small(new_key), small(7));
    let expected_cells = sorted_cells(&cells, added.0, added.1, &atoms);
    // The plan's exact figure: the head (`2 + 3` slots), the node it copies is the
    // collision node itself (`1 + 4` cells — the source's three plus the new one) and
    // the new leaf's cons cell (`2`).
    let required = map_put_cow_words_required(map, added.0).unwrap();
    assert_eq!(required, (2 + 3) + (1 + 4) + 2);

    let mut out = Buf::new(required);
    // SAFETY: exactly the figure `map_put_cow_words_required` reported, in a heap that
    // outlives the source map.
    let got = unsafe { map_put_cow(out.base(), map, added.0, added.1, &atoms) }.unwrap();
    out.check_tail();
    assert_content(got, &with(&hand_pairs(&root), added.0, added.1, &atoms), &atoms);
    assert_eq!(map_size(got, &atoms).unwrap(), 36);

    // The cells stay in `cmp` order, and the stored order is the hand-built trie's with
    // the new cell spliced in — the copied head keeps its slot order.
    assert_eq!(
        collision_cell_keys(got, 2),
        expected_cells.iter().map(|(k, _)| *k).collect::<Vec<_>>()
    );
    let mut want_order: Vec<(Eterm, Eterm)> = hand_storage_order(&root);
    want_order.truncate(32); // the two 16-leaf fan-outs
    want_order.extend(expected_cells);
    assert_eq!(map_pairs_storage_order(got).unwrap(), want_order);
}

#[test]
fn collision_node_put_replaces_a_value_without_a_size_change() {
    let atoms = atoms();
    let cells: Vec<(Eterm, Eterm)> = slot_keys(2, 3, 300_000)
        .into_iter()
        .map(|k| (small(k), small(k * 10)))
        .collect();
    let root = fan_root(vec![(2, Hand::Collision(cells.clone()))]);
    let (_src, map) = build_hand_map(&root);

    // A value replacement copies the head (`2 + 3`), the collision node (`1 + 3`
    // cells) and the changed cell's fresh cons cell (`2`) — the cell itself is
    // path-copied like the node, so the size stays unchanged.
    let target = cells[1].0;
    let required = map_put_cow_words_required(map, target).unwrap();
    assert_eq!(required, (2 + 3) + (1 + 3) + 2);
    let mut out = Buf::new(required);
    // SAFETY: exactly the figure `map_put_cow_words_required` reported.
    let got = unsafe { map_put_cow(out.base(), map, target, small(999), &atoms) }.unwrap();
    out.check_tail();
    assert_content(
        got,
        &with(&hand_pairs(&root), target, small(999), &atoms),
        &atoms,
    );
    assert_eq!(map_size(got, &atoms).unwrap(), 35);
    assert_eq!(
        collision_cell_keys(got, 2),
        cells.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        "a value replacement keeps every cell in place"
    );

    // The identical value word writes nothing at all.
    let mut out = Buf::new(map_put_cow_words_capacity(35));
    // SAFETY: the buffer reserved the capacity bound; this arm writes nothing.
    let same = unsafe { map_put_cow(out.base(), map, cells[1].0, cells[1].1, &atoms) }.unwrap();
    assert_eq!(same, map);
    assert!(out.is_pristine());
    // The figure carries no value, so it prices the *writing* put — a sound upper
    // bound for the arm that turned out to write nothing.
    assert_eq!(
        map_put_cow_words_required(map, cells[1].0).unwrap(),
        (2 + 3) + (1 + 3) + 2
    );
}

#[test]
fn collision_node_remove_shrinks_then_collapses_then_goes_flat() {
    let atoms = atoms();
    let cells: Vec<(Eterm, Eterm)> = slot_keys(2, 3, 300_000)
        .into_iter()
        .map(|k| (small(k), small(k * 10)))
        .collect();
    let root = fan_root(vec![(2, Hand::Collision(cells.clone()))]);
    let (src, map) = build_hand_map(&root);
    assert_eq!(map_size(map, &atoms).unwrap(), 35);
    // Every result shares its source's storage, so the whole chain stays alive.
    let mut chain: Vec<(Buf, Eterm)> = vec![(src, map)];

    // 35 -> 34: a `CollisionShrink` — the head (`2 + 3` slots) and the collision node
    // (`1 + 2` cells) are copied; a remove writes no cons cell.
    let required = map_remove_cow_words_required(map, cells[0].0).unwrap();
    assert_eq!(required, (2 + 3) + (1 + 2));
    let mut out = Buf::new(required);
    // SAFETY: exactly the figure `map_remove_cow_words_required` reported.
    let shrunk = unsafe { map_remove_cow(out.base(), map, cells[0].0, &atoms) }
        .unwrap()
        .expect("the key is present");
    out.check_tail();
    assert_content(
        shrunk,
        &without(&hand_pairs(&root), cells[0].0, &atoms),
        &atoms,
    );
    assert_eq!(map_size(shrunk, &atoms).unwrap(), 34);
    assert_eq!(
        collision_cell_keys(shrunk, 2),
        cells[1..].iter().map(|(k, _)| *k).collect::<Vec<_>>()
    );
    chain.push((out, shrunk));

    // 34 -> 33: the two remaining cells collapse — the survivor rises into the head's
    // slot, so only the head is written (`2 + 3`) and the node itself costs nothing.
    let current = chain[chain.len() - 1].1;
    let required = map_remove_cow_words_required(current, cells[1].0).unwrap();
    assert_eq!(required, 2 + 3);
    let mut out = Buf::new(required);
    // SAFETY: as above.
    let collapsed = unsafe { map_remove_cow(out.base(), current, cells[1].0, &atoms) }
        .unwrap()
        .expect("the key is present");
    out.check_tail();
    assert_content(
        collapsed,
        &without(&without(&hand_pairs(&root), cells[0].0, &atoms), cells[1].0, &atoms),
        &atoms,
    );
    assert_eq!(map_size(collapsed, &atoms).unwrap(), 33);
    chain.push((out, collapsed));

    // 33 -> 32: at most `MAP_SMALL_MAP_LIMIT` keys, so the result is a flat map again.
    let current = chain[chain.len() - 1].1;
    let (o, flat) = cow_remove(current, cells[2].0, &atoms);
    let flat = flat.expect("the key is present");
    assert!(unsafe { is_flatmap(flat) }, "32 pairs must be flat again");
    assert_eq!(map_size(flat, &atoms).unwrap(), 32);
    assert_content(
        flat,
        &without(
            &without(&without(&hand_pairs(&root), cells[0].0, &atoms), cells[1].0, &atoms),
            cells[2].0,
            &atoms,
        ),
        &atoms,
    );
    chain.push((o, flat));
    let _ = chain;
}

#[test]
fn a_wide_collision_node_is_refused_before_any_write() {
    let atoms = atoms();
    let cells: Vec<(Eterm, Eterm)> = slot_keys(2, 20, 500_000)
        .into_iter()
        .map(|k| (small(k), small(k * 10)))
        .collect();
    let root = Hand::Node(vec![(2, Hand::Collision(cells.clone()))]);
    let (_src, map) = build_hand_map(&root);
    assert_eq!(map_size(map, &atoms).unwrap(), 20);

    let key = cells[0].0;
    // Both figures refuse the same map the writers refuse, so a `required` figure can
    // never lead a caller into a reservation the write then exceeds.
    assert_eq!(map_put_cow_words_required(map, key), Err(TermError::BadMap));
    assert_eq!(
        map_remove_cow_words_required(map, key),
        Err(TermError::BadMap)
    );

    let mut out = Buf::new(map_put_cow_words_capacity(20));
    // SAFETY: the buffer reserved the size-only capacity bound; the plan refuses before
    // a word is written.
    assert_eq!(
        unsafe { map_put_cow(out.base(), map, key, small(1), &atoms) },
        Err(TermError::BadMap)
    );
    assert!(out.is_pristine(), "a refused put writes nothing");
    out.check_tail();

    let mut out = Buf::new(map_remove_cow_words_capacity(20));
    // SAFETY: as above.
    assert_eq!(
        unsafe { map_remove_cow(out.base(), map, key, &atoms) },
        Err(TermError::BadMap)
    );
    assert!(out.is_pristine(), "a refused remove writes nothing");
    out.check_tail();
}

#[test]
fn the_collision_cell_limits_are_where_the_word_bounds_put_them() {
    let atoms = atoms();
    // The thresholds are derived from the *widest* spine, so a shallow hand-built map
    // refuses at exactly the same count — that is what makes them a sound bound for
    // every shape rather than for this one.
    let at_limit = |cells: usize| {
        let root = Hand::Node(vec![(
            2,
            Hand::Collision(
                slot_keys(2, cells, 500_000)
                    .into_iter()
                    .map(|k| (small(k), small(k * 10)))
                    .collect(),
            ),
        )]);
        build_hand_map(&root)
    };
    let key_of = |map: Eterm| collision_cell_keys(map, 2)[0];

    // Put: `HAMT_PUT_COLLISION_CELLS_MAX` cells fit; one more is refused. The key is
    // absent from the node, so this is the insert arm.
    let (_a, ok) = at_limit(HAMT_PUT_COLLISION_CELLS_MAX);
    let fresh = slot_keys(2, 1, 900_000)[0];
    let required = map_put_cow_words_required(ok, small(fresh)).unwrap();
    // The head (`2 + 1` slot), the grown node (`1 + cells + 1`) and the new leaf.
    assert_eq!(required, (2 + 1) + (1 + HAMT_PUT_COLLISION_CELLS_MAX + 1) + 2);
    let mut out = Buf::new(required);
    // SAFETY: exactly the figure `map_put_cow_words_required` reported.
    assert!(unsafe { map_put_cow(out.base(), ok, small(fresh), small(1), &atoms) }.is_ok());
    out.check_tail();
    let (_b, too_wide) = at_limit(HAMT_PUT_COLLISION_CELLS_MAX + 1);
    assert_eq!(
        map_put_cow_words_required(too_wide, key_of(too_wide)),
        Err(TermError::BadMap)
    );

    // Remove: `HAMT_REMOVE_COLLISION_CELLS_MAX` cells fit; one more is refused.
    let (_c, ok) = at_limit(HAMT_REMOVE_COLLISION_CELLS_MAX);
    assert!(map_remove_cow_words_required(ok, key_of(ok)).is_ok());
    let (_d, too_wide) = at_limit(HAMT_REMOVE_COLLISION_CELLS_MAX + 1);
    assert_eq!(
        map_remove_cow_words_required(too_wide, key_of(too_wide)),
        Err(TermError::BadMap)
    );
}

#[test]
fn a_one_cell_collision_node_is_refused() {
    let atoms = atoms();
    // A collision node exists only for keys sharing a whole `erts_map_hash`, so it holds
    // two cells or more; a one-cell node is refused rather than read (its collapse arm
    // would look for a sibling cell that is not there).
    let mut buf = Buf::new(3 + 3);
    let base = buf.base();
    let key = slot_keys(2, 1, 500_000)[0];
    // SAFETY: `buf` reserved the head (`2 + 1` slot) and a one-cell collision node
    // (`1 + 1`, two words of a one-cell cons cell).
    unsafe {
        *base = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 1 << 2);
        *base.add(1) = Eterm::from_raw(1);
        *base.add(2) = make_boxed(base.add(3));
        *base.add(3) = make_arityval(1);
        *base.add(4) = small(key);
        *base.add(5) = small(1);
    }
    let map = make_boxed(base as *const Eterm);
    assert_eq!(
        map_put_cow_words_required(map, small(key)),
        Err(TermError::BadMap)
    );
    assert_eq!(
        map_remove_cow_words_required(map, small(key)),
        Err(TermError::BadMap)
    );
    let mut out = Buf::new(map_put_cow_words_capacity(1));
    // SAFETY: the buffer reserved the capacity bound; the plan refuses before a write.
    assert_eq!(
        unsafe { map_put_cow(out.base(), map, small(key), small(2), &atoms) },
        Err(TermError::BadMap)
    );
    assert!(out.is_pristine());
    out.check_tail();
}

#[test]
fn a_flat_map_with_an_inconsistent_head_word_is_refused() {
    let atoms = atoms();
    let base_pairs = pairs_1_to(3);
    let (src, map) = build_map(&base_pairs, &atoms);
    let head = boxed_val(map) as *mut Eterm;
    // The pair count lives in two places — the head's `size` word and the keys tuple's
    // arity — and every builder writes both. A forged map where they disagree is
    // refused (the reservation and the writer's extent must not be able to disagree),
    // in *both* directions: an understated head word and an overstated one.
    for forged in [1usize, 5] {
        // SAFETY: `map` is a live flat map, so word 1 is its `size` word.
        unsafe { *head.add(1) = Eterm::from_raw(forged) };
        assert_eq!(
            map_put_cow_words_required(map, small(9)),
            Err(TermError::BadMap),
            "head word {forged} vs a 3-pair keys tuple"
        );
        assert_eq!(
            map_remove_cow_words_required(map, small(1)),
            Err(TermError::BadMap)
        );
        let mut out = Buf::new(map_put_cow_words_capacity(3));
        // SAFETY: the buffer reserved the capacity bound; the plan refuses before a
        // write, so a forged extent cannot reach the reservation either.
        assert_eq!(
            unsafe { map_put_cow(out.base(), map, small(9), small(90), &atoms) },
            Err(TermError::BadMap)
        );
        assert!(out.is_pristine());
        out.check_tail();
        let mut out = Buf::new(map_remove_cow_words_capacity(3));
        // SAFETY: as above.
        assert_eq!(
            unsafe { map_remove_cow(out.base(), map, small(1), &atoms) },
            Err(TermError::BadMap)
        );
        assert!(out.is_pristine());
        out.check_tail();
    }
    let _ = src;
}

