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

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use beam_common::{BeamError, ExceptionClass, ExitReason, Raise, StackTrace};
use beam_loader::{Atom, HeapNeed, Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{cmp, make_atom, make_small, Eterm, NIL};

use super::test_support::{seam, Fixture};
use super::{CatchFrame, Continuation, Machine, RunOutcome};
use crate::context::MinimalContext;
use crate::Step;

use super::local_callee::Verdict;

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
    machine.set_current_code(5, 11);

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
    machine.set_current_code(5, 11);

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
    machine.set_current_code(5, 3);
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

// ---------------------------------------------------------------------------
// The local-call memo (`Machine::local_callees`): identity + host verdict
// ---------------------------------------------------------------------------
/// The served callee's entry label.
const HELPER: Label = Label(5);
/// The second (different) callee's entry label.
const HELPER2: Label = Label(6);

/// A module with **two** consecutive local (non-tail) calls to the same non-exported
/// callee, plus a `call_last` form:
///
/// ```text
/// run/0:       label 1;  allocate 0,0; call helper; call helper; deallocate 0; return
/// run_last/0:  label 10; allocate 2,0; move x0 -> y0; call_last helper 2; return
/// helper/0:    label 5;  allocate 0,0; move 99 -> x0; deallocate 0; return
/// ```
///
/// `helper/0` is never exported, so a local `call`/`call_last` is the only way to reach
/// it — the shape the local-call seam exists for. Its **in-machine** body answers `99`
/// while a serving host answers `7`, so which route ran is visible in the value.
fn two_call_module() -> Module {
    let mut fixture = Fixture::new("m");
    let run = fixture.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Call {
                target: HELPER,
                arity: 0,
            },
            Instr::Call {
                target: HELPER,
                arity: 0,
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    let run_last = fixture.add_function(
        "run_last",
        0,
        vec![
            Instr::Label(Label(10)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(2),
                live: 0,
            },
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::Y(0),
            },
            Instr::CallLast {
                target: HELPER,
                arity: 0,
                deallocate: 2,
            },
            Instr::Return,
        ],
    );
    fixture.add_function(
        "helper",
        0,
        vec![
            Instr::Label(HELPER),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Move {
                src: Operand::Integer(99),
                dst: Operand::X(0),
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    fixture.add_export("run", 0, run);
    fixture.add_export("run_last", 0, run_last);
    fixture.finish()
}

/// A module with one call to **each** of two distinct non-exported callees:
///
/// ```text
/// run/0:      label 1; allocate 0,0; call helper; call helper2; deallocate 0; return
/// helper/0:   label 5; move 99 -> x0; return
/// helper2/0:  label 6; move 98 -> x0; return
/// ```
fn two_callee_module() -> Module {
    let mut fixture = Fixture::new("m");
    let run = fixture.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Call {
                target: HELPER,
                arity: 0,
            },
            Instr::Call {
                target: HELPER2,
                arity: 0,
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    fixture.add_function(
        "helper",
        0,
        vec![
            Instr::Label(HELPER),
            Instr::Move {
                src: Operand::Integer(99),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.add_function(
        "helper2",
        0,
        vec![
            Instr::Label(HELPER2),
            Instr::Move {
                src: Operand::Integer(98),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.add_export("run", 0, run);
    fixture.finish()
}

/// A module whose `run/0` calls `helper/0`, dispatches `erlang:load_nif/2`, and calls
/// `helper/0` again — the one shape that makes the host's answer set change mid-run:
///
/// ```text
/// run/0:     label 1; allocate 0,0; call helper; move 0 -> x0;
///            move 0 -> x1; call_ext erlang:load_nif/2; call helper;
///            deallocate 0; return
/// helper/0:  label 5; move 99 -> x0; return
/// ```
fn load_nif_module() -> Module {
    let mut fixture = Fixture::new("m");
    let load_nif = fixture.import("erlang", "load_nif", 2);
    let run = fixture.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Call {
                target: HELPER,
                arity: 0,
            },
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(1),
            },
            Instr::CallExt {
                target: load_nif,
                arity: 2,
            },
            Instr::Call {
                target: HELPER,
                arity: 0,
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    fixture.add_function(
        "helper",
        0,
        vec![
            Instr::Label(HELPER),
            Instr::Move {
                src: Operand::Integer(99),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.add_export("run", 0, run);
    fixture.finish()
}

/// The atom `name` in `module`'s own table.
fn atom_of(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .expect("atom is interned in the fixture")
}

/// What one seam observation recorded.
#[derive(Default)]
struct SeamObservations {
    /// How many times the host's `resolve_local_call` was consulted.
    consultations: Rc<Cell<usize>>,
    /// The `(module, function, arity)` triples the host was asked about, in order — the
    /// identity the seam derived from the callee's **own** function entry.
    asked: Rc<std::cell::RefCell<Vec<(String, String, u32)>>>,
    /// How many times the host's BIF hook was called.
    bif_calls: Rc<Cell<usize>>,
}

impl SeamObservations {
    /// The triples the host was asked about.
    fn asked(&self) -> Vec<(String, String, u32)> {
        self.asked.borrow().clone()
    }
}

/// The host for the seam tests: a BIF hook that serves `m:helper/0` as `7` (and
/// `erlang:load_nif/2` as a small int the fixture's caller discards), and a
/// `resolve_local_call` resolver that is consulted (and counted) whenever the seam asks,
/// answering `serve` for `m:helper/0`.
fn seam_context(serve: bool, observations: &SeamObservations) -> MinimalContext {
    let bif_calls = Rc::clone(&observations.bif_calls);
    let mut ctx = MinimalContext::with_bif_hook(Box::new(
        move |module: &str, function: &str, arity: u32, _args: &[Eterm]| -> beam_common::Result<Eterm> {
            bif_calls.set(bif_calls.get() + 1);
            match (module, function, arity) {
                ("m", "helper", 0) => Ok(make_small(7)),
                ("erlang", "load_nif", 2) => Ok(make_small(0)),
                _ => Err(crate::error::unsupported("seam test: no such BIF")),
            }
        },
    ));
    let consultations = Rc::clone(&observations.consultations);
    let asked = Rc::clone(&observations.asked);
    ctx.set_local_call_resolver(Box::new(move |module: &str, function: &str, arity: u32| {
        consultations.set(consultations.get() + 1);
        asked
            .borrow_mut()
            .push((module.to_string(), function.to_string(), arity));
        Ok(serve && module == "m" && function == "helper" && arity == 0)
    }));
    ctx
}

/// A host whose `resolve_local_call` answers with a **typed error** — a stateful host that
/// cannot decide yet. A failed consultation must not leave a verdict behind.
fn erroring_seam_context(observations: &SeamObservations) -> MinimalContext {
    let mut ctx = seam_context(false, observations);
    let consultations = Rc::clone(&observations.consultations);
    ctx.set_local_call_resolver(Box::new(move |_module, _function, _arity| {
        consultations.set(consultations.get() + 1);
        Err(crate::error::unsupported(
            "seam test: the host cannot answer yet",
        ))
    }));
    ctx
}

/// The verdict the machine's **current** module context holds for function index `callee`.
fn verdict_of(machine: &Machine, callee: u32) -> Verdict {
    machine
        .verdict_table()
        .expect("the executing context has a verdict table")
        .get(callee)
}

/// How many callee-identity derivations the **current** context's table has paid.
fn derivations(machine: &Machine) -> u64 {
    machine
        .verdict_table()
        .expect("the executing context has a verdict table")
        .derivation_count()
}

/// Run `export/0` of a fixture and report the value plus the observations.
fn run_fixture(
    module: &Module,
    export: &str,
    serve: bool,
) -> (RunOutcome, SeamObservations, Machine) {
    let observations = SeamObservations::default();
    let mut ctx = seam_context(serve, &observations);
    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(module, atom_of(module, export), 0, &[], &mut ctx)
        .expect("the fixture runs");
    (outcome, observations, machine)
}

/// **(a) The host is consulted once per distinct callee per run.** Two consecutive local
/// calls to the same callee ask `resolve_local_call` **once** and derive the callee's
/// identity **once**, then answer both calls from the table's slot: the host's verdict for
/// a fixed `(module, function, arity)` triple is a pure function of that triple, and ERTS
/// itself charges nothing for an intra-module call, so caching is the faithful choice.
#[test]
fn the_local_call_seam_consults_the_host_once_per_callee_and_derives_the_identity_once() {
    let module = two_call_module();
    let (outcome, observations, machine) = run_fixture(&module, "run", false);
    // A declined callee runs in this machine, so the caller's two calls both reach the
    // in-machine body (99).
    assert_eq!(outcome, RunOutcome::Returned(make_small(99)));
    assert_eq!(
        observations.consultations.get(),
        1,
        "one `resolve_local_call` consultation per distinct callee, not per call"
    );
    assert_eq!(observations.bif_calls.get(), 0, "a declined callee is not a BIF");
    assert_eq!(
        derivations(&machine),
        1,
        "the identity is derived once and answered from the table's slot on the second call"
    );
    // The identity the seam derived is the callee's **own**: its function entry's module
    // atom and name atom, and its declared arity.
    assert_eq!(
        observations.asked(),
        vec![("m".to_string(), "helper".to_string(), 0)],
        "the host was asked about the callee's own triple"
    );
    // The verdict is filed in the slot the callee's **function index** names — `helper` is
    // function index 2 of the fixture.
    assert_eq!(
        verdict_of(&machine, 2),
        Verdict::Declined,
        "the host's answer is filed under the callee's function index"
    );
}

/// **(b) A call to a different callee is consulted again, and never reads the other
/// callee's slot** — the table is index-addressed, so one callee's verdict can never
/// answer another's question.
#[test]
fn a_different_local_callee_is_consulted_again() {
    let module = two_callee_module();
    let (outcome, observations, machine) = run_fixture(&module, "run", false);
    // The second call's value flows out, and each callee ran in this machine.
    assert_eq!(outcome, RunOutcome::Returned(make_small(98)));
    assert_eq!(
        observations.consultations.get(),
        2,
        "two distinct callees are two questions"
    );
    assert_eq!(derivations(&machine), 2, "two identities");
    assert_eq!(
        verdict_of(&machine, func_of(&module, HELPER)),
        Verdict::Declined
    );
    assert_eq!(
        verdict_of(&machine, func_of(&module, HELPER2)),
        Verdict::Declined
    );
}

/// A callee the module cannot **name** (no function entry, or a name atom its atom table
/// does not resolve) costs the host **zero** consultations, however often it is called: the
/// table records [`Verdict::Unnameable`] and the seam never asks about a fabricated name.
#[test]
fn an_unnameable_callee_is_never_consulted() {
    let mut module = two_call_module();
    // `helper` is function index 2; `Atom::MAX` is the "no such atom" id, so the callee's
    // name cannot be resolved from the module's own atom table.
    module.functions[2].name = Atom::MAX;
    let (outcome, observations, machine) = run_fixture(&module, "run", false);
    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(99)),
        "the un-nameable callee still runs in this machine"
    );
    assert_eq!(
        observations.consultations.get(),
        0,
        "an un-nameable callee is never asked about"
    );
    assert!(observations.asked().is_empty(), "no name was fabricated");
    assert_eq!(
        verdict_of(&machine, 2),
        Verdict::Unnameable,
        "the un-nameable verdict is filed once"
    );
    assert_eq!(derivations(&machine), 0, "there was no identity to derive");
}

/// A host **error** is not a verdict: the failed consultation leaves the slot
/// [`Verdict::Unknown`], so the next call asks again instead of answering from a failure.
#[test]
fn a_host_error_is_not_memoized_as_a_verdict() {
    let module = two_call_module();
    let observations = SeamObservations::default();
    let mut ctx = erroring_seam_context(&observations);
    let mut machine = Machine::new();
    let err = machine
        .run_export_outcome(&module, atom_of(&module, "run"), 0, &[], &mut ctx)
        .expect_err("the host's error propagates out of the seam");
    assert!(
        matches!(err, BeamError::Raise(_)),
        "the host's typed error is raised, not swallowed: {err:?}"
    );
    assert_eq!(observations.consultations.get(), 1);
    assert_eq!(
        verdict_of(&machine, 2),
        Verdict::Unknown,
        "a failed consultation leaves no verdict behind"
    );
    assert_eq!(derivations(&machine), 1, "the identity was derived once");
}

/// **(c) `Machine::invalidate_local_call_verdicts` forces a re-consultation.** This is the
/// door a host with a stateful answer set calls (`erlang:load_nif/2` reaches it through the
/// BIF-dispatch funnel, and the dedicated case below pins that route end to end).
#[test]
fn invalidating_the_verdicts_forces_a_re_consultation() {
    let module = two_call_module();
    let observations = SeamObservations::default();
    let mut ctx = seam_context(false, &observations);
    let mut machine = Machine::new();
    let name = atom_of(&module, "run");
    machine
        .run_export_outcome(&module, name, 0, &[], &mut ctx)
        .expect("the first run completes");
    assert_eq!(observations.consultations.get(), 1);

    // Invalidate, then run again on the **same** machine: the seam must ask again.
    machine.invalidate_local_call_verdicts();
    assert_eq!(
        verdict_of(&machine, 2),
        Verdict::Unknown,
        "invalidation resets the slot to `Unknown`"
    );
    let outcome = machine
        .run_export_outcome(&module, name, 0, &[], &mut ctx)
        .expect("the second run completes");
    assert_eq!(outcome, RunOutcome::Returned(make_small(99)));
    assert_eq!(
        observations.consultations.get(),
        2,
        "an invalidated verdict is re-asked"
    );
}

/// **(d) `Machine::reset` forces a re-consultation**: the tables belong to the run that
/// filled them, so a new run must not answer from a previous one.
#[test]
fn resetting_the_machine_forces_a_re_consultation_and_a_re_derivation() {
    let module = two_call_module();
    let (outcome, observations, machine) = run_fixture(&module, "run", false);
    assert_eq!(outcome, RunOutcome::Returned(make_small(99)));
    assert_eq!(derivations(&machine), 1);
    assert_eq!(observations.consultations.get(), 1);
    let mut machine = machine;
    machine.reset();
    assert_eq!(
        verdict_of(&machine, 2),
        Verdict::Unknown,
        "`reset` drops the entry module's table"
    );
    assert_eq!(derivations(&machine), 0, "… into a fresh, all-unknown table");
    let observations = SeamObservations::default();
    let mut ctx = seam_context(false, &observations);
    machine
        .run_export_outcome(&module, atom_of(&module, "run"), 0, &[], &mut ctx)
        .expect("the fixture runs again");
    assert_eq!(
        derivations(&machine),
        1,
        "the reset dropped the table, so the new run derived again"
    );
    assert_eq!(
        observations.consultations.get(),
        1,
        "… and consulted the host again"
    );
}

/// **(e) The seam is still consulted at least once, so a NIF-stub route still works** —
/// and it is consulted exactly once per callee even though the callee is *served* twice.
#[test]
fn a_served_local_callee_is_dispatched_once_per_call_from_the_memoized_verdict() {
    let module = two_call_module();
    let (outcome, observations, machine) = run_fixture(&module, "run", true);
    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(7)),
        "a served callee's BIF value flows out (the in-machine body answers 99)"
    );
    assert_eq!(
        observations.consultations.get(),
        1,
        "the host is asked once, not once per call"
    );
    assert_eq!(
        observations.bif_calls.get(),
        2,
        "and its answer is honoured once per call"
    );
    assert_eq!(
        derivations(&machine),
        1,
        "the identity derivation is paid once, on the asking call"
    );
    assert_eq!(
        verdict_of(&machine, 2),
        Verdict::Served,
        "a served verdict is filed too"
    );
}

/// The `call_last` arm keeps its frame discipline when the verdict comes from the table:
/// the served route's resting depths equal the in-machine route's (the frame pop is the
/// discriminant — a skipped pop leaves the caller's two Y slots on the stack).
#[test]
fn a_served_local_tail_call_still_pops_its_frame_with_the_memoized_verdict() {
    let module = two_call_module();
    let (declined, declined_observations, declined_machine) =
        run_fixture(&module, "run_last", false);
    assert_eq!(declined, RunOutcome::Returned(make_small(99)));
    assert_eq!(declined_observations.consultations.get(), 1);
    assert_eq!(declined_observations.bif_calls.get(), 0);
    assert_eq!(derivations(&declined_machine), 1);
    assert_eq!(
        (
            declined_machine.frame_depth(),
            declined_machine.stack_depth()
        ),
        (1, 0),
        "`call_last` pops its two-Y-slot frame before the callee runs"
    );
    let (served, served_observations, served_machine) =
        run_fixture(&module, "run_last", true);
    assert_eq!(served, RunOutcome::Returned(make_small(7)));
    assert_eq!(served_observations.consultations.get(), 1);
    assert_eq!(served_observations.bif_calls.get(), 1);
    assert_eq!(derivations(&served_machine), 1);
    assert_eq!(
        (
            served_machine.frame_depth(),
            served_machine.stack_depth()
        ),
        (1, 0),
        "a served `call_last` pops the same frame the in-machine route does"
    );
}

/// **`erlang:load_nif/2` invalidates the verdicts** — the one documented way a host's
/// answer set changes mid-run. A local call **before** the install and the same local call
/// **after** it are two questions, so a stub the NIF installed is reached by the second.
///
/// The invalidation is **lazy**: the BIF-dispatch funnel
/// ([`Machine::dispatch_bif_call`], which stays `&self`) only arms
/// [`Machine::local_call_invalidation_pending`]; the seam clears the verdicts at its next
/// local call. So the funnel covers every tier by construction while the hot BIF door keeps
/// its `&self` codegen; the interpreter is the tier exercised here.
#[test]
fn dispatching_load_nif_invalidates_the_memoized_verdicts() {
    let module = load_nif_module();
    let observations = SeamObservations::default();
    let mut ctx = seam_context(false, &observations);
    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(&module, atom_of(&module, "run"), 0, &[], &mut ctx)
        .expect("the fixture runs");
    assert_eq!(outcome, RunOutcome::Returned(make_small(99)));
    assert_eq!(
        observations.consultations.get(),
        2,
        "the `load_nif/2` dispatch clears the verdicts, so the call after it re-asks"
    );
    assert_eq!(
        observations.bif_calls.get(),
        1,
        "the install itself is the only BIF the fixture dispatches"
    );
}

/// A module whose `run/0` calls `helper/0` and then dispatches `erlang:load_nif/2` as its
/// **last** instruction — the shape that leaves the armed invalidation **pending** when the
/// run returns:
///
/// ```text
/// run/0:     label 1; allocate 0,0; call helper; move 0 -> x0;
///            move 0 -> x1; call_ext erlang:load_nif/2; deallocate 0; return
/// helper/0:  label 5; move 99 -> x0; return
/// ```
fn load_nif_last_module() -> Module {
    let mut fixture = Fixture::new("m");
    let load_nif = fixture.import("erlang", "load_nif", 2);
    let run = fixture.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Call {
                target: HELPER,
                arity: 0,
            },
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(1),
            },
            Instr::CallExt {
                target: load_nif,
                arity: 2,
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    fixture.add_function(
        "helper",
        0,
        vec![
            Instr::Label(HELPER),
            Instr::Move {
                src: Operand::Integer(99),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.add_export("run", 0, run);
    fixture.finish()
}

/// The function index whose label table holds `label`, i.e. the [`CalleeKey`] field the
/// local-call memo is keyed on (a function may own several labels).
fn func_of(module: &Module, label: Label) -> u32 {
    module
        .functions
        .iter()
        .position(|function| function.labels.get(&label).is_some())
        .expect("the fixture's function") as u32
}

/// The `load_nif/2` invalidation is **lazy**: the dispatch arms it and the seam consumes it
/// at the next local call, so right after a run whose last instruction is the dispatch the
/// flag is set and the memo still holds the pre-install answers. This is what lets the BIF
/// door keep its `&self` receiver (the hottest call in the interpreted tier) while every
/// tier still re-asks the host.
#[test]
fn the_load_nif_invalidation_stays_pending_until_the_next_local_call() {
    let module = load_nif_last_module();
    let observations = SeamObservations::default();
    let mut ctx = seam_context(false, &observations);
    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(&module, atom_of(&module, "run"), 0, &[], &mut ctx)
        .expect("the fixture runs");
    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(0)),
        "the tail `load_nif/2` dispatch's own value flows out"
    );
    assert_eq!(
        observations.consultations.get(),
        1,
        "the one local call before the install asked the host"
    );
    assert_eq!(
        verdict_of(&machine, func_of(&module, HELPER)),
        Verdict::Declined,
        "the dispatch did not clear the verdict itself — the seam does, at the next local call"
    );
    assert!(
        machine.local_call_invalidation_pending.get(),
        "the `load_nif/2` dispatch armed the pending invalidation"
    );
    // The public door consumes a pending invalidation too, so a host that invalidates
    // explicitly is never followed by a redundant clear.
    machine.invalidate_local_call_verdicts();
    assert!(!machine.local_call_invalidation_pending.get());
    assert_eq!(
        verdict_of(&machine, func_of(&module, HELPER)),
        Verdict::Unknown,
        "the verdicts were reset, so the next call re-asks"
    );
}

/// A pending invalidation does **not** survive [`Machine::reset`]: a fresh run has dispatched
/// nothing, so the flag belongs to the run that armed it.
#[test]
fn resetting_the_machine_clears_a_pending_invalidation() {
    let module = load_nif_last_module();
    let observations = SeamObservations::default();
    let mut ctx = seam_context(false, &observations);
    let mut machine = Machine::new();
    machine
        .run_export_outcome(&module, atom_of(&module, "run"), 0, &[], &mut ctx)
        .expect("the fixture runs");
    assert!(machine.local_call_invalidation_pending.get());
    machine.reset();
    assert!(
        !machine.local_call_invalidation_pending.get(),
        "`reset` clears the flag with the memo it belongs to"
    );
}

/// A BIF dispatch that is **not** `load_nif/2` leaves the verdicts alone: the funnel's
/// compare must not clear the memo on every BIF (that would defeat the cache entirely).
#[test]
fn an_ordinary_bif_dispatch_keeps_the_memoized_verdicts() {
    let module = two_callee_module();
    let observations = SeamObservations::default();
    let mut ctx = seam_context(false, &observations);
    let mut machine = Machine::new();
    machine
        .run_export_outcome(&module, atom_of(&module, "run"), 0, &[], &mut ctx)
        .expect("the fixture runs");
    assert!(
        !machine.local_call_invalidation_pending.get(),
        "no `load_nif/2` was dispatched, so nothing was armed"
    );
    let verdicts: Vec<_> = [HELPER, HELPER2]
        .iter()
        .map(|label| verdict_of(&machine, func_of(&module, *label)))
        .collect();
    assert_eq!(
        verdicts,
        vec![Verdict::Declined, Verdict::Declined],
        "both verdicts survived (no `load_nif/2` was dispatched)"
    );
    assert_eq!(derivations(&machine), 2, "one derivation per callee");
}

/// A module named `n` whose `run/0` makes a **local** call to its own unexported
/// `helper/0` — the callee shape a registered module context executes in-machine.
fn callee_module() -> Module {
    let mut fixture = Fixture::new("n");
    let run = fixture.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Call {
                target: HELPER,
                arity: 0,
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    fixture.add_function(
        "helper",
        0,
        vec![
            Instr::Label(HELPER),
            Instr::Move {
                src: Operand::Integer(99),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.add_export("run", 0, run);
    fixture.finish()
}

/// A module named `m` whose `run/0` is a single `call_ext n:run/0` — the entry module that
/// makes **no** local call at all.
fn calling_entry_module() -> Module {
    let mut fixture = Fixture::new("m");
    let callee = fixture.import("n", "run", 0);
    let run = fixture.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::CallExt {
                target: callee,
                arity: 0,
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    fixture.add_export("run", 0, run);
    fixture.finish()
}

/// **The table is per module context**, so a function index means the same thing in one
/// table only: a local call made *inside* a registered cross-module callee files its verdict
/// in **that** context's table, and the run's entry module — which makes no local call —
/// keeps an all-`Unknown` table. A shared table would file `n:helper/0`'s verdict under
/// `m`'s function index `2` (or vice versa), which is exactly the wrong-answer bug the
/// context-owned table cannot have.
#[test]
fn a_local_call_inside_a_registered_context_files_its_verdict_in_that_contexts_table() {
    let callee = Arc::new(callee_module());
    let entry = calling_entry_module();
    let observations = SeamObservations::default();
    let mut ctx = seam(
        vec![("n".to_string(), Arc::clone(&callee))],
    );
    // The cross-module seam context counts nothing; the **local**-call resolver must be
    // consulted exactly once, for the callee module's own `helper/0`.
    let consultations = Rc::clone(&observations.consultations);
    let asked = Rc::clone(&observations.asked);
    ctx.set_local_call_resolver(Box::new(move |module: &str, function: &str, arity: u32| {
        consultations.set(consultations.get() + 1);
        asked
            .borrow_mut()
            .push((module.to_string(), function.to_string(), arity));
        Ok(false)
    }));
    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(&entry, atom_of(&entry, "run"), 0, &[], &mut ctx)
        .expect("the cross-module call runs in this machine");
    assert_eq!(outcome, RunOutcome::Returned(make_small(99)));
    assert_eq!(
        observations.asked(),
        vec![("n".to_string(), "helper".to_string(), 0)],
        "the callee module's own local call, asked in the callee's own context"
    );
    // The registered context (`n`, context id 1) holds the verdict...
    let table = machine
        .contexts
        .verdicts(1)
        .expect("context 1 is the registered callee module");
    assert_eq!(
        table.get(func_of(&callee, HELPER)),
        Verdict::Declined,
        "the callee module's table holds its own callee's verdict"
    );
    assert!(
        table.describes(&callee),
        "the table belongs to the registered module image"
    );
    // ... and the entry module's table was never touched: `m:run/0` makes no local call.
    assert!(
        (0..entry.functions.len() as u32).all(|func| machine.entry_verdicts.get(func)
            == Verdict::Unknown),
        "the entry module filed nothing: it made no local call"
    );
}
