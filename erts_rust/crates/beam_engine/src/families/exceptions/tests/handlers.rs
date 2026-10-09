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

//! `try`/`catch` frame pushes, the catch-frame handler opcodes
//! (`try_case`/`try_end`/`catch_end`) and the `catch_end` fold.
use super::*;

// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so alias
// `terms` to the test-only wrappers that keep the old ergonomic arity. (An explicit
// import shadows the glob-imported `terms` above.)
use crate::terms::test_support as terms;

// ------------------------------------------------------------------
// try / catch push a frame
// ------------------------------------------------------------------

#[test]
fn try_pushes_a_catch_frame_and_blanks_the_slot() {
    let module = Fixture::new(&["case_clause", "error"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    let mut labels = LabelMap::new();
    labels.insert(Label(3), (0u32, 7u32));

    let step = exec(
        &mut machine,
        &module,
        &labels,
        &Instr::Try {
            frame: Operand::X(1),
            fail: Label(3),
        },
        0,
        0,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.catch_frame_depth(), 1);
    assert_eq!(machine.x(1), Some(NIL));
}

#[test]
fn catch_is_the_same_as_try() {
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    let mut labels = LabelMap::new();
    labels.insert(Label(9), (0u32, 4u32));
    let step = exec(
        &mut machine,
        &module,
        &labels,
        &Instr::Catch {
            frame: Operand::X(0),
            fail: Label(9),
        },
        0,
        0,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.catch_frame_depth(), 1);
}

#[test]
fn try_with_an_unknown_fail_label_is_a_typed_error() {
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    let err = exec(
        &mut machine,
        &module,
        &no_labels(),
        &Instr::Try {
            frame: Operand::X(1),
            fail: Label(42),
        },
        0,
        0,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::BadArg));
    assert_eq!(machine.catch_frame_depth(), 0);
}

// ------------------------------------------------------------------
// The handler opcodes
// ------------------------------------------------------------------

#[test]
fn try_case_moves_the_class_atom_into_x0() {
    let module = Fixture::new(&["error"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));
    let error_atom = make_atom(module.atom_table.lookup("error").unwrap());
    machine.set_x(0, Eterm::non_value()).unwrap();
    machine.set_x(1, make_small(99)).unwrap();
    machine.set_x(3, error_atom).unwrap();

    let mut ctx = MinimalContext::new();
    let step = exec_one(
        &module,
        &Instr::TryCase {
            frame: Operand::X(1),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(0), Some(error_atom));
    assert_eq!(machine.x(1), Some(NIL), "the frame slot is blanked");
    assert_eq!(machine.catch_frame_depth(), 0);
}

#[test]
fn try_case_requires_the_non_value_marker() {
    let module = Fixture::new(&["error"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));
    // x(0) is a plain NIL (no exception landed).
    let mut ctx = MinimalContext::new();
    let err = exec_one(
        &module,
        &Instr::TryCase {
            frame: Operand::X(1),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)));
    // The frame was consumed before the check failed.
    assert_eq!(machine.catch_frame_depth(), 0);
}

#[test]
fn try_end_pops_the_frame_and_blanks_the_slot() {
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(5, 5, 0));
    machine.set_x(1, make_small(3)).unwrap();
    let mut ctx = MinimalContext::new();
    let step = exec_one(
        &module,
        &Instr::TryEnd {
            frame: Operand::X(1),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(NIL));
    assert_eq!(machine.catch_frame_depth(), 0);
}

// ------------------------------------------------------------------
// catch_end — both branches, plus the preconditions
// ------------------------------------------------------------------

#[test]
fn catch_end_passes_a_normal_result_through() {
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));
    machine.set_x(0, make_small(123)).unwrap();
    let mut ctx = MinimalContext::new();
    let step = exec_one(
        &module,
        &Instr::CatchEnd {
            frame: Operand::X(5),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(0), Some(make_small(123)));
    assert_eq!(machine.catch_frame_depth(), 0);
}

#[test]
fn catch_end_throw_class_yields_the_thrown_value() {
    let module = Fixture::new(&["throw"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));
    let throw_atom = make_atom(module.atom_table.lookup("throw").unwrap());
    machine.set_class_atom(ExceptionClass::Throw, throw_atom);
    // Land a thrown exception (this sets x0=non_value, x1=reason, x3=class).
    let reason = make_small(77);
    machine
        .throw(ExceptionClass::Throw, reason)
        .expect("a live frame catches");
    let mut ctx = MinimalContext::new();
    let step = exec_one(
        &module,
        &Instr::CatchEnd {
            frame: Operand::X(5),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(0), Some(reason));
}

#[test]
fn catch_end_error_class_builds_an_exit_tuple() {
    let module = Fixture::new(&["EXIT", "error"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));
    machine.set_class_atom(
        ExceptionClass::Error,
        make_atom(module.atom_table.lookup("error").unwrap()),
    );
    let reason = make_small(88);
    machine
        .throw(ExceptionClass::Error, reason)
        .expect("a live frame catches");
    let mut ctx = MinimalContext::new();
    let step = exec_one(
        &module,
        &Instr::CatchEnd {
            frame: Operand::X(5),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    let exit_atom = make_atom(module.atom_table.lookup("EXIT").unwrap());
    let expected = terms::make_tuple(&mut ctx, &[exit_atom, reason]).unwrap();
    let actual = machine.x(0).unwrap();
    assert!(cmp(actual, expected, &module.atom_table).is_eq());
}

#[test]
fn catch_end_without_the_exit_atom_is_a_typed_error() {
    // Class error with no 'EXIT' atom: the tuple is not representable.
    let module = Fixture::new(&["error"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));
    machine.set_class_atom(
        ExceptionClass::Error,
        make_atom(module.atom_table.lookup("error").unwrap()),
    );
    machine
        .throw(ExceptionClass::Error, make_small(1))
        .expect("a live frame catches");
    let mut ctx = MinimalContext::new();
    let err = exec_one(
        &module,
        &Instr::CatchEnd {
            frame: Operand::X(5),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)));
}

#[test]
fn handler_opcode_without_a_catch_frame_is_a_typed_error() {
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    for instr in [
        Instr::TryCase {
            frame: Operand::X(1),
        },
        Instr::TryEnd {
            frame: Operand::X(1),
        },
        Instr::CatchEnd {
            frame: Operand::X(1),
        },
    ] {
        let err = exec_one(&module, &instr, &mut machine, &mut ctx).unwrap_err();
        assert!(matches!(err, BeamError::Raise(_)));
    }
}
