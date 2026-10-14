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

//! The **synthesized callee head item** of an `undef` raise
//! ([`crate::families::exceptions::callee`]).
//!
//! Real OTP names an unresolvable call target **only** through the cooked
//! stacktrace's innermost item — `{TargetModule, TargetFunction, ArgList, []}` —
//! and `logger_server:call_h/4` matches on exactly that shape (see the oracle
//! evidence in the module docs). The engine's raise snapshot is
//! `(context, func, pc)` metadata and can therefore only emit an *arity*, so the
//! failing call sites synthesize the one item the snapshot cannot and publish it
//! as the raise's own raw trace, which [`Instr::BuildStacktrace`] then conses in
//! front of its frames.

use super::*;

use beam_common::{ExitReason, Raise};
use beam_loader::{Atom, Export, Import, ImportRef};

use crate::families::exceptions::callee;

// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so alias
// `terms` to the test-only wrappers that keep the old ergonomic arity. (An explicit
// import shadows the glob-imported `terms` above.)
use crate::terms::test_support as terms;

/// The `error:undef` raise the host produces for a call target it cannot resolve
/// (`beam_rt`'s `error::undef()`, `BeamError::Raise(Raise::error(ExitReason::atom("undef")))`).
fn undef_error() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("undef")))
}

/// A machine holding one live catch frame at `(func=0, pc=0)` owned by frame 0.
fn machine_with_a_catch_frame() -> Machine {
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));
    machine
}

// ---------------------------------------------------------------------------
// the item builder
// ---------------------------------------------------------------------------

#[test]
fn callee_head_item_names_the_callee_with_its_argument_list() {
    // The oracle's own innermost cooked item for `apply(logger_simple_h,
    // changing_config, [a,b,c])`: the callee named with an argument LIST and the
    // empty-list Location (the callee does not exist, so no source position).
    let module = Fixture::new(&["error", "undef", "logger_simple_h", "changing_config"]).finish();
    let callee_module = make_atom(module.atom_table.lookup("logger_simple_h").unwrap());
    let callee_function = make_atom(module.atom_table.lookup("changing_config").unwrap());

    let mut machine = machine_with_a_catch_frame();
    for (i, value) in [1, 2, 3].into_iter().enumerate() {
        machine.set_x(i as u32, make_small(value)).unwrap();
    }
    let mut ctx = MinimalContext::new();
    let item = callee::callee_head_item(
        &mut machine,
        &mut ctx,
        &undef_error(),
        callee_module,
        callee_function,
        3,
    )
    .unwrap()
    .expect("a live catch frame and a known callee synthesize the item");

    let args = terms::make_list(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let expected =
        terms::make_tuple(&mut ctx, &[callee_module, callee_function, args, NIL]).unwrap();
    assert!(
        cmp(item, expected, &module.atom_table).is_eq(),
        "the item must be {{logger_simple_h,changing_config,[1,2,3],[]}}"
    );
}

#[test]
fn callee_head_item_is_declined_without_a_live_catch_frame() {
    // With no live catch frame the raise cannot land, so the cooked trace is never
    // reached: nothing may be synthesized (and the latch must stay clear).
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    for (i, value) in [1, 2, 3].into_iter().enumerate() {
        machine.set_x(i as u32, make_small(value)).unwrap();
    }
    let mut ctx = MinimalContext::new();
    assert_eq!(
        callee::callee_head_item(&mut machine, &mut ctx, &undef_error(), NIL, NIL, 3).unwrap(),
        None
    );
}

#[test]
fn callee_head_item_is_declined_for_a_reason_that_is_not_bare_undef() {
    // Only an `undef` names a missing callee; every other failure keeps today's
    // cooked trace.
    let mut machine = machine_with_a_catch_frame();
    machine.set_x(0, make_small(1)).unwrap();
    let mut ctx = MinimalContext::new();
    for error in [
        BeamError::BadArg,
        BeamError::Raise(Raise::error(ExitReason::atom("badarg"))),
        BeamError::Raise(Raise::error(ExitReason::Structured)),
    ] {
        assert!(
            !callee::is_unresolved_undef(&error),
            "{error:?} is not the bare undef"
        );
        assert_eq!(
            callee::callee_head_item(&mut machine, &mut ctx, &error, NIL, NIL, 1).unwrap(),
            None,
            "{error:?} must not synthesize a callee head"
        );
    }
    assert!(callee::is_unresolved_undef(&undef_error()));
}

#[test]
fn callee_head_item_is_declined_when_the_callee_arguments_are_unreadable() {
    // `argc` comes from the instruction, so an unreadable register is a malformed
    // call, not a runtime value: nothing is fabricated in its place.
    let mut machine = machine_with_a_catch_frame();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        callee::callee_head_item(
            &mut machine,
            &mut ctx,
            &undef_error(),
            NIL,
            NIL,
            crate::machine::MAX_REGISTER + 1,
        )
        .unwrap(),
        None,
        "an out-of-range argument register synthesizes nothing"
    );
}

// ---------------------------------------------------------------------------
// the cooked trace
// ---------------------------------------------------------------------------

/// The frame chain of [`build_stacktrace_cooks_the_raise_snapshot_innermost_first`]:
/// `run/0` raising, with `outer` and `caller` suspended above it in the same module.
/// Returns the module and the machine parked just before the raise.
fn three_frame_machine() -> (Module, Machine) {
    let mut fixture = Fixture::new(&["error", "undef", "logger_simple_h", "changing_config"]);
    let run_entry = fixture.add_function(
        "run",
        0,
        vec![Instr::Label(Label(1)), Instr::BuildStacktrace, Instr::Return],
    );
    let caller_entry =
        fixture.add_function("caller", 0, vec![Instr::Label(Label(2)), Instr::Return]);
    let outer_entry = fixture.add_function("outer", 0, vec![Instr::Label(Label(3)), Instr::Return]);
    let module = fixture.finish();
    assert_eq!(run_entry, Label(1));
    assert_eq!(caller_entry, Label(2));
    assert_eq!(outer_entry, Label(3));

    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(crate::machine::Continuation::Address {
            func: 1,
            pc: 1,
            context: 0,
        })
        .unwrap();
    machine.push_frame(0).unwrap();
    machine
        .set_continuation(crate::machine::Continuation::Address {
            func: 2,
            pc: 1,
            context: 0,
        })
        .unwrap();
    // The raising instruction: `run/0` at code offset 1.
    machine.current_code = Some((0, 1));
    // A catch frame owned by frame 1: landing there unwinds frame 2 (`outer`) away.
    machine.push_catch_frame(CatchFrame::new(1, 0, 1));
    machine.set_class_atom(ExceptionClass::Error, make_atom(Atom::from_raw(11)));
    (module, machine)
}

#[test]
fn a_headed_raise_cooks_the_callee_item_in_front_of_the_frames() {
    let (module, mut machine) = three_frame_machine();
    let callee_module = make_atom(module.atom_table.lookup("logger_simple_h").unwrap());
    let callee_function = make_atom(module.atom_table.lookup("changing_config").unwrap());
    for (i, value) in [1, 2, 3].into_iter().enumerate() {
        machine.set_x(i as u32, make_small(value)).unwrap();
    }
    let mut ctx = MinimalContext::new();

    let error = undef_error();
    let head = callee::callee_head_item(
        &mut machine,
        &mut ctx,
        &error,
        callee_module,
        callee_function,
        3,
    )
    .unwrap()
    .expect("a live catch frame and a known callee synthesize the item");
    let step = machine
        .raise_bif_error_with_head(&module, error, Some(head))
        .expect("a live frame catches");
    assert_eq!(step, Step::Goto { func: 1, pc: 0 });
    assert_eq!(machine.frame_depth(), 2, "the raise unwound `outer` away");

    // `land_exception` published the head item as the exception's raw trace in
    // x(2), and the compiled handler moves it into x(0) before `build_stacktrace`
    // (`erl -S`: `move {x,2} -> {x,0}` + `build_stacktrace`) — which is exactly what
    // keeps the item inside the frozen root set across the cooking reservation.
    let raw = machine.x(2).expect("x(2) holds the raw trace");
    let args = terms::make_list(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let head_item =
        terms::make_tuple(&mut ctx, &[callee_module, callee_function, args, NIL]).unwrap();
    assert!(
        cmp(raw, head_item, &module.atom_table).is_eq(),
        "the landed raw trace IS the synthesized callee item"
    );
    machine.set_x(0, raw).unwrap();

    let step = exec(
        &mut machine,
        &module,
        &module,
        &no_labels(),
        &Instr::BuildStacktrace,
        0,
        0,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));

    let m = module.atom_table.lookup("m").unwrap();
    let item = |ctx: &mut MinimalContext, name: &str, frame: u32| {
        terms::make_tuple(
            ctx,
            &[
                make_atom(m),
                make_atom(module.atom_table.lookup(name).unwrap()),
                make_small(frame.into()),
                NIL,
            ],
        )
        .unwrap()
    };
    let item_run = item(&mut ctx, "run", 0);
    let item_outer = item(&mut ctx, "outer", 0);
    let item_caller = item(&mut ctx, "caller", 0);
    let expected =
        terms::make_list(&mut ctx, &[head_item, item_run, item_outer, item_caller]).unwrap();
    assert!(
        cmp(machine.x(0).unwrap(), expected, &module.atom_table).is_eq(),
        "the cooked trace must be [{{logger_simple_h,changing_config,[1,2,3],[]}}, \
         {{m,run,0,[]}}, {{m,outer,0,[]}}, {{m,caller,0,[]}}] — the oracle's own shape"
    );
}

#[test]
fn an_empty_frame_chain_with_a_head_cooks_to_a_one_element_list() {
    // An empty (or wholly skipped) raise snapshot is normally the empty list — but a
    // live head item is always the FIRST item, so the cooked trace is the
    // **one-element** list `[Item]`, never `NIL`.
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    assert!(machine.raise_snapshot().is_empty());
    // The synthesized head is normally announced by the raise funnel; standing in for
    // it here lets the cooking be exercised with no frames at all (a landed raise
    // always snapshots at least the raising frame).
    machine.raise_head_item = true;
    let mut ctx = MinimalContext::new();
    let item = terms::make_tuple(&mut ctx, &[NIL, NIL, NIL, NIL]).unwrap();
    machine.set_x(0, item).unwrap();

    let step = exec(
        &mut machine,
        &module,
        &module,
        &no_labels(),
        &Instr::BuildStacktrace,
        0,
        0,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    let cooked = machine.x(0).unwrap();
    let elements = crate::terms::list_elements(cooked).unwrap();
    assert_eq!(elements.len(), 1, "the head item is the only element");
    assert_eq!(elements[0], item);
}

#[test]
fn a_cooked_trace_without_a_head_is_unchanged() {
    // The flag's absence is byte-identical to the pre-head behaviour: the frames
    // alone, in the same order.
    let (module, mut machine) = three_frame_machine();
    let mut ctx = MinimalContext::new();
    let error = undef_error();
    let step = machine
        .raise_bif_error_with_head(&module, error, None)
        .expect("a live frame catches");
    assert_eq!(step, Step::Goto { func: 1, pc: 0 });
    let raw = machine.x(2).unwrap();
    assert_eq!(raw, NIL, "an unheaded raise still lands with a NIL raw trace");
    machine.set_x(0, raw).unwrap();

    let step = exec(
        &mut machine,
        &module,
        &module,
        &no_labels(),
        &Instr::BuildStacktrace,
        0,
        0,
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    let frames = crate::terms::list_elements(machine.x(0).unwrap()).unwrap();
    assert_eq!(frames.len(), 3, "no head item is prepended");
    for (frame, name) in frames.iter().zip(["run", "outer", "caller"]) {
        assert_eq!(
            crate::terms::tuple_element(*frame, 1).unwrap(),
            make_atom(module.atom_table.lookup(name).unwrap())
        );
    }
}

// ---------------------------------------------------------------------------
// latch hygiene
// ---------------------------------------------------------------------------

#[test]
fn a_head_is_not_armed_when_the_raise_cannot_land() {
    // `raise_bif_error_with_head` returns the error unchanged when there is no live
    // catch frame — no `throw_with_trace` runs, so the one-shot latch must stay
    // clear: a later, unrelated raise must never inherit this head.
    let module = Fixture::new(&[]).finish();
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    let mut ctx = MinimalContext::new();
    let item = terms::make_tuple(&mut ctx, &[NIL, NIL, NIL, NIL]).unwrap();
    let err = machine
        .raise_bif_error_with_head(&module, undef_error(), Some(item))
        .expect_err("no catch frame");
    assert!(matches!(err, BeamError::Raise(_)));
    assert!(
        !machine.take_raise_head_item(),
        "an unlanded head must not arm the live-raise state"
    );
    assert!(!machine.pending_callee_head, "and the latch stays clear");
}

#[test]
fn a_raise_without_a_head_clears_the_live_raise_state() {
    // Every raise re-derives the state from the one-shot latch as its FIRST act, so
    // a raise that published no head can never inherit a stale one.
    let mut machine = Machine::new();
    machine.push_frame(0).unwrap();
    machine.raise_head_item = true;
    let err = machine
        .throw_with_trace(ExceptionClass::Error, make_small(1), NIL)
        .expect_err("no catch frame");
    assert!(matches!(err, BeamError::Raise(_)));
    assert!(
        !machine.take_raise_head_item(),
        "a head-less raise clears the live-raise state"
    );
    assert!(!machine.pending_callee_head);
}

#[test]
fn reset_clears_both_callee_head_flags() {
    let mut machine = Machine::new();
    machine.pending_callee_head = true;
    machine.raise_head_item = true;
    machine.reset();
    assert!(!machine.pending_callee_head);
    assert!(!machine.raise_head_item);
}

// ---------------------------------------------------------------------------
// the whole route: a compiled `call_ext` to a missing export
// ---------------------------------------------------------------------------

/// The oracle shape (`erlc -S` of `run() -> try apply(logger_simple_h,
/// changing_config, [1,2,3]) catch C:R:S -> S end`), as a compiled `call_ext`:
///
/// ```text
/// run/0:
///     allocate 1
///     move 1 -> x(0); move 2 -> x(1); move 3 -> x(2)
///     try y(0) 20
///     call_ext logger_simple_h:changing_config/3
///     try_end y(0)
///     deallocate 1
///     return
/// 20: try_case y(0)          ; x(1)=reason, x(2)=trace, x(3)=class
///     move x(2) -> x(0)
///     build_stacktrace
///     deallocate 1
///     return
/// ```
#[test]
fn a_call_ext_to_a_missing_export_cooks_the_callee_head_item() {
    let mut fixture = Fixture::new(&["error", "undef", "logger_simple_h", "changing_config"]);
    fixture.module.name = fixture.module.atom_table.intern("p3");
    let module_atom = fixture.module.atom_table.intern("logger_simple_h");
    let function_atom = fixture.module.atom_table.intern("changing_config");
    fixture.module.imports.push(Import {
        module: module_atom,
        function: function_atom,
        arity: 3,
    });
    let run_atom = fixture.module.atom_table.intern("run");
    let entry = fixture.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(2),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Integer(3),
                dst: Operand::X(2),
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(20),
            },
            Instr::CallExt {
                target: ImportRef(0),
                arity: 3,
            },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(20)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(2),
                dst: Operand::X(0),
            },
            Instr::BuildStacktrace,
            Instr::Deallocate(1),
            Instr::Return,
        ],
    );
    fixture.module.exports.push(Export {
        function: run_atom,
        arity: 0,
        label: entry,
    });
    let module = fixture.finish();

    // The host cannot resolve the import (`beam_rt`'s code server answers exactly
    // this for a missing export).
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|_, _, _, _| Err(undef_error())));
    let mut machine = Machine::new();
    let cooked = machine
        .run_export(&module, run_atom, 0, &[], &mut ctx)
        .expect("the handler returns the cooked trace");

    let head = crate::terms::list_elements(cooked).unwrap();
    assert_eq!(head.len(), 2, "the head item plus the raising frame");
    let item = crate::terms::tuple_element(head[0], 0).unwrap();
    assert_eq!(item, make_atom(module_atom), "the callee's module");
    assert_eq!(
        crate::terms::tuple_element(head[0], 1).unwrap(),
        make_atom(function_atom),
        "the callee's function"
    );
    let args = crate::terms::list_elements(crate::terms::tuple_element(head[0], 2).unwrap())
        .expect("the item's ArgList is a proper list");
    assert_eq!(
        args,
        vec![make_small(1), make_small(2), make_small(3)],
        "the callee's REAL argument list, not an arity"
    );
    assert_eq!(
        crate::terms::tuple_element(head[0], 3).unwrap(),
        NIL,
        "the Location of a callee that does not exist is the empty list"
    );
    assert_eq!(
        crate::terms::tuple_element(head[1], 1).unwrap(),
        make_atom(run_atom),
        "then the raising frame"
    );
}
