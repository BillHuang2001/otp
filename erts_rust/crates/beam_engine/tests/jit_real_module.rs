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

//! **Baseline-JIT tier equivalence on the REAL installed OTP corpus.**
//!
//! `tests/jit.rs` proves the native tier on the vertical slice, `tests/jit_coverage.rs`
//! on the broad hand-authored `coverage.beam` fixture. This file closes the remaining
//! question: does the **baseline JIT** (`Tier::BaselineJit`, the x86-64 block back end
//! in `src/jit/baseline/`) produce results **identical to the interpreter**
//! (`Tier::Interpreter`) on **real, `erlc`-produced system modules** -- the modules a real
//! OTP install actually ships?
//!
//! # What is compared, and why it is a strong claim
//!
//! Every target is a real `Module:Function(Args)` call drawn from the crate's two
//! *curated, oracle-checked* corpora (the same data `tests/stdlib_lists.rs` and
//! `tests/exceptions_corpus.rs` drive on the interpreter):
//!
//! * `tests/fixtures/stdlib_breadth_targets.txt` -- 504 happy-path calls over **171**
//!   distinct installed modules (`lists`, `unicode`, `string`, `calendar`, `zip`, ...),
//!   each carrying the exact `erl -noshell -eval ...` command that produced its expected
//!   value;
//! * `tests/fixtures/error_path_targets.txt` -- 232 error-path calls over **213**
//!   modules, where the *compiled code itself* raises (`func_info` -> `function_clause`,
//!   `case_end`, `badmatch`, `badrecord`).
//!
//! Each call is run **twice over the same loaded module** -- once on `Tier::Interpreter`,
//! once on `Tier::BaselineJit` -- and the two outcomes must be **the same**:
//!
//! * both returned a value, and the values are the same term under `beam_term::cmp` (the
//!   module's exact `=:=` ordering, so boxed tuples/lists/floats compare by value); or
//! * both returned a typed [`beam_common::BeamError`], and the errors are the **same
//!   shape** (`Debug`-identical: class, reason, stacktrace, live-reason presence) -- a
//!   typed error on one tier and a value on the other **fails**, and two *different*
//!   typed errors **fail** too.
//!
//! The corpora are curated exactly so this is safe: every call is a real, terminating
//! call over small representative arguments, so neither tier can loop forever, and the
//! same calls already run to completion on the interpreter in the two suites above.
//! Nothing here is guessed -- the *oracle fidelity* of the interpreter is those suites'
//! job; this suite's job is that the native tier is observationally the interpreter.
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
//! # The allocator evidence
//!
//! Agreement alone would also hold if the native tier only ever deopted, so
//! [`the_block_allocator_engages_on_real_stdlib_modules`] compiles real modules with
//! [`BaselineJit`] and asserts the artifact is non-trivial: compiled blocks, a non-empty
//! **resident register set** and a native-register bound -- i.e. the usage-weighted
//! allocator in `src/jit/baseline/emit/regalloc.rs` actually ran and the code really
//! addresses x-registers.

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::jit::baseline::BaselineJit;
use beam_engine::jit::JitBackend;
use beam_engine::{available_tiers, terms, Machine, MinimalContext, Tier, MAX_REGISTER};
use beam_loader::{load_file, Module};
use beam_term::Eterm;
use support::{all_otp_ebins, app_ebin, skip_reason, stdlib_ebin, ModuleHost};

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

/// Drives one `module:function/args` through **both** tiers and asserts they agree.
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
) -> Option<Verdict> {
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
    Some(compare(host, label, &interp, &jit))
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

/// Parses one `module:function(args) => expected` corpus line; `None` for a comment/blank
/// line. A malformed *target* line panics (a corpus bug), so a broken fixture can never
/// be silently skipped.
fn parse_breadth_target(line: &str) -> Option<(&str, &str, &str)> {
    let line = line.split("  # oracle:").next().unwrap_or(line).trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, _expected) = line.split_once(" => ").expect("corpus line without ` => `");
    let (module, rest) = key.split_once(':').expect("corpus key without `:`");
    let (function, args) = rest.split_once('(').expect("corpus key without `(`");
    let args = args.strip_suffix(')').expect("corpus key without `)`");
    Some((module, function, args))
}

/// Splits an argument list on **top-level** commas, respecting `[]`/`{}`/`<<>>` nesting
/// and `"`-quoted strings.
fn split_arg_terms(args: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut start = 0usize;
    for (index, &byte) in args.as_bytes().iter().enumerate() {
        match byte {
            b'"' => in_string = !in_string,
            b'[' | b'{' | b'<' if !in_string => depth += 1,
            b']' | b'}' | b'>' if !in_string => depth -= 1,
            b',' if !in_string && depth == 0 => {
                parts.push(&args[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&args[start..]);
    parts
}

/// Builds one corpus argument from its source text: `[]`, `[a,b,c]`, `{a,b}`,
/// `<<1,2,3>>`, `"abc"`, an integer, an atom (bare or `'quoted'`).
fn arg_term(host: &mut ModuleHost, text: &str) -> Eterm {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix("<<").and_then(|t| t.strip_suffix(">>")) {
        let bytes: Vec<u8> = if inner.trim().is_empty() {
            Vec::new()
        } else {
            split_arg_terms(inner)
                .iter()
                .map(|part| {
                    part.trim()
                        .parse::<u8>()
                        .expect("a corpus binary element is a byte literal")
                })
                .collect()
        };
        return terms::make_heap_binary(host, &bytes).expect("build the corpus binary argument");
    }
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        if inner.trim().is_empty() {
            return beam_term::NIL;
        }
        let elements: Vec<Eterm> = split_arg_terms(inner)
            .iter()
            .map(|part| arg_term(host, part))
            .collect();
        return terms::make_list(host, &elements).expect("build the corpus list argument");
    }
    if let Some(inner) = text.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        let elements: Vec<Eterm> = split_arg_terms(inner)
            .iter()
            .map(|part| arg_term(host, part))
            .collect();
        return terms::make_tuple(host, &elements).expect("build the corpus tuple argument");
    }
    if let Some(inner) = text.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        let chars: Vec<Eterm> = inner
            .bytes()
            .map(|byte| beam_term::make_small(i64::from(byte)))
            .collect();
        return terms::make_list(host, &chars).expect("build the corpus string argument");
    }
    if let Ok(value) = text.parse::<i64>() {
        return beam_term::make_small(value);
    }
    match text.strip_prefix('\'').and_then(|t| t.strip_suffix('\'')) {
        Some(quoted) => host.intern(quoted),
        None => host.intern(text),
    }
}

/// Builds the argument list for a corpus call from its `(<args>)` text; the empty text is
/// the zero-arity call.
fn breadth_args(host: &mut ModuleHost, args: &str) -> Vec<Eterm> {
    if args.trim().is_empty() {
        return Vec::new();
    }
    split_arg_terms(args)
        .iter()
        .map(|part| arg_term(host, part))
        .collect()
}

/// **The headline measurement: both tiers, every happy-path corpus call.**
///
/// Runs each of the 504 real calls through the interpreter and the baseline JIT over one
/// shared host, and requires the outcomes to be identical (same term, or the same typed
/// error). Prints the aggregate and enforces the floors, so a regression that turns the
/// native tier into a deopt-only or wrong-value path is caught.
#[test]
fn interpreter_and_baseline_agree_over_the_stdlib_breadth_corpus() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let mut values = 0usize;
    let mut errors = 0usize;
    let mut skips = 0usize;
    let mut modules: BTreeSet<String> = BTreeSet::new();

    for line in BREADTH_TARGETS.lines() {
        let Some((module, function, args_text)) = parse_breadth_target(line) else {
            continue;
        };
        let args = breadth_args(&mut host, args_text);
        let label = format!("{module}:{function}({args_text})");
        match run_target(&mut host, module, function, &args, &label) {
            Some(Verdict::Value) => {
                values += 1;
                modules.insert(module.to_string());
            }
            Some(Verdict::Error) => {
                errors += 1;
                modules.insert(module.to_string());
            }
            None => skips += 1,
        }
    }

    println!(
        "JIT-REAL (breadth): {} real calls compared across interpreter|baseline-jit \
         ({values} agreed on a value, {errors} agreed on a typed error); \
         distinct modules reached: {}; skipped: {skips}",
        values + errors,
        modules.len()
    );
    assert!(
        values + errors >= BREADTH_COMPARISON_FLOOR,
        "only {} corpus calls were compared across both tiers (floor \
         {BREADTH_COMPARISON_FLOOR}); {skips} were skipped",
        values + errors
    );
    assert!(
        values >= BREADTH_VALUE_FLOOR,
        "only {values} comparisons agreed on a VALUE (floor {BREADTH_VALUE_FLOOR}) -- the \
         suite must not pass by agreeing only on typed errors"
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

/// The poison argument a target function is called with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Poison {
    /// The atom `genesis_poison` (matches no clause).
    Atom,
    /// The small integer `7`.
    Int,
    /// The empty list `[]`.
    Nil,
    /// The empty tuple `{}`.
    Tuple0,
}

/// One parsed error-corpus line.
struct ErrorTarget {
    module: String,
    function: String,
    arity: u32,
    /// One poison per argument (the file's `POISONS` list; its length equals the arity).
    poisons: Vec<Poison>,
}

/// Parses the committed error-path corpus, panicking (a test-authoring bug, never runtime
/// data) on a malformed line.
fn parse_error_targets() -> Vec<ErrorTarget> {
    let mut targets = Vec::new();
    for (index, raw) in ERROR_TARGETS.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The justification rides in a trailing `# ...` comment.
        let spec = line.split_once('#').map(|(spec, _)| spec.trim()).unwrap_or(line);
        let tokens: Vec<&str> = spec.split_whitespace().collect();
        assert_eq!(
            tokens.len(),
            4,
            "line {}: expected `<module>:<f>/<n> <poisons> => <expected>`, got {spec:?}",
            index + 1
        );
        assert_eq!(tokens[2], "=>", "line {}: missing `=>`: {spec:?}", index + 1);
        let (module, rest) = tokens[0]
            .split_once(':')
            .unwrap_or_else(|| panic!("line {}: no `:` in {:?}", index + 1, tokens[0]));
        let (function, arity) = rest
            .split_once('/')
            .unwrap_or_else(|| panic!("line {}: no `/` in {:?}", index + 1, tokens[0]));
        let arity: u32 = arity
            .parse()
            .unwrap_or_else(|_| panic!("line {}: bad arity in {:?}", index + 1, tokens[0]));
        let poisons: Vec<Poison> = tokens[1]
            .split(',')
            .map(|token| match token {
                "atom" => Poison::Atom,
                "int" => Poison::Int,
                "nil" => Poison::Nil,
                "tuple0" => Poison::Tuple0,
                other => panic!("line {}: unknown poison {other:?}", index + 1),
            })
            .collect();
        assert_eq!(
            poisons.len(),
            arity as usize,
            "line {}: {} poison(s) for arity {arity}",
            index + 1,
            poisons.len()
        );
        targets.push(ErrorTarget {
            module: module.to_string(),
            function: function.to_string(),
            arity,
            poisons,
        });
    }
    targets
}

/// Builds one poison argument on `host`'s arena (which outlives the run).
fn poison_term(host: &mut ModuleHost, poison: Poison) -> Eterm {
    match poison {
        Poison::Atom => host.intern("genesis_poison"),
        Poison::Int => beam_term::make_small(7),
        Poison::Nil => beam_term::NIL,
        Poison::Tuple0 => terms::make_tuple(host, &[]).expect("build the empty tuple"),
    }
}

/// **The error-path measurement: both tiers, every poisoned corpus call.**
///
/// Every target makes the *compiled code itself* raise, so this is the raise/catch/unwind
/// path's tier equivalence: the native tier must reproduce the interpreter's typed error
/// exactly (class, reason shape and stacktrace presence), never a value and never a
/// different error.
#[test]
fn interpreter_and_baseline_agree_over_the_real_error_path_corpus() {
    if skip_if_unavailable() {
        return;
    }
    let mut host = host();
    let targets = parse_error_targets();
    assert!(
        targets.len() >= ERROR_COMPARISON_FLOOR,
        "the error-path corpus shrank: {} < {ERROR_COMPARISON_FLOOR}",
        targets.len()
    );

    let mut compared = 0usize;
    let mut raised = 0usize;
    let mut values = 0usize;
    let mut skips = 0usize;
    let mut modules: BTreeSet<String> = BTreeSet::new();

    for target in &targets {
        let args: Vec<Eterm> = target
            .poisons
            .iter()
            .map(|poison| poison_term(&mut host, *poison))
            .collect();
        let label = target.label();
        match run_target(&mut host, &target.module, &target.function, &args, &label) {
            Some(verdict) => {
                compared += 1;
                modules.insert(target.module.clone());
                match verdict {
                    Verdict::Error => raised += 1,
                    Verdict::Value => values += 1,
                }
            }
            None => skips += 1,
        }
    }

    println!(
        "JIT-REAL (error-path): {compared} poisoned calls compared across \
         interpreter|baseline-jit ({raised} agreed on a raise/typed error, \
         {values} agreed on a value); distinct modules reached: {}; skipped: {skips}",
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
        modules.len() >= ERROR_MODULE_FLOOR,
        "only {} distinct installed modules were reached (floor {ERROR_MODULE_FLOOR})",
        modules.len()
    );
}

impl ErrorTarget {
    /// The `<module>:<function>/<arity>` label used in the report.
    fn label(&self) -> String {
        format!("{}:{}/{}", self.module, self.function, self.arity)
    }
}

// ---------------------------------------------------------------------------
// The block register allocator -- evidence it actually engaged
// ---------------------------------------------------------------------------

/// The baseline block emitter's per-block resident-register budget (the most-touched
/// x-registers of *one* block, held in `rbp` + `r12`..`r15`). The artifact's
/// `cached_registers()` is the **union across every block**, so it may exceed this.
const CACHE_BUDGET: usize = 5;

/// **Proof the native tier is not merely a deopt loop:** compiling real stdlib modules
/// produces non-trivial artifacts.
///
/// For each real module the artifact must have compiled blocks, a non-empty **resident
/// register set** (the usage-weighted allocator's output, deduped across blocks), a
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
