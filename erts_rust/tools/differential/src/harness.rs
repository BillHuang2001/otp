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

//! Orchestration: run a case under both runtimes and turn the observations into a verdict.
//!
//! The runner is deliberate and symmetrical. Per case it builds a *plan* — the argv each
//! runtime receives and which observables are compared — then observes the oracle, checks
//! the oracle against the case golden, observes the Rust side if it exists, and derives
//! the verdict.
//!
//! * Expression cases ([`CaseKind::Eval`]) hand **one** argv to both runtimes (`beam_rt` is
//!   a drop-in `erl` for them, contract A.6) and compare stdout + exit status + stderr.
//! * Environment-variable expression cases ([`CaseKind::EvalEnv`]) are the same, except both
//!   runtimes are additionally spawned with the case's explicit environment
//!   ([`runtime::run_with_env`]), so the `ERL_*FLAGS` merge is pinnable.
//! * Module-run cases ([`CaseKind::Module`]) hand **different** argv to the two runtimes
//!   (the oracle's `-pa` + `-eval` vs. the Rust `run` action) and compare stdout + exit
//!   status only (the Rust runtime's stderr boot banner is not a semantic difference).
//!   A case may carry a C/NIF **fixture** (contract A.3): a real `.so` (or a decoy file) is
//!   materialized next to the compiled `.beam`, so the module loads it through
//!   `code:which(?MODULE)` and the argv stays exactly the same.
//!
//! * The **oracle** is mandatory: without it there is nothing to compare against, so a
//!   missing oracle is a loud [`Error::Env`] (exit 2) — never a fabricated result.
//! * The **Rust runtime** may legitimately be absent (`beam_rt` not built): the Rust side
//!   is then [`SideResult::Unsupported`] and the case can only ever be `unsupported`,
//!   never `agree`.
//! * A Rust runtime that *runs* but exits with [`UNSUPPORTED_EXIT_CODE`] (`3`) has hit a
//!   documented **capability gap**; that side is classified `unsupported` (carrying its
//!   `beam_rt: unsupported: …` stderr line), **not** a differing observable result.
//! * A **probe-wrapped** case (an expression case) whose probe *caught* the runtime's
//!   documented capability-gap raise is classified `unsupported` too: the probe prints the
//!   caught raise as its single `exception <class> <reason>` stdout line and exits `1`, so
//!   the gap would otherwise masquerade as a differing result. The detection
//!   ([`caught_unsupported_reason`]) is deliberately narrow — only the runtime's documented
//!   `unsupported` reason convention can trigger it — so a genuine Erlang reason (`badarg`,
//!   `system_limit`, `'erlang:open_port/2 is not a BIF'`, …) stays an observation.
//! * A case whose *oracle* disagrees with its own golden is **corpus drift** and is a
//!   mismatch even when no Rust runtime exists — the corpus is self-testing.
//!
//! [`UNSUPPORTED_EXIT_CODE`]: crate::runtime::UNSUPPORTED_EXIT_CODE

use crate::compare::{
    check_golden, compare_observations_with, verdict_with, FieldDiff, GoldenCheck, Observables,
    SideResult, Verdict,
};
use crate::corpus::{self, Case, CaseKind, Contract, Golden, ModuleRun, Probe};
use crate::error::Result;
use crate::module_build;
use crate::runtime::{self, RuntimeSpec};

/// The two resolved runtimes a run observes (resolved once per invocation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeContext {
    /// The oracle runtime (always present — otherwise the harness cannot run at all).
    pub oracle: RuntimeSpec,
    /// The Rust runtime, when one exists.
    pub rust: Option<RuntimeSpec>,
}

/// The observed, compared result of one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseOutcome {
    /// The case id (`"ad-hoc"` for [`compare_expr`]).
    pub case_id: String,
    /// A one-line description of what was checked.
    pub description: String,
    /// The contracts the case covers.
    pub contracts: Vec<Contract>,
    /// The oracle's `-eval` expression (for a module case, the `io:format` that applies
    /// the function — the Rust side is driven through its `run` action instead).
    pub eval: String,
    /// The argv handed to the oracle.
    pub oracle_argv: Vec<String>,
    /// The argv handed to the Rust runtime (equal to [`Self::oracle_argv`] for expression
    /// cases; different for module-run cases). Empty when the case was skipped before it
    /// could run.
    pub rust_argv: Vec<String>,
    /// The oracle side.
    pub oracle: SideResult,
    /// The Rust side.
    pub rust: SideResult,
    /// The oracle's check against the case golden (not asserted for ad-hoc comparisons).
    pub golden: GoldenCheck,
    /// Whether a golden was actually checked (`false` for [`compare_expr`], and for a case
    /// that was skipped before it could run).
    pub golden_asserted: bool,
    /// The derived verdict.
    pub verdict: Verdict,
    /// The field-by-field differences between the two observed sides.
    pub diffs: Vec<FieldDiff>,
    /// The resolved oracle spec (recorded so the report is reproducible).
    pub oracle_spec: RuntimeSpec,
    /// The resolved Rust spec, when one exists.
    pub rust_spec: Option<RuntimeSpec>,
}

impl CaseOutcome {
    /// The process exit code this single outcome implies (`--strict` is a suite concept).
    pub fn exit_code(&self) -> i32 {
        self.verdict.exit_contribution(false)
    }
}

/// The result of running the whole corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteOutcome {
    /// One outcome per corpus case, in corpus order.
    pub cases: Vec<CaseOutcome>,
    /// The resolved runtimes.
    pub context: RuntimeContext,
    /// Whether `--strict` was requested.
    pub strict: bool,
    /// The process exit code: `1` on any mismatch (or, under `--strict`, any
    /// unsupported/skipped case); `0` otherwise.
    pub exit_code: i32,
}

/// Resolve both runtimes once (oracle mandatory, Rust optional).
pub fn resolve_context() -> Result<RuntimeContext> {
    Ok(RuntimeContext {
        oracle: runtime::resolve_oracle()?,
        rust: runtime::resolve_rust()?,
    })
}

/// Run one corpus case under both runtimes and compare the observations.
pub fn run_case(case: &Case) -> Result<CaseOutcome> {
    let context = resolve_context()?;
    run_case_in(&context, case)
}

/// [`run_case`] with an already resolved [`RuntimeContext`].
pub fn run_case_in(context: &RuntimeContext, case: &Case) -> Result<CaseOutcome> {
    match &case.kind {
        CaseKind::Eval {
            expr,
            extra_argv,
            probe,
        } => {
            let eval = corpus::probe_expr(expr, *probe);
            let argv = argv_for(&eval, extra_argv, &[]);
            let plan = RunPlan {
                case_id: case.id.to_string(),
                description: case.description.to_string(),
                contracts: case.contracts.to_vec(),
                eval,
                oracle_argv: argv.clone(),
                rust_argv: argv,
                observables: Observables::All,
                golden: Some(case.golden),
                probe_wrapped: true,
                env: &[],
            };
            execute(context, plan, None)
        }
        CaseKind::EvalEnv {
            expr,
            extra_argv,
            env,
            probe,
        } => {
            // An environment-variable expression case: the argv is the same drop-in `erl`
            // shape as `CaseKind::Eval`, but both runtimes are additionally spawned with the
            // case's explicit environment, so the `ERL_*FLAGS` merge is pinned end to end.
            let eval = corpus::probe_expr(expr, *probe);
            let argv = argv_for(&eval, extra_argv, &[]);
            let plan = RunPlan {
                case_id: case.id.to_string(),
                description: case.description.to_string(),
                contracts: case.contracts.to_vec(),
                eval,
                oracle_argv: argv.clone(),
                rust_argv: argv,
                observables: Observables::All,
                golden: Some(case.golden),
                probe_wrapped: true,
                env: *env,
            };
            execute(context, plan, None)
        }
        CaseKind::Module(run) => run_module_case(context, case, run),
    }
}

/// Evaluate an ad-hoc expression under both runtimes and compare (no golden is asserted).
pub fn compare_expr(expr: &str, pa: &[String]) -> Result<CaseOutcome> {
    let context = resolve_context()?;
    let eval = corpus::probe_expr(expr, Probe::TermToBinary);
    let argv = argv_for(&eval, &[], pa);
    let plan = RunPlan {
        case_id: "ad-hoc".to_string(),
        description: format!("ad-hoc comparison of `{expr}`"),
        contracts: Vec::new(),
        eval,
        oracle_argv: argv.clone(),
        rust_argv: argv,
        observables: Observables::All,
        golden: None,
        probe_wrapped: true,
        env: &[],
    };
    execute(&context, plan, None)
}

/// Run the whole built-in corpus.
pub fn run_suite(strict: bool) -> Result<SuiteOutcome> {
    let context = resolve_context()?;
    let mut cases = Vec::with_capacity(corpus::cases().len());
    for case in corpus::cases() {
        cases.push(run_case_in(&context, case)?);
    }
    let exit_code = cases
        .iter()
        .map(|outcome| outcome.verdict.exit_contribution(strict))
        .max()
        .unwrap_or(0);
    Ok(SuiteOutcome {
        cases,
        context,
        strict,
        exit_code,
    })
}

/// The reason recorded for the Rust side when no `beam_rt` binary could be found.
pub fn rust_unsupported_reason() -> String {
    "no Rust runtime found: $BEAM_RT is unset and no built beam_rt exists under the \
     workspace target/{release,debug} directory"
        .to_string()
}

/// Run one planned case and derive its outcome.
///
/// `_module_guard` (a [`module_build::CompiledModule`]) owns the compiled module's
/// temporary directory and must stay alive for the whole call, so it is dropped only
/// after both runtimes have been observed.
fn execute(
    context: &RuntimeContext,
    plan: RunPlan,
    _module_guard: Option<module_build::CompiledModule>,
) -> Result<CaseOutcome> {
    let oracle_observation = runtime::run_with_env(&context.oracle, &plan.oracle_argv, plan.env)?;
    let golden_check = match &plan.golden {
        Some(golden) => check_golden(&oracle_observation, golden),
        None => GoldenCheck::not_asserted(),
    };
    let oracle = SideResult::observed(oracle_observation);

    let rust = match &context.rust {
        Some(spec) => classify_rust(
            runtime::run_with_env(spec, &plan.rust_argv, plan.env)?,
            plan.probe_wrapped,
        ),
        None => SideResult::unsupported(rust_unsupported_reason()),
    };

    let diffs = match (&oracle, &rust) {
        (SideResult::Observed(oracle), SideResult::Observed(rust)) => {
            compare_observations_with(oracle, rust, plan.observables)
        }
        _ => Vec::new(),
    };
    let derived_verdict = verdict_with(&oracle, &rust, &golden_check, plan.observables);

    Ok(CaseOutcome {
        case_id: plan.case_id,
        description: plan.description,
        contracts: plan.contracts,
        eval: plan.eval,
        oracle_argv: plan.oracle_argv,
        rust_argv: plan.rust_argv,
        oracle,
        rust,
        golden: golden_check,
        golden_asserted: plan.golden.is_some(),
        verdict: derived_verdict,
        diffs,
        oracle_spec: context.oracle.clone(),
        rust_spec: context.rust.clone(),
    })
}

/// Run a module-run case: build the embedded module (plus any C/NIF fixture), then compare
/// the two runtimes.
///
/// The compile is memoized process-locally by `(erlc, module, source)` (see
/// [`module_build::compile`]), so the corpus's handful of embedded sources is compiled once
/// even though ~50 module-run cases each ask for it. Each case still gets its own temp dir
/// (owning it through [`module_build::CompiledModule`]), so the per-case argv is unchanged.
/// A case that carries a [`module_build::NifFixture`] (contract A.3) has its `.so` written
/// into that same temp dir, where the module finds it through `code:which(?MODULE)` — neither
/// runtime's argv changes.
///
/// A missing host tool (`erlc`, and `cc`/`erl_nif.h` for a C-compiled fixture) is a loud
/// [`Verdict::Skipped`] with the resolver's reason: the case is never run, so it can never be
/// reported as agreement.
fn run_module_case(context: &RuntimeContext, case: &Case, run: &ModuleRun) -> Result<CaseOutcome> {
    let compiled = match module_build::build(run.source, run.module, run.nif)? {
        module_build::Build::Compiled(compiled) => compiled,
        module_build::Build::Skipped(reason) => {
            return Ok(skipped_module_outcome(context, case, run, reason));
        }
    };
    let plan = RunPlan {
        case_id: case.id.to_string(),
        description: case.description.to_string(),
        contracts: case.contracts.to_vec(),
        eval: run.oracle_eval(),
        oracle_argv: run.oracle_argv(compiled.dir()),
        rust_argv: run.rust_argv(compiled.dir()),
        observables: Observables::StdoutAndExit,
        golden: Some(case.golden),
        probe_wrapped: false,
        env: &[],
    };
    execute(context, plan, Some(compiled))
}

/// The outcome of a module-run case that could not be built because a host tool is missing.
///
/// Nothing was observed, so this is a loud [`Verdict::Skipped`] carrying `reason` — never a
/// pass and never agreement. It is non-fatal by default and fatal under `--strict`.
fn skipped_module_outcome(
    context: &RuntimeContext,
    case: &Case,
    run: &ModuleRun,
    reason: &'static str,
) -> CaseOutcome {
    let side = SideResult::unsupported(reason);
    CaseOutcome {
        case_id: case.id.to_string(),
        description: case.description.to_string(),
        contracts: case.contracts.to_vec(),
        eval: run.oracle_eval(),
        oracle_argv: Vec::new(),
        rust_argv: Vec::new(),
        oracle: side.clone(),
        rust: side,
        golden: GoldenCheck::not_asserted(),
        golden_asserted: false,
        verdict: Verdict::Skipped,
        diffs: Vec::new(),
        oracle_spec: context.oracle.clone(),
        rust_spec: context.rust.clone(),
    }
}

/// The prefix every documented capability-gap reason atom starts with — the runtime's
/// convention (mirrors `crates/beam_rt/src/error.rs`'s `UNSUPPORTED_PREFIX`).
const UNSUPPORTED_REASON_PREFIX: &str = "unsupported";

/// The probe protocol's failure-line prefix (`exception <class> <reason>`).
const PROBE_EXCEPTION_PREFIX: &str = "exception ";

/// Classify a Rust-runtime observation.
///
/// A **capability gap** is reported to the harness two ways and both mean
/// [`SideResult::Unsupported`], never a differing observable result:
///
/// * the runtime exits with the documented [`runtime::UNSUPPORTED_EXIT_CODE`] (`3`) — the
///   signal for a raise that escaped the program (e.g. a malformed `-eval`); the side then
///   carries the runtime's `beam_rt: unsupported: …` stderr line; or
/// * a **probe-wrapped** case whose probe *caught* the raise: the probe prints it as its
///   single `exception <class> <reason>` stdout line and exits `1`, and `<reason>` follows
///   the runtime's documented `unsupported` convention ([`caught_unsupported_reason`]).
///
/// `probe_wrapped` must be `true` only for the expression cases ([`CaseKind::Eval`],
/// [`CaseKind::EvalEnv`] and [`compare_expr`]), whose stdout is the probe's self-terminating
/// line; the module-run cases print `~w` of a result (a different shape), so the
/// caught-signal detection is deliberately disabled for them. Every other exit status is a
/// genuine observation.
fn classify_rust(observation: runtime::Observation, probe_wrapped: bool) -> SideResult {
    if observation.exit_code == Some(runtime::UNSUPPORTED_EXIT_CODE) {
        return SideResult::unsupported(unsupported_reason(&observation));
    }
    if probe_wrapped {
        if let Some(reason) = caught_unsupported_reason(&observation) {
            return SideResult::unsupported(format!(
                "beam_rt: unsupported (caught by the probe): {reason}"
            ));
        }
    }
    SideResult::observed(observation)
}

/// The documented capability-gap reason when the **probe caught** the runtime's raise.
///
/// An expression case wraps the expression in
/// `try (EXPR) of V -> io:format("ok ~w~n", [V]), halt(0) catch C:R -> io:format("exception ~w ~w~n", [C, R]), halt(1) end.`
/// so a raise inside the expression is *caught* and printed as exactly one stdout line
/// `exception <class> <reason>` with exit status `1` — it never reaches the CLI's exit-3
/// signal. This recognises the narrow, documented slice of that protocol that is a
/// capability gap and returns the reason in the same stripped form the runtime uses:
///
/// * the exit status is `1`;
/// * stdout is **exactly one** line, of the shape `exception <class> <reason>` with
///   `<class>` one of `error`/`exit`/`throw`;
/// * `<reason>`, after stripping one pair of surrounding `'…'` quotes, starts with the
///   runtime's `unsupported` prefix and the remainder is empty or begins with `:`, `_` or
///   whitespace; the returned reason is that remainder with the leading `:`/`_`/whitespace
///   stripped — exactly `crates/beam_rt/src/error.rs::unsupported_reason`'s rule.
///
/// Everything else — a genuine Erlang reason such as `badarg`, `system_limit`,
/// `function_clause`, or a free-form message like `'erlang:open_port/2 is not a BIF'` — is
/// `None`. The detection is narrow by construction: it can only ever fire on the documented
/// convention, so it can never fabricate a capability gap out of a real divergence.
fn caught_unsupported_reason(observation: &runtime::Observation) -> Option<String> {
    if observation.exit_code != Some(1) {
        return None;
    }
    let stdout = std::str::from_utf8(&observation.stdout).ok()?;
    // The probe's `~n` is mandatory, and the line must be the *only* one.
    let line = stdout.strip_suffix('\n')?;
    if line.contains('\n') || line.contains('\r') {
        return None;
    }
    let rest = line.strip_prefix(PROBE_EXCEPTION_PREFIX)?;
    let (class, reason) = rest.split_once(' ')?;
    if !matches!(class, "error" | "exit" | "throw") {
        return None;
    }
    let reason = strip_surrounding_quotes(reason);
    let after_prefix = reason.strip_prefix(UNSUPPORTED_REASON_PREFIX)?;
    let begins_with_marker = after_prefix.is_empty()
        || after_prefix.starts_with(':')
        || after_prefix.starts_with('_')
        || after_prefix.starts_with(char::is_whitespace);
    if !begins_with_marker {
        return None;
    }
    Some(
        after_prefix
            .trim_start_matches(|c: char| c == ':' || c == '_' || c.is_whitespace())
            .to_string(),
    )
}

/// `s` with one pair of surrounding single quotes removed, if it has them.
///
/// The probe renders the reason with `~w`, so an atom that needs quoting arrives as
/// `'unsupported_opcode: …'`; a bare atom (`badarg`) has no quotes to strip.
fn strip_surrounding_quotes(s: &str) -> &str {
    match s.strip_prefix('\'').and_then(|inner| inner.strip_suffix('\'')) {
        Some(inner) => inner,
        None => s,
    }
}

/// The reason recorded for an exit-3 observation: its first non-empty stderr line, with
/// trailing whitespace trimmed (the runtime prints `beam_rt: unsupported: <reason>`).
fn unsupported_reason(observation: &runtime::Observation) -> String {
    let stderr = String::from_utf8_lossy(&observation.stderr);
    for line in stderr.lines() {
        if !line.trim().is_empty() {
            return line.trim_end().to_string();
        }
    }
    format!(
        "the Rust runtime exited {} (unsupported) with no diagnostic on stderr",
        runtime::UNSUPPORTED_EXIT_CODE
    )
}

/// Everything one planned run needs (kept in a struct so the runner's signature stays
/// readable).
struct RunPlan {
    case_id: String,
    description: String,
    contracts: Vec<Contract>,
    eval: String,
    oracle_argv: Vec<String>,
    rust_argv: Vec<String>,
    observables: Observables,
    golden: Option<Golden>,
    /// Whether the Rust side's stdout is the self-terminating **probe** line
    /// (`exception <class> <reason>` on failure), i.e. an expression case. Enables the
    /// caught-capability-gap classification ([`classify_rust`]); `false` for module-run
    /// cases, whose stdout is `~w` of a result.
    probe_wrapped: bool,
    /// The explicit environment both runtimes are spawned with (the `ERL_*FLAGS` merge;
    /// `&[]` for every case that does not pin it). See
    /// [`CaseKind::EvalEnv`](crate::corpus::CaseKind::EvalEnv).
    env: &'static [(&'static str, &'static str)],
}

/// Build the argv an expression case gets: any `-pa <dir>` code paths, the case's own
/// extra argv, then the standard `["-noshell", … , "-eval", <probe>]` shape.
fn argv_for(eval: &str, extra_argv: &[&str], pa: &[String]) -> Vec<String> {
    let mut extra: Vec<String> = Vec::with_capacity(extra_argv.len() + pa.len() * 2);
    for dir in pa {
        extra.push("-pa".to_string());
        extra.push(dir.clone());
    }
    extra.extend(extra_argv.iter().map(|arg| (*arg).to_string()));
    runtime::eval_argv_owned(eval, &extra)
}

#[cfg(test)]
mod tests {
    use super::{
        argv_for, caught_unsupported_reason, classify_rust, rust_unsupported_reason,
        unsupported_reason,
    };
    use crate::compare::SideResult;
    use crate::corpus::{cases, find_case, probe_expr, CaseKind, Probe};
    use crate::runtime::{Observation, RuntimeId, RuntimeSource, RuntimeSpec, UNSUPPORTED_EXIT_CODE};
    use std::path::PathBuf;

    fn observation(exit_code: i32, stdout: &[u8], stderr: &[u8]) -> Observation {
        Observation {
            runtime: RuntimeId::Rust,
            program: PathBuf::from("/opt/beam_rt"),
            argv: vec!["run".to_string()],
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            exit_code: Some(exit_code),
        }
    }

    #[test]
    fn argv_is_identical_for_both_runtimes_and_carries_pa_and_flags() {
        let case = find_case("flags/custom_argument").expect("case");
        let CaseKind::Eval {
            expr,
            extra_argv,
            probe,
        } = case.kind
        else {
            panic!("flags/custom_argument must be an eval case");
        };
        let eval = probe_expr(expr, probe);
        let pa = vec!["ebin".to_string(), "/other".to_string()];
        let argv = argv_for(&eval, extra_argv, &pa);
        assert_eq!(argv[0], "-noshell");
        assert_eq!(
            &argv[1..5],
            &["-pa", "ebin", "-pa", "/other"],
            "code paths come first"
        );
        assert_eq!(
            &argv[5..7],
            &["-differential_test_flag", "hello"],
            "the case's extra argv follows"
        );
        assert_eq!(argv[7], "-eval");
        assert_eq!(argv[8], eval);

        // The same argv is handed to both runtimes: only the program differs.
        let oracle = RuntimeSpec {
            id: RuntimeId::Oracle,
            program: PathBuf::from("/opt/erl"),
            source: RuntimeSource::Path,
        };
        let rust = RuntimeSpec {
            id: RuntimeId::Rust,
            program: PathBuf::from("/opt/beam_rt"),
            source: RuntimeSource::Env("$BEAM_RT"),
        };
        assert_eq!(oracle.id, RuntimeId::Oracle);
        assert_ne!(render(&oracle), render(&rust));
    }

    #[test]
    fn every_eval_case_probe_is_self_terminating() {
        for case in cases() {
            let CaseKind::Eval { expr, probe, .. } = case.kind else {
                continue;
            };
            let eval = probe_expr(expr, probe);
            assert!(
                eval.ends_with("end."),
                "{}: probe not self-terminating",
                case.id
            );
            assert!(
                matches!(probe, Probe::TermToBinary | Probe::Value),
                "{}",
                case.id
            );
        }
    }

    fn render(spec: &RuntimeSpec) -> String {
        format!("{}:{}", spec.id.id(), spec.program.display())
    }

    #[test]
    fn the_unsupported_reason_is_explicit_about_the_missing_runtime() {
        let reason = rust_unsupported_reason();
        assert!(reason.contains("$BEAM_RT"));
        assert!(reason.contains("beam_rt"));
        assert!(reason.contains("no Rust runtime found"));
    }

    #[test]
    fn the_capability_gap_signal_is_classified_unsupported_not_observed() {
        let exit_three = observation(
            UNSUPPORTED_EXIT_CODE,
            b"",
            b"beam_rt: unsupported: cannot evaluate \"length([a,b,c])\"\n",
        );
        // The exit-3 signal is honoured regardless of the case kind.
        for probe_wrapped in [false, true] {
            let side = classify_rust(exit_three.clone(), probe_wrapped);
            assert!(!side.is_observed(), "an exit-3 observation is not observed");
            assert_eq!(
                side.unsupported_reason(),
                Some("beam_rt: unsupported: cannot evaluate \"length([a,b,c])\""),
                "the reason carries the runtime's stderr line"
            );
        }
    }

    #[test]
    fn any_other_exit_status_is_a_genuine_observation() {
        for (exit_code, stdout) in [(0, b"5\n".as_slice()), (1, b"".as_slice())] {
            let side = classify_rust(observation(exit_code, stdout, b""), true);
            assert!(side.is_observed(), "exit {exit_code} is a real observation");
            assert_eq!(side.observation().expect("observed").exit_code, Some(exit_code));
        }
    }

    #[test]
    fn the_unsupported_reason_uses_the_first_non_empty_stderr_line() {
        // A leading blank line is skipped; trailing whitespace and later lines are dropped.
        let padded = observation(
            UNSUPPORTED_EXIT_CODE,
            b"",
            b"\nbeam_rt: unsupported: unsupported_bif   \nsecond line\n",
        );
        assert_eq!(
            unsupported_reason(&padded),
            "beam_rt: unsupported: unsupported_bif"
        );
        let silent = observation(UNSUPPORTED_EXIT_CODE, b"", b"");
        assert!(unsupported_reason(&silent).contains("no diagnostic on stderr"));
    }

    #[test]
    fn an_unsupported_classification_is_never_agreement() {
        // The unit-level guarantee behind the harness fix: an `Unsupported` side (either the
        // exit-3 signal or a probe-caught capability gap) can only be `Unsupported` in the
        // comparison, never `Agree`.
        let exit_three = classify_rust(observation(UNSUPPORTED_EXIT_CODE, b"", b"x\n"), false);
        assert!(matches!(exit_three, SideResult::Unsupported { .. }));

        let caught = classify_rust(
            observation(1, b"exception error 'unsupported_opcode: nope'\n", b""),
            true,
        );
        assert!(matches!(caught, SideResult::Unsupported { .. }));
        assert!(!caught.is_observed());
    }

    /// The probe *caught* the runtime's documented capability-gap raise: the reason is
    /// stripped exactly as the runtime's `unsupported_reason` does, and the side becomes
    /// `Unsupported` — never a `mismatch`.
    #[test]
    fn a_probe_caught_capability_gap_is_classified_unsupported() {
        // The engine's `unsupported_opcode: <what>` reason, quoted by the `~w` of the probe.
        let side = classify_rust(
            observation(
                1,
                b"exception error 'unsupported_opcode: widened opcode not yet executed'\n",
                b"",
            ),
            true,
        );
        assert!(!side.is_observed(), "a capability gap is not an observation");
        assert_eq!(
            side.unsupported_reason(),
            Some("beam_rt: unsupported (caught by the probe): opcode: widened opcode not yet executed"),
            "the reason is stripped by the runtime's rule"
        );

        // The BIF registry's bare `unsupported_bif` atom (no surrounding quotes).
        let side = classify_rust(
            observation(1, b"exception error unsupported_bif\n", b""),
            true,
        );
        assert_eq!(
            side.unsupported_reason(),
            Some("beam_rt: unsupported (caught by the probe): bif")
        );
    }

    /// NARROWNESS IS THE CONTRACT: a genuine Erlang reason must stay an observation, so a
    /// real divergence is never laundered into a capability gap.
    #[test]
    fn a_genuine_caught_reason_is_still_an_observation() {
        for (stdout, what) in [
            (b"exception error badarg\n".as_slice(), "badarg"),
            (b"exception throw noproc\n".as_slice(), "a throw reason"),
            (b"exception exit normal\n".as_slice(), "an exit reason"),
            (b"exception error system_limit\n".as_slice(), "system_limit"),
            (
                b"exception error 'erlang:open_port/2 is not a BIF'\n".as_slice(),
                "a free-form diagnostic",
            ),
            (
                b"exception error 'cannot find init.beam in the code path'\n".as_slice(),
                "a boot diagnostic",
            ),
            // A reason that merely *contains* the prefix is not the convention.
            (
                b"exception error 'not an unsupported thing'\n".as_slice(),
                "the prefix in the middle",
            ),
        ] {
            let side = classify_rust(observation(1, stdout, b""), true);
            assert!(side.is_observed(), "{what} must stay an observation");
            assert_eq!(side.unsupported_reason(), None, "{what}");
        }
    }

    /// The detection is shape-strict: a two-line stdout, a non-probe stdout, a bad class, a
    /// missing reason and a non-`1` exit are all observations.
    #[test]
    fn a_malformed_probe_line_is_not_a_capability_gap() {
        for (exit_code, stdout, what) in [
            (
                1,
                b"ok 1\nexception error 'unsupported_opcode: x'\n".as_slice(),
                "a two-line stdout",
            ),
            (1, b"hello\n".as_slice(), "a non-probe stdout"),
            (1, b"exception warning 'unsupported_opcode: x'\n".as_slice(), "a non-probe class"),
            (1, b"exception error\n".as_slice(), "a missing reason"),
            (0, b"exception error 'unsupported_opcode: x'\n".as_slice(), "a success exit"),
            (1, b"exception error 'unsupported_opcode: x'".as_slice(), "no trailing newline"),
        ] {
            assert_eq!(
                caught_unsupported_reason(&observation(exit_code, stdout, b"")),
                None,
                "{what} is not the probe failure protocol"
            );
            let side = classify_rust(observation(exit_code, stdout, b""), true);
            assert!(side.is_observed(), "{what} stays an observation");
        }
    }

    /// The caught-signal detection only fires for **probe-wrapped** cases; a module-run case
    /// prints `~w` of a result, so a same-shaped stdout there is an ordinary observation.
    #[test]
    fn the_caught_signal_only_fires_for_probe_wrapped_cases() {
        let observation = observation(1, b"exception error 'unsupported_opcode: x'\n", b"");
        assert!(
            matches!(
                classify_rust(observation.clone(), true),
                SideResult::Unsupported { .. }
            ),
            "a probe-wrapped case is classified"
        );
        assert!(
            classify_rust(observation, false).is_observed(),
            "a module-run case is never classified by the probe shape"
        );
    }
}
