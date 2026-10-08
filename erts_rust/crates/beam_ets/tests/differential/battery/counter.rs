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

    /// `{Pos, Incr, Threshold, SetValue}` combined with a `/4` default object for
    /// an absent key (the oracle's `update_counter_threshold_default/0`): a plain
    /// increment stays below the threshold, crossing it upward (`>=` for a
    /// positive increment) and downward (`=<` for a negative one) lands on
    /// `SetValue`, and a value exactly at the threshold is left alone.
    fn update_counter_threshold_default(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);

        let operand = self.ints(&[2, 5, 100, 0]);
        let default = self.ints(&[1, 10]);
        let v1 = table
            .update_counter_with(make_small(1), operand, default, &self.atoms, &mut self.out)
            .expect("update_counter/4");
        // Rendered *now*: `lookup` aliases table storage and `update_counter`
        // increments in place, so a term captured here would show a later update.
        let v1 = self.r(v1);
        let l1 = self.srt(&table.lookup(make_small(1), &self.atoms));

        let operand = self.ints(&[2, 95, 100, -1]);
        let v2 = table
            .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let v2 = self.r(v2);
        let l2 = self.srt(&table.lookup(make_small(1), &self.atoms));

        let operand = self.ints(&[2, 1, 100, 999]);
        let v3 = table
            .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let v3 = self.r(v3);
        let l3 = self.srt(&table.lookup(make_small(1), &self.atoms));

        let operand = self.ints(&[2, -20, 0, -5]);
        let default = self.ints(&[2, 1]);
        let v4 = table
            .update_counter_with(make_small(2), operand, default, &self.atoms, &mut self.out)
            .expect("update_counter/4");
        let v4 = self.r(v4);
        let l4 = self.srt(&table.lookup(make_small(2), &self.atoms));

        let operand = self.ints(&[2, -1, 0, 77]);
        let v5 = table
            .update_counter(make_small(2), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let v5 = self.r(v5);
        let l5 = self.srt(&table.lookup(make_small(2), &self.atoms));

        tuple_of(&[v1, l1, v2, l2, v3, l3, v4, l4, v5, l5])
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
    fn update_counter_bignum_overflow(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, MAX_SMALL]);
        table.insert(object, &self.atoms).expect("insert");
        // `MAX_SMALL + 1` leaves the 60-bit range and is promoted to a bignum.
        let v1 = table
            .update_counter(make_small(1), make_small(1), &self.atoms, &mut self.out)
            .expect("update_counter");
        let v1 = self.r(v1);
        let l1 = self.srt(&table.lookup(make_small(1), &self.atoms));
        // …and subtracting 1 normalises it back to a small.
        let v2 = table
            .update_counter(make_small(1), make_small(-1), &self.atoms, &mut self.out)
            .expect("update_counter");
        let v2 = self.r(v2);
        let l2 = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[v1, l1, v2, l2])
    }

    fn update_counter_bignum_kinds(&mut self) -> String {
        let mut rendered = Vec::new();
        for kind in [TableKind::Set, TableKind::OrderedSet] {
            let mut table = self.table(kind, 1);
            let object = self.ints(&[1, MAX_SMALL]);
            table.insert(object, &self.atoms).expect("insert");
            let v1 = table
                .update_counter(make_small(1), make_small(1), &self.atoms, &mut self.out)
                .expect("update_counter");
            let v1 = self.r(v1);
            let operand = self.big(false, &pow2(100));
            let v2 = table
                .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
                .expect("update_counter");
            let v2 = self.r(v2);
            let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
            rendered.push(tuple_of(&[v1, v2, stored]));
        }
        list_of(&rendered)
    }

    fn update_counter_bignum_operand(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10]);
        table.insert(object, &self.atoms).expect("insert");
        let operand = self.big(false, &pow2(100));
        let v1 = table
            .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let v1 = self.r(v1);
        let operand = self.big(true, &pow2(100));
        let v2 = table
            .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let v2 = self.r(v2);
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[v1, v2, stored])
    }

    fn update_counter_bignum_element(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.big_object(1, true, &pow2(100));
        table.insert(object, &self.atoms).expect("insert");
        let v1 = table
            .update_counter(make_small(1), make_small(5), &self.atoms, &mut self.out)
            .expect("update_counter");
        let v1 = self.r(v1);
        let v2 = table
            .update_counter(make_small(1), make_small(-3), &self.atoms, &mut self.out)
            .expect("update_counter");
        let v2 = self.r(v2);
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[v1, v2, stored])
    }

    fn update_counter_bignum_add(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.big_object(1, false, &pow2(200));
        table.insert(object, &self.atoms).expect("insert");
        // `2^200 + (2^192 + 7)`: a carry-free two-limb-bignum addition.
        let operand = self.big(false, &pow2_plus(192, 7));
        let v1 = table
            .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let v1 = self.r(v1);
        let operand = self.big(true, &pow2(200));
        let v2 = table
            .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let v2 = self.r(v2);
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[v1, v2, stored])
    }

    fn update_counter_bignum_mixed_sign(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.big_object(1, true, &pow2(100));
        table.insert(object, &self.atoms).expect("insert");
        let operand = self.big(false, &pow2(100));
        let v1 = table
            .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let v1 = self.r(v1);
        let operand = self.big(true, &pow2_plus(100, 1));
        let v2 = table
            .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let v2 = self.r(v2);
        let operand = self.big(false, &pow2(100));
        let v3 = table
            .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let v3 = self.r(v3);
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[v1, v2, v3, stored])
    }

    fn update_counter_bignum_threshold(&mut self) -> String {
        let mut rendered = Vec::new();
        for which in 0..3 {
            let mut table = self.table(TableKind::Set, 1);
            let object = self.ints(&[1, 0]);
            table.insert(object, &self.atoms).expect("insert");
            let operand = match which {
                0 => {
                    let incr = self.big(false, &pow2(100));
                    let threshold = self.big(false, &pow2(99));
                    self.tuple(&[make_small(2), incr, threshold, make_small(7)])
                }
                1 => {
                    let incr = self.big(true, &pow2(100));
                    let threshold = self.big(true, &pow2(99));
                    self.tuple(&[make_small(2), incr, threshold, make_small(-7)])
                }
                _ => {
                    let incr = self.big(false, &pow2(100));
                    let threshold = self.big(false, &pow2(101));
                    self.tuple(&[make_small(2), incr, threshold, make_small(0)])
                }
            };
            let value = table
                .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
                .expect("update_counter");
            let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
            rendered.push(tuple_of(&[self.r(value), stored]));
        }
        list_of(&rendered)
    }

    fn update_counter_bignum_warp_value(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10]);
        table.insert(object, &self.atoms).expect("insert");
        // `10 + 1 = 11 > 5` fires the threshold rule, which stores the bignum
        // `SetValue`.
        let set_value = self.big(false, &pow2(100));
        let operand = self.tuple(&[make_small(2), make_small(1), make_small(5), set_value]);
        let value = table
            .update_counter(make_small(1), operand, &self.atoms, &mut self.out)
            .expect("update_counter");
        let value = self.r(value);
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[value, stored])
    }

    fn update_counter_bignum_list_ops(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 0, 0]);
        table.insert(object, &self.atoms).expect("insert");
        let first = {
            let incr = self.big(false, &pow2(100));
            self.tuple(&[make_small(2), incr])
        };
        let second = {
            let incr = self.big(true, &pow2(100));
            self.tuple(&[make_small(3), incr])
        };
        let operations = self.list(&[first, second]);
        let value = table
            .update_counter(make_small(1), operations, &self.atoms, &mut self.out)
            .expect("update_counter");
        let value = self.r(value);
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[value, stored])
    }

    fn update_counter_bignum_position_badarg(&mut self) -> String {
        let mut table = self.table(TableKind::Set, 1);
        let object = self.ints(&[1, 10]);
        table.insert(object, &self.atoms).expect("insert");

        // A bignum *position* is badarg (`erl_db.c:1573` requires a small there).
        let position = self.big(false, &pow2(100));
        let operand = self.tuple(&[position, make_small(1)]);
        let c1 = cap_value(
            table.update_counter(make_small(1), operand, &self.atoms, &mut self.out),
            &self.atoms,
        );
        // A bignum *threshold* is accepted: `10 + 1 = 11` is not above `2^100`.
        let threshold = self.big(false, &pow2(100));
        let operand = self.tuple(&[make_small(2), make_small(1), threshold, make_small(5)]);
        let c2 = cap_value(
            table.update_counter(make_small(1), operand, &self.atoms, &mut self.out),
            &self.atoms,
        );
        // A non-integer operand is still badarg, scalar and positional alike.
        let operand = self.atom("nope");
        let c3 = cap_value(
            table.update_counter(make_small(1), operand, &self.atoms, &mut self.out),
            &self.atoms,
        );
        let operand = {
            let atom = self.atom("nope");
            self.tuple(&[make_small(2), atom])
        };
        let c4 = cap_value(
            table.update_counter(make_small(1), operand, &self.atoms, &mut self.out),
            &self.atoms,
        );
        let stored = self.srt(&table.lookup(make_small(1), &self.atoms));
        tuple_of(&[c1, c2, c3, c4, stored])
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
        "update_counter_threshold_default".into(),
        battery.update_counter_threshold_default(),
    ));
    cases.push((
        "update_counter_multi_kind".into(),
        battery.update_counter_multi_kind(),
    ));
    cases.push((
        "update_element_variants".into(),
        battery.update_element_variants(),
    ));
    cases.push((
        "update_counter_bignum_overflow".into(),
        battery.update_counter_bignum_overflow(),
    ));
    cases.push((
        "update_counter_bignum_kinds".into(),
        battery.update_counter_bignum_kinds(),
    ));
    cases.push((
        "update_counter_bignum_operand".into(),
        battery.update_counter_bignum_operand(),
    ));
    cases.push((
        "update_counter_bignum_element".into(),
        battery.update_counter_bignum_element(),
    ));
    cases.push((
        "update_counter_bignum_add".into(),
        battery.update_counter_bignum_add(),
    ));
    cases.push((
        "update_counter_bignum_mixed_sign".into(),
        battery.update_counter_bignum_mixed_sign(),
    ));
    cases.push((
        "update_counter_bignum_threshold".into(),
        battery.update_counter_bignum_threshold(),
    ));
    cases.push((
        "update_counter_bignum_warp_value".into(),
        battery.update_counter_bignum_warp_value(),
    ));
    cases.push((
        "update_counter_bignum_list_ops".into(),
        battery.update_counter_bignum_list_ops(),
    ));
    cases.push((
        "update_counter_bignum_position_badarg".into(),
        battery.update_counter_bignum_position_badarg(),
    ));

    cases
}
