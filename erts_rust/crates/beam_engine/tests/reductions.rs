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

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

use beam_common::{BeamError, Result};
use beam_engine::{ExecutionContext, Machine, MinimalContext};
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
// A custom ExecutionContext: a counting BIF and a record-keeping reduce hook
// ---------------------------------------------------------------------------

/// A context whose `host:counter/0` BIF succeeds `stop_after` times and then returns a
/// typed error, and whose [`reduce`](ExecutionContext::reduce) records every charge
/// (and, with a `budget`, pre-empts after exactly that many).
struct ReduceCtx {
    /// The BIF succeeds while `calls <= stop_after`, then returns a typed error.
    stop_after: u32,
    /// When set, `reduce` returns a typed error once it has accepted this many charges.
    budget: Option<u32>,
    /// The `n` of every `reduce` call, in order (the failing call included).
    charges: Vec<u32>,
    /// The number of `counter/0` calls made.
    calls: u32,
}

impl ReduceCtx {
    fn new(stop_after: u32) -> ReduceCtx {
        ReduceCtx {
            stop_after,
            budget: None,
            charges: Vec::new(),
            calls: 0,
        }
    }

    fn with_budget(mut self, budget: u32) -> ReduceCtx {
        self.budget = Some(budget);
        self
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
