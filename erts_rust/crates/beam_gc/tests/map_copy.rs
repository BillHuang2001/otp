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

//! Copy-correctness evidence for the **map** term shapes through a collection.
//!
//! A map is a boxed term with one of two interiors owned by `beam_term::map`: a
//! **flat map** (`MAP_HEADER_FLATMAP`, at most [`MAP_SMALL_MAP_LIMIT`] = 32 pairs,
//! whose keys are a back-pointing tuple laid out *before* the head word) and a
//! **hash map** — a 16-way HAMT/CHAMP trie whose nodes are themselves boxed map
//! headers, whose leaves are two-word `[Key, Value]` cons cells and whose collision
//! nodes are transparent `arityval` tuples. Both shapes are sized and scanned by
//! `beam_gc::copy::move_size` / the Cheney scan, so a mis-sized or misaligned walk
//! would garble the copied map (dangling slots, a wrong key count).
//!
//! These tests build each shape in a real [`HeapSpace`], install the map as the sole
//! root, run a collection, and then assert **from the updated root** that (a) the
//! root moved into the to-space, (b) `map_size` is preserved, (c) **every** key still
//! resolves via `map_get`, (d) `map_pairs` round-trips, and (e) unreachable garbage
//! was dropped — the to-space holds exactly the map's independently-measured
//! footprint and the reclaimed words are exactly the from-space garbage.
//!
//! They exercise the public API only; every `unsafe` block carries a `// SAFETY:`
//! rationale and only touches words this test allocated itself.

use beam_alloc::default_allocator;
use beam_gc::prelude::{collect, HeapSpace};
use beam_term::prelude::{
    boxed_val, flatmap_size_words, list_val, make_arityval, make_boxed, make_list, make_map_header,
    make_small, map_get, map_pairs, map_size, map_words_capacity, write_map, AtomTable, Eterm,
    MAP_HEADER_TAG_HAMT_HEAD_BITMAP, MAP_SMALL_MAP_LIMIT,
};

/// Allocates an empty `words`-word semi-space over the default allocator.
fn space(words: usize) -> HeapSpace {
    HeapSpace::with_allocator(default_allocator().clone(), words).expect("heap allocation")
}

/// The `i -> 3*i` input pair set the tests build maps from (`n` distinct keys).
fn pairs(n: usize) -> Vec<(Eterm, Eterm)> {
    (0..n as i64)
        .map(|i| (make_small(i), make_small(i * 3)))
        .collect()
}

/// A sentinel word written into a reserved-but-unwritten region so a map's *actual*
/// contiguous footprint can be recovered by scanning back over the words `write_map`
/// did not touch. Its 6-bit subtag is the reserved `0x3C` (no map/node/cons/immediate
/// word `write_map` produces has that subtag), so it never collides with real data —
/// making the measurement independent of every size heuristic in the codebase.
const MARKER: usize = 0xDEAD_BEEF_DEAD_BE3C;

/// The hash-map test needs `> MAP_SMALL_MAP_LIMIT` keys; the flat-map test needs the
/// converse. Checked at compile time so a change to the limit fails the build.
const _: () = assert!(500 > MAP_SMALL_MAP_LIMIT);
const _: () = assert!(12 <= MAP_SMALL_MAP_LIMIT);

/// Builds the `n`-key `i -> 3*i` map in a fresh space and returns
/// `(space, map, footprint_words)`.
///
/// `footprint_words` is the exact number of words the map occupies, measured by
/// pre-filling a scratch reservation with [`MARKER`] and scanning back to the last
/// word `write_map` overwrote. The returned space reserves exactly that many words
/// (plus `slack` spare words the caller may fill with garbage), so
/// `space.used_words() == footprint_words` right after the call.
fn build_map(n: usize, atoms: &AtomTable, slack: usize) -> (HeapSpace, Eterm, usize) {
    let input = pairs(n);
    let cap = map_words_capacity(n);

    // Pass 1: measure the map's contiguous footprint (write_map lays the cons cells
    // and the trie nodes out as one gap-free prefix of the reservation).
    let footprint = {
        let mut scratch = space(cap);
        // SAFETY: `cap` exclusively-owned, writable words are reserved; filling them
        // with the marker is a plain write, and `write_map` then overwrites a
        // contiguous prefix of them with well-formed terms.
        unsafe {
            let hp = scratch
                .heap_mut()
                .alloc_words(cap)
                .expect("reserve the scratch block");
            for i in 0..cap {
                *hp.add(i) = Eterm::from_raw(MARKER);
            }
            write_map(hp, &input, atoms).expect("build the scratch map");
            let mut end = cap;
            // Scan back over untouched markers: the first non-marker word is the last
            // word write_map wrote.
            while end > 0 && (*hp.add(end - 1)).raw() == MARKER {
                end -= 1;
            }
            end
        }
    };
    assert!(footprint > 0 && footprint <= cap);

    // Pass 2: build the real map with an *exact* reservation (write_map is
    // deterministic, so it writes precisely `footprint` words again).
    let mut from = space(footprint + slack);
    // SAFETY: `footprint` was just measured as exactly the words `write_map` writes,
    // so the reservation is exact; every written word stays live in `from`.
    let map = unsafe {
        let hp = from
            .heap_mut()
            .alloc_words(footprint)
            .expect("reserve the exact map block");
        write_map(hp, &input, atoms).expect("build the map")
    };
    assert_eq!(from.used_words(), footprint);
    (from, map, footprint)
}

/// Asserts that `map` is a well-formed `n`-key map of the `i -> 3*i` pair set.
fn assert_pairs_round_trip(map: Eterm, n: usize, atoms: &AtomTable) {
    assert_eq!(map_size(map, atoms).expect("map_size"), n);
    for i in 0..n as i64 {
        assert_eq!(
            map_get(map, make_small(i), atoms).expect("map_get"),
            Some(make_small(i * 3)),
            "key {i} must resolve to its value"
        );
    }
    let pairs = map_pairs(map, atoms).expect("map_pairs");
    assert_eq!(pairs.len(), n);
    // Small-integer keys sort numerically ascending, so the pair set's key order is
    // exactly `0..n`.
    let keys: Vec<Eterm> = pairs.into_iter().map(|(k, _)| k).collect();
    let expected: Vec<Eterm> = (0..n as i64).map(make_small).collect();
    assert_eq!(keys, expected, "the key set must be exactly `0..n`");
}

#[test]
fn hamt_map_survives_collection_intact() {
    const N: usize = 500; // > MAP_SMALL_MAP_LIMIT => a HAMT (hash) map

    let atoms = AtomTable::new();
    let (mut from, map, footprint) = build_map(N, &atoms, 3);

    // The map under-fills its sound upper bound, so this really is a deep HAMT trie
    // (cons cells + inner nodes), not a single flat node.
    assert!(footprint < map_words_capacity(N));
    assert_eq!(from.used_words(), footprint);
    assert_pairs_round_trip(map, N, &atoms);

    // Deliberate garbage: an unreferenced `{7,8}` tuple.
    let _garbage = from
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("garbage tuple");
    assert_eq!(from.used_words(), footprint + 3);

    let mut to = space(footprint);
    let mut roots = [map];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    // (a) The root moved into the to-space, out of the from-space.
    let copied = roots[0];
    assert!(
        to.contains_term(copied),
        "root must point into the to-space"
    );
    assert!(
        !from.contains_term(copied),
        "root must no longer point into the from-space"
    );

    // (b)+(c)+(d) The map survived the Cheney scan intact.
    assert_pairs_round_trip(copied, N, &atoms);

    // (e) Garbage dropped: the to-space holds exactly the live map; the reclaimed
    // words are exactly the deliberate garbage (the reservation was exact).
    assert_eq!(
        to.used_words(),
        footprint,
        "only the live {footprint}-word map is in the to-space"
    );
    assert_eq!(stats.words_copied, footprint);
    assert_eq!(stats.words_reclaimed, 3, "only the {{7,8}} garbage tuple");
}

#[test]
fn flat_map_survives_collection_intact() {
    const N: usize = 12; // <= MAP_SMALL_MAP_LIMIT => a flat map

    let atoms = AtomTable::new();

    // A flat map is sized *exactly* by `flatmap_size_words` ([`map_words_capacity`]
    // returns it for `n <= 32`), so the block is reserved precisely and the measured
    // footprint agrees with the layout formula.
    let words = flatmap_size_words(N);
    assert_eq!(map_words_capacity(N), words, "flat maps are sized exactly");
    assert_eq!(words, 4 + 2 * N);

    let (mut from, map, footprint) = build_map(N, &atoms, 3);
    assert_eq!(
        footprint, words,
        "the measured footprint is the layout size"
    );

    // Deliberate garbage: an unreferenced `{7,8}` tuple (3 words).
    let _garbage = from
        .heap_mut()
        .tuple(&[make_small(7), make_small(8)])
        .expect("garbage tuple");
    assert_eq!(from.used_words(), words + 3);
    assert_pairs_round_trip(map, N, &atoms);

    let mut to = space(words);
    let mut roots = [map];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    let copied = roots[0];
    assert!(
        to.contains_term(copied),
        "root must point into the to-space"
    );
    assert!(!from.contains_term(copied));

    // The flat-map keys back-pointer (a tuple laid out *before* the head word) is
    // forwarded to a fresh to-space copy, so `map_size`/`map_get`/`map_pairs` all
    // still resolve.
    assert_pairs_round_trip(copied, N, &atoms);

    assert_eq!(to.used_words(), words, "only the live flat map is copied");
    assert_eq!(stats.words_copied, words);
    assert_eq!(stats.words_reclaimed, 3, "only the {{7,8}} garbage tuple");
    // A flat map has no per-key cons cells: the only boxed objects are the head
    // block (the root) and the back-pointing keys tuple.
    assert_eq!(
        stats.objects_copied, 2,
        "the flat-map head + its keys tuple"
    );
}

/// The focused `move_size` proof: for a range of sizes spanning both shapes, the
/// copier's word accounting equals the map's *independently measured* footprint — and
/// a to-space sized exactly to that footprint still fits the copy precisely.
#[test]
fn map_copy_size_equals_the_measured_footprint() {
    let atoms = AtomTable::new();

    for &n in &[1usize, 5, 12, 32, 33, 100, 500] {
        let (from, map, footprint) = build_map(n, &atoms, 0);
        assert_eq!(from.used_words(), footprint);
        if n <= MAP_SMALL_MAP_LIMIT {
            assert_eq!(footprint, flatmap_size_words(n), "flat map n = {n}");
        } else {
            assert!(
                footprint < map_words_capacity(n),
                "a hash map under-fills its upper bound (n = {n})"
            );
        }

        // A to-space sized *exactly* to the measured footprint: the copy must fit.
        let mut to = space(footprint);
        let mut roots = [map];
        let stats = collect(&from, &mut to, &mut roots).expect("collect");

        assert_eq!(to.used_words(), footprint, "copied size for n = {n}");
        assert_eq!(stats.words_copied, footprint, "words_copied for n = {n}");
        assert_eq!(stats.words_reclaimed, 0, "the whole from-space was live");
        assert_eq!(
            map_get(roots[0], make_small(n as i64 - 1), &atoms).expect("map_get"),
            Some(make_small((n as i64 - 1) * 3)),
            "the largest key resolves for n = {n}"
        );
    }
}

/// A HAMT slot may hold a **collision node**: a *transparent* `arityval` tuple of the
/// `[Key, Value]` cons cells of keys that share a full 64-bit hash (`erl_map.c` builds
/// one at `HAMT_LAST_NODE_LEVEL`). `beam_term`'s structural `term_hash` is injective for
/// a fixed prefix (`mix` xors then multiplies by the odd `FNV_PRIME`), so integer keys
/// can never produce a genuine collision; the test therefore hand-builds the minimal
/// valid node — a head-bitmap whose single slot points at a two-cell collision tuple —
/// and proves the copier copies the tuple and its cells intact. It is the *same* scan
/// path a real, deeply-nested collision node takes: the copier sees a boxed slot, sizes
/// the tuple as `arity + 1`, then walks the tuple's transparent header to forward each
/// cons cell individually.
#[test]
fn hashed_map_with_a_collision_node_is_copied() {
    // Layout (10 words): `[k0,v0] [k1,v1] [arityval(2),cell0,cell1] [head,size,slot]`.
    const WORDS: usize = 10;
    let mut from = space(WORDS);
    // SAFETY: `from` owns `WORDS` words; every word written below is in-bounds and part
    // of the single, live map graph rooted at `root`.
    let root = unsafe {
        let hp = from
            .heap_mut()
            .alloc_words(WORDS)
            .expect("reserve the minimal map block");
        // The two `[Key, Value]` cons cells (both immediates).
        *hp = make_small(0);
        *hp.add(1) = make_small(0);
        *hp.add(2) = make_small(1);
        *hp.add(3) = make_small(3);
        let cell0 = make_list(hp as *const Eterm);
        let cell1 = make_list(hp.add(2) as *const Eterm);
        // The collision tuple: a transparent `arityval(2)` of the two cells.
        let coll = hp.add(4);
        *coll = make_arityval(2);
        *coll.add(1) = cell0;
        *coll.add(2) = cell1;
        // The head-bitmap node: `[hdr, size=2, slot0 -> collision tuple]`.
        let head = hp.add(7);
        *head = make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 1);
        *head.add(1) = Eterm::from_raw(2);
        *head.add(2) = make_boxed(coll);
        make_boxed(head)
    };
    assert_eq!(from.used_words(), WORDS);

    let mut to = space(WORDS);
    let mut roots = [root];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    // The head node + the collision tuple + the two cons cells.
    let copied = roots[0];
    assert!(
        to.contains_term(copied),
        "root must point into the to-space"
    );
    assert!(!from.contains_term(copied));
    assert_eq!(stats.words_copied, WORDS);
    assert_eq!(stats.objects_copied, 4);
    assert_eq!(to.used_words(), WORDS);

    // The copied head node keeps its header and size word.
    // SAFETY: `copied` is the live, copied 3-word head node.
    let head = boxed_val(copied);
    assert_eq!(
        unsafe { *head },
        make_map_header(MAP_HEADER_TAG_HAMT_HEAD_BITMAP, 1, 1)
    );
    assert_eq!(unsafe { *head.add(1) }.raw(), 2);

    // Its slot was forwarded to a copied collision tuple, whose cells were forwarded to
    // copied cons cells carrying the original keys and values.
    let slot = unsafe { *head.add(2) };
    assert!(to.contains_term(slot), "the collision tuple must be copied");
    // SAFETY: `slot` is the live, copied collision tuple.
    let coll = boxed_val(slot);
    assert_eq!(unsafe { *coll }, make_arityval(2));
    // SAFETY: the tuple holds two cons-cell terms (in-bounds words 1 and 2).
    let (c0, c1) = unsafe { (*coll.add(1), *coll.add(2)) };
    assert!(to.contains_term(c0) && to.contains_term(c1));
    // SAFETY: `c0`/`c1` are live, copied cons cells.
    unsafe {
        assert_eq!(*list_val(c0), make_small(0));
        assert_eq!(*list_val(c0).add(1), make_small(0));
        assert_eq!(*list_val(c1), make_small(1));
        assert_eq!(*list_val(c1).add(1), make_small(3));
    }
}
