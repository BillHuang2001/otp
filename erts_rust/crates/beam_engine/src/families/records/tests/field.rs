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

//! `get_record_field` (opcode 190).
//!
//! An atom `Id` is the `i_get_local_record_field` form (the record must be named
//! that atom in the running module; no export requirement, the `_` name skips the
//! name test); a `{Module, Name}` pair `Id` is the `i_get_record_field` form (the
//! definition must match that pair **and** be exported).

use crate::interpreter::LabelMap;

use super::*;

#[test]
fn get_record_field_local_form_reads_a_field_without_an_export_requirement() {
    // An atom `Id` is the `i_get_local_record_field` form: the record must be named
    // that atom in the running module, but need not be exported.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, _b, r) = two_field_record(&mut fx, &mut ctx);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: NO_FAIL,
            src: Operand::X(0),
            id: Operand::Atom(r),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(make_small(11)));
}

#[test]
fn get_record_field_local_form_with_the_underscore_name_skips_the_name_test() {
    // `am_Underscore` is the wildcard name: any record's field `a` is read.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a) = (fx.atom("m"), fx.atom("r"), fx.atom("a"));
    let underscore = fx.atom("_");
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(11))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: NO_FAIL,
            src: Operand::X(0),
            id: Operand::Atom(underscore),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(make_small(11)));
}

#[test]
fn get_record_field_local_form_branches_to_fail_on_a_name_mismatch() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let other = fx.atom("other");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: Label(7),
            src: Operand::X(0),
            id: Operand::Atom(other),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
    assert_eq!(machine.x(1), Some(NIL), "the destination is untouched on a fail");
}

#[test]
fn get_record_field_local_form_branches_to_fail_on_an_absent_field() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: Label(7),
            src: Operand::X(0),
            id: Operand::Atom(r),
            field: Operand::Atom(z),
            dst: Operand::X(1),
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_field_branches_to_fail_on_a_non_record_source() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (a, r) = (fx.atom("a"), fx.atom("r"));
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, make_small(5)).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: Label(7),
            src: Operand::X(0),
            id: Operand::Atom(r),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_field_foreign_form_reads_an_exported_record_from_another_module() {
    // A `{Module, Name}` pair `Id` is the `i_get_record_field` form: the definition
    // must match that pair **and** be exported.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a) = (fx.atom("other"), fx.atom("r"), fx.atom("a"));
    let rec = record(&mut ctx, other, r, true, &[(a, make_small(11))]);
    let id = terms::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(3, id).unwrap();

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: NO_FAIL,
            src: Operand::X(0),
            id: Operand::X(3),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(make_small(11)));
}

#[test]
fn get_record_field_foreign_form_branches_to_fail_on_a_non_exported_record() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a) = (fx.atom("other"), fx.atom("r"), fx.atom("a"));
    let rec = record(&mut ctx, other, r, false, &[(a, make_small(11))]);
    let id = terms::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(3, id).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: Label(7),
            src: Operand::X(0),
            id: Operand::X(3),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_field_foreign_form_branches_to_fail_on_a_name_mismatch() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, s, a) = (fx.atom("other"), fx.atom("r"), fx.atom("s"), fx.atom("a"));
    // The record is named `s`, but the `Id` asks for `r`.
    let rec = record(&mut ctx, other, s, true, &[(a, make_small(11))]);
    let id = terms::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(3, id).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: Label(7),
            src: Operand::X(0),
            id: Operand::X(3),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_field_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: NO_FAIL,
            src: Operand::X(0),
            id: Operand::Atom(r),
            field: Operand::Atom(z),
            dst: Operand::X(1),
        })),
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: get_record_field/no_fail");
}
