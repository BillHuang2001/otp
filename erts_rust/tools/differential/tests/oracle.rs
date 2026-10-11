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

//! Oracle-backed integration tests for the differential harness.
//!
//! These assert the harness's *contract with reality*: the system ERTS on this machine
//! reproduces the corpus goldens, oracle-vs-oracle agrees, and a missing Rust runtime
//! degrades to `unsupported` instead of a fabricated pass.
//!
//! Everything here **skips loudly** (`eprintln!("SKIP: …")`, never a silent pass) when
//! the oracle cannot be resolved, so `cargo test` stays honest on a machine without
//! Erlang. Nothing here requires a functional `beam_rt`.

use differential::compare::{check_golden, compare_observations, SideResult, Verdict};
use differential::corpus::{find_case, CaseKind};
use differential::harness::{run_case, run_case_in, RuntimeContext};
use differential::module_build;
use differential::runtime::{
    eval_argv, own_env, resolve_oracle, resolve_rust, resolve_rust_from, run, run_with_env,
    RuntimeSpec,
};

mod common;

/// The canonical `{ok,42}` ETF probe line (the frozen A.2 golden).
const OK_42_GOLDEN: &str = "ok <<131,104,2,119,2,111,107,97,42>>";

/// Resolve the oracle, or print a loud skip notice and return `None`.
fn oracle_or_skip() -> Option<RuntimeSpec> {
    match resolve_oracle() {
        Ok(spec) => Some(spec),
        Err(error) => {
            eprintln!("SKIP: oracle erl not available ({error})");
            None
        }
    }
}

#[test]
fn oracle_reproduces_the_ok_42_golden() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let case = find_case("etf/term_to_binary_tuple").expect("built-in case");
    let argv = eval_argv(&case.eval(), case.extra_argv());
    let observation = run(&oracle, &argv).expect("spawn the oracle");

    assert_eq!(observation.exit_code, Some(0), "the probe halts with 0");
    assert_eq!(
        differential::compare::normalize_probe_stdout(&observation.stdout),
        OK_42_GOLDEN.as_bytes(),
        "the oracle must print the canonical ETF bytes of {{ok,42}}"
    );
    assert!(
        observation.stderr.is_empty(),
        "the oracle must not write to stderr: {:?}",
        String::from_utf8_lossy(&observation.stderr)
    );

    let golden = check_golden(&observation, &case.golden);
    assert!(golden.matched, "golden drift: {:?}", golden.diffs);

    // Observe the real Rust runtime through an explicit context so the freshly built debug
    // `beam_rt` is used, never the (stale-prone) `release`-first workspace default.
    let context = RuntimeContext {
        oracle,
        rust: common::fresh_rust_spec(),
    };
    let outcome = run_case_in(&context, case).expect("harness run");
    assert!(outcome.golden.matched, "{:?}", outcome.golden.diffs);
    assert_eq!(outcome.eval, case.eval());
}

#[test]
fn oracle_against_itself_agrees() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let case = find_case("etf/term_to_binary_tuple").expect("built-in case");
    let argv = eval_argv(&case.eval(), case.extra_argv());
    let first = run(&oracle, &argv).expect("first run");
    let second = run(&oracle, &argv).expect("second run");

    assert!(
        compare_observations(&first, &second).is_empty(),
        "the oracle is deterministic: {:?}",
        compare_observations(&first, &second)
    );

    // The same comparison through the real verdict path: oracle vs oracle ⇒ Agree.
    let golden = check_golden(&first, &case.golden);
    assert!(golden.matched, "{:?}", golden.diffs);
    let verdict = differential::compare::verdict(
        &SideResult::observed(first),
        &SideResult::observed(second),
        &golden,
    );
    assert_eq!(verdict, Verdict::Agree);
}

#[test]
fn a_rust_runtime_forced_to_a_nonexistent_path_is_a_loud_env_error() {
    // $BEAM_RT is an explicit override: if it is set but does not exist, the harness must
    // fail loudly (exit 2) rather than silently fall back to a default or a skip.
    let scratch = std::env::temp_dir();
    let error = resolve_rust_from(
        Some("/nonexistent/beam_rt-differential-test".into()),
        &scratch,
    )
    .expect_err("a nonexistent $BEAM_RT must not resolve");
    assert_eq!(error.exit_code(), 2);
    let message = error.to_string();
    assert!(message.contains("$BEAM_RT"), "{message}");
    assert!(message.contains("not an existing file"), "{message}");
}

#[test]
fn an_absent_rust_runtime_reports_unsupported_never_agreement() {
    // There is no default beam_rt in a tree that has only built this crate.
    let scratch = std::env::temp_dir()
        .join("differential-absent")
        .join("erts_rust/tools/differential");
    let absent = resolve_rust_from(None, &scratch).expect("resolution must not fail");
    assert!(
        absent.is_none(),
        "expected no workspace default under {}",
        scratch.display()
    );

    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let case = find_case("etf/term_to_binary_tuple").expect("built-in case");

    // The Rust side is exercised with an explicitly absent runtime: never `agree`.
    let context = RuntimeContext {
        oracle: oracle.clone(),
        rust: None,
    };
    let outcome = run_case_in(&context, case).expect("oracle-backed run");
    assert!(
        matches!(outcome.rust, SideResult::Unsupported { .. }),
        "a missing runtime is unsupported, got {:?}",
        outcome.rust
    );
    assert_ne!(outcome.verdict, Verdict::Agree);
    assert_eq!(outcome.verdict, Verdict::Unsupported);
    assert!(outcome.diffs.is_empty(), "nothing was observed to differ");
    assert_eq!(outcome.exit_code(), 0, "unsupported is not fatal by default");
    assert!(outcome.rust_spec.is_none());

    // And the same through the real environment, when it really has no Rust runtime.
    match resolve_rust() {
        Ok(None) => {
            let real = run_case(case).expect("harness run");
            assert!(matches!(real.rust, SideResult::Unsupported { .. }));
            assert_ne!(real.verdict, Verdict::Agree);
        }
        Ok(Some(spec)) => eprintln!(
            "SKIP: a Rust runtime exists at {} — the real-environment assertion is skipped",
            spec.program.display()
        ),
        Err(error) => panic!("$BEAM_RT is set but unusable: {error}"),
    }
}

#[test]
fn every_eval_corpus_case_matches_its_oracle_golden() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    let mut checked = 0;
    let mut checked_env = 0;
    for case in differential::corpus::cases() {
        // Module-run cases (including the environment-variable `ModuleEnv` ones, whose
        // environment embeds a `{dir}` placeholder that only exists after the compile) are
        // compiled and checked separately (see below).
        if matches!(
            case.kind,
            CaseKind::Module(_) | CaseKind::ModuleEnv { .. }
        ) {
            continue;
        }
        checked += 1;
        let argv = eval_argv(&case.eval(), case.extra_argv());
        // An environment-variable case (A.6) is spawned with its explicit environment, so
        // the `ERL_*FLAGS` merge is part of what the oracle must reproduce here too.
        if !case.env().is_empty() {
            checked_env += 1;
        }
        let observation = run_with_env(&oracle, &argv, &own_env(case.env()))
            .unwrap_or_else(|error| panic!("case {}: failed to run the oracle: {error}", case.id));
        let golden = check_golden(&observation, &case.golden);
        assert!(
            golden.matched,
            "case {} drifted from its golden: {:?} (stdout={:?})",
            case.id,
            golden.diffs,
            String::from_utf8_lossy(&observation.stdout)
        );
        assert_eq!(
            observation.exit_code,
            Some(case.golden.exit_code),
            "case {}: unexpected exit status",
            case.id
        );
    }
    assert!(checked > 0, "the corpus must contain expression cases");
    assert!(
        checked_env > 0,
        "the corpus must carry ERL_*FLAGS environment cases"
    );
}

/// Every module-run case must reproduce its golden under the oracle too: the embedded
/// module is compiled with the system `erlc` (and, for a NIF case, its `.so` is built next to
/// it with the host C compiler and `erl_nif.h`) and applied through `-pa <dir> -eval …`.
/// Skips loudly (never a silent pass) when `erlc` is unavailable, and reports each case whose
/// C/NIF fixture cannot be built as skipped rather than passing it.
#[test]
fn module_run_cases_reproduce_their_golden_under_the_oracle() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    match module_build::resolve_erlc() {
        Ok(Some(_)) => {}
        Ok(None) => {
            eprintln!("SKIP: erlc not available — cannot compile the module-run cases");
            return;
        }
        Err(error) => panic!("$ERLC is set but unusable: {error}"),
    };

    let mut checked = 0;
    let mut skipped = 0;
    for case in differential::corpus::cases() {
        let module_run = match &case.kind {
            CaseKind::Module(module_run) => module_run,
            // An environment-variable module-run case (`ModuleEnv`, contract A.6) is driven
            // like a plain module-run case, except its helper modules are compiled into the
            // same dir, the oracle argv carries **no** `-pa`, and both runtimes are spawned
            // with the case environment (its `{dir}` placeholder expanded).
            CaseKind::ModuleEnv { run, .. } => run,
            _ => continue,
        };
        checked += 1;
        let compiled = match module_build::build(module_run.source, module_run.module, module_run.nif)
        {
            Ok(module_build::Build::Compiled(compiled)) => compiled,
            Ok(module_build::Build::Skipped(reason)) => {
                // A host tool the case needs is missing: it was not run, so it can neither
                // pass nor fail here — say so loudly and move on.
                eprintln!("SKIP: case {} needs a missing host tool: {reason}", case.id);
                skipped += 1;
                continue;
            }
            Err(error) => panic!("case {}: building the module failed: {error}", case.id),
        };
        let (argv, env) = match &case.kind {
            CaseKind::ModuleEnv { extra, env, .. } => {
                for helper in *extra {
                    module_build::compile_extra_into(compiled.dir(), helper.source, helper.module)
                        .unwrap_or_else(|error| {
                            panic!(
                                "case {}: compiling helper {} failed: {error}",
                                case.id, helper.module
                            )
                        });
                }
                (
                    module_run.oracle_argv_env_only(),
                    differential::corpus::expand_env(env, compiled.dir()),
                )
            }
            _ => (module_run.oracle_argv(compiled.dir()), Vec::new()),
        };
        let observation = run_with_env(&oracle, &argv, &env).unwrap_or_else(|error| {
            panic!("case {}: failed to run the oracle: {error}", case.id)
        });
        let golden = check_golden(&observation, &case.golden);
        assert!(
            golden.matched,
            "case {} drifted from its golden: {:?} (stdout={:?})",
            case.id,
            golden.diffs,
            String::from_utf8_lossy(&observation.stdout)
        );
        assert_eq!(observation.exit_code, Some(0), "case {}", case.id);
    }
    assert!(
        checked >= 2,
        "the corpus must carry at least two module-run cases"
    );
    if skipped > 0 {
        eprintln!("NOTE: {skipped} module-run case(s) skipped for a missing host tool");
    }
}

#[test]
fn exception_cases_are_observable_through_the_probe() {
    let Some(oracle) = oracle_or_skip() else {
        return;
    };
    for (id, expected) in [
        ("errors/error_boom", "exception error boom"),
        ("errors/badarith", "exception error badarith"),
    ] {
        let case = find_case(id).expect("built-in case");
        let argv = eval_argv(&case.eval(), case.extra_argv());
        let observation = run(&oracle, &argv).expect("spawn the oracle");
        assert_eq!(
            differential::compare::normalize_probe_stdout(&observation.stdout),
            expected.as_bytes(),
            "case {id}: the probe must make the exception observable"
        );
        assert_eq!(observation.exit_code, Some(1), "case {id}: probe exit status");
        assert!(check_golden(&observation, &case.golden).matched);
    }
}

#[test]
fn an_eval_case_shares_one_argv_shape_for_both_runtimes() {
    let case = find_case("flags/custom_argument").expect("built-in case");
    let argv = eval_argv(&case.eval(), case.extra_argv());
    assert_eq!(
        argv,
        vec![
            "-noshell".to_string(),
            "-differential_test_flag".to_string(),
            "hello".to_string(),
            "-eval".to_string(),
            case.eval(),
        ],
        "the expression-case argv is built once and handed to both runtimes"
    );

    // A module-run case, by contrast, drives the two runtimes with different argv.
    let module_case = find_case("modules/add").expect("built-in case");
    let CaseKind::Module(module_run) = &module_case.kind else {
        panic!("modules/add must be a module-run case");
    };
    let dir = std::path::Path::new("/code");
    assert_eq!(module_run.oracle_argv(dir)[1..3], ["-pa", "/code"]);
    assert_eq!(module_run.rust_argv(dir)[0], "run");
}
