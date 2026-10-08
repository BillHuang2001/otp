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

//! The raising opcodes (`badmatch`/`case_end`/`try_case_end`/`if_end`/
//! `badrecord`), the re-raise opcode `raise`, `raw_raise`, and the
//! not-this-family fall-through.
use super::*;
// ------------------------------------------------------------------
// Raising opcodes (uncaught ⇒ Err, reason expanded + recorded)
// ------------------------------------------------------------------

#[test]
fn badmatch_raises_the_expanded_tuple_uncaught() {
    let module = Fixture::new(&["badmatch", "error"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    let err = exec_one(
        &module,
        &Instr::Badmatch {
            value: Operand::Integer(7),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)));
    let (class, reason) = machine.pending_exception().expect("a reason is recorded");
    assert_eq!(class, ExceptionClass::Error);
    assert_reason_tuple(&mut ctx, &module, reason, "badmatch", make_small(7));
}

#[test]
fn case_end_if_end_badrecord_and_try_case_end_expand_their_reasons() {
    // case_end ⇒ {case_clause,V}
    let module = Fixture::new(&["case_clause"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    let _ = exec_one(
        &module,
        &Instr::CaseEnd {
            value: Operand::Integer(2),
        },
        &mut machine,
        &mut ctx,
    );
    let (_, reason) = machine.pending_exception().unwrap();
    assert_reason_tuple(&mut ctx, &module, reason, "case_clause", make_small(2));

    // if_end ⇒ the bare atom if_clause
    let module = Fixture::new(&["if_clause"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let _ = exec_one(&module, &Instr::IfEnd, &mut machine, &mut ctx);
    let (_, reason) = machine.pending_exception().unwrap();
    assert_eq!(
        reason,
        make_atom(module.atom_table.lookup("if_clause").unwrap())
    );

    // badrecord ⇒ {badrecord,V}
    let module = Fixture::new(&["badrecord"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let _ = exec_one(
        &module,
        &Instr::Badrecord {
            value: Operand::Integer(3),
        },
        &mut machine,
        &mut ctx,
    );
    let (_, reason) = machine.pending_exception().unwrap();
    assert_reason_tuple(&mut ctx, &module, reason, "badrecord", make_small(3));

    // try_case_end ⇒ {try_clause,V}
    let module = Fixture::new(&["try_clause"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let _ = exec_one(
        &module,
        &Instr::TryCaseEnd {
            value: Operand::Integer(4),
        },
        &mut machine,
        &mut ctx,
    );
    let (_, reason) = machine.pending_exception().unwrap();
    assert_reason_tuple(&mut ctx, &module, reason, "try_clause", make_small(4));
}

#[test]
fn a_raising_opcode_without_the_tag_atom_is_a_typed_error() {
    // No `badmatch` atom interned: the module cannot name the reason.
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    let err = exec_one(
        &module,
        &Instr::Badmatch {
            value: Operand::Integer(1),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)));
    assert_eq!(machine.pending_exception(), None);
}

#[test]
fn an_uncaught_structured_raise_carries_its_live_reason_term() {
    // An **uncaught** structured raise: `throw_with_trace` keeps the reason
    // term-independently (`ExitReason::Structured`, so `as_atom()` stays `None`)
    // but rides the live reason term out of the machine in the raise's
    // `live_reason` slot, so a caller with a term representation recovers the
    // exact compound reason.
    for (instr, tag, value) in [
        (
            Instr::Badmatch {
                value: Operand::Integer(7),
            },
            "badmatch",
            make_small(7),
        ),
        (
            Instr::CaseEnd {
                value: Operand::Integer(2),
            },
            "case_clause",
            make_small(2),
        ),
    ] {
        let module = Fixture::new(&[tag]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        let mut ctx = MinimalContext::new();
        let err = exec_one(&module, &instr, &mut machine, &mut ctx).unwrap_err();

        let raise = match err {
            BeamError::Raise(raise) => raise,
            other => panic!("expected a typed raise, got {other:?}"),
        };
        assert_eq!(raise.reason, ExitReason::Structured);
        assert_eq!(raise.reason.as_atom(), None);
        let live = raise
            .live_reason()
            .and_then(|live| live.downcast_ref::<Eterm>())
            .copied()
            .expect("the live reason term rides along");
        // The recovered term is exactly the `{Tag,Value}` the machine recorded as
        // its pending exception — the term `erl` prints for the same raise.
        assert_eq!(
            machine.pending_exception().map(|(_, reason)| reason),
            Some(live)
        );
        assert_reason_tuple(&mut ctx, &module, live, tag, value);
    }
}

#[test]
fn a_caught_structured_raise_still_lands_in_the_handler() {
    // Regression guard: a **live** catch frame is unchanged — the raise unwinds
    // to the handler (`Step::Goto`) with `x(1)` = the expanded reason term, and
    // no error escapes.
    let module = Fixture::new(&["badmatch", "error"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(3, 7, 0));
    let mut ctx = MinimalContext::new();
    let step = exec_one(
        &module,
        &Instr::Badmatch {
            value: Operand::Integer(7),
        },
        &mut machine,
        &mut ctx,
    )
    .expect("a live frame catches the raise")
    .expect("the badmatch family claims the instruction");

    assert_eq!(step, Step::Goto { func: 3, pc: 7 });
    assert_eq!(machine.x(0), Some(Eterm::non_value()));
    let reason = machine.x(1).expect("x(1) holds the landed reason");
    assert_reason_tuple(&mut ctx, &module, reason, "badmatch", make_small(7));
    assert_eq!(
        machine.catch_frame_depth(),
        1,
        "the handler opcode pops the frame, not the raise"
    );
}

// ------------------------------------------------------------------
// raise (the re-raise opcode)
// ------------------------------------------------------------------

#[test]
fn raise_re_raises_with_the_pending_class() {
    let module = Fixture::new(&["throw"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));
    machine.set_class_atom(
        ExceptionClass::Throw,
        make_atom(module.atom_table.lookup("throw").unwrap()),
    );
    // Establish a pending `throw` class by landing one, then re-raise.
    machine
        .throw(ExceptionClass::Throw, make_small(1))
        .expect("a live frame catches");
    machine.set_x(1, make_small(42)).unwrap(); // raise value
    machine.set_x(2, NIL).unwrap(); // raise trace

    // Consume the frame the earlier landing left in place, then re-arm it so
    // the re-raise lands again.
    machine.pop_catch_frame();
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));

    let mut ctx = MinimalContext::new();
    let step = exec_one(
        &module,
        &Instr::Raise {
            trace: Operand::X(2),
            value: Operand::X(1),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 0 }));
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Throw, make_small(42)))
    );
}

#[test]
fn raise_without_a_pending_class_is_a_typed_error() {
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    let err = exec_one(
        &module,
        &Instr::Raise {
            trace: Operand::X(2),
            value: Operand::X(1),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)));
}

// ------------------------------------------------------------------
// documented unsupported opcodes + the not-mine fallthrough
// ------------------------------------------------------------------

// ------------------------------------------------------------------
// raw_raise (the x(0)/x(1)/x(2) class protocol)
// ------------------------------------------------------------------

#[test]
fn raw_raise_with_a_valid_class_raises_through_the_machine() {
    // C: `raw_raise` sets the class in `c_p->freason` and the value/trace in
    // `fvalue`/`ftrace`, then `goto find_func_info` — i.e. an ordinary raise.
    for class in [
        ExceptionClass::Error,
        ExceptionClass::Exit,
        ExceptionClass::Throw,
    ] {
        let module = Fixture::new(&[class.as_str()]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(4, 9, 0));
        let class_atom = make_atom(module.atom_table.lookup(class.as_str()).unwrap());
        machine.set_x(0, class_atom).unwrap();
        machine.set_x(1, make_small(42)).unwrap();
        machine.set_x(2, NIL).unwrap();

        let mut ctx = MinimalContext::new();
        let step = exec_one(&module, &Instr::RawRaise, &mut machine, &mut ctx).unwrap();
        assert_eq!(
            step,
            Some(Step::Goto { func: 4, pc: 9 }),
            "raw_raise with class {} lands in the live catch frame",
            class.as_str()
        );
        assert_eq!(machine.pending_exception(), Some((class, make_small(42))));
        // The landing convention: reason in x(1), the trace in x(2), the class
        // atom in x(3).
        assert_eq!(machine.x(0), Some(Eterm::non_value()));
        assert_eq!(machine.x(1), Some(make_small(42)));
        assert_eq!(machine.x(2), Some(NIL));
        assert_eq!(machine.x(3), Some(class_atom));
    }
}

#[test]
fn raw_raise_uncaught_propagates_a_typed_error() {
    let module = Fixture::new(&["throw"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine
        .set_x(0, make_atom(module.atom_table.lookup("throw").unwrap()))
        .unwrap();
    machine.set_x(1, make_small(7)).unwrap();
    machine.set_x(2, NIL).unwrap();

    let mut ctx = MinimalContext::new();
    let err = exec_one(&module, &Instr::RawRaise, &mut machine, &mut ctx).unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)));
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Throw, make_small(7)))
    );
}

#[test]
fn raw_raise_with_an_unknown_class_sets_x0_to_badarg_and_continues() {
    // C: an unknown class makes `raw_raise` return 1 and the instruction stores
    // `am_badarg` in x(0) **without raising** (`beam_common.c:1176-1178`,
    // `emu/instrs.tab:1258-1260`) — the oracle-witnessed `badclass/1` value.
    // A non-class *atom*, then a term that is not an atom at all.
    for unknown in [Some("nonsense"), None] {
        let module = Fixture::new(&["badarg", "nonsense"]).finish();
        let mut machine = Machine::new();
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(4, 9, 0));
        machine.set_x(1, make_small(1)).unwrap();
        machine.set_x(2, NIL).unwrap();
        let unknown_term = match unknown {
            Some(name) => make_atom(module.atom_table.lookup(name).unwrap()),
            None => make_small(3),
        };
        machine.set_x(0, unknown_term).unwrap();

        let mut ctx = MinimalContext::new();
        let step = exec_one(&module, &Instr::RawRaise, &mut machine, &mut ctx).unwrap();
        assert_eq!(step, Some(Step::Next), "an unknown class does not raise");
        assert_eq!(
            machine.x(0),
            Some(make_atom(module.atom_table.lookup("badarg").unwrap()))
        );
        // Nothing was raised: no pending exception, and the catch frame is
        // still live for the next instruction.
        assert_eq!(machine.pending_exception(), None);
        assert_eq!(machine.catch_frame_depth(), 1);
    }
}

#[test]
fn raw_raise_without_the_badarg_atom_is_a_typed_error() {
    // The engine may not substitute a different atom for `badarg`.
    let module = Fixture::new(&["nonsense"]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine
        .set_x(0, make_atom(module.atom_table.lookup("nonsense").unwrap()))
        .unwrap();
    let mut ctx = MinimalContext::new();
    let err = exec_one(&module, &Instr::RawRaise, &mut machine, &mut ctx).unwrap_err();
    assert!(matches!(err, BeamError::Raise(_)));
    assert_eq!(machine.pending_exception(), None);
}

#[test]
fn a_foreign_instruction_returns_none() {
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    let step = exec_one(
        &module,
        &Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        &mut machine,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, None);
}
