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

//! The `bs_get_utf8` arm's own suite — the value-binding twin of `bs_skip_utf8`: the four
//! character widths, the **eleven** malformed characters, the `dst == ctx` hazard, the `Y`-slot
//! destination and its out-of-frame rejection, the legitimate-deopt idempotency pin, the
//! compile-time refusals and the live `erl` oracle cross-check.
//!
//! Every row drives the **same** input through [`Tier::Interpreter`] and [`Tier::BaselineJit`],
//! asserts the two agree under `beam_term::cmp`, and then asserts *what* each tier answered — so
//! an arm that bound a plausible-but-wrong scalar cannot hide behind two tiers that share it.
//!
//! The get form has two observable facts, and the modules here separate them: an
//! [`Answer::Value`] module returns what the bind wrote, and an [`Answer::Position`] module
//! returns the context's `start` word afterwards. A lowering that committed without binding
//! passes only the second; one that bound without committing passes only the first.
//!
//! | row | what the emitted sequence does | proof |
//! |---|---|---|
//! | `0x41` (ASCII) | the lead byte is `<= 0x7F`, so the ASCII path reaches the shared commit with `total = 8` | value `0x41`, position `8` |
//! | `C3 A9` (`U+00E9`) | the two-byte path decodes `(lead & 0x1F) << 6 \| (b1 & 0x3F)` and commits `16` | value `0xE9`, position `16` |
//! | `E2 82 AC` (`U+20AC`) | the three-byte path | value `0x20AC`, position `24` |
//! | `F0 9F 98 80` (`U+1F600`) | the four-byte path | value `0x1F600`, position `32` |
//! | a second `bs_get_utf8` after the first | the second character is read from offset 8 | value `0x58` (`X`) |
//! | an invalid lead (`80`, `F5`), a truncated 2/3/4-byte form, `C3 41`, an overlong `C0 80` / `E0 80 80`, a surrogate `ED A0 80`, `F4 90 80 80`, an empty subject | the classification's invalid bands, the taken path's window check, the continuation tag test, the `result < minimum` test, the surrogate range test and the `result > 0x10FFFF` test each fire | the sentinel `-1`, **nothing bound** |
//! | `Dst == Ctx` (`x(1)` is both) | the bind overwrites the only register naming the context, so the commit must precede it | value `0x41`, position read back through a `y(0)` alias |
//! | a `Y`-slot `dst`, and one past the frame | the guarded store publishes the window; an out-of-frame slot rejects to the fallback | value bound / both tiers raise identically |
//! | a non-byte-aligned `start` | the `test <start>, 7` guard takes `fallback` | the family answers, the character consumed **exactly once** |
//! | `NO_FAIL` / an un-resolvable label / an un-addressable `ctx` or `dst` | the shape predicate refuses, **zero bytes** emitted | both tiers answer the family |
//!
//! The arm's emitted sequence is **not** proved non-vacuously by a cross-tier comparison alone,
//! which also passes for an implementation that deopts every instruction (a deopt is executed by
//! the interpreter on *both* sides): [`assert_get_emitted`] adds the cache-free compile's
//! `inlined` counter, its census variant name and **zero** static deopts.

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_common::Result as BeamResult;
use beam_loader::{Instr, Label, Module, Operand};
use beam_term::{Atom, Eterm};

use super::{get_utf8_shape, Utf8Shape, SHAPE_GET_UTF8};
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
// The subjects — the same characters the skip twin's suite uses, so the two suites reason about
// one set: `41` = U+0041, `C3 A9` = U+00E9, `E2 82 AC` = U+20AC, `F0 9F 98 80` = U+1F600.
// ---------------------------------------------------------------------------

/// `U+0041` — one ASCII byte, plus a tail (`XXX`) so the position rows and the second-character
/// row have something to read after the first character.
const ASCII: &[u8] = &[0x41, b'X', b'X', b'X'];

/// `U+00E9` (`é`) as the two-byte form `C3 A9`, plus a tail.
const TWO_BYTE: &[u8] = &[0xC3, 0xA9, b'X', b'X', b'X'];

/// `U+20AC` (`€`) as the three-byte form `E2 82 AC`, plus a tail.
const THREE_BYTE: &[u8] = &[0xE2, 0x82, 0xAC, b'X', b'X', b'X'];

/// `U+1F600` (GRINNING FACE) as the four-byte form `F0 9F 98 80`, plus a tail.
const FOUR_BYTE: &[u8] = &[0xF0, 0x9F, 0x98, 0x80, b'X', b'X', b'X'];

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

/// The second character of the [`ASCII`] subject: `X`, `0x58`.
const TAIL_CHAR: i64 = 0x58;

// ---------------------------------------------------------------------------
// The modules under test
// ---------------------------------------------------------------------------

/// What a get-form module answers.
#[derive(Clone, Copy, Debug)]
enum Answer {
    /// The bound scalar — exactly what `bs_get_utf8` wrote to its destination.
    Value,
    /// The context's position **after** the get (`bs_get_position`).
    Position,
    /// A **second** `bs_get_utf8` from the same context, bound into `x(0)`: the one question a
    /// value row cannot ask, because a first character that consumed the wrong number of bits
    /// (or nothing at all) shows up as a different *second* character.
    Second,
}

/// A get-form module's variable part.
struct GetSpec {
    /// The `bs_get_utf8` destination operand.
    dst: Operand,
    /// What `f/1` answers.
    answer: Answer,
    /// A `bs_skip_bits2` emitted before the get (the misaligned-`start` rows).
    skip_bits: Option<u32>,
    /// The `flags` word handed to the instruction (accepted, never refused — UTF-8 has no
    /// endianness — so the flags row proves it lowers).
    flags: u32,
    /// The fail label (a sentinel block unless a row asks for `NO_FAIL`).
    fail: Label,
    /// The Y frame's slot count: `2` gives `y(0)` (the context alias) and `y(1)` (a `Y`
    /// destination); the out-of-frame row lowers a **one-slot** frame and names `y(5)`.
    frame: u32,
    /// Copy the context into `y(0)` **before** the get. This is what makes a `dst == ctx` row's
    /// commit observable: the bind overwrites the only register naming the context, so the
    /// position it committed is read back through the alias (the same block, so the same `start`
    /// word).
    alias_in_y: bool,
}

impl GetSpec {
    /// The plain form: `dst = x(0)`, the answer is the bound scalar, no frame at all.
    fn plain() -> GetSpec {
        GetSpec {
            dst: Operand::X(0),
            answer: Answer::Value,
            skip_bits: None,
            flags: 0,
            fail: Label(3),
            frame: 0,
            alias_in_y: false,
        }
    }

    /// `f/1` answers the bound scalar bound into `dst`.
    fn value(dst: Operand) -> GetSpec {
        GetSpec {
            dst,
            ..GetSpec::plain()
        }
    }

    /// `f/1` answers the context position after a get into `x(0)`.
    fn position() -> GetSpec {
        GetSpec {
            answer: Answer::Position,
            ..GetSpec::plain()
        }
    }

    /// `f/1` answers a **second** character read from `x(1)` after the first.
    fn second() -> GetSpec {
        GetSpec {
            answer: Answer::Second,
            ..GetSpec::plain()
        }
    }
}

/// The get-form module: `bs_start_match3` [`bs_skip_bits2`] -> `bs_get_utf8` -> the answer,
/// with the fail label answering [`sentinel_block`]'s `-1`.
fn get_module(name: &str, spec: GetSpec) -> Module {
    bitstr_module(name, &[], 1, |_| {
        let mut code = vec![Instr::Label(Label(1)), Instr::Label(Label(2))];
        if spec.frame > 0 {
            code.push(Instr::Allocate {
                stack_need: beam_loader::HeapNeed::Words(spec.frame),
                live: 2,
            });
        }
        code.push(Instr::BsStartMatch3 {
            fail: Label(3),
            bin: Operand::X(0),
            live: 1,
            dst: Operand::X(1),
        });
        if spec.alias_in_y {
            code.push(Instr::Move {
                src: Operand::X(1),
                dst: Operand::Y(0),
            });
        }
        if let Some(skip) = spec.skip_bits {
            code.push(Instr::BsSkipBits2 {
                fail: Label(3),
                ctx: Operand::X(1),
                size: Operand::Integer(i64::from(skip)),
                unit: 1,
                flags: 0,
            });
        }
        code.push(Instr::BsGetUtf8 {
            fail: spec.fail,
            ctx: Operand::X(1),
            live: 2,
            flags: spec.flags,
            dst: spec.dst.clone(),
        });
        match spec.answer {
            Answer::Position => code.push(Instr::BsGetPosition {
                ctx: if spec.alias_in_y {
                    Operand::Y(0)
                } else {
                    Operand::X(1)
                },
                dst: Operand::X(0),
                live: 2,
            }),
            Answer::Second => {
                // The second character lands straight in `x(0)`, which is what `f/1` returns.
                code.push(Instr::BsGetUtf8 {
                    fail: Label(3),
                    ctx: Operand::X(1),
                    live: 2,
                    flags: 0,
                    dst: Operand::X(0),
                });
            }
            Answer::Value => {
                if !matches!(spec.dst, Operand::X(0)) {
                    code.push(Instr::Move {
                        src: spec.dst,
                        dst: Operand::X(0),
                    });
                }
            }
        }
        if spec.frame > 0 {
            code.push(Instr::Deallocate(spec.frame));
        }
        code.push(Instr::Return);
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
fn assert_get_emitted(label: &str, module: &Module) {
    let (emitted, snapshot) = emitted_shapes_for(module, 0);
    println!(
        "{label}: inlined(BsGetUtf8) emitted={} variant_instances={} static_deopts={}",
        emitted.contains(&SHAPE_GET_UTF8),
        visited_instances(&snapshot, "BsGetUtf8"),
        static_deopts(&snapshot, "BsGetUtf8"),
    );
    assert!(
        emitted.contains(&SHAPE_GET_UTF8),
        "{label}: the cache-free compile must emit the `bs_get_utf8` arm \
         (emitted {emitted:?}, wanted {SHAPE_GET_UTF8})"
    );
    assert!(
        visited_instances(&snapshot, "BsGetUtf8") >= 1,
        "{label}: `BsGetUtf8` must be the census's variant name, else the deopt assertion \
         below is vacuous (visited {:?})",
        snapshot.instr_instances,
    );
    assert_eq!(
        static_deopts(&snapshot, "BsGetUtf8"),
        0,
        "{label}: the arm must lower the instruction, not statically deopt it ({:?})",
        snapshot.instr_deopts,
    );
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

/// The bits `start .. start + nbits` of `subject`, read big-endian — the same field
/// `decode_utf8` reads, so the misaligned row's subject can be asserted rather than assumed.
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

// ---------------------------------------------------------------------------
// Rows: all four widths bind the scalar AND advance the position
// ---------------------------------------------------------------------------

/// **Each of the four widths binds its own scalar and commits its own bit length.**
///
/// `decode_utf8`'s lead-byte classification is `trailing_bytes_for_utf8`'s five bands, and the
/// four valid ones are the emitted sequence's four **(static)** paths. One row per band, each
/// with three modules: the [`Answer::Value`] module answers what the bind wrote, the
/// [`Answer::Position`] module answers the context's `start` word afterwards, and the
/// [`Answer::Second`] module asks the question neither can — whether the **first** character
/// consumed exactly its own width.
///
/// A binding that ran with the wrong shift or mask answers a neighbouring scalar; a commit that
/// ran twice doubles the position, one that ran not at all answers `0`, and an off-by-one in any
/// band's `total` answers one of the neighbouring widths.
#[test]
fn each_of_the_four_widths_binds_its_scalar_and_advances_its_own_width() {
    for (name, subject, scalar, position) in [
        ("bs_get_utf8/ascii", ASCII, 0x41i64, AFTER_ASCII),
        ("bs_get_utf8/two byte", TWO_BYTE, 0xE9, AFTER_TWO),
        ("bs_get_utf8/three byte", THREE_BYTE, 0x20AC, AFTER_THREE),
        ("bs_get_utf8/four byte", FOUR_BYTE, 0x1F600, AFTER_FOUR),
    ] {
        let values = get_module("inline_bs_get_utf8", GetSpec::value(Operand::X(0)));
        assert_get_emitted(name, &values);
        one_row(name, &values, subject, scalar);

        let positions = get_module("inline_bs_get_utf8", GetSpec::position());
        assert_get_emitted(name, &positions);
        one_row(name, &positions, subject, position);
    }

    let seconds = get_module("inline_bs_get_utf8", GetSpec::second());
    assert_get_emitted("bs_get_utf8/second character", &seconds);
    one_row(
        "bs_get_utf8/second character",
        &seconds,
        ASCII,
        TAIL_CHAR,
    );
}

// ---------------------------------------------------------------------------
// Rows: the eleven malformed characters are the FAIL label
// ---------------------------------------------------------------------------

/// **Every malformed character takes the fail label with nothing bound** — never a trap, never a
/// fabricated scalar.
///
/// Eleven rows, one per rejection `decode_utf8` performs:
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
/// All answer the sentinel block's `-1` on both tiers, which is exactly what proves the
/// destination was **not** written: a partially executed get would have left `x(0)` holding the
/// subject or a stale word, and neither is `-1`.
#[test]
fn every_malformed_character_takes_the_fail_label_without_binding() {
    let module = get_module("inline_bs_get_utf8", GetSpec::value(Operand::X(0)));
    assert_get_emitted("bs_get_utf8/failures", &module);
    for (name, subject) in [
        ("bs_get_utf8/continuation lead", CONTINUATION_LEAD),
        ("bs_get_utf8/lead above F4", ABOVE_LEAD_MAX),
        ("bs_get_utf8/truncated two", TRUNCATED_TWO),
        ("bs_get_utf8/truncated three", TRUNCATED_THREE),
        ("bs_get_utf8/truncated four", TRUNCATED_FOUR),
        ("bs_get_utf8/non-continuation", NON_CONTINUATION),
        ("bs_get_utf8/overlong two", OVERLONG_TWO),
        ("bs_get_utf8/overlong three", OVERLONG_THREE),
        ("bs_get_utf8/surrogate", SURROGATE),
        ("bs_get_utf8/above Unicode", ABOVE_UNICODE),
        ("bs_get_utf8/empty", EMPTY),
    ] {
        one_row(name, &module, subject, FAIL);
    }
}

// ---------------------------------------------------------------------------
// Rows: the destination hazards
// ---------------------------------------------------------------------------

/// **THE `Dst == Ctx` HAZARD — the commit runs through the block pointer, never a re-read of the
/// overwritten operand.**
///
/// `beam_asm:filter_essentials/1` is free to compile a `bs_get_utf8` whose destination **is** the
/// match context's own register (`x(1)` here). The bind therefore overwrites the only register
/// naming the context, so the emitted sequence must commit *first* — through the pointer
/// `emit_ctx_guard` left in `MOVE_SCRATCH` — and bind last. A sequence that re-read `x(1)` after
/// storing would dereference a small integer: a wrong result or an abort, never a typed error.
///
/// Two rows: the bound scalar lands in the aliased register (the [`Answer::Value`] module moves
/// `x(1)` out), and the commit really ran — observed as the position `8` read back through a
/// **`y(0)` alias** of the same context block taken before the get.
#[test]
fn a_destination_that_is_the_context_binds_the_scalar_and_commits_through_the_block_pointer() {
    let values = get_module("inline_bs_get_utf8_ctx", GetSpec::value(Operand::X(1)));
    assert_get_emitted("bs_get_utf8/dst == ctx (value)", &values);
    one_row("bs_get_utf8/dst == ctx (value)", &values, ASCII, 0x41);

    let positions = get_module(
        "inline_bs_get_utf8_ctx_alias",
        GetSpec {
            dst: Operand::X(1),
            alias_in_y: true,
            frame: 2,
            ..GetSpec::position()
        },
    );
    assert_get_emitted("bs_get_utf8/dst == ctx (position)", &positions);
    one_row(
        "bs_get_utf8/dst == ctx (position)",
        &positions,
        FOUR_BYTE,
        AFTER_FOUR,
    );
}

/// **A `Y`-slot destination lowers through the guarded frame window.**
///
/// The get form's `dst.guard_store` is emitted **first**, so an out-of-frame slot can only ever
/// reject (to one interpreter step) and never leave a half-written frame behind. The in-frame
/// row proves the published window is what the final store goes through.
#[test]
fn a_y_slot_destination_is_bound_through_the_frame_window() {
    let module = get_module(
        "inline_bs_get_utf8_y",
        GetSpec {
            dst: Operand::Y(1),
            frame: 2,
            ..GetSpec::plain()
        },
    );
    assert_get_emitted("bs_get_utf8/Y dst", &module);
    for (name, subject, expected) in [
        ("bs_get_utf8/Y dst ascii", ASCII, 0x41),
        ("bs_get_utf8/Y dst four byte", FOUR_BYTE, 0x1F600),
    ] {
        one_row(name, &module, subject, expected);
    }
}

/// **An out-of-frame `Y` destination rejects to the fallback and agrees on the error.**
///
/// The block allocates a **one-slot** frame and the instruction names `y(5)`, so the emitted
/// destination guard fires on every execution. The interpreter's own `y(5)` bounds check then
/// produces its exact error, which both tiers must report identically — never an out-of-frame
/// write and never a fabricated value.
#[test]
fn an_out_of_frame_y_destination_rejects_to_the_fallback_and_agrees_on_the_error() {
    let module = get_module(
        "inline_bs_get_utf8_y_out",
        GetSpec {
            dst: Operand::Y(5),
            frame: 1,
            ..GetSpec::plain()
        },
    );
    assert_get_emitted("bs_get_utf8/out-of-frame Y dst", &module);
    let (interpreters, baselines) = with_both_tiers_bytes(&module, ASCII, |interpreter, baseline| {
        (interpreter.is_err(), baseline.is_err())
    });
    assert!(
        interpreters && baselines,
        "an out-of-frame `Y` destination must be the interpreter's own error on both tiers \
         (interpreter err: {interpreters}, baseline err: {baselines})"
    );
    with_both_tiers_bytes(&module, ASCII, |interpreter, baseline| {
        assert_same_outcome(
            "bs_get_utf8/out-of-frame Y dst",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

// ---------------------------------------------------------------------------
// The idempotency pin: the legitimate deopt
// ---------------------------------------------------------------------------

/// **THE GET FORM'S IDEMPOTENCY PIN — the shape deopts and still consumes the character exactly
/// once.**
///
/// A `fallback` is a *deopt*: `jit_step_one` re-executes the **whole** instruction in the
/// interpreter against the machine's live state, so any state a partial execution committed is
/// visible to that re-execution. The one mutation this shape makes is the context's `start`
/// word, which the emitted sequence writes **last** — after the destination's frame-window guard,
/// the match-context guard, the remaining-window check, the subject classification, the
/// alignment guard, the lead byte's bounds guard and the taken path's own window and bounds
/// guards, i.e. after every point that can reject — and the bind follows it inside the same
/// block, so a rejection taken anywhere above (including on a **later** continuation byte)
/// cannot double-advance the position and cannot leave a stale bind behind.
///
/// A preceding 4-bit `bs_skip_bits2` leaves `start == 4`, so the emitted `test <start>, 7` /
/// `jne fallback` guard fires on every execution and the family's own bit-by-bit reader answers
/// instead. Three widths are pinned: an ASCII character at bits `4..12` must answer `12`, a
/// two-byte one at bits `4..20` `20`, and a four-byte one at bits `4..36` `36`. A double commit
/// would answer `20`/`36`/`68`; a missing one `4`.
#[test]
fn a_non_byte_aligned_start_deopts_the_get_form_to_exactly_one_interpreter_step() {
    let ascii = place_bits(4, &ASCII[..1]);
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

    for (name, subject, expected_position, scalar) in [
        (
            "bs_get_utf8/misaligned ascii",
            &ascii[..],
            4 + AFTER_ASCII,
            0x41i64,
        ),
        (
            "bs_get_utf8/misaligned two byte",
            &two[..],
            4 + AFTER_TWO,
            0xE9,
        ),
        (
            "bs_get_utf8/misaligned four byte",
            &four[..],
            4 + AFTER_FOUR,
            0x1F600,
        ),
    ] {
        let positions = get_module(
            "inline_bs_get_utf8_misaligned",
            GetSpec {
                skip_bits: Some(4),
                ..GetSpec::position()
            },
        );
        // The arm still *emits* for this module — the alignment test is a run-time guard, not a
        // compile-time predicate — which is what makes the row a deopt rather than a refusal.
        assert_get_emitted(name, &positions);
        let observed = with_both_tiers_bytes(&positions, subject, |interpreter, baseline| {
            assert_same_outcome(name, interpreter, baseline, &positions.atom_table);
            returned_i64(name, baseline)
        });
        assert_eq!(
            observed, expected_position,
            "{name}: the character must be consumed exactly once from a bit offset of 4 (a \
             double commit would answer {}, a missing one 4)",
            expected_position + (expected_position - 4)
        );

        let values = get_module(
            "inline_bs_get_utf8_misaligned",
            GetSpec {
                skip_bits: Some(4),
                ..GetSpec::value(Operand::X(0))
            },
        );
        assert_get_emitted(name, &values);
        one_row(name, &values, subject, scalar);
    }
}

// ---------------------------------------------------------------------------
// Rows: the compile-time refusals, and the flags words that are NOT refused
// ---------------------------------------------------------------------------

/// **The get form's compile-time refusals emit zero bytes.**
///
/// The conditions are the skip twin's plus the destination's addressability — the fail half is
/// literally shared ([`super::skip_utf8_shape`] and [`get_utf8_shape`] funnel through one
/// `utf8_fail_target`), so this row's job is to prove the *get* predicate carries it too:
///
/// * a `NO_FAIL` fail code — the miss path *raises* `{badmatch, Subject}`, which allocates;
/// * an unresolvable fail label — the family keeps its exact `unknown_label`;
/// * an un-addressable `ctx` or `dst` — neither an in-bound x-register nor an addressable `Y`
///   slot, so no `Loc`.
///
/// Each refusal is total: the shape predicate is `None`, so the arm's counter cannot move and
/// the instruction stays on the family's own reader.
///
/// **No `flags` word is refused**, and that is asserted here too, because it is the one respect
/// in which the UTF-8 pair differs from the UTF-16 and UTF-32 pairs: `decode_utf8` takes no
/// flags (a UTF-8 sequence has no endianness), so `get_utf8_shape` has no `flags` parameter at
/// all and every word the instruction carries lowers. The `-little` module below therefore
/// *emits* the arm and both tiers answer the emitted sequence's scalar.
#[test]
fn the_get_form_refuses_no_fail_an_unresolved_label_and_an_unaddressable_operand() {
    let module = get_module("inline_bs_get_utf8", GetSpec::plain());
    let labels = build_label_map(&module);
    // The inlinable form, as the row's own control.
    assert!(
        get_utf8_shape(&labels, Label(3), &Operand::X(1), &Operand::X(0)).is_some(),
        "the plain form must be inlined"
    );
    assert!(
        get_utf8_shape(&labels, beam_loader::NO_FAIL, &Operand::X(1), &Operand::X(0)).is_none(),
        "a NO_FAIL fail code allocates its raise, so it must be refused"
    );
    assert!(
        get_utf8_shape(&labels, Label(9_999), &Operand::X(1), &Operand::X(0)).is_none(),
        "an unresolved fail label must be refused"
    );
    assert!(
        get_utf8_shape(&labels, Label(3), &Operand::X(MAX_REGISTER), &Operand::X(0)).is_none(),
        "an out-of-bound x-register context must be refused"
    );
    assert!(
        get_utf8_shape(&labels, Label(3), &Operand::Integer(7), &Operand::X(0)).is_none(),
        "a non-register, non-`Y` context must be refused"
    );
    assert!(
        get_utf8_shape(&labels, Label(3), &Operand::X(1), &Operand::X(MAX_REGISTER)).is_none(),
        "a destination past the register-file bound has no `Loc` and must be refused"
    );
    assert!(
        get_utf8_shape(&labels, Label(3), &Operand::X(1), &Operand::Integer(7)).is_none(),
        "a non-register, non-`Y` destination must be refused"
    );
    // The shape type is the skip form's, carrying `dst: None` when built label-free.
    assert!(
        Utf8Shape::of(&Operand::X(1)).is_some()
            && Utf8Shape::of_get(&Operand::X(1), &Operand::X(0)).is_some(),
        "both label-free constructors must accept the plain form"
    );

    // A `NO_FAIL` module emits **zero bytes** for the get shape and both tiers agree on the
    // family's allocating raise.
    let no_fail = get_module(
        "inline_bs_get_utf8_no_fail",
        GetSpec {
            fail: beam_loader::NO_FAIL,
            ..GetSpec::plain()
        },
    );
    let (emitted, snapshot) = emitted_shapes_for(&no_fail, 0);
    assert!(
        !emitted.contains(&SHAPE_GET_UTF8),
        "a NO_FAIL get must not emit the arm (emitted {emitted:?})"
    );
    assert!(
        visited_instances(&snapshot, "BsGetUtf8") >= 1,
        "the refused instruction must still be the census's variant name (visited {:?})",
        snapshot.instr_instances,
    );
    with_both_tiers_bytes(&no_fail, TRUNCATED_TWO, |interpreter, baseline| {
        assert_same_outcome(
            "bs_get_utf8/NO_FAIL raise",
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
/// `decode_utf8` takes no flags and both UTF-8 family arms destructure `flags: _`, so the get
/// form has nothing to refuse: `get_utf8_shape` has no `flags` parameter, and a `-little`
/// (`BSF_LITTLE`, `2`) or `-native` (`16`) word must still emit the shape and answer the emitted
/// sequence's scalar. This is the row that would fail if a future change gave UTF-8 an
/// endianness refusal copied from the UTF-16 or UTF-32 pair.
#[test]
fn every_flags_word_lowers_because_utf8_has_no_endianness() {
    for (flags, what) in [(2u32, "BSF_LITTLE"), (16, "BSF_NATIVE"), (18, "both")] {
        let module = get_module(
            "inline_bs_get_utf8_flags",
            GetSpec {
                flags,
                ..GetSpec::value(Operand::X(0))
            },
        );
        assert_get_emitted(what, &module);
        one_row(what, &module, THREE_BYTE, 0x20AC);
        one_row(what, &module, ASCII, 0x41);
    }
}

// ---------------------------------------------------------------------------
// The live oracle cross-check
// ---------------------------------------------------------------------------

/// The oracle module: the same character decoded in Erlang source (`decode/1`, the scalar the
/// bind must produce) and the **bit count** the match consumed (`advance/1`, so the 8/16/24/32
/// width decision is the oracle's own). `-1` means the subject does not match.
const ORACLE_SRC: &str = "\
-module(utf8get).
-export([advance/1, decode/1]).

advance(Bin) ->
    case Bin of
        <<_/utf8, Rest/binary>> -> byte_size(Bin) * 8 - byte_size(Rest) * 8;
        _ -> -1
    end.

decode(Bin) ->
    case Bin of
        <<C/utf8, _/binary>> -> C;
        _ -> -1
    end.
";

/// Compile [`ORACLE_SRC`] into a temp dir and return that dir. `None` means `erlc` could not be
/// **spawned** (the caller skips loudly).
fn compile_oracle() -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("beam_engine_utf8_get");
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    std::fs::write(dir.join("utf8get.erl"), ORACLE_SRC)
        .unwrap_or_else(|error| panic!("write utf8get.erl: {error}"));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("utf8get.erl")
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling utf8get.erl\n  stderr: {}",
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

/// **Every get row agrees with the live `erl` oracle, on each tier.**
///
/// [`assert_same_outcome`] already proves the two tiers agree; this row proves both agree with
/// the *language*: `utf8get:decode/1` (the scalar the bind must write, `-1` when no `utf8`
/// matches) for the value module and `utf8get:advance/1` (the bit count the match consumed) for
/// the position one, evaluated by the system `erl` on the same subject bytes.
#[test]
fn the_get_form_agrees_with_the_live_erl_oracle() {
    let Some(dir) = compile_oracle() else {
        eprintln!("SKIP: no usable `erlc` on PATH -- bs_get_utf8 oracle cross-check skipped");
        return;
    };
    let values = get_module("inline_bs_get_utf8", GetSpec::value(Operand::X(0)));
    let positions = get_module("inline_bs_get_utf8", GetSpec::position());
    let rows: [&[u8]; 12] = [
        ASCII,
        TWO_BYTE,
        THREE_BYTE,
        FOUR_BYTE,
        CONTINUATION_LEAD,
        ABOVE_LEAD_MAX,
        TRUNCATED_TWO,
        NON_CONTINUATION,
        OVERLONG_TWO,
        SURROGATE,
        ABOVE_UNICODE,
        EMPTY,
    ];

    let mut checked = 0usize;
    for (function, module) in [("decode", &values), ("advance", &positions)] {
        for subject in rows {
            let label = format!("oracle utf8get:{function}/1 on {subject:?}");
            let ours = with_both_tiers_bytes(module, subject, |interpreter, baseline| {
                assert_same_outcome(&label, interpreter, baseline, &module.atom_table);
                returned_i64(&label, baseline)
            });
            let eval = format!(
                "io:format(\"~p\", [utf8get:{function}(<<{}>>)]), halt(0).",
                byte_literal(subject)
            );
            match erl_eval(&dir, &eval) {
                Some(out) => {
                    let theirs: i64 = out
                        .trim()
                        .parse()
                        .unwrap_or_else(|_| panic!("{label}: oracle printed {out:?}"));
                    assert_eq!(
                        theirs, ours,
                        "{label}: the tiers answered {ours} but the oracle answered {theirs}"
                    );
                    checked += 1;
                }
                None => {
                    eprintln!("SKIP: no system `erl` on PATH -- {label} oracle value skipped");
                    return;
                }
            }
        }
    }
    println!("bs_get_utf8: {checked} oracle rows checked");
    assert_eq!(
        checked,
        2 * rows.len(),
        "every row of every oracle function must have been checked"
    );
}
