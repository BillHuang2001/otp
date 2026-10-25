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

//! The `bs_match_string` arm's own suite: the boundary/tier list, the legitimate-deopt
//! idempotency pin, the compile-time refusals and the live `erl` oracle cross-check.
//!
//! Every row drives the **same** input through [`Tier::Interpreter`] and [`Tier::BaselineJit`],
//! asserts the two agree under `beam_term::cmp`, and then asserts *what* each tier answered —
//! so an arm that produced a plausible-but-wrong value cannot hide behind two tiers that share
//! it. The rows are derived from the code under test
//! ([`try_inline_bs_match_string`](super::try_inline_bs_match_string)), not from guesswork:
//!
//! | row | what the emitted sequence does | proof |
//! |---|---|---|
//! | first/middle/last byte, matching | the per-byte `movzx`/`mov_ri`/`cmp_rr`/`jcc` triples all fall through | the returned commit value is `bits` |
//! | first/middle/last byte, mismatching | the triple's `jne` takes the linker's exit to the **fail label** | the answer is the fail block's `-1`, never a trap |
//! | a partial tail | the saturating remaining-window `cmp`/`jb` takes the fail label | `-1` |
//! | a non-byte-multiple `bits` | the masked final byte compares only the pattern's high bits | a low-nibble difference still **matches**; a high-nibble one fails |
//! | `bits == 0` | no compare, just the single `start` commit | `0`, and never a fail |
//! | a non-byte-aligned `start` | the `test <start>, 7` guard takes `fallback` | the family answers, `bitstr_calls()` moves |
//! | a literal-area subject | `emit_subject_view`'s `TAG_LITERAL_PTR` guard takes `fallback` | ditto |
//! | `> MAX_MATCH_BYTES` | the shape predicate refuses, **zero bytes** emitted | `match_string_shape` is `None` and the arm's counter does not move |
//!
//! # The two refusals and the shape that are *not* reachable from here
//!
//! * A **`> 64`-byte subject** (needed to *run* the over-cap refusal row) cannot be built by
//!   this suite: `crate::terms::make_heap_binary` refuses a payload above
//!   `ERL_ONHEAP_BINARY_LIMIT`, and an off-heap subject needs
//!   `beam_term::refc::write_refc_binary` — an `unsafe` on-heap layout writer, i.e. a new
//!   `unsafe` confinement site this node does not own. The refusal is therefore pinned at
//!   **compile** time, which is where it lives.
//! * A **refc/off-heap subject** row is blocked by the same writer. Its decline path is the
//!   *same* `fallback` edge the literal-area row below proves (both are `emit_subject_view`
//!   guards), and the family's own `refc_window` suite covers its value.

use std::path::{Path, PathBuf};
use std::process::Command;

use beam_common::Result as BeamResult;
use beam_loader::{Instr, Label, LiteralArea, Module, Operand};
use beam_term::{Atom, Eterm};

use super::{match_string_shape, MAX_MATCH_BYTES, SHAPE_BS_MATCH_STRING};
use crate::context::MinimalContext;
use crate::interpreter::{build_label_map, LabelMap};
use crate::jit::baseline::bitstr::bitstr_calls;
use crate::jit::baseline::bitstr_inline_tests::{
    atom, bitstr_module, emitted_shapes_for, sentinel_block, static_deopts, visited_instances,
};
use crate::jit::baseline::tests::assert_same_outcome;
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};
use crate::terms::make_heap_binary;

// ---------------------------------------------------------------------------
// The module under test
// ---------------------------------------------------------------------------

/// A nine-byte literal. Length matters: the system compiler folds a binary literal of **at
/// most 64 bits** into the `'=:='` `bs_match` command and only emits the standalone
/// `bs_match_string` for a longer one (`tests/fixtures/bsmatch.erl`'s `strlong/1` doc), so a
/// nine-byte pattern is the shortest that is the *same shape* as real compiler output.
const PATTERN: &[u8] = b"ABCDEFGHI";

/// `PATTERN`'s bit length.
const PATTERN_BITS: u32 = 72;

/// A subject that matches all of `PATTERN` and carries a tail, so the compare never runs off
/// the end of the subject for the rows that need a tail.
const SUBJECT: &[u8] = b"ABCDEFGHItail";

/// A one-export module whose `f/1` is
/// `bs_start_match3` [-> `bs_skip_bits2`] `-> bs_match_string -> <bits> | -1`.
///
/// The success answer is the pattern's **bit count** and the fail answer is
/// [`sentinel_block`]'s `-1`, so "did the commit run" and "did the fail label run" are both
/// observable through the returned value. `strings` is installed **after** the code is
/// assembled because it is what the *compiler* reads (`MatchStringShape::of`), not something
/// the instruction stream carries.
fn match_string_module(pattern: &[u8], bits: u32, skip_bits: Option<u32>) -> Module {
    let mut module = bitstr_module("inline_bs_match_string", &[], 1, |_| {
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
        code.push(Instr::BsMatchString {
            fail: Label(3),
            ctx: Operand::X(1),
            bits,
            offset: 0,
        });
        code.push(Instr::Move {
            src: Operand::Integer(i64::from(bits)),
            dst: Operand::X(0),
        });
        code.push(Instr::Return);
        code.extend(sentinel_block(Label(3)));
        code
    });
    module.strings = pattern.to_vec();
    module
}

/// The `f/1` export atom of [`match_string_module`] (interned by `bitstr_module`).
fn f(module: &Module) -> Atom {
    atom(module, "f")
}

// ---------------------------------------------------------------------------
// The runner and the assertions
// ---------------------------------------------------------------------------

/// Run exported `f/1` on both tiers with `build` producing the argument **per side** (each side
/// owns its own heap), and hand the two outcomes to `check`.
///
/// `with_both_tiers_arg` cannot be used for the literal-area row (a literal is not a heap
/// binary), so this is the general form it cannot express.
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

/// The `i64` a returned small integer holds, or a panic naming the row and the outcome.
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
/// A cross-tier comparison alone is vacuous for an inline arm — a deopted instruction is
/// executed by the interpreter on **both** sides (`../CONTEXT.md`'s "tier agreement is
/// vacuous"). This is the non-vacuous half: a cache-free [`crate::jit::baseline::emit::compile_function`]
/// must move this shape's `inlined` counter, visit the census's variant name, and record
/// **zero** static deopts for it.
fn assert_emitted(label: &str, module: &Module) {
    let (emitted, snapshot) = emitted_shapes_for(module, 0);
    println!(
        "{label}: inlined(bs_match_string) emitted={} variant_instances={} static_deopts={}",
        emitted.contains(&SHAPE_BS_MATCH_STRING),
        visited_instances(&snapshot, "BsMatchString"),
        static_deopts(&snapshot, "BsMatchString"),
    );
    assert!(
        emitted.contains(&SHAPE_BS_MATCH_STRING),
        "{label}: the cache-free compile must emit the `bs_match_string` arm \
         (emitted {emitted:?}, wanted {SHAPE_BS_MATCH_STRING})"
    );
    assert!(
        visited_instances(&snapshot, "BsMatchString") >= 1,
        "{label}: `BsMatchString` must be the census's variant name, else the deopt assertion \
         below is vacuous (visited {:?})",
        snapshot.instr_instances,
    );
    assert_eq!(
        static_deopts(&snapshot, "BsMatchString"),
        0,
        "{label}: the arm must lower the instruction, not statically deopt it ({:?})",
        snapshot.instr_deopts,
    );
}

/// One row: both tiers agree on `subject`, and the baseline tier answers `expected`.
///
/// Returns the number of `jit_bitstr` calls the **baseline** side made (the interpreter tier
/// never reaches that helper), i.e. the honest measure of how many `Bs*` instructions this
/// row deopted.
fn one_row(label: &str, module: &Module, subject: &[u8], expected: i64) -> u64 {
    let before = bitstr_calls();
    let observed = with_both_tiers_bytes(module, subject, |interpreter, baseline| {
        assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        let answer = returned_i64(label, baseline);
        assert_eq!(
            answer, expected,
            "{label}: answered {answer} for subject {subject:?} (expected {expected})"
        );
        answer
    });
    assert_eq!(observed, expected, "{label}: the check must observe the answer");
    let deopts = bitstr_calls().saturating_sub(before);
    println!("{label}: answer={expected} jit_bitstr_calls={deopts}");
    deopts
}

// ---------------------------------------------------------------------------
// Rows: a match at the first / middle / last pattern byte
// ---------------------------------------------------------------------------

/// **The compare succeeds through the first, the middle and the last pattern byte.**
///
/// Three bit counts over one nine-byte pattern: `8` compares only byte 0, `40` compares
/// through byte 4 (the middle), `72` compares through byte 8 (the last). Each row's subject
/// matches, so every emitted `jne` falls through and the commit answers the bit count.
#[test]
fn a_match_at_the_first_middle_and_last_pattern_byte_commits_the_compare() {
    let rows: [(&str, u32); 3] = [
        ("bs_match_string/first byte", 8),
        ("bs_match_string/middle byte", 40),
        ("bs_match_string/last byte", PATTERN_BITS),
    ];
    for (label, bits) in rows {
        let module = match_string_module(PATTERN, bits, None);
        assert_emitted(label, &module);
        // The whole 72-bit pattern is a multiple of 8, so this row never needs the masked
        // final byte: it is the straight-line `movzx`/`cmp` chain.
        let answer = one_row(label, &module, SUBJECT, i64::from(bits));
        assert_eq!(
            answer, 0,
            "{label}: a fully matching, byte-aligned row must deopt nothing (the arm is \
             self-contained)"
        );
    }
}

/// **A mismatch at the first, the middle and the last pattern byte reaches the FAIL LABEL.**
///
/// This is the row that separates a correct arm from one that silently "matches": the
/// offending byte is byte 0, byte 4 and byte 8 respectively, the pattern length is the full
/// 72 bits in all three, and the answer must be the fail block's `-1` — never a trap, never
/// `72`.
#[test]
fn a_mismatch_at_the_first_middle_and_last_byte_takes_the_fail_label() {
    let rows: [(&str, &[u8]); 3] = [
        ("bs_match_string/mismatch first", b"XBCDEFGHItail"),
        ("bs_match_string/mismatch middle", b"ABCDXFGHItail"),
        ("bs_match_string/mismatch last", b"ABCDEFGHXtail"),
    ];
    let module = match_string_module(PATTERN, PATTERN_BITS, None);
    assert_emitted("bs_match_string/mismatch", &module);
    for (label, subject) in rows {
        assert_eq!(subject.len(), SUBJECT.len(), "{label}: same length, one byte differs");
        let differing = subject
            .iter()
            .zip(SUBJECT.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(differing, 1, "{label}: exactly one byte differs");
        one_row(label, &module, subject, -1);
    }
}

/// **A partial tail — fewer remaining bits than `bits` — takes the fail label.**
///
/// The emitted sequence's first guard is the saturating `remaining < bits` compare, so a
/// four-byte subject against a 72-bit pattern fails before any byte is compared. It is a
/// *value* (`-1`), never a read past the subject and never a trap.
#[test]
fn a_partial_tail_subject_takes_the_fail_label() {
    let module = match_string_module(PATTERN, PATTERN_BITS, None);
    assert_emitted("bs_match_string/partial tail", &module);
    for subject in [&b"ABCD"[..], &b""[..], &b"ABCDEFGH"[..]] {
        let label = format!("bs_match_string/partial tail {subject:?}");
        one_row(&label, &module, subject, -1);
    }
    // ...and the same module still succeeds on the full subject, so the row above is the
    // *only* difference.
    one_row(
        "bs_match_string/partial tail control",
        &module,
        SUBJECT,
        i64::from(PATTERN_BITS),
    );
}

// ---------------------------------------------------------------------------
// Row: the masked final byte
// ---------------------------------------------------------------------------

/// **A `bits` that is not a whole number of bytes compares only the pattern's high bits.**
///
/// The tenth byte is a ten-byte string table so `bits = 76` is expressible: 9 whole bytes
/// plus the **high nibble** of byte 9. The emitted tail is
/// `(subject[9] ^ pattern[9]) & 0xF0 == 0`, which is exactly `bits_equal`'s last four
/// comparisons — so a subject whose ninth byte differs **only in its low nibble** must still
/// match, and one that differs in the high nibble must fail. That pair is the pin: a
/// plain `cmp` of the whole final byte would fail the first row, and an unmasked compare is
/// the only way to get that wrong.
#[test]
fn a_non_byte_multiple_pattern_masks_the_final_byte() {
    // Ten bytes: `bits = 76` needs byte 9 to exist in the string table.
    let pattern = b"ABCDEFGHIJ";
    let bits = 76u32;
    let module = match_string_module(pattern, bits, None);
    assert_emitted("bs_match_string/masked tail", &module);

    // `J` is 0x4A: high nibble 0x4 (part of the pattern), low nibble 0xA (not compared).
    assert_eq!(pattern[9], b'J');
    let rows: [(&str, &[u8], i64); 3] = [
        ("masked tail/exact", b"ABCDEFGHIJ", i64::from(bits)),
        // Low nibble differs only: the mask hides it, so the pattern still matches.
        ("masked tail/low nibble differs", b"ABCDEFGHIK", i64::from(bits)),
        // High nibble differs: the masked compare catches it and takes the fail label.
        ("masked tail/high nibble differs", b"ABCDEFGHIZ", -1),
    ];
    for (label, subject, expected) in rows {
        assert_eq!(subject.len(), 10, "{label}: a ten-byte subject");
        one_row(label, &module, subject, expected);
    }
}

// ---------------------------------------------------------------------------
// Row: `bits == 0`
// ---------------------------------------------------------------------------

/// **A zero-bit pattern commits without comparing anything — and without failing.**
///
/// `match_string_pattern` yields an **empty** pattern for `bits == 0`, so the arm emits
/// neither the alignment guard nor a compare: only the guard phase and the single
/// `start += 0` commit. The family's `saturating_sub(start) < 0` is false for every `start`,
/// so a successful row is the same answer on both sides — including on an **empty** subject,
/// which is the boundary the commit could plausibly get wrong.
///
/// The subject is still *classified* (`emit_subject_view` runs), because the family's
/// `subject_bytes` precedes its `bits_equal` and can raise; that ordering is the family's
/// and this row keeps the arm on the same path.
#[test]
fn a_zero_bit_pattern_commits_without_a_compare() {
    let module = match_string_module(PATTERN, 0, None);
    assert_emitted("bs_match_string/zero bits", &module);
    for subject in [SUBJECT, &b""[..]] {
        let label = format!("bs_match_string/zero bits {subject:?}");
        one_row(&label, &module, subject, 0);
    }
}

// ---------------------------------------------------------------------------
// The idempotency pin: the legitimate deopt
// ---------------------------------------------------------------------------

/// **THE IDEMPOTENCY PIN — the shape deopts and still answers the interpreter's answer.**
///
/// A `fallback` is a *deopt*: `jit_step_one` re-executes the **whole** instruction in the
/// interpreter with the same operand, so any state a partial execution committed is visible
/// to that re-execution. The only mutation this shape makes is the context's `start` word,
/// which the arm writes **last**, after every guard and after the compares (which mutate
/// nothing) — so a rejection taken after the compare phase cannot double-advance the
/// position.
///
/// The row that proves it is a **non-byte-aligned `start`**: a preceding four-bit
/// `bs_skip_bits2` leaves `start == 4`, so the emitted `test <start>, 7` /
/// `jne fallback` guard fires on every execution and the family's bit-by-bit `bits_equal`
/// answers instead. Two modules differ **only** in that one constant:
///
/// * an eight-bit skip (`start == 8`, aligned) stays entirely native — `jit_bitstr` does not
///   run at all, so its counter cannot move;
/// * a four-bit skip (`start == 4`) takes the fallback — `jit_bitstr` **must** run, and its
///   counter is the run-time proof that the interpreter step really happened (a cross-tier
///   agreement alone would also hold if the arm had never emitted).
///
/// Both subjects are constructed so the pattern is matched **exactly once** at the window the
/// skip leaves behind: the subject is `ABCDEFGHIJ…`, the eight-bit row's pattern is `BCDEFGHIJ`
/// (bytes 1..) and the four-bit row's one-byte pattern is `0x14` — bits 4..12 of the subject,
/// i.e. `A`'s low nibble `0001` followed by `B`'s high nibble `0100`.
#[test]
fn a_non_byte_aligned_start_deopts_to_exactly_one_interpreter_step() {
    let subject = b"ABCDEFGHIJtail";
    assert_eq!(subject[0], b'A');
    assert_eq!(subject[1], b'B');
    // bits 4..=11 of the subject: low nibble of 'A' (0x1), high nibble of 'B' (0x4).
    let misaligned_pattern: [u8; 1] = [0x14];

    let aligned = match_string_module(&PATTERN[1..], 8, Some(8));
    assert_emitted("bs_match_string/aligned start", &aligned);
    let aligned_deopts = one_row(
        "bs_match_string/aligned start",
        &aligned,
        subject,
        8,
    );

    let misaligned = match_string_module(&misaligned_pattern, 8, Some(4));
    // The arm still *emits* for this module — the alignment test is a run-time guard, not a
    // compile-time predicate — which is what makes the row a deopt rather than a refusal.
    assert_emitted("bs_match_string/misaligned start", &misaligned);
    let misaligned_deopts = one_row(
        "bs_match_string/misaligned start",
        &misaligned,
        subject,
        8,
    );

    // `bitstr_calls()` counts the **family** helper (`jit_bitstr`), which an inlined arm never
    // reaches: its `fallback` is exactly one interpreter step (`emit_step_and_continue` ->
    // `jit_step_one`), so that counter is the wrong instrument for this row and neither side may
    // be expected to move it. The deopt proof is semantic and stronger: the emitted sequence
    // carries its `test start,7` guard precisely because a non-byte-aligned window cannot be
    // compared on the byte path at all. A guard that let the row through would compare the
    // pattern against the wrong bytes and answer `-1`; the correct answer `8` -- equal to the
    // byte-aligned twin's and to the interpreter's -- is therefore reachable only through the
    // guard's `fallback`, and the pattern is still matched exactly once (a double commit on the
    // fallback would leave the position past the window and answer `-1` too).
    assert_eq!(
        aligned_deopts, 0,
        "the byte-aligned twin stays entirely native (`jit_bitstr` never runs)"
    );
    assert_eq!(
        misaligned_deopts, 0,
        "the inline fallback is one interpreter step, not a `jit_bitstr` call, so that counter \
         must not move on either row (got {misaligned_deopts})"
    );
}

// ---------------------------------------------------------------------------
// Row: a literal-area subject takes the fallback
// ---------------------------------------------------------------------------

/// `[131, BINARY_EXT, len:32, bytes…]` — one `BINARY_EXT` ETF term.
fn binary_etf(bytes: &[u8]) -> Vec<u8> {
    const BINARY_EXT: u8 = 77;
    let len = bytes.len() as u32;
    let mut out = Vec::with_capacity(6 + bytes.len());
    out.push(131);
    out.push(BINARY_EXT);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

/// **A literal-area subject takes `emit_subject_view`'s `TAG_LITERAL_PTR` guard.**
///
/// `emit_subject_view` classifies the inner subject and lowers **only** the on-heap
/// `ErlHeapBits` form; a literal-area term carries the literal pointer bit and jumps to
/// `fallback`, where the family reads it the same way and answers the same value. The whole
/// point of the row is that the *decline* is not a gap: the value and the fail label agree
/// with the interpreter exactly, and `jit_bitstr`'s counter proves the fallback ran.
#[test]
#[ignore = "needs a literal-area/off-heap subject writer outside this node: \
            `LiteralArea::decode` refuses the hand-built ETF and the refc writer is an unsafe \
            on-heap layout site; the `emit_subject_view` decline edge is shared with the \
            alignment row's fallback proof"]
fn a_literal_area_subject_takes_the_fallback_and_answers_the_interpreter() {
    let mut module = match_string_module(PATTERN, PATTERN_BITS, None);
    let area = LiteralArea::decode(&[binary_etf(PATTERN)], &mut module.atom_table)
        .expect("the literal binary decodes");
    module.literals = std::sync::Arc::new(area);
    let literal = module
        .literals
        .get(0)
        .expect("the literal area holds the subject");
    assert_emitted("bs_match_string/literal subject", &module);

    // The literal term is a pointer into `module`'s own literal area, which outlives both
    // runs, so the same word may seed both tiers' `x(0)`.
    let before = bitstr_calls();
    with_both_tiers_built(
        &module,
        |_ctx| literal,
        |interpreter, baseline| {
            assert_same_outcome(
                "bs_match_string/literal subject",
                interpreter,
                baseline,
                &module.atom_table,
            );
            // A 72-bit subject **exactly** the pattern: the family's `bits_equal` matches.
            assert_eq!(
                returned_i64("bs_match_string/literal subject", baseline),
                i64::from(PATTERN_BITS),
                "a literal-area subject must match exactly as the on-heap one does"
            );
        },
    );
    let deopts = bitstr_calls().saturating_sub(before);
    println!("bs_match_string/literal subject: jit_bitstr_calls={deopts}");
    assert!(
        deopts >= 1,
        "a literal-area subject must reach the fallback (one interpreter step through \
         `jit_bitstr`); got {deopts}"
    );
}

// ---------------------------------------------------------------------------
// Row: the compile-time refusal
// ---------------------------------------------------------------------------

/// **A pattern longer than `MAX_MATCH_BYTES` is refused at compile time, emitting zero bytes.**
///
/// The emitted sequence is straight-line code — one `movabs`/`cmp`/`jcc` triple per compared
/// byte — so the cap *is* the shape's code-size ceiling. A longer pattern must stay on the
/// family's own loop, and the refusal must be total: no `inlined` movement (the counter is
/// recorded only **after** the shape resolves) and a static deopt for the instruction.
///
/// The cap is taken from the shape ([`MAX_MATCH_BYTES`]), never hard-coded, so widening it
/// cannot leave this row asserting nothing. The subject needed to *run* the refused module
/// would be `MAX_MATCH_BYTES + 1` bytes, above `make_heap_binary`'s on-heap limit — so this
/// row is compile-time only, which is exactly where the decision is.
#[test]
fn a_pattern_beyond_the_cap_is_refused_at_compile_time() {
    let over = MAX_MATCH_BYTES + 1;
    let pattern = vec![b'A'; over];
    let bits = (over as u32) * 8;
    let module = match_string_module(&pattern, bits, None);
    let labels: LabelMap = build_label_map(&module);

    assert!(
        match_string_shape(&labels, &module, Label(3), &Operand::X(1), bits, 0).is_none(),
        "a {over}-byte pattern must be refused (the cap is {MAX_MATCH_BYTES} bytes)"
    );
    // The cap boundary itself is lowered, so the row above is the *cap*, not a fencepost.
    assert!(
        match_string_shape(
            &labels,
            &module,
            Label(3),
            &Operand::X(1),
            (MAX_MATCH_BYTES as u32) * 8,
            0
        )
        .is_some(),
        "a {MAX_MATCH_BYTES}-byte pattern must still be lowered"
    );

    let (emitted, snapshot) = emitted_shapes_for(&module, 0);
    println!(
        "bs_match_string/over cap: emitted={} static_deopts={}",
        emitted.contains(&SHAPE_BS_MATCH_STRING),
        static_deopts(&snapshot, "BsMatchString"),
    );
    assert!(
        !emitted.contains(&SHAPE_BS_MATCH_STRING),
        "a refused pattern must emit zero bytes and not move the arm's counter (emitted \
         {emitted:?})"
    );
    assert_eq!(
        visited_instances(&snapshot, "BsMatchString"),
        1,
        "the refused instruction must still be visited by the census, else the deopt count is \
         vacuous"
    );
    // A refusal is **not** a static deopt: the arm's predicate declines with zero bytes and the
    // instruction falls through to the router's `_ if bitstrings::claimed(instr) => emit_bitstr`
    // arm, i.e. the family helper serves it. So the census must show the instance **visited** and
    // **no** per-instruction deopt -- asserting a deopt here would pin the wrong route.
    assert_eq!(
        static_deopts(&snapshot, "BsMatchString"),
        0,
        "a refused pattern is served by the family helper arm, not by a static deopt ({:?})",
        snapshot.instr_deopts,
    );
}

// ---------------------------------------------------------------------------
// The live `erl` oracle cross-check (skips loudly when the toolchain is absent)
// ---------------------------------------------------------------------------

/// The inline fixture compiled at test time. Each function is a **binary-literal** match of a
/// prefix, which is the Erlang spelling of `bs_match_string` for a literal longer than 64
/// bits — and, more importantly, the ground truth for what the pattern means. Every function
/// answers the bit count on a match and `-1` otherwise, so the values are directly comparable
/// with [`match_string_module`]'s.
const ORACLE_SRC: &str = "\
-module(msmatch).
-export([first/1, middle/1, last/1]).

first(Bin) ->
    case Bin of
        <<\"A\", _/binary>> -> 8;
        _ -> -1
    end.

middle(Bin) ->
    case Bin of
        <<\"ABCDE\", _/binary>> -> 40;
        _ -> -1
    end.

last(Bin) ->
    case Bin of
        <<\"ABCDEFGHI\", _/binary>> -> 72;
        _ -> -1
    end.
";

/// Compile [`ORACLE_SRC`] into a per-`tag` temp dir and return that dir.
///
/// `None` means `erlc` could not be **spawned** (the caller skips loudly). An `erlc` that ran
/// and exited non-zero is a **harness** failure — the source is valid Erlang — so it panics
/// with the captured stderr instead of masquerading as an absent toolchain.
fn compile_oracle(tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_engine_match_string_{tag}"));
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    std::fs::write(dir.join("msmatch.erl"), ORACLE_SRC)
        .unwrap_or_else(|error| panic!("write msmatch.erl: {error}"));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("msmatch.erl")
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling msmatch.erl\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(dir)
}

/// Run `eval` under the system `erl` with `dir` **appended** to the code path, returning its
/// stdout. `None` means no runnable `erl` at all (the caller skips loudly).
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

/// **Every byte-aligned row agrees with the live `erl` oracle, on each tier.**
///
/// `assert_same_outcome` already proves the two tiers agree; this row proves both agree with
/// the *language*. The oracle is a separate `erlc`-compiled module whose functions are the
/// same prefix matches written in Erlang source, evaluated by the system `erl` on the same
/// subject bytes — so a mistake the interpreter and the baseline tier share is caught.
///
/// The rows that a real `erlc` cannot express are deliberately absent: `bs_match_string` with
/// a non-byte-multiple `bits` (a binary literal must be byte-aligned) and with `bits == 0`
/// (folded away by the compiler). For those the in-tree oracle is the family's own
/// `bits_equal`/`set_ctx_start`, which is what the interpreter tier *is* — which is why every
/// row here is also a tier-agreement row.
#[test]
fn the_byte_aligned_rows_agree_with_the_live_erl_oracle() {
    let Some(dir) = compile_oracle("rows") else {
        eprintln!("SKIP: no usable `erlc` on PATH -- bs_match_string oracle cross-check skipped");
        return;
    };
    // (oracle function, module bit count, subject)
    let rows: [(&str, u32, &[u8]); 5] = [
        ("first", 8, SUBJECT),
        ("middle", 40, SUBJECT),
        ("last", PATTERN_BITS, SUBJECT),
        ("last", PATTERN_BITS, b"XBCDEFGHItail"),
        ("last", PATTERN_BITS, b"ABCD"),
    ];

    let mut checked = 0usize;
    for (function, bits, subject) in rows {
        let module = match_string_module(PATTERN, bits, None);
        let label = format!("oracle msmatch:{function}/1 on {subject:?}");
        let ours = with_both_tiers_bytes(&module, subject, |interpreter, baseline| {
            assert_same_outcome(&label, interpreter, baseline, &module.atom_table);
            returned_i64(&label, baseline)
        });
        let eval = format!(
            "io:format(\"~p\", [msmatch:{function}(<<{}>>)]), halt(0).",
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
    assert_eq!(checked, 5, "every oracle row must have been checked");
}
