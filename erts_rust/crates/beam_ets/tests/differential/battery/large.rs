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

//! The **large-table** case group: two 10 001-key tables (`set` +
//! `ordered_set`) exercising size, lookups, the ordered `next` chain and a
//! guarded `select_count`.

use super::*;

impl Battery {
    fn set_large(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        for key in 0..LARGE_N {
            let object = self.object(key, key * 2);
            table.insert(object, &self.atoms).expect("insert");
        }
        let mut looked: Vec<Eterm> = Vec::new();
        for key in [0i64, 5000, 10000, 10001] {
            looked.extend(table.lookup(make_small(key), &self.atoms));
        }
        let spec = self.count_gt_spec(10000);
        let count = table
            .select_count(&spec, &self.atoms, &mut self.out)
            .expect("select_count");
        tuple_of(&[
            table.size().to_string(),
            self.srt(&looked),
            count.to_string(),
        ])
    }

    fn ordered_large(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in 0..LARGE_N {
            let object = self.object(key, key * 2);
            table.insert(object, &self.atoms).expect("insert");
        }
        let first = table.first(&self.atoms);
        let last = table.last(&self.atoms);
        let walk_len = self.walk_next(&table, first).len();
        let mut looked: Vec<Eterm> = Vec::new();
        for key in [0i64, 5000, 10000, 10001] {
            looked.extend(table.lookup(make_small(key), &self.atoms));
        }
        let spec = self.count_gt_spec(10000);
        let count = table
            .select_count(&spec, &self.atoms, &mut self.out)
            .expect("select_count");
        let next5000 = table
            .next(make_small(5000), &self.atoms)
            .expect("ordered_set next");
        let next10000 = table
            .next(make_small(10000), &self.atoms)
            .expect("ordered_set next");
        tuple_of(&[
            table.size().to_string(),
            self.opt(first),
            self.opt(last),
            walk_len.to_string(),
            self.opt(next5000),
            self.opt(next10000),
            self.srt(&looked),
            count.to_string(),
        ])
    }
}

/// The large-table cases, in the oracle's order.
pub(super) fn large_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push(("set_large".into(), battery.set_large()));
    cases.push(("ordered_large".into(), battery.ordered_large()));

    cases
}
