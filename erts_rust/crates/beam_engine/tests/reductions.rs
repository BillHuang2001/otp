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

//! The **per-instruction reduction hook**: [`ExecutionContext::reduce`] and the
//! engine-side slice counter [`Machine::reductions`]/[`Machine::reset_reductions`].
//!
//! # Why a per-instruction charge exists
//!
//! A BEAM process is pre-emptible because the runtime prices work in *reductions*.
//! The engine's **only** other charging point is
//! [`ExecutionContext::ensure_heap`](ExecutionContext::ensure_heap) (reached by
//! `test_heap`/`allocate_heap`), which a tight **non-allocating** loop — the one
//! shape that can monopolise a scheduler — never touches. [`ExecutionContext::reduce`]
//! closes that gap: it is called **once per interpreted instruction** with `n = 1`,
//! after the instruction succeeded, so a host can price instructions rather than heap
//! admissions, and can pre-empt by returning its own typed error.
//!
//! # The loop under test
//!
//! A hand-built, **non-allocating** module: `label 1 | bif0 host:counter/0 x0 | jump 1`.
//! One iteration executes **three** interpreted instructions (the `label` marker, the
//! `bif0`, the `jump`), and the host's `counter/0` BIF is the terminator: it
//! succeeds `N` times and then returns a typed error, so the run is deterministic.
//!
//! The `label` marker is itself an instruction the loop executes each time (the jump
//! target is the label's own code index), which is why an `N`-call run charges
//! `3·N + 1` reductions: `N` full iterations plus the final iteration's `label` (its
//! raising `bif0` is **not** charged — `drive` charges only after the instruction
//! succeeded).
//!
//! # One `drive` call is one slice — and a suspend ends it
//!
//! [`Machine::drive`] zeroes the counter when it is entered, so the count it returns
//! through [`Machine::reductions`] is the work of *that* slice, never a running
//! total. A `receive` that finds no message **is** one of those slices: the
//! suspending `wait` is charged like any other instruction before the driver reports
//! [`RunOutcome::Wait`], and [`Machine::resume_outcome`] then starts a **new** slice
//! at the address the `wait` named. Because `wait` reports its *target label* as that
//! address, the resume re-enters at the label — the `wait` itself is executed (and
//! therefore charged) exactly once, and the instructions between the loop head and
//! the `wait` are the only ones a resume re-executes, as new executions of new
//! instructions in a new slice.
//!
//! The suspend/resume suites below pin that boundary exactly: per-slice counts,
//! the slice-local (not accumulated) reading of [`Machine::reductions`], and the
//! total charges the host hook saw across every slice — each derived from the
//! instruction sequence the module actually runs.

use std::cell::Cell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_engine::{ExecutionContext, Machine, MinimalContext, RunOutcome};
use beam_loader::{Export, Function, Import, ImportRef, Instr, Label, Module, Operand};
use beam_term::{make_small, Eterm};

// ---------------------------------------------------------------------------
// Minimal synthetic-module scaffolding (see tests/external_call.rs)
// ---------------------------------------------------------------------------

/// Map every `Instr::Label` in `code` to its index.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A builder for one synthetic module with its own atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> beam_term::Atom {
        self.module.atom_table.intern(name)
    }

    fn import(&mut self, module: &str, function: &str, arity: u32) -> ImportRef {
        let module = self.atom(module);
        let function = self.atom(function);
        self.module.imports.push(Import {
            module,
            function,
            arity,
        });
        ImportRef((self.module.imports.len() - 1) as u32)
    }

    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn add_export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.atom(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// The non-allocating counting loop: `run/0` = `label 1 | bif0 host:counter/0 x0 |
/// jump 1` (no `allocate`/`test_heap` anywhere, so `ensure_heap` is never reached).
fn counting_loop_module() -> (Module, beam_term::Atom) {
    let mut f = Fixture::new("loop");
    let counter = f.import("host", "counter", 0);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Bif0 {
                bif: counter,
                dst: Operand::X(0),
            },
            Instr::Jump { target: Label(1) },
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The number of successful `counter/0` calls a run makes before the BIF stops the
/// loop, and the number of interpreted instructions one iteration executes.
const SUCCESSFUL_CALLS: u32 = 5;
const INSTRUCTIONS_PER_ITERATION: u64 = 3;
/// The exact reduction count of an [`SUCCESSFUL_CALLS`]-call run: `N` full iterations
/// plus the final iteration's `label`.
const CHARGES_FOR_CALLS: u64 = INSTRUCTIONS_PER_ITERATION * SUCCESSFUL_CALLS as u64 + 1;

// ---------------------------------------------------------------------------
// A receive that suspends, and a module that suspends twice
// ---------------------------------------------------------------------------

// The `run/0` code indices of [`suspending_receive_module`] (see its doc). Naming
// them lets the executed instruction *sequences* below be pinned by instruction
// identity, so every asserted count is derived from the sequence and never a
// fabricated number.
const LABEL_HEAD: usize = 0;
const LOOP_REC: usize = 1;
const REMOVE_MESSAGE: usize = 2;
const JUMP_MATCHED: usize = 3;
const LABEL_EMPTY: usize = 4;
const WAIT: usize = 5;
const LABEL_MATCHED: usize = 6;
const BIF_COUNTER: usize = 7;
const RETURN: usize = 8;

/// The code indices the **first** slice executes, in order, when the mailbox is
/// empty: the entry `label`, the `loop_rec` that finds nothing (jumping to the
/// `wait`), the fail `label`, and the `wait` that suspends.
const FIRST_SLICE_CODE: &[usize] = &[LABEL_HEAD, LOOP_REC, LABEL_EMPTY, WAIT];

/// The code indices the **resumed** slice executes, in order, once a message has
/// arrived: the resume re-enters at the `wait`'s target label — code index
/// [`LABEL_HEAD`], the receive's loop head — so the `wait` (index [`WAIT`]) is *not*
/// among them, while the loop head and the `loop_rec` are (as new executions).
const RESUMED_SLICE_CODE: &[usize] = &[
    LABEL_HEAD,
    LOOP_REC,
    REMOVE_MESSAGE,
    JUMP_MATCHED,
    LABEL_MATCHED,
    BIF_COUNTER,
    RETURN,
];

/// A receive that suspends on an empty mailbox and completes once a message
/// arrives — the real receive instruction shape (`loop_rec` → fail → `wait` →
/// resume at the loop head → `remove_message` → matched path):
///
/// ```text
/// run/0:
///   0 label 1
///   1 loop_rec 9 x0        ; no message queued -> jump to the `wait`
///   2 remove_message       ; matched: unlink the candidate
///   3 jump 3
///   4 label 9
///   5 wait 1               ; suspend; resume at the loop head
///   6 label 3
///   7 bif0 host:counter/0 x1
///   8 return
/// ```
///
/// `x(0)` carries the matched message into the `return` (the `bif0` writes `x(1)`),
/// so a resumed run's value is the message itself — an independent check that the
/// resume really re-drove the matched path.
fn suspending_receive_module() -> (Module, beam_term::Atom) {
    let mut f = Fixture::new("receive");
    let counter = f.import("host", "counter", 0);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::LoopRec {
                fail: Label(9),
                dst: Operand::X(0),
            },
            Instr::RemoveMessage,
            Instr::Jump { target: Label(3) },
            Instr::Label(Label(9)),
            Instr::Wait { target: Label(1) },
            Instr::Label(Label(3)),
            Instr::Bif0 {
                bif: counter,
                dst: Operand::X(1),
            },
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// A module that suspends **twice**, isolating the slice boundary from the mailbox:
/// `run/0` = `label 1 | wait 2 | label 2 | wait 3 | label 3 | bif0 host:counter/0 x0
/// | return` (code indices `0..=6`).
///
/// A plain `wait` is this family's suspend opcode and consults no mailbox, so this
/// module needs none: it drives three slices (`0`, `2`, `4` charges) whose resume
/// addresses advance one `wait` target at a time, which is what makes the
/// per-boundary accounting observable.
fn two_waits_module() -> (Module, beam_term::Atom) {
    let mut f = Fixture::new("waits");
    let counter = f.import("host", "counter", 0);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Wait { target: Label(2) },
            Instr::Label(Label(2)),
            Instr::Wait { target: Label(3) },
            Instr::Label(Label(3)),
            Instr::Bif0 {
                bif: counter,
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The code index each of [`two_waits_module`]'s three slices starts at, and the
/// number of instructions each of them executes: `label 1` + `wait`, then the
/// resumed `label 2` + `wait`, then `label 3` + the `bif0` + the `return`.
const TWO_WAITS_SLICE_STARTS: [usize; 3] = [0, 2, 4];
const TWO_WAITS_SLICE_CHARGES: [u64; 3] = [2, 2, 3];
// ---------------------------------------------------------------------------
// A custom ExecutionContext: a counting BIF and a record-keeping reduce hook
// ---------------------------------------------------------------------------

/// A context whose `host:counter/0` BIF succeeds `stop_after` times and then returns a
/// typed error, and whose [`reduce`](ExecutionContext::reduce) records every charge
/// (and, with a `budget`, pre-empts after exactly that many).
///
/// It also serves a **minimal mailbox**: the candidate is the front message and
/// `remove_message` unlinks it, which is all a receive needs to suspend on an empty
/// mailbox and to complete once [`ReduceCtx::deliver`] has queued one.
struct ReduceCtx {
    /// The BIF succeeds while `calls <= stop_after`, then returns a typed error.
    stop_after: u32,
    /// When set, `reduce` returns a typed error once it has accepted this many charges.
    budget: Option<u32>,
    /// The `n` of every `reduce` call, in order (the failing call included). Never
    /// cleared: it is the record of *every* slice's charges.
    charges: Vec<u32>,
    /// The number of `counter/0` calls made.
    calls: u32,
    /// The queued messages; only the front one is ever the candidate.
    mailbox: VecDeque<Eterm>,
}

impl ReduceCtx {
    fn new(stop_after: u32) -> ReduceCtx {
        ReduceCtx {
            stop_after,
            budget: None,
            charges: Vec::new(),
            calls: 0,
            mailbox: VecDeque::new(),
        }
    }

    fn with_budget(mut self, budget: u32) -> ReduceCtx {
        self.budget = Some(budget);
        self
    }

    /// Queue a message the receive's `loop_rec` will find.
    fn deliver(&mut self, message: Eterm) {
        self.mailbox.push_back(message);
    }
}

impl ExecutionContext for ReduceCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        if module == "host" && function == "counter" && arity == 0 {
            self.calls += 1;
            return if self.calls > self.stop_after {
                Err(beam_engine::unsupported("stop"))
            } else {
                Ok(make_small(self.calls as i64))
            };
        }
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }

    fn reduce(&mut self, n: u32) -> Result<()> {
        self.charges.push(n);
        if let Some(budget) = self.budget {
            if self.charges.len() as u32 > budget {
                return Err(beam_engine::unsupported("reduction budget exhausted"));
            }
        }
        Ok(())
    }

    /// `loop_rec`: the candidate at the scan position — the front queued message, or
    /// nothing when the mailbox is empty (which is what makes the receive suspend).
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Ok(self.mailbox.front().copied())
    }

    /// `remove_message`: unlink the matched candidate.
    fn receive_remove(&mut self) -> Result<()> {
        self.mailbox.pop_front();
        Ok(())
    }
}

/// The `.n` of a typed engine raise.
fn reason(error: BeamError) -> String {
    match error {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .expect("a typed engine error names its reason")
            .to_string(),
        other => panic!("expected a typed raise, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// the count is exact, starts at zero, and `reset_reductions` re-bases it
// ---------------------------------------------------------------------------

#[test]
fn reductions_counts_exactly_the_interpreted_instructions() {
    let (module, run) = counting_loop_module();
    // Nothing has run, so nothing has been charged.
    let mut machine = Machine::new();
    assert_eq!(machine.reductions(), 0, "a fresh machine has charged nothing");

    let mut ctx = ReduceCtx::new(SUCCESSFUL_CALLS);
    let err = machine
        .run_export_outcome(&module, run, 0, &[], &mut ctx)
        .expect_err("the counter BIF stops the loop");
    assert_eq!(reason(err), "unsupported_opcode: stop");

    // `N` full iterations (label + bif0 + jump) plus the final iteration's label —
    // the raising `bif0` is never charged.
    assert_eq!(machine.reductions(), CHARGES_FOR_CALLS);
    // The failing call counts as a call (it is the `N + 1`th).
    assert_eq!(ctx.calls, SUCCESSFUL_CALLS + 1);
}

#[test]
fn reset_reductions_returns_and_zeroes_the_count() {
    let (module, run) = counting_loop_module();
    let mut machine = Machine::new();
    let mut ctx = ReduceCtx::new(SUCCESSFUL_CALLS);
    machine
        .run_export_outcome(&module, run, 0, &[], &mut ctx)
        .expect_err("the counter BIF stops the loop");
    assert_eq!(machine.reductions(), CHARGES_FOR_CALLS);

    // Re-basing returns the discarded count and zeroes the live one.
    assert_eq!(machine.reset_reductions(), CHARGES_FOR_CALLS);
    assert_eq!(machine.reductions(), 0);
}

// ---------------------------------------------------------------------------
// the hook is offered exactly one reduction per interpreted instruction
// ---------------------------------------------------------------------------

#[test]
fn reduce_is_called_once_per_instruction_with_one_reduction() {
    let (module, run) = counting_loop_module();
    let mut machine = Machine::new();
    let mut ctx = ReduceCtx::new(SUCCESSFUL_CALLS);
    machine
        .run_export_outcome(&module, run, 0, &[], &mut ctx)
        .expect_err("the counter BIF stops the loop");

    // One charge per executed instruction, each of exactly one reduction, and the
    // host's tally agrees with the engine's counter.
    assert_eq!(ctx.charges.len() as u64, machine.reductions());
    assert_eq!(ctx.charges.len() as u64, CHARGES_FOR_CALLS);
    assert!(
        ctx.charges.iter().all(|&n| n == 1),
        "every charge is one reduction, got {:?}",
        ctx.charges
    );
}

// ---------------------------------------------------------------------------
// a host can pre-empt through the hook: its own typed error ends the run
// ---------------------------------------------------------------------------

#[test]
fn a_host_that_pre_empts_through_reduce_stops_the_loop_with_its_own_error() {
    const BUDGET: u32 = 7;
    let (module, run) = counting_loop_module();
    let mut machine = Machine::new();
    // The BIF never stops the loop; only the reduction budget does.
    let mut ctx = ReduceCtx::new(u32::MAX).with_budget(BUDGET);

    let err = machine
        .run_export_outcome(&module, run, 0, &[], &mut ctx)
        .expect_err("the reduction budget pre-empts the loop");
    // The host's OWN typed error surfaces — the hook is not swallowed, and the engine
    // does not substitute one of its own reasons.
    assert_eq!(reason(err), "unsupported_opcode: reduction budget exhausted");

    // `BUDGET` charges were accepted, and the `BUDGET + 1`th returned the error.
    assert_eq!(ctx.charges.len() as u32, BUDGET + 1);
    assert_eq!(machine.reductions(), u64::from(BUDGET) + 1);
    // 8 charges = two full iterations (6) plus `label` + `bif0` of the third.
    assert_eq!(ctx.calls, 3);
}

// ---------------------------------------------------------------------------
// the MinimalContext default is a no-op, yet the counter still counts
// ---------------------------------------------------------------------------

#[test]
fn the_minimal_context_default_reduce_is_a_no_op_that_still_counts() {
    let (module, run) = counting_loop_module();

    // A `MinimalContext` that does not override `reduce`: the trait default is a
    // no-op, so the run behaves exactly as the custom-host run above.
    let calls = Rc::new(Cell::new(0u32));
    let observed = Rc::clone(&calls);
    let mut ctx = MinimalContext::with_bif_hook(Box::new(move |module, function, arity, _args| {
        if module == "host" && function == "counter" && arity == 0 {
            observed.set(observed.get() + 1);
            return if observed.get() > SUCCESSFUL_CALLS {
                Err(beam_engine::unsupported("stop"))
            } else {
                Ok(make_small(observed.get() as i64))
            };
        }
        Err(beam_engine::unsupported(&format!("bif {module}:{function}")))
    }));
    let mut machine = Machine::new();

    let err = machine
        .run_export_outcome(&module, run, 0, &[], &mut ctx)
        .expect_err("the counter BIF stops the loop");
    // Byte-identical typed error and count to the custom-host run: the default hook
    // changes nothing observable, but the engine-side counter still works.
    assert_eq!(reason(err), "unsupported_opcode: stop");
    assert_eq!(machine.reductions(), CHARGES_FOR_CALLS);
    assert_eq!(calls.get(), SUCCESSFUL_CALLS + 1);
}

// ---------------------------------------------------------------------------
// the slice boundary: a suspend/resume charges every instruction exactly once
// ---------------------------------------------------------------------------

/// A receive that finds no message **is** a slice: the `wait` is charged like any
/// other instruction before the driver reports the suspension, and the resume starts
/// a fresh slice at the `wait`'s target label — so the `wait` is never charged twice,
/// and [`Machine::reductions`] reads the *resumed* slice's work, not a running total.
#[test]
fn a_suspended_receive_charges_its_wait_once_and_the_resume_starts_a_fresh_slice() {
    let (module, run) = suspending_receive_module();
    let mut machine = Machine::new();
    let mut ctx = ReduceCtx::new(u32::MAX);

    // --- slice 1: `run/0` finds an empty mailbox and suspends ---
    let outcome = machine
        .run_export_outcome(&module, run, 0, &[], &mut ctx)
        .expect("an empty mailbox suspends the receive, it does not fail the run");
    assert_eq!(
        outcome,
        RunOutcome::Wait {
            func: 0,
            pc: LABEL_HEAD as u32
        },
        "the `wait` reports its target label (the receive's loop head) as the resume address"
    );
    assert_eq!(ctx.calls, 0, "the matched path has not run yet");

    // The slice charged exactly the instructions it executed — the `wait` included.
    assert_eq!(machine.reductions(), FIRST_SLICE_CODE.len() as u64);
    assert_eq!(ctx.charges.len(), FIRST_SLICE_CODE.len());
    assert!(
        FIRST_SLICE_CODE.contains(&WAIT),
        "the suspending `wait` is one of the charged instructions of the slice it ends"
    );

    // `reset_reductions` re-bases a running host tally; the next slice would zero the
    // counter anyway (see the two-suspend suite below).
    assert_eq!(machine.reset_reductions(), FIRST_SLICE_CODE.len() as u64);
    assert_eq!(machine.reductions(), 0);

    // --- a message arrives; the process resumes where the `wait` said ---
    ctx.deliver(make_small(7));
    let outcome = machine
        .resume_outcome(&module, 0, LABEL_HEAD as u32, &mut ctx)
        .expect("the resumed receive matches the queued message");
    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(7)),
        "the run finished with the matched message, so the `wait` was NOT re-executed \
         (a resume at the `wait` itself would suspend again)"
    );

    // The resumed slice's count is slice-local: it starts at zero at the slice entry,
    // so it is the resume's own work, not `4 + 7`.
    assert_eq!(machine.reductions(), RESUMED_SLICE_CODE.len() as u64);
    assert!(
        !RESUMED_SLICE_CODE.contains(&WAIT),
        "the resume enters at the `wait`'s target label, so the `wait` is not among its instructions"
    );

    // The host hook saw **every** instruction of **both** slices, one charge each: the
    // two sequences together are the whole execution, so nothing was charged twice
    // across the boundary.
    assert_eq!(
        ctx.charges.len(),
        FIRST_SLICE_CODE.len() + RESUMED_SLICE_CODE.len()
    );
    assert!(
        ctx.charges.iter().all(|&n| n == 1),
        "every charge is one reduction, got {:?}",
        ctx.charges
    );
    assert_eq!(machine.reset_reductions(), RESUMED_SLICE_CODE.len() as u64);
    assert_eq!(ctx.calls, 1, "the post-match `bif0` ran exactly once");
}

/// Two suspensions in a row: every resume is its own slice with its own count, the
/// resume address advances one `wait` at a time, and the three slices together charge
/// exactly the instructions the module executed — `2 + 2 + 3`, derived from the code.
#[test]
fn each_resume_is_its_own_slice_and_no_instruction_is_charged_twice() {
    let (module, run) = two_waits_module();
    let mut machine = Machine::new();
    let mut ctx = ReduceCtx::new(u32::MAX);

    // A plain `wait` consults no mailbox: the three slices are driven purely by the
    // resume addresses, with no message involved.
    let mut executed: u64 = 0;
    for (slice, (start, charges)) in TWO_WAITS_SLICE_STARTS
        .iter()
        .zip(TWO_WAITS_SLICE_CHARGES)
        .enumerate()
    {
        let outcome = if slice == 0 {
            machine
                .run_export_outcome(&module, run, 0, &[], &mut ctx)
                .expect("the first `wait` suspends")
        } else {
            machine
                .resume_outcome(&module, 0, *start as u32, &mut ctx)
                .expect("the resumed slice runs from the address the previous `wait` named")
        };
        executed += charges;

        // The slice counter is that slice's work only: it is re-based at the slice
        // entry, so it never accumulates a running total.
        assert_eq!(
            machine.reductions(),
            charges,
            "slice {slice} (entering at code index {start}) charges its own instructions only"
        );
        // ...and the host hook saw the same number of charges, each of one reduction.
        assert_eq!(ctx.charges.len() as u64, executed);

        // The suspend address advances one `wait` target per suspend; the last slice
        // returns the `bif0`'s value instead.
        if slice + 1 < TWO_WAITS_SLICE_STARTS.len() {
            assert_eq!(
                outcome,
                RunOutcome::Wait {
                    func: 0,
                    pc: TWO_WAITS_SLICE_STARTS[slice + 1] as u32
                },
                "slice {slice} suspends at the next `wait`'s target label"
            );
        } else {
            assert_eq!(outcome, RunOutcome::Returned(make_small(1)));
        }
    }

    assert!(
        ctx.charges.iter().all(|&n| n == 1),
        "every charge is one reduction, got {:?}",
        ctx.charges
    );
    assert_eq!(ctx.calls, 1, "the `bif0` after the last resume ran exactly once");
}