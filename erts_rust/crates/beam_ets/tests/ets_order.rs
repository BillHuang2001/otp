//! Key identity and ordering across number representations.
//!
//! `ordered_set` compares keys **arithmetically** (`erl_db_tree.c` `cmp_key_eq`,
//! `CMP = erts_cmp(a, b, 0, 0)`), so `1` and `1.0` are the *same* key and numbers
//! iterate in value order across integer/float representations. `set` (and `bag`)
//! keep exact (`=:=`) key equality, so `1` and `1.0` stay distinct there. Whole-
//! object identity in `delete_object/2` also stays exact (`db_eq`). Every
//! expectation below was probed against the system `erl` (OTP 29 / erts-17.1).

mod common;

use beam_ets::TableKind;
use beam_term::prelude::*;
use common::*;
use core::cmp::Ordering;

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
