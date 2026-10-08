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

/// The `bag` variant: `delete_object` removes the object whose storage the
/// entry's key aliased while the entry survives on its other objects.
#[test]
fn bag_delete_object_keeps_the_key_anchor_alive() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = table(TableKind::Bag);

    let key = int_tuple(&mut heap, &[7]);
    let a = tuple_of(&mut heap, &[key, make_small(10)]);
    let b = tuple_of(&mut heap, &[key, make_small(20)]);
    t.insert(a, &atoms).unwrap();
    t.insert(b, &atoms).unwrap();
    assert_eq!(t.size(), 2);

    // Remove the *first* object, which is the one the entry's key anchors to; the
    // entry survives on `b`.
    t.delete_object(a, &atoms).unwrap();
    assert_eq!(t.size(), 1);

    // Reuse the freed runs with same-shaped objects under fresh keys.
    for k in 8..12i64 {
        let other = int_tuple(&mut heap, &[k]);
        t.insert(tuple_of(&mut heap, &[other, make_small(k)]), &atoms)
            .unwrap();
    }

    let looked = t.lookup(key, &atoms);
    assert_eq!(looked.len(), 1, "the surviving bag entry lost its key anchor");
    assert!(eq(looked[0], b, &atoms));
    assert!(t.member(key, &atoms));
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
