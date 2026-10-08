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

//! `ets:update_counter/3,4` and `ets:update_element/3`.
//!
//! Every expected value below was derived from the system `erl` (OTP 29 /
//! erts-17.1); the arbitrary-precision promotion of an out-of-range sum (and a
//! bignum element/operand) is asserted explicitly.

mod common;

use beam_common::BeamError;
use beam_ets::{Access, OwnerId, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;
use common::*;

/// `ets:update_counter/3` semantics, on both counter-capable kinds.
#[test]
fn update_counter_three_increments_the_element_after_the_key() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut out = rebuild_heap();
        let mut t = new_table(kind);

        // `{k, 10, 20}`: `/3` touches `keypos + 1` = element 2.
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        t.insert(int_tuple(&mut heap, &[1, 10, 20]), &atoms)
            .unwrap();
        assert_eq!(
            t.update_counter(make_small(1), make_small(5), &atoms, &mut out)
                .unwrap(),
            make_small(15),
            "{kind:?}: +5"
        );
        assert!(
            eq(
                t.lookup(make_small(1), &atoms)[0],
                int_tuple(&mut heap, &[1, 15, 20]),
                &atoms
            ),
            "{kind:?}: after +5"
        );
        assert_eq!(
            t.update_counter(make_small(1), make_small(-3), &atoms, &mut out)
                .unwrap(),
            make_small(12),
            "{kind:?}: -3"
        );
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            int_tuple(&mut heap, &[1, 12, 20]),
            &atoms
        ));

        // The `/3` form takes the full operand grammar too: a `{Pos, Incr}` tuple
        // is a scalar operation.
        assert_eq!(
            t.update_counter(
                make_small(1),
                int_tuple(&mut heap, &[3, 5]),
                &atoms,
                &mut out
            )
            .unwrap(),
            make_small(25),
            "{kind:?}: {{3,5}}"
        );
        // …and a *list* operand returns a list of the new values.
        let op_tuples = [int_tuple(&mut heap, &[2, 1]), int_tuple(&mut heap, &[3, 1])];
        let ops = proper_list(&mut heap, &op_tuples);
        let listed = t
            .update_counter(make_small(1), ops, &atoms, &mut out)
            .unwrap();
        let expected = proper_list(&mut heap, &[make_small(13), make_small(26)]);
        assert!(eq(listed, expected, &atoms), "{kind:?}: list result");
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            int_tuple(&mut heap, &[1, 13, 26]),
            &atoms
        ));
    }
}

/// The `/3` failure modes: an absent key (no default), a non-integer increment,
/// and an object whose arity has no element `keypos + 1`.
#[test]
fn update_counter_three_rejects_bad_operands_with_badarg() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = rebuild_heap();
        let mut t = new_table(kind);
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        let atom = interned_atom(&mut atoms, "x");

        // An absent key is `badarg`: `/3` has no default object.
        assert!(matches!(
            t.update_counter(make_small(2), make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        // A non-integer increment is `badarg`.
        assert!(matches!(
            t.update_counter(make_small(1), atom, &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        // A `{Pos,Incr}` tuple whose position is out of range is `badarg`.
        assert!(matches!(
            t.update_counter(
                make_small(1),
                int_tuple(&mut heap, &[3, 1]),
                &atoms,
                &mut out
            ),
            Err(BeamError::BadArg)
        ));
        // Nothing was written by any of the failures.
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            object(&mut heap, 1, 10),
            &atoms
        ));

        // An arity-1 object has no element `keypos + 1`.
        let mut t1 = new_table(kind);
        t1.insert(int_tuple(&mut heap, &[1]), &atoms).unwrap();
        assert!(matches!(
            t1.update_counter(make_small(1), make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
    }
}

/// `update_counter` is bound to `keypos + 1`, not to a fixed position: on a
/// `keypos = 2` table the counter is element 3.
#[test]
fn update_counter_three_honours_the_table_keypos() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut out = rebuild_heap();
    let mut opts = TableOptions::new();
    opts.set_keypos(2).unwrap();
    let mut t = Table::new(TableId::next(), opts).unwrap();

    t.insert(int_tuple(&mut heap, &[100, 1, 7]), &atoms)
        .unwrap();
    assert_eq!(
        t.update_counter(make_small(1), make_small(5), &atoms, &mut out)
            .unwrap(),
        make_small(12)
    );
    assert!(eq(
        t.lookup(make_small(1), &atoms)[0],
        int_tuple(&mut heap, &[100, 1, 12]),
        &atoms
    ));
}

/// `ets:update_counter/4`: the default object is used (and deposited) only for an
/// absent key, and its key element is replaced by the requested key.
#[test]
fn update_counter_four_uses_and_deposits_the_default_object() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut out = rebuild_heap();
        let mut t = new_table(kind);

        // A present key ignores the default (`erl` verified).
        t.insert(int_tuple(&mut heap, &[1, 10, 20]), &atoms)
            .unwrap();
        let default = int_tuple(&mut heap, &[99, 0, 0]);
        assert_eq!(
            t.update_counter_with(make_small(1), make_small(5), default, &atoms, &mut out)
                .unwrap(),
            make_small(15),
            "{kind:?}: present key"
        );
        assert!(
            !t.member(make_small(99), &atoms),
            "{kind:?}: default not used"
        );

        // An absent key behaves as if `{zzz,0,0}` (key element replaced) existed.
        let default = int_tuple(&mut heap, &[99, 0, 0]);
        assert_eq!(
            t.update_counter_with(make_small(2), make_small(5), default, &atoms, &mut out)
                .unwrap(),
            make_small(5),
            "{kind:?}: absent key"
        );
        assert!(
            eq(
                t.lookup(make_small(2), &atoms)[0],
                int_tuple(&mut heap, &[2, 5, 0]),
                &atoms
            ),
            "{kind:?}: deposited default"
        );

        // `{Pos, Incr}` addresses an explicit position of the stored object.
        let default = int_tuple(&mut heap, &[1, 0, 0]);
        assert_eq!(
            t.update_counter_with(
                make_small(1),
                int_tuple(&mut heap, &[3, 7]),
                default,
                &atoms,
                &mut out
            )
            .unwrap(),
            make_small(27),
            "{kind:?}: {{3,7}}"
        );
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            int_tuple(&mut heap, &[1, 15, 27]),
            &atoms
        ));
    }
}

/// A list operand returns a list of the new values, one per operation, in order —
/// including the empty list, which still deposits the default object.
#[test]
fn update_counter_four_list_operand_returns_a_list() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut out = rebuild_heap();
        let mut t = new_table(kind);
        t.insert(int_tuple(&mut heap, &[1, 10, 20]), &atoms)
            .unwrap();

        let default = int_tuple(&mut heap, &[1, 0, 0]);
        let op_tuples = [int_tuple(&mut heap, &[2, 1]), int_tuple(&mut heap, &[3, 1])];
        let ops = proper_list(&mut heap, &op_tuples);
        let listed = t
            .update_counter_with(make_small(1), ops, default, &atoms, &mut out)
            .unwrap();
        assert!(eq(
            listed,
            proper_list(&mut heap, &[make_small(11), make_small(21)]),
            &atoms
        ));
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            int_tuple(&mut heap, &[1, 11, 21]),
            &atoms
        ));

        // A one-element list is still a list result (`erl` verified).
        let default = int_tuple(&mut heap, &[1, 0, 0]);
        let single = int_tuple(&mut heap, &[2, 5]);
        let listed = t
            .update_counter_with(
                make_small(1),
                proper_list(&mut heap, &[single]),
                default,
                &atoms,
                &mut out,
            )
            .unwrap();
        assert!(eq(
            listed,
            proper_list(&mut heap, &[make_small(16)]),
            &atoms
        ));

        // An empty list on an absent key yields `[]` and still deposits the default.
        let default = int_tuple(&mut heap, &[99, 0, 0]);
        let listed = t
            .update_counter_with(make_small(7), NIL, default, &atoms, &mut out)
            .unwrap();
        assert!(eq(listed, NIL, &atoms), "{kind:?}: empty list result");
        assert!(
            eq(
                t.lookup(make_small(7), &atoms)[0],
                int_tuple(&mut heap, &[7, 0, 0]),
                &atoms
            ),
            "{kind:?}: default deposited for the empty list"
        );
    }
}

/// The `{Pos, Incr, Threshold, SetValue}` rule: a non-negative increment that
/// would push the counter **above** the threshold, or a negative one that would
/// drop it **below**, makes the counter `SetValue`.
#[test]
fn update_counter_four_applies_the_threshold_and_set_value() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut out = rebuild_heap();

        // (operation, expected counter) on a fresh `{k, 10}` object — every pair
        // is `erl`-verified.
        let cases: [([i64; 4], i64); 5] = [
            ([2, 5, 12, 100], 100), // 15 > 12  ⇒ warp to 100
            ([2, 2, 12, 100], 12),  // 12 not > 12
            ([2, -9, 3, 100], 100), // 1 < 3    ⇒ warp to 100
            ([2, -7, 3, 100], 3),   // 3 not < 3
            ([2, 0, 5, 100], 100),  // 10 > 5   ⇒ warp to 100
        ];
        for (op, expected) in cases {
            let mut t = new_table(kind);
            t.insert(int_tuple(&mut heap, &[1, 10]), &atoms).unwrap();
            let default = int_tuple(&mut heap, &[1, 0]);
            let got = t
                .update_counter_with(
                    make_small(1),
                    int_tuple(&mut heap, &op),
                    default,
                    &atoms,
                    &mut out,
                )
                .unwrap();
            assert_eq!(got, make_small(expected), "{kind:?}: op {op:?}");
            assert!(
                eq(
                    t.lookup(make_small(1), &atoms)[0],
                    int_tuple(&mut heap, &[1, expected]),
                    &atoms
                ),
                "{kind:?}: stored after op {op:?}"
            );
        }
    }
}

/// `update_counter/4` `badarg` conditions, mirroring `erl` exactly: a bad default
/// (even when the key exists), `pos == keypos`, an out-of-range `pos`, a
/// non-integer element, increment, threshold or set value, a malformed operand,
/// and a multi kind.
#[test]
fn update_counter_four_rejects_bad_arguments_with_badarg() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut out = rebuild_heap();
        let mut t = new_table(kind);
        t.insert(int_tuple(&mut heap, &[1, 10, 20]), &atoms)
            .unwrap();
        let good_default = int_tuple(&mut heap, &[1, 0, 0]);
        let atom = interned_atom(&mut atoms, "x");

        // A non-integer element is `badarg`.
        let mut t_bad_elem = new_table(kind);
        // `{2, 1}` on a tuple whose element 2 is fine; corrupt element 2 instead:
        let non_int = tuple_of(&mut heap, &[make_small(5), atom, make_small(20)]);
        t_bad_elem.insert(non_int, &atoms).unwrap();
        assert!(matches!(
            t_bad_elem.update_counter_with(
                make_small(5),
                int_tuple(&mut heap, &[2, 1]),
                int_tuple(&mut heap, &[5, 0, 0]),
                &atoms,
                &mut out
            ),
            Err(BeamError::BadArg)
        ));

        // `pos == keypos` (keypos is 1) and out-of-range positions.
        for op in [[1i64, 1], [0, 1], [4, 1]] {
            assert!(
                matches!(
                    t.update_counter_with(
                        make_small(1),
                        int_tuple(&mut heap, &op),
                        int_tuple(&mut heap, &[1, 0, 0]),
                        &atoms,
                        &mut out
                    ),
                    Err(BeamError::BadArg)
                ),
                "{kind:?}: pos {op:?}"
            );
        }

        // A non-integer increment, threshold and set value.
        let bad_ops = [
            tuple_of(&mut heap, &[make_small(2), atom]),
            tuple_of(
                &mut heap,
                &[make_small(2), make_small(1), atom, make_small(5)],
            ),
            tuple_of(
                &mut heap,
                &[make_small(2), make_small(1), make_small(5), atom],
            ),
        ];
        for op in bad_ops {
            assert!(matches!(
                t.update_counter_with(
                    make_small(1),
                    op,
                    int_tuple(&mut heap, &[1, 0, 0]),
                    &atoms,
                    &mut out
                ),
                Err(BeamError::BadArg)
            ));
        }

        // Malformed operands: a tuple of the wrong arity and an atom.
        assert!(matches!(
            t.update_counter_with(
                make_small(1),
                int_tuple(&mut heap, &[2, 1, 5]),
                good_default,
                &atoms,
                &mut out
            ),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.update_counter_with(make_small(1), atom, good_default, &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        // An improper list operand is `badarg`.
        let improper_head = int_tuple(&mut heap, &[2, 1]);
        let improper = heap.cons(improper_head, atom).expect("test heap has room");
        assert!(matches!(
            t.update_counter_with(make_small(1), improper, good_default, &atoms, &mut out),
            Err(BeamError::BadArg)
        ));

        // An invalid default fails even when the key exists (verified `erl`).
        assert!(matches!(
            t.update_counter_with(make_small(1), make_small(5), atom, &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        // …including a tuple shorter than `keypos`.
        let mut opts = TableOptions::new();
        opts.set_kind(kind);
        opts.set_keypos(2).unwrap();
        let mut kp2 = Table::new(TableId::next(), opts).unwrap();
        assert!(matches!(
            kp2.update_counter_with(
                make_small(1),
                make_small(5),
                int_tuple(&mut heap, &[1]),
                &atoms,
                &mut out
            ),
            Err(BeamError::BadArg)
        ));

        // Nothing was written by any of the failures.
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            int_tuple(&mut heap, &[1, 10, 20]),
            &atoms
        ));
        assert_eq!(t.size(), 1, "{kind:?}: no spurious deposits");
    }
}

/// The multi kinds have no counters: `erts` raises `badarg` (`table_type`).
#[test]
fn update_counter_and_element_reject_multi_kinds_with_badarg() {
    for kind in [TableKind::Bag, TableKind::DuplicateBag] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = rebuild_heap();
        let mut t = new_table(kind);
        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

        assert!(matches!(
            t.update_counter(make_small(1), make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.update_counter_with(
                make_small(1),
                make_small(1),
                object(&mut heap, 1, 0),
                &atoms,
                &mut out
            ),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.update_element(make_small(1), &[(2, make_small(9))], &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        // The kind check precedes the key lookup: even an absent key is `badarg`.
        assert!(matches!(
            t.update_element(make_small(2), &[(0, make_small(9))], &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            object(&mut heap, 1, 10),
            &atoms
        ));
    }
}

/// `ets:update_element/3` on both counter-capable kinds: it replaces elements,
/// reports existence (an absent key is `false`, never an error), and validates
/// every position before writing any.
#[test]
fn update_element_replaces_elements_and_reports_existence() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut out = rebuild_heap();
        let mut t = new_table(kind);
        t.insert(int_tuple(&mut heap, &[1, 10, 20, 30]), &atoms)
            .unwrap();

        // A single replacement.
        assert!(t
            .update_element(make_small(1), &[(2, make_small(99))], &atoms, &mut out)
            .unwrap());
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            int_tuple(&mut heap, &[1, 99, 20, 30]),
            &atoms
        ));

        // Several replacements at once; a repeated position keeps the last value.
        assert!(t
            .update_element(
                make_small(1),
                &[(3, make_small(2)), (2, make_small(1)), (2, make_small(5))],
                &atoms,
                &mut out
            )
            .unwrap());
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            int_tuple(&mut heap, &[1, 5, 2, 30]),
            &atoms
        ));

        // An empty update list on a present key is `true` and changes nothing.
        assert!(t
            .update_element(make_small(1), &[], &atoms, &mut out)
            .unwrap());
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            int_tuple(&mut heap, &[1, 5, 2, 30]),
            &atoms
        ));

        // An absent key is `false` — even for a spec that would be `badarg` on a
        // present key (`erl` answers `false` before validating the spec).
        assert!(!t
            .update_element(make_small(2), &[(9, make_small(1))], &atoms, &mut out)
            .unwrap());
        assert!(!t
            .update_element(make_small(2), &[(0, make_small(1))], &atoms, &mut out)
            .unwrap());

        // `pos == keypos`, `pos == 0` and `pos > arity` are `badarg`, and nothing
        // is applied when any entry of the list is bad.
        for updates in [
            vec![(1usize, make_small(9))],
            vec![(0usize, make_small(9))],
            vec![(5usize, make_small(9))],
            vec![(2usize, make_small(9)), (9usize, make_small(9))],
        ] {
            assert!(
                matches!(
                    t.update_element(make_small(1), &updates, &atoms, &mut out),
                    Err(BeamError::BadArg)
                ),
                "{kind:?}: {updates:?}"
            );
        }
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            int_tuple(&mut heap, &[1, 5, 2, 30]),
            &atoms
        ));
    }
}

/// `update_element/3` honours `keypos`: on a `keypos = 2` table element 2 is the
/// key and may not be written, while element 3 may.
#[test]
fn update_element_honours_the_table_keypos() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = rebuild_heap();
        let mut opts = TableOptions::new();
        opts.set_kind(kind);
        opts.set_keypos(2).unwrap();
        let mut t = Table::new(TableId::next(), opts).unwrap();
        t.insert(int_tuple(&mut heap, &[100, 1, 7]), &atoms)
            .unwrap();

        assert!(matches!(
            t.update_element(make_small(1), &[(2, make_small(9))], &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        assert!(t
            .update_element(make_small(1), &[(3, make_small(9))], &atoms, &mut out)
            .unwrap());
        assert!(eq(
            t.lookup(make_small(1), &atoms)[0],
            int_tuple(&mut heap, &[100, 1, 9]),
            &atoms
        ));
    }
}

/// A counter update leaves the 60-bit small range: real ETS (and this crate)
/// promotes the sum to a **bignum**, and a bignum element/operand is accepted.
#[test]
fn update_counter_promotes_to_bignums_and_accepts_bignum_operands() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut out = rebuild_heap();
        let mut t = new_table(kind);
        t.insert(int_tuple(&mut heap, &[1, MAX_SMALL]), &atoms)
            .unwrap();

        // `MAX_SMALL + 1` is `2^59` (verified against `erl`: it returns
        // `576460752303423488`), a one-limb bignum.
        let sum = t
            .update_counter(make_small(1), make_small(1), &atoms, &mut out)
            .unwrap();
        let two_pow_59 = big(&mut heap, false, &[1u64 << 59]);
        assert!(unsafe { is_big(sum) }, "{kind:?}: the sum is a bignum");
        assert!(eq(sum, two_pow_59, &atoms), "{kind:?}: exact value");
        let expected = tuple_of(&mut heap, &[make_small(1), two_pow_59]);
        assert!(
            eq(t.lookup(make_small(1), &atoms)[0], expected, &atoms),
            "{kind:?}: the stored object holds the bignum"
        );

        // Subtracting 1 lands back exactly on `MAX_SMALL` — a small again.
        assert_eq!(
            t.update_counter(make_small(1), make_small(-1), &atoms, &mut out)
                .unwrap(),
            make_small(MAX_SMALL),
            "{kind:?}: normalised back to a small"
        );

        // A small element plus a bignum operand: `10 + 2^60` is the one-limb
        // bignum `2^60 + 10` (`bignum_term()` is `2^60`).
        t.insert(int_tuple(&mut heap, &[2, 10]), &atoms).unwrap();
        let operand = bignum_term();
        let sum = t
            .update_counter(make_small(2), operand, &atoms, &mut out)
            .unwrap();
        let expected = big(&mut heap, false, &[(1u64 << 60) + 10]);
        assert!(eq(sum, expected, &atoms), "{kind:?}: small + bignum");

        // …and a negative bignum operand brings it back down exactly.
        let negative = big(&mut heap, true, &[1u64 << 60]);
        let sum = t
            .update_counter(make_small(2), negative, &atoms, &mut out)
            .unwrap();
        assert_eq!(sum, make_small(10), "{kind:?}: sum cancels to a small");
        assert!(eq(
            t.lookup(make_small(2), &atoms)[0],
            int_tuple(&mut heap, &[2, 10]),
            &atoms
        ));
    }
}

/// A **bignum position** is still `badarg` even though a bignum `Incr`,
/// `Threshold` and `SetValue` are accepted (`erl_db.c:1573`).
#[test]
fn update_counter_rejects_a_bignum_position_with_badarg() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = rebuild_heap();
        let mut t = new_table(kind);
        t.insert(int_tuple(&mut heap, &[1, 10, 20]), &atoms)
            .unwrap();

        let position = bignum_term();
        let operand = tuple_of(&mut heap, &[position, make_small(1)]);
        assert!(matches!(
            t.update_counter(make_small(1), operand, &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        // A bignum `Threshold`/`SetValue` is *accepted* (only the position is
        // small-only): `10 + 1 = 11 > 5` fires the warp rule and stores the bignum
        // `2^60`.
        let set_value = bignum_term();
        let operand = {
            let pos = make_small(2);
            let incr = make_small(1);
            let thr = make_small(5);
            tuple_of(&mut heap, &[pos, incr, thr, set_value])
        };
        let warped = t
            .update_counter(make_small(1), operand, &atoms, &mut out)
            .unwrap();
        assert!(eq(warped, set_value, &atoms), "{kind:?}: warped to the bignum");
    }
}

/// The enforcing `TableAccess` view applies the protection matrix to the new
/// operations.
#[test]
fn table_access_view_enforces_write_access_for_element_updates() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut out = rebuild_heap();
    let mut opts = TableOptions::new();
    opts.set_access(Access::Protected);
    let mut t = Table::new(TableId::next(), opts).unwrap();
    t.set_owner(Some(OwnerId::new(1)));
    t.insert(int_tuple(&mut heap, &[1, 10]), &atoms).unwrap();

    // A non-owner may not write.
    let mut other = t.as_requester(Some(OwnerId::new(2)));
    assert!(matches!(
        other.update_counter(make_small(1), make_small(1), &atoms, &mut out),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        other.update_element(make_small(1), &[(2, make_small(1))], &atoms, &mut out),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        other.update_counter_with(
            make_small(1),
            make_small(1),
            int_tuple(&mut heap, &[1, 0]),
            &atoms,
            &mut out
        ),
        Err(BeamError::BadArg)
    ));
    assert!(eq(
        t.lookup(make_small(1), &atoms)[0],
        int_tuple(&mut heap, &[1, 10]),
        &atoms
    ));

    // The owner may.
    let mut owner = t.as_requester(Some(OwnerId::new(1)));
    assert_eq!(
        owner
            .update_counter(make_small(1), make_small(5), &atoms, &mut out)
            .unwrap(),
        make_small(15)
    );
    assert!(owner
        .update_element(make_small(1), &[(2, make_small(7))], &atoms, &mut out)
        .unwrap());
    assert!(eq(
        t.lookup(make_small(1), &atoms)[0],
        int_tuple(&mut heap, &[1, 7]),
        &atoms
    ));
}
