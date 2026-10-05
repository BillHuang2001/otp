//! The **`select` / `match`** case group: `select/2` with variables, wildcards,
//! literals and guards (comparison, type test, `andalso`/`orelse`) and bodies
//! (`'$_'`, `'$N'`, constants, `{{…}}` construction); `select_delete/2` +
//! `select_count/2`; the `match` family; and malformed specs (`badarg`).

use super::*;

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

    /// The compiled spec is what the caller needs; this is the shared
    /// compile-and-run step of the simple select cases.
    fn select(&mut self, table: &Table, spec_term: Eterm) -> Vec<Eterm> {
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
    cases.push(("select_body_whole".into(), battery.select_body_whole()));
    cases.push(("select_body_var".into(), battery.select_body_var()));
    cases.push(("select_body_const".into(), battery.select_body_const()));
    cases.push((
        "select_body_construct".into(),
        battery.select_body_construct(),
    ));
    cases.push((
        "select_delete_and_count".into(),
        battery.select_delete_and_count(),
    ));
    cases.push(("match_family".into(), battery.match_family()));
    cases.push(("malformed_specs".into(), battery.malformed_specs()));

    cases
}
