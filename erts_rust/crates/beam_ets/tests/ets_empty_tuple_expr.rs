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

//! **A bare `{}` (0-arity tuple) as a match-spec guard/body expression.**
//!
//! A `{}` written directly as a guard or body expression is neither a function
//! call (`{Name, Arg…}`) nor the `{{ … }}` construct form, so the oracle rejects
//! the whole spec with `badarg` — `erl` even names the reason:
//! `"{} is neither a function call, nor a tuple (tuples are written {{ ... }})."`
//!
//! Because `expr::decode` recurses through tuples, lists and maps, a `{}` at **any**
//! depth reaches the same decoder (e.g. the body `{{{}}}`), and both the guard and
//! the body position go through it. Every entry point that compiles a raw spec is
//! therefore watchable: `ets:select/2,3`, `select_delete`, `select_count`,
//! `select_reverse`, `select_replace`, the `match*` family, `ets:match_spec_compile/1`,
//! `ets:match_spec_run/2` and `erlang:match_spec_test/3`.
//!
//! The `{{}}` construct form (a one-element tuple whose element is the empty tuple)
//! and `{const,{}}` must keep working: both produce the empty tuple `{}`.
//!
//! # How the two sides are tied together
//!
//! [`CASES`] is the single source of truth: each entry knows both the Erlang source
//! of its spec ([`Case::erlang`]) and how to build the identical term through this
//! crate's public API ([`Case::spec`]). The oracle module is generated from the
//! Erlang side, compiled with `erlc` and run under `erl` (both resolved by
//! `tests/common/oracle.rs`), and its `ets:select/2` output is compared byte for byte
//! with this crate's own `compile` + `Table::select` rendering.
//!
//! Three test groups:
//!
//! 1. [`the_bare_empty_tuple_is_a_typed_badarg_and_never_panics`] — the crate against
//!    the `erl`-derived expectations. Runs **always**, so the regression evidence keeps
//!    its meaning on a machine with no Erlang.
//! 2. [`the_live_oracle_reproduces_the_recorded_table`] — the live oracle against the
//!    same expectations, so a stale recording or an oracle upgrade fails.
//! 3. [`the_crate_agrees_with_the_live_oracle`] — the real cross-check: live `erl`
//!    output vs live `beam_ets` output, case by case.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_ets::prelude::*;
use beam_term::prelude::*;

// The differential suite's canonical renderer (`render`/`rendered`/`list_of`) lives
// in `tests/differential/`, which Cargo does not build as an integration-test target;
// the `#[path]` include is the same one `tests/differential.rs` uses. Only the
// renderer is used here, so the rest of the battery is `dead_code`.
#[path = "differential/battery/mod.rs"]
#[allow(dead_code)]
mod battery;

mod common;

use common::oracle;

// ---------------------------------------------------------------------------------------
// The cases (the single source of truth for both sides)
// ---------------------------------------------------------------------------------------

/// The Erlang module name of the generated oracle program.
const MODULE: &str = "beam_ets_empty_tuple_expr";

/// How long the oracle is given to compile and then to run. Generous: a loaded CI
/// box can be slow.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// One `ets:select/2` case: a spec that writes `{}` somewhere in its guard or body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Case {
    /// `[{'$1',[],[{}]}]` — a bare `{}` as the body expression.
    BodyEmptyTuple,
    /// `[{'$1',[{}],[ok]}]` — a bare `{}` as the guard expression.
    GuardEmptyTuple,
    /// `[{'$1',[],[{{{}}}]}]` — a bare `{}` nested under two construct forms.
    BodyNestedEmptyTuple,
    /// `[{'$1',[],[{{}}]}]` — the one-element construct form whose element is `{}`
    /// (constructs the empty tuple; must keep working).
    BodyOneElemConstruct,
    /// `[{'$1',[],[{const,{}}]}]` — `const` wrapping the empty tuple (must keep
    /// working).
    BodyConstEmptyTuple,
    /// `[{'$1',[],[{{a,b}}]}]` — a non-empty tuple construct (no regression).
    BodyTupleConstruct,
    /// `[{{'$1','$2'},[{'==',{'tuple_size','$2'},2}],['$1']}]` — a `tuple_size/1`
    /// guard over a tuple element (no regression).
    GuardTupleSize,
}

impl Case {
    /// The case name — used as the oracle's emitted key and as the [`EXPECTED`] key.
    fn name(self) -> &'static str {
        match self {
            Case::BodyEmptyTuple => "body_empty_tuple",
            Case::GuardEmptyTuple => "guard_empty_tuple",
            Case::BodyNestedEmptyTuple => "body_nested_empty_tuple",
            Case::BodyOneElemConstruct => "body_one_elem_construct",
            Case::BodyConstEmptyTuple => "body_const_empty_tuple",
            Case::BodyTupleConstruct => "body_tuple_construct",
            Case::GuardTupleSize => "guard_tuple_size",
        }
    }

    /// The Erlang **source** of the spec term (the oracle splices it verbatim into
    /// the generated program's `cases()` list).
    fn erlang(self) -> &'static str {
        match self {
            Case::BodyEmptyTuple => "[{'$1',[],[{}]}]",
            Case::GuardEmptyTuple => "[{'$1',[{}],[ok]}]",
            Case::BodyNestedEmptyTuple => "[{'$1',[],[{{{}}}]}]",
            Case::BodyOneElemConstruct => "[{'$1',[],[{{}}]}]",
            Case::BodyConstEmptyTuple => "[{'$1',[],[{const,{}}]}]",
            Case::BodyTupleConstruct => "[{'$1',[],[{{a,b}}]}]",
            Case::GuardTupleSize => "[{{'$1','$2'},[{'==',{'tuple_size','$2'},2}],['$1']}]",
        }
    }

    /// The live term of this case's spec, built through the crate's own term arena —
    /// the Rust mirror of [`Self::erlang`].
    fn spec(self, heap: &mut Heap, atoms: &AtomTable) -> Eterm {
        match self {
            Case::BodyEmptyTuple => {
                let body = empty_tuple(heap);
                one_clause(heap, at(atoms, "$1"), &[], &[body])
            }
            Case::GuardEmptyTuple => {
                let guard = empty_tuple(heap);
                one_clause(heap, at(atoms, "$1"), &[guard], &[at(atoms, "ok")])
            }
            Case::BodyNestedEmptyTuple => {
                let empty = empty_tuple(heap);
                // `{{}}` — a one-element tuple whose element is `{}`.
                let construct_empty = tup(heap, &[empty]);
                // `{{{}}}` — a one-element tuple whose element is `{{}}`.
                let nested = tup(heap, &[construct_empty]);
                one_clause(heap, at(atoms, "$1"), &[], &[nested])
            }
            Case::BodyOneElemConstruct => {
                let empty = empty_tuple(heap);
                let construct_empty = tup(heap, &[empty]);
                one_clause(heap, at(atoms, "$1"), &[], &[construct_empty])
            }
            Case::BodyConstEmptyTuple => {
                let empty = empty_tuple(heap);
                // `{const,{}}` — the `const` call whose argument is the empty tuple.
                let term = tup(heap, &[at(atoms, "const"), empty]);
                one_clause(heap, at(atoms, "$1"), &[], &[term])
            }
            Case::BodyTupleConstruct => {
                let pair = tup(heap, &[at(atoms, "a"), at(atoms, "b")]);
                // `{{a,b}}` — a one-element tuple whose element is `{a,b}`.
                let construct_pair = tup(heap, &[pair]);
                one_clause(heap, at(atoms, "$1"), &[], &[construct_pair])
            }
            Case::GuardTupleSize => {
                let head = tup(heap, &[at(atoms, "$1"), at(atoms, "$2")]);
                let size = call(heap, at(atoms, "tuple_size"), &[at(atoms, "$2")]);
                let guard = call(heap, at(atoms, "=="), &[size, make_small(2)]);
                one_clause(heap, head, &[guard], &[at(atoms, "$1")])
            }
        }
    }
}

/// Every case, in the order the oracle emits them.
const CASES: &[Case] = &[
    Case::BodyEmptyTuple,
    Case::GuardEmptyTuple,
    Case::BodyNestedEmptyTuple,
    Case::BodyOneElemConstruct,
    Case::BodyConstEmptyTuple,
    Case::BodyTupleConstruct,
    Case::GuardTupleSize,
];

/// The `erl`-derived reference rendering of every case (OTP 29.1.1 / erts-17.1). A
/// `badarg` is rendered as its bare reason atom, a successful `ets:select/2` as the
/// selected list — exactly what the oracle's `~w` prints (and what the differential
/// suite's canonical renderer produces for the crate side). Never written by hand:
/// [`the_live_oracle_reproduces_the_recorded_table`] re-derives it from the live
/// oracle, so a stale recording fails instead of passing vacuously.
const EXPECTED: &[(&str, &str)] = &[
    ("body_empty_tuple", "badarg"),
    ("guard_empty_tuple", "badarg"),
    ("body_nested_empty_tuple", "badarg"),
    ("body_one_elem_construct", "[{}]"),
    ("body_const_empty_tuple", "[{}]"),
    ("body_tuple_construct", "[{a,b}]"),
    ("guard_tuple_size", "[1]"),
];

/// The three cases the oracle rejects with `badarg` — a bare `{}` as a guard/body
/// expression, directly or nested.
const BADARG_CASES: &[Case] = &[
    Case::BodyEmptyTuple,
    Case::GuardEmptyTuple,
    Case::BodyNestedEmptyTuple,
];

// ---------------------------------------------------------------------------------------
// The generated oracle program
// ---------------------------------------------------------------------------------------

/// The fixed, case-independent part of the generated oracle program, with the `cases()`
/// entries spliced in by [`oracle_program`].
const ORACLE_BODY: &str = r#"
%% One `Name|Rendered` line per case: a fresh one-object `set` holding `{1,{a,b}}`
%% (arity 2, so both a `'$1'` head and a `{'$1','$2'}` head match, and `'$2'` is a
%% tuple for the `tuple_size` guard), then `ets:select/2` over the case's spec.
%% `~w` prints a bare `badarg` reason as the atom, and a selected list as-is.
run() ->
    T = ets:new(empty_tuple_expr, [set, public]),
    true = ets:insert(T, {1, {a, b}}),
    lists:foreach(fun({Name, Spec}) ->
        io:format("~s|", [Name]),
        io:format("~w~n", [cap(T, Spec)])
    end, cases()),
    ets:delete(T),
    ok.

cap(T, Spec) ->
    try ets:select(T, Spec) of
        Result -> Result
    catch
        error:Reason -> Reason
    end.

cases() ->
    [
"#;

/// The generated oracle program: the header, one `cases()` entry per [`CASES`] entry,
/// and the closing list.
fn oracle_program() -> String {
    let mut program = format!("-module({MODULE}).\n-export([run/0]).\n");
    program.push_str(ORACLE_BODY);
    // Erlang lists have no trailing comma, so the entries are joined.
    let entries: Vec<String> = CASES
        .iter()
        .map(|case| format!("     {{\"{}\", {}}}", case.name(), case.erlang()))
        .collect();
    program.push_str(&entries.join(",\n"));
    program.push('\n');
    program.push_str("    ].\n");
    program
}

/// The resolved oracle executables.
struct Oracle {
    erl: PathBuf,
    erlc: PathBuf,
}

/// Resolve `erl`/`erlc` from `$ERTS_ORACLE`, then `$PATH`; `None` means the caller
/// skipped (loudly — see [`oracle::skip`]).
fn oracle_or_skip(test: &str) -> Option<Oracle> {
    let Some(erl) = oracle::resolve_erl() else {
        oracle::skip(test, "no `erl` on $PATH (and no executable $ERTS_ORACLE)");
        return None;
    };
    let Some(erlc) = oracle::resolve_erlc() else {
        oracle::skip(test, "no `erlc` ($ERLC unset, none beside `erl`, none on $PATH)");
        return None;
    };
    Some(Oracle { erl, erlc })
}

/// Compile the generated oracle program into a fresh `$TMPDIR` scratch directory, run
/// it under the system `erl`, and parse its `Name|Rendered` lines.
///
/// A compile failure, a crash, a non-zero exit or a timeout is a **hard failure** —
/// a broken oracle must never be mistaken for a skip.
fn run_oracle(oracle: &Oracle) -> Vec<(String, String)> {
    let scratch = oracle::Scratch::new("empty_tuple_expr");
    oracle::compile_module(
        &oracle.erlc,
        &scratch,
        MODULE,
        &oracle_program(),
        ORACLE_TIMEOUT,
    );

    let mut run = Command::new(&oracle.erl);
    run.arg("-noshell")
        .arg("-pa")
        .arg(scratch.path())
        // `current_dir` confines a crash dump (`erl_crash.dump`) to the scratch dir.
        .current_dir(scratch.path())
        .arg("-eval")
        .arg(format!("{MODULE}:run()"))
        .arg("-s")
        .arg("init")
        .arg("stop");
    let output = oracle::run_bounded(&mut run, ORACLE_TIMEOUT).unwrap_or_else(|error| {
        panic!(
            "erl ({}) could not run the empty-tuple battery: {error}",
            oracle.erl.display()
        )
    });
    assert!(
        output.status.success(),
        "erl ({}) exited with {}:\n{}",
        oracle.erl.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    parse_emitted(&String::from_utf8_lossy(&output.stdout))
}

/// Parse the oracle's `Name|Rendered` lines.
fn parse_emitted(stdout: &str) -> Vec<(String, String)> {
    let mut cases = Vec::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let (name, rendered) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("the oracle emitted a malformed line: {line:?}"));
        cases.push((name.to_string(), rendered.to_string()));
    }
    cases
}

// ---------------------------------------------------------------------------------------
// The crate side: the same specs through the public API
// ---------------------------------------------------------------------------------------

/// The atoms the crate-side objects/specs reference.
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in ["$1", "$2", "a", "b", "ok", "const", "tuple_size", "==", "true", "false"] {
        atoms.intern(name);
    }
    atoms
}

/// The term of the interned atom `name`.
fn at(atoms: &AtomTable, name: &str) -> Eterm {
    make_atom(atoms.lookup(name).expect("interned by `atoms`"))
}

/// A tuple of `elements` on `heap`.
fn tup(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    heap.tuple(elements).expect("the battery heap has room")
}

/// The empty tuple `{}` on `heap` (a boxed arity-0 tuple).
fn empty_tuple(heap: &mut Heap) -> Eterm {
    tup(heap, &[])
}

/// A proper list of `elements` on `heap`.
fn list(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for element in elements.iter().rev() {
        acc = heap.cons(*element, acc).expect("the battery heap has room");
    }
    acc
}

/// A match-spec call term `{name, args…}` on `heap`.
fn call(heap: &mut Heap, name: Eterm, args: &[Eterm]) -> Eterm {
    let mut elements = Vec::with_capacity(args.len() + 1);
    elements.push(name);
    elements.extend_from_slice(args);
    tup(heap, &elements)
}

/// A one-clause spec `[{Head, Guards, Body}]` on `heap`.
fn one_clause(heap: &mut Heap, head: Eterm, guards: &[Eterm], body: &[Eterm]) -> Eterm {
    let guard_list = list(heap, guards);
    let body_list = list(heap, body);
    let clause = tup(heap, &[head, guard_list, body_list]);
    list(heap, &[clause])
}

/// A fresh one-object `set` holding `{1,{a,b}}`: arity 2, so both a `'$1'` head and a
/// `{'$1','$2'}` head match and `'$2'` is a tuple for the `tuple_size` guard.
fn one_object_table(atoms: &AtomTable) -> Table {
    let mut heap = Heap::new(1024).expect("the object heap is allocatable");
    let inner = tup(&mut heap, &[at(atoms, "a"), at(atoms, "b")]);
    let object = tup(&mut heap, &[make_small(1), inner]);
    let mut table = common::new_table(TableKind::Set);
    table
        .insert(object, atoms)
        .expect("a one-object `set` accepts the object");
    table
}

/// The rendered error class of `error` — `badarg` bare, anything else
/// `error:<reason>`, matching the oracle's caught-reason `~w`.
fn error_class(error: &BeamError) -> String {
    match error.as_atom() {
        Some("badarg") => "badarg".to_string(),
        Some(reason) => format!("error:{reason}"),
        None => "error:unknown".to_string(),
    }
}

/// Run one case through the crate: compile the spec, then `Table::select` and render
/// the result (or the error class) with the differential suite's canonical renderer.
fn run_case(case: Case, atoms: &AtomTable, heap: &mut Heap) -> String {
    let table = one_object_table(atoms);
    let spec_term = case.spec(heap, atoms);
    match compile(spec_term, atoms) {
        Err(error) => error_class(&error),
        Ok(spec) => match table.select(&spec, atoms, heap) {
            Ok(results) => battery::list_of(&battery::rendered(&results, atoms)),
            Err(error) => error_class(&error),
        },
    }
}

/// Run every [`CASES`] entry through the crate's public API. Returns
/// `(case_name, rendered_result)` in [`CASES`] order.
fn beam_ets_battery() -> Vec<(String, String)> {
    let atoms = atoms();
    let mut heap = Heap::new(1 << 12).expect("the battery heap is allocatable");
    CASES
        .iter()
        .map(|case| (case.name().to_string(), run_case(*case, &atoms, &mut heap)))
        .collect()
}

// ---------------------------------------------------------------------------------------
// Comparing
// ---------------------------------------------------------------------------------------

/// Index cases by name, asserting the names are unique (a duplicate would otherwise
/// silently shadow a case).
fn case_map(cases: &[(String, String)], what: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for (name, rendered) in cases {
        assert!(
            map.insert(name.clone(), rendered.clone()).is_none(),
            "{what} emitted the case `{name}` twice"
        );
    }
    map
}

/// The recorded reference table as a map.
fn expected_map() -> BTreeMap<String, String> {
    let cases: Vec<(String, String)> = EXPECTED
        .iter()
        .map(|(name, rendered)| (name.to_string(), rendered.to_string()))
        .collect();
    case_map(&cases, "the recorded empty-tuple table")
}

/// Assert two case maps agree, with a full per-case report on failure.
fn assert_agree(expected: &BTreeMap<String, String>, actual: &BTreeMap<String, String>, what: &str) {
    let mut report = String::new();
    for (name, wanted) in expected {
        match actual.get(name) {
            None => report.push_str(&format!("  {name}: MISSING (expected {wanted})\n")),
            Some(got) if got != wanted => {
                report.push_str(&format!("  {name}:\n    expected: {wanted}\n    actual:   {got}\n"));
            }
            Some(_) => {}
        }
    }
    for name in actual.keys() {
        if !expected.contains_key(name) {
            report.push_str(&format!("  {name}: UNEXPECTED\n"));
        }
    }
    assert!(report.is_empty(), "{what} differs from the reference:\n{report}");
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

/// **The regression.** A bare `{}` as a guard/body expression — directly or nested —
/// is the typed `badarg`, never a panic. This runs through every public entry point
/// that compiles a raw spec, and each case is first asserted to be the exact
/// `BeamError::BadArg` the neighbouring "unknown function" fallthrough produces.
#[test]
fn the_bare_empty_tuple_is_a_typed_badarg_and_never_panics() {
    let atoms = atoms();
    let mut heap = Heap::new(1 << 12).expect("the battery heap is allocatable");

    for case in BADARG_CASES {
        // `beam_ets::compile` (the path behind `ets:select/2,3`, `select_delete`,
        // `select_count`, `select_reverse`, `select_replace`, the `match*` family and
        // `ets:match_spec_compile/1`): a typed `badarg`, not an index-out-of-bounds.
        let spec_term = case.spec(&mut heap, &atoms);
        let error = compile(spec_term, &atoms)
            .err()
            .unwrap_or_else(|| panic!("{}: a bare `{{}}` must be refused", case.name()));
        assert!(
            matches!(error, BeamError::BadArg),
            "{}: the rejection must be the typed `BeamError::BadArg`, got {error:?}",
            case.name()
        );

        // The whole `Table::select` path renders the same typed `badarg` — never a
        // panic — through the public API.
        assert_eq!(
            run_case(*case, &atoms, &mut heap),
            "badarg",
            "{}: the select family reports the typed `badarg`",
            case.name()
        );

        // `ets:match_spec_run/2` (the table-independent runner) refuses it too.
        let inner = tup(&mut heap, &[at(&atoms, "a"), at(&atoms, "b")]);
        let object = tup(&mut heap, &[make_small(1), inner]);
        let input = list(&mut heap, &[object]);
        let error = match_spec_run(input, spec_term, &atoms, &mut heap)
            .err()
            .unwrap_or_else(|| panic!("{}: match_spec_run must refuse", case.name()));
        assert!(
            matches!(error, BeamError::BadArg),
            "{}: match_spec_run must be the typed `BeamError::BadArg`, got {error:?}",
            case.name()
        );
    }
}

/// The `{{}}` construct form and `{const,{}}` must keep building the empty tuple: they
/// do **not** reach the bare-tuple rejection.
#[test]
fn the_construct_form_and_const_still_build_an_empty_tuple() {
    let atoms = atoms();
    let mut heap = Heap::new(1 << 12).expect("the battery heap is allocatable");

    for case in [Case::BodyOneElemConstruct, Case::BodyConstEmptyTuple] {
        assert_eq!(
            run_case(case, &atoms, &mut heap),
            "[{}]",
            "{}: `{{}}`/`{{const,{{}}}}` constructs the empty tuple",
            case.name()
        );
    }
}

/// A non-empty tuple construct in a body and a `tuple_size/1` guard over a tuple
/// element both still work — the empty-tuple rejection must not perturb them.
#[test]
fn a_non_empty_tuple_body_and_a_tuple_size_guard_still_work() {
    let atoms = atoms();
    let mut heap = Heap::new(1 << 12).expect("the battery heap is allocatable");

    assert_eq!(run_case(Case::BodyTupleConstruct, &atoms, &mut heap), "[{a,b}]");
    assert_eq!(run_case(Case::GuardTupleSize, &atoms, &mut heap), "[1]");
}

/// This crate against the `erl`-derived table. Always runs, so the regression evidence
/// keeps its meaning on a machine with no Erlang.
#[test]
fn the_crate_reproduces_the_recorded_oracle_table() {
    let ours = case_map(&beam_ets_battery(), "the beam_ets empty-tuple battery");
    assert_agree(
        &expected_map(),
        &ours,
        "the beam_ets empty-tuple battery",
    );
}

/// The live oracle against the recorded table: catches an oracle upgrade (a changed
/// spec-validation semantic) or a stale expectation.
#[test]
fn the_live_oracle_reproduces_the_recorded_table() {
    let Some(oracle) = oracle_or_skip("the_live_oracle_reproduces_the_recorded_table") else {
        return;
    };
    let emitted = run_oracle(&oracle);
    eprintln!(
        "[empty-tuple] oracle: {} — {} cases emitted",
        oracle.erl.display(),
        emitted.len()
    );
    assert_eq!(
        emitted.len(),
        CASES.len(),
        "the oracle emitted {} cases but the table has {}",
        emitted.len(),
        CASES.len()
    );
    assert_agree(
        &expected_map(),
        &case_map(&emitted, "the oracle battery"),
        "the live oracle battery",
    );
}

/// **The cross-check**: the live oracle and the live `beam_ets` must render every case
/// identically — the three `{}` cases as `badarg`, the construct forms as `[{}]`.
#[test]
fn the_crate_agrees_with_the_live_oracle() {
    let Some(oracle) = oracle_or_skip("the_crate_agrees_with_the_live_oracle") else {
        return;
    };
    let oracle_cases = case_map(&run_oracle(&oracle), "the oracle battery");
    let rust_cases = case_map(&beam_ets_battery(), "the beam_ets empty-tuple battery");
    eprintln!(
        "[empty-tuple] oracle: {} — comparing {} cases against beam_ets",
        oracle.erl.display(),
        oracle_cases.len()
    );
    assert_agree(&oracle_cases, &rust_cases, "the beam_ets empty-tuple battery");
}
