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

//! `ets:safe_fixtable/2` — the table's *fix* flag and the deferred reclamation
//! that makes a traversal of a table being modified safe.
//!
//! Ground truth (verified against the system `erl`, OTP 29 / erts-17.1):
//!
//! * `ets:safe_fixtable(Tab, true)` and `ets:safe_fixtable(Tab, false)` both
//!   answer `true`; a non-boolean argument is `badarg` (that type check is the BIF
//!   layer's — this crate takes the already-parsed `bool`).
//! * While the table is fixed, deleting objects does **not** release their
//!   storage: `ets:info(Tab, memory)` stays put (the oracle even grows it by its
//!   fixed-deleted bookkeeping) and only drops at
//!   `ets:safe_fixtable(Tab, false)`.
//! * `ets:delete_all_objects/1` on a fixed table empties it (`size` is `0`) while
//!   the arena — and so `memory` — is untouched until the table is unfixed.
//! * A traversal that deletes each object it visits still sees **every** object
//!   exactly once, in order, and ends at `'$end_of_table'`.

mod common;

use beam_ets::{Access, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;

use common::{int_tuple, new_table};

/// An `open` (access-wise) table of `kind`; the fix flag is orthogonal to access
/// control, so the tests use the most permissive mode to keep the focus on
/// fixation.
fn open_table(kind: TableKind) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind).set_access(Access::Public);
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// Insert `{1,1} .. {n,n}` into `table`; returns the number of objects.
fn fill(table: &mut Table, heap: &mut Heap, atoms: &AtomTable, n: i64) {
    for i in 1..=n {
        table.insert(int_tuple(heap, &[i, i]), atoms).unwrap();
    }
}

#[test]
fn fixing_and_unfixing_is_idempotent_and_reported() {
    let atoms = AtomTable::new();
    let mut table = new_table(TableKind::Set);

    assert!(!table.is_fixed(), "a fresh table is not fixed");
    assert_eq!(table.deferred_objects(), 0);

    table.set_fixation(true);
    assert!(table.is_fixed());
    table.set_fixation(true);
    assert!(table.is_fixed(), "fixing an already-fixed table is a no-op");

    table.set_fixation(false);
    assert!(!table.is_fixed());
    table.set_fixation(false);
    assert!(!table.is_fixed(), "clearing an unfixed table is a no-op");
    let _ = atoms;
}

/// The storage of deleted objects is held while the table is fixed and released
/// exactly once, when the table is unfixed — the memory step the oracle shows.
///
/// The comparison is against an *identical* table that deleted the same objects
/// unfixed, so the two differ only in the fix flag: the fixed one must hold
/// strictly more memory.
#[test]
fn deleted_storage_is_held_while_fixed_and_released_at_unfix() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(8192);

    let mut fixed = new_table(TableKind::Set);
    let mut released = new_table(TableKind::Set);
    fill(&mut fixed, &mut heap, &atoms, 200);
    fill(&mut released, &mut heap, &atoms, 200);
    assert_eq!(fixed.size(), 200);
    assert!(fixed.memory() > 0);

    fixed.set_fixation(true);
    for i in 1..=200 {
        fixed.delete(make_small(i), &atoms).unwrap();
    }
    for i in 1..=200 {
        released.delete(make_small(i), &atoms).unwrap();
    }

    assert_eq!(fixed.size(), 0, "every object left the index");
    assert_eq!(released.size(), 0);
    assert_eq!(
        fixed.deferred_objects(),
        200,
        "every removed object's storage is held"
    );
    assert_eq!(released.deferred_objects(), 0);

    let held_memory = fixed.memory();
    assert!(
        held_memory > released.memory(),
        "a fixed table keeps the deleted objects' storage, an unfixed one does \
         not: {held_memory} > {}",
        released.memory()
    );

    fixed.set_fixation(false);
    assert_eq!(fixed.deferred_objects(), 0, "unfixing releases everything held");
    assert_eq!(
        fixed.memory(),
        released.memory(),
        "after unfixing, both tables account for exactly the same storage"
    );
    // The table is still perfectly usable after unfixing.
    fixed.insert(int_tuple(&mut heap, &[7, 7]), &atoms).unwrap();
    assert_eq!(fixed.size(), 1);
}

/// `ets:delete_all_objects/1` on a fixed table empties it but keeps the arena,
/// exactly as the oracle does (`size` drops to `0`, `memory` does not).
#[test]
fn delete_all_objects_while_fixed_keeps_the_arena() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(4096);

    let mut fixed = new_table(TableKind::Set);
    let mut released = new_table(TableKind::Set);
    fill(&mut fixed, &mut heap, &atoms, 50);
    fill(&mut released, &mut heap, &atoms, 50);

    fixed.set_fixation(true);
    fixed.delete_all_objects();
    released.delete_all_objects();

    assert_eq!(fixed.size(), 0);
    assert!(fixed.is_empty());
    assert_eq!(fixed.deferred_objects(), 50);
    assert!(
        fixed.memory() > released.memory(),
        "the fixed arena is untouched until unfix: {} > {}",
        fixed.memory(),
        released.memory()
    );

    fixed.set_fixation(false);
    assert_eq!(fixed.deferred_objects(), 0);
    assert_eq!(fixed.memory(), released.memory());
}

/// The end-to-end traversal guarantee: walking an `ordered_set` while deleting
/// each visited key yields every key once, in ascending order, and terminates with
/// `'$end_of_table'` — identical to the system `erl`.
#[test]
fn an_ordered_set_walks_and_deletes_every_key_in_order_under_fixation() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(4096);
    let mut table = open_table(TableKind::OrderedSet);
    // A **tuple** key, so the key term a traversal holds really is table-owned
    // storage (a small integer key would be an immediate with nothing to reclaim).
    // The expected keys are built on the same arena, so they stay valid for the
    // whole test.
    let mut expected = Vec::new();
    for i in 1..=5i64 {
        let key = heap.tuple(&[make_small(i), make_small(i)]).unwrap();
        expected.push(key);
        let obj = heap.tuple(&[key, make_small(i)]).unwrap();
        table.insert(obj, &atoms).unwrap();
    }

    table.set_fixation(true);
    let mut visited = Vec::new();
    let mut cursor = table.first(&atoms);
    while let Some(key) = cursor {
        visited.push(key);
        table.delete(key, &atoms).unwrap();
        // The anchor key has just been deleted, so its term's storage lives only
        // because the table is fixed. `next` still resolves it as a range bound.
        cursor = table.next(key, &atoms).expect("an ordered_set range bound");
    }

    assert_eq!(
        visited.len(),
        5,
        "a fixed traversal visits every object exactly once"
    );
    for (got, want) in visited.iter().zip(&expected) {
        assert!(eq(*got, *want, &atoms), "the walk is in ascending key order");
    }
    assert_eq!(table.size(), 0, "the walk deleted every object");
    assert_eq!(
        table.deferred_objects(),
        5,
        "the deleted objects' storage is held until the table is unfixed"
    );

    table.set_fixation(false);
    assert_eq!(table.deferred_objects(), 0);
}

/// A hash-kind (`set`) traversal keeps the oracle's anchor rule: an absent anchor
/// key is `badarg` for `ets:next/2`, fixed or not. (Only `ordered_set` reads the
/// anchor as a range bound.)
#[test]
fn a_hash_kind_still_requires_a_present_anchor_when_fixed() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut table = open_table(TableKind::Set);
    fill(&mut table, &mut heap, &atoms, 5);

    table.set_fixation(true);
    let anchor = table.first(&atoms).expect("a non-empty table has a first key");
    table.delete(anchor, &atoms).unwrap();
    assert!(matches!(
        table.next(anchor, &atoms),
        Err(beam_common::BeamError::BadArg)
    ));
    table.set_fixation(false);
}
