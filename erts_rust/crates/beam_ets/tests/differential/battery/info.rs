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

//! The **`info` / option-surface** case group: the `name` vs `named_table`
//! distinction, the renderable `ets:info/2` option fields for four option
//! combinations, the runtime-shape fields (`heir`/`node`/`decentralized_counters`)
//! for a `set` and an `ordered_set` with write concurrency, an unknown
//! `ets:info/2` key, `lookup_element/3,4` on the multi-object kinds
//! (`bag`/`duplicate_bag`), the atomicity of `ets:insert/2` over a list, and the
//! **ownership** surface (`ets:give_away/3` and the `{heir, ...}` option of
//! `ets:setopts/2`).
//!
//! `id`, `memory` and `owner` are implementation-specific (a table id, an
//! allocation figure, a pid) and are therefore never rendered; the renderable
//! fields are read through `ets:info/2`.

use super::*;
use beam_ets::{Access, Heir, OwnerId, SetOpt};

impl Battery {
    /// `ets:info(T, Field)`: the reported value, or `badarg` for a key ETS does
    /// not report (the oracle raises rather than answering `undefined`).
    pub(super) fn info_field(&mut self, table: &Table, field: &str) -> BeamResult<Eterm> {
        let entries = table.info().entries(&mut self.atoms);
        match self.atoms.lookup(field) {
            Some(atom) => entries
                .iter()
                .find(|(name, _)| *name == atom)
                .map(|(_, value)| *value)
                .ok_or(BeamError::BadArg),
            None => Err(BeamError::BadArg),
        }
    }

    /// The eight renderable `ets:info/2` fields of `table`, as one tuple.
    fn info_opts(&mut self, table: &Table) -> String {
        let fields = [
            "name",
            "named_table",
            "type",
            "keypos",
            "protection",
            "compressed",
            "write_concurrency",
            "read_concurrency",
        ];
        let mut parts = Vec::with_capacity(fields.len());
        for field in fields {
            let value = self
                .info_field(table, field)
                .expect("a renderable info field is reported");
            parts.push(self.r(value));
        }
        tuple_of(&parts)
    }

    /// A recorded `name` and the `named_table` flag are independent: a name is
    /// reported whether or not `named_table` was requested, and only the option
    /// sets the flag.
    fn info_name_named_table(&mut self) -> String {
        let anon_name = self.atoms.intern("anon_case_name");
        let mut anon_opts = TableOptions::new();
        anon_opts.set_name(anon_name);
        let anon = Table::new(TableId::next(), anon_opts).expect("a fresh table");
        let n1 = self.info_field(&anon, "name").expect("name is reported");
        let b1 = self
            .info_field(&anon, "named_table")
            .expect("named_table is reported");

        let named_name = self.atoms.intern("named_case_name");
        let mut named_opts = TableOptions::new();
        named_opts.set_name(named_name).set_named(true);
        let named = Table::new(TableId::next(), named_opts).expect("a fresh table");
        let n2 = self.info_field(&named, "name").expect("name is reported");
        let b2 = self
            .info_field(&named, "named_table")
            .expect("named_table is reported");

        tuple_of(&[self.r(n1), self.r(b1), self.r(n2), self.r(b2)])
    }

    /// The renderable option fields for four `ets:new/2` combinations: default,
    /// `{keypos, 2}`, `[ordered_set, public, compressed, {write_concurrency,
    /// true}, {read_concurrency, true}]`, and a `private` table requesting both
    /// concurrency flags (which a `private` table forces off).
    fn info_option_fields(&mut self) -> String {
        // Every table records the name `diff`; none is `named_table`, so no name
        // is registered globally.
        let name = self.atoms.intern("diff");

        let mut default_opts = TableOptions::new();
        default_opts.set_name(name);
        let default = Table::new(TableId::next(), default_opts).expect("a fresh table");

        let mut keypos_opts = TableOptions::new();
        keypos_opts
            .set_name(name)
            .set_keypos(2)
            .expect("keypos is >= 1");
        let keypos = Table::new(TableId::next(), keypos_opts).expect("a fresh table");

        let mut public_opts = TableOptions::new();
        public_opts
            .set_name(name)
            .set_kind(TableKind::OrderedSet)
            .set_access(Access::Public)
            .set_compressed(true)
            .set_write_concurrency(true)
            .set_read_concurrency(true);
        let public = Table::new(TableId::next(), public_opts).expect("a fresh table");

        let mut private_opts = TableOptions::new();
        private_opts
            .set_name(name)
            .set_access(Access::Private)
            .set_write_concurrency(true)
            .set_read_concurrency(true);
        let private = Table::new(TableId::next(), private_opts).expect("a fresh table");

        let a = self.info_opts(&default);
        let b = self.info_opts(&keypos);
        let c = self.info_opts(&public);
        let d = self.info_opts(&private);
        tuple_of(&[a, b, c, d])
    }

    /// The three `ets:info/1` fields fixed by the runtime shape rather than by
    /// the table, read for a plain `set` and for a public `ordered_set` with
    /// `write_concurrency` — one tuple of six values, in this order:
    /// `{set_heir, set_node, set_decentralized_counters,
    ///   ordered_heir, ordered_node, ordered_decentralized_counters}`.
    ///
    /// `heir` is always `none`, `node` always `nonode@nohost` (the runtime is
    /// never distributed), and `decentralized_counters` is `true` iff an
    /// `ordered_set` has `write_concurrency` on (the CATREE path).
    fn info_static_fields(&mut self) -> String {
        // Both tables record the name `diff` and are `public`, mirroring the
        // oracle's `new/1`/`new/2` helpers.
        let name = self.atoms.intern("diff");

        let mut set_opts = TableOptions::new();
        set_opts.set_name(name).set_access(Access::Public);
        let set = Table::new(TableId::next(), set_opts).expect("a fresh table");

        let mut ordered_opts = TableOptions::new();
        ordered_opts
            .set_name(name)
            .set_kind(TableKind::OrderedSet)
            .set_access(Access::Public)
            .set_write_concurrency(true);
        let ordered = Table::new(TableId::next(), ordered_opts).expect("a fresh table");

        let fields = ["heir", "node", "decentralized_counters"];
        let mut parts = Vec::with_capacity(2 * fields.len());
        for table in [&set, &ordered] {
            for field in fields {
                let value = self
                    .info_field(table, field)
                    .expect("a fixed info field is reported");
                parts.push(self.r(value));
            }
        }
        tuple_of(&parts)
    }

    /// `ets:info(T, no_such_key)` is `badarg`.
    fn info_unknown_key(&mut self) -> String {
        let table = self.table(TableKind::Set, 1);
        cap_value(self.info_field(&table, "no_such_key"), &self.atoms)
    }

    /// `ets:lookup_element/3` on a multi-object kind answers the *list* of the
    /// `Pos`-th element of every object under the key (a `set` stays scalar); an
    /// absent key is `badarg` (`/3`) or the default (`/4`), `Pos == 0` is
    /// `badarg`, and one object too short for `Pos` makes the whole call
    /// `badarg`.
    fn lookup_element_multi(&mut self) -> String {
        // Two objects under key 1 → the element list, in stored order.
        let mut bag = self.table(TableKind::Bag, 1);
        bag.insert(self.object(1, 10), &self.atoms).expect("insert");
        bag.insert(self.object(1, 20), &self.atoms).expect("insert");
        let bag_e = bag
            .lookup_element_all(make_small(1), 2, &self.atoms)
            .expect("lookup_element_all");

        // A single object under the key → a one-element list.
        let mut one = self.table(TableKind::Bag, 1);
        one.insert(self.object(2, 7), &self.atoms).expect("insert");
        let one_e = one
            .lookup_element_all(make_small(2), 1, &self.atoms)
            .expect("lookup_element_all");

        // `duplicate_bag` preserves duplicates.
        let mut dup = self.table(TableKind::DuplicateBag, 1);
        for _ in 0..2 {
            dup.insert(self.object(1, 10), &self.atoms).expect("insert");
        }
        dup.insert(self.object(1, 20), &self.atoms).expect("insert");
        let dup_e = dup
            .lookup_element_all(make_small(1), 2, &self.atoms)
            .expect("lookup_element_all");

        // `set` stays scalar.
        let mut set = self.table(TableKind::Set, 1);
        set.insert(self.object(1, 10), &self.atoms).expect("insert");
        let set_e = set
            .lookup_element(make_small(1), 2, &self.atoms)
            .expect("lookup_element");

        let absent3 = cap_terms(
            bag.lookup_element_all(make_small(99), 2, &self.atoms),
            &self.atoms,
        );
        let default = self.atom("default");
        let absent4 = bag
            .lookup_element_all_or(make_small(99), 2, default, &self.atoms)
            .expect("lookup_element_all_or");
        // `lookup_element_all_or` reports an absent key as `None`; the caller then
        // yields `default`. A present key would yield the element list.
        let absent4 = match absent4 {
            None => self.r(default),
            Some(values) => self.inorder(&values),
        };
        let pos0 = cap_terms(
            bag.lookup_element_all(make_small(1), 0, &self.atoms),
            &self.atoms,
        );

        // One object too short for `Pos` makes the whole call `badarg`.
        let mut short = self.table(TableKind::Bag, 1);
        short
            .insert(self.ints(&[3, 5, 6]), &self.atoms)
            .expect("insert");
        short
            .insert(self.ints(&[3, 7]), &self.atoms)
            .expect("insert");
        let short_err = cap_terms(
            short.lookup_element_all(make_small(3), 3, &self.atoms),
            &self.atoms,
        );

        tuple_of(&[
            self.inorder(&bag_e),
            self.inorder(&one_e),
            self.inorder(&dup_e),
            self.r(set_e),
            absent3,
            absent4,
            pos0,
            short_err,
        ])
    }

    /// `ets:insert/2` over a list is atomic: one invalid element makes the whole
    /// call `badarg` and inserts none of the elements, not even the valid ones
    /// that precede it.
    fn insert_list_atomic(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        table
            .insert(self.object(1, 10), &self.atoms)
            .expect("insert");
        let good = self.object(2, 20);
        // A bare small integer is not an object at all.
        let bad = make_small(7);
        let r1 = cap_insert_list(table.insert_list(&[good, bad], &self.atoms));
        let l1 = self.srt(&table.tab2list());

        // A `{keypos, 2}` table: `{c}` is a tuple but shorter than the key position.
        let mut keypos = self.table(TableKind::Set, 2);
        let a_atom = self.atom("a");
        let a = self.tuple(&[a_atom, make_small(1)]);
        keypos.insert(a, &self.atoms).expect("insert");
        let b_atom = self.atom("b");
        let b = self.tuple(&[b_atom, make_small(2)]);
        let c_atom = self.atom("c");
        let c = self.tuple(&[c_atom]);
        let r2 = cap_insert_list(keypos.insert_list(&[b, c], &self.atoms));
        let l2 = self.srt(&keypos.tab2list());

        tuple_of(&[r1, l1, r2, l2])
    }

    /// `ets:give_away/3`: `true` only when the caller is the current owner and the
    /// target is a *different* live process; the not-owner, self-gift and non-pid
    /// target cases are all `badarg`. Pids are unrenderable, so only the return
    /// value (or the error) is rendered.
    ///
    /// The crate models the target as an opaque [`OwnerId`]; a `None` new owner is
    /// its stand-in for the oracle's non-pid/dead-pid target (see
    /// `src/table/ownership.rs`).
    fn ownership_give_away(&mut self) -> String {
        let owner = OwnerId::new(1);
        let child = OwnerId::new(2);

        // (a) success: the owner hands the table to a different process.
        let mut handed = self.table(TableKind::Set, 1);
        handed.set_owner(Some(owner));
        let success = cap_unit(handed.give_away(Some(owner), Some(child)));

        // (b) the caller is no longer the owner — it just gave the table away.
        let not_owner = cap_unit(handed.give_away(Some(owner), Some(owner)));

        // (c) the caller's own pid: an owner gifting the table to itself.
        let mut self_gift = self.table(TableKind::Set, 1);
        self_gift.set_owner(Some(owner));
        let own_pid = cap_unit(self_gift.give_away(Some(owner), Some(owner)));

        // (d) a non-pid target — `None` is this crate's stand-in.
        let mut bad_target = self.table(TableKind::Set, 1);
        bad_target.set_owner(Some(owner));
        let non_pid = cap_unit(bad_target.give_away(Some(owner), None));

        tuple_of(&[success, not_owner, own_pid, non_pid])
    }

    /// The `{heir, ...}` surface of `ets:setopts/2`: `ets:info(T, heir)` is `none`
    /// by default, installing and clearing the heir both answer `true`, and
    /// `ets:info(T, holder)` is `badarg`. The heir *pid* is never rendered (only
    /// the pid-less `none`/`undefined` the crate reports).
    fn ownership_heir(&mut self) -> String {
        let owner = OwnerId::new(1);
        let heir_pid = OwnerId::new(9);
        let mut table = self.table(TableKind::Set, 1);
        table.set_owner(Some(owner));

        let default_heir_term = self
            .info_field(&table, "heir")
            .expect("a fresh table reports heir = none");
        let default_heir = self.r(default_heir_term);
        let install = cap_unit(table.setopts(
            Some(owner),
            SetOpt::Heir(Heir {
                owner: heir_pid,
                data: Some(make_small(7)),
            }),
        ));
        let clear = cap_unit(table.setopts(Some(owner), SetOpt::ClearHeir));
        let cleared_heir_term = self
            .info_field(&table, "heir")
            .expect("a cleared table reports heir = none");
        let cleared_heir = self.r(cleared_heir_term);
        let holder = cap_value(self.info_field(&table, "holder"), &self.atoms);

        tuple_of(&[default_heir, install, clear, cleared_heir, holder])
    }
}

/// The `info` cases, in the oracle's order.
pub(super) fn info_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push((
        "info_name_named_table".into(),
        battery.info_name_named_table(),
    ));
    cases.push(("info_option_fields".into(), battery.info_option_fields()));
    cases.push(("info_static_fields".into(), battery.info_static_fields()));
    cases.push(("info_unknown_key".into(), battery.info_unknown_key()));
    cases.push((
        "lookup_element_multi".into(),
        battery.lookup_element_multi(),
    ));
    cases.push(("insert_list_atomic".into(), battery.insert_list_atomic()));
    cases.push((
        "ownership_give_away".into(),
        battery.ownership_give_away(),
    ));
    cases.push(("ownership_heir".into(), battery.ownership_heir()));

    cases
}
