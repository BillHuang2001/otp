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

//! Core `set`/`ordered_set` object operations: insert, lookup, member, delete,
//! `delete_object`, `delete_all_objects`, `size`, `tab2list`, `insert_list`,
//! `insert_new` and malformed-object rejection.

mod common;

use beam_common::BeamError;
use beam_ets::{compile, key_of, MatchSpec, SetInterior, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;
use common::*;

#[test]
fn insert_lookup_member_delete_size_tab2list() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);

        assert!(t.is_empty());
        assert_eq!(t.size(), 0);
        assert!(t.tab2list().is_empty());

        let o1 = object(&mut heap, 1, 10);
        t.insert(o1, &atoms).unwrap();
        assert!(t.member(make_small(1), &atoms));
        assert!(!t.member(make_small(2), &atoms));
        assert_eq!(t.size(), 1);
        assert!(!t.is_empty());

        let looked = t.lookup(make_small(1), &atoms);
        assert_eq!(looked.len(), 1);
        assert!(eq(looked[0], o1, &atoms));
        assert!(t.lookup(make_small(2), &atoms).is_empty());

        let all = t.tab2list();
        assert_eq!(all.len(), 1);
        assert!(eq(all[0], o1, &atoms));

        // `delete` drops the entry; deleting an absent key is a no-op.
        t.delete(make_small(1), &atoms).unwrap();
        assert!(!t.member(make_small(1), &atoms));
        assert_eq!(t.size(), 0);
        assert!(t.is_empty());
        t.delete(make_small(99), &atoms).unwrap();
        assert_eq!(t.size(), 0);
    }
}

#[test]
fn insert_replaces_the_object_on_an_existing_key() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = new_table(kind);

        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        t.insert(object(&mut heap, 1, 20), &atoms).unwrap();

        assert_eq!(t.size(), 1);
        let looked = t.lookup(make_small(1), &atoms);
        assert_eq!(looked.len(), 1);
        assert!(eq(looked[0], object(&mut heap, 1, 20), &atoms));
    }
}

#[test]
fn insert_new_is_false_when_the_key_exists() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = new_table(kind);

        assert!(t.insert_new(object(&mut heap, 1, 10), &atoms).unwrap());
        // The key now exists: a second `insert_new` is a no-op returning `false`.
        assert!(!t.insert_new(object(&mut heap, 1, 20), &atoms).unwrap());
        assert_eq!(t.size(), 1);
        let looked = t.lookup(make_small(1), &atoms);
        assert!(eq(looked[0], object(&mut heap, 1, 10), &atoms));

        // A fresh key inserts and returns `true`.
        assert!(t.insert_new(object(&mut heap, 2, 20), &atoms).unwrap());
        assert_eq!(t.size(), 2);
    }
}

#[test]
fn lookup_element_is_one_based_and_badarg_out_of_range() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = new_table(kind);

        let o = heap
            .tuple(&[make_small(1), make_small(10), make_small(20)])
            .unwrap();
        t.insert(o, &atoms).unwrap();

        assert_eq!(
            t.lookup_element(make_small(1), 1, &atoms).unwrap(),
            make_small(1)
        );
        assert_eq!(
            t.lookup_element(make_small(1), 2, &atoms).unwrap(),
            make_small(10)
        );
        assert_eq!(
            t.lookup_element(make_small(1), 3, &atoms).unwrap(),
            make_small(20)
        );

        // Out of range, 0 and an absent key are all `badarg`.
        assert!(matches!(
            t.lookup_element(make_small(1), 4, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.lookup_element(make_small(1), 0, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.lookup_element(make_small(99), 1, &atoms),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn delete_object_removes_only_a_matching_object() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);

        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

        // A non-matching object leaves the entry alone.
        t.delete_object(object(&mut heap, 1, 99), &atoms).unwrap();
        assert!(t.member(make_small(1), &atoms));

        // A structurally equal (freshly built) object removes the stored one.
        t.delete_object(object(&mut heap, 1, 10), &atoms).unwrap();
        assert!(!t.member(make_small(1), &atoms));
        assert_eq!(t.size(), 0);
    }
}

#[test]
fn delete_all_objects_empties_the_table() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);

        for k in 0..5 {
            t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
        }
        assert_eq!(t.size(), 5);

        t.delete_all_objects();
        assert_eq!(t.size(), 0);
        assert!(t.is_empty());
        assert!(t.tab2list().is_empty());
        assert!(!t.member(make_small(0), &atoms));
    }
}

#[test]
fn insert_list_inserts_every_object() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);

    let objects = [
        object(&mut heap, 1, 10),
        object(&mut heap, 2, 20),
        object(&mut heap, 3, 30),
    ];
    t.insert_list(&objects, &atoms).unwrap();
    assert_eq!(t.size(), 3);
    for k in 1..=3 {
        assert!(t.member(make_small(k), &atoms));
    }

    // An empty list is a no-op.
    t.insert_list(&[], &atoms).unwrap();
    assert_eq!(t.size(), 3);
}

/// `ets:insert/2` over a list is **atomic**: the whole list is validated first,
/// so one invalid element raises `badarg` and inserts none of the elements.
///
/// Verified against the system `erl` (OTP 29 / erts-17.1): with `{1,10}` in `T`,
/// `ets:insert(T, [{2,20}, foo])` raises `badarg` and `ets:tab2list(T)` is still
/// `[{1,10}]` — the valid element is not inserted either.
#[test]
fn insert_list_is_atomic_when_an_element_is_invalid() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    // A valid element followed by a non-tuple: nothing is inserted.
    let good = object(&mut heap, 2, 20);
    assert!(matches!(
        t.insert_list(&[good, make_small(9)], &atoms),
        Err(BeamError::BadArg)
    ));
    assert!(!t.member(make_small(2), &atoms));
    let only = t.tab2list();
    assert_eq!(only.len(), 1);
    assert!(eq(only[0], object(&mut heap, 1, 10), &atoms));

    // ... and the invalid element first behaves the same way.
    assert!(matches!(
        t.insert_list(&[make_small(9), good], &atoms),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.size(), 1);

    // A list of valid objects still inserts all of them, so the failed calls left
    // no half-applied state behind.
    t.insert_list(&[good, object(&mut heap, 3, 30)], &atoms).unwrap();
    assert_eq!(t.size(), 3);
}

/// The atomicity holds for a `{keypos, 2}` table too: an element shorter than
/// `keypos` (`{b}` is a 1-tuple) makes the whole list `badarg`.
///
/// Verified against the system `erl` (OTP 29 / erts-17.1).
#[test]
fn insert_list_is_atomic_under_a_custom_keypos() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut opts = TableOptions::new();
    opts.set_keypos(2).unwrap();
    let mut t = Table::new(TableId::next(), opts).unwrap();

    t.insert(tuple_of(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();

    let valid = tuple_of(&mut heap, &[make_small(2), make_small(20)]);
    let too_short = tuple_of(&mut heap, &[make_small(3)]);
    assert!(matches!(
        t.insert_list(&[valid, too_short], &atoms),
        Err(BeamError::BadArg)
    ));

    // The key is element 2 here, so `{2,20}` would have been keyed by `20`.
    assert_eq!(t.size(), 1);
    assert!(t.member(make_small(10), &atoms));
    assert!(!t.member(make_small(20), &atoms));
}

#[test]
fn insert_rejects_malformed_objects_with_badarg() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut t = new_table(TableKind::Set);

    // Non-tuples are not valid ETS objects.
    assert!(matches!(
        t.insert(make_small(1), &atoms),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(t.insert(NIL, &atoms), Err(BeamError::BadArg)));
    let list = proper_list(&mut heap, &[make_small(1)]);
    assert!(matches!(t.insert(list, &atoms), Err(BeamError::BadArg)));
    assert_eq!(t.size(), 0);

    // A keypos-2 table rejects a 1-tuple (arity < keypos).
    let mut opts = TableOptions::new();
    opts.set_keypos(2).unwrap();
    let mut t2 = Table::new(TableId::next(), opts).unwrap();
    let one = heap.tuple(&[make_small(1)]).unwrap();
    assert!(matches!(t2.insert(one, &atoms), Err(BeamError::BadArg)));
    assert_eq!(t2.size(), 0);
}

#[test]
fn take_returns_the_objects_and_reclaims_the_stored_ones() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);

    // `take` returns a structurally equal object and removes the key.
    let o = object(&mut heap, 1, 10);
    t.insert(o, &atoms).unwrap();
    assert_eq!(t.size(), 1);
    let taken = t.take(make_small(1), &atoms).unwrap();
    assert_eq!(taken.len(), 1);
    assert!(
        eq(taken[0], o, &atoms),
        "take returns an object equal to the stored one"
    );
    assert!(!t.member(make_small(1), &atoms));
    assert_eq!(t.size(), 0);
    assert!(t.is_empty());

    // An absent key (including a second `take` of the same key) is empty, not an
    // error.
    assert!(t.take(make_small(1), &atoms).unwrap().is_empty());
    assert!(t.take(make_small(9), &atoms).unwrap().is_empty());

    // Reclamation: `insert` + `take` of the same object must not grow the table's
    // memory. Each removed object's storage is reclaimed after being copied out, so
    // 1000 cycles cost no more than 10 (a leak would grow the figure linearly —
    // ~3 words per cycle).
    let churn = object(&mut heap, 2, 20);
    for _ in 0..10 {
        t.insert(churn, &atoms).unwrap();
        assert_eq!(t.take(make_small(2), &atoms).unwrap().len(), 1);
    }
    let after_10 = t.memory();
    for _ in 0..1000 {
        t.insert(churn, &atoms).unwrap();
        assert_eq!(t.take(make_small(2), &atoms).unwrap().len(), 1);
    }
    assert_eq!(
        t.memory(),
        after_10,
        "take must reclaim the removed objects (no build-up under churn)"
    );
    assert_eq!(t.size(), 0);
}

#[test]
fn a_taken_object_stays_valid_while_the_store_is_churned() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(4096);
    let mut t = new_table(TableKind::Set);

    // Take an object and hold on to it.
    let expected = object(&mut heap, 1, 10);
    t.insert(expected, &atoms).unwrap();
    let captured = t.take(make_small(1), &atoms).unwrap().pop().unwrap();

    // Churn the store hard enough to force free-list reuse of the same-sized runs:
    // insert many, delete them all, then `insert`+`take` a same-size object 50 times.
    // A returned taken object must never sit in reusable storage, so it must still
    // read back exactly as it was handed out.
    for k in 2..300i64 {
        t.insert(object(&mut heap, k, k), &atoms).unwrap();
    }
    for k in 2..300i64 {
        t.delete(make_small(k), &atoms).unwrap();
    }
    for _ in 0..50 {
        t.insert(object(&mut heap, 5, 55), &atoms).unwrap();
        assert_eq!(t.take(make_small(5), &atoms).unwrap().len(), 1);
    }

    // The captured copy is untouched.
    assert!(
        eq(captured, expected, &atoms),
        "a taken object must not be overwritten by later store activity"
    );
    assert_eq!(
        cmp(captured, expected, &atoms),
        core::cmp::Ordering::Equal,
        "the captured object still compares equal"
    );
}

// ---------------------------------------------------------------------------
// Bounded memory under removal churn (per-object reclamation)
// ---------------------------------------------------------------------------
//
// Every removal path must reclaim the removed object's **table-owned storage**,
// so a long insert/remove loop reaches a stable figure and stays there: a path
// that forgot to `free` the removed root would grow `Table::memory()` linearly
// with the cycle count. Each cycle therefore inserts one object per key and then
// removes them all, and the proof is that the figure after a *long* churn equals
// the figure after a *short* one (a leak of even one store word per cycle shows
// up as a four-figure difference over the long run).
//
// The objects carry a proper-list payload so a leak is measurable: a `{Key,
// Value}` tuple of small ints is three store words, a list payload makes each
// object's run an order of magnitude larger. (An immediate-only object could not
// leak at all — an immediate owns no store storage.)

/// The keys one churn cycle inserts.
const CHURN_KEYS: i64 = 64;

/// Cycles run before the first measurement (the store and the interior reach
/// their high-water mark during the very first populate).
const CHURN_WARMUP_CYCLES: usize = 1;

/// Cycles run before the "short" measurement.
const CHURN_SHORT_CYCLES: usize = 10;

/// Cycles run before the "long" measurement. A per-cycle leak of one store word
/// would add `990 * 64` words between the two measurements.
const CHURN_LONG_CYCLES: usize = 1000;

/// A `{Key, Payload}` object whose payload is a proper list of `len` small ints,
/// so the object owns a contiguous, reclaimable run of store words.
fn payload_object(heap: &mut Heap, key: i64, len: usize) -> Eterm {
    let elements: Vec<Eterm> = (0..len as i64).map(|i| make_small(key * 1000 + i)).collect();
    let payload = proper_list(heap, &elements);
    tuple_of(heap, &[make_small(key), payload])
}

/// The atoms the churn specs reference: `'_'` (the head wildcard) and `true`
/// (the `select_delete` body value, resolved by name).
fn churn_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in ["_", "true"] {
        atoms.intern(name);
    }
    atoms
}

/// The objects one churn cycle inserts, built **once** so the long loop never
/// exhausts the test heap (`beam_term::heap::Heap` is a bump arena). Terms are
/// immutable, so the very same terms can be inserted by every cycle: the table
/// deep-copies on insert and never retains a caller pointer.
fn churn_objects(heap: &mut Heap) -> Vec<Eterm> {
    (0..CHURN_KEYS).map(|key| payload_object(heap, key, 6)).collect()
}

/// Which removal path a churn cycle exercises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Removal {
    /// `Tab::delete/2` — remove every object under a key, one key at a time.
    DeleteKey,
    /// `Tab::delete_object/2` — remove the exact object.
    DeleteObject,
    /// `Tab::match_delete/2`.
    MatchDelete,
    /// `Tab::select_delete/2`.
    SelectDelete,
    /// `Tab::delete_all_objects/1`.
    DeleteAll,
    /// No removal: re-`insert` the same objects, replacing them in place.
    Overwrite,
}

/// The table kinds and `set` interiors the memory proofs sweep.
///
/// `SetInterior::Chained` is listed separately because it is an *opt-in* interior
/// ([`Table::with_set_interior`]) with its own allocation profile.
fn churn_shapes() -> Vec<(TableKind, SetInterior)> {
    vec![
        (TableKind::Set, SetInterior::Champ),
        (TableKind::Set, SetInterior::Chained),
        (TableKind::Bag, SetInterior::Champ),
        (TableKind::DuplicateBag, SetInterior::Champ),
        (TableKind::OrderedSet, SetInterior::Champ),
    ]
}

/// One churn cycle: insert every object, then apply `path`'s removal.
fn churn_cycle(
    t: &mut Table,
    atoms: &AtomTable,
    objects: &[Eterm],
    pattern: Eterm,
    spec: &MatchSpec,
    out: &mut beam_alloc::Heap,
    path: Removal,
) {
    for &object in objects {
        t.insert(object, atoms).expect("every churn object is insertable");
    }
    match path {
        Removal::DeleteKey => {
            for &object in objects {
                let key = key_of(object, 1).expect("a tuple object has a key");
                t.delete(key, atoms).expect("delete");
            }
        }
        Removal::DeleteObject => {
            for &object in objects {
                t.delete_object(object, atoms).expect("delete_object");
            }
        }
        Removal::MatchDelete => {
            let deleted = t
                .match_delete(pattern, atoms, out)
                .expect("match_delete on a {Key, Payload} pattern");
            assert_eq!(deleted, CHURN_KEYS as usize, "match_delete removes them all");
        }
        Removal::SelectDelete => {
            let deleted = t
                .select_delete(spec, atoms, out)
                .expect("select_delete with a `true` body");
            assert_eq!(deleted, CHURN_KEYS as usize, "select_delete removes them all");
        }
        Removal::DeleteAll => t.delete_all_objects(),
        // An overwrite replaces the stored object in place, so nothing is removed
        // and the table still holds one object per key.
        Removal::Overwrite => {}
    }
    match path {
        Removal::Overwrite => assert_eq!(t.size(), CHURN_KEYS as usize, "size after overwrite"),
        _ => assert_eq!(t.size(), 0, "size after removal"),
    }
}

/// The bounded-memory proof for one `(kind, set interior)` shape and one removal
/// path: after the warm-up cycle the table's memory must not grow any further,
/// however many cycles are run.
fn assert_memory_is_bounded(path: Removal, kind: TableKind, set_interior: SetInterior) {
    let mut atoms = churn_atoms();
    let mut heap = Heap::new(16 * 1024);
    let objects = churn_objects(&mut heap);

    // `{'_', '_'}` — every churn object is a 2-tuple, so this matches them all.
    let wildcard = interned_atom(&mut atoms, "_");
    let pattern = tuple_of(&mut heap, &[wildcard, wildcard]);

    // `[{{'_','_'},[], [true]}]` — the `select_delete` form of `match_delete`.
    let body = proper_list(&mut heap, &[interned_atom(&mut atoms, "true")]);
    let clause = tuple_of(&mut heap, &[pattern, NIL, body]);
    let spec_term = proper_list(&mut heap, &[clause]);
    let spec = compile(spec_term, &atoms).expect("the churn spec compiles");

    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    let mut t = Table::with_set_interior(TableId::next(), opts, set_interior)
        .expect("a fresh table is constructible");
    let mut out = beam_alloc::Heap::new(4096).expect("a test heap is allocatable");

    for _ in 0..CHURN_WARMUP_CYCLES {
        churn_cycle(&mut t, &atoms, &objects, pattern, &spec, &mut out, path);
    }
    let after_warmup = t.memory();

    for _ in 0..CHURN_SHORT_CYCLES {
        churn_cycle(&mut t, &atoms, &objects, pattern, &spec, &mut out, path);
    }
    let after_short = t.memory();

    for _ in 0..CHURN_LONG_CYCLES {
        churn_cycle(&mut t, &atoms, &objects, pattern, &spec, &mut out, path);
    }
    let after_long = t.memory();

    let what = format!("{kind:?}/{set_interior:?} under {path:?}");
    assert_eq!(
        after_short, after_long,
        "{what}: memory must stabilize, not grow with the cycle count \
         (warmup {after_warmup}, after {CHURN_SHORT_CYCLES} {after_short}, \
         after {} {after_long})",
        CHURN_SHORT_CYCLES + CHURN_LONG_CYCLES
    );
    assert!(
        after_long <= after_warmup,
        "{what}: the high-water mark is reached by the first cycle \
         ({after_warmup} -> {after_long})"
    );
}

/// Run [`assert_memory_is_bounded`] for `path` over every kind/interior shape.
///
/// `duplicate_bag` is skipped for `Overwrite`: re-inserting the same object into a
/// multiset legally adds another copy (`ets:insert` on a `duplicate_bag` never
/// deduplicates), so its memory is *expected* to grow — that is the kind's
/// semantics, not a reclamation leak.
fn assert_memory_is_bounded_for_every_shape(path: Removal) {
    for (kind, set_interior) in churn_shapes() {
        if path == Removal::Overwrite && kind == TableKind::DuplicateBag {
            continue;
        }
        assert_memory_is_bounded(path, kind, set_interior);
    }
}

#[test]
fn delete_churn_keeps_memory_bounded() {
    assert_memory_is_bounded_for_every_shape(Removal::DeleteKey);
}

#[test]
fn delete_object_churn_keeps_memory_bounded() {
    assert_memory_is_bounded_for_every_shape(Removal::DeleteObject);
}

#[test]
fn match_delete_churn_keeps_memory_bounded() {
    assert_memory_is_bounded_for_every_shape(Removal::MatchDelete);
}

#[test]
fn select_delete_churn_keeps_memory_bounded() {
    assert_memory_is_bounded_for_every_shape(Removal::SelectDelete);
}

#[test]
fn delete_all_objects_churn_keeps_memory_bounded() {
    assert_memory_is_bounded_for_every_shape(Removal::DeleteAll);
}

#[test]
fn overwrite_in_place_keeps_memory_bounded() {
    assert_memory_is_bounded_for_every_shape(Removal::Overwrite);
}

/// A `bag` rejects a re-inserted *identical* object, and the rejected copy must be
/// reclaimed rather than orphaned in the store — the third way an insert can walk
/// away from storage it allocated.
#[test]
fn a_bag_rejected_duplicate_does_not_grow_memory() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(1024);
    let mut t = new_table(TableKind::Bag);

    let object = payload_object(&mut heap, 1, 6);
    t.insert(object, &atoms).unwrap();
    let after_first = t.memory();
    assert_eq!(t.size(), 1);

    // Every further insert of the same object under the same key is a no-op, and
    // the deep copy it made is freed again.
    for _ in 0..1000 {
        t.insert(object, &atoms).unwrap();
    }
    assert_eq!(t.size(), 1);
    assert_eq!(
        t.memory(),
        after_first,
        "a rejected duplicate's copy must be reclaimed"
    );

    // The stored object really is the one inserted first.
    let stored = t.lookup(make_small(1), &atoms);
    assert_eq!(stored.len(), 1);
    assert!(eq(stored[0], object, &atoms));
}

/// The keys one *rotating* churn cycle inserts.
const ROTATING_KEYS: i64 = 32;

/// Rotating cycles before the "short" measurement.
const ROTATING_SHORT_CYCLES: i64 = 6;

/// Rotating cycles before the "long" measurement: every cycle uses a **fresh** key
/// window (`cycle * ROTATING_KEYS + k`), so the index is built and torn down along
/// many distinct hash/allocation paths.
const ROTATING_LONG_CYCLES: i64 = 500;

/// The strongest bounded-memory proof: a *different* key window every cycle, so an
/// interior that retained a node it no longer needs (a collapsed-away CHAMP
/// sub-node, a B+-tree arena slot, a stale hash bucket entry) would accumulate one
/// per cycle instead of reaching a stable figure.
fn assert_rotating_churn_is_bounded(kind: TableKind, set_interior: SetInterior, path: Removal) {
    let atoms = AtomTable::new();
    // A fresh heap per shape/path: each cycle allocates its own objects (they are
    // only needed until the cycle's removal), and a bump arena never rewinds.
    let mut heap = Heap::new(192 * 1024);
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    let mut t = Table::with_set_interior(TableId::next(), opts, set_interior)
        .expect("a fresh table is constructible");

    let mut after_short = None;
    for cycle in 0..(ROTATING_SHORT_CYCLES + ROTATING_LONG_CYCLES) {
        let objects: Vec<Eterm> = (0..ROTATING_KEYS)
            .map(|k| payload_object(&mut heap, cycle * ROTATING_KEYS + k, 1))
            .collect();
        for &object in &objects {
            t.insert(object, &atoms).expect("insert");
        }
        match path {
            Removal::DeleteKey => {
                for &object in &objects {
                    let key = key_of(object, 1).expect("a tuple object has a key");
                    t.delete(key, &atoms).expect("delete");
                }
            }
            Removal::DeleteObject => {
                for &object in &objects {
                    t.delete_object(object, &atoms).expect("delete_object");
                }
            }
            Removal::DeleteAll => t.delete_all_objects(),
            // The rotating proof drives only the simple removal paths.
            Removal::MatchDelete | Removal::SelectDelete | Removal::Overwrite => {
                unreachable!("the rotating churn drives delete/delete_object/delete_all only")
            }
        }
        assert_eq!(t.size(), 0, "size after a rotating cycle");
        if cycle == ROTATING_SHORT_CYCLES - 1 {
            after_short = Some(t.memory());
        }
    }

    let after_short = after_short.expect("the short measurement is taken");
    let after_long = t.memory();
    let what = format!("{kind:?}/{set_interior:?} under {path:?} with rotating keys");
    assert_eq!(
        after_short, after_long,
        "{what}: memory must stabilize across distinct key windows \
         (after {ROTATING_SHORT_CYCLES} {after_short}, after {} {after_long})",
        ROTATING_SHORT_CYCLES + ROTATING_LONG_CYCLES
    );
}

#[test]
fn rotating_key_churn_keeps_memory_bounded() {
    for (kind, set_interior) in churn_shapes() {
        for path in [Removal::DeleteKey, Removal::DeleteObject, Removal::DeleteAll] {
            assert_rotating_churn_is_bounded(kind, set_interior, path);
        }
    }
}