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
//! The **legacy (pre-`try`) `catch`** fold's cooked stacktrace.
//!
//! A legacy `catch` folds a class-`error` exception into
//! `{'EXIT',{Reason,Stacktrace}}` — C's `add_stacktrace` pair
//! (`emu/instrs.tab:1188-1211`, `beam_common.c:730-735`) — which is the **same**
//! cooked trace a `try … catch C:R:S` produces for the very same raise. Both routes
//! therefore have to cook the raise's one-shot synthesized **head item** (the
//! failing BIF's `{Module,Function,ArgList,Location}`, [`super::bif_frame`]): the
//! item [`Instr::BuildStacktrace`] has always consumed for the `try` route, and the
//! one [`Instr::CatchEnd`](../mod.rs)'s class-`error` arm must consume too.
//!
//! Before the latch consumption landed there, that arm cooked with `head = false`
//! and never took the latch, so its list started at the first `raise_snapshot`
//! frame — the **caller** that invoked the BIF — where the oracle names the BIF
//! (`erts_rust/tools/suites/results/stdlib-diagnostics/FINDINGS.md` §D5; the
//! suite-level symptom is `maps_SUITE:t_from_keys/t_with_2/…`).
//!
//! This module pins the fold's whole matrix against the system oracle:
//!
//! * the head-carrying shapes — a `maps:with/2` raise (a **live term** reason,
//!   `{badmap,a}`) and a `lists:reverse/1` `function_clause` (a bare atom reason,
//!   whose item therefore carries **no** `error_info` extension);
//! * the two head-less ones — a bare `badarith` (an instruction-level raise, so no
//!   head item exists at all) and the `throw`/`exit` folds, which cook no trace;
//! * an exception **propagating** out of a `try` into an enclosing legacy `catch`
//!   (the propagation site keeps the in-flight head item);
//! * an `erlang:raise/3` re-raise inside a legacy `catch` (the supplied trace keeps
//!   winning over the latch).
//!
//! Every row runs on the interpreter **and** the native baseline tier and the two
//! must answer identically; each is then compared with the system oracle.
//!
//! # The residual (documented, never fabricated)
//!
//! The oracle's `maps:with/2` item carries the BIF's **source position**
//! (`[{file,"maps.erl"},{line,1303},{error_info,#{module => erl_stdlib_errors}}]`)
//! and its `lists:reverse/1` item `[{file,"lists.erl"},{line,292}]`. A synthesized
//! item has no code to look a position up in (see [`super::bif_frame`] and
//! `src/families/exceptions/CONTEXT.md`), so this engine cooks the extension alone
//! (`[{error_info,#{module => …}}]`) or `[]`. The head item's `Module`/`Function`/
//! `ArgList` and every **frame after it** (four fields, source position included)
//! are compared in full.
//!
//! # Oracle discipline
//!
//! The fixture is compiled by the system `erlc` and the same call is asked of the
//! system `erl`, printing each field with `~500p` so the comparison is textual,
//! single-line and exact (never a hand-written expectation that could drift). Every
//! test **skips loudly** (`eprintln!`) when `erlc`/`erl` are absent; a spawned
//! tool that ran and exited non-zero panics with its stderr — a harness failure must
//! not masquerade as an absent toolchain.
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_common::{ExitReason, LiveReason, Raise};
use beam_loader::Atom;
use beam_term::{is_atom, Eterm};

use crate::jit::Tier;

use super::oracle::{
    assert_matches_the_oracle, atom, compile_module, engine_trace, load_module, oracle_trace,
    render_atom, render_flat, render_item, run_fixture,
};
use super::*;
// The production builders take the `Machine` plus a reader closure so a moving
// collector can relocate operands between the reserve and the read; these tests
// build from already-materialized terms against a non-collecting arena, so alias
// `terms` to the test-only wrappers that keep the old ergonomic arity. (An explicit
// import shadows the glob-imported `terms` above.)
use crate::terms::test_support as terms;

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// The fixture module, compiled by the system `erlc`.
///
/// Every row answers the **same** `{Class,Reason,Trace}` triple, whether it reached
/// it through a legacy `catch` fold (`fold/1` normalises the three fold shapes) or
/// through a `try … catch C:R:S`, so one probe compares both routes.
const FIXTURE_SRC: &str = r#"-module(cf).
-export([legacy_maps_with/0,
         modern_maps_with/0,
         legacy_reverse/0,
         modern_reverse/0,
         legacy_badarith/0,
         modern_badarith/0,
         legacy_throw/0,
         modern_throw/0,
         legacy_exit/0,
         modern_exit/0,
         propagated/0,
         reraise/0]).
%% The legacy `catch` fold: `{'EXIT',{Reason,Trace}}` for class `error`,
%% `{'EXIT',Reason}` for class `exit`, the bare value for class `throw`.
fold({'EXIT',{R,S}}) -> {error,R,S};
fold({'EXIT',R}) -> {error,R,[]};
fold(Other) -> {throw,Other,[]}.
%% A local call keeps the argument out of the compiler's constant folder.
id(X) -> X.
legacy_maps_with() -> fold(catch maps:with([a,b], id(a))).
modern_maps_with() ->
    try maps:with([a,b], id(a)) of V -> {ok,V} catch C:R:S -> {C,R,S} end.
legacy_reverse() -> fold(catch lists:reverse(a)).
modern_reverse() ->
    try lists:reverse(a) of V -> {ok,V} catch C:R:S -> {C,R,S} end.
legacy_badarith() -> fold(catch 1/0).
modern_badarith() -> try 1/0 of V -> {ok,V} catch C:R:S -> {C,R,S} end.
%% A legacy `catch` of a `throw` yields the bare value, of an `exit` the
%% `{'EXIT',Value}` tuple; neither carries a trace at all. `'EXIT'` needs quoting in
%% the oracle's text, so the exit row re-tags the wrapper: the fold shape is still
%% observable (`{exited,V}` never appears if the fold is wrong).
legacy_throw() -> {throw, catch throw(x), []}.
legacy_exit() ->
    case catch exit(x) of
        {'EXIT', V} -> {exit, {exited, V}, []};
        V -> {exit, {not_wrapped, V}, []}
    end.
modern_throw() -> try throw(x) catch C:R:S -> {C,R,S} end.
modern_exit() -> try exit(x) catch C:R:S -> {C,R,S} end.
%% An exception propagating out of a `try` into an enclosing legacy `catch`: the
%% `raise` propagation site must carry the in-flight head item onward.
propagated() ->
    fold(catch (try maps:with([a,b], id(a)) catch nomatch -> nomatch end)).
%% An `erlang:raise/3` re-raise of the caught triple, inside a legacy `catch`: the
%% supplied trace is echoed verbatim and must keep winning over the latch.
reraise() ->
    case catch maps:with([a,b], id(a)) of
        {'EXIT',{R,S}} -> fold(catch erlang:raise(error, R, S));
        Other -> Other
    end.
"#;

/// The names a real runtime keeps interned for **every** module but the OTP compiler
/// does not emit into the fixture's atom chunk: the reason atoms the raising
/// instruction/BIFs carry and the `error_info` extension's three names.
const EXTRA_ATOMS: &[&str] = &[
    "badarith",
    "badmap",
    "function_clause",
    "error_info",
    "module",
    "erl_stdlib_errors",
];

/// Compile the fixture and load it. `None` when no runnable `erlc` is on `PATH`
/// (the caller skips loudly).
fn fixture(tag: &str) -> Option<(PathBuf, Module)> {
    let dir = compile_module(tag, "cf", FIXTURE_SRC)?;
    let module = load_module(&dir, "cf", EXTRA_ATOMS);
    Some((dir, module))
}

// ---------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------

/// A class-`error` raise whose reason is a **live term** — the shape a `beam_bifs`
/// BIF raises for `{badmap,M}`/`{badkey,K}` (the `live_reason` payload convention is
/// the reason term itself).
fn structured_error(reason: Eterm) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::Structured).with_live_reason(LiveReason::new(reason)))
}

/// A class-`error` raise with a bare atom reason.
fn atom_error(name: &'static str) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(name)))
}

/// The `MinimalContext` BIF floor the fixture needs: the four failing calls its rows
/// make. Every other call is the engine's typed `unsupported`, so a stray call the
/// fixture is not supposed to make fails loudly instead of quietly passing the test.
///
/// The `{badmap,a}` reason is built **before** the hook is installed: the hook has no
/// allocator, and a `MinimalContext` arena is pointer-stable (it grows in place), so
/// the word stays valid for the whole run.
fn fixture_host(module: &Module) -> MinimalContext {
    let mut ctx = MinimalContext::new();
    let badmap = terms::make_tuple(
        &mut ctx,
        &[make_atom(atom(module, "badmap")), make_atom(atom(module, "a"))],
    )
    .expect("the {badmap,a} reason term");
    ctx.set_bif_hook(Box::new(
        move |bif_module, function, arity, args| match (bif_module, function, arity) {
            // `maps:with([a,b], a)` ⇒ `error:{badmap,a}` (the `maps` table's
            // `format_error/2` owner is `erl_stdlib_errors`).
            ("maps", "with", 2) => Err(structured_error(badmap)),
            // `lists:reverse(a)` ⇒ `error:function_clause` with **no** `error_info`
            // extension (the shape rule drops it for that bare reason).
            ("lists", "reverse", 1) => Err(atom_error("function_clause")),
            ("erlang", "throw", 1) | ("erlang", "exit", 1) => {
                assert!(is_atom(args[0]), "the fixture throws/ exits an atom, got {:?}", args[0]);
                Err(BeamError::Raise(if function == "throw" {
                    Raise::throw(ExitReason::atom("x"))
                } else {
                    Raise::exit(ExitReason::atom("x"))
                }))
            }
            // The fixture's only re-raise is `erlang:raise(error, R, S)`: the reason
            // is a live term and the third argument is the trace to echo verbatim.
            ("erlang", "raise", 3) => Err(BeamError::Raise(
                Raise::error(ExitReason::Structured)
                    .with_live_reason(LiveReason::new(args[1]))
                    .with_supplied_trace(LiveReason::new(args[2])),
            )),
            _ => Err(crate::error::unsupported(&format!(
                "bif {bif_module}:{function}/{arity}"
            ))),
        },
    ));
    ctx
}

// ---------------------------------------------------------------------------
// Rendering — the engine's terms in the oracle probe's text
// ---------------------------------------------------------------------------

/// The `Module|Function|Third` fields of one cooked item — everything except the
/// `Location`, which is the documented residual for a synthesized item and is
/// asserted separately ([`location_keys`]).
fn three_fields(item: Eterm, module: &Module) -> String {
    let name = render_atom(
        crate::terms::tuple_element(item, 0).expect("the item's module"),
        module,
    );
    let function = render_atom(
        crate::terms::tuple_element(item, 1).expect("the item's function"),
        module,
    );
    let third = crate::terms::tuple_element(item, 2).expect("the item's third field");
    format!("{name}|{function}|{}", render_flat(third, module))
}

/// The **keys** of a cooked item's `Location` list, in order: none for `[]`, the
/// `file`/`line` pair of a resolved source position, and at most one trailing
/// `error_info` extension (C's `erts_build_mfa_item`,
/// `../../erts/emulator/beam/utils.c:3430-3457`).
fn location_keys(location: Eterm, module: &Module) -> Vec<String> {
    if is_nil(location) {
        return Vec::new();
    }
    crate::terms::list_elements(location)
        .expect("a location is a proper list")
        .iter()
        .map(|pair| {
            render_atom(
                crate::terms::tuple_element(*pair, 0).expect("a location pair's key"),
                module,
            )
        })
        .collect()
}

/// One row's answer, rendered the way the oracle probe prints it.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    /// `Class|Reason`.
    head: String,
    /// `Module|Function|Third` per cooked item, the innermost first.
    fields: Vec<String>,
    /// The full four-field render of every cooked item **after the first** — the
    /// builder's source position included, so a frame's `{file,_}`/`{line,_}` is
    /// pinned whenever it is not the synthesized head item.
    frames: Vec<String>,
    /// [`location_keys`] of the first cooked item.
    first_location: Vec<String>,
}

/// Run one fixture row on `tier` and render its `{Class,Reason,Trace}` answer.
fn engine_row(module: &Module, ctx: &mut MinimalContext, name: Atom, tier: Tier) -> Row {
    let (class, reason, cooked) = run_fixture(module, ctx, name, 0, &[], tier);
    let head = format!("{}|{}", render_flat(class, module), render_flat(reason, module));
    let items = crate::terms::list_elements(cooked).expect("the cooked stacktrace is a proper list");
    let fields = items.iter().map(|item| three_fields(*item, module)).collect();
    let frames = items.iter().skip(1).map(|item| render_item(*item, module)).collect();
    let first_location = items
        .first()
        .map(|item| {
            location_keys(
                crate::terms::tuple_element(*item, 3).expect("the item's location"),
                module,
            )
        })
        .unwrap_or_default();
    Row { head, fields, frames, first_location }
}

/// Ask the system `erl` for the same row: the `Class|Reason` line, then every
/// cooked item's three `Module`/`Function`/`Third` fields, then (after a `---`
/// marker) every item's **four** fields.
///
/// `~500p` keeps every field on one line — a `{file,"maps.erl"}` location list is
/// longer than `~p`'s default 80-column budget, and this module slices the answer by
/// line. `-pz`, never `-pa`: appending keeps the real stdlib ahead of the fixture.
/// `None` when no runnable `erl` is on `PATH`; a spawned `erl` that exits non-zero is
/// a harness failure and panics with its stderr.
fn oracle_row(dir: &Path, call: &str) -> Option<Row> {
    let probe = format!(
        "{{C,R,S}} = {call}, \
         io:format(\"~500p|~500p~n\",[C,R]), \
         lists:foreach(fun(I) -> \
             io:format(\"~500p|~500p|~500p~n\", \
                 [element(1,I),element(2,I),element(3,I)]) end, S), \
         io:format(\"---~n\",[]), \
         lists:foreach(fun(I) -> \
             io:format(\"~500p|~500p|~500p|~500p~n\", \
                 [element(1,I),element(2,I),element(3,I),element(4,I)]) end, S), \
         halt(0)."
    );
    let output = Command::new("erl")
        .args(["-noshell", "-pz"])
        .arg(dir)
        .args(["-eval", &probe])
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for {call}\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut lines = text.lines().map(str::trim_end).filter(|line| !line.is_empty());
    let head = lines
        .next()
        .expect("the oracle probe prints a class|reason line")
        .to_string();
    let rest: Vec<&str> = lines.collect();
    let marker = rest
        .iter()
        .position(|line| *line == "---")
        .expect("the oracle probe prints the second block after a `---` marker");
    // The first block renders one line per item with its three `Module`/`Function`/
    // `Third` fields; the second repeats them with the `Location` included.
    let fields = rest[..marker].iter().map(|line| line.to_string()).collect();
    // The item `engine_row` calls the head is the first line of the second block;
    // its location is the documented residual, so `frames` starts at the second.
    let frames = rest[marker + 1..].iter().skip(1).map(|line| line.to_string()).collect();
    Some(Row { head, fields, frames, first_location: Vec::new() })
}

/// Assert the engine's row agrees with the oracle's: the same `Class|Reason`, and a
/// **prefix** of the oracle's items (the oracle appends its own `erl_eval`/`init`
/// caller frames, which live outside this engine's run — see
/// [`super::oracle`]'s module docs), three-field and four-field alike.
fn assert_row_matches(what: &str, engine: &Row, oracle: &Row) {
    assert_eq!(engine.head, oracle.head, "{what}: the class/reason disagrees");
    assert!(
        engine.fields.len() <= oracle.fields.len(),
        "{what}: the engine cooked {} items, the oracle {}\n  engine: {:#?}\n  oracle: {:#?}",
        engine.fields.len(),
        oracle.fields.len(),
        engine.fields,
        oracle.fields
    );
    assert_eq!(
        engine.fields,
        &oracle.fields[..engine.fields.len()],
        "{what}: the cooked items are not a prefix of the oracle's"
    );
    assert!(
        engine.frames.len() <= oracle.frames.len(),
        "{what}: {} frame(s) after the first item, the oracle has {}\n  engine: {:#?}\n  oracle: {:#?}",
        engine.frames.len(),
        oracle.frames.len(),
        engine.frames,
        oracle.frames
    );
    assert_eq!(
        engine.frames,
        &oracle.frames[..engine.frames.len()],
        "{what}: the frames after the first item (four fields) are not a prefix of the oracle's"
    );
}

// ---------------------------------------------------------------------------
// The rows
// ---------------------------------------------------------------------------

/// One asserted row: which `cf` function to call, and what this engine must cook for
/// it — the item count, the first item's `Module|Function|Third` fields, and the
/// first item's `Location` **keys** (the oracle's `{file,_}`/`{line,_}` pair is the
/// documented residual of a synthesized item).
struct Expect {
    /// The `cf` export to call.
    call: &'static str,
    /// How many items the engine cooks (the oracle's trailing host frames excluded).
    items: usize,
    /// The first cooked item's `Module|Function|Third` fields.
    first: &'static str,
    /// The first cooked item's [`location_keys`].
    location: &'static [&'static str],
}

/// Run every row of `rows` on both tiers, assert the two agree, compare each with the
/// system oracle, and assert the exact cooked shape.
fn check_rows(tag: &str, rows: &[Expect]) {
    let Some((dir, module)) = fixture(tag) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the compiled legacy-catch rows are skipped");
        return;
    };
    for row in rows {
        let name = atom(&module, row.call);
        let mut interpreter_ctx = fixture_host(&module);
        let interpreted = engine_row(&module, &mut interpreter_ctx, name, Tier::Interpreter);
        let mut baseline_ctx = fixture_host(&module);
        let baseline = engine_row(&module, &mut baseline_ctx, name, Tier::BaselineJit);
        assert_eq!(
            interpreted, baseline,
            "cf:{}: the interpreter and the baseline tier cook differently",
            row.call
        );
        assert_eq!(
            interpreted.fields.len(),
            row.items,
            "cf:{}: cooked {} item(s), expected {}\n  engine: {:#?}",
            row.call,
            interpreted.fields.len(),
            row.items,
            interpreted.fields
        );
        if row.items > 0 {
            assert_eq!(
                interpreted.fields[0], row.first,
                "cf:{}: the first cooked item is not the expected head item\n  engine: {:#?}",
                row.call, interpreted.fields
            );
            assert_eq!(
                interpreted.first_location, row.location,
                "cf:{}: the first item's Location keys are {:?}, expected {:?} (the synthesized \
                 item's source position is the documented residual, never fabricated)",
                row.call, interpreted.first_location, row.location
            );
        }
        if row.items > 1 {
            assert_eq!(
                interpreted.fields[1],
                format!("cf|{}|0", row.call),
                "cf:{}: the frame right after the head item must be the raising function\n  \
                 engine: {:#?}",
                row.call,
                interpreted.fields
            );
        }
        let Some(oracle) = oracle_row(&dir, &format!("cf:{}()", row.call)) else {
            eprintln!("SKIP: no system `erl` on PATH -- the legacy-catch oracle check is skipped");
            return;
        };
        assert_row_matches(&format!("cf:{}/0", row.call), &interpreted, &oracle);
    }
}

// ---------------------------------------------------------------------------
// (a) the head-carrying raise shapes, folded and `try`-caught
// ---------------------------------------------------------------------------

/// `cf:legacy_maps_with/0` is `fold(catch maps:with([a,b], id(a)))` and its
/// `modern_` twin is the same raise under `try … catch C:R:S`; both must cook the
/// BIF as the innermost item, `{maps,with,[[a,b],a]}` with the
/// `#{module => erl_stdlib_errors}` extension.
///
/// `cf:legacy_reverse/0` is `fold(catch lists:reverse(a))` — a bare
/// `function_clause` reason, so the shape rule drops the extension and the item's
/// `Location` is `[]` (the oracle's `{file,"lists.erl"},{line,292}` is the residual).
#[test]
fn a_legacy_fold_cooks_the_same_bif_head_item_as_its_try_catch_twin() {
    check_rows(
        "head_items",
        &[
            Expect {
                call: "legacy_maps_with",
                items: 2,
                first: "maps|with|[[a,b],a]",
                location: &["error_info"],
            },
            Expect {
                call: "modern_maps_with",
                items: 2,
                first: "maps|with|[[a,b],a]",
                location: &["error_info"],
            },
            Expect {
                call: "legacy_reverse",
                items: 2,
                first: "lists|reverse|[a]",
                location: &[],
            },
            Expect {
                call: "modern_reverse",
                items: 2,
                first: "lists|reverse|[a]",
                location: &[],
            },
        ],
    );
}

// ---------------------------------------------------------------------------
// (b) an exception propagating into — and re-raised inside — a legacy `catch`
// ---------------------------------------------------------------------------

/// The head item survives both detours.
///
/// `cf:propagated/0` raises inside a `try` whose handler does not match, so the
/// exception propagates out of it into the enclosing legacy `catch`: the `raise`
/// propagation site ([`Instr::Raise`], `Machine::throw_propagating`) must keep the
/// in-flight head item instead of starting a new trace.
///
/// `cf:reraise/0` hands the caught `{Reason,Trace}` to `erlang:raise/3` inside a
/// legacy `catch`: the **supplied** trace is echoed verbatim, so it must win over the
/// latch (which the re-raise's own raise leaves clear).
#[test]
fn an_exception_propagating_into_or_re_raised_in_a_legacy_catch_keeps_the_head_item() {
    check_rows(
        "propagation",
        &[
            Expect {
                call: "propagated",
                items: 2,
                first: "maps|with|[[a,b],a]",
                location: &["error_info"],
            },
            Expect {
                call: "reraise",
                items: 2,
                first: "maps|with|[[a,b],a]",
                location: &["error_info"],
            },
        ],
    );
}

// ---------------------------------------------------------------------------
// (c) the head-less shapes
// ---------------------------------------------------------------------------

/// `catch 1/0` raises `badarith` from an **instruction**, so no head item exists: the
/// cooked list starts at the raising function itself, and the engine must not invent
/// an item there.
///
/// Compared with the shared four-field harness ([`oracle_trace`] /
/// [`engine_trace`]) because a `badarith` frame's source position **is** resolved on
/// both sides — the location agrees, so the full item is comparable.
#[test]
fn a_badarith_legacy_fold_carries_no_synthesized_head_item() {
    let Some((dir, module)) = fixture("badarith") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the compiled badarith rows are skipped");
        return;
    };
    for call in ["legacy_badarith", "modern_badarith"] {
        let name = atom(&module, call);
        let mut interpreter_ctx = fixture_host(&module);
        let (class, reason, cooked) =
            run_fixture(&module, &mut interpreter_ctx, name, 0, &[], Tier::Interpreter);
        let (head, items) = engine_trace(class, reason, cooked, &module);
        let mut baseline_ctx = fixture_host(&module);
        let (bc, br, bcooked) =
            run_fixture(&module, &mut baseline_ctx, name, 0, &[], Tier::BaselineJit);
        let baseline = engine_trace(bc, br, bcooked, &module);
        assert_eq!(
            (head.clone(), items.clone()),
            baseline,
            "cf:{call}/0: the interpreter and the baseline tier cook differently"
        );
        assert_eq!(items.len(), 1, "cf:{call}/0: cooked extra items\n  {items:#?}");
        assert!(
            items[0].starts_with(&format!("cf|{call}|0|")),
            "cf:{call}/0: the innermost item must be the raising function, got {}",
            items[0]
        );
        let Some(oracle) = oracle_trace(&dir, &format!("cf:{call}()")) else {
            eprintln!("SKIP: no system `erl` on PATH -- the badarith oracle check is skipped");
            return;
        };
        assert_matches_the_oracle(&format!("cf:{call}/0"), &head, &items, &oracle);
    }
}

/// A legacy `catch` of a `throw` yields the bare value and of an `exit` the
/// `{'EXIT',Value}` tuple — **no trace at all** — so neither consumes the head
/// latch, and neither may pick one up. Their `try … catch C:R:S` twins do cook a
/// trace, with the raising function as its only item.
#[test]
fn a_legacy_fold_of_a_throw_or_an_exit_carries_no_trace() {
    check_rows(
        "no_trace",
        &[
            Expect {
                call: "legacy_throw",
                items: 0,
                first: "",
                location: &[],
            },
            Expect {
                call: "legacy_exit",
                items: 0,
                first: "",
                location: &[],
            },
            Expect {
                call: "modern_throw",
                items: 1,
                first: "cf|modern_throw|0",
                location: &["file", "line"],
            },
            Expect {
                call: "modern_exit",
                items: 1,
                first: "cf|modern_exit|0",
                location: &["file", "line"],
            },
        ],
    );
}
