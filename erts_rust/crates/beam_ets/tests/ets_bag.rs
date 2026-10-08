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

//! `bag` semantics over the shared hash interior.

mod common;

use beam_common::BeamError;
use beam_ets::{TableKind, TableOptions};
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

/// `ets:lookup_element/3` on a `bag` returns the **list** of the `Pos`-th
/// elements of *all* objects under `Key` (which is why the crate's scalar
/// [`Table::lookup_element`] refuses the multi kinds and this
/// [`Table::lookup_element_all`] exists).
///
/// Verified against the system `erl` (OTP 29 / erts-17.1): a lone object yields a
/// one-element list, duplicates are preserved, an absent key is `badarg`, `Pos == 0`
/// is `badarg`, and one object shorter than `Pos` makes the whole call `badarg`.
#[test]
fn lookup_element_all_returns_the_element_list_for_the_multi_kinds() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(512);

    // `{k, Value, Tag}` — three objects under the same key.
    let mut bag = new_table(TableKind::Bag);
    bag.insert(tuple_of(&mut heap, &[make_small(1), make_small(10), make_small(100)]), &atoms)
        .unwrap();
    bag.insert(tuple_of(&mut heap, &[make_small(1), make_small(20), make_small(200)]), &atoms)
        .unwrap();
    bag.insert(tuple_of(&mut heap, &[make_small(1), make_small(30), make_small(300)]), &atoms)
        .unwrap();

    let values = bag.lookup_element_all(make_small(1), 2, &atoms).unwrap();
    assert_eq!(values.len(), 3, "one element per object under the key");
    for (got, want) in values.iter().zip([10, 20, 30]) {
        assert!(eq(*got, make_small(want), &atoms), "expected {want}, got {got:?}");
    }
    // The key element repeats once per object.
    let keys = bag.lookup_element_all(make_small(1), 1, &atoms).unwrap();
    assert_eq!(keys.len(), 3);
    assert!(keys.iter().all(|k| eq(*k, make_small(1), &atoms)));

    // A key holding a single object still yields a one-element list.
    bag.insert(object(&mut heap, 2, 7), &atoms).unwrap();
    let lone = bag.lookup_element_all(make_small(2), 2, &atoms).unwrap();
    assert_eq!(lone.len(), 1);
    assert!(eq(lone[0], make_small(7), &atoms));

    // `duplicate_bag` keeps identical objects, so identical elements repeat.
    let mut dup = new_table(TableKind::DuplicateBag);
    dup.insert(object(&mut heap, 5, 1), &atoms).unwrap();
    dup.insert(object(&mut heap, 5, 1), &atoms).unwrap();
    dup.insert(object(&mut heap, 5, 2), &atoms).unwrap();
    let elements = dup.lookup_element_all(make_small(5), 2, &atoms).unwrap();
    assert_eq!(elements.len(), 3);
    assert!(eq(elements[0], make_small(1), &atoms));
    assert!(eq(elements[1], make_small(1), &atoms));
    assert!(eq(elements[2], make_small(2), &atoms));

    // The single-object kinds store one object per key, so their list has one
    // element — the same one the scalar form returns.
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let mut t = new_table(kind);
        t.insert(object(&mut heap, 3, 33), &atoms).unwrap();
        let one = t.lookup_element_all(make_small(3), 2, &atoms).unwrap();
        assert_eq!(one.len(), 1, "{kind:?}");
        assert!(eq(one[0], t.lookup_element(make_small(3), 2, &atoms).unwrap(), &atoms));
    }
}

/// The element-list projection fails exactly where the oracle fails, and the `/4`
/// form reports an absent key as `None` (the caller substitutes the default).
#[test]
fn lookup_element_all_reports_the_oracle_failures() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut bag = new_table(TableKind::Bag);
    bag.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    assert!(matches!(
        bag.lookup_element_all(make_small(1), 0, &atoms),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        bag.lookup_element_all(make_small(99), 1, &atoms),
        Err(BeamError::BadArg)
    ));
    // `{1,10}` has arity 2, so position 3 is out of range for the whole call.
    assert!(matches!(
        bag.lookup_element_all(make_small(1), 3, &atoms),
        Err(BeamError::BadArg)
    ));

    let default = make_small(-1);
    assert_eq!(
        bag.lookup_element_all_or(make_small(99), 9, default, &atoms)
            .unwrap(),
        None
    );
    assert!(matches!(
        bag.lookup_element_all_or(make_small(99), 0, default, &atoms),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        bag.lookup_element_all_or(make_small(1), 3, default, &atoms),
        Err(BeamError::BadArg)
    ));
    let present = bag
        .lookup_element_all_or(make_small(1), 2, default, &atoms)
        .unwrap()
        .expect("a present key yields its elements");
    assert_eq!(present.len(), 1);
    assert!(eq(present[0], make_small(10), &atoms));

    // The scalar form must never fabricate one of those elements.
    let err = bag
        .lookup_element(make_small(1), 2, &atoms)
        .expect_err("a scalar answer is undefined for a bag");
    let raise = err
        .into_raise()
        .expect("the multi-kind refusal is an explicit raise");
    assert_eq!(raise.class, beam_common::ExceptionClass::Error);
    assert!(
        raise
            .reason
            .as_atom()
            .expect("the reason is an atom")
            .starts_with("unsupported_ets: ets:lookup_element/3,4"),
        "the raise names the unsupported scalar lookup"
    );

    // The table is still usable (nothing was mutated by the refusals).
    assert_eq!(bag.size(), 1);
    bag.insert(object(&mut heap, 1, 20), &atoms).unwrap();
    assert_eq!(bag.lookup_element_all(make_small(1), 2, &atoms).unwrap().len(), 2);
}

/// A `{keypos, 2}` `bag`: the projected position is independent of `keypos`.
#[test]
fn lookup_element_all_honours_a_custom_keypos() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::Bag);
    opts.set_keypos(2).unwrap();
    let mut t = beam_ets::Table::new(beam_ets::TableId::next(), opts).unwrap();

    // `{Tag, Key}` objects: element 2 is the key, element 1 the tag.
    t.insert(tuple_of(&mut heap, &[make_small(1), make_small(50)]), &atoms)
        .unwrap();
    t.insert(tuple_of(&mut heap, &[make_small(2), make_small(50)]), &atoms)
        .unwrap();

    let keys = t.lookup_element_all(make_small(50), 2, &atoms).unwrap();
    assert_eq!(keys.len(), 2);
    assert!(keys.iter().all(|k| eq(*k, make_small(50), &atoms)));
    let tags = t.lookup_element_all(make_small(50), 1, &atoms).unwrap();
    assert_eq!(tags.len(), 2);
    assert!(eq(tags[0], make_small(1), &atoms) && eq(tags[1], make_small(2), &atoms));
}