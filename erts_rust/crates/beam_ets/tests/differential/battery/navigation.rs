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

//! The **`ordered_set` navigation** case group: `first`/`last`/`next`/`prev`
//! over a populated table, absent anchors (between, before and after), and the
//! empty table.

use super::*;

impl Battery {
    fn ordered_iteration(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [3i64, 1, 2] {
            let object = self.object(key, key * 10);
            table.insert(object, &self.atoms).expect("insert");
        }
        let first = table.first(&self.atoms);
        let last = table.last(&self.atoms);
        let forward = self.walk_next(&table, first);
        let backward = self.walk_prev(&table, last);
        let tab_keys: Vec<Eterm> = table
            .tab2list()
            .into_iter()
            .map(|object| {
                // SAFETY: every object this battery inserts is a `{Key, Value}`
                // tuple, so element 0 exists.
                unsafe { tuple_element(object, 0) }
            })
            .collect();
        let after_last = table
            .next(make_small(3), &self.atoms)
            .expect("ordered_set next");
        let before_first = table
            .prev(make_small(1), &self.atoms)
            .expect("ordered_set prev");
        tuple_of(&[
            self.opt(first),
            self.opt(last),
            self.inorder(&forward),
            self.inorder(&backward),
            self.inorder(&tab_keys),
            self.opt(after_last),
            self.opt(before_first),
        ])
    }

    fn ordered_absent_bounds(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [10i64, 20, 30] {
            let object = self.object(key, key);
            table.insert(object, &self.atoms).expect("insert");
        }
        let next15 = table
            .next(make_small(15), &self.atoms)
            .expect("ordered_set next");
        let prev15 = table
            .prev(make_small(15), &self.atoms)
            .expect("ordered_set prev");
        let next5 = table
            .next(make_small(5), &self.atoms)
            .expect("ordered_set next");
        let prev5 = table
            .prev(make_small(5), &self.atoms)
            .expect("ordered_set prev");
        let next35 = table
            .next(make_small(35), &self.atoms)
            .expect("ordered_set next");
        let prev35 = table
            .prev(make_small(35), &self.atoms)
            .expect("ordered_set prev");
        tuple_of(&[
            self.opt(next15),
            self.opt(prev15),
            self.opt(next5),
            self.opt(prev5),
            self.opt(next35),
            self.opt(prev35),
        ])
    }

    fn ordered_empty_nav(&mut self) -> String {
        let table = self.table(TableKind::OrderedSet, 1);
        let first = table.first(&self.atoms);
        let last = table.last(&self.atoms);
        let next = table
            .next(make_small(5), &self.atoms)
            .expect("ordered_set next");
        let prev = table
            .prev(make_small(5), &self.atoms)
            .expect("ordered_set prev");
        tuple_of(&[
            self.opt(first),
            self.opt(last),
            self.opt(next),
            self.opt(prev),
        ])
    }

    /// The one observable `ordered_set` contract: the key **order**, pinned across
    /// mixed key types (integers incl. negative and zero, atoms, tuples, lists,
    /// binaries).
    ///
    /// Binaries cannot be rendered by the canonical renderer, so every inserted
    /// object carries a small-integer tag as element 2 and the case renders the
    /// resulting *tag permutation* — the `tab2list` order, the tag of the
    /// first/last object, and the `next`/`prev` chains — never a binary key itself
    /// (mirroring the oracle program, which does the same).
    ///
    /// The integer/float interleaving is pinned separately by
    /// [`Self::ordered_float_key_order`] (the same arithmetic key order).
    fn ordered_mixed_keys(&mut self) -> String {
        // The inserted `(key, tag)` pairs, in the oracle's insertion order. The
        // keys cover every renderer-invisible class (binary) exactly once.
        let i1 = self.atom("i1");
        let i2 = self.atom("i2");
        let i3 = self.atom("i3");
        let a1 = self.atom("a1");
        let a2 = self.atom("a2");
        let t1 = self.atom("t1");
        let t2 = self.atom("t2");
        let l1 = self.atom("l1");
        let b1 = self.atom("b1");
        let key_5 = make_small(5);
        let key_neg3 = make_small(-3);
        let key_0 = make_small(0);
        let key_abc = self.atom("abc");
        let key_xyz = self.atom("xyz");
        let key_tuple12 = self.ints(&[1, 2]);
        let atom_a = self.atom("a");
        let atom_b = self.atom("b");
        let key_tupleab = self.tuple(&[atom_a, atom_b]);
        let one = make_small(1);
        let two = make_small(2);
        let key_list12 = self.list(&[one, two]);
        let key_binary = self.binary(&[1, 2]);
        let keys: Vec<(Eterm, Eterm)> = vec![
            (key_5, i1),
            (key_neg3, i2),
            (key_0, i3),
            (key_abc, a1),
            (key_xyz, a2),
            (key_tuple12, t1),
            (key_tupleab, t2),
            (key_list12, l1),
            (key_binary, b1),
        ];

        self.ordered_key_permutation(&keys)
    }

    /// The integer/float interleaving of an `ordered_set`. `erl` orders numbers
    /// by *value* across representations (`-3 < -2.5 < 0 < 1.5 < 5`); this crate
    /// now matches it (`compare::cmp_arith`), so the case is compared strictly.
    fn ordered_float_key_order(&mut self) -> String {
        let i1 = self.atom("i1");
        let i2 = self.atom("i2");
        let i3 = self.atom("i3");
        let f1 = self.atom("f1");
        let f2 = self.atom("f2");
        let key_5 = make_small(5);
        let key_neg3 = make_small(-3);
        let key_0 = make_small(0);
        let key_float15 = self.float(1.5);
        let key_float_neg25 = self.float(-2.5);
        let keys: Vec<(Eterm, Eterm)> = vec![
            (key_5, i1),
            (key_neg3, i2),
            (key_0, i3),
            (key_float15, f1),
            (key_float_neg25, f2),
        ];

        self.ordered_key_permutation(&keys)
    }

    /// Insert `keys` (a `(key, tag)` list) into a fresh `ordered_set` and render
    /// the tag permutation: the `tab2list` order, the first/last tags, and the
    /// `next`/`prev` tag chains. Never renders a key (a float/binary key is not
    /// printable).
    fn ordered_key_permutation(&mut self, keys: &[(Eterm, Eterm)]) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, tag) in keys {
            let object = self.tuple(&[*key, *tag]);
            table.insert(object, &self.atoms).expect("insert");
        }

        // `tab2list` of an `ordered_set` is in ascending key order; render the tag
        // (element 2) of each object, never the key.
        let objects = table.tab2list();
        let order: Vec<Eterm> = objects
            .iter()
            .map(|object| {
                // SAFETY: every inserted object is a 2-tuple, so element 1 exists.
                unsafe { tuple_element(*object, 1) }
            })
            .collect();
        let idx_order = self.inorder(&order);
        // SAFETY: a non-empty `ordered_set`; every object is a 2-tuple.
        let first = self.r(unsafe { tuple_element(objects[0], 1) });
        let last_object = objects.last().expect("the table is non-empty");
        // SAFETY: as above.
        let last = self.r(unsafe { tuple_element(*last_object, 1) });

        let first_key = table.first(&self.atoms);
        let last_key = table.last(&self.atoms);
        let forward = self.walk_next(&table, first_key);
        let backward = self.walk_prev(&table, last_key);
        let forward_tags: Vec<Eterm> = forward
            .iter()
            .map(|key| self.tag_of(keys, *key))
            .collect();
        let backward_tags: Vec<Eterm> = backward
            .iter()
            .map(|key| self.tag_of(keys, *key))
            .collect();

        tuple_of(&[
            idx_order,
            first,
            last,
            self.inorder(&forward_tags),
            self.inorder(&backward_tags),
        ])
    }

    /// An insert whose key is **arithmetically equal** to a stored key but a
    /// *different term* (`{1,a}` then `{1.0,b}`) replaces the object **and** the
    /// stored **key term**, so `first`/`last`/`next` report the last-inserted key
    /// term. Floats are not renderable, so the case renders `is_float/1` of the
    /// key terms (an atom) rather than the terms themselves.
    fn ordered_replaced_key_term(&mut self) -> String {
        let one = make_small(1);
        let one_float = self.float(1.0);
        let int_then_float = self.replaced_key_terms(one, one_float);
        let float_then_int = self.replaced_key_terms(one_float, one);
        let three = {
            let mut table = self.table(TableKind::OrderedSet, 1);
            let key_two = make_small(2);
            let tags = [self.atom("a"), self.atom("b"), self.atom("c")];
            for (key, tag) in [(one, tags[0]), (one_float, tags[1]), (key_two, tags[2])] {
                let object = self.tuple(&[key, tag]);
                table.insert(object, &self.atoms).expect("insert");
            }
            let first = table.first(&self.atoms);
            let last = table.last(&self.atoms);
            let next = table
                .next(first.expect("the table is non-empty"), &self.atoms)
                .expect("ordered_set next");
            tuple_of(&[
                self.r(make_small(table.size() as i64)),
                self.is_float_key(first),
                self.is_float_key(last),
                self.opt(next),
            ])
        };
        tuple_of(&[int_then_float, float_then_int, three])
    }

    /// The 2-insert collapse: `{FirstKey, a}` then `{SecondKey, b}` (the two keys
    /// arithmetically equal but different terms), rendered as
    /// `{Size, is_float(First), is_float(Last), Next}`.
    fn replaced_key_terms(&mut self, first_key: Eterm, second_key: Eterm) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let first_tag = self.atom("a");
        let second_tag = self.atom("b");
        let first_object = self.tuple(&[first_key, first_tag]);
        table.insert(first_object, &self.atoms).expect("insert");
        let second_object = self.tuple(&[second_key, second_tag]);
        table.insert(second_object, &self.atoms).expect("insert");
        let first = table.first(&self.atoms);
        let last = table.last(&self.atoms);
        let next = table
            .next(first.expect("the table is non-empty"), &self.atoms)
            .expect("ordered_set next");
        tuple_of(&[
            self.r(make_small(table.size() as i64)),
            self.is_float_key(first),
            self.is_float_key(last),
            self.opt(next),
        ])
    }

    /// The renderable outcome of the oracle's `is_float(<key term>)` — a float key
    /// term is not printable, so the case renders the type test as an atom.
    fn is_float_key(&self, key: Option<Eterm>) -> String {
        match key {
            // SAFETY: `is_float` classifies its argument by its tag/header word.
            Some(term) if unsafe { is_float(term) } => "true".to_string(),
            _ => "false".to_string(),
        }
    }

    /// The tag of the inserted key equal to `key` (structural `==`, via
    /// `beam_term::compare::eq`).
    fn tag_of(&self, keys: &[(Eterm, Eterm)], key: Eterm) -> Eterm {
        keys.iter()
            .find(|(candidate, _)| eq(*candidate, key, &self.atoms))
            .map(|(_, tag)| *tag)
            .expect("a walked key is one of the inserted keys")
    }
}

/// The `ordered_set` navigation cases, in the oracle's order.
pub(super) fn navigation_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push(("ordered_iteration".into(), battery.ordered_iteration()));
    cases.push((
        "ordered_absent_bounds".into(),
        battery.ordered_absent_bounds(),
    ));
    cases.push(("ordered_empty_nav".into(), battery.ordered_empty_nav()));
    cases.push((
        "ordered_mixed_keys".into(),
        battery.ordered_mixed_keys(),
    ));
    cases.push((
        "ordered_float_key_order".into(),
        battery.ordered_float_key_order(),
    ));
    cases.push((
        "ordered_replaced_key_term".into(),
        battery.ordered_replaced_key_term(),
    ));

    cases
}
