// %CopyrightBegin%
//
// Copyright Ericsson AB 2026. All Rights Reserved.
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

//! Pins for a publishing block's **post-publish alternate entry**.
//!
//! A block whose prologue publishes the Y window reports a second entry point — the buffer
//! offset just past the publish, where `rdi` again holds `rt` and `rsp` is again
//! `≡ 8 (mod 16)`. `jit_deallocate_return` publishes the popped frame's window itself and then
//! enters its continuation through that alternate, so the up leg of a returning
//! `deallocate_return` pays **one** helper call where it used to pay two (the helper, plus the
//! continuation sub-block's own prologue publish).
//!
//! Three invariants are pinned here, each as a *relation*, never as a byte count:
//!
//! 1. the merged transfer publishes the window **once per entering frame, never once per
//!    element** — the same recursive loop run at `N` and at `2N` records the same `publish_y`
//!    figure, while the alternate is entered once per element;
//! 2. a continuation sub-block really does carry a publish in its own prologue (so (1) is not
//!    vacuous) and that alternate really is the address the transfer takes;
//! 3. the publish sequence is exactly `push rdi` + three `movabs` + `call r11` + `pop rdi` —
//!    measured against the encoder emitting those same six primitives — which is what makes
//!    the reported offset "the normal entry plus the publish sequence".
//!
//! The counters are `#[cfg(test)]`-only and cost nothing in production; nothing here depends
//! on host load or timing.

use beam_loader::{HeapNeed, ImportRef, Instr, Label, Operand, TestKind, NO_FAIL};
use beam_term::{make_small, Atom, Eterm};

use super::counters::{self, Counters};
use super::emit::publish_y_len;
use super::tests::{build_module, export_atom};
use super::*;
use crate::context::MinimalContext;
use crate::jit::emitter::{CodeBuffer, Reg};
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};

/// `sum(N)` recursing through a frame-preserving local `call`, with the per-element
/// increment parked in a `Y` slot of the **continuation sub-block** (the instruction after
/// the `call`), so every returned element re-enters that sub-block through its alternate.
///
/// ```text
/// sum/1:
///   label 1:                         ; block 0 (the function entry label)
///   label 2:                         ; block 1 — Y-free, never publishes
///     test is_eq_exact x0, 0
///     fail -> label 4
///     move 0 -> x0
///     return
///   label 4:                         ; block 2 — publishes (no frame helper before it)
///     allocate 1 1                   ; frame: one Y slot
///     move 1 -> y0                   ; the per-element increment
///     gc_bif erlang:'-'/2 x0, 1 -> x0 fail NO_FAIL
///     call 1 label 2                 ; continuation = the next instruction
///     move y0 -> x1                  ; the continuation SUB-BLOCK (publishes too)
///     gc_bif erlang:'+'/2 x0, x1 -> x0 fail NO_FAIL
///     deallocate_return 1
/// ```
fn sum_module() -> (Module, Atom) {
    let module = build_module(
        "post_publish_sum",
        &[("erlang", "-", 2), ("erlang", "+", 2)],
        vec![(
            "sum",
            1,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::Test {
                    kind: TestKind::IsEqExact,
                    fail: Label(4),
                    args: vec![Operand::X(0), Operand::Integer(0)],
                },
                Instr::Move {
                    src: Operand::Integer(0),
                    dst: Operand::X(0),
                },
                Instr::Return,
                Instr::Label(Label(4)),
                Instr::Allocate {
                    stack_need: HeapNeed::Words(1),
                    live: 1,
                },
                Instr::Move {
                    src: Operand::Integer(1),
                    dst: Operand::Y(0),
                },
                Instr::GcBif {
                    arity: 2,
                    bif: ImportRef(0),
                    fail: NO_FAIL,
                    live: 1,
                    args: vec![Operand::X(0), Operand::Integer(1)],
                    dst: Operand::X(0),
                },
                Instr::Call {
                    arity: 1,
                    target: Label(2),
                },
                Instr::Move {
                    src: Operand::Y(0),
                    dst: Operand::X(1),
                },
                Instr::GcBif {
                    arity: 2,
                    bif: ImportRef(1),
                    fail: NO_FAIL,
                    live: 2,
                    args: vec![Operand::X(0), Operand::X(1)],
                    dst: Operand::X(0),
                },
                Instr::DeallocateReturn(1),
            ],
        )],
        &[("sum", 1, Label(2))],
    );
    let function = export_atom(&module, "sum");
    (module, function)
}

/// Run `sum(N)` on `tier`, answering the returned value (if the run returned one) beside the
/// structural counters recorded over that one run — counters enabled only around the run.
fn run_sum(tier: Tier, iterations: i32) -> (beam_common::Result<RunOutcome>, Counters) {
    let (module, function) = sum_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(crate::jit::bench::support::arith_hook(None, None));
    counters::reset();
    counters::set_enabled(true);
    let outcome = machine.run_export_outcome_with_tier(
        &module,
        function,
        1,
        &[make_small(i64::from(iterations))],
        &mut ctx,
        tier,
    );
    counters::set_enabled(false);
    let recorded = counters::snapshot();
    (outcome, recorded)
}

/// The returned value of `sum(iterations)`, panicking with the run's own outcome when the loop
/// does not return one — so a tier that raises names its error instead of a bare `None`.
fn sum_value(tier: Tier, iterations: i32) -> Eterm {
    match run_sum(tier, iterations).0 {
        Ok(RunOutcome::Returned(value)) => value,
        Ok(_) => panic!("sum({iterations}) must return on {tier:?}, not suspend"),
        Err(error) => panic!("sum({iterations}) raised on {tier:?}: {error:?}"),
    }
}

/// The value assertion for the loop, on both tiers, so the counters below are read beside a
/// run that provably executed the elements it is charged for.
#[test]
fn the_recursive_loop_returns_the_same_sum_on_both_tiers() {
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        assert_eq!(
            beam_term::number::big_to_f64(sum_value(tier, 100)),
            Some(5050.0),
            "sum(100) is 5050 on {tier:?}"
        );
    }
}

/// **(a) + (b):** the merged transfer publishes the window **once per entering frame**, never
/// once per element — and the alternate really is taken once per element.
///
/// The relational form is the pin: the same loop at `N` and at `2N` records the *same*
/// `publish_y` figure (a per-element publish would double it), while `post_publish_entries`
/// grows by exactly `N` — one alternate entry per returning `deallocate_return`.
#[test]
fn the_merged_transfer_publishes_once_and_enters_the_alternate_once_per_element() {
    const N: i32 = 100;
    let (short_outcome, short_counters) = run_sum(Tier::BaselineJit, N);
    let (long_outcome, long_counters) = run_sum(Tier::BaselineJit, N * 2);
    assert!(
        matches!(short_outcome, Ok(RunOutcome::Returned(_))),
        "sum({N}) must return a value"
    );
    assert!(
        matches!(long_outcome, Ok(RunOutcome::Returned(_))),
        "sum({}) must return a value",
        N * 2
    );

    assert_eq!(
        short_counters.publish_y, long_counters.publish_y,
        "the publish does not grow with the element count: {} at N={N} vs {} at N={}",
        short_counters.publish_y,
        long_counters.publish_y,
        N * 2
    );
    assert!(
        short_counters.publish_y >= 1,
        "the continuation sub-block's prologue does publish, so this pin is not vacuous"
    );

    assert_eq!(
        short_counters.post_publish_entries,
        u64::from(N as u32) - 1,
        "one alternate entry per inner returning element at N={N}"
    );
    assert_eq!(
        long_counters
            .post_publish_entries
            .saturating_sub(short_counters.post_publish_entries),
        u64::from(N as u32),
        "the alternate is taken once per element, not once per run"
    );
    assert_eq!(
        short_counters.post_publish_entries, short_counters.post_publish_requests,
        "every probe the returning transfer makes finds its alternate (no null-slot residual)"
    );

    assert_eq!(
        beam_term::number::big_to_f64(sum_value(Tier::Interpreter, N)),
        Some(5050.0),
        "the interpreter executes the same loop to the same value"
    );
}

/// **(c):** the publish sequence is `push rdi` + three `movabs` + `call r11` + `pop rdi`,
/// measured against the encoder emitting those same six primitives into a fresh buffer — so the
/// length is derived from the emitter, never copied from the byte arithmetic.
///
/// `emit_block_prologue` reports exactly this length as the block's second entry point (the
/// buffer offset taken immediately after the publish), so "the alternate is the normal entry
/// plus the publish sequence" is the relation this pins.
#[test]
fn the_publish_sequence_is_the_rt_save_three_movabs_call_and_the_restore() -> Result<()> {
    let mut reference = CodeBuffer::new()?;
    reference.push_r(Reg::Rdi)?;
    reference.mov_ri(Reg::Rsi, 0)?;
    reference.mov_ri(Reg::Rdx, 0)?;
    reference.mov_ri(Reg::R11, 0)?;
    reference.call_r(Reg::R11)?;
    reference.pop_r(Reg::Rdi)?;
    let expected = reference.len();

    let measured = publish_y_len()?;
    assert_eq!(
        measured, expected,
        "the publish is push(rt) + 3 movabs + call + pop(rt): {measured} B emitted, while the \
         same six primitives cost {expected} B"
    );

    // The bracketing is load-bearing: the `push` supplies the C ABI's 16-byte `rsp` alignment
    // at the call (a block entry has `rsp ≡ 8 (mod 16)`), and the `pop` restores `rt`, so the
    // alternate entry sees exactly the normal entry's post-`pop` state. Dropping either would
    // shorten the sequence below three `movabs` plus a `call`, which the assertion above
    // rejects; state the arithmetic form too so a failure names the missing piece.
    assert!(
        measured > 3 * 10 + 3,
        "the sequence carries the rt save/restore on top of three movabs and a call"
    );
    Ok(())
}

/// The alternate is *only* preferred by the returning `deallocate_return`: a plain `return`
/// transfer passes `prefer = false`, so a loop whose continuation is reached by `jit_return`
/// records no alternate probe at all.
#[test]
fn only_the_deallocate_return_transfer_asks_for_the_alternate() {
    // A `Y`-touching block whose frame is popped by a plain `return`, so every transfer on the
    // way out is `jit_return` (which asks for no alternate).
    let module = build_module(
        "post_publish_return",
        &[],
        vec![(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::Allocate {
                    stack_need: HeapNeed::Words(1),
                    live: 0,
                },
                Instr::Move {
                    src: Operand::Integer(7),
                    dst: Operand::Y(0),
                },
                Instr::Call {
                    arity: 0,
                    target: Label(3),
                },
                Instr::Return,
                Instr::Label(Label(3)),
                Instr::Move {
                    src: Operand::Integer(0),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        )],
        &[("f", 0, Label(2))],
    );
    let function = export_atom(&module, "f");

    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        counters::reset();
        counters::set_enabled(true);
        let outcome =
            machine.run_export_outcome_with_tier(&module, function, 0, &[], &mut ctx, tier);
        counters::set_enabled(false);
        let recorded = counters::snapshot();
        assert!(
            matches!(outcome, Ok(RunOutcome::Returned(_))),
            "f/0 must return on {tier:?}"
        );
        assert_eq!(
            recorded.post_publish_requests, 0,
            "a `return` transfer passes prefer=false, so it asks for no alternate ({tier:?})"
        );
    }
}
