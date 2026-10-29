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
//! The result model: one [`RunReport`] per sweep, with the JSON and Markdown renderings.
//!
//! Every row names **both sides** (`sys` and `rust`) by status and pass/fail counts, and
//! carries a verdict:
//!
//! * `agree` — both sides ran the same set with the same counts;
//! * `agree-fail-identical` — both sides fail identically (a missing `ct` config/data dir, a
//!   pre-existing upstream defect on this host): **excluded from both numerators** and
//!   reported as such, never hidden;
//! * `divergence` — the Rust runtime differs from the oracle;
//! * `rust-gap` — the Rust runtime could not run the row (typed `unsupported`, timeout, or a
//!   crash of the harness-level run) where the oracle could: a capability gap, not a wrong
//!   result;
//! * `not-run` — a side was not resolved at all (e.g. no `beam_rt` binary).
use crate::json::{arr, obj, s, Json};
/// The report schema id; bumped when a field changes meaning.
pub const SCHEMA: &str = "suites/1";
/// What happened on one side for one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Ran and everything passed.
    Pass,
    /// Ran with at least one failure.
    Fail,
    /// The target holds no tests of this kind (helper module, `common_test` suite asked for
    /// eunit).
    NoTests,
    /// The run was cancelled before it measured anything (setup crash, missing module).
    Cancelled,
    /// Every case was skipped by the suite itself.
    Skip,
    /// The side refused the work with a typed `unsupported` (a capability gap).
    Unsupported,
    /// The harness killed the run at the timeout.
    Timeout,
    /// Output that carried no recognisable result.
    Error,
    /// `init_per_suite` failed (all cases of the suite unusable).
    InitFail,
    /// The side was not resolved/attempted.
    NotRun,
}
impl Status {
    /// The stable JSON/Markdown token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::NoTests => "no_tests",
            Status::Cancelled => "cancelled",
            Status::Skip => "skip",
            Status::Unsupported => "unsupported",
            Status::Timeout => "timeout",
            Status::Error => "error",
            Status::InitFail => "init_fail",
            Status::NotRun => "not_run",
        }
    }
    /// Whether this status counts as "ran and agreed" for totals.
    pub fn is_pass(&self) -> bool {
        matches!(self, Status::Pass)
    }
}
/// One side's observation for one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The status.
    pub status: Status,
    /// Tests/cases that passed.
    pub passed: u32,
    /// Tests/cases that failed.
    pub failed: u32,
    /// Tests/cases skipped by the framework itself.
    pub skipped: u32,
    /// The first divergence-grade diagnostic: the first failing test, or the cancellation
    /// reason, or the runtime's one-line `unsupported` message.
    pub detail: String,
}
impl Outcome {
    /// A passing outcome with `passed` tests.
    pub fn pass(passed: u32) -> Self {
        Outcome { status: Status::Pass, passed, failed: 0, skipped: 0, detail: String::new() }
    }
    /// A failing outcome.
    pub fn fail(passed: u32, failed: u32, skipped: u32, detail: impl Into<String>) -> Self {
        Outcome { status: Status::Fail, passed, failed, skipped, detail: detail.into() }
    }
    /// A status-only outcome (no counts).
    pub fn bare(status: Status, detail: impl Into<String>) -> Self {
        Outcome { status, passed: 0, failed: 0, skipped: 0, detail: detail.into() }
    }
    /// The `P/F` cell used by the Markdown table (or the status token when counts are
    /// meaningless).
    pub fn cell(&self) -> String {
        match self.status {
            Status::Pass | Status::Fail => format!("{}/{}", self.passed, self.failed),
            other => other.as_str().to_string(),
        }
    }
    /// The reason *class* of a non-passing outcome: the first token before `:` of the detail
    /// (`badarg`, `undef`, `{badmatch,` …). Used to decide whether two failures are the same
    /// failure without comparing stack traces.
    pub fn reason_class(&self) -> String {
        let detail = self.detail.trim();
        if detail.is_empty() {
            return self.status.as_str().to_string();
        }
        let head = detail.split(':').next().unwrap_or(detail);
        head.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect()
    }
}
/// The comparison outcome of a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Both sides ran it with identical counts.
    Agree,
    /// Both sides fail identically — excluded from both numerators, reported explicitly.
    AgreeFailIdentical,
    /// The Rust runtime answers differently (or does not answer) where the oracle is fine.
    Divergence,
    /// The Rust side could not run the row (typed gap / timeout / unparsed output).
    RustGap,
    /// Only the oracle fails (the Rust runtime does better) — still reported.
    OracleOnlyFail,
    /// A side was not available.
    NotRun,
}
impl Verdict {
    /// The stable token.
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Agree => "agree",
            Verdict::AgreeFailIdentical => "agree-fail-identical",
            Verdict::Divergence => "divergence",
            Verdict::RustGap => "rust-gap",
            Verdict::OracleOnlyFail => "oracle-only-fail",
            Verdict::NotRun => "not-run",
        }
    }
    /// Whether this verdict is a genuine parity miss.
    pub fn is_divergence(&self) -> bool {
        matches!(self, Verdict::Divergence | Verdict::OracleOnlyFail)
    }
}
/// Computes the verdict of a row from the two sides.
pub fn verdict(sys: &Outcome, rust: &Outcome) -> Verdict {
    use Status::*;
    if sys.status == NotRun || rust.status == NotRun {
        return Verdict::NotRun;
    }
    match (sys.status, rust.status) {
        (Pass, Pass) => {
            if sys.passed == rust.passed && sys.skipped == rust.skipped {
                Verdict::Agree
            } else {
                Verdict::Divergence
            }
        }
        (Fail, Fail) => {
            if sys.passed == rust.passed && sys.failed == rust.failed {
                Verdict::AgreeFailIdentical
            } else {
                Verdict::Divergence
            }
        }
        (Pass, Fail) => Verdict::Divergence,
        (Fail, Pass) => Verdict::OracleOnlyFail,
        (Pass, Unsupported | Timeout | Error | Cancelled | NoTests | InitFail | NotRun) => Verdict::RustGap,
        (_, Fail | Pass) => Verdict::OracleOnlyFail,
        (a, b) if a == b => {
            if sys.reason_class() == rust.reason_class() {
                Verdict::AgreeFailIdentical
            } else {
                Verdict::Divergence
            }
        }
        _ => Verdict::Divergence,
    }
}
/// What kind of row this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// One eunit module (`eunit:test(Module, [verbose])`).
    Eunit,
    /// One `common_test` case driven directly (`Suite:Case()`).
    CtCase,
    /// A row that is reported but not measured, e.g. a `common_test` suite that carries no
    /// eunit tests (reported as such instead of vanishing).
    Notice,
}
impl RowKind {
    /// The stable token.
    pub fn as_str(&self) -> &'static str {
        match self {
            RowKind::Eunit => "eunit",
            RowKind::CtCase => "ct_case",
            RowKind::Notice => "notice",
        }
    }
}
/// One measured (or explicitly not-measured) row.
#[derive(Debug, Clone)]
pub struct Row {
    /// Row id: the module name, or `<suite>:<case>`.
    pub id: String,
    /// The owning suite/module group, when the id is a case.
    pub suite: Option<String>,
    /// What kind of row this is.
    pub kind: RowKind,
    /// The oracle's observation.
    pub sys: Outcome,
    /// The Rust runtime's observation.
    pub rust: Outcome,
    /// A harness note (why a row is excluded, what the first divergence says, …).
    pub note: String,
}
impl Row {
    /// The row's verdict.
    pub fn verdict(&self) -> Verdict {
        verdict(&self.sys, &self.rust)
    }
}
/// Where a target's sources came from.
#[derive(Debug, Clone, Default)]
pub struct Source {
    /// The upstream project (a foreign repo id or `otp`).
    pub repo: String,
    /// The absolute path of the read-only original.
    pub path: String,
    /// The upstream commit, when known.
    pub sha: Option<String>,
    /// The staged copy the build and runs actually used.
    pub staged: Option<String>,
}
/// How the target was built (always on one side: the toolchain build is the oracle's).
#[derive(Debug, Clone, Default)]
pub struct Build {
    /// The exact command line.
    pub command: String,
    /// Whether it succeeded.
    pub ok: bool,
    /// Whether a previous successful build was reused.
    pub reused: bool,
    /// A short note / the tail of the output on failure.
    pub detail: String,
}
/// An exact invocation recorded for reproducibility.
#[derive(Debug, Clone)]
pub struct Invocation {
    /// `sys` or `rust`.
    pub side: String,
    /// What it was for (`eunit simple_get`, `ct lists_SUITE:reverse_1`).
    pub purpose: String,
    /// The full command line (shell-quoted).
    pub command: String,
}
/// One target's report (a project/suite-family).
#[derive(Debug, Clone)]
pub struct TargetReport {
    /// `khepri` / `stdlib`.
    pub id: String,
    /// `eunit` (one row per module) or `ct` (one row per case).
    pub kind: String,
    /// The source checkout.
    pub source: Source,
    /// The build.
    pub build: Build,
    /// The `-pa` set both sides received (identical by construction).
    pub code_path: Vec<String>,
    /// Recorded invocations (a representative sample: one per side plus the build).
    pub invocations: Vec<Invocation>,
    /// The rows.
    pub rows: Vec<Row>,
    /// Free-form notes (exclusions, harness caveats).
    pub notes: Vec<String>,
}
impl TargetReport {
    /// `(agree, agree-fail-identical, divergence, rust-gap, not-run)` counts.
    pub fn tally(&self) -> Tally {
        let mut t = Tally::default();
        for row in &self.rows {
            match row.verdict() {
                Verdict::Agree => t.agree += 1,
                Verdict::AgreeFailIdentical => t.agree_fail_identical += 1,
                Verdict::Divergence => t.divergence += 1,
                Verdict::RustGap => t.rust_gap += 1,
                Verdict::OracleOnlyFail => t.oracle_only_fail += 1,
                Verdict::NotRun => t.not_run += 1,
            }
            if row.kind != RowKind::Notice {
                t.measured += 1;
            }
            t.sys_passed += u64::from(row.sys.passed);
            t.sys_failed += u64::from(row.sys.failed);
            t.rust_passed += u64::from(row.rust.passed);
            t.rust_failed += u64::from(row.rust.failed);
        }
        t
    }
    fn to_json(&self) -> Json {
        let t = self.tally();
        obj(vec![
            ("id", s(&self.id)),
            ("kind", s(&self.kind)),
            (
                "source",
                obj(vec![
                    ("repo", s(&self.source.repo)),
                    ("path", s(&self.source.path)),
                    ("sha", crate::json::opt_str(self.source.sha.as_deref())),
                    ("staged", crate::json::opt_str(self.source.staged.as_deref())),
                ]),
            ),
            (
                "build",
                obj(vec![
                    ("command", s(&self.build.command)),
                    ("ok", Json::Bool(self.build.ok)),
                    ("reused", Json::Bool(self.build.reused)),
                    ("detail", s(&self.build.detail)),
                ]),
            ),
            ("code_path", arr(self.code_path.iter().map(s).collect())),
            (
                "invocations",
                arr(self
                    .invocations
                    .iter()
                    .map(|inv| {
                        obj(vec![
                            ("side", s(&inv.side)),
                            ("purpose", s(&inv.purpose)),
                            ("command", s(&inv.command)),
                        ])
                    })
                    .collect()),
            ),
            (
                "tally",
                obj(vec![
                    ("measured", crate::json::i(t.measured)),
                    ("agree", crate::json::i(t.agree)),
                    ("agree_fail_identical", crate::json::i(t.agree_fail_identical)),
                    ("divergence", crate::json::i(t.divergence)),
                    ("oracle_only_fail", crate::json::i(t.oracle_only_fail)),
                    ("rust_gap", crate::json::i(t.rust_gap)),
                    ("not_run", crate::json::i(t.not_run)),
                    ("sys_passed", crate::json::i(t.sys_passed)),
                    ("sys_failed", crate::json::i(t.sys_failed)),
                    ("rust_passed", crate::json::i(t.rust_passed)),
                    ("rust_failed", crate::json::i(t.rust_failed)),
                ]),
            ),
            (
                "rows",
                arr(self
                    .rows
                    .iter()
                    .map(|row| {
                        obj(vec![
                            ("id", s(&row.id)),
                            ("suite", crate::json::opt_str(row.suite.as_deref())),
                            ("kind", s(row.kind.as_str())),
                            ("sys", outcome_json(&row.sys)),
                            ("rust", outcome_json(&row.rust)),
                            ("verdict", s(row.verdict().as_str())),
                            ("note", s(&row.note)),
                        ])
                    })
                    .collect()),
            ),
            ("notes", arr(self.notes.iter().map(s).collect())),
        ])
    }
    fn to_markdown(&self, out: &mut String) {
        let t = self.tally();
        out.push_str(&format!("### Target `{}` ({})\n\n", self.id, self.kind));
        out.push_str(&format!("- source: `{}` ({})", self.source.path, self.source.repo));
        if let Some(sha) = &self.source.sha {
            out.push_str(&format!(" at `{}`", &sha[..sha.len().min(12)]));
        }
        out.push('\n');
        if let Some(staged) = &self.source.staged {
            out.push_str(&format!("- staged copy: `{staged}`\n"));
        }
        out.push_str(&format!(
            "- build: `{}` — {}{}\n",
            self.build.command,
            if self.build.ok { "ok" } else { "FAILED" },
            if self.build.reused { " (reused a completed build)" } else { "" }
        ));
        if !self.build.detail.is_empty() {
            out.push_str(&format!("- build detail: {}\n", one_line(&self.build.detail, 400)));
        }
        out.push_str(&format!(
            "- tally: measured {} · agree {} · agree-fail-identical (excluded) {} · divergence {} · oracle-only-fail {} · rust-gap {} · not-run {}\n",
            t.measured, t.agree, t.agree_fail_identical, t.divergence, t.oracle_only_fail, t.rust_gap, t.not_run
        ));
        out.push_str(&format!(
            "- totals: sys {}/{} passed/failed · rust {}/{} passed/failed\n\n",
            t.sys_passed, t.sys_failed, t.rust_passed, t.rust_failed
        ));
        if !self.notes.is_empty() {
            out.push_str("Notes:\n");
            for note in &self.notes {
                out.push_str(&format!("- {note}\n"));
            }
            out.push('\n');
        }
        out.push_str("| row | sys (P/F) | rust (P/F) | verdict | first divergence / note |\n");
        out.push_str("|---|---|---|---|---|\n");
        for row in &self.rows {
            let verdict = row.verdict();
            let mut note = if row.note.is_empty() {
                row.rust.detail.clone()
            } else {
                row.note.clone()
            };
            if note.is_empty() && verdict.is_divergence() {
                note = format!("{} → {}", row.sys.detail, row.rust.detail);
            }
            out.push_str(&format!(
                "| `{}` | {} | {} | {} | {} |\n",
                row.id,
                row.sys.cell(),
                row.rust.cell(),
                verdict.as_str(),
                one_line(&note, 160)
            ));
        }
        out.push('\n');
        if !self.invocations.is_empty() {
            out.push_str("Recorded invocations (the two sides get the *same* `-pa` set and the *same* `-eval`):\n\n```\n");
            for inv in &self.invocations {
                out.push_str(&format!("# {} [{}]\n{}\n", inv.purpose, inv.side, inv.command));
            }
            out.push_str("```\n\n");
        }
    }
}
/// Per-target aggregate counters.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tally {
    /// Rows that are not bare notices.
    pub measured: u64,
    /// `agree`.
    pub agree: u64,
    /// `agree-fail-identical`.
    pub agree_fail_identical: u64,
    /// `divergence`.
    pub divergence: u64,
    /// `oracle-only-fail`.
    pub oracle_only_fail: u64,
    /// `rust-gap`.
    pub rust_gap: u64,
    /// `not-run`.
    pub not_run: u64,
    /// Sum of oracle passes.
    pub sys_passed: u64,
    /// Sum of oracle failures.
    pub sys_failed: u64,
    /// Sum of Rust passes.
    pub rust_passed: u64,
    /// Sum of Rust failures.
    pub rust_failed: u64,
}
/// Host facts recorded around the sweep (the host is shared; these are not a benchmark).
#[derive(Debug, Clone, Default)]
pub struct HostInfo {
    /// `/proc/loadavg` before the first target ran.
    pub loadavg_before: Option<String>,
    /// `/proc/loadavg` after the last target finished.
    pub loadavg_after: Option<String>,
    /// Logical CPUs.
    pub nproc: Option<u64>,
    /// The oracle program path.
    pub oracle: String,
    /// `<otp_release>/<erts_version>` as the oracle reports it.
    pub oracle_version: Option<String>,
}
/// The Rust runtime that was measured.
#[derive(Debug, Clone, Default)]
pub struct RustInfo {
    /// Program path (`None` when absent).
    pub path: Option<String>,
    /// The workspace commit measured (`git rev-parse HEAD`), never guessed.
    pub sha: Option<String>,
    /// `<otp_release>/<erts_version>` as the Rust runtime reports it.
    pub version: Option<String>,
    /// The tier selection in force (`$BEAM_RT_TIER`, else the default).
    pub tier: Option<String>,
}
/// The whole sweep.
#[derive(Debug, Clone)]
pub struct RunReport {
    /// Seconds since the epoch.
    pub generated_at: u64,
    /// The command line the user ran (so the record is reproducible).
    pub run_command: String,
    /// Host facts.
    pub host: HostInfo,
    /// The Rust runtime measured.
    pub rust: RustInfo,
    /// Per-target results.
    pub targets: Vec<TargetReport>,
}
impl RunReport {
    /// Renders the JSON artifact.
    pub fn to_json(&self) -> Json {
        obj(vec![
            ("schema", s(SCHEMA)),
            ("generated_at", crate::json::i(self.generated_at)),
            ("run_command", s(&self.run_command)),
            (
                "host",
                obj(vec![
                    ("loadavg_before", crate::json::opt_str(self.host.loadavg_before.as_deref())),
                    ("loadavg_after", crate::json::opt_str(self.host.loadavg_after.as_deref())),
                    ("nproc", crate::json::opt_int(self.host.nproc.map(|n| n as i64))),
                    ("oracle", s(&self.host.oracle)),
                    ("oracle_version", crate::json::opt_str(self.host.oracle_version.as_deref())),
                ]),
            ),
            (
                "rust",
                obj(vec![
                    ("path", crate::json::opt_str(self.rust.path.as_deref())),
                    ("sha", crate::json::opt_str(self.rust.sha.as_deref())),
                    ("version", crate::json::opt_str(self.rust.version.as_deref())),
                    ("tier", crate::json::opt_str(self.rust.tier.as_deref())),
                ]),
            ),
            ("targets", arr(self.targets.iter().map(TargetReport::to_json).collect())),
        ])
    }
    /// Renders the human summary (the shape committed as `RESULTS.md` bodies).
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# Test-suite parity sweep\n\n");
        out.push_str(&format!(
            "Generated at `{}` (Unix seconds) by `{}`.\n\n",
            self.generated_at, self.run_command
        ));
        out.push_str("## Environment\n\n");
        out.push_str(&format!("- oracle: `{}`", self.host.oracle));
        if let Some(v) = &self.host.oracle_version {
            out.push_str(&format!(" — `{v}`"));
        }
        out.push('\n');
        out.push_str(&format!("- rust runtime: `{}`", self.rust.path.clone().unwrap_or_else(|| "<absent>".into())));
        if let Some(v) = &self.rust.version {
            out.push_str(&format!(" — `{v}`"));
        }
        out.push('\n');
        out.push_str(&format!("- rust workspace commit: `{}`\n", self.rust.sha.clone().unwrap_or_else(|| "<unknown>".into())));
        out.push_str(&format!("- tier: `{}`\n", self.rust.tier.clone().unwrap_or_else(|| "default".into())));
        out.push_str(&format!(
            "- loadavg before/after: `{}` / `{}` on {} logical CPUs\n",
            self.host.loadavg_before.clone().unwrap_or_else(|| "?".into()),
            self.host.loadavg_after.clone().unwrap_or_else(|| "?".into()),
            self.host.nproc.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
        ));
        out.push_str("- the host is **shared**: only pass/fail counts are portable, never a wall time\n\n");
        for target in &self.targets {
            target.to_markdown(&mut out);
        }
        out
    }
}
fn outcome_json(o: &Outcome) -> Json {
    obj(vec![
        ("status", s(o.status.as_str())),
        ("passed", crate::json::i(o.passed)),
        ("failed", crate::json::i(o.failed)),
        ("skipped", crate::json::i(o.skipped)),
        ("detail", s(one_line(&o.detail, 400))),
    ])
}
/// Collapses whitespace and truncates to `max` characters (table/JSON hygiene).
pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = flat.chars().take(max).collect();
    if flat.chars().count() > max {
        out.push('…');
    }
    out.replace('|', "\\|")
}
#[cfg(test)]
mod tests {
    use super::*;
    fn row(sys: Outcome, rust: Outcome) -> Row {
        Row { id: "m".into(), suite: None, kind: RowKind::Eunit, sys, rust, note: String::new() }
    }
    #[test]
    fn verdicts_cover_the_matrix() {
        assert_eq!(verdict(&Outcome::pass(5), &Outcome::pass(5)), Verdict::Agree);
        assert_eq!(verdict(&Outcome::pass(5), &Outcome::pass(4)), Verdict::Divergence);
        assert_eq!(verdict(&Outcome::pass(5), &Outcome::fail(4, 1, 0, "")), Verdict::Divergence);
        assert_eq!(verdict(&Outcome::fail(4, 1, 0, ""), &Outcome::fail(4, 1, 0, "")), Verdict::AgreeFailIdentical);
        assert_eq!(verdict(&Outcome::fail(3, 2, 0, ""), &Outcome::fail(4, 1, 0, "")), Verdict::Divergence);
        assert_eq!(verdict(&Outcome::pass(5), &Outcome::bare(Status::Unsupported, "unsupported_opcode")), Verdict::RustGap);
        assert_eq!(verdict(&Outcome::pass(5), &Outcome::bare(Status::NotRun, "")), Verdict::NotRun);
        assert_eq!(verdict(&Outcome::fail(4, 1, 0, "x"), &Outcome::pass(5)), Verdict::OracleOnlyFail);
        assert_eq!(
            verdict(&Outcome::bare(Status::InitFail, "badarg"), &Outcome::bare(Status::InitFail, "badarg")),
            Verdict::AgreeFailIdentical
        );
    }
    #[test]
    fn tally_counts_rows_and_tests() {
        let target = TargetReport {
            id: "t".into(),
            kind: "eunit".into(),
            source: Source::default(),
            build: Build::default(),
            code_path: vec![],
            invocations: vec![],
            rows: vec![
                row(Outcome::pass(3), Outcome::pass(3)),
                row(Outcome::pass(2), Outcome::fail(1, 1, 0, "boom")),
                row(Outcome::bare(Status::NoTests, ""), Outcome::bare(Status::NoTests, "")),
            ],
            notes: vec![],
        };
        let t = target.tally();
        assert_eq!(t.measured, 3);
        assert_eq!(t.agree, 1);
        assert_eq!(t.divergence, 1);
        assert_eq!(t.agree_fail_identical, 1);
        assert_eq!((t.sys_passed, t.rust_passed), (5, 4));
    }
    #[test]
    fn markdown_escapes_pipes_and_names_both_sides() {
        let target = TargetReport {
            id: "khepri".into(),
            kind: "eunit".into(),
            source: Source { repo: "khepri".into(), path: "/x".into(), sha: Some("abcdef0123456789".into()), staged: None },
            build: Build { command: "rebar3 as test compile".into(), ok: true, reused: false, detail: String::new() },
            code_path: vec!["/x/ebin".into()],
            invocations: vec![],
            rows: vec![row(Outcome::pass(2), Outcome::fail(1, 1, 0, "a|b"))],
            notes: vec![],
        };
        let report = RunReport {
            generated_at: 1,
            run_command: "suites run".into(),
            host: HostInfo::default(),
            rust: RustInfo::default(),
            targets: vec![target],
        };
        let md = report.to_markdown();
        assert!(md.contains("| `m` | 2/0 | 1/1 | divergence |"), "{md}");
        assert!(md.contains("a\\|b"), "{md}");
        assert!(md.contains("rust workspace commit"), "{md}");
    }
}
