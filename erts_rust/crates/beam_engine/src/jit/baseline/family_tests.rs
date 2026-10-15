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
//! Tier-equivalence and structural pins for the family-helper lowering
//! (`super::family`). A `#[path]` child of `super::tests`, so it shares that module's
//! harness (`with_both_tiers`, `assert_same_outcome`, `body_module`, `build_module`).
//!
//! The three claims these tests pin, one per concern:
//!
//! * the two constructors `emit_block` claims for the family helper (`put_list`,
//!   `put_tuple2`) produce the **interpreter's exact outcome** on both tiers — a boxed
//!   cons cell / tuple, never a small int and never a divergence;
//! * a family raise (an out-of-range register operand) is the **same typed error** on
//!   both tiers, byte-for-byte (`error_reason`);
//! * the two variants are **no longer a per-instruction deopt**, statically (the
//!   emitter's own decision) or at run time — and, because the helper writes
//!   destinations through `machine.x` while the register allocator was **not** changed,
//!   a later natively-executed read of such a destination is served by
//!   `emit_continue_check`'s partial refill. That last one is the coherence pin the
//!   whole lowering rests on: it asserts both the structural fact (the destination IS a
//!   cached register, and the helper's fill set reloads it) and the observable one (both
//!   tiers return the built term through a native read of it).

use super::{assert_same_outcome, body_module, error_reason, run_counting, with_both_tiers};
use crate::jit::Tier;
use crate::machine::RunOutcome;
use beam_loader::{Instr, Label, Module, Operand};

/// The `[start, end)` of the block of `function` that contains `pc` — the range the
/// emitter allocates registers over (and therefore the range [`allocate_registers`] must
/// be asked about for an assertion to describe the emitted block).
///
/// [`allocate_registers`]: super::super::emit::allocate_registers
fn block_of(module: &Module, function: usize, pc: u32) -> (usize, usize) {
    module.functions[function]
        .blocks
        .iter()
        .filter(|block| block.start as u32 <= pc && pc < block.end as u32)
        .next_back()
        .map(|block| (block.start, block.end))
        .expect("the pc must lie inside a block")
}

/// **Both family-owned constructors agree across the tiers and return a boxed term.**
///
/// The shapes are exactly `tests.rs`'s `put_list_case`/`put_tuple2_case` (a body block
/// whose single constructor writes `x(0)`, then `return`), so the only thing that
/// changed for them is the lowering: the family helper instead of a `jit_step_one`
/// deopt. The concrete anchors (a proper cons list for `put_list`, a 2-tuple for
/// `put_tuple2`) are what keep the cross-tier comparison from being vacuously true on
/// two identically-wrong sides.
#[test]
fn the_family_owned_constructors_agree_across_the_tiers() {
    let (list, list_name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::Nil,
            dst: Operand::X(0),
        },
        Instr::Return,
    ]);
    with_both_tiers(&list, list_name, |interpreter, baseline| {
        assert_same_outcome("put_list", interpreter, baseline, &list.atom_table);
        for (label, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
            match outcome.as_ref().expect("the run completes") {
                RunOutcome::Returned(value) => {
                    assert!(
                        beam_term::is_list(*value),
                        "{label}: `put_list` must return a proper cons list"
                    );
                    assert!(
                        !beam_term::is_small(*value),
                        "{label}: a cons cell is boxed, never a small int"
                    );
                    assert_eq!(
                        crate::terms::cons_head(*value).expect("a cons cell"),
                        beam_term::make_small(1),
                        "{label}: the head must be the `1` operand"
                    );
                }
                other => panic!("{label}: unexpected outcome {other:?}"),
            }
        }
    });

    let (tuple, tuple_name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::Integer(1), Operand::Integer(2)],
        },
        Instr::Return,
    ]);
    with_both_tiers(&tuple, tuple_name, |interpreter, baseline| {
        assert_same_outcome("put_tuple2", interpreter, baseline, &tuple.atom_table);
        for (label, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
            match outcome.as_ref().expect("the run completes") {
                RunOutcome::Returned(value) => {
                    assert!(
                        !beam_term::is_small(*value),
                        "{label}: a tuple is boxed, never a small int"
                    );
                    assert_eq!(
                        crate::terms::tuple_arity(*value).expect("a tuple"),
                        2,
                        "{label}: the two element operands must build a 2-tuple"
                    );
                }
                other => panic!("{label}: unexpected outcome {other:?}"),
            }
        }
    });
}

/// **A family raise is the interpreter's exact typed error on both tiers.**
///
/// The only thing a `put_list`/`put_tuple2` can fail on is a register operand outside
/// the machine's files: `read_operand`/`write_operand` report `bad_register`
/// ([`crate::error::bad_register`]) and the family propagates it, so the helper must
/// surface the interpreter's own error word-for-word rather than a substituted one.
/// Three operands, one per read/write site: `put_list`'s `head` and `tail` (a read) and
/// `put_tuple2`'s `dst` (a write after a successful build).
#[test]
fn a_family_owned_constructor_raise_is_identical_on_both_tiers() {
    let cases = [
        (
            "put_list head",
            body_module(vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::PutList {
                    head: Operand::X(100_000),
                    tail: Operand::Nil,
                    dst: Operand::X(0),
                },
                Instr::Return,
            ]),
        ),
        (
            "put_list tail",
            body_module(vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::PutList {
                    head: Operand::Integer(1),
                    tail: Operand::Y(100_000),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ]),
        ),
        (
            "put_tuple2 dst",
            body_module(vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::PutTuple2 {
                    dst: Operand::X(100_000),
                    elements: vec![Operand::Integer(1), Operand::Integer(2)],
                },
                Instr::Return,
            ]),
        ),
    ];
    for (label, (module, name)) in cases {
        with_both_tiers(&module, name, |interpreter, baseline| {
            let interpreter_err = interpreter
                .as_ref()
                .expect_err("an out-of-range register operand is a typed error");
            let baseline_err = baseline
                .as_ref()
                .expect_err("an out-of-range register operand is a typed error");
            assert_eq!(
                error_reason(interpreter_err),
                "BadArg",
                "{label}: the interpreter reports `bad_register` as `BadArg`"
            );
            assert_eq!(
                error_reason(interpreter_err),
                error_reason(baseline_err),
                "{label}: the two tiers raised different errors"
            );
        });
    }
}

/// **The two constructors are lowered without a per-instruction `jit_step_one` deopt** —
/// the structural pin.
///
/// One module executes both in one body block (so a single `emit_block` visit covers
/// each), and the recorded counters must show each one **visited** (the pin is not
/// vacuous) yet **never** deopted, statically (`instr_deopts`, the emitter's own
/// decision) or at run time (`deopts_by_variant`). Every other instruction of the module
/// (`label`, `return`) is lowered too, so the whole run must pay **zero** interpreter
/// steps.
#[test]
fn the_family_owned_constructors_are_lowered_without_a_jit_step_one_deopt() {
    let (module, name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::Nil,
            dst: Operand::X(0),
        },
        Instr::PutTuple2 {
            dst: Operand::X(1),
            elements: vec![Operand::X(0), Operand::Integer(2)],
        },
        Instr::Return,
    ]);
    let (outcome, snapshot) = run_counting(&module, name, Tier::BaselineJit);
    assert!(
        matches!(outcome, Ok(RunOutcome::Returned(_))),
        "the run completes with a built term; saw {outcome:?}"
    );
    for opcode in ["PutList", "PutTuple2"] {
        assert!(
            snapshot.instr_instances.get(opcode).copied().unwrap_or(0) > 0,
            "`{opcode}` must have been visited by the emitter, or this pin proves \
             nothing; instances: {:?}",
            snapshot.instr_instances,
        );
        assert_eq!(
            snapshot.instr_deopts.get(opcode).copied().unwrap_or(0),
            0,
            "`{opcode}` is lowered, so it must record zero static deopts; deopts: {:?}",
            snapshot.instr_deopts,
        );
        assert!(
            !snapshot.instr_deopts.contains_key(opcode),
            "`{opcode}` is lowered, so it must have no static deopt entry at all; \
             deopts: {:?}",
            snapshot.instr_deopts,
        );
        assert!(
            !snapshot.deopts_by_variant.contains_key(opcode),
            "`{opcode}` must be lowered natively, but it deopted {} time(s); \
             deopts by variant: {:?}",
            snapshot.deopts_by_variant.get(opcode).copied().unwrap_or(0),
            snapshot.deopts_by_variant,
        );
    }
    assert_eq!(
        snapshot.step_one, 0,
        "every instruction of this module is lowered (both constructors through the \
         family helper), so no interpreter step may remain; deopts by variant: {:?}",
        snapshot.deopts_by_variant,
    );
}

/// **A family-built term reaches a later native read through the cache refill** — the
/// cached-register coherence pin, with **no register-allocator change**.
///
/// `put_list` writes its destination through `machine.x` (the helper *is* the family),
/// while the register allocator caches `x(1)` for the whole block and `PutList`
/// contributes no `cacheable_writes` entry — so the block's cached copy of `x(1)` is
/// stale the moment the helper returns, and the following `move x(1) -> x(0)`, which
/// reads `x(1)` **through its cached machine register**, is only correct if
/// [`emit_continue_check`]'s partial refill reloaded it.
///
/// The two halves of the pin, so neither can pass vacuously:
///
/// * **structural** — `x(1)` is a cached register of the emitted block's range
///   ([`allocate_registers`](super::super::emit::allocate_registers), the allocator
///   `emit::compile` uses) *and* the helper's fill set at the `put_list` pc lists it: the
///   refill, not a lucky cache membership, is what carries the value;
/// * **behavioural** — both tiers return the cons cell (head `7`), the interpreter by
///   writing `machine.x` and the baseline tier by reloading it into the cache and moving
///   it natively.
///
/// [`emit_continue_check`]: super::super::emit
#[test]
fn a_family_built_term_reaches_a_later_native_read_through_the_cache_refill() {
    let (module, name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::PutList {
            head: Operand::Integer(7),
            tail: Operand::Nil,
            dst: Operand::X(1),
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]);
    // The `put_list` is at index 2 of the function's code (label, label, put_list, …).
    const PUT_LIST_PC: u32 = 2;
    let code = &module.functions[0].code;
    assert!(
        matches!(code[PUT_LIST_PC as usize], Instr::PutList { .. }),
        "the pin's pc arithmetic must name the `put_list`; code: {code:?}"
    );
    let (start, end) = block_of(&module, 0, PUT_LIST_PC);
    let allocation = super::super::emit::allocate_registers(&module, code, start, end);
    assert!(
        allocation.slots.iter().any(|(x, _)| *x == 1),
        "x(1) must be a cached register of the block, or the refill is not load-bearing; \
         slots: {:?}",
        allocation.slots,
    );
    assert!(
        allocation
            .fill_slots(PUT_LIST_PC)
            .iter()
            .any(|(x, _)| *x == 1),
        "the family helper's continuation at pc {PUT_LIST_PC} must refill the cached \
         x(1) from `machine.x`, or the following `move` reads a stale register; fill \
         slots: {:?}",
        allocation.fill_slots(PUT_LIST_PC),
    );

    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(
            "a cached family result read by a later move",
            interpreter,
            baseline,
            &module.atom_table,
        );
        for (label, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
            match outcome.as_ref().expect("the run completes") {
                RunOutcome::Returned(value) => {
                    assert!(
                        beam_term::is_list(*value) && !beam_term::is_small(*value),
                        "{label}: the `move` must carry the cons cell `put_list` built, \
                         not a stale cache word"
                    );
                    assert_eq!(
                        crate::terms::cons_head(*value).expect("a cons cell"),
                        beam_term::make_small(7),
                        "{label}: the head must be the `7` operand"
                    );
                }
                other => panic!("{label}: unexpected outcome {other:?}"),
            }
        }
    });
}
