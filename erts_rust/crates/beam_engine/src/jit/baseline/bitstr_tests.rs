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
//! Pins for the bitstring-family helper (`super::bitstr`). A `#[path]` sibling of
//! `super::tests`, so it reuses that module's harness (`body_module`,
//! `assert_same_outcome`, the `MinimalContext` route).
//!
//! Four claims, one per concern:
//!
//! * the **route predicate** [`bitstr::claimed`] agrees, instruction for instruction, with
//!   the bitstring family's own statement of its variant set
//!   ([`crate::families::bitstrings::exec`]'s `Ok(None)` contract) over the committed real
//!   `.beam` fixtures — so the bypass can neither miss a `Bs*` variant (the router hop stays
//!   paid) nor take one another family owns;
//! * it covers **every** `Bs*` variant the router names — each one is asserted on a
//!   hand-built instance, so a future `Instr` variant cannot silently fall out of the set;
//! * every other family's instructions keep the generic family route (the predicate declines
//!   them, and the specialised helper's counter does not move for them);
//! * the specialised path is **interpreter-identical** on real fixture functions covering
//!   matching, string matching, tail reads and construction — while the helper's own counter
//!   proves the emitted arm actually took it (a cross-tier agreement alone would also hold
//!   if the arm never fired).

use beam_common::Result;
use beam_loader::{load_file, BsGetArgs, Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{Atom, Eterm};

use super::bitstr::{self, bitstr_calls};
use super::tests::{assert_same_outcome, body_module, with_both_tiers};
use crate::context::MinimalContext;
use crate::families::bitstrings;
use crate::interpreter::LabelMap;
use crate::jit::Tier;
use crate::machine::{Machine, RunOutcome};
use crate::terms::{make_heap_binary, make_integer};

/// Load one of the committed `erlc` fixtures (`tests/fixtures/*.beam`).
fn fixture(name: &str) -> Module {
    let path = format!("{}/tests/fixtures/{name}.beam", env!("CARGO_MANIFEST_DIR"));
    load_file(&path).unwrap_or_else(|error| panic!("load {path}: {error:?}"))
}

/// The fixtures whose instructions the route predicate is checked against: the whole
/// committed bitstring surface (matching, string matching, UTF, wide fields, construction,
/// failure paths) plus one variant-proof module. They carry the family's real compiler output,
/// so the comparison runs over the exact opcodes `erlc` emits — not a hand-written IR.
const FIXTURE_SET: [&str; 8] = [
    "bitstrings",
    "bsmatch",
    "bscreate",
    "bssmall",
    "bsstring",
    "bsbig",
    "bsfailseg",
    "vb_bitstr",
];

/// Whether the family itself claims `instr`: its own `Ok(None)` contract is the authority
/// [`bitstr::claimed`] must agree with.
fn family_claims(module: &Module, instr: &Instr) -> bool {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let labels = LabelMap::new();
    !matches!(
        bitstrings::exec(&mut machine, module, &labels, instr, 0, 0, &mut ctx),
        Ok(None)
    )
}

/// **The predicate agrees with the family's own variant set, over real `.beam`s.**
///
/// For every instruction of every fixture, `claimed(instr)` must equal the family's own
/// answer. Both directions matter:
///
/// * a `Bs*` variant the predicate misses keeps paying the router hop (the whole point of the
///   lever), and
/// * a variant the predicate claims but the family does not own would take the router's job
///   away from the emitter.
#[test]
fn the_route_predicate_agrees_with_the_family_on_real_beam_instructions() {
    let mut checked = 0usize;
    let mut claimed = 0usize;
    for name in FIXTURE_SET {
        let module = fixture(name);
        for (func, function) in module.functions.iter().enumerate() {
            for (pc, instr) in function.code.iter().enumerate() {
                let ours = bitstr::claimed(instr);
                let theirs = family_claims(&module, instr);
                assert_eq!(
                    ours, theirs,
                    "{name}:{func}:{pc}: claimed() says {ours} but the family says {theirs} \
                     for {instr:?}"
                );
                checked += 1;
                claimed += usize::from(ours);
            }
        }
    }
    assert!(
        checked > 1_000,
        "the fixture set must cover a real instruction corpus (checked {checked})"
    );
    assert!(
        claimed > 50,
        "the fixture set must exercise the family's variant set (claimed {claimed} of {checked})"
    );
}

/// One hand-built instance per `Bs*` variant the router names, with plausible operands.
///
/// The list is the router's bitstring arm transcribed **independently** of
/// [`bitstr::claimed`] (a pin is only useful if it is written down separately), so a variant
/// added to — or dropped from — the predicate without the router fails here.
fn every_bitstring_variant() -> Vec<Instr> {
    let ctx = || Operand::X(1);
    vec![
        Instr::BsStartMatch3 {
            fail: Label(3),
            bin: Operand::X(0),
            live: 1,
            dst: ctx(),
        },
        Instr::BsStartMatch4 {
            fail: Operand::Integer(3),
            live: 1,
            src: Operand::X(0),
            dst: ctx(),
        },
        Instr::BsGetPosition {
            ctx: ctx(),
            dst: Operand::X(2),
            live: 2,
        },
        Instr::BsSetPosition {
            ctx: ctx(),
            pos: Operand::X(2),
        },
        Instr::BsGetTail {
            ctx: ctx(),
            dst: Operand::X(2),
            live: 2,
        },
        Instr::BsTestTail2 {
            fail: Label(3),
            ctx: ctx(),
            bits: 8,
        },
        Instr::BsGetInteger2(Box::new(BsGetArgs {
            fail: Label(3),
            ctx: ctx(),
            live: 2,
            size: Operand::Integer(8),
            unit: 1,
            flags: 0,
            dst: Operand::X(2),
        })),
        Instr::BsGetBinary2(Box::new(BsGetArgs {
            fail: Label(3),
            ctx: ctx(),
            live: 2,
            size: Operand::Integer(8),
            unit: 8,
            flags: 0,
            dst: Operand::X(2),
        })),
        Instr::BsGetFloat2(Box::new(BsGetArgs {
            fail: Label(3),
            ctx: ctx(),
            live: 2,
            size: Operand::Integer(64),
            unit: 1,
            flags: 0,
            dst: Operand::X(2),
        })),
        Instr::BsSkipBits2 {
            fail: Label(3),
            ctx: ctx(),
            size: Operand::Integer(8),
            unit: 1,
            flags: 0,
        },
        Instr::BsMatch {
            fail: Label(3),
            ctx: ctx(),
            commands: vec![Operand::Atom(Atom::MAX), Operand::Integer(8)],
        },
        Instr::BsMatchString {
            fail: Label(3),
            ctx: ctx(),
            bits: 8,
            offset: 0,
        },
        Instr::BsGetUtf8 {
            fail: Label(3),
            ctx: ctx(),
            live: 2,
            flags: 0,
            dst: Operand::X(2),
        },
        Instr::BsGetUtf16 {
            fail: Label(3),
            ctx: ctx(),
            live: 2,
            flags: 0,
            dst: Operand::X(2),
        },
        Instr::BsGetUtf32 {
            fail: Label(3),
            ctx: ctx(),
            live: 2,
            flags: 0,
            dst: Operand::X(2),
        },
        Instr::BsSkipUtf8 {
            fail: Label(3),
            ctx: ctx(),
            live: 2,
            flags: 0,
        },
        Instr::BsSkipUtf16 {
            fail: Label(3),
            ctx: ctx(),
            live: 2,
            flags: 0,
        },
        Instr::BsSkipUtf32 {
            fail: Label(3),
            ctx: ctx(),
            live: 2,
            flags: 0,
        },
        Instr::BsInitWritable,
        Instr::BsCreateBin {
            fail: NO_FAIL,
            alloc: 0,
            live: 1,
            unit: 8,
            dst: Operand::X(0),
            segments: Vec::new(),
        },
    ]
}

/// **Every `Bs*` variant the router names is claimed, and nothing else is.**
#[test]
fn the_predicate_claims_the_whole_bitstring_family_and_only_it() {
    let variants = every_bitstring_variant();
    assert_eq!(
        variants.len(),
        20,
        "the router's bitstring arm names twenty variants"
    );
    for instr in &variants {
        assert!(
            bitstr::claimed(instr),
            "the predicate must claim every Bs* variant: {instr:?}"
        );
    }

    // The converse: a representative of every **other** family, plus core-loop instructions,
    // must keep its own route. `GetList`/`PutList` are the `lists` family's, `Try` the
    // exceptions', `Send`/`LoopRec` the receive family's, `Fmove` the floats', `MakeFun3` the
    // funs', `PutTuple2`/`GetTupleElement` the tuples', `Move`/`Return`/`Label` core arms.
    let others = vec![
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::Nil,
            dst: Operand::X(0),
        },
        Instr::GetList {
            source: Operand::X(0),
            head: Operand::X(1),
            tail: Operand::X(2),
        },
        Instr::GetHd {
            source: Operand::X(0),
            head: Operand::X(1),
        },
        Instr::GetTl {
            source: Operand::X(0),
            tail: Operand::X(1),
        },
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::Integer(1)],
        },
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 0,
            dst: Operand::X(1),
        },
        Instr::Try {
            frame: Operand::X(0),
            fail: Label(4),
        },
        Instr::Send,
        Instr::LoopRec {
            fail: Label(3),
            dst: Operand::X(0),
        },
        Instr::Fmove {
            src: Operand::X(0),
            dst: Operand::X(1),
        },
        Instr::MakeFun3 {
            index: 0,
            dst: Operand::X(0),
            env: Vec::new(),
        },
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(2)),
    ];
    for instr in &others {
        assert!(
            !bitstr::claimed(instr),
            "the predicate must not claim another family's (or a core-arm) instruction: \
             {instr:?}"
        );
    }
}

/// The one argument a case row passes to the exported function under test, built **per side**
/// (each run owns its own `MinimalContext` heap).
#[derive(Clone, Copy)]
enum Arg {
    Bytes(&'static [u8]),
    Int(i64),
}

impl Arg {
    fn build(self, ctx: &mut MinimalContext) -> Eterm {
        match self {
            Arg::Bytes(bytes) => make_heap_binary(ctx, bytes).expect("the argument builds"),
            Arg::Int(value) => make_integer(ctx, value).expect("the argument builds"),
        }
    }
}

/// Run exported `name/1` with `arg` on both tiers, handing the two outcomes to `check`.
///
/// Each run builds its own heap (a returned boxed term points into it), so the comparison
/// happens *inside* this call.
fn with_both_tiers_arg<R>(
    module: &Module,
    name: Atom,
    arg: Arg,
    check: impl FnOnce(&Result<RunOutcome>, &Result<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    let value = arg.build(&mut interpreter_ctx);
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        name,
        1,
        &[value],
        &mut interpreter_ctx,
        Tier::Interpreter,
    );

    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    let value = arg.build(&mut baseline_ctx);
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        name,
        1,
        &[value],
        &mut baseline_ctx,
        Tier::BaselineJit,
    );
    check(&interpreter_outcome, &baseline_outcome)
}

/// The `name/1` of `module`, or a panic naming the fixture.
fn export(module: &Module, fixture_name: &str, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("{fixture_name}:{name}/1 must be exported"))
}

/// **The helper executes every claimed shape on the real fixtures and is interpreter-identical.**
///
/// Each case is a committed `erlc`-compiled function whose body is a different member of the
/// family — `bs_start_match3` + `bs_get_integer2` (`first8/1`), `bs_test_tail2` (`head16/1`),
/// the fixed-width `bs_match` (`u64/1`), the standalone `bs_match_string` (`strlong/1`) and
/// `bs_create_bin` (`u8/1`) — and each must return the interpreter's value on the baseline
/// tier **with the helper's counter moving**, which is what proves the emitted arm (rather
/// than the generic family arm) ran.
#[test]
fn every_claimed_shape_runs_through_the_helper_and_agrees_with_the_interpreter() {
    // `<<1,2,3,4,5,6,7,8,...>>`: `first8`/`u64` read its first byte/eight bytes.
    let subject: &'static [u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    // `head16/1` is the exhaustive `<<X:16>>` form, so its subject must be exactly two bytes.
    let pair: &'static [u8] = &[1, 2];
    // `bsmatch:strlong/1` starts with the literal `<<"abcdefghij", R/binary>>`.
    let literal: &'static [u8] = b"abcdefghijk";
    // (fixture, function, argument, the expected small-int result — `None` for a binary result,
    // which the bit-exact cross-tier comparison covers on its own).
    let cases: [(&str, &str, Arg, Option<i64>); 5] = [
        ("bitstrings", "first8", Arg::Bytes(subject), Some(1)),
        ("bitstrings", "head16", Arg::Bytes(pair), Some(0x0102)),
        ("bssmall", "u64", Arg::Bytes(subject), Some(0x0102_0304_0506_0708)),
        // `strlong/1` returns the tail **binary** after the 10-byte literal.
        ("bsmatch", "strlong", Arg::Bytes(literal), None),
        ("bscreate", "u8", Arg::Int(65), None),
    ];
    for (fixture_name, function, arg, expected) in cases {
        let module = fixture(fixture_name);
        let atom = export(&module, fixture_name, function);
        let before = bitstr_calls();
        with_both_tiers_arg(&module, atom, arg, |interpreter, baseline| {
            assert_same_outcome(
                &format!("{fixture_name}:{function}/1"),
                interpreter,
                baseline,
                &module.atom_table,
            );
            let returned = baseline
                .as_ref()
                .unwrap_or_else(|error| panic!("{fixture_name}:{function}/1: {error:?}"));
            let RunOutcome::Returned(value) = returned else {
                panic!("{fixture_name}:{function}/1: unexpected outcome {returned:?}");
            };
            if let Some(expected) = expected {
                assert_eq!(
                    beam_term::signed_value(*value),
                    expected,
                    "{fixture_name}:{function}/1 answered the wrong value"
                );
            }
        });
        let after = bitstr_calls();
        assert!(
            after > before,
            "{fixture_name}:{function}/1 must execute through the bitstring helper \
             (counter {before} -> {after})"
        );
    }
}

/// **A failing match takes the same path**: `first8/1` on an empty subject raises exactly
/// what the interpreter raises, and the helper still ran (`bs_start_match3` succeeded, the
/// `bs_get_integer2` failed into its fail label — a `Goto` the fold encodes for the driver).
#[test]
fn a_failing_match_agrees_with_the_interpreter_and_still_bypasses_the_router() {
    let module = fixture("bitstrings");
    let atom = export(&module, "bitstrings", "first8");
    let before = bitstr_calls();
    with_both_tiers_arg(&module, atom, Arg::Bytes(&[]), |interpreter, baseline| {
        assert_same_outcome(
            "first8/1 on an empty subject",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
    assert!(
        bitstr_calls() > before,
        "a failing match still reaches the bitstring family through the helper"
    );
}

/// **A non-bitstring family instruction keeps the generic route.**
///
/// The helper's counter must not move for a `put_list` body: the emitted arm claims only
/// [`bitstr::claimed`]'s variants, and everything else keeps the shared `jit_family` helper.
#[test]
fn a_non_bitstring_body_keeps_the_generic_family_route() {
    let (module, name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::Nil,
            dst: Operand::X(0),
        },
        Instr::Return,
    ]);
    for function in &module.functions {
        for instr in &function.code {
            assert!(
                !bitstr::claimed(instr),
                "a put_list body must not be claimed: {instr:?}"
            );
        }
    }
    let before = bitstr_calls();
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome("put_list", interpreter, baseline, &module.atom_table);
    });
    assert_eq!(
        bitstr_calls(),
        before,
        "a put_list must not reach the bitstring helper"
    );
}
