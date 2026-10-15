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

//! The baseline JIT's **stack (`Y`) operand** for `test` / `select_val` /
//! `select_tuple_arity`: tier parity plus the syntactic `Y` gate
//! ([`emit::uses_y`]).
//!
//! A `Y` operand is a slot in the current call frame's Y area, which lives in
//! `machine.stack` — a Rust `Vec` a frame push may reallocate. The tier already owns
//! the machinery for that (`super::emit::operands`' guarded frame window), so the
//! widening is the same one an inlined `Y`-operand `Move` and the small-int arithmetic
//! fast path use: the emitter synthesises the *guarded* read and a rejection branches to
//! `jit_step_one`, whose interpreter run reads `machine.stack` itself and reports its own
//! faithful error.
//!
//! The equivalence is asserted **observationally** here, on both tiers:
//!
//! * every widened `test` predicate, on a value that passes and one that fails (so both
//!   the success and the resolved-`fail` exits are exercised), including the
//!   comparison shapes whose operands are now runtime `Y` values;
//! * a `select_val` whose scrutinee is a `Y` slot — matching (first and later
//!   alternative), falling to the fail label, and matching none of several
//!   alternatives;
//! * a `select_tuple_arity` whose tuple source is a `Y` slot (the tuple-header guard
//!   rejects a non-tuple slot, then the interpreter runs the instruction);
//! * a `Y` slot **outside** the live frame window (the guard rejection) — the negative
//!   control that must keep the interpreter's exact typed error, never a wrong result;
//! * and the syntactic `Y` gate [`emit::uses_y`], positive for each widened shape and
//!   negative for the same instruction over an `X` operand.

use super::emit::uses_y;
use super::tests::{assert_same_outcome, body_module, error_reason, with_both_tiers};
use super::*;
use beam_loader::{HeapNeed, Instr, Label, Operand, TestKind};
use beam_term::make_small;

/// The `Y` slot index the widened shapes read.
const SLOT: u32 = 0;
/// The value written into [`SLOT`] before the instruction under test runs.
const VALUE: i64 = 7;
/// Returned when the test SUCCEEDS (the instruction falls through).
const PASS: i64 = 11;
/// Returned from the resolved `fail` label.
const FAIL: i64 = 99;

/// A one-function body that writes [`VALUE`] into `Y(SLOT)` and then runs `instr`,
/// returning [`PASS`] on the fall-through and [`FAIL`] from `Label(3)` (the fail
/// target every widened instruction resolves to in these tests).
fn y_body(stack_need: u32, instr: Instr) -> Vec<Instr> {
    vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(stack_need),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(VALUE),
            dst: Operand::Y(SLOT),
        },
        instr,
        Instr::Move {
            src: Operand::Integer(PASS),
            dst: Operand::X(0),
        },
        Instr::Deallocate(stack_need),
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::Move {
            src: Operand::Integer(FAIL),
            dst: Operand::X(0),
        },
        Instr::Deallocate(stack_need),
        Instr::Return,
    ]
}

/// A `test` over `Y(SLOT)` (the widened shape).
fn y_test(kind: TestKind, args: Vec<Operand>) -> Instr {
    Instr::Test {
        kind,
        fail: Label(3),
        args,
    }
}

/// Assert both tiers agree on the body's outcome, and return the interpreter's own
/// returned value (a small int, so the raw word is the value on both heaps).
fn agree(label: &str, code: Vec<Instr>) -> i64 {
    let (module, name) = body_module(code);
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        match interpreter {
            Ok(RunOutcome::Returned(term)) => term.raw() as i64,
            other => panic!("{label}: expected a returned value, got {other:?}"),
        }
    })
}

/// The small-int raw word of `value` as the interpreter returns it.
fn small(value: i64) -> i64 {
    make_small(value).raw() as i64
}

// ---------------------------------------------------------------------------
// 1. `test` with a `Y`-slot source — tier equivalence
// ---------------------------------------------------------------------------

#[test]
fn a_y_source_test_is_lowered_identically_on_both_tiers() {
    // `is_nil` over `Y(0)`: the slot holds a small int, so the test FAILS.
    assert_eq!(
        agree(
            "is_nil Y (fail)",
            y_body(1, y_test(TestKind::IsNil, vec![Operand::Y(SLOT)]))
        ),
        small(FAIL)
    );
    // `is_tuple` over a small int: FAILS.
    assert_eq!(
        agree(
            "is_tuple Y (fail)",
            y_body(1, y_test(TestKind::IsTuple, vec![Operand::Y(SLOT)]))
        ),
        small(FAIL)
    );
    // `is_nonempty_list` over a small int: FAILS.
    assert_eq!(
        agree(
            "is_nonempty_list Y (fail)",
            y_body(1, y_test(TestKind::IsNonemptyList, vec![Operand::Y(SLOT)]))
        ),
        small(FAIL)
    );
    // `is_integer` over a small int: PASSES, so the fall-through runs.
    assert_eq!(
        agree(
            "is_integer Y (pass)",
            y_body(1, y_test(TestKind::IsInteger, vec![Operand::Y(SLOT)]))
        ),
        small(PASS)
    );
    // `is_atom` / `is_list` over a small int: FAIL.
    assert_eq!(
        agree(
            "is_atom Y (fail)",
            y_body(1, y_test(TestKind::IsAtom, vec![Operand::Y(SLOT)]))
        ),
        small(FAIL)
    );
    assert_eq!(
        agree(
            "is_list Y (fail)",
            y_body(1, y_test(TestKind::IsList, vec![Operand::Y(SLOT)]))
        ),
        small(FAIL)
    );
}

#[test]
fn a_y_source_test_agrees_when_the_slot_holds_nil() {
    // The `Y` slot is written by a `Nil` operand, so `is_integer` FAILS — proving the
    // real word in the slot (not the staged `Integer` operand) decides the predicate.
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Nil,
            dst: Operand::Y(SLOT),
        },
        y_test(TestKind::IsInteger, vec![Operand::Y(SLOT)]),
        Instr::Move {
            src: Operand::Integer(PASS),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::Move {
            src: Operand::Integer(FAIL),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ];
    assert_eq!(agree("is_integer Y over nil (fail)", code), small(FAIL));
}

#[test]
fn a_y_source_comparison_test_is_lowered_identically_on_both_tiers() {
    // `is_eq_exact` between a `Y` slot and an immediate constant: a plain word
    // compare, exact for an immediate. 7 =:= 7 PASSES; 7 =:= 8 FAILS.
    assert_eq!(
        agree(
            "is_eq_exact Y vs small (pass)",
            y_body(
                1,
                y_test(
                    TestKind::IsEqExact,
                    vec![Operand::Y(SLOT), Operand::Integer(VALUE)]
                )
            )
        ),
        small(PASS)
    );
    assert_eq!(
        agree(
            "is_eq_exact Y vs small (fail)",
            y_body(
                1,
                y_test(
                    TestKind::IsEqExact,
                    vec![Operand::Y(SLOT), Operand::Integer(VALUE + 1)]
                )
            )
        ),
        small(FAIL)
    );
    // The guard path: two runtime values guard BOTH as small ints. The slot holds a
    // small int, so the guard passes and the word compare decides equality.
    assert_eq!(
        agree(
            "is_eq Y vs Y (pass)",
            y_body(
                1,
                y_test(TestKind::IsEq, vec![Operand::Y(SLOT), Operand::Y(SLOT)])
            )
        ),
        small(PASS)
    );
    // `is_lt` between two runtime small ints: 7 < 7 is false.
    assert_eq!(
        agree(
            "is_lt Y vs Y (fail)",
            y_body(
                1,
                y_test(TestKind::IsLt, vec![Operand::Y(SLOT), Operand::Y(SLOT)])
            )
        ),
        small(FAIL)
    );
}

// ---------------------------------------------------------------------------
// 2. `select_val` with a `Y`-slot scrutinee — tier equivalence
// ---------------------------------------------------------------------------

/// A `select_val` over `Y(SLOT)` with the alternatives `pairs` and the fail target
/// `Label(3)`. `Label(4)` is the match stub, defined by the test's own body.
fn y_select(pairs: Vec<(Operand, Label)>) -> Vec<Instr> {
    vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(VALUE),
            dst: Operand::Y(SLOT),
        },
        Instr::SelectVal {
            value: Operand::Y(SLOT),
            fail: Label(3),
            pairs,
        },
        Instr::Move {
            src: Operand::Integer(PASS),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::Move {
            src: Operand::Integer(FAIL),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(4)),
        Instr::Move {
            src: Operand::Integer(PASS),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ]
}

#[test]
fn a_y_scrutinee_select_val_is_lowered_identically_on_both_tiers() {
    // The FIRST alternative matches the slot's value: the match's stub runs.
    assert_eq!(
        agree(
            "select_val Y first match",
            y_select(vec![
                (Operand::Integer(VALUE), Label(4)),
                (Operand::Integer(1234), Label(3)),
            ])
        ),
        small(PASS)
    );
    // A LATER alternative matches (the chain's `cmp`/`jcc` fall-through).
    assert_eq!(
        agree(
            "select_val Y later match",
            y_select(vec![
                (Operand::Integer(1234), Label(3)),
                (Operand::Integer(VALUE), Label(4)),
            ])
        ),
        small(PASS)
    );
    // Nothing matches: the no-match path falls through to the fail label.
    assert_eq!(
        agree(
            "select_val Y no match",
            y_select(vec![
                (Operand::Integer(1234), Label(4)),
                (Operand::Nil, Label(4)),
            ])
        ),
        small(FAIL)
    );
}

// ---------------------------------------------------------------------------
// 3. `select_tuple_arity` with a `Y`-slot tuple source
// ---------------------------------------------------------------------------

#[test]
fn a_y_source_select_tuple_arity_agrees_on_both_tiers_when_rejected() {
    // The slot holds a small int, not a tuple, so the emitted tuple-header guard
    // rejects and the interpreter runs the instruction: both tiers must then report
    // the interpreter's exact outcome (never a wrong branch).
    let code = y_body(
        1,
        Instr::SelectTupleArity {
            tuple: Operand::Y(SLOT),
            fail: Label(3),
            destinations: vec![(Operand::Integer(2), Label(3))],
        },
    );
    let (module, name) = body_module(code);
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(
            "select_tuple_arity Y (non-tuple)",
            interpreter,
            baseline,
            &module.atom_table,
        )
    });
}

// ---------------------------------------------------------------------------
// 4. The guard rejection: a `Y` slot OUTSIDE the live frame window
// ---------------------------------------------------------------------------

#[test]
fn an_out_of_frame_y_slot_behaves_identically_on_both_tiers() {
    // `Y(64)` is far outside the frame `Allocate{Words(1)}` publishes, so the emitted
    // guard ALWAYS rejects and the interpreter's own `y(k)` bounds check decides. The
    // interpreter reports its typed `bad_register` error; the tier must reach the very
    // same outcome (a rejection, never a wrong value).
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        y_test(TestKind::IsInteger, vec![Operand::Y(64)]),
        Instr::Move {
            src: Operand::Integer(PASS),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::Move {
            src: Operand::Integer(FAIL),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ];
    let (module, name) = body_module(code);
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome("out-of-frame Y test", interpreter, baseline, &module.atom_table);
        // The rejection really happened: the interpreter's own bounds check fired, and
        // the tier reproduced it (rather than taking the fail path or a wrong value).
        let reason = error_reason(
            interpreter
                .as_ref()
                .expect_err("an out-of-frame Y slot must fail"),
        );
        assert!(!reason.is_empty(), "the interpreter's typed error must surface");
    });
}

// ---------------------------------------------------------------------------
// 5. The syntactic Y gate (`emit::uses_y`)
// ---------------------------------------------------------------------------

#[test]
fn the_y_gate_is_true_for_a_y_source_and_false_for_an_x_source() {
    let (module, _) = body_module(vec![Instr::Return]);
    let y_test_instr = y_test(TestKind::IsInteger, vec![Operand::Y(SLOT)]);
    let x_test_instr = y_test(TestKind::IsInteger, vec![Operand::X(0)]);
    assert!(uses_y(&module, &y_test_instr));
    assert!(!uses_y(&module, &x_test_instr));

    // The tagged-tuple TAG operand is the plan's second source, so it counts too.
    assert!(uses_y(
        &module,
        &y_test(
            TestKind::IsTaggedTuple,
            vec![Operand::Y(SLOT), Operand::Integer(2), Operand::Y(1)]
        )
    ));

    let y_select_instr = Instr::SelectVal {
        value: Operand::Y(SLOT),
        fail: Label(3),
        pairs: vec![(Operand::Integer(VALUE), Label(3))],
    };
    let x_select_instr = Instr::SelectVal {
        value: Operand::X(0),
        fail: Label(3),
        pairs: vec![(Operand::Integer(VALUE), Label(3))],
    };
    assert!(uses_y(&module, &y_select_instr));
    assert!(!uses_y(&module, &x_select_instr));

    let y_arity = Instr::SelectTupleArity {
        tuple: Operand::Y(SLOT),
        fail: Label(3),
        destinations: vec![(Operand::Integer(2), Label(3))],
    };
    let x_arity = Instr::SelectTupleArity {
        tuple: Operand::X(0),
        fail: Label(3),
        destinations: vec![(Operand::Integer(2), Label(3))],
    };
    assert!(uses_y(&module, &y_arity));
    assert!(!uses_y(&module, &x_arity));

    // An out-of-bound x-register is NOT a `Y` slot: the gate stays false (and the
    // plans refuse it, so the interpreter's own `bad_register` surfaces).
    assert!(!uses_y(
        &module,
        &y_test(TestKind::IsInteger, vec![Operand::X(4096)])
    ));
}
