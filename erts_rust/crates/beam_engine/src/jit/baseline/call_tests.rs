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
//! Tier-equivalence and structural pins for the frame-preserving local `call` lowering
//! (`super::call`). A `#[path]` child of `super::tests`, so it shares that module's
//! harness (`with_both_tiers`, `assert_same_outcome`, `body_module`, `build_module`).
//!
//! The subject is [`Instr::Call`] — the **frame-preserving** local call, the one local
//! call form that needs a continuation. The two **tail** forms (`call_last`/`call_only`)
//! are already linked natively (see `super::emit::link`); this suite covers the third:
//!
//! * the continuation store itself, which lives in Rust-side `Machine` frame state a
//!   compiled block cannot address, so it goes through the `super::call::jit_call_store`
//!   helper while the transfer to the callee reuses the linker's exit;
//! * the transfer, which is a direct linked jump when the callee's entry is a compiled
//!   block of this compilation and a returned `(func, pc)` word to the driver otherwise;
//! * the inevitable refusal — a `call` whose target label is not in the module's label
//!   map — which must still be **exactly** the interpreter's own behaviour (a deopt to
//!   `jit_step_one`, never an invented target).

use super::call::CALL_STORED;
use super::emit::{allocate_registers, cacheable_touches, compile_function, compile_module};
use super::*;
use crate::jit::baseline::BaselineJit;
use crate::jit::JitBackend;
/// Run exported `name/arity` with `args` on both tiers and hand the two outcomes to
/// `check` — the arity-parameterised sibling of [`super::with_both_tiers`] (which
/// hard-codes arity 0), for the multi-argument calls below.
///
/// Like its sibling, each run builds its boxed terms on its **own** [`MinimalContext`]
/// heap, so the comparison has to happen inside this call: returning the outcomes and
/// comparing them after the contexts were dropped would dereference freed memory.
fn with_both_tiers_args<R>(
    module: &Module,
    name: Atom,
    arity: u32,
    args: &[Eterm],
    check: impl FnOnce(&Result<RunOutcome>, &Result<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        name,
        arity,
        args,
        &mut interpreter_ctx,
        Tier::Interpreter,
    );
    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        name,
        arity,
        args,
        &mut baseline_ctx,
        Tier::BaselineJit,
    );
    check(&interpreter_outcome, &baseline_outcome)
}

// ---------------------------------------------------------------------------
// (a) outcome identity
// ---------------------------------------------------------------------------

/// The [`super::local_call_case`] shape — `allocate 0 0`, `call f/0 -> g/0`,
/// `deallocate_return 0` in the caller and a leaf callee whose `return` follows the
/// continuation the caller's `call` stored — must produce the **same** observable
/// result on both tiers.
///
/// This is the end-to-end evidence for the lowering: the callee's `x(0)` (the small
/// integer `7`) reaches the caller's return **through the continuation slot the helper
/// wrote**. On the baseline tier the helper runs (`jit_call_store`), the block
/// transfers, the callee returns to the continuation's **sub-block**, and the
/// `deallocate_return` yields `x(0)`.
#[test]
fn the_frame_preserving_local_call_agrees_with_the_interpreter() {
    let (module, name) = local_call_case();
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_eq!(
            interpreter.as_ref().expect("the interpreter completes"),
            &RunOutcome::Returned(make_small(7)),
            "the leaf's `return` must transfer x(0) through the caller's continuation"
        );
        assert_same_outcome("local call", interpreter, baseline, &module.atom_table);
    });
}

/// A local call **with arguments**: the caller passes `x(0)`/`x(1)` through the call
/// (the callee's register-argument convention is the interpreter's — the call's `arity`
/// is the callee's argument count, not part of the continuation store) and the callee's
/// `move x(1) -> x(0)` result arrives back at the caller.
#[test]
fn a_two_argument_local_call_agrees_with_the_interpreter() {
    let module = build_module(
        "deopt_call2",
        &[],
        vec![
            (
                "f",
                2,
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::Allocate {
                        stack_need: beam_loader::HeapNeed::Words(0),
                        live: 0,
                    },
                    Instr::Call {
                        arity: 2,
                        target: Label(3),
                    },
                    Instr::DeallocateReturn(0),
                ],
            ),
            (
                "g",
                2,
                vec![
                    Instr::Label(Label(3)),
                    Instr::Move {
                        src: Operand::X(1),
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 2, Label(2))],
    );
    let name = export_atom(&module, "f");
    let args = [make_small(11), make_small(22)];
    with_both_tiers_args(&module, name, 2, &args, |interpreter, baseline| {
        assert_eq!(
            interpreter.as_ref().expect("the interpreter completes"),
            &RunOutcome::Returned(make_small(22)),
            "the callee's x(1) must travel back through the caller's continuation"
        );
        assert_same_outcome("2-arg local call", interpreter, baseline, &module.atom_table);
    });
}

/// A **nested** local call chain `f/0 -> g/0 -> h/0`: each frame-preserving call in the
/// chain writes its own continuation into its own live frame, and each callee's `return`
/// lands on it, so the innermost `x(0)` (`5`) unwinds both continuations back to `f/0`'s
/// entry (which returns through the bottom `Halt`).
///
/// A chain — rather than a single call — is what makes the store observable as *per
/// frame*: the two continuations are distinct addresses in two different functions and
/// both must be installed, in the right frames, for the result to arrive (and for the
/// two `deallocate_return`s to pop the right frames).
#[test]
fn a_nested_local_call_chain_agrees_with_the_interpreter() {
    let module = build_module(
        "deopt_nested_call",
        &[],
        vec![
            (
                "f",
                0,
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::Allocate {
                        stack_need: beam_loader::HeapNeed::Words(0),
                        live: 0,
                    },
                    Instr::Call {
                        arity: 0,
                        target: Label(3),
                    },
                    Instr::DeallocateReturn(0),
                ],
            ),
            (
                "g",
                0,
                vec![
                    Instr::Label(Label(3)),
                    Instr::Allocate {
                        stack_need: beam_loader::HeapNeed::Words(0),
                        live: 0,
                    },
                    Instr::Call {
                        arity: 0,
                        target: Label(4),
                    },
                    Instr::DeallocateReturn(0),
                ],
            ),
            (
                "h",
                0,
                vec![
                    Instr::Label(Label(4)),
                    Instr::Move {
                        src: Operand::Integer(5),
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 0, Label(2))],
    );
    let name = export_atom(&module, "f");
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_eq!(
            interpreter.as_ref().expect("the interpreter completes"),
            &RunOutcome::Returned(make_small(5)),
            "the innermost result must unwind both continuations"
        );
        assert_same_outcome("nested local call", interpreter, baseline, &module.atom_table);
    });
}

// ---------------------------------------------------------------------------
// (b) the static deopt is gone
// ---------------------------------------------------------------------------

/// **The `call` no longer costs a per-instruction interpreter step** — the structural
/// pin, in its two halves: the **emitter's own dispatch decision** and the **run's**
/// deopt attribution.
///
/// The static half is measured on a **cache-free** compile ([`compile_function`] lowers
/// the function straight into its own buffer), so it can never observe a thread-local
/// `CodeCache` hit — a hit re-uses an already-emitted artifact and would record nothing,
/// making the pin vacuous rather than red. It pins that the one `call` of this module was
/// **visited** by the emitter yet did **not** take the `emit_deopt` path.
///
/// The run-time half drives the module on the baseline tier with the counters recording:
/// the whole fixture is lowered — the caller's `allocate`/`deallocate_return` (the heap /
/// frame-admission and frame-discipline families), the `call`, and the callee's
/// `move`/`return` — so the run must pay **zero** interpreter steps and must not attribute
/// a single deopt to `Call`.
#[test]
fn the_frame_preserving_local_call_records_no_static_deopt() {
    let (module, name) = local_call_case();

    // (1) The emitter's dispatch decision, on a compile that cannot hit the cache.
    super::counters::reset();
    super::counters::set_enabled(true);
    let labels = crate::interpreter::build_label_map(&module);
    let _function = compile_function(&module, &labels, 0).expect("f/0 compiles");
    super::counters::set_enabled(false);
    let emitted = super::counters::snapshot();
    assert!(
        emitted.instr_instances.get("Call").copied().unwrap_or(0) > 0,
        "`Call` must have been visited by the emitter, or this pin proves nothing; \
         instances: {:?}",
        emitted.instr_instances,
    );
    assert_eq!(
        emitted.instr_deopts.get("Call").copied().unwrap_or(0),
        0,
        "`Call` is lowered, so it must record zero static deopts; deopts: {:?}",
        emitted.instr_deopts,
    );

    // (2) The run: the call must execute natively (its result arrives) and no deopt may be
    // attributed to it.
    let (outcome, snapshot) = run_counting(&module, name, Tier::BaselineJit);
    assert_eq!(
        outcome.expect("the run completes"),
        RunOutcome::Returned(make_small(7)),
        "the lowered fixture must still return the callee's x(0)"
    );
    assert!(
        !snapshot.deopts_by_variant.contains_key("Call"),
        "`Call` must be lowered natively, but it deopted {} time(s) at run time; \
         deopts by variant: {:?}",
        snapshot.deopts_by_variant.get("Call").copied().unwrap_or(0),
        snapshot.deopts_by_variant,
    );
    assert_eq!(
        snapshot.step_one, 0,
        "every instruction of this fixture is lowered, so no interpreter step may \
         remain; deopts by variant: {:?}",
        snapshot.deopts_by_variant,
    );
}

// ---------------------------------------------------------------------------
// (c) the continuation still gets a native sub-block
// ---------------------------------------------------------------------------

/// The `call` is lowered **in-block**, so the continuation address it stores is what the
/// driver re-enters after the callee returns. Nothing about that changed: the address
/// still lies strictly inside an IR block (the loader cuts blocks only at labels), so it
/// still needs — and still gets — its own native **sub-block** in **both** artifact
/// layouts, and `block_count` still counts IR blocks only.
///
/// The property is reproduced here from this file (the sibling pin in `super::tests`
/// describes the sub-block mechanism, this one guards it against the new `call` arm).
#[test]
fn the_lowered_call_still_gives_its_continuation_a_native_sub_block() {
    let (module, _) = local_call_case();
    let labels = crate::interpreter::build_label_map(&module);
    let function = compile_function(&module, &labels, 0).expect("f/0 compiles");

    // `f/0`'s `call` sits at pc 3 inside the `Label(2)` block `[1, 5)`, so the
    // continuation pc 4 is strictly inside that block.
    assert!(function.entries[4].is_none(), "pc 4 is not an IR block start");
    assert!(function.sub_entries[4].is_some(), "pc 4 has a sub-block entry");
    // An IR block start is still served by the block table, and gains no sub-block.
    assert!(function.entries[1].is_some(), "pc 1 is the `Label(2)` block start");
    assert!(function.sub_entries[1].is_none(), "a block start needs no sub-block");
    // A sub-block is not a block: the summary counts the two IR blocks only.
    assert_eq!(function.summary.block_count, 2, "only the IR blocks are counted");

    // The lazy layout (`run_baseline_cached*`'s artifact) serves the continuation address
    // from its sub-block table, after the block table.
    let compiled = CompiledModule::lazy(&module);
    let (sub, _) = compiled.native_entry(&module, 0, 4).expect("pc 4 is a sub-block");
    assert_eq!(
        compiled.entry(0, 4),
        Some(sub),
        "the continuation address is natively enterable"
    );
    assert_eq!(compiled.block_count(), 2, "sub-blocks add no block");

    // The eager layout lowers the same continuation sub-blocks, so the address is
    // natively enterable there too and is still not counted as a block.
    let eager = compile_module(&module).expect("the module compiles eagerly");
    assert!(
        eager.entry(0, 4).is_some(),
        "the eager layout has a sub-block at the continuation address"
    );
    assert_eq!(eager.block_count(), 3, "sub-blocks add no block");
}

// ---------------------------------------------------------------------------
// (d) the refusal
// ---------------------------------------------------------------------------

/// A `call` whose target label is **not** in the module's label map cannot be transferred
/// to — so the arm must keep the documented deopt and let `jit_step_one` run the
/// interpreter's own `call` arm, which reports its own typed error (`unknown_label` is a
/// fieldless `BadArg`). Inventing a target is forbidden; a wrong result is forbidden.
///
/// The evidence is threefold: the **emitter** refuses the call (a static `instr_deopts`
/// entry, measured on a cache-free compile), the **run** attributes the interpreter step
/// to `Call` (`deopts_by_variant`), and both **tiers agree** on the typed error — which is
/// exactly the interpreter's own `unknown_label` (`BadArg`).
#[test]
fn a_call_to_an_undefined_label_still_deopts_to_the_interpreter() {
    let (module, name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Call {
            arity: 0,
            target: Label(9),
        },
        Instr::Return,
    ]);

    // (1) The emitter must refuse: `Label(9)` is nowhere in the module, so there is no
    // target to link — and none may be invented. Measured on a compile that cannot hit
    // the thread-local `CodeCache` (which would emit nothing).
    super::counters::reset();
    super::counters::set_enabled(true);
    let labels = crate::interpreter::build_label_map(&module);
    let _function = compile_function(&module, &labels, 0).expect("f/0 compiles");
    super::counters::set_enabled(false);
    let emitted = super::counters::snapshot();
    assert!(
        emitted.instr_deopts.get("Call").copied().unwrap_or(0) > 0,
        "an unresolvable target must fall back to the documented deopt; deopts: {:?}",
        emitted.instr_deopts,
    );

    // (2) The run: the block leaves for the driver with the interpreter's own error, and
    // the step is attributed to `Call`.
    let (outcome, snapshot) = run_counting(&module, name, Tier::BaselineJit);
    assert!(
        outcome.is_err(),
        "the unresolvable `call` must not return a value: {outcome:?}"
    );
    assert!(
        snapshot.deopts_by_variant.contains_key("Call"),
        "the fallback is the interpreter's own step, so it must be attributed to `Call`; \
         deopts by variant: {:?}",
        snapshot.deopts_by_variant,
    );

    // (3) Both tiers report the interpreter's own typed error.
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(
            "undefined call target",
            interpreter,
            baseline,
            &module.atom_table,
        );
        // `unknown_label` is `BeamError::BadArg` (a fieldless variant), so its rendering
        // is the bare `BadArg`; pin it so a future change to the refusal is visible here.
        assert_eq!(
            error_reason(interpreter.as_ref().expect_err("the call must fail")),
            error_reason(&beam_common::BeamError::BadArg),
            "an unresolvable `call` target must raise the interpreter's own \
             `unknown_label` (`BadArg`) error"
        );
        assert_eq!(
            error_reason(baseline.as_ref().expect_err("the call must fail")),
            error_reason(interpreter.as_ref().expect_err("the call must fail")),
        );
    });
}

// ---------------------------------------------------------------------------
// (e) no register-allocator participation
// ---------------------------------------------------------------------------

/// A `call` contributes **nothing** to the block's register cache: it has no x-register
/// operand at all — its operands are a label (resolved at compile time) and an arity
/// (handed to the callee's register-file convention, never an addressable register) —
/// so it must not appear in the block's cacheable touches, nor claim a resident slot, nor
/// enter the spill set.
///
/// This is a **structural** pin on the allocator's decision, complementing the tier
/// equivalence above: a register charged here would both evict a real touch from the
/// five-slot cache and raise the x-register bound the driver's native-entry guard reads
/// (`super::emit::regalloc::cacheable_touches` / `cacheable_writes`, whose `_` arms own
/// this instruction).
#[test]
fn a_local_call_contributes_no_register_touch_or_write() {
    let (module, _) = local_call_case();
    let labels = crate::interpreter::build_label_map(&module);
    let code = &module.functions[0].code;

    let call = &code[3];
    assert!(
        matches!(call, Instr::Call { .. }),
        "pc 3 of `f/0` is the `call`; found {call:?}"
    );
    assert!(
        cacheable_touches(&module, &labels, call).is_empty(),
        "a `call` names no x-register, so it must contribute no native touch"
    );

    // The whole block is `allocate 0 0; call f/0 -> g/0; deallocate_return 0` — none of
    // which is cacheable — so the allocator must select nothing at all.
    let block = &module.functions[0].blocks[1];
    assert_eq!((block.start, block.end), (1, 5), "the `Label(2)` block");
    let allocation = allocate_registers(&module, code, block.start, block.end);
    assert!(
        allocation.slots.is_empty(),
        "no register is touched natively, so none may be cached: {:?}",
        allocation.slots,
    );
    assert!(
        allocation.written.is_empty(),
        "a `call` writes no x-register, so the spill set must be empty: {:?}",
        allocation.written,
    );
    assert_eq!(
        allocation.live_candidates, 0,
        "no register has a live interval in this block"
    );
}

// ---------------------------------------------------------------------------
// (f) the eager sub-block's allocation describes its own range
// ---------------------------------------------------------------------------

/// `f(A, B) -> g(B)` with `g/1` returning the constant `1000`: the call's result is
/// carried across the continuation by two inlined `move`s and returned.
///
/// The shape matters: the frame-preserving `call` sits **strictly inside** the `Label(2)`
/// IR block (the loader cuts blocks only at labels), so the continuation address `pc + 1`
/// gets its own native **sub-block**, and `x(0)` — written by the argument `move` before
/// the call, read by the moves after it — is exactly the register whose cache state the
/// containing block's allocation and the sub-range's own disagree about.
///
/// The trailing reader is deliberately **unguarded**: `move x(0) -> x(1)`,
/// `move x(1) -> x(0)` are plain inlined register moves, so whatever the sub-block's
/// cached slot for `x(0)` holds *is* the returned value. A small-int `gc_bif` such as
/// `'+'/2` would **mask** the defect instead: its run-time small-int guard rejects a stale
/// machine-register word, deopts to `jit_step_one`, and the interpreter step then reads the
/// correct `machine.x` — the right answer by accident.
///
/// The callee is deliberately **cache-free**: its one value-producing instruction is a
/// `bif` the emitter does not lower (`erlang:abs/1`), so its block caches no x-register at
/// all and its `return` leaves `1000` in `machine.x` *only*. Nothing therefore refreshes
/// the physical register the caller's block cached `x(0)` in (a block's teardown restores
/// the callee-saved registers it saved, so a returned-to driver holds the *driver's*
/// leftovers there), which is what makes the sub-block's missing entry load observable as a
/// value rather than a coincidence of the callee writing the same physical register.
///
/// ```text
/// f/2                       g/1
///  0 label(1)                0 label(3)
///  1 label(2)                1 bif abs/1 [1000] -> x(0)   (helper, no cache slot)
///  2 allocate 0 0            2 return
///  3 move x(1) -> x(0)     <- x(0)'s first touch in the block is this WRITE
///  4 call g/1 -> label(3)  <- continuation = pc 5, inside the block
///  5 move x(0) -> x(1)     <- the sub-block's first instruction READS x(0)
///  6 move x(1) -> x(0)
///  7 deallocate_return 0   <- spills x(0)
/// ```
fn call_result_passthrough_case() -> Module {
    build_module(
        "eager_sub_block",
        &[("erlang", "abs", 1)],
        vec![
            (
                "f",
                2,
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::Allocate {
                        stack_need: beam_loader::HeapNeed::Words(0),
                        live: 0,
                    },
                    Instr::Move {
                        src: Operand::X(1),
                        dst: Operand::X(0),
                    },
                    Instr::Call {
                        arity: 1,
                        target: Label(3),
                    },
                    Instr::Move {
                        src: Operand::X(0),
                        dst: Operand::X(1),
                    },
                    Instr::Move {
                        src: Operand::X(1),
                        dst: Operand::X(0),
                    },
                    Instr::DeallocateReturn(0),
                ],
            ),
            (
                "g",
                1,
                vec![
                    Instr::Label(Label(3)),
                    Instr::Bif {
                        arity: 1,
                        bif: ImportRef(0),
                        fail: NO_FAIL,
                        args: vec![Operand::Integer(1000)],
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 2, Label(2))],
    )
}

/// A context serving the callee's helper BIF `erlang:abs/1`; nothing else is called.
fn eager_sub_block_context() -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(
        |module: &str, function: &str, arity: u32, args: &[Eterm]| {
            if module == "erlang" && function == "abs" && arity == 1 && args.len() == 1 {
                return Ok(make_small(beam_term::signed_value(args[0]).abs()));
            }
            Err(crate::error::unsupported(&format!(
                "eager sub-block bif {module}:{function}/{arity}"
            )))
        },
    ))
}

/// **The eager continuation sub-block is allocated over its own range.**
///
/// A continuation address the driver re-enters is a **fresh native function**: the caller's
/// block has already run its frame teardown, so every cached machine register holds whatever
/// the *driver* left there — not the value the call left in `machine.x`. Its [`Allocation`]
/// must therefore be entry-point-relative to the address it starts at, exactly like the lazy
/// layout's: `entry_fill` must load `x(0)` (the call's result, which the sub-range reads
/// before it writes) and `spill_sets` must not carry the containing block's dirty bits.
///
/// With the containing block's whole-range allocation instead, the sub-block's cached slot
/// for `x(0)` is never loaded from `machine.x`, so the stale machine-register word is copied
/// into `x(0)` and spilled back into `machine.x` by the exit.
#[test]
fn the_eager_sub_block_reads_the_call_result_out_of_machine_x() {
    let module = call_result_passthrough_case();
    let name = export_atom(&module, "f");
    let args = [make_small(10), make_small(20)];

    // The premise, on the two allocations themselves: `x(0)` is cached either way, but only
    // the sub-range's own allocation has it in the live-in set.
    let code = &module.functions[0].code;
    assert!(matches!(code[4], Instr::Call { .. }), "pc 4 is the call");
    let block = &module.functions[0].blocks[1];
    assert_eq!((block.start, block.end), (1, 8), "the `Label(2)` IR block");
    let containing = allocate_registers(&module, code, block.start, block.end);
    let sub_range = allocate_registers(&module, code, 5, code.len());
    assert!(
        containing.slot(0).is_some() && sub_range.slot(0).is_some(),
        "x(0) is touched natively in both ranges, so both cache it"
    );
    assert!(
        !containing.entry_loads(0),
        "the containing block writes x(0) before it first reads it, so its live-in set \
         leaves it unloaded"
    );
    assert!(
        sub_range.entry_loads(0),
        "the sub-range's first instruction reads x(0), so its prologue must load it"
    );

    // (1) The interpreter: `g(20) = 1000` and the two moves pass the value through.
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = eager_sub_block_context();
    let interpreted = interpreter
        .run_export_outcome_with_tier(
            &module,
            name,
            2,
            &args,
            &mut interpreter_ctx,
            Tier::Interpreter,
        )
        .expect("the interpreter run completes");
    assert_eq!(
        interpreted,
        RunOutcome::Returned(make_small(1000)),
        "the interpreter's answer is the hand-computed one"
    );

    // (2) The eager artifact, through the compile-once / run-many entry (the layout
    // `jit::bench` and `CodeCache::get_or_compile` use — the tier entries go through the
    // **lazy** artifact, so they can never reach this layout).
    let mut backend = BaselineJit::new();
    let compiled = backend.compile(&module).expect("the module compiles eagerly");
    assert!(
        compiled.entry(0, 5).is_some(),
        "the continuation at pc 5 must have a native sub-block entry, or this pin proves \
         nothing"
    );
    let mut machine = Machine::new();
    let mut ctx = eager_sub_block_context();
    let eager = backend
        .run_compiled(&compiled, &mut machine, &module, name, 2, &args, &mut ctx)
        .expect("the eager run completes");
    assert_eq!(
        eager,
        make_small(1000),
        "the eager sub-block must read the callee's x(0) from `machine.x`, not the stale \
         word the machine register holds on entry"
    );
}

/// The block-ABI sentinel the `call` arm consumes in-block is **disjoint** from every
/// other word a block can return: it is negative (so it can never be a `(func, pc)`
/// target word) and distinct from the four sentinels the driver decodes.
///
/// The whole probe-then-transfer shape depends on that: `CALL_STORED` means "the
/// continuation is in place, transfer now", every other word means "leave the block"
/// (a served callee's folded `Step`, or a typed diagnostic), and the driver must never
/// see it.
#[test]
fn the_call_store_sentinel_is_disjoint_from_every_other_block_word() {
    assert_ne!(CALL_STORED, HALT, "the store sentinel is not the halt word");
    assert_ne!(CALL_STORED, ERROR, "the store sentinel is not the error word");
    assert_ne!(CALL_STORED, ENTER, "the store sentinel is not the enter word");
    assert_ne!(
        CALL_STORED,
        LOCAL_DECLINE,
        "the store sentinel is not the local-call decline word"
    );
    assert!(
        CALL_STORED < 0,
        "the store sentinel must be negative, so it can never collide with an encoded \
         `(func, pc)` target word"
    );
    assert_eq!(
        decode_target(CALL_STORED),
        None,
        "the driver must never decode the store sentinel as a target"
    );
}

// ---------------------------------------------------------------------------
// (g) the in-block non-tail `call` transfer (`runtime::enter_in_block`)
// ---------------------------------------------------------------------------
//
// `jit_call_store_linked` stores the continuation, probes the host's local-call
// seam, and — only on the *store* path — resolves the callee's native entry so the
// block tail-jumps into it instead of handing the driver a `(func, pc)` word. The
// transfer can only fire on the **lazy** per-function artifact (`CodeCache::
// compiled_handle`, the layout `cache::run_baseline_cached*` stages), because that
// is the only layout in which a *cross-function* callee is absent from the
// compiling function's own block map (`Linker::is_compiled` false). An **eager**
// artifact links the same call directly through `Linker::emit_exit` and therefore
// never asks for the linking twin at all.
//
// The five tests below pin, respectively: the transfer is taken (and is net
// cheaper than the round-trip it replaced); deep cross-function non-tail recursion
// stays flat because the emitted arm tears its native frame down before the `jmp`;
// a refused pre-emption charge parks at the *callee entry* and the resumed slice
// replays nothing; a **served** local callee is never transferred to; and every
// decline leaves the pre-existing driver/ interpreter-step behaviour untouched.

/// Run exported `name/arity` with `args` on `tier` with the structural counters
/// recording — the argument-taking sibling of `super::run_counting` (which is
/// hard-coded to arity 0 and to a fresh [`MinimalContext`]).
fn run_counting_args(
    module: &Module,
    name: Atom,
    arity: u32,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
    tier: Tier,
) -> (Result<RunOutcome>, super::counters::Counters) {
    super::counters::reset();
    super::counters::set_enabled(true);
    let mut machine = Machine::new();
    let outcome = machine.run_export_outcome_with_tier(module, name, arity, args, ctx, tier);
    super::counters::set_enabled(false);
    (outcome, super::counters::snapshot())
}

/// `f/1` calls `g/1` **twice**, each time non-tail, and uses both results:
///
/// ```text
/// f/1                        g/1
///  0 label(1)                 0 label(3)
///  1 label(2)                 1 bif host:inc/1 [x(0)] -> x(0)
///  2 allocate 0 0             2 return
///  3 call g/1 -> label(3)   <- continuation = pc 4, inside the block
///  4 move x(0) -> x(1)
///  5 call g/1 -> label(3)   <- continuation = pc 6, inside the block
///  6 move x(1) -> x(0)
///  7 deallocate_return 0
/// ```
///
/// `f(10)`: `g(10) = 11` (kept in `x(1)`), `g(11) = 12`, then `x(0) <- x(1) = 11`.
fn two_call_module() -> Module {
    build_module(
        "in_block_two_calls",
        &[("host", "inc", 1)],
        vec![
            (
                "f",
                1,
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::Allocate {
                        stack_need: beam_loader::HeapNeed::Words(0),
                        live: 1,
                    },
                    Instr::Call {
                        arity: 1,
                        target: Label(3),
                    },
                    Instr::Move {
                        src: Operand::X(0),
                        dst: Operand::X(1),
                    },
                    Instr::Call {
                        arity: 1,
                        target: Label(3),
                    },
                    Instr::Move {
                        src: Operand::X(1),
                        dst: Operand::X(0),
                    },
                    Instr::DeallocateReturn(0),
                ],
            ),
            (
                "g",
                1,
                vec![
                    Instr::Label(Label(3)),
                    Instr::Bif {
                        arity: 1,
                        bif: ImportRef(0),
                        fail: NO_FAIL,
                        args: vec![Operand::X(0)],
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 1, Label(2))],
    )
}

/// A context serving `host:inc/1` (the callee's only value-producing instruction),
/// counting how many times it ran — the callee's own `enter` counter.
fn inc_context(calls: std::rc::Rc<std::cell::Cell<u32>>) -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(
        move |module: &str, function: &str, arity: u32, args: &[Eterm]| {
            if module == "host"
                && function == "inc"
                && arity == 1
                && args.len() == 1
                && beam_term::is_small(args[0])
            {
                calls.set(calls.get() + 1);
                return Ok(make_small(beam_term::signed_value(args[0]) + 1));
            }
            Err(crate::error::unsupported(&format!(
                "in-block call bif {module}:{function}/{arity}"
            )))
        },
    ))
}

/// **A lazy cross-function non-tail `call` is transferred in-block.**
///
/// Both calls of `two_call_module` resolve `g/1`'s entry in the helper and tail-jump
/// into it, so neither costs a driver round-trip: the only word the driver decodes is
/// the final `HALT` of the outermost `deallocate_return`. The evidence:
///
/// * the value is the interpreter's **and** the hand-computed one (`11`), so the
///   transferred calls really executed their callee (the `inc` counter says twice);
/// * `in_block_calls` counts the transfers and `in_block_call_declines` is zero; and
/// * `driver_round_trips` is strictly smaller than the number of executed `call`
///   instructions — the property the transfer exists for.
#[test]
fn a_lazy_cross_function_call_is_transferred_in_block() {
    let module = two_call_module();
    let name = export_atom(&module, "f");
    let args = [make_small(10)];

    // (1) The interpreter's answer, plus the hand-computed one.
    let interpreted_calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = inc_context(std::rc::Rc::clone(&interpreted_calls));
    let interpreted = interpreter
        .run_export_outcome_with_tier(
            &module,
            name,
            1,
            &args,
            &mut interpreter_ctx,
            Tier::Interpreter,
        )
        .expect("the interpreter run completes");
    assert_eq!(
        interpreted,
        RunOutcome::Returned(make_small(11)),
        "g(10) = 11 is kept in x(1), g(11) = 12 is discarded, so x(0) is 11"
    );
    assert_eq!(
        interpreted_calls.get(),
        2,
        "both non-tail calls ran their callee"
    );

    // (2) The baseline tier, through the LAZY per-function artifact — the only layout
    // where a cross-function callee is absent from the compiling function's block map.
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut ctx = inc_context(std::rc::Rc::clone(&calls));
    let (outcome, snapshot) =
        run_counting_args(&module, name, 1, &args, &mut ctx, Tier::BaselineJit);
    assert_eq!(
        outcome.expect("the baseline run completes"),
        RunOutcome::Returned(make_small(11)),
        "the transferred calls must still return the callee's value"
    );
    assert_eq!(calls.get(), 2, "the callee still ran exactly twice");
    assert!(
        snapshot.in_block_calls > 0,
        "the lazy cross-function `call` must transfer in-block; counters: {:?}",
        snapshot.in_block_calls,
    );
    assert_eq!(
        snapshot.in_block_call_declines, 0,
        "no transfer of this fixture may decline; counters: {:?}",
        snapshot.in_block_call_declines,
    );
    assert!(
        snapshot.driver_round_trips < 2,
        "two executed `call`s must cost strictly fewer than two driver round-trips; \
         saw {} round trip(s) ({:?})",
        snapshot.driver_round_trips,
        snapshot.round_trips_by_word,
    );
}

/// `a/1` and `b/1` mutually recursive, each with a **non-tail** `call` into the other:
///
/// ```text
/// a(N) -> case N of 0 -> 0; _ -> 1 + b(N - 1) end
/// b(N) -> case N of 0 -> 0; _ -> 1 + a(N - 1) end
/// ```
///
/// `1 + b(N-1)` is a frame-preserving call, so every level needs a continuation: the
/// callee returns into the caller's continuation **sub-block**, which then adds `1`
/// and returns in turn. Both levels' transfers are tail-jumps.
fn mutual_non_tail_module() -> Module {
    let branch = |fail: Label| {
        vec![
            Instr::Allocate {
                stack_need: beam_loader::HeapNeed::Words(0),
                live: 1,
            },
            // `is_ne_exact x(0), 0`: the test SUCCEEDS when `N /= 0` (fall through to the
            // recursive arm) and FAILS when `N == 0` (branch to the base arm at `fail`).
            Instr::Test {
                kind: TestKind::IsNeExact,
                fail,
                args: vec![Operand::X(0), Operand::Integer(0)],
            },
            // x(0) = N - 1
            Instr::GcBif {
                arity: 2,
                bif: ImportRef(0),
                fail: NO_FAIL,
                live: 1,
                args: vec![Operand::X(0), Operand::Integer(1)],
                dst: Operand::X(0),
            },
        ]
    };
    build_module(
        "mutual_non_tail",
        &[("erlang", "-", 2), ("erlang", "+", 2)],
        vec![
            (
                "a",
                1,
                {
                    let mut code = vec![Instr::Label(Label(1)), Instr::Label(Label(2))];
                    code.extend(branch(Label(5)));
                    code.extend(vec![
                        Instr::Call {
                            arity: 1,
                            target: Label(3),
                        },
                        // x(0) = x(0) + 1
                        Instr::GcBif {
                            arity: 2,
                            bif: ImportRef(1),
                            fail: NO_FAIL,
                            live: 1,
                            args: vec![Operand::X(0), Operand::Integer(1)],
                            dst: Operand::X(0),
                        },
                        Instr::DeallocateReturn(0),
                        Instr::Label(Label(5)),
                        Instr::Move {
                            src: Operand::Integer(0),
                            dst: Operand::X(0),
                        },
                        Instr::DeallocateReturn(0),
                    ]);
                    code
                },
            ),
            (
                "b",
                1,
                {
                    let mut code = vec![Instr::Label(Label(3))];
                    code.extend(branch(Label(6)));
                    code.extend(vec![
                        Instr::Call {
                            arity: 1,
                            target: Label(2),
                        },
                        Instr::GcBif {
                            arity: 2,
                            bif: ImportRef(1),
                            fail: NO_FAIL,
                            live: 1,
                            args: vec![Operand::X(0), Operand::Integer(1)],
                            dst: Operand::X(0),
                        },
                        Instr::DeallocateReturn(0),
                        Instr::Label(Label(6)),
                        Instr::Move {
                            src: Operand::Integer(0),
                            dst: Operand::X(0),
                        },
                        Instr::DeallocateReturn(0),
                    ]);
                    code
                },
            ),
        ],
        &[("a", 1, Label(2))],
    )
}

/// A context serving `erlang:'-'/2` and `erlang:'+'/2` (the emitter inlines the small-int
/// shapes, so this is only reached for a rounded-up operand; it keeps the fixture
/// independent of that choice).
fn arith_context() -> MinimalContext {
    MinimalContext::with_bif_hook(Box::new(
        |module: &str, function: &str, arity: u32, args: &[Eterm]| {
            if module != "erlang" || arity != 2 || args.len() != 2 {
                return Err(crate::error::unsupported(&format!(
                    "arith bif {module}:{function}/{arity}"
                )));
            }
            if !beam_term::is_small(args[0]) || !beam_term::is_small(args[1]) {
                return Err(beam_error_bad_arith());
            }
            let (a, b) = (beam_term::signed_value(args[0]), beam_term::signed_value(args[1]));
            match function {
                "+" => Ok(make_small(a + b)),
                "-" => Ok(make_small(a - b)),
                _ => Err(crate::error::unsupported(&format!("arith bif {function}/2"))),
            }
        },
    ))
}

/// `error:badarith`, spelled through the public constructor the engine itself uses.
fn beam_error_bad_arith() -> beam_common::BeamError {
    beam_common::BeamError::BadArith
}

/// **Deep cross-function non-tail recursion does not grow the native stack.**
///
/// Every level of `mutual_non_tail_module` is entered by an in-block transfer, and the
/// emitted arm (`emit::link::emit_in_block_call`) restores `rsp` and the callee-saved
/// registers it pushed — [`emit_frame_teardown`] — **before** the `jmp r11`. Without
/// that teardown each level would leave its native frame behind on the `jmp`, so a
/// 20 000-deep recursion would consume 20 000 native frames and overflow the thread
/// stack. The property is observable as the value: both tiers must unwind the whole
/// chain to `20 000`.
#[test]
fn deep_cross_function_non_tail_recursion_does_not_grow_the_native_stack() {
    const DEPTH: i64 = 20_000;
    let module = mutual_non_tail_module();
    let name = export_atom(&module, "a");
    let args = [make_small(DEPTH)];

    let mut interpreter = Machine::new();
    let mut interpreter_ctx = arith_context();
    let interpreted = interpreter
        .run_export_outcome_with_tier(
            &module,
            name,
            1,
            &args,
            &mut interpreter_ctx,
            Tier::Interpreter,
        )
        .expect("the interpreter unwinds the whole chain");
    assert_eq!(
        interpreted,
        RunOutcome::Returned(make_small(DEPTH)),
        "a(N) = N for every N >= 0"
    );

    let mut baseline = Machine::new();
    let mut baseline_ctx = arith_context();
    let native = baseline
        .run_export_outcome_with_tier(
            &module,
            name,
            1,
            &args,
            &mut baseline_ctx,
            Tier::BaselineJit,
        )
        .expect("the baseline tier unwinds the whole chain");
    assert_eq!(
        native,
        RunOutcome::Returned(make_small(DEPTH)),
        "the native recursion must return the interpreter's value"
    );
}

/// `f/0` ticks, calls `g/0` **non-tail**, and returns; `g/0` ticks and returns its own
/// value. The two ticks are the per-function entry counters a replayed block prefix would
/// double.
fn charge_parking_module() -> Module {
    build_module(
        "in_block_charge",
        &[("host", "f_tick", 0), ("host", "g_tick", 0)],
        vec![
            (
                "f",
                0,
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::Allocate {
                        stack_need: beam_loader::HeapNeed::Words(0),
                        live: 0,
                    },
                    Instr::Bif {
                        arity: 0,
                        bif: ImportRef(0),
                        fail: NO_FAIL,
                        args: vec![],
                        dst: Operand::X(1),
                    },
                    Instr::Call {
                        arity: 0,
                        target: Label(3),
                    },
                    Instr::DeallocateReturn(0),
                ],
            ),
            (
                "g",
                0,
                vec![
                    Instr::Label(Label(3)),
                    Instr::Bif {
                        arity: 0,
                        bif: ImportRef(1),
                        fail: NO_FAIL,
                        args: vec![],
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 0, Label(2))],
    )
}

/// The host's own pre-emption diagnostic — the exact error the budget wrapper refuses with.
const PREEMPTION: &str = "host: reduction budget exhausted";

/// A [`MinimalContext`] behind a **pre-empting** host: `reduce` records every charge and,
/// while a `budget` is set, refuses the first charge past it with its **own** typed error;
/// the two `host:*_tick/0` BIFs count each function's entries.
struct BudgetCtx {
    inner: MinimalContext,
    budget: Option<u32>,
    charges: u32,
    f_ticks: u32,
    g_ticks: u32,
}

impl BudgetCtx {
    fn new(budget: Option<u32>) -> BudgetCtx {
        BudgetCtx {
            inner: MinimalContext::new(),
            budget,
            charges: 0,
            f_ticks: 0,
            g_ticks: 0,
        }
    }
}

impl ExecutionContext for BudgetCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if module == "host" && arity == 0 && args.is_empty() {
            if function == "f_tick" {
                self.f_ticks += 1;
                return Ok(make_small(1));
            }
            if function == "g_tick" {
                self.g_ticks += 1;
                return Ok(make_small(42));
            }
        }
        self.inner.call_bif(module, function, arity, args)
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

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
    }

    fn reduce(&mut self, _n: u32) -> Result<()> {
        self.charges += 1;
        if let Some(budget) = self.budget {
            if self.charges > budget {
                return Err(crate::error::unsupported(PREEMPTION));
            }
        }
        Ok(())
    }

    fn wants_reduce(&self) -> bool {
        true
    }
}

/// **A refused pre-emption charge parks at the callee entry and the resume replays
/// nothing.**
///
/// The in-block transfer performs the driver iteration it replaces — publish the target
/// position, bump the slice counter, offer `ExecutionContext::reduce(1)` — so a refusal is
/// a clean pre-emption at the **callee's** entry (nothing of the callee's block has run),
/// not a replayed prefix of the caller's. The pins:
///
/// (a) the run's error is the host's own typed error, unchanged;
/// (b) `Machine::current_code` names the callee's `(function, entry pc)`;
/// (c) after lifting the budget the resumed slice returns the unrefused run's value; and
/// (d) each function's tick ran exactly **once** across both slices — no replayed `call`,
///     no duplicated side effect.
#[test]
fn a_refused_preemption_charge_parks_at_the_callee_entry_and_resumes_exactly_once() {
    let module = charge_parking_module();
    let name = export_atom(&module, "f");
    let labels = crate::interpreter::build_label_map(&module);
    let (callee, callee_pc) = labels.resolve(Label(3)).expect("label 3 resolves");
    assert_eq!(
        (callee, callee_pc),
        (1, 0),
        "g/0 is the module's second function and `Label(3)` its entry"
    );

    // One accepted charge is the driver's own block entry for `f`; the second is the
    // in-block transfer's, and it is refused.
    let mut ctx = BudgetCtx::new(Some(1));
    let mut machine = Machine::new();
    let outcome = machine.run_export_outcome_with_tier(
        &module,
        name,
        0,
        &[],
        &mut ctx,
        Tier::BaselineJit,
    );

    // (a) The host's error, unchanged.
    let err = outcome.expect_err("the transfer's charge ends the slice");
    assert_eq!(
        error_reason(&err),
        error_reason(&crate::error::unsupported(PREEMPTION)),
        "the driver must report the host's own typed error verbatim"
    );
    assert_eq!(
        ctx.charges, 2,
        "the driver's block-entry charge was accepted and the in-block transfer's refused"
    );

    // (b) The parked position is the CALLEE's entry.
    assert_eq!(
        machine.current_code(),
        Some((callee, callee_pc)),
        "a refused in-block charge parks on the callee's entry, not the caller's block"
    );

    // (c) Lift the budget and resume exactly there.
    ctx.budget = None;
    let resumed = machine
        .resume_outcome_with_tier(&module, callee, callee_pc, &mut ctx, Tier::BaselineJit)
        .expect("the resumed slice completes");
    assert_eq!(
        resumed,
        RunOutcome::Returned(make_small(42)),
        "the resumed slice must return the callee's value"
    );

    // (d) Each function's entry ran exactly once across the two slices.
    assert_eq!(
        (ctx.f_ticks, ctx.g_ticks),
        (1, 1),
        "no replayed `call` and no duplicated side effect: each tick ran once"
    );

    // The unrefused control agrees on both the value and the entry counts.
    let mut control = BudgetCtx::new(None);
    let mut control_machine = Machine::new();
    let unrefused = control_machine
        .run_export_outcome_with_tier(
            &module,
            name,
            0,
            &[],
            &mut control,
            Tier::BaselineJit,
        )
        .expect("the unrefused run completes");
    assert_eq!(
        unrefused,
        RunOutcome::Returned(make_small(42)),
        "the pre-empted slice must reach the same value as the unrefused run"
    );
    assert_eq!(
        (control.f_ticks, control.g_ticks),
        (1, 1),
        "the unrefused run ticks each function exactly once, matching the resumed slice"
    );
}

/// `f/0` calls `g/0` non-tail; `g`'s own body must never run (it would return `99`).
fn served_local_call_module() -> Module {
    build_module(
        "served_local_call",
        &[],
        vec![
            (
                "f",
                0,
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::Allocate {
                        stack_need: beam_loader::HeapNeed::Words(0),
                        live: 0,
                    },
                    Instr::Call {
                        arity: 0,
                        target: Label(3),
                    },
                    Instr::DeallocateReturn(0),
                ],
            ),
            (
                "g",
                0,
                vec![
                    Instr::Label(Label(3)),
                    Instr::Move {
                        src: Operand::Integer(99),
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 0, Label(2))],
    )
}

/// A context whose `resolve_local_call` **serves** `g/0` as a BIF returning `42`, so the
/// callee's code must never run and the call must never be transferred to.
fn served_call_context() -> MinimalContext {
    let mut ctx = MinimalContext::with_bif_hook(Box::new(
        |module: &str, function: &str, arity: u32, _args: &[Eterm]| {
            if module == "served_local_call" && function == "g" && arity == 0 {
                return Ok(make_small(42));
            }
            Err(crate::error::unsupported(&format!(
                "served bif {module}:{function}/{arity}"
            )))
        },
    ));
    ctx.set_local_call_resolver(Box::new(
        |module: &str, function: &str, arity: u32| {
            Ok(module == "served_local_call" && function == "g" && arity == 0)
        },
    ));
    ctx
}

/// **A served local callee is never transferred to.**
///
/// The helper runs the continuation store **and** the host's local-call seam before it
/// resolves any entry, and a served callee returns a folded step instead of `CALL_STORED`
/// — so the block leaves for the driver and `g`'s own code never runs (its `x(0) = 99` is
/// never written; the value is the BIF's `42`). `in_block_calls` must therefore stay zero,
/// and the interpreter run of the same served fixture must agree.
#[test]
fn a_served_local_call_is_never_transferred() {
    let module = served_local_call_module();
    let name = export_atom(&module, "f");

    let mut interpreter = Machine::new();
    let mut interpreter_ctx = served_call_context();
    let interpreted = interpreter
        .run_export_outcome_with_tier(
            &module,
            name,
            0,
            &[],
            &mut interpreter_ctx,
            Tier::Interpreter,
        )
        .expect("the served interpreter run completes");
    assert_eq!(
        interpreted,
        RunOutcome::Returned(make_small(42)),
        "a served local callee delivers the BIF's value, not the callee's body"
    );

    let mut ctx = served_call_context();
    let (outcome, snapshot) = run_counting_args(&module, name, 0, &[], &mut ctx, Tier::BaselineJit);
    assert_eq!(
        outcome.expect("the served baseline run completes"),
        RunOutcome::Returned(make_small(42)),
        "the native tier must deliver the same served value"
    );
    assert_eq!(
        snapshot.in_block_calls, 0,
        "a served callee must never be transferred to (the continuation was not stored)"
    );
    assert_eq!(
        snapshot.in_block_call_declines, 0,
        "a served callee is neither a transfer nor a decline — the seam answered first"
    );
}

/// The `local_call_case` caller, but with a callee the artifact lowers **no block** for.
///
/// `Instr::Generic` does **not** make a function uncompilable: the emitter's `_` arm sends
/// it to `emit_deopt`, so the function compiles and its blocks are entered normally (the
/// `Generic` instruction then deopts per-instruction at run time). The reachable form of
/// the "address no block covers" decline is therefore a callee whose IR carries no
/// **block**: `compile_function` still produces an artifact, but its entry tables hold
/// nothing, so `native_entry` answers `None` for the address the label map names and the
/// transfer declines exactly as a callee the tier cannot enter would.
fn declined_callee_module() -> Module {
    let mut module = build_module(
        "declined_callee",
        &[],
        vec![
            (
                "f",
                0,
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::Allocate {
                        stack_need: beam_loader::HeapNeed::Words(0),
                        live: 0,
                    },
                    Instr::Call {
                        arity: 0,
                        target: Label(3),
                    },
                    Instr::DeallocateReturn(0),
                ],
            ),
            (
                "g",
                0,
                vec![
                    Instr::Label(Label(3)),
                    Instr::Move {
                        src: Operand::Integer(7),
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 0, Label(2))],
    );
    // The IR block decomposition is what `compile_function` iterates: with none, the
    // callee lowers to an artifact that has no entry for any address.
    module.functions[1].blocks.clear();
    module
}

/// **A callee the artifact cannot enter declines to the driver.**
///
/// The decline is the pre-existing behaviour and must be untouched: the helper reports the
/// resolved `(func, pc)` word, the block's emitted arm takes its `declined` path, and the
/// driver serves the address with exactly one interpreter step — so the value is
/// unchanged and the counters attribute the `call` to `in_block_call_declines`, never to
/// `in_block_calls`.
#[test]
fn an_uncompilable_callee_declines_to_the_driver() {
    let module = declined_callee_module();
    let name = export_atom(&module, "f");
    let labels = crate::interpreter::build_label_map(&module);
    let (callee, callee_pc) = labels.resolve(Label(3)).expect("label 3 resolves");

    // The premise, in two halves: the callee does produce an artifact, and that artifact
    // has no native entry for the address the label map names.
    assert!(
        compile_function(&module, &labels, callee).is_ok(),
        "the callee still lowers to an artifact (it is not a compile failure)"
    );
    let compiled = CompiledModule::lazy(&module);
    assert!(
        compiled.native_entry(&module, callee, callee_pc).is_none(),
        "the premise: no native block covers the callee entry, so the transfer must decline"
    );

    // The interpreter's answer, for the unchanged-result assertion.
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    let interpreted = interpreter
        .run_export_outcome_with_tier(
            &module,
            name,
            0,
            &[],
            &mut interpreter_ctx,
            Tier::Interpreter,
        )
        .expect("the interpreter run completes");
    assert_eq!(interpreted, RunOutcome::Returned(make_small(7)));

    let mut ctx = MinimalContext::new();
    let (outcome, snapshot) = run_counting_args(&module, name, 0, &[], &mut ctx, Tier::BaselineJit);
    assert_eq!(
        outcome.expect("the baseline run completes"),
        RunOutcome::Returned(make_small(7)),
        "a declined transfer must leave the result exactly as before the transfer existed"
    );
    assert!(
        snapshot.in_block_call_declines > 0,
        "the declined transfer must be counted: {:?}",
        snapshot.in_block_call_declines,
    );
    assert_eq!(
        snapshot.in_block_calls, 0,
        "a declined transfer is never an in-block call: {:?}",
        snapshot.in_block_calls,
    );
    assert!(
        snapshot.driver_interp_steps > 0,
        "the driver's own interpreter step serves the declined callee: {:?}",
        snapshot.driver_interp_steps,
    );
}
