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

//! The **`{keypos, 2}`** case group: a non-first key position through
//! insert/lookup/replace, `update_counter` and `update_element`.

use super::*;

impl Battery {
    fn keypos2_basic(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 2);
        let a = self.atom("a");
        let b = self.atom("b");
        let first = self.tuple(&[a, make_small(1)]);
        let second = self.tuple(&[b, make_small(2)]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let before = table.lookup(make_small(1), &self.atoms);
        let z = self.atom("z");
        let replacement = self.tuple(&[z, make_small(1)]);
        table.insert(replacement, &self.atoms).expect("insert");
        let after = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            self.srt(&before),
            self.srt(&after),
            table.size().to_string(),
            bool_str(table.member(make_small(2), &self.atoms)),
            bool_str(table.member(make_small(1), &self.atoms)),
        ])
    }

    fn keypos2_update_counter(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 2);
        let tag = self.atom("tag");
        let object = self.tuple(&[tag, make_small(1), make_small(10)]);
        table.insert(object, &self.atoms).expect("insert");
        let value = table
            .update_counter(make_small(1), make_small(5), &self.atoms, &mut self.out)
            .expect("update_counter");
        let rendered = self.r(value);
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[rendered, stored])
    }

    fn keypos2_update_element(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 2);
        let tag = self.atom("tag");
        let object = self.tuple(&[tag, make_small(1), make_small(10), make_small(20)]);
        table.insert(object, &self.atoms).expect("insert");
        let updated = table
            .update_element(
                make_small(1),
                &[(3, make_small(99))],
                &self.atoms,
                &mut self.out,
            )
            .expect("update_element");
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[bool_str(updated), stored])
    }
}

/// The `{keypos, 2}` cases, in the oracle's order.
pub(super) fn keypos_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push(("keypos2_basic".into(), battery.keypos2_basic()));
    cases.push((
        "keypos2_update_counter".into(),
        battery.keypos2_update_counter(),
    ));
    cases.push((
        "keypos2_update_element".into(),
        battery.keypos2_update_element(),
    ));

    cases
}
