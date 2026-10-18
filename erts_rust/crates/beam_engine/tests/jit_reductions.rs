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
//! The **native tier's block-entry reduction charge**: the driver's one pre-emption
//! point for a loop that never crosses the interpreter's per-instruction hook.
//!
//! # Why the charge exists, and why it is taken at the block entry
//!
//! [`ExecutionContext::reduce`] is offered once per **interpreted** instruction by
//! `Machine::drive`, so a native basic block — which runs without going through
//! `step_instruction` at its top level — used to cost the budget nothing: a compiled
//! loop whose work is BIFs and calls (no `test_heap`/`allocate_heap`, hence no
//! heap-admission door either) could run a whole scheduling slice for free. The
//! baseline-JIT driver therefore charges **one unit per driver-loop iteration** —
//! one block entry, or the per-instruction interpreter step it takes for an address
//! no block covers — *before* entering the block.
//!
//! The position is load-bearing: the charge is only a *pre-emption* (rather than a
//! wrong result) if the machine's published position is the address about to run
//! and nothing of it has run yet, because the runner parks an exhausted machine at
//! `Machine::current_code` and re-enters there. The driver publishes `(func, pc)`
//! itself immediately before charging, so a refusal parks exactly at the block entry
//! and the resume executes that block **once** — no prefix replay. (A refusing
//! charge from inside the block's call-resolution seam was measured to replay the
//! block prefix; see `crate::jit::baseline`'s charge comment.)
//!
//! # What is pinned here (structural only — no wall clock)
//!
//! * a compiled loop really does end its slice after a **bounded** number of blocks,
//!   with the machine parked on the block entry it refused at;
//! * the resumed slice continues the loop **exactly once** per iteration (no replay);
//! * with no reduce hook (`wants_reduce() == false`) the same loop completes with the
//!   same result, the same iteration count and **zero** hook calls, while the slice
//!   counter still counts the block entries;
//! * the slice counter is monotone in the number of block entries; and
//! * the per-**block** unit does not materially inflate what a slice reports relative
//!   to the interpreter's per-instruction unit on a real, call-heavy module (the
//!   measured ratio, with a generous bound — the shape is deterministic, the exact
//!   ratio depends on the module's block structure).
use std::collections::BTreeMap;
use beam_common::{BeamError, Result};
use beam_engine::{ExecutionContext, Machine, Tier};
use beam_loader::{
    load_file, Export, Function, Import, ImportRef, Instr, Label, Module, Operand,
};
use beam_term::{is_small, make_small, signed_value, Eterm};
/// The `beam_loader` oracle fixture (reused, never copied): a real `erlc`-produced
/// module, so the ratio measured below is over real compiler output.
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);
// ---------------------------------------------------------------------------
// A synthetic non-allocating loop: `label 1 | bif0 host:counter/0 x0 | jump 1`
// ---------------------------------------------------------------------------
/// The code index the `jump` returns to: the loop head's `label` marker.
const LOOP_HEAD: u32 = 0;
/// The number of successful `counter/0` calls before the BIF stops the loop, and
/// therefore also the number of loop iterations that complete.
const SUCCESSFUL_CALLS: u32 = 5;
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
/// A builder for one synthetic module with its own atom table. (The same shape
/// `tests/reductions.rs` uses, trimmed to what these suites need.)
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
/// The non-allocating counting loop — no `allocate`/`test_heap` anywhere, so no
/// heap-admission door is ever reached and the block-entry charge is the only
/// pre-emption point this loop has.
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
// ---------------------------------------------------------------------------
// The test context: a counting BIF plus a record-keeping / pre-empting reduce hook
// ---------------------------------------------------------------------------
/// A context whose `host:counter/0` BIF succeeds `stop_after` times and then returns a
/// typed error (so the loop terminates deterministically), whose
/// [`reduce`](ExecutionContext::reduce) records every charge and — with a `budget` —
/// refuses (with a typed error) the charge that would exceed it, and which can
/// **decline the hook** entirely to model a host that does not pre-empt.
struct Ctx {
    stop_after: u32,
    budget: Option<u32>,
    /// The `n` of every `reduce` call, in order (the refusing call included). Never
    /// cleared, so it records every slice's charges.
    charges: Vec<u32>,
    calls: u32,
    decline_hook: bool,
}
impl Ctx {
    fn new(stop_after: u32) -> Ctx {
        Ctx {
            stop_after,
            budget: None,
            charges: Vec::new(),
            calls: 0,
            decline_hook: false,
        }
    }
    fn with_budget(mut self, budget: u32) -> Ctx {
        self.budget = Some(budget);
        self
    }
    fn declining_the_hook(mut self) -> Ctx {
        self.decline_hook = true;
        self
    }
}
impl ExecutionContext for Ctx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if module == "host" && function == "counter" && arity == 0 {
            self.calls += 1;
            return if self.calls > self.stop_after {
                Err(beam_engine::unsupported("stop"))
            } else {
                Ok(make_small(self.calls as i64))
            };
        }
        // The real fixture's arithmetic (`slice.beam`'s `fib/1`).
        arith_hook(module, function, arity, args)
    }    fn ensure_heap(
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
    fn wants_reduce(&self) -> bool {
        !self.decline_hook
    }
}
/// The small-int arithmetic the real `slice.beam` fixture calls.
fn arith_hook(module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
    if module != "erlang" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    match function {
        "+" => Ok(make_small(a + b)),
        "-" => Ok(make_small(a - b)),
        "*" => Ok(make_small(a.wrapping_mul(b))),
        _ => Err(BeamError::BadArg),
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
/// Drive the counting loop on the native tier, returning the module (kept alive for a
/// resume), the machine and the outcome.
fn run_native_loop(ctx: &mut Ctx) -> (Module, Machine, Result<beam_engine::RunOutcome>) {
    let (module, run) = counting_loop_module();
    let mut machine = Machine::new();
    let outcome = machine.run_export_outcome_with_tier(&module, run, 0, &[], ctx, Tier::BaselineJit);
    (module, machine, outcome)
}
// ---------------------------------------------------------------------------
// the slice ends at a block entry, and the resume replays nothing
// ---------------------------------------------------------------------------
/// A compiled loop really does end its scheduling slice after a **bounded** number of
/// block entries, with the machine parked on the entry it refused at — and the
/// resumed slice continues the loop without replaying the block prefix.
#[test]
fn a_compiled_loop_ends_its_slice_at_a_block_entry() {
    const BUDGET: u32 = 3;
    let mut ctx = Ctx::new(SUCCESSFUL_CALLS).with_budget(BUDGET);
    let (module, mut machine, outcome) = run_native_loop(&mut ctx);
    let err = outcome.expect_err("the block-entry charge ends the slice");
    assert_eq!(reason(err), "unsupported_opcode: reduction budget exhausted");
    // Exactly `BUDGET` charges were accepted and the next one refused, so the slice
    // ran `BUDGET` iterations of the loop (and nothing else — the loop has one block).
    assert_eq!(ctx.charges.len() as u32, BUDGET + 1, "accepted charges plus the refusal");
    assert!(ctx.charges.iter().all(|n| *n == 1), "one unit per block");
    // The loop's blocks: the `label 1` marker is a block entry of its own, so one
    // loop iteration is two driver round-trips — the budget therefore bounds the
    // iteration count from above without pinning the block structure here.
    assert!(
        ctx.calls <= BUDGET,
        "the number of loop iterations is bounded by the slice budget ({} calls)",
        ctx.calls
    );
    assert_eq!(
        machine.reductions(),
        u64::from(BUDGET) + 1,
        "the slice counter agrees with the hook (the refusing charge included)"
    );
    // The parked address is the block entry the charge belonged to — the loop head,
    // not some mid-block instruction (the tier publishes block granularity).
    let (func, pc) = machine
        .current_code()
        .expect("a pre-empted machine has a resumable address");
    assert!(
        pc == LOOP_HEAD || pc == 1,
        "parked on a compiled block entry of the loop, not mid-block (pc {pc})"
    );
    // Resume without a budget: the loop finishes, and every iteration ran exactly
    // once across the two slices — the refused block was not executed twice, and no
    // prefix of it was replayed.
    ctx.budget = None;
    let err = machine
        .resume_outcome_with_tier(&module, func, pc, &mut ctx, Tier::BaselineJit)
        .err()
        .expect("the counter BIF stops the loop");
    assert_eq!(reason(err), "unsupported_opcode: stop");
    assert_eq!(
        ctx.calls,
        SUCCESSFUL_CALLS + 1,
        "each iteration ran once across the two slices (no replay, no skipped block)"
    );
}// ---------------------------------------------------------------------------
// with no reduce hook the loop is unchanged
// ---------------------------------------------------------------------------
/// A host that declines the hook sees **zero** hook calls and the same result and
/// iteration count as one that accepts every charge — while the engine-side slice
/// counter still counts the block entries (the interpreter driver's rule: the
/// counter is bumped whether or not the hook is offered the charge).
#[test]
fn with_no_reduce_hook_the_compiled_loop_is_unchanged() {
    let mut charging = Ctx::new(SUCCESSFUL_CALLS);
    let (_m1, charging_machine, charging_outcome) = run_native_loop(&mut charging);
    let charging_err = charging_outcome.err().expect("the counter BIF stops the loop");
    let mut declining = Ctx::new(SUCCESSFUL_CALLS).declining_the_hook();
    let (_m2, declining_machine, declining_outcome) = run_native_loop(&mut declining);
    let declining_err = declining_outcome.err().expect("the counter BIF stops the loop");
    assert!(declining.charges.is_empty(), "a declining host is never charged");
    assert_eq!(charging.charges.len() as u64, charging_machine.reductions());
    assert_eq!(reason(declining_err), reason(charging_err));
    assert_eq!(declining.calls, charging.calls);
    assert_eq!(
        declining_machine.reductions(),
        charging_machine.reductions(),
        "the counter counts block entries either way"
    );
}
// ---------------------------------------------------------------------------
// the counter is monotone in the number of block entries
// ---------------------------------------------------------------------------
/// The slice counter grows strictly with the number of loop iterations, and equals
/// the number of block entries the driver took (one per iteration, plus the final
/// iteration's raising `bif0` block).
#[test]
fn the_slice_counter_is_monotone_in_block_count() {
    let mut counts = Vec::new();
    for stop_after in [1u32, 2, 4, 8] {
        let mut ctx = Ctx::new(stop_after);
        let (_m, machine, outcome) = run_native_loop(&mut ctx);
        outcome.err().expect("the counter BIF stops the loop");
        let charged = machine.reductions();
        assert_eq!(
            ctx.charges.len() as u64,
            charged,
            "the hook saw every block entry the counter counted"
        );
        assert!(
            charged > u64::from(stop_after),
            "every loop iteration costs at least one block entry ({charged} charges for {stop_after} iterations)"
        );
        counts.push(charged);
    }
    for pair in counts.windows(2) {
        assert!(pair[1] > pair[0], "counter is strictly monotone: {counts:?}");
    }
}
// ---------------------------------------------------------------------------
// the per-block unit against the interpreter's per-instruction unit
// ---------------------------------------------------------------------------
/// The measured relationship between the two tiers' units on the same work, with a
/// host that accepts every charge.
///
/// The native tier charges per **block entry** and the interpreter per
/// **instruction**, so for the synthetic counting loop — `label 1 | bif0 | jump 1`,
/// whose `label` marker is a block entry of its own, i.e. two blocks per iteration
/// against three interpreted instructions — the native slice reports **two thirds**
/// of the interpreter's units. Both counts are deterministic, so this pins the
/// *shape* of the pricing: the per-block unit under-counts (a block holds several
/// instructions) but does not inflate what a slice reports, and does not collapse
/// the loop to "free".
#[test]
fn the_block_unit_prices_each_loop_iteration() {
    let mut interpreter = Ctx::new(SUCCESSFUL_CALLS);
    let (module, run) = counting_loop_module();
    let mut interpreter_machine = Machine::new();
    interpreter_machine
        .run_export_outcome_with_tier(&module, run, 0, &[], &mut interpreter, Tier::Interpreter)
        .err()
        .expect("the counter BIF stops the loop");
    let mut jit = Ctx::new(SUCCESSFUL_CALLS);
    let (_module, jit_machine, outcome) = run_native_loop(&mut jit);
    outcome.err().expect("the counter BIF stops the loop");
    assert_eq!(interpreter.calls, jit.calls, "both tiers run the same iterations");
    assert_eq!(
        interpreter.charges.len() as u64,
        interpreter_machine.reductions(),
        "the interpreter charges one unit per instruction"
    );
    assert_eq!(
        jit.charges.len() as u64,
        jit_machine.reductions(),
        "the native tier charges one unit per block entry"
    );
    // This synthetic loop is **one block per instruction** (each of `label`,
    // `bif0`, `jump` is its own block entry — measured), so on it the per-block unit
    // and the per-instruction unit coincide exactly: `N` iterations cost
    // `3·N − 1` charges on both tiers. That is the "no distortion" end of the scale;
    // the linked tail-call case below is the other end, and the invariant that holds
    // on both is that per-block pricing never reports MORE than per-instruction
    // pricing for the same work.
    let iterations = u64::from(SUCCESSFUL_CALLS) + 1;
    assert_eq!(interpreter_machine.reductions(), 3 * iterations - 1);
    assert_eq!(jit_machine.reductions(), interpreter_machine.reductions());
    assert!(jit_machine.reductions() <= interpreter_machine.reductions());
}

/// A **real** compiler output whose hot loop is a natively linked local tail call
/// (`slice.beam`'s tail-recursive `fib/1`) is priced at **one unit per driver
/// round-trip**, not per iteration: the linker turns each `call_last` into a direct
/// jump between compiled blocks, so the whole loop stays inside one driver entry.
///
/// This is the honest upper bound on the block-entry charge's resolution, and it is
/// the residual pre-emption gap on the default tier: such a chain is priced once and
/// (having no refusing seam inside it) cannot end its slice. Recorded here so the
/// ratio is not mistaken for a measurement error.
#[test]
fn a_natively_linked_tail_call_loop_costs_one_round_trip() {
    let module = load_file(SLICE_BEAM).expect("load slice.beam");
    let fib = module
        .atom_table
        .lookup("fib")
        .expect("slice.beam exports fib/1");
    let args = [make_small(18)];
    let mut interpreter = Ctx::new(0);
    let mut interpreter_machine = Machine::new();
    let expected = interpreter_machine
        .run_with_tier(&module, fib, 1, &args, &mut interpreter, Tier::Interpreter)
        .expect("fib/1 runs on the interpreter");
    let mut jit = Ctx::new(0);
    let mut jit_machine = Machine::new();
    let got = jit_machine
        .run_with_tier(&module, fib, 1, &args, &mut jit, Tier::BaselineJit)
        .expect("fib/1 runs on the native tier");
    assert_eq!(got, expected, "both tiers compute the same value");
    assert!(interpreter_machine.reductions() > 100);
    assert!(
        jit_machine.reductions() * 8 < interpreter_machine.reductions(),
        "a linked tail-call chain is one round-trip (jit {}, interpreter {})",
        jit_machine.reductions(),
        interpreter_machine.reductions()
    );
    assert!(jit_machine.reductions() > 0, "the run is still priced");
}
