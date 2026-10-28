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
//! Five invariants are pinned here, each as a *relation*, never as a byte count:
//!
//! 1. the loop really executes the elements its counters are read beside: the recursive
//!    fixture returns its exact triangular sum on **both** tiers, compared with
//!    [`beam_term::eq`] while the arena that owns the value is still alive;
//! 2. the window is published **once per element in total** — a returning
//!    `deallocate_return` publishes the popped frame's window itself (substituting for the
//!    continuation sub-block's prologue publish, which no longer runs) while a prologue only
//!    publishes on a **fresh** native entry, so a per-element prologue publish would double
//!    the `publish_y` figure between `N` and `2N`;
//! 3. the alternate is entered **once per element**, and every probe finds one;
//! 4. a continuation sub-block really does carry a publish in its own prologue (so (2) is not
//!    vacuous): over a **cache-free** compile its `pc` has both a normal entry and a
//!    non-null alternate, and the emitted publishing prologues are exactly the non-null
//!    alternate slots;
//! 5. the alternate is the normal entry **plus the publish sequence**, and it lies strictly
//!    inside the block's own emitted extent — with the sequence's length taken from the
//!    emitter — while a block that touches no `Y` slot has **no** alternate at all.
//!
//! The counters are `#[cfg(test)]`-only and cost nothing in production; nothing here depends
//! on host load or timing.

use beam_loader::{HeapNeed, ImportRef, Instr, Label, Operand, TestKind, NO_FAIL};
use beam_term::{make_small, Atom};

use super::counters::{self, Counters};
use super::emit::{allocate_registers, compile_function, publish_y_len};
use super::tests::{build_module, export_atom};
use super::*;
use crate::context::MinimalContext;
use crate::jit::emitter::{CodeBuffer, Reg};
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};

/// `sum(N)` recursing through a frame-preserving local `call`, accumulating the element's own
/// argument out of a `Y` slot of the **continuation sub-block** (the instruction after the
/// `call`), so every returned element re-enters that sub-block through its alternate entry.
///
/// ```text
/// sum/1:                             ; sum(N) = N + sum(N-1), sum(0) = 0
///   label 1:                         ; block 0 (the function entry label)
///   label 2:                         ; block 1 — Y-free, never publishes
///     test is_eq_exact x0, 0
///     fail -> label 4
///     move 0 -> x0
///     return                         ; the base case: the ONE fresh entry into the
///                                    ; continuation sub-block (`jit_return`, no alternate)
///   label 4:                         ; block 2 — publishes nothing (`allocate` comes first,
///                                    ; its helper re-publishes the window itself)
///     allocate 1 1                   ; frame: one Y slot
///     move x0 -> y0                  ; the element's own argument
///     gc_bif erlang:'-'/2 x0, 1 -> x0
///     call 1 label 2                 ; continuation = the next instruction
///     move y0 -> x1                  ; the continuation SUB-BLOCK — it does publish
///     gc_bif erlang:'+'/2 x0, x1 -> x0
///     deallocate_return 1            ; publishes the popped frame's window, enters the
///                                    ; caller's alternate
/// ```
///
/// The arithmetic is a `gc_bif` on the interpreter, so the fixture is hosted on
/// [`crate::jit::bench::support::arith_hook`] — the same small-int `erlang` hook the in-crate
/// benchmark kernels install — instead of `MinimalContext::new()`, which accepts no BIF at all.
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
                    src: Operand::X(0),
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

/// The value `sum(N)` must return: `Σ 1..=N`.
fn expected_sum(iterations: i32) -> i64 {
    let n = i64::from(iterations);
    n * (n + 1) / 2
}

/// The `pc` of the fixture's **continuation sub-block** — the instruction a frame-preserving
/// `call` resumes at — derived from the code itself rather than restated as a constant.
fn continuation_pc(module: &Module) -> usize {
    module.functions[0]
        .code
        .iter()
        .position(|instr| matches!(instr, Instr::Call { .. }))
        .expect("the fixture carries one frame-preserving call")
        + 1
}

/// What one `sum(N)` run did, with everything that needs the producing arena reduced to owned
/// data **before** that arena drops.
///
/// Reading a term after its `MinimalContext` is gone is a use-after-free that surfaces as a
/// wrong value or a term-integrity abort, never as a typed error
/// (`beam_engine/src/CONTEXT.md`), so the comparison happens inside [`run_sum`], where the
/// arena is still alive.
struct SumRun {
    /// `Ok(())` when the loop returned a value, else a rendering of what it did instead.
    outcome: std::result::Result<(), String>,
    /// Whether the returned value equals [`expected_sum`], compared with
    /// [`beam_term::eq`] while the arena that owns it was still alive.
    equals_expected: bool,
    /// The returned value as an `i64` when it is a small integer (for the failure message).
    value: Option<i64>,
    /// The structural counters recorded over that one run.
    counters: Counters,
}

/// Run `sum(iterations)` on `tier` under the small-int arithmetic hook, answering what it did
/// and the structural counters recorded over that one run — counters enabled only around it.
fn run_sum(tier: Tier, iterations: i32) -> SumRun {
    let expected = expected_sum(iterations);
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
    // The arena `ctx` owns the returned term's words: compare while it is still alive.
    let (outcome, equals_expected, value) = match outcome {
        Ok(RunOutcome::Returned(returned)) => (
            Ok(()),
            beam_term::eq(returned, make_small(expected), &module.atom_table),
            beam_term::is_small(returned).then(|| beam_term::signed_value(returned)),
        ),
        Ok(_) => (Err("the run suspended instead of returning".to_string()), false, None),
        Err(error) => (Err(format!("{error:?}")), false, None),
    };
    SumRun {
        outcome,
        equals_expected,
        value,
        counters: recorded,
    }
}

/// The non-null **alternate** slots of one cache-free `compile_function`: the entries whose
/// prologue published, i.e. the emitted publishing prologues.
///
/// The per-function entry vector is `2 * code.len()` slots — `[0, n)` the folded entry table
/// and `[n, 2n)` the post-publish alternates (`emit/compile.rs`) — so the second half is
/// exactly the set of publishing entries.
fn alternate_slots(function: &super::code::CompiledFunction, code_len: usize) -> Vec<usize> {
    (0..code_len)
        .filter(|pc| !function.entries[code_len + pc].is_null())
        .collect()
}

/// **(a) + (b) + (c):** the loop provably runs the elements it is charged for, the window is
/// published **once per element in total**, and the alternate is entered once per element —
/// with a static non-vacuity check that the continuation sub-block really publishes.
#[test]
fn the_merged_transfer_publishes_once_and_enters_the_alternate_once_per_element() {
    const N: i32 = 100;

    // (a) The loop really executes what it is charged for, on **both** tiers: the returned
    // value is `N*(N+1)/2`, so the `N` elements below are proven by the value, not assumed.
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let run = run_sum(tier, N);
        assert!(
            run.outcome.is_ok(),
            "sum({N}) must return on {tier:?}: {:?}",
            run.outcome
        );
        assert!(
            run.equals_expected,
            "sum({N}) = {:?} on {tier:?}, expected {} (compared with beam_term::eq while the \
             arena that owns the value was still alive)",
            run.value,
            expected_sum(N)
        );
    }

    let short = run_sum(Tier::BaselineJit, N);
    let long = run_sum(Tier::BaselineJit, N * 2);
    assert!(
        long.equals_expected,
        "sum({}) = {:?}, expected {}",
        N * 2,
        long.value,
        expected_sum(N * 2)
    );

    // (b) A prologue publish happens on a **fresh** native entry, never once per element: the
    // continuation sub-block publishes in its own prologue exactly once — on the base case's
    // `jit_return`, which asks for no alternate (see the plain-`return` pin below) and so
    // lands on the sub-block's normal entry. Every other element's window is published by the
    // returning helper instead, which is why a per-element prologue publish would double this
    // figure between `N` and `2N`.
    assert_eq!(
        short.counters.publish_y, 1,
        "the window is published from a prologue exactly once — the one fresh entry into the \
         publishing sub-block — not once per element"
    );
    assert_eq!(
        short.counters.publish_y, long.counters.publish_y,
        "the prologue publish does not grow with the element count: {} at N={N} vs {} at N={}",
        short.counters.publish_y,
        long.counters.publish_y,
        N * 2
    );

    // (c) ... and the alternate is taken once per element, with no null-slot residual.
    assert_eq!(
        short.counters.post_publish_entries,
        u64::from(N as u32) - 1,
        "one alternate entry per returning element at N={N}"
    );
    assert_eq!(
        long.counters
            .post_publish_entries
            .saturating_sub(short.counters.post_publish_entries),
        u64::from(N as u32),
        "the alternate is taken once per element, not once per run"
    );
    assert_eq!(
        short.counters.post_publish_entries, short.counters.post_publish_requests,
        "every probe the returning transfer makes finds its alternate (no null-slot residual)"
    );

    // The window is published exactly **once per element in total**: by the returning
    // helper (which publishes the popped frame's window itself before entering the
    // alternate) for the `N - 1` elements that return through `deallocate_return`, and by the
    // sub-block's own prologue for the one element that enters it freshly.
    assert_eq!(
        short.counters.publish_y + short.counters.post_publish_entries,
        u64::from(N as u32),
        "one window publish per element in total at N={N}"
    );
    assert_eq!(
        long.counters.publish_y + long.counters.post_publish_entries,
        u64::from(N as u32) * 2,
        "one window publish per element in total at N={}",
        N * 2
    );

    // (d) Non-vacuity: the continuation sub-block is a *publishing* block with BOTH entries,
    // measured on a **cache-free** compile (a warmed `CodeCache` emits nothing, so a counter
    // read there would be zero).
    let (module, _) = sum_module();
    let code_len = module.functions[0].code.len();
    let pc = continuation_pc(&module);
    let labels = crate::interpreter::build_label_map(&module);
    counters::reset();
    counters::set_enabled(true);
    let function = compile_function(&module, &labels, 0).expect("the fixture compiles");
    counters::set_enabled(false);
    let compiled = counters::snapshot();
    assert!(
        !function.entries[pc].is_null(),
        "the continuation sub-block at pc {pc} has a normal entry"
    );
    assert!(
        !function.entries[code_len + pc].is_null(),
        "the continuation sub-block at pc {pc} has a post-publish alternate"
    );
    let published = alternate_slots(&function, code_len);
    assert_eq!(
        compiled.y_prologue_publishes as usize,
        published.len(),
        "the emitted publishing prologues are exactly the non-null alternate slots: {:?}",
        published
    );
    assert!(
        !published.is_empty(),
        "the continuation sub-block's prologue does publish, so the publish claim is not vacuous"
    );
}

/// **(c1):** a publishing block's alternate entry is its normal entry **plus the publish
/// sequence**, and that address lies strictly inside the block's own emitted extent.
///
/// The length comes from [`publish_y_len`] — derived by emitting the sequence — never from a
/// byte count copied out of the emitter's arithmetic. `emit_block_prologue` reports the
/// alternate as the buffer offset immediately past the publish it emits **first**, so
/// `post[pc] - entry[pc]` is that length for every publishing entry of a **cache-free**
/// `compile_function` (the warmed `CodeCache` re-emits nothing, so it could not carry this).
#[test]
fn the_alternate_entry_is_the_block_entry_plus_the_publish_sequence() -> Result<()> {
    let (module, _) = sum_module();
    let code_len = module.functions[0].code.len();
    let labels = crate::interpreter::build_label_map(&module);
    let function = compile_function(&module, &labels, 0)?;
    let publish = publish_y_len()?;
    let base = function.buffer.code_ptr() as usize;
    let mapping_end = base + function.buffer.len();

    // Every native **entry** this function compiled, in the one sealed mapping: the IR blocks
    // and the continuation sub-blocks are laid down contiguously, so the next entry above a
    // block's own entry is that block's exclusive end. Only the folded first half is an
    // entry address — the alternates live *inside* a block's code and so are not boundaries.
    let mut offsets: Vec<usize> = (0..code_len)
        .map(|pc| function.entries[pc] as usize)
        .filter(|offset| *offset != 0)
        .collect();
    offsets.sort_unstable();
    offsets.dedup();

    let mut checked = 0usize;
    for pc in 0..code_len {
        let entry = function.entries[pc] as usize;
        let alternate = function.entries[code_len + pc] as usize;
        if alternate == 0 {
            // Not a publishing entry; the Y-free half of the rule is pinned separately.
            continue;
        }
        assert_ne!(entry, 0, "a publishing entry at pc {pc} has a normal entry too");
        assert!(
            alternate > entry,
            "the alternate at pc {pc} is past its own entry: {alternate:#x} vs {entry:#x}"
        );
        assert_eq!(
            alternate,
            entry + publish,
            "the alternate at pc {pc} is the normal entry plus the {publish}-byte publish \
             sequence: {alternate:#x} vs {entry:#x}"
        );
        let block_end = offsets
            .iter()
            .copied()
            .find(|offset| *offset > entry)
            .unwrap_or(mapping_end);
        assert!(
            alternate < block_end,
            "the alternate at pc {pc} lies inside the block's own emitted extent \
             [{entry:#x}, {block_end:#x}), not past it: {alternate:#x}"
        );
        assert!(
            alternate < mapping_end,
            "the alternate at pc {pc} lies inside the sealed mapping"
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "the fixture must carry at least one publishing entry"
    );
    Ok(())
}

/// **(c2):** a block that touches no `Y` slot publishes nothing, so it keeps its normal entry
/// and has **no** alternate — the normal entry serves, and
/// [`EntryTables::post_publish_entry`] falls back to it.
///
/// Two readings, both over cache-free compiles:
///
/// * the fixture's own Y-free block (`sum/1`'s `test`/`move`/`return` block, whose allocation
///   answers `!prologue_publishes_y()`): `entries[pc]` is non-null and `post[pc]` is null;
/// * a function whose every block is Y-free: no entry carries an alternate at all, and the
///   compile records **zero** emitted publishing prologues.
#[test]
fn a_y_free_block_has_no_alternate_entry() -> Result<()> {
    // (1) The fixture's Y-free block — `sum/1`'s `test`/`move`/`return` block, which touches
    // no `Y` slot. (`block_start > 0` skips the entry-label block, whose code is the label
    // alone.)
    let (module, _) = sum_module();
    let code_len = module.functions[0].code.len();
    let labels = crate::interpreter::build_label_map(&module);
    let y_free_pc = module.functions[0]
        .blocks
        .iter()
        .find(|block| {
            block.start > 0
                && !allocate_registers(&module, &module.functions[0].code, block.start, block.end)
                    .prologue_publishes_y()
        })
        .map(|block| block.start)
        .expect("the fixture carries a Y-free block past the entry label");
    let function = compile_function(&module, &labels, 0)?;
    assert!(
        !function.entries[y_free_pc].is_null(),
        "the Y-free block at pc {y_free_pc} keeps its normal entry"
    );
    assert!(
        function.entries[code_len + y_free_pc].is_null(),
        "the Y-free block at pc {y_free_pc} has no alternate entry"
    );

    // (2) A whole function with no `Y` access at all: every block is Y-free, so no entry
    // carries an alternate and no prologue publishes.
    let flat = build_module(
        "post_publish_y_free",
        &[("erlang", "+", 2)],
        vec![(
            "f",
            1,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::GcBif {
                    arity: 2,
                    bif: ImportRef(0),
                    fail: NO_FAIL,
                    live: 1,
                    args: vec![Operand::X(0), Operand::Integer(1)],
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        )],
        &[("f", 1, Label(2))],
    );
    let flat_code_len = flat.functions[0].code.len();
    let start = flat.functions[0].blocks[0].start;
    let end = flat.functions[0].blocks[0].end;
    assert!(
        !allocate_registers(&flat, &flat.functions[0].code, start, end).prologue_publishes_y(),
        "a block with no Y access never publishes"
    );
    let labels = crate::interpreter::build_label_map(&flat);
    counters::reset();
    counters::set_enabled(true);
    let flat_function = compile_function(&flat, &labels, 0)?;
    counters::set_enabled(false);
    let compiled = counters::snapshot();
    assert!(
        !flat_function.entries[start].is_null(),
        "the Y-free block keeps its normal entry"
    );
    assert!(
        alternate_slots(&flat_function, flat_code_len).is_empty(),
        "no entry of a Y-free function carries an alternate: {:?}",
        alternate_slots(&flat_function, flat_code_len)
    );
    assert_eq!(
        compiled.y_prologue_publishes, 0,
        "a Y-free function emits no publishing prologue, so `y_prologue_publishes` counts \
         exactly the prologues that publish"
    );
    Ok(())
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
