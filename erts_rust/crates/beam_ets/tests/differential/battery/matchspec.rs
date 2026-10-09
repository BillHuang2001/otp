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

//! The **`select` / `match`** case group: `select/2` with variables, wildcards,
//! literals and guards (comparison, type test, `andalso`/`orelse`) and bodies
//! (`'$_'`, `'$N'`, constants, `{{…}}` construction); `select_delete/2` +
//! `select_count/2`; the `match` family; malformed specs (`badarg`);
//! `ets:match_spec_run/2` (bindings/whole-element/all-bindings bodies, a repeated
//! head variable, multi-clause first-match-wins, an empty list and a non-list
//! `badarg`); and the body-error `EXIT` semantics.

use super::*;
use beam_ets::match_spec_run;

impl Battery {
    fn select_head_vars(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, value) in [(1i64, 10i64), (2, 20)] {
            let object = self.object(key, value);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let body = self.list(&[v2]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_wildcard_literal(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20), (3, "c", 30)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let wildcard = self.atom("_");
        let head = self.tuple(&[make_small(1), wildcard, wildcard]);
        let whole = self.atom("$_");
        let body = self.list(&[whole]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_guard_comparison(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20), (3, "c", 30)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let gt = self.atom(">");
        let guard = self.tuple(&[gt, v3, make_small(15)]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_guard_typetest(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let first = self.tuple(&[make_small(1), a]);
        let second = self.tuple(&[make_small(2), make_small(7)]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let is_integer = self.atom("is_integer");
        let guard = self.tuple(&[is_integer, v2]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_guard_andalso_orelse(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20), (3, "c", 30)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let body = self.list(&[v1]);

        let andalso = self.atom("andalso");
        let gt = self.atom(">");
        let lt = self.atom("<");
        let lower = self.tuple(&[gt, v3, make_small(5)]);
        let upper = self.tuple(&[lt, v3, make_small(25)]);
        let conjunction = self.tuple(&[andalso, lower, upper]);
        let conjunction = self.list(&[conjunction]);
        let s1 = self.spec(head, conjunction, body);

        let orelse = self.atom("orelse");
        let exact = self.atom("=:=");
        let one = self.tuple(&[exact, v1, make_small(1)]);
        let three = self.tuple(&[exact, v1, make_small(3)]);
        let disjunction = self.tuple(&[orelse, one, three]);
        let disjunction = self.list(&[disjunction]);
        let s2 = self.spec(head, disjunction, body);

        let first = self.select(&table, s1);
        let second = self.select(&table, s2);
        tuple_of(&[self.inorder(&first), self.inorder(&second)])
    }

    /// `{'==', {'/', '$2', 2}, 10.0}` — the `/` operator in a guard.
    ///
    /// `/` always yields a **float** in Erlang, and `DefaultArithHook` refuses it
    /// (`unsupported_ets`) rather than guessing a small integer, so `beam_ets`
    /// propagates the typed error instead of silently under-matching — rendered by
    /// [`Battery::cap_select`] as `error:unsupported_ets: …`. A **recorded
    /// divergence** (`tests/differential.rs::KNOWN_DIVERGENCES`).
    fn select_guard_div(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2, 3] {
            let object = self.object(key, key * 10);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let arith_eq = self.atom("==");
        let div = self.atom("/");
        let half = self.tuple(&[div, v2, make_small(2)]);
        let ten = self.float(10.0);
        let guard = self.tuple(&[arith_eq, half, ten]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        self.cap_select(&table, spec_term)
    }

    /// `{'==', {float, '$2'}, 20.0}` — `float/1` in a guard, over a small integer
    /// and over a float value (both round to the same `f64` the guard compares
    /// against).
    fn select_guard_float(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let first = self.ints(&[1, 10]);
        let value = self.float(20.0);
        let second = self.tuple(&[make_small(2), value]);
        let third = self.ints(&[3, 30]);
        for object in [first, second, third] {
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let arith_eq = self.atom("==");
        let float_fn = self.atom("float");
        let call = self.tuple(&[float_fn, v2]);
        let twenty = self.float(20.0);
        let guard = self.tuple(&[arith_eq, call, twenty]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        self.cap_select(&table, spec_term)
    }

    /// `{'>', {element, 3, '$_'}, 15}` — `element/2` applied to the whole object
    /// (`'$_'` is an ordinary expression outside a head).
    fn select_guard_element(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20), (3, "c", 30)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let gt = self.atom(">");
        let element = self.atom("element");
        let whole = self.atom("$_");
        let call = self.tuple(&[element, make_small(3), whole]);
        let guard = self.tuple(&[gt, call, make_small(15)]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        self.cap_select(&table, spec_term)
    }

    /// `abs/1` in a guard, over a small integer (`{'>', {abs,'$2'}, 15}`) and over
    /// a float (`{'==', {abs,'$2'}, 10.0}`).
    fn select_guard_abs(&mut self) -> String {
        let mut integer_table = self.table(TableKind::OrderedSet, 1);
        for (key, value) in [(1i64, -10i64), (2, 20), (3, -30)] {
            let object = self.object(key, value);
            integer_table.insert(object, &self.atoms).expect("insert");
        }
        let mut float_table = self.table(TableKind::OrderedSet, 1);
        let negative = self.float(-10.0);
        let first = self.tuple(&[make_small(1), negative]);
        let positive = self.float(20.0);
        let second = self.tuple(&[make_small(2), positive]);
        float_table.insert(first, &self.atoms).expect("insert");
        float_table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let abs = self.atom("abs");
        let call = self.tuple(&[abs, v2]);
        let body = self.list(&[v1]);

        let gt = self.atom(">");
        let int_guard = self.tuple(&[gt, call, make_small(15)]);
        let int_guards = self.list(&[int_guard]);
        let int_spec = self.spec(head, int_guards, body);

        let arith_eq = self.atom("==");
        let ten = self.float(10.0);
        let float_guard = self.tuple(&[arith_eq, call, ten]);
        let float_guards = self.list(&[float_guard]);
        let float_spec = self.spec(head, float_guards, body);

        let ints = self.cap_select(&integer_table, int_spec);
        let floats = self.cap_select(&float_table, float_spec);
        tuple_of(&[ints, floats])
    }

    /// `{'==', {map_size, '$2'}, 2}` — `map_size/1` over stored flat maps.
    fn select_guard_map_size(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let key_a = self.atom("a");
        let key_b = self.atom("b");
        let two_keys = self.map(&[(key_a, make_small(1)), (key_b, make_small(2))]);
        let first = self.tuple(&[make_small(1), two_keys]);
        let one_key = self.map(&[(key_a, make_small(1))]);
        let second = self.tuple(&[make_small(2), one_key]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let arith_eq = self.atom("==");
        let map_size = self.atom("map_size");
        let call = self.tuple(&[map_size, v2]);
        let guard = self.tuple(&[arith_eq, call, make_small(2)]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        self.cap_select(&table, spec_term)
    }

    /// `{is_map_key, a, '$2'}` — the key test, `true` only for the map holding `a`.
    fn select_guard_is_map_key(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let key_a = self.atom("a");
        let key_b = self.atom("b");
        let with_a = self.map(&[(key_a, make_small(10))]);
        let first = self.tuple(&[make_small(1), with_a]);
        let with_b = self.map(&[(key_b, make_small(20))]);
        let second = self.tuple(&[make_small(2), with_b]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let is_map_key = self.atom("is_map_key");
        let guard = self.tuple(&[is_map_key, key_a, v2]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        self.cap_select(&table, spec_term)
    }

    /// `{'==', {map_get, a, '$2'}, 10}` — `map_get/2` with a present key, and with
    /// an absent key (which raises `badarg` ⇒ the clause is skipped).
    fn select_guard_map_get(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let key_a = self.atom("a");
        let key_b = self.atom("b");
        let with_a = self.map(&[(key_a, make_small(10))]);
        let first = self.tuple(&[make_small(1), with_a]);
        let with_b = self.map(&[(key_b, make_small(20))]);
        let second = self.tuple(&[make_small(2), with_b]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let arith_eq = self.atom("==");
        let map_get = self.atom("map_get");
        let call = self.tuple(&[map_get, key_a, v2]);
        let guard = self.tuple(&[arith_eq, call, make_small(10)]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        self.cap_select(&table, spec_term)
    }

    /// `{'==', {tuple_size, '$2'}, 3}` — `tuple_size/1` over stored tuples.
    fn select_guard_tuple_size(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let b = self.atom("b");
        let c = self.atom("c");
        let triple = self.tuple(&[a, b, c]);
        let first = self.tuple(&[make_small(1), triple]);
        let pair = self.tuple(&[a, b]);
        let second = self.tuple(&[make_small(2), pair]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let arith_eq = self.atom("==");
        let tuple_size = self.atom("tuple_size");
        let call = self.tuple(&[tuple_size, v2]);
        let guard = self.tuple(&[arith_eq, call, make_small(3)]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        self.cap_select(&table, spec_term)
    }

    /// `{'andalso', {'==', {byte_size,'$2'}, 3}, {'==', {bit_size,'$2'}, 24}}` —
    /// the two bitstring size guards over stored binaries.
    fn select_guard_size_family(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let three = self.binary(b"abc");
        let first = self.tuple(&[make_small(1), three]);
        let two = self.binary(b"ab");
        let second = self.tuple(&[make_small(2), two]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");

        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let arith_eq = self.atom("==");
        let byte_size = self.atom("byte_size");
        let byte_call = self.tuple(&[byte_size, v2]);
        let byte_guard = self.tuple(&[arith_eq, byte_call, make_small(3)]);
        let bit_size = self.atom("bit_size");
        let bit_call = self.tuple(&[bit_size, v2]);
        let bit_guard = self.tuple(&[arith_eq, bit_call, make_small(24)]);
        let andalso = self.atom("andalso");
        let conjunction = self.tuple(&[andalso, byte_guard, bit_guard]);
        let guards = self.list(&[conjunction]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        self.cap_select(&table, spec_term)
    }

    /// `{'==', {'*', '$2', 1.5}, 30.0}` — match-spec arithmetic with a **float**
    /// operand, which `DefaultArithHook` refuses (`unsupported_ets`) rather than
    /// guessing. A **recorded divergence**
    /// (`tests/differential.rs::KNOWN_DIVERGENCES`).
    fn select_guard_float_arith(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2] {
            let object = self.object(key, key * 10);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let arith_eq = self.atom("==");
        let star = self.atom("*");
        let factor = self.float(1.5);
        let product = self.tuple(&[star, v2, factor]);
        let thirty = self.float(30.0);
        let guard = self.tuple(&[arith_eq, product, thirty]);
        let guards = self.list(&[guard]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, guards, body);
        self.cap_select(&table, spec_term)
    }

    fn select_body_whole(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let whole = self.atom("$_");
        let body = self.list(&[whole]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_body_var(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let body = self.list(&[v1]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_body_const(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for (key, letter, value) in [(1i64, "a", 10i64), (2, "b", 20)] {
            let letter = self.atom(letter);
            let object = self.tuple(&[make_small(key), letter, make_small(value)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let ok = self.atom("ok");
        let body = self.list(&[ok]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    fn select_body_construct(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let a = self.atom("a");
        let b = self.atom("b");
        let first = self.tuple(&[make_small(1), a]);
        let second = self.tuple(&[make_small(2), b]);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        // The `{{ … }}` tuple-construction form: a one-element tuple whose
        // element is itself a tuple template.
        let inner = self.tuple(&[v1, v2]);
        let construction = self.tuple(&[inner]);
        let body = self.list(&[construction]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    /// A map **constructed in a body** from the bound variables
    /// (`#{'$1' => '$2'}`), read back with `map_get/2` so the clause value stays a
    /// small integer — the canonical renderer has no map clause.
    fn select_body_map_construct(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        let first = self.object(1, 10);
        let second = self.object(2, 20);
        table.insert(first, &self.atoms).expect("insert");
        table.insert(second, &self.atoms).expect("insert");
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let built = self.map(&[(v1, v2)]);
        let map_get = self.atom("map_get");
        let call = self.tuple(&[map_get, v1, built]);
        let body = self.list(&[call]);
        let spec_term = self.spec(head, NIL, body);
        self.cap_select(&table, spec_term)
    }
    fn select_delete_and_count(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2, 3] {
            let a = self.atom("a");
            let object = self.tuple(&[make_small(key), a, make_small(key * 10)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let head = self.tuple(&[v1, v2, v3]);
        let gt = self.atom(">");
        let guard = self.tuple(&[gt, v3, make_small(15)]);
        let guards = self.list(&[guard]);
        let truth = self.atom("true");
        let falsity = self.atom("false");
        let true_body = self.list(&[truth]);
        let false_body = self.list(&[falsity]);

        let false_spec_term = self.spec(head, guards, false_body);
        let count0 = {
            let spec = compile(false_spec_term, &self.atoms).expect("spec compiles");
            table
                .select_count(&spec, &self.atoms, &mut self.out)
                .expect("select_count")
        };
        let delete0 = {
            let spec = compile(false_spec_term, &self.atoms).expect("spec compiles");
            table
                .select_delete(&spec, &self.atoms, &mut self.out)
                .expect("select_delete")
        };
        let true_spec_term = self.spec(head, guards, true_body);
        let count1 = {
            let spec = compile(true_spec_term, &self.atoms).expect("spec compiles");
            table
                .select_count(&spec, &self.atoms, &mut self.out)
                .expect("select_count")
        };
        let delete1 = {
            let spec = compile(true_spec_term, &self.atoms).expect("spec compiles");
            table
                .select_delete(&spec, &self.atoms, &mut self.out)
                .expect("select_delete")
        };
        let remaining: Vec<Eterm> = table
            .tab2list()
            .into_iter()
            .map(|object| {
                // SAFETY: every inserted object is a 3-tuple, so element 0 exists.
                unsafe { tuple_element(object, 0) }
            })
            .collect();
        tuple_of(&[
            count0.to_string(),
            delete0.to_string(),
            count1.to_string(),
            delete1.to_string(),
            table.size().to_string(),
            self.inorder(&remaining),
        ])
    }

    fn match_family(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2, 3] {
            let a = self.atom("a");
            let object = self.tuple(&[make_small(key), a, make_small(key * 10)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let wildcard = self.atom("_");
        let v3 = self.atom("$3");
        let pattern = self.tuple(&[v1, wildcard, v3]);

        let objects = table
            .match_object(pattern, &self.atoms, &mut self.out)
            .expect("match_object");
        let matches = table
            .match_(pattern, &self.atoms, &mut self.out)
            .expect("match/2");
        let before = table.size();
        let middle = self.atom("_");
        let delete_pattern = self.tuple(&[make_small(1), middle, middle]);
        let deleted = table
            .match_delete(delete_pattern, &self.atoms, &mut self.out)
            .expect("match_delete");
        let after = table.size();
        // `ets:match_delete/2` answers `true` (the atom), while `Table::match_delete`
        // returns the deletion count. Both observe the *same* deletion, so we render
        // the ETS-visible result (`true`) and separately check the count against the
        // size drop; `before`/`after`/`remaining` below pin exactly what was deleted.
        assert_eq!(deleted, before - after, "match_delete count == size drop");
        assert_eq!(deleted, 1, "the `{{1,'_','_'}}` pattern matches exactly one object");
        let remaining: Vec<Eterm> = table
            .tab2list()
            .into_iter()
            .map(|object| {
                // SAFETY: every inserted object is a 3-tuple, so element 0 exists.
                unsafe { tuple_element(object, 0) }
            })
            .collect();
        tuple_of(&[
            self.inorder(&objects),
            self.inorder(&matches),
            "true".to_string(),
            before.to_string(),
            after.to_string(),
            self.inorder(&remaining),
        ])
    }

    fn malformed_specs(&mut self) -> String {
        // Each entry is a *term*, exactly as the oracle builds it.
        let not_a_list = make_small(1);
        let list_of_atom = {
            let a_list = self.atom("a_list");
            self.list(&[a_list])
        };
        let wildcard = self.atom("_");
        let two_tuple_clause = {
            let empty = NIL;
            let clause = self.tuple(&[wildcard, empty]);
            self.list(&[clause])
        };
        let empty_body = {
            let empty = NIL;
            let clause = self.tuple(&[wildcard, empty, empty]);
            self.list(&[clause])
        };
        let unknown_guard = {
            let v1 = self.atom("$1");
            let head = self.tuple(&[v1]);
            let is_foo = self.atom("is_foo");
            let guard = self.tuple(&[is_foo, v1]);
            let guards = self.list(&[guard]);
            let whole = self.atom("$_");
            let body = self.list(&[whole]);
            let clause = self.tuple(&[head, guards, body]);
            self.list(&[clause])
        };
        let unbound_body_var = {
            let v2 = self.atom("$2");
            let body = self.list(&[v2]);
            let clause = self.tuple(&[wildcard, NIL, body]);
            self.list(&[clause])
        };
        let unbound_guard_var = {
            let v1 = self.atom("$1");
            let is_integer = self.atom("is_integer");
            let guard = self.tuple(&[is_integer, v1]);
            let guards = self.list(&[guard]);
            let whole = self.atom("$_");
            let body = self.list(&[whole]);
            let clause = self.tuple(&[wildcard, guards, body]);
            self.list(&[clause])
        };
        let well_formed = {
            let whole = self.atom("$_");
            let body = self.list(&[whole]);
            let clause = self.tuple(&[wildcard, NIL, body]);
            self.list(&[clause])
        };

        let mut rendered = Vec::new();
        let mut table = self.table(TableKind::Set, 1);
        let object = self.object(1, 10);
        table.insert(object, &self.atoms).expect("insert");
        for spec in [
            not_a_list,
            list_of_atom,
            two_tuple_clause,
            empty_body,
            unknown_guard,
            unbound_body_var,
            unbound_guard_var,
            well_formed,
        ] {
            rendered.push(self.cap_select(&table, spec));
        }
        list_of(&rendered)
    }

    /// `ets:match_spec_run(List, ets:match_spec_compile([{head, [], body}]))` — the
    /// oracle requires the spec already compiled by `ets:match_spec_compile/1`,
    /// while this crate's [`match_spec_run`] takes the **raw** spec term.
    fn run_on_list(&mut self, list: Eterm, head: Eterm, body: Eterm) -> Vec<Eterm> {
        let spec = self.spec(head, NIL, body);
        match_spec_run(list, spec, &self.atoms, &mut self.out).expect("match_spec_run")
    }

    /// `ets:match_spec_run/2` with the three body forms: `['$1']` (a bound
    /// variable), `['$_']` (the whole element) and `['$$']` (every bound head
    /// variable, ascending).
    ///
    /// The all-bindings atom is `'$$'` (two characters) — **not** the one-character
    /// `'$'` an Erlang source literal `'$$'` lexes to — so the oracle program builds
    /// it with `list_to_atom([36,36])` and this side interns the `"$$"` name.
    fn match_spec_run_bindings(&mut self) -> String {
        let objects = [self.object(1, 10), self.object(2, 20), self.object(3, 30)];
        let list = self.list(&objects);
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let body_var = self.list(&[v1]);
        let whole = self.atom("$_");
        let body_whole = self.list(&[whole]);
        let all_bindings = self.atom("$$");
        let body_all = self.list(&[all_bindings]);

        let vars = self.run_on_list(list, head, body_var);
        let whole_objects = self.run_on_list(list, head, body_whole);
        let bindings = self.run_on_list(list, head, body_all);
        tuple_of(&[
            self.inorder(&vars),
            self.inorder(&whole_objects),
            self.inorder(&bindings),
        ])
    }

    /// A repeated head variable (`{'$1','$1'}`) enforces equality: only the equal
    /// pairs survive.
    fn match_spec_run_repeated_var(&mut self) -> String {
        let objects = [self.object(1, 1), self.object(1, 2), self.object(2, 2)];
        let list = self.list(&objects);
        let v1 = self.atom("$1");
        let head = self.tuple(&[v1, v1]);
        let body = self.list(&[v1]);
        let result = self.run_on_list(list, head, body);
        self.inorder(&result)
    }

    /// Multiple clauses (the first match wins) plus an element no clause matches
    /// (a bare `42` against a 2-tuple head) — dropped from the result.
    fn match_spec_run_multi_clause(&mut self) -> String {
        let objects = [
            self.object(1, 10),
            self.object(2, 20),
            self.object(3, 30),
            make_small(42),
        ];
        let list = self.list(&objects);
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let gt = self.atom(">");
        let guard = self.tuple(&[gt, v2, make_small(15)]);
        let guards = self.list(&[guard]);
        let big = self.atom("big");
        let small = self.atom("small");
        let body_big = self.list(&[big]);
        let body_small = self.list(&[small]);
        let clause_big = self.clause(head, guards, body_big);
        let clause_small = self.clause(head, NIL, body_small);
        let spec = self.list(&[clause_big, clause_small]);
        let result = match_spec_run(list, spec, &self.atoms, &mut self.out).expect("match_spec_run");
        self.inorder(&result)
    }

    /// An empty list yields `[]`; a non-list first argument is `badarg`.
    fn match_spec_run_empty_and_badarg(&mut self) -> String {
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let head = self.tuple(&[v1, v2]);
        let body = self.list(&[v1]);
        let spec = self.spec(head, NIL, body);
        let empty = match_spec_run(NIL, spec, &self.atoms, &mut self.out).expect("the empty run");
        let badarg = cap_terms(
            match_spec_run(make_small(1), spec, &self.atoms, &mut self.out),
            &self.atoms,
        );
        tuple_of(&[self.inorder(&empty), badarg])
    }

    /// A raising body expression yields the atom `EXIT` (the C `FAIL_TERM`): a head
    /// matching every 2-tuple with body `[{hd,5}]` selects `['EXIT','EXIT']`.
    fn body_error_exit(&mut self) -> String {
        // The runtime's global atom table always knows `EXIT` (the C `FAIL_TERM`),
        // and the executor resolves a failed body against it by name; the battery's
        // minimal table has to say so explicitly (like `true`/`false`).
        let _exit = self.atom("EXIT");
        let mut table = self.table(TableKind::OrderedSet, 1);
        table.insert(self.object(1, 10), &self.atoms).expect("insert");
        table.insert(self.object(2, 20), &self.atoms).expect("insert");
        let wildcard = self.atom("_");
        let head = self.tuple(&[wildcard, wildcard]);
        let hd = self.atom("hd");
        let call = self.tuple(&[hd, make_small(5)]);
        let body = self.list(&[call]);
        let spec_term = self.spec(head, NIL, body);
        let selected = self.select(&table, spec_term);
        self.inorder(&selected)
    }

    /// The whole `select` family over a hand-built `fun2ms`-shaped spec
    /// (`[{{Head}, [Guards], [Body]}]`), exercising every special body form: a
    /// bound variable `'$N'`, the whole object `'$_'` and the all-bindings atom
    /// `'$$'`.
    ///
    /// `select_count/2` counts only objects whose body evaluates to `true`, so it
    /// needs a separate `[true]` body — the `['$1']` body `select/2` uses would
    /// count zero. `select_replace/2` and `select_delete/2` mutate the table, so
    /// each runs on its own fresh `ordered_set`.
    fn fun2ms_spec_family(&mut self) -> String {
        let mut table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2, 3] {
            let object = self.tuple(&[make_small(key), make_small(key), make_small(key * 10)]);
            table.insert(object, &self.atoms).expect("insert");
        }
        let v1 = self.atom("$1");
        let v2 = self.atom("$2");
        let v3 = self.atom("$3");
        let wildcard = self.atom("_");
        let whole = self.atom("$_");
        let all = self.atom("$$");
        let gt = self.atom(">");
        let star = self.atom("*");
        let truth = self.atom("true");

        let head3 = self.tuple(&[v1, v2, v3]);
        let guard = self.tuple(&[gt, v3, make_small(15)]);
        let guards = self.list(&[guard]);
        let body_var = self.list(&[v1]);
        let body_true = self.list(&[truth]);
        let body_whole = self.list(&[whole]);
        let body_all = self.list(&[all]);

        // `select/2` with the `'$N'` body: the keys above the guard.
        let sel = self.spec(head3, guards, body_var);
        let selected = self.select(&table, sel);
        let r1 = self.inorder(&selected);

        // `select_count/2` with a `[true]` body (the body that counts).
        let count_spec = self.spec(head3, guards, body_true);
        let compiled = compile(count_spec, &self.atoms).expect("the spec compiles");
        let r2 = table
            .select_count(&compiled, &self.atoms, &mut self.out)
            .expect("select_count")
            .to_string();

        // The `match` family over the `{'$1','_','$3'}` pattern.
        let pattern = self.tuple(&[v1, wildcard, v3]);
        let mo = table
            .match_object(pattern, &self.atoms, &mut self.out)
            .expect("match_object");
        let mo = self.inorder(&mo);
        let m = table
            .match_(pattern, &self.atoms, &mut self.out)
            .expect("match/2");
        let m = self.inorder(&m);

        // `select/2` with the `'$_'` whole-object body.
        let whole_spec = self.spec(pattern, NIL, body_whole);
        let selected = self.select(&table, whole_spec);
        let r3 = self.inorder(&selected);

        // `select/2` with the `'$$'` all-bindings body.
        let all_spec = self.spec(head3, NIL, body_all);
        let selected = self.select(&table, all_spec);
        let r4 = self.inorder(&selected);

        // `match_spec_run/2` over a list.
        let run_head = self.tuple(&[v1, v2]);
        let run_body = self.list(&[whole]);
        let run_spec = self.spec(run_head, NIL, run_body);
        let first = self.object(1, 2);
        let second = self.object(3, 4);
        let run_list = self.list(&[first, second]);
        let r5 = match_spec_run(run_list, run_spec, &self.atoms, &mut self.out)
            .expect("match_spec_run");
        let r5 = self.inorder(&r5);

        // `select_replace/2` on a fresh table (the guarded doubling rewrite).
        let mut replace_table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2, 3] {
            replace_table
                .insert(self.object(key, key * 10), &self.atoms)
                .expect("insert");
        }
        let replace_head = self.tuple(&[v1, v2]);
        let doubled = self.tuple(&[star, v2, make_small(2)]);
        let inner = self.tuple(&[v1, doubled]);
        let construction = self.tuple(&[inner]);
        let replace_body = self.list(&[construction]);
        let replace_guard = self.tuple(&[gt, v2, make_small(10)]);
        let replace_guards = self.list(&[replace_guard]);
        let replace_spec_term = self.spec(replace_head, replace_guards, replace_body);
        let replace_spec = compile(replace_spec_term, &self.atoms).expect("the spec compiles");
        let rc = cap_count(replace_table.select_replace(&replace_spec, &self.atoms, &mut self.out));
        let rt = self.srt(&replace_table.tab2list());

        // `select_delete/2` on a fresh table (the same guard, a `[true]` body).
        let mut delete_table = self.table(TableKind::OrderedSet, 1);
        for key in [1i64, 2, 3] {
            delete_table
                .insert(self.object(key, key * 10), &self.atoms)
                .expect("insert");
        }
        let delete_spec_term = self.spec(replace_head, replace_guards, body_true);
        let delete_spec = compile(delete_spec_term, &self.atoms).expect("the spec compiles");
        let dc = delete_table
            .select_delete(&delete_spec, &self.atoms, &mut self.out)
            .expect("select_delete")
            .to_string();
        let dt = self.srt(&delete_table.tab2list());

        tuple_of(&[r1, r2, mo, m, r3, r4, r5, rc, rt, dc, dt])
    }

    /// The compiled spec is what the caller needs; this is the shared
    /// compile-and-run step of the simple select cases.
    pub(super) fn select(&mut self, table: &Table, spec_term: Eterm) -> Vec<Eterm> {
        let spec = compile(spec_term, &self.atoms).expect("the spec compiles");
        table
            .select(&spec, &self.atoms, &mut self.out)
            .expect("select")
    }
}

/// The `select` / `match` cases, in the oracle's order.
pub(super) fn matchspec_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push(("select_head_vars".into(), battery.select_head_vars()));
    cases.push((
        "select_wildcard_literal".into(),
        battery.select_wildcard_literal(),
    ));
    cases.push((
        "select_guard_comparison".into(),
        battery.select_guard_comparison(),
    ));
    cases.push((
        "select_guard_typetest".into(),
        battery.select_guard_typetest(),
    ));
    cases.push((
        "select_guard_andalso_orelse".into(),
        battery.select_guard_andalso_orelse(),
    ));
    cases.push(("select_guard_div".into(), battery.select_guard_div()));
    cases.push((
        "select_guard_float".into(),
        battery.select_guard_float(),
    ));
    cases.push((
        "select_guard_element".into(),
        battery.select_guard_element(),
    ));
    cases.push(("select_guard_abs".into(), battery.select_guard_abs()));
    cases.push((
        "select_guard_map_size".into(),
        battery.select_guard_map_size(),
    ));
    cases.push((
        "select_guard_is_map_key".into(),
        battery.select_guard_is_map_key(),
    ));
    cases.push((
        "select_guard_map_get".into(),
        battery.select_guard_map_get(),
    ));
    cases.push((
        "select_guard_tuple_size".into(),
        battery.select_guard_tuple_size(),
    ));
    cases.push((
        "select_guard_size_family".into(),
        battery.select_guard_size_family(),
    ));
    cases.push((
        "select_guard_float_arith".into(),
        battery.select_guard_float_arith(),
    ));
    cases.push(("select_body_whole".into(), battery.select_body_whole()));
    cases.push(("select_body_var".into(), battery.select_body_var()));
    cases.push(("select_body_const".into(), battery.select_body_const()));
    cases.push((
        "select_body_construct".into(),
        battery.select_body_construct(),
    ));
    cases.push((
        "select_body_map_construct".into(),
        battery.select_body_map_construct(),
    ));
    cases.push((
        "select_delete_and_count".into(),
        battery.select_delete_and_count(),
    ));
    cases.push(("match_family".into(), battery.match_family()));
    cases.push(("malformed_specs".into(), battery.malformed_specs()));
    cases.push((
        "match_spec_run_bindings".into(),
        battery.match_spec_run_bindings(),
    ));
    cases.push((
        "match_spec_run_repeated_var".into(),
        battery.match_spec_run_repeated_var(),
    ));
    cases.push((
        "match_spec_run_multi_clause".into(),
        battery.match_spec_run_multi_clause(),
    ));
    cases.push((
        "match_spec_run_empty_and_badarg".into(),
        battery.match_spec_run_empty_and_badarg(),
    ));
    cases.push(("body_error_exit".into(), battery.body_error_exit()));
    cases.push((
        "fun2ms_spec_family".into(),
        battery.fun2ms_spec_family(),
    ));

    cases
}
