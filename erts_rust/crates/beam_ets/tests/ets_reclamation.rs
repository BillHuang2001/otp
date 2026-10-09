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

//! Reclamation / storage-lifetime stress: the index's stored key term must
//! **always** alias a live, table-owned object, and every removal must reclaim
//! exactly once.
//!
//! The focused scenarios reproduce the dangling-key bug directly; the stress
//! tests drive insert/delete/upsert churn across every kind and interior shape
//! and assert the observable memory accounting stays bounded (and returns to a
//! stable floor for a balanced insert/delete churn).

mod common;

use beam_ets::{
    key_of, OrderedInterior, SetInterior, Table, TableId, TableKind, TableOptions, TermStore,
};
use beam_term::prelude::*;
use common::*;

/// A table of `kind` with the default interiors.
fn table(kind: TableKind) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// A `set` table with an explicit interior selection.
fn set_table(interior: SetInterior) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::Set);
    Table::with_set_interior(TableId::next(), opts, interior)
        .expect("a fresh table is constructible")
}

/// An `ordered_set` table with an explicit interior selection.
fn ordered_table(interior: OrderedInterior) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);
    Table::with_ordered_interior(TableId::next(), opts, interior)
        .expect("a fresh table is constructible")
}

/// A table of `kind` with an explicit `keypos` — the shape that exercises the
/// keypos-aware re-anchor (`HashInterior::with_keypos`).
fn keypos_table(kind: TableKind, keypos: usize) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    opts.set_keypos(keypos).expect("a keypos >= 1");
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// Every multi-object kind, with a display name.
fn multi_kinds() -> [(&'static str, TableKind); 2] {
    [
        ("bag", TableKind::Bag),
        ("duplicate_bag", TableKind::DuplicateBag),
    ]
}

/// A table builder under stress: a display name and a way to make a fresh table.
type Shape = (&'static str, fn() -> Table);

/// Every observable kind/interior shape the stress suite drives.
fn all_shapes() -> Vec<Shape> {
    vec![
        ("set/champ", || set_table(SetInterior::Champ)),
        ("set/chained", || set_table(SetInterior::Chained)),
        ("ordered_set/b+tree", || {
            ordered_table(OrderedInterior::BPlusTree)
        }),
        ("ordered_set/sorted", || ordered_table(OrderedInterior::Sorted)),
        ("bag", || table(TableKind::Bag)),
        ("duplicate_bag", || table(TableKind::DuplicateBag)),
    ]
}

/// Every key the table yields by walking `first`/`next` (order-insensitive).
fn walked_keys(t: &Table, atoms: &AtomTable) -> Vec<Eterm> {
    let mut keys = Vec::new();
    let mut cursor = t.first(atoms);
    while let Some(key) = cursor {
        keys.push(key);
        match t.next(key, atoms).expect("a stored key is navigable") {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    keys
}

/// The primary leak: a `set` upsert replaces the object (freeing its storage,
/// including the key copy the index aliased) but the index keeps pointing at the
/// freed key term. A later same-size allocation reuses the freed run, so the
/// stored key silently reads a different term — `lookup` then loses the entry.
#[test]
fn set_upsert_keeps_the_key_anchor_alive_for_every_interior() {
    for interior in [SetInterior::Champ, SetInterior::Chained] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = set_table(interior);

        // A boxed key `{7}` so the stored key is a pointer into table storage.
        let key = int_tuple(&mut heap, &[7]);
        let first = tuple_of(&mut heap, &[key, make_small(10)]);
        t.insert(first, &atoms).unwrap();

        // Upsert under the same key: replaces `first`, whose storage (the object
        // tuple *and* its key copy, which the index key aliased) is reclaimed.
        let second = tuple_of(&mut heap, &[key, make_small(20)]);
        t.insert(second, &atoms).unwrap();

        // A fresh, same-shaped object under a different key forces the store to
        // reuse the freed runs, overwriting the words the stale key pointed at.
        let other = int_tuple(&mut heap, &[8]);
        t.insert(tuple_of(&mut heap, &[other, make_small(30)]), &atoms)
            .unwrap();

        assert_eq!(t.size(), 2, "interior={interior:?}");
        let looked = t.lookup(key, &atoms);
        assert_eq!(
            looked.len(),
            1,
            "interior={interior:?}: the upserted entry lost its key anchor"
        );
        assert!(eq(looked[0], second, &atoms), "interior={interior:?}");
        assert!(t.member(key, &atoms), "interior={interior:?}");

        // Navigation must report the live key `{7}` exactly once, not the reused
        // `{8}` in its place.
        let keys = walked_keys(&t, &atoms);
        assert_eq!(keys.len(), 2, "interior={interior:?}");
        assert_eq!(
            keys.iter().filter(|&&k| eq(k, key, &atoms)).count(),
            1,
            "interior={interior:?}: navigation found a dangling key term"
        );
        assert_eq!(keys.iter().filter(|&&k| eq(k, other, &atoms)).count(), 1);
    }
}

/// The multi-kind variant: `delete_object/2` removes the object whose storage the
/// entry's key aliased while the entry survives on its other objects, so the
/// surviving entry must be re-anchored to a live object's own key element — the
/// keypos-aware `HashInterior::with_keypos` path.
///
/// Both multi kinds are driven, and both argument shapes the oracle accepts:
/// the **stored object itself** (the original anchor argument) and a separately
/// built **structurally-equal copy** that is not the stored term. Memory
/// accounting is asserted around the removal (the removed object's storage is
/// reclaimed; the survivor's is not).
#[test]
fn multi_kind_delete_object_keeps_the_key_anchor_alive() {
    for (shape, kind) in multi_kinds() {
        // (a) the argument is the *stored* object (the anchor).
        {
            let atoms = AtomTable::new();
            let mut heap = Heap::new(256);
            let mut t = table(kind);

            let key = int_tuple(&mut heap, &[7]);
            let a = tuple_of(&mut heap, &[key, make_small(10)]);
            let b = tuple_of(&mut heap, &[key, make_small(20)]);
            t.insert(a, &atoms).unwrap();
            t.insert(b, &atoms).unwrap();
            assert_eq!(t.size(), 2, "{shape}");

            let loaded = t.memory();
            // Remove the *first* object, which is the one the entry's key anchors
            // to; the entry survives on `b`.
            t.delete_object(a, &atoms).unwrap();
            assert_eq!(t.size(), 1, "{shape}");
            assert!(
                t.memory() < loaded,
                "{shape}: the removed object's storage was not reclaimed"
            );

            // Reuse the freed runs with same-shaped objects under fresh keys.
            for k in 8..12i64 {
                let other = int_tuple(&mut heap, &[k]);
                t.insert(tuple_of(&mut heap, &[other, make_small(k)]), &atoms)
                    .unwrap();
            }

            let looked = t.lookup(key, &atoms);
            assert_eq!(
                looked.len(),
                1,
                "{shape}: the surviving entry lost its key anchor"
            );
            assert!(eq(looked[0], b, &atoms), "{shape}");
            assert!(t.member(key, &atoms), "{shape}");
        }

        // (b) the argument is a separately built **equal copy**, never the stored
        // term — the shape a caller who re-builds the tuple produces.
        {
            let atoms = AtomTable::new();
            let mut heap = Heap::new(256);
            let mut t = table(kind);

            let key = int_tuple(&mut heap, &[7]);
            let a = tuple_of(&mut heap, &[key, make_small(10)]);
            let b = tuple_of(&mut heap, &[key, make_small(20)]);
            t.insert(a, &atoms).unwrap();
            t.insert(b, &atoms).unwrap();

            let copy = tuple_of(&mut heap, &[key, make_small(10)]);
            assert!(
                eq(copy, a, &atoms) && copy.raw() != a.raw(),
                "{shape}: the argument must be an equal but distinct term"
            );
            t.delete_object(copy, &atoms).unwrap();
            assert_eq!(t.size(), 1, "{shape}: the equal copy removed nothing");

            for k in 8..12i64 {
                let other = int_tuple(&mut heap, &[k]);
                t.insert(tuple_of(&mut heap, &[other, make_small(k)]), &atoms)
                    .unwrap();
            }

            let looked = t.lookup(key, &atoms);
            assert_eq!(
                looked.len(),
                1,
                "{shape}: the copy-deleted entry lost its key anchor"
            );
            assert!(eq(looked[0], b, &atoms), "{shape}");
            assert!(t.member(key, &atoms), "{shape}");
        }

        // (c) a `duplicate_bag` holding two *identical* objects: removing them by
        // an equal copy leaves the third object's entry, re-anchored to it.
        {
            let atoms = AtomTable::new();
            let mut heap = Heap::new(256);
            let mut t = table(kind);

            let key = int_tuple(&mut heap, &[7]);
            let a1 = tuple_of(&mut heap, &[key, make_small(10)]);
            let a2 = tuple_of(&mut heap, &[key, make_small(10)]);
            let b = tuple_of(&mut heap, &[key, make_small(20)]);
            t.insert(a1, &atoms).unwrap();
            t.insert(a2, &atoms).unwrap();
            t.insert(b, &atoms).unwrap();
            // A `bag` keeps one `{7,10}`; a `duplicate_bag` keeps both.
            let expected = if kind == TableKind::Bag { 2 } else { 3 };
            assert_eq!(t.size(), expected, "{shape}");

            let copy = tuple_of(&mut heap, &[key, make_small(10)]);
            t.delete_object(copy, &atoms).unwrap();
            assert_eq!(t.size(), 1, "{shape}: every equal object must go");

            for k in 8..12i64 {
                let other = int_tuple(&mut heap, &[k]);
                t.insert(tuple_of(&mut heap, &[other, make_small(k)]), &atoms)
                    .unwrap();
            }

            let looked = t.lookup(key, &atoms);
            assert_eq!(
                looked.len(),
                1,
                "{shape}: the entry surviving the duplicate purge lost its anchor"
            );
            assert!(eq(looked[0], b, &atoms), "{shape}");
            assert!(t.member(key, &atoms), "{shape}");
        }
    }
}

/// The same re-anchor on a `{keypos, 2}` multi-kind table: the surviving entry's
/// key must be re-derived from **element 2** (the `HashInterior::with_keypos`
/// path). Element 1 of the survivor is a deliberately different term, so a
/// re-anchor that used position 1 (or none at all) would make `lookup/2` miss the
/// entry after the freed words are reused.
#[test]
fn multi_kind_delete_object_reanchors_with_a_custom_keypos() {
    for (shape, kind) in multi_kinds() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(512);
        let mut t = keypos_table(kind, 2);

        // `{Tag, Key}` objects; the key (element 2) is boxed, so the entry's stored
        // key term aliases table-owned storage.
        let key = int_tuple(&mut heap, &[7]);
        let a = tuple_of(&mut heap, &[make_small(1), key]);
        let b = tuple_of(&mut heap, &[make_small(2), key]);
        t.insert(a, &atoms).unwrap();
        t.insert(b, &atoms).unwrap();
        assert_eq!(t.size(), 2, "{shape}");

        let loaded = t.memory();
        t.delete_object(a, &atoms).unwrap();
        assert_eq!(t.size(), 1, "{shape}");
        assert!(
            t.memory() < loaded,
            "{shape}: the removed object's storage was not reclaimed"
        );

        // Same-shaped objects under fresh keys (element 1 is the tag, element 2 the
        // key), so the freed runs are reused by a later insert.
        for k in 8..12i64 {
            let other = int_tuple(&mut heap, &[k]);
            t.insert(tuple_of(&mut heap, &[make_small(k), other]), &atoms)
                .unwrap();
        }

        let looked = t.lookup(key, &atoms);
        assert_eq!(
            looked.len(),
            1,
            "{shape}: the re-anchor used the wrong keypos (or none)"
        );
        assert!(eq(looked[0], b, &atoms), "{shape}");
        assert!(t.member(key, &atoms), "{shape}");
    }
}

/// `delete_object/2` accepts a structurally-equal **copy** (never the stored term)
/// for every kind and interior shape, and reclaims the removed object's storage.
/// Afterwards the freed key's runs are reused and the key must resolve to the
/// freshly inserted object — a stale entry (or a lost anchor) would show up here.
#[test]
fn delete_object_copy_removes_the_object_for_every_shape() {
    for (shape, make) in all_shapes() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(512);
        let mut t = make();

        let key = int_tuple(&mut heap, &[7]);
        let a = tuple_of(&mut heap, &[key, make_small(10)]);
        t.insert(a, &atoms).unwrap();
        assert_eq!(t.size(), 1, "{shape}");
        let loaded = t.memory();

        let copy = tuple_of(&mut heap, &[key, make_small(10)]);
        assert!(
            eq(copy, a, &atoms) && copy.raw() != a.raw(),
            "{shape}: the argument must be an equal but distinct term"
        );
        t.delete_object(copy, &atoms).unwrap();
        assert_eq!(t.size(), 0, "{shape}: the equal copy removed nothing");
        assert!(t.memory() < loaded, "{shape}: storage was not reclaimed");
        assert!(t.lookup(key, &atoms).is_empty(), "{shape}: the entry survived");

        // Reuse the freed runs, then re-insert the key and confirm the fresh object.
        let other = int_tuple(&mut heap, &[8]);
        t.insert(tuple_of(&mut heap, &[other, make_small(30)]), &atoms)
            .unwrap();
        let b = tuple_of(&mut heap, &[key, make_small(20)]);
        t.insert(b, &atoms).unwrap();
        let looked = t.lookup(key, &atoms);
        assert_eq!(looked.len(), 1, "{shape}");
        assert!(eq(looked[0], b, &atoms), "{shape}");
    }
}

/// A `{keypos, 2}` `delete_object/2` on the **non-anchor copy**: the key is
/// derived from element 2 of the caller's copy (never element 1), and the purge
/// still finds and re-anchors the surviving entry.
#[test]
fn multi_kind_delete_object_copy_with_a_custom_keypos() {
    for (shape, kind) in multi_kinds() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(512);
        let mut t = keypos_table(kind, 2);

        let key = int_tuple(&mut heap, &[7]);
        let a = tuple_of(&mut heap, &[make_small(1), key]);
        let b = tuple_of(&mut heap, &[make_small(2), key]);
        t.insert(a, &atoms).unwrap();
        t.insert(b, &atoms).unwrap();

        let copy = tuple_of(&mut heap, &[make_small(1), key]);
        t.delete_object(copy, &atoms).unwrap();
        assert_eq!(t.size(), 1, "{shape}: the copy removed nothing");

        for k in 8..12i64 {
            let other = int_tuple(&mut heap, &[k]);
            t.insert(tuple_of(&mut heap, &[make_small(k), other]), &atoms)
                .unwrap();
        }

        let looked = t.lookup(key, &atoms);
        assert_eq!(looked.len(), 1, "{shape}: the copy-deleted entry lost its key");
        assert!(eq(looked[0], b, &atoms), "{shape}");
    }
}

/// The `ordered_set` B+-tree variant of the same defect, on its **internal
/// separators** rather than its leaf entries.
///
/// A branch separator is the maximum key of a child subtree, stored as a term that
/// aliases the key element inside the object that established that maximum. An
/// upsert of that key replaces the object and reclaims its storage, so a separator
/// kept as-is would read a later, reused word as a different key and misroute the
/// next search — silently inserting a *duplicate* key instead of replacing the
/// existing entry. Enough keys are inserted to build branches (well past the
/// node order), and every key is then upserted, so every separator is hit.
#[test]
fn ordered_set_upsert_keeps_the_subtree_separators_live() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 20);
    let keys: Vec<Eterm> = (0..256i64).map(|i| churn_key(&mut heap, i)).collect();

    for interior in [OrderedInterior::BPlusTree, OrderedInterior::Sorted] {
        let mut t = ordered_table(interior);
        for (n, &key) in keys.iter().enumerate() {
            let object = churn_object(&mut heap, key, n as i64, 0);
            t.insert(object, &atoms).unwrap();
        }
        assert_eq!(t.size(), keys.len(), "interior={interior:?}: the baseline is loaded");

        // Upsert every key with a **freshly built** object, forcing the store to
        // reuse the replaced objects' runs — which is what would expose a stale
        // separator (or a stale leaf key anchor).
        for (n, &key) in keys.iter().enumerate() {
            let object = churn_object(&mut heap, key, n as i64, 1);
            t.insert(object, &atoms).unwrap();
        }
        assert_eq!(
            t.size(),
            keys.len(),
            "interior={interior:?}: an upsert inserted a duplicate instead of replacing"
        );
        for (n, &key) in keys.iter().enumerate() {
            let looked = t.lookup(key, &atoms);
            assert_eq!(looked.len(), 1, "interior={interior:?}: key {n} was lost");
            let want = churn_value(&mut heap, n as i64, 1);
            let stored = tuple_of(&mut heap, &[key, want]);
            assert!(
                eq(looked[0], stored, &atoms),
                "interior={interior:?}: key {n} kept the replaced object"
            );
        }

        // Delete every key and confirm the tree still reports empty.
        for &key in &keys {
            t.delete(key, &atoms).unwrap();
        }
        assert_eq!(t.size(), 0, "interior={interior:?}: the deletes left entries");
        assert_eq!(t.first(&atoms), None, "interior={interior:?}: navigation after deletes");
    }
}

// ---------------------------------------------------------------------------
// Memory-accounting stress
// ---------------------------------------------------------------------------
//
// The scenarios above pin the *key-anchor* invariant; these drive prolonged
// insert/delete/upsert churn with large boxed keys **and** boxed values across
// every kind and interior shape and assert the observable memory accounting
// (`Table::memory` / `TableInfo::memory` / `TermStore::words`) is correct and
// bounded. A balanced insert/delete churn must return to a *stable* floor (the
// store reclaims every object, leaving only the interior's grown index); an
// overwrite-only churn must not grow at all.

/// The number of distinct keys a churn pass uses.
const KEYS: usize = 300;

/// A **boxed** key: a four-element integer tuple, so the stored key term is a
/// pointer into table storage — the shape the dangling-key bug needed.
fn churn_key(heap: &mut Heap, i: i64) -> Eterm {
    int_tuple(heap, &[i, i + 1000, i + 2000, i + 3000])
}

/// A **boxed** value: an eight-element integer tuple, so both the key and the
/// value of every object occupy store words.
fn churn_value(heap: &mut Heap, i: i64, variant: i64) -> Eterm {
    int_tuple(
        heap,
        &[i, variant, i, variant, i, variant, i, variant],
    )
}

/// A `{Key, Value}` object with a boxed key and a boxed value.
fn churn_object(heap: &mut Heap, key: Eterm, i: i64, variant: i64) -> Eterm {
    let value = churn_value(heap, i, variant);
    tuple_of(heap, &[key, value])
}

/// A balanced insert/delete churn — the store reclaims every object, so the
/// memory returns to a *stable* floor (only the interior's grown index remains)
/// and three identical cycles must land on exactly the same floor: nothing
/// accumulates, and the key anchors survive the churn.
#[test]
fn balanced_churn_returns_to_a_stable_floor_for_every_shape() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 18);

    // Source terms, built once and re-inserted each cycle (the table deep-copies
    // them, which is exactly what a real workload re-using caller terms does).
    let keys: Vec<Eterm> = (0..KEYS as i64).map(|i| churn_key(&mut heap, i)).collect();
    let per_key: Vec<Vec<Eterm>> = keys
        .iter()
        .enumerate()
        .map(|(n, &key)| {
            // Two distinct objects under the same key: `set`/`ordered_set` upsert
            // (so exactly one survives), `bag`/`duplicate_bag` keep both.
            (0..2i64)
                .map(|variant| churn_object(&mut heap, key, n as i64, variant))
                .collect()
        })
        .collect();

    for (shape, make) in all_shapes() {
        let mut t = make();
        let empty = t.memory();

        let expected = if t.kind().is_multi() { 2 * KEYS } else { KEYS };
        let mut floors = Vec::new();
        for cycle in 0..3 {
            for objects in &per_key {
                for &object in objects {
                    t.insert(object, &atoms).unwrap();
                }
            }
            assert_eq!(t.size(), expected, "{shape} cycle {cycle}: size after insert");
            let populated = t.memory();
            assert!(
                populated > empty,
                "{shape} cycle {cycle}: a populated table owns storage"
            );

            for &key in &keys {
                t.delete(key, &atoms).unwrap();
            }
            assert_eq!(t.size(), 0, "{shape} cycle {cycle}: size after delete");
            let floor = t.memory();
            assert_eq!(
                t.info().memory,
                floor,
                "{shape} cycle {cycle}: `TableInfo::memory` disagrees with `Table::memory`"
            );
            assert!(
                floor < populated,
                "{shape} cycle {cycle}: the delete reclaimed nothing ({populated} -> {floor})"
            );
            floors.push(floor);
        }

        assert_eq!(
            floors[0], floors[1],
            "{shape}: the memory floor moved on the second cycle ({floors:?})"
        );
        assert_eq!(
            floors[1], floors[2],
            "{shape}: the memory floor moved on the third cycle ({floors:?})"
        );
        assert!(
            floors[2] >= empty,
            "{shape}: the post-delete floor fell below the empty floor ({floors:?})"
        );

        // Every key is still anchored after three full cycles: a churned-away key
        // must be re-insertable and findable, and a stale key must not answer.
        for &key in &keys {
            assert!(!t.member(key, &atoms), "{shape}: a deleted key is still present");
        }
        for objects in &per_key {
            for &object in objects {
                t.insert(object, &atoms).unwrap();
            }
        }
        assert_eq!(t.size(), expected, "{shape}: the table is reusable after churn");
        for &key in &keys {
            assert!(t.member(key, &atoms), "{shape}: a key was lost to the churn");
        }
    }
}

/// A `delete_object/2`-driven churn across the multi kinds: every round removes
/// each stored object by its exact value — which re-anchors the surviving entry on
/// every key — then reuses the freed runs under fresh keys, and the observable
/// memory returns to a stable floor, three cycles landing on exactly the same
/// figure.
#[test]
fn delete_object_churn_returns_to_a_stable_floor_for_the_multi_kinds() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 18);
    let keys: Vec<Eterm> = (0..KEYS as i64).map(|i| churn_key(&mut heap, i)).collect();
    let per_key: Vec<Vec<Eterm>> = keys
        .iter()
        .enumerate()
        .map(|(n, &key)| {
            // Two distinct objects under the same boxed key.
            (0..2i64)
                .map(|variant| churn_object(&mut heap, key, n as i64, variant))
                .collect()
        })
        .collect();

    for (shape, kind) in multi_kinds() {
        let mut t = table(kind);
        let empty = t.memory();
        let mut floors = Vec::new();
        for cycle in 0..3 {
            for objects in &per_key {
                for &object in objects {
                    t.insert(object, &atoms).unwrap();
                }
            }
            assert_eq!(t.size(), 2 * KEYS, "{shape} cycle {cycle}: size after insert");

            // Remove only the first variant of each key, so every entry keeps a
            // survivor and its key has to be re-anchored to it.
            for objects in &per_key {
                t.delete_object(objects[0], &atoms).unwrap();
            }
            assert_eq!(t.size(), KEYS, "{shape} cycle {cycle}: size after delete_object");

            // Reuse the freed runs with same-shaped objects under fresh keys, then
            // confirm each original entry still resolves to its survivor.
            for n in KEYS..KEYS + 16 {
                let key = churn_key(&mut heap, n as i64);
                t.insert(churn_object(&mut heap, key, n as i64, 0), &atoms)
                    .unwrap();
            }
            for (n, objects) in per_key.iter().enumerate() {
                let key = keys[n];
                let looked = t.lookup(key, &atoms);
                assert_eq!(
                    looked.len(),
                    1,
                    "{shape} cycle {cycle}: key {n} lost its anchor"
                );
                assert!(
                    eq(looked[0], objects[1], &atoms),
                    "{shape} cycle {cycle}: key {n} kept the removed object"
                );
            }
            for n in KEYS..KEYS + 16 {
                t.delete(churn_key(&mut heap, n as i64), &atoms).unwrap();
            }
            for objects in &per_key {
                t.delete_object(objects[1], &atoms).unwrap();
            }
            assert_eq!(t.size(), 0, "{shape} cycle {cycle}: size after purge");
            let floor = t.memory();
            assert!(floor >= empty, "{shape} cycle {cycle}: floor below empty");
            floors.push(floor);
        }
        assert_eq!(floors[0], floors[1], "{shape}: the memory floor moved ({floors:?})");
        assert_eq!(floors[1], floors[2], "{shape}: the memory floor moved ({floors:?})");
    }
}

/// The `{keypos, 2}` `delete_object/2` churn: each key has two objects sharing the
/// key element (element 2) and differing in the tag (element 1), so every
/// `delete_object/2` leaves a survivor whose entry must be re-anchored **from
/// element 2** — the `HashInterior::with_keypos` path — while the freed runs are
/// reused by later same-shaped inserts.
#[test]
fn delete_object_churn_reanchors_for_a_custom_keypos() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 18);
    let keys: Vec<Eterm> = (0..KEYS as i64).map(|i| churn_key(&mut heap, i)).collect();
    // `{Tag, Key}` with the boxed key at element 2 (tag 0 and tag 1 under each key).
    let per_key: Vec<Vec<Eterm>> = keys
        .iter()
        .map(|&key| {
            (0..2i64)
                .map(|tag| tuple_of(&mut heap, &[make_small(tag), key]))
                .collect()
        })
        .collect();

    for (shape, kind) in multi_kinds() {
        let mut t = keypos_table(kind, 2);
        let empty = t.memory();
        let mut floors = Vec::new();
        for cycle in 0..3 {
            for objects in &per_key {
                for &object in objects {
                    t.insert(object, &atoms).unwrap();
                }
            }
            assert_eq!(t.size(), 2 * KEYS, "{shape} cycle {cycle}: size after insert");

            for objects in &per_key {
                t.delete_object(objects[0], &atoms).unwrap();
            }
            assert_eq!(t.size(), KEYS, "{shape} cycle {cycle}: size after delete_object");

            // Reuse the freed (tag-0) runs under fresh keys, then confirm every
            // original key still resolves to its tag-1 survivor.
            for n in KEYS..KEYS + 16 {
                let key = churn_key(&mut heap, n as i64);
                t.insert(tuple_of(&mut heap, &[make_small(0), key]), &atoms)
                    .unwrap();
            }
            for (n, objects) in per_key.iter().enumerate() {
                let looked = t.lookup(keys[n], &atoms);
                assert_eq!(
                    looked.len(),
                    1,
                    "{shape} cycle {cycle}: key {n} lost its anchor"
                );
                assert!(
                    eq(looked[0], objects[1], &atoms),
                    "{shape} cycle {cycle}: the re-anchor used the wrong keypos"
                );
            }
            for n in KEYS..KEYS + 16 {
                t.delete(churn_key(&mut heap, n as i64), &atoms).unwrap();
            }
            for objects in &per_key {
                t.delete_object(objects[1], &atoms).unwrap();
            }
            assert_eq!(t.size(), 0, "{shape} cycle {cycle}: size after purge");
            let floor = t.memory();
            assert!(floor >= empty, "{shape} cycle {cycle}: floor below empty");
            floors.push(floor);
        }
        assert_eq!(floors[0], floors[1], "{shape}: the memory floor moved ({floors:?})");
        assert_eq!(floors[1], floors[2], "{shape}: the memory floor moved ({floors:?})");
    }
}

/// An overwrite-only churn: repeatedly re-inserting the same keys with fresh
/// (boxed) values must not grow the table at all — every replaced object's
/// storage is reclaimed, and the index is untouched. Only the kinds with upsert
/// semantics (`set`/`ordered_set`) can do this; `bag`/`duplicate_bag` only append.
#[test]
fn overwrite_only_churn_stays_bounded_for_the_upsert_kinds() {
    let atoms = AtomTable::new();

    for (shape, make) in all_shapes() {
        let mut t = make();
        if t.kind().is_multi() {
            continue;
        }
        // A fresh source heap per shape: every round builds new boxed values.
        let mut heap = Heap::new(1 << 20);
        let keys: Vec<Eterm> = (0..KEYS as i64).map(|i| churn_key(&mut heap, i)).collect();
        for (n, &key) in keys.iter().enumerate() {
            let object = churn_object(&mut heap, key, n as i64, 0);
            t.insert(object, &atoms).unwrap();
        }
        let baseline = t.memory();
        assert_eq!(t.size(), KEYS, "{shape}: the baseline is fully loaded");

        // 200 rounds, each rewriting every key with a fresh boxed value.
        for round in 1..=200i64 {
            for (n, &key) in keys.iter().enumerate() {
                let object = churn_object(&mut heap, key, n as i64, round);
                t.insert(object, &atoms).unwrap();
            }
            assert_eq!(t.size(), KEYS, "{shape} round {round}: upsert changed the size");
        }

        assert_eq!(
            t.memory(),
            baseline,
            "{shape}: overwrite churn grew the table ({baseline} -> {})",
            t.memory()
        );
        for &key in &keys {
            assert!(t.member(key, &atoms), "{shape}: a key was lost to the upsert churn");
        }
    }
}

/// Repeatedly creating and dropping tables must leave nothing behind: every fresh
/// table starts at exactly the same empty floor, however many loaded tables came
/// before it, and each is fully usable.
#[test]
fn repeated_table_create_and_drop_leaves_no_accumulation() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 18);
    let keys: Vec<Eterm> = (0..64).map(|i| churn_key(&mut heap, i)).collect();
    let objects: Vec<Eterm> = keys
        .iter()
        .enumerate()
        .map(|(n, &key)| churn_object(&mut heap, key, n as i64, 0))
        .collect();

    for (shape, make) in all_shapes() {
        let empty = make().memory();
        for round in 0..50 {
            let mut loaded = make();
            assert_eq!(
                loaded.memory(),
                empty,
                "{shape} round {round}: a fresh table is not at the empty floor"
            );
            for &object in &objects {
                loaded.insert(object, &atoms).unwrap();
            }
            assert!(loaded.memory() > empty, "{shape} round {round}: a loaded table owns storage");
            drop(loaded);

            // Storage must not leak across tables: the next fresh table is back at
            // the floor and works exactly like the first.
            let mut fresh = make();
            assert_eq!(
                fresh.memory(),
                empty,
                "{shape} round {round}: table storage leaked across a drop"
            );
            for &object in &objects {
                fresh.insert(object, &atoms).unwrap();
            }
            assert_eq!(fresh.size(), 64, "{shape} round {round}: a fresh table is unusable");
            for &key in &keys {
                assert!(fresh.member(key, &atoms), "{shape} round {round}: a key is missing");
            }
        }
    }
}

/// The `TermStore::words` accounting directly: a copy/free cycle owns words while
/// the copies are live and returns to **zero** when every root is freed, cycle
/// after cycle, without acquiring a fresh arena block.
#[test]
fn term_store_word_accounting_returns_to_zero_under_copy_free_churn() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 16);
    let terms: Vec<Eterm> = (0..64)
        .map(|i| {
            let key = churn_key(&mut heap, i);
            churn_object(&mut heap, key, i, 0)
        })
        .collect();

    let mut store = TermStore::with_default_allocator();
    assert_eq!(store.words(), 0, "a fresh store owns no words");

    let mut blocks = Vec::new();
    for cycle in 0..5 {
        let roots: Vec<Eterm> = terms.iter().map(|&t| store.copy(t).unwrap()).collect();
        assert!(
            store.words() > 0,
            "cycle {cycle}: the copied objects own words"
        );
        // The copies are intact and readable: each retains its own key element.
        for (n, &root) in roots.iter().enumerate() {
            let keypos = key_of(root, 1).unwrap();
            let want = key_of(terms[n], 1).unwrap();
            assert!(eq(keypos, want, &atoms), "cycle {cycle}: copy {n} is corrupt");
        }
        for root in roots {
            store.free(root);
        }
        assert_eq!(
            store.words(),
            0,
            "cycle {cycle}: freeing every root must return the word count to zero"
        );
        blocks.push(store.blocks());
    }

    assert_eq!(
        blocks[4], blocks[0],
        "the store acquired a fresh arena block per cycle: {blocks:?}"
    );
}
