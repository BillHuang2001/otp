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

use beam_ets::{SetInterior, Table, TableId, TableKind, TableOptions};
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
