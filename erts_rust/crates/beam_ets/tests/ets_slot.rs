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

//! `Table::slot/3` — `ets:slot/2` — over the public API.
//!
//! The edge semantics below mirror the system `erl` (OTP 29.1.1 / erts-17.1),
//! verified with `escript`:
//!
//!   * slot numbers are **0-based**; an `ordered_set` slot `n` is the object with
//!     the (n+1)-th smallest key, `n == size` is `'$end_of_table'`, `n > size` is
//!     `badarg`, and a negative slot is `badarg`;
//!   * a hash-kind slot (`set`/`bag`/`duplicate_bag`) is a **hash bucket**, so a
//!     slot holds that bucket's objects (several or none). The crate's bucket
//!     *count* and per-object index are an interior freedom (crate `CONTEXT.md`
//!     §2), so only the *contract* is pinned here: slots `0..nactive-1` partition
//!     every stored object, `nactive` is `'$end_of_table'`, `nactive + 1` is
//!     `badarg`.
//!
//! The exact per-slot rendering of an `ordered_set` — and a live cross-check of
//! both shapes against the real `erl` — lives in the differential battery
//! (`tests/differential/battery/slot.rs`).

mod common;

use beam_common::{BeamError, Result as BeamResult};
use beam_ets::{
    Access, OrderedInterior, OwnerId, SetInterior, SharedTable, Table, TableId, TableKind,
    TableOptions, TableRead, TermStore,
};
use beam_term::prelude::*;
use common::*;

/// A table of `kind` with the listed `{Key, Value}` objects inserted.
fn table_with(kind: TableKind, objects: &[(i64, i64)]) -> (Table, AtomTable, Heap) {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1024);
    let mut table = new_table(kind);
    for &(k, v) in objects {
        table
            .insert(object(&mut heap, k, v), &atoms)
            .expect("insert");
    }
    (table, atoms, heap)
}

/// The `Value` element (element 2) of a `{Key, Value}` object.
fn value_of(object: Eterm) -> i64 {
    // SAFETY: every object built for these tests is a live arity-2 tuple, so
    // element 1 exists.
    unsafe { signed_value(tuple_element(object, 1)) }
}

/// The values a slot holds, with the crate's `Option` convention.
fn slot_values(table: &Table, atoms: &AtomTable, slot: i64) -> BeamResult<Option<Vec<i64>>> {
    match table.slot(slot, atoms)? {
        Some(objects) => Ok(Some(objects.iter().map(|&o| value_of(o)).collect())),
        None => Ok(None),
    }
}

/// Walk slots `0, 1, …` until the end-of-table slot, returning every value seen
/// and the index of the end-of-table slot.
fn walk_slots(table: &Table, atoms: &AtomTable) -> (Vec<i64>, i64) {
    let mut values = Vec::new();
    let mut slot = 0i64;
    loop {
        match table.slot(slot, atoms) {
            Ok(Some(here)) => {
                values.extend(here.iter().map(|&o| value_of(o)));
                slot += 1;
            }
            Ok(None) => return (values, slot),
            Err(error) => panic!("slot {slot} failed: {error:?}"),
        }
    }
}

// --------------------------------------------------------------- ordered_set

#[test]
fn ordered_set_slots_are_ascending_objects_and_exact_bounds() {
    let (table, atoms, _) = table_with(TableKind::OrderedSet, &[(30, 300), (10, 100), (20, 200)]);
    assert_eq!(slot_values(&table, &atoms, 0).unwrap(), Some(vec![100]));
    assert_eq!(slot_values(&table, &atoms, 1).unwrap(), Some(vec![200]));
    assert_eq!(slot_values(&table, &atoms, 2).unwrap(), Some(vec![300]));
    // `n == size` is `'$end_of_table'`, `n > size` is `badarg`.
    assert_eq!(slot_values(&table, &atoms, 3).unwrap(), None);
    assert!(matches!(table.slot(4, &atoms), Err(BeamError::BadArg)));
    // A negative slot is `badarg` (never a wrap-around).
    assert!(matches!(table.slot(-1, &atoms), Err(BeamError::BadArg)));
}

#[test]
fn ordered_set_slots_follow_ascending_key_order_regardless_of_insert_order() {
    let (ascending, atoms, _) = table_with(
        TableKind::OrderedSet,
        &[(10, 1), (20, 2), (30, 3), (40, 4), (50, 5)],
    );
    let (descending, _, _) = table_with(
        TableKind::OrderedSet,
        &[(50, 5), (40, 4), (30, 3), (20, 2), (10, 1)],
    );
    for slot in 0..5 {
        assert_eq!(
            slot_values(&ascending, &atoms, slot).unwrap(),
            slot_values(&descending, &atoms, slot).unwrap(),
            "slot {slot} must be identical for both insert orders"
        );
    }
    assert_eq!(slot_values(&descending, &atoms, 0).unwrap(), Some(vec![1]));
    assert_eq!(slot_values(&descending, &atoms, 4).unwrap(), Some(vec![5]));
    assert_eq!(slot_values(&descending, &atoms, 5).unwrap(), None);
    assert!(matches!(descending.slot(6, &atoms), Err(BeamError::BadArg)));
}

#[test]
fn ordered_set_empty_table_has_only_the_end_of_table_slot() {
    let (table, atoms, _) = table_with(TableKind::OrderedSet, &[]);
    assert_eq!(slot_values(&table, &atoms, 0).unwrap(), None);
    assert!(matches!(table.slot(1, &atoms), Err(BeamError::BadArg)));
}

#[test]
fn ordered_set_slots_follow_the_key_position() {
    // `{keypos, 2}`: the slots order by the *second* element; distinct keys are
    // inserted out of order so the slot order is observable.
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);
    opts.set_keypos(2).expect("keypos 2 is valid");
    let mut table = Table::new(TableId::next(), opts).expect("constructible");
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    for (tag, key) in [(9i64, 30i64), (7, 10), (8, 20)] {
        // `{Tag, Key}` objects: the key is element 2.
        table
            .insert(
                tuple_of(&mut heap, &[make_small(tag), make_small(key)]),
                &atoms,
            )
            .expect("insert");
    }
    assert_eq!(table.size(), 3);
    // Ascending by key (element 2): key 10, then 20, then 30.
    for (slot, &(tag, key)) in [(7i64, 10i64), (8, 20), (9, 30)].iter().enumerate() {
        let got = table
            .slot(slot as i64, &atoms)
            .expect("in range")
            .expect("not the end");
        assert_eq!(got.len(), 1);
        // SAFETY: a live arity-2 tuple.
        let tag_got = unsafe { signed_value(tuple_element(got[0], 0)) };
        let key_got = unsafe { signed_value(tuple_element(got[0], 1)) };
        assert_eq!((tag_got, key_got), (tag, key), "slot {slot}");
    }
    assert_eq!(table.slot(3, &atoms).unwrap(), None);
    assert!(matches!(table.slot(4, &atoms), Err(BeamError::BadArg)));
}

// ----------------------------------------------------------------- hash kinds

#[test]
fn hash_kind_slots_partition_every_object() {
    for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
        let (table, atoms, _) = table_with(kind, &[(1, 10), (1, 20), (2, 30), (3, 40)]);
        let (mut union, eot) = walk_slots(&table, &atoms);
        // The union over slots `0..nactive-1` is exactly the stored multiset.
        let mut want: Vec<i64> = table.tab2list().iter().map(|&o| value_of(o)).collect();
        union.sort_unstable();
        want.sort_unstable();
        assert_eq!(
            union, want,
            "{kind:?}: the slots must partition every object"
        );

        // `nactive` is the end-of-table slot; one past it is `badarg`, as is a
        // negative slot. (`bag` refuses the duplicate `{1,20}`, so its multiset is
        // one shorter — `want` already reflects that through `tab2list`.)
        assert_eq!(table.slot(eot, &atoms).unwrap(), None, "{kind:?} eot");
        assert!(
            matches!(table.slot(eot + 1, &atoms), Err(BeamError::BadArg)),
            "{kind:?} one past the end"
        );
        assert!(matches!(table.slot(-1, &atoms), Err(BeamError::BadArg)));
    }
}

#[test]
fn hash_kind_slots_are_a_partition_even_when_empty() {
    for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
        let (table, atoms, _) = table_with(kind, &[]);
        let (union, eot) = walk_slots(&table, &atoms);
        assert!(union.is_empty(), "{kind:?}: an empty table has no objects");
        assert_eq!(table.slot(eot, &atoms).unwrap(), None);
        assert!(matches!(
            table.slot(eot + 1, &atoms),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn every_hash_slot_holds_only_objects_the_table_stores() {
    // A slot never invents an object: each value a slot yields is a value the
    // table's `tab2list` also yields.
    for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
        let (table, atoms, _) = table_with(kind, &[(1, 10), (2, 20), (3, 30), (4, 40)]);
        let stored: Vec<i64> = table.tab2list().iter().map(|&o| value_of(o)).collect();
        let mut slot = 0i64;
        loop {
            match table.slot(slot, &atoms).expect("in range") {
                Some(objects) => {
                    for &o in &objects {
                        assert!(
                            stored.contains(&value_of(o)),
                            "{kind:?}: slot {slot} yielded an unknown object"
                        );
                    }
                    slot += 1;
                }
                None => break,
            }
        }
        assert!(
            slot > 0,
            "{kind:?}: a non-empty table has at least one slot"
        );
    }
}

#[test]
fn chained_set_interior_exposes_the_same_slot_contract() {
    // The opt-in chained `set` interior (a bucket array) must satisfy the same
    // contract as the CHAMP default.
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::Set);
    let mut table = Table::with_set_interior(TableId::next(), opts, SetInterior::Chained)
        .expect("constructible");
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    for key in 1..=5 {
        table
            .insert(object(&mut heap, key, key * 10), &atoms)
            .expect("insert");
    }
    let (mut union, eot) = walk_slots(&table, &atoms);
    union.sort_unstable();
    assert_eq!(union, vec![10, 20, 30, 40, 50]);
    assert_eq!(table.slot(eot, &atoms).unwrap(), None);
    assert!(matches!(
        table.slot(eot + 1, &atoms),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn sorted_ordered_interior_exposes_the_same_slot_contract() {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);
    let mut table = Table::with_ordered_interior(TableId::next(), opts, OrderedInterior::Sorted)
        .expect("constructible");
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    for key in [3i64, 1, 2] {
        table
            .insert(object(&mut heap, key, key * 10), &atoms)
            .expect("insert");
    }
    assert_eq!(slot_values(&table, &atoms, 0).unwrap(), Some(vec![10]));
    assert_eq!(slot_values(&table, &atoms, 1).unwrap(), Some(vec![20]));
    assert_eq!(slot_values(&table, &atoms, 2).unwrap(), Some(vec![30]));
    assert_eq!(slot_values(&table, &atoms, 3).unwrap(), None);
    assert!(matches!(table.slot(4, &atoms), Err(BeamError::BadArg)));
}

// ----------------------------------------------------------- access + views

#[test]
fn slot_is_read_gated_through_table_access() {
    let mut table = new_table(TableKind::Set);
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    table
        .insert(object(&mut heap, 1, 10), &atoms)
        .expect("insert");
    table.set_access(Access::Private);
    let owner = table.owner();

    // The owner reads its own slot.
    {
        let access = table.as_requester(owner);
        assert_eq!(access.slot(0, &atoms).unwrap().map(|o| o.len()), Some(1));
    }

    // A non-owner is denied by the read gate, changing nothing.
    let access = table.as_requester(Some(OwnerId::new(9_999_999)));
    assert!(matches!(access.slot(0, &atoms), Err(BeamError::BadArg)));
}

#[test]
fn slot_is_available_on_the_shared_read_view() {
    let mut table = new_table(TableKind::OrderedSet);
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    for key in [30i64, 10, 20] {
        table
            .insert(object(&mut heap, key, key), &atoms)
            .expect("insert");
    }
    let shared = SharedTable::new(table);
    shared
        .with_read(|view: &mut TableRead<'_>| {
            let slot0 = view.slot(0, &atoms).expect("in range").expect("not eot");
            assert_eq!(slot0.len(), 1);
            let object = &slot0[0];
            assert!(object.is_tuple());
            assert_eq!(
                object.tuple_element(0).expect("element 0").as_small_int(),
                Some(10)
            );
            assert!(view.slot(3, &atoms).expect("in range").is_none());
            assert!(matches!(view.slot(4, &atoms), Err(BeamError::BadArg)));
            assert!(matches!(view.slot(-1, &atoms), Err(BeamError::BadArg)));
            // A term copied out of the slot survives the guard.
            let mut dest = TermStore::with_default_allocator();
            let owned = object.copy_into(&mut dest).expect("copy out");
            assert!(dest.words() > 0);
            assert_eq!(value_of(owned), 10);
        })
        .expect("read view");
}
