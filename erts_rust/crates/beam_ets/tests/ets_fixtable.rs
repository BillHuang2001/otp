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
//! * That traversal guarantee survives a **B+-tree node split** and interleaved
//!   mutation — deleting each visited key (the anchor's own deletion) while
//!   inserting fresh keys ahead of *and* behind the cursor — and an absent-key
//!   anchor stays a range bound across the split (the anchor rule itself is
//!   pinned against the live oracle in `ets_nav.rs`). See the split-crossing
//!   walks at the end of this file.

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

// ---------------------------------------------------------- split-crossing walks
//
// The walk above uses five keys, so it never leaves a single B+-tree leaf. The
// walks below are the split-crossing sharp edges: the first pair interleaves
// mutation with the traversal, the last starts the chain from an *absent* anchor
// (a strict range bound).

/// The key count the split-crossing walks use: strictly more than **two**
/// B+-tree nodes hold (`ORDER = 32` in `src/ordered_interior/node.rs`), so the
/// tree grows an internal root over several leaves and every walk below crosses
/// at least one node **split** boundary. It is spelled out rather than read from
/// the crate-private `ORDER` so the test states the scale it needs.
const SPLIT_KEYS: i64 = 100;

/// A two-tuple key `{k, k}` on `heap`.
///
/// A **boxed** key is deliberate: the anchors a walk gets back from
/// `first`/`next` are table-owned storage, so deleting the anchor mid-walk really
/// exercises "the anchor term outlives its own object" — an immediate key has no
/// storage to reclaim and would prove far less.
fn key_term(heap: &mut Heap, k: i64) -> Eterm {
    heap.tuple(&[make_small(k), make_small(k)])
        .expect("test heap has room")
}

/// Insert the object `{{k,k}, k}` (keypos 1, so the key is `{k,k}`) into `table`.
fn insert_key(table: &mut Table, heap: &mut Heap, atoms: &AtomTable, k: i64) {
    let key = key_term(heap, k);
    let object = heap
        .tuple(&[key, make_small(k)])
        .expect("test heap has room");
    table.insert(object, atoms).unwrap();
}

/// The direction of a `first`→`next` / `last`→`prev` chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Walk {
    /// `first`, then `next`: ascending arithmetic key order.
    Ascending,
    /// `last`, then `prev`: descending arithmetic key order.
    Descending,
}

impl Walk {
    /// The chain's own starting anchor: `first` or `last`.
    fn start(self, table: &Table, atoms: &AtomTable) -> Option<Eterm> {
        match self {
            Walk::Ascending => table.first(atoms),
            Walk::Descending => table.last(atoms),
        }
    }

    /// One step off `anchor`. An `ordered_set` reads the anchor as a **range
    /// bound**, so an anchor that was just deleted — or was never stored — still
    /// resolves; the chain ends at `'$end_of_table'`.
    fn step(self, table: &Table, anchor: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        let stepped = match self {
            Walk::Ascending => table.next(anchor, atoms),
            Walk::Descending => table.prev(anchor, atoms),
        };
        stepped.expect("an ordered_set anchor is a range bound, never a badarg")
    }

    /// The `(ahead, behind)` key values the mutation walk inserts while visiting
    /// the `j`-th original key (`j` is 1-based): `ahead` is beyond every original
    /// key in this direction's sense of travel — so a correct walk must reach it
    /// — and `behind` is already behind the cursor, so a correct walk must never
    /// revisit it.
    fn fresh_keys(self, j: i64) -> (i64, i64) {
        match self {
            Walk::Ascending => (SPLIT_KEYS + j, -j),
            Walk::Descending => (-j, SPLIT_KEYS + j),
        }
    }

    /// The key values a correct walk with the interleaved mutation visits, in
    /// this direction's exact arithmetic order: the originals `1..=SPLIT_KEYS`
    /// and then the `SPLIT_KEYS` keys the mutation inserted ahead of the cursor.
    fn expected(self) -> Vec<i64> {
        let n = SPLIT_KEYS;
        match self {
            Walk::Ascending => (1..=2 * n).collect(),
            Walk::Descending => (1..=n).rev().chain((-n..=-1).rev()).collect(),
        }
    }
}

/// THE walk driver both split-crossing tests share.
///
/// Chains `first`→`next` (`Walk::Ascending`) or `last`→`prev`
/// (`Walk::Descending`) over `table`, starting either at the chain's own end
/// (`anchor == None`) or by stepping off `anchor` — which may be absent, or
/// **just deleted**: this kind is `ordered_set`, so either is read as a range
/// bound. Every visited key (the **navigation** term, i.e. table-owned storage)
/// is handed to `visit`, which may mutate the table before the next step, and
/// collected; the walk always ends at `'$end_of_table'`.
fn walk(
    table: &mut Table,
    atoms: &AtomTable,
    dir: Walk,
    anchor: Option<Eterm>,
    mut visit: impl FnMut(&mut Table, Eterm),
) -> Vec<Eterm> {
    let mut cursor = match anchor {
        Some(key) => dir.step(table, key, atoms),
        None => dir.start(table, atoms),
    };
    let mut visited = Vec::new();
    while let Some(key) = cursor {
        visited.push(key);
        visit(table, key);
        cursor = dir.step(table, key, atoms);
    }
    visited
}

/// Assert `visited` is exactly `want`, position by position and in order — the
/// "no skip, no repeat, exact arithmetic-key order" check. `visited` holds
/// table-owned navigation terms while `want` is built on `heap`, so the two sides
/// are compared structurally (`=:=`, through `beam_term`).
fn assert_walk(visited: &[Eterm], want: &[i64], heap: &mut Heap, atoms: &AtomTable, what: &str) {
    assert_eq!(
        visited.len(),
        want.len(),
        "{what}: the walk visited {} keys, expected {} — a skip or a repeat",
        visited.len(),
        want.len()
    );
    for (position, (got, value)) in visited.iter().zip(want).enumerate() {
        let expected = key_term(heap, *value);
        assert!(
            eq(*got, expected, atoms),
            "{what}: the key visited at position {position} is out of order \
             (expected the key {{{value},{value}}})"
        );
    }
}

/// Test (a)'s script, run once per direction: fill an `ordered_set` with more
/// keys than two B+-tree nodes hold, then walk it while **deleting every visited
/// key** (the anchor's own deletion) and inserting a fresh key ahead of and
/// behind the cursor at every original step. The walk must visit every key
/// exactly once, in exact arithmetic-key order, with no skip and no repeat.
fn assert_split_mutation_walk(dir: Walk) {
    let atoms = AtomTable::new();
    // `3 * SPLIT_KEYS` objects' worth of key tuples and objects, plus the
    // assertion terms: a few thousand words.
    let mut heap = Heap::new(1 << 14);
    let mut table = open_table(TableKind::OrderedSet);
    for value in 1..=SPLIT_KEYS {
        insert_key(&mut table, &mut heap, &atoms, value);
    }
    assert_eq!(table.size(), SPLIT_KEYS as usize);

    // Fixation keeps a deleted anchor's storage alive for the next step — without
    // it the walk would step off a reclaimed term, which is exactly what the fix
    // flag exists to prevent.
    table.set_fixation(true);

    let mut visits = 0i64;
    let visited = walk(&mut table, &atoms, dir, None, |tbl, key| {
        visits += 1;
        tbl.delete(key, &atoms).unwrap();
        if visits <= SPLIT_KEYS {
            let (ahead, behind) = dir.fresh_keys(visits);
            insert_key(tbl, &mut heap, &atoms, ahead);
            insert_key(tbl, &mut heap, &atoms, behind);
        }
    });
    let what = format!("the {dir:?} split-crossing mutation walk");
    assert_walk(&visited, &dir.expected(), &mut heap, &atoms, &what);

    // Every visited key was deleted, and the keys the mutation left *behind* the
    // cursor were never revisited, so they are all that is left of the table.
    assert_eq!(
        table.size(),
        SPLIT_KEYS as usize,
        "{what}: only the keys inserted behind the cursor may remain"
    );
    assert_eq!(
        table.deferred_objects(),
        2 * SPLIT_KEYS as usize,
        "{what}: the fixed table holds every deleted key's storage"
    );
    table.set_fixation(false);
    assert_eq!(table.deferred_objects(), 0);
}

/// (a) A `first`→`next` chain over **more keys than two B+-tree nodes hold**
/// (100 keys against an order of 32), interleaved with mutation: the walk deletes
/// each visited key — the anchor's own deletion — and inserts a fresh key ahead
/// of *and* behind the cursor at every original step, yet still visits every key
/// exactly once, in exact ascending arithmetic-key order, with no skip and no
/// repeat.
#[test]
fn a_split_crossing_ascending_walk_interleaved_with_mutations_visits_every_key_once() {
    assert_split_mutation_walk(Walk::Ascending);
}

/// (a) The `last`→`prev` mirror of the walk above, over the same key count.
#[test]
fn a_split_crossing_descending_walk_interleaved_with_mutations_visits_every_key_once() {
    assert_split_mutation_walk(Walk::Descending);
}

/// (b) A chain walk started from an **absent-key anchor**. An `ordered_set` reads
/// the anchor as a range bound, so a key strictly between two stored keys resumes
/// at the next stored key and crosses the split boundary to the end, a key below
/// `first` starts the chain at `first`, and a key above `last` is immediately
/// `'$end_of_table'` — in both directions.
#[test]
fn an_absent_key_anchor_is_a_range_bound_across_the_split_boundary() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1 << 14);
    let mut table = open_table(TableKind::OrderedSet);
    // The stored keys are the even values `2, 4, ..., 2 * SPLIT_KEYS` as `{k,k}`
    // tuples, so every odd two-tuple is a key that was never stored.
    for value in 1..=SPLIT_KEYS {
        insert_key(&mut table, &mut heap, &atoms, 2 * value);
    }
    let stored_ascending: Vec<i64> = (1..=SPLIT_KEYS).map(|i| 2 * i).collect();
    let stored_descending: Vec<i64> = (1..=SPLIT_KEYS).rev().map(|i| 2 * i).collect();
    assert_eq!(table.size(), stored_ascending.len());

    // Both ends of the walks are `'$end_of_table'`.
    let last = table.last(&atoms).expect("a filled table has a last key");
    assert_eq!(
        table.next(last, &atoms).unwrap(),
        None,
        "'$end_of_table' past the last key"
    );
    let first = table.first(&atoms).expect("a filled table has a first key");
    assert_eq!(
        table.prev(first, &atoms).unwrap(),
        None,
        "'$end_of_table' before the first key"
    );

    // (1) Strictly between `{2,2}` and `{4,4}`: ascending resumes at `{4,4}` and
    // walks to the end — crossing every split boundary above the anchor —
    // descending ends at once, with only `{2,2}` below the anchor.
    let mid = key_term(&mut heap, 3);
    let visited = walk(&mut table, &atoms, Walk::Ascending, Some(mid), |_, _| {});
    assert_walk(
        &visited,
        &stored_ascending[1..],
        &mut heap,
        &atoms,
        "ascending from an absent mid anchor",
    );
    let visited = walk(&mut table, &atoms, Walk::Descending, Some(mid), |_, _| {});
    assert_walk(
        &visited,
        &stored_ascending[..1],
        &mut heap,
        &atoms,
        "descending from an absent mid anchor",
    );

    // (2) Below `first`: ascending walks the whole table, descending is over at
    // once.
    let below = key_term(&mut heap, 1);
    let visited = walk(&mut table, &atoms, Walk::Ascending, Some(below), |_, _| {});
    assert_walk(
        &visited,
        &stored_ascending,
        &mut heap,
        &atoms,
        "ascending from an absent anchor below `first`",
    );
    let visited = walk(&mut table, &atoms, Walk::Descending, Some(below), |_, _| {});
    assert!(
        visited.is_empty(),
        "descending from an absent anchor below `first` is `'$end_of_table'` \
         (visited {} keys)",
        visited.len()
    );

    // (3) Above `last`: the mirror — ascending is over at once, descending walks
    // the whole table.
    let above = key_term(&mut heap, 2 * SPLIT_KEYS + 1);
    let visited = walk(&mut table, &atoms, Walk::Ascending, Some(above), |_, _| {});
    assert!(
        visited.is_empty(),
        "ascending from an absent anchor above `last` is `'$end_of_table'` \
         (visited {} keys)",
        visited.len()
    );
    let visited = walk(&mut table, &atoms, Walk::Descending, Some(above), |_, _| {});
    assert_walk(
        &visited,
        &stored_descending,
        &mut heap,
        &atoms,
        "descending from an absent anchor above `last`",
    );

    assert_eq!(
        table.size(),
        stored_ascending.len(),
        "an absent-key walk never mutates the table"
    );
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
