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
    let id = crate::terms::test_support::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
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
    let id = crate::terms::test_support::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
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
    let id = crate::terms::test_support::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
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
fn get_record_field_local_form_with_no_fail_raises_badfield_naming_the_record_and_field() {
    // `erl_get_local_record_field` sets `p->fvalue = TUPLE2(TUPLE2(defp->module,
    // defp->name), field); p->freason = EXC_BADFIELD` when no key matches
    // (`erl_record.c:814-821`), and `$FAIL_HEAD_OR_BODY($Fail)`
    // (`emu/instrs.tab:1447`) then raises it: class `error`, reason
    // `{badfield, {{Module,Name}, Field}}`.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let badfield = fx.atom("badfield");
    fx.atom("error");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let result = run(
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
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, beam_common::ExceptionClass::Error);
    assert_field_failure_reason(reason, badfield, fx.module().name, r, z);
    assert_eq!(machine.x(1), Some(NIL), "the destination is untouched on a raise");
}

#[test]
fn get_record_field_local_form_with_no_fail_raises_badrecord_carrying_the_source_term() {
    // Every `badrecord` exit of `erl_get_local_record_field` (`erl_record.c:790-803`)
    // is `p->fvalue = src; p->freason = EXC_BADRECORD` — the **source term itself**,
    // not the `{Module,Name}` id (that payload shape belongs to the create form).
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let other = fx.atom("other");
    let badrecord = fx.atom("badrecord");
    fx.atom("error");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let result = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: NO_FAIL,
            src: Operand::X(0),
            // The record is named `r`; the atom `other` is a name mismatch.
            id: Operand::Atom(other),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        })),
        &mut ctx,
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, beam_common::ExceptionClass::Error);
    let payload = assert_badrecord_reason(reason, badrecord, rec);
    assert!(
        predicates::is_record(payload),
        "the payload is the source record term, not an id pair"
    );
    assert_eq!(machine.x(1), Some(NIL), "the destination is untouched on a raise");
}

#[test]
fn get_record_field_with_no_fail_raises_badrecord_on_a_non_record_source() {
    // `is_not_record(src)` enters the C helper's `badrecord:` label directly
    // (`erl_record.c:791-793`), so the payload is the non-record term.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (a, r) = (fx.atom("a"), fx.atom("r"));
    let badrecord = fx.atom("badrecord");
    fx.atom("error");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, make_small(5)).unwrap();

    let result = run(
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
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, beam_common::ExceptionClass::Error);
    assert_badrecord_reason(reason, badrecord, make_small(5));
}

#[test]
fn get_record_field_foreign_form_with_no_fail_raises_badrecord_on_a_non_exported_record() {
    // `if (defp->is_exported == am_false) goto badrecord;` — `p->fvalue = src;
    // p->freason = EXC_BADRECORD` (`erl_record.c:856-858`).
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a) = (fx.atom("other"), fx.atom("r"), fx.atom("a"));
    let badrecord = fx.atom("badrecord");
    fx.atom("error");
    let rec = record(&mut ctx, other, r, false, &[(a, make_small(11))]);
    let id = crate::terms::test_support::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(3, id).unwrap();

    let result = run(
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
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, beam_common::ExceptionClass::Error);
    assert_badrecord_reason(reason, badrecord, rec);
}

#[test]
fn get_record_field_foreign_form_with_no_fail_raises_badfield_naming_the_record_and_field() {
    // `erl_get_record_field`'s absent-field exit (`erl_record.c:872-879`) is the same
    // `{badfield, {{defp->module, defp->name}, field}}` reason as the local form.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a) = (fx.atom("other"), fx.atom("r"), fx.atom("a"));
    let z = fx.atom("z");
    let badfield = fx.atom("badfield");
    fx.atom("error");
    let rec = record(&mut ctx, other, r, true, &[(a, make_small(11))]);
    let id = crate::terms::test_support::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(3, id).unwrap();

    let result = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: NO_FAIL,
            src: Operand::X(0),
            id: Operand::X(3),
            field: Operand::Atom(z),
            dst: Operand::X(1),
        })),
        &mut ctx,
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, beam_common::ExceptionClass::Error);
    assert_field_failure_reason(reason, badfield, other, r, z);
}
