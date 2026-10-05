//! The **`update_counter` / `update_element`** case group: `update_counter/3`
//! (scalar, `{Pos,Incr}`, `{Pos,Incr,Threshold,SetValue}`, operation lists,
//! absent keys, counter-incapable kinds) and `update_counter/4` (defaults,
//! lists, thresholds), plus `update_element/3`.

use super::*;

impl Battery {
    fn update_counter_3_variants(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10, 20]);
        table.insert(object, &self.atoms).expect("insert");
        let v1 = table
            .update_counter(make_small(1), make_small(5), &self.atoms, &mut self.out)
            .expect("update_counter");
        let v2 = table
            .update_counter(make_small(1), make_small(-3), &self.atoms, &mut self.out)
            .expect("update_counter");
        let positional = self.ints(&[3, 5]);
        let v3 = table
            .update_counter(make_small(1), positional, &self.atoms, &mut self.out)
            .expect("update_counter");
        let first = self.ints(&[2, 1]);
        let second = self.ints(&[3, 1]);
        let operations = self.list(&[first, second]);
        let v4 = table
            .update_counter(make_small(1), operations, &self.atoms, &mut self.out)
            .expect("update_counter");
        let stored = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            self.r(v1),
            self.r(v2),
            self.r(v3),
            self.r(v4),
            self.srt(&stored),
        ])
    }

    fn update_counter_3_absent(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let scalar = table.update_counter(make_small(1), make_small(5), &self.atoms, &mut self.out);
        let c1 = cap_value(scalar, &self.atoms);
        let positional = self.ints(&[2, 1]);
        let tuple_op =
            table.update_counter(make_small(1), positional, &self.atoms, &mut self.out);
        let c2 = cap_value(tuple_op, &self.atoms);
        tuple_of(&[c1, c2, table.size().to_string()])
    }

    fn update_counter_3_threshold(&mut self) -> String {
        let mut rendered = Vec::new();
        for operation in [[2i64, 5, 12, 100], [2, -9, 3, 100]] {
            let mut table = self.table(TableKind::Set, 1);
            let object = self.object(1, 10);
            table.insert(object, &self.atoms).expect("insert");
            let operand = self.ints(&operation);
            let result =
                table.update_counter(make_small(1), operand, &self.atoms, &mut self.out);
            rendered.push(cap_value(result, &self.atoms));
        }
        list_of(&rendered)
    }

    fn update_counter_4_default(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let default = self.ints(&[1, 0, 0]);
        let v1 = table
            .update_counter_with(
                make_small(1),
                make_small(5),
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let v1 = self.r(v1);
        // Rendered *now*: a `lookup` result aliases table storage, and
        // `update_counter` increments in place — a term captured here and
        // rendered after `v2` would show the second increment.
        let l1 = self.srt(&table.lookup(make_small(1), &self.atoms));

        let default = self.ints(&[1, 0, 0]);
        let v2 = table
            .update_counter_with(
                make_small(1),
                make_small(5),
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let v2 = self.r(v2);

        let default = self.ints(&[7, 0, 0]);
        let v3 = table
            .update_counter_with(
                make_small(7),
                make_small(3),
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let v3 = self.r(v3);
        let l7 = self.srt(&table.lookup(make_small(7), &self.atoms));
        tuple_of(&[v1, l1, v2, v3, l7])
    }

    fn update_counter_4_list(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10, 20]);
        table.insert(object, &self.atoms).expect("insert");

        let first = self.ints(&[2, 1]);
        let second = self.ints(&[3, 1]);
        let operations = self.list(&[first, second]);
        let default = self.ints(&[1, 0, 0]);
        let v1 = table
            .update_counter_with(
                make_small(1),
                operations,
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let v1 = self.r(v1);
        // Rendered now: the stored object is mutated in place by the next call.
        let l1 = self.srt(&table.lookup(make_small(1), &self.atoms));

        let only = self.ints(&[2, 5]);
        let operations = self.list(&[only]);
        let default = self.ints(&[1, 0, 0]);
        let v2 = table
            .update_counter_with(
                make_small(1),
                operations,
                default,
                &self.atoms,
                &mut self.out,
            )
            .expect("update_counter/4");
        let v2 = self.r(v2);

        let default = self.ints(&[9, 0, 0]);
        let v3 = table
            .update_counter_with(make_small(9), NIL, default, &self.atoms, &mut self.out)
            .expect("update_counter/4");
        let v3 = self.r(v3);
        let l9 = self.srt(&table.lookup(make_small(9), &self.atoms));
        tuple_of(&[v1, l1, v2, v3, l9])
    }

    fn update_counter_4_threshold(&mut self) -> String {
        let mut rendered = Vec::new();
        for operation in [
            [2i64, 5, 12, 100],
            [2, 2, 12, 100],
            [2, -9, 3, 100],
            [2, -7, 3, 100],
            [2, 0, 5, 100],
        ] {
            let mut table = self.table(TableKind::Set, 1);
            let object = self.object(1, 10);
            table.insert(object, &self.atoms).expect("insert");
            let operand = self.ints(&operation);
            let default = self.ints(&[1, 0]);
            let value = table
                .update_counter_with(
                    make_small(1),
                    operand,
                    default,
                    &self.atoms,
                    &mut self.out,
                )
                .expect("update_counter/4");
            let stored = table.lookup(make_small(1), &self.atoms);
            rendered.push(tuple_of(&[self.r(value), self.srt(&stored)]));
        }
        list_of(&rendered)
    }

    fn update_counter_multi_kind(&mut self) -> String {
        let mut rendered = Vec::new();
        for kind in [TableKind::Bag, TableKind::DuplicateBag] {
            let mut table = self.table(kind, 1);
            let object = self.object(1, 10);
            table.insert(object, &self.atoms).expect("insert");
            let result =
                table.update_counter(make_small(1), make_small(1), &self.atoms, &mut self.out);
            rendered.push(cap_value(result, &self.atoms));
        }
        list_of(&rendered)
    }

    fn update_element_variants(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10, 20, 30]);
        table.insert(object, &self.atoms).expect("insert");

        let r1 = table
            .update_element(
                make_small(1),
                &[(2, make_small(99))],
                &self.atoms,
                &mut self.out,
            )
            .expect("update_element");
        let l1 = table.lookup(make_small(1), &self.atoms);

        let r2 = table
            .update_element(
                make_small(1),
                &[(3, make_small(2)), (2, make_small(5))],
                &self.atoms,
                &mut self.out,
            )
            .expect("update_element");
        let l2 = table.lookup(make_small(1), &self.atoms);

        let r3 = table
            .update_element(make_small(1), &[], &self.atoms, &mut self.out)
            .expect("update_element");
        let l3 = table.lookup(make_small(1), &self.atoms);

        let r4 = table
            .update_element(
                make_small(99),
                &[(2, make_small(9))],
                &self.atoms,
                &mut self.out,
            )
            .expect("update_element");

        let e1 = {
            let result = table.update_element(
                make_small(1),
                &[(1, make_small(9))],
                &self.atoms,
                &mut self.out,
            );
            cap_bool(result)
        };
        let e2 = {
            let result = table.update_element(
                make_small(1),
                &[(0, make_small(9))],
                &self.atoms,
                &mut self.out,
            );
            cap_bool(result)
        };
        let e3 = {
            let result = table.update_element(
                make_small(1),
                &[(5, make_small(9))],
                &self.atoms,
                &mut self.out,
            );
            cap_bool(result)
        };
        let l4 = table.lookup(make_small(1), &self.atoms);
        tuple_of(&[
            bool_str(r1),
            self.srt(&l1),
            bool_str(r2),
            self.srt(&l2),
            bool_str(r3),
            self.srt(&l3),
            bool_str(r4),
            e1,
            e2,
            e3,
            self.srt(&l4),
        ])
    }
}

/// The `update_counter` / `update_element` cases, in the oracle's order.
pub(super) fn counter_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push((
        "update_counter_3_variants".into(),
        battery.update_counter_3_variants(),
    ));
    cases.push((
        "update_counter_3_absent".into(),
        battery.update_counter_3_absent(),
    ));
    cases.push((
        "update_counter_3_threshold".into(),
        battery.update_counter_3_threshold(),
    ));
    cases.push((
        "update_counter_4_default".into(),
        battery.update_counter_4_default(),
    ));
    cases.push((
        "update_counter_4_list".into(),
        battery.update_counter_4_list(),
    ));
    cases.push((
        "update_counter_4_threshold".into(),
        battery.update_counter_4_threshold(),
    ));
    cases.push((
        "update_counter_multi_kind".into(),
        battery.update_counter_multi_kind(),
    ));
    cases.push((
        "update_element_variants".into(),
        battery.update_element_variants(),
    ));

    cases
}
