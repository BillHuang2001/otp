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

//! Small-map (flat-map) key **ordering**: the discriminating pin for the
//! `erts_cmp_flatmap_keys` rule.
//!
//! A flat map (at most [`MAP_SMALL_MAP_LIMIT`] pairs) stores its keys sorted by
//! the C's **internal** flat-map key order, `erts_cmp_flatmap_keys`
//! (`erts/emulator/beam/erl_utils.h:263-269`):
//!
//! ```c
//! if (is_atom(key_a) && is_atom(key_b)) {
//!     return key_a - key_b;          /* raw atom word == atom-table INDEX */
//! }
//! return erts_cmp(key_a, key_b, 1, 0); /* everything else: full term order */
//! ```
//!
//! so two **atom** keys order by their atom-table *index* — for a seeded table,
//! by interning order — and *not* by name, which is the order
//! [`beam_term::compare::cmp`] (and therefore [`map_pairs`]) reports.
//!
//! # Why this file exists
//!
//! The neighbouring suites cannot discriminate the two rules:
//!
//! * `tests/oracle.rs`'s flat-map atom check and `tests/map_hash.rs`'s atom
//!   fixtures use atoms whose two orders **coincide** (atom indices seeded from
//!   `atom.names`, whose file order is largely name-sorted, or a single inverted
//!   pair) — a `write_map` that sorted atoms by name would still pass;
//! * `src/map.rs`'s own `atom_keys_use_index_order_in_a_flatmap` inverts *one*
//!   pair, so "index order" and "name order with a swapped pair" are the same
//!   observation.
//!
//! Everything here therefore starts from an [`AtomTable`] whose **index order is
//! neither the name order nor the order the pairs arrive in**, so every
//! assertion below fails for a name-ordered or insertion-ordered implementation.
//!
//! # What is pinned
//!
//! * [`write_map`]'s stored key order is **atom-index** order (the flat-map
//!   keys tuple), and [`map_pairs_storage_order`] returns exactly that;
//! * [`map_pairs`] is the `cmp` (**atom-name**) order instead — a different
//!   order, which is *not* what an image-preserving serializer may use;
//! * a mixed-class key set keeps the C class ladder for the non-atom keys while
//!   the atom subsequence stays in index order;
//! * `write_map`'s output words are **insertion-order free**: the input pairs
//!   reversed produce byte-identical words (modulo the block's own base — the
//!   permanent keys tuple is a heap pointer), and a duplicate key leaves no trace.

use beam_term::prelude::*;

/// A word written around every term buffer to detect any overflow of the reserved
/// capacity: `write_map` must never touch it.
const SENTINEL: Eterm = Eterm::from_raw(0xDEAD_BEEF);

/// Builds a map from `pairs` into a sentinel-guarded buffer and returns the buffer
/// plus the tagged map root. The buffer owns the term's storage and must stay
/// alive while `map` is used.
fn build(pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> (Vec<Eterm>, Eterm) {
    let cap = map_words_capacity(pairs.len());
    let mut buf = vec![SENTINEL; cap + 64];
    let base = buf.as_mut_ptr();
    // SAFETY: `buf` owns `cap + 64 >= map_words_capacity(pairs.len())` words, and it
    // is kept alive by the returned buffer.
    let map = unsafe { write_map(base, pairs, atoms).expect("write_map") };
    for i in 0..64 {
        assert_eq!(
            buf[cap + i],
            SENTINEL,
            "write_map overflowed map_words_capacity by at least {} words",
            i + 1
        );
    }
    (buf, map)
}

/// The stored key terms of `map`, in the order the map stores them.
fn stored_keys(map: Eterm) -> Vec<Eterm> {
    map_pairs_storage_order(map)
        .expect("map_pairs_storage_order")
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

/// Normalizes a map's words for **base-independent** comparison: each word becomes
/// `(true, offset)` when it is a tagged pointer into `words` (relative to `base`,
/// the block's first word), else `(false, raw)`. Two canonically laid-out maps built
/// in different buffers normalize equal iff their shapes are identical — the flat
/// map's keys *tuple* is one heap pointer even when every key and value is an
/// immediate.
fn normalize(words: &[Eterm], base: *const Eterm) -> Vec<(bool, usize)> {
    let base_addr = base as usize;
    let end = base_addr + std::mem::size_of_val(words);
    words
        .iter()
        .map(|w| {
            let raw = w.raw();
            // LIST (0x1) and BOXED (0x2) primary tags carry a tagged pointer.
            if raw & 0x3 == 0x1 || raw & 0x3 == 0x2 {
                let p = raw & !0x7usize;
                if p >= base_addr && p < end {
                    return (true, p - base_addr);
                }
            }
            (false, raw)
        })
        .collect()
}

/// The keys of `map` as atom names, in the order the map stores them.
fn stored_names(map: Eterm, atoms: &AtomTable) -> Vec<String> {
    stored_keys(map)
        .into_iter()
        .map(|k| {
            assert!(is_atom(k), "expected an atom key, got a non-atom");
            atoms
                .name(atom_val(k))
                .expect("a key's name is in the passed table")
                .to_owned()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The rule: atom keys order by index, every other key by term order
// ---------------------------------------------------------------------------

/// A five-atom table whose index order is neither the name order nor the input
/// pair order, so all three orders are distinguishable by one assertion.
///
/// Interning order (= index order) is `delta, charlie, bravo, alpha, echo`;
/// name (`cmp`) order is `alpha, bravo, charlie, delta, echo`; the map below is
/// fed as `alpha, echo, delta, bravo, charlie`.
const SCRAMBLED: &[&str] = &["delta", "charlie", "bravo", "alpha", "echo"];
const INPUT_ORDER: &[&str] = &["alpha", "echo", "delta", "bravo", "charlie"];

/// The stored key order of a flat map is **atom-index** order, which here is
/// neither the name order `map_pairs` reports nor the insertion order.
#[test]
fn stored_order_is_atom_index_order() {
    let mut atoms = AtomTable::new();
    atoms.seed(SCRAMBLED.iter().copied());

    // The premise: the three orders really are different.
    let index_order: Vec<&str> = SCRAMBLED.to_vec();
    let name_order: Vec<&str> = {
        let mut v = SCRAMBLED.to_vec();
        v.sort_unstable();
        v
    };
    assert_eq!(index_order, ["delta", "charlie", "bravo", "alpha", "echo"]);
    assert_eq!(name_order, ["alpha", "bravo", "charlie", "delta", "echo"]);
    assert_ne!(index_order, name_order);
    assert_ne!(index_order, INPUT_ORDER);
    assert_ne!(name_order, INPUT_ORDER);

    // Build with the pairs in a fourth, deliberately unrelated order.
    let pairs: Vec<(Eterm, Eterm)> = INPUT_ORDER
        .iter()
        .enumerate()
        .map(|(ordinal, name)| {
            let atom = atoms.lookup(name).expect("seeded");
            (make_atom(atom), make_small(ordinal as i64))
        })
        .collect();
    let (_buf, map) = build(&pairs, &atoms);
    // SAFETY: `map` is a live boxed map with five pairs.
    assert!(unsafe { is_flatmap(map) }, "5 keys must build a flat map");

    // 1. The stored order IS the index order (what the C's `key_a - key_b` means).
    assert_eq!(
        stored_names(map, &atoms),
        index_order,
        "a flat map stores atom keys by atom index (interning order)",
    );

    // 2. `map_pairs_storage_order` reports that stored order, pair for pair, and
    //    it is not the (canonical) `cmp` order — the serializer's order is the
    //    storage one.
    let stored = map_pairs_storage_order(map).expect("storage order");
    assert_eq!(stored.len(), 5);
    for (key, value) in &stored {
        // The ordinal is the key's position in INPUT_ORDER.
        let ordinal = signed_value(*value) as usize;
        assert_eq!(INPUT_ORDER[ordinal], atoms.name(atom_val(*key)).unwrap());
    }

    // 3. `map_pairs` is the name (`cmp`) order instead, and the two differ.
    let canonical: Vec<String> = map_pairs(map, &atoms)
        .expect("map_pairs")
        .into_iter()
        .map(|(k, _)| atoms.name(atom_val(k)).unwrap().to_owned())
        .collect();
    assert_eq!(canonical, name_order, "map_pairs sorts in `cmp`/name order");
    assert_ne!(
        canonical,
        stored_names(map, &atoms),
        "the name order and the stored order must not coincide in this fixture",
    );

    // 4. `cmp_flatmap_keys` is the order the writer used (atoms: by index).
    let keys = stored_keys(map);
    for pair in keys.windows(2) {
        assert_eq!(
            cmp_flatmap_keys(pair[0], pair[1], &atoms),
            std::cmp::Ordering::Less,
            "consecutive stored keys must be strictly ascending in cmp_flatmap_keys",
        );
        // ... while the same pair is *not* name-ascending wherever the index order
        // disagrees: `delta` (index 0) sorts after `charlie` by name.
        if atoms.name(atom_val(pair[0])).unwrap() > atoms.name(atom_val(pair[1])).unwrap() {
            assert_eq!(
                cmp(pair[0], pair[1], &atoms),
                std::cmp::Ordering::Greater,
                "the test fixture must contain an index-vs-name inversion",
            );
        }
    }

    // 5. Every value still follows its key.
    for (key, value) in map_pairs(map, &atoms).expect("map_pairs") {
        let ordinal = signed_value(value) as usize;
        assert_eq!(INPUT_ORDER[ordinal], atoms.name(atom_val(key)).unwrap());
    }
}

/// Mixed-class keys: the atom subsequence stays in **index** order while the
/// non-atom keys keep the C class ladder (`small < atom < tuple < nil < list`),
/// so the whole stored sequence is exactly [`cmp_flatmap_keys`] order.
#[test]
fn mixed_class_keys_follow_cmp_flatmap_keys_verbatim() {
    let mut atoms = AtomTable::new();
    // Index order (`zzz_mixed`, `aaa_mixed`) contradicts name order.
    atoms.seed(["zzz_mixed", "aaa_mixed"]);
    let zzz = make_atom(atoms.lookup("zzz_mixed").unwrap());
    let aaa = make_atom(atoms.lookup("aaa_mixed").unwrap());
    assert!(zzz.raw() < aaa.raw(), "premise: `zzz_mixed` has the lower index");

    // A heap-backed tuple and list key (their payloads must outlive the map).
    let mut heap = Heap::new(16);
    let tuple_key = heap.tuple(&[make_small(7)]).unwrap();
    let list_key = heap.cons(make_small(1), NIL).unwrap();

    let pairs = [
        (list_key, make_small(4)),
        (aaa, make_small(1)),
        (make_small(3), make_small(0)),
        (zzz, make_small(2)),
        (tuple_key, make_small(3)),
        (NIL, make_small(5)),
    ];
    let (_buf, map) = build(&pairs, &atoms);
    // SAFETY: `map` is a live boxed map with six pairs.
    assert!(unsafe { is_flatmap(map) }, "6 keys must build a flat map");

    // The stored order equals `cmp_flatmap_keys` order over the same key set.
    let mut expected: Vec<(Eterm, Eterm)> = pairs.to_vec();
    expected.sort_by(|a, b| cmp_flatmap_keys(a.0, b.0, &atoms));
    assert_eq!(
        map_pairs_storage_order(map).expect("storage order"),
        expected,
        "stored order must be exactly cmp_flatmap_keys order",
    );

    // The atom subsequence is index order (`zzz_mixed` before `aaa_mixed`) even
    // though the name order is the reverse, and the classes stay segregated.
    let stored = stored_keys(map);
    let atoms_in_order: Vec<String> = stored
        .iter()
        .filter(|k| is_atom(**k))
        .map(|k| atoms.name(atom_val(*k)).unwrap().to_owned())
        .collect();
    assert_eq!(atoms_in_order, ["zzz_mixed", "aaa_mixed"]);
    assert_eq!(
        stored[0],
        make_small(3),
        "a small integer precedes every atom",
    );
    assert_eq!(*stored.last().unwrap(), list_key, "a list sorts last here");

    // `map_pairs` is the `cmp` order, so the atom subsequence flips to name order.
    let canonical: Vec<String> = map_pairs(map, &atoms)
        .expect("map_pairs")
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| is_atom(*k))
        .map(|k| atoms.name(atom_val(k)).unwrap().to_owned())
        .collect();
    assert_eq!(canonical, ["aaa_mixed", "zzz_mixed"]);
}

/// The producer's pair order is irrelevant: reversing the input produces a
/// **byte-identical** block (not merely an `eq`-equal map), so no part of the
/// canonical layout — the keys tuple included — can depend on insertion order.
///
/// The keys and values are immediates only (atoms and small integers), so the one
/// pointer in the block is the keys tuple itself; [`normalize`] makes the two
/// buffers' bases comparable without hiding any other difference.
#[test]
fn reversed_input_pairs_produce_byte_identical_words() {
    let mut atoms = AtomTable::new();
    atoms.seed(SCRAMBLED.iter().copied());

    let forward: Vec<(Eterm, Eterm)> = INPUT_ORDER
        .iter()
        .enumerate()
        .map(|(ordinal, name)| {
            (
                make_atom(atoms.lookup(name).unwrap()),
                make_small(ordinal as i64),
            )
        })
        .collect();
    let mut backward = forward.clone();
    backward.reverse();

    let cap = map_words_capacity(forward.len());
    let (buf_fwd, map_fwd) = build(&forward, &atoms);
    let (buf_rev, map_rev) = build(&backward, &atoms);
    // SAFETY: both maps are live boxed maps with five pairs.
    assert!(unsafe { is_flatmap(map_fwd) } && unsafe { is_flatmap(map_rev) });

    assert_eq!(
        normalize(&buf_fwd[..cap], buf_fwd.as_ptr()),
        normalize(&buf_rev[..cap], buf_rev.as_ptr()),
        "a flat map's words must not depend on the input pair order",
    );
    assert_eq!(stored_keys(map_fwd), stored_keys(map_rev));
    assert_eq!(
        stored_names(map_fwd, &atoms),
        SCRAMBLED.to_vec(),
        "the shared stored order is the atom index order",
    );
    assert!(eq(map_fwd, map_rev, &atoms));
}

/// A duplicate key keeps its **first** key word and takes the **last** value,
/// and the surviving layout is the same as if the duplicate had never been there
/// — on the flat branch (the `collapse_eq_runs` path) and independent of the
/// order the duplicate arrives in.
#[test]
fn duplicate_atom_keys_collapse_without_disturbing_the_order() {
    let mut atoms = AtomTable::new();
    atoms.seed(SCRAMBLED.iter().copied());
    let delta = make_atom(atoms.lookup("delta").unwrap());
    let alpha = make_atom(atoms.lookup("alpha").unwrap());

    let with_dup = [(delta, make_small(1)), (alpha, make_small(2)), (delta, make_small(3))];
    let without_dup = [(delta, make_small(3)), (alpha, make_small(2))];

    let (buf_dup, map_dup) = build(&with_dup, &atoms);
    let (buf_clean, map_clean) = build(&without_dup, &atoms);

    assert_eq!(map_size(map_dup, &atoms).unwrap(), 2);
    assert_eq!(
        map_get(map_dup, delta, &atoms).unwrap(),
        Some(make_small(3)),
        "last value wins",
    );
    assert_eq!(
        stored_names(map_dup, &atoms),
        ["delta", "alpha"],
        "the collapsed map still stores keys in atom index order",
    );
    // The two blocks were reserved for different *input* lengths (3 vs 2 pairs) but
    // the produced map occupies `flatmap_size_words(2)` words either way: the
    // duplicate must leave no trace at all.
    let written = flatmap_size_words(2);
    assert_eq!(
        normalize(&buf_dup[..written], buf_dup.as_ptr()),
        normalize(&buf_clean[..written], buf_clean.as_ptr()),
        "deduplication must leave no trace in the produced flat map",
    );
    assert!(eq(map_dup, map_clean, &atoms));
}
