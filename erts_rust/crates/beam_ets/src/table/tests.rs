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
//! Unit tests for [`super`] — the `Table` handle: ids, the write ops, the
//! metadata and the access/protection handling.
//!
//! Split out of `table/mod.rs` so that file stays within the crate's ~1000-line
//! guideline (crate-layer `CONTEXT.md`, Constraints).

    use super::*;
    use beam_common::BeamError;
    use beam_term::heap::Heap;

    /// A `{Key, Value}` object (keypos 1) on `heap`.
    fn object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value)]).unwrap()
    }

    /// A `{Key, Value, Tag}` object (keypos 1) on `heap`.
    fn triple(heap: &mut Heap, key: i64, value: i64, tag: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value), make_small(tag)])
            .unwrap()
    }

    /// A fresh keypos-1 table of `kind`.
    fn table(kind: TableKind) -> Table {
        let mut opts = TableOptions::new();
        opts.set_kind(kind);
        Table::new(TableId::next(), opts).expect("a fresh table is constructible")
    }

    /// `err` is the crate's typed "unsupported operation" raise (not a `badarg`),
    /// and it names `needle`.
    fn assert_unsupported_naming(err: BeamError, needle: &str) {
        let raise = err.into_raise().expect("the unsupported marker is a raise");
        let reason = raise.reason.as_atom().expect("the reason is an atom");
        assert_eq!(raise.class, beam_common::ExceptionClass::Error);
        assert_eq!(
            reason,
            format!("unsupported_ets: {needle}"),
            "the raise names this crate's unsupported surface"
        );
    }
    /// The interior reports a rejected `bag` duplicate, so `Table::insert` reclaims
    /// the deep copy it just made: the store's live-word count does not grow.
    #[test]
    fn a_rejected_bag_duplicate_costs_no_store_words() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = table(TableKind::Bag);

        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        let live = t.store.words();
        assert!(live > 0, "the stored object owns store words");

        for _ in 0..10 {
            t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        }
        assert_eq!(
            t.store.words(),
            live,
            "a rejected duplicate's copy must be reclaimed, not orphaned"
        );
        assert_eq!(t.size(), 1);
    }

    /// `insert_new` reclaims the deep copy it made when the key already exists.
    #[test]
    fn a_refused_insert_new_costs_no_store_words() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = table(TableKind::Set);

        assert!(t.insert_new(object(&mut heap, 1, 10), &atoms).unwrap());
        let live = t.store.words();

        for _ in 0..10 {
            assert!(!t.insert_new(object(&mut heap, 1, 20), &atoms).unwrap());
        }
        assert_eq!(t.store.words(), live, "a refused insert_new leaks no copy");
        assert_eq!(t.size(), 1);
    }

    /// `take` reclaims the stored object (the store holds no live words afterwards)
    /// and hands the caller a checked-out copy that stays valid.
    #[test]
    fn take_reclaims_the_stored_object_and_returns_a_checked_out_copy() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = table(TableKind::Set);

        let source = object(&mut heap, 1, 10);
        t.insert(source, &atoms).unwrap();
        assert!(t.store.words() > 0);

        let taken = t.take(make_small(1), &atoms).unwrap();
        assert_eq!(taken.len(), 1);
        assert_eq!(
            t.store.words(),
            0,
            "the stored object is reclaimed; the returned copy is checked out"
        );
        assert_eq!(t.size(), 0);
        assert!(
            eq(taken[0], source, &atoms),
            "the returned copy is structurally the removed object"
        );
    }

    /// `insert_list` validates the whole list **before** the first store write, so
    /// an invalid element inserts none of the elements — including the valid ones
    /// that precede it (verified `erl` behaviour).
    #[test]
    fn insert_list_validates_every_object_before_inserting_any() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = table(TableKind::Set);

        let kept = object(&mut heap, 1, 10);
        t.insert(kept, &atoms).unwrap();
        let live = t.store.words();
        let good = object(&mut heap, 2, 20);

        // A non-tuple element after a valid one: nothing is inserted.
        assert!(matches!(
            t.insert_list(&[good, make_small(7)], &atoms),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.size(), 1, "only the pre-existing object survives");
        assert!(!t.member(make_small(2), &atoms));
        assert_eq!(
            t.store.words(),
            live,
            "the valid element's copy must not be left in the store"
        );

        // The invalid element may also come first.
        assert!(matches!(
            t.insert_list(&[make_small(7), good], &atoms),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.size(), 1);
        assert_eq!(t.store.words(), live);

        // A valid list still inserts everything (the failure above left no residue).
        t.insert_list(&[good, object(&mut heap, 3, 30)], &atoms).unwrap();
        assert_eq!(t.size(), 3);
    }

    /// A `{keypos,2}` table rejects a list containing an element whose arity is
    /// `< keypos` without inserting the valid elements either.
    #[test]
    fn insert_list_is_atomic_under_a_custom_keypos() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut opts = TableOptions::new();
        opts.set_keypos(2).unwrap();
        let mut t = Table::new(TableId::next(), opts).unwrap();

        let kept = heap.tuple(&[make_small(1), make_small(10)]).unwrap();
        t.insert(kept, &atoms).unwrap();
        let live = t.store.words();

        // `{b}` is a 1-tuple: arity 1 < keypos 2, so the whole list is `badarg`.
        let good = heap.tuple(&[make_small(2), make_small(20)]).unwrap();
        let short = heap.tuple(&[make_small(3)]).unwrap();
        assert!(matches!(
            t.insert_list(&[good, short], &atoms),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.size(), 1);
        assert!(t.member(make_small(10), &atoms));
        assert!(!t.member(make_small(20), &atoms));
        assert_eq!(t.store.words(), live);
    }

    /// The element projection of every object under a key, for all four kinds:
    /// the list `ets:lookup_element/3` returns for `bag`/`duplicate_bag`, and the
    /// one-element list the single-object kinds yield.
    #[test]
    fn lookup_element_all_projects_every_object_under_a_key() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);

        let mut bag = table(TableKind::Bag);
        bag.insert(triple(&mut heap, 1, 10, 100), &atoms).unwrap();
        bag.insert(triple(&mut heap, 1, 20, 200), &atoms).unwrap();
        let values = bag.lookup_element_all(make_small(1), 2, &atoms).unwrap();
        assert!(values
            .iter()
            .zip([make_small(10), make_small(20)])
            .all(|(got, want)| eq(*got, want, &atoms)));
        let tags = bag.lookup_element_all(make_small(1), 3, &atoms).unwrap();
        assert!(eq(tags[1], make_small(200), &atoms));

        // A key element repeats once per object.
        let keys = bag.lookup_element_all(make_small(1), 1, &atoms).unwrap();
        assert_eq!(keys.len(), 2);
        assert!(eq(keys[0], make_small(1), &atoms) && eq(keys[1], make_small(1), &atoms));

        // `duplicate_bag` preserves identical objects (and so identical elements).
        let mut dup = table(TableKind::DuplicateBag);
        dup.insert(object(&mut heap, 5, 1), &atoms).unwrap();
        dup.insert(object(&mut heap, 5, 1), &atoms).unwrap();
        assert_eq!(dup.lookup_element_all(make_small(5), 2, &atoms).unwrap().len(), 2);

        // The single-object kinds store at most one object per key.
        for kind in [TableKind::Set, TableKind::OrderedSet] {
            let mut t = table(kind);
            t.insert(triple(&mut heap, 9, 10, 100), &atoms).unwrap();
            let one = t.lookup_element_all(make_small(9), 2, &atoms).unwrap();
            assert_eq!(one.len(), 1, "{kind:?} holds one object per key");
            assert!(eq(one[0], t.lookup_element(make_small(9), 2, &atoms).unwrap(), &atoms));
        }
    }

    /// The element-list projection's failures mirror the oracle: an absent key, a
    /// `pos` of 0, and an object too short to have `pos` are all `badarg`.
    #[test]
    fn lookup_element_all_rejects_absent_keys_zero_and_short_objects() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut bag = table(TableKind::Bag);
        bag.insert(object(&mut heap, 1, 10), &atoms).unwrap();

        assert!(matches!(
            bag.lookup_element_all(make_small(1), 0, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            bag.lookup_element_all(make_small(99), 2, &atoms),
            Err(BeamError::BadArg)
        ));
        // `{1,10}` has arity 2, so position 3 is out of range for the whole call.
        assert!(matches!(
            bag.lookup_element_all(make_small(1), 3, &atoms),
            Err(BeamError::BadArg)
        ));

        // `/4` reports the absent key as `None` (the caller yields `default`), but
        // validates `pos` first and keeps a present key's `badarg`.
        let default = make_small(-1);
        assert_eq!(
            bag.lookup_element_all_or(make_small(99), 9, default, &atoms).unwrap(),
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
            .expect("a present key yields the elements");
        assert_eq!(present.len(), 1);
        assert!(eq(present[0], make_small(10), &atoms));
    }

    /// The scalar `lookup_element`/`lookup_element_or` are honest for the
    /// multi-object kinds: they raise the crate's typed "unsupported" (never a
    /// wrong scalar) while still answering the oracle's failures with `badarg`.
    #[test]
    fn scalar_lookup_element_is_unsupported_for_multi_kinds() {
        for kind in [TableKind::Bag, TableKind::DuplicateBag] {
            let atoms = AtomTable::new();
            let mut heap = Heap::new(64);
            let mut t = table(kind);
            t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
            t.insert(object(&mut heap, 1, 20), &atoms).unwrap();

            // A present key with an in-range position: the oracle returns a list.
            assert_unsupported_naming(
                t.lookup_element(make_small(1), 2, &atoms).unwrap_err(),
                MULTI_ELEMENT_SCALAR,
            );
            // ... and the `/4` form of the same call.
            assert_unsupported_naming(
                t.lookup_element_or(make_small(1), 2, make_small(-1), &atoms)
                    .unwrap_err(),
                MULTI_ELEMENT_SCALAR,
            );

            // The oracle's failures stay `badarg`, not "unsupported".
            assert!(matches!(
                t.lookup_element(make_small(1), 0, &atoms),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                t.lookup_element(make_small(99), 2, &atoms),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                t.lookup_element(make_small(1), 3, &atoms),
                Err(BeamError::BadArg)
            ));
            // An absent key is `default` for `/4`, whatever the kind.
            assert_eq!(
                t.lookup_element_or(make_small(99), 9, make_small(-1), &atoms)
                    .unwrap(),
                make_small(-1)
            );
            assert!(matches!(
                t.lookup_element_or(make_small(99), 0, make_small(-1), &atoms),
                Err(BeamError::BadArg)
            ));
        }
    }

    /// `set_access` is the owner-level primitive (no access check) and the current
    /// access mode — `access()` and `info().access` — follows it.
    #[test]
    fn set_access_is_the_owner_level_primitive() {
        let mut t = table(TableKind::Set);
        assert_eq!(t.access(), Access::Protected, "the default access mode");
        assert_eq!(t.info().access, Access::Protected);

        t.set_access(Access::Public);
        assert_eq!(t.access(), Access::Public);
        assert_eq!(t.info().access, Access::Public, "info reports the current mode");

        // No owner is involved: the primitive is what the runtime itself uses.
        assert_eq!(t.owner(), None);
        t.set_access(Access::Private);
        assert_eq!(t.access(), Access::Private);
        assert_eq!(t.info().access, Access::Private);
    }

    /// A **runtime** protection change must not re-normalize the creation-time
    /// fine-locking flags (`write_concurrency` / `decentralized_counters`).
    ///
    /// Oracle-verified: a `public` table created with `{write_concurrency, true}`
    /// and then moved to `private` still reports `write_concurrency => true`, while
    /// a table *created* `private` with the same option reports `false`. Modelling
    /// the change as a mutation of [`TableOptions`] would retro-flip them.
    #[test]
    fn a_runtime_protection_change_does_not_renormalize_the_fine_locking_flags() {
        // Created `public` + `{write_concurrency, true}`: the flags are on.
        let mut opts = TableOptions::new();
        opts.set_kind(TableKind::Set)
            .set_access(Access::Public)
            .set_write_concurrency(true);
        let mut t = Table::new(TableId::next(), opts).expect("a fresh table");
        assert!(t.info().write_concurrency, "public: the flag is on");

        t.set_access(Access::Private);
        assert_eq!(t.access(), Access::Private, "the access mode did change");
        assert!(
            t.info().write_concurrency,
            "the runtime move to `private` must NOT re-normalize `write_concurrency`"
        );

        // The same option on a table *created* `private` normalizes the flag off.
        let mut opts = TableOptions::new();
        opts.set_kind(TableKind::Set)
            .set_access(Access::Private)
            .set_write_concurrency(true);
        let created_private = Table::new(TableId::next(), opts).expect("a fresh table");
        assert_eq!(created_private.access(), Access::Private);
        assert!(
            !created_private.info().write_concurrency,
            "creation-time `private` forces `write_concurrency` off"
        );

        // `decentralized_counters` is derived from the same un-renormalized flag: an
        // `ordered_set` (whose default is `true`) keeps it `true` across the move.
        let mut opts = TableOptions::new();
        opts.set_kind(TableKind::OrderedSet)
            .set_access(Access::Public)
            .set_write_concurrency(true);
        let mut ordered = Table::new(TableId::next(), opts).expect("a fresh table");
        assert!(ordered.info().decentralized_counters);
        ordered.set_access(Access::Private);
        assert_eq!(ordered.access(), Access::Private);
        assert!(ordered.info().write_concurrency);
        assert!(
            ordered.info().decentralized_counters,
            "the runtime move must not drop `decentralized_counters` either"
        );
    }

    /// The atom term named `name` (interned in `atoms`).
    fn var(atoms: &AtomTable, name: &str) -> Eterm {
        make_atom(atoms.lookup(name).expect("interned by the test"))
    }

    /// A `{Key, Value}` object on `heap` (the corpus the `replace_key` rebuild
    /// reclaims).
    fn replaced_object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value)]).unwrap()
    }

    /// `ets:select_replace/2` on a `duplicate_bag` must **reclaim** the objects it
    /// replaces, so repeated replacement keeps the table's storage at a stable
    /// floor.
    ///
    /// The key's entry is rebuilt because several objects share the key and the
    /// replacement must land on the *specific* matched object. Rebuilding from
    /// `Table::take`'s checked-out copies would leave each checked-out copy's
    /// storage held for the table's lifetime (`take`'s documented `relinquish`
    /// semantics), so every round would leak a generation of the affected keys and
    /// the store would keep mapping fresh blocks that are never handed out again.
    /// `Table::replace_key` instead deep-copies the rebuilt list **first** and then
    /// frees the old objects through the ordinary removal path, so the store's
    /// free-run reuse reaches a steady state.
    ///
    /// The observable `Table::memory()` (the store's *live* word count) is flat on
    /// either implementation — the leak lived in storage the relinquished copies
    /// still hold — so the invariant is pinned where it is visible: the store's own
    /// `words`/`blocks` accounting.
    #[test]
    fn a_duplicate_bag_replacement_reclaims_the_replaced_objects() {
        let mut atoms = AtomTable::new();
        for name in ["$1", "$2"] {
            atoms.intern(name);
        }
        let mut heap = Heap::new(4096);
        let mut t = table(TableKind::DuplicateBag);

        // Three distinct keys, three objects each. Every object matches the head,
        // and the body rewrites the value while keeping the key, so every round
        // replaces all nine objects and rebuilds all three keys.
        for key in 1..=3 {
            for value in [10, 20, 30] {
                t.insert(replaced_object(&mut heap, key, value), &atoms)
                    .unwrap();
            }
        }
        assert_eq!(t.size(), 9);

        // `[{{'$1','$2'},[],[{{'$1',99}}]}]` — the `{{ … }}` construction form whose
        // key element is syntactically the head's key, which the static body check
        // (`static_keeps_key`) requires.
        let head = heap.tuple(&[var(&atoms, "$1"), var(&atoms, "$2")]).unwrap();
        let inner = heap.tuple(&[var(&atoms, "$1"), make_small(99)]).unwrap();
        let body = heap.tuple(&[inner]).unwrap();
        let body_list = heap.cons(body, NIL).unwrap();
        let clause = heap.tuple(&[head, NIL, body_list]).unwrap();
        let spec = crate::select::compile(heap.cons(clause, NIL).unwrap(), &atoms)
            .expect("the construction body keeps the key");

        // Warmup: the first round also builds the initial free runs the later rounds
        // reuse, so the floor is taken *after* it. A fresh scratch heap per call
        // mirrors the BIF layer, which evaluates each request on its own heap.
        let mut out = beam_alloc::Heap::new(4096).expect("a scratch heap is allocatable");
        assert_eq!(t.select_replace(&spec, &atoms, &mut out).unwrap(), 9);
        let floor_words = t.store.words();
        let floor_blocks = t.store.blocks();

        for round in 1..=100 {
            let mut out = beam_alloc::Heap::new(4096).expect("a scratch heap is allocatable");
            assert_eq!(
                t.select_replace(&spec, &atoms, &mut out).unwrap(),
                9,
                "round {round}: every object is replaced on every round"
            );
            assert_eq!(
                t.store.words(),
                floor_words,
                "round {round}: the live-word count drifted from the warmup floor"
            );
            assert_eq!(
                t.store.blocks(),
                floor_blocks,
                "round {round}: the store mapped a fresh block that is never reused"
            );
        }

        // Correctness is preserved: each key still holds its three replaced objects
        // and nothing else, and the table is still navigable.
        assert_eq!(t.size(), 9);
        for key in 1..=3 {
            let looked = t.lookup(make_small(key), &atoms);
            assert_eq!(looked.len(), 3, "key {key} lost objects");
            let expected = replaced_object(&mut heap, key, 99);
            for object in looked {
                assert!(
                    eq(object, expected, &atoms),
                    "key {key}: the stored object is not the replacement"
                );
            }
        }
    }