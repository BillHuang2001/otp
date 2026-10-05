//! Orchestration: run a case under both runtimes and turn the observations into a verdict.
//!
//! The runner is deliberate and symmetrical. Per case it builds a *plan* — the argv each
//! runtime receives and which observables are compared — then observes the oracle, checks
//! the oracle against the case golden, observes the Rust side if it exists, and derives
//! the verdict.
//!
//! * Expression cases ([`CaseKind::Eval`]) hand **one** argv to both runtimes (`beam_rt` is
//!   a drop-in `erl` for them, contract A.6) and compare stdout + exit status + stderr.
//! * Module-run cases ([`CaseKind::Module`]) hand **different** argv to the two runtimes
//!   (the oracle's `-pa` + `-eval` vs. the Rust `run` action) and compare stdout + exit
//!   status only (the Rust runtime's stderr boot banner is not a semantic difference).
//!
//! * The **oracle** is mandatory: without it there is nothing to compare against, so a
//!   missing oracle is a loud [`Error::Env`] (exit 2) — never a fabricated result.
//! * The **Rust runtime** may legitimately be absent (`beam_rt` not built): the Rust side
//!   is then [`SideResult::Unsupported`] and the case can only ever be `unsupported`,
//!   never `agree`.
//! * A Rust runtime that *runs* but exits with [`UNSUPPORTED_EXIT_CODE`] (`3`) has hit a
//!   documented **capability gap**; that side is classified `unsupported` (carrying its
//!   `beam_rt: unsupported: …` stderr line), **not** a differing observable result.
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
    let oracle_observation = runtime::run(&context.oracle, &plan.oracle_argv)?;
    let golden_check = match &plan.golden {
        Some(golden) => check_golden(&oracle_observation, golden),
        None => GoldenCheck::not_asserted(),
    };
    let oracle = SideResult::observed(oracle_observation);

    let rust = match &context.rust {
        Some(spec) => classify_rust(runtime::run(spec, &plan.rust_argv)?),
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

/// Run a module-run case: compile the embedded module, then compare the two runtimes.
fn run_module_case(context: &RuntimeContext, case: &Case, run: &ModuleRun) -> Result<CaseOutcome> {
    let Some(erlc) = module_build::resolve_erlc()? else {
        return Ok(skipped_module_outcome(context, case, run));
    };
    let compiled = module_build::compile(&erlc, run.source, run.module)?;
    let plan = RunPlan {
        case_id: case.id.to_string(),
        description: case.description.to_string(),
        contracts: case.contracts.to_vec(),
        eval: run.oracle_eval(),
        oracle_argv: run.oracle_argv(compiled.dir()),
        rust_argv: run.rust_argv(compiled.dir()),
        observables: Observables::StdoutAndExit,
        golden: Some(case.golden),
    };
    execute(context, plan, Some(compiled))
}

/// The outcome of a module-run case that could not be compiled (no `erlc` on the machine).
///
/// Nothing was observed, so this is a loud [`Verdict::Skipped`] — never a pass and never
/// agreement. It is non-fatal by default and fatal under `--strict`.
fn skipped_module_outcome(context: &RuntimeContext, case: &Case, run: &ModuleRun) -> CaseOutcome {
    let side = SideResult::unsupported(module_build::MISSING_ERLC_REASON);
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

/// Classify a Rust-runtime observation.
///
/// The documented exit-3 signal ([`runtime::UNSUPPORTED_EXIT_CODE`]) means the runtime hit
/// a **capability gap**, so the side is recorded as [`SideResult::Unsupported`] carrying its
/// stderr diagnostic — **not** as an observation that would diff against the oracle (which
/// would wrongly be a `mismatch`). Every other exit status is a genuine observation.
fn classify_rust(observation: runtime::Observation) -> SideResult {
    if observation.exit_code == Some(runtime::UNSUPPORTED_EXIT_CODE) {
        SideResult::unsupported(unsupported_reason(&observation))
    } else {
        SideResult::observed(observation)
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
    use super::{argv_for, classify_rust, rust_unsupported_reason, unsupported_reason};
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
        let side = classify_rust(exit_three);
        assert!(!side.is_observed(), "an exit-3 observation is not observed");
        assert_eq!(
            side.unsupported_reason(),
            Some("beam_rt: unsupported: cannot evaluate \"length([a,b,c])\""),
            "the reason carries the runtime's stderr line"
        );
    }

    #[test]
    fn any_other_exit_status_is_a_genuine_observation() {
        for (exit_code, stdout) in [(0, b"5\n".as_slice()), (1, b"".as_slice())] {
            let side = classify_rust(observation(exit_code, stdout, b""));
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
        // The unit-level guarantee behind the harness fix: an exit-3 side can only be
        // `Unsupported` in the comparison, never `Agree`.
        let side = classify_rust(observation(UNSUPPORTED_EXIT_CODE, b"", b"x\n"));
        assert!(matches!(side, SideResult::Unsupported { .. }));
    }
}
