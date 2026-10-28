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
//! Frame geometry pin for a sliced and resumed run of the DEFAULT tier.
//!
//! A frame-changing instruction must leave the Y window and the frame geometry
//! **re-derivable from Rust state** ([`Machine::push_frame_window`]), because
//! emitted code cannot reproduce them at a later entry: a resume re-enters a
//! block at its entry and re-executes no prefix. So `allocate` lowers to exactly
//! one `jit_allocate` helper call, and a block that touches a Y slot publishes
//! the window in its prologue -- the property below fails the moment a push
//! commits state that a parked-then-resumed slice does not re-derive (measured
//! as a `badfun`-class structured error inside `gen_server:try_handle_call/4`,
//! whose fun lives in a frame's Y slot).
//!
//! The shape below is that function's, reduced to its frame-relevant core:
//! `get_tuple_element x0, 6 -> x0` (the fun), `allocate 2, 4`,
//! `init_yregs [y0]`, `try y1, {handler}`, a Y-slot write/read pair, the three
//! real `swap`s, then `call_fun2 {atom, safe}, 3, x3` -- the instruction that
//! reads the fun out of the frame's Y slot. The function is split into two
//! blocks right after the `try` so the tier's in-block transfer is a second
//! reduction charge point *after* the push, which is what makes the preempt ->
//! resume loop park **behind** a live frame instead of only at the block entry.
//!
//! The structural half of the pin (the `allocate` arm is one `jit_allocate`
//! helper call and emitted code writes neither the Y-window slots nor a store
//! header) lives in `emit/heap.rs`'s
//! `the_allocate_arm_pushes_the_frame_with_a_helper_call_only`.

use super::{assemble_function, error_reason};
use crate::jit::Tier;
use crate::machine::RunOutcome;
use crate::{ExecutionContext, Machine, MinimalContext};
use beam_common::Result;
use beam_loader::{HeapNeed, Instr, Label, Module, Operand};
use beam_term::{is_small, make_small, signed_value, Eterm};

/// The host's typed pre-emption error, mirroring `call_tests.rs`'s `PREEMPTION`.
const PREEMPTION: &str = "host: reduction budget exhausted";

/// A slicing context: it spends a one-unit reduction budget, then answers the
/// driver's charge with the typed pre-emption error, so the run really is cut
/// into slices and re-entered rather than executed in one pass.
struct SliceCtx {
    inner: MinimalContext,
    budget: u32,
    charges: u32,
    fun_args: Vec<Eterm>,
}

impl SliceCtx {
    fn new() -> Self {
        SliceCtx {
            inner: MinimalContext::new(),
            // One unit: the run parks at the *first* charge point behind the
            // starting budget, and every resume buys exactly one more.
            budget: 0,
            charges: 0,
            fun_args: Vec::new(),
        }
    }
}

impl ExecutionContext for SliceCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
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
        if self.charges > self.budget {
            return Err(crate::error::unsupported(PREEMPTION));
        }
        Ok(())
    }

    fn wants_reduce(&self) -> bool {
        true
    }

    /// The `call_fun2` target: a local closure the host applies itself. The
    /// arguments are recorded so the test can pin the register state the frame's
    /// Y slot fed.
    fn apply_fun(&mut self, _fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
        self.fun_args = args.to_vec();
        Ok(make_small(7))
    }
}

/// The `gen_server:try_handle_call/4` shape, reduced to its frame-relevant core.
fn sliced_frame_module() -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern("frame_push_slice");
    let safe = module.atom_table.intern("safe");
    let code = vec![
        // The function label, then the export's entry label (as a real `.beam`
        // is decomposed: the entry is a label *after* the function label).
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        // `get_tuple_element x0, 6 -> x0`: the fun rides in the tuple.
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 6,
            dst: Operand::X(0),
        },
        // The frame push under test: two Y slots, `live = 4`.
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 4,
        },
        Instr::InitYregs {
            registers: vec![Operand::Y(0)],
        },
        Instr::Try {
            frame: Operand::Y(1),
            fail: Label(3),
        },
        // A Y-slot write/read pair: the read can only answer the fun if the
        // published Y window names *this* frame's slots.
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        // A block boundary behind the push, so the in-block transfer's
        // reproduced reduction charge is a second charge point with the frame
        // already live.
        Instr::Label(Label(4)),
        // The real `swap x1,x2 / swap x3,x1 / swap x0,x3` sequence.
        Instr::Swap {
            a: Operand::X(1),
            b: Operand::X(2),
        },
        Instr::Swap {
            a: Operand::X(3),
            b: Operand::X(1),
        },
        Instr::Swap {
            a: Operand::X(0),
            b: Operand::X(3),
        },
        // `call_fun2 {atom,safe}, 3, x3`: arity 3, the fun in x3.
        Instr::CallFun2 {
            tag: Operand::Atom(safe),
            arity: 3,
            func: Operand::X(3),
        },
        Instr::Return,
        // The try's handler. It must never run on the accepted path; it touches
        // both Y slots so a stray landing is observable in the value.
        Instr::Label(Label(3)),
        Instr::TryCase {
            frame: Operand::Y(1),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::TryEnd {
            frame: Operand::Y(1),
        },
        Instr::Return,
    ];
    assemble_function(&mut module, "f", 5, code, Some(Label(2)));
    module
}

/// One tier's sliced run: the returned value, the frame geometry it ended on,
/// how many slices it took and the recorded `call_fun2` arguments.
struct Sliced {
    value: i64,
    frames: Vec<(usize, u32)>,
    stack_len: usize,
    slices: u32,
    fun_args: Vec<i64>,
}

/// Drive `f/5` on `tier` through a preempt -> resume loop until it returns.
fn run_sliced(tier: Tier) -> Sliced {
    let module = sliced_frame_module();
    let mut ctx = SliceCtx::new();
    // The tuple is x0: seven elements, the fun last, so `element 6` is the fun.
    let fun = crate::terms::make_fun(&mut ctx, 3, 0, 0, &[]).expect("materialize the closure");
    let tuple = crate::terms::make_tuple(
        &mut ctx,
        &[
            make_small(1),
            make_small(2),
            make_small(3),
            make_small(4),
            make_small(5),
            make_small(6),
            fun,
        ],
    )
    .expect("materialize the tuple");
    let args = [
        tuple,
        make_small(20),
        make_small(21),
        make_small(22),
        make_small(23),
    ];

    let mut machine = Machine::new();
    let mut outcome = machine.run_label_outcome_with_tier(
        &module,
        Label(2),
        5,
        &args,
        &mut ctx,
        tier,
    );
    let mut slices = 1u32;
    loop {
        match outcome {
            Ok(RunOutcome::Returned(value)) => {
                assert!(
                    is_small(value),
                    "{tier:?}: the run must return the applied closure's small integer, got {value:?}"
                );
                let frames: Vec<(usize, u32)> = machine
                    .frames
                    .as_slice()
                    .iter()
                    .map(|frame| (frame.y_base, frame.y_count))
                    .collect();
                return Sliced {
                    value: signed_value(value),
                    frames,
                    stack_len: machine.stack_depth(),
                    slices,
                    fun_args: ctx.fun_args.iter().copied().map(signed_value).collect(),
                };
            }
            Ok(other) => panic!("{tier:?}: unexpected run outcome {other:?}"),
            Err(err) => {
                assert_eq!(
                    error_reason(&err),
                    error_reason(&crate::error::unsupported(PREEMPTION)),
                    "{tier:?}: the sliced run raised {err:?}"
                );
                let (func, pc) = machine
                    .current_code()
                    .expect("a pre-empted run parks on an exact instruction");
                // Buy exactly one more charge, so the run is cut into slices
                // one reduction at a time.
                ctx.budget = ctx.charges + 1;
                slices += 1;
                assert!(
                    slices < 512,
                    "{tier:?}: the sliced run never finished ({slices} slices)"
                );
                outcome = machine.resume_outcome_with_tier(&module, func, pc, &mut ctx, tier);
            }
        }
    }
}

/// The `gen_server:try_handle_call/4` frame shape, sliced and resumed: each
/// frame is pushed **exactly once**, the Y slots stay addressable, and the
/// default tier agrees with the interpreter on value and geometry.
#[test]
fn a_sliced_and_resumed_baseline_run_pushes_each_frame_exactly_once() {
    let interpreter = run_sliced(Tier::Interpreter);
    let baseline = run_sliced(Tier::BaselineJit);

    assert!(
        interpreter.slices > 1,
        "the interpreter run must be sliced, got {} slice(s)",
        interpreter.slices
    );
    assert!(
        baseline.slices > 1,
        "the baseline run must be sliced and re-entered, got {} slice(s)",
        baseline.slices
    );

    assert_eq!(
        interpreter.value, baseline.value,
        "the tiers returned different values"
    );
    assert_eq!(interpreter.value, 7, "the applied closure's value");
    // The swap sequence feeds `call_fun2` x0..x2 = [x1, x2, x0] of the entry
    // arguments after the three swaps: a differing order means the register
    // file the frame was resumed over had drifted.
    assert_eq!(
        interpreter.fun_args,
        vec![21, 22, 20],
        "the interpreter must reach the closure with the swapped arguments"
    );
    assert_eq!(
        baseline.fun_args, interpreter.fun_args,
        "the baseline tier must reach the closure with the interpreter's arguments"
    );

    // Geometry: one push, one frame. A re-executed `allocate` would leave an
    // extra frame and its two stack slots behind.
    assert_eq!(
        baseline.frames, interpreter.frames,
        "the tiers ended on different frame geometry"
    );
    assert_eq!(
        baseline.stack_len, interpreter.stack_len,
        "the tiers ended on different stack depths"
    );
    // The run halts on the `Halt` continuation, so the pushed frame is still on
    // the frame stack when it returns: exactly ONE frame may carry the two Y
    // slots, and the stack must hold exactly those two words. A re-executed
    // `allocate` would leave a second two-slot frame and four stack words.
    let pushed = interpreter.frames.iter().filter(|frame| frame.1 == 2).count();
    assert_eq!(
        pushed, 1,
        "exactly one frame may carry the two Y slots, got {:?}",
        interpreter.frames
    );
    assert_eq!(
        interpreter.stack_len, 2,
        "the pushed frame's two Y slots must be reserved exactly once"
    );
}
