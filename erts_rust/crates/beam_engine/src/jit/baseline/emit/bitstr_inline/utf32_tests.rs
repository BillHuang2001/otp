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

//! The `bs_skip_utf32` arm's own suite: the scalar/boundary rows, the legitimate-deopt
//! idempotency pin, the compile-time refusals and the live `erl` oracle cross-check.
//!
//! Every row drives the **same** input through [`Tier::Interpreter`] and [`Tier::BaselineJit`],
//! asserts the two agree under `beam_term::cmp`, and then asserts *what* each tier answered —
//! so an arm that produced a plausible-but-wrong value cannot hide behind two tiers that share
//! it.
//!
//! | row | what the emitted sequence does | proof |
//! |---|---|---|
//! | `U+0041` / `U+1F600` | the four `movzx` loads, both validity tests fall through, the commit runs | the context position is `32` |
//! | `0x00110000` | `cmp value, 0x10FFFF` / `ja` takes the **fail label** | the sentinel `-1`, never a trap |
//! | `0xD800` | the `value - 0xD800 <= 0x7FF` range test takes the fail label | `-1` |
//! | a three-byte subject | the saturating `remaining` check takes the fail label | `-1` |
//! | a non-byte-aligned `start` | the `test <start>, 7` guard takes `fallback` | the family answers, the character consumed **exactly once** |
//! | a `Y`-slot context | the guarded frame-window load rejects an out-of-frame slot | both tiers raise the interpreter's own error |
//! | a `NO_FAIL` fail code | the shape predicate refuses, **zero bytes** emitted | the family raises `{badmatch, _}` on both tiers |
//!
//! The arm's own emitted sequence is proved **non-vacuously** by [`assert_emitted`]: a
//! cross-tier comparison alone also passes for an implementation that deopts every
//! instruction (a deopt is executed by the interpreter on *both* sides).

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_common::Result as BeamResult;
use beam_loader::{Instr, Label, Module, Operand};
use beam_term::{Atom, Eterm};

use super::{skip_utf32_shape, SHAPE_SKIP_UTF32};
use crate::context::MinimalContext;
use crate::interpreter::build_label_map;
use crate::jit::baseline::bitstr::bitstr_calls;
use crate::jit::baseline::bitstr_inline_tests::{
    bitstr_module, emitted_shapes_for, sentinel_block, static_deopts, visited_instances,
};
use crate::jit::baseline::tests::assert_same_outcome;
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};
use crate::terms::make_heap_binary;

// ---------------------------------------------------------------------------
// The subjects
// ---------------------------------------------------------------------------

/// `U+0041` (a BMP scalar) as the four big-endian bytes of a UTF-32 field, plus a tail byte so
/// the readable-bounds guard has slack on both sides.
const BMP: &[u8] = &[0x00, 0x00, 0x00, 0x41, b'X'];

/// `U+1F600` (an astral scalar, U+1F600 GRINNING FACE) plus a tail.
const ASTRAL: &[u8] = &[0x00, 0x01, 0xF6, 0x00, b'X'];

/// `0x00110000` — one past `U+10FFFF`, i.e. **not** a Unicode scalar.
const TOO_BIG: &[u8] = &[0x00, 0x11, 0x00, 0x00, b'X'];

/// `0xD800` — the first UTF-16 surrogate, which UTF-32 rejects.
const SURROGATE: &[u8] = &[0x00, 0x00, 0xD8, 0x00, b'X'];

/// Three bytes: a field that cannot hold 32 bits, so the `remaining` check fires.
const TRUNCATED: &[u8] = &[0x00, 0x00, 0x41];

/// The position the commit leaves behind after one 32-bit character.
const AFTER_ONE: i64 = 32;

// ---------------------------------------------------------------------------
// The modules under test
// ---------------------------------------------------------------------------

/// `f/1` = `bs_start_match3` [`bs_skip_bits2`] `-> bs_skip_utf32 -> bs_get_position`, with the
/// fail label answering [`sentinel_block`]'s `-1`.
///
/// The success answer is the context's **position**, so "did the commit run" and "did it run
/// once" are both observable through the returned value, and "did the fail branch run" is the
/// distinguishable `-1`.
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
        code.push(Instr::BsSkipUtf32 {
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
            Instr::BsSkipUtf32 {
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
fn assert_emitted(label: &str, module: &Module) {
    let (emitted, snapshot) = emitted_shapes_for(module, 0);
    println!(
        "{label}: inlined(bs_skip_utf32) emitted={} variant_instances={} static_deopts={}",
        emitted.contains(&SHAPE_SKIP_UTF32),
        visited_instances(&snapshot, "BsSkipUtf32"),
        static_deopts(&snapshot, "BsSkipUtf32"),
    );
    assert!(
        emitted.contains(&SHAPE_SKIP_UTF32),
        "{label}: the cache-free compile must emit the `bs_skip_utf32` arm \
         (emitted {emitted:?}, wanted {SHAPE_SKIP_UTF32})"
    );
    assert!(
        visited_instances(&snapshot, "BsSkipUtf32") >= 1,
        "{label}: `BsSkipUtf32` must be the census's variant name, else the deopt assertion \
         below is vacuous (visited {:?})",
        snapshot.instr_instances,
    );
    assert_eq!(
        static_deopts(&snapshot, "BsSkipUtf32"),
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

// ---------------------------------------------------------------------------
// Rows: the scalar decodes and the position advances by 32 bits
// ---------------------------------------------------------------------------

/// **A BMP and an astral scalar both decode and commit `start += 32`.**
///
/// The two rows are the same emitted sequence over different payloads: `U+0041` exercises the
/// three leading all-zero bytes and `U+1F600` a value whose top byte is still zero but whose
/// middle bytes are not — i.e. the `shl`/`or` assembly is checked on more than one shape of
/// input. Both must leave the context at bit `32`.
#[test]
fn a_bmp_and_an_astral_scalar_advance_the_position_by_32_bits() {
    for (name, subject) in [
        ("bs_skip_utf32/bmp", BMP),
        ("bs_skip_utf32/astral", ASTRAL),
    ] {
        let module = plain_skip_module("inline_bs_skip_utf32");
        assert_emitted(name, &module);
        one_row(name, &module, subject, AFTER_ONE);
    }
}

// ---------------------------------------------------------------------------
// Rows: the two validity tests and the truncation check take the FAIL label
// ---------------------------------------------------------------------------

/// **Every rejection is the fail label — never a trap, never a fallback, never a commit.**
///
/// Three rows, one per rejection in the emitted sequence:
///
/// * `0x00110000` — `cmp value, 0x10FFFF` / `ja fail_arm`;
/// * `0xD800` — the `value - 0xD800 <= 0x7FF` surrogate range test / `jbe fail_arm`;
/// * a three-byte subject — the saturating `remaining < 32` check / `jb fail_arm`.
///
/// All three answer the sentinel block's `-1` on both tiers. That the position is **not** left
/// at `32` is exactly what the `-1` proves: no commit ran on these paths.
#[test]
fn a_non_scalar_a_surrogate_and_a_truncated_field_all_take_the_fail_label() {
    let module = plain_skip_module("inline_bs_skip_utf32");
    assert_emitted("bs_skip_utf32/failures", &module);
    for (name, subject) in [
        ("bs_skip_utf32/above-u10ffff", TOO_BIG),
        ("bs_skip_utf32/surrogate", SURROGATE),
        ("bs_skip_utf32/truncated", TRUNCATED),
    ] {
        one_row(name, &module, subject, -1);
    }
}

/// **The boundary scalars are accepted.** `U+10FFFF` (the largest scalar) and `U+D7FF` /
/// `U+E000` (the code points immediately outside the surrogate range) are all valid, so a
/// range test with an off-by-one at either edge would fail here while the rows above still
/// passed.
#[test]
fn the_largest_scalar_and_the_range_edges_are_accepted() {
    let module = plain_skip_module("inline_bs_skip_utf32");
    assert_emitted("bs_skip_utf32/range edges", &module);
    for (name, subject) in [
        ("bs_skip_utf32/max scalar", &[0x00, 0x10, 0xFF, 0xFF, b'X'][..]),
        ("bs_skip_utf32/below surrogates", &[0x00, 0x00, 0xD7, 0xFF, b'X'][..]),
        ("bs_skip_utf32/above surrogates", &[0x00, 0x00, 0xE0, 0x00, b'X'][..]),
    ] {
        one_row(name, &module, subject, AFTER_ONE);
    }
}

// ---------------------------------------------------------------------------
// The idempotency pin: the legitimate deopt
// ---------------------------------------------------------------------------

/// The bits `start .. start + nbits` of `subject`, read big-endian — the same field
/// `decode_utf32` reads, so the row's subject can be asserted rather than assumed.
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
/// the arm writes **last**, after every guard and after the four loads and the two validity
/// tests (which mutate nothing) — so a rejection taken anywhere above cannot double-advance the
/// position.
///
/// The row that proves it is a **non-byte-aligned `start`**: a preceding 36-bit `bs_skip_bits2`
/// leaves `start == 36`, so the emitted `test <start>, 7` / `jne fallback` guard fires on every
/// execution and the family's own bit-by-bit reader answers instead. The subject is built so
/// the field at bits `36..68` is exactly `U+0041`, and the answer — the context position after
/// the skip — must be `68`. A double commit would answer `100`; a missing commit would answer
/// `36`; the family's own reader is what makes `68` the only right answer.
#[test]
fn a_non_byte_aligned_start_deopts_to_exactly_one_interpreter_step() {
    let mut subject = [0u8; 10];
    // Set the field at bits 36..68 to 0x00000041 by construction, then prove it.
    subject[7] = 0x04; // field bits 25..; the low-nibble position of byte 7
    subject[8] = 0x10; // field bit 31 (LSB), i.e. bit 4 of byte 8
    assert_eq!(
        field_bits(&subject, 36, 32),
        0x41,
        "the row's subject must carry U+0041 at bits 36..68"
    );

    let module = skip_module("inline_bs_skip_utf32_misaligned", Some(36), 0, Label(3));
    // The arm still *emits* for this module — the alignment test is a run-time guard, not a
    // compile-time predicate — which is what makes the row a deopt rather than a refusal.
    assert_emitted("bs_skip_utf32/misaligned start", &module);

    let before = bitstr_calls();
    let observed = with_both_tiers_bytes(&module, &subject, |interpreter, baseline| {
        assert_same_outcome(
            "bs_skip_utf32/misaligned start",
            interpreter,
            baseline,
            &module.atom_table,
        );
        returned_i64("bs_skip_utf32/misaligned start", baseline)
    });
    let deopts = bitstr_calls().saturating_sub(before);
    println!("bs_skip_utf32/misaligned start: answer={observed} jit_bitstr_calls={deopts}");
    assert_eq!(
        observed,
        68,
        "the character must be consumed exactly once from a bit offset of 36 (a double commit \
         would answer 100, a missing one 36)"
    );
    // The fallback is exactly one interpreter step (`emit_step_and_continue` -> `jit_step_one`),
    // not a `jit_bitstr` call, so that counter is not the instrument for this row: the semantic
    // pin above is stronger (only the family's byte-wise reader can produce 68 from a
    // non-aligned window).
    assert_eq!(
        deopts, 0,
        "the inline fallback is one interpreter step, not a `jit_bitstr` call (got {deopts})"
    );
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
    let module = y_skip_module("inline_bs_skip_utf32_y", 0);
    assert_emitted("bs_skip_utf32/Y context", &module);
    let observed = with_both_tiers_bytes(&module, BMP, |interpreter, baseline| {
        assert_same_outcome(
            "bs_skip_utf32/Y context",
            interpreter,
            baseline,
            &module.atom_table,
        );
        returned_i64("bs_skip_utf32/Y context", baseline)
    });
    assert_eq!(observed, AFTER_ONE, "a Y-slot context must commit the same 32 bits");
}

/// **An out-of-frame `Y` context rejects to the fallback and agrees on the error.**
///
/// The block allocates a **one-slot** frame and the instruction names `y(5)`, so the emitted
/// guard's `cmp addr, y_end` / `jae fallback` fires on every execution. The interpreter's own
/// `y(5)` bounds check then produces its exact error, which both tiers must report identically —
/// never an out-of-frame dereference and never a fabricated value.
#[test]
fn an_out_of_frame_y_context_rejects_and_agrees_on_the_error() {
    let module = y_skip_module("inline_bs_skip_utf32_y_out", 5);
    assert_emitted("bs_skip_utf32/out-of-frame Y", &module);
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
            "bs_skip_utf32/out-of-frame Y",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

// ---------------------------------------------------------------------------
// Rows: the compile-time refusals
// ---------------------------------------------------------------------------

/// **The two compile-time refusals emit zero bytes.**
///
/// * a `NO_FAIL` fail code — the miss path *raises* `{badmatch, Subject}`, which allocates, so
///   the shape stays on the family (and the family's raise is the row below);
/// * a `-little` (`BSF_LITTLE`, `2`) or `-native` (`16`) flags word — the arm emits the
///   big-endian read only, so a byte-swapped field is the family's own reader.
///
/// The refusal is total: the shape predicate is `None`, so the arm's counter cannot move and the
/// instruction statically deopts to `emit_bitstr`.
#[test]
fn the_no_fail_code_and_the_little_endian_flag_word_are_refused() {
    let module = plain_skip_module("inline_bs_skip_utf32");
    let labels = build_label_map(&module);
    // The inlinable form, as the row's own control.
    assert!(
        skip_utf32_shape(&labels, Label(3), &Operand::X(1), 0).is_some(),
        "the plain big-endian form must be inlined"
    );
    assert!(
        skip_utf32_shape(&labels, beam_loader::NO_FAIL, &Operand::X(1), 0).is_none(),
        "a NO_FAIL fail code allocates its raise, so it must be refused"
    );
    for (flags, what) in [(2u32, "BSF_LITTLE"), (16, "BSF_NATIVE"), (18, "both")] {
        assert!(
            skip_utf32_shape(&labels, Label(3), &Operand::X(1), flags).is_none(),
            "a {what} flags word must be refused (flags {flags})"
        );
    }
    // An unresolved fail label keeps the family's exact `unknown_label`.
    assert!(
        skip_utf32_shape(&labels, Label(9_999), &Operand::X(1), 0).is_none(),
        "an unresolved fail label must be refused"
    );

    // And the refusal is what the emitter does: a `NO_FAIL` module emits **zero bytes** for the
    // shape, so the instruction stays on the family's own arm (`emit_bitstr`), while both tiers
    // still agree. (`emit_bitstr` is the claimed-shape route, not the `_` deopt arm, so the
    // static-deopt census is not the instrument here — the absent emission is.)
    let no_fail = skip_module("inline_bs_skip_utf32_no_fail", None, 0, beam_loader::NO_FAIL);
    let (emitted, snapshot) = emitted_shapes_for(&no_fail, 0);
    assert!(
        !emitted.contains(&SHAPE_SKIP_UTF32),
        "a NO_FAIL instruction must not emit the arm (emitted {emitted:?})"
    );
    assert!(
        visited_instances(&snapshot, "BsSkipUtf32") >= 1,
        "the refused instruction must still be the census's variant name (visited {:?})",
        snapshot.instr_instances,
    );
    // The truncated subject makes the family raise `{badmatch, Subject}` — an allocating
    // reason, which is exactly why the arm refuses this fail code — and both tiers must report
    // the same class and reason.
    with_both_tiers_bytes(&no_fail, TRUNCATED, |interpreter, baseline| {
        assert_same_outcome(
            "bs_skip_utf32/NO_FAIL raise",
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

// ---------------------------------------------------------------------------
// The live oracle cross-check
// ---------------------------------------------------------------------------

/// The oracle module: the same character skip written in Erlang source, plus a row that reports
/// the **position** the skip left behind (`<<_/utf32>>`'s remaining bit count).
const ORACLE_SRC: &str = "\
-module(utf32match).
-export([skip/1, advance/1]).

skip(Bin) ->
    case Bin of
        <<_/utf32, _/binary>> -> 32;
        _ -> -1
    end.

advance(Bin) ->
    case Bin of
        <<_/utf32, Rest/binary>> -> byte_size(Bin) * 8 - byte_size(Rest) * 8;
        _ -> -1
    end.
";

/// Compile [`ORACLE_SRC`] into a per-`tag` temp dir and return that dir. `None` means `erlc`
/// could not be **spawned** (the caller skips loudly).
fn compile_oracle(tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_engine_utf32_{tag}"));
    std::fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    std::fs::write(dir.join("utf32match.erl"), ORACLE_SRC)
        .unwrap_or_else(|error| panic!("write utf32match.erl: {error}"));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("utf32match.erl")
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling utf32match.erl\n  stderr: {}",
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
/// the *language*. Two oracle functions: `skip/1` (match or not, `32` / `-1`) and `advance/1`
/// (the bit count the skip consumed), evaluated by the system `erl` on the same subject bytes.
#[test]
fn every_row_agrees_with_the_live_erl_oracle() {
    let Some(dir) = compile_oracle("rows") else {
        eprintln!("SKIP: no usable `erlc` on PATH -- bs_skip_utf32 oracle cross-check skipped");
        return;
    };
    let module = plain_skip_module("inline_bs_skip_utf32");
    let rows: [&[u8]; 6] = [BMP, ASTRAL, TOO_BIG, SURROGATE, TRUNCATED, &[0x00, 0x10, 0xFF, 0xFF, b'X']];

    let mut checked = 0usize;
    for (function, expected_kind) in [("skip", "match"), ("advance", "bits")] {
        for subject in rows {
            let label = format!("oracle utf32match:{function}/1 on {subject:?}");
            let ours = with_both_tiers_bytes(&module, subject, |interpreter, baseline| {
                assert_same_outcome(&label, interpreter, baseline, &module.atom_table);
                returned_i64(&label, baseline)
            });
            let eval = format!(
                "io:format(\"~p\", [utf32match:{function}(<<{}>>)]), halt(0).",
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
                    // The oracle's own answer must mean what the row expects: a match is 32,
                    // and a successful `advance/1` is exactly 32 too.
                    if function == "skip" {
                        assert!(
                            theirs == AFTER_ONE || theirs == -1,
                            "{label}: a `{expected_kind}` answer is 32 or -1, got {theirs}"
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
    assert_eq!(checked, 2 * rows.len(), "every oracle row must have been checked");
}
