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

//! The **per-kind** case group: every [`TableKind`] run through insert/lookup,
//! `insert_new`, identical-object handling, `delete_object` (matching and
//! non-matching), `tab2list`+size, delete-by-key and `take` — the block the
//! oracle's `emit/2` repeats for all four kinds.

use super::*;

impl Battery {
    fn kind_insert_lookup(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for (key, value) in [(1i64, 10i64), (2, 20), (3, 30)] {
            let object = self.object(key, value);
            table.insert(object, &self.atoms).expect("insert");
        }
        let found = table.lookup(make_small(1), &self.atoms);
        let missing = table.lookup(make_small(9), &self.atoms);
        let present = table.member(make_small(2), &self.atoms);
        let absent = table.member(make_small(9), &self.atoms);
        let before = table.size();
        table.delete(make_small(2), &self.atoms).expect("delete");
        let after = table.size();
        let deleted = table.member(make_small(2), &self.atoms);
        tuple_of(&[
            self.srt(&found),
            self.srt(&missing),
            bool_str(present),
            bool_str(absent),
            before.to_string(),
            after.to_string(),
            bool_str(deleted),
        ])
    }

    fn kind_insert_new(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let first = self.object(1, 10);
        let same_key = self.object(1, 20);
        let new_key = self.object(2, 20);
        let b1 = table.insert_new(first, &self.atoms).expect("insert_new");
        let b2 = table
            .insert_new(same_key, &self.atoms)
            .expect("insert_new");
        let b3 = table.insert_new(new_key, &self.atoms).expect("insert_new");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            bool_str(b1),
            bool_str(b2),
            bool_str(b3),
            table.size().to_string(),
            self.srt(&stored),
        ])
    }

    fn kind_identical_object(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for _ in 0..2 {
            let object = self.object(1, 10);
            table.insert(object, &self.atoms).expect("insert");
        }
        let other = self.object(1, 20);
        table.insert(other, &self.atoms).expect("insert");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[table.size().to_string(), self.srt(&stored)])
    }

    fn kind_delete_object_same(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let first = self.object(1, 10);
        let second = self.object(1, 20);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let doomed = self.object(1, 20);
        table
            .delete_object(doomed, &self.atoms)
            .expect("delete_object");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            table.size().to_string(),
            bool_str(table.member(make_small(1), &self.atoms)),
            self.srt(&stored),
        ])
    }

    fn kind_delete_object_other(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let first = self.object(1, 10);
        let second = self.object(1, 20);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let other = self.object(1, 99);
        table
            .delete_object(other, &self.atoms)
            .expect("delete_object");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            table.size().to_string(),
            bool_str(table.member(make_small(1), &self.atoms)),
            self.srt(&stored),
        ])
    }

    fn kind_tab2list_count(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for key in [1i64, 2, 3, 4, 5] {
            let object = self.object(key, key * 10);
            table.insert(object, &self.atoms).expect("insert");
        }
        tuple_of(&[
            table.tab2list().len().to_string(),
            table.size().to_string(),
        ])
    }

    fn kind_delete_by_key(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let first = self.object(1, 10);
        let second = self.object(2, 20);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        table.delete(make_small(1), &self.atoms).expect("delete");
        table.delete(make_small(99), &self.atoms).expect("delete");
        tuple_of(&[
            table.size().to_string(),
            bool_str(table.member(make_small(1), &self.atoms)),
            bool_str(table.member(make_small(2), &self.atoms)),
        ])
    }

    fn kind_take(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let first = self.object(1, 10);
        table.insert(first, &self.atoms).expect("insert");
        if kind.is_multi() {
            let second = self.object(1, 20);
            table.insert(second, &self.atoms).expect("insert");
        }
        let third = self.object(2, 20);
        table.insert(third, &self.atoms).expect("insert");
        let taken = table.take(make_small(1), &self.atoms).expect("take");
        let absent = table.take(make_small(99), &self.atoms).expect("take");
        tuple_of(&[
            self.srt(&taken),
            self.srt(&absent),
            table.size().to_string(),
            bool_str(table.member(make_small(1), &self.atoms)),
        ])
    }
}

/// The per-kind cases: eight cases × four kinds, in the oracle's order.
pub(super) fn kind_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    for (kind, name) in [
        (TableKind::Set, "set"),
        (TableKind::OrderedSet, "ordered_set"),
        (TableKind::Bag, "bag"),
        (TableKind::DuplicateBag, "duplicate_bag"),
    ] {
        cases.push((
            format!("{name}_insert_lookup"),
            battery.kind_insert_lookup(kind),
        ));
        cases.push((format!("{name}_insert_new"), battery.kind_insert_new(kind)));
        cases.push((
            format!("{name}_identical_object"),
            battery.kind_identical_object(kind),
        ));
        cases.push((
            format!("{name}_delete_object_same"),
            battery.kind_delete_object_same(kind),
        ));
        cases.push((
            format!("{name}_delete_object_other"),
            battery.kind_delete_object_other(kind),
        ));
        cases.push((
            format!("{name}_tab2list_count"),
            battery.kind_tab2list_count(kind),
        ));
        cases.push((
            format!("{name}_delete_by_key"),
            battery.kind_delete_by_key(kind),
        ));
        cases.push((format!("{name}_take"), battery.kind_take(kind)));
    }

    cases
}
