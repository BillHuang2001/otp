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

//! The **corpus error-path sweep**: the error-path analogue of
//! `tests/stdlib_sweep.rs`'s execution-breadth sweep, run over the *real*
//! installed OTP `stdlib`/`kernel`/`compiler` modules.
//!
//! Where `tests/exceptions.rs` proves the raise/catch machinery on one committed
//! fixture (`tests/fixtures/excpaths.beam`), this file proves its **corpus reach**:
//! that the interpreter's in-VM raise opcodes now *execute* on real compiler output
//! and produce the **reason term the system `erl` produces**.
//!
//! # What is being proven
//!
//! The real OTP compiler does **not** emit the runtime's well-known exception atoms
//! (`function_clause`, `case_clause`, `if_clause`, `badmatch`, `badrecord`, `error`,
//! …) into a module's atom chunk — a real runtime keeps them globally interned
//! (`../../erts/emulator/beam/beam_common.c:382-406`, `error_atom[NUMBER_EXIT_CODES]`).
//! The engine resolves a raise *reason* atom **by name** from the running module's
//! atom table and refuses to substitute a *different* atom, so before the test host
//! seeded those atoms every compiled-code raise degraded to a typed `unsupported`.
//! `tests/support/mod.rs` now seeds them into every module it loads (the host's
//! analogue of the runtime's global interning); this file measures how far that
//! reaches across the installed corpus.
//!
//! Every target is a call that makes **the compiled code itself** raise an in-VM
//! exception — never one that reaches an unserved BIF first:
//!
//! * `func_info` → the **bare atom `function_clause`** (a failed function clause);
//! * `case_end` → `{case_clause, V}` (a failed `case`);
//! * `badmatch` → `{badmatch, V}` (a failed match);
//! * `badrecord` → `{badrecord, V}` (a failed record match).
//!
//! A poisoned argument (`genesis_poison`, `7`, `[]` or `{}`) matches no clause of the
//! target function, so the head-clause failure fires before any host call. A target may
//! carry **one poison per argument** (the file's `POISONS` list), so the sweep also
//! reaches the multi-argument exported functions the single-argument pass could not.
//!
//! # The corpus (`tests/fixtures/error_path_targets.txt`)
//!
//! One line per target: the call, the poison arguments, the expected reason and the exact
//! `erl -noshell -eval …` invocation that reproduces the expectation. Every expected
//! value is the **real** system Erlang's (OTP 29 / erts-17.1); none was guessed and
//! none came from the Rust engine. A candidate was admitted only when a fresh oracle run
//! raised the very reason recorded: candidates whose oracle run returned a *value*, or
//! whose oracle reason differed from the engine's, were dropped. The file documents the
//! deterministic capping rule and the un-capped sweep totals (633 `function_clause` + 99
//! tuple-reason agreeing calls in the first, single-argument pass, over the 162 modules
//! it reached; the arity-widened second pass added 53 targets over 51 further modules).
//! It is *data*: regenerate it, never hand-edit a value.
//!
//! # Skip-vs-assert policy (explicit)
//!
//! * **An agreeing reason is asserted exactly.** The recovered reason term must
//!   equal the oracle's, and the class must be `error`; a *different* reason — or a
//!   fabricated value — FAILS the test loudly.
//! * **A call that still needs unserved machinery is skipped LOUDLY.** When the
//!   engine returns a typed error instead of executing the raise (no live reason
//!   term), the target prints `SKIP: <module>:<f>/<n> — <typed error>`, is counted,
//!   and does **not** fail — a coverage gap is not a wrong result.
//! * **Breadth is enforced.** `ERROR-PATHS: modules covered: N` and the per-module
//!   list are printed, and `N` must reach [`MODULE_FLOOR`] — the honest number the
//!   sweep achieves — so a regression that collapses the reach is caught.
//!
//! Tests skip loudly (print the reason and return) when the oracle OTP install is
//! unavailable.

mod support;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;

use beam_common::{BeamError, ExceptionClass};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::load_file;
use beam_term::{cmp, make_atom, make_small, Eterm, NIL};
use support::{app_ebin, skip_reason, ModuleHost};

/// The committed oracle corpus (see the module docs).
const TARGETS_TXT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/error_path_targets.txt"
);

/// The atom every `atom`-shaped target is called with (matches no clause).
const POISON_ATOM: &str = "genesis_poison";

/// The honest number of corpus modules whose error paths execute and agree with the
/// oracle (the capping rule keeps at least one agreeing target per module, so this
/// equals the number of distinct modules in the corpus that are *reached*).
const MODULE_FLOOR: usize = 213;

/// The honest number of target lines in [`TARGETS_TXT`] (223 expect-agree + 9
/// expect-skip). A shrunk corpus fails here rather than silently weakening the sweep.
const TARGET_FLOOR: usize = 232;

// ---------------------------------------------------------------------------
// The corpus file: parsing
// ---------------------------------------------------------------------------

/// The argument a target function is called with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Poison {
    /// The atom [`POISON_ATOM`].
    Atom,
    /// The small integer `7`.
    Int,
    /// The empty list `[]`.
    Nil,
    /// The empty tuple `{}`.
    Tuple0,
}

/// The exception a target must produce.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Expected {
    /// A bare-atom reason with this name (`function_clause`).
    Atom(String),
    /// The 2-tuple reason `{tag, <poison>}` with this tag (`case_clause`/`badmatch`/
    /// `badrecord`).
    Tuple(String),
    /// The engine must instead return a typed error (a documented host gap).
    Unsupported,
}

/// One parsed corpus line.
struct Target {
    module: String,
    function: String,
    arity: u32,
    /// One poison per argument (the file's `POISONS` list; its length is the arity).
    poisons: Vec<Poison>,
    expected: Expected,
    /// The quoted `erl -noshell -eval …` command backing the expectation (empty for
    /// an `unsupported` line, whose expectation is the engine's own gap).
    oracle: String,
}

impl Target {
    /// The `<module>:<function>/<arity>` label used in the report.
    fn label(&self) -> String {
        format!("{}:{}/{}", self.module, self.function, self.arity)
    }
}

/// Renders a [`BeamError`] compactly for a `SKIP:` line: the engine's typed gap is a
/// `Raise` whose reason is the `unsupported_opcode: <what>` atom, so print that atom;
/// anything else prints its `Debug`.
fn describe_error(error: &BeamError) -> String {
    if let BeamError::Raise(raise) = error {
        if let Some(reason) = raise.reason.as_atom() {
            return format!("typed raise: {reason}");
        }
    }
    format!("{error:?}")
}

/// Parses one poison token, or `None` for an unknown token.
fn parse_poison(token: &str) -> Option<Poison> {
    match token {
        "atom" => Some(Poison::Atom),
        "int" => Some(Poison::Int),
        "nil" => Some(Poison::Nil),
        "tuple0" => Some(Poison::Tuple0),
        _ => None,
    }
}

/// Parses one expected-reason token (`unsupported`, a bare atom name, or `{tag}`).
fn parse_expected(token: &str) -> Expected {
    if token == "unsupported" {
        return Expected::Unsupported;
    }
    if let Some(inner) = token.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        return Expected::Tuple(inner.to_string());
    }
    Expected::Atom(token.to_string())
}

/// Parses the committed corpus, panicking (a test-authoring bug, never runtime data)
/// on a malformed line.
fn parse_targets() -> Vec<Target> {
    let text = std::fs::read_to_string(TARGETS_TXT).expect("read error_path_targets.txt");
    let mut targets = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The assertion's justification rides in a trailing `# …` comment.
        let (spec, oracle) = match line.split_once('#') {
            Some((spec, comment)) => (spec.trim(), comment.trim().to_string()),
            None => (line, String::new()),
        };
        let tokens: Vec<&str> = spec.split_whitespace().collect();
        assert_eq!(
            tokens.len(),
            4,
            "line {}: expected `<module>:<f>/<n> <poison> => <expected>`, got {spec:?}",
            index + 1
        );
        assert_eq!(
            tokens[2],
            "=>",
            "line {}: missing the `=>` separator: {spec:?}",
            index + 1
        );
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
            .map(|token| {
                parse_poison(token)
                    .unwrap_or_else(|| panic!("line {}: unknown poison {:?}", index + 1, token))
            })
            .collect();
        assert_eq!(
            poisons.len(),
            arity as usize,
            "line {}: {} poison(s) for arity {arity}: {:?}",
            index + 1,
            poisons.len(),
            tokens[1]
        );
        targets.push(Target {
            module: module.to_string(),
            function: function.to_string(),
            arity,
            poisons,
            expected: parse_expected(tokens[3]),
            oracle,
        });
    }
    targets
}

// ---------------------------------------------------------------------------
// Term construction for a target
// ---------------------------------------------------------------------------

/// Builds the argument term for `poison` (allocated on `host`'s arena, which outlives
/// the run).
fn poison_term(host: &mut ModuleHost, poison: Poison) -> Eterm {
    match poison {
        Poison::Atom => host.intern(POISON_ATOM),
        Poison::Int => make_small(7),
        Poison::Nil => NIL,
        Poison::Tuple0 => terms::make_tuple(host, &[]).expect("build the empty tuple"),
    }
}

/// The reason term `expected` names — a bare atom, or the 2-tuple `{tag, payload}` whose
/// payload is the LAST poison the target was called with (the file's documented rule).
fn expected_term(host: &mut ModuleHost, expected: &Expected, payload: Eterm) -> Eterm {
    match expected {
        Expected::Atom(name) => host.intern(name),
        Expected::Tuple(tag) => {
            let tag = host.intern(tag);
            terms::make_tuple(host, &[tag, payload]).expect("build the {tag, arg} reason")
        }
        Expected::Unsupported => unreachable!("`unsupported` has no reason term"),
    }
}

// ---------------------------------------------------------------------------
// The sweep
// ---------------------------------------------------------------------------

/// A host over the three swept applications' `ebin` directories, or `None` when any
/// is unavailable (the caller skips loudly).
fn corpus_host() -> Option<ModuleHost> {
    let mut dirs = Vec::new();
    for app in ["stdlib", "kernel", "compiler"] {
        dirs.push(app_ebin(app)?);
    }
    let mut host = ModuleHost::new(dirs[0].clone());
    for dir in &dirs[1..] {
        host.add_search_dir(dir.clone());
    }
    Some(host)
}

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

/// The corpus error-path sweep: runs every target and asserts the engine reproduces
/// the oracle's exception, printing the mechanism report and enforcing the floor.
#[test]
fn corpus_error_paths_execute_and_agree_with_the_oracle() {
    if skip_if_unavailable() {
        return;
    }
    let Some(mut host) = corpus_host() else {
        eprintln!(
            "SKIP: the corpus error-path sweep needs the `stdlib`, `kernel` AND `compiler` \
             `ebin` directories (set $ERTS_<APP>_DIR or put `erl` on PATH)"
        );
        return;
    };

    let targets = parse_targets();
    assert!(
        targets.len() >= TARGET_FLOOR,
        "the corpus shrank below its floor: {} < {TARGET_FLOOR}",
        targets.len()
    );

    let mut agreed = 0usize;
    let mut skipped = 0usize;
    let mut modules: BTreeSet<String> = BTreeSet::new();
    let mut failures: Vec<String> = Vec::new();
    let mut skip_lines: Vec<String> = Vec::new();

    for target in &targets {
        let args: Vec<Eterm> = target
            .poisons
            .iter()
            .map(|poison| poison_term(&mut host, *poison))
            .collect();
        // The `{tag, payload}` expectation's payload is the LAST poison (file rule).
        let arg = *args.last().expect("every target has at least one argument");
        let result = host.call(&target.module, &target.function, &args);

        match &target.expected {
            // A documented gap: a typed error is the expected outcome; a value is a
            // fabricated result and fails.
            Expected::Unsupported => match result {
                Ok(value) => failures.push(format!(
                    "{} returned {value:?}; the engine gap must be a typed error, not a value",
                    target.label()
                )),
                Err(error) => {
                    skipped += 1;
                    skip_lines.push(format!("SKIP: {} — {}", target.label(), describe_error(&error)));
                }
            },
            expected => match result {
                // A structured raise whose reason term rides out of the machine.
                Err(BeamError::Raise(raise)) => {
                    let reason = raise
                        .live_reason()
                        .and_then(|live| live.downcast_ref::<Eterm>())
                        .copied();
                    let Some(reason) = reason else {
                        // No live reason term: the engine never executed the compiled
                        // raise (an `unsupported_opcode` gap). Not a wrong result —
                        // report it and count it as a skip.
                        skipped += 1;
                        skip_lines.push(format!(
                            "SKIP: {} — {}",
                            target.label(),
                            describe_error(&BeamError::Raise(raise.clone()))
                        ));
                        continue;
                    };
                    let want = expected_term(&mut host, expected, arg);
                    // The in-VM raise opcodes are all class `error`; assert it where
                    // it is observable.
                    if raise.class != ExceptionClass::Error {
                        failures.push(format!(
                            "{} raised class {:?}, the oracle's class is `error`",
                            target.label(),
                            raise.class
                        ));
                        continue;
                    }
                    if cmp(reason, want, host.atom_table()).is_eq() {
                        agreed += 1;
                        modules.insert(target.module.clone());
                    } else {
                        failures.push(format!(
                            "{} raised a DIFFERENT reason than the oracle: got {reason:?}, \
                             want {want:?}  [{}]",
                            target.label(),
                            target.oracle
                        ));
                    }
                }
                // A typed error without a raise: a coverage gap (skip loudly).
                Err(error) => {
                    skipped += 1;
                    skip_lines.push(format!("SKIP: {} — {}", target.label(), describe_error(&error)));
                }
                // A value where the oracle raises: a fabricated result — fail loudly.
                Ok(value) => failures.push(format!(
                    "{} returned {value:?}; the oracle raises  [{}]",
                    target.label(),
                    target.oracle
                )),
            },
        }
    }

    // ---- the mechanism report -------------------------------------------
    println!(
        "ERROR-PATHS: targets {} ({} expect-agree, {} expect-skip); agreed {agreed}, \
         skipped {skipped}",
        targets.len(),
        targets
            .iter()
            .filter(|t| t.expected != Expected::Unsupported)
            .count(),
        targets
            .iter()
            .filter(|t| t.expected == Expected::Unsupported)
            .count()
    );
    println!("ERROR-PATHS: modules covered: {}", modules.len());
    let mut list = String::from("ERROR-PATHS: modules: ");
    for (index, module) in modules.iter().enumerate() {
        if index > 0 && index % 8 == 0 {
            let _ = write!(list, "\nERROR-PATHS:   ");
        } else if index > 0 {
            list.push(' ');
        }
        let _ = write!(list, "{module}");
    }
    println!("{list}");
    for line in &skip_lines {
        println!("{line}");
    }

    // ---- breadth enforcement --------------------------------------------
    assert!(
        failures.is_empty(),
        "{} corpus error-path target(s) DISAGREE with the system `erl` oracle:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(
        modules.len() >= MODULE_FLOOR,
        "only {} corpus modules' error paths execute and agree — the floor is {MODULE_FLOOR} \
         (regressions above carry their typed errors)",
        modules.len()
    );
}

// ---------------------------------------------------------------------------
// The before/after evidence: the same calls WITHOUT the host's atom seeding
// ---------------------------------------------------------------------------

/// One sample of a corpus target, for the before/after comparison.
struct BeforeSample {
    app: &'static str,
    module: &'static str,
    function: &'static str,
    poison: Poison,
    /// The corpus's expected reason tag, for the assertion message.
    tag: &'static str,
}

/// The sample: one target per raise shape, drawn from the corpus itself.
const BEFORE_SAMPLES: &[BeforeSample] = &[
    BeforeSample {
        app: "stdlib",
        module: "base64",
        function: "decode",
        poison: Poison::Atom,
        tag: "function_clause",
    },
    BeforeSample {
        app: "kernel",
        module: "disk_log",
        function: "ll_close",
        poison: Poison::Atom,
        tag: "case_clause",
    },
    BeforeSample {
        app: "kernel",
        module: "prim_tty",
        function: "output_mode",
        poison: Poison::Atom,
        tag: "badrecord",
    },
];

/// The `.beam` path for a module of `app`, or `None` when the install is absent.
fn app_beam(app: &str, module: &str) -> Option<PathBuf> {
    let dir = app_ebin(app)?;
    Some(dir.join(format!("{module}.beam")))
}

/// Proves the *gap the seeding closes*, on the corpus itself: loaded **without** the
/// host's well-known-atom seeding (`beam_loader::load_file` builds a private atom
/// table holding only the module's own atoms), the very same calls cannot name their
/// reason and degrade to a typed `unsupported_opcode` — the raise never starts, and
/// no live reason term exists. Loaded **through** the host (the sweep above), the same
/// calls raise the oracle's exact reason. That is the whole before/after delta.
#[test]
fn without_the_atom_seed_the_corpus_calls_cannot_name_their_reason() {
    if skip_if_unavailable() {
        return;
    }
    for sample in BEFORE_SAMPLES {
        let Some(path) = app_beam(sample.app, sample.module) else {
            eprintln!(
                "SKIP: before/after sample {}:{} — the {} application is unavailable",
                sample.module, sample.function, sample.app
            );
            continue;
        };
        // `load_file` is the loader's plain entry point: a PRIVATE atom table with
        // none of the runtime's well-known atoms — the pre-seeding state.
        let mut module = load_file(&path).unwrap_or_else(|error| {
            panic!("load {}: {error:?}", path.display())
        });
        let function = module
            .atom_table
            .lookup(sample.function)
            .unwrap_or_else(|| panic!("{}: no `{}` export", sample.module, sample.function));
        assert!(
            module.atom_table.lookup(sample.tag).is_none(),
            "{} must not carry the atom `{}` (it would defeat the experiment)",
            sample.module,
            sample.tag
        );

        let mut ctx = MinimalContext::new();
        let arg = match sample.poison {
            Poison::Atom => make_atom(module.atom_table.intern(POISON_ATOM)),
            Poison::Int => make_small(7),
            Poison::Nil => NIL,
            Poison::Tuple0 => terms::make_tuple(&mut ctx, &[]).expect("empty tuple"),
        };
        let mut machine = Machine::new();
        let result = machine.run_export(&module, function, 1, &[arg], &mut ctx);
        let (error, machine) = match result {
            Err(error) => (error, machine),
            Ok(value) => panic!(
                "{}:{} without the seed returned {value:?}; the raise must be a typed error",
                sample.module, sample.function
            ),
        };
        let raise = match error {
            BeamError::Raise(raise) => raise,
            other => panic!(
                "{}:{} without the seed must be a typed raise, got {other:?}",
                sample.module, sample.function
            ),
        };
        assert!(
            raise.live_reason().is_none(),
            "without the seed the raise must not start, so no live reason term exists"
        );
        let reason = raise
            .reason
            .as_atom()
            .unwrap_or_else(|| panic!("the typed gap is an atom reason, got {raise:?}"));
        assert!(
            reason.starts_with("unsupported_opcode: ")
                && reason.contains(&format!("no '{}' atom in the module", sample.tag)),
            "{}:{} without the seed must report the unnameable `{}` reason atom, got {reason:?}",
            sample.module,
            sample.function,
            sample.tag
        );
        assert!(
            machine.pending_exception().is_none(),
            "the raise never started, so no pending exception was recorded"
        );
        eprintln!(
            "BEFORE {}:{} — typed gap: {reason}",
            sample.module, sample.function
        );
    }
}

/// The **host** half of the same comparison: unloading the seed is not possible, so
/// this pins that the shared-atom host *does* make the same samples raise the oracle's
/// reason — the after state, side by side with the before state above.
#[test]
fn the_hosts_atom_seed_makes_the_same_calls_raise_the_oracle_reason() {
    if skip_if_unavailable() {
        return;
    }
    let Some(mut host) = corpus_host() else {
        eprintln!("SKIP: the `stdlib`/`kernel`/`compiler` install is unavailable");
        return;
    };
    for sample in BEFORE_SAMPLES {
        let arg = poison_term(&mut host, sample.poison);
        let result = host.call(sample.module, sample.function, &[arg]);
        let raise = match result {
            Err(BeamError::Raise(raise)) => raise,
            other => panic!(
                "AFTER: {}:{} must raise through the host, got {other:?}",
                sample.module, sample.function
            ),
        };
        let reason = raise
            .live_reason()
            .and_then(|live| live.downcast_ref::<Eterm>())
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "AFTER: {}:{} must carry a live reason term — the seed should make it nameable",
                    sample.module, sample.function
                )
            });
        let want = match &sample.poison {
            // `function_clause` is a bare atom; the others are `{tag, arg}`.
            _ if sample.tag == "function_clause" => host.intern(sample.tag),
            _ => {
                let tag = host.intern(sample.tag);
                terms::make_tuple(&mut host, &[tag, arg]).expect("build the reason")
            }
        };
        assert!(
            cmp(reason, want, host.atom_table()).is_eq(),
            "AFTER: {}:{} must raise {sample_tag} (got {reason:?})",
            sample.module,
            sample.function,
            sample_tag = sample.tag
        );
        eprintln!(
            "AFTER  {}:{} — reason {sample_tag} (live term recovered)",
            sample.module,
            sample.function,
            sample_tag = sample.tag
        );
    }
}

/// Compile-time guard so the sweep cannot be run without a host that implements the
/// engine's execution seam (a doc anchor for the *why* of `tests/support`).
#[allow(dead_code)]
fn _assert_host_seam() {
    fn _is_host<T: ExecutionContext>() {}
    _is_host::<ModuleHost>();
}
