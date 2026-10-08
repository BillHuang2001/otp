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

//! The **reclamation-observable** case group: repeated insert/replace/delete
//! churn and a `delete_all_objects/1` must leave only an *observable* trace —
//! `size`, `member/2`, `lookup/2` and `tab2list/1`.
//!
//! `memory` and the table `id` are implementation-specific (an allocation figure
//! and a table handle), so they are **never** rendered: the case pins only what
//! the `ets` contract fixes, which is exactly the point — the crate's `TermStore`
//! reuses freed runs, and that must not be observable.

use super::*;

impl Battery {
    fn reclamation_observable(&mut self) -> String {
        // Five rounds of `insert` 1..10 then `delete` the odd keys. Each round
        // ends on the same set of even keys, so the reclamation (freed-run reuse)
        // in between is invisible: the final state must be exactly the evens.
        let mut table = self.table(TableKind::Set, 1);
        for _ in 0..5 {
            for key in 1..=10i64 {
                table
                    .insert(self.object(key, key), &self.atoms)
                    .expect("insert");
            }
            for key in (1..=10i64).step_by(2) {
                table.delete(make_small(key), &self.atoms).expect("delete");
            }
        }

        let s1 = table.size().to_string();
        let members: Vec<String> = [1i64, 2, 10, 11]
            .iter()
            .map(|key| bool_str(table.member(make_small(*key), &self.atoms)))
            .collect();
        let member_line = list_of(&members);
        let lookup_4 = self.srt(&table.lookup(make_small(4), &self.atoms));

        // Cleared: no membership, no objects, zero size.
        table.delete_all_objects();
        let s2 = table.size().to_string();
        let empty = self.srt(&table.tab2list());

        // Reusable after a clear.
        table
            .insert(self.object(7, 70), &self.atoms)
            .expect("insert");
        let s3 = table.size().to_string();
        let lookup_7 = self.srt(&table.lookup(make_small(7), &self.atoms));

        tuple_of(&[s1, member_line, lookup_4, s2, empty, s3, lookup_7])
    }
}

/// The reclamation-observable case, in the oracle's order.
pub(super) fn reclamation_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push((
        "reclamation_observable".into(),
        battery.reclamation_observable(),
    ));

    cases
}
