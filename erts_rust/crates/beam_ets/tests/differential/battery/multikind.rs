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

//! The **multi-object + ordered** discrimination group: the behaviours that
//! distinguish the multi-object kinds (`bag`/`duplicate_bag`) from the single
//! object kinds (`set`/`ordered_set`), plus the `ordered_set` facts the per-kind
//! block and the `{keypos, 2}` group do not reach.
//!
//! Every case was probed against the *system* `erl` (OTP 29.1.1 / erts-17.1)
//! before it was written down; the facts the oracle forced:
//!
//! * **`insert_new/2` refuses whenever the KEY exists**, for *every* kind
//!   including `duplicate_bag`, and even for an object identical to a stored one
//!   — the key, not the object, is what the check looks at.
//! * **A `bag` keeps distinct objects (an exact duplicate is a no-op) while a
//!   `duplicate_bag` keeps every insert.** The multiplicity is observable through
//!   `size`, `lookup`, `select`, `match_object`, `match` and `select_count`.
//! * **`delete_object/2` removes *every* stored object equal to it** (not one):
//!   a `duplicate_bag` holding two `{1,10}`s loses both from one
//!   `ets:delete_object(T, {1,10})`. (`delete/2` still drops the whole key.)
//! * **`size` counts OBJECTS, not keys**, for the multi kinds.
//! * **`update_counter/3,4` and `update_element/3` are `badarg` on the multi
//!   kinds** (only `set`/`ordered_set` have counters), and the update leaves the
//!   table untouched.
//! * **`select_replace/2` works on `duplicate_bag`** (rewriting each matched
//!   object, multiplicity preserved) even though `bag` is refused.
//! * **`ordered_set` `delete_object/2` uses the exact `=:=` object identity**
//!   (`{1.0, a}` does not delete a stored `{1, a}`), while an `ordered_set`
//!   *insert* of an arithmetically-equal key still replaces.
//! * **A `{keypos, 2}` `ordered_set`** orders and navigates by its second
//!   element.
//!
//! The hash-kind navigation cases (in the sibling `navigation` group) pin the
//! order-independent facts: the empty-table `first`/`last` are `'$end_of_table'`,
//! an **absent anchor is `badarg` for the hash kinds** (unlike an `ordered_set`'s
//! range bound), and a `first`+`next` walk visits each key exactly once.

use super::*;

impl Battery {
    /// The shared `insert_new/2` probe: `{1,10}` inserts, then a *different*
    /// object under the same key and an *identical* object are both refused, and a
    /// fresh key inserts. Rendered as `{b1,b2,b3,b4, size, lookup(1), lookup(2)}`.
    fn insert_new_key_exists(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        let b1 = table
            .insert_new(self.object(1, 10), &self.atoms)
            .expect("insert_new");
        let b2 = table
            .insert_new(self.object(1, 20), &self.atoms)
            .expect("insert_new");
        let b3 = table
            .insert_new(self.object(1, 10), &self.atoms)
            .expect("insert_new");
        let b4 = table
            .insert_new(self.object(2, 20), &self.atoms)
            .expect("insert_new");
        let one = table.lookup(make_small(1), &self.atoms);
        let two = table.lookup(make_small(2), &self.atoms);
        tuple_of(&[
            bool_str(b1),
            bool_str(b2),
            bool_str(b3),
            bool_str(b4),
            table.size().to_string(),
            self.srt(&one),
            self.srt(&two),
        ])
    }

    /// A `{'_','_'}` head tuple (both elements wildcards), built once.
    fn any_pair_head(&mut self) -> Eterm {
        let wildcard = self.atom("_");
        self.tuple(&[wildcard, wildcard])
    }

    /// The duplicate-object multiplicity of a multi kind. Two `{1,10}`s and one
    /// `{1,20}` are inserted; a `bag` keeps **two** distinct objects, a
    /// `duplicate_bag` keeps **three**. The multiplicity is rendered through
    /// `size`, `lookup`, `select`/`match_object` bodies, the `match` result count
    /// and `select_count`.
    fn duplicate_object_multiplicity(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for value in [10i64, 10, 20] {
            table.insert(self.object(1, value), &self.atoms).expect("insert");
        }
        let stored = table.lookup(make_small(1), &self.atoms);

        let head = self.any_pair_head();
        let whole = self.atom("$_");
        let body = self.list(&[whole]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);

        let objects = table
            .match_object(head, &self.atoms, &mut self.out)
            .expect("match_object");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let var_pattern = self.tuple(&[v1, v2]);
        let matches = table
            .match_(var_pattern, &self.atoms, &mut self.out)
            .expect("match/2");

        // `select_count/2` counts the objects whose body evaluates to `true`.
        let truth = self.atom("true");
        let count_body = self.list(&[truth]);
        let count_term = self.spec(head, NIL, count_body);
        let count_spec = compile(count_term, &self.atoms).expect("the spec compiles");
        let count = table
            .select_count(&count_spec, &self.atoms, &mut self.out)
            .expect("select_count");

        tuple_of(&[
            table.size().to_string(),
            self.srt(&stored),
            self.srt(&selected),
            self.srt(&objects),
            matches.len().to_string(),
            count.to_string(),
        ])
    }

    /// `delete_object/2` removes **every** stored object equal to the argument.
    /// The table holds `{1,10}`,`{1,10}`,`{1,20}`; deleting `{1,10}` leaves only
    /// `{1,20}` — even for the `duplicate_bag` that stored two copies.
    fn delete_object_all_occurrences(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for value in [10i64, 10, 20] {
            table.insert(self.object(1, value), &self.atoms).expect("insert");
        }
        table
            .delete_object(self.object(1, 10), &self.atoms)
            .expect("delete_object");
        let stored = table.lookup(make_small(1), &self.atoms);
        let all = table.tab2list();
        tuple_of(&[
            table.size().to_string(),
            self.srt(&stored),
            self.srt(&all),
        ])
    }

    /// `size`/`tab2list`/`select`/`select_count` all count **objects**, not keys:
    /// three objects under key 1 and two under key 2 make a size of five.
    fn size_counts_objects(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for (key, value) in [(1i64, 10i64), (1, 20), (1, 30), (2, 40), (2, 50)] {
            table.insert(self.object(key, value), &self.atoms).expect("insert");
        }
        let head = self.any_pair_head();
        let whole = self.atom("$_");
        let body = self.list(&[whole]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);

        let truth = self.atom("true");
        let count_body = self.list(&[truth]);
        let count_term = self.spec(head, NIL, count_body);
        let count_spec = compile(count_term, &self.atoms).expect("the spec compiles");
        let count = table
            .select_count(&count_spec, &self.atoms, &mut self.out)
            .expect("select_count");

        tuple_of(&[
            table.size().to_string(),
            table.tab2list().len().to_string(),
            selected.len().to_string(),
            count.to_string(),
        ])
    }

    /// `take/2` removes the whole key (every object under it); a subsequent
    /// `delete/2` of the other key empties the table.
    fn take_and_delete_key(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for value in [10i64, 20, 30] {
            table.insert(self.object(1, value), &self.atoms).expect("insert");
        }
        table.insert(self.object(2, 40), &self.atoms).expect("insert");
        let taken = table.take(make_small(1), &self.atoms).expect("take");
        let after_take = table.size();
        table.delete(make_small(2), &self.atoms).expect("delete");
        tuple_of(&[
            self.srt(&taken),
            after_take.to_string(),
            table.size().to_string(),
            bool_str(table.member(make_small(1), &self.atoms)),
            bool_str(table.member(make_small(2), &self.atoms)),
        ])
    }

    /// `update_counter/3,4` is `badarg` on a multi kind, both without and with a
    /// default object.
    fn update_counter_badarg(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        table.insert(self.object(1, 10), &self.atoms).expect("insert");
        let c3 = table.update_counter(make_small(1), make_small(1), &self.atoms, &mut self.out);
        let c3 = cap_value(c3, &self.atoms);
        let default = self.object(1, 0);
        let c4 = table.update_counter_with(
            make_small(1),
            make_small(1),
            default,
            &self.atoms,
            &mut self.out,
        );
        let c4 = cap_value(c4, &self.atoms);
        tuple_of(&[c3, c4])
    }

    /// `update_element/3` is `badarg` on a multi kind and leaves the object alone.
    fn update_element_badarg(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        table.insert(self.ints(&[1, 10, 20]), &self.atoms).expect("insert");
        let updated = table.update_element(
            make_small(1),
            &[(3, make_small(99))],
            &self.atoms,
            &mut self.out,
        );
        let updated = cap_bool(updated);
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[updated, self.srt(&stored)])
    }

    /// The `'$2' * 2` rewrite spec `{{'$1','$2'}} -> {{'$1', '$2' * 2}}`.
    fn double_value_spec(&mut self) -> Eterm {
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let star = self.atom("*");
        let doubled = self.tuple(&[star, v2, make_small(2)]);
        let inner = self.tuple(&[v1, doubled]);
        let construction = self.tuple(&[inner]);
        let body = self.list(&[construction]);
        self.spec(head, NIL, body)
    }

    /// `select_replace/2` rewrites **every** matched object of a `duplicate_bag`
    /// (only `bag` is refused), keeping the multiplicity and the replacement
    /// order.
    fn duplicate_bag_select_replace(&mut self) -> String {
        let mut table = self.table(TableKind::DuplicateBag, 1);
        table.insert(self.object(1, 10), &self.atoms).expect("insert");
        table.insert(self.object(1, 20), &self.atoms).expect("insert");
        let spec_term = self.double_value_spec();
        let spec = compile(spec_term, &self.atoms).expect("the spec compiles");
        let count = cap_count(table.select_replace(&spec, &self.atoms, &mut self.out));
        let objects = table.tab2list();
        tuple_of(&[count, self.srt(&objects)])
    }

    /// The identity `'$_'` body on a `duplicate_bag` counts both copies as
    /// replaced and preserves the duplicate.
    fn duplicate_bag_select_replace_identity(&mut self) -> String {
        let mut table = self.table(TableKind::DuplicateBag, 1);
        table.insert(self.object(1, 10), &self.atoms).expect("insert");
        table.insert(self.object(1, 10), &self.atoms).expect("insert");
        let head = self.any_pair_head();
        let whole = self.atom("$_");
        let body = self.list(&[whole]);
        let spec_term = self.spec(head, NIL, body);
        let spec = compile(spec_term, &self.atoms).expect("the spec compiles");
        let count = cap_count(table.select_replace(&spec, &self.atoms, &mut self.out));
        let objects = table.tab2list();
        tuple_of(&[count, self.srt(&objects)])
    }

    /// `lookup_element/4` on a multi kind: a present key yields the element
    /// **list** in insertion order, an absent key yields the default, and an
    /// object too short for the position is `badarg`.
    fn multi_lookup_element_or(&mut self) -> String {
        let mut table = self.table(TableKind::Bag, 1);
        table.insert(self.ints(&[1, 10, 100]), &self.atoms).expect("insert");
        table.insert(self.ints(&[1, 20, 200]), &self.atoms).expect("insert");
        let default = self.atom("dflt");

        let present = table
            .lookup_element_all_or(make_small(1), 2, default, &self.atoms)
            .expect("lookup_element_all_or");
        let present = match present {
            Some(values) => self.inorder(&values),
            None => self.r(default),
        };
        let absent = table
            .lookup_element_all_or(make_small(99), 2, default, &self.atoms)
            .expect("lookup_element_all_or");
        let absent = match absent {
            Some(values) => self.inorder(&values),
            None => self.r(default),
        };
        // One object has arity 3, so `pos == 4` is `badarg` for the whole call.
        let short = cap_terms(
            table.lookup_element_all(make_small(1), 4, &self.atoms),
            &self.atoms,
        );
        tuple_of(&[present, absent, short])
    }

    /// `ordered_set` `delete_object/2` matches the **exact** object (`=:=`): an
    /// inserted `{1, a}` survives a `delete_object/{1.0, a}` and only the identical
    /// term removes it.
    fn ordered_delete_object_exact_eq(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        table
            .insert(self.tuple(&[make_small(1), a]), &self.atoms)
            .expect("insert");
        let float_key = self.float(1.0);
        table
            .delete_object(self.tuple(&[float_key, a]), &self.atoms)
            .expect("delete_object");
        let after_float = table.size();
        table
            .delete_object(self.tuple(&[make_small(1), a]), &self.atoms)
            .expect("delete_object");
        tuple_of(&[after_float.to_string(), table.size().to_string()])
    }

    /// A `{keypos, 2}` `ordered_set`: `tab2list` ascends by the second element,
    /// `first`/`last` report that key, and `lookup` finds by it.
    fn ordered_keypos2(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 2);
        let a = self.atom("a");
        let b = self.atom("b");
        let c = self.atom("c");
        table
            .insert(self.tuple(&[a, make_small(3)]), &self.atoms)
            .expect("insert");
        table
            .insert(self.tuple(&[b, make_small(1)]), &self.atoms)
            .expect("insert");
        table
            .insert(self.tuple(&[c, make_small(2)]), &self.atoms)
            .expect("insert");
        let tab_first: Vec<Eterm> = table
            .tab2list()
            .into_iter()
            .map(|object| {
                // SAFETY: every inserted object is a 2-tuple, so element 0 exists.
                unsafe { tuple_element(object, 0) }
            })
            .collect();
        let looked: Vec<Eterm> = table
            .lookup(make_small(2), &self.atoms)
            .into_iter()
            .map(|object| {
                // SAFETY: as above.
                unsafe { tuple_element(object, 0) }
            })
            .collect();
        tuple_of(&[
            table.size().to_string(),
            self.inorder(&tab_first),
            self.opt(table.first(&self.atoms)),
            self.opt(table.last(&self.atoms)),
            self.inorder(&looked),
        ])
    }

    /// `ordered_set` `update_counter/3` and `update_element/3` both work (only the
    /// *multi* kinds refuse them).
    fn ordered_update_element(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        table.insert(self.ints(&[1, 10, 20]), &self.atoms).expect("insert");
        let value = table
            .update_counter(make_small(1), make_small(5), &self.atoms, &mut self.out)
            .expect("update_counter");
        let updated = table
            .update_element(
                make_small(1),
                &[(3, make_small(99))],
                &self.atoms,
                &mut self.out,
            )
            .expect("update_element");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[self.r(value), bool_str(updated), self.inorder(&stored)])
    }

    /// `ordered_set` `update_counter/4` deposits the default object for an absent
    /// key (the bare `Incr` form updates element 2).
    fn ordered_update_counter_default(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let default = self.object(1, 0);
        let value = table
            .update_counter_with(
                make_small(1),
                make_small(5),
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[self.r(value), self.inorder(&stored)])
    }

    /// Hash-kind navigation: the empty-table `first`/`last` are `'$end_of_table'`
    /// and an **absent anchor is `badarg`** (unlike an `ordered_set`'s range
    /// bound). Pinned for `set`/`bag`/`duplicate_bag`.
    fn hash_nav_absent(&mut self) -> String {
        let mut rendered = Vec::new();
        for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
            let mut table = self.table(kind, 1);
            let first = self.opt(table.first(&self.atoms));
            let last = self.opt(table.last(&self.atoms));
            let next_fresh = cap_opt(
                table.next(make_small(5), &self.atoms),
                &self.atoms,
            );
            let prev_fresh = cap_opt(
                table.prev(make_small(5), &self.atoms),
                &self.atoms,
            );
            table.insert(self.object(1, 10), &self.atoms).expect("insert");
            table.insert(self.object(2, 20), &self.atoms).expect("insert");
            let next_absent = cap_opt(
                table.next(make_small(99), &self.atoms),
                &self.atoms,
            );
            let prev_absent = cap_opt(
                table.prev(make_small(99), &self.atoms),
                &self.atoms,
            );
            rendered.push(tuple_of(&[
                first,
                last,
                next_fresh,
                prev_fresh,
                next_absent,
                prev_absent,
            ]));
        }
        list_of(&rendered)
    }

    /// A hash-kind `first`+`next` walk visits each **key** exactly once (the
    /// order is not contract, so the visited keys are rendered **sorted**).
    fn hash_key_walk(&mut self) -> String {
        let mut rendered = Vec::new();
        for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
            let mut table = self.table(kind, 1);
            table.insert(self.object(1, 10), &self.atoms).expect("insert");
            table.insert(self.object(1, 20), &self.atoms).expect("insert");
            table.insert(self.object(2, 20), &self.atoms).expect("insert");
            let from = table.first(&self.atoms);
            let keys = self.walk_next(&table, from);
            rendered.push(tuple_of(&[
                self.srt(&keys),
                keys.len().to_string(),
            ]));
        }
        list_of(&rendered)
    }

    /// The hash-kind **`last`/`prev` aliasing** of `first`/`next`: ERTS's hash
    /// tables implement `ets:last/1` as `ets:first/1` and `ets:prev/2` as
    /// `ets:next/2` (`erl_db_hash.c:853-856`), so `last == first`, `prev(K) ==
    /// next(K)` for every present `K`, and a `prev` walk from `last` reproduces the
    /// `next` walk from `first` exactly. The internal order is not contract, so the
    /// case renders the **relationships** (booleans) for the empty, single-key and
    /// 5-key shapes, plus a sorted multiset of the walked keys.
    fn hash_last_prev_alias_first_next(&mut self) -> String {
        let mut rendered = Vec::new();
        for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
            let empty = self.table(kind, 1);
            let empty_alias = empty.last(&self.atoms) == empty.first(&self.atoms);

            let mut one = self.table(kind, 1);
            one.insert(self.object(1, 10), &self.atoms).expect("insert");
            let one_alias = one.last(&self.atoms) == one.first(&self.atoms);
            let one_prev_next = one.prev(make_small(1), &self.atoms).expect("prev")
                == one.next(make_small(1), &self.atoms).expect("next");

            let mut table = self.table(kind, 1);
            for key in 1..=5i64 {
                table
                    .insert(self.object(key, key * 10), &self.atoms)
                    .expect("insert");
            }
            let first = table.first(&self.atoms);
            let last = table.last(&self.atoms);
            let next_walk = self.walk_next(&table, first);
            let prev_walk = self.walk_prev(&table, last);
            let alias_all = next_walk.iter().all(|&key| {
                table.prev(key, &self.atoms).expect("prev")
                    == table.next(key, &self.atoms).expect("next")
            });
            rendered.push(tuple_of(&[
                bool_str(empty_alias),
                bool_str(one_alias),
                bool_str(one_prev_next),
                bool_str(last == first),
                bool_str(alias_all),
                bool_str(prev_walk == next_walk),
                self.srt(&prev_walk),
            ]));
        }
        list_of(&rendered)
    }

    /// The hash-kind **`select_reverse/2` aliasing** of `select/2`: ERTS's
    /// `db_select_reverse_hash` delegates to the forward chunk function
    /// (`erl_db_hash.c:2310-2313`), so the reverse traversal returns the same list in
    /// the same order — including each key's stored object order for a `bag` holding
    /// two objects under one key. Rendered as the boolean relationship plus a sorted
    /// multiset, since the internal order is not contract.
    fn hash_select_reverse_aliases_select(&mut self) -> String {
        let dollar1 = self.atom("$1");
        let underscore = self.atom("_");
        let whole = self.atom("$_");
        let head = self.tuple(&[dollar1, underscore]);
        let body = self.list(&[whole]);
        let spec_term = self.spec(head, NIL, body);
        let spec = compile(spec_term, &self.atoms).expect("the whole-object spec compiles");

        let mut rendered = Vec::new();
        for kind in [TableKind::Set, TableKind::Bag, TableKind::DuplicateBag] {
            let mut table = self.table(kind, 1);
            // Key 1 holds two objects for the multi kinds; both traversals must keep
            // that per-key stored order.
            table.insert(self.object(1, 10), &self.atoms).expect("insert");
            table.insert(self.object(1, 20), &self.atoms).expect("insert");
            table.insert(self.object(2, 30), &self.atoms).expect("insert");
            let forward = table
                .select(&spec, &self.atoms, &mut self.out)
                .expect("select");
            let reverse = table
                .select_reverse(&spec, &self.atoms, &mut self.out)
                .expect("select_reverse");
            rendered.push(tuple_of(&[
                bool_str(reverse == forward),
                self.srt(&reverse),
            ]));
        }
        list_of(&rendered)
    }

    /// `lookup_element/3` on a multi kind: a present key yields the element
    /// **list** (order not contract → sorted), while an absent key (no `default`)
    /// and a stored object too short for the position are both `badarg` — the
    /// distinction from the `/4` form's `default`.
    fn multi_lookup_element(&mut self) -> String {
        let mut table = self.table(TableKind::Bag, 1);
        table.insert(self.ints(&[1, 10]), &self.atoms).expect("insert");
        table.insert(self.ints(&[1, 20]), &self.atoms).expect("insert");
        table.insert(self.ints(&[2, 5]), &self.atoms).expect("insert");
        let present = table
            .lookup_element_all(make_small(1), 2, &self.atoms)
            .expect("lookup_element/3");
        // `{2, 5}` has arity 2, so `pos == 3` is `badarg` for the whole call.
        let short = cap_terms(
            table.lookup_element_all(make_small(2), 3, &self.atoms),
            &self.atoms,
        );
        let absent = cap_terms(
            table.lookup_element_all(make_small(99), 2, &self.atoms),
            &self.atoms,
        );
        tuple_of(&[self.srt(&present), absent, short])
    }

    /// `match_delete/2` on a multi kind: one call removes **every** matching
    /// object — so a `duplicate_bag`'s duplicate copies disappear together — and
    /// the call answers the atom `true`.
    fn multi_match_delete(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for value in [10i64, 10, 20] {
            table.insert(self.object(1, value), &self.atoms).expect("insert");
        }
        table.insert(self.object(2, 30), &self.atoms).expect("insert");
        let before = table.size();
        let wildcard = self.atom("_");
        let pattern = self.tuple(&[make_small(1), wildcard]);
        let deleted = table
            .match_delete(pattern, &self.atoms, &mut self.out)
            .expect("match_delete");
        let after = table.size();
        assert_eq!(deleted, before - after, "match_delete count == size drop");
        tuple_of(&[
            "ok:true".to_string(),
            before.to_string(),
            after.to_string(),
            self.srt(&table.tab2list()),
        ])
    }

    /// `select_delete/2` on a multi kind reports the number of **objects** it
    /// removed, so a `duplicate_bag`'s duplicate copies each count.
    fn multi_select_delete(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 1);
        for value in [10i64, 10, 20] {
            table.insert(self.object(1, value), &self.atoms).expect("insert");
        }
        table.insert(self.object(2, 30), &self.atoms).expect("insert");
        let wildcard = self.atom("_");
        let head = self.tuple(&[make_small(1), wildcard]);
        let truth = self.atom("true");
        let body = self.list(&[truth]);
        let spec_term = self.spec(head, NIL, body);
        let spec = compile(spec_term, &self.atoms).expect("the spec compiles");
        let count = cap_count(table.select_delete(&spec, &self.atoms, &mut self.out));
        tuple_of(&[
            count,
            self.srt(&table.tab2list()),
            table.size().to_string(),
        ])
    }

    /// A `duplicate_bag` `select_replace/2` whose body **changes the key** is
    /// `badarg` and leaves the table unchanged (only `bag` refuses replace
    /// outright).
    fn duplicate_bag_select_replace_changed_key(&mut self) -> String {
        let mut table = self.table(TableKind::DuplicateBag, 1);
        table.insert(self.object(1, 10), &self.atoms).expect("insert");
        table.insert(self.object(1, 20), &self.atoms).expect("insert");
        // The `{{2,'$2'}}` body builds `{2, Value}`, moving the key off 1.
        let v2 = self.atom("$2");
        let new_pair = self.tuple(&[make_small(2), v2]);
        let construction = self.tuple(&[new_pair]);
        let body = self.list(&[construction]);
        let v1 = self.atom("$1");
        let head = self.tuple(&[v1, v2]);
        let spec_term = self.spec(head, NIL, body);
        let spec = compile(spec_term, &self.atoms).expect("the spec compiles");
        let count = cap_count(table.select_replace(&spec, &self.atoms, &mut self.out));
        tuple_of(&[count, self.srt(&table.tab2list())])
    }

    /// A `{keypos, 2}` multi kind indexes by the second element: a `bag` still
    /// rejects an identical object (its size does not grow), a `duplicate_bag`
    /// keeps it.
    fn multi_kind_keypos2(&mut self, kind: TableKind) -> String {
        let mut table = self.table(kind, 2);
        let a = self.atom("a");
        let b = self.atom("b");
        let c = self.atom("c");
        table
            .insert(self.tuple(&[a, make_small(1)]), &self.atoms)
            .expect("insert");
        table
            .insert(self.tuple(&[b, make_small(1)]), &self.atoms)
            .expect("insert");
        table
            .insert(self.tuple(&[c, make_small(2)]), &self.atoms)
            .expect("insert");
        let size = table.size();
        // Re-inserting `{a, 1}` is a no-op for a `bag`, a second copy for a
        // `duplicate_bag`.
        table
            .insert(self.tuple(&[a, make_small(1)]), &self.atoms)
            .expect("insert");
        tuple_of(&[
            size.to_string(),
            self.srt(&table.lookup(make_small(1), &self.atoms)),
            self.srt(&table.lookup(make_small(2), &self.atoms)),
            bool_str(table.member(make_small(1), &self.atoms)),
            table.size().to_string(),
        ])
    }
}

/// The multi-object + ordered discrimination cases, in the oracle's order.
pub(super) fn multikind_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    for (kind, name) in [
        (TableKind::Set, "set"),
        (TableKind::OrderedSet, "ordered_set"),
        (TableKind::Bag, "bag"),
        (TableKind::DuplicateBag, "duplicate_bag"),
    ] {
        cases.push((
            format!("{name}_insert_new_key_exists"),
            battery.insert_new_key_exists(kind),
        ));
    }

    for (kind, name) in [
        (TableKind::Bag, "bag"),
        (TableKind::DuplicateBag, "duplicate_bag"),
    ] {
        cases.push((
            format!("{name}_duplicate_object_multiplicity"),
            battery.duplicate_object_multiplicity(kind),
        ));
        cases.push((
            format!("{name}_delete_object_all_occurrences"),
            battery.delete_object_all_occurrences(kind),
        ));
        cases.push((
            format!("{name}_size_counts_objects"),
            battery.size_counts_objects(kind),
        ));
        cases.push((
            format!("{name}_take_and_delete_key"),
            battery.take_and_delete_key(kind),
        ));
        cases.push((
            format!("{name}_update_counter_badarg"),
            battery.update_counter_badarg(kind),
        ));
        cases.push((
            format!("{name}_update_element_badarg"),
            battery.update_element_badarg(kind),
        ));
    }

    cases.push((
        "duplicate_bag_select_replace".into(),
        battery.duplicate_bag_select_replace(),
    ));
    cases.push((
        "duplicate_bag_select_replace_identity".into(),
        battery.duplicate_bag_select_replace_identity(),
    ));
    cases.push((
        "multi_lookup_element_or".into(),
        battery.multi_lookup_element_or(),
    ));
    cases.push((
        "multi_lookup_element".into(),
        battery.multi_lookup_element(),
    ));
    for (kind, name) in [
        (TableKind::Bag, "bag"),
        (TableKind::DuplicateBag, "duplicate_bag"),
    ] {
        cases.push((
            format!("{name}_match_delete"),
            battery.multi_match_delete(kind),
        ));
        cases.push((
            format!("{name}_select_delete"),
            battery.multi_select_delete(kind),
        ));
        cases.push((
            format!("{name}_keypos2"),
            battery.multi_kind_keypos2(kind),
        ));
    }
    cases.push((
        "duplicate_bag_select_replace_changed_key".into(),
        battery.duplicate_bag_select_replace_changed_key(),
    ));
    cases.push((
        "ordered_set_delete_object_exact_eq".into(),
        battery.ordered_delete_object_exact_eq(),
    ));
    cases.push((
        "ordered_set_keypos2".into(),
        battery.ordered_keypos2(),
    ));
    cases.push((
        "ordered_set_update_element".into(),
        battery.ordered_update_element(),
    ));
    cases.push((
        "ordered_set_update_counter_default".into(),
        battery.ordered_update_counter_default(),
    ));
    cases.push(("hash_nav_absent".into(), battery.hash_nav_absent()));
    cases.push(("hash_key_walk".into(), battery.hash_key_walk()));
    cases.push((
        "hash_last_prev_alias_first_next".into(),
        battery.hash_last_prev_alias_first_next(),
    ));
    cases.push((
        "hash_select_reverse_aliases_select".into(),
        battery.hash_select_reverse_aliases_select(),
    ));

    cases
}
