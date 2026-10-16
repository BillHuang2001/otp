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

//! The **ratcheted** strict gate: classify the current capability gaps against the
//! allowlist, render a `GAPS.md`, and derive the suite's exit code.
//!
//! A **gap** is a non-agreeing case whose verdict is `Unsupported` *or* `Skipped`. A
//! `Mismatch` is never a gap — it is always a hard failure. With an allowlist the gate
//! tolerates the gaps it records and fails the moment an *unauthorized* gap appears (the
//! allowlist must be widened deliberately) or an entry goes *stale* (its case is no longer
//! a gap — the allowlist must be tightened). Without an allowlist the behaviour is exactly
//! as before (`strict` escalates any `unsupported`/`skipped` case to a failure).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::allowlist::Allowlist;
use crate::compare::Verdict;
use crate::error::{Error, Result};
use crate::harness::SuiteOutcome;
use crate::report::{CaseReport, RunReport};

/// Whether `verdict` is a capability gap the allowlist may tolerate.
///
/// `Mismatch` is deliberately excluded: a differing observable result is never a gap.
fn is_gap(verdict: Verdict) -> bool {
    matches!(verdict, Verdict::Unsupported | Verdict::Skipped)
}

/// Whether `report` contains a case whose observable result actually diverged.
///
/// This is the gate's hard-failure ground truth, and it is derived from the **case verdicts**
/// first — the same source of truth [`RatchetReport::classify`] reads, so the "is it a gap?"
/// and "is it a hard failure?" questions can never disagree. The summary's `mismatch` count is
/// honoured *in addition*, so a report whose summary under-reports a mismatch still fails.
/// The function can only ever *widen* the mismatch set, never narrow it: once a case is
/// `Verdict::Mismatch`, no allowlist entry and no summary can make it disappear.
fn has_mismatch(report: &RunReport) -> bool {
    report.summary.mismatch > 0 || report.cases.iter().any(|case| case.verdict.is_mismatch())
}

/// The current run's gap classification against the allowlist.
///
/// This is the top-level `ratchet` key of a [`RunReport`] produced with an allowlist; it is
/// omitted entirely from a run without one, so that report is byte-for-byte unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RatchetReport {
    /// The allowlist path as given (verbatim), when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowlist: Option<String>,
    /// Gaps whose case id is recorded in the allowlist.
    pub allowed: usize,
    /// Gaps whose case id is *not* recorded — the ratchet must be updated.
    pub unauthorized: usize,
    /// Allowlist entries whose case did not reproduce as a gap (a closed gap, a renamed id
    /// or a typo) — the ratchet must be tightened.
    pub stale: usize,
}

impl RatchetReport {
    /// Classify the case reports of a run against `allowlist`.
    pub fn classify(cases: &[CaseReport], allowlist: &Allowlist) -> RatchetReport {
        let mut allowed = 0;
        let mut unauthorized = 0;
        let mut gap_ids: BTreeSet<&str> = BTreeSet::new();
        for case in cases {
            if !is_gap(case.verdict) {
                continue;
            }
            gap_ids.insert(case.case_id.as_str());
            if allowlist.contains(&case.case_id) {
                allowed += 1;
            } else {
                unauthorized += 1;
            }
        }
        let stale = allowlist
            .entries()
            .iter()
            .filter(|entry| !gap_ids.contains(entry.case_id.as_str()))
            .count();
        RatchetReport {
            allowlist: allowlist.path().map(|path| path.display().to_string()),
            allowed,
            unauthorized,
            stale,
        }
    }
}

/// Build the run report of a suite run, carrying the ratchet classification of `allowlist`.
pub fn report_from_suite(suite: &SuiteOutcome, allowlist: &Allowlist) -> RunReport {
    let cases: Vec<CaseReport> = suite.cases.iter().map(CaseReport::from_outcome).collect();
    let ratchet = RatchetReport::classify(&cases, allowlist);
    RunReport::new_with_ratchet(
        "suite",
        &suite.context.oracle,
        suite.context.rust.as_ref(),
        cases,
        Some(ratchet),
    )
}

/// The process exit code of a suite run.
///
/// * `1` on any `mismatch` (always a hard failure — see [`has_mismatch`], which reads the
///   case verdicts so a mismatch can never be hidden by the summary *or* tolerated by an
///   allowlist);
/// * with a ratchet: `1` when an unauthorized gap appeared or an entry went stale, else `0`
///   (an allowlisted gap set is tolerated);
/// * without a ratchet (today's behaviour): `1` under `strict` when any `unsupported`/
///   `skipped` case remains, else `0`.
pub fn suite_exit_code(report: &RunReport, strict: bool) -> i32 {
    if has_mismatch(report) {
        return 1;
    }
    if let Some(ratchet) = &report.ratchet {
        return if ratchet.unauthorized > 0 || ratchet.stale > 0 {
            1
        } else {
            0
        };
    }
    if strict && (report.summary.unsupported > 0 || report.summary.skipped > 0) {
        1
    } else {
        0
    }
}

/// Escape a value for a markdown table cell (`|` and line breaks).
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace(['\n', '\r'], " ")
}

/// Render the generated `GAPS.md` document of a run.
///
/// One table lists every current gap (in `report.cases` order), classified `allowed` or
/// `unauthorized`; stale allowlist entries follow with status `stale`.
pub fn gaps_markdown(report: &RunReport, allowlist: &Allowlist) -> String {
    let ratchet = RatchetReport::classify(&report.cases, allowlist);
    let summary = &report.summary;
    let mut out = String::new();
    out.push_str("# Known capability gaps\n\n");
    out.push_str(
        "Generated by `differential suite --strict --allowlist <file> --gaps <path>` — do\n\
         not hand-edit. Edit the allowlist and re-run the gate instead.\n\n",
    );
    let _ = writeln!(
        out,
        "Run: total={} agree={} mismatch={} unsupported={} skipped={}\n",
        summary.total, summary.agree, summary.mismatch, summary.unsupported, summary.skipped
    );
    let _ = writeln!(
        out,
        "Allowlist: {} entries={} allowed={} unauthorized={} stale={}\n",
        ratchet.allowlist.as_deref().unwrap_or("(none)"),
        allowlist.len(),
        ratchet.allowed,
        ratchet.unauthorized,
        ratchet.stale
    );
    out.push_str("| Case ID | Verdict | Owning crate | Status | Reason |\n");
    out.push_str("|---|---|---|---|---|\n");
    for case in &report.cases {
        if !is_gap(case.verdict) {
            continue;
        }
        let entry = allowlist.get(&case.case_id);
        let (status, crate_name) = match entry {
            Some(entry) => ("allowed", entry.crate_name.as_str()),
            None => ("unauthorized", "—"),
        };
        let reason = case.rust.reason.as_deref().unwrap_or("—");
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            cell(&case.case_id),
            case.verdict.as_str(),
            cell(crate_name),
            status,
            cell(reason)
        );
    }
    let gap_ids: BTreeSet<&str> = report
        .cases
        .iter()
        .filter(|case| is_gap(case.verdict))
        .map(|case| case.case_id.as_str())
        .collect();
    for entry in allowlist.entries() {
        if gap_ids.contains(entry.case_id.as_str()) {
            continue;
        }
        let _ = writeln!(
            out,
            "| {} | — | {} | stale | {} |",
            cell(&entry.case_id),
            cell(&entry.crate_name),
            cell(&entry.reason)
        );
    }
    out
}

/// The file name of the hand-maintained divergences ledger.
///
/// When `suite --gaps <path>` writes the generated report, a file with this name sitting in
/// the SAME directory as `<path>` is appended verbatim after a blank line, so the document
/// ends with the ledger. Its absence leaves the generated output byte-for-byte unchanged.
pub const DIVERGENCES_FILE: &str = "divergences.md";

/// Write the `--gaps` document: the generated gap table (see [`gaps_markdown`]) plus, when a
/// divergences ledger sits beside `path`, that ledger appended verbatim after a blank line.
///
/// The file read is deliberately confined here (never in the pure [`gaps_markdown`] renderer),
/// so the renderer stays a pure function of the report and the allowlist. Only a *missing*
/// ledger is tolerated (the section is then omitted); any other IO error is loud.
pub fn write_gaps(path: &Path, report: &RunReport, allowlist: &Allowlist) -> Result<()> {
    let mut text = gaps_markdown(report, allowlist);
    if let Some(ledger) = read_divergences(path)? {
        text.push('\n');
        text.push_str(&ledger);
    }
    write_markdown(path, &text)
}

/// Read the divergences ledger that sits in the same directory as `gaps_path`.
///
/// Returns `Ok(None)` only when the ledger is absent (the documented "no section" case); an
/// unreadable or non-UTF-8 ledger is a loud [`Error::Io`], never silently dropped.
fn read_divergences(gaps_path: &Path) -> Result<Option<String>> {
    let dir = match gaps_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    let ledger = dir.join(DIVERGENCES_FILE);
    match std::fs::read_to_string(&ledger) {
        Ok(text) => Ok(Some(text)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::io(
            format!("reading the divergences ledger '{}'", ledger.display()),
            source,
        )),
    }
}

/// Write the generated `GAPS.md`, creating its parent directory if needed.
pub fn write_markdown(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| {
                Error::io(
                    format!("writing the gaps report '{}'", path.display()),
                    source,
                )
            })?;
        }
    }
    std::fs::write(path, text).map_err(|source| {
        Error::io(
            format!("writing the gaps report '{}'", path.display()),
            source,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{gaps_markdown, report_from_suite, suite_exit_code, write_gaps, RatchetReport};
    use crate::allowlist::Allowlist;
    use crate::compare::{FieldDiff, SideResult, Verdict};
    use crate::corpus::Contract;
    use crate::report::{CaseReport, RunReport, SideReport, Summary};
    use crate::runtime::{RuntimeId, RuntimeSource, RuntimeSpec};
    use std::path::PathBuf;

    fn spec() -> RuntimeSpec {
        RuntimeSpec {
            id: RuntimeId::Oracle,
            program: PathBuf::from("/x/erl"),
            source: RuntimeSource::Path,
        }
    }

    fn side(available: bool, reason: Option<&str>) -> SideReport {
        SideReport {
            available,
            reason: reason.map(|reason| reason.to_string()),
            stdout: if available {
                Some("ok\n".to_string())
            } else {
                None
            },
            stdout_hex: if available {
                Some("6f6b0a".to_string())
            } else {
                None
            },
            exit_code: if available { Some(0) } else { None },
            matched_golden: None,
        }
    }

    fn case(case_id: &str, verdict: Verdict, rust_reason: Option<&str>) -> CaseReport {
        CaseReport {
            case_id: case_id.to_string(),
            description: format!("the {case_id} case"),
            contracts: vec![Contract::LanguageSemantics],
            eval: "try (ok) of V -> ok end.".to_string(),
            verdict,
            oracle: side(true, None),
            rust: side(verdict == Verdict::Agree, rust_reason),
            diffs: Vec::<FieldDiff>::new(),
        }
    }

    fn report(cases: Vec<CaseReport>, ratchet: Option<RatchetReport>) -> RunReport {
        RunReport::new_with_ratchet("suite", &spec(), None, cases, ratchet)
    }

    fn allowlist(source: &str) -> Allowlist {
        Allowlist::parse(source, None).expect("parses")
    }

    /// A synthetic [`CaseOutcome`] carrying only the fields the ratchet path reads, so
    /// `report_from_suite` can be exercised without an oracle-backed harness run.
    fn suite_case(case_id: &str, verdict: Verdict) -> crate::harness::CaseOutcome {
        crate::harness::CaseOutcome {
            case_id: case_id.to_string(),
            description: "d".to_string(),
            contracts: vec![Contract::OtpApps],
            eval: "try (ok) of V -> ok end.".to_string(),
            oracle_argv: vec!["-noshell".to_string()],
            rust_argv: vec!["-noshell".to_string()],
            oracle: SideResult::unsupported("oracle"),
            rust: SideResult::unsupported("beam_rt: unsupported: x"),
            golden: crate::compare::GoldenCheck {
                matched: true,
                diffs: Vec::new(),
            },
            golden_asserted: true,
            verdict,
            diffs: Vec::new(),
            oracle_spec: spec(),
            rust_spec: None,
        }
    }
    #[test]
    fn classify_buckets_gaps_and_stale_entries() {
        let cases = vec![
            case("a/agree", Verdict::Agree, None),
            case("a/gap", Verdict::Unsupported, Some("beam_rt: unsupported: x")),
            case("a/skipped", Verdict::Skipped, Some("no erlc")),
            case("a/other", Verdict::Unsupported, Some("beam_rt: unsupported: y")),
            case("a/bad", Verdict::Mismatch, None),
        ];
        // `a/gap` is allowed; `a/skipped` is NOT; `stale/case` is not reproduced at all.
        let list = allowlist("a/gap  crate_a  allowed reason\nstale/case  crate_b  a closed gap\n");
        let ratchet = RatchetReport::classify(&cases, &list);
        assert_eq!(ratchet.allowed, 1, "{ratchet:?}");
        assert_eq!(ratchet.unauthorized, 2, "{ratchet:?}");
        assert_eq!(ratchet.stale, 1, "{ratchet:?}");
        assert_eq!(ratchet.allowlist, None);
    }

    #[test]
    fn a_mismatch_is_never_a_gap() {
        let cases = vec![case("a/bad", Verdict::Mismatch, None)];
        let list = allowlist("a/bad  crate  tolerated\n");
        let ratchet = RatchetReport::classify(&cases, &list);
        assert_eq!(ratchet.allowed, 0, "{ratchet:?}");
        assert_eq!(ratchet.unauthorized, 0, "{ratchet:?}");
        // The entry did not reproduce as a gap → stale (the allowlist must be tightened).
        assert_eq!(ratchet.stale, 1, "{ratchet:?}");
    }

    /// The core invariant: a `Mismatch` whose id IS recorded in the allowlist must never be
    /// tolerant — it is not `allowed` (nor `unauthorized`, the other gap counter), it is
    /// `stale`, and the gate still exits `1`.
    #[test]
    fn an_allowlisted_mismatch_is_stale_and_still_fails_the_gate() {
        let cases = vec![
            case("a/agree", Verdict::Agree, None),
            case("a/bad", Verdict::Mismatch, Some("oracle said x, rust said y")),
        ];
        let list = allowlist("a/bad  crate  this entry must never tolerate a mismatch\n");
        let ratchet = RatchetReport::classify(&cases, &list);
        assert_eq!(ratchet.allowed, 0, "a mismatch is never an allowed gap: {ratchet:?}");
        assert_eq!(ratchet.unauthorized, 0, "a mismatch is no gap at all: {ratchet:?}");
        assert_eq!(ratchet.stale, 1, "the recorded id did not reproduce as a gap: {ratchet:?}");

        let run = report(cases, Some(ratchet));
        // Exit 1 under the strict ratcheted gate `gate.sh` runs — the allowlist can never turn
        // a mismatch into a pass (`0`) or a mere ratchet update (`3`). `--strict` is irrelevant
        // to a mismatch: it is a hard failure either way.
        assert_eq!(suite_exit_code(&run, true), 1);
        assert_eq!(suite_exit_code(&run, false), 1);
    }

    /// A per-verdict matrix proving only `unsupported`/`skipped` reach the gap counters, so a
    /// `mismatch` can never be recorded as an allowed (or unauthorized) gap.
    #[test]
    fn a_mismatch_never_contributes_to_the_gap_counters() {
        for (verdict, is_gap) in [
            (Verdict::Agree, false),
            (Verdict::Mismatch, false),
            (Verdict::Unsupported, true),
            (Verdict::Skipped, true),
        ] {
            let cases = vec![case("a/x", verdict, Some("a reason"))];
            let recorded = allowlist("a/x  crate  recorded\n");
            let unrecorded = allowlist("# nothing recorded\n");
            let allowed = RatchetReport::classify(&cases, &recorded);
            let unauthorized = RatchetReport::classify(&cases, &unrecorded);
            if is_gap {
                assert_eq!(
                    (allowed.allowed, allowed.unauthorized, allowed.stale),
                    (1, 0, 0),
                    "{verdict:?} is an allowlisted gap"
                );
                assert_eq!(
                    (unauthorized.allowed, unauthorized.unauthorized, unauthorized.stale),
                    (0, 1, 0),
                    "{verdict:?} is an unauthorized gap"
                );
            } else {
                // Neither gap counter ever sees a non-gap; a recorded non-gap is `stale`.
                assert_eq!(
                    (allowed.allowed, allowed.unauthorized),
                    (0, 0),
                    "{verdict:?} must never reach a gap counter"
                );
                assert_eq!(allowed.stale, 1, "{verdict:?} recorded but not a gap");
                assert_eq!(
                    (unauthorized.allowed, unauthorized.unauthorized, unauthorized.stale),
                    (0, 0, 0),
                    "{verdict:?} not recorded and not a gap"
                );
            }
        }
    }

    /// Defense in depth: the hard-failure condition reads the **case verdicts** (the same
    /// ground truth `RatchetReport::classify` uses), not only the summary, so a report whose
    /// summary under-reports a mismatch still fails — as does one with a mismatching case but
    /// an allowlist that records it.
    #[test]
    fn suite_exit_code_reads_the_case_verdicts_not_only_the_summary() {
        let mut run = report(vec![case("a/bad", Verdict::Mismatch, None)], None);
        // A summary that lies (it under-reports the mismatch) can never hide the case.
        run.summary = Summary {
            total: 1,
            agree: 1,
            mismatch: 0,
            unsupported: 0,
            skipped: 0,
        };
        assert_eq!(suite_exit_code(&run, true), 1);
        assert_eq!(suite_exit_code(&run, false), 1);
    }

    #[test]
    fn classify_records_the_allowlist_path() {
        let list = Allowlist::parse("", Some(PathBuf::from("known-gaps.txt"))).expect("parses");
        let ratchet = RatchetReport::classify(&[], &list);
        assert_eq!(ratchet.allowlist.as_deref(), Some("known-gaps.txt"));
        assert_eq!((ratchet.allowed, ratchet.unauthorized, ratchet.stale), (0, 0, 0));
    }

    #[test]
    fn suite_exit_code_prioritises_mismatch_then_the_ratchet() {
        // A mismatch is always exit 1, whatever the ratchet says.
        let with_mismatch = report(
            vec![
                case("a/bad", Verdict::Mismatch, None),
                case("a/gap", Verdict::Unsupported, Some("x")),
            ],
            Some(RatchetReport {
                allowlist: None,
                allowed: 1,
                unauthorized: 0,
                stale: 0,
            }),
        );
        assert_eq!(suite_exit_code(&with_mismatch, true), 1);

        // An unauthorized gap or a stale entry fails; a clean ratchet tolerates its gaps.
        let unauthorized = report(
            vec![case("a/gap", Verdict::Unsupported, Some("x"))],
            Some(RatchetReport {
                allowlist: None,
                allowed: 0,
                unauthorized: 1,
                stale: 0,
            }),
        );
        assert_eq!(suite_exit_code(&unauthorized, true), 1);

        let stale = report(
            vec![case("a/agree", Verdict::Agree, None)],
            Some(RatchetReport {
                allowlist: None,
                allowed: 0,
                unauthorized: 0,
                stale: 1,
            }),
        );
        assert_eq!(suite_exit_code(&stale, true), 1);

        let clean = report(
            vec![
                case("a/agree", Verdict::Agree, None),
                case("a/gap", Verdict::Unsupported, Some("x")),
            ],
            Some(RatchetReport {
                allowlist: None,
                allowed: 1,
                unauthorized: 0,
                stale: 0,
            }),
        );
        assert_eq!(suite_exit_code(&clean, true), 0);
        assert_eq!(suite_exit_code(&clean, false), 0);
    }

    #[test]
    fn suite_exit_code_without_a_ratchet_is_todays_behaviour() {
        let gaps = report(
            vec![case("a/gap", Verdict::Unsupported, Some("x"))],
            None,
        );
        assert_eq!(suite_exit_code(&gaps, true), 1);
        assert_eq!(suite_exit_code(&gaps, false), 0);

        let mismatch = report(vec![case("a/bad", Verdict::Mismatch, None)], None);
        assert_eq!(suite_exit_code(&mismatch, true), 1);
        assert_eq!(suite_exit_code(&mismatch, false), 1);

        let all_agree = report(vec![case("a/agree", Verdict::Agree, None)], None);
        assert_eq!(suite_exit_code(&all_agree, true), 0);
    }

    #[test]
    fn gaps_markdown_renders_allowed_unauthorized_and_stale_rows() {
        let cases = vec![
            case("a/agree", Verdict::Agree, None),
            case(
                "a/gap|piped",
                Verdict::Unsupported,
                Some("beam_rt: unsupported: a|b\nsecond line"),
            ),
            case("a/unauth", Verdict::Skipped, Some("no erlc")),
        ];
        let list = allowlist("a/gap|piped  crate_a  an allowed gap\nstale/case  crate_b  a closed gap\n");
        let run = report(cases, Some(RatchetReport::classify(&[], &list)));
        let markdown = gaps_markdown(&run, &list);

        assert!(markdown.starts_with("# Known capability gaps"), "{markdown}");
        assert!(markdown.contains("| Case ID | Verdict | Owning crate | Status | Reason |"));
        // An allowed gap: its crate comes from the allowlist.
        assert!(
            markdown.contains("| a/gap\\|piped | unsupported | crate_a | allowed |"),
            "{markdown}"
        );
        // The reason's `|` is escaped and its newline collapsed.
        assert!(markdown.contains("beam_rt: unsupported: a\\|b second line"), "{markdown}");
        // An unauthorized gap: no crate, status `unauthorized`.
        assert!(
            markdown.contains("| a/unauth | skipped | — | unauthorized | no erlc |"),
            "{markdown}"
        );
        // A stale entry: verdict `—`, status `stale`, the allowlist's reason.
        assert!(
            markdown.contains("| stale/case | — | crate_b | stale | a closed gap |"),
            "{markdown}"
        );
        // An agreeing case never appears.
        assert!(!markdown.contains("a/agree"), "{markdown}");
    }

    #[test]
    fn gaps_markdown_carries_the_run_and_allowlist_lines() {
        let run = report(
            vec![case("a/gap", Verdict::Unsupported, Some("x"))],
            None,
        );
        let list = allowlist("a/gap  some_crate  why\n");
        let markdown = gaps_markdown(&run, &list);
        assert!(
            markdown.contains("Run: total=1 agree=0 mismatch=0 unsupported=1 skipped=0"),
            "{markdown}"
        );
        assert!(
            markdown
                .contains("Allowlist: (none) entries=1 allowed=1 unauthorized=0 stale=0"),
            "{markdown}"
        );
    }

    #[test]
    fn write_gaps_appends_the_divergences_ledger_when_present() {
        use crate::test_support::Scratch;
        let scratch = Scratch::new("write-gaps-present");
        let run = report(
            vec![case("a/gap", Verdict::Unsupported, Some("beam_rt: unsupported: x"))],
            None,
        );
        let list = allowlist("a/gap  crate_a  why\n");
        let ledger = "# Observed runtime divergences (handoffs)\n\n- a ledger line\n";
        std::fs::write(scratch.dir.join("divergences.md"), ledger).expect("write the ledger");

        let gaps_path = scratch.dir.join("GAPS.md");
        write_gaps(&gaps_path, &run, &list).expect("writes the gaps report");
        let written = std::fs::read_to_string(&gaps_path).expect("the report was written");

        // The pure renderer's table, then a blank line, then the ledger verbatim.
        assert_eq!(written, format!("{}\n{}", gaps_markdown(&run, &list), ledger));
        assert!(written.ends_with(ledger), "the document ends with the ledger: {written}");
    }

    #[test]
    fn write_gaps_omits_the_ledger_when_absent() {
        use crate::test_support::Scratch;
        let scratch = Scratch::new("write-gaps-absent");
        let run = report(
            vec![case("a/gap", Verdict::Unsupported, Some("beam_rt: unsupported: x"))],
            None,
        );
        let list = allowlist("a/gap  crate_a  why\n");

        let gaps_path = scratch.dir.join("GAPS.md");
        write_gaps(&gaps_path, &run, &list).expect("writes the gaps report");
        let written = std::fs::read_to_string(&gaps_path).expect("the report was written");

        // No ledger beside the report → byte-for-byte the pure renderer's output.
        assert_eq!(written, gaps_markdown(&run, &list));
    }
    #[test]
    fn report_from_suite_carries_the_ratchet_and_the_suite_exit_code_still_gates() {
        // A pure synthetic suite: build a real one through the harness is oracle-backed, so
        // exercise `report_from_suite` only through a constructed `SuiteOutcome`.
        use crate::harness::{RuntimeContext, SuiteOutcome};
        let suite = SuiteOutcome {
            cases: vec![
                suite_case("a/gap", Verdict::Unsupported),
                suite_case("a/agree_ish", Verdict::Skipped),
            ],
            context: RuntimeContext {
                oracle: spec(),
                rust: None,
            },
            strict: true,
            exit_code: 1,
        };
        let list = allowlist("a/gap  c  r\n");
        let report = report_from_suite(&suite, &list);
        let ratchet = report.ratchet.clone().expect("the report carries a ratchet");
        assert_eq!(ratchet.allowed, 1);
        assert_eq!(ratchet.unauthorized, 1);
        assert_eq!(ratchet.stale, 0);
        // An unauthorized gap → exit 1.
        assert_eq!(suite_exit_code(&report, true), 1);
        // Allowlisting the second gap makes the ratchet clean → exit 0.
        let list = allowlist("a/gap  c  r\na/agree_ish  c  r\n");
        let report = report_from_suite(&suite, &list);
        assert_eq!(suite_exit_code(&report, true), 0);
    }

    /// The same flow, but with a `Mismatch` in the suite whose id the allowlist records: the
    /// report's ratchet must NOT count it `allowed`, and the suite must still exit `1`.
    #[test]
    fn report_from_suite_never_lets_an_allowlisted_mismatch_pass() {
        use crate::harness::{RuntimeContext, SuiteOutcome};
        let suite = SuiteOutcome {
            cases: vec![
                suite_case("a/gap", Verdict::Unsupported),
                suite_case("a/bad", Verdict::Mismatch),
            ],
            context: RuntimeContext {
                oracle: spec(),
                rust: None,
            },
            strict: true,
            exit_code: 1,
        };
        // The allowlist records BOTH the real gap and the mismatching case.
        let list = allowlist("a/gap  c  r\na/bad  c  this must never tolerate a mismatch\n");
        let report = report_from_suite(&suite, &list);
        let ratchet = report.ratchet.clone().expect("the report carries a ratchet");
        assert_eq!(ratchet.allowed, 1, "only the real gap is allowed: {ratchet:?}");
        assert_eq!(ratchet.unauthorized, 0, "{ratchet:?}");
        assert_eq!(ratchet.stale, 1, "the mismatch id is stale, not allowed: {ratchet:?}");
        assert_eq!(report.summary.mismatch, 1, "the mismatch is counted");
        // No allowlist can turn the mismatch into a pass.
        assert_eq!(suite_exit_code(&report, true), 1);
        assert_eq!(suite_exit_code(&report, false), 1);
    }

    /// The `merge` route folds per-case reports into one suite-shaped report whose gap table
    /// is byte-identical to what `suite --gaps` renders from the same cases — the whole point
    /// of the cheap (no re-run) regeneration route.
    #[test]
    fn merge_folds_per_case_reports_and_gaps_markdown_lists_the_gap() {
        // Two `run --out-file` shape reports: one agreeing case and one capability gap, plus a
        // REDUNDANT re-run of the gap (its own report must be deduped, not double-counted).
        let agree = report(vec![case("a/agree", Verdict::Agree, None)], None);
        let gap = report(
            vec![case(
                "b/gap",
                Verdict::Unsupported,
                Some("beam_rt: unsupported: x"),
            )],
            None,
        );
        let merged = RunReport::merge(vec![agree, gap.clone(), gap], None).expect("merge");

        assert_eq!(merged.mode, "merge");
        assert_eq!(merged.schema, crate::report::SCHEMA);
        assert_eq!(merged.summary.total, 2, "the re-run is deduped");
        assert_eq!(merged.summary.agree, 1);
        assert_eq!(merged.summary.unsupported, 1);
        assert_eq!(merged.summary.mismatch, 0);
        // Cases are ordered by id, so the merged document is deterministic.
        assert_eq!(merged.cases[0].case_id, "a/agree");
        assert_eq!(merged.cases[1].case_id, "b/gap");
        // The per-contract rollup is recomputed from the merged cases.
        assert_eq!(merged.by_contract["A.1"].total, 2);
        assert!(merged.ratchet.is_none(), "no allowlist ⇒ no ratchet block");

        // With the gap recorded, the ratchet is clean and the generated table names it
        // `allowed`.
        let list = allowlist("b/gap  beam_bifs  a documented gap\n");
        let merged = RunReport::merge(
            vec![
                report(vec![case("a/agree", Verdict::Agree, None)], None),
                report(
                    vec![case(
                        "b/gap",
                        Verdict::Unsupported,
                        Some("beam_rt: unsupported: x"),
                    )],
                    None,
                ),
            ],
            Some(&list),
        )
        .expect("merge");
        let ratchet = merged.ratchet.clone().expect("the allowlist attached a ratchet");
        assert_eq!(
            (ratchet.allowed, ratchet.unauthorized, ratchet.stale),
            (1, 0, 0)
        );
        assert_eq!(suite_exit_code(&merged, true), 0, "a recorded gap is tolerated");

        let markdown = gaps_markdown(&merged, &list);
        assert!(
            markdown.contains("Run: total=2 agree=1 mismatch=0 unsupported=1 skipped=0"),
            "{markdown}"
        );
        assert!(
            markdown.contains(
                "| b/gap | unsupported | beam_bifs | allowed | beam_rt: unsupported: x |"
            ),
            "{markdown}"
        );
        assert!(!markdown.contains("| a/agree |"), "only gaps are listed: {markdown}");

        // An unrecorded gap is `unauthorized` ⇒ exit 1 (never silently tolerated).
        let empty = allowlist("# nothing recorded\n");
        let merged = RunReport::merge(
            vec![report(
                vec![case("b/gap", Verdict::Unsupported, Some("beam_rt: unsupported: x"))],
                None,
            )],
            Some(&empty),
        )
        .expect("merge");
        assert_eq!(merged.ratchet.as_ref().expect("ratchet").unauthorized, 1);
        assert_eq!(suite_exit_code(&merged, true), 1);

        // An empty fold is a loud usage error, never an empty pass.
        assert!(RunReport::merge(Vec::new(), None).is_err());
    }
}
