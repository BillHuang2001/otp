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

//! The **match-spec MAP** case group: head maps, the map guards and map bodies.
//! This is the observable `ets:*` side of contract A.1 (term/key/value semantics)
//! for maps inside match specifications, cross-checked against the system `erl`.
//!
//! The head-map rule the C `db_match_compile`/`dmc_expr` enforces (and this crate
//! reproduces — `src/select/pattern.rs`) is the point of the group:
//!
//! * a head map's **keys are literal** — a variable (`'$N'`) or `'_'` key is
//!   `badarg` at the **top level of a key**, at every nesting depth reachable
//!   through a map *value* (`#{{…} => #{'$1' => …}}` is `badarg` too). Nested
//!   inside a **compound key** (a tuple/list key) a variable/`'_'` is instead an
//!   ordinary **literal**, so the spec compiles and the key simply fails to match;
//! * a head map's **values match structurally** (wildcard/variable/literal), so
//!   `#{a => '_'}` matches any value and `#{a => '$2'}` binds it;
//! * an object map may carry **extra keys** (`matchMap` only requires every
//!   pattern key to be present), and the empty map `#{}` matches any map;
//! * keys compare **exactly** (`1` and `1.0` are distinct keys), and non-atom keys
//!   (integer, float, tuple, binary, nested map) are legal literals;
//! * a map in a **guard or body** is *constructed* at evaluation time (its
//!   keys/values may be variables) and read back with `map_get`/`map_size`/
//!   `is_map_key`; `map_get/2` on an absent key and `map_size/1`/`is_map_key/2` on
//!   a non-map raise `badarg` (a guard's `badarg` skips the clause, a body's is
//!   the `EXIT` sentinel);
//! * a `select_replace/2` body that is a **bare map** is refused by the C's static
//!   body check (`badarg` for the whole call, table unchanged).
//!
//! Every case agrees with the system `erl` (OTP 29.1.1 / erts-17.1); the
//! expectations in `../differential.rs` were derived from it, not written by hand.

use super::*;

impl Battery {
    /// One `{'$1', #{Key => '$2'}}` select (body `['$2']`) over `table` — the
    /// shared shape of the non-atom-key-shape cases. `Key` is an arbitrary literal
    /// key the head map carries.
    fn cap_map_key_select(&mut self, table: &Table, key: Eterm) -> String {
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head_map = self.map(&[(key, v2)]);
        let head = self.tuple(&[v1, head_map]);
        let body = self.list(&[v2]);
        let spec_term = self.spec(head, NIL, body);
        self.cap_select(table, spec_term)
    }

    /// A head map with a **literal atom key** matches only the object whose value
    /// map holds `a`, and the key-only pattern ignores that object's other keys.
    fn map_literal_atom_key(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let b = self.atom("b");
        let with_a = self.map(&[(a, make_small(10))]);
        let first = self.tuple(&[make_small(1), with_a]);
        let with_b = self.map(&[(b, make_small(20))]);
        let second = self.tuple(&[make_small(2), with_b]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head_map = self.map(&[(a, v2)]);
        let head = self.tuple(&[v1, head_map]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, NIL, body);
        self.cap_select(&table, spec_term)
    }

    /// A wildcard **value** in a head map matches any value for that key.
    fn map_wildcard_value(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let b = self.atom("b");
        let wildcard = self.atom("_");
        let with_a = self.map(&[(a, make_small(10))]);
        let first = self.tuple(&[make_small(1), with_a]);
        let with_b = self.map(&[(b, make_small(20))]);
        let second = self.tuple(&[make_small(2), with_b]);
        let both = self.map(&[(a, make_small(30)), (b, make_small(40))]);
        let third = self.tuple(&[make_small(3), both]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        table.insert(third, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let head_map = self.map(&[(a, wildcard)]);
        let head = self.tuple(&[v1, head_map]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, NIL, body);
        self.cap_select(&table, spec_term)
    }

    /// An object with **extra keys** still matches: the head map only requires its
    /// own keys to be present, and the variable value binds.
    fn map_extra_keys_match(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let b = self.atom("b");
        let c = self.atom("c");
        let three = self.map(&[(a, make_small(1)), (b, make_small(2)), (c, make_small(3))]);
        let first = self.tuple(&[make_small(1), three]);
        let only_b = self.map(&[(b, make_small(2))]);
        let second = self.tuple(&[make_small(2), only_b]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head_map = self.map(&[(a, v2)]);
        let head = self.tuple(&[v1, head_map]);
        let body = self.list(&[v2]);
        let spec_term = self.spec(head, NIL, body);
        self.cap_select(&table, spec_term)
    }

    /// The **empty map** `#{}` matches any map (and only a map): a non-map value is
    /// not matched.
    fn map_empty_pattern(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let empty = self.map(&[]);
        let first = self.tuple(&[make_small(1), empty]);
        let a = self.atom("a");
        let with_a = self.map(&[(a, make_small(1))]);
        let second = self.tuple(&[make_small(2), with_a]);
        let third = self.tuple(&[make_small(3), make_small(5)]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        table.insert(third, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let head_map = self.map(&[]);
        let head = self.tuple(&[v1, head_map]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, NIL, body);
        self.cap_select(&table, spec_term)
    }

    /// **Non-atom keys** are legal literals: an integer, a float, a tuple, a binary
    /// and a nested-map key each select exactly the object carrying it.
    fn map_nonatom_keys(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let int_key = self.map(&[(make_small(1), make_small(10))]);
        let first = self.tuple(&[make_small(1), int_key]);
        let one_float = self.float(1.0);
        let float_key = self.map(&[(one_float, make_small(20))]);
        let second = self.tuple(&[make_small(2), float_key]);
        let pair_key = self.tuple(&[make_small(1), make_small(2)]);
        let tuple_key = self.map(&[(pair_key, make_small(30))]);
        let third = self.tuple(&[make_small(3), tuple_key]);
        let bin_key = self.binary(b"ab");
        let binary_key = self.map(&[(bin_key, make_small(40))]);
        let fourth = self.tuple(&[make_small(4), binary_key]);
        let x = self.atom("x");
        let inner = self.map(&[(x, make_small(1))]);
        let nested_key = self.map(&[(inner, make_small(50))]);
        let fifth = self.tuple(&[make_small(5), nested_key]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        table.insert(third, &self.atoms).expect("insert");
        table.insert(fourth, &self.atoms).expect("insert");
        table.insert(fifth, &self.atoms).expect("insert");

        let int_select = self.cap_map_key_select(&table, make_small(1));
        let float_select = self.cap_map_key_select(&table, one_float);
        let tuple_select = self.cap_map_key_select(&table, pair_key);
        let binary_select = self.cap_map_key_select(&table, bin_key);
        let nested_select = self.cap_map_key_select(&table, inner);
        tuple_of(&[
            int_select,
            float_select,
            tuple_select,
            binary_select,
            nested_select,
        ])
    }

    /// `1` and `1.0` are **distinct map keys** (exact `=:=` comparison), so each
    /// head map selects exactly its own object.
    fn map_int_vs_float_keys_distinct(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let int_atom = self.atom("int");
        let int_key = self.map(&[(make_small(1), int_atom)]);
        let first = self.tuple(&[make_small(1), int_key]);
        let float_atom = self.atom("afloat");
        let one_float = self.float(1.0);
        let float_key = self.map(&[(one_float, float_atom)]);
        let second = self.tuple(&[make_small(2), float_key]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let int_select = self.cap_map_key_select(&table, make_small(1));
        let float_select = self.cap_map_key_select(&table, one_float);
        tuple_of(&[int_select, float_select])
    }

    /// The head-map **key rule**: a variable/`'_'` key is `badarg` when it is a key
    /// of the map itself, but a variable/`'_'` **inside a compound (tuple) key** is
    /// an ordinary literal — the spec compiles and simply fails to match the
    /// object's `{1,2}` key.
    fn map_nonliteral_key_badarg(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let only_a = self.map(&[(a, make_small(1))]);
        let first = self.tuple(&[make_small(1), only_a]);
        let pair_key = self.tuple(&[make_small(1), make_small(2)]);
        let pair_map = self.map(&[(pair_key, make_small(20))]);
        let second = self.tuple(&[make_small(2), pair_map]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let wildcard = self.atom("_");
        let body = self.list(&[v1]);

        // `#{'$2' => '$3'}` — a variable key: `badarg`.
        let variable_key = self.map(&[(v2, v3)]);
        let head1 = self.tuple(&[v1, variable_key]);
        let direct_spec = self.spec(head1, NIL, body);
        let direct = self.cap_select(&table, direct_spec);

        // `#{'_' => '$3'}` — an underscore key: `badarg`.
        let wildcard_key = self.map(&[(wildcard, v3)]);
        let head2 = self.tuple(&[v1, wildcard_key]);
        let underscore_spec = self.spec(head2, NIL, body);
        let underscore = self.cap_select(&table, underscore_spec);

        // `#{{1,'$2'} => '$3'}` — a variable nested inside a *compound* key: legal,
        // but literal, so the object's tuple key `{1,2}` is not matched.
        let inner_key = self.tuple(&[make_small(1), v2]);
        let compound_key = self.map(&[(inner_key, v3)]);
        let head3 = self.tuple(&[v1, compound_key]);
        let nested_spec = self.spec(head3, NIL, body);
        let nested = self.cap_select(&table, nested_spec);

        tuple_of(&[direct, underscore, nested])
    }

    /// Maps nested inside **tuples** and **lists** in a head pattern recurse
    /// structurally.
    fn map_nested_in_containers(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let k = self.atom("k");
        let inside_map = self.map(&[(k, make_small(10))]);
        let tuple_value = self.tuple(&[a, inside_map]);
        let first = self.tuple(&[make_small(1), tuple_value]);
        let b = self.atom("b");
        let list_map = self.map(&[(k, make_small(20))]);
        let list_value = self.list(&[b, list_map]);
        let second = self.tuple(&[make_small(2), list_value]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");

        let nested_map = self.map(&[(k, v3)]);
        let tuple_inner = self.tuple(&[v2, nested_map]);
        let tuple_head = self.tuple(&[v1, tuple_inner]);
        let tuple_body = self.list(&[v3]);
        let tuple_spec = self.spec(tuple_head, NIL, tuple_body);
        let tuple_select = self.cap_select(&table, tuple_spec);

        let list_map = self.map(&[(k, v3)]);
        let list_inner = self.list(&[v2, list_map]);
        let list_head = self.tuple(&[v1, list_inner]);
        let list_body = self.list(&[v3]);
        let list_spec = self.spec(list_head, NIL, list_body);
        let list_select = self.cap_select(&table, list_spec);

        tuple_of(&[tuple_select, list_select])
    }

    /// A map **value** that is itself a compound pattern is matched structurally —
    /// a tuple value binds its elements.
    fn map_value_matched_structurally(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let x = self.atom("x");
        let pair_x = self.tuple(&[x, make_small(1)]);
        let first_map = self.map(&[(a, pair_x)]);
        let first = self.tuple(&[make_small(1), first_map]);
        let y = self.atom("y");
        let pair_y = self.tuple(&[y, make_small(2)]);
        let second_map = self.map(&[(a, pair_y)]);
        let second = self.tuple(&[make_small(2), second_map]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let value = self.tuple(&[v2, v3]);
        let head_map = self.map(&[(a, value)]);
        let head = self.tuple(&[v1, head_map]);
        // `{{ '$2', '$3' }}` — the `{{ … }}` tuple-construction body form.
        let inner = self.tuple(&[v2, v3]);
        let construction = self.tuple(&[inner]);
        let body = self.list(&[construction]);
        let spec_term = self.spec(head, NIL, body);
        self.cap_select(&table, spec_term)
    }

    /// A pattern key that no object carries selects nothing — an **absent key** is
    /// a non-match, not an error.
    fn map_absent_key_empty(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let b = self.atom("b");
        let c = self.atom("c");
        let a = self.atom("a");
        let with_b = self.map(&[(b, make_small(1))]);
        let first = self.tuple(&[make_small(1), with_b]);
        let with_c = self.map(&[(c, make_small(2))]);
        let second = self.tuple(&[make_small(2), with_c]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head_map = self.map(&[(a, v2)]);
        let head = self.tuple(&[v1, head_map]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, NIL, body);
        self.cap_select(&table, spec_term)
    }

    /// A **HAMT** head map (> 32 keys, so the pattern is a hash map): all 35
    /// pattern keys must be present, so only the 40-key object matches, not the
    /// object that lacks key `1`.
    fn map_large_head_map(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let obj1_pairs: Vec<(Eterm, Eterm)> = (1..=40i64)
            .map(|key| (make_small(key), make_small(key * 10)))
            .collect();
        let obj1 = self.map(&obj1_pairs);
        let first = self.tuple(&[make_small(1), obj1]);
        let obj2_pairs: Vec<(Eterm, Eterm)> = (2..=41i64)
            .map(|key| (make_small(key), make_small(key * 10)))
            .collect();
        let obj2 = self.map(&obj2_pairs);
        let second = self.tuple(&[make_small(2), obj2]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let wildcard = self.atom("_");
        let head_pairs: Vec<(Eterm, Eterm)> =
            (1..=35i64).map(|key| (make_small(key), wildcard)).collect();
        let head_map = self.map(&head_pairs);
        let v1 = self.atom("$1");
        let head = self.tuple(&[v1, head_map]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, NIL, body);
        self.cap_select(&table, spec_term)
    }

    /// A map **constructed in a guard** from the bound variables and then read back
    /// with `map_size/1` and `map_get/2` — its keys/values may be variables.
    fn map_construct_guard(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        table
            .insert(self.object(1, 10), &self.atoms)
            .expect("insert");
        table
            .insert(self.object(2, 20), &self.atoms)
            .expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let built = self.map(&[(v1, v2)]);
        let andalso = self.atom("andalso");
        let arith_eq = self.atom("==");
        let map_size = self.atom("map_size");
        let size_call = self.tuple(&[map_size, built]);
        let size_guard = self.tuple(&[arith_eq, size_call, make_small(1)]);
        let map_get = self.atom("map_get");
        let get_call = self.tuple(&[map_get, v1, built]);
        let get_guard = self.tuple(&[arith_eq, get_call, v2]);
        let guard = self.tuple(&[andalso, size_guard, get_guard]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let head = self.tuple(&[v1, v2]);
        let spec_term = self.spec(head, guards, body);
        self.cap_select(&table, spec_term)
    }

    /// A **nested map constructed in a body** and read back by two chained
    /// `map_get/2` calls.
    fn map_construct_body_nested(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        table
            .insert(self.object(1, 10), &self.atoms)
            .expect("insert");
        table
            .insert(self.object(2, 20), &self.atoms)
            .expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let k = self.atom("k");
        let inner_map = self.map(&[(k, v2)]);
        let outer_map = self.map(&[(k, inner_map)]);
        let map_get = self.atom("map_get");
        let inner_call = self.tuple(&[map_get, k, outer_map]);
        let outer_call = self.tuple(&[map_get, k, inner_call]);
        let body = self.list(&[outer_call]);
        let head = self.tuple(&[v1, v2]);
        let spec_term = self.spec(head, NIL, body);
        self.cap_select(&table, spec_term)
    }

    /// `map_get/2` on an **absent key** raises `badarg`: as a guard it skips the
    /// clause (`[]`), as a body it yields the `EXIT` sentinel.
    fn map_get_absent_badarg(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let b = self.atom("b");
        let with_b = self.map(&[(b, make_small(1))]);
        let object = self.tuple(&[make_small(1), with_b]);
        table.insert(object, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let a = self.atom("a");
        let arith_eq = self.atom("==");
        let map_get = self.atom("map_get");
        let call = self.tuple(&[map_get, a, v2]);
        let guard = self.tuple(&[arith_eq, call, make_small(0)]);
        let guards = self.list(&[guard]);
        let guard_body = self.list(&[v1]);
        let guard_spec = self.spec(head, guards, guard_body);
        let guard_select = self.cap_select(&table, guard_spec);

        let body_call = self.tuple(&[map_get, a, v2]);
        let body = self.list(&[body_call]);
        let body_spec = self.spec(head, NIL, body);
        let body_select = self.cap_select(&table, body_spec);

        tuple_of(&[guard_select, body_select])
    }

    /// `map_size/1` and `is_map_key/2` on a **non-map** raise `badarg`, so the
    /// clause over the integer object is skipped and only the map object matches.
    fn map_size_is_map_key_on_nonmap(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        table
            .insert(self.object(1, 10), &self.atoms)
            .expect("insert");
        let a = self.atom("a");
        let with_a = self.map(&[(a, make_small(1))]);
        let second = self.tuple(&[make_small(2), with_a]);
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let body = self.list(&[v1]);

        let arith_eq = self.atom("==");
        let map_size = self.atom("map_size");
        let size_call = self.tuple(&[map_size, v2]);
        let size_guard = self.tuple(&[arith_eq, size_call, make_small(1)]);
        let size_guards = self.list(&[size_guard]);
        let size_spec = self.spec(head, size_guards, body);
        let size_select = self.cap_select(&table, size_spec);

        let is_map_key = self.atom("is_map_key");
        let key_guard = self.tuple(&[is_map_key, a, v2]);
        let key_guards = self.list(&[key_guard]);
        let key_spec = self.spec(head, key_guards, body);
        let key_select = self.cap_select(&table, key_spec);

        tuple_of(&[size_select, key_select])
    }

    /// A `select_replace/2` body that is a **bare map** is refused by the C's
    /// static body check: the whole call is `badarg` and the table is unchanged.
    fn select_replace_bare_map_body(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        table
            .insert(self.object(1, 10), &self.atoms)
            .expect("insert");
        table
            .insert(self.object(2, 20), &self.atoms)
            .expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let map_body = self.map(&[(v1, v2)]);
        let body = self.list(&[map_body]);
        let spec_term = self.spec(head, NIL, body);

        let spec = compile(spec_term, &self.atoms).expect("the spec compiles");
        let count = cap_count(table.select_replace(&spec, &self.atoms, &mut self.out));
        let objects = table.tab2list();
        tuple_of(&[count, self.srt(&objects)])
    }
}

/// The match-spec **map** cases, in the oracle's order.
pub(super) fn maps_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push((
        "map_literal_atom_key".into(),
        battery.map_literal_atom_key(),
    ));
    cases.push(("map_wildcard_value".into(), battery.map_wildcard_value()));
    cases.push((
        "map_extra_keys_match".into(),
        battery.map_extra_keys_match(),
    ));
    cases.push(("map_empty_pattern".into(), battery.map_empty_pattern()));
    cases.push(("map_nonatom_keys".into(), battery.map_nonatom_keys()));
    cases.push((
        "map_int_vs_float_keys_distinct".into(),
        battery.map_int_vs_float_keys_distinct(),
    ));
    cases.push((
        "map_nonliteral_key_badarg".into(),
        battery.map_nonliteral_key_badarg(),
    ));
    cases.push((
        "map_nested_in_containers".into(),
        battery.map_nested_in_containers(),
    ));
    cases.push((
        "map_value_matched_structurally".into(),
        battery.map_value_matched_structurally(),
    ));
    cases.push((
        "map_absent_key_empty".into(),
        battery.map_absent_key_empty(),
    ));
    cases.push(("map_large_head_map".into(), battery.map_large_head_map()));
    cases.push(("map_construct_guard".into(), battery.map_construct_guard()));
    cases.push((
        "map_construct_body_nested".into(),
        battery.map_construct_body_nested(),
    ));
    cases.push((
        "map_get_absent_badarg".into(),
        battery.map_get_absent_badarg(),
    ));
    cases.push((
        "map_size_is_map_key_on_nonmap".into(),
        battery.map_size_is_map_key_on_nonmap(),
    ));
    cases.push((
        "select_replace_bare_map_body".into(),
        battery.select_replace_bare_map_body(),
    ));

    cases
}
