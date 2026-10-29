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
use crate::boxed::{boxed_val, is_flatmap, is_hashmap, map_header_type, map_header_val};
use crate::compare::eq;
use crate::error::TermError;
use crate::ihash::{erts_map_hash, hashmap_nibble};
use crate::immediate::make_small;
use crate::map::{
    map_get, map_pairs, map_pairs_storage_order, map_size, map_words_capacity, write_map,
    HAMT_MAX_LEVEL, MAP_SMALL_MAP_LIMIT,
};
use crate::tag::{MAP_HEADER_TAG_HAMT_HEAD_ARRAY, MAP_HEADER_TAG_HAMT_HEAD_BITMAP};

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
