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

//! End-to-end synthetic sequences: `try` → `case_end` → `try_case` → `return`,
//! and nested handlers with a re-raise.
use super::*;

// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so alias
// `terms` to the test-only wrappers that keep the old ergonomic arity. (An explicit
// import shadows the glob-imported `terms` above.)
use crate::terms::test_support as terms;

// ------------------------------------------------------------------
// End-to-end synthetic sequence: try → case_end → try_case → return
// ------------------------------------------------------------------

#[test]
fn full_try_raise_handler_sequence_runs() {
    // f() -> try (case 1 of 2 -> ok end) catch error:{case_clause,_} -> caught end
    // The handler moves x(1) (the expanded reason) into x(0) and returns it;
    // x(0) == x(3) (the class atom) is asserted through `try_case`.
    let mut fixture = Fixture::new(&["case_clause", "error"]);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(3),
        },
        Instr::CaseEnd {
            value: Operand::Integer(1),
        },
        Instr::Label(Label(3)),
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let result = machine.run_label(&module, entry, 0, &[], &mut ctx).unwrap();
    let expected = terms::make_tuple(
        &mut ctx,
        &[
            make_atom(module.atom_table.lookup("case_clause").unwrap()),
            make_small(1),
        ],
    )
    .unwrap();
    assert!(cmp(result, expected, &module.atom_table).is_eq());
    assert_eq!(machine.catch_frame_depth(), 0);
}

#[test]
fn nested_try_handler_runs_and_does_not_disturb_the_outer_frame() {
    // outer() -> try inner() catch error:{case_clause,_} -> caught end
    //   inner() -> try (case 2 of 1 -> ok end) catch throw:_ -> nope end
    // `inner/0` raises `{case_clause,2}`; its handler does not match `throw`,
    // so `raise` re-raises with class `error`, landing in `outer`'s handler
    // after unwinding `inner`'s frame.
    let mut fixture = Fixture::new(&["case_clause", "error", "throw"]);
    let inner = vec![
        Instr::Label(Label(10)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(12),
        },
        Instr::CaseEnd {
            value: Operand::Integer(2),
        },
        Instr::Label(Label(12)),
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        // Re-raise: the not-matching tail (value = x(1), trace = x(2)).
        Instr::Raise {
            trace: Operand::X(2),
            value: Operand::X(1),
        },
    ];
    let outer = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(3),
        },
        Instr::Call {
            arity: 0,
            target: Label(10),
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ];
    let entry = fixture.add_function("outer", 0, outer);
    fixture.add_function("inner", 0, inner);
    let module = fixture.finish();

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let result = machine.run_label(&module, entry, 0, &[], &mut ctx).unwrap();
    let expected = terms::make_tuple(
        &mut ctx,
        &[
            make_atom(module.atom_table.lookup("case_clause").unwrap()),
            make_small(2),
        ],
    )
    .unwrap();
    assert!(cmp(result, expected, &module.atom_table).is_eq());
    assert_eq!(
        machine.frame_depth(),
        1,
        "the run is back at the bottom frame"
    );
}
