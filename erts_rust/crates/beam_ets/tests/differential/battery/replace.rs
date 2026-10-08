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

//! The **`ets:select_replace/2`** case group: a small-integer rewrite (a `{{…}}`
//! construction body with an arithmetic operator), an arity-1 result body, the
//! `bag` refusal (`badarg`), a key-changing body (`badarg`, table unchanged) and
//! the identity `'$_'` body.
//!
//! Every case renders the call through [`cap_count`] (the oracle's `cap/1`, so a
//! successful replacement is `ok:<count>`) followed by the **resulting**
//! `tab2list` (sorted — the tables are `set`s, whose iteration order is not
//! contract). All cases agree with the system `erl` (OTP 29.1.1 / erts-17.1).

use super::*;

impl Battery {
    /// The shared shape: run `ets:select_replace/2` with the compiled `spec_term`
    /// and render `{cap(count), srt(tab2list)}`.
    fn replace(&mut self, table: &mut Table, spec_term: Eterm) -> String {
        let spec = compile(spec_term, &self.atoms).expect("the spec compiles");
        let count = cap_count(table.select_replace(&spec, &self.atoms, &mut self.out));
        let objects = table.tab2list();
        tuple_of(&[count, self.srt(&objects)])
    }

    /// A `set` of `{1,10}`/`{2,20}`/`{3,30}`.
    fn three_object_set(&mut self) -> Table {
        let mut table = self.table(TableKind::Set, 1);
        for key in [1i64, 2, 3] {
            table
                .insert(self.object(key, key * 10), &self.atoms)
                .expect("insert");
        }
        table
    }

    /// A guarded small-integer rewrite: `{'$2' > 10}` rewrites `{'$1','$2'}` to
    /// `{'$1','$2'*2}`, replacing two of the three objects.
    fn select_replace_small_int(&mut self) -> String {
        let mut table = self.three_object_set();
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let gt = self.atom(">");
        let guard = self.tuple(&[gt, v2, make_small(10)]);
        let guards = self.list(&[guard]);
        let star = self.atom("*");
        let doubled = self.tuple(&[star, v2, make_small(2)]);
        let inner = self.tuple(&[v1, doubled]);
        // The `{{ … }}` tuple-construction form.
        let construction = self.tuple(&[inner]);
        let body = self.list(&[construction]);
        let spec_term = self.spec(head, guards, body);
        self.replace(&mut table, spec_term)
    }

    /// An arity-1 result body `{{'$1'}}`: every `{'$1','$2'}` becomes `{'$1'}`,
    /// keeping the key at keypos 1.
    fn select_replace_arity1(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let a = self.atom("a");
        let b = self.atom("b");
        table
            .insert(self.tuple(&[make_small(1), a]), &self.atoms)
            .expect("insert");
        table
            .insert(self.tuple(&[make_small(2), b]), &self.atoms)
            .expect("insert");
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let inner = self.tuple(&[v1]);
        let construction = self.tuple(&[inner]);
        let body = self.list(&[construction]);
        let spec_term = self.spec(head, NIL, body);
        self.replace(&mut table, spec_term)
    }

    /// `ets:select_replace/2` refuses the `bag` kind up front — the whole call is
    /// `badarg` even though the spec matches every object and the table is
    /// unchanged.
    fn select_replace_bag_badarg(&mut self) -> String {
        let mut table = self.table(TableKind::Bag, 1);
        table.insert(self.object(1, 10), &self.atoms).expect("insert");
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let inner = self.tuple(&[v1, v2]);
        let construction = self.tuple(&[inner]);
        let body = self.list(&[construction]);
        let spec_term = self.spec(head, NIL, body);
        self.replace(&mut table, spec_term)
    }

    /// A body that changes the key is `badarg` and leaves the table unchanged.
    fn select_replace_changed_key_badarg(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        table.insert(self.object(1, 10), &self.atoms).expect("insert");
        table.insert(self.object(2, 20), &self.atoms).expect("insert");
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        // Every replacement gets key `99`, so it does not keep the matched key.
        let inner = self.tuple(&[make_small(99), v2]);
        let construction = self.tuple(&[inner]);
        let body = self.list(&[construction]);
        let spec_term = self.spec(head, NIL, body);
        self.replace(&mut table, spec_term)
    }

    /// The identity body `['$_']` counts every object as replaced but leaves the
    /// table unchanged.
    fn select_replace_identity(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        table.insert(self.object(1, 10), &self.atoms).expect("insert");
        table.insert(self.object(2, 20), &self.atoms).expect("insert");
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let whole = self.atom("$_");
        let body = self.list(&[whole]);
        let spec_term = self.spec(head, NIL, body);
        self.replace(&mut table, spec_term)
    }
}

/// The `ets:select_replace/2` cases, in the oracle's order.
pub(super) fn replace_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push((
        "select_replace_small_int".into(),
        battery.select_replace_small_int(),
    ));
    cases.push((
        "select_replace_arity1".into(),
        battery.select_replace_arity1(),
    ));
    cases.push((
        "select_replace_bag_badarg".into(),
        battery.select_replace_bag_badarg(),
    ));
    cases.push((
        "select_replace_changed_key_badarg".into(),
        battery.select_replace_changed_key_badarg(),
    ));
    cases.push((
        "select_replace_identity".into(),
        battery.select_replace_identity(),
    ));

    cases
}
