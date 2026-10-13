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

//! The **`ets:slot/2`** group: the crate's `Table::slot` against the real `erl`
//! (OTP 29.1.1 / erts-17.1).
//!
//! Two shapes, because the two table families decompose a slot differently:
//!
//! * an **`ordered_set`** slot is *exact* — slot `n` is the object with the
//!   (n+1)-th smallest key, `n == size` is `'$end_of_table'`, `n > size` is
//!   `badarg`, a negative or non-integer slot is `badarg`. Every case below is
//!   therefore compared **per slot**, byte for byte.
//! * a **hash-kind** slot (`set`/`bag`/`duplicate_bag`) is a hash *bucket*
//!   (`erl_db_hash.c:1733` `db_slot_hash`), so which objects a slot holds — and
//!   how many slots there are — is the interior's own bucket allocation. The
//!   crate's interior shape is explicitly **not** pinned (`../../CONTEXT.md` §2),
//!   so these cases assert only the *contract*: the slots `0..nactive-1`
//!   **partition** every stored object (rendered as the sorted multiset, so a
//!   different bucket count cannot show up as a diff), `nactive` is
//!   `'$end_of_table'`, `nactive + 1` is `badarg`, and a negative slot is
//!   `badarg`. The partition is found by walking to the end-of-table slot, which
//!   is why the boundary itself never appears in a rendering.
//!
//! Renders through the oracle's `cap/1` (a slot read can raise), so a success is
//! `ok:<objects or $end_of_table>` and every failure is `badarg`.

use super::*;

impl Battery {
    /// The oracle's `cap(fun() -> ets:slot(T, Slot) end)`: the slot's object list
    /// (or `'$end_of_table'`) or the error class.
    fn cap_slot(&self, table: &Table, slot: i64) -> String {
        match table.slot(slot, &self.atoms) {
            Ok(Some(objects)) => format!("ok:{}", list_of(&rendered(&objects, &self.atoms))),
            Ok(None) => "ok:$end_of_table".to_string(),
            Err(error) => cap_error(&error),
        }
    }

    /// Walk slots `0, 1, …` until the end-of-table slot, returning every object
    /// seen (in walk order) and the end-of-table slot index.
    ///
    /// The oracle's `hash_slot_walk/3`. Every slot below the boundary must be
    /// readable — an error there is a bug in the interior's decomposition, so it
    /// panics rather than rendering.
    fn hash_slot_walk(&self, table: &Table) -> (Vec<Eterm>, i64) {
        let mut objects = Vec::new();
        let mut slot = 0i64;
        loop {
            match table.slot(slot, &self.atoms) {
                Ok(Some(here)) => {
                    objects.extend(here);
                    slot += 1;
                }
                Ok(None) => return (objects, slot),
                Err(error) => panic!("hash slot {slot} failed: {error:?}"),
            }
        }
    }

    /// A `{K, K * 10}` object, the shape every case below stores.
    fn slot_object(&mut self, key: i64) -> Eterm {
        self.object(key, key * 10)
    }

    // ------------------------------------------------------------- ordered_set

    /// Slots `0..size` of an `ordered_set` built by ascending inserts.
    fn slot_ordered_ascending(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [10i64, 20, 30] {
            let object = self.slot_object(key);
            table.insert(object, &self.atoms).expect("insert");
        }
        let parts: Vec<String> = (0..=3).map(|slot| self.cap_slot(&table, slot)).collect();
        tuple_of(&parts)
    }

    /// The same slots for a `descending` insert order: the slot order is the key
    /// order, not the insertion order.
    fn slot_ordered_descending(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [50i64, 40, 30, 20, 10] {
            let object = self.slot_object(key);
            table.insert(object, &self.atoms).expect("insert");
        }
        let parts: Vec<String> = (0..=5).map(|slot| self.cap_slot(&table, slot)).collect();
        tuple_of(&parts)
    }

    /// The boundary: the last real slot, the end-of-table slot, one past it, and
    /// a negative slot.
    fn slot_ordered_bounds(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [10i64, 20, 30] {
            let object = self.slot_object(key);
            table.insert(object, &self.atoms).expect("insert");
        }
        tuple_of(&[
            self.cap_slot(&table, 2),
            self.cap_slot(&table, 3),
            self.cap_slot(&table, 4),
            self.cap_slot(&table, -1),
        ])
    }

    /// A non-integer slot is `badarg` — including the `'$end_of_table'` atom,
    /// which `ets:slot/2` does **not** accept as an input.
    ///
    /// `Table::slot` takes a signed integer, so an atom/float/list slot never
    /// reaches the interior: the BIF layer's own type check answers `badarg`, and
    /// the battery mirrors that. The real slot-`0` read is included so the case is
    /// not purely a type-level assertion.
    fn slot_ordered_noninteger(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let object = self.slot_object(1);
        table.insert(object, &self.atoms).expect("insert");
        tuple_of(&[
            self.cap_slot(&table, 0),
            // `an_atom`, `1.0`, `[]` and `'$end_of_table'` — none is an integer.
            self.badarg(),
            self.badarg(),
            self.badarg(),
            self.badarg(),
        ])
    }

    /// `badarg` as the oracle renders it (the BIF layer's type check for a query
    /// argument that is not the type the crate's method takes).
    fn badarg(&self) -> String {
        cap_error(&BeamError::BadArg)
    }
    /// An empty `ordered_set` has exactly one slot boundary at `0`.
    fn slot_ordered_empty(&mut self) -> String {
        let table = self.table(TableKind::OrderedSet, 1);
        tuple_of(&[self.cap_slot(&table, 0), self.cap_slot(&table, 1)])
    }

    /// `{keypos, 2}`: the slots follow the second element.
    fn slot_ordered_keypos2(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 2);
        for (tag, key) in [(9i64, 30i64), (7, 10), (8, 20)] {
            let object = self.object(tag, key);
            table.insert(object, &self.atoms).expect("insert");
        }
        let parts: Vec<String> = (0..=3).map(|slot| self.cap_slot(&table, slot)).collect();
        tuple_of(&parts)
    }

    /// The slots renumber after a delete.
    fn slot_ordered_after_delete(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2, 3, 4, 5] {
            let object = self.slot_object(key);
            table.insert(object, &self.atoms).expect("insert");
        }
        table.delete(make_small(3), &self.atoms).expect("delete");
        let parts: Vec<String> = (0..=4).map(|slot| self.cap_slot(&table, slot)).collect();
        tuple_of(&parts)
    }

    /// Slot `n` is the (n+1)-th object of the `ets:next/2` chain — the two
    /// positional reads agree.
    fn slot_ordered_slot_order_is_next_order(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [30i64, 10, 20] {
            let object = self.slot_object(key);
            table.insert(object, &self.atoms).expect("insert");
        }
        tuple_of(&[
            self.cap_slot(&table, 0),
            self.cap_slot(&table, 1),
            self.cap_slot(&table, 2),
            cap_opt(table.next(make_small(10), &self.atoms), &self.atoms),
        ])
    }

    /// A small-but-huge slot (`2^40`) and a bignum slot (`2^70`) are both
    /// `badarg`. `Table::slot` takes an `i64`, so the battery drives the
    /// representable `2^40` into the interior and answers the bignum the way the
    /// BIF layer must (an `i64` cannot carry `2^70`; the oracle rejects it as a
    /// non-small integer).
    fn slot_ordered_big_slot(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let object = self.slot_object(1);
        table.insert(object, &self.atoms).expect("insert");
        tuple_of(&[
            self.cap_slot(&table, 1i64 << 40),
            self.badarg(), // `1 bsl 70`, beyond every integer slot
        ])
    }

    // -------------------------------------------------------------- hash kinds

    /// The hash kinds' contract: the slots partition every object, the
    /// end-of-table slot is one past the last, and one past *that* is `badarg`.
    /// The per-slot contents are deliberately not compared — the bucket count and
    /// the per-object index are an interior freedom.
    fn slot_hash_partition(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for key in [1i64, 2, 3] {
            let object = self.slot_object(key);
            table.insert(object, &self.atoms).expect("insert");
        }
        let (objects, eot) = self.hash_slot_walk(&table);
        tuple_of(&[
            self.srt(&objects),
            self.cap_slot(&table, eot),
            self.cap_slot(&table, eot + 1),
            self.cap_slot(&table, -1),
        ])
    }

    /// The partition keeps duplicates (`duplicate_bag`).
    fn slot_hash_duplicate_union(&mut self) -> String {
        let mut table = self.table(TableKind::DuplicateBag, 1);
        let (a, b) = (self.slot_object(1), self.slot_object(2));
        let a_again = self.slot_object(1);
        table.insert(a, &self.atoms).expect("insert");
        table.insert(b, &self.atoms).expect("insert");
        table.insert(a_again, &self.atoms).expect("insert");
        let (objects, eot) = self.hash_slot_walk(&table);
        tuple_of(&[self.srt(&objects), self.cap_slot(&table, eot + 1)])
    }

    /// An empty hash table still has a well-formed slot decomposition.
    fn slot_hash_empty(&mut self, kind: TableKind) -> String {
        let table = self.table(kind, 1);
        let (objects, eot) = self.hash_slot_walk(&table);
        tuple_of(&[
            self.srt(&objects),
            self.cap_slot(&table, eot),
            self.cap_slot(&table, eot + 1),
        ])
    }
}

/// The `ets:slot/2` cases, in the oracle's order.
pub(super) fn slot_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push((
        "slot_ordered_ascending".into(),
        battery.slot_ordered_ascending(),
    ));
    cases.push((
        "slot_ordered_descending".into(),
        battery.slot_ordered_descending(),
    ));
    cases.push(("slot_ordered_bounds".into(), battery.slot_ordered_bounds()));
    cases.push((
        "slot_ordered_noninteger".into(),
        battery.slot_ordered_noninteger(),
    ));
    cases.push(("slot_ordered_empty".into(), battery.slot_ordered_empty()));
    cases.push((
        "slot_ordered_keypos2".into(),
        battery.slot_ordered_keypos2(),
    ));
    cases.push((
        "slot_ordered_after_delete".into(),
        battery.slot_ordered_after_delete(),
    ));
    cases.push((
        "slot_ordered_slot_order_is_next_order".into(),
        battery.slot_ordered_slot_order_is_next_order(),
    ));
    cases.push((
        "slot_ordered_big_slot".into(),
        battery.slot_ordered_big_slot(),
    ));

    for (kind, name) in [
        (TableKind::Set, "set"),
        (TableKind::Bag, "bag"),
        (TableKind::DuplicateBag, "duplicate_bag"),
    ] {
        cases.push((
            format!("slot_{name}_partition"),
            battery.slot_hash_partition(kind),
        ));
        cases.push((format!("slot_{name}_empty"), battery.slot_hash_empty(kind)));
    }
    cases.push((
        "slot_duplicate_bag_duplicate_union".into(),
        battery.slot_hash_duplicate_union(),
    ));

    cases
}
