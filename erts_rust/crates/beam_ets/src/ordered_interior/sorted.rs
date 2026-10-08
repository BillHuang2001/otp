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

//! A second `ordered_set` interior: a sorted `Vec` of entries.
//!
//! Selected with [`OrderedInterior::Sorted`](crate::OrderedInterior::Sorted); the
//! crate default is the B+-tree in [`super`]. It exists so the two ordered
//! interiors can be cross-checked (see `src/interior_parity.rs`) and measured
//! (see `src/interior_bench.rs`) against each other, exactly as the `Chained`
//! and `Champ` `set` interiors are.
//!
//! The representation is deliberately the simplest correct one: one
//! `(key, object)` entry per key, kept in a plain `Vec` sorted by
//! `beam_term::compare::cmp_arith` (the arithmetic key order `ordered_set` uses).
//! Membership/order/iteration are therefore:
//!
//! * **lookup** — a `binary_search_by(cmp_arith)` walk, `O(log n)`;
//! * **insert** / **delete** — the binary search plus a `Vec::insert`/`Vec::remove`
//!   shift of the tail, `O(n)`;
//! * **ordered iteration** — a straight slice walk in ascending key order,
//!   `O(n)`.
//!
//! Term order is never re-implemented: every **key** comparison goes through
//! `beam_term::compare::cmp_arith`, so `1` and `1.0` are the *same* key exactly as
//! in the B+-tree; the whole-object identity check ([`Self::delete_object`]) is the
//! sole exception and keeps the exact `beam_term::compare::eq`.
//!
//! Public for testing and reuse, but **not** the cross-crate seam: dependents go
//! through [`crate::Table`].

use beam_common::Result;
use beam_term::prelude::*;

use super::node::{search_entries, words_of, Entry};
use crate::error::badarg;
use crate::key::key_of;
use crate::table::Interior;

/// The sorted-`Vec`-backed interior for `ordered_set`.
///
/// One object per key: an insert on an equal key replaces the object,
/// `insert_new` refuses an existing key, and every read/iteration is in arithmetic
/// key order (`cmp_arith`) — the same observable rules as the B+-tree, only a
/// different shape.
pub struct SortedOrderedInterior {
    /// The entries, kept sorted by `beam_term::compare::cmp_arith` (one object per
    /// key).
    entries: Vec<Entry>,
}

impl SortedOrderedInterior {
    /// A new, empty ordered interior.
    pub fn new() -> SortedOrderedInterior {
        SortedOrderedInterior {
            entries: Vec::new(),
        }
    }

    /// The number of stored keys (one object per key).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no key is stored.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// An honest, non-negative word count of the entry vector — the interior
    /// contribution to `ets:info(Tab, memory)`.
    ///
    /// Measured at capacity, so it never under-reports what is allocated.
    pub fn memory(&self) -> usize {
        self.entries.capacity() * words_of::<Entry>()
    }

    /// The position of `key` among the sorted entries: `(pos, true)` on a hit,
    /// `(pos, false)` with the ascending insertion point on a miss. Delegates to the
    /// same search the B+-tree leaves use (arithmetic key order, `cmp_arith`).
    fn position(&self, key: Eterm, atoms: &AtomTable) -> (usize, bool) {
        search_entries(&self.entries, key, atoms)
    }

    /// The object stored under `key`, if the key is present.
    pub fn find(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        let (pos, found) = self.position(key, atoms);
        if found {
            Some(self.entries[pos].obj)
        } else {
            None
        }
    }

    /// The smallest key, or `None` when empty (`atoms` is unused, accepted for
    /// call-site symmetry with the other navigation methods).
    pub fn first(&self, _atoms: &AtomTable) -> Option<Eterm> {
        self.entries.first().map(|entry| entry.key)
    }

    /// The largest key, or `None` when empty.
    pub fn last(&self, _atoms: &AtomTable) -> Option<Eterm> {
        self.entries.last().map(|entry| entry.key)
    }

    /// [`Self::first`] under the name the range API uses.
    pub fn first_key(&self, atoms: &AtomTable) -> Option<Eterm> {
        self.first(atoms)
    }

    /// [`Self::last`] under the name the range API uses.
    pub fn last_key(&self, atoms: &AtomTable) -> Option<Eterm> {
        self.last(atoms)
    }

    /// The smallest key **strictly greater** than `key`, whether or not `key` is
    /// present ([`Self::next_from`] with `inclusive == false`).
    pub fn next(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        self.next_from(key, false, atoms)
    }

    /// The largest key **strictly less** than `key`, whether or not `key` is
    /// present ([`Self::prev_from`] with `inclusive == false`).
    pub fn prev(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        self.prev_from(key, false, atoms)
    }

    /// The smallest stored key `>= bound`, or `> bound` when `inclusive` is false.
    /// `bound` need not be a stored key: on a hit an exclusive bound skips the
    /// equal key; on a miss the insertion point already names the first greater
    /// key.
    pub fn next_from(&self, bound: Eterm, inclusive: bool, atoms: &AtomTable) -> Option<Eterm> {
        let (pos, found) = self.position(bound, atoms);
        let start = if found && !inclusive { pos + 1 } else { pos };
        self.entries.get(start).map(|entry| entry.key)
    }

    /// The largest stored key `<= bound`, or `< bound` when `inclusive` is false.
    /// The mirror of [`Self::next_from`]; `bound` need not be a stored key.
    pub fn prev_from(&self, bound: Eterm, inclusive: bool, atoms: &AtomTable) -> Option<Eterm> {
        let (pos, found) = self.position(bound, atoms);
        let at = if found && inclusive {
            Some(pos)
        } else {
            pos.checked_sub(1)
        };
        at.and_then(|i| self.entries.get(i)).map(|entry| entry.key)
    }

    /// Insert `object` under `key`, replacing the object when `key` already
    /// exists. Returns whether a *new* key was added.
    pub fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> bool {
        let (pos, found) = self.position(key, atoms);
        if found {
            // Replace *both* fields: the stored key may be arithmetically equal to
            // `key` without being the same term (`1` vs `1.0`) and the oracle makes
            // the key follow the replaced object (see the B+-tree's `insert_rec`).
            // The two keys are `cmp_arith`-equal, so the sorted order is unchanged.
            let entry = &mut self.entries[pos];
            entry.key = key;
            entry.obj = object;
            false
        } else {
            self.entries.insert(pos, Entry { key, obj: object });
            true
        }
    }

    /// Insert `object` under `key` only if `key` is absent; returns whether the
    /// key was added.
    pub fn insert_new(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> bool {
        let (pos, found) = self.position(key, atoms);
        if found {
            false
        } else {
            self.entries.insert(pos, Entry { key, obj: object });
            true
        }
    }

    /// Remove `key` (and its object); returns whether the key was present.
    pub fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> bool {
        let (pos, found) = self.position(key, atoms);
        if found {
            self.entries.remove(pos);
            true
        } else {
            false
        }
    }

    /// Remove `key`'s object only when it is term-equal to `object`; returns
    /// whether something was removed.
    pub fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> bool {
        let (pos, found) = self.position(key, atoms);
        if found && eq(self.entries[pos].obj, object, atoms) {
            self.entries.remove(pos);
            true
        } else {
            false
        }
    }
}

impl Interior for SortedOrderedInterior {
    fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        SortedOrderedInterior::insert(self, key, object, atoms);
        // A `set`-family insert always retains the object (an equal key replaces).
        Ok(true)
    }

    fn insert_new(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        Ok(SortedOrderedInterior::insert_new(self, key, object, atoms))
    }

    fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        SortedOrderedInterior::delete(self, key, atoms);
        Ok(())
    }

    fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()> {
        SortedOrderedInterior::delete_object(self, key, object, atoms);
        Ok(())
    }

    fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {
        self.find(key, atoms).into_iter().collect()
    }

    fn member(&self, key: Eterm, atoms: &AtomTable) -> bool {
        self.find(key, atoms).is_some()
    }

    fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        let object = self.find(key, atoms).ok_or_else(badarg)?;
        // `pos` is a 0-based element index; `key_of` takes a 1-based position and
        // rejects a non-tuple object or an out-of-range position with `badarg`.
        let keypos = pos.checked_add(1).ok_or_else(badarg)?;
        key_of(object, keypos)
    }

    fn size(&self) -> usize {
        self.entries.len()
    }

    fn memory(&self) -> usize {
        // The honest entry-vector count, not the `Interior` default of 0.
        SortedOrderedInterior::memory(self)
    }

    fn iter_objects(&self) -> Box<dyn Iterator<Item = Eterm> + '_> {
        Box::new(self.entries.iter().map(|entry| entry.obj))
    }

    fn keys(&self) -> Vec<Eterm> {
        // Ascending arithmetic key order is the vector order itself.
        self.entries.iter().map(|entry| entry.key).collect()
    }

    fn first_key(&self, atoms: &AtomTable) -> Option<Eterm> {
        SortedOrderedInterior::first_key(self, atoms)
    }

    fn last_key(&self, atoms: &AtomTable) -> Option<Eterm> {
        SortedOrderedInterior::last_key(self, atoms)
    }

    fn next_key(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        SortedOrderedInterior::next(self, key, atoms)
    }

    fn prev_key(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        SortedOrderedInterior::prev(self, key, atoms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::Interior;
    use beam_term::heap::Heap;

    /// A `{Key, Value}` object tuple on `heap`.
    fn object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value)]).unwrap()
    }

    /// A heap-allocated flonum term (`[HEADER_FLONUM, bits]`); `beam_term` has no
    /// `Heap::float` constructor, so the two words are leaked for the test's life.
    fn float_term(value: f64) -> Eterm {
        let words: Box<[Eterm; 2]> =
            Box::new([HEADER_FLONUM, Eterm::from_raw(value.to_bits() as usize)]);
        let leaked: &'static [Eterm; 2] = Box::leak(words);
        make_boxed(leaked.as_ptr())
    }

    #[test]
    fn an_empty_interior_has_no_extremes_or_neighbours() {
        let atoms = AtomTable::new();
        let mut t = SortedOrderedInterior::new();

        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert!(Interior::iter_objects(&t).next().is_none());
        assert_eq!(t.first(&atoms), None);
        assert_eq!(t.last(&atoms), None);
        assert_eq!(t.next(make_small(1), &atoms), None);
        assert_eq!(t.prev(make_small(1), &atoms), None);
        assert_eq!(t.next_from(make_small(1), true, &atoms), None);
        assert_eq!(t.prev_from(make_small(1), true, &atoms), None);
        assert!(t.lookup(make_small(1), &atoms).is_empty());
        assert!(!t.member(make_small(1), &atoms));

        // Deleting from an empty interior is a no-op, not an error.
        assert!(!t.delete(make_small(1), &atoms));
        assert!(!t.delete_object(make_small(1), make_small(1), &atoms));
        assert_eq!(Interior::size(&t), 0);
    }

    #[test]
    fn insert_new_replace_and_delete_keep_ascending_order() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = SortedOrderedInterior::new();

        // Insert 0..20 in reverse; the interior sorts them.
        for v in (0..20).rev() {
            let o = object(&mut heap, v, v * 10);
            assert!(t.insert_new(make_small(v), o, &atoms));
        }
        assert_eq!(t.len(), 20);

        // `insert_new` on an existing key returns `false` and changes nothing.
        assert!(!t.insert_new(make_small(7), make_small(-1), &atoms));
        let stored = object(&mut heap, 7, 70);
        assert!(eq(t.find(make_small(7), &atoms).unwrap(), stored, &atoms));

        // `insert` replaces the object on an equal key.
        let replaced = object(&mut heap, 7, 77);
        assert!(!t.insert(make_small(7), replaced, &atoms));
        assert!(eq(t.find(make_small(7), &atoms).unwrap(), replaced, &atoms));
        assert_eq!(t.len(), 20);

        // Misses and deletes.
        assert!(t.find(make_small(100), &atoms).is_none());
        assert!(t.delete(make_small(7), &atoms));
        assert!(!t.delete(make_small(7), &atoms));
        assert_eq!(t.len(), 19);

        // `delete_object` only removes when the stored object equals it.
        let o3 = object(&mut heap, 3, 30);
        assert!(!t.delete_object(make_small(3), make_small(999), &atoms));
        assert!(t.delete_object(make_small(3), o3, &atoms));
        assert_eq!(t.len(), 18);

        // The remaining objects stay in ascending arithmetic key order.
        let keys: Vec<Eterm> = Interior::keys(&t);
        assert!(!keys.is_empty());
        for pair in keys.windows(2) {
            assert_eq!(cmp_arith(pair[0], pair[1], &atoms), core::cmp::Ordering::Less);
        }
    }

    #[test]
    fn navigation_reports_neighbours_of_present_and_absent_keys() {
        let atoms = AtomTable::new();
        let mut t = SortedOrderedInterior::new();
        for v in [10i64, 20, 30] {
            t.insert(make_small(v), make_small(v), &atoms);
        }

        assert_eq!(t.first(&atoms), Some(make_small(10)));
        assert_eq!(t.last(&atoms), Some(make_small(30)));
        assert_eq!(t.next(make_small(10), &atoms), Some(make_small(20)));
        assert_eq!(t.next(make_small(30), &atoms), None);
        assert_eq!(t.prev(make_small(30), &atoms), Some(make_small(20)));
        assert_eq!(t.prev(make_small(10), &atoms), None);

        // Keys that are *not* in the table (a partial-bound search).
        assert_eq!(t.next(make_small(15), &atoms), Some(make_small(20)));
        assert_eq!(t.prev(make_small(15), &atoms), Some(make_small(10)));
        assert_eq!(t.next(make_small(5), &atoms), Some(make_small(10)));
        assert_eq!(t.prev(make_small(5), &atoms), None);
        assert_eq!(t.next(make_small(35), &atoms), None);
        assert_eq!(t.prev(make_small(35), &atoms), Some(make_small(30)));

        // Inclusive/exclusive range bounds.
        assert_eq!(t.next_from(make_small(20), true, &atoms), Some(make_small(20)));
        assert_eq!(
            t.next_from(make_small(20), false, &atoms),
            Some(make_small(30))
        );
        assert_eq!(
            t.prev_from(make_small(20), true, &atoms),
            Some(make_small(20))
        );
        assert_eq!(
            t.prev_from(make_small(20), false, &atoms),
            Some(make_small(10))
        );
        assert_eq!(t.next_from(make_small(30), false, &atoms), None);
        assert_eq!(t.prev_from(make_small(10), false, &atoms), None);
    }

    #[test]
    fn lookup_element_reads_the_object_and_rejects_out_of_range() {
        use beam_common::BeamError;

        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = SortedOrderedInterior::new();

        let k = make_small(1);
        let o = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        Interior::insert(&mut t, k, o, &atoms).unwrap();

        assert!(eq(
            Interior::lookup_element(&t, k, 0, &atoms).unwrap(),
            k,
            &atoms
        ));
        assert!(eq(
            Interior::lookup_element(&t, k, 1, &atoms).unwrap(),
            make_small(10),
            &atoms
        ));
        assert!(matches!(
            Interior::lookup_element(&t, k, 3, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            Interior::lookup_element(&t, make_small(99), 0, &atoms),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn a_same_arith_key_replace_updates_the_stored_key_term() {
        // The sorted-`Vec` mirror of the B+-tree test of the same name: an insert on
        // an arithmetically equal but *different* key term (`1` vs `1.0`) makes the
        // key follow the replaced object. Probed against the system `erl` (OTP 29 /
        // erts-17.1): `{1,a}` then `{1.0,b}` leaves `ets:first(T) =:= 1.0`, and the
        // reverse order leaves `ets:first(T) =:= 1`.
        let atoms = AtomTable::new();
        for int_first in [true, false] {
            let one = make_small(1);
            let one_float = float_term(1.0);
            let (old, want) = if int_first {
                (one, one_float)
            } else {
                (one_float, one)
            };

            let mut t = SortedOrderedInterior::new();
            assert!(t.insert(old, make_small(10), &atoms));
            assert!(!t.insert(want, make_small(20), &atoms), "an equal key replaces");
            assert_eq!(t.len(), 1);
            assert_eq!(t.first(&atoms), Some(want), "the key follows the object");
            assert_eq!(t.last(&atoms), Some(want));
            assert_eq!(Interior::keys(&t), vec![want]);
            assert_eq!(t.next(one, &atoms), None);
            assert_eq!(t.prev(one_float, &atoms), None);
            // Either spelling reads back the last-inserted object.
            assert_eq!(Interior::lookup(&t, one, &atoms), vec![make_small(20)]);
            assert_eq!(Interior::lookup(&t, one_float, &atoms), vec![make_small(20)]);
        }
    }

    #[test]
    fn memory_is_monotonic_and_at_least_one_word_per_entry() {
        let atoms = AtomTable::new();
        let mut t = SortedOrderedInterior::new();
        assert_eq!(t.memory(), 0, "an empty vector owns no storage");

        let mut prev = 0;
        for i in 0..100i64 {
            t.insert(make_small(i), make_small(i), &atoms);
            let now = t.memory();
            assert!(now >= prev, "count shrank on insert {i}");
            prev = now;
        }
        assert!(t.memory() >= t.len(), "at least one word per entry");
        assert_eq!(Interior::memory(&t), t.memory());
    }
}
