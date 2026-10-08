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

//! `get_record_elements` (opcode 187) / `get_record_elements_id` (opcode 192).
//!
//! Both read each listed `(field name, destination)` pair's value out of the
//! record; `emu/ops.tab:1643` lowers the `_id` spelling to the same
//! implementation, so the `Id` operand never reaches the semantics.

use crate::interpreter::LabelMap;

use super::*;

#[test]
fn get_record_elements_writes_each_listed_field_in_list_order() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, b, _r) = two_field_record(&mut fx, &mut ctx);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::GetRecordElements {
            fail: NO_FAIL,
            src: Operand::X(0),
            fields: vec![
                (Operand::Atom(b), Operand::X(2)),
                (Operand::Atom(a), Operand::X(1)),
            ],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(make_small(11)));
    assert_eq!(machine.x(2), Some(make_small(22)));
}

#[test]
fn get_record_elements_branches_to_fail_after_writing_the_fields_before_the_absent_one() {
    // C fills each `PUT_TERM_REG` *before* the final `return false`, so a field the
    // record lacks fails the instruction but leaves the earlier destinations set.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z"); // never a field of this record
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 1, 3),
        &mut machine,
        &Instr::GetRecordElements {
            fail: Label(7),
            src: Operand::X(0),
            fields: vec![
                (Operand::Atom(a), Operand::X(1)),
                (Operand::Atom(z), Operand::X(2)),
            ],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 1, pc: 3 }));
    assert_eq!(machine.x(1), Some(make_small(11)), "the first field is written");
    assert_eq!(machine.x(2), Some(NIL), "the absent field's destination is untouched");
}

#[test]
fn get_record_elements_branches_to_fail_on_a_non_record_source() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let a = fx.atom("a");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, make_small(5)).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordElements {
            fail: Label(7),
            src: Operand::X(0),
            fields: vec![(Operand::Atom(a), Operand::X(1))],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_elements_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::GetRecordElements {
            fail: NO_FAIL,
            src: Operand::X(0),
            fields: vec![(Operand::Atom(z), Operand::X(1))],
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: get_record_elements/no_fail");
}

#[test]
fn get_record_elements_id_is_get_record_elements_with_the_id_discarded() {
    // `emu/ops.tab:1643` lowers `get_record_elements_id Fail _Id Src …` to
    // `i_get_record_elements Fail Src …`: the `Id` never reaches the implementation,
    // so even a nonsense `Id` operand is harmless.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, b, _r) = two_field_record(&mut fx, &mut ctx);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(3, make_small(99)).unwrap(); // not a record id at all

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::GetRecordElementsId {
            fail: NO_FAIL,
            id: Operand::X(3),
            src: Operand::X(0),
            fields: vec![
                (Operand::Atom(a), Operand::X(1)),
                (Operand::Atom(b), Operand::X(2)),
            ],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(make_small(11)));
    assert_eq!(machine.x(2), Some(make_small(22)));
}

#[test]
fn get_record_elements_id_branches_to_fail_on_an_absent_field() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordElementsId {
            fail: Label(7),
            id: Operand::Nil,
            src: Operand::X(0),
            fields: vec![(Operand::Atom(z), Operand::X(1))],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_elements_id_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::GetRecordElementsId {
            fail: NO_FAIL,
            id: Operand::Nil,
            src: Operand::X(0),
            fields: vec![(Operand::Atom(z), Operand::X(1))],
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: get_record_elements_id/no_fail");
}
