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

//! The **limit + continuation** case group: `ets:select/3`+`select/1`,
//! `ets:match/3`+`match/1` and `ets:match_object/3`+`match_object/1` chained to
//! completion, plus the degenerate shapes (`limit 0` ⇒ `badarg`, an empty table's
//! `/3` ⇒ the bare atom `'$end_of_table'`).
//!
//! The oracle's continuation is an **opaque ref** and is therefore NEVER rendered:
//! each case chains the `/3` entry point and its `/1` resumptions to exhaustion and
//! renders only the concatenated, materialized results — in the table's iteration
//! order (an `ordered_set`, so `inorder` applies on both sides). Every case agrees
//! with the system `erl` (OTP 29.1.1 / erts-17.1).

use super::*;
use beam_ets::MatchContinuation;

/// A `/1` resumption step matching one originating `/3` form
/// (`Table::select_continue` / `match_continue` / `match_object_continue`).
type ResumeStep = fn(
    &Table,
    &MatchContinuation,
    &AtomTable,
    &mut OutHeap,
) -> BeamResult<(Vec<Eterm>, Option<MatchContinuation>)>;

impl Battery {
    /// A 5-object `ordered_set` `{1,x}`..`{5,x}` (deterministic iteration order).
    fn chain_table(&mut self) -> Table {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2, 3, 4, 5] {
            let value = self.atom("x");
            table
                .insert(self.tuple(&[make_small(key), value]), &self.atoms)
                .expect("insert");
        }
        table
    }

    /// Chain a `/3` traversal and its `/1` resumptions to exhaustion, returning
    /// every materialized result in order (the continuation itself is never
    /// rendered).
    fn chain(
        &mut self,
        table: &Table,
        first: BeamResult<(Vec<Eterm>, Option<MatchContinuation>)>,
        resume: ResumeStep,
    ) -> Vec<Eterm> {
        let mut acc = Vec::new();
        let (mut chunk, mut cont) = first.expect("the first chunk succeeds");
        acc.append(&mut chunk);
        while let Some(continuation) = cont {
            let (mut chunk, next) = resume(table, &continuation, &self.atoms, &mut self.out)
                .expect("a resume succeeds");
            acc.append(&mut chunk);
            cont = next;
        }
        acc
    }

    /// The `[{{'$1','$2'}, [], ['$1']}]` spec the `select` cases run.
    fn key_spec(&mut self) -> MatchSpec {
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, NIL, body);
        compile(spec_term, &self.atoms).expect("the spec compiles")
    }

    /// `ets:select/3` + `select/1` chained with `limit = 2` to completion.
    fn select_limit_chain(&mut self) -> String {
        let table = self.chain_table();
        let spec = self.key_spec();
        let first = table.select_limit(&spec, 2, &self.atoms, &mut self.out);
        let results = self.chain(&table, first, Table::select_continue);
        self.inorder(&results)
    }

    /// `ets:match/3` + `match/1` chained with `limit = 2` to completion.
    fn match_limit_chain(&mut self) -> String {
        let table = self.chain_table();
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let pattern = self.tuple(&[v1, v2]);
        let first = table.match_limit(pattern, 2, &self.atoms, &mut self.out);
        let results = self.chain(&table, first, Table::match_continue);
        self.inorder(&results)
    }

    /// `ets:match_object/3` + `match_object/1` chained with `limit = 2` to
    /// completion.
    fn match_object_limit_chain(&mut self) -> String {
        let table = self.chain_table();
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let pattern = self.tuple(&[v1, v2]);
        let first = table.match_object_limit(pattern, 2, &self.atoms, &mut self.out);
        let results = self.chain(&table, first, Table::match_object_continue);
        self.inorder(&results)
    }

    /// `limit == 0` is `badarg` (the scan never starts).
    fn select_limit_zero_badarg(&mut self) -> String {
        let table = self.chain_table();
        let spec = self.key_spec();
        cap_chunk(
            table.select_limit(&spec, 0, &self.atoms, &mut self.out),
            &self.atoms,
        )
    }

    /// An empty table's `/3` is the **bare** atom `'$end_of_table'` — no results
    /// and no continuation.
    fn select_limit_empty_table(&mut self) -> String {
        let table = self.table(TableKind::OrderedSet, 1);
        let spec = self.key_spec();
        let (results, continuation) = table
            .select_limit(&spec, 2, &self.atoms, &mut self.out)
            .expect("an empty table's select/3 succeeds");
        assert!(
            results.is_empty() && continuation.is_none(),
            "an empty table's /3 yields no chunk and no continuation (the oracle's bare \
             '$end_of_table')"
        );
        "$end_of_table".to_string()
    }
}

/// The limit/continuation cases, in the oracle's order.
pub(super) fn continuation_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push(("select_limit_chain".into(), battery.select_limit_chain()));
    cases.push(("match_limit_chain".into(), battery.match_limit_chain()));
    cases.push((
        "match_object_limit_chain".into(),
        battery.match_object_limit_chain(),
    ));
    cases.push((
        "select_limit_zero_badarg".into(),
        battery.select_limit_zero_badarg(),
    ));
    cases.push((
        "select_limit_empty_table".into(),
        battery.select_limit_empty_table(),
    ));

    cases
}
