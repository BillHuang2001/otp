//! Comparing observable results: byte-level diffs, golden checks and the verdict.
//!
//! The harness compares only what a black-box observer can see (contract A.2/A.8):
//! the **exact stdout bytes** and the **exit status** of the two runtimes, plus the case's
//! own **golden** (an independent expectation that makes the corpus self-testing — a
//! golden miss by the *oracle* is corpus drift, i.e. a real failure, never a skip).
//!
//! Stderr is compared too for the expression cases ([`Observables::All`]), where both
//! runtimes are expected to be silent. It is deliberately **not** compared for the
//! module-run cases ([`Observables::StdoutAndExit`]): the Rust runtime prints a boot
//! banner to stderr while `erl` prints nothing, and that is not a semantic difference —
//! only stdout and the exit status are.

use crate::corpus::Golden;
use crate::runtime::Observation;

/// What one runtime contributed to a comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideResult {
    /// The runtime ran and produced an observation.
    Observed(Observation),
    /// The runtime could not be run (e.g. no `beam_rt` binary exists). This is reported
    /// as `unsupported` and is **never** turned into agreement or a pass.
    Unsupported {
        /// Why the side could not be observed.
        reason: String,
    },
}

impl SideResult {
    /// Wrap an observation.
    pub fn observed(observation: Observation) -> SideResult {
        SideResult::Observed(observation)
    }

    /// Record that this side could not run.
    pub fn unsupported(reason: impl Into<String>) -> SideResult {
        SideResult::Unsupported {
            reason: reason.into(),
        }
    }

    /// Whether this side produced an observation.
    pub fn is_observed(&self) -> bool {
        matches!(self, SideResult::Observed(_))
    }

    /// The observation, when this side ran.
    pub fn observation(&self) -> Option<&Observation> {
        match self {
            SideResult::Observed(observation) => Some(observation),
            SideResult::Unsupported { .. } => None,
        }
    }

    /// The unsupported reason, when this side could not run.
    pub fn unsupported_reason(&self) -> Option<&str> {
        match self {
            SideResult::Observed(_) => None,
            SideResult::Unsupported { reason } => Some(reason),
        }
    }
}

/// One field-by-field difference between two sides.
///
/// For [`compare_observations`] `oracle`/`rust` are the two observed renderings; for
/// [`check_golden`] `oracle` is the observed rendering and `rust` is the expected one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FieldDiff {
    /// The observable that differs: `"stdout"`, `"stderr"` or `"exit_code"`.
    pub field: String,
    /// The oracle-side (or observed) rendering.
    pub oracle: String,
    /// The Rust-side (or expected) rendering.
    pub rust: String,
}

impl FieldDiff {
    /// Build a diff of one observable.
    pub fn new(field: impl Into<String>, oracle: impl Into<String>, rust: impl Into<String>) -> FieldDiff {
        FieldDiff {
            field: field.into(),
            oracle: oracle.into(),
            rust: rust.into(),
        }
    }
}

/// The result of checking an observation against a case's golden expectation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldenCheck {
    /// `true` when the observation reproduced every asserted golden field.
    pub matched: bool,
    /// One entry per golden field the observation did not reproduce.
    pub diffs: Vec<FieldDiff>,
}

impl GoldenCheck {
    /// A check that asserted nothing (used for ad-hoc comparisons without a golden).
    pub fn not_asserted() -> GoldenCheck {
        GoldenCheck {
            matched: true,
            diffs: Vec::new(),
        }
    }
}

/// The outcome of comparing the two sides of one case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Both sides ran and every observable (and the golden) agreed.
    Agree,
    /// The sides disagreed, or the oracle disagreed with its own golden (corpus drift).
    Mismatch,
    /// At least one side could not run; nothing was proven either way.
    Unsupported,
    /// The case was intentionally not executed (reported, never a pass).
    Skipped,
}

impl Verdict {
    /// Stable lowercase identifier used in reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Agree => "agree",
            Verdict::Mismatch => "mismatch",
            Verdict::Unsupported => "unsupported",
            Verdict::Skipped => "skipped",
        }
    }

    /// Whether this verdict is a hard failure regardless of `--strict`.
    pub fn is_mismatch(&self) -> bool {
        matches!(self, Verdict::Mismatch)
    }

    /// This verdict's contribution to the process exit code.
    ///
    /// A [`Verdict::Mismatch`] always contributes `1`; [`Verdict::Unsupported`] and
    /// [`Verdict::Skipped`] contribute `1` only under `--strict` (so a capability gap in
    /// the Rust runtime does not fail CI before the feature lands).
    pub fn exit_contribution(&self, strict: bool) -> i32 {
        match self {
            Verdict::Agree => 0,
            Verdict::Mismatch => 1,
            Verdict::Unsupported | Verdict::Skipped => i32::from(strict),
        }
    }
}

impl serde::Serialize for Verdict {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// Which observables a comparison inspects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observables {
    /// stdout, exit status **and** stderr — the expression cases, where both runtimes are
    /// driven by the identical `-eval` argv.
    All,
    /// stdout and exit status only — the module-run cases, where the Rust runtime's boot
    /// banner on stderr is not a semantic difference.
    StdoutAndExit,
}

/// Compare two observations field by field (byte-exact, exact exit status).
///
/// Equal to [`compare_observations_with`]`(…, `[`Observables::All`]`)` — the full
/// comparison used by the expression corpus cases. Only the observables are compared —
/// never internals, never the program path.
pub fn compare_observations(oracle: &Observation, rust: &Observation) -> Vec<FieldDiff> {
    compare_observations_with(oracle, rust, Observables::All)
}

/// [`compare_observations`] restricted to the observables selected by `observables`.
///
/// A [`FieldDiff`] is pushed for stdout, then the exit status, then (for
/// [`Observables::All`]) stderr, so a report is stable and readable.
pub fn compare_observations_with(
    oracle: &Observation,
    rust: &Observation,
    observables: Observables,
) -> Vec<FieldDiff> {
    let mut diffs = Vec::new();
    if oracle.stdout != rust.stdout {
        diffs.push(FieldDiff::new(
            "stdout",
            render_bytes(&oracle.stdout),
            render_bytes(&rust.stdout),
        ));
    }
    if oracle.exit_code != rust.exit_code {
        diffs.push(FieldDiff::new(
            "exit_code",
            render_exit_code(oracle.exit_code),
            render_exit_code(rust.exit_code),
        ));
    }
    if observables == Observables::All && oracle.stderr != rust.stderr {
        diffs.push(FieldDiff::new(
            "stderr",
            render_bytes(&oracle.stderr),
            render_bytes(&rust.stderr),
        ));
    }
    diffs
}

/// Check an observation against a case's golden expectation.
///
/// The stdout golden is the probe line **without** the terminating newline that the
/// probe's `~n` emits (see [`normalize_probe_stdout`]); the exit code is compared
/// exactly. Both the observed and the expected values are reported in
/// [`FieldDiff`]s so the failure is diagnosable from the report alone.
pub fn check_golden(observation: &Observation, golden: &Golden) -> GoldenCheck {
    let mut diffs = Vec::new();

    if let Some(expected) = golden.stdout {
        let observed = normalize_probe_stdout(&observation.stdout);
        if observed != expected.as_bytes() {
            diffs.push(FieldDiff::new(
                "stdout",
                render_bytes(observed),
                render_bytes(expected.as_bytes()),
            ));
        }
    }

    if observation.exit_code != Some(golden.exit_code) {
        diffs.push(FieldDiff::new(
            "exit_code",
            render_exit_code(observation.exit_code),
            golden.exit_code.to_string(),
        ));
    }

    GoldenCheck {
        matched: diffs.is_empty(),
        diffs,
    }
}

/// The probe's stdout line without its terminating line break (`~n`), as a byte slice.
///
/// The probe always prints exactly one line, so a golden records the line itself and
/// this normalization strips the trailing `\n` (and a preceding `\r`) before comparing.
pub fn normalize_probe_stdout(bytes: &[u8]) -> &[u8] {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    bytes.strip_suffix(b"\r").unwrap_or(bytes)
}

/// Derive the [`Verdict`] of one case from its two sides and the oracle's golden check.
///
/// Equal to [`verdict_with`]`(…, `[`Observables::All`]`)` — the full comparison used by the
/// expression corpus cases.
pub fn verdict(oracle: &SideResult, rust: &SideResult, golden: &GoldenCheck) -> Verdict {
    verdict_with(oracle, rust, golden, Observables::All)
}

/// [`verdict`] restricted to the observables selected by `observables`.
///
/// The comparison used to decide agreement MUST match the comparison whose
/// [`FieldDiff`]s are reported, otherwise a case could show an empty diff list yet be a
/// `Mismatch` (or vice versa).
pub fn verdict_with(
    oracle: &SideResult,
    rust: &SideResult,
    golden: &GoldenCheck,
    observables: Observables,
) -> Verdict {
    if !golden.matched {
        // The oracle itself did not reproduce the expectation: the corpus has drifted.
        return Verdict::Mismatch;
    }
    match (oracle, rust) {
        (SideResult::Observed(oracle), SideResult::Observed(rust)) => {
            if compare_observations_with(oracle, rust, observables).is_empty() {
                Verdict::Agree
            } else {
                Verdict::Mismatch
            }
        }
        _ => Verdict::Unsupported,
    }
}

/// Render captured bytes for a report: UTF-8 verbatim, otherwise lossy text plus hex.
pub fn render_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => format!(
            "{} (hex:{})",
            String::from_utf8_lossy(bytes),
            hex_string(bytes)
        ),
    }
}

/// Render an exit status for a report (`"N"`, or `"signal"` when the process was killed).
pub fn render_exit_code(code: Option<i32>) -> String {
    match code {
        Some(code) => code.to_string(),
        None => "signal".to_string(),
    }
}

/// A dependency-free lowercase hex rendering of raw bytes (no leading `0x`).
pub fn hex_string(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        check_golden, compare_observations, compare_observations_with, hex_string,
        normalize_probe_stdout, render_bytes, render_exit_code, verdict, verdict_with, FieldDiff,
        GoldenCheck, Observables, SideResult, Verdict,
    };
    use crate::corpus::Golden;
    use crate::runtime::{Observation, RuntimeId};
    use std::path::PathBuf;

    fn observation(runtime: RuntimeId, stdout: &[u8], exit_code: Option<i32>) -> Observation {
        Observation {
            runtime,
            program: PathBuf::from("/somewhere/erl"),
            argv: vec!["-noshell".to_string(), "-eval".to_string()],
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit_code,
        }
    }

    #[test]
    fn identical_observations_have_no_diffs_and_agree() {
        let oracle = observation(RuntimeId::Oracle, b"out\n", Some(0));
        let rust = observation(RuntimeId::Rust, b"out\n", Some(0));
        assert!(compare_observations(&oracle, &rust).is_empty());
        let golden = check_golden(&oracle, &Golden { stdout: Some("out"), exit_code: 0 });
        assert!(golden.matched, "{:?}", golden.diffs);
        assert_eq!(
            verdict(
                &SideResult::observed(oracle.clone()),
                &SideResult::observed(rust),
                &golden
            ),
            Verdict::Agree
        );
    }

    #[test]
    fn differing_observations_produce_diffs_and_mismatch() {
        let oracle = observation(RuntimeId::Oracle, b"ok 7\n", Some(0));
        let rust = observation(RuntimeId::Rust, b"ok 8\n", Some(1));
        let diffs = compare_observations(&oracle, &rust);
        assert_eq!(diffs.len(), 2, "{diffs:?}");
        assert_eq!(diffs[0].field, "stdout");
        assert_eq!(diffs[0].oracle, "ok 7\n");
        assert_eq!(diffs[0].rust, "ok 8\n");
        assert_eq!(diffs[1].field, "exit_code");
        assert_eq!(diffs[1].oracle, "0");
        assert_eq!(diffs[1].rust, "1");

        let golden = check_golden(&oracle, &Golden { stdout: Some("ok 7"), exit_code: 0 });
        assert!(golden.matched);
        assert_eq!(
            verdict(
                &SideResult::observed(oracle),
                &SideResult::observed(rust),
                &golden
            ),
            Verdict::Mismatch
        );
    }

    #[test]
    fn stderr_is_compared_too() {
        let oracle = observation(RuntimeId::Oracle, b"out\n", Some(0));
        let mut rust = observation(RuntimeId::Rust, b"out\n", Some(0));
        rust.stderr = b"warning\n".to_vec();
        let diffs = compare_observations(&oracle, &rust);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].field, "stderr");
    }

    #[test]
    fn stdout_and_exit_only_ignores_stderr_but_still_flags_stdout() {
        let oracle = observation(RuntimeId::Oracle, b"5\n", Some(0));
        let mut rust = observation(RuntimeId::Rust, b"5\n", Some(0));
        // The Rust runtime's boot banner on stderr must not turn agreement into a diff.
        rust.stderr = b"beam_rt erts-17.1 booted: 22 preloaded modules\n".to_vec();
        assert!(
            compare_observations_with(&oracle, &rust, Observables::StdoutAndExit).is_empty(),
            "stderr is not compared for module-run cases"
        );
        // But a differing stdout is still a diff.
        rust.stdout = b"6\n".to_vec();
        let diffs = compare_observations_with(&oracle, &rust, Observables::StdoutAndExit);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].field, "stdout");
    }

    #[test]
    fn a_signalled_process_is_rendered_and_differs_from_an_exit_code() {
        let oracle = observation(RuntimeId::Oracle, b"", None);
        let rust = observation(RuntimeId::Rust, b"", Some(0));
        let diffs = compare_observations(&oracle, &rust);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].oracle, "signal");
        assert_eq!(diffs[0].rust, "0");
        assert_eq!(render_exit_code(None), "signal");
        assert_eq!(render_exit_code(Some(139)), "139");
    }

    #[test]
    fn golden_check_matches_despite_the_probe_newline() {
        let oracle = observation(RuntimeId::Oracle, b"ok 3\n", Some(0));
        let check = check_golden(&oracle, &Golden { stdout: Some("ok 3"), exit_code: 0 });
        assert!(check.matched, "{:?}", check.diffs);
        assert!(check.diffs.is_empty());
    }

    #[test]
    fn golden_check_fails_on_stdout_and_exit_code() {
        let oracle = observation(RuntimeId::Oracle, b"ok 3\n", Some(0));
        let check = check_golden(
            &oracle,
            &Golden {
                stdout: Some("ok 4"),
                exit_code: 1,
            },
        );
        assert!(!check.matched);
        assert_eq!(check.diffs.len(), 2);
        assert_eq!(check.diffs[0].field, "stdout");
        assert_eq!(check.diffs[0].oracle, "ok 3");
        assert_eq!(check.diffs[0].rust, "ok 4");
        assert_eq!(check.diffs[1].field, "exit_code");
        assert_eq!(check.diffs[1].oracle, "0");
        assert_eq!(check.diffs[1].rust, "1");
    }

    #[test]
    fn oracle_golden_drift_is_a_mismatch_not_a_skip() {
        let oracle = observation(RuntimeId::Oracle, b"ok 3\n", Some(0));
        let drifted = check_golden(&oracle, &Golden { stdout: Some("ok 99"), exit_code: 0 });
        assert!(!drifted.matched);
        // Even with no Rust side at all, drift in the oracle is a hard failure.
        assert_eq!(
            verdict(
                &SideResult::observed(oracle),
                &SideResult::unsupported("no beam_rt"),
                &drifted
            ),
            Verdict::Mismatch
        );
    }

    #[test]
    fn an_unsupported_side_is_never_agreement() {
        let oracle = observation(RuntimeId::Oracle, b"ok <<131,97,42>>\n", Some(0));
        let golden = check_golden(
            &oracle,
            &Golden {
                stdout: Some("ok <<131,97,42>>"),
                exit_code: 0,
            },
        );
        assert!(golden.matched);
        let rust = SideResult::unsupported("no Rust runtime found");
        assert!(!rust.is_observed());
        assert!(rust.observation().is_none());
        assert_eq!(rust.unsupported_reason(), Some("no Rust runtime found"));
        assert_eq!(
            verdict(&SideResult::observed(oracle), &rust, &golden),
            Verdict::Unsupported
        );
        assert_eq!(
            verdict(
                &SideResult::observed(observation(RuntimeId::Oracle, b"x\n", Some(0))),
                &rust,
                &GoldenCheck::not_asserted()
            ),
            Verdict::Unsupported
        );
    }

    #[test]
    fn a_stdout_and_exit_verdict_ignores_a_stderr_only_difference() {
        // The module-run case: identical stdout + exit status but a differing stderr (the
        // Rust runtime's boot banner) must AGREE, not mismatch.
        let oracle = observation(RuntimeId::Oracle, b"5\n", Some(0));
        let mut rust = observation(RuntimeId::Rust, b"5\n", Some(0));
        rust.stderr = b"beam_rt erts-17.1 booted\n".to_vec();
        let golden = GoldenCheck::not_asserted();
        assert_eq!(
            verdict(&SideResult::observed(oracle.clone()), &SideResult::observed(rust.clone()), &golden),
            Verdict::Mismatch,
            "the full comparison still flags the stderr difference"
        );
        assert_eq!(
            verdict_with(
                &SideResult::observed(oracle),
                &SideResult::observed(rust),
                &golden,
                Observables::StdoutAndExit
            ),
            Verdict::Agree
        );
    }

    #[test]
    fn verdict_exit_contributions_follow_the_strict_rule() {
        assert_eq!(Verdict::Agree.exit_contribution(true), 0);
        assert_eq!(Verdict::Mismatch.exit_contribution(false), 1);
        assert_eq!(Verdict::Mismatch.exit_contribution(true), 1);
        assert_eq!(Verdict::Unsupported.exit_contribution(false), 0);
        assert_eq!(Verdict::Unsupported.exit_contribution(true), 1);
        assert_eq!(Verdict::Skipped.exit_contribution(false), 0);
        assert_eq!(Verdict::Skipped.exit_contribution(true), 1);
        assert!(Verdict::Mismatch.is_mismatch());
        assert_eq!(Verdict::Agree.as_str(), "agree");
    }

    #[test]
    fn probe_stdout_normalization_strips_one_line_break() {
        assert_eq!(normalize_probe_stdout(b"ok 1\n"), b"ok 1");
        assert_eq!(normalize_probe_stdout(b"ok 1\r\n"), b"ok 1");
        assert_eq!(normalize_probe_stdout(b"ok 1"), b"ok 1");
        assert_eq!(normalize_probe_stdout(b""), b"");
    }

    #[test]
    fn non_utf8_bytes_fall_back_to_a_hex_rendering() {
        assert_eq!(render_bytes(b"plain"), "plain");
        let rendered = render_bytes(&[0x6f, 0xff, 0x6b]);
        assert!(rendered.contains("hex:6fff6b"), "{rendered}");
        assert_eq!(hex_string(&[0x00, 0xab, 0xff]), "00abff");
        assert_eq!(hex_string(&[]), "");
    }

    #[test]
    fn field_diff_serializes_with_its_three_fields() {
        let diff = FieldDiff::new("stdout", "a", "b");
        let json = serde_json::to_value(&diff).expect("serialize");
        assert_eq!(json["field"], "stdout");
        assert_eq!(json["oracle"], "a");
        assert_eq!(json["rust"], "b");
    }
}
