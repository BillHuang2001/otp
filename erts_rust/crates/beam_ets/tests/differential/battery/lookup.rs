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

//! The **`take` / `lookup_element`** case group: `lookup_element/3` with
//! present/absent keys and out-of-range positions, `lookup_element/4` defaults,
//! and `take/2`.

use super::*;

impl Battery {
    fn take_and_lookup_element(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10, 20]);
        table.insert(object, &self.atoms).expect("insert");

        let e1 = table
            .lookup_element(make_small(1), 2, &self.atoms)
            .expect("lookup_element");
        let e2 = table
            .lookup_element(make_small(1), 3, &self.atoms)
            .expect("lookup_element");

        let b1 = table.lookup_element(make_small(1), 4, &self.atoms);
        let b1 = cap_value(b1, &self.atoms);
        let b2 = table.lookup_element(make_small(99), 1, &self.atoms);
        let b2 = cap_value(b2, &self.atoms);
        let b3 = table.lookup_element(make_small(1), 0, &self.atoms);
        let b3 = cap_value(b3, &self.atoms);

        let default = self.atom("default");
        let d1 = table
            .lookup_element_or(make_small(99), 2, default, &self.atoms)
            .expect("lookup_element/4");
        let d2 = table
            .lookup_element_or(make_small(1), 2, default, &self.atoms)
            .expect("lookup_element/4");
        let d3 = table
            .lookup_element_or(make_small(99), 5, default, &self.atoms)
            .expect("lookup_element/4");
        let b4 = table.lookup_element_or(make_small(1), 4, default, &self.atoms);
        let b4 = cap_value(b4, &self.atoms);

        tuple_of(&[
            self.r(e1),
            self.r(e2),
            b1,
            b2,
            b3,
            self.r(d1),
            self.r(d2),
            self.r(d3),
            b4,
        ])
    }
}

/// The `take` / `lookup_element` case, in the oracle's order.
pub(super) fn lookup_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push((
        "take_and_lookup_element".into(),
        battery.take_and_lookup_element(),
    ));

    cases
}
