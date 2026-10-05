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
