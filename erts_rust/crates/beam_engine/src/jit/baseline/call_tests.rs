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
