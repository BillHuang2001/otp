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
//! Deopt-idempotency pins for the seven bitstring inline arms that had none.
//!
//! # The rule these rows exist to protect
//!
//! The baseline tier's `fallback` edge is **a deopt**: it runs exactly one interpreter
//! step ([`super::runtime::jit_step_one`] == `Machine::step_instruction_with_entry`),
//! which **re-executes the whole instruction from its operands**. So an inline arm must
//! either decline *before* its first observable effect, or commit fully and stay in
//! block. A guard that can reach the fallback while sitting **after** a write would let
//! the interpreter step apply that write a second time — for the position-writing
//! shapes that means the match context's `start` word advances twice, i.e. a wrong
//! value no tier comparison can catch, because both tiers would have to disagree for
//! [`super::tests::assert_same_outcome`] to fail.
//!
//! `bs_match` (`../../bs_match_deopt_tests.rs`) and `bs_create_bin`
//! (`../../bs_create_bin_tests.rs`) already carry that pin, as do the six UTF shapes
//! and `bs_match_string` (`../emit/bitstr_inline/*_tests.rs`). These are the seven
//! shapes that had no row:
//!
//! | shape | commit site |
//! |---|---|
//! | `bs_get_position` | none — it only reads |
//! | `bs_set_position` | `mov_mr(.., SUB_BITS_START_DISP, Rsi)`, after every read |
//! | `bs_test_tail2` | none — a comparison only |
//! | `bs_skip_bits2` | `mov_mr(.., SUB_BITS_START_DISP, Rcx)`, last |
//! | `bs_get_integer2` | `mov_mr(.., SUB_BITS_START_DISP, Rsi)`, after the extract |
//! | `bs_start_match3` | `dst.store_at`, after the header classification |
//! | `bs_start_match4` (label + `resume`) | `dst.store_at`, after the classification |
//!
//! # What each row asserts, in this order
//!
//! 1. **tier agreement** — [`super::tests::assert_same_outcome`] on the interpreter and
//!    the baseline tier over the same input, so a mistake *both* tiers share still
//!    fails the literal assertion that follows;
//! 2. **the literal expectation** — the returned value, or the family's exact typed
//!    error reason, so "agreed" cannot mean "both did nothing";
//! 3. **the side effect happened exactly once** — every position-writing shape is
//!    followed by a `bs_get_position` on the fail arm *and* on the success path, so a
//!    double commit shows up as `16` where the correct answer is `8`. The fail arm is
//!    the interesting one: it is precisely the path on which a mis-ordered guard would
//!    have already mutated the context;
//! 4. **the arm is still native** — a cache-free compile
//!    ([`super::bitstr_inline_tests::emitted_shapes`]) must emit this shape's arm and
//!    record **zero** static deopts for its variant. Without it, "fixing" the ordering
//!    by dropping the arm would pass every row above.
//!
//! The one shape whose *run-time* decline is unreachable from a test is
//! `bs_get_position` (its only fallback guard compares the position against
//! `MAX_SMALL`, which needs a ≥2^56-byte subject); its row pins the emission
//! instrument and tier agreement instead and says so, matching
//! `bitstr_inline_tests.rs::the_get_position_arm_emits_the_max_small_guard`.

use beam_loader::{BsGetArgs, Instr, Label, Module, Operand};
use beam_term::Atom;

use super::bitstr_inline_tests::{
    atom, bitstr_module, emitted_shapes, sentinel_block, static_deopts, visited_instances,
};
use super::emit::bitstr_inline::{
    SHAPE_GET_INTEGER2, SHAPE_GET_POSITION, SHAPE_SET_POSITION, SHAPE_SKIP_BITS2, SHAPE_START_MATCH3,
    SHAPE_START_MATCH4, SHAPE_TEST_TAIL2,
};
use super::tests::assert_same_outcome;
use crate::context::{ExecutionContext, MinimalContext};
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};
use crate::terms::{make_heap_binary, make_integer};

// ---------------------------------------------------------------------------
// Harness: two tiers, any arity, any argument vector
// ---------------------------------------------------------------------------

/// Run exported `name/arity` on both tiers with `build` supplying each side's own
/// argument vector, and hand the two outcomes to `check`.
///
/// Each run builds its **own** [`MinimalContext`] heap (a returned boxed term points
/// into it), so the comparison has to happen inside `check` — the same discipline as
/// [`super::bitstr_tests::with_both_tiers_arg`], generalised to a caller-chosen arity
/// and argument list. `build` is called once per side, so a row cannot accidentally
/// share one heap's terms between the two runs.
fn with_both_tiers_args<R>(
    module: &Module,
    name: Atom,
    arity: u32,
    build: impl Fn(&mut MinimalContext) -> Vec<beam_term::Eterm>,
    check: impl FnOnce(&beam_common::Result<RunOutcome>, &beam_common::Result<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    let args = build(&mut interpreter_ctx);
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        name,
        arity,
        &args,
        &mut interpreter_ctx,
        Tier::Interpreter,
    );

    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    let args = build(&mut baseline_ctx);
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        name,
        arity,
        &args,
        &mut baseline_ctx,
        Tier::BaselineJit,
    );
    check(&interpreter_outcome, &baseline_outcome)
}

/// The signed integer the **baseline** tier returned for `f/1` over `subject`, after
/// asserting the interpreter tier produced the observably identical outcome.
fn baseline_i64(label: &str, module: &Module, subject: &'static [u8]) -> i64 {
    let name = atom(module, "f");
    with_both_tiers_args(
        module,
        name,
        1,
        |ctx| vec![make_heap_binary(ctx, subject).expect("the subject builds")],
        |interpreter, baseline| {
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
            let returned = baseline
                .as_ref()
                .unwrap_or_else(|error| panic!("{label}: {error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("{label}: unexpected outcome {returned:?}");
            };
            beam_term::signed_value(*value)
        },
    )
}

/// **The arm is still native**: a cache-free compile emits `shape` and records no static
/// deopt for `variant`.
///
/// A tier-agreement row alone also passes for an arm that deopts every instruction, so
/// this is the assertion that stops the ordering rule returning as a silent deopt.
fn assert_still_native(label: &str, module: &Module, shape: usize, variant: &str) {
    let (emitted, snapshot) = emitted_shapes(module);
    assert!(
        emitted.contains(&shape),
        "{label}: a cache-free compile must still emit the arm (emitted {emitted:?}, wanted \
         {shape})"
    );
    assert!(
        visited_instances(&snapshot, variant) >= 1,
        "{label}: `{variant}` must be the census's variant name (visited {:?})",
        snapshot.instr_instances
    );
    assert_eq!(
        static_deopts(&snapshot, variant),
        0,
        "{label}: the instruction must be lowered, never statically deopted (deopts {:?})",
        snapshot.instr_deopts
    );
}

/// `bs_get_integer2` operand block over `ctx`.
fn get_integer(module: &Module, ctx: Operand, size: i64, live: u32, dst: Operand) -> Instr {
    let _ = module;
    Instr::BsGetInteger2(Box::new(BsGetArgs {
        fail: Label(4),
        ctx,
        live,
        size: Operand::Integer(size),
        unit: 1,
        flags: 0,
        dst,
    }))
}

/// `move -1 -> x(0); return` at `label`, i.e. [`sentinel_block`].
fn fail_sentinel(label: Label) -> Vec<Instr> {
    sentinel_block(label)
}

// ---------------------------------------------------------------------------
// 1. `bs_skip_bits2` — the commit is the *last* thing the arm emits
// ---------------------------------------------------------------------------
/// A module whose body is
///
/// ```text
///   Label(1) Label(2)
///   bs_start_match3 fail=Label(5) x(0) -> x(1)
///   bs_skip_bits2 fail=Label(4) x(1) 8 1
///   bs_get_position x(1) -> x(0)        ; the success path answers the position
///   return
///   Label(4): bs_get_position x(1) -> x(0); return   ; the fail arm answers the position
///   Label(5): move -1 -> x(0); return
/// ```
///
/// Both arms answer the same observable — the context's `start` word — so the row reads
/// the side effect whether the skip committed or declined.
fn module_skip_bits2_position() -> Module {
    bitstr_module("deopt_skip_bits2", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(5),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsSkipBits2 {
                fail: Label(4),
                ctx: Operand::X(1),
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
            },
            Instr::BsGetPosition {
                ctx: Operand::X(1),
                dst: Operand::X(0),
                live: 2,
            },
            Instr::Return,
            Instr::Label(Label(4)),
            Instr::BsGetPosition {
                ctx: Operand::X(1),
                dst: Operand::X(0),
                live: 2,
            },
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(5)));
        code
    })
}

/// **`bs_skip_bits2` commits the advanced position exactly once.**
///
/// * **in-block commit** — `skip 8` over a two-byte subject succeeds and leaves
///   `start == 8` (a double application would report `16`).
/// * **decline** — `skip 16` over a **one-byte** subject takes the fail label
///   (`next > end`), and the fail arm reports `start == 0`: the emitted
///   `cmp_rr`/`jcc(Cond::A, fail_arm)` pair runs *before* the `mov_mr` that writes
///   `start`, so the declined instruction mutated nothing. This is the row that fails if
///   the commit is hoisted above the range check.
#[test]
fn bs_skip_bits2_commits_the_position_exactly_once() {
    let module = module_skip_bits2_position();
    // In-block commit: 8 bits skipped, so the position is 8.
    assert_eq!(
        baseline_i64("bs_skip_bits2 commit", &module, &[7, 9]),
        8,
        "a successful `skip 8` must leave the context at 8 — an arm that applied the skip \
         twice would answer 16"
    );
    // Decline: 16 bits cannot be skipped over an 8-bit window.
    assert_eq!(
        baseline_i64("bs_skip_bits2 decline", &module, &[]),
        0,
        "a `skip 8` over an empty window must take the fail label with the \
         context untouched (a commit before the range check would answer 16)"
    );
    assert_still_native(
        "bs_skip_bits2",
        &module,
        SHAPE_SKIP_BITS2,
        "BsSkipBits2",
    );
}

/// **`bs_skip_bits2`'s non-context decline is a fallback and raises the family's own
/// error, unchanged by the round trip.**
///
/// The subject is a real bitstring but the *operand* is not a match context (the arm's
/// `emit_ctx_guard` rejects it), so the arm jumps to `fallback`, one interpreter step
/// re-executes the instruction, and the family's `read_ctx` raises. Because the
/// instruction committed nothing before that guard, the re-execution sees the same
/// operand — the pinned assertion is that both tiers raise the family's exact wording
/// rather than one of them having half-applied a skip.
#[test]
fn bs_skip_bits2_declines_a_non_context_operand_before_any_effect() {
    let module = bitstr_module("deopt_skip_bits2_nonctx", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            // x(1) is the raw bitstring argument, *not* a match context.
            Instr::Move {
                src: Operand::X(0),
                dst: Operand::X(1),
            },
            Instr::BsSkipBits2 {
                fail: Label(3),
                ctx: Operand::X(1),
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(3)));
        code
    });
    let name = atom(&module, "f");
    with_both_tiers_args(
        &module,
        name,
        1,
        |ctx| vec![make_heap_binary(ctx, &[7, 9]).expect("the subject builds")],
        |interpreter, baseline| {
            assert_same_outcome(
                "bs_skip_bits2 non-context",
                interpreter,
                baseline,
                &module.atom_table,
            );
            for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
                let error = match outcome {
                    Err(error) => error,
                    Ok(outcome) => {
                        panic!("{tier}: a non-context operand must raise, got {outcome:?}")
                    }
                };
                assert_eq!(
                    super::tests::error_reason(error),
                    "unsupported_opcode: bs_skip_bits2: operand is not a match context",
                    "{tier}: the fallback's re-execution must surface the family's own error, \
                     with nothing applied by the arm"
                );
            }
        },
    );
}

// ---------------------------------------------------------------------------
// 2. `bs_get_integer2` — the extract's fallback sits *above* the commit
// ---------------------------------------------------------------------------
/// A module that reads an `size`-bit big-endian field and answers the resulting
/// position, with the fail arm answering the position it saw.
fn module_get_integer2_position(size: i64) -> Module {
    bitstr_module("deopt_get_integer2", &[], 1, move |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(5),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            get_integer(&Module::default(), Operand::X(1), size, 2, Operand::X(2)),
            Instr::BsGetPosition {
                ctx: Operand::X(1),
                dst: Operand::X(0),
                live: 3,
            },
            Instr::Return,
            Instr::Label(Label(4)),
            Instr::BsGetPosition {
                ctx: Operand::X(1),
                dst: Operand::X(0),
                live: 3,
            },
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(5)));
        code
    })
}

/// **`bs_get_integer2` advances the context by exactly one field.**
///
/// * **in-block commit** — a 16-bit read over `[1, 2]` leaves `start == 16`.
/// * **decline** — the same read over a **one-byte** subject fails the range test
///   (`nbits > end - start`) and the fail arm reports `start == 0`, i.e. the arm's
///   `cmp`/`jcc(B, fail_arm)` pair precedes the `mov_mr` that commits `start`.
#[test]
fn bs_get_integer2_commits_the_position_exactly_once() {
    let module = module_get_integer2_position(16);
    assert_eq!(
        baseline_i64("bs_get_integer2 commit", &module, &[1, 2]),
        16,
        "a 16-bit field must leave the context at 16 — a double application would answer 32"
    );
    assert_eq!(
        baseline_i64("bs_get_integer2 decline", &module, &[1]),
        0,
        "a 16-bit field over an 8-bit window must fail with the context untouched (a commit \
         before the range check would answer 16)"
    );
    assert_still_native(
        "bs_get_integer2",
        &module,
        SHAPE_GET_INTEGER2,
        "BsGetInteger2",
    );
}

/// **A 48-bit `bs_get_integer2` really does take the arm's `fallback` and is still
/// idempotent.**
///
/// [`super::emit::bitstr_inline::emit_bit_extract`] refuses `nbits > 32` with an
/// unconditional jump to `fallback`, so the instruction is re-executed by one
/// interpreter step. The commit sits *after* the extract, so the re-execution still sees
/// the whole window: the value is the full 48 bits and the position afterwards is
/// exactly `48`, never `96`. Both are asserted, because a value-only row would pass if
/// the context happened to be re-read from the start.
#[test]
fn a_wide_integer_field_deopts_and_advances_once() {
    let module = module_get_integer2_position(48);
    let subject: &'static [u8] = &[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
    assert_eq!(
        baseline_i64("48-bit field", &module, subject),
        48,
        "the 48-bit read must advance the context by exactly 48 bits through the fallback"
    );
    // The value itself, read separately so a wrong extraction cannot hide behind the
    // position assertion.
    let value_module = bitstr_module("deopt_wide_integer_value", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(5),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            get_integer(&Module::default(), Operand::X(1), 48, 2, Operand::X(0)),
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(5)));
        code
    });
    assert_eq!(
        baseline_i64("48-bit value", &value_module, subject),
        0x1234_5678_9abc,
        "the fallback's re-execution must read the field from the untouched start"
    );
}

// ---------------------------------------------------------------------------
// 3. `bs_test_tail2` — a comparison only, and the fail transfer writes nothing
// ---------------------------------------------------------------------------
/// A module that reads 16 bits, tests for an empty tail, and answers the context
/// position; the fail arms answer the position they saw (and a foreign-partition
/// sentinel for the start-match failure).
fn module_test_tail2_position() -> Module {
    bitstr_module("deopt_test_tail2", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(5),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            get_integer(&Module::default(), Operand::X(1), 16, 2, Operand::X(2)),
            Instr::BsTestTail2 {
                fail: Label(4),
                ctx: Operand::X(1),
                bits: 0,
            },
            Instr::BsGetPosition {
                ctx: Operand::X(1),
                dst: Operand::X(0),
                live: 3,
            },
            Instr::Return,
            Instr::Label(Label(4)),
            Instr::BsGetPosition {
                ctx: Operand::X(1),
                dst: Operand::X(0),
                live: 3,
            },
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(5)));
        code
    })
}

/// **`bs_test_tail2` writes nothing on either path** — the match path and the mismatch
/// path both answer the position the preceding `bs_get_integer2` committed (`16`).
///
/// The shape has no commit of its own, so the property to pin is the negative one: the
/// fail transfer must not consume the tail. A lowering that advanced the context to
/// "make the tail empty" would answer anything but `16` on the mismatch row.
#[test]
fn bs_test_tail2_writes_nothing_on_either_path() {
    let module = module_test_tail2_position();
    assert_eq!(
        baseline_i64("bs_test_tail2 match", &module, &[1, 2]),
        16,
        "an empty tail must accept and leave the context where the field read left it"
    );
    assert_eq!(
        baseline_i64("bs_test_tail2 mismatch", &module, &[1, 2, 3]),
        16,
        "a non-empty tail must take the fail arm without consuming the remaining bits"
    );
    assert_still_native(
        "bs_test_tail2",
        &module,
        SHAPE_TEST_TAIL2,
        "BsTestTail2",
    );
}

// ---------------------------------------------------------------------------
// 4. `bs_set_position` — one commit, after every read
// ---------------------------------------------------------------------------
/// A two-argument module `f(Bin, Pos)`: start a match on `Bin`, rewind the context to
/// `Pos`, read one byte, and answer the resulting position.
fn module_set_position_arity2() -> Module {
    bitstr_module("deopt_set_position", &[], 2, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(5),
                bin: Operand::X(0),
                live: 2,
                dst: Operand::X(2),
            },
            // Move the context off the start so the rewind is observable.
            get_integer(&Module::default(), Operand::X(2), 16, 3, Operand::X(3)),
            Instr::BsSetPosition {
                ctx: Operand::X(2),
                pos: Operand::X(1),
            },
            Instr::BsGetPosition {
                ctx: Operand::X(2),
                dst: Operand::X(0),
                live: 4,
            },
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(5)));
        code
    })
}

/// **`bs_set_position` applies the rewind exactly once, and a negative position declines
/// to the family's own error with the context untouched.**
///
/// * **in-block commit** — the context is advanced to 16 by a field read, then rewound to
///   `4`; the answer is `4`. A double application would also be `4`, so the *observable*
///   that separates the two is the negative row below (which must not have applied
///   anything) plus the `0` rewind, whose double application is still `0`; the real
///   discriminator is the family path: the arm's guards all precede the single
///   `mov_mr`, and the row that proves it is that a declined instruction leaves `start`
///   where the field read put it.
/// * **decline** — a **negative small** position trips `jcc(Cond::S, fallback)`, one
///   interpreter step re-executes the instruction, and the family raises
///   `bs_set_position: negative bit position`. Both tiers must report that exact
///   wording; a commit hoisted above the sign guard would have rewritten `start` with a
///   nonsense value before the raise, which the following row would expose as a
///   differing position.
#[test]
fn bs_set_position_commits_once_and_declines_a_negative_position() {
    let module = module_set_position_arity2();
    let name = atom(&module, "f");
    // In-block: rewind to bit 4 after a 16-bit read.
    with_both_tiers_args(
        &module,
        name,
        2,
        |ctx| {
            vec![
                make_heap_binary(ctx, &[7, 9, 11]).expect("the subject builds"),
                make_integer(ctx, 4).expect("the position builds"),
            ]
        },
        |interpreter, baseline| {
            assert_same_outcome("bs_set_position commit", interpreter, baseline, &module.atom_table);
            let returned = baseline.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("unexpected outcome {returned:?}");
            };
            assert_eq!(
                beam_term::signed_value(*value),
                4,
                "the rewind to bit 4 must leave the context at exactly 4"
            );
        },
    );
    // Decline: a negative small position is not a bit offset.
    with_both_tiers_args(
        &module,
        name,
        2,
        |ctx| {
            vec![
                make_heap_binary(ctx, &[7, 9, 11]).expect("the subject builds"),
                make_integer(ctx, -5).expect("the position builds"),
            ]
        },
        |interpreter, baseline| {
            assert_same_outcome(
                "bs_set_position negative",
                interpreter,
                baseline,
                &module.atom_table,
            );
            for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
                let error = match outcome {
                    Err(error) => error,
                    Ok(outcome) => panic!("{tier}: a negative position must raise, got {outcome:?}"),
                };
                assert_eq!(
                    super::tests::error_reason(error),
                    "unsupported_opcode: bs_set_position: negative bit position",
                    "{tier}: the declined rewind must surface the family's own error, not a \
                     fabricated position"
                );
            }
        },
    );
    assert_still_native(
        "bs_set_position",
        &module,
        SHAPE_SET_POSITION,
        "BsSetPosition",
    );
}

// ---------------------------------------------------------------------------
// 5. `bs_start_match3` — the destination store is the arm's only write
// ---------------------------------------------------------------------------
/// A module that starts a match on `x(0)` into `x(1)` and reads one byte from it.
fn module_start_match3_value() -> Module {
    bitstr_module("deopt_start_match3", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            get_integer(&Module::default(), Operand::X(1), 8, 2, Operand::X(0)),
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(3)));
        code
    })
}

/// **`bs_start_match3` commits its destination only on the hit path.**
///
/// * **in-block commit** — a one-byte bitstring yields the fresh context and the byte.
/// * **decline** — a **non-bitstring** subject fails the header classification, takes
///   the fail label, and answers the sentinel: the arm declined before writing `x(1)`,
///   so the fallback (or the fail transfer) never sees a half-built context. The
///   already-a-context subject exercises the other route — `enter_match`'s reuse, which
///   the arm sends to `fallback` — and must still agree.
#[test]
fn bs_start_match3_commits_its_destination_once() {
    let module = module_start_match3_value();
    assert_eq!(
        baseline_i64("bs_start_match3 commit", &module, &[5]),
        5,
        "a fresh match context over `<<5>>` must read back the byte"
    );
    // Decline: a non-bitstring subject (a small integer) must take the fail label.
    let name = atom(&module, "f");
    with_both_tiers_args(
        &module,
        name,
        1,
        |ctx| vec![make_integer(ctx, 5).expect("the argument builds")],
        |interpreter, baseline| {
            assert_same_outcome(
                "bs_start_match3 non-bitstring",
                interpreter,
                baseline,
                &module.atom_table,
            );
            let returned = baseline.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("unexpected outcome {returned:?}");
            };
            assert_eq!(
                beam_term::signed_value(*value),
                -1,
                "a non-bitstring subject must take the fail label, never fabricate a context"
            );
        },
    );
    assert_still_native(
        "bs_start_match3",
        &module,
        SHAPE_START_MATCH3,
        "BsStartMatch3",
    );
}

// ---------------------------------------------------------------------------
// 6. `bs_start_match4` (label form) — the same classification, a different fail code
// ---------------------------------------------------------------------------
/// A module that starts a match, re-binds it through a second `bs_start_match4` with a
/// real fail **label**, and reads one byte from the rebound context.
fn module_start_match4_label_value() -> Module {
    bitstr_module("deopt_start_match4_label", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(5),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsStartMatch4 {
                fail: Operand::Label(Label(4)),
                live: 2,
                src: Operand::X(1),
                dst: Operand::X(2),
            },
            get_integer(&Module::default(), Operand::X(2), 8, 3, Operand::X(0)),
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(4)));
        code.extend(fail_sentinel(Label(5)));
        code
    })
}

/// A module whose `bs_start_match4` label-form source is **not** a match context (the
/// raw bitstring), so the classification misses and the fail label runs.
fn module_start_match4_label_miss() -> Module {
    bitstr_module("deopt_start_match4_label_miss", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch4 {
                fail: Operand::Label(Label(4)),
                live: 1,
                src: Operand::X(0),
                dst: Operand::X(1),
            },
            get_integer(&Module::default(), Operand::X(1), 8, 2, Operand::X(0)),
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(4)));
        code
    })
}

/// **`bs_start_match4`'s label form stores the rebound context on the hit path and
/// declines on a miss, writing nothing.**
#[test]
fn bs_start_match4_label_form_commits_once_and_declines_a_miss() {
    let module = module_start_match4_label_value();
    assert_eq!(
        baseline_i64("bs_start_match4 label commit", &module, &[5]),
        5,
        "re-binding an existing match context must reuse it and still read the byte"
    );
    assert_still_native(
        "bs_start_match4 (label)",
        &module,
        SHAPE_START_MATCH4,
        "BsStartMatch4",
    );
    // The miss: a heap-bits bitstring is not a match context.
    let miss = module_start_match4_label_miss();
    let miss_name = atom(&miss, "f");
    with_both_tiers_args(
        &miss,
        miss_name,
        1,
        |ctx| vec![make_integer(ctx, 5).expect("the argument builds")],
        |interpreter, baseline| {
            assert_same_outcome(
                "bs_start_match4 label miss",
                interpreter,
                baseline,
                &miss.atom_table,
            );
            let returned = baseline.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("unexpected outcome {returned:?}");
            };
            assert_eq!(
                beam_term::signed_value(*value),
                -1,
                "a source that is neither a bitstring nor a match context must take the fail label"
            );
        },
    );
}

// ---------------------------------------------------------------------------
// 7. `bs_start_match4` (resume form) — the guard is `HEADER_SUB_BITS` alone
// ---------------------------------------------------------------------------
/// A module whose `bs_start_match4` carries the **`resume`** atom fail code, so the arm
/// takes [`super::emit::bitstr_inline::MatchFail::Resume`]'s branch.
fn module_start_match4_resume_value() -> Module {
    bitstr_module("deopt_start_match4_resume", &["resume"], 1, |module| {
        let resume = Operand::Atom(atom(module, "resume"));
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::BsStartMatch4 {
                fail: resume,
                live: 2,
                src: Operand::X(1),
                dst: Operand::X(2),
            },
            get_integer(&Module::default(), Operand::X(2), 8, 3, Operand::X(0)),
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(3)));
        code
    })
}

/// The `resume` form over a source that is **not** a context, so the arm's single
/// classified guard rejects and the family's `resume` handling runs one interpreter step.
fn module_start_match4_resume_miss() -> Module {
    bitstr_module("deopt_start_match4_resume_miss", &["resume"], 1, |module| {
        let resume = Operand::Atom(atom(module, "resume"));
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch4 {
                fail: resume,
                live: 1,
                src: Operand::X(0),
                dst: Operand::X(1),
            },
            get_integer(&Module::default(), Operand::X(1), 8, 2, Operand::X(0)),
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(3)));
        code
    })
}

/// **`bs_start_match4`'s `resume` form commits the reused context once and declines
/// (one interpreter step) when the source is not a context.**
///
/// The `resume` form's only run-time guard is the `HEADER_SUB_BITS` test, and it is the
/// arm's *last* decision before `dst.store_at` — so a rejection must leave the
/// destination unwritten and the family's own `fail_step` (a `resume` request over a
/// non-context is a match failure, not a reuse) must be what answers.
#[test]
fn bs_start_match4_resume_form_commits_once_and_declines() {
    let module = module_start_match4_resume_value();
    assert_eq!(
        baseline_i64("bs_start_match4 resume commit", &module, &[5]),
        5,
        "the resume form must reuse the existing context and still read the byte"
    );
    assert_still_native(
        "bs_start_match4 (resume)",
        &module,
        SHAPE_START_MATCH4,
        "BsStartMatch4",
    );
    let miss = module_start_match4_resume_miss();
    let miss_name = atom(&miss, "f");
    with_both_tiers_args(
        &miss,
        miss_name,
        1,
        |ctx| vec![make_heap_binary(ctx, &[5]).expect("the subject builds")],
        |interpreter, baseline| {
            assert_same_outcome(
                "bs_start_match4 resume decline",
                interpreter,
                baseline,
                &miss.atom_table,
            );
            for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
                let error = match outcome {
                    Err(error) => error,
                    Ok(outcome) => {
                        panic!("{tier}: a `resume` over a non-context must fail, got {outcome:?}")
                    }
                };
                assert_eq!(
                    super::tests::error_reason(error),
                    "BadMatch",
                    "{tier}: the resumed shape must fail through the family after the arm \
                     declined with the destination untouched"
                );
            }
        },
    );
}

// ---------------------------------------------------------------------------
// 8. `bs_get_position` — a read-only shape whose one guard needs an out-of-range start
// ---------------------------------------------------------------------------
/// **`bs_get_position` is read-only, so it is trivially re-execution safe.**
///
/// The arm's single decline edge compares the context position against `MAX_SMALL`; no
/// *program* can reach it (see
/// [`bs_get_position_declines_a_position_beyond_the_small_range`], which supplies the one
/// input that does), so this row pins the property the idempotency rule actually needs for
/// a read-only shape: the instruction writes nothing, and its answer is the position the
/// *preceding* instruction committed — never one it moved.
///
/// The row therefore asserts the value after each of the two preceding shapes whose
/// commits it reads (a field read, and a skip), on both tiers.
#[test]
fn bs_get_position_is_read_only() {
    let module = bitstr_module("deopt_get_position", &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(5),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            get_integer(&Module::default(), Operand::X(1), 8, 2, Operand::X(2)),
            Instr::BsSkipBits2 {
                fail: Label(4),
                ctx: Operand::X(1),
                size: Operand::Integer(8),
                unit: 1,
                flags: 0,
            },
            Instr::BsGetPosition {
                ctx: Operand::X(1),
                dst: Operand::X(0),
                live: 3,
            },
            Instr::Return,
        ];
        code.extend(fail_sentinel(Label(4)));
        code.extend(fail_sentinel(Label(5)));
        code
    });
    // One 8-bit read plus one 8-bit skip: the position is 16, and `bs_get_position`
    // itself contributes nothing to it.
    assert_eq!(
        baseline_i64("bs_get_position", &module, &[7, 9]),
        16,
        "`bs_get_position` must report the position the commit left (8 + 8), never one it \
         moved itself"
    );
    assert_still_native(
        "bs_get_position",
        &module,
        SHAPE_GET_POSITION,
        "BsGetPosition",
    );
}

/// A match-context term over `subject` whose `start` word is `start` bits, built in `ctx`'s
/// own arena with `beam_term`'s `ErlSubBits` writer — the `[HEADER_SUB_BITS, base_flags,
/// start, end, orig]` block a `bs_start_match3` would have produced, with a position no
/// match advance can reach.
fn context_at(ctx: &mut MinimalContext, subject: &[u8], start: usize) -> beam_term::Eterm {
    let orig = make_heap_binary(ctx, subject).expect("the subject builds");
    let hp = ctx
        .alloc_words(5)
        .expect("the context block reserves in the arena");
    // SAFETY: `alloc_words` handed back five exclusively-owned, writable words in a
    // pointer-stable arena that outlives the run (a `MinimalContext` never moves or
    // reclaims its regions), and `beam_term`'s writer fills all five. `orig` is a live heap
    // binary in the same arena and `end` is that binary's own bit size, so the block is a
    // well-formed match context in every word but the deliberately out-of-range `start`.
    unsafe {
        beam_term::bitstring::write_sub_bits(
            hp,
            beam_term::Eterm::from_raw(0),
            beam_term::Eterm::from_raw(start),
            beam_term::Eterm::from_raw(subject.len() * 8),
            orig,
        )
    }
}

/// **`bs_get_position`'s `MAX_SMALL` guard declines, and the decline is idempotent.**
///
/// No program can reach the guard: every writer of a context's `start` is bounded by
/// `MAX_SMALL` (`bs_set_position` accepts a non-negative small — at most `MAX_SMALL` — or a
/// context whose own start is bounded the same way), and a match advance is bounded by
/// `end`, i.e. by the subject's real bit length. The row therefore supplies the one input
/// that does reach it: [`context_at`] builds a match context over a real two-byte subject
/// whose `start` is `2^60`, one bit past the 60-bit immediate range.
///
/// The two routes answer different observables:
///
/// * **guard intact** — the arm falls back, one interpreter step publishes the family's
///   whole-context snapshot (a context carrying `2^60`), `bs_set_position` rewinds to it,
///   and the 8-bit read then has no window left, so it takes its fail label and the module
///   answers `42`;
/// * **guard removed** — the arm would `shl` `2^60` by 4 (wrapping to `0`) and publish
///   `make_small(0)`, the rewind would land at bit 0, and the same read would answer the
///   subject's first byte, `7`.
#[test]
fn bs_get_position_declines_a_position_beyond_the_small_range() {
    const HUGE: usize = 1 << 60;
    let module = bitstr_module("deopt_get_position_max_small", &[], 1, |_| {
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsGetPosition {
                ctx: Operand::X(0),
                dst: Operand::X(1),
                live: 2,
            },
            Instr::BsSetPosition {
                ctx: Operand::X(0),
                pos: Operand::X(1),
            },
            get_integer(&Module::default(), Operand::X(0), 8, 1, Operand::X(0)),
            Instr::Return,
            Instr::Label(Label(4)),
            Instr::Move {
                src: Operand::Integer(42),
                dst: Operand::X(0),
            },
            Instr::Return,
        ]
    });
    let name = atom(&module, "f");
    with_both_tiers_args(
        &module,
        name,
        1,
        |ctx| vec![context_at(ctx, &[7, 9], HUGE)],
        |interpreter, baseline| {
            assert_same_outcome(
                "bs_get_position MAX_SMALL",
                interpreter,
                baseline,
                &module.atom_table,
            );
            let returned = baseline
                .as_ref()
                .unwrap_or_else(|error| panic!("bs_get_position MAX_SMALL: {error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("bs_get_position MAX_SMALL: unexpected outcome {returned:?}");
            };
            assert_eq!(
                beam_term::signed_value(*value),
                42,
                "a position beyond `MAX_SMALL` must take the arm's `fallback`: the snapshot \
                 the family publishes rewinds the context to 2^60, so the 8-bit read has no \
                 window left and takes its fail label (an arm that published \
                 `make_small(2^60 << 4)` would answer the subject's first byte instead)"
            );
        },
    );
    assert_still_native(
        "bs_get_position (MAX_SMALL)",
        &module,
        SHAPE_GET_POSITION,
        "BsGetPosition",
    );
}
