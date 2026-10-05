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

    cases
}
