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
    compile, key_of, OrderedInterior, SetInterior, Table, TableId, TableKind, TableOptions,
    TermStore,
};
use beam_term::prelude::*;
use common::*;
use std::time::Duration;

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

// ---------------------------------------------------------------------------
// `ets:info(T, memory)` against the live oracle (the ballpark check)
// ---------------------------------------------------------------------------
//
// The absolute figure is implementation-specific (a word count of the table's own
// term storage plus its index), so it can never equal the oracle's. But it *is*
// a word count in the same unit as `ets:info(_, memory)`, so a large table's
// figure must land in the same order of magnitude: a wildly different number
// would mean the accounting covered a different unit, or that storage was
// leaked or grossly undercounted. Boundedness and monotonicity at the crate
// level are pinned by the stress tests above; this test pins the *unit* and the
// scale against the oracle.

/// The object count of the memory ballpark table.
const MEMORY_N: i64 = 100_000;

/// The oracle program: the identical `{I, I*2}` table per kind, printing the
/// table's `memory` field.
const MEMORY_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~w~n", [K, V]) end,
    [begin
        T = ets:new(mem, [Kind]),
        [ets:insert(T, {I, I * 2}) || I <- lists:seq(0, 99999)],
        P(atom_to_list(Kind), ets:info(T, memory)),
        ets:delete(T)
     end || Kind <- [set, ordered_set, bag, duplicate_bag]],
    ok.
"#;

/// A large table's `ets:info(T, memory)` agrees with the oracle to within one
/// order of magnitude, for every kind.
#[test]
fn ets_info_memory_is_in_the_oracles_ballpark() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "ets_info_memory_is_in_the_oracles_ballpark",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable escript on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        MEMORY_ORACLE_PROGRAM,
        &[],
        "memory",
        Duration::from_secs(120),
    );
    let live = parse_lines(&String::from_utf8_lossy(&output.stdout));

    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(1 << 21);
        let mut t = table(kind);
        for key in 0..MEMORY_N {
            t.insert(object(&mut heap, key, key * 2), &atoms).unwrap();
        }
        let ours = t.memory();
        let oracle_mem: usize = live
            .get(kind.as_atom_name())
            .unwrap_or_else(|| panic!("the oracle printed no memory for {kind:?}"))
            .parse()
            .expect("the oracle's memory field is an integer");
        // Same order of magnitude: a ratio bound wide enough to be robust across
        // allocator/64-bit-word differences, tight enough to catch a wrong unit
        // or a large leak.
        assert!(
            ours <= oracle_mem * 10 && ours * 10 >= oracle_mem,
            "{kind:?}: the crate reports {ours} words but the oracle {oracle_mem} \
             (not the same order of magnitude)"
        );
        println!("[ets] memory {kind:?}: crate={ours} words, oracle={oracle_mem} words");
    }
}

// ---------------------------------------------------------------------------
// Removal/`take` accounting against the oracle's *shape*
// ---------------------------------------------------------------------------
//
// The oracle shapes these tests pin (system `erl`, OTP 29 / erts-17.1; the
// absolute words are its own interior's, so they are never compared):
//
// * `ets:info(T, size)` is the **object count** and drops to `0` on every removal;
// * `ets:info(T, memory)` is monotone with inserts, falls once the objects are
//   gone, and a balanced removal (or an overwrite on the upsert kinds) returns it
//   to a floor that does not move (`set` 306→9617→306, `ordered_set`
//   140→8140→140, the hash kinds keep their bucket storage);
// * `ets:take/2` moves the objects off the table, so `size` drops and the memory
//   floor is NOT re-grown by a repeated `take` + re-`insert` churn.
//
// Probe: an `escript` inserting `{I, I*2}` for `I in 0..1000`, deleting every key,
// then `delete_all_objects`, printed per kind (`erl`/`escript`, OTP 29.1.1 /
// erts-17.1):
// `set.empty -> 306 ; set.loaded -> 9617 ; set.size -> 1000 ;
//  set.after_delete -> 2617 ; set.size_after_delete -> 0 ; set.after_all -> 306`
// `ordered_set.empty -> 140 ; ordered_set.loaded -> 8140 ;
//  ordered_set.after_delete -> 140 ; ordered_set.after_all -> 140`
//
// The crate's absolute figures differ (word counts of its own store + index); what
// must hold is the counter *contract*: `size`/`is_empty`/`memory`/`info()` track
// the removals, every removal reclaims, and no removal path leaks across churn.

/// `ets:take/2` churn: taking a key hands its objects to the caller and reclaims
/// the table's own originals, so `size`/`is_empty` fall to the empty state and the
/// memory floor **stays put** cycle after cycle, while every checked-out object is
/// still a valid, distinct copy.
#[test]
fn take_churn_reclaims_the_table_originals_and_keeps_the_counters_flat() {
    let atoms = AtomTable::new();

    for (shape, make) in all_shapes() {
        let mut heap = Heap::new(1 << 20);
        let keys: Vec<Eterm> = (0..KEYS as i64).map(|i| churn_key(&mut heap, i)).collect();
        let per_key: Vec<Vec<Eterm>> = keys
            .iter()
            .enumerate()
            .map(|(n, &key)| {
                (0..2i64)
                    .map(|variant| churn_object(&mut heap, key, n as i64, variant))
                    .collect()
            })
            .collect();

        let mut t = make();
        // `set`/`ordered_set` upsert the two variants down to one; the multi kinds
        // keep both.
        let per_key_kept = if t.kind().is_multi() { 2 } else { 1 };
        let total = per_key_kept * KEYS;

        let mut floors = Vec::new();
        let mut populated = Vec::new();
        for cycle in 0..4 {
            for objects in &per_key {
                for &object in objects {
                    t.insert(object, &atoms).unwrap();
                }
            }
            assert_eq!(t.size(), total, "{shape} cycle {cycle}: size after insert");
            assert!(!t.is_empty(), "{shape} cycle {cycle}: a loaded table is not empty");
            populated.push(t.memory());

            let mut taken_all = Vec::new();
            for (n, &key) in keys.iter().enumerate() {
                let taken = t.take(key, &atoms).unwrap();
                assert_eq!(
                    taken.len(),
                    per_key_kept,
                    "{shape} cycle {cycle}: `take` on key {n} returned the wrong object count"
                );
                for &object in &taken {
                    let source = per_key[n]
                        .iter()
                        .find(|&&s| eq(s, object, &atoms))
                        .unwrap_or_else(|| {
                            panic!("{shape} cycle {cycle}: a taken object is not one of the key's")
                        });
                    assert!(
                        object.raw() != source.raw(),
                        "{shape} cycle {cycle}: `take` handed out the table's own storage, \
                         not a copy"
                    );
                }
                taken_all.extend(taken);
            }
            assert_eq!(t.size(), 0, "{shape} cycle {cycle}: size after take");
            assert!(t.is_empty(), "{shape} cycle {cycle}: an emptied table is empty");
            assert_eq!(t.info().size, 0, "{shape} cycle {cycle}: `info().size` after take");
            let floor = t.memory();
            assert_eq!(
                t.info().memory,
                floor,
                "{shape} cycle {cycle}: `TableInfo::memory` disagrees with `Table::memory`"
            );
            assert!(
                floor < populated[cycle],
                "{shape} cycle {cycle}: `take` did not reclaim the originals \
                 ({floor} !< {})",
                populated[cycle]
            );
            floors.push(floor);

            // The checked-out objects stay valid for the table's lifetime: each is
            // still readable and keeps its own key element, and every key's objects
            // are accounted for.
            for &key in &keys {
                let checked_out = taken_all
                    .iter()
                    .filter(|&&o| eq(key_of(o, 1).unwrap(), key, &atoms))
                    .count();
                assert_eq!(
                    checked_out,
                    per_key_kept,
                    "{shape} cycle {cycle}: a checked-out object became unreadable"
                );
            }
        }

        // A `take` cycle reclaims the table's own objects, so the post-take floor
        // is the **same** every cycle. The loaded figure also stabilizes: after the
        // warm-up cycle the interior's index (e.g. the `ordered_set` node arena,
        // which never shrinks) has reached its steady shape, and the store's part is
        // unchanged, so no cycle after the first may grow the table.
        for other in &floors[1..] {
            assert_eq!(
                *other, floors[0],
                "{shape}: the take floor moved across cycles ({floors:?})"
            );
        }
        for other in &populated[2..] {
            assert_eq!(
                *other, populated[1],
                "{shape}: a take cycle grew the table ({populated:?})"
            );
        }

        // The table is reusable and correct after the take churn.
        for objects in &per_key {
            for &object in objects {
                t.insert(object, &atoms).unwrap();
            }
        }
        assert_eq!(t.size(), total, "{shape}: the table is reusable after a take churn");
        for &key in &keys {
            assert!(t.member(key, &atoms), "{shape}: a key was lost to the take churn");
        }
    }
}

/// `ets:select_delete/2` and `ets:match_delete/2` remove every object through the
/// same per-object free path as `delete`, so the object count drops to zero, the
/// memory falls below the loaded figure and does **not** accumulate across cycles
/// — for every kind and interior shape. The two paths land on the same floor.
#[test]
fn select_delete_and_match_delete_reclaim_and_zero_the_counters() {
    for (shape, make) in all_shapes() {
        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(1 << 20);
        let keys: Vec<Eterm> = (0..KEYS as i64).map(|i| churn_key(&mut heap, i)).collect();
        // One object per key with a boxed key *and* a boxed value, so a removal has
        // table-owned storage to reclaim.
        let objects: Vec<Eterm> = keys
            .iter()
            .enumerate()
            .map(|(n, &key)| churn_object(&mut heap, key, n as i64, 0))
            .collect();

        // `[{{'_','_'},[],[true]}]` — the `select_delete` form of `match_delete`,
        // and the `{'_','_'}` pattern for `match_delete` itself.
        let wildcard = interned_atom(&mut atoms, "_");
        let pattern = tuple_of(&mut heap, &[wildcard, wildcard]);
        let body = proper_list(&mut heap, &[interned_atom(&mut atoms, "true")]);
        let clause = tuple_of(&mut heap, &[pattern, NIL, body]);
        let spec_term = proper_list(&mut heap, &[clause]);
        let spec = compile(spec_term, &atoms).expect("the wildcard spec compiles");

        let mut out = out_heap();
        let mut t = make();
        let empty = t.memory();
        let mut floors = Vec::new();

        for cycle in 0..3 {
            for &object in &objects {
                t.insert(object, &atoms).unwrap();
            }
            assert_eq!(t.size(), KEYS, "{shape} cycle {cycle}: size after insert");
            let populated = t.memory();
            assert!(populated > empty, "{shape} cycle {cycle}: a loaded table owns storage");

            let deleted = t.select_delete(&spec, &atoms, &mut out).unwrap();
            assert_eq!(deleted, KEYS, "{shape} cycle {cycle}: the deletion count");
            assert_eq!(t.size(), 0, "{shape} cycle {cycle}: size after select_delete");
            assert!(t.is_empty(), "{shape} cycle {cycle}: an emptied table is empty");
            assert_eq!(
                t.info().size,
                0,
                "{shape} cycle {cycle}: `info().size` after select_delete"
            );
            let floor = t.memory();
            assert_eq!(
                t.info().memory,
                floor,
                "{shape} cycle {cycle}: `TableInfo::memory` disagrees with `Table::memory`"
            );
            assert!(
                floor < populated,
                "{shape} cycle {cycle}: select_delete reclaimed nothing ({populated} -> {floor})"
            );
            assert!(
                floor >= empty,
                "{shape} cycle {cycle}: the floor fell below the empty floor ({floor} < {empty})"
            );
            floors.push(floor);
        }

        for cycle in 0..3 {
            for &object in &objects {
                t.insert(object, &atoms).unwrap();
            }
            let deleted = t.match_delete(pattern, &atoms, &mut out).unwrap();
            assert_eq!(deleted, KEYS, "{shape} match cycle {cycle}: the deletion count");
            assert_eq!(t.size(), 0, "{shape} match cycle {cycle}: size after match_delete");
            assert!(t.is_empty(), "{shape} match cycle {cycle}");
            assert_eq!(t.info().size, 0, "{shape} match cycle {cycle}: `info().size`");
            assert_eq!(
                t.info().memory,
                t.memory(),
                "{shape} match cycle {cycle}: `TableInfo::memory` disagrees"
            );
            floors.push(t.memory());
        }

        for other in &floors[1..] {
            assert_eq!(
                *other, floors[0],
                "{shape}: a removal path changed the stable floor ({floors:?})"
            );
        }
    }
}

/// The `TermStore` accounting behind `ets:take/2`: `relinquish` hands an object's
/// storage to the caller — `words` drops by exactly its word count (so it stops
/// counting toward `Table::memory` / `ets:info(Tab, memory)`) while the words stay
/// valid for the store's lifetime, never to be handed out again (a later `copy`
/// cannot alias the caller's term). `free` (the ordinary removal path) *does*
/// return the words to the per-size free lists, so a copy/free churn reuses them
/// and the store acquires no fresh block.
#[test]
fn term_store_relinquish_hands_storage_out_and_free_reuses_it() {
    let mut heap = Heap::new(1 << 16);
    let terms: Vec<Eterm> = (0..16)
        .map(|i| {
            let key = churn_key(&mut heap, i);
            churn_object(&mut heap, key, i, 0)
        })
        .collect();

    let mut store = TermStore::with_default_allocator();
    assert_eq!(store.words(), 0, "a fresh store owns no words");

    // Head every object out: copy it, then check it out.
    let checked_out: Vec<Eterm> = terms.iter().map(|&t| store.copy(t).unwrap()).collect();
    let live = store.words();
    assert!(live > 0, "the checked-out objects own words while live");
    let mut relinquished = 0;
    for &root in &checked_out {
        let words = store.relinquish(root);
        assert!(words > 0, "the first relinquish reclaims exactly its own run");
        relinquished += words;
    }
    assert_eq!(
        relinquished, live,
        "relinquishing every copy must account for exactly the live words"
    );
    assert_eq!(
        store.words(),
        0,
        "a checked-out object no longer counts as table storage"
    );
    // A second relinquish is a harmless no-op (the root is no longer tracked).
    for &root in &checked_out {
        assert_eq!(store.relinquish(root), 0, "a checked-out root is not tracked twice");
    }

    // The ordinary removal path reuses runs: the copy/free churn acquires no fresh
    // block, and a fresh copy never lands on a checked-out address.
    let mut blocks = Vec::new();
    for cycle in 0..5 {
        let roots: Vec<Eterm> = terms.iter().map(|&t| store.copy(t).unwrap()).collect();
        assert!(store.words() > 0, "cycle {cycle}: the copies own words");
        if cycle == 0 {
            for &root in &roots {
                assert!(
                    !checked_out.iter().any(|&c| c == root),
                    "a later `copy` handed out a checked-out object's address"
                );
            }
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
        "the free churn acquired a fresh arena block per cycle: {blocks:?}"
    );
}

/// The other half of the `take`-storage contract, pinned explicitly: repeated
/// `relinquish` (the `ets:take/2` check-out) **retains** its storage — every round
/// grows the store's arena (`blocks`) — because a checked-out term must stay valid
/// and can therefore never be handed out again. `words()` still returns to zero
/// each round, so the *counters* stay correct and the retention is released only
/// when the store is dropped or `reset` (`ets:delete_all_objects/1`).
///
/// This is the observable behaviour the oracle also has: `ets:info(T, memory)`
/// (which `Table::memory` feeds from `store.words()`) does **not** grow across a
/// `take` + re-`insert` churn, so the retained arena bytes are not table memory.
/// The growth is inherent to the current `take` API, which hands back
/// table-owned terms (the caller supplies no heap), so the audit records it as the
/// known `relinquish` retention, not a counter defect.
#[test]
fn take_style_relinquish_churn_retains_its_arena_until_the_store_is_reset() {
    let mut heap = Heap::new(1 << 16);
    let terms: Vec<Eterm> = (0..16)
        .map(|i| {
            let key = churn_key(&mut heap, i);
            churn_object(&mut heap, key, i, 0)
        })
        .collect();

    let mut store = TermStore::with_default_allocator();
    let mut blocks = Vec::new();
    for cycle in 0..4 {
        let roots: Vec<Eterm> = terms.iter().map(|&t| store.copy(t).unwrap()).collect();
        for root in roots {
            store.relinquish(root);
        }
        assert_eq!(
            store.words(),
            0,
            "cycle {cycle}: a checked-out churn still returns the live-word count to zero"
        );
        blocks.push(store.blocks());
    }
    assert!(
        blocks[3] > blocks[0],
        "the relinquish churn retained no arena at all: {blocks:?}"
    );
    println!("[ets] relinquish churn arena blocks: {blocks:?}");

    // `reset` — the storage side of `ets:delete_all_objects/1` — releases the
    // retained arena.
    store.reset();
    assert_eq!(store.words(), 0, "reset clears the live-word count");
    assert_eq!(store.blocks(), 0, "reset releases every retained arena block");
}
