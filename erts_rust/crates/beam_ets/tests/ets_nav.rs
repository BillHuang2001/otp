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

//! Navigation (`first`/`next`/`last`/`prev`), `take/2` and `lookup_element/4`.
//!
//! The edge semantics below mirror the system `erl` (OTP 29 / erts-17.1), verified
//! with `erl -noshell -eval '...'`:
//!
//!   * an empty table: `first`/`last` return `'$end_of_table'` for every kind;
//!     `next`/`prev` return `'$end_of_table'` for `ordered_set` but raise `badarg`
//!     for the hash kinds (their walk is anchored at a present entry);
//!   * an absent anchor key: `ordered_set` treats it as a range bound (the nearest
//!     stored key above/below it) while the hash kinds raise `badarg`;
//!   * `next` past the largest stored key is `'$end_of_table'` for every kind.

mod common;

use beam_common::BeamError;
use beam_ets::TableKind;
use beam_term::prelude::*;
use common::*;

#[test]
fn first_and_last_are_none_for_every_kind_on_an_empty_table() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let atoms = AtomTable::new();
        let t = new_table(kind);
        assert_eq!(t.first(&atoms), None, "{kind:?} empty first");
        assert_eq!(t.last(&atoms), None, "{kind:?} empty last");
    }
}

#[test]
fn next_and_prev_on_an_empty_table_depend_on_the_kind() {
    let atoms = AtomTable::new();
    // `ordered_set` reads the key as a range bound, so an empty table is simply
    // `'$end_of_table'` — not an error.
    let ordered = new_table(TableKind::OrderedSet);
    assert_eq!(ordered.next(make_small(5), &atoms).unwrap(), None);
    assert_eq!(ordered.prev(make_small(5), &atoms).unwrap(), None);

    // The hash kinds anchor the walk at a present entry, so every key of an empty
    // table is unknown and `ets:next/2` raises.
    for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
        let t = new_table(kind);
        assert!(
            matches!(t.next(make_small(5), &atoms), Err(BeamError::BadArg)),
            "{kind:?} empty next"
        );
        assert!(
            matches!(t.prev(make_small(5), &atoms), Err(BeamError::BadArg)),
            "{kind:?} empty prev"
        );
    }
}

#[test]
fn ordered_set_navigation_is_a_cmp_ordered_range_scan() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut t = new_table(TableKind::OrderedSet);
    for k in [3i64, 1, 2] {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    assert_eq!(t.first(&atoms), Some(make_small(1)));
    assert_eq!(t.last(&atoms), Some(make_small(3)));

    // Present keys: the strict successor/predecessor, with `'$end_of_table'` at the
    // two ends.
    assert_eq!(t.next(make_small(1), &atoms).unwrap(), Some(make_small(2)));
    assert_eq!(t.next(make_small(2), &atoms).unwrap(), Some(make_small(3)));
    assert_eq!(t.next(make_small(3), &atoms).unwrap(), None);
    assert_eq!(t.prev(make_small(3), &atoms).unwrap(), Some(make_small(2)));
    assert_eq!(t.prev(make_small(2), &atoms).unwrap(), Some(make_small(1)));
    assert_eq!(t.prev(make_small(1), &atoms).unwrap(), None);

    // Absent keys are range bounds, never errors.
    assert_eq!(t.next(make_small(0), &atoms).unwrap(), Some(make_small(1)));
    assert_eq!(t.next(make_small(99), &atoms).unwrap(), None);
    assert_eq!(t.prev(make_small(99), &atoms).unwrap(), Some(make_small(3)));
    assert_eq!(t.prev(make_small(0), &atoms).unwrap(), None);

    // Deleting everything empties the navigation again.
    t.delete_all_objects();
    assert_eq!(t.first(&atoms), None);
    assert_eq!(t.last(&atoms), None);
    assert_eq!(t.next(make_small(1), &atoms).unwrap(), None);
    assert_eq!(t.prev(make_small(1), &atoms).unwrap(), None);
}

#[test]
fn hash_navigation_walks_every_key_once_and_rejects_an_absent_anchor() {
    for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);
        for k in [3i64, 1, 2] {
            t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
        }

        // The hash order is internal (bucket order), so only the *set* of visited
        // keys is contract: `first` + `next` visits each stored key exactly once.
        let mut walk = Vec::new();
        let mut cursor = t.first(&atoms);
        while let Some(key) = cursor {
            walk.push(key);
            cursor = t.next(key, &atoms).unwrap();
        }
        assert_eq!(walk.len(), 3, "{kind:?} walk length");
        for k in [1i64, 2, 3] {
            assert!(
                walk.iter()
                    .any(|&visited| eq(visited, make_small(k), &atoms)),
                "{kind:?} walk covers key {k}"
            );
        }

        // `last` closes the same order, and `prev(first)` is the start of it.
        let first = t.first(&atoms).unwrap();
        assert_eq!(t.last(&atoms), walk.last().copied(), "{kind:?} last");
        assert_eq!(t.prev(first, &atoms).unwrap(), None, "{kind:?} prev(first)");
        let last = t.last(&atoms).unwrap();
        assert_eq!(t.next(last, &atoms).unwrap(), None, "{kind:?} next(last)");

        // An anchor key that is not stored is `badarg` (verified `erl` behaviour).
        assert!(matches!(
            t.next(make_small(0), &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.next(make_small(99), &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.prev(make_small(0), &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.prev(make_small(99), &atoms),
            Err(BeamError::BadArg)
        ));

        // A multi kind reports one key per entry, so a re-walk of a `bag` whose key
        // holds several objects still visits that key once.
        if kind.is_multi() {
            t.insert(object(&mut heap, 1, 11), &atoms).unwrap();
            let mut second_walk = Vec::new();
            let mut cursor = t.first(&atoms);
            while let Some(key) = cursor {
                second_walk.push(key);
                cursor = t.next(key, &atoms).unwrap();
            }
            assert_eq!(second_walk.len(), 3, "{kind:?} multi-object walk");
        }
    }
}

/// The hash kinds navigate in an **unspecified internal order**: the ETS contract
/// fixes *which* keys are reachable and how an absent anchor is rejected, never the
/// order. What is contract — and is pinned here — is:
///
///   * `first`/`last` return a **present** key (never a bogus one) or
///     `'$end_of_table'`;
///   * the `first` + `next` walk visits every stored key exactly once, and `last`
///     closes that same walk (`prev(first)` and `next(last)` are `'$end_of_table'`);
///   * `next`/`prev` on an **absent** anchor key are `badarg` — the walk is anchored
///     at a present entry.
///
/// Rationalised against the real `erl` (OTP 29 / erts-17.1): a 3-key `set` answers
/// `first = last = 1` there, but that is the *internal* order, not a rule — so this
/// test deliberately asserts nothing of the form `last == first`.
#[test]
fn hash_navigation_order_is_unspecified_but_every_returned_key_is_present() {
    for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(2048);
        let mut t = new_table(kind);

        const KEYS: i64 = 25;
        for k in 0..KEYS {
            t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
        }
        let present: Vec<Eterm> = (0..KEYS).map(make_small).collect();
        let is_present = |key: Eterm| present.iter().any(|&p| eq(p, key, &atoms));

        // A non-empty table has a first (present) key; an empty one does not
        // (`'$end_of_table'`) — covered by the empty-table test above.
        let first = t.first(&atoms).expect("a non-empty table has a first key");
        assert!(is_present(first), "{kind:?}: first is a stored key");

        // `first` + `next` visits every stored key exactly once.
        let mut walk = Vec::new();
        let mut cursor = Some(first);
        while let Some(key) = cursor {
            assert!(is_present(key), "{kind:?}: every walked key is stored");
            assert!(
                !walk.iter().any(|&seen| eq(seen, key, &atoms)),
                "{kind:?}: each stored key is visited exactly once"
            );
            walk.push(key);
            cursor = t.next(key, &atoms).unwrap();
        }
        assert_eq!(walk.len(), KEYS as usize, "{kind:?}: the walk covers every key");

        // `last` closes the same order; the two ends are `'$end_of_table'`.
        let last = t.last(&atoms).expect("a non-empty table has a last key");
        assert!(is_present(last), "{kind:?}: last is a stored key");
        assert!(
            eq(last, *walk.last().unwrap(), &atoms),
            "{kind:?}: last closes the walk"
        );
        assert_eq!(t.next(last, &atoms).unwrap(), None, "{kind:?}: next(last)");
        assert_eq!(t.prev(first, &atoms).unwrap(), None, "{kind:?}: prev(first)");

        // An absent anchor key is always `badarg`, whatever its value.
        for absent in [make_small(-1), make_small(KEYS), make_small(999)] {
            assert!(
                matches!(t.next(absent, &atoms), Err(BeamError::BadArg)),
                "{kind:?}: next on an absent key is badarg"
            );
            assert!(
                matches!(t.prev(absent, &atoms), Err(BeamError::BadArg)),
                "{kind:?}: prev on an absent key is badarg"
            );
        }
    }
}

#[test]
fn take_removes_and_returns_the_keys_objects_for_every_kind() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);

        // An absent key yields an empty list, not an error.
        assert!(t.take(make_small(9), &atoms).unwrap().is_empty());

        let a = object(&mut heap, 1, 10);
        t.insert(a, &atoms).unwrap();
        if kind.is_multi() {
            t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
        }

        let taken = t.take(make_small(1), &atoms).unwrap();
        let expected = if kind.is_multi() { 2 } else { 1 };
        assert_eq!(taken.len(), expected, "{kind:?} take count");
        assert!(
            taken.iter().any(|&o| eq(o, a, &atoms)),
            "{kind:?} take value"
        );
        assert!(!t.member(make_small(1), &atoms), "{kind:?} taken key gone");
        assert_eq!(t.size(), 0, "{kind:?} take empties the entry");

        // The entry really is gone: a second `take` is empty again.
        assert!(t.take(make_small(1), &atoms).unwrap().is_empty());

        // The table is still usable afterwards.
        t.insert(object(&mut heap, 2, 20), &atoms).unwrap();
        assert_eq!(t.size(), 1);
    }
}

#[test]
fn lookup_element_or_returns_the_default_only_for_an_absent_key() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = new_table(kind);

        // {1, 10, 20}
        let o = heap
            .tuple(&[make_small(1), make_small(10), make_small(20)])
            .unwrap();
        t.insert(o, &atoms).unwrap();
        let default = interned_atom(&mut atoms, "default");

        // Present key: the element, `pos` being 1-based.
        assert_eq!(
            t.lookup_element_or(make_small(1), 1, default, &atoms)
                .unwrap(),
            make_small(1)
        );
        assert_eq!(
            t.lookup_element_or(make_small(1), 2, default, &atoms)
                .unwrap(),
            make_small(10)
        );
        assert_eq!(
            t.lookup_element_or(make_small(1), 3, default, &atoms)
                .unwrap(),
            make_small(20)
        );

        // Absent key: the default, whatever `pos` (verified `erl` behaviour).
        assert_eq!(
            t.lookup_element_or(make_small(9), 2, default, &atoms)
                .unwrap(),
            default
        );
        assert_eq!(
            t.lookup_element_or(make_small(9), 5, default, &atoms)
                .unwrap(),
            default
        );

        // `pos == 0` is `badarg` before the key is even consulted …
        assert!(matches!(
            t.lookup_element_or(make_small(9), 0, default, &atoms),
            Err(BeamError::BadArg)
        ));
        // … and a present key with an out-of-range `pos` stays `badarg`.
        assert!(matches!(
            t.lookup_element_or(make_small(1), 4, default, &atoms),
            Err(BeamError::BadArg)
        ));

        // The `/3` form keeps its old behaviour: `badarg` on an absent key.
        assert!(matches!(
            t.lookup_element(make_small(9), 2, &atoms),
            Err(BeamError::BadArg)
        ));
    }
}
