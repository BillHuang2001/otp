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

//! Cross-cutting end-to-end tests of the machine's **uniform raise funnel**.
//!
//! The funnel's two halves ([`Machine::with_uncaught_frames`] at the
//! `step_instruction` boundary, and again at the `Machine::drive` boundary for a
//! raise that surfaced inside a cross-module callee) are unit-tested beside their
//! implementation in `../machine/catch.rs`. What those tests cannot show is that a
//! *real run* of a real run entry propagates each raise kind with the right
//! frames: that is what this module pins. Every test builds a synthetic [`Module`],
//! runs it through a real entry ([`Machine::run_label_outcome`] /
//! [`Machine::run_export_outcome`], which drive [`Machine::drive`]) and asserts the
//! propagated `Err(BeamError::Raise(r))` names the raising function and every live
//! caller, innermost first — plus the guard that a **caught** raise is unchanged.
//! Only derivable facts are asserted (the module/function/arity of a frame and
//! their order); a name the module cannot resolve is never invented here.
//!
//! The fixture scaffolding is shared with `drive.rs` via [`super::test_support`].

use std::sync::Arc;

use beam_common::{BeamError, ExceptionClass, ExitReason, Raise, StackTrace};
use beam_loader::{Atom, HeapNeed, Instr, Label, Operand, NO_FAIL};
use beam_term::{cmp, make_atom, make_small, Eterm, NIL};

use super::test_support::{seam, Fixture};
use super::{CatchFrame, Continuation, Machine, RunOutcome};
use crate::context::MinimalContext;
use crate::Step;

// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so alias
// `terms` to the test-only wrappers that keep the old ergonomic arity.
use crate::terms::test_support as terms;

/// The `(module, function, arity)` name triples of a stacktrace, innermost first.
fn frame_names(trace: &StackTrace) -> Vec<(&str, &str, u8)> {
    trace
        .frames
        .iter()
        .map(|frame| (frame.module.as_str(), frame.function.as_str(), frame.arity))
        .collect()
}

/// Run `entry` (a run entry on a fresh machine) and return the propagated
/// **uncaught** raise.
///
/// Panics when the run did not raise, so a test can never pass by observing a
/// different outcome than the one it means to pin.
fn uncaught_raise(
    entry: impl FnOnce(&mut Machine) -> beam_common::Result<RunOutcome>,
) -> Raise {
    let mut machine = Machine::new();
    match entry(&mut machine) {
        Err(BeamError::Raise(raise)) => raise,
        other => panic!("expected an uncaught raise, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 1. `throw` — the `raw_raise` class protocol with the `throw` class atom
// ---------------------------------------------------------------------------

#[test]
fn a_throw_raise_names_the_raising_function() {
    // f() -> throw(7). The `raw_raise` register protocol: x(0) = the class atom,
    // x(1) = the value, x(2) = the raw stacktrace.
    let mut fixture = Fixture::new("m");
    let throw = fixture.atom("throw");
    let entry = fixture.add_function(
        "f",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Atom(throw),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Nil,
                dst: Operand::X(2),
            },
            Instr::RawRaise,
        ],
    );
    fixture.add_export("f", 0, entry);
    let f = fixture.atom("f");
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();

    let raise = uncaught_raise(|m| m.run_export_outcome(&module, f, 0, &[], &mut ctx));
    assert_eq!(raise.class, ExceptionClass::Throw, "the class is kept");
    assert_eq!(frame_names(&raise.stacktrace), vec![("m", "f", 0)]);
    // The synthetic module has no `Line` chunk, so no location is claimed.
    assert_eq!(raise.stacktrace.frames[0].location, None);
}

// ---------------------------------------------------------------------------
// 2. `error` — a family raise expanding a reason tuple (`case_end`)
// ---------------------------------------------------------------------------

#[test]
fn an_error_raise_names_the_raising_function() {
    // f() -> case 1 of 2 -> ok end ⇒ error:{case_clause,1}.
    let mut fixture = Fixture::new("m");
    fixture.atom("case_clause");
    fixture.atom("error");
    let entry = fixture.add_function(
        "f",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::CaseEnd {
                value: Operand::Integer(1),
            },
        ],
    );
    fixture.add_export("f", 0, entry);
    let f = fixture.atom("f");
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();

    let raise = uncaught_raise(|m| m.run_export_outcome(&module, f, 0, &[], &mut ctx));
    assert_eq!(raise.class, ExceptionClass::Error);
    assert_eq!(frame_names(&raise.stacktrace), vec![("m", "f", 0)]);
}

// ---------------------------------------------------------------------------
// 3. `exit` — the same `raw_raise` protocol with the `exit` class atom
// ---------------------------------------------------------------------------

#[test]
fn an_exit_raise_names_the_raising_function() {
    // f() -> exit(7), folded by the compiler into `raw_raise` with class `exit`.
    let mut fixture = Fixture::new("m");
    let exit = fixture.atom("exit");
    let entry = fixture.add_function(
        "f",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Atom(exit),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Nil,
                dst: Operand::X(2),
            },
            Instr::RawRaise,
        ],
    );
    fixture.add_export("f", 0, entry);
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();

    let raise = uncaught_raise(|m| m.run_label_outcome(&module, entry, 0, &[], &mut ctx));
    assert_eq!(raise.class, ExceptionClass::Exit);
    assert_eq!(frame_names(&raise.stacktrace), vec![("m", "f", 0)]);
}

// ---------------------------------------------------------------------------
// 4. A BIF error — `raise_bif_error` over a no-fail-label `bif`
// ---------------------------------------------------------------------------

#[test]
fn a_bif_error_raise_names_the_calling_function() {
    // f() -> erlang:error(7), where the host BIF hook raises the no-clause shape a
    // `beam_bifs`-backed failure uses (a `Raise` whose reason is an atom).
    let mut fixture = Fixture::new("m");
    let bif = fixture.import("erlang", "error", 1);
    let entry = fixture.add_function(
        "f",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Bif {
                arity: 1,
                bif,
                fail: NO_FAIL,
                args: vec![Operand::Integer(7)],
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.add_export("f", 0, entry);
    let f = fixture.atom("f");
    let module = fixture.finish();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|_module, _function, _arity, _args| {
        Err(BeamError::Raise(Raise::error(ExitReason::atom("badarg"))))
    }));

    let raise = uncaught_raise(|m| m.run_export_outcome(&module, f, 0, &[], &mut ctx));
    assert_eq!(frame_names(&raise.stacktrace), vec![("m", "f", 0)]);
}

/// A bare **reason variant** (not a [`BeamError::Raise`]) is left untouched by the
/// funnel — the same `bif` call with a host hook returning `BadArg` propagates as
/// `BadArg`, with no frames invented for an error that is not a raise.
#[test]
fn a_bare_bif_error_variant_propagates_unchanged() {
    let mut fixture = Fixture::new("m");
    let bif = fixture.import("erlang", "error", 1);
    let entry = fixture.add_function(
        "f",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Bif {
                arity: 1,
                bif,
                fail: NO_FAIL,
                args: vec![Operand::Integer(7)],
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.add_export("f", 0, entry);
    let f = fixture.atom("f");
    let module = fixture.finish();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|_module, _function, _arity, _args| {
        Err(BeamError::BadArg)
    }));

    let mut machine = Machine::new();
    let err = machine
        .run_export_outcome(&module, f, 0, &[], &mut ctx)
        .expect_err("the BIF error propagates");
    assert!(
        matches!(err, BeamError::BadArg),
        "a reason variant is not a raise, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// 5. `func_info` — the failed head-clause/guard raise (`function_clause`)
// ---------------------------------------------------------------------------

#[test]
fn a_func_info_raise_names_the_clause_failing_function() {
    // A failed head pattern jumps into `func_info M F A`, which raises class
    // `error` with the bare atom `function_clause`.
    let mut fixture = Fixture::new("m");
    fixture.atom("function_clause");
    fixture.atom("error");
    let name = fixture.atom("f");
    let module_atom = fixture.atom("m");
    let entry = fixture.add_function(
        "f",
        2,
        vec![
            Instr::Label(Label(1)),
            Instr::FuncInfo {
                module: module_atom,
                function: name,
                arity: 2,
            },
            Instr::Return,
        ],
    );
    fixture.add_export("f", 2, entry);
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();

    let raise =
        uncaught_raise(|m| m.run_export_outcome(&module, name, 2, &[make_small(1), make_small(2)], &mut ctx));
    assert_eq!(raise.class, ExceptionClass::Error);
    assert_eq!(frame_names(&raise.stacktrace), vec![("m", "f", 2)]);
}

// ---------------------------------------------------------------------------
// 6. A nested call — every live caller frame is named, innermost first
// ---------------------------------------------------------------------------

#[test]
fn a_nested_raise_names_the_callers_innermost_first() {
    // a() -> b(), b() -> c(), c() -> case 1 of 2 -> ok end. Each non-leaf caller
    // allocates its own frame, so all three functions are live when `c` raises.
    let mut fixture = Fixture::new("m");
    fixture.atom("case_clause");
    fixture.atom("error");
    let a = fixture.add_function(
        "a",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Call {
                arity: 0,
                target: Label(20),
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    fixture.add_export("a", 0, a);
    fixture.add_function(
        "b",
        0,
        vec![
            Instr::Label(Label(20)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Call {
                arity: 0,
                target: Label(30),
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    fixture.add_function(
        "c",
        0,
        vec![
            Instr::Label(Label(30)),
            Instr::CaseEnd {
                value: Operand::Integer(1),
            },
        ],
    );
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();

    let raise = uncaught_raise(|m| m.run_label_outcome(&module, a, 0, &[], &mut ctx));
    assert_eq!(
        frame_names(&raise.stacktrace),
        vec![("m", "c", 0), ("m", "b", 0), ("m", "a", 0)],
        "the raising function, then its callers innermost first"
    );
}

// ---------------------------------------------------------------------------
// 7. A cross-module callee — the frame set is derived at the `drive` boundary
// ---------------------------------------------------------------------------

#[test]
fn a_raise_inside_a_cross_module_callee_names_the_callee_and_its_caller() {
    // m:f() -> c:boom(), c:boom() -> case 1 of 2 -> ok end. The callee runs in the
    // same machine (a non-zero module context), so `step_instruction` leaves the
    // raise unframed and `Machine::drive` derives the frames — naming the callee's
    // function and the caller's, each against its own module.
    let mut caller_fixture = Fixture::new("m");
    let call = caller_fixture.import("c", "boom", 0);
    let entry = caller_fixture.add_function(
        "f",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::CallExt {
                target: call,
                arity: 0,
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    caller_fixture.add_export("f", 0, entry);
    let f = caller_fixture.atom("f");
    let caller = caller_fixture.finish();

    let mut callee_fixture = Fixture::new("c");
    callee_fixture.atom("case_clause");
    callee_fixture.atom("error");
    let boom = callee_fixture.add_function(
        "boom",
        0,
        vec![
            Instr::Label(Label(50)),
            Instr::CaseEnd {
                value: Operand::Integer(1),
            },
        ],
    );
    callee_fixture.add_export("boom", 0, boom);
    let callee = Arc::new(callee_fixture.finish());

    let mut ctx = seam(vec![("c".to_string(), Arc::clone(&callee))]);
    let raise = uncaught_raise(|m| m.run_export_outcome(&caller, f, 0, &[], &mut ctx));
    assert_eq!(
        frame_names(&raise.stacktrace),
        vec![("c", "boom", 0), ("m", "f", 0)]
    );
}

// ---------------------------------------------------------------------------
// Guard: a caught raise is byte-identical to before the frames existed
// ---------------------------------------------------------------------------

/// With a live `try`/`catch` frame the raise **lands** — the run returns the
/// handler's value, the handler opcode consumes the catch frame, and no frames are
/// ever observable (the funnel only decorates an *uncaught* raise).
#[test]
fn a_caught_raise_lands_unchanged_and_exposes_no_frames() {
    // f() -> try (case 1 of 2 -> ok end) catch error:{case_clause,_} -> _ end: the
    // handler moves x(1) (the landed reason) into x(0) and returns it.
    let mut fixture = Fixture::new("m");
    fixture.atom("case_clause");
    fixture.atom("error");
    let entry = fixture.add_function(
        "f",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(2),
            },
            Instr::CaseEnd {
                value: Operand::Integer(1),
            },
            Instr::Label(Label(2)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
        ],
    );
    fixture.add_export("f", 0, entry);
    let module = fixture.finish();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();

    let outcome = machine
        .run_label_outcome(&module, entry, 0, &[], &mut ctx)
        .expect("a caught raise never propagates");
    let RunOutcome::Returned(value) = outcome else {
        panic!("expected the handler's return value, got {outcome:?}");
    };
    // The landed reason is `{case_clause,1}`, exactly as before the frames existed.
    let expected = terms::make_tuple(
        &mut ctx,
        &[
            make_atom(module.atom_table.lookup("case_clause").unwrap()),
            make_small(1),
        ],
    )
    .unwrap();
    assert!(cmp(value, expected, &module.atom_table).is_eq());
    // The handler consumed its catch frame; no exception is in flight.
    assert_eq!(machine.catch_frame_depth(), 0, "the handler popped its frame");
}

// ---------------------------------------------------------------------------
// The raise-time snapshot: the pre-unwind frame chain
// ---------------------------------------------------------------------------

/// A raise captures the frame chain **before** it unwinds, so the frames the raise
/// unwound out of are still nameable when the handler's `build_stacktrace` runs.
///
/// The snapshot is the engine's replacement for C's saved raw `StackTrace`
/// (`c_p->ftrace`): the raising frame first, then every live caller innermost-first
/// as `(context, func, pc)`.
#[test]
fn a_raise_snapshots_the_pre_unwind_frame_chain() {
    let mut machine = Machine::new();
    // A caller chain: the bottom frame, then F suspended in a call (resumes at
    // `(7, 3)`) and G suspended in a call (resumes at `(9, 1)`).
    machine.push_frame(0).unwrap();
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(Continuation::Address {
            func: 7,
            pc: 3,
            context: 0,
        })
        .unwrap();
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(Continuation::Address {
            func: 9,
            pc: 1,
            context: 0,
        })
        .unwrap();
    // `step_instruction` records `current_code` at the raising instruction; here it
    // is the function at index 5, code offset 11, in the entry context (0).
    machine.current_code = Some((5, 11));

    // A catch frame owned by frame 1: landing there unwinds frame 2 (G) away.
    machine.push_catch_frame(CatchFrame::new(20, 2, 1));
    machine.set_class_atom(ExceptionClass::Error, make_atom(Atom::from_raw(11)));

    let step = machine
        .throw_with_trace(ExceptionClass::Error, Eterm::from_raw(1), NIL)
        .expect("a live frame catches");
    assert_eq!(step, Step::Goto { func: 20, pc: 2 });

    // The raise unwound frame 2 away…
    assert_eq!(machine.frame_depth(), 2, "the raise unwound frame 2 away");
    // …but the snapshot still holds the chain as it was *before* the unwind: the
    // raising function `(5, 11)` first, then every caller innermost-first — G's
    // frame `(9, 1)`, which the unwind dropped, is still named.
    assert_eq!(machine.raise_snapshot(), &[(0, 5, 11), (0, 9, 1), (0, 7, 3)]);
}

/// The snapshot is refreshed on the **uncaught** path too: the refresh happens
/// before the early `Err(BeamError::Raise)` return, so the uncaught funnel
/// (`Machine::with_uncaught_frames`) can name the same chain.
#[test]
fn an_uncaught_raise_snapshots_the_frame_chain() {
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(Continuation::Address {
            func: 7,
            pc: 3,
            context: 0,
        })
        .unwrap();
    machine.current_code = Some((5, 11));

    let err = machine
        .throw(ExceptionClass::Exit, Eterm::from_raw(2))
        .expect_err("no catch frame");
    assert!(matches!(err, BeamError::Raise(_)));
    assert_eq!(machine.raise_snapshot(), &[(0, 5, 11), (0, 7, 3)]);
}

/// The reset-vs-resume split the module contexts follow applies to the snapshot
/// too: `Machine::reset` (the start of every fresh run) clears it, while a resume
/// ([`Machine::resume_outcome`]) preserves it.
#[test]
fn reset_clears_the_raise_snapshot_but_a_resume_preserves_it() {
    // A module whose only function returns immediately, to resume into.
    let mut fixture = Fixture::new("m");
    fixture.add_function("f", 0, vec![Instr::Label(Label(1)), Instr::Return]);
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.current_code = Some((5, 3));
    // An uncaught raise refreshes the snapshot (nothing is unwound here).
    let _ = machine
        .throw(ExceptionClass::Throw, Eterm::from_raw(0x99))
        .expect_err("no catch frame");
    assert_eq!(machine.raise_snapshot(), &[(0, 5, 3)]);

    // A resume is not a reset: the snapshot survives re-entering the machine.
    let outcome = machine
        .resume_outcome(&module, 0, 0, &mut ctx)
        .expect("the resumed function returns");
    assert!(matches!(outcome, RunOutcome::Returned(_)));
    assert_eq!(
        machine.raise_snapshot(),
        &[(0, 5, 3)],
        "a resume must not clear the snapshot"
    );

    // A reset — the start of every fresh run — clears it.
    machine.reset();
    assert!(
        machine.raise_snapshot().is_empty(),
        "reset clears the snapshot"
    );
}
