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

//! The **resolved-but-unlinked local tail call** (`call_last`): tier parity, error
//! fidelity, and the structural pin that its per-instruction deopt is gone.
//!
//! [`Linker::emit_tail_exit_to_driver`](super::emit::link) is the third `call_last`
//! outcome: a target the module *defines* but this compilation emitted **no block** for
//! — the common case on the runtime's DEFAULT lazy per-function artifact, where a
//! `fib/1` → `fib/3` tail call crosses a function boundary nothing in the artifact maps.
//! Its contract is that the exit is the interpreter's own `call_last` by construction:
//! the same checked frame pop (`jit_tail_call` = `Machine::pop_frame_checked`) in the
//! same resolve-then-pop order, and then the resolved `(func, pc)` word in `rax` for the
//! driver — i.e. exactly the interpreter's `Step::Goto`.
//!
//! The equivalence is asserted **observationally** here, on both tiers, over the shapes
//! that can tell the two apart:
//!
//! * a *value* through a cross-function tail call (a wrong pop or a wrong word would
//!   change the value or the outcome);
//! * a cross-function tail call in both function orders (a *forward* reference into a
//!   later function — the real-module shape — and one into an earlier function);
//! * a **self** tail call (the *linked* route, `emit_tail_exit`), so a change to the
//!   unlinked arm cannot silently break the compiled one;
//! * a raise **inside** the tail-called function, which is what proves the pop did not
//!   break the unwind (the raise leaves the machine with the callee's own reason);
//! * a `deallocate` that does **not** match the live frame, which must keep the
//!   interpreter's exact typed frame error;
//! * an **undefined** label, the negative control that must still deopt and keep the
//!   interpreter's exact `unknown_label` error.
//!
//! The last test is the structural pin: emitting one function that contains the call
//! records the instruction as an *instance* and records **no** deopt for it — statically
//! at emission and at run time through the tier's `jit_step_one` counter — so a
//! regression that put the interpreter step back cannot pass.

use super::counters;
use super::emit::{compile_function, SpillSite};
use super::tests::{assert_same_outcome, build_module, error_reason, export_atom, with_both_tiers};
use super::*;
use crate::jit::Tier;
use crate::MinimalContext;
use beam_loader::{HeapNeed, ImportRef, Instr, Operand, NO_FAIL};
use beam_term::make_small;

/// The term a returned outcome carries, panicking (naming `label`) on anything else.
fn returned(label: &str, outcome: &Result<RunOutcome>) -> Eterm {
    match outcome {
        Ok(RunOutcome::Returned(value)) => *value,
        other => panic!("{label}: expected a returned term, got {other:?}"),
    }
}

/// The typed error an outcome carries, panicking (naming `label`) on anything else.
fn failed(label: &str, outcome: &Result<RunOutcome>) -> String {
    match outcome {
        Err(err) => error_reason(err),
        Ok(other) => panic!("{label}: expected a typed error, got {other:?}"),
    }
}

/// `g/0` — the tail-called leaf: return the small integer `42` from the frame it is
/// handed (the caller's frame is already gone, so a `Return` follows the caller's own
/// continuation).
fn callee_code() -> Vec<Instr> {
    vec![
        Instr::Label(Label(1)),
        Instr::Move {
            src: Operand::Integer(42),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
}

/// `f/0` — allocate a **one-slot** frame, then tail-call `target` popping it.
///
/// The `allocate` is what makes the pop observable: `call_last` pops *this* frame (the
/// callee runs in the caller's place), so the target's `return` follows the original
/// caller's continuation. Without a frame of its own the pop would reach the bottom
/// frame and the callee's `return` would find none.
fn caller_code(target: Label, deallocate: u32) -> Vec<Instr> {
    vec![
        Instr::Label(Label(2)),
        Instr::Label(Label(3)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::CallLast {
            arity: 0,
            target,
            deallocate,
        },
    ]
}

/// A two-function module whose `f/0` tail-calls `g/0` (returning `42`).
///
/// `target_first` decides which **function index** the callee occupies: `false` puts the
/// caller first, so the tail call is a *forward* reference into a later function (the
/// real-module shape, and — on the lazy per-function artifact — the unlinked route);
/// `true` puts the callee in an earlier function. The module defines the callee either
/// way, so both take [`Linker::emit_tail_exit_to_driver`](super::emit::link) unless the
/// artifact happened to compile that function.
fn tail_call_module(name: &str, target_first: bool) -> (Module, Atom) {
    let callee = ("g", 0u32, callee_code());
    let caller = ("f", 0u32, caller_code(Label(1), 1));
    let functions = if target_first {
        vec![callee, caller]
    } else {
        vec![caller, callee]
    };
    let module = build_module(name, &[], functions, &[("f", 0, Label(3))]);
    let name = export_atom(&module, "f");
    (module, name)
}

/// A module whose `f/0` tail-calls a **later** function through `call_last`, with the
/// callee's body `callee` and the pop's frame size `deallocate`.
fn tail_call_with(name: &str, callee: Vec<Instr>, deallocate: u32, target: Label) -> (Module, Atom) {
    let module = build_module(
        name,
        &[],
        vec![
            ("f", 0u32, caller_code(target, deallocate)),
            ("g", 0u32, callee),
        ],
        &[("f", 0, Label(3))],
    );
    let name = export_atom(&module, "f");
    (module, name)
}

// ---------------------------------------------------------------------------
// value identity across the unlinked tail exit
// ---------------------------------------------------------------------------

/// A tail call into a **later** function: the forward reference a real module emits
/// (`fib/1` → `fib/3`) and, on the runtime's default lazy per-function artifact, the
/// route that used to pay a full `jit_step_one` interpreter step on every execution.
///
/// The value is asserted on both tiers *and* through [`assert_same_outcome`], because a
/// wrong frame pop or a wrong successor word would change one of them.
#[test]
fn a_tail_call_to_a_later_function_agrees_with_the_interpreter() {
    let (module, name) = tail_call_module("tail_exit_forward", false);
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(
            "call_last/later-function",
            interpreter,
            baseline,
            &module.atom_table,
        );
        assert_eq!(
            (
                returned("interpreter", interpreter),
                returned("baseline", baseline)
            ),
            (make_small(42), make_small(42)),
            "the unlinked tail exit must reach the callee's own result"
        );
    });
}

/// The mirror order: the callee sits in an **earlier** function of the module. Both
/// orders must agree, so the exit cannot depend on the callee having been emitted first
/// (the lazy artifact compiles exactly one function at a time, so neither order links).
#[test]
fn a_tail_call_to_an_earlier_function_agrees_with_the_interpreter() {
    let (module, name) = tail_call_module("tail_exit_backward", true);
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(
            "call_last/earlier-function",
            interpreter,
            baseline,
            &module.atom_table,
        );
        assert_eq!(
            (
                returned("interpreter", interpreter),
                returned("baseline", baseline)
            ),
            (make_small(42), make_small(42)),
        );
    });
}

/// The **linked** route (`emit_tail_exit`) next to the unlinked one: a `call_last` whose
/// target is a later block of the **same** function — a compiled block's entry, so this
/// compilation emits the direct jump — must keep working.
#[test]
fn a_self_tail_call_agrees_with_the_interpreter() {
    let module = build_module(
        "tail_exit_self",
        &[],
        vec![(
            "f",
            0u32,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::Allocate {
                    stack_need: HeapNeed::Words(0),
                    live: 0,
                },
                Instr::Move {
                    src: Operand::Integer(7),
                    dst: Operand::X(0),
                },
                Instr::CallLast {
                    arity: 0,
                    target: Label(3),
                    deallocate: 0,
                },
                Instr::Label(Label(3)),
                Instr::Move {
                    src: Operand::Integer(42),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        )],
        &[("f", 0, Label(2))],
    );
    let name = export_atom(&module, "f");
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(
            "call_last/self",
            interpreter,
            baseline,
            &module.atom_table,
        );
        assert_eq!(
            (
                returned("interpreter", interpreter),
                returned("baseline", baseline)
            ),
            (make_small(42), make_small(42)),
            "the linked tail call must overwrite the first block's result"
        );
    });
}

// ---------------------------------------------------------------------------
// error fidelity through the unlinked tail exit
// ---------------------------------------------------------------------------

/// A raise **inside** the tail-called function: the pop has already happened when the
/// callee runs, so the unwind must carry the callee's own reason on both tiers. The
/// callee calls a BIF the [`MinimalContext`] host rejects, which is the engine's typed
/// `unsupported_opcode: …` raise.
#[test]
fn a_raise_inside_the_tail_callee_agrees_with_the_interpreter() {
    let callee = vec![
        Instr::Label(Label(1)),
        Instr::Bif {
            arity: 0,
            bif: ImportRef(0),
            fail: NO_FAIL,
            args: vec![],
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let module = build_module(
        "tail_exit_raise",
        &[("erlang", "nope", 0)],
        vec![
            ("f", 0u32, caller_code(Label(1), 1)),
            ("g", 0u32, callee),
        ],
        &[("f", 0, Label(3))],
    );
    let name = export_atom(&module, "f");
    with_both_tiers(&module, name, |interpreter, baseline| {
        let interpreter_reason = failed("interpreter", interpreter);
        let baseline_reason = failed("baseline", baseline);
        assert_eq!(
            interpreter_reason, baseline_reason,
            "a raise inside the tail callee must surface identically"
        );
        assert!(
            interpreter_reason.contains("bif erlang:nope"),
            "the reason must be the callee's own BIF raise, got {interpreter_reason}"
        );
    });
}

/// A `deallocate` that does **not** match the live frame: the pop goes through the same
/// `jit_tail_call` helper, so the failure must be the interpreter's exact typed error
/// (`deallocate: frame size mismatch`) — never a wrong transfer and never a panic.
#[test]
fn a_tail_call_whose_frame_size_does_not_match_agrees_with_the_interpreter() {
    let (module, name) = tail_call_with("tail_exit_mismatch", callee_code(), 5, Label(1));
    with_both_tiers(&module, name, |interpreter, baseline| {
        let interpreter_reason = failed("interpreter", interpreter);
        let baseline_reason = failed("baseline", baseline);
        assert_eq!(
            interpreter_reason, baseline_reason,
            "the checked pop must report the identical frame error on both tiers"
        );
        assert!(
            interpreter_reason.contains("frame size mismatch"),
            "the reason must be the frame-size error, got {interpreter_reason}"
        );
    });
}

/// The negative control: a label the module does **not** define keeps the documented
/// deopt, so the interpreter's own `call_last` reports its exact `unknown_label` error
/// (`beam_engine::error::unknown_label` → `BeamError::BadArg`) on both tiers.
#[test]
fn an_undefined_tail_call_label_keeps_the_interpreters_unknown_label_error() {
    let (module, name) = tail_call_with("tail_exit_undefined", callee_code(), 1, Label(99));
    with_both_tiers(&module, name, |interpreter, baseline| {
        let interpreter_reason = failed("interpreter", interpreter);
        let baseline_reason = failed("baseline", baseline);
        assert_eq!(
            interpreter_reason, baseline_reason,
            "an undefined label must keep the interpreter's own error"
        );
        assert_eq!(
            interpreter_reason, "BadArg",
            "`unknown_label` is `BeamError::BadArg`"
        );
    });
}

// ---------------------------------------------------------------------------
// the structural pin: no `CallLast` deopt, statically or at run time
// ---------------------------------------------------------------------------

/// Emitting a function whose body contains a `call_last` into another function of the
/// module must record the instruction as an **instance** and record **no** deopt for it:
///
/// * statically — `counters::record_instr_deopt` is reached only from the deopt wrapper
///   ([`super::emit`]'s `emit_deopt`), which every refusal inside the lowering calls, so
///   a surviving per-instruction deopt leaves an `instr_deopts["CallLast"]` entry;
/// * at run time — the tier's `jit_step_one` counter (`deopts_by_variant`, keyed by
///   opcode) counts exactly the interpreter steps a compiled block pays, so the
///   interpreter step this exit replaced would show up there;
/// * and non-vacuously — the instruction really was seen (`instr_instances`), the
///   function really emitted bytes, and the exit spilled at the **exit** site (the site
///   only the exits use) rather than the helper-deopt site.
///
/// The counters are test-only and thread-local, so `reset` at the start of each half is
/// enough isolation.
#[test]
fn the_unlinked_tail_exit_records_no_call_last_deopt() {
    let (module, name) = tail_call_module("tail_exit_pin", false);
    let labels = crate::interpreter::build_label_map(&module);
    crate::jit::cache::clear_baseline_cache();

    // (a) the static view: emitting `f/0` alone, cache-free, so a warmed `CodeCache`
    // cannot make the count read zero for the wrong reason.
    counters::reset();
    counters::set_enabled(true);
    let compiled = compile_function(&module, &labels, 0).expect("f/0 compiles");
    counters::set_enabled(false);
    let emitted = counters::snapshot();

    assert_eq!(
        emitted.instr_instances.get("CallLast").copied(),
        Some(1),
        "the emitter must visit the `call_last` (an instance), not skip the block"
    );
    assert_eq!(
        emitted.instr_deopts.get("CallLast").copied().unwrap_or(0),
        0,
        "the resolved cross-function `call_last` must not be deopted; static deopts: {:?}",
        emitted.instr_deopts,
    );
    assert!(
        emitted.block_emitted_bytes > 0,
        "the fixture's block must emit native bytes"
    );
    assert!(
        compiled.entries.iter().any(|entry| entry.is_some()),
        "the fixture's function must compile at least one block"
    );
    assert!(
        emitted
            .spill_calls_by_site
            .get(&SpillSite::Exit)
            .copied()
            .unwrap_or(0)
            >= 1,
        "the tail exit spills the cached x-registers at the exit site — the one contract \
         no exit may skip; spills by site: {:?}",
        emitted.spill_calls_by_site,
    );

    // (b) the run-time view: the same module through the real runtime path (the lazy
    // per-function artifact), which is where the replaced `jit_step_one` used to be paid.
    counters::reset();
    counters::set_enabled(true);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let outcome = machine.run_export_outcome_with_tier(
        &module,
        name,
        0,
        &[],
        &mut ctx,
        Tier::BaselineJit,
    );
    counters::set_enabled(false);
    let run = counters::snapshot();

    assert_eq!(
        returned("baseline", &outcome),
        make_small(42),
        "the pinned run must still reach the callee's result"
    );
    assert_eq!(
        run.deopts_by_variant.get("CallLast").copied().unwrap_or(0),
        0,
        "no `CallLast` interpreter step may remain; run-time deopts by opcode: {:?}",
        run.deopts_by_variant,
    );
    assert!(
        run.block_emitted_bytes > 0,
        "the run's blocks must emit native bytes"
    );

    println!("=== unlinked call_last structural pin ===");
    println!(
        "  static: CallLast instances={} deopts={} | block emitted bytes={} | spills by site={:?}",
        emitted.instr_instances.get("CallLast").copied().unwrap_or(0),
        emitted.instr_deopts.get("CallLast").copied().unwrap_or(0),
        emitted.block_emitted_bytes,
        emitted.spill_calls_by_site,
    );
    println!(
        "  runtime: step_one={} deopts by opcode={:?}",
        run.step_one, run.deopts_by_variant,
    );
}

/// The exit really does hand the driver a word the **driver** resolves: the byte-level
/// evidence (the `jit_tail_call` helper call, the resolved successor word, and the
/// absence of the `jit_step_one` helper) is pinned in [`super::emit::link`]'s own test
/// module, which owns the `emitted` byte view; this test pins the *decision* — that the
/// same instruction, made undefined, DOES deopt (so the pin above cannot pass because
/// the emitter simply never reaches a `call_last`).
#[test]
fn the_undefined_label_variant_of_the_pin_still_deopts() {
    let (module, _name) = tail_call_with("tail_exit_pin_negative", callee_code(), 1, Label(99));
    let labels = crate::interpreter::build_label_map(&module);

    counters::reset();
    counters::set_enabled(true);
    compile_function(&module, &labels, 0).expect("f/0 compiles");
    counters::set_enabled(false);
    let emitted = counters::snapshot();

    assert_eq!(
        emitted.instr_deopts.get("CallLast").copied(),
        Some(1),
        "an undefined label must keep the deopt (the interpreter's `unknown_label`); \
         static deopts: {:?}",
        emitted.instr_deopts,
    );
    assert_eq!(
        emitted.instr_instances.get("CallLast").copied(),
        Some(1),
        "the negative control must visit the same instruction"
    );
    println!(
        "  negative control (undefined label): instances={} deopts={} spills by site={:?}",
        emitted.instr_instances.get("CallLast").copied().unwrap_or(0),
        emitted.instr_deopts.get("CallLast").copied().unwrap_or(0),
        emitted.spill_calls_by_site,
    );
}
