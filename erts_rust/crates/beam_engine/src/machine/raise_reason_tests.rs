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

//! Pins for the **raise-reason producer rooting** contract behind
//! [`Machine::land_exception`] — the raw-word landing rule.
//!
//! The landing stores the raise's `reason` in `x(1)` and its
//! supplied-trace-or-raw-trace word in `x(2)` **as the raw words it was handed**:
//! no copy, no re-home, no validation. `Instr::CatchEnd` then embeds that very word
//! in the caught tuple and `Instr::BuildStacktrace` cooks `x(2)`. The properties,
//! each its own test:
//!
//! * **Identity, not equality** — a boxed reason lands in `x(1)` as the identical
//!   word the raise carried, and `catch_end` embeds that same word in the caught
//!   tuple (a re-building or copying seam would keep `=:=` and lose the identity).
//! * **The supplied trace is echoed verbatim** — a supplied trace term lands in
//!   `x(2)` as the identical word, and an explicitly empty trace (`[]`, `NIL`)
//!   stays distinguishable from "no trace supplied" through the one-shot verbatim
//!   marker the raise's latch carries.
//! * **The landing allocates nothing** — a raise that arrives through the BIF door
//!   (`Machine::raise_bif_error`, i.e. `ExecutionContext::call_bif`'s `BeamError`)
//!   crosses **zero** allocating doors from its reason word to the landed register,
//!   so no collection can run between them.
//! * **An uncaught raise keeps its word** — `Machine::with_uncaught_frames` decorates
//!   the symbolic frames only; the live reason word is passed through untouched (no
//!   re-home, no substitution).
//!
//! Every term here is built in a [`MinimalContext`]'s region heap, which is the
//! machine heap of these fixtures: a real boxed/cons pointer word, so identity is an
//! address comparison and a substituted word would be visible.

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason, Raise};
use beam_loader::{Instr, Module, Operand};
use beam_term::{make_small, Eterm, NIL};

use super::test_support::Fixture;
use super::{CatchFrame, Machine};
use crate::context::{ExecutionContext, MinimalContext, RootSet};
use crate::families::exceptions;
use crate::interpreter::LabelMap;
use crate::terms::test_support as terms;
use crate::Step;

/// A module that names the three exception classes plus `'EXIT'`, so
/// [`Machine::ensure_class_atoms`] and `catch_end` can resolve every atom they need
/// from its load-time snapshot.
fn class_module() -> Module {
    let mut fixture = Fixture::new("m");
    for name in ["error", "exit", "throw", "EXIT"] {
        fixture.atom(name);
    }
    fixture.finish()
}

/// A machine with one bottom frame and a live catch frame landing at `(0, 0)`.
fn catchable_machine() -> Machine {
    let mut machine = Machine::new();
    machine.push_frame(0).expect("a bottom frame fits the stack");
    machine.push_catch_frame(CatchFrame::new(0, 0, 0));
    machine
}

/// Run one `catch_end` body over `machine`, returning its step.
fn run_catch_end(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
) -> Result<Option<Step>, BeamError> {
    exceptions::exec(
        machine,
        module,
        module,
        &LabelMap::new(),
        &Instr::CatchEnd {
            frame: Operand::X(5),
        },
        0,
        0,
        ctx,
    )
}

/// The raw word of `term` — what "the identical word" means here. Two terms can be
/// `=:=` while being different words (a copy); only the raw word pins the landing's
/// no-copy rule.
fn raw(term: Eterm) -> usize {
    term.raw()
}

#[test]
fn a_boxed_reason_lands_in_x1_as_the_identical_word() {
    let module = class_module();
    let mut ctx = MinimalContext::new();
    // A boxed term, so a re-building landing would be visible.
    let reason = terms::make_tuple(&mut ctx, &[make_small(7)]).expect("a boxed tuple");
    let mut machine = catchable_machine();
    machine.ensure_class_atoms(&mut ctx, &module);

    let step = machine
        .throw_with_trace(ExceptionClass::Error, reason, NIL)
        .expect("a live catch frame catches");
    assert_eq!(step, Step::Goto { func: 0, pc: 0 });

    assert_eq!(
        machine.x(0),
        Some(Eterm::non_value()),
        "C's `handle_error` marker survives the landing"
    );
    assert_eq!(
        machine.x(1).map(raw),
        Some(raw(reason)),
        "the landed `x(1)` must be the very word the raise carried, not a copy of it"
    );
    assert_eq!(
        machine.x(2),
        Some(NIL),
        "a raise that supplied no trace lands the raw-trace word `NIL`"
    );
}

#[test]
fn catch_end_embeds_the_landed_reason_word_verbatim() {
    let module = class_module();
    let mut ctx = MinimalContext::new();
    let reason = terms::make_tuple(&mut ctx, &[make_small(8)]).expect("a boxed tuple");
    let mut machine = catchable_machine();
    machine.ensure_class_atoms(&mut ctx, &module);
    machine
        .throw_with_trace(ExceptionClass::Error, reason, NIL)
        .expect("a live catch frame catches");

    let step = run_catch_end(&module, &mut machine, &mut ctx).expect("catch_end executes");
    assert_eq!(step, Some(Step::Next));

    // Class `error` folds to `{'EXIT',{Reason,Stacktrace}}` (`add_stacktrace`).
    let caught = machine.x(0).expect("catch_end publishes its result in x(0)");
    let inner = crate::terms::tuple_element(caught, 1).expect("the outer tuple's second element");
    let embedded = crate::terms::tuple_element(inner, 0).expect("the inner pair's first element");
    assert_eq!(
        raw(embedded),
        raw(reason),
        "the caught tuple must embed the landed reason word itself, not a re-built copy"
    );
}

#[test]
fn catch_end_yields_the_thrown_reason_word_for_class_throw() {
    // The class-`throw` arm of `catch_end` publishes the reason itself in `x(0)`, so
    // the same identity must hold where no tuple is built at all.
    let module = class_module();
    let mut ctx = MinimalContext::new();
    let reason = terms::make_list(&mut ctx, &[make_small(9), make_small(10)]).expect("a cons list");
    let mut machine = catchable_machine();
    machine.ensure_class_atoms(&mut ctx, &module);
    machine
        .throw_with_trace(ExceptionClass::Throw, reason, NIL)
        .expect("a live catch frame catches");

    let step = run_catch_end(&module, &mut machine, &mut ctx).expect("catch_end executes");
    assert_eq!(step, Some(Step::Next));
    assert_eq!(
        machine.x(0).map(raw),
        Some(raw(reason)),
        "a thrown value is caught as the identical word"
    );
}

#[test]
fn a_supplied_trace_lands_in_x2_as_the_identical_word() {
    let module = class_module();
    let mut ctx = MinimalContext::new();
    // Both terms are heap words, so both the reason and the trace prove the rule.
    let reason = terms::make_tuple(&mut ctx, &[make_small(11)]).expect("a boxed tuple");
    let supplied = terms::make_list(&mut ctx, &[make_small(12)]).expect("a cons list");
    let mut machine = catchable_machine();
    machine.ensure_class_atoms(&mut ctx, &module);

    machine
        .raise_supplied(ExceptionClass::Error, reason, supplied, true)
        .expect("a live catch frame catches");

    assert_eq!(
        machine.x(1).map(raw),
        Some(raw(reason)),
        "the reason is landed verbatim alongside a supplied trace"
    );
    assert_eq!(
        machine.x(2).map(raw),
        Some(raw(supplied)),
        "the supplied trace term is echoed into x(2) as the identical word"
    );
    assert!(
        machine.live_raise_supplied_term_verbatim(),
        "the verbatim marker survives the landing: it is what makes the echo an echo"
    );
}

#[test]
fn an_explicitly_empty_supplied_trace_is_not_indistinguishable_from_no_trace() {
    let module = class_module();
    let mut ctx = MinimalContext::new();
    let reason = make_small(13);
    let mut machine = catchable_machine();
    machine.ensure_class_atoms(&mut ctx, &module);

    // `erlang:raise(C, R, [])` — the oracle's empty trace. The landed word `NIL` is
    // word-identical to "nothing was supplied", so only the marker can tell them
    // apart; that is exactly what `symbolic_stacktrace` reads.
    machine
        .raise_supplied(ExceptionClass::Error, reason, NIL, true)
        .expect("a live catch frame catches");
    assert_eq!(machine.x(2), Some(NIL));
    assert!(
        machine.live_raise_supplied_term_verbatim(),
        "an explicitly empty supplied trace must stay distinguishable from no trace"
    );

    // Control: a raise that supplied nothing leaves the marker clear.
    let mut other = catchable_machine();
    other.ensure_class_atoms(&mut ctx, &module);
    other
        .throw_with_trace(ExceptionClass::Error, reason, NIL)
        .expect("a live catch frame catches");
    assert!(!other.live_raise_supplied_term_verbatim());
}

/// A counting, non-collecting [`ExecutionContext`]: it serves every allocation out
/// of a [`MinimalContext`] arena (pointer-stable, never freed) and counts the three
/// doors that may **collect** — the two admissions ([`ensure_heap`], and
/// [`ensure_heap_full`](ExecutionContext::ensure_heap_full), whose default forwards
/// to `ensure_heap`) and the two term-construction doors.
struct CountingContext {
    arena: MinimalContext,
    admission_doors: usize,
    allocation_doors: usize,
}

impl CountingContext {
    fn new() -> CountingContext {
        CountingContext {
            arena: MinimalContext::new(),
            admission_doors: 0,
            allocation_doors: 0,
        }
    }

    /// The arena a caller builds its fixture terms in.
    fn arena(&mut self) -> &mut MinimalContext {
        &mut self.arena
    }

    /// Every door a collection could run under: both admissions plus both
    /// allocation seams.
    fn collecting_doors(&self) -> usize {
        self.admission_doors + self.allocation_doors
    }
}

impl ExecutionContext for CountingContext {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> beam_common::Result<Eterm> {
        self.arena.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> beam_common::Result<()> {
        self.admission_doors += 1;
        self.arena.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> beam_common::Result<*mut Eterm> {
        self.allocation_doors += 1;
        self.arena.alloc_words(n)
    }

    fn alloc_words_rooted(
        &mut self,
        n: usize,
        roots: RootSet<'_>,
    ) -> beam_common::Result<*mut Eterm> {
        self.allocation_doors += 1;
        self.arena.alloc_words_rooted(n, roots)
    }
}

#[test]
fn a_raise_through_the_bif_door_lands_without_crossing_an_allocating_door() {
    // The BIF door's word is held in the `BeamError` — a Rust local, i.e. *outside*
    // the frozen `RootSet` — until the landing publishes it in `x(1)`. The landing
    // itself must therefore allocate nothing (no admission, no term construction), so
    // no host collection can run between the seam's word and the register that roots
    // it. This is the invariant a collecting host cannot express directly here: it is
    // the *absence* of a door, which is what this pin measures.
    let module = class_module();
    let mut context = CountingContext::new();
    let reason = terms::make_tuple(context.arena(), &[make_small(14)]).expect("a boxed tuple");
    let mut machine = catchable_machine();
    machine.ensure_class_atoms(context.arena(), &module);
    let before = context.collecting_doors();

    let error = BeamError::Raise(
        Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(reason)),
    );
    let step = machine
        .raise_bif_error(&module, error, &mut context)
        .expect("a live catch frame catches");
    assert_eq!(step, Step::Goto { func: 0, pc: 0 });

    assert_eq!(
        machine.x(1).map(raw),
        Some(raw(reason)),
        "the BIF door's live reason term lands as the identical word"
    );
    assert_eq!(
        context.collecting_doors(),
        before,
        "no allocating door may run between the BIF seam's reason word and the landing"
    );
}

#[test]
fn an_uncaught_raise_keeps_its_reason_word_through_the_uncaught_funnel() {
    let module = class_module();
    let mut ctx = MinimalContext::new();
    let reason = terms::make_tuple(&mut ctx, &[make_small(15)]).expect("a boxed tuple");
    // No catch frame: the raise leaves the machine.
    let mut machine = Machine::new();
    machine.push_frame(0).expect("a bottom frame fits the stack");

    let err = machine
        .throw_with_trace(ExceptionClass::Error, reason, NIL)
        .expect_err("no catch frame is live");
    let decorated = machine.with_uncaught_frames(err, Some(&module), 0, 0);

    let BeamError::Raise(raise) = decorated else {
        panic!("an uncaught structured raise stays a Raise");
    };
    assert_eq!(raise.class, ExceptionClass::Error);
    let live = raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied();
    assert_eq!(
        live.map(raw),
        Some(raw(reason)),
        "the uncaught funnel decorates the frames only: it never re-homes or \
         substitutes the reason word"
    );
}
