//! Core `set`/`ordered_set` object operations: insert, lookup, member, delete,
//! `delete_object`, `delete_all_objects`, `size`, `tab2list`, `insert_list`,
//! `insert_new` and malformed-object rejection.

mod common;

use beam_common::BeamError;
use beam_ets::{Table, TableId, TableKind, TableOptions};
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