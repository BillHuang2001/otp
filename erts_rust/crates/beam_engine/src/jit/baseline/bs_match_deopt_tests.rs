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
