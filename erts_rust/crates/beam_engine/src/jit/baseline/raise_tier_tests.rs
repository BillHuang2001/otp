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
//! Frame-discipline pin: a **caught raise** whose handler block is the raising
//! instruction's own `pc + 1`, on the native baseline tier. A `#[path]` child of
//! `super::tests`, so it shares that module's harness (`with_both_tiers`,
//! `assert_same_outcome`, `run_counting`, `body_module`).
//!
//! # The shape
//!
//! One function: `allocate 2` (**frame A**, 2 Y slots) → `y(0) := 11` → `try` (the catch
//! frame records frame A) → `allocate 1` (**frame B**) → `case_end` raising
//! `{case_clause, x(0)}` → the handler label **immediately** after it (so the handler is the
//! raise's own `pc + 1`) → `try_case` → `y(0) → x(0)` → `deallocate 2` → `return`.
//!
//! The raise unwinds past frame B into frame A, so the handler must read the **11** frame A
//! wrote. A handler whose `y(0)` read resolved through frame B's (destroyed) window would
//! read a raw stack word instead — the failure mode this pin exists for.
//!
//! # Why the pin is kept
//!
//! `runtime::fold_step` encodes `Step::Next` and `Step::Goto { pc: pc + 1 }` as the **same**
//! block-ABI word, so a family raise whose catch handler is the raising instruction's own
//! `pc + 1` is folded by the emitted continue check into an **in-block** continue: the block
//! then runs off its end and the linker's exit jumps **straight into the handler block**.
//! **No driver-loop iteration runs** between the unwind — `Machine::unwind_to_frame`
//! truncates `machine.frames` **and** `machine.stack` — and the handler.
//!
//! That is sound today only because the **two emitted points that can re-derive the
//! `[y_base, y_end)` window are both unconditional**: the handler block's prologue publish
//! (`jit_publish_y`) and the gated in-block refresh after the `try_case` family helper
//! (`jit_refresh_y`, emitted because the block's `y(0)` read follows it) each re-derive the
//! window from the *live* frame A, so either one alone keeps the read correct. A future
//! change that makes a block prologue's window derivation — or the position store /
//! reduction charge that shares its prologue helper — conditional on prior state (for
//! instance a cross-block "the window is already current" witness) must first close this
//! hole: on this route nothing between the unwind and the handler's `Y` read observes the
//! frame change, so with both derivations skipped the handler reads frame B's stale window
//! (a raw stack word, 59) where the interpreter reads 11.
//!
//! The tiers must agree, and neither may deopt: the shape is fully lowered, so a
//! `jit_step_one` here would hide the emitted window discipline behind the interpreter's own
//! `machine.stack` read.
use super::*;
use crate::jit::Tier;
use beam_loader::HeapNeed;

/// The engine resolves a raise's **reason** and **class** atoms through the running
/// module's own atom table (`machine::catch::namespace_atom`, whose fallback is the module
/// snapshot), so a hand-built fixture that raises must name them itself —
/// [`build_module`](super::build_module) interns only its module/function/import names.
fn intern_exception_atoms(module: &mut Module) {
    for name in ["case_clause", "error", "exit", "throw", "EXIT"] {
        module.atom_table.intern(name);
    }
}

/// The fixture: frame A holds `y(0) = 11` across a `try`, a second `allocate` creates frame
/// B, and the `case_end` immediately after it raises into the handler at its own `pc + 1`.
fn raise_at_own_pc_plus_one_module() -> (Module, Atom) {
    let (mut module, atom) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(11),
            dst: Operand::Y(0),
        },
        // The catch frame records the function depth of frame A.
        Instr::Try {
            frame: Operand::X(1),
            fail: Label(9),
        },
        // A second frame, so the raise has to unwind past one to reach its handler.
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        // A family raise (`{case_clause, x(0)}`); the handler label below sits at exactly
        // this instruction's `pc + 1`, so the landing folds into an in-block continue.
        Instr::CaseEnd {
            value: Operand::X(0),
        },
        // The handler: its own block.
        Instr::Label(Label(9)),
        Instr::TryCase {
            frame: Operand::X(1),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
    ]);
    intern_exception_atoms(&mut module);
    debug_assert!(atom == export_atom(&module, "f"));
    (module, atom)
}

/// The baseline tier runs this shape to completion and reads frame A's `y(0)`: the handler
/// block's `Y` read resolves through a window derived from the live (post-unwind) frame,
/// and every instruction of the shape is lowered, so nothing is masked by an interpreter
/// step. The interpreter — which reads `machine.stack` directly — is the reference for what
/// the destroyed frame B must not contribute.
#[test]
fn a_raise_at_its_own_pc_plus_one_reads_the_enclosing_frames_y_slot() {
    let (module, atom) = raise_at_own_pc_plus_one_module();
    let (outcome, snapshot) = run_counting(&module, atom, Tier::BaselineJit);
    match &outcome {
        Ok(RunOutcome::Returned(value)) => assert_eq!(
            value.raw(),
            make_small(11).raw(),
            "the handler must read frame A's y(0) (11) through the live frame window, even \
             though it is the raising instruction's own pc + 1"
        ),
        other => panic!("the raise-at-pc-plus-one shape must return 11, got {other:?}"),
    }
    assert_eq!(
        snapshot.step_one, 0,
        "every instruction of the shape is lowered; a deopt would mask the emitted window \
         discipline behind the interpreter's own stack read (deopts={:?})",
        snapshot.deopts_by_variant,
    );
    with_both_tiers(&module, atom, |interpreter, baseline| {
        for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
            match outcome {
                Ok(RunOutcome::Returned(value)) => assert_eq!(
                    value.raw(),
                    make_small(11).raw(),
                    "{tier}: the handler must read frame A's y(0)"
                ),
                other => panic!("{tier}: the shape must return 11, got {other:?}"),
            }
        }
        assert_same_outcome(
            "raise_at_pc_plus_one",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}
