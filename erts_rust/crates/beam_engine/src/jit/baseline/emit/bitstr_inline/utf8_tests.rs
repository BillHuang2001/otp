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

//! The `bs_skip_utf8` arm's own suite: the four character widths, the **ten** malformed
//! characters, the legitimate-deopt idempotency pin, the compile-time refusals and the live
//! `erl` oracle cross-check.
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
//! | `0x41` (ASCII) | the lead byte is `<= 0x7F`, so the ASCII path reaches the shared commit with `total = 8` | the position is `8` |
//! | `C3 A9` (`U+00E9`) | the lead byte announces **one** continuation, so the two-byte path commits `16` | `16` |
//! | `E2 82 AC` (`U+20AC`) | the lead byte announces **two** continuations | `24` |
//! | `F0 9F 98 80` (`U+1F600`) | the lead byte announces **three** continuations | `32` |
//! | an invalid lead (`80`, `F5`) | the classification's two invalid bands jump straight to the fail label | the sentinel `-1`, never a trap |
//! | a truncated 2/3/4-byte form | the taken path's own window check (`end - start < total`) takes the fail label | `-1` |
//! | `C3 41` | the `b & 0xC0 == 0x80` continuation test takes the fail label | `-1` |
//! | an overlong form (`C0 80`, `E0 80 80`) | the path's `result < minimum` test takes the fail label | `-1` |
//! | a surrogate (`ED A0 80`) | the `result - 0xD800 <= 0x7FF` range test takes the fail label | `-1` |
//! | `F4 90 80 80` (`> U+10FFFF`) | the `result > 0x10FFFF` test takes the fail label | `-1` |
//! | a non-byte-aligned `start` | the `test <start>, 7` guard takes `fallback` | the family answers, the character consumed **exactly once** |
//! | a `Y`-slot context | the guarded frame-window load rejects an out-of-frame slot | both tiers raise the interpreter's own error |
//! | a `NO_FAIL` fail code | the shape predicate refuses, **zero bytes** emitted | the family raises `{badmatch, _}` on both tiers |
//! | every `flags` word (`BSF_LITTLE`, `BSF_NATIVE`, …) | UTF-8 has no endianness, so the arm accepts **all** of them | both tiers answer the emitted sequence's position |
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

use super::{skip_utf8_shape, SHAPE_SKIP_UTF8};
use crate::context::MinimalContext;
use crate::interpreter::build_label_map;
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

/// `U+0041` — one ASCII byte, plus a tail so the lead byte's readable-bounds guard has slack.
const ASCII: &[u8] = &[0x41, b'X', b'X', b'X'];

/// `U+0041` as the **whole** subject: exactly one byte, so the lead byte's guard is tight.
const ASCII_EXACT: &[u8] = &[0x41];

/// `U+00E9` (`é`) as the two-byte form `C3 A9`, plus a tail.
const TWO_BYTE: &[u8] = &[0xC3, 0xA9, b'X', b'X', b'X'];

/// `U+20AC` (`€`) as the three-byte form `E2 82 AC`, plus a tail.
const THREE_BYTE: &[u8] = &[0xE2, 0x82, 0xAC, b'X', b'X', b'X'];

/// `U+1F600` (GRINNING FACE) as the four-byte form `F0 9F 98 80`, plus a tail.
const FOUR_BYTE: &[u8] = &[0xF0, 0x9F, 0x98, 0x80, b'X', b'X', b'X'];

/// The four-byte form as the **whole** subject: exactly four bytes, so every over-read guard is
/// tight and the widest path commits on the last available byte.
const FOUR_BYTE_EXACT: &[u8] = &[0xF0, 0x9F, 0x98, 0x80];

/// A **continuation byte** as the lead: no sequence may start here.
const CONTINUATION_LEAD: &[u8] = &[0x80, b'X', b'X'];

/// A lead byte **above `0xF4`**, which would encode beyond `U+10FFFF` whatever follows.
const ABOVE_LEAD_MAX: &[u8] = &[0xF5, 0x80, 0x80, 0x80];

/// A **truncated two-byte** form: the lead byte announces a continuation that is not there.
const TRUNCATED_TWO: &[u8] = &[0xC3];

/// A **truncated three-byte** form: two of the three bytes are present.
const TRUNCATED_THREE: &[u8] = &[0xE2, 0x82];

/// A **truncated four-byte** form: three of the four bytes are present.
const TRUNCATED_FOUR: &[u8] = &[0xF0, 0x9F, 0x98];

/// A **non-continuation** byte where a continuation is required.
const NON_CONTINUATION: &[u8] = &[0xC3, 0x41, b'X'];

/// An **overlong** two-byte form of `U+0000` (must have been one byte).
const OVERLONG_TWO: &[u8] = &[0xC0, 0x80, b'X'];

/// An **overlong** three-byte form of `U+0000` (must have been one byte).
const OVERLONG_THREE: &[u8] = &[0xE0, 0x80, 0x80, b'X'];

/// `U+D800` — a **surrogate**, encoded as UTF-8 (`ED A0 80`), which UTF-8 may not carry.
const SURROGATE: &[u8] = &[0xED, 0xA0, 0x80, b'X'];

/// `U+110000` — **above** the last Unicode scalar value (`F4 90 80 80`).
const ABOVE_UNICODE: &[u8] = &[0xF4, 0x90, 0x80, 0x80, b'X'];

/// No bytes at all: not even a lead byte.
const EMPTY: &[u8] = &[];

/// The position the commit leaves behind after one ASCII character.
const AFTER_ASCII: i64 = 8;

/// The position the commit leaves behind after one two-byte character.
const AFTER_TWO: i64 = 16;

/// The position the commit leaves behind after one three-byte character.
const AFTER_THREE: i64 = 24;

/// The position the commit leaves behind after one four-byte character.
const AFTER_FOUR: i64 = 32;

/// The sentinel [`sentinel_block`] answers when the fail label is taken.
const FAIL: i64 = -1;

// ---------------------------------------------------------------------------
// The modules under test
// ---------------------------------------------------------------------------

/// `f/1` = `bs_start_match3` [`bs_skip_bits2`] `-> bs_skip_utf8 -> bs_get_position`, with the
/// fail label answering [`sentinel_block`]'s `-1`.
///
/// The success answer is the context's **position**, so "which of the four static paths ran",
/// "did it run at all" and "did it run **once**" are all observable through one returned number,
/// and "did the fail branch run" is the distinguishable `-1`.
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
        code.push(Instr::BsSkipUtf8 {
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

/// The standard module: a real fail label, no pre-skip, a zero flags word.
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
            Instr::BsSkipUtf8 {
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

/// [`assert_shape_emitted`] for the UTF-8 skip arm.
fn assert_emitted(label: &str, module: &Module) {
    assert_shape_emitted(label, module, SHAPE_SKIP_UTF8, "BsSkipUtf8");
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
// Rows: the four static paths commit 8 / 16 / 24 / 32 bits
// ---------------------------------------------------------------------------

/// **Each of the four character widths commits its own bit length.**
///
/// `decode_utf8`'s lead-byte classification is `trailing_bytes_for_utf8`'s five bands, and the
/// four valid ones are the emitted sequence's four **(static)** paths. One row per band:
/// `0x41` is `<= 0x7F` (the ASCII path, `total = 8`), `0xC3` announces one continuation
/// (`16`), `0xE2` two (`24`) and `0xF0` three (`32`). A commit that ran twice doubles the
/// answer, one that ran not at all answers `0`, and an off-by-one in any band's `total` answers
/// one of the neighbouring widths.
#[test]
fn each_of_the_four_character_widths_commits_its_own_bit_length() {
    let module = plain_skip_module("inline_bs_skip_utf8");
    assert_emitted("bs_skip_utf8/widths", &module);
    for (name, subject, expected) in [
        ("bs_skip_utf8/ascii", ASCII, AFTER_ASCII),
        ("bs_skip_utf8/two byte", TWO_BYTE, AFTER_TWO),
        ("bs_skip_utf8/three byte", THREE_BYTE, AFTER_THREE),
        ("bs_skip_utf8/four byte", FOUR_BYTE, AFTER_FOUR),
    ] {
        one_row(name, &module, subject, expected);
    }
}

/// **The tightest subjects still commit.** A one-byte subject is the shortest an ASCII character
/// can occupy and a four-byte subject the shortest a four-byte character can, so these two rows
/// are the lead guard and the widest path's guard at their exact boundary
/// (`start / 8 + 1 == num_bytes` and `start / 8 + 4 == num_bytes`, both admitted — `ja`, not
/// `jae`).
///
/// The ASCII row is also the reason the wider forms' guards live **only** on their own paths: a
/// one-byte subject could never fit a four-byte character, yet the ASCII character in it must
/// still commit natively. A hoisted guard would leave this row answering `8` through the
/// fallback instead — still `8`, but no longer native, so the emission counter assertion is what
/// separates the two.
#[test]
fn the_shortest_subjects_still_commit_on_the_emitted_path() {
    let module = plain_skip_module("inline_bs_skip_utf8");
    assert_emitted("bs_skip_utf8/shortest", &module);
    for (name, subject, expected) in [
        ("bs_skip_utf8/shortest ascii", ASCII_EXACT, AFTER_ASCII),
        ("bs_skip_utf8/shortest four byte", FOUR_BYTE_EXACT, AFTER_FOUR),
    ] {
        one_row(name, &module, subject, expected);
    }
}

// ---------------------------------------------------------------------------
// Rows: the ten malformed characters are the FAIL label
// ---------------------------------------------------------------------------

/// **Every malformed character is the fail label — never a trap, never a commit.**
///
/// Ten rows, one per rejection `decode_utf8` performs:
///
/// * a **continuation byte** as the lead (`0x80`) — the classification's `<= 0xBF` band;
/// * a lead byte **above `0xF4`** (`0xF5`) — the chain's final `jmp fail_arm`;
/// * a **truncated** two-, three- and four-byte form at end-of-subject — the taken path's own
///   window check `end - start < total`;
/// * a **non-continuation** byte where one is required (`C3 41`) — the `b & 0xC0 == 0x80` test;
/// * an **overlong** two- (`C0 80`) and three-byte (`E0 80 80`) form — the path's
///   `result < minimum` test;
/// * a **surrogate** (`ED A0 80`) — the `result - 0xD800 <= 0x7FF` range test;
/// * a value **above `U+10FFFF`** (`F4 90 80 80`) — the `result > 0x10FFFF` test;
/// * no bytes at all — the saturating `remaining < 8` check.
///
/// All answer the sentinel block's `-1` on both tiers. That the position is **not** left at a
/// positive width is exactly what `-1` proves: no commit ran on any of these paths, and nothing
/// trapped.
#[test]
fn every_malformed_character_takes_the_fail_label() {
    let module = plain_skip_module("inline_bs_skip_utf8");
    assert_emitted("bs_skip_utf8/failures", &module);
    for (name, subject) in [
        ("bs_skip_utf8/continuation lead", CONTINUATION_LEAD),
        ("bs_skip_utf8/lead above F4", ABOVE_LEAD_MAX),
        ("bs_skip_utf8/truncated two", TRUNCATED_TWO),
        ("bs_skip_utf8/truncated three", TRUNCATED_THREE),
        ("bs_skip_utf8/truncated four", TRUNCATED_FOUR),
        ("bs_skip_utf8/non-continuation", NON_CONTINUATION),
        ("bs_skip_utf8/overlong two", OVERLONG_TWO),
        ("bs_skip_utf8/overlong three", OVERLONG_THREE),
        ("bs_skip_utf8/surrogate", SURROGATE),
        ("bs_skip_utf8/above Unicode", ABOVE_UNICODE),
        ("bs_skip_utf8/empty", EMPTY),
    ] {
        one_row(name, &module, subject, FAIL);
    }
}

// ---------------------------------------------------------------------------
// The idempotency pin: the legitimate deopt
// ---------------------------------------------------------------------------

/// The bits `start .. start + nbits` of `subject`, read big-endian — the same field
/// `decode_utf8` reads, so the row's subject can be asserted rather than assumed.
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

/// A subject whose bits `offset ..` carry the bytes of `pattern`, big-endian bit order,
/// everything else zero — how a **non-byte-aligned** character is laid out for a row.
fn place_bits(offset: u32, pattern: &[u8]) -> Vec<u8> {
    let total = offset as usize + 8 * pattern.len();
    let mut bytes = vec![0u8; total.div_ceil(8)];
    for bit in offset as usize..total {
        let source = bit - offset as usize;
        if (pattern[source / 8] >> (7 - (source % 8))) & 1 == 1 {
            bytes[bit / 8] |= 0x80 >> (bit % 8);
        }
    }
    bytes
}

/// **THE IDEMPOTENCY PIN — the shape deopts and still consumes the character exactly once.**
///
/// A `fallback` is a *deopt*: `jit_step_one` re-executes the **whole** instruction in the
/// interpreter with the same operand, so any state a partial execution committed is visible to
/// that re-execution. The only mutation this shape makes is the context's `start` word, which the
/// arm writes **last**, after every guard and after the loads and validity tests (which mutate
/// nothing) — so a rejection taken anywhere above cannot double-advance the position.
///
/// The row that proves it is a **non-byte-aligned `start`**: a preceding 4-bit `bs_skip_bits2`
/// leaves `start == 4`, so the emitted `test <start>, 7` / `jne fallback` guard fires on every
/// execution and the family's own bit-by-bit reader answers instead. Three widths are pinned —
/// an ASCII character at bits `4..12` must answer `12`, a two-byte one at bits `4..20` must
/// answer `20`, and a four-byte one at bits `4..36` must answer `36` (the widest path, and the
/// one the four-way classification has to get right off an unaligned window). A double commit
/// would answer `20`/`36`/`68`; a missing one `4`.
#[test]
fn a_non_byte_aligned_start_deopts_to_exactly_one_interpreter_step() {
    let ascii = place_bits(4, ASCII_EXACT);
    assert_eq!(
        field_bits(&ascii, 4, 8),
        0x41,
        "the row's subject must carry U+0041 at bits 4..12"
    );
    let two = place_bits(4, &TWO_BYTE[..2]);
    assert_eq!(
        field_bits(&two, 4, 16),
        0xC3A9,
        "the row's subject must carry the two-byte form at bits 4..20"
    );
    let four = place_bits(4, &FOUR_BYTE[..4]);
    assert_eq!(
        field_bits(&four, 4, 32),
        0xF0_9F_98_80,
        "the row's subject must carry the four-byte form at bits 4..36"
    );

    for (name, subject, expected) in [
        ("bs_skip_utf8/misaligned ascii", &ascii[..], 4 + AFTER_ASCII),
        ("bs_skip_utf8/misaligned two byte", &two[..], 4 + AFTER_TWO),
        (
            "bs_skip_utf8/misaligned four byte",
            &four[..],
            4 + AFTER_FOUR,
        ),
    ] {
        let module = skip_module("inline_bs_skip_utf8_misaligned", Some(4), 0, Label(3));
        // The arm still *emits* for this module — the alignment test is a run-time guard, not a
        // compile-time predicate — which is what makes the row a deopt rather than a refusal.
        assert_emitted(name, &module);

        let observed = with_both_tiers_bytes(&module, subject, |interpreter, baseline| {
            assert_same_outcome(name, interpreter, baseline, &module.atom_table);
            returned_i64(name, baseline)
        });
        println!("{name}: answer={observed}");
        assert_eq!(
            observed, expected,
            "{name}: the character must be consumed exactly once from a bit offset of 4 (a \
             double commit would answer {}, a missing one 4)",
            expected + (expected - 4)
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
/// agree — hence [`assert_emitted`] plus the zero-deopt assertion, which hold either way, and the
/// value, which a mis-published window could only get wrong by rejecting.
#[test]
fn a_y_slot_context_lowers_through_the_frame_window() {
    let module = y_skip_module("inline_bs_skip_utf8_y", 0);
    assert_emitted("bs_skip_utf8/Y context", &module);
    for (name, subject, expected) in [
        ("bs_skip_utf8/Y context ascii", ASCII, AFTER_ASCII),
        ("bs_skip_utf8/Y context four byte", FOUR_BYTE, AFTER_FOUR),
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
    let module = y_skip_module("inline_bs_skip_utf8_y_out", 5);
    assert_emitted("bs_skip_utf8/out-of-frame Y", &module);
    let (interpreters, baselines) = with_both_tiers_bytes(&module, ASCII, |interpreter, baseline| {
        (interpreter.is_err(), baseline.is_err())
    });
    assert!(
        interpreters && baselines,
        "an out-of-frame `Y` context must be the interpreter's own error on both tiers \
         (interpreter err: {interpreters}, baseline err: {baselines})"
    );
    with_both_tiers_bytes(&module, ASCII, |interpreter, baseline| {
        assert_same_outcome(
            "bs_skip_utf8/out-of-frame Y",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

// ---------------------------------------------------------------------------
// Rows: the compile-time refusals, and the flags words that are NOT refused
// ---------------------------------------------------------------------------

/// **The compile-time refusals emit zero bytes.**
///
/// * a `NO_FAIL` fail code — the miss path *raises* `{badmatch, Subject}`, which allocates, so
///   the shape stays on the family (and the family's raise is the row below);
/// * an unresolvable fail label — the fail transfer needs its `(function, code)` target, so the
///   family keeps its exact `unknown_label`;
/// * an un-addressable `ctx` — the operand is neither an in-bound x-register nor an addressable
///   `Y` slot.
///
/// The refusal is total: the shape predicate is `None`, so the arm's counter cannot move and the
/// instruction stays on the family's own reader.
///
/// **No `flags` word is refused**, and that is asserted here too, because it is the one respect
/// in which the UTF-8 pair differs from the UTF-16 and UTF-32 pairs: `decode_utf8` takes no flags
/// (a UTF-8 sequence has no endianness), so `skip_utf8_shape` has no `flags` parameter at all and
/// every word the instruction carries lowers. The `-little` module below therefore *emits* the
/// arm and both tiers answer the emitted sequence's position.
#[test]
fn the_no_fail_code_an_unresolved_label_and_an_unaddressable_context_emit_nothing() {
    let module = plain_skip_module("inline_bs_skip_utf8");
    let labels = build_label_map(&module);
    // The inlinable form, as the row's own control.
    assert!(
        skip_utf8_shape(&labels, Label(3), &Operand::X(1)).is_some(),
        "the plain form must be inlined"
    );
    assert!(
        skip_utf8_shape(&labels, beam_loader::NO_FAIL, &Operand::X(1)).is_none(),
        "a NO_FAIL fail code allocates its raise, so it must be refused"
    );
    // An unresolved fail label keeps the family's exact `unknown_label`.
    assert!(
        skip_utf8_shape(&labels, Label(9_999), &Operand::X(1)).is_none(),
        "an unresolved fail label must be refused"
    );
    // An operand that is neither an in-bound x-register nor an addressable `Y` slot.
    assert!(
        skip_utf8_shape(&labels, Label(3), &Operand::X(MAX_REGISTER)).is_none(),
        "an out-of-bound x-register must be refused"
    );
    assert!(
        skip_utf8_shape(&labels, Label(3), &Operand::Integer(7)).is_none(),
        "a non-register, non-`Y` operand must be refused"
    );

    // And the refusal is what the emitter does: a `NO_FAIL` module emits **zero bytes** for the
    // shape, so the instruction stays on the family's own arm (`emit_bitstr`), while both tiers
    // still agree. (`emit_bitstr` is the claimed-shape route, not the `_` deopt arm, so the
    // static-deopt census is not the instrument here — the absent emission is.)
    let no_fail = skip_module("inline_bs_skip_utf8_no_fail", None, 0, beam_loader::NO_FAIL);
    let (emitted, snapshot) = emitted_shapes_for(&no_fail, 0);
    assert!(
        !emitted.contains(&SHAPE_SKIP_UTF8),
        "a NO_FAIL instruction must not emit the arm (emitted {emitted:?})"
    );
    assert!(
        visited_instances(&snapshot, "BsSkipUtf8") >= 1,
        "the refused instruction must still be the census's variant name (visited {:?})",
        snapshot.instr_instances,
    );
    // The truncated subject makes the family raise `{badmatch, Subject}` — an allocating reason,
    // which is exactly why the arm refuses this fail code — and both tiers must report the same
    // class and reason.
    with_both_tiers_bytes(&no_fail, TRUNCATED_TWO, |interpreter, baseline| {
        assert_same_outcome(
            "bs_skip_utf8/NO_FAIL raise",
            interpreter,
            baseline,
            &no_fail.atom_table,
        );
        assert!(
            interpreter.is_err(),
            "a NO_FAIL miss must raise rather than take a fail label"
        );
    });
}

/// **Every `flags` word lowers**, because a UTF-8 sequence has no endianness.
///
/// `decode_utf8` takes no flags and both UTF-8 family arms destructure `flags: _`, so the arm has
/// nothing to refuse: `skip_utf8_shape` has no `flags` parameter, and a `-little`
/// (`BSF_LITTLE`, `2`) or `-native` (`16`) word must still emit the shape and answer the emitted
/// sequence's position. This is the row that would fail if a future change gave UTF-8 an
/// endianness refusal copied from the UTF-16 or UTF-32 pair.
#[test]
fn every_flags_word_lowers_because_utf8_has_no_endianness() {
    for (flags, what) in [(2u32, "BSF_LITTLE"), (16, "BSF_NATIVE"), (18, "both")] {
        let module = skip_module("inline_bs_skip_utf8_flags", None, flags, Label(3));
        assert_emitted(what, &module);
        one_row(what, &module, THREE_BYTE, AFTER_THREE);
    }
}

// ---------------------------------------------------------------------------
// The live oracle cross-check
// ---------------------------------------------------------------------------

/// The oracle module: the same character skipped in Erlang source, reporting the **bit count**
/// the match consumed (so the 8/16/24/32 width decision is the oracle's own), plus a
/// match-or-not answer and the decoded scalar.
const ORACLE_SRC: &str = "\
-module(utf8match).
-export([advance/1, skip/1, decode/1]).

advance(Bin) ->
    case Bin of
        <<_/utf8, Rest/binary>> -> byte_size(Bin) * 8 - byte_size(Rest) * 8;
        _ -> -1
    end.

skip(Bin) ->
    case Bin of
        <<C/utf8, _/binary>> when C < 16#80 -> 8;
        <<C/utf8, _/binary>> when C < 16#800 -> 16;
        <<C/utf8, _/binary>> when C < 16#10000 -> 24;
        <<_/utf8, _/binary>> -> 32;
        _ -> -1
    end.

decode(Bin) ->
    case Bin of
        <<C/utf8, _/binary>> -> C;
        _ -> -1
    end.
";

/// Compile [`ORACLE_SRC`] into a per-`tag` temp dir and return that dir. `None` means `erlc`
/// could not be **spawned** (the caller skips loudly).
fn compile_oracle(tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_engine_utf8_{tag}"));
    std::fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    std::fs::write(dir.join("utf8match.erl"), ORACLE_SRC)
        .unwrap_or_else(|error| panic!("write utf8match.erl: {error}"));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("utf8match.erl")
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling utf8match.erl\n  stderr: {}",
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
/// the *language*. Three oracle functions: `advance/1` (the bit count the skip consumed — `8`
/// for ASCII, `16`/`24`/`32` for the wider forms, `-1` when the subject does not match), `skip/1`
/// (the same answer through a guard on the decoded scalar) and `decode/1` (the scalar itself,
/// which is the value a `bs_get_utf8` will bind), evaluated by the system `erl` on the same
/// subject bytes.
#[test]
fn every_row_agrees_with_the_live_erl_oracle() {
    let Some(dir) = compile_oracle("rows") else {
        eprintln!("SKIP: no usable `erlc` on PATH -- bs_skip_utf8 oracle cross-check skipped");
        return;
    };
    let module = plain_skip_module("inline_bs_skip_utf8");
    let rows: [&[u8]; 12] = [
        ASCII,
        TWO_BYTE,
        THREE_BYTE,
        FOUR_BYTE,
        ASCII_EXACT,
        FOUR_BYTE_EXACT,
        CONTINUATION_LEAD,
        TRUNCATED_TWO,
        NON_CONTINUATION,
        OVERLONG_TWO,
        SURROGATE,
        ABOVE_UNICODE,
    ];

    let mut checked = 0usize;
    for function in ["advance", "skip", "decode"] {
        for subject in rows {
            let label = format!("oracle utf8match:{function}/1 on {subject:?}");
            let eval = format!(
                "io:format(\"~p\", [utf8match:{function}(<<{}>>)]), halt(0).",
                byte_literal(subject)
            );
            // The skip arm's answer is the position (or the fail sentinel); the oracle's
            // `advance`/`skip` answer the consumed width (or `-1`), so the two are directly
            // comparable. `decode` is not observable through this fixture, so it is compared
            // against the width its scalar implies instead.
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
                        // between the oracle's scalar and this fixture's width* rather than the
                        // width itself: the scalar's own magnitude names the encoding length it
                        // must have had, which is what `advance/1` reports as 8/16/24/32.
                        let expected_width = if theirs < 0 {
                            FAIL
                        } else if theirs < 0x80 {
                            AFTER_ASCII
                        } else if theirs < 0x800 {
                            AFTER_TWO
                        } else if theirs < 0x1_0000 {
                            AFTER_THREE
                        } else {
                            AFTER_FOUR
                        };
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
    println!("bs_skip_utf8: {checked} oracle rows checked");
    assert_eq!(
        checked,
        3 * rows.len(),
        "every row of every oracle function must have been checked"
    );
}
