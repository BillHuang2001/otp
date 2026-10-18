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

//! **Data-access variants on real compiler output**: `GetTupleElement`, `GetList`,
//! `GetHd`, `GetTl`, `UpdateRecord`, `Badrecord`.
//!
//! Sources: the committed `erlc` fixtures under `tests/fixtures/` (and, where a
//! variant is best reached through library code, a module of the installed system
//! OTP). Every row asserts the dynamic census count of its variant **and** the
//! oracle's answer for the same call — see the crate docs of
//! `real_beam_variant_proofs.rs` for the proof shape and `harness` for the
//! implementation.
//!
//! # How the compiler chooses among the list reads
//!
//! `get_list`, `get_hd` and `get_tl` are all emitted from an ordinary cons match;
//! *which* one depends on how many parts of the cell the body uses (verified with
//! `beam_disasm:file/1` over the compiled fixtures, not assumed):
//!
//! | body | instruction |
//! |---|---|
//! | `[H \| T] -> {H, T}` (head **and** tail) | `get_list` |
//! | `[H \| _] -> H` (head only) | `get_hd` |
//! | `[_ \| T] -> T` (tail only) | `get_tl` |
//!
//! `tests/fixtures/vb_data.beam` carries exactly those three exports (plus the
//! record shapes) under a module name no OTP application uses, so the **same**
//! call can be asked of the engine and of a live `erl`. (`tests/fixtures/
//! lists.beam` compiles to the same opcodes but is *named* `lists`, so on the
//! oracle side it is shadowed by the system `lists` — which has no `head/1` —
//! and no oracle answer exists for its exports.)

use beam_common::BeamError;
use beam_term::{make_small, Eterm};

use crate::harness::{host, oracle_call, oracle_w, render, run, skip_if_unavailable};
use crate::support::{term_atom, term_list, term_tuple, ModuleHost};

/// `GetTupleElement` — `get_tuple_element` in a `case` head, from committed
/// compiler output.
///
/// `tuples.beam:first/1` is `case T of {A,_B} -> A end`, so the element read is
/// the compiler's `get_tuple_element {x,0},1` (see
/// `tests/fixtures/README.md`), not a hand-built instruction.
///
/// Observed census over this run: `GetTupleElement = 1` (with `Label`, `Test(IsTuple)`,
/// `Test(TestArity)`, `Return`).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[tuples:first({a,1})]),halt(0).'`
/// → `a`. (Appended, never prepended: several fixtures shadow stdlib modules, and
/// `-pa` makes the emulator die at boot — see `tests/CONTEXT.md`.)
#[test]
fn get_tuple_element_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("tuples", "tuples.beam")]);
    let tag = term_atom(&mut host, "a");
    let tuple = term_tuple(&mut host, &[tag, make_small(1)]);
    let proof = run(&mut host, "tuples", "first", &[tuple]);
    proof.require(&["GetTupleElement"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("tuples", "first", "{a,1}")
    );
}

/// `GetList` — the cons split the compiler emits when a `case` head uses **both**
/// halves of the cell.
///
/// `vb_data.beam:split/1` is `case L of [H|T] -> {H,T} end`; `beam_disasm:file/1`
/// on the committed `.beam` lists `get_list` in `split/1` and in no other export.
///
/// Observed census over this run: `GetList = 1` (with `Label`, `Test(IsNonemptyList)`,
/// `TestHeap`, `PutTuple2`, `Return`).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_data:split([1,2,3])]),halt(0).'`
/// → `{1,[2,3]}`.
#[test]
fn get_list_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_data", "vb_data.beam")]);
    let list = term_list(&mut host, &[make_small(1), make_small(2), make_small(3)]);
    let proof = run(&mut host, "vb_data", "split", &[list]);
    proof.require(&["GetList"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_data", "split", "[1,2,3]")
    );
}

/// `GetHd` — the head-only cons read.
///
/// `vb_data.beam:head/1` is `case L of [H|_T] -> H end`: the tail is unused, so
/// the compiler emits `get_hd` instead of a `get_list` plus a discarded register.
/// (`tests/fixtures/lists.beam:head/1` compiles to the same instruction; this
/// module is used because its name is oracle-loadable.)
///
/// Observed census over this run: `GetHd = 1` (with `Label`, `Test(IsNonemptyList)`,
/// `Return`).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_data:head([7,8,9])]),halt(0).'`
/// → `7`.
#[test]
fn get_hd_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_data", "vb_data.beam")]);
    let list = term_list(&mut host, &[make_small(7), make_small(8), make_small(9)]);
    let proof = run(&mut host, "vb_data", "head", &[list]);
    proof.require(&["GetHd"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_data", "head", "[7,8,9]")
    );
}

/// `GetTl` — the tail-only cons read.
///
/// `vb_data.beam:tail/1` is `case L of [_H|T] -> T end`: the head is unused, so
/// the compiler emits `get_tl`.
///
/// Observed census over this run: `GetTl = 1` (with `Label`, `Test(IsNonemptyList)`,
/// `Return`).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_data:tail([7,8,9])]),halt(0).'`
/// → `[8,9]`.
#[test]
fn get_tl_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_data", "vb_data.beam")]);
    let list = term_list(&mut host, &[make_small(7), make_small(8), make_small(9)]);
    let proof = run(&mut host, "vb_data", "tail", &[list]);
    proof.require(&["GetTl"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_data", "tail", "[7,8,9]")
    );
}

/// `UpdateRecord` — a single-field record update with the `reuse` hint.
///
/// `vb_data.beam:update_a/1` is `R#r{a = 10}` over `-record(r, {a, b, c})` (the
/// tuple `{r, A, B, C}`), so the update is the compiler's `update_record` with
/// destination == source register. The engine always copies, which is
/// observationally identical for a returned record.
///
/// Observed census over this run: `UpdateRecord = 1` (with `Label`,
/// `Test(IsTaggedTuple)`, `TestHeap`, `Return`).
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_data:update_a({r,1,2,3})]),halt(0).'`
/// → `{r,10,2,3}`.
#[test]
fn update_record_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_data", "vb_data.beam")]);
    let record = record_r(&mut host, 1, 2, 3);
    let proof = run(&mut host, "vb_data", "update_a", &[record]);
    proof.require(&["UpdateRecord"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_data", "update_a", "{r,1,2,3}")
    );
}

/// `UpdateRecord` again — the **multi-field** form the compiler emits when more
/// than one field changes at once (the `copy` hint, two update offsets).
///
/// `vb_data.beam:update_a_c/1` is `R#r{a = 11, c = 33}`, i.e. one `update_record`
/// carrying the flat field/value list `[2, 11, 4, 33]` (offsets 2 and 4) with the
/// `copy` hint — the shape a single-field row cannot cover.
///
/// Observed census over this run: `UpdateRecord = 1` (with `Label`,
/// `Test(IsTaggedTuple)`, `TestHeap`, `Return`) — one instruction carrying both
/// field updates.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[vb_data:update_a_c({r,1,2,3})]),halt(0).'`
/// → `{r,11,2,33}`.
#[test]
fn update_record_multi_field_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_data", "vb_data.beam")]);
    let record = record_r(&mut host, 1, 2, 3);
    let proof = run(&mut host, "vb_data", "update_a_c", &[record]);
    proof.require(&["UpdateRecord"]);
    assert_eq!(
        render(&host, proof.value()),
        oracle_call("vb_data", "update_a_c", "{r,1,2,3}")
    );
}

/// `Badrecord` — the compiler's generated raise for a record access on a
/// non-record term.
///
/// `vb_data.beam:non_record/1` is `V#r{a = 1}`: `is_tagged_tuple` rejects a
/// non-record, so the generated `badrecord` instruction raises
/// `error:{badrecord, V}` — the compiled `update_record` itself never runs. The
/// raise is **uncaught** here, so it leaves the machine as the typed
/// `BeamError::Raise` whose `live_reason` slot carries the reason *term*
/// (`ExitReason::Structured`, the `beam_common` convention), which is rendered and
/// compared with the oracle below.
///
/// Observed census over this run: `Badrecord = 1` (with `Label` x2,
/// `Test(IsTaggedTuple)`), i.e. the raising instruction is counted as executed and
/// the raise still surfaces as the typed error.
///
/// Oracle: `erl -noshell -pz tests/fixtures -eval 'io:format("~w~n",[try vb_data:non_record(7) catch C:R -> {C,R} end]),halt(0).'`
/// → `{error,{badrecord,7}}`.
#[test]
fn badrecord_executes_on_real_compiler_output() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host(&[("vb_data", "vb_data.beam")]);
    let proof = run(&mut host, "vb_data", "non_record", &[make_small(7)]);
    proof.require(&["Badrecord"]);
    // The census is also the evidence for the claim above: this run took the
    // `is_tagged_tuple` failure arm, so the compiled `update_record` never ran.
    proof.require_exactly("UpdateRecord", 0);
    let raise = match proof.error() {
        BeamError::Raise(raise) => raise,
        other => panic!(
            "expected the `{{badrecord, 7}}` raise, got {other:?}\n{}",
            proof.report()
        ),
    };
    let reason = live_reason_term(raise)
        .unwrap_or_else(|| panic!("the raise carries no live reason term\n{}", proof.report()));
    // `{Class,Reason}` in the oracle's own notation, so the comparison covers the
    // class *and* the reason term rather than a hand-written expectation.
    let observed = format!("{{{},{}}}", raise.class.as_str(), render(&host, reason));
    assert_eq!(
        observed,
        oracle_w("try vb_data:non_record(7) catch C:R -> {C,R} end")
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The record `#r{}` of `vb_data.beam` as a term: `{r, A, B, C}`.
fn record_r(host: &mut ModuleHost, a: i64, b: i64, c: i64) -> Eterm {
    let tag = term_atom(host, "r");
    term_tuple(host, &[tag, make_small(a), make_small(b), make_small(c)])
}

/// The raise's **live reason term** — what a live catch frame would land in `x(1)`
/// (the same recovery `tests/records.rs` and `families/maps/tests.rs` use).
fn live_reason_term(raise: &beam_common::Raise) -> Option<Eterm> {
    raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
}
