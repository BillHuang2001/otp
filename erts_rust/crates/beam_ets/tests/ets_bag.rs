//! `bag` semantics over the shared hash interior.

mod common;

use beam_ets::TableKind;
use beam_term::prelude::*;
use common::*;

#[test]
fn bag_stores_distinct_objects_per_key() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut t = new_table(TableKind::Bag);

    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
    // An identical object is a no-op for a bag.
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    assert_eq!(t.size(), 2);
    let looked = t.lookup(make_small(1), &atoms);
    assert_eq!(looked.len(), 2);
    assert!(looked
        .iter()
        .any(|o| eq(*o, object(&mut heap, 1, 10), &atoms)));
    assert!(looked
        .iter()
        .any(|o| eq(*o, object(&mut heap, 1, 20), &atoms)));

    // `delete` removes the whole key (all its objects).
    t.delete(make_small(1), &atoms).unwrap();
    assert_eq!(t.size(), 0);
    assert!(t.is_empty());
}

#[test]
fn bag_duplicate_insert_does_not_leak_a_copy() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Bag);

    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    let one = t.memory();

    // An identical object is a no-op: the interior rejects the deep copy, so the
    // table reclaims it and the memory figure does not grow — no orphaned copy is
    // left behind for `delete_all_objects` to clean up.
    for _ in 0..20 {
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    }
    assert_eq!(
        t.memory(),
        one,
        "a rejected `bag` duplicate must not leak a copy"
    );
    assert_eq!(t.size(), 1);

    // A structurally *different* object under the same key IS stored and does grow.
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
    assert!(
        t.memory() > one,
        "a distinct object is stored and grows the memory count"
    );
    assert_eq!(t.size(), 2);
}

#[test]
fn insert_new_on_an_existing_key_does_not_leak_a_copy() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);

        assert!(t.insert_new(object(&mut heap, 1, 10), &atoms).unwrap());
        let one = t.memory();

        // Every refused `insert_new` deep-copies then discards the object; the copy
        // must be reclaimed, so the memory figure stays put.
        for _ in 0..20 {
            assert!(!t.insert_new(object(&mut heap, 1, 20), &atoms).unwrap());
        }
        assert_eq!(
            t.memory(),
            one,
            "{kind:?}: a refused `insert_new` must not leak a copy"
        );
        assert_eq!(t.size(), 1);
    }
}