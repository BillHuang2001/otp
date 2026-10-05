//! Orchestration: run a case under both runtimes and turn the observations into a verdict.
//!
//! The runner is deliberately dumb and symmetrical: it resolves the two runtimes once,
//! builds **one** argv (the same for both — `beam_rt` is a drop-in `erl`, contract A.6),
//! observes the oracle, checks the oracle against the case golden, observes the Rust side
//! if it exists, and derives the verdict.
//!
//! * The **oracle** is mandatory: without it there is nothing to compare against, so a
//!   missing oracle is a loud [`Error::Env`] (exit 2) — never a fabricated result.
//! * The **Rust runtime** may legitimately be absent (`beam_rt` does not exist yet): the
//!   Rust side is then [`SideResult::Unsupported`] and the case can only ever be
//!   `unsupported`, never `agree`.
//! * A case whose *oracle* disagrees with its own golden is **corpus drift** and is a
//!   mismatch even when no Rust runtime exists — the corpus is self-testing.

use crate::compare::{
    check_golden, compare_observations, verdict, FieldDiff, GoldenCheck, SideResult, Verdict,
};
use crate::corpus::{self, Case, Contract, Golden, Probe};
use crate::error::Result;
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
    /// The probe expression that was handed to both runtimes.
    pub eval: String,
    /// The argv both runtimes were spawned with.
    pub argv: Vec<String>,
    /// The oracle side.
    pub oracle: SideResult,
    /// The Rust side.
    pub rust: SideResult,
    /// The oracle's check against the case golden (not asserted for ad-hoc comparisons).
    pub golden: GoldenCheck,
    /// Whether a golden was asserted at all (`false` for [`compare_expr`]).
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
    run_probe(
        context,
        ProbeRequest {
            case_id: case.id.to_string(),
            description: case.description.to_string(),
            contracts: case.contracts.to_vec(),
            expr: case.expr,
            extra_argv: case.extra_argv,
            pa: &[],
            probe: case.probe,
            golden: Some(case.golden),
        },
    )
}

/// Evaluate an ad-hoc expression under both runtimes and compare (no golden is asserted).
pub fn compare_expr(expr: &str, pa: &[String]) -> Result<CaseOutcome> {
    let context = resolve_context()?;
    run_probe(
        &context,
        ProbeRequest {
            case_id: "ad-hoc".to_string(),
            description: format!("ad-hoc comparison of `{expr}`"),
            contracts: Vec::new(),
            expr,
            extra_argv: &[],
            pa,
            probe: Probe::TermToBinary,
            golden: None,
        },
    )
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
    "no Rust runtime found: $BEAM_RT is unset and no built beam_rt exists under the workspace \
     target/{release,debug} directory (crates/beam_rt is a stub that cannot execute cases)"
        .to_string()
}

/// Everything one probe run needs (kept in a struct so the runner's signature stays readable).
struct ProbeRequest<'a> {
    case_id: String,
    description: String,
    contracts: Vec<Contract>,
    expr: &'a str,
    extra_argv: &'a [&'a str],
    pa: &'a [String],
    probe: Probe,
    golden: Option<Golden>,
}

/// The symmetrical runner: one argv, two observations, one verdict.
fn run_probe(context: &RuntimeContext, request: ProbeRequest<'_>) -> Result<CaseOutcome> {
    let eval = corpus::probe_expr(request.expr, request.probe);
    let argv = argv_for(&eval, request.extra_argv, request.pa);

    // The oracle comes first: it defines the reference observation and the golden check.
    let oracle_observation = runtime::run(&context.oracle, &argv)?;
    let golden_check = match &request.golden {
        Some(golden) => check_golden(&oracle_observation, golden),
        None => GoldenCheck::not_asserted(),
    };
    let oracle = SideResult::observed(oracle_observation);

    let rust = match &context.rust {
        Some(spec) => SideResult::observed(runtime::run(spec, &argv)?),
        None => SideResult::unsupported(rust_unsupported_reason()),
    };

    let diffs = match (&oracle, &rust) {
        (SideResult::Observed(oracle), SideResult::Observed(rust)) => {
            compare_observations(oracle, rust)
        }
        _ => Vec::new(),
    };
    let derived_verdict = verdict(&oracle, &rust, &golden_check);

    Ok(CaseOutcome {
        case_id: request.case_id,
        description: request.description,
        contracts: request.contracts,
        eval,
        argv,
        oracle,
        rust,
        golden: golden_check,
        golden_asserted: request.golden.is_some(),
        verdict: derived_verdict,
        diffs,
        oracle_spec: context.oracle.clone(),
        rust_spec: context.rust.clone(),
    })
}

/// Build the argv both runtimes get: any `-pa <dir>` for module cases, the case's own
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
    use super::{argv_for, rust_unsupported_reason};
    use crate::corpus::{cases, find_case, probe_expr, Probe};
    use crate::runtime::{RuntimeId, RuntimeSource, RuntimeSpec};
    use std::path::PathBuf;

    #[test]
    fn argv_is_identical_for_both_runtimes_and_carries_pa_and_flags() {
        let case = find_case("flags/custom_argument").expect("case");
        let eval = probe_expr(case.expr, case.probe);
        let pa = vec!["ebin".to_string(), "/other".to_string()];
        let argv = argv_for(&eval, case.extra_argv, &pa);
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
        for case in cases() {
            let eval = probe_expr(case.expr, case.probe);
            assert!(eval.ends_with("end."), "{}: probe not self-terminating", case.id);
            assert!(
                matches!(case.probe, Probe::TermToBinary | Probe::Value),
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
        assert!(reason.contains("stub"));
    }
}
