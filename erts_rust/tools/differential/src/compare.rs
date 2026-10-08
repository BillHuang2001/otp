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

//! Comparing observable results: byte-level diffs, golden checks and the verdict.
//!
//! The harness compares only what a black-box observer can see (contract A.2/A.8):
//! the **exact stdout bytes** and the **exit status** of the two runtimes, plus the case's
//! own **golden** (an independent expectation that makes the corpus self-testing — a
//! golden miss by the *oracle* is corpus drift, i.e. a real failure, never a skip).
//!
//! Stderr is compared too for the expression cases ([`Observables::All`]), but through a
//! deliberately **narrow classifier**: only the Rust runtime's documented one-line
//! start-up *boot banner* ([`is_runtime_banner_line`]) is treated as noise — it is a
//! diagnostic of *this build*, not an observable of the program under test, and `erl`
//! prints nothing there. Every other stderr byte stays significant, so a genuine
//! diagnostic difference is still a [`Verdict::Mismatch`]: the classifier can only ever
//! drop a known banner, never fabricate agreement. It is deliberately **not** used for
//! the module-run cases ([`Observables::StdoutAndExit`]), which do not compare stderr at
//! all.

use crate::corpus::Golden;
use crate::runtime::Observation;

/// The literal prefix of the Rust runtime's start-up **boot banner** line.
///
/// `beam_rt` prints exactly one such line on stderr when it boots, e.g.
/// `beam_rt erts-17.1 booted: 22 preloaded modules, 21 core atoms, 1 process`
/// (see `crates/beam_rt/src/runtime.rs`). The oracle `erl` prints nothing there, so the
/// banner is noise rather than a program observable.
pub const BOOT_BANNER_PREFIX: &str = "beam_rt ";

/// The marker that distinguishes the **boot banner** from any other `beam_rt …` stderr
/// line. A line must start with [`BOOT_BANNER_PREFIX`] *and* carry this ` booted: ` marker
/// to be ignored, which keeps the classifier narrow enough that an arbitrary `beam_rt …`
/// diagnostic — and in particular the `beam_rt: unsupported: …` capability-gap line — is
/// never mistaken for a banner.
pub const BOOT_BANNER_MARKER: &str = " booted: ";

/// Whether one line of child stderr is the Rust runtime's documented, non-semantic
/// **boot banner**.
///
/// This is intentionally strict: it matches only `beam_rt <version> booted: …`. Any other
/// line — a different `beam_rt …` message, the `beam_rt: unsupported: …` capability-gap
/// signal, or anything the program itself wrote — is significant and a difference in it
/// remains a [`Verdict::Mismatch`].
pub fn is_runtime_banner_line(line: &str) -> bool {
    line.starts_with(BOOT_BANNER_PREFIX) && line.contains(BOOT_BANNER_MARKER)
}

/// `stderr` with the documented boot-banner lines removed; everything else is verbatim.
///
/// Only lines for which [`is_runtime_banner_line`] holds are dropped. Every other byte —
/// ordering, spacing, a missing trailing newline, a non-UTF-8 byte — is preserved, so
/// comparing the returned buffers is still a real byte-level diff of everything that is
/// *not* a known banner. Boiling this down to no more than "ignore the boot banner" is the
/// whole point: it can never turn a real difference into agreement.
pub fn significant_stderr(stderr: &[u8]) -> Vec<u8> {
    let mut kept = Vec::with_capacity(stderr.len());
    for line in stderr.split_inclusive(|&byte| byte == b'\n') {
        // The banner is compared without its line terminator, so a `\r\n`-terminated line
        // is still recognised on the (unlikely) Windows path.
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let body = body.strip_suffix(b"\r").unwrap_or(body);
        let is_banner = std::str::from_utf8(body)
            .map(is_runtime_banner_line)
            .unwrap_or(false);
        if !is_banner {
            kept.extend_from_slice(line);
        }
    }
    kept
}

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
    /// stdout, exit status **and** (classified) stderr — the expression cases, where both
    /// runtimes are driven by the identical `-eval` argv. The stderr comparison ignores
    /// only the Rust runtime's documented boot banner ([`significant_stderr`]); every
    /// other stderr difference is still a mismatch.
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
///
/// The stderr diff is taken on the **classified** stderr ([`significant_stderr`]): the
/// Rust runtime's documented boot banner is dropped from both sides first, so it cannot
/// turn a legitimate agreement into a diff while every other stderr byte is still compared
/// exactly. The reported [`FieldDiff`] renders the classified buffers, so it shows the
/// significant difference rather than the banner noise.
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
    if observables == Observables::All {
        let oracle_stderr = significant_stderr(&oracle.stderr);
        let rust_stderr = significant_stderr(&rust.stderr);
        if oracle_stderr != rust_stderr {
            diffs.push(FieldDiff::new(
                "stderr",
                render_bytes(&oracle_stderr),
                render_bytes(&rust_stderr),
            ));
        }
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
        is_runtime_banner_line, normalize_probe_stdout, render_bytes, render_exit_code,
        significant_stderr, verdict, verdict_with, FieldDiff, GoldenCheck, Observables, SideResult,
        Verdict,
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
    fn a_banner_only_stderr_difference_agrees_for_expression_cases() {
        // The expression-case observables ([`Observables::All`]): `beam_rt` writes its
        // one-line boot banner to stderr, `erl` writes nothing. That is not a semantic
        // difference, so the case must AGREE with no diffs — otherwise a legitimate
        // `-eval` agreement would flip to a mismatch purely because of the banner.
        let oracle = observation(RuntimeId::Oracle, b"ok <<131,97,42>>\n", Some(0));
        let mut rust = observation(RuntimeId::Rust, b"ok <<131,97,42>>\n", Some(0));
        rust.stderr = b"beam_rt erts-17.1 booted: 22 preloaded modules\n".to_vec();
        assert!(
            compare_observations(&oracle, &rust).is_empty(),
            "the boot banner must not produce a diff"
        );
        assert_eq!(
            verdict(
                &SideResult::observed(oracle),
                &SideResult::observed(rust),
                &GoldenCheck::not_asserted()
            ),
            Verdict::Agree
        );
    }

    #[test]
    fn a_banner_on_the_oracle_side_is_ignored_symmetrically() {
        // The classifier is applied to both sides, so a stray banner on the oracle's
        // stderr is also non-semantic (the oracle prints none today; this pins the rule).
        let mut oracle = observation(RuntimeId::Oracle, b"5\n", Some(0));
        oracle.stderr = b"beam_rt erts-17.1 booted: 1 preloaded modules\n".to_vec();
        let rust = observation(RuntimeId::Rust, b"5\n", Some(0));
        assert!(compare_observations(&oracle, &rust).is_empty());
    }

    #[test]
    fn any_non_banner_stderr_line_is_still_a_mismatch() {
        let oracle = observation(RuntimeId::Oracle, b"ok 1\n", Some(0));
        let mut rust = observation(RuntimeId::Rust, b"ok 1\n", Some(0));
        // A banner *plus* a genuine diagnostic: only the banner is dropped, so the
        // surviving diagnostic is still a diff (and it is what the diff reports).
        rust.stderr = b"beam_rt erts-17.1 booted: 22 preloaded modules\n".to_vec();
        rust.stderr.extend_from_slice(b"evaluated 1 expression\n");
        let diffs = compare_observations(&oracle, &rust);
        assert_eq!(diffs.len(), 1, "{diffs:?}");
        assert_eq!(diffs[0].field, "stderr");
        assert!(
            diffs[0].rust.contains("evaluated 1 expression"),
            "the significant line is reported: {:?}",
            diffs[0]
        );
        assert!(
            !diffs[0].rust.contains("booted"),
            "the banner is not part of the reported diff: {:?}",
            diffs[0]
        );
        assert_eq!(
            verdict(
                &SideResult::observed(oracle),
                &SideResult::observed(rust),
                &GoldenCheck::not_asserted()
            ),
            Verdict::Mismatch
        );
    }

    #[test]
    fn the_unsupported_capability_line_is_not_a_banner() {
        // The `beam_rt: unsupported: …` capability-gap line must never be swallowed as
        // banner noise — it is a real signal, matched only under the exit-3 rule.
        assert!(!is_runtime_banner_line(
            "beam_rt: unsupported: unknown switch \"--nope\""
        ));
        // A line that merely shares the prefix but lacks the boot marker is significant.
        assert!(!is_runtime_banner_line("beam_rt loaded a module"));
        assert!(!is_runtime_banner_line("beam_rt "));
        // And the real banner (with its marker) is recognised.
        assert!(is_runtime_banner_line(
            "beam_rt erts-17.1 booted: 22 preloaded modules, 21 core atoms, 1 process"
        ));

        let oracle = observation(RuntimeId::Oracle, b"ok 1\n", Some(0));
        let mut rust = observation(RuntimeId::Rust, b"ok 1\n", Some(0));
        rust.stderr = b"beam_rt: unsupported: nope\n".to_vec();
        assert_eq!(
            compare_observations(&oracle, &rust).len(),
            1,
            "the unsupported line is a significant stderr difference"
        );
    }

    #[test]
    fn significant_stderr_drops_only_banner_lines_and_preserves_the_rest() {
        assert!(significant_stderr(b"").is_empty());
        assert!(
            significant_stderr(b"beam_rt 1 booted: x\n").is_empty(),
            "a lone banner is fully removed"
        );
        assert_eq!(
            significant_stderr(b"beam_rt 1 booted: x\nkeep me\n"),
            b"keep me\n"
        );
        assert_eq!(
            significant_stderr(b"keep me\nbeam_rt 1 booted: x\n"),
            b"keep me\n",
            "trailing banners go too"
        );
        assert_eq!(
            significant_stderr(b"kept line without newline"),
            b"kept line without newline",
            "a banner-less buffer is byte-identical"
        );
        assert_eq!(
            significant_stderr(&[0xff, 0xfe, b'\n']),
            &[0xff, 0xfe, b'\n'],
            "non-UTF-8 stderr can never be a banner and is preserved"
        );
    }

    #[test]
    fn the_probe_etf_bytes_are_a_first_class_observable() {
        // The probe prints `ok <term_to_binary(Result)>`; a single differing byte in the
        // encoded term is a real difference, not banner noise, so it must be a mismatch.
        let oracle = observation(RuntimeId::Oracle, b"ok <<131,97,42>>\n", Some(0));
        let rust = observation(RuntimeId::Rust, b"ok <<131,97,43>>\n", Some(0));
        let diffs = compare_observations(&oracle, &rust);
        assert_eq!(diffs.len(), 1, "{diffs:?}");
        assert_eq!(diffs[0].field, "stdout");
        assert!(diffs[0].oracle.contains("131,97,42"), "{:?}", diffs[0]);
        assert!(diffs[0].rust.contains("131,97,43"), "{:?}", diffs[0]);
    }

    #[test]
    fn the_probe_exception_class_and_reason_are_a_first_class_observable() {
        // The probe prints `exception <class> <reason>` and exits 1 on an uncaught error,
        // so both the class+reason text (arriving on stdout) and the exit status are
        // compared — a differing class/reason is a mismatch.
        let oracle = observation(RuntimeId::Oracle, b"exception error badarith\n", Some(1));
        let same = observation(RuntimeId::Rust, b"exception error badarith\n", Some(1));
        assert!(compare_observations(&oracle, &same).is_empty());

        let different_reason = observation(RuntimeId::Rust, b"exception error badarg\n", Some(1));
        let diffs = compare_observations(&oracle, &different_reason);
        assert_eq!(diffs.len(), 1, "{diffs:?}");
        assert_eq!(diffs[0].field, "stdout");

        let different_class = observation(RuntimeId::Rust, b"exception exit normal\n", Some(1));
        assert_eq!(compare_observations(&oracle, &different_class).len(), 1);

        // Disagreeing on whether it raised at all shows up in both stdout and exit status.
        let no_raise = observation(RuntimeId::Rust, b"ok <<131,97,42>>\n", Some(0));
        let diffs = compare_observations(&oracle, &no_raise);
        assert_eq!(diffs.len(), 2, "{diffs:?}");
        assert!(diffs.iter().any(|d| d.field == "stdout"));
        assert!(diffs.iter().any(|d| d.field == "exit_code"));
    }

    #[test]
    fn the_golden_check_is_unaffected_by_stderr_classification() {
        // The golden asserts stdout + exit code only; neither the banner nor any other
        // stderr byte can sway it.
        let mut oracle = observation(RuntimeId::Oracle, b"ok 3\n", Some(0));
        oracle.stderr = b"beam_rt 1 booted: x\nsomething else\n".to_vec();
        let check = check_golden(
            &oracle,
            &Golden {
                stdout: Some("ok 3"),
                exit_code: 0,
            },
        );
        assert!(check.matched, "{:?}", check.diffs);
        assert!(check.diffs.is_empty());
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
    fn verdicts_ignore_the_banner_but_never_other_stderr() {
        let oracle = observation(RuntimeId::Oracle, b"5\n", Some(0));
        let mut banner = observation(RuntimeId::Rust, b"5\n", Some(0));
        banner.stderr = b"beam_rt erts-17.1 booted: 22 preloaded modules\n".to_vec();
        let golden = GoldenCheck::not_asserted();

        // A banner-only difference agrees under *both* observable sets.
        for observables in [Observables::All, Observables::StdoutAndExit] {
            assert_eq!(
                verdict_with(
                    &SideResult::observed(oracle.clone()),
                    &SideResult::observed(banner.clone()),
                    &golden,
                    observables
                ),
                Verdict::Agree,
                "a banner-only difference must agree for {observables:?}"
            );
        }

        // A banner plus a genuine diagnostic is still a mismatch under the full
        // comparison, but the module-run observables never look at stderr at all.
        let mut noise = observation(RuntimeId::Rust, b"5\n", Some(0));
        noise.stderr = b"beam_rt erts-17.1 booted: 22 preloaded modules\nwarning: slow\n".to_vec();
        assert_eq!(
            verdict_with(
                &SideResult::observed(oracle.clone()),
                &SideResult::observed(noise.clone()),
                &golden,
                Observables::All
            ),
            Verdict::Mismatch
        );
        assert_eq!(
            verdict_with(
                &SideResult::observed(oracle),
                &SideResult::observed(noise),
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
