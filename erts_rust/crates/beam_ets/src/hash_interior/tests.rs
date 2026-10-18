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

//! Unit tests for the chained hash interior: per-kind insert/lookup/delete
//! rules, `rehash` reachability, `size` object-vs-key counting, the
//! counter/element-update operations and the honest `memory` count.
//!
//! The bucket-array **growth/release** behaviour is pinned next to the policy it
//! exercises, in `capacity.rs`; the table-level memory trajectory is
//! `tests/ets_capacity.rs`.

use super::capacity::INITIAL_BUCKETS;
use super::*;
use beam_common::BeamError;
use beam_term::heap::Heap;

    /// A `{Key, Value}` object tuple on `heap` (the key element repeated as the
    /// tuple's first element, mirroring `keypos = 1`).
    pub(crate) fn object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value)]).unwrap()
    }

    #[test]
    fn set_insert_lookup_member_delete() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = HashInterior::new(TableKind::Set);

        let k = make_small(1);
        let o1 = object(&mut heap, 1, 10);
        assert!(!t.member(k, &atoms));
        assert!(t.lookup(k, &atoms).is_empty());

        t.insert(k, o1, &atoms).unwrap();
        assert!(t.member(k, &atoms));
        assert_eq!(t.lookup(k, &atoms), vec![o1]);
        assert_eq!(t.size(), 1);

        // Re-inserting the same key *replaces* the object (size stays 1).
        let o2 = object(&mut heap, 1, 20);
        t.insert(k, o2, &atoms).unwrap();
        assert_eq!(t.lookup(k, &atoms), vec![o2]);
        assert_eq!(t.size(), 1);

        // `insert_new` on an existing key is a no-op returning `false`.
        let o3 = object(&mut heap, 1, 30);
        assert!(!t.insert_new(k, o3, &atoms).unwrap());
        assert_eq!(t.lookup(k, &atoms), vec![o2]);
        assert_eq!(t.size(), 1);

        // `insert_new` on a fresh key inserts and returns `true`.
        let k2 = make_small(2);
        let o4 = object(&mut heap, 2, 40);
        assert!(t.insert_new(k2, o4, &atoms).unwrap());
        assert_eq!(t.lookup(k2, &atoms), vec![o4]);
        assert_eq!(t.size(), 2);

        // `delete` drops the whole entry, and is a no-op for an absent key.
        t.delete(k, &atoms).unwrap();
        assert!(!t.member(k, &atoms));
        assert_eq!(t.size(), 1);
        t.delete(make_small(99), &atoms).unwrap();
        assert_eq!(t.size(), 1);

        // `delete_object` removes only a matching object.
        t.delete_object(k2, o4, &atoms).unwrap();
        assert!(!t.member(k2, &atoms));
        assert_eq!(t.size(), 0);
        assert!(t.iter_objects().next().is_none());
    }

    #[test]
    fn rehash_keeps_every_key_lookupable() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(4096);
        let mut t = HashInterior::new(TableKind::Set);

        const N: i64 = 300;
        let keys: Vec<Eterm> = (0..N).map(make_small).collect();
        let objs: Vec<Eterm> = (0..N).map(|i| object(&mut heap, i, i * 2)).collect();

        for i in 0..N as usize {
            t.insert(keys[i], objs[i], &atoms).unwrap();
        }
        assert_eq!(t.size(), N as usize);
        // The bucket array must have grown (and rehashed) well past its start.
        assert!(t.buckets.len() > INITIAL_BUCKETS);

        for i in 0..N as usize {
            assert_eq!(t.lookup(keys[i], &atoms), vec![objs[i]], "key {i}");
            assert!(t.member(keys[i], &atoms), "key {i}");
        }
        assert_eq!(t.iter_objects().count(), N as usize);

        // Deletion still locates every key after the rehash.
        for i in 0..N as usize {
            t.delete(keys[i], &atoms).unwrap();
        }
        assert_eq!(t.size(), 0);
        assert!(!t.member(make_small(0), &atoms));
    }

    #[test]
    fn bag_rejects_identical_objects_while_duplicate_bag_allows_them() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let k = make_small(7);
        let o1 = object(&mut heap, 7, 1);
        let o2 = object(&mut heap, 7, 2);

        let mut bag = HashInterior::new(TableKind::Bag);
        bag.insert(k, o1, &atoms).unwrap();
        // An identical object is a no-op for a bag.
        bag.insert(k, o1, &atoms).unwrap();
        assert_eq!(bag.lookup(k, &atoms), vec![o1]);
        assert_eq!(bag.size(), 1);
        // A structurally different object is appended.
        bag.insert(k, o2, &atoms).unwrap();
        assert_eq!(bag.lookup(k, &atoms), vec![o1, o2]);
        assert_eq!(bag.size(), 2);
        // `delete_object` removes only the equal object.
        bag.delete_object(k, o1, &atoms).unwrap();
        assert_eq!(bag.lookup(k, &atoms), vec![o2]);
        assert_eq!(bag.size(), 1);

        let mut dup = HashInterior::new(TableKind::DuplicateBag);
        dup.insert(k, o1, &atoms).unwrap();
        // Identical objects are all kept by a duplicate_bag.
        dup.insert(k, o1, &atoms).unwrap();
        assert_eq!(dup.lookup(k, &atoms), vec![o1, o1]);
        assert_eq!(dup.size(), 2);
        // `delete_object` removes every equal object (the entry then disappears).
        dup.delete_object(k, o1, &atoms).unwrap();
        assert_eq!(dup.size(), 0);
        assert!(!dup.member(k, &atoms));
    }

    #[test]
    fn size_counts_objects_not_keys() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = HashInterior::new(TableKind::DuplicateBag);
        let k = make_small(1);
        for v in 0..5 {
            t.insert(k, object(&mut heap, 1, v), &atoms).unwrap();
        }
        assert_eq!(t.size(), 5);
        assert_eq!(t.lookup(k, &atoms).len(), 5);
        assert_eq!(t.iter_objects().count(), 5);
    }

    #[test]
    fn lookup_element_reads_the_object_and_rejects_out_of_range() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = HashInterior::new(TableKind::Set);

        let k = make_small(1);
        let o = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        t.insert(k, o, &atoms).unwrap();

        // `pos` is 0-based.
        assert_eq!(t.lookup_element(k, 0, &atoms).unwrap(), k);
        assert_eq!(t.lookup_element(k, 1, &atoms).unwrap(), make_small(10));
        assert_eq!(t.lookup_element(k, 2, &atoms).unwrap(), make_small(20));

        // Out-of-range position and absent key are both `badarg`.
        assert!(matches!(
            t.lookup_element(k, 3, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.lookup_element(k, usize::MAX, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.lookup_element(make_small(99), 0, &atoms),
            Err(BeamError::BadArg)
        ));

        // A non-tuple object has no elements.
        let k2 = make_small(2);
        t.insert(k2, make_small(5), &atoms).unwrap();
        assert!(matches!(
            t.lookup_element(k2, 0, &atoms),
            Err(BeamError::BadArg)
        ));
    }

    /// Read element `i` (0-based) of a stored `object` (a live tuple).
    fn elem(object: Eterm, i: usize) -> Eterm {
        // SAFETY: `object` is a live tuple; the callers pass in-range indices.
        unsafe { tuple_element(object, i) }
    }

    #[test]
    fn take_returns_and_removes_the_keys_objects() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let k = make_small(1);

        let mut set = HashInterior::new(TableKind::Set);
        let o = object(&mut heap, 1, 10);
        set.insert(k, o, &atoms).unwrap();
        assert_eq!(set.take(k, &atoms), vec![o]);
        assert!(!set.member(k, &atoms));
        assert_eq!(set.size(), 0);
        // A second `take` (and any absent key) yields nothing.
        assert!(set.take(k, &atoms).is_empty());
        assert!(set.take(make_small(42), &atoms).is_empty());

        // A `duplicate_bag` hands back every object, in insertion order.
        let mut dup = HashInterior::new(TableKind::DuplicateBag);
        let a = object(&mut heap, 7, 1);
        let b = object(&mut heap, 7, 2);
        dup.insert(make_small(7), a, &atoms).unwrap();
        dup.insert(make_small(7), b, &atoms).unwrap();
        assert_eq!(dup.take(make_small(7), &atoms), vec![a, b]);
        assert_eq!(dup.size(), 0);
        assert!(!dup.member(make_small(7), &atoms));
    }

    /// A bignum term of `digits` (least significant first) built on the caller's
    /// `beam_alloc::Heap` — the heap an `update_counter` bignum result lands on.
    fn big(heap: &mut beam_alloc::Heap, negative: bool, digits: &[u64]) -> Eterm {
        // SAFETY: `alloc_words` hands out exactly `big_size_words(n)` writable
        // words, which `write_big` fills completely.
        let hp = unsafe { heap.alloc_words(big_size_words(digits.len())).unwrap() };
        // SAFETY: `hp` addresses `big_size_words(n)` words and `digits` is non-empty.
        unsafe { write_big(hp, negative, digits) }
    }

    /// The `(negative, magnitude)` view of an integer term, in the same
    /// little-endian limb representation (leading zero limbs stripped) the
    /// production arithmetic uses — so a test can assert the exact value
    /// independent of its small/bignum encoding.
    fn magnitude_of(term: Eterm) -> (bool, Vec<u64>) {
        let (negative, mut digits) = if is_small(term) {
            let value = signed_value(term);
            (value < 0, vec![value.unsigned_abs()])
        } else {
            assert!(unsafe { is_big(term) }, "the test only asks for integers");
            // SAFETY: `term` is a live bignum, so `big_parts` reads its header and
            // digit words.
            let (negative, digits) = unsafe { big_parts(term) };
            (negative, digits.to_vec())
        };
        while matches!(digits.last(), Some(0)) {
            digits.pop();
        }
        (negative, digits)
    }

    #[test]
    fn update_counter_increments_decrements_and_writes_back() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(64).expect("a test heap is allocatable");
        let k = make_small(1);
        // {1, 10, 20}
        let object = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        let mut t = HashInterior::new(TableKind::Set);
        t.insert(k, object, &atoms).unwrap();

        // Increment the element at pos 2, and check it was stored back.
        assert_eq!(
            t.update_counter(k, 2, make_small(5), &atoms, &mut out)
                .unwrap(),
            make_small(15)
        );
        assert_eq!(elem(t.lookup(k, &atoms)[0], 1), make_small(15));

        // A negative increment at a different position.
        assert_eq!(
            t.update_counter(k, 3, make_small(-25), &atoms, &mut out)
                .unwrap(),
            make_small(-5)
        );
        assert_eq!(
            t.update_counter(k, 3, make_small(7), &atoms, &mut out)
                .unwrap(),
            make_small(2)
        );
        // Counter updates never change the object count (set: one object per key).
        assert_eq!(t.size(), 1);
        assert_eq!(t.lookup(k, &atoms).len(), 1);
    }

    #[test]
    fn update_counter_absent_key_and_wrong_kind_are_badarg() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(64).expect("a test heap is allocatable");
        let k = make_small(1);

        // Absent key: real ETS raises `badarg` ("no object ... and no default").
        let mut set = HashInterior::new(TableKind::Set);
        assert!(matches!(
            set.update_counter(k, 2, make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        assert_eq!(set.size(), 0, "a failed update must not create an object");

        // `bag` / `duplicate_bag` have no counters.
        for kind in [TableKind::Bag, TableKind::DuplicateBag] {
            let mut t = HashInterior::new(kind);
            t.insert(k, object(&mut heap, 1, 10), &atoms).unwrap();
            assert!(matches!(
                t.update_counter(k, 2, make_small(1), &atoms, &mut out),
                Err(BeamError::BadArg)
            ));
        }
    }

    #[test]
    fn update_counter_position_and_type_errors_are_badarg() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(64).expect("a test heap is allocatable");
        let k = make_small(1);
        // {1, 10, 20}
        let object = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        let mut t = HashInterior::new(TableKind::Set);
        t.insert(k, object, &atoms).unwrap();

        let an_atom = make_atom(Atom::from_raw(0));
        // pos below 1 and above the arity are both `badarg`.
        assert!(matches!(
            t.update_counter(k, 0, make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.update_counter(k, 4, make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        // A non-integer increment is `badarg`.
        assert!(matches!(
            t.update_counter(k, 2, an_atom, &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        // Nothing changed after the failures.
        assert_eq!(t.lookup(k, &atoms), vec![object]);

        // A non-integer *element* is `badarg` too.
        let k2 = make_small(2);
        let bad = heap.tuple(&[k2, an_atom]).unwrap();
        t.insert(k2, bad, &atoms).unwrap();
        assert!(matches!(
            t.update_counter(k2, 2, make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.lookup(k2, &atoms), vec![bad]);
    }

    #[test]
    fn update_counter_promotes_out_of_range_sums_to_bignums() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(256).expect("a test heap is allocatable");
        let mut t = HashInterior::new(TableKind::Set);

        // `MAX_SMALL + 1` leaves the 60-bit range: real ETS promotes to a bignum.
        let hi = make_small(1);
        let object = heap.tuple(&[hi, make_small(MAX_SMALL)]).unwrap();
        t.insert(hi, object, &atoms).unwrap();
        let sum = t
            .update_counter(hi, 2, make_small(1), &atoms, &mut out)
            .unwrap();
        assert!(unsafe { is_big(sum) });
        // `2^59` is one limb and does not fit a small.
        assert_eq!(magnitude_of(sum), (false, vec![1u64 << 59]));
        // The stored object now holds the bignum (the interior rebuilt it).
        assert_eq!(elem(t.lookup(hi, &atoms)[0], 1), sum);

        // Subtracting back lands exactly on `MAX_SMALL` — normalised to a small.
        let back = t
            .update_counter(hi, 2, make_small(-1), &atoms, &mut out)
            .unwrap();
        assert_eq!(back, make_small(MAX_SMALL));

        // `MIN_SMALL - 1` underflows the small range the same way.
        let lo = make_small(2);
        let object = heap.tuple(&[lo, make_small(MIN_SMALL)]).unwrap();
        t.insert(lo, object, &atoms).unwrap();
        let sum = t
            .update_counter(lo, 2, make_small(-1), &atoms, &mut out)
            .unwrap();
        assert!(unsafe { is_big(sum) });
        assert_eq!(magnitude_of(sum), (true, vec![(1u64 << 59) + 1]));

        // A sum landing exactly on the boundary is still a small.
        assert_eq!(
            t.update_counter(lo, 2, make_small(1), &atoms, &mut out)
                .unwrap(),
            make_small(MIN_SMALL)
        );
    }

    #[test]
    fn update_counter_accepts_bignum_elements_and_operands() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(256).expect("a test heap is allocatable");
        let mut t = HashInterior::new(TableKind::Set);
        let k = make_small(1);

        // Element `2^64` (two limbs `[0, 1]`), a bignum.
        let element = big(&mut out, false, &[0, 1]);
        let object = heap.tuple(&[k, element]).unwrap();
        t.insert(k, object, &atoms).unwrap();

        // bignum element + small increment.
        let sum = t
            .update_counter(k, 2, make_small(10), &atoms, &mut out)
            .unwrap();
        assert_eq!(magnitude_of(sum), (false, vec![10, 1]));

        // bignum element + bignum increment (`2^64 + 10` plus `2^64 + 5`).
        let operand = big(&mut out, false, &[5, 1]);
        let sum = t.update_counter(k, 2, operand, &atoms, &mut out).unwrap();
        assert_eq!(magnitude_of(sum), (false, vec![15, 2]));

        // A bignum that exactly cancels the element leaves the small zero.
        let operand = big(&mut out, true, &[15, 2]);
        let sum = t.update_counter(k, 2, operand, &atoms, &mut out).unwrap();
        assert_eq!(sum, make_small(0));
    }
    #[test]
    fn update_element_replaces_elements_and_reports_existence() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(64).expect("a test heap is allocatable");
        let k = make_small(1);
        // {1, 10, 20, 30}
        let object = heap
            .tuple(&[k, make_small(10), make_small(20), make_small(30)])
            .unwrap();
        let mut t = HashInterior::new(TableKind::Set);
        t.insert(k, object, &atoms).unwrap();

        // A single element update returns `true` and rewrites only that element.
        assert!(t
            .update_element(k, &[(2, make_small(99))], &mut out, &atoms)
            .unwrap());
        let stored = t.lookup(k, &atoms)[0];
        assert_eq!(elem(stored, 0), k);
        assert_eq!(elem(stored, 1), make_small(99));
        assert_eq!(elem(stored, 2), make_small(20));
        assert_eq!(elem(stored, 3), make_small(30));

        // Later entries win for a repeated position.
        assert!(t
            .update_element(k, &[(2, make_small(1)), (2, make_small(5))], &mut out, &atoms)
            .unwrap());
        assert_eq!(elem(t.lookup(k, &atoms)[0], 1), make_small(5));

        // An empty update list is a no-op that still reports the key exists.
        assert!(t.update_element(k, &[], &mut out, &atoms).unwrap());

        // An absent key reports `false` — not an error.
        assert!(!t
            .update_element(make_small(42), &[(2, make_small(9))], &mut out, &atoms)
            .unwrap());
        assert_eq!(t.size(), 1);
    }

    #[test]
    fn update_element_out_of_range_or_wrong_kind_is_badarg() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(64).expect("a test heap is allocatable");
        let k = make_small(1);
        // {1, 10, 20}
        let object = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        let mut t = HashInterior::new(TableKind::Set);
        t.insert(k, object, &atoms).unwrap();

        // pos 0 and pos > arity are `badarg`, and nothing is written.
        for bad in [0usize, 4] {
            assert!(matches!(
                t.update_element(k, &[(bad, NIL)], &mut out, &atoms),
                Err(BeamError::BadArg)
            ));
        }
        // Validation is all-or-nothing across the list.
        assert!(matches!(
            t.update_element(k, &[(2, NIL), (9, NIL)], &mut out, &atoms),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.lookup(k, &atoms), vec![object]);

        // `bag` is rejected with `badarg`.
        let mut bag = HashInterior::new(TableKind::Bag);
        bag.insert(k, object, &atoms).unwrap();
        assert!(matches!(
            bag.update_element(k, &[(2, NIL)], &mut out, &atoms),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn memory_is_a_non_negative_honest_word_count() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(4096);
        let mut t = HashInterior::new(TableKind::Set);

        // The count is a `usize` (non-negative by construction); even an empty
        // table owns its bucket array, so it is strictly positive.
        let empty = t.memory(&atoms);
        assert!(empty > 0);

        // Inserting only ever adds storage, so the count never decreases.
        let mut prev = empty;
        for i in 0..200i64 {
            t.insert(make_small(i), object(&mut heap, i, i), &atoms).unwrap();
            let now = t.memory(&atoms);
            assert!(now >= prev, "count shrank on insert {i} ({prev} -> {now})");
            prev = now;
        }
        let populated = t.memory(&atoms);
        assert!(populated >= empty);
        // Never under-reports: at least one word per stored object (keys, buckets
        // and vector overhead only add to it).
        assert!(populated >= t.size());
        assert_eq!(t.size(), 200);
    }

    #[test]
    fn insert_reports_whether_the_object_was_retained() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let k = make_small(7);

        // `set`: a fresh insert and a replace both retain the object.
        let mut set = HashInterior::new(TableKind::Set);
        assert!(set.insert(k, object(&mut heap, 7, 1), &atoms).unwrap());
        assert!(set.insert(k, object(&mut heap, 7, 2), &atoms).unwrap(), "a replace retains");

        // `bag`: an identical object is rejected (`false`), a distinct one is kept.
        let mut bag = HashInterior::new(TableKind::Bag);
        assert!(bag.insert(k, object(&mut heap, 7, 1), &atoms).unwrap());
        assert!(
            !bag.insert(k, object(&mut heap, 7, 1), &atoms).unwrap(),
            "an identical `bag` object is not retained"
        );
        assert!(bag.insert(k, object(&mut heap, 7, 2), &atoms).unwrap());
        assert_eq!(bag.size(), 2);

        // `duplicate_bag`: every object is retained, even an identical one.
        let mut dup = HashInterior::new(TableKind::DuplicateBag);
        assert!(dup.insert(k, object(&mut heap, 7, 1), &atoms).unwrap());
        assert!(dup.insert(k, object(&mut heap, 7, 1), &atoms).unwrap());
        assert_eq!(dup.size(), 2);
    }
