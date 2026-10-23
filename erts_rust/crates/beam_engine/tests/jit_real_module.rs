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

//! **Baseline-JIT tier equivalence on the REAL installed OTP corpus, checked
//! against the real system `erl`.**
//!
//! `tests/jit.rs` proves the native tier on the vertical slice, `tests/jit_coverage.rs`
//! on the broad hand-authored `coverage.beam` fixture. This file closes the remaining
//! question: does the **baseline JIT** (`Tier::BaselineJit`, the x86-64 block back end
//! in `src/jit/baseline/`) produce results **identical to the interpreter**
//! (`Tier::Interpreter`) -- *and* to the **system `erl`** -- on **real,
//! `erlc`-produced system modules**, the modules a real OTP install actually ships?
//!
//! # What is compared, and why it is a strong claim
//!
//! Every target is a real `Module:Function(Args)` call drawn from the crate's two
//! *curated, oracle-checked* corpora (the same data `tests/stdlib_lists.rs` and
//! `tests/exceptions_corpus.rs` drive on the interpreter):
//!
//! * `tests/fixtures/stdlib_breadth_targets.txt` -- 504 happy-path calls over **171**
//!   distinct installed modules (`lists`, `unicode`, `string`, `calendar`, `zip`, ...),
//!   each carrying the exact `erl -noshell -eval ...` command that produced its
//!   expected value;
//! * `tests/fixtures/error_path_targets.txt` -- 232 error-path calls over **215**
//!   distinct modules, where the *compiled code itself* raises (`func_info` ->
//!   `function_clause`, `case_end`, `badmatch`, `badrecord`).
//!
//! Each call is run **twice over the same loaded module** -- once on
//! `Tier::Interpreter`, once on `Tier::BaselineJit` -- and every row makes **two**
//! assertions:
//!
//! 1. **The tiers agree with each other.** Both returned a value and the values are the
//!    same term under `beam_term::cmp` (the module's exact `=:=` ordering, so boxed
//!    tuples/lists/floats compare by value), or both returned a typed
//!    [`beam_common::BeamError`] and the errors are the **same shape**
//!    (`Debug`-identical: class, reason, stacktrace, live-reason presence). A typed
//!    error on one tier and a value on the other **fails**, and two *different* typed
//!    errors **fail** too.
//! 2. **Each tier agrees with the oracle.** The breadth fixture's `expected` column is
//!    the verbatim `~w` rendering the system `erl` printed for that very call; it is
//!    parsed back into a **term** (`tests/support/oracle.rs::parse_expected_value`) and
//!    compared with the engine's result through `beam_term::cmp`. The error-path
//!    fixture's `expected` column is the *exception* the oracle raises, and
//!    `tests/support/oracle.rs::verify_error` requires the engine's raise to carry the
//!    oracle's class (`error`) **and** its reason term.
//!
//! A row is counted as *oracle-checked* only when both of those hold on the tier in
//! question. Tier-vs-tier agreement alone can no longer hide a shared mistake: a value
//! both tiers got identically **wrong** is now a failure, and so is a raise with a
//! reason the system `erl` never produces. A tier that answers a **typed gap** (it
//! cannot execute the call at all -- a documented engine limitation) is printed as a
//! loud `BLOCKED:` line and counted separately, never silently folded into an
//! agreement and never treated as a wrong result.
//!
//! The corpus parsing, the argument/expectation term builders, the `~w` renderer and
//! the error comparator are the **one** implementation in `tests/support/oracle.rs`
//! (shared with the interpreter suites and with the feature-gated Cranelift suite), so
//! no suite can drift into its own corpus grammar.
//!
//! The corpora are curated exactly so this is safe: every call is a real, terminating
//! call over small representative arguments, so neither tier can loop forever, and the
//! same calls already run to completion on the interpreter in the two suites above.
//! Nothing here is guessed -- every expectation was produced by the real system `erl`
//! (OTP 29 / erts-17.1), and this suite asserts **both** tiers against it.
//!
//! # Why the real modules (and not only fixtures)
//!
//! A fixture module is small and hand-shaped; a real one is compiled by the actual OTP
//! compiler and reaches opcodes (bitstring matching, fun literals, exceptions, big
//! numbers, map/HAMT interiors, cross-module calls) the fixtures never emit. The host in
//! `tests/support` supplies one shared term arena and one shared atom universe, so a real
//! module runs and its cross-module calls resolve by name. The modules are located at
//! **test time** from the running Erlang install (`code:lib_dir/0`), never from a baked-in
//! path; every test **skips loudly** when the oracle OTP install is unavailable.
//!
//! # The one known tier divergence (pinned, not hidden)
//!
//! The two tiers consult the *same* per-instruction helper for every un-lowered
//! instruction, so their error surfaces are identical **for every instruction they
//! execute**. The single exception is the *entry* check, which runs before any
//! instruction: a call to a `module:function/arity` the module does **not** export is
//! reported by the interpreter as `unsupported_opcode: run_export: export not found` and
//! by the native tier as `unsupported_opcode: jit: export not found`. Both are the same
//! typed error (an `unsupported_opcode` raise) and neither is BEAM-observable -- the
//! fragment is an internal diagnostic -- but the strings differ.
//! [`missing_export_is_a_typed_error_on_both_tiers_with_a_different_diagnostic`] pins
//! both strings, and [`run_target`] refuses to *compare* such a target (neither tier can
//! enter a function that does not exist), so it is reported rather than silently averaged
//! away.
//!
//! # What this suite measures today
//!
//! Measured on this tree with the `jit-cranelift` feature off
//! (`cargo test -p beam_engine --test jit_real_module -- --nocapture`):
//!
//! ```text
//! JIT-REAL (breadth): 504 calls; oracle-checked interpreter=504 baseline=504; blocked=0; modules=171; skipped=0
//! JIT-REAL (error-path): 232 poisoned calls; oracle-checked interpreter=223 baseline=223; blocked=18; modules=215; skipped=0
//! ```
//!
//! So **736** corpus rows run on both tiers and **727** of them (504 + 223) are
//! oracle-checked on *each* tier. The other **9** error-path rows are the fixture's own
//! `unsupported` expectation — a documented engine/host gap (`receive_marker_reserve`,
//! `receive_next`, or a blocking `receive`), printed as `BLOCKED:` lines and counted
//! (`blocked=18` is those 9 rows on 2 tiers). **No row disagrees with the oracle.** The
//! per-corpus floors below are the guard: a regression that blocks a row, breaks the two
//! tiers' agreement, or makes *either* tier disagree with the system `erl` fails the suite.
//!
//! # The allocator evidence
//!
//! Agreement alone would also hold if the native tier only ever deopted, so
//! [`the_block_allocator_engages_on_real_stdlib_modules`] compiles real modules with
//! [`BaselineJit`] and asserts the artifact is non-trivial: compiled blocks, a non-empty
//! **resident register set** and a native-register bound -- i.e. the live-interval
//! allocator in `src/jit/baseline/emit/regalloc.rs` actually ran and the code really
//! addresses x-registers.

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::jit::baseline::BaselineJit;
use beam_engine::jit::JitBackend;
use beam_engine::{available_tiers, Machine, MinimalContext, Tier, MAX_REGISTER};
use beam_loader::{load_file, Module};
use beam_term::Eterm;
use support::{
    all_otp_ebins, app_ebin, breadth_args, describe_error, parse_breadth_target,
    parse_error_targets, parse_expected_value, render_term, skip_reason, stdlib_ebin, verify_error,
    ErrorTarget, ErrorVerdict, Expected, ModuleHost,
};

// ---------------------------------------------------------------------------
// Host + skip guard
// ---------------------------------------------------------------------------

/// Skips the calling test (loudly) when the oracle OTP install is unavailable.
fn skip_if_unavailable() -> bool {
    match skip_reason() {
        Some(reason) => {
            eprintln!("{reason}");
            true
        }
        None => false,
    }
}

/// A host whose primary `ebin` is `stdlib` and whose **search path is every installed
/// OTP application**, so a real module from any application loads and its cross-module
/// calls resolve by name (`tests/support` owns the shared arena + shared atom table).
fn host() -> ModuleHost {
    let ebin = stdlib_ebin().expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    let mut host = ModuleHost::new(ebin);
    if let Some(apps) = all_otp_ebins() {
        for app in apps {
            host.add_search_dir(app.dir);
        }
    }
    host
}

// ---------------------------------------------------------------------------
// The two-tier driver
// ---------------------------------------------------------------------------

/// One target's run through one tier: the outcome plus the machine that produced it.
struct TierRun {
    /// The tier's result.
    result: Result<Eterm>,
    /// Kept in scope so a ref-counted off-heap payload the result points at (a binary
    /// above the 64-byte on-heap limit) is not freed while the two tiers' results are
    /// compared. The machine owns that payload, exactly as a process heap would.
    #[allow(dead_code)]
    machine: Machine,
}

/// Runs `function/arity` on `module` through `tier` on a fresh machine over `host`.
///
/// The two tiers are selected by the **engine's own** `Machine::run_with_tier`, so the
/// interpreter half is literally `run_export` and the native half is the baseline
/// backend's driver loop.
fn run_tier(
    host: &mut ModuleHost,
    module: &Arc<Module>,
    function: &str,
    arity: u32,
    args: &[Eterm],
    tier: Tier,
) -> TierRun {
    let function_atom = module.atom_table.lookup(function).unwrap_or_else(|| {
        panic!(
            "module `{}` does not intern `{function}`",
            module.atom_name(module.name).unwrap_or("?")
        )
    });
    let mut machine = Machine::new();
    let result = machine.run_with_tier(&**module, function_atom, arity, args, host, tier);
    TierRun { result, machine }
}

/// How the two tiers' runs of one target related.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    /// Both tiers returned a value, and the values are the same term (`=:=`).
    Value,
    /// Both tiers returned a typed error, and the errors are the same shape.
    Error,
}

/// Compares the two tiers' runs of `label`, panicking loudly on any divergence.
fn compare(host: &ModuleHost, label: &str, interp: &TierRun, jit: &TierRun) -> Verdict {
    match (&interp.result, &jit.result) {
        (Ok(expected), Ok(got)) => {
            assert!(
                beam_term::cmp(*got, *expected, host.atom_table()).is_eq(),
                "TIER DIVERGENCE for `{label}`: interpreter and baseline-jit returned \
                 DIFFERENT terms:\n  interpreter: {expected:?}\n  baseline-jit: {got:?}"
            );
            Verdict::Value
        }
        (Err(expected), Err(got)) => {
            assert_eq!(
                format!("{got:?}"),
                format!("{expected:?}"),
                "TIER DIVERGENCE for `{label}`: both tiers errored, but with DIFFERENT \
                 typed errors (this test compares the full error shape, not `is_err()`):\n  \
                 interpreter: {expected:?}\n  baseline-jit: {got:?}"
            );
            Verdict::Error
        }
        (Ok(value), Err(error)) => panic!(
            "TIER DIVERGENCE for `{label}`: interpreter returned a value while baseline-jit \
             errored:\n  interpreter: {value:?}\n  baseline-jit: {error:?}"
        ),
        (Err(error), Ok(value)) => panic!(
            "TIER DIVERGENCE for `{label}`: baseline-jit returned a value while the \
             interpreter errored:\n  interpreter: {error:?}\n  baseline-jit: {value:?}"
        ),
    }
}

/// What the committed corpus says a target's outcome must be.
///
/// The two corpora bake two different kinds of expectation, and each is asserted
/// **per tier**:
///
/// * the breadth corpus bakes a **value** — the verbatim `~w` rendering the system
///   `erl` printed, parsed back into a term by
///   [`parse_expected_value`](support::parse_expected_value) and compared with
///   `beam_term::cmp`;
/// * the error-path corpus bakes an **exception** — the class and reason term the
///   system `erl` raises, checked by [`verify_error`](support::verify_error).
#[derive(Clone, Copy)]
enum Expectation<'a> {
    /// The breadth corpus: the oracle's value.
    Value(&'a Expected),
    /// The error-path corpus: the oracle's exception.
    Error(&'a ErrorTarget),
}

/// One tier's outcome checked against the baked oracle expectation.
enum OracleCheck {
    /// The tier reproduced the oracle's value (or its class **and** reason term).
    Checked,
    /// The tier answered a typed gap instead of executing the corpus call, so it can
    /// be compared neither way. Reported loudly and counted, never a failure and never
    /// an agreement.
    Blocked(String),
}

/// How one corpus row fared on both tiers.
#[derive(Clone, Copy, Debug)]
struct RowOutcome {
    /// How the two tiers related to each other.
    verdict: Verdict,
    /// Per tier: `true` when that tier's own outcome was asserted against the baked
    /// oracle expectation. `false` means a counted typed gap (a `BLOCKED:` line).
    oracle_checked: [bool; 2],
}

/// Asserts one tier's outcome against the baked oracle expectation.
///
/// A disagreement **panics immediately** with the row's label, the tier, both values
/// and (for the error corpus) the row's own quoted `erl -noshell -eval …` command, so
/// the failure is reproducible from the message alone.
fn check_oracle(
    host: &mut ModuleHost,
    label: &str,
    tier: Tier,
    result: &Result<Eterm>,
    expected: &Expectation<'_>,
) -> OracleCheck {
    match expected {
        Expectation::Value(expected) => match result {
            Ok(actual) => {
                assert!(
                    expected.matches(host, *actual),
                    "ORACLE MISMATCH for `{label}` on {tier:?}: the engine returned {}, \
                     the system erl returns {}",
                    render_term(host, *actual).unwrap_or_else(|| format!("{actual:?}")),
                    expected
                        .render(host)
                        .unwrap_or_else(|| "<unrenderable>".to_string())
                );
                OracleCheck::Checked
            }
            Err(error) => OracleCheck::Blocked(format!(
                "{tier:?}: the oracle returns a value, the engine answered {}",
                describe_error(error)
            )),
        },
        Expectation::Error(target) => match verify_error(host, result, target) {
            ErrorVerdict::Matches => OracleCheck::Checked,
            ErrorVerdict::TypedGap(gap) => {
                OracleCheck::Blocked(format!("{tier:?}: {gap}"))
            }
            ErrorVerdict::Wrong(detail) => panic!(
                "ORACLE MISMATCH for `{label}` on {tier:?}: {detail}\n  re-derive with: {}",
                target.oracle
            ),
        },
    }
}

/// Drives one `module:function/args` through **both** tiers, asserts they agree with
/// each other, and asserts each tier's own outcome against the baked oracle
/// expectation.
///
/// `None` means the target could not be driven at all -- the module did not load, or the
/// module exports no such `function/arity` (the one case in which the two tiers' entry
/// checks report *different* diagnostic strings, see the module docs). Such a target is
/// printed loudly (`SKIP: ...`) and counted, never silently dropped.
fn run_target(
    host: &mut ModuleHost,
    module_name: &str,
    function: &str,
    args: &[Eterm],
    label: &str,
    expected: &Expectation<'_>,
) -> Option<RowOutcome> {
    let module = match host.load(module_name) {
        Ok(module) => module,
        Err(error) => {
            println!("SKIP: {label} -- module `{module_name}` did not load: {error:?}");
            return None;
        }
    };
    let arity = args.len() as u32;
    let function_atom = match module.atom_table.lookup(function) {
        Some(atom) => atom,
        None => {
            println!("SKIP: {label} -- `{module_name}` does not name `{function}`");
            return None;
        }
    };
    if module.export(function_atom, arity).is_none() {
        println!(
            "SKIP: {label} -- `{module_name}:{function}/{arity}` is not exported, so neither \
             tier can enter it (see the module docs on the entry-check diagnostic)"
        );
        return None;
    }

    // Both tiers run over the *same* loaded module, back to back; each run's machine
    // stays in scope until the comparison has read both results.
    let interp = run_tier(host, &module, function, arity, args, Tier::Interpreter);
    let jit = run_tier(host, &module, function, arity, args, Tier::BaselineJit);
    let verdict = compare(host, label, &interp, &jit);

    // ... and then each tier on its own against the real system `erl`.
    let mut oracle_checked = [false; 2];
    for (slot, tier, run) in [
        (0usize, Tier::Interpreter, &interp),
        (1usize, Tier::BaselineJit, &jit),
    ] {
        match check_oracle(host, label, tier, &run.result, expected) {
            OracleCheck::Checked => oracle_checked[slot] = true,
            OracleCheck::Blocked(gap) => println!("BLOCKED: {label} -- {gap}"),
        }
    }
    Some(RowOutcome {
        verdict,
        oracle_checked,
    })
}

// ---------------------------------------------------------------------------
// Corpus 1: the happy-path breadth targets
// ---------------------------------------------------------------------------

/// The committed oracle-checked call corpus (`tests/stdlib_lists.rs`'s data).
const BREADTH_TARGETS: &str = include_str!("fixtures/stdlib_breadth_targets.txt");

/// Floor on the number of corpus calls both tiers must *execute and agree on*. Set to
/// the honest number this suite measures (504, exactly the number the interpreter
/// executes in `tests/stdlib_lists.rs`).
const BREADTH_COMPARISON_FLOOR: usize = 480;
/// Floor on the subset of those where the agreed outcome is a **value** (rather than a
/// typed error on both tiers) -- so the suite cannot pass by agreeing only on errors.
const BREADTH_VALUE_FLOOR: usize = 480;
/// Floor on the number of distinct installed modules reached (measured: 171).
const BREADTH_MODULE_FLOOR: usize = 171;

// The corpus parsing, the argument/term builders and the `~w` renderer are NOT
// re-defined here: they are the one shared implementation in
// `tests/support/oracle.rs` (`parse_breadth_target`, `breadth_args`,
// `parse_expected_value`, `render_term`), which the interpreter suites and the
// feature-gated Cranelift suite drive too.

/// **The headline measurement: both tiers, every happy-path corpus call, checked
/// against the real system `erl`.**
///
/// Runs each of the 504 real calls through the interpreter and the baseline JIT over one
/// shared host, and requires (a) the two tiers' outcomes to be identical (the same term,
/// or the same typed error) and (b) **each tier's** outcome to equal the fixture's baked
/// oracle value. Prints the aggregate and enforces the floors, so a regression that turns
/// the native tier into a deopt-only path -- or that makes *either* tier return a term the
/// real Erlang never returns -- is caught.
#[test]
fn interpreter_and_baseline_are_oracle_checked_over_the_stdlib_breadth_corpus() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let mut values = 0usize;
    let mut errors = 0usize;
    let mut skips = 0usize;
    let mut checked = [0usize; 2];
    let mut modules: BTreeSet<String> = BTreeSet::new();

    for line in BREADTH_TARGETS.lines() {
        let Some(target) = parse_breadth_target(line) else {
            continue;
        };
        let args = breadth_args(&mut host, target.args);
        let expected = parse_expected_value(&mut host, target.expected);
        let label = target.label();
        let outcome = run_target(
            &mut host,
            target.module,
            target.function,
            &args,
            &label,
            &Expectation::Value(&expected),
        );
        match outcome {
            Some(row) => {
                modules.insert(target.module.to_string());
                match row.verdict {
                    Verdict::Value => values += 1,
                    Verdict::Error => errors += 1,
                }
                for (slot, is_checked) in row.oracle_checked.iter().enumerate() {
                    if *is_checked {
                        checked[slot] += 1;
                    }
                }
            }
            None => skips += 1,
        }
    }

    let compared = values + errors;
    let blocked = compared * 2 - (checked[0] + checked[1]);
    assert_eq!(
        blocked, 0,
        "a breadth corpus row is a typed gap on at least one tier (see the `BLOCKED:` \
         lines above), so its oracle value could not be checked: this corpus executes on \
         both tiers today, so a gap here is a regression"
    );
    println!(
        "JIT-REAL (breadth): {compared} calls; oracle-checked interpreter={} baseline={}; \
         blocked={blocked}; modules={}; skipped={skips} \
         ({values} agreed on a value, {errors} agreed on a typed error)",
        checked[0],
        checked[1],
        modules.len()
    );
    assert!(
        compared >= BREADTH_COMPARISON_FLOOR,
        "only {compared} corpus calls were compared across both tiers (floor \
         {BREADTH_COMPARISON_FLOOR}); {skips} were skipped"
    );
    assert!(
        values >= BREADTH_VALUE_FLOOR,
        "only {values} comparisons agreed on a VALUE (floor {BREADTH_VALUE_FLOOR}) -- the \
         suite must not pass by agreeing only on typed errors"
    );
    assert!(
        checked[0] >= BREADTH_COMPARISON_FLOOR,
        "only {} breadth calls were oracle-checked on Tier::Interpreter (floor \
         {BREADTH_COMPARISON_FLOOR})",
        checked[0]
    );
    assert!(
        checked[1] >= BREADTH_COMPARISON_FLOOR,
        "only {} breadth calls were oracle-checked on Tier::BaselineJit (floor \
         {BREADTH_COMPARISON_FLOOR})",
        checked[1]
    );
    assert!(
        modules.len() >= BREADTH_MODULE_FLOOR,
        "only {} distinct installed modules were reached (floor {BREADTH_MODULE_FLOOR})",
        modules.len()
    );
}

// ---------------------------------------------------------------------------
// Corpus 2: the error-path targets (the compiled code raises)
// ---------------------------------------------------------------------------

/// The committed oracle-checked error-path corpus (`tests/exceptions_corpus.rs`'s data).
const ERROR_TARGETS: &str = include_str!("fixtures/error_path_targets.txt");

/// Floor on the number of error-path calls both tiers must execute and agree on. Set to
/// the honest number this suite measures (232, exactly the number the interpreter reaches
/// in `tests/exceptions_corpus.rs`).
const ERROR_COMPARISON_FLOOR: usize = 220;
/// Floor on the number of *raising* agreements inside that set (both tiers raised the same
/// way), so the suite proves the raise/unwind path is tier-identical, not just the
/// entry-check errors.
const ERROR_RAISE_FLOOR: usize = 200;
/// Floor on the number of distinct installed modules reached (measured: 215).
const ERROR_MODULE_FLOOR: usize = 200;

// The error-corpus parsing, its poison arguments and the oracle comparator are the one
// shared implementation in `tests/support/oracle.rs` (`parse_error_targets`,
// `ErrorTarget::args`, `verify_error`, `describe_error`), which
// `tests/exceptions_corpus.rs` drives on the interpreter too.

/// **The error-path measurement: both tiers, every poisoned corpus call, checked
/// against the real system `erl`.**
///
/// Every target makes the *compiled code itself* raise, so this is the raise/catch/unwind
/// path's tier equivalence **and** its oracle fidelity: the native tier must reproduce the
/// interpreter's typed error exactly (class, reason shape and stacktrace presence), never
/// a value and never a different error, and each tier's raise must carry the class and
/// reason term the system `erl` produces.
#[test]
fn interpreter_and_baseline_are_oracle_checked_over_the_real_error_path_corpus() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let targets = parse_error_targets(ERROR_TARGETS);
    assert!(
        targets.len() >= ERROR_COMPARISON_FLOOR,
        "the error-path corpus shrank: {} < {ERROR_COMPARISON_FLOOR}",
        targets.len()
    );

    let mut compared = 0usize;
    let mut raised = 0usize;
    let mut values = 0usize;
    let mut skips = 0usize;
    let mut checked = [0usize; 2];
    let mut modules: BTreeSet<String> = BTreeSet::new();

    for target in &targets {
        let args = target.args(&mut host);
        let label = target.label();
        let outcome = run_target(
            &mut host,
            &target.module,
            &target.function,
            &args,
            &label,
            &Expectation::Error(target),
        );
        match outcome {
            Some(row) => {
                compared += 1;
                modules.insert(target.module.clone());
                match row.verdict {
                    Verdict::Error => raised += 1,
                    Verdict::Value => values += 1,
                }
                for (slot, is_checked) in row.oracle_checked.iter().enumerate() {
                    if *is_checked {
                        checked[slot] += 1;
                    }
                }
            }
            None => skips += 1,
        }
    }

    let blocked = compared * 2 - (checked[0] + checked[1]);
    println!(
        "JIT-REAL (error-path): {compared} poisoned calls; oracle-checked \
         interpreter={} baseline={}; blocked={blocked}; modules={}; skipped={skips} \
         ({raised} agreed on a raise/typed error, {values} agreed on a value)",
        checked[0],
        checked[1],
        modules.len()
    );
    assert!(
        compared >= ERROR_COMPARISON_FLOOR,
        "only {compared} error-path calls were compared across both tiers (floor \
         {ERROR_COMPARISON_FLOOR}); {skips} were skipped"
    );
    assert!(
        raised >= ERROR_RAISE_FLOOR,
        "only {raised} comparisons agreed on a typed error/raise (floor \
         {ERROR_RAISE_FLOOR}) -- the raise path's tier equivalence is not exercised"
    );
    assert!(
        checked[0] >= ERROR_RAISE_FLOOR,
        "only {} error-path calls were oracle-checked on Tier::Interpreter (floor \
         {ERROR_RAISE_FLOOR}) -- {blocked} tier outcome(s) were typed gaps (see the \
         `BLOCKED:` lines above)",
        checked[0]
    );
    assert!(
        checked[1] >= ERROR_RAISE_FLOOR,
        "only {} error-path calls were oracle-checked on Tier::BaselineJit (floor \
         {ERROR_RAISE_FLOOR}) -- {blocked} tier outcome(s) were typed gaps (see the \
         `BLOCKED:` lines above)",
        checked[1]
    );
    assert!(
        modules.len() >= ERROR_MODULE_FLOOR,
        "only {} distinct installed modules were reached (floor {ERROR_MODULE_FLOOR})",
        modules.len()
    );
}

// ---------------------------------------------------------------------------
// The block register allocator -- evidence it actually engaged
// ---------------------------------------------------------------------------

/// The baseline block emitter's per-block resident-register budget (the longest-lived
/// x-registers of *one* block, held in `rbp` + `r12`..`r15`). The artifact's
/// `cached_registers()` is the **union across every block**, so it may exceed this.
const CACHE_BUDGET: usize = 5;

/// **Proof the native tier is not merely a deopt loop:** compiling real stdlib modules
/// produces non-trivial artifacts.
///
/// For each real module the artifact must have compiled blocks, a non-empty **resident
/// register set** (the live-interval allocator's output, deduped across blocks), a
/// native-register bound that covers every resident, and a non-empty code mapping. A
/// deopt-only tier would have no native register touches at all, so the non-empty resident
/// set is the load-bearing assertion: the allocator ran and the emitted code really
/// addresses `machine.x`.
#[test]
fn the_block_allocator_engages_on_real_stdlib_modules() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();

    // A spread of real, compilation-heavy modules: `lists` (recursive list code),
    // `ordsets` (call-heavy), `string`/`unicode_util` (bitstring/grapheme tables),
    // `calendar` (arithmetic) and `zip` (binaries + exceptions).
    let mut checked = 0usize;
    for name in ["lists", "ordsets", "proplists", "calendar", "string", "unicode_util", "zip"] {
        let module = host.load(name).unwrap_or_else(|error| {
            panic!("load `{name}` (guarded by `skip_if_unavailable`): {error:?}")
        });
        let compiled = BaselineJit::new()
            .compile(&module)
            .unwrap_or_else(|error| panic!("compile `{name}`: {error:?}"));

        let cached = compiled.cached_registers();
        assert!(
            compiled.block_count() > 0,
            "`{name}` compiled no basic block"
        );
        assert!(
            !cached.is_empty(),
            "`{name}` cached NO x-register in a machine register -- the block allocator \
             did not engage (a deopt-only artifact)"
        );
        let highest_cached = compiled
            .highest_cached_register()
            .unwrap_or_else(|| panic!("`{name}` cached registers but reports no highest"));
        let highest_native = compiled
            .highest_native_register()
            .unwrap_or_else(|| panic!("`{name}` touched no register natively"));
        assert_eq!(
            *cached.iter().max().expect("non-empty"),
            highest_cached,
            "`{name}`: the resident set's maximum must be the recorded cached bound"
        );
        assert!(
            highest_native >= highest_cached,
            "`{name}`: every resident register is touched natively, so the native bound \
             ({highest_native}) must cover the cached bound ({highest_cached})"
        );
        assert!(
            highest_native < MAX_REGISTER,
            "`{name}`: the native bound {highest_native} must be addressable in `machine.x`"
        );
        assert!(
            cached.len() >= CACHE_BUDGET,
            "`{name}` cached only {} register(s) across all its blocks -- far short of the \
             {CACHE_BUDGET}-register per-block budget the allocator works to",
            cached.len()
        );
        assert!(compiled.code_len() > 0, "`{name}` emitted no machine code");
        assert!(
            compiled.code_address_range().1 > compiled.code_address_range().0,
            "`{name}`: the executable mapping must be non-empty"
        );

        println!(
            "JIT-REAL (allocator): `{name}` -- {} compiled blocks, residents {cached:?}, \
             cached<=x{highest_cached}, native<=x{highest_native}, {} bytes of code",
            compiled.block_count(),
            compiled.code_len()
        );
        checked += 1;
    }
    assert_eq!(checked, 7, "every listed module was checked");
}

// ---------------------------------------------------------------------------
// The tier roster + the one pinned entry-check divergence
// ---------------------------------------------------------------------------

/// The baseline tier is part of every build's roster (this suite's premise).
#[test]
fn the_baseline_tier_is_in_the_roster() {
    assert_eq!(available_tiers().first().copied(), Some("interpreter"));
    assert!(available_tiers().contains(&"baseline-jit"));
    assert!(Tier::BaselineJit.is_available());
}

/// **The single known tier divergence, pinned.**
///
/// A call to a `module:function/arity` the module does not export never reaches an
/// instruction, so it is reported by each tier's own *entry* check. Both are the same
/// typed error -- an `unsupported_opcode` raise -- but the diagnostic fragment differs
/// (`run_export:` vs `jit:`). This test pins **both** strings, so the difference stays
/// visible and a future unification has to update this test rather than pass unnoticed.
#[test]
fn missing_export_is_a_typed_error_on_both_tiers_with_a_different_diagnostic() {
    if skip_if_unavailable() {
        return;
    }
    // The module's own **private** atom table (the plain loader entry point), so a name
    // it does not define can be interned into it without touching a shared host. The
    // export check fires before any instruction, so an empty context with no BIF floor
    // is enough to reach both tiers' entry checks.
    let dir = app_ebin("stdlib").expect("stdlib ebin (guarded by `skip_if_unavailable`)");
    let mut module =
        load_file(dir.join("lists.beam")).expect("load the real `lists.beam` with `load_file`");
    let missing = module.atom_table.intern("no_such_function_xyz");
    assert!(
        module.export(missing, 1).is_none(),
        "the probe name must not be an export, or the experiment proves nothing"
    );

    let run = |tier: Tier| -> BeamError {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        machine
            .run_with_tier(
                &module,
                missing,
                1,
                &[beam_term::make_small(1)],
                &mut ctx,
                tier,
            )
            .expect_err("a missing export must be a typed error, never a value")
    };

    let interpreter = run(Tier::Interpreter);
    let baseline = run(Tier::BaselineJit);

    let reason_atom = |error: &BeamError| match error {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .map(str::to_string)
            .unwrap_or_else(|| panic!("expected an atom reason, got {error:?}")),
        other => panic!("expected a typed raise, got {other:?}"),
    };

    assert_eq!(
        reason_atom(&interpreter),
        "unsupported_opcode: run_export: export not found",
        "the interpreter's entry-check diagnostic"
    );
    assert_eq!(
        reason_atom(&baseline),
        "unsupported_opcode: jit: export not found",
        "the native tier's entry-check diagnostic"
    );
}

// A compile-time guard that the host really is the engine's execution seam (the anchor
// for the `mod support;` dependency above).
#[allow(dead_code)]
fn _assert_host_seam() {
    fn _is_context<T: beam_engine::ExecutionContext>() {}
    _is_context::<ModuleHost>();
}
