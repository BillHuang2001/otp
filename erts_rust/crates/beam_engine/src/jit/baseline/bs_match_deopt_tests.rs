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
//! The `bs_match` inline arm's **deopt-idempotency** pins: the exact shape a
//! base62 decoder runs per character, its command-list boundaries, and the emission
//! instrument that keeps the arm native.
//!
//! `bs_match` is the one bitstring shape with a **mid-list** mutation: every
//! `skip`/`integer` command advances the running position. The arm keeps that position in
//! a machine register and writes the match context's `start` word **once** on the accepted
//! path and **once** on the fail arm — never per command — because its `fallback` edge is a
//! *deopt*: `jit_step_one` re-executes the whole instruction in the interpreter with the
//! same operand, so any state the emitted sequence mutated before a fallback-capable guard
//! would be visible to that re-execution and would shorten its window twice.
//!
//! The victim that motivated the rule is the per-character body of a base62 decoder. Real
//! `erlc` output (OTP 29 / erts-17.1) for `decode(<<_:2, F:6>>) -> F.` is
//!
//! ```text
//!   {test,bs_start_match3,{f,1},1,[{x,0}],{x,1}}
//!   {bs_get_position,{x,1},{x,0},2}
//!   {bs_match,{f,3},{x,1},
//!       {commands,[{ensure_exactly,8},{skip,2},{integer,2,{literal,[]},6,1,{x,2}}]}}
//!   {move,{x,2},{x,0}}
//!   return
//! ```
//!
//! over a **1-byte** subject. The `skip 2` committed `start = 2`, the 6-bit extraction then
//! declined to the fallback (the subject view's readable bound was the strict
//! `ceil(bits/8) = 1`, too narrow for the granular form's two-byte window), and the
//! interpreter re-ran `ensure_exactly 8` against a 6-bit window and **failed a match the
//! interpreter tier accepts** — a wrong result, the one outcome a tier must never produce.
//! The fix has two halves, and this file pins both: the `start` commit moved to the two
//! ends of the list, and the on-heap subject's readable byte count is the block's
//! word-padded data extent `round_up_8(ceil(bits/8))`.
//!
//! One claim per concern:
//!
//! * **the decoder character stays native.** A tier-agreement row alone also passes for an
//!   arm that deopts every instruction, so the shape is proved through a **cache-free**
//!   compile: its emission counter ([`super::emit::bitstr_inline::inlined`]) must move and
//!   the variant must record **zero** static deopts. This is the assertion that stops the
//!   bug class returning as a silent deopt — the very change that would have avoided the
//!   wrong result by giving up on the arm.
//! * **the values are right, not merely equal.** A matched-but-wrong extraction is the
//!   failure mode an inline arm risks, so every row asserts the returned integer against a
//!   hand-computed bit vector (and, for the decoder shape, against the live `erl` oracle).
//! * **the command-list boundaries** — `ensure_exactly` in both directions, a skip to the
//!   exact end followed by a zero-width field, a skip past the end, and a zero-length
//!   segment between two real ones — are pinned explicitly, each with a fail arm whose
//!   answer is distinguishable from every real one ([`sentinel_block`]).
//! * **a `NO_FAIL` match raises the family's `{badmatch, Subject}`** with the exact subject.
//! * **a legitimately deopting shape is still idempotent** — the root-cause pin: a command
//!   list whose field extraction really does decline to the fallback must answer exactly
//!   what the interpreter tier answers. Reintroducing a per-command commit fails that row.
//! * **the subject's two readable forms are both native** — an on-heap `ErlHeapBits` *and* a
//!   refc window (an `ErlSubBits` whose `orig` is a `BinRef`, which is what a match over a
//!   binary above `ERL_ONHEAP_BINARY_LIMIT` gets, i.e. every binary comprehension's
//!   subject). A **run-time** deopt count of zero per form is the assertion, because an arm
//!   that declines answers the same value through the interpreter step — and a non-refc
//!   `ErlSubBits` subject still declines, on both tiers.

use beam_common::ExceptionClass;
use beam_loader::{Instr, Label, Module, Operand, NO_FAIL};
use beam_term::Atom;

use super::bitstr_inline_tests::{
    atom, bitstr_module, emitted_shapes, ensure_at_least as ensure_at_least_command,
    integer_command, sentinel_block, static_deopts, visited_instances,
};
use super::bitstr_tests::{with_both_tiers_arg, Arg};
use super::counters;
use super::emit::bitstr_inline::SHAPE_BS_MATCH;
use super::tests::assert_same_outcome;
use crate::context::MinimalContext;
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};

// ---------------------------------------------------------------------------
// Shape builders
// ---------------------------------------------------------------------------
/// A `bs_match` `ensure_exactly Stride` command group.
fn ensure_exactly_command(module: &Module, stride: i64) -> Vec<Operand> {
    vec![
        Operand::Atom(atom(module, "ensure_exactly")),
        Operand::Integer(stride),
    ]
}

/// A `bs_match` `skip Stride` command group.
fn skip_command(module: &Module, stride: i64) -> Vec<Operand> {
    vec![
        Operand::Atom(atom(module, "skip")),
        Operand::Integer(stride),
    ]
}

/// A one-argument module whose body is
///
/// ```text
///   Label(1) Label(2)
///   bs_start_match4 Fail=Label(3) Src=x(0) -> Ctx=x(1)
///   bs_match Fail=Label(3) Ctx=x(1) <commands>
///   <after...>
///   return
///   Label(3): move -1 -> x(0); return
/// ```
///
/// — the compiler's own match preamble (`mv`-free: the subject is the argument in `x(0)` and
/// the context lands in `x(1)`, exactly as `beam_disasm` shows for `decode(<<_:2, F:6>>)`),
/// with the fail label pointed at a [`sentinel_block`] so "took the fail branch" is
/// observable in the returned value.
fn match_module_with(
    name: &str,
    atoms: &[&str],
    build: impl FnOnce(&Module) -> Vec<Operand>,
    after: Vec<Instr>,
) -> Module {
    bitstr_module(name, atoms, 1, move |module| {
        let commands = build(module);
        let mut code = vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::BsStartMatch4 {
                fail: Operand::Label(Label(3)),
                live: 1,
                src: Operand::X(0),
                dst: Operand::X(1),
            },
            Instr::BsMatch {
                fail: Label(3),
                ctx: Operand::X(1),
                commands,
            },
        ];
        code.extend(after);
        code.push(Instr::Return);
        code.extend(sentinel_block(Label(3)));
        code
    })
}

/// [`match_module_with`] with a bare `return` after the match.
fn match_module(name: &str, atoms: &[&str], build: impl FnOnce(&Module) -> Vec<Operand>) -> Module {
    match_module_with(name, atoms, build, Vec::new())
}

/// The atom names a `bs_match` command list needs, so a call site cannot forget one.
const MATCH_ATOMS: [&str; 3] = ["ensure_exactly", "skip", "integer"];

/// The subject bytes the decoder-shape rows run over.
///
/// `0xac`/`0xcd` are the vectors that make "matched but read the wrong bits" fail: the 6-bit
/// fields they hide (`0b101100` / `0b001101`) differ from each other, from the whole byte and
/// from the field a *shifted* read would produce (`0b001011` = 11 for `0xac`).
const DECODER_SUBJECTS: [&[u8]; 4] = [&[0xac], &[0xcd], &[0xff], &[0x00]];

// ---------------------------------------------------------------------------
// Harness helpers
// ---------------------------------------------------------------------------
/// The module's `bs_match` operands — the shape predicate's input.
fn match_operands(module: &Module) -> (Label, Operand, Vec<Operand>) {
    for instr in &module.functions[0].code {
        if let Instr::BsMatch {
            fail,
            ctx,
            commands,
        } = instr
        {
            return (*fail, ctx.clone(), commands.clone());
        }
    }
    panic!("the module must hold a `bs_match`");
}

/// Assert that `module`'s `bs_match` really is one the inline arm claims.
///
/// A row whose shape the predicate **refuses** runs on the family helper and proves nothing
/// about the arm, so every hand-built row states this explicitly.
fn assert_inlinable(label: &str, module: &Module) {
    let (fail, ctx, commands) = match_operands(module);
    let labels = crate::interpreter::build_label_map(module);
    assert!(
        super::emit::bitstr_inline::bs_match_shape(&labels, module, fail, &ctx, &commands)
            .is_some(),
        "{label}: the `bs_match` must stay natively inlinable — a refused shape would run on \
         the family helper and leave this row proving nothing"
    );
}

/// Run exported `f/1` on both tiers over `subject`, assert the two tiers agree, and answer
/// the **baseline** tier's integer result.
///
/// `assert_same_outcome` first (a tier divergence is the failure the arm can produce), then the
/// value — so a shared wrong extraction still fails here.
fn baseline_value(label: &str, module: &Module, subject: &'static [u8]) -> i64 {
    let name = atom(module, "f");
    with_both_tiers_arg(
        module,
        name,
        Arg::Bytes(subject),
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

/// [`super::tests::run_counting`]'s argument-taking shape: one baseline/interpreter run of
/// `f/1` over `subject` with the counters on, so a row can prove the **run-time** deopt
/// (`jit_step_one`) really happened.
fn run_counting_bytes(
    module: &Module,
    name: Atom,
    subject: &[u8],
    tier: Tier,
) -> (beam_common::Result<RunOutcome>, counters::Counters) {
    counters::reset();
    counters::set_enabled(true);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let value = crate::terms::make_heap_binary(&mut ctx, subject).expect("the argument builds");
    let outcome = machine.run_export_outcome_with_tier(module, name, 1, &[value], &mut ctx, tier);
    counters::set_enabled(false);
    (outcome, counters::snapshot())
}

// ---------------------------------------------------------------------------
// 1. The decoder character shape, on both tiers, still native
// ---------------------------------------------------------------------------
/// **The exact shape `emqx_base62:decode/2` runs per character, on both tiers, still
/// native.**
///
/// The subject is a **1-byte on-heap bitstring** (the `ErlHeapBits` block a `bs_create_bin`
/// of a single static `integer 8` segment produces — equivalent to the
/// `make_heap_binary`-built argument these rows pass, which is the same block shape: 2 head
/// words plus one word-padded data word), the command list is the compiler's own
/// `[ensure_exactly 8, skip 2, integer 6 1 x(2)]`, and the value is the extracted 6-bit field.
///
/// Two `bs_start_match4`/`bs_start_match3` start spellings run the *same* command list: the
/// `bs_start_match4` fresh form the arm models and the `bs_start_match3` form OTP 29's `erlc`
/// actually emits for this source (see the module docs). Both are inlined, and the shape under
/// test is the `bs_match`.
///
/// The second half is the **instrument**: a cache-free compile must still emit the arm and
/// record no static deopt for `BsMatch`. Without it, "fixing" a wrong result by dropping the
/// arm would pass the value rows above.
#[test]
fn the_decoder_character_shape_agrees_with_the_interpreter_and_stays_native() {
    // (label, the start instruction the row builds, expected 6-bit field per subject)
    let rows: [(&str, bool, [i64; 4]); 2] = [
        (
            "bs_start_match4 (fresh)",
            true,
            [0b101100, 0b001101, 0b111111, 0],
        ),
        (
            "bs_start_match3 (the erlc spelling)",
            false,
            [0b101100, 0b001101, 0b111111, 0],
        ),
    ];
    for (label, start4, expected) in rows {
        let commands = |module: &Module| {
            let mut commands = ensure_exactly_command(module, 8);
            commands.extend(skip_command(module, 2));
            commands.extend(integer_command(module, 6, 1, Operand::X(2)));
            commands
        };
        let module = if start4 {
            match_module_with(
                "bsm_decoder_start4",
                &MATCH_ATOMS,
                commands,
                vec![Instr::Move {
                    src: Operand::X(2),
                    dst: Operand::X(0),
                }],
            )
        } else {
            // The compiler's spelling: the same list behind a `bs_start_match3`.
            bitstr_module("bsm_decoder_start3", &MATCH_ATOMS, 1, |module| {
                let mut code = vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::BsStartMatch3 {
                        fail: Label(3),
                        bin: Operand::X(0),
                        live: 1,
                        dst: Operand::X(1),
                    },
                    Instr::BsMatch {
                        fail: Label(3),
                        ctx: Operand::X(1),
                        commands: {
                            let mut commands = ensure_exactly_command(module, 8);
                            commands.extend(skip_command(module, 2));
                            commands.extend(integer_command(module, 6, 1, Operand::X(2)));
                            commands
                        },
                    },
                    Instr::Move {
                        src: Operand::X(2),
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ];
                code.extend(sentinel_block(Label(3)));
                code
            })
        };
        assert_inlinable(label, &module);
        for (subject, want) in DECODER_SUBJECTS.iter().zip(expected) {
            let got = baseline_value(&format!("{label} {subject:?}"), &module, subject);
            assert_eq!(
                got, want,
                "{label}: the 6-bit field after a two-bit skip on {subject:?} must be \
                 {want:#b}, not {got:#b}"
            );
        }
        // The arm is still native: it emits, the variant name is real, and nothing statically
        // deopts. This is what stops the bug class returning as a silent deopt.
        let (emitted, snapshot) = emitted_shapes(&module);
        assert!(
            emitted.contains(&SHAPE_BS_MATCH),
            "{label}: a cache-free compile must still emit the `bs_match` arm (emitted \
             {emitted:?})"
        );
        assert!(
            visited_instances(&snapshot, "BsMatch") >= 1,
            "{label}: `BsMatch` must be the census's variant name (visited {:?})",
            snapshot.instr_instances
        );
        assert_eq!(
            static_deopts(&snapshot, "BsMatch"),
            0,
            "{label}: the instruction must be lowered, never statically deopted (deopts {:?})",
            snapshot.instr_deopts
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The same row against the live oracle
// ---------------------------------------------------------------------------
/// **The decoder shape's values against the live `erl` oracle.**
///
/// The oracle module is the source-level shape this row models
/// (`decode(<<_:2, F:6>>) -> F.`), compiled with the system `erlc` and run per subject byte; its
/// answers must equal what both tiers answered. The Rust side is compared against the oracle's
/// own output (not against a constant), so a wrong expected value in this file cannot make the
/// row vacuous — and the hand-computed constants are then checked against the oracle too.
///
/// Skips **loudly** when no `erlc`/`erl` is on `PATH`: a green run without the toolchain proves
/// nothing about corpus parity, and the print says so.
#[test]
fn the_decoder_character_shape_matches_the_system_oracle() {
    let module = match_module_with(
        "bsm_decoder_oracle",
        &MATCH_ATOMS,
        |module| {
            let mut commands = ensure_exactly_command(module, 8);
            commands.extend(skip_command(module, 2));
            commands.extend(integer_command(module, 6, 1, Operand::X(2)));
            commands
        },
        vec![Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        }],
    );
    assert_inlinable("oracle row", &module);
    let ours: Vec<i64> = DECODER_SUBJECTS
        .iter()
        .map(|subject| baseline_value(&format!("oracle row {subject:?}"), &module, subject))
        .collect();
    // The hand-computed 6-bit fields, so a hand-edited constant here could not track a
    // changed shape: `0b101100`, `0b001101`, `0b111111`, `0`.
    assert_eq!(
        ours,
        vec![0b101100, 0b001101, 0b111111, 0],
        "the Rust tiers' own answers"
    );

    let erlc = std::env::var("ERLC").unwrap_or_else(|_| "erlc".to_string());
    let erl = std::env::var("ERTS_ORACLE").unwrap_or_else(|_| "erl".to_string());
    let dir = std::env::temp_dir().join(format!(
        "beam_engine_bs_match_deopt_oracle_{}",
        std::process::id()
    ));
    if std::fs::create_dir_all(&dir).is_err() {
        eprintln!(
            "SKIP the_decoder_character_shape_matches_the_system_oracle: no temp dir — the \
             Rust tiers' values were still asserted above, but nothing was compared with the \
             system oracle"
        );
        return;
    }
    let source = dir.join("bs_match_deopt_oracle.erl");
    // The exact source shape the hand-built command list models.
    let text = "\
-module(bs_match_deopt_oracle).
-export([decode/1]).
decode(<<_:2, F:6>>) -> F.
";
    if std::fs::write(&source, text).is_err() {
        eprintln!(
            "SKIP the_decoder_character_shape_matches_the_system_oracle: cannot write \
             {source:?} — the Rust tiers were asserted, the oracle leg did not run"
        );
        return;
    }
    let compiled = std::process::Command::new(&erlc)
        .arg("+deterministic")
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .output();
    let Ok(compiled) = compiled else {
        eprintln!(
            "SKIP the_decoder_character_shape_matches_the_system_oracle: no `{erlc}` on PATH — \
             the Rust tiers were asserted, nothing was compared with the oracle"
        );
        return;
    };
    if !compiled.status.success() {
        eprintln!(
            "SKIP the_decoder_character_shape_matches_the_system_oracle: `{erlc}` failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        return;
    }
    let eval = "io:format(\"~~p~~n\",\
                [[bs_match_deopt_oracle:decode(<<B>>) || B <- [16#ac,16#cd,16#ff,16#00]]]),\
                halt().";
    let output = std::process::Command::new(&erl)
        .args([
            "-noshell",
            "-pa",
            dir.to_str().unwrap_or("."),
            "-eval",
            eval,
        ])
        .output();
    let Ok(output) = output else {
        eprintln!("SKIP the_decoder_character_shape_matches_the_system_oracle: no `{erl}` on PATH");
        return;
    };
    if !output.status.success() {
        eprintln!(
            "SKIP the_decoder_character_shape_matches_the_system_oracle: `{erl}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let oracle: Vec<i64> = text
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .split(',')
        .map(|field| {
            field
                .trim()
                .parse::<i64>()
                .unwrap_or_else(|_| panic!("an oracle integer, got {text:?}"))
        })
        .collect();
    assert_eq!(
        oracle.len(),
        DECODER_SUBJECTS.len(),
        "the oracle must answer one value per subject: {text}"
    );
    assert_eq!(
        ours, oracle,
        "the tiers must answer exactly what the system oracle answers for `decode(<<_:2,F:6>>)` \
         over {DECODER_SUBJECTS:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// 3. `ensure_exactly` rejects in both directions
// ---------------------------------------------------------------------------
/// **`ensure_exactly` is an exact-equality test, not a "at least" one — in both directions.**
///
/// * a stride **narrower** than the remaining window (`4` over 8 bits) must reject: a
///   lowering that compared with `>=` would accept it;
/// * a stride **wider** than the window (`16` over 8 bits) must reject too: the `<=` half.
///
/// Both rows take the fail label (the sentinel `-1`), on both tiers, and both shapes are
/// asserted inlinable so neither row can pass by quietly running on the family helper.
#[test]
fn an_ensure_exactly_rejection_takes_the_fail_label_in_both_directions() {
    // (label, stride, the direction the rejection pins)
    let cases: [(&str, i64, &str); 2] = [
        ("remaining > stride", 4, "a window wider than the stride"),
        (
            "remaining < stride",
            16,
            "a window narrower than the stride",
        ),
    ];
    for (label, stride, why) in cases {
        let module = match_module("bsm_exactly_reject", &MATCH_ATOMS, move |module| {
            let mut commands = ensure_exactly_command(module, stride);
            commands.extend(integer_command(module, 8, 1, Operand::X(0)));
            commands
        });
        assert_inlinable(label, &module);
        // `0xac` is one byte, i.e. exactly 8 bits of remaining window.
        let got = baseline_value(label, &module, &[0xac]);
        assert_eq!(
            got, -1,
            "{label}: `ensure_exactly {stride}` over 8 bits ({why}) must take the fail label"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. A skip to the exact end, then a zero-width field
// ---------------------------------------------------------------------------
/// **A skip that lands exactly on the end is a success, and a zero-width field after it binds
/// `0`.**
///
/// The two rows differ only in the unit of the zero-width segment (`integer 0 1` and
/// `integer 0 8`, both `nbits == 0`): the first walks the granular extractor with a mask that
/// clears everything, the second exercises the same product rule through the alignment form.
/// Both must answer `0` — not the fail label (a lowering that required `nbits > 0` bits of
/// remaining window would reject) and never a stale value.
#[test]
fn a_skip_to_the_exact_end_then_a_zero_width_field_succeeds() {
    for (label, size, unit) in [("integer 0 1", 0i64, 1i64), ("integer 0 8", 0, 8)] {
        let module = match_module("bsm_skip_to_end", &MATCH_ATOMS, move |module| {
            let mut commands = skip_command(module, 8);
            commands.extend(integer_command(module, size, unit, Operand::X(0)));
            commands
        });
        assert_inlinable(label, &module);
        let got = baseline_value(label, &module, &[0xac]);
        assert_eq!(
            got, 0,
            "{label}: a zero-width field after `skip 8` over 8 bits must bind 0"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. A skip past the end
// ---------------------------------------------------------------------------
/// **A skip past the end takes the fail label** — never a trap, never a value, and never an
/// out-of-range read: the emitted `skip` is a checked advance (`stride + pos` without unsigned
/// overflow, then `> end`), and the family's `skip_cmd` fails the match at the same boundary.
#[test]
fn a_skip_past_the_end_takes_the_fail_label() {
    let module = match_module("bsm_skip_past_end", &MATCH_ATOMS, |module| {
        let mut commands = skip_command(module, 12);
        commands.extend(integer_command(module, 8, 1, Operand::X(0)));
        commands
    });
    assert_inlinable("skip past the end", &module);
    let got = baseline_value("skip past the end", &module, &[0xac]);
    assert_eq!(
        got, -1,
        "`skip 12` over an 8-bit window must take the fail label"
    );
}

// ---------------------------------------------------------------------------
// 6. A zero-length segment between two real ones
// ---------------------------------------------------------------------------
/// **A `size 0 unit 1` field between two real ones binds `0` and does not move the position.**
///
/// The subject is two bytes; the middle field must read nothing, so the trailing field still
/// sees the *second* byte. The answer is a 3-tuple of all three fields, so a lowering that
/// consumed a bit at the zero-width segment (or that let it shift the extraction by anything)
/// fails on the element values rather than only on the position.
#[test]
fn a_zero_length_segment_mid_list_agrees() {
    let module = match_module_with(
        "bsm_zero_segment_mid_list",
        &MATCH_ATOMS,
        |module| {
            let mut commands = integer_command(module, 8, 1, Operand::X(2));
            commands.extend(integer_command(module, 0, 1, Operand::X(3)));
            commands.extend(integer_command(module, 8, 1, Operand::X(4)));
            commands
        },
        vec![Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::X(2), Operand::X(3), Operand::X(4)],
        }],
    );
    assert_inlinable("zero-length segment", &module);
    let name = atom(&module, "f");
    with_both_tiers_arg(
        &module,
        name,
        Arg::Bytes(&[0x12, 0x34]),
        |interpreter, baseline| {
            assert_same_outcome(
                "zero-length segment",
                interpreter,
                baseline,
                &module.atom_table,
            );
            for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
                let returned = outcome
                    .as_ref()
                    .unwrap_or_else(|error| panic!("{tier}: {error:?}"));
                let RunOutcome::Returned(value) = returned else {
                    panic!("{tier}: unexpected outcome {returned:?}");
                };
                let tuple = |index: usize| {
                    beam_term::signed_value(
                        crate::terms::tuple_element(*value, index)
                            .unwrap_or_else(|error| panic!("{tier}: element {index}: {error:?}")),
                    )
                };
                assert_eq!(
                    crate::terms::tuple_arity(*value)
                        .unwrap_or_else(|error| panic!("{tier}: {error:?}")),
                    3,
                    "{tier}: the answer must be the 3-tuple of the three fields"
                );
                assert_eq!(tuple(0), 0x12, "{tier}: the leading 8-bit field");
                assert_eq!(tuple(1), 0, "{tier}: the zero-width field must bind 0");
                assert_eq!(
                    tuple(2),
                    0x34,
                    "{tier}: the trailing field must still read the second byte"
                );
            }
        },
    );
}

// ---------------------------------------------------------------------------
// 7. A `NO_FAIL` match raises `{badmatch, Subject}`
// ---------------------------------------------------------------------------
/// **A `NO_FAIL` `bs_match` whose match rejects raises `{badmatch, Subject}`** — class `error`
/// and the *exact* subject — on both tiers, never a fail branch (there is none) and never a
/// fabricated value.
///
/// A `NO_FAIL` fail code is the one shape the inline predicate refuses (`bs_match_shape`
/// declines it: the reason allocates), so this row always runs through the family helper —
/// the machinery `a_no_fail_shape_still_goes_through_the_family` uses (`with_both_tiers_arg`
/// plus `assert_same_outcome`, then each tier's own raise). The family hands the match
/// **context** operand in as the subject (`matching::commands`' `fail_step(.., ctx_op, ..)`),
/// so the reason is the 2-tuple `{badmatch, Context}` whose second element windows exactly the
/// subject of *this* run: the failing command is always the **first** one (`ensure_at_least`),
/// so the window has consumed nothing and `[start, end)` is the whole subject.
///
/// Two subjects of **different bit lengths** run the row, so the reason is tied to the run's
/// own subject (8 bits for `<<0xac>>`, 16 for `<<0xac,0xcd>>`) and not to a constant, to a
/// neighbouring register, or to another run's term. The window's *bytes* are deliberately not
/// re-read here: an engine match context is a `base_flags == 0` `ErlSubBits`, and
/// `beam_term`'s comparison/`bit_at` resolve bit data through that word, so handing one to
/// `beam_term::cmp` dereferences a null base (a `beam_term` gap, out of this file's scope).
/// The subject's *content* is instead pinned by every row above, which reads these same
/// subjects through the extracted fields.
#[test]
fn a_no_fail_match_raises_the_badmatch_reason_with_the_exact_subject() {
    // (label, subject, the `ensure_at_least` bit count that cannot be met)
    let rows: [(&str, &'static [u8], i64); 2] = [
        ("<<0xac>>", &[0xac], 16),
        ("<<0xac,0xcd>>", &[0xac, 0xcd], 24),
    ];
    for (label, subject, need) in rows {
        let module = bitstr_module(
            "bsm_no_fail_match",
            &["ensure_at_least", "integer", "badmatch"],
            1,
            move |module| {
                let mut commands = ensure_at_least_command(module, need, 8);
                commands.extend(integer_command(module, 8, 1, Operand::X(0)));
                let mut code = vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::BsStartMatch4 {
                        fail: Operand::Label(Label(3)),
                        live: 1,
                        src: Operand::X(0),
                        dst: Operand::X(1),
                    },
                    // `NO_FAIL`: the family raises. The shape predicate refuses it, so the
                    // inline arm is out of the picture by construction.
                    Instr::BsMatch {
                        fail: NO_FAIL,
                        ctx: Operand::X(1),
                        commands,
                    },
                    Instr::Return,
                ];
                code.extend(sentinel_block(Label(3)));
                code
            },
        );
        // A `NO_FAIL` `bs_match` must never be inlined (the raise allocates).
        let (fail, ctx, commands) = match_operands(&module);
        assert_eq!(
            fail, NO_FAIL,
            "{label}: the row must carry the `NO_FAIL` fail code"
        );
        assert!(
            super::emit::bitstr_inline::bs_match_shape(
                &crate::interpreter::build_label_map(&module),
                &module,
                fail,
                &ctx,
                &commands,
            )
            .is_none(),
            "{label}: a `NO_FAIL` `bs_match` must stay on the family helper"
        );

        let badmatch = beam_term::make_atom(atom(&module, "badmatch"));
        let name = atom(&module, "f");
        with_both_tiers_arg(
            &module,
            name,
            Arg::Bytes(subject),
            |interpreter, baseline| {
                assert_same_outcome(
                    &format!("NO_FAIL bs_match {label}"),
                    interpreter,
                    baseline,
                    &module.atom_table,
                );
                for (tier, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
                    let error = match outcome {
                        Err(error) => error,
                        Ok(outcome) => panic!("{tier}: `NO_FAIL` must raise, got {outcome:?}"),
                    };
                    let beam_common::BeamError::Raise(raise) = error else {
                        panic!("{tier}: `NO_FAIL` must raise a BEAM exception, got {error:?}");
                    };
                    assert_eq!(
                        raise.class,
                        ExceptionClass::Error,
                        "{tier}: a match failure is an `error` class raise"
                    );
                    // The reason term lives in the machine's own heap, which is alive for exactly
                    // the duration of this closure — so it is read here.
                    let reason = raise
                        .live_reason()
                        .and_then(|live| live.downcast_ref::<beam_term::Eterm>().copied())
                        .unwrap_or_else(|| panic!("{tier}: the raise must carry its reason term"));
                    assert_eq!(
                        crate::terms::tuple_arity(reason)
                            .unwrap_or_else(|error| panic!("{tier}: the reason: {error:?}")),
                        2,
                        "{tier}: the reason must be the `{{badmatch, Subject}}` pair"
                    );
                    assert_eq!(
                        crate::terms::tuple_element(reason, 0)
                            .unwrap_or_else(|error| panic!("{tier}: the reason tag: {error:?}")),
                        badmatch,
                        "{tier}: the reason tag must be exactly the `badmatch` atom"
                    );
                    let subject_term = crate::terms::tuple_element(reason, 1)
                        .unwrap_or_else(|error| panic!("{tier}: the reason subject: {error:?}"));
                    assert_eq!(
                    crate::families::bitstrings::bit_size(subject_term),
                    Some(subject.len() * 8),
                    "{tier}: the raise must carry the subject bitstring itself, `{label}`, which \
                     this run's failing `ensure_at_least` had consumed nothing of"
                );
                }
            },
        );
    }
}

// ---------------------------------------------------------------------------
// 8. A legitimately deopting shape is still idempotent
// ---------------------------------------------------------------------------
/// **The root-cause regression pin: a command list whose field extraction really does decline
/// to the fallback answers exactly what the interpreter tier answers.**
///
/// A 16-bit field at a **non-byte-aligned** position (after `skip 2`) has no form in
/// [`super::emit::bitstr_inline::emit_bit_extract`]'s granular path (`rem + nbits > 16`), so the
/// emitted sequence jumps to `fallback` and one interpreter step re-executes the instruction.
/// The subject here is three bytes, so the *only* thing between the two tiers is the state the
/// emitted sequence mutated before that fallback: `ensure_exactly 24` re-run after a committed
/// `skip 2` would see a 22-bit window (and take the fail label), while an untouched context
/// sees all 24 bits. The value assertion therefore fails on exactly the class of bug the
/// one-commit rule closed, on any subject — no bit-vector coincidence can hide it, because the
/// fail arm answers the sentinel `-1` and the correct read answers a 16-bit field.
///
/// The `jit_step_one` counter proves the row really took the fallback: without it, this test
/// would pass on an arm that inverted the shape into something it never emits.
#[test]
fn a_legitimately_deopting_shape_is_still_idempotent() {
    let module = match_module("bsm_deopt_idempotent", &MATCH_ATOMS, |module| {
        let mut commands = ensure_exactly_command(module, 24);
        commands.extend(skip_command(module, 2));
        commands.extend(integer_command(module, 16, 1, Operand::X(0)));
        commands
    });
    assert_inlinable("deopting 16-bit field", &module);
    let subject: &'static [u8] = &[0x12, 0x34, 0x56];
    // Bits 2..18 of `0x12,0x34,0x56` = `0100100011010001` = `0x48d1`.
    assert_eq!(
        baseline_value("deopting 16-bit field", &module, subject),
        0x48d1,
        "a 16-bit field two bits into the subject must be read through the fallback, and the \
         re-execution must see the whole window"
    );
    let name = atom(&module, "f");
    let (outcome, snapshot) = run_counting_bytes(&module, name, subject, Tier::BaselineJit);
    let returned = outcome.as_ref().unwrap_or_else(|error| panic!("{error:?}"));
    let RunOutcome::Returned(value) = returned else {
        panic!("unexpected outcome {returned:?}");
    };
    assert_eq!(
        beam_term::signed_value(*value),
        0x48d1,
        "the counted run must answer the same value"
    );
    assert!(
        snapshot.step_one >= 1,
        "the 16-bit field must really deopt (jit_step_one ran {} times): otherwise this row \
         does not exercise the fallback it exists to pin",
        snapshot.step_one
    );
}

// ---------------------------------------------------------------------------
// 9. An off-heap refc-binary window is a bitstring subject the arm lowers
// ---------------------------------------------------------------------------
/// The `long_append` bench workload's program, verbatim.
///
/// `crate::bench::long_append`'s `SOURCE` holds the same text, but that module is private to
/// `crate::bench` (`mod long_append;` inside a `#[cfg(test)] mod bench;`), and the bench's
/// driving scaffolding (`compile_fixture`, `measure`, `CountingContext`) is private to
/// `crate::bench` in the same way — this `baseline` child cannot reach any of it, so the pin
/// stages and drives the workload itself rather than widening a non-test module's visibility
/// for one test.
const LONG_APPEND_SOURCE: &str = r#"-module(long_append).
-export([input/1, expand/1]).

input(N) ->
    <<16#00112233445566778899aabbccddeeff:N/unit:8,
      16#ffeeddccbbaa99887766554433221100:N/unit:8>>.

expand(Bin) -> << <<C:8,C:8,C:8,C:8>> || <<C:8>> <= Bin >>.
"#;

/// `input/1`'s argument: two 8 KiB integer segments, so the subject is 16 KiB — 256× the
/// 64-byte on-heap binary limit, i.e. it can only be an off-heap refc binary.
const LONG_APPEND_N: usize = 8 * 1024;

/// The two 16-byte markers `input/1` encodes, big-endian, at the tail of each segment.
const LONG_APPEND_MARKERS: [u128; 2] = [
    0x0011_2233_4455_6677_8899_aabb_ccdd_eeff,
    0xffee_ddcc_bbaa_9988_7766_5544_3322_1100,
];

/// What `input(N)` returns: each marker zero-extended to an `N`-byte segment.
fn long_append_input_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 * LONG_APPEND_N);
    for marker in LONG_APPEND_MARKERS {
        let encoded = marker.to_be_bytes();
        bytes.resize(bytes.len() + LONG_APPEND_N - encoded.len(), 0);
        bytes.extend_from_slice(&encoded);
    }
    bytes
}

/// What `expand/1` must return: four copies of every input byte (64 KiB).
fn long_append_expected_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 * long_append_input_bytes().len());
    for byte in long_append_input_bytes() {
        bytes.extend_from_slice(&[byte; 4]);
    }
    bytes
}

/// Stage [`LONG_APPEND_SOURCE`] in a temp directory and compile it with the system `erlc`.
///
/// `Err` means **skip** (no spawnable `erlc`, or one that refused the source): the pin needs a
/// real `.beam` because the subject has to be a real off-heap refc binary built by the engine's
/// own append path.
fn stage_long_append_fixture() -> Result<std::path::PathBuf, String> {
    let dir = std::env::temp_dir().join(format!(
        "beam_engine_bs_match_refc_window_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    let source = dir.join("long_append.erl");
    std::fs::write(&source, LONG_APPEND_SOURCE)
        .map_err(|error| format!("write {}: {error}", source.display()))?;
    let erlc = std::env::var("ERLC").unwrap_or_else(|_| "erlc".to_string());
    let compiled = std::process::Command::new(&erlc)
        .arg("+deterministic")
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .output()
        .map_err(|error| format!("no usable `{erlc}` on PATH ({error})"))?;
    if !compiled.status.success() {
        return Err(format!(
            "`{erlc}` exited {} on {}: {}",
            compiled.status,
            source.display(),
            String::from_utf8_lossy(&compiled.stderr)
        ));
    }
    Ok(dir.join("long_append.beam"))
}

/// **A `bs_match` over an off-heap refc-binary window takes no interpreter step.**
///
/// The subject of the `long_append` comprehension (`expand/1` = `<< <<C:8,C:8,C:8,C:8>> ||
/// <<C:8>> <= Bin >>`) is an `ErlSubBits` whose own `orig` is a `BinRef`, i.e. the refc-window
/// form [`super::emit::bitstr_inline`]'s subject view now classifies. Before that arm the view
/// refused the form and the arm's `fallback` paid **one whole interpreter step per element** —
/// exactly **16 384** `BsMatch` deopts for one call over a 16 KiB subject (1.000 per append).
/// Tier agreement cannot see the regression: a deopted instruction is executed by the
/// interpreter on *both* sides, so an arm that silently degraded back to a deopt would still
/// agree. This pin is therefore made of the run-time counters, not of a value comparison: the
/// per-variant deopt attribution must record no `BsMatch`, the residual per-instruction deopts
/// must stay the accumulator's own growth fallbacks rather than a per-element cost, and the
/// bitstring family's own helper ([`super::bitstr::bitstr_calls`]) must not become the
/// per-element route — the shape is lowered natively. The workload's own 64 KiB result is
/// checked byte for byte, so a shape that lowered into a wrong value cannot pass.
///
/// The two counter figures that are *not* zero are worth naming, so a later reader does not
/// mistake them for the regression: `bs_create_bin` still takes its own capacity fallback a
/// handful of times over the build (8 measured — the writable payload's growth events, a
/// function of the growth schedule, not of the 16 384 elements), and `jit_bitstr` runs twice
/// per call for instructions that have no inline shape at all (`bs_init_writable`, and the
/// single fresh-subject `bs_start_match3`, which must `make_ctx`).
#[test]
fn a_bs_match_over_an_off_heap_refc_window_takes_no_interpreter_step() {
    let beam = match stage_long_append_fixture() {
        Ok(beam) => beam,
        Err(why) => {
            println!("SKIP: {why} -- the off-heap refc-window `bs_match` pin did not run");
            return;
        }
    };
    let module = beam_loader::load_file(&beam)
        .unwrap_or_else(|error| panic!("load {}: {error:?}", beam.display()));
    let input_entry = atom(&module, "input");
    let expand_entry = atom(&module, "expand");

    let mut ctx = MinimalContext::new();
    let mut machine = Machine::new();
    // Setup: the 16 KiB subject, built by the fixture's own `input/1` on the interpreter and
    // untimed/unrecorded. It is an off-heap refc binary (256× the on-heap limit) whose payload
    // lives in this machine's reservoir — the same store `expand/1` will read it from.
    let subject = match machine
        .run_export_outcome_with_tier(
            &module,
            input_entry,
            1,
            &[beam_term::make_small(LONG_APPEND_N as i64)],
            &mut ctx,
            Tier::Interpreter,
        )
        .unwrap_or_else(|error| panic!("the fixture's input/1: {error:?}"))
    {
        RunOutcome::Returned(value) => value,
        other => panic!("the fixture's input/1 must return: {other:?}"),
    };
    assert!(
        machine
            .refc_binaries()
            .iter()
            .any(|payload| payload.len() == 2 * LONG_APPEND_N),
        "the subject must be an off-heap refc binary (no {} -byte payload in the machine's \
         reservoir: {:?})",
        2 * LONG_APPEND_N,
        machine
            .refc_binaries()
            .iter()
            .map(|payload| payload.len())
            .collect::<Vec<_>>()
    );
    let subject_bytes = crate::terms::binary_bytes(subject).expect("the subject is a binary");
    assert_eq!(
        subject_bytes,
        long_append_input_bytes().as_slice(),
        "the fixture's input/1 must build the modelled 16 KiB subject"
    );

    // The append pass, on the baseline tier, with the run-time counters on. `bitstr_calls` is a
    // monotonic per-thread tally (the counters' `reset` does not touch it), so the pin reads a
    // delta around the pass rather than the absolute value.
    counters::reset();
    counters::set_enabled(true);
    let family_before = super::bitstr::bitstr_calls();
    let outcome = machine.run_export_outcome_with_tier(
        &module,
        expand_entry,
        1,
        &[subject],
        &mut ctx,
        Tier::BaselineJit,
    );
    let family_calls = super::bitstr::bitstr_calls() - family_before;
    counters::set_enabled(false);
    let snapshot = counters::snapshot();

    let result = match outcome.unwrap_or_else(|error| panic!("the fixture's expand/1: {error:?}"))
    {
        RunOutcome::Returned(value) => value,
        other => panic!("the fixture's expand/1 must return: {other:?}"),
    };
    let expected = long_append_expected_bytes();
    let produced = crate::terms::binary_bytes(result).expect("the result is a binary");
    println!(
        "off_heap_refc_window: produced {} of {} bytes, step_one {}, BsMatch deopts {}, \
         bitstr_calls {}",
        produced.len(),
        expected.len(),
        snapshot.step_one,
        snapshot
            .deopts_by_variant
            .get("BsMatch")
            .copied()
            .unwrap_or(0),
        family_calls
    );
    assert_eq!(
        produced,
        expected.as_slice(),
        "expand/1 must return four copies of every input byte"
    );
    // **The pin.** Before the refc-window subject arm this pass recorded exactly one `BsMatch`
    // deopt per element (16 384 for this subject); with the shape lowered natively the
    // per-variant attribution holds no `BsMatch` at all.
    let match_deopts = snapshot
        .deopts_by_variant
        .get("BsMatch")
        .copied()
        .unwrap_or(0);
    assert_eq!(
        match_deopts, 0,
        "a `bs_match` over the off-heap refc-binary window must not deopt: it paid one whole \
         interpreter step per element ({} for this subject) before the refc-window subject arm \
         landed (deopts by variant: {:?})",
        2 * LONG_APPEND_N,
        snapshot.deopts_by_variant
    );
    // What is left is the accumulator's own construction, never a per-element figure: measured
    // 8, and a payload growing 32 B -> 64 KiB cannot take more than ~12 doublings, so this
    // bound separates a constant of the growth schedule from the total a per-element regression
    // would show (16 384 — which the assertion above also catches by name).
    assert!(
        snapshot.step_one < 64,
        "the residual per-instruction deopts must be the writable accumulator's growth \
         fallbacks, not a per-element cost; jit_step_one ran {} times (deopts by variant: {:?})",
        snapshot.step_one,
        snapshot.deopts_by_variant
    );
    // `jit_bitstr` is reached once per call for `bs_init_writable` (family-owned: it has no
    // inline shape) and once for the single fresh-subject match start (`bs_start_match3` must
    // `make_ctx`, which allocates); the per-element `bs_match` is not on the family's route.
    assert!(
        family_calls <= 2,
        "the bitstring family helper must not become the per-element route ({} calls for {} \
         elements): the `bs_match` shape has to be lowered natively",
        family_calls,
        2 * LONG_APPEND_N
    );
}

// ---------------------------------------------------------------------------
// 10. A refc-window subject — the binary-comprehension inner loop
// ---------------------------------------------------------------------------
/// Build an off-heap (**refc**) bitstring term over `payload_bytes` with the window
/// `[offset_bits, offset_bits + size_bits)`.
///
/// This is the subject form a match over a binary **larger than
/// [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`]** (64) bytes gets: the payload is off-heap and
/// the term is an `ErlSubBits` whose `orig` is a `BinRef`. The payload words are answered
/// beside the term because the term only *names* them — the caller keeps them alive for as
/// long as the run reads the subject, which is `raw::subject_bytes`' ownership contract.
#[allow(unsafe_code)]
fn refc_window(
    ctx: &mut MinimalContext,
    payload_bytes: &[u8],
    offset_bits: usize,
    size_bits: usize,
) -> (beam_term::Eterm, Vec<beam_term::Eterm>) {
    use crate::context::ExecutionContext;
    let word_bytes = core::mem::size_of::<beam_term::Eterm>();
    let words = payload_bytes.len().div_ceil(word_bytes).max(1);
    let mut payload: Vec<beam_term::Eterm> = vec![beam_term::Eterm::default(); words];
    // SAFETY: `payload` owns `words * size_of::<Eterm>() >= payload_bytes.len()` bytes and is
    // exclusively borrowed here, so the whole payload fits.
    unsafe {
        core::ptr::copy_nonoverlapping(
            payload_bytes.as_ptr(),
            payload.as_mut_ptr().cast::<u8>(),
            payload_bytes.len(),
        );
    }
    let base = payload.as_ptr().cast::<u8>();
    let val = payload.as_mut_ptr();
    let hp = ctx
        .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
        .expect("arena words");
    // SAFETY: `hp` is 8 exclusively-owned arena words (the `BinRef` + `ErlSubBits` pair),
    // `val`/`base` point at the live word-aligned payload and `next` is the null chain link.
    let term = unsafe {
        beam_term::refc::write_refc_binary(
            hp,
            val,
            core::ptr::null_mut(),
            base,
            offset_bits,
            size_bits,
        )
    };
    (term, payload)
}

/// [`run_counting_bytes`]'s refc-subject twin: one run of `f/1` over a refc window with the
/// deopt counters on.
#[allow(unsafe_code)]
fn run_counting_refc(
    module: &Module,
    name: Atom,
    payload: &[u8],
    offset_bits: usize,
    size_bits: usize,
    tier: Tier,
) -> (beam_common::Result<RunOutcome>, counters::Counters) {
    counters::reset();
    counters::set_enabled(true);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let (subject, payload_words) = refc_window(&mut ctx, payload, offset_bits, size_bits);
    let outcome =
        machine.run_export_outcome_with_tier(module, name, 1, &[subject], &mut ctx, tier);
    counters::set_enabled(false);
    drop(payload_words);
    (outcome, counters::snapshot())
}

/// Run `f/1` over a refc window on **both** tiers, assert the two agree, and answer the
/// baseline tier's integer result.
#[allow(unsafe_code)]
fn refc_value(
    label: &str,
    module: &Module,
    payload: &[u8],
    offset_bits: usize,
    size_bits: usize,
) -> i64 {
    let name = atom(module, "f");
    let (interpreted, _) = run_counting_refc(
        module,
        name,
        payload,
        offset_bits,
        size_bits,
        Tier::Interpreter,
    );
    let (baseline, snapshot) = run_counting_refc(
        module,
        name,
        payload,
        offset_bits,
        size_bits,
        Tier::BaselineJit,
    );
    assert!(
        snapshot.deopts_by_variant.get("BsMatch").copied().unwrap_or(0) == 0,
        "{label}: a refc-window subject must be read by the `bs_match` arm itself, never through \
         the family helper (run-time deopts {:?})",
        snapshot.deopts_by_variant
    );
    assert_same_outcome(label, &interpreted, &baseline, &module.atom_table);
    let returned = baseline
        .as_ref()
        .unwrap_or_else(|error| panic!("{label}: {error:?}"));
    let RunOutcome::Returned(value) = returned else {
        panic!("{label}: unexpected outcome {returned:?}");
    };
    beam_term::signed_value(*value)
}

/// **A `refc`-window subject is read by the arm, not by the family helper.**
///
/// The subject here is the shape a compiled **binary comprehension** matches element by
/// element: a 16 KiB (i.e. far above [`beam_term::refc::ERL_ONHEAP_BINARY_LIMIT`]) off-heap
/// payload, so the match context's `orig` is a refc `ErlSubBits`. The command list is the
/// comprehension's own `[ensure_at_least 8 1, integer 8 1 x(2)]`.
///
/// Before the subject view learned the refc arm this row still answered correctly — through
/// [`super::runtime::jit_step_one`], one full interpreter step **per element**, which is
/// exactly the measured 72 % of a `binary_append` iteration. So the value alone proves
/// nothing: the assertion that matters is the deopt counter, checked in [`refc_value`], plus
/// the cache-free emission pin below. A regression that drops the refc arm answers the same
/// number and fails here.
#[test]
fn a_refc_window_subject_inlines_and_agrees_with_the_interpreter() {
    let module = match_module_with(
        "bsm_refc_subject",
        &["ensure_at_least", "integer"],
        |module| {
            let mut commands = ensure_at_least_command(module, 8, 1);
            commands.extend(integer_command(module, 8, 1, Operand::X(2)));
            commands
        },
        vec![Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        }],
    );
    assert_inlinable("refc subject", &module);
    // The comprehension's subject size, filled with a byte pattern whose first field is
    // distinguishable from every neighbour (`0x5a`).
    let payload = vec![0x5au8; 16 * 1024];
    let bits = payload.len() * 8;
    assert_eq!(
        refc_value("refc subject", &module, &payload, 0, bits),
        0x5a,
        "the first 8 bits of a refc-window subject must be its first payload byte"
    );
    // The shape is still claimed *statically* too: a cache-free compile emits the arm and
    // records no static deopt for `BsMatch` (a shape-level regression fails here, a
    // subject-level one in `refc_value`).
    let (emitted, snapshot) = emitted_shapes(&module);
    assert!(
        emitted.contains(&SHAPE_BS_MATCH),
        "a cache-free compile must still emit the `bs_match` arm (emitted {emitted:?})"
    );
    assert_eq!(
        static_deopts(&snapshot, "BsMatch"),
        0,
        "the arm must be lowered, never statically deopted (deopts {:?})",
        snapshot.instr_deopts
    );
}

/// **A refc window that starts at a non-zero bit offset reads the right bits.**
///
/// The window's `start` word is what moves the payload's byte base and rotates the field's
/// bit position (`base = payload + (start + pos) / 8`, `bit_pos = (start + pos) % 8`), so a
/// byte-aligned row alone cannot pin the arithmetic — an arm that ignored `start` would pass
/// it. Here the window begins 4 bits into the payload, so the first whole byte spans two
/// payload bytes: bits 4..12 of `0x12 0x34 …` are `0x23`.
#[test]
fn a_refc_window_at_a_bit_offset_reads_the_right_bits() {
    let module = match_module_with(
        "bsm_refc_offset_subject",
        &["ensure_at_least", "integer"],
        |module| {
            let mut commands = ensure_at_least_command(module, 8, 1);
            commands.extend(integer_command(module, 8, 1, Operand::X(2)));
            commands
        },
        vec![Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        }],
    );
    assert_inlinable("refc offset subject", &module);
    let mut payload = vec![0x12u8, 0x34, 0x56, 0x78];
    payload.resize(256, 0xff);
    let offset = 4;
    let bits = payload.len() * 8 - offset;
    assert_eq!(
        refc_value("refc offset subject", &module, &payload, offset, bits),
        0x23,
        "bits 4..12 of the payload must read `0x23` (the low nibble of 0x12 then the high \
         nibble of 0x34)"
    );
}

/// **A non-refc `ErlSubBits` subject still declines, and both tiers answer the same thing.**
///
/// [`super::emit::bitstr_inline::emit_subject_view`] claims the refc form by testing the
/// subject's `orig` word for a boxed `BinRef`. An `ErlSubBits` whose `orig` is *not* one — a
/// match context used as a subject, a sub-bitstring over a heap block: here the widest form,
/// a non-boxed `orig` — is not readable by the family either
/// (`raw::heap_bits` refuses it), so the arm must fall through to the family helper and the
/// two tiers must agree on its typed refusal. Without this row, "accept every `ErlSubBits`"
/// would look like a simplification instead of the wrong answer it is.
#[test]
#[allow(unsafe_code)]
fn a_non_refc_sub_bits_subject_still_declines_to_the_family() {
    let module = match_module_with(
        "bsm_non_refc_subject",
        &["ensure_at_least", "integer"],
        |module| {
            let mut commands = ensure_at_least_command(module, 8, 1);
            commands.extend(integer_command(module, 8, 1, Operand::X(2)));
            commands
        },
        vec![Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        }],
    );
    assert_inlinable("non-refc subject", &module);
    let name = atom(&module, "f");
    let run = |tier: Tier| {
        use crate::context::ExecutionContext;
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let hp = ctx
            .alloc_words(beam_term::ERL_SUB_BITS_SIZE)
            .expect("arena words");
        // SAFETY: `hp` is 5 exclusively-owned arena words (one `ErlSubBits`), and this term is
        // the widest non-refc form: its `orig` word is a small int, so neither the arm nor the
        // family ever dereferences the `base_flags` word this term also carries.
        let subject = unsafe {
            beam_term::bitstring::write_sub_bits(
                hp,
                beam_term::Eterm::from_raw(0),
                beam_term::make_small(0),
                beam_term::make_small(8),
                beam_term::make_small(0),
            )
        };
        let outcome = machine.run_export_outcome_with_tier(
            &module,
            name,
            1,
            &[subject],
            &mut ctx,
            tier,
        );
        outcome
    };
    let interpreted = run(Tier::Interpreter);
    counters::reset();
    counters::set_enabled(true);
    let baseline = run(Tier::BaselineJit);
    counters::set_enabled(false);
    let snapshot = counters::snapshot();
    assert!(
        snapshot.deopts_by_variant.get("BsMatch").copied().unwrap_or(0) >= 1,
        "the arm must decline a non-refc `ErlSubBits` subject (run-time deopts {:?})",
        snapshot.deopts_by_variant
    );
    assert_same_outcome(
        "non-refc subject",
        &interpreted,
        &baseline,
        &module.atom_table,
    );
}
