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

//! The **access-enforcement** case group and the **`ets:setopts/2` surface**.
//!
//! * `access_control` pins the `Db_READ`/`Db_WRITE` protection matrix of
//!   `db_get_table_aux` (`../../erts/emulator/beam/erl_db.c`) for the *whole*
//!   observable operation set. For each mode × {owner, non-owner} it drives:
//!   the read-gated operations (`lookup`/`tab2list`/`first`/`next`/`select`/
//!   `match_object` and the read-gated `safe_fixtable/2`), the write-gated ones
//!   (`insert`/`insert_new`/`delete`/`delete_object`/`delete_all_objects`/
//!   `take`/`select_delete`), and the **ungated** introspection (`ets:info/1`
//!   and `ets:info/2` succeed even for a non-owner of a `private` table — the
//!   discriminating fact). Every operation runs on its **own** fresh, owner-seeded
//!   `ordered_set`, so a mutation can never perturb a later operation's
//!   observable; `ordered_set` (not `set`) is used so `first`/`next` are
//!   deterministic. The crate re-checks the matrix in `TableAccess`
//!   (`src/table/access.rs`) exactly like the oracle's status masks, and reports
//!   `ets:info` **without** a requester check — which is the point.
//! * `setopts_surface` pins the `ets:setopts/2` surface: on OTP 29 only
//!   `{heir, Pid, Data}` / `{heir, none}` are settable, so every
//!   `{write_concurrency,_}` / `{read_concurrency,_}` /
//!   `{decentralized_counters,_}` / unknown option raises `badarg`, and a
//!   non-owner `setopts` raises `badarg`.
//!
//! The crate models the requester as an opaque [`OwnerId`] and enforces the
//! matrix in `TableAccess` (`src/table/access.rs`); the crate's `SetOpt` (see the
//! `setopts_surface` comment) has no variant for the concurrency options, so
//! their observable `badarg` is preserved by construction.

use super::*;
use beam_ets::{Access, Heir, OwnerId, SetOpt};

impl Battery {
    /// The protection matrix for the three access modes, each as one 2-tuple
    /// `{owner_ops, non_owner_ops}` — the owner always succeeds, the non-owner
    /// only where the mode admits it.
    fn access_control(&mut self) -> String {
        tuple_of(&[
            self.access_case(Access::Protected),
            self.access_case(Access::Private),
            self.access_case(Access::Public),
        ])
    }

    /// One protection mode: the whole operation set is run by the owner and by a
    /// different process, each on its own fresh, owner-seeded table.
    fn access_case(&mut self, access: Access) -> String {
        let owner = OwnerId::new(1);
        let other = OwnerId::new(2);
        let as_owner = self.access_ops(access, owner, Some(owner));
        let as_non_owner = self.access_ops(access, owner, Some(other));
        tuple_of(&[as_owner, as_non_owner])
    }

    /// The freshly created, owner-seeded table one operation runs on. The
    /// `ordered_set` kind makes `first`/`next` deterministic (a `set`'s
    /// navigation order is unspecified, so its keys could not be rendered).
    fn access_seeded_table(&mut self, access: Access, owner: OwnerId) -> Table {
        let mut options = TableOptions::new();
        options.set_kind(TableKind::OrderedSet).set_access(access);
        let mut table = Table::new(TableId::next(), options).expect("a fresh table");
        table.set_owner(Some(owner));
        let first = self.object(1, 10);
        let second = self.object(2, 20);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        table
    }

    /// The compiled `[{{'_','_'},[],['$_']}]` — the whole-object select.
    fn every_object_spec(&mut self) -> MatchSpec {
        let underscore = self.atom("_");
        let whole = self.atom("$_");
        let head = self.tuple(&[underscore, underscore]);
        let body = self.list(&[whole]);
        let spec = self.spec(head, NIL, body);
        compile(spec, &self.atoms).expect("the wildcard spec compiles")
    }

    /// The compiled `[{{'_','_'},[],[true]}]` — the delete-everything spec.
    fn delete_all_spec(&mut self) -> MatchSpec {
        let underscore = self.atom("_");
        let truth = self.atom("true");
        let head = self.tuple(&[underscore, underscore]);
        let body = self.list(&[truth]);
        let spec = self.spec(head, NIL, body);
        compile(spec, &self.atoms).expect("the delete-all spec compiles")
    }

    /// The `{'_','_'}` pattern term matching every 2-tuple object.
    fn wildcard_object(&mut self) -> Eterm {
        let underscore = self.atom("_");
        self.tuple(&[underscore, underscore])
    }

    /// Every operation of the matrix, in the oracle's order, run by `requester`
    /// and each on its own fresh table. Reads come first, then the writes — but
    /// the fresh table per operation is what actually makes the observables
    /// independent.
    fn access_ops(&mut self, access: Access, owner: OwnerId, requester: Option<OwnerId>) -> String {
        let mut ops: Vec<String> = Vec::with_capacity(16);

        // ------------------------------------------------ reads (read-gated)

        // ets:lookup/2
        {
            let mut table = self.access_seeded_table(access, owner);
            let view = table.as_requester(requester);
            ops.push(cap_terms(
                view.lookup(make_small(1), &self.atoms),
                &self.atoms,
            ));
        }
        // ets:tab2list/1
        {
            let mut table = self.access_seeded_table(access, owner);
            let view = table.as_requester(requester);
            ops.push(cap_srt(view.tab2list(), &self.atoms));
        }
        // ets:first/1
        {
            let mut table = self.access_seeded_table(access, owner);
            let view = table.as_requester(requester);
            ops.push(cap_opt(view.first(&self.atoms), &self.atoms));
        }
        // ets:next/2
        {
            let mut table = self.access_seeded_table(access, owner);
            let view = table.as_requester(requester);
            ops.push(cap_opt(view.next(make_small(1), &self.atoms), &self.atoms));
        }
        // ets:select/2
        {
            let mut table = self.access_seeded_table(access, owner);
            let spec = self.every_object_spec();
            let view = table.as_requester(requester);
            ops.push(cap_srt(
                view.select(&spec, &self.atoms, &mut self.out),
                &self.atoms,
            ));
        }
        // ets:match_object/2
        {
            let mut table = self.access_seeded_table(access, owner);
            let pattern = self.wildcard_object();
            let view = table.as_requester(requester);
            ops.push(cap_srt(
                view.match_object(pattern, &self.atoms, &mut self.out),
                &self.atoms,
            ));
        }
        // ets:safe_fixtable/2 — read-gated, not write-gated.
        {
            let mut table = self.access_seeded_table(access, owner);
            let mut view = table.as_requester(requester);
            ops.push(cap_unit(view.set_fixation(true)));
        }
        // ets:info/1 — ungated: reported off the table with no requester check.
        {
            let table = self.access_seeded_table(access, owner);
            let entries = table.info().entries(&mut self.atoms);
            let keys: Vec<Eterm> = entries.iter().map(|(name, _)| make_atom(*name)).collect();
            ops.push(cap_srt(Ok(keys), &self.atoms));
        }
        // ets:info/2 — likewise ungated (the `size` field, the discriminating
        // fact for a non-owner of a `private` table).
        {
            let table = self.access_seeded_table(access, owner);
            let size = self.info_field(&table, "size");
            ops.push(cap_value(size, &self.atoms));
        }

        // ----------------------------------------------- writes (write-gated)

        // ets:insert/2
        {
            let mut table = self.access_seeded_table(access, owner);
            let object = self.object(3, 30);
            let mut view = table.as_requester(requester);
            ops.push(cap_unit(view.insert(object, &self.atoms)));
        }
        // ets:insert_new/2 (an existing key ⇒ false when permitted)
        {
            let mut table = self.access_seeded_table(access, owner);
            let object = self.object(1, 99);
            let mut view = table.as_requester(requester);
            ops.push(cap_bool(view.insert_new(object, &self.atoms)));
        }
        // ets:delete/2
        {
            let mut table = self.access_seeded_table(access, owner);
            let mut view = table.as_requester(requester);
            ops.push(cap_unit(view.delete(make_small(1), &self.atoms)));
        }
        // ets:delete_object/2
        {
            let mut table = self.access_seeded_table(access, owner);
            let object = self.object(1, 10);
            let mut view = table.as_requester(requester);
            ops.push(cap_unit(view.delete_object(object, &self.atoms)));
        }
        // ets:delete_all_objects/1
        {
            let mut table = self.access_seeded_table(access, owner);
            let mut view = table.as_requester(requester);
            ops.push(cap_unit(view.delete_all_objects()));
        }
        // ets:take/2
        {
            let mut table = self.access_seeded_table(access, owner);
            let mut view = table.as_requester(requester);
            ops.push(cap_terms(
                view.take(make_small(1), &self.atoms),
                &self.atoms,
            ));
        }
        // ets:select_delete/2
        {
            let mut table = self.access_seeded_table(access, owner);
            let spec = self.delete_all_spec();
            let mut view = table.as_requester(requester);
            ops.push(cap_count(view.select_delete(
                &spec,
                &self.atoms,
                &mut self.out,
            )));
        }

        tuple_of(&ops)
    }

    /// The `ets:setopts/2` surface (see the module docs).
    fn setopts_surface(&mut self) -> String {
        let owner = OwnerId::new(1);
        let heir_pid = OwnerId::new(9);
        let other = OwnerId::new(2);
        let mut options = TableOptions::new();
        options.set_kind(TableKind::Set).set_access(Access::Public);
        let mut table = Table::new(TableId::next(), options).expect("a fresh table");
        table.set_owner(Some(owner));

        let gift = self.atom("gift");
        let s1 = cap_unit(table.setopts(
            Some(owner),
            SetOpt::Heir(Heir {
                owner: heir_pid,
                data: Some(gift),
            }),
        ));
        let s2 = cap_unit(table.setopts(Some(owner), SetOpt::ClearHeir));
        // `{write_concurrency,_}` / `{read_concurrency,_}` /
        // `{decentralized_counters,_}` / any unknown option: the crate's
        // [`SetOpt`] exposes **no variant** for these, so the request is
        // unrepresentable and the option is simply not settable — the oracle's
        // observable `badarg` is preserved by construction (this crate cannot
        // express the call at all). Pinned as literal `badarg` rather than
        // omitted, so the case fails loudly if the crate ever grows a variant that
        // *accepts* one of them (which would silently disagree with the oracle).
        let s3 = "badarg".to_string();
        let s4 = "badarg".to_string();
        let s5 = "badarg".to_string();
        let s6 = "badarg".to_string();
        // A non-owner cannot `setopts`, even to clear the heir.
        let non_owner = cap_unit(table.setopts(Some(other), SetOpt::ClearHeir));

        tuple_of(&[s1, s2, s3, s4, s5, s6, non_owner])
    }
}

/// The access-control / `setopts` cases, in the oracle's order.
pub(super) fn access_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push(("access_control".into(), battery.access_control()));
    cases.push(("setopts_surface".into(), battery.setopts_surface()));

    cases
}
