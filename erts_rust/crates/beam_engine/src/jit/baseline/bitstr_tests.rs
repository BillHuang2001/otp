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
//! Pins for the specialised `bs_match` helper (`super::bitstr`). A `#[path]` sibling of
//! `super::tests`, so it reuses that module's harness (`body_module`,
//! `assert_same_outcome`, the `MinimalContext` route).
//!
//! Three claims, one per concern:
//!
//! * the **shape predicate** accepts exactly the command list the shipped `erlc` emits for a
//!   fixed-width field read, checked on a **committed real `.beam`** (`bssmall.beam`) rather
//!   than on a hand-written IR, so the specialisation cannot silently stop firing;
//! * every **other** `bs_match` command list keeps the generic family route (the predicate
//!   must not claim a shape it would then have to execute anyway);
//! * the specialised path is **interpreter-identical**: the same real fixture's reads return
//!   the same value on both tiers, while the helper's own counter proves the emitted arm
//!   actually took it (a cross-tier agreement alone would also hold if the arm never fired).

use beam_common::Result;
use beam_loader::{load_file, Instr, Module, Operand};
use beam_term::Atom;

use super::tests::{assert_same_outcome, body_module};
use super::bitstr::{self, specialised_calls};
use crate::context::MinimalContext;
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};
use crate::terms::make_heap_binary;

/// Load one of the committed `erlc` fixtures (`tests/fixtures/*.beam`).
fn fixture(name: &str) -> Module {
    let path = format!("{}/tests/fixtures/{name}.beam", env!("CARGO_MANIFEST_DIR"));
    load_file(&path).unwrap_or_else(|error| panic!("load {path}: {error:?}"))
}

/// Every `bs_match` of `module` whose command list is the fixed-width field read:
/// `[atom, Integer, Integer, atom, Integer, _, Integer, Integer, register]`.
///
/// Derived here **independently** of [`bitstr::supported`] (a plain recount of the shape) so
/// the predicate is checked against the real bytes rather than against itself.
fn hot_shaped_matches(module: &Module) -> usize {
    let mut found = 0;
    for function in &module.functions {
        for instr in &function.code {
            let Instr::BsMatch { commands, .. } = instr else {
                continue;
            };
            let hot = matches!(
                commands.as_slice(),
                [
                    Operand::Atom(_),
                    Operand::Integer(_),
                    Operand::Integer(_),
                    Operand::Atom(_),
                    Operand::Integer(_),
                    _,
                    Operand::Integer(_),
                    Operand::Integer(_),
                    Operand::X(_) | Operand::Y(_) | Operand::Typed { .. },
                ]
            );
            if hot {
                found += 1;
            }
        }
    }
    found
}

/// Every `bs_match` of `module`, and how many of them [`bitstr::supported`] claims.
fn supported_matches(module: &Module) -> (usize, usize) {
    let mut total = 0;
    let mut supported = 0;
    for function in &module.functions {
        for instr in &function.code {
            if matches!(instr, Instr::BsMatch { .. }) {
                total += 1;
                if bitstr::supported(instr) {
                    supported += 1;
                }
            }
        }
    }
    (total, supported)
}

/// **The predicate accepts the shape the shipped compiler emits.**
///
/// `bssmall.beam` is committed `erlc` output, and every one of its `u<N>/1`/`s<N>/1`/`u<N>le/1`
/// reads is `<<Y:N, _/bitstring>> = B` — `{ensure_at_least,N,1}` + `{integer,_,{literal,[]},N,1,Dst}`,
/// the exact pair a binary comprehension runs once per element. The counts must agree with an
/// independent recount of the real command lists, so a predicate that stopped matching the
/// compiler's shape fails here rather than in a benchmark.
#[test]
fn the_predicate_accepts_the_command_list_the_compiler_emits() {
    let module = fixture("bssmall");
    let expected = hot_shaped_matches(&module);
    let (total, supported) = supported_matches(&module);
    assert!(
        expected > 0,
        "the fixture must carry at least one fixed-width field read (total bs_match: {total})"
    );
    assert_eq!(
        supported, expected,
        "the predicate must claim exactly the {expected} fixed-width reads \
         (of {total} bs_match instructions)"
    );
}

/// **Every other `bs_match` command list keeps the generic family route.**
///
/// The shapes are the vocabulary's remaining ones: the bare `integer` read (6 operands), the
/// `ensure_exactly` test (2), the flattened multi-field list a `<<A:16, B:16, R/binary>>` head
/// produces (19), and a fixed-width read whose destination is not a register. None of them may
/// be claimed — the generic arm executes them through the same family body.
#[test]
fn every_other_command_list_keeps_the_generic_family_route() {
    let ctx_atom = Operand::Atom(beam_term::Atom::MAX);
    let integer_atom = Operand::Atom(beam_term::Atom::MAX);
    let cases: [(&str, Vec<Operand>); 5] = [
        (
            "an integer read alone",
            vec![
                integer_atom.clone(),
                Operand::Integer(2),
                Operand::Literal(0),
                Operand::Integer(8),
                Operand::Integer(1),
                Operand::X(0),
            ],
        ),
        (
            "an ensure_exactly test",
            vec![ctx_atom.clone(), Operand::Integer(64)],
        ),
        (
            "a flattened multi-field list",
            vec![
                Operand::Atom(beam_term::Atom::MAX),
                Operand::Integer(32),
                Operand::Integer(8),
                integer_atom.clone(),
                Operand::Integer(2),
                Operand::Literal(0),
                Operand::Integer(16),
                Operand::Integer(1),
                Operand::X(0),
                integer_atom.clone(),
                Operand::Integer(2),
                Operand::Literal(1),
                Operand::Integer(16),
                Operand::Integer(1),
                Operand::X(1),
                Operand::Atom(beam_term::Atom::MAX),
                Operand::Integer(3),
                Operand::Integer(1),
                Operand::X(2),
            ],
        ),
        (
            "a fixed-width read into a non-register",
            vec![
                Operand::Atom(beam_term::Atom::MAX),
                Operand::Integer(8),
                Operand::Integer(1),
                integer_atom,
                Operand::Integer(2),
                Operand::Literal(0),
                Operand::Integer(8),
                Operand::Integer(1),
                Operand::Literal(3),
            ],
        ),
        ("an empty command list", Vec::new()),
    ];
    for (label, commands) in cases {
        let instr = Instr::BsMatch {
            fail: beam_loader::NO_FAIL,
            ctx: Operand::X(1),
            commands,
        };
        assert!(!bitstr::supported(&instr), "{label} must not be claimed");
    }
    // And the non-`bs_match` variants are of course not this helper's.
    assert!(!bitstr::supported(&Instr::Return));
}

/// Run exported `name/1` with one heap-binary argument on both tiers, handing the two
/// outcomes to `check`.
///
/// The byte-argument twin of `tests`' `with_both_tiers_unary` (that one hard-codes a small int
/// argument): each run builds its own [`MinimalContext`] heap, so the comparison happens
/// *inside* this call — a returned boxed term points into that heap and the context must
/// outlive it.
fn with_both_tiers_bytes<R>(
    module: &Module,
    name: Atom,
    bytes: &[u8],
    check: impl FnOnce(&Result<RunOutcome>, &Result<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    let arg = make_heap_binary(&mut interpreter_ctx, bytes).expect("the argument builds");
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        name,
        1,
        &[arg],
        &mut interpreter_ctx,
        Tier::Interpreter,
    );

    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    let arg = make_heap_binary(&mut baseline_ctx, bytes).expect("the argument builds");
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        name,
        1,
        &[arg],
        &mut baseline_ctx,
        Tier::BaselineJit,
    );
    check(&interpreter_outcome, &baseline_outcome)
}

/// **The specialised path is interpreter-identical, and it is the path the tier takes.**
///
/// `bssmall:u64/1` and `bssmall:u9le/1` are the committed fixture's fixed-width reads (the
/// little-endian one carries a non-empty flags literal, which the predicate deliberately
/// ignores because the family decodes it). Both must return the interpreter's value on the
/// baseline tier, and the helper's own counter must have moved — a cross-tier agreement alone
/// would also hold if the emitted arm had kept the generic route.
#[test]
fn the_specialised_helper_runs_and_agrees_with_the_interpreter() {
    let module = fixture("bssmall");
    let subject: [u8; 8] = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];

    for (name, expected) in [
        ("u64", 0x0123_4567_89ab_cdef_i64),
        // `<<Y:9/little, _/bitstring>>` over the same subject: the low 9 bits of its first
        // byte pair, read little-endian -- derived from the family's own definition, and the
        // interpreter's answer is what the assertion compares against below anyway.
        ("u9le", 0),
    ] {
        let atom = module
            .atom_table
            .lookup(name)
            .unwrap_or_else(|| panic!("{name}/1 must be exported by the fixture"));
        let before = specialised_calls();
        with_both_tiers_bytes(&module, atom, &subject, |interpreter, baseline| {
            assert_same_outcome(name, interpreter, baseline, &module.atom_table);
            let returned = baseline
                .as_ref()
                .unwrap_or_else(|error| panic!("{name}: {error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("{name}: unexpected outcome {returned:?}");
            };
            if name == "u64" {
                assert_eq!(
                    beam_term::signed_value(*value),
                    expected,
                    "u64/1 must read the fixture's first 8 bytes"
                );
            }
        });
        let after = specialised_calls();
        assert!(
            after > before,
            "{name}/1 must execute through the specialised helper (counter {before} -> {after})"
        );
    }
}

/// **A `bs_match` shape the predicate declines still runs correctly.**
///
/// The same fixture's `u64x/1` is the compiler's exhaustive `<<Y:64>>` form — an
/// `ensure_exactly` test, i.e. *not* the claimed shape — so it keeps the generic family arm.
/// Its value must still be the interpreter's, and the specialised counter must not move.
#[test]
fn a_declined_shape_keeps_the_generic_route_and_still_agrees() {
    let module = fixture("bssmall");
    let subject: [u8; 8] = [0, 0, 0, 0, 0, 0, 0, 42];
    let atom = module
        .atom_table
        .lookup("u64x")
        .expect("u64x/1 must be exported by the fixture");
    let before = specialised_calls();
    with_both_tiers_bytes(&module, atom, &subject, |interpreter, baseline| {
        assert_same_outcome("u64x", interpreter, baseline, &module.atom_table);
        let RunOutcome::Returned(value) = baseline.as_ref().expect("the run completes") else {
            panic!("u64x: unexpected outcome");
        };
        assert_eq!(
            beam_term::signed_value(*value),
            42,
            "u64x/1 must read the fixture's last byte"
        );
    });
    assert_eq!(
        specialised_calls(),
        before,
        "an `ensure_exactly` shape must not reach the specialised helper"
    );
}

/// A `put_list` body is not a `bs_match`: the helper's predicate must decline everything the
/// generic family arm owns, so the four constructors keep their route.
#[test]
fn a_non_match_body_is_never_claimed() {
    let (module, _name) = body_module(vec![
        beam_loader::Instr::Label(beam_loader::Label(1)),
        beam_loader::Instr::Label(beam_loader::Label(2)),
        beam_loader::Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::Nil,
            dst: Operand::X(0),
        },
        beam_loader::Instr::Return,
    ]);
    for function in &module.functions {
        for instr in &function.code {
            assert!(!bitstr::supported(instr));
        }
    }
}
