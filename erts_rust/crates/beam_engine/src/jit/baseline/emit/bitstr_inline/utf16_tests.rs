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

//! The `bs_skip_utf16` arm's own suite: the BMP / surrogate-pair rows, the **four** malformed
//! characters, the two over-read guards, the legitimate-deopt idempotency pin, the compile-time
//! refusals and the live `erl` oracle cross-check.
//!
//! Every row drives the **same** input through [`Tier::Interpreter`] and [`Tier::BaselineJit`],
//! asserts the two agree under `beam_term::cmp`, and then asserts *what* each tier answered — so
//! an arm that produced a plausible-but-wrong position cannot hide behind two tiers that share
//! it.
//!
//! The fixture returns the context's **position** after the skip, which is the one number that
//! makes every question this arm can get wrong observable at once:
//!
//! | row | what the emitted sequence does | proof |
//! |---|---|---|
//! | `U+0041` (`00 41`) | the first word is outside the surrogate range, so the BMP path reaches the shared commit with `len = 16` | the position is `16` |
//! | `U+1F600` (`D8 3D DE 00`) | the first word is a **high** surrogate, so the pair path reads a second word and reaches the same commit with `len = 32` | the position is `32` |
//! | `D7FF` / `E000` | the two code points immediately outside the surrogate range are BMP characters | `16` |
//! | a lone **low** surrogate (`DE 00`) | the `w1 > 0xDBFF` test takes the **fail label** | the sentinel `-1`, never a trap |
//! | a high surrogate + a non-low word (`D8 3D 00 41`) | the `w2 - 0xDC00 <= 0x3FF` range test takes the fail label | `-1` |
//! | a high surrogate with exactly 16 bits left (`D8 3D`) | the pair-window test `end - start < 32` takes the fail label | `-1` |
//! | one byte, or nothing at all | the saturating `remaining < 16` check takes the fail label | `-1` |
//! | `00 41` as the **whole** subject | a BMP character at the very end is *not* forced to deopt by the pair's guards | `16` |
//! | a non-byte-aligned `start` | the `test <start>, 7` guard takes `fallback` | the family answers, the character consumed **exactly once** |
//! | a `Y`-slot context | the guarded frame-window load rejects an out-of-frame slot | both tiers raise the interpreter's own error |
//! | a `NO_FAIL` fail code | the shape predicate refuses, **zero bytes** emitted | the family raises `{badmatch, _}` on both tiers |
//! | `BSF_LITTLE` / `BSF_NATIVE` | the shape predicate refuses, **zero bytes** emitted | both tiers agree with the family's byte-swapping reader |
//!
//! The arm's emitted sequence is **not** proved non-vacuously by a cross-tier comparison alone,
//! which also passes for an implementation that deopts every instruction (a deopt is executed by
//! the interpreter on *both* sides): [`assert_emitted`] adds the cache-free compile's `inlined`
//! counter, its census variant name and zero static deopts.

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_common::Result as BeamResult;
use beam_loader::{Instr, Label, Module, Operand};
use beam_term::{Atom, Eterm};

use super::{skip_utf16_shape, SHAPE_SKIP_UTF16};
use crate::context::MinimalContext;
use crate::interpreter::build_label_map;
use crate::jit::baseline::bitstr::bitstr_calls;
use crate::jit::baseline::bitstr_inline_tests::{
    bitstr_module, emitted_shapes_for, sentinel_block, static_deopts, visited_instances,
};
use crate::jit::baseline::tests::assert_same_outcome;
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome, MAX_REGISTER};
use crate::terms::make_heap_binary;

// ---------------------------------------------------------------------------
// The subjects
// ---------------------------------------------------------------------------

/// `U+0041` (a BMP scalar) as the two big-endian bytes of a UTF-16 word, plus a tail so the
/// first word's readable-bounds guard has slack.
const BMP: &[u8] = &[0x00, 0x41, b'X', b'X'];

/// `U+0041` as the **whole** subject: exactly two bytes, so the first word's guard is tight and
/// the pair's guards must not be consulted at all.
const BMP_EXACT: &[u8] = &[0x00, 0x41];

/// `U+1F600` (an astral scalar, U+1F600 GRINNING FACE) as the surrogate pair `D83D DE00`, plus a
/// tail.
const ASTRAL: &[u8] = &[0xD8, 0x3D, 0xDE, 0x00, b'X', b'X'];

/// The same pair as the **whole** subject: exactly four bytes, so both over-read guards are
/// tight and the pair commits `32` on the last available byte.
const ASTRAL_EXACT: &[u8] = &[0xD8, 0x3D, 0xDE, 0x00];

/// `U+D7FF` — the last code point **below** the surrogate range: a BMP character.
const BELOW_SURROGATES: &[u8] = &[0xD7, 0xFF, b'X'];

/// `U+E000` — the first code point **above** the surrogate range: a BMP character.
const ABOVE_SURROGATES: &[u8] = &[0xE0, 0x00, b'X'];

/// `0xDE00` — a **lone low** surrogate, which may not start a character.
const LONE_LOW: &[u8] = &[0xDE, 0x00, b'X', b'X'];

/// A **high** surrogate followed by a word that is **not** a low surrogate (`0x0041`).
const HIGH_THEN_NON_LOW: &[u8] = &[0xD8, 0x3D, 0x00, 0x41];

/// A high surrogate with exactly **16** bits left, i.e. no room for the pair.
const HIGH_ALONE: &[u8] = &[0xD8, 0x3D];

/// One byte: fewer than 16 bits remain, so the character is truncated.
const TRUNCATED: &[u8] = &[0x00];

/// No bytes at all.
const EMPTY: &[u8] = &[];

/// The position the commit leaves behind after one BMP character.
const AFTER_BMP: i64 = 16;

/// The position the commit leaves behind after one surrogate pair.
const AFTER_PAIR: i64 = 32;

/// The sentinel [`sentinel_block`] answers when the fail label is taken.
const FAIL: i64 = -1;

// ---------------------------------------------------------------------------
// The modules under test
// ---------------------------------------------------------------------------

/// `f/1` = `bs_start_match3` [`bs_skip_bits2`] `-> bs_skip_utf16 -> bs_get_position`, with the
/// fail label answering [`sentinel_block`]'s `-1`.
///
/// The success answer is the context's **position**, so "which width did the commit advance by"
/// (`16` vs `32`), "did it run at all" and "did it run **once**" are all observable through one
/// returned number, and "did the fail branch run" is the distinguishable `-1`.
fn skip_module(name: &str, skip_bits: Option<u32>, flags: u32, fail: Label) -> Module {
    bitstr_module(name, &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
        ];
        if let Some(skip) = skip_bits {
            code.push(Instr::BsSkipBits2 {
                fail: Label(3),
                ctx: Operand::X(1),
                size: Operand::Integer(i64::from(skip)),
                unit: 1,
                flags: 0,
            });
        }
        code.push(Instr::BsSkipUtf16 {
            fail,
            ctx: Operand::X(1),
            live: 2,
            flags,
        });
        code.push(Instr::BsGetPosition {
            ctx: Operand::X(1),
            dst: Operand::X(0),
            live: 2,
        });
        code.push(Instr::Return);
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// The standard module: a plain big-endian field, a real fail label, no pre-skip.
fn plain_skip_module(name: &str) -> Module {
    skip_module(name, None, 0, Label(3))
}

/// `f/1` with the match context copied into a **`Y` slot** before the skip, so the arm's guarded
/// frame-window load is what feeds the sequence. `y_slot` is the slot index; an index past the
/// one-slot frame is the row that must reject to the fallback.
fn y_skip_module(name: &str, y_slot: u32) -> Module {
    bitstr_module(name, &[], 1, |_| {
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::Allocate {
                stack_need: beam_loader::HeapNeed::Words(1),
                live: 2,
            },
            Instr::BsStartMatch3 {
                fail: Label(3),
                bin: Operand::X(0),
                live: 1,
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::Y(0),
            },
            Instr::BsSkipUtf16 {
                fail: Label(3),
                ctx: Operand::Y(y_slot),
                live: 2,
                flags: 0,
            },
            Instr::BsGetPosition {
                ctx: Operand::Y(y_slot),
                dst: Operand::X(0),
                live: 2,
            },
            Instr::Deallocate(1),
            Instr::Return,
        ];
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// The `f/1` export atom of every module above (interned by [`bitstr_module`]).
fn f(module: &Module) -> Atom {
    module
        .atom_table
        .lookup("f")
        .unwrap_or_else(|| panic!("`f` must be interned"))
}

// ---------------------------------------------------------------------------
// The runner and the assertions
// ---------------------------------------------------------------------------

/// Run exported `f/1` on both tiers with `build` producing the argument **per side** (each side
/// owns its own heap).
fn with_both_tiers_built<R>(
    module: &Module,
    build: impl Fn(&mut MinimalContext) -> Eterm,
    check: impl FnOnce(&BeamResult<RunOutcome>, &BeamResult<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    let value = build(&mut interpreter_ctx);
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        f(module),
        1,
        &[value],
        &mut interpreter_ctx,
        Tier::Interpreter,
    );

    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    let value = build(&mut baseline_ctx);
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        f(module),
        1,
        &[value],
        &mut baseline_ctx,
        Tier::BaselineJit,
    );
    check(&interpreter_outcome, &baseline_outcome)
}

/// [`with_both_tiers_built`] over an **on-heap** binary subject.
fn with_both_tiers_bytes<R>(
    module: &Module,
    subject: &[u8],
    check: impl FnOnce(&BeamResult<RunOutcome>, &BeamResult<RunOutcome>) -> R,
) -> R {
    with_both_tiers_built(
        module,
        |ctx| make_heap_binary(ctx, subject).expect("the subject builds"),
        check,
    )
}

/// The `i64` a returned small integer holds.
fn returned_i64(label: &str, outcome: &BeamResult<RunOutcome>) -> i64 {
    let returned = outcome
        .as_ref()
        .unwrap_or_else(|error| panic!("{label}: `f/1` failed: {error:?}"));
    let RunOutcome::Returned(value) = returned else {
        panic!("{label}: unexpected outcome {returned:?}");
    };
    beam_term::signed_value(*value)
}

/// **The emitted-arm instrument for one module.**
///
/// A cross-tier comparison alone is vacuous for an inline arm. This is the non-vacuous half: a
/// cache-free compile must move the shape's `inlined` counter, visit the census's variant name,
/// and record **zero** static deopts for it.
fn assert_shape_emitted(label: &str, module: &Module, shape: usize, variant: &str) {
    let (emitted, snapshot) = emitted_shapes_for(module, 0);
    println!(
        "{label}: inlined({variant}) emitted={} variant_instances={} static_deopts={}",
        emitted.contains(&shape),
        visited_instances(&snapshot, variant),
        static_deopts(&snapshot, variant),
    );
    assert!(
        emitted.contains(&shape),
        "{label}: the cache-free compile must emit the `{variant}` arm \
         (emitted {emitted:?}, wanted {shape})"
    );
    assert!(
        visited_instances(&snapshot, variant) >= 1,
        "{label}: `{variant}` must be the census's variant name, else the deopt assertion \
         below is vacuous (visited {:?})",
        snapshot.instr_instances,
    );
    assert_eq!(
        static_deopts(&snapshot, variant),
        0,
        "{label}: the arm must lower the instruction, not statically deopt it ({:?})",
        snapshot.instr_deopts,
    );
}

/// [`assert_shape_emitted`] for the UTF-16 arm.
fn assert_emitted(label: &str, module: &Module) {
    assert_shape_emitted(label, module, SHAPE_SKIP_UTF16, "BsSkipUtf16");
}

/// One row: both tiers agree on `subject` and the baseline tier answers `expected`.
fn one_row(label: &str, module: &Module, subject: &[u8], expected: i64) {
    let observed = with_both_tiers_bytes(module, subject, |interpreter, baseline| {
        assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        returned_i64(label, baseline)
    });
    assert_eq!(
        observed, expected,
        "{label}: answered {observed} for subject {subject:?} (expected {expected})"
    );
}

// ---------------------------------------------------------------------------
// Rows: the two successful paths commit 16 or 32 bits
// ---------------------------------------------------------------------------

/// **A BMP character commits `start += 16`, a surrogate pair commits `start += 32`.**
///
/// The two rows are the two mutually exclusive paths through the emitted sequence: `U+0041`
/// leaves the first word outside `0xD800..=0xDFFF`, so the BMP arm reaches the shared commit
/// with `len = 16`; `U+1F600`'s first word `0xD83D` is a **high** surrogate, so the pair arm
/// reads the second word, validates it as a low surrogate, and reaches the *same* commit with
/// `len = 32`. A commit that ran twice answers `32` / `64`; one that ran not at all answers
/// `0`.
#[test]
fn a_bmp_character_commits_16_bits_and_a_surrogate_pair_commits_32() {
    for (name, subject, expected) in [
        ("bs_skip_utf16/bmp", BMP, AFTER_BMP),
        ("bs_skip_utf16/pair", ASTRAL, AFTER_PAIR),
    ] {
        let module = plain_skip_module("inline_bs_skip_utf16");
        assert_emitted(name, &module);
        one_row(name, &module, subject, expected);
    }
}

/// **The tightest subjects still commit.** A two-byte subject is the shortest a BMP character
/// can occupy and a four-byte subject the shortest a pair can, so these rows are the two
/// over-read guards at their exact boundary — `start / 8 + 2 == num_bytes` and
/// `start / 8 + 4 == num_bytes` respectively, both admitted (`ja`, not `jae`).
///
/// The BMP row is also the hazard the objective names explicitly: a BMP character at the very
/// end of a subject must **not** be forced to deopt merely because a *pair* would not have fit,
/// which is why the pair-window test and the second over-read guard live on the surrogate path
/// only. A hoisted pair guard would leave this row answering `16` through the fallback instead
/// of through the emitted commit — still `16`, but no longer native, so the emission counter
/// assertion is what separates the two.
#[test]
fn the_shortest_subjects_still_commit_on_the_emitted_path() {
    for (name, subject, expected) in [
        ("bs_skip_utf16/shortest bmp", BMP_EXACT, AFTER_BMP),
        ("bs_skip_utf16/shortest pair", ASTRAL_EXACT, AFTER_PAIR),
    ] {
        let module = plain_skip_module("inline_bs_skip_utf16");
        assert_emitted(name, &module);
        one_row(name, &module, subject, expected);
    }
}

/// **The surrogate range's two edges are BMP characters.** `U+D7FF` (the last code point below
/// the range) and `U+E000` (the first above it) both commit 16 bits, so a range test with an
/// off-by-one at either edge would fail here while the rows above still passed.
#[test]
fn the_code_points_just_outside_the_surrogate_range_are_bmp_characters() {
    let module = plain_skip_module("inline_bs_skip_utf16");
    assert_emitted("bs_skip_utf16/range edges", &module);
    for (name, subject) in [
        ("bs_skip_utf16/below surrogates", BELOW_SURROGATES),
        ("bs_skip_utf16/above surrogates", ABOVE_SURROGATES),
    ] {
        one_row(name, &module, subject, AFTER_BMP);
    }
}

// ---------------------------------------------------------------------------
// Rows: the four malformed characters are the FAIL label
// ---------------------------------------------------------------------------

/// **Every malformed character is the fail label — never a trap, never a commit.**
///
/// Four rows, one per rejection `decode_utf16` performs:
///
/// * a **lone low** surrogate (`0xDE00`) — the `w1 > 0xDBFF` test / `ja fail_arm`;
/// * a high surrogate followed by a **non-low** word (`0x0041`) — the
///   `w2 - 0xDC00 <= 0x3FF` range test / `ja fail_arm`;
/// * a high surrogate with exactly **16** bits left — the pair-window test
///   `end - start < 32` / `jb fail_arm`;
/// * one byte, and no bytes at all — the saturating `remaining < 16` check / `jb fail_arm`.
///
/// All answer the sentinel block's `-1` on both tiers. That the position is **not** left at `16`
/// is exactly what `-1` proves: no commit ran on any of these paths, and nothing trapped.
#[test]
fn every_malformed_character_takes_the_fail_label() {
    let module = plain_skip_module("inline_bs_skip_utf16");
    assert_emitted("bs_skip_utf16/failures", &module);
    for (name, subject) in [
        ("bs_skip_utf16/lone low surrogate", LONE_LOW),
        ("bs_skip_utf16/high then non-low", HIGH_THEN_NON_LOW),
        ("bs_skip_utf16/high without room", HIGH_ALONE),
        ("bs_skip_utf16/truncated", TRUNCATED),
        ("bs_skip_utf16/empty", EMPTY),
    ] {
        one_row(name, &module, subject, FAIL);
    }
}

// ---------------------------------------------------------------------------
// The idempotency pin: the legitimate deopt
// ---------------------------------------------------------------------------

/// The bits `start .. start + nbits` of `subject`, read big-endian — the same field
/// `decode_utf16` reads, so the row's subject can be asserted rather than assumed.
fn field_bits(subject: &[u8], start: u32, nbits: u32) -> u32 {
    let mut value = 0u32;
    for k in 0..nbits {
        let bit = start + k;
        let byte = subject[(bit / 8) as usize];
        let set = (byte >> (7 - (bit % 8))) & 1;
        value = (value << 1) | u32::from(set);
    }
    value
}

/// **THE IDEMPOTENCY PIN — the shape deopts and still consumes the character exactly once.**
///
/// A `fallback` is a *deopt*: `jit_step_one` re-executes the **whole** instruction in the
/// interpreter with the same operand, so any state a partial execution committed is visible to
/// that re-execution. The only mutation this shape makes is the context's `start` word, which
/// the arm writes **last**, after every guard and after the loads and validity tests (which
/// mutate nothing) — so a rejection taken anywhere above cannot double-advance the position.
///
/// The row that proves it is a **non-byte-aligned `start`**: a preceding 4-bit `bs_skip_bits2`
/// leaves `start == 4`, so the emitted `test <start>, 7` / `jne fallback` guard fires on every
/// execution and the family's own bit-by-bit reader answers instead. Both widths are pinned —
/// a BMP character at bits `4..20` must answer `20`, and a **surrogate pair** at bits `4..36`
/// must answer `36`, which is the stronger row because only the family's reader can walk a
/// pair across a byte boundary at all. A double commit would answer `36` / `68`; a missing one
/// would answer `4`.
#[test]
fn a_non_byte_aligned_start_deopts_to_exactly_one_interpreter_step() {
    // `U+0041` at bits 4..20.
    let mut bmp = [0u8; 5];
    bmp[1] = 0x41;
    bmp[2] = 0x10;
    assert_eq!(
        field_bits(&bmp, 4, 16),
        0x41,
        "the row's subject must carry U+0041 at bits 4..20"
    );

    // `U+1F600`'s pair (`D83D DE00`) at bits 4..36.
    let mut pair = [0u8; 5];
    pair[0] = 0x0D;
    pair[1] = 0x83;
    pair[2] = 0xDE;
    assert_eq!(
        field_bits(&pair, 4, 16),
        0xD83D,
        "the row's subject must carry a high surrogate at bits 4..20"
    );
    assert_eq!(
        field_bits(&pair, 20, 16),
        0xDE00,
        "the row's subject must carry a low surrogate at bits 20..36"
    );

    for (name, subject, expected) in [
        ("bs_skip_utf16/misaligned bmp", &bmp[..], 4 + AFTER_BMP),
        ("bs_skip_utf16/misaligned pair", &pair[..], 4 + AFTER_PAIR),
    ] {
        let module = skip_module("inline_bs_skip_utf16_misaligned", Some(4), 0, Label(3));
        // The arm still *emits* for this module — the alignment test is a run-time guard, not a
        // compile-time predicate — which is what makes the row a deopt rather than a refusal.
        assert_emitted(name, &module);

        let before = bitstr_calls();
        let observed = with_both_tiers_bytes(&module, subject, |interpreter, baseline| {
            assert_same_outcome(name, interpreter, baseline, &module.atom_table);
            returned_i64(name, baseline)
        });
        let deopts = bitstr_calls().saturating_sub(before);
        println!("{name}: answer={observed} jit_bitstr_calls={deopts}");
        assert_eq!(
            observed, expected,
            "{name}: the character must be consumed exactly once from a bit offset of 4 (a \
             double commit would answer {}, a missing one 4)",
            expected + (expected - 4)
        );
        // The fallback is exactly one interpreter step (`emit_step_and_continue` ->
        // `jit_step_one`), not a `jit_bitstr` call, so that counter is not the instrument for
        // this row: the semantic pin above is stronger (only the family's bit-wise reader can
        // produce these answers from a non-aligned window).
        assert_eq!(
            deopts, 0,
            "{name}: the inline fallback is one interpreter step, not a `jit_bitstr` call \
             (got {deopts})"
        );
    }
}

// ---------------------------------------------------------------------------
// Rows: the Y-slot context
// ---------------------------------------------------------------------------

/// **A `Y`-slot context lowers through the guarded frame window and answers the same value.**
///
/// The arm's `emit_ctx_guard` loads the context through [`super::super::operands::Loc`]'s guarded
/// `Y` path, so this row proves the shape's `uses_y()` gate really made the block publish the
/// window: without it the emitted load would reject to the fallback and the row would still
/// agree — hence [`assert_emitted`] plus the zero-deopt assertion, which hold either way, and
/// the value, which a mis-published window could only get wrong by rejecting.
#[test]
fn a_y_slot_context_lowers_through_the_frame_window() {
    let module = y_skip_module("inline_bs_skip_utf16_y", 0);
    assert_emitted("bs_skip_utf16/Y context", &module);
    for (name, subject, expected) in [
        ("bs_skip_utf16/Y context bmp", BMP, AFTER_BMP),
        ("bs_skip_utf16/Y context pair", ASTRAL, AFTER_PAIR),
    ] {
        let observed = with_both_tiers_bytes(&module, subject, |interpreter, baseline| {
            assert_same_outcome(name, interpreter, baseline, &module.atom_table);
            returned_i64(name, baseline)
        });
        assert_eq!(observed, expected, "{name}: a Y-slot context must commit the same bits");
    }
}

/// **An out-of-frame `Y` context rejects to the fallback and agrees on the error.**
///
/// The block allocates a **one-slot** frame and the instruction names `y(5)`, so the emitted
/// guard's `cmp addr, y_end` / `jae fallback` fires on every execution. The interpreter's own
/// `y(5)` bounds check then produces its exact error, which both tiers must report identically —
/// never an out-of-frame dereference and never a fabricated value.
#[test]
fn an_out_of_frame_y_context_rejects_and_agrees_on_the_error() {
    let module = y_skip_module("inline_bs_skip_utf16_y_out", 5);
    assert_emitted("bs_skip_utf16/out-of-frame Y", &module);
    let (interpreters, baselines) = with_both_tiers_bytes(&module, BMP, |interpreter, baseline| {
        (interpreter.is_err(), baseline.is_err())
    });
    assert!(
        interpreters && baselines,
        "an out-of-frame `Y` context must be the interpreter's own error on both tiers \
         (interpreter err: {interpreters}, baseline err: {baselines})"
    );
    with_both_tiers_bytes(&module, BMP, |interpreter, baseline| {
        assert_same_outcome(
            "bs_skip_utf16/out-of-frame Y",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

// ---------------------------------------------------------------------------
// Rows: the compile-time refusals
// ---------------------------------------------------------------------------

/// **The compile-time refusals emit zero bytes.**
///
/// * a `NO_FAIL` fail code — the miss path *raises* `{badmatch, Subject}`, which allocates, so
///   the shape stays on the family (and the family's raise is the row below);
/// * a `-little` (`BSF_LITTLE`, `2`) or `-native` (`16`) flags word — `decode_utf16` honours the
///   endianness and reads each 16-bit word byte-swapped, so a swapped field is the family's own
///   reader (the arm emits the big-endian read only);
/// * an unresolvable fail label — the fail transfer needs its `(function, code)` target, so the
///   family keeps its exact `unknown_label`;
/// * an un-addressable `ctx` — the operand is neither an in-bound x-register nor an addressable
///   `Y` slot.
///
/// The refusal is total: the shape predicate is `None`, so the arm's counter cannot move and the
/// instruction statically deopts to `emit_bitstr`.
#[test]
fn the_no_fail_code_the_little_endian_flag_and_the_other_refusals_emit_nothing() {
    let module = plain_skip_module("inline_bs_skip_utf16");
    let labels = build_label_map(&module);
    // The inlinable form, as the row's own control.
    assert!(
        skip_utf16_shape(&labels, Label(3), &Operand::X(1), 0).is_some(),
        "the plain big-endian form must be inlined"
    );
    assert!(
        skip_utf16_shape(&labels, beam_loader::NO_FAIL, &Operand::X(1), 0).is_none(),
        "a NO_FAIL fail code allocates its raise, so it must be refused"
    );
    for (flags, what) in [(2u32, "BSF_LITTLE"), (16, "BSF_NATIVE"), (18, "both")] {
        assert!(
            skip_utf16_shape(&labels, Label(3), &Operand::X(1), flags).is_none(),
            "a {what} flags word must be refused (flags {flags})"
        );
    }
    // An unresolved fail label keeps the family's exact `unknown_label`.
    assert!(
        skip_utf16_shape(&labels, Label(9_999), &Operand::X(1), 0).is_none(),
        "an unresolved fail label must be refused"
    );
    // An operand that is neither an in-bound x-register nor an addressable `Y` slot.
    assert!(
        skip_utf16_shape(&labels, Label(3), &Operand::X(MAX_REGISTER), 0).is_none(),
        "an out-of-bound x-register must be refused"
    );
    assert!(
        skip_utf16_shape(&labels, Label(3), &Operand::Integer(7), 0).is_none(),
        "a non-register, non-`Y` operand must be refused"
    );

    // And the refusal is what the emitter does: a `NO_FAIL` module emits **zero bytes** for the
    // shape, so the instruction stays on the family's own arm (`emit_bitstr`), while both tiers
    // still agree. (`emit_bitstr` is the claimed-shape route, not the `_` deopt arm, so the
    // static-deopt census is not the instrument here — the absent emission is.)
    let no_fail = skip_module("inline_bs_skip_utf16_no_fail", None, 0, beam_loader::NO_FAIL);
    let (emitted, snapshot) = emitted_shapes_for(&no_fail, 0);
    assert!(
        !emitted.contains(&SHAPE_SKIP_UTF16),
        "a NO_FAIL instruction must not emit the arm (emitted {emitted:?})"
    );
    assert!(
        visited_instances(&snapshot, "BsSkipUtf16") >= 1,
        "the refused instruction must still be the census's variant name (visited {:?})",
        snapshot.instr_instances,
    );
    // The truncated subject makes the family raise `{badmatch, Subject}` — an allocating
    // reason, which is exactly why the arm refuses this fail code — and both tiers must report
    // the same class and reason.
    with_both_tiers_bytes(&no_fail, TRUNCATED, |interpreter, baseline| {
        assert_same_outcome(
            "bs_skip_utf16/NO_FAIL raise",
            interpreter,
            baseline,
            &no_fail.atom_table,
        );
        assert!(
            interpreter.is_err(),
            "a NO_FAIL miss must raise rather than take a fail label"
        );
    });

    // The same for the endianness refusal: zero bytes, and the family's byte-swapped reader is
    // what both tiers answer. `0x4100` little-endian is `U+0041`, so the row's subject is the
    // byte-swapped form of the BMP row's.
    let little = skip_module("inline_bs_skip_utf16_little", None, 2, Label(3));
    let (emitted, _) = emitted_shapes_for(&little, 0);
    assert!(
        !emitted.contains(&SHAPE_SKIP_UTF16),
        "a BSF_LITTLE instruction must not emit the arm (emitted {emitted:?})"
    );
    with_both_tiers_bytes(&little, &[0x41, 0x00, b'X'], |interpreter, baseline| {
        assert_same_outcome(
            "bs_skip_utf16/little-endian",
            interpreter,
            baseline,
            &little.atom_table,
        );
        assert_eq!(
            returned_i64("bs_skip_utf16/little-endian", baseline),
            AFTER_BMP,
            "the family's own little-endian reader must still consume one 16-bit word"
        );
    });
}

// ---------------------------------------------------------------------------
// The live oracle cross-check
// ---------------------------------------------------------------------------

/// The oracle module: the same character skipped in Erlang source, reporting the **bit count**
/// the match consumed (so the `16`-vs-`32` width decision is the oracle's own), plus a
/// match-or-not answer and the decoded scalar.
const ORACLE_SRC: &str = "\
-module(utf16match).
-export([advance/1, skip/1, decode/1]).

advance(Bin) ->
    case Bin of
        <<_/utf16, Rest/binary>> -> byte_size(Bin) * 8 - byte_size(Rest) * 8;
        _ -> -1
    end.

skip(Bin) ->
    case Bin of
        <<C/utf16, _/binary>> when C < 16#10000 -> 16;
        <<_/utf16, _/binary>> -> 32;
        _ -> -1
    end.

decode(Bin) ->
    case Bin of
        <<C/utf16, _/binary>> -> C;
        _ -> -1
    end.
";

/// Compile [`ORACLE_SRC`] into a per-`tag` temp dir and return that dir. `None` means `erlc`
/// could not be **spawned** (the caller skips loudly).
fn compile_oracle(tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_engine_utf16_{tag}"));
    std::fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    std::fs::write(dir.join("utf16match.erl"), ORACLE_SRC)
        .unwrap_or_else(|error| panic!("write utf16match.erl: {error}"));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("utf16match.erl")
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling utf16match.erl\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(dir)
}

/// Run `eval` under the system `erl` with `dir` **appended** to the code path. `None` means no
/// runnable `erl` at all (the caller skips loudly).
fn erl_eval(dir: &Path, eval: &str) -> Option<String> {
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-pz")
        .arg(dir)
        .arg("-eval")
        .arg(eval)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for `{eval}`\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The bytes of a binary literal, for the inline `erl -eval` source.
fn byte_literal(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// **Every row agrees with the live `erl` oracle, on each tier.**
///
/// [`assert_same_outcome`] already proves the two tiers agree; this row proves both agree with
/// the *language*. Three oracle functions: `advance/1` (the bit count the skip consumed —
/// `16` for a BMP character, `32` for a pair, `-1` when the subject does not match), `skip/1`
/// (the same answer through a guard on the decoded scalar) and `decode/1` (the scalar itself,
/// which is the value a `bs_get_utf16` will bind), evaluated by the system `erl` on the same
/// subject bytes.
#[test]
fn every_row_agrees_with_the_live_erl_oracle() {
    let Some(dir) = compile_oracle("rows") else {
        eprintln!("SKIP: no usable `erlc` on PATH -- bs_skip_utf16 oracle cross-check skipped");
        return;
    };
    let module = plain_skip_module("inline_bs_skip_utf16");
    let rows: [&[u8]; 8] = [
        BMP,
        ASTRAL,
        BMP_EXACT,
        ASTRAL_EXACT,
        BELOW_SURROGATES,
        LONE_LOW,
        HIGH_THEN_NON_LOW,
        TRUNCATED,
    ];

    let mut checked = 0usize;
    for function in ["advance", "skip", "decode"] {
        for subject in rows {
            let label = format!("oracle utf16match:{function}/1 on {subject:?}");
            let eval = format!(
                "io:format(\"~p\", [utf16match:{function}(<<{}>>)]), halt(0).",
                byte_literal(subject)
            );
            // The skip arm's answer is the position (or the fail sentinel); the oracle's
            // `advance`/`skip` answer the consumed width (or `-1`), so the two are directly
            // comparable. `decode` is not observable through this fixture, so it is compared
            // against the width's own meaning instead.
            let ours = with_both_tiers_bytes(&module, subject, |interpreter, baseline| {
                assert_same_outcome(&label, interpreter, baseline, &module.atom_table);
                returned_i64(&label, baseline)
            });
            match erl_eval(&dir, &eval) {
                Some(out) => {
                    let theirs: i64 = out
                        .trim()
                        .parse()
                        .unwrap_or_else(|_| panic!("{label}: oracle printed {out:?}"));
                    if function == "decode" {
                        // A decoded scalar carries no width, so the row asserts the *agreement
                        // between the oracle and this fixture's width* rather than the width
                        // itself: a scalar below `0x10000` is one BMP character's worth of
                        // subject, which is what `advance/1` reports as `16`.
                        let expected_width = if theirs < 0 {
                            FAIL
                        } else if theirs < 0x10000 {
                            AFTER_BMP
                        } else {
                            AFTER_PAIR
                        };
                        assert_eq!(
                            theirs.min(0xFFFF_FFFF).max(-1),
                            theirs,
                            "{label}: the oracle's scalar is a scalar"
                        );
                        assert_eq!(
                            expected_width, ours,
                            "{label}: the oracle decoded {theirs}, which pairs with width \
                             {expected_width}, but the tiers answered {ours}"
                        );
                    } else {
                        assert_eq!(
                            theirs, ours,
                            "{label}: the tiers answered {ours} but the oracle answered {theirs}"
                        );
                    }
                    checked += 1;
                }
                None => {
                    eprintln!("SKIP: no system `erl` on PATH -- {label} oracle value skipped");
                    return;
                }
            }
        }
    }
    println!("bs_skip_utf16: {checked} oracle rows checked");
    assert_eq!(
        checked,
        3 * rows.len(),
        "every row of every oracle function must have been checked"
    );
}
