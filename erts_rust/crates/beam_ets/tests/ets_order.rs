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

//! Key identity, ordering and traversal ORDER across all four table kinds.
//!
//! `ordered_set` compares keys **arithmetically** (`erl_db_tree.c` `cmp_key_eq`,
//! `CMP = erts_cmp(a, b, 0, 0)`), so `1` and `1.0` are the *same* key and numbers
//! iterate in value order across integer/float representations. `set` (and `bag`)
//! keep exact (`=:=`) key equality, so `1` and `1.0` stay distinct there. Whole-
//! object identity in `delete_object/2` also stays exact (`db_eq`). Every
//! expectation below was probed against the system `erl` (OTP 29 / erts-17.1).
//!
//! The second half pins the **traversal order** of the select family
//! (`select`/`select_reverse`/`match`/`match_object`/`tab2list`): `ordered_set` is
//! the exact ascending arithmetic key order (`select_reverse` descending), while a
//! hash kind's order is unspecified — the oracle-comparable claim there is the
//! multiset, so the recorded battery sorts before rendering.

mod common;

use beam_ets::TableKind;
use beam_term::prelude::*;
use common::*;
use core::cmp::Ordering;
use std::collections::BTreeMap;
use std::time::Duration;

#[test]
fn set_keeps_integer_and_float_keys_distinct() {
    for kind in [TableKind::Set, TableKind::Bag] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = new_table(kind);

        let one = make_small(1);
        let one_float = flonum(1.0);
        let o_int = heap.tuple(&[one, make_small(10)]).unwrap();
        let o_float = heap.tuple(&[one_float, make_small(20)]).unwrap();

        t.insert(o_int, &atoms).unwrap();
        t.insert(o_float, &atoms).unwrap();

        // `1 =:= 1.0` is false, so the two keys stay separate entries.
        assert_eq!(t.size(), 2);
        assert!(t.member(one, &atoms));
        assert!(t.member(one_float, &atoms));

        let int_obj = t.lookup(one, &atoms);
        let float_obj = t.lookup(one_float, &atoms);
        assert_eq!(int_obj.len(), 1);
        assert_eq!(float_obj.len(), 1);
        assert!(eq(int_obj[0], o_int, &atoms));
        assert!(eq(float_obj[0], o_float, &atoms));
    }
}

#[test]
fn ordered_set_treats_one_and_one_point_zero_as_the_same_key() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(128);

    let one = make_small(1);
    let one_float = flonum(1.0);
    let o_int = heap.tuple(&[one, make_small(10)]).unwrap();
    let o_float = heap.tuple(&[one_float, make_small(20)]).unwrap();

    // int first, then float: one key survives; the *last* object wins.
    let mut t = new_table(TableKind::OrderedSet);
    t.insert(o_int, &atoms).unwrap();
    t.insert(o_float, &atoms).unwrap();

    assert_eq!(t.size(), 1);
    assert!(t.member(one, &atoms));
    assert!(t.member(one_float, &atoms));
    let by_int = t.lookup(one, &atoms);
    let by_float = t.lookup(one_float, &atoms);
    assert_eq!(by_int.len(), 1);
    assert_eq!(by_float.len(), 1);
    assert!(eq(by_int[0], o_float, &atoms), "the last object is stored");
    assert!(eq(by_float[0], o_float, &atoms));
    // `lookup_element` reads through the collapsed key too.
    assert!(eq(
        t.lookup_element(one, 2, &atoms).unwrap(),
        make_small(20),
        &atoms
    ));

    // Either representation removes the single key.
    t.delete(one_float, &atoms).unwrap();
    assert_eq!(t.size(), 0);

    // float first, then int: still one key, the last (int) object wins.
    let mut t = new_table(TableKind::OrderedSet);
    t.insert(o_float, &atoms).unwrap();
    t.insert(o_int, &atoms).unwrap();
    assert_eq!(t.size(), 1);
    let by_float = t.lookup(one_float, &atoms);
    assert!(eq(by_float[0], o_int, &atoms));
    let by_int = t.lookup(one, &atoms);
    assert!(eq(by_int[0], o_int, &atoms));
    t.delete(one, &atoms).unwrap();
    assert_eq!(t.size(), 0);
}#[test]
fn ordered_set_deletes_a_key_by_the_other_number_representation() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut t = new_table(TableKind::OrderedSet);

    // A float key is removed by its integer spelling ...
    let o = heap.tuple(&[flonum(3.0), make_small(7)]).unwrap();
    t.insert(o, &atoms).unwrap();
    t.delete(make_small(3), &atoms).unwrap();
    assert_eq!(t.size(), 0);

    // ... and an integer key by its float spelling.
    let o = heap.tuple(&[make_small(3), make_small(7)]).unwrap();
    t.insert(o, &atoms).unwrap();
    t.delete(flonum(3.0), &atoms).unwrap();
    assert_eq!(t.size(), 0);
}

#[test]
fn ordered_set_next_is_representation_independent() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(128);
    let mut t = new_table(TableKind::OrderedSet);
    for k in [1, 2, 3] {
        let o = heap.tuple(&[make_small(k)]).unwrap();
        t.insert(o, &atoms).unwrap();
    }

    // `2` and `2.0` name the same key, so both spellings step to the same
    // successor; an absent bound in either representation enters the run the same.
    assert_eq!(t.next(make_small(2), &atoms).unwrap(), Some(make_small(3)));
    assert_eq!(t.next(flonum(2.0), &atoms).unwrap(), Some(make_small(3)));
    assert_eq!(t.next(flonum(0.5), &atoms).unwrap(), Some(make_small(1)));
    assert_eq!(t.next(make_small(0), &atoms).unwrap(), Some(make_small(1)));
}

#[test]
fn ordered_set_treats_negative_zero_and_zero_as_the_same_key() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(128);
    let mut t = new_table(TableKind::OrderedSet);

    let pos = heap.tuple(&[flonum(0.0), make_small(1)]).unwrap();
    let neg = heap.tuple(&[flonum(-0.0), make_small(2)]).unwrap();
    t.insert(pos, &atoms).unwrap();
    t.insert(neg, &atoms).unwrap();

    // IEEE `==` makes `-0.0` and `+0.0` the same key; the last object wins.
    assert_eq!(t.size(), 1);
    assert!(t.member(flonum(-0.0), &atoms));
    assert!(t.member(flonum(0.0), &atoms));
    let found = t.lookup(flonum(0.0), &atoms);
    assert!(eq(found[0], neg, &atoms));
}

#[test]
fn ordered_set_iterates_numeric_keys_in_value_order() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(512);
    let mut t = new_table(TableKind::OrderedSet);

    // `1` then `1.0` collapse to one key whose stored object is the *last* one, so
    // the value `1.0` appears where the number `1` sorts.
    for key in [
        make_small(5),
        make_small(-3),
        make_small(0),
        flonum(1.5),
        flonum(-2.5),
        make_small(1),
        flonum(1.0),
    ] {
        let o = heap.tuple(&[key, key]).unwrap();
        t.insert(o, &atoms).unwrap();
    }
    assert_eq!(t.size(), 6);

    // The oracle's ascending numeric order: `-3 < -2.5 < 0 < 1.0 < 1.5 < 5`.
    let expected: Vec<Eterm> = [
        make_small(-3),
        flonum(-2.5),
        make_small(0),
        flonum(1.0),
        flonum(1.5),
        make_small(5),
    ]
    .iter()
    .map(|k| heap.tuple(&[*k, *k]).unwrap())
    .collect();

    let got = t.tab2list();
    assert_eq!(got.len(), expected.len());
    for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
        assert!(eq(*g, *e, &atoms), "object {i} differs");
    }
    // Consecutive objects ascend in arithmetic key order.
    for w in got.windows(2) {
        assert_eq!(cmp_arith(w[0], w[1], &atoms), Ordering::Less);
    }
}

#[test]
fn ordered_set_delete_object_compares_the_whole_object_exactly() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(128);
    let mut t = new_table(TableKind::OrderedSet);

    let b = make_atom(atoms.intern("b"));
    // The stored object is keyed by the float `1.0`.
    let stored = heap.tuple(&[flonum(1.0), b]).unwrap();
    t.insert(stored, &atoms).unwrap();
    assert_eq!(t.size(), 1);

    // `{1, b}` keys to the same entry (arithmetic key equality) but is not the
    // *same object* (`1 =:= 1.0` is false), so `delete_object/2` leaves it.
    let other = heap.tuple(&[make_small(1), b]).unwrap();
    t.delete_object(other, &atoms).unwrap();
    assert_eq!(t.size(), 1, "delete_object must compare objects exactly");

    // The exact object is removed.
    t.delete_object(stored, &atoms).unwrap();
    assert_eq!(t.size(), 0);
}

#[test]
fn ordered_set_key_term_follows_the_replaced_object() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);

    let one = make_small(1);
    let one_float = flonum(1.0);

    // Oracle probe (system `erl`, OTP 29 / erts-17.1), int first:
    //   ets:insert(T,{1,a}), ets:insert(T,{1.0,b}) =>
    //   size 1, tab2list [{1.0,b}], first 1.0, last 1.0,
    //   next(T, first) '$end_of_table'.
    // The stored *key term* is therefore the last-inserted one (the key follows
    // the replaced object), not the first-inserted `1`.
    let mut t = new_table(TableKind::OrderedSet);
    t.insert(heap.tuple(&[one, make_small(10)]).unwrap(), &atoms)
        .unwrap();
    t.insert(heap.tuple(&[one_float, make_small(20)]).unwrap(), &atoms)
        .unwrap();

    assert_eq!(t.size(), 1, "1 and 1.0 are one key");
    assert_eq!(key_kind(t.first(&atoms)), "f1", "first reports the new key term");
    assert_eq!(key_kind(t.last(&atoms)), "f1", "last reports the new key term");
    assert_eq!(
        key_kind(t.next(t.first(&atoms).unwrap(), &atoms).unwrap()),
        "end",
        "the single collapsed key has no successor"
    );
    assert_eq!(
        key_kind(t.prev(t.last(&atoms).unwrap(), &atoms).unwrap()),
        "end"
    );

    // `tab2list`/`lookup` still return the last-inserted object, under either
    // spelling of the key.
    let want = heap.tuple(&[one_float, make_small(20)]).unwrap();
    let list = t.tab2list();
    assert_eq!(list.len(), 1);
    assert!(eq(list[0], want, &atoms));
    assert!(eq(t.lookup(one, &atoms)[0], want, &atoms));
    assert!(eq(t.lookup(one_float, &atoms)[0], want, &atoms));

    // Reverse insert order: the oracle leaves `first =:= 1` / `tab2list [{1,a}]`.
    let mut t = new_table(TableKind::OrderedSet);
    t.insert(heap.tuple(&[one_float, make_small(20)]).unwrap(), &atoms)
        .unwrap();
    t.insert(heap.tuple(&[one, make_small(10)]).unwrap(), &atoms)
        .unwrap();
    assert_eq!(t.size(), 1);
    assert_eq!(key_kind(t.first(&atoms)), "i1", "the int key term survives");
    assert_eq!(key_kind(t.last(&atoms)), "i1");
    assert_eq!(
        key_kind(t.next(t.first(&atoms).unwrap(), &atoms).unwrap()),
        "end"
    );
    assert!(eq(
        t.tab2list()[0],
        heap.tuple(&[one, make_small(10)]).unwrap(),
        &atoms
    ));
}

#[test]
fn ordered_set_replaced_key_term_keeps_the_arithmetic_order() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);

    let one = make_small(1);
    let one_float = flonum(1.0);

    // Oracle probe (system `erl`, OTP 29 / erts-17.1):
    //   {1,a}, {1.0,b}, {2,c} => tab2list [{1.0,b},{2,c}], first 1.0,
    //   next(T, first) 2, last 2, prev(T, last) 1.0.
    let mut t = new_table(TableKind::OrderedSet);
    for object in [
        heap.tuple(&[one, make_small(10)]).unwrap(),
        heap.tuple(&[one_float, make_small(20)]).unwrap(),
        heap.tuple(&[make_small(2), make_small(30)]).unwrap(),
    ] {
        t.insert(object, &atoms).unwrap();
    }
    assert_eq!(t.size(), 2);

    // The collapsed key's term is the float, and it still orders *below* `2`.
    assert_eq!(key_kind(t.first(&atoms)), "f1");
    assert_eq!(key_kind(t.last(&atoms)), "i2");
    let after = t.next(t.first(&atoms).unwrap(), &atoms).unwrap();
    assert_eq!(key_kind(after), "i2", "next steps past the collapsed key");
    let before = t.prev(t.last(&atoms).unwrap(), &atoms).unwrap();
    assert_eq!(key_kind(before), "f1", "prev steps back onto the new key term");

    // The ascending arithmetic order is preserved, and `tab2list` returns the
    // last-inserted object for the collapsed key.
    let list = t.tab2list();
    assert_eq!(list.len(), 2);
    assert!(eq(
        list[0],
        heap.tuple(&[one_float, make_small(20)]).unwrap(),
        &atoms
    ));
    assert!(eq(
        list[1],
        heap.tuple(&[make_small(2), make_small(30)]).unwrap(),
        &atoms
    ));
    for window in list.windows(2) {
        assert_eq!(cmp_arith(window[0], window[1], &atoms), Ordering::Less);
    }
}

/// Render a key term returned by `first`/`last`/`next`/`prev` so an integer `1`
/// and a float `1.0` are distinguishable (`end` is `'$end_of_table'`), matching
/// the oracle probe output.
fn key_kind(term: Option<Eterm>) -> String {
    match term {
        None => "end".to_string(),
        Some(term) if is_small(term) => format!("i{}", signed_value(term)),
        // SAFETY: the terms read here are the ordered table's stored key terms,
        // which this file only ever builds as small ints or live boxed flonums.
        Some(term) => format!("f{}", f64::from_bits(unsafe { float_bits(term) })),
    }
}

#[test]
fn ordered_set_iterates_in_ascending_key_order() {
    let mut atoms = AtomTable::new();
    let a = make_atom(atoms.intern("a"));
    let mut heap = Heap::new(256);

    let small = make_small(5);
    let tuple = heap.tuple(&[make_small(1)]).unwrap();
    let list = heap.cons(make_small(1), NIL).unwrap();

    let mut t = new_table(TableKind::OrderedSet);
    // Insert out of key order; the objects are 1-tuples keyed by the term.
    for key in [list, tuple, a, small] {
        let o = heap.tuple(&[key]).unwrap();
        t.insert(o, &atoms).unwrap();
    }
    assert_eq!(t.size(), 4);

    let ordered = t.tab2list();
    assert_eq!(ordered.len(), 4);
    // Ascending arithmetic key order between consecutive objects ({k_i} < {k_i+1}).
    for window in ordered.windows(2) {
        assert_eq!(cmp_arith(window[0], window[1], &atoms), Ordering::Less);
    }
    // The concrete key order: number < atom < tuple < list.
    for (object, key) in ordered.iter().zip([small, a, tuple, list]) {
        let want = heap.tuple(&[key]).unwrap();
        assert!(
            eq(*object, want, &atoms),
            "unexpected object at this position"
        );
    }
}

// ---------------------------------------------------------------------------
// Traversal ORDER of the whole select family, for all four kinds
// ---------------------------------------------------------------------------
//
// `select`, `select_reverse`, `match`, `match_object` and `tab2list` are five
// traversals of the *same* table, so their orders are related by construction:
//
//   * `select`, `match`, `match_object` and `tab2list` all walk the interior's own
//     iteration order (`select` iterates `tab2list`, and the `match` family is
//     `select` over a body that returns the whole object / the bound variables);
//   * `select_reverse` walks the interior's **reverse-navigation** order: the exact
//     reverse of it for an `ordered_set`, and the *same* order for a hash kind
//     (`set`/`bag`/`duplicate_bag`), whose `ets:last/1` is `ets:first/1` and
//     `ets:prev/2` is `ets:next/2` (`erl_db_hash.c:853-856`).
//
// For an `ordered_set` that order is the ascending arithmetic key order — exact and
// oracle-comparable. For the hash kinds (`set`/`bag`/`duplicate_bag`) ERTS leaves
// the order unspecified, so the oracle-comparable claim is the *multiset* only: the
// recorded battery sorts a hash kind's sequences before rendering, and the live
// cross-check compares those sorted renderings against the system `erl`.

/// The bound key of a one-element `[K]` `match/2` result.
fn match_key(result: Eterm) -> i64 {
    // SAFETY: `match/2` with a `{'$1','_'}` head always yields a two-word cons cell
    // whose head is the bound key (a small integer in this battery).
    unsafe { signed_value(car(result)) }
}

/// Render a list of one-element integer lists the way `erl`'s `~p` does:
/// `[[1],[2]]` — the shape of a `match/2` result list.
fn render_int_lists(values: &[i64]) -> String {
    let joined = values
        .iter()
        .map(|value| format!("[{value}]"))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{joined}]")
}

/// The five select-family sequences over `{1,10}..{5,50}`, one set of facts per
/// kind.
///
/// An `ordered_set` keeps its exact iteration order; a hash kind's order is
/// unspecified, so its sequences are **sorted** (the oracle-comparable claim is the
/// multiset, never the internal order).
fn traversal_facts(kind: TableKind) -> BTreeMap<String, String> {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1024);
    let mut table = new_table(kind);
    for object in objects_1_to_5(&mut heap) {
        table.insert(object, &atoms).unwrap();
    }
    let name = kind.as_atom_name();
    let ordered = kind.is_ordered();

    let mut forward: Vec<(i64, i64)> = table.tab2list().iter().map(|o| pair_of(*o)).collect();

    let key_spec = key_spec(&mut heap, &mut atoms);
    let mut select_keys: Vec<i64> = table
        .select(&key_spec, &atoms, &mut out_heap())
        .unwrap()
        .iter()
        .map(|key| signed_value(*key))
        .collect();
    let mut select_reverse_keys: Vec<i64> = table
        .select_reverse(&key_spec, &atoms, &mut out_heap())
        .unwrap()
        .iter()
        .map(|key| signed_value(*key))
        .collect();

    let pattern = heap
        .tuple(&[
            interned_atom(&mut atoms, "$1"),
            interned_atom(&mut atoms, "_"),
        ])
        .unwrap();
    let mut match_keys: Vec<i64> = table
        .match_(pattern, &atoms, &mut out_heap())
        .unwrap()
        .iter()
        .map(|result| match_key(*result))
        .collect();
    let mut match_object: Vec<(i64, i64)> = table
        .match_object(pattern, &atoms, &mut out_heap())
        .unwrap()
        .iter()
        .map(|o| pair_of(*o))
        .collect();

    if !ordered {
        select_keys.sort();
        select_reverse_keys.sort();
        match_keys.sort();
        match_object.sort();
        forward.sort();
    }

    let mut facts = BTreeMap::new();
    facts.insert(format!("{name}.select"), render_ints(&select_keys));
    facts.insert(
        format!("{name}.select_reverse"),
        render_ints(&select_reverse_keys),
    );
    facts.insert(format!("{name}.match"), render_int_lists(&match_keys));
    facts.insert(format!("{name}.match_object"), render_pairs(&match_object));
    facts.insert(format!("{name}.tab2list"), render_pairs(&forward));
    facts
}

/// The traversal-order facts the battery must produce — the figures printed by the
/// system `erl` (OTP 29.1.1 / erts-17.1) for the identical script. Recording them
/// here keeps the rules pinned with no `erl` on `$PATH`; the live-oracle test below
/// re-derives every one from the real runtime.
///
/// A hash kind is recorded **sorted** (its order is unspecified); `ordered_set`
/// keeps its exact ascending / descending order.
#[rustfmt::skip]
const ORDER_EXPECTED: &[(&str, &str)] = &[
    ("set.select", "[1,2,3,4,5]"),
    ("set.select_reverse", "[1,2,3,4,5]"),
    ("set.match", "[[1],[2],[3],[4],[5]]"),
    ("set.match_object", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
    ("set.tab2list", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
    ("ordered_set.select", "[1,2,3,4,5]"),
    ("ordered_set.select_reverse", "[5,4,3,2,1]"),
    ("ordered_set.match", "[[1],[2],[3],[4],[5]]"),
    ("ordered_set.match_object", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
    ("ordered_set.tab2list", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
    ("bag.select", "[1,2,3,4,5]"),
    ("bag.select_reverse", "[1,2,3,4,5]"),
    ("bag.match", "[[1],[2],[3],[4],[5]]"),
    ("bag.match_object", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
    ("bag.tab2list", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
    ("duplicate_bag.select", "[1,2,3,4,5]"),
    ("duplicate_bag.select_reverse", "[1,2,3,4,5]"),
    ("duplicate_bag.match", "[[1],[2],[3],[4],[5]]"),
    ("duplicate_bag.match_object", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
    ("duplicate_bag.tab2list", "[{1,10},{2,20},{3,30},{4,40},{5,50}]"),
];

/// The four kinds, in the battery's order.
const KINDS: [TableKind; 4] = [
    TableKind::Set,
    TableKind::OrderedSet,
    TableKind::Bag,
    TableKind::DuplicateBag,
];

/// Every fact of the traversal-order battery, keyed by `"<kind>.<api>"`.
fn all_traversal_facts() -> BTreeMap<String, String> {
    let mut facts = BTreeMap::new();
    for kind in KINDS {
        facts.extend(traversal_facts(kind));
    }
    facts
}

/// The crate's traversal-order rules, pinned to the recorded oracle figures (no
/// oracle needed): `select`/`match`/`match_object`/`tab2list` agree on the table's
/// iteration order, `select_reverse` is its exact reverse, and an `ordered_set` is
/// the ascending key order.
#[test]
fn traverse_order_matches_the_recorded_oracle_figures() {
    let facts = all_traversal_facts();
    for (key, want) in ORDER_EXPECTED {
        assert_eq!(facts.get(*key).map(String::as_str), Some(*want), "fact `{key}`");
    }
    assert_eq!(
        facts.len(),
        ORDER_EXPECTED.len(),
        "the battery produced a different number of facts than recorded"
    );
}

/// The invariant behind the recorded figures, asserted directly on the sequences
/// (not their renderings): for **every** kind the four forward traversals visit the
/// objects in exactly `tab2list` order, and `select_reverse` visits them in the
/// interior's reverse-navigation order — the exact reverse of `tab2list` for an
/// `ordered_set` (the ascending numeric key order), and the **same** order for a
/// hash kind (`set`/`bag`/`duplicate_bag`, whose `prev` is `next`).
#[test]
fn select_family_visits_objects_in_tab2list_order_for_every_kind() {
    for kind in KINDS {
        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(1024);
        let mut table = new_table(kind);
        for object in objects_1_to_5(&mut heap) {
            table.insert(object, &atoms).unwrap();
        }

        let forward = table.tab2list();
        let forward_keys: Vec<i64> = forward.iter().map(|o| pair_of(*o).0).collect();

        // `select` with a whole-object body (`['$_']`) is `tab2list`, in order.
        let whole = whole_object_spec(&mut heap, &mut atoms);
        let selected = table.select(&whole, &atoms, &mut out_heap()).unwrap();
        assert_eq!(selected.len(), forward.len(), "{kind:?}: select count");
        for (got, want) in selected.iter().zip(&forward) {
            assert!(
                eq(*got, *want, &atoms),
                "{kind:?}: select order differs from tab2list"
            );
        }

        // `select` with a key body returns exactly the `tab2list` keys, in order.
        let key_spec = key_spec(&mut heap, &mut atoms);
        let keys: Vec<i64> = table
            .select(&key_spec, &atoms, &mut out_heap())
            .unwrap()
            .iter()
            .map(|key| signed_value(*key))
            .collect();
        assert_eq!(keys, forward_keys, "{kind:?}: select keys vs tab2list keys");

        // The `match` family walks the same order: `match_object` yields whole
        // objects, `match` yields one-element lists of the same keys.
        let pattern = heap
            .tuple(&[
                interned_atom(&mut atoms, "$1"),
                interned_atom(&mut atoms, "_"),
            ])
            .unwrap();
        let matched = table.match_object(pattern, &atoms, &mut out_heap()).unwrap();
        assert_eq!(matched.len(), forward.len(), "{kind:?}: match_object count");
        for (got, want) in matched.iter().zip(&forward) {
            assert!(
                eq(*got, *want, &atoms),
                "{kind:?}: match_object order differs from tab2list"
            );
        }
        let matched_keys: Vec<i64> = table
            .match_(pattern, &atoms, &mut out_heap())
            .unwrap()
            .iter()
            .map(|result| match_key(*result))
            .collect();
        assert_eq!(
            matched_keys, forward_keys,
            "{kind:?}: match order differs from tab2list"
        );

        // `select_reverse` visits the objects in the interior's reverse-navigation
        // order: the exact reverse of `tab2list` for an `ordered_set`, and the same
        // order for a hash kind (whose `last`/`prev` are `first`/`next` — the
        // hash-table function table in `erl_db_hash.c:853-856`).
        let reversed = table
            .select_reverse(&whole, &atoms, &mut out_heap())
            .unwrap();
        assert_eq!(reversed.len(), forward.len(), "{kind:?}: reverse count");
        let expected_reverse: Vec<Eterm> = if kind.is_ordered() {
            forward.iter().rev().copied().collect()
        } else {
            forward.clone()
        };
        for (got, want) in reversed.iter().zip(&expected_reverse) {
            assert!(
                eq(*got, *want, &atoms),
                "{kind:?}: select_reverse is not the interior's reverse order"
            );
        }

        // An `ordered_set` is the exact ascending key order.
        if kind.is_ordered() {
            assert_eq!(forward_keys, vec![1, 2, 3, 4, 5], "{kind:?}: ascending order");
        }
    }
}

/// The oracle program of the traversal-order battery (see [`ORDER_EXPECTED`]): it
/// builds one table per kind over `{1,10}..{5,50}` and prints the five select-family
/// sequences, **sorting** a hash kind's output (`norm/2`) so only the unspecified
/// part is normalised away.
const ORDER_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~p~n", [K, V]) end,
    Objs = [{N, N * 10} || N <- lists:seq(1, 5)],
    S = [{{'$1','_'}, [], ['$1']}],
    Pat = {'$1','_'},
    F = fun(Kind) ->
        KS = atom_to_list(Kind),
        T = ets:new(order, [Kind]),
        [ets:insert(T, O) || O <- Objs],
        P(KS ++ ".select", norm(Kind, ets:select(T, S))),
        P(KS ++ ".select_reverse", norm(Kind, ets:select_reverse(T, S))),
        P(KS ++ ".match", lists:sort(ets:match(T, Pat))),
        P(KS ++ ".match_object", norm(Kind, ets:match_object(T, Pat))),
        P(KS ++ ".tab2list", norm(Kind, ets:tab2list(T))),
        ets:delete(T)
    end,
    [F(K) || K <- [set, ordered_set, bag, duplicate_bag]],
    ok.

norm(ordered_set, L) -> L;
norm(_, L) -> lists:sort(L).
"#;

/// The live cross-check of the traversal order: the system `erl` runs the identical
/// battery and both sides must reproduce every recorded figure. For an `ordered_set`
/// the comparison is exact (ascending / descending); for the hash kinds it is the
/// multiset the recorded battery pins.
///
/// The oracle is **required by default**, so a missing Erlang fails this check
/// rather than skipping it (`BEAM_ETS_ALLOW_NO_ORACLE=1` opts out;
/// `BEAM_ETS_REQUIRE_ORACLE=1` forces it).
#[test]
fn traverse_order_agrees_with_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "traverse_order_agrees_with_the_live_oracle",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable escript on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        ORDER_ORACLE_PROGRAM,
        &[],
        "order_traversal",
        Duration::from_secs(60),
    );
    let live = parse_lines(&String::from_utf8_lossy(&output.stdout));
    let facts = all_traversal_facts();

    for (key, want) in ORDER_EXPECTED {
        assert_eq!(
            live.get(*key).map(String::as_str),
            Some(*want),
            "the oracle disagrees with the recorded figure for `{key}`"
        );
        assert_eq!(
            facts.get(*key).map(String::as_str),
            Some(*want),
            "the crate disagrees with the recorded figure for `{key}`"
        );
    }
    assert_eq!(
        live.len(),
        ORDER_EXPECTED.len(),
        "the oracle printed a different number of facts than recorded:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    println!(
        "[ets] traversal order: {} facts agree with the live oracle",
        ORDER_EXPECTED.len()
    );
}