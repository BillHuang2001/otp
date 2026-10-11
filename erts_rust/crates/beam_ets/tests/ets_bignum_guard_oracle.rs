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

//! **Live oracle cross-check for the bignum guard arithmetic.**
//!
//! The match-spec/guard evaluator materialises the oracle's **bignum** (rather than
//! its old typed `unsupported_ets` gap) for `abs/1` on a bignum operand or on
//! `MIN_SMALL`, for `bnot/1` on a bignum, and for `trunc/1`/`round/1`/`ceil/1`/
//! `floor/1` of a float whose integral value leaves the 60-bit small range. Those
//! values were pinned only as *literals* — the `src/select/expr/guard.rs` unit tests
//! and `tests/select.rs` compare them against constants written from a probe. This
//! suite is the end-to-end evidence: the **same** `ets:select/2` workload runs under
//! this crate and under the live system `erl`, and the two printed results are
//! compared byte for byte.
//!
//! # How the two sides are tied together
//!
//! [`CASES`] is the single source of truth: one `(name, operand, guard BIF)` triple
//! per case. [`oracle_program`] **generates** the Erlang module from it (a fresh
//! one-object `set`, the object inserted, then `ets:select/2` over the one-clause
//! spec `[{{'$1','$2'}, [], [{Bif,'$2'}]}]`, printed with `~w`), and
//! [`beam_ets_battery`] builds the identical operand term and spec through this
//! crate's public API (`beam_ets::compile` + `Table::select`) and renders the result
//! with the differential suite's canonical renderer (`battery::render`, the renderer
//! `tests/differential.rs` uses — it renders a bignum in full decimal). Because the
//! operand expression and the BIF come from the *same* table, the only thing that can
//! differ between the two sides is the computed result — which is exactly what is
//! compared.
//!
//! The oracle module is written to a scratch directory under `$TMPDIR`, compiled with
//! `erlc` and run under `erl` (both resolved by `tests/common/oracle.rs` — `erl` from
//! `$ERTS_ORACLE` then `$PATH`, `erlc` from `$ERLC` then beside `erl` then `$PATH`),
//! polled under a wall-clock bound and removed afterwards. A missing `erl`/`erlc` is a
//! **loud** failure, not a silent pass: the oracle is required by default, and the only
//! way out is `BEAM_ETS_ALLOW_NO_ORACLE=1` (see that module's docs).
//!
//! # What is asserted
//!
//! 1. [`the_crate_reproduces_the_recorded_oracle_table`] — this crate against the
//!    `erl`-derived reference table. Runs **always**, so the evidence keeps its
//!    meaning on a machine with no Erlang.
//! 2. [`the_live_oracle_reproduces_the_recorded_table`] — the live oracle against the
//!    same table, so an oracle upgrade or a **stale recording** cannot pass.
//! 3. [`the_crate_agrees_with_the_live_oracle`] — the real cross-check: live `erl`
//!    output vs live `beam_ets` output, case by case.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use beam_alloc::Heap;
use beam_ets::{compile, TableKind};
use beam_term::prelude::*;

// The differential suite's canonical renderer (`render`/`rendered`/`list_of`) lives
// in `tests/differential/`, which Cargo does not build as an integration-test target;
// the `#[path]` include is the same one `tests/differential.rs` uses (see its module
// docs). Only the renderer is used here, so the rest of the battery is `dead_code`.
#[path = "differential/battery/mod.rs"]
#[allow(dead_code)]
mod battery;

mod common;

use common::oracle;

// ---------------------------------------------------------------------------------------
// The cases (the single source of truth for both sides)
// ---------------------------------------------------------------------------------------

/// The Erlang module name of the generated oracle program.
const MODULE: &str = "beam_ets_bignum_guard";

/// How long the oracle is given to compile and then to run. Generous: a loaded CI box
/// can be slow.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The numeric guard BIF whose bignum result each case exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bif {
    Abs,
    Bnot,
    Trunc,
    Round,
    Ceil,
    Floor,
}

impl Bif {
    /// The atom naming the guard BIF — used both as the match-spec call name and as
    /// the atom the crate's `AtomTable` must have interned.
    fn name(self) -> &'static str {
        match self {
            Bif::Abs => "abs",
            Bif::Bnot => "bnot",
            Bif::Trunc => "trunc",
            Bif::Round => "round",
            Bif::Ceil => "ceil",
            Bif::Floor => "floor",
        }
    }
}

/// The operand the object's second element carries.
///
/// Each variant knows both the Erlang source it spells (`erlang`) and the live term
/// this crate builds (`term`), so the two sides cannot drift apart.
#[derive(Debug, Clone, Copy)]
enum Operand {
    /// The bignum `2^exp`.
    Pow2(u32),
    /// The bignum `-(2^exp)`.
    NegPow2(u32),
    /// The small integer `MIN_SMALL` (`-(2^59)`); its `abs/1` result is a bignum.
    MinSmall,
    /// The bignum `2^64 - 1` — a limb-boundary operand for `bnot/1`.
    Mask64,
    /// The float literal `value`.
    Float(f64),
}

impl Operand {
    /// The Erlang expression for this operand.
    fn erlang(self) -> String {
        match self {
            Operand::Pow2(exp) => format!("1 bsl {exp}"),
            Operand::NegPow2(exp) => format!("-(1 bsl {exp})"),
            Operand::MinSmall => MIN_SMALL.to_string(),
            Operand::Mask64 => "(1 bsl 64) - 1".to_string(),
            // Erlang has no `1e30`: its float literals need a decimal point, so the
            // exponent form is spelled with one mantissa digit (`1.0e30`).
            Operand::Float(value) => format!("{value:.1e}"),
        }
    }

    /// The live operand term on `heap` — the Rust mirror of [`Self::erlang`].
    fn term(self, heap: &mut Heap) -> Eterm {
        match self {
            Operand::Pow2(exp) => bignum(heap, false, &pow2(exp)),
            Operand::NegPow2(exp) => bignum(heap, true, &pow2(exp)),
            Operand::MinSmall => make_small(MIN_SMALL),
            Operand::Mask64 => bignum(heap, false, &[u64::MAX]),
            Operand::Float(value) => float(heap, value),
        }
    }
}

/// Every case: the case name, the operand (as the object's second element) and the
/// guard BIF applied to it.
///
/// `bnot(1 bsl 70)` is `-(2^70 + 1)` and `bnot(2^64 - 1)` is `-(2^64)` (`bnot/1` on a
/// non-negative bignum adds one — growing a limb at `2^64 - 1` — and negates), while
/// `abs/1` drops the sign and `trunc`/`round`/`ceil`/`floor` of a `1.0e30`-magnitude
/// float produce the (inexact) integral bignum the oracle produces.
const CASES: &[(&str, Operand, Bif)] = &[
    ("bnot_2pow70", Operand::Pow2(70), Bif::Bnot),
    ("bnot_neg_2pow70", Operand::NegPow2(70), Bif::Bnot),
    ("abs_2pow70", Operand::Pow2(70), Bif::Abs),
    ("abs_neg_2pow70", Operand::NegPow2(70), Bif::Abs),
    ("abs_min_small", Operand::MinSmall, Bif::Abs),
    ("bnot_2pow64_minus_1", Operand::Mask64, Bif::Bnot),
    ("trunc_1e30", Operand::Float(1.0e30), Bif::Trunc),
    ("round_1e30", Operand::Float(1.0e30), Bif::Round),
    ("ceil_1e30", Operand::Float(1.0e30), Bif::Ceil),
    ("floor_1e30", Operand::Float(1.0e30), Bif::Floor),
    ("trunc_neg_1e30", Operand::Float(-1.0e30), Bif::Trunc),
    ("round_neg_1e30", Operand::Float(-1.0e30), Bif::Round),
    ("ceil_neg_1e30", Operand::Float(-1.0e30), Bif::Ceil),
    ("floor_neg_1e30", Operand::Float(-1.0e30), Bif::Floor),
];

/// The `erl`-derived reference rendering of every case, captured by running the
/// generated program under the system `erl` (OTP 29.1.1 / erts-17.1) — never written
/// by hand. [`the_live_oracle_reproduces_the_recorded_table`] re-derives it from the
/// live oracle, so a stale recording fails instead of passing vacuously.
const EXPECTED: &[(&str, &str)] = &[
    ("bnot_2pow70", "[-1180591620717411303425]"),
    ("bnot_neg_2pow70", "[1180591620717411303423]"),
    ("abs_2pow70", "[1180591620717411303424]"),
    ("abs_neg_2pow70", "[1180591620717411303424]"),
    ("abs_min_small", "[576460752303423488]"),
    ("bnot_2pow64_minus_1", "[-18446744073709551616]"),
    ("trunc_1e30", "[1000000000000000019884624838656]"),
    ("round_1e30", "[1000000000000000019884624838656]"),
    ("ceil_1e30", "[1000000000000000019884624838656]"),
    ("floor_1e30", "[1000000000000000019884624838656]"),
    ("trunc_neg_1e30", "[-1000000000000000019884624838656]"),
    ("round_neg_1e30", "[-1000000000000000019884624838656]"),
    ("ceil_neg_1e30", "[-1000000000000000019884624838656]"),
    ("floor_neg_1e30", "[-1000000000000000019884624838656]"),
];

// ---------------------------------------------------------------------------------------
// The oracle: the generated program, the compile + run, the parsed lines
// ---------------------------------------------------------------------------------------

/// The fixed, case-independent part of the generated oracle program, with the case
/// list spliced in by [`oracle_program`] (so the module name is the only formatted
/// part of the header and the Erlang braces in the body need no escaping).
const ORACLE_BODY: &str = r#"
%% One `Name|Rendered` line per case: a fresh one-object `set`, the object inserted,
%% then `ets:select/2` over the one-clause spec whose body computes the guard BIF over
%% the object's value. `~w` prints the bignum result in full decimal.
run() ->
    lists:foreach(fun({Name, Object, Spec}) ->
        T = ets:new(bignum, [set, public]),
        true = ets:insert(T, Object),
        io:format("~s|", [Name]),
        io:format("~w~n", [ets:select(T, [Spec])]),
        ets:delete(T)
    end, cases()),
    ok.

spec(Fun) -> {{'$1','$2'}, [], [{Fun, '$2'}]}.

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
        .map(|(name, operand, bif)| {
            format!(
                "     {{\"{name}\", {{1, {}}}, spec('{}')}}",
                operand.erlang(),
                bif.name()
            )
        })
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
    let scratch = oracle::Scratch::new("bignum_guard");
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
            "erl ({}) could not run the bignum battery: {error}",
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
// The crate side: the same object + spec through the public API
// ---------------------------------------------------------------------------------------

/// The atoms the crate-side objects/specs reference. `true`/`false` are interned too:
/// a guard's truth and a boolean *body* render through them (the spec here has an
/// empty guard list and bignum bodies, but a stray boolean must not masquerade as a
/// capability gap).
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in [
        "$1", "$2", "abs", "bnot", "trunc", "round", "ceil", "floor", "true", "false",
    ] {
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

/// A float term on `heap` (the two words are `[HEADER_FLONUM, bits]`).
fn float(heap: &mut Heap, value: f64) -> Eterm {
    // SAFETY: `alloc_words(2)` hands back the two exclusively-owned, writable words a
    // float occupies; `write_float` writes both before the term escapes, and the heap
    // outlives it (it is never reset here).
    unsafe {
        let words = heap.alloc_words(2).expect("the battery heap has room for a float");
        write_float(words, value)
    }
}

/// A bignum term on `heap` (least-significant-first `u64` digits).
fn bignum(heap: &mut Heap, negative: bool, digits: &[u64]) -> Eterm {
    // SAFETY: `alloc_words(big_size_words(digits.len()))` hands back exactly those
    // exclusively-owned, writable words, every one of which `write_big` fills; the
    // heap is a never-relocating arena that outlives the term.
    unsafe {
        let hp = heap
            .alloc_words(big_size_words(digits.len()))
            .expect("the battery heap has room for a bignum");
        write_big(hp, negative, digits)
    }
}

/// The little-endian `u64` limbs of `2^exp` — the magnitude `1 bsl exp` produces.
fn pow2(exp: u32) -> Vec<u64> {
    let mut digits = vec![0u64; (exp / 64 + 1) as usize];
    digits[(exp / 64) as usize] = 1u64 << (exp % 64);
    digits
}

/// The one-clause spec `[{{'$1','$2'}, [], [{Bif,'$2'}]}]` on `heap` — the object's
/// second element is the guard operand, and the body value is the computed result.
fn spec_for(heap: &mut Heap, atoms: &AtomTable, bif: Bif) -> Eterm {
    let head = tup(heap, &[at(atoms, "$1"), at(atoms, "$2")]);
    let body = call(heap, at(atoms, bif.name()), &[at(atoms, "$2")]);
    let guards = NIL;
    let body_list = list(heap, &[body]);
    let clause = tup(heap, &[head, guards, body_list]);
    list(heap, &[clause])
}

/// Run every [`CASES`] entry through the crate: build the `{1, Operand}` object, insert
/// it into a fresh `set`, compile the generated spec and `select`, then render the
/// result with the differential suite's canonical renderer.
///
/// Returns `(case_name, rendered_result)` in [`CASES`] order.
fn beam_ets_battery() -> Vec<(String, String)> {
    let atoms = atoms();
    let mut heap = Heap::new(1 << 16).expect("the battery heap is allocatable");
    let mut cases = Vec::with_capacity(CASES.len());
    for (name, operand, bif) in CASES {
        let mut table = common::new_table(TableKind::Set);
        let value = operand.term(&mut heap);
        let object = tup(&mut heap, &[make_small(1), value]);
        table
            .insert(object, &atoms)
            .unwrap_or_else(|error| panic!("{name}: a one-object `set` accepts the object: {error:?}"));

        let spec = compile(spec_for(&mut heap, &atoms, *bif), &atoms)
            .unwrap_or_else(|error| panic!("{name}: the one-clause spec compiles: {error:?}"));
        let results = table
            .select(&spec, &atoms, &mut heap)
            .unwrap_or_else(|error| panic!("{name}: the select runs: {error:?}"));
        cases.push((
            (*name).to_string(),
            battery::list_of(&battery::rendered(&results, &atoms)),
        ));
    }
    cases
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
    case_map(&cases, "the recorded bignum table")
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
    assert!(
        report.is_empty(),
        "{what} differs from the reference:\n{report}"
    );
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

/// This crate against the `erl`-derived table. Always runs, so the bignum evidence
/// keeps its meaning on a machine with no Erlang.
#[test]
fn the_crate_reproduces_the_recorded_oracle_table() {
    let ours = case_map(&beam_ets_battery(), "the beam_ets bignum battery");
    assert_agree(&expected_map(), &ours, "the beam_ets bignum battery");
}

/// The live oracle against the recorded table: catches an oracle upgrade (a changed
/// guard semantic) or a stale expectation.
#[test]
fn the_live_oracle_reproduces_the_recorded_table() {
    let Some(oracle) = oracle_or_skip("the_live_oracle_reproduces_the_recorded_table") else {
        return;
    };
    let emitted = run_oracle(&oracle);
    eprintln!(
        "[bignum] oracle: {} — {} cases emitted",
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
/// identically — in particular the bignum results are compared as full decimal digits,
/// not merely as "some integer".
#[test]
fn the_crate_agrees_with_the_live_oracle() {
    let Some(oracle) = oracle_or_skip("the_crate_agrees_with_the_live_oracle") else {
        return;
    };
    let oracle_cases = case_map(&run_oracle(&oracle), "the oracle battery");
    let rust_cases = case_map(&beam_ets_battery(), "the beam_ets bignum battery");
    eprintln!(
        "[bignum] oracle: {} — comparing {} cases against beam_ets",
        oracle.erl.display(),
        oracle_cases.len()
    );
    assert_agree(&oracle_cases, &rust_cases, "the beam_ets bignum battery");
}
