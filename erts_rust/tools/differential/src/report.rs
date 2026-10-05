//! Machine-readable reports: a stable JSON schema plus a human rendering.
//!
//! The JSON schema is versioned by the `schema` field ([`SCHEMA`]) and only ever grows.
//! An observable that does not apply is `null` (`stdout`/`stdout_hex`/`exit_code` for a
//! side that did not run, `matched_golden` where no golden was asserted), and the two
//! optional keys are omitted rather than guessed: `reason` only appears for an
//! `unsupported` side and `matched_golden` only where a golden was actually checked.
//! Reports are written under the crate's gitignored `out/` directory unless `--out <dir>`
//! overrides it.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::compare::{FieldDiff, SideResult, Verdict};
use crate::corpus::{self, Case, Contract};
use crate::error::{Error, Result};
use crate::harness::{CaseOutcome, SuiteOutcome};
use crate::runtime::{RuntimeId, RuntimeSpec};

/// The report schema identifier (bump on any backwards-incompatible change).
pub const SCHEMA: &str = "differential/1";

/// The schema identifier of the `list` listing.
pub const LIST_SCHEMA: &str = "differential/list/1";

/// The whole run report: the resolved runtimes, the summary counts and every case.
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    /// Always [`SCHEMA`].
    pub schema: &'static str,
    /// `"run"`, `"compare"` or `"suite"`.
    pub mode: &'static str,
    /// The oracle runtime that was used.
    pub oracle: RuntimeReport,
    /// The Rust runtime, whether or not it exists.
    pub rust: RuntimeReport,
    /// Verdict counts over `cases`.
    pub summary: Summary,
    /// One entry per executed case, in execution order.
    pub cases: Vec<CaseReport>,
}

impl RunReport {
    /// Build a report from resolved runtimes and per-case reports.
    pub fn new(
        mode: &'static str,
        oracle: &RuntimeSpec,
        rust: Option<&RuntimeSpec>,
        cases: Vec<CaseReport>,
    ) -> RunReport {
        let summary = Summary::of(&cases);
        RunReport {
            schema: SCHEMA,
            mode,
            oracle: RuntimeReport::of(RuntimeId::Oracle, Some(oracle)),
            rust: RuntimeReport::of(RuntimeId::Rust, rust),
            summary,
            cases,
        }
    }

    /// Build a one-case report (the `run` and `compare` commands).
    pub fn from_case(mode: &'static str, outcome: &CaseOutcome) -> RunReport {
        RunReport::new(
            mode,
            &outcome.oracle_spec,
            outcome.rust_spec.as_ref(),
            vec![CaseReport::from_outcome(outcome)],
        )
    }

    /// Build the report of a whole suite run.
    pub fn from_suite(suite: &SuiteOutcome) -> RunReport {
        RunReport::new(
            "suite",
            &suite.context.oracle,
            suite.context.rust.as_ref(),
            suite.cases.iter().map(CaseReport::from_outcome).collect(),
        )
    }

    /// Render this report as pretty JSON (the same bytes `--json` prints).
    pub fn to_json_string(&self) -> Result<String> {
        to_json_string(self)
    }
}

/// One runtime's identity and availability.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeReport {
    /// `"oracle"` or `"rust"`.
    pub id: &'static str,
    /// Whether this runtime was resolved (and therefore observed).
    pub available: bool,
    /// The resolved program path, if any.
    pub program: Option<String>,
    /// How the program was resolved (`"$PATH"`, `"$ERTS_ORACLE"`, `"workspace-default"`).
    pub source: Option<&'static str>,
}

impl RuntimeReport {
    fn of(id: RuntimeId, spec: Option<&RuntimeSpec>) -> RuntimeReport {
        match spec {
            Some(spec) => RuntimeReport {
                id: id.id(),
                available: true,
                program: Some(spec.program.display().to_string()),
                source: Some(spec.source.as_str()),
            },
            None => RuntimeReport {
                id: id.id(),
                available: false,
                program: None,
                source: None,
            },
        }
    }
}

/// Verdict counts over a set of case reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Number of cases in the report.
    pub total: usize,
    /// Cases where both sides agreed.
    pub agree: usize,
    /// Cases where the sides (or the oracle and its golden) disagreed.
    pub mismatch: usize,
    /// Cases where a side could not be observed.
    pub unsupported: usize,
    /// Cases that were intentionally not executed.
    pub skipped: usize,
}

impl Summary {
    /// Count the verdicts of `cases`.
    pub fn of(cases: &[CaseReport]) -> Summary {
        let mut summary = Summary {
            total: cases.len(),
            agree: 0,
            mismatch: 0,
            unsupported: 0,
            skipped: 0,
        };
        for case in cases {
            match case.verdict {
                Verdict::Agree => summary.agree += 1,
                Verdict::Mismatch => summary.mismatch += 1,
                Verdict::Unsupported => summary.unsupported += 1,
                Verdict::Skipped => summary.skipped += 1,
            }
        }
        summary
    }
}

/// One case's report entry.
#[derive(Debug, Clone, Serialize)]
pub struct CaseReport {
    /// The case id.
    pub case_id: String,
    /// The case description.
    pub description: String,
    /// The compatibility contracts covered (serialized as `"A.1"`…`"A.9"`).
    pub contracts: Vec<Contract>,
    /// The probe expression handed to both runtimes.
    pub eval: String,
    /// The verdict (`"agree"`, `"mismatch"`, `"unsupported"`, `"skipped"`).
    pub verdict: Verdict,
    /// The oracle side.
    pub oracle: SideReport,
    /// The Rust side.
    pub rust: SideReport,
    /// Field-by-field differences between the two observed sides (empty unless both ran).
    pub diffs: Vec<FieldDiff>,
}

impl CaseReport {
    /// Project a [`CaseOutcome`] onto the report schema.
    pub fn from_outcome(outcome: &CaseOutcome) -> CaseReport {
        let matched_golden = if outcome.golden_asserted {
            Some(outcome.golden.matched)
        } else {
            None
        };
        CaseReport {
            case_id: outcome.case_id.clone(),
            description: outcome.description.clone(),
            contracts: outcome.contracts.clone(),
            eval: outcome.eval.clone(),
            verdict: outcome.verdict,
            oracle: SideReport::of(&outcome.oracle, matched_golden),
            rust: SideReport::of(&outcome.rust, None),
            diffs: outcome.diffs.clone(),
        }
    }
}

/// One side's observation, as reported.
#[derive(Debug, Clone, Serialize)]
pub struct SideReport {
    /// Whether the side ran at all.
    pub available: bool,
    /// Why the side did not run (key omitted when the side ran).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Captured stdout as text (`null` when the side did not run).
    pub stdout: Option<String>,
    /// Captured stdout as dependency-free lowercase hex (`null` when the side did not run).
    pub stdout_hex: Option<String>,
    /// The process exit code (`null` when the side did not run, or when signalled).
    pub exit_code: Option<i32>,
    /// Whether the observation reproduced the case golden (key omitted when no golden was
    /// asserted, and for the Rust side).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_golden: Option<bool>,
}

impl SideReport {
    fn of(side: &SideResult, matched_golden: Option<bool>) -> SideReport {
        match side {
            SideResult::Observed(observation) => SideReport {
                available: true,
                reason: None,
                stdout: Some(String::from_utf8_lossy(&observation.stdout).into_owned()),
                stdout_hex: Some(crate::compare::hex_string(&observation.stdout)),
                exit_code: observation.exit_code,
                matched_golden,
            },
            SideResult::Unsupported { reason } => SideReport {
                available: false,
                reason: Some(reason.clone()),
                stdout: None,
                stdout_hex: None,
                exit_code: None,
                matched_golden: None,
            },
        }
    }
}

/// The `list` listing: the corpus, its contracts and its prerequisites.
#[derive(Debug, Clone, Serialize)]
pub struct ListReport {
    /// Always [`LIST_SCHEMA`].
    pub schema: &'static str,
    /// The number of cases.
    pub total: usize,
    /// One entry per corpus case, in corpus order.
    pub cases: Vec<ListEntry>,
}

impl ListReport {
    /// Build the listing of the built-in corpus.
    pub fn from_corpus() -> ListReport {
        let cases = corpus::cases();
        ListReport {
            schema: LIST_SCHEMA,
            total: cases.len(),
            cases: cases.iter().map(ListEntry::of).collect(),
        }
    }
}

/// One corpus entry in the `list` listing.
#[derive(Debug, Clone, Serialize)]
pub struct ListEntry {
    /// The case id.
    pub case_id: &'static str,
    /// The case description.
    pub description: &'static str,
    /// The probe wrapper used.
    pub probe: &'static str,
    /// The contracts covered, with their labels.
    pub contracts: Vec<ContractEntry>,
}

impl ListEntry {
    fn of(case: &Case) -> ListEntry {
        ListEntry {
            case_id: case.id,
            description: case.description,
            probe: case.probe.as_str(),
            contracts: case
                .contracts
                .iter()
                .map(|contract| ContractEntry {
                    id: contract.id(),
                    label: contract.label(),
                })
                .collect(),
        }
    }
}

/// A contract id with its human label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ContractEntry {
    /// The stable id (`"A.1"`…`"A.9"`).
    pub id: &'static str,
    /// The human label.
    pub label: &'static str,
}

/// The default output directory: the crate's gitignored `out/`.
pub fn default_out_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("out")
}

/// Write a run report as `<dir>/report.json`, creating `dir` if needed.
///
/// `out == None` writes to [`default_out_dir`]. Returns the file that was written.
pub fn write_json(report: &RunReport, out: Option<&Path>) -> Result<PathBuf> {
    write_json_value(report, out)
}

/// Write any serializable report as `<dir>/report.json` (the `list` command uses this).
pub fn write_json_value<T: Serialize>(value: &T, out: Option<&Path>) -> Result<PathBuf> {
    let dir = match out {
        Some(dir) => dir.to_path_buf(),
        None => default_out_dir(),
    };
    std::fs::create_dir_all(&dir).map_err(|source| {
        Error::io(
            format!("failed to create the report directory '{}'", dir.display()),
            source,
        )
    })?;
    let path = dir.join("report.json");
    let json = to_json_string(value)?;
    std::fs::write(&path, json).map_err(|source| {
        Error::io(
            format!("failed to write the report '{}'", path.display()),
            source,
        )
    })?;
    Ok(path)
}

/// Serialize a value as pretty JSON, mapping failures onto a tool error.
pub fn to_json_string<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string_pretty(value).map_err(|source| {
        Error::io(
            "failed to serialize the report as JSON",
            std::io::Error::new(std::io::ErrorKind::Other, source),
        )
    })
}

/// Render a run report for a human reader.
pub fn render_human(report: &RunReport, out: &mut impl Write) -> std::io::Result<()> {
    writeln!(
        out,
        "differential report — schema {} — mode {}",
        report.schema, report.mode
    )?;
    render_runtime(out, "oracle", &report.oracle)?;
    render_runtime(out, "rust", &report.rust)?;
    let summary = &report.summary;
    writeln!(
        out,
        "summary: total={} agree={} mismatch={} unsupported={} skipped={}",
        summary.total, summary.agree, summary.mismatch, summary.unsupported, summary.skipped
    )?;
    if report.cases.is_empty() {
        writeln!(out, "(no cases)")?;
        return Ok(());
    }
    for case in &report.cases {
        let contracts: Vec<&str> = case.contracts.iter().map(Contract::id).collect();
        let contracts = if contracts.is_empty() {
            String::new()
        } else {
            format!(" ({})", contracts.join(","))
        };
        writeln!(
            out,
            "\n[{}] {}{}",
            case.verdict.as_str().to_uppercase(),
            case.case_id,
            contracts
        )?;
        writeln!(out, "    {}", case.description)?;
        render_side(out, "oracle", &case.oracle)?;
        render_side(out, "rust  ", &case.rust)?;
        for diff in &case.diffs {
            writeln!(
                out,
                "    diff {}: oracle={:?} rust={:?}",
                diff.field, diff.oracle, diff.rust
            )?;
        }
    }
    Ok(())
}

fn render_runtime(
    out: &mut impl Write,
    label: &str,
    runtime: &RuntimeReport,
) -> std::io::Result<()> {
    match (&runtime.program, runtime.source) {
        (Some(program), Some(source)) if runtime.available => {
            writeln!(out, "{label}: {program} ({source})")
        }
        _ => writeln!(out, "{label}: unavailable"),
    }
}

fn render_side(out: &mut impl Write, label: &str, side: &SideReport) -> std::io::Result<()> {
    if !side.available {
        return writeln!(
            out,
            "    {label}: unsupported: {}",
            side.reason.as_deref().unwrap_or("unknown reason")
        );
    }
    let stdout = side.stdout.as_deref().unwrap_or("");
    let golden = match side.matched_golden {
        Some(true) => "golden=ok",
        Some(false) => "golden=MISMATCH",
        None => "golden=not-asserted",
    };
    writeln!(
        out,
        "    {label}: exit {} {golden} stdout={stdout:?}",
        side.exit_code
            .map(|code| code.to_string())
            .unwrap_or_else(|| "signal".to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::{
        default_out_dir, render_human, to_json_string, write_json_value, CaseReport, ListReport,
        RunReport, Summary, SCHEMA,
    };
    use crate::compare::{FieldDiff, GoldenCheck, SideResult, Verdict};
    use crate::corpus::{find_case, Contract};
    use crate::harness::run_case;
    use crate::runtime::{Observation, RuntimeId, RuntimeSource, RuntimeSpec};
    use std::path::PathBuf;

    fn spec(id: RuntimeId, program: &str, source: RuntimeSource) -> RuntimeSpec {
        RuntimeSpec {
            id,
            program: PathBuf::from(program),
            source,
        }
    }

    fn observation(stdout: &[u8], exit_code: Option<i32>) -> Observation {
        Observation {
            runtime: RuntimeId::Rust,
            program: PathBuf::from("/x/beam_rt"),
            argv: vec!["-noshell".to_string()],
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit_code,
        }
    }

    fn report(verdict: Verdict, rust: SideResult, matched_golden: Option<bool>) -> RunReport {
        let oracle = spec(RuntimeId::Oracle, "/x/erl", RuntimeSource::Path);
        let outcome = crate::harness::CaseOutcome {
            case_id: "etf/term_to_binary_tuple".to_string(),
            description: "a case".to_string(),
            contracts: vec![Contract::BeamAsm],
            eval: "try ({ok,42}) of V -> ok end.".to_string(),
            argv: vec!["-noshell".to_string()],
            oracle: SideResult::observed(observation(b"ok <<131,97,42>>\n", Some(0))),
            rust,
            golden: GoldenCheck {
                matched: matched_golden.unwrap_or(true),
                diffs: Vec::new(),
            },
            golden_asserted: matched_golden.is_some(),
            verdict,
            diffs: vec![FieldDiff::new("stdout", "ok 1", "ok 2")],
            oracle_spec: oracle.clone(),
            rust_spec: None,
        };
        RunReport::new(
            "run",
            &oracle,
            None,
            vec![CaseReport::from_outcome(&outcome)],
        )
    }

    #[test]
    fn the_schema_is_stable_and_complete() {
        let report = report(Verdict::Unsupported, SideResult::unsupported("no beam_rt"), Some(true));
        assert_eq!(report.schema, SCHEMA);
        let json: serde_json::Value = serde_json::from_str(&report.to_json_string().unwrap()).unwrap();
        assert_eq!(json["schema"], "differential/1");
        assert_eq!(json["mode"], "run");
        assert_eq!(json["oracle"]["available"], true);
        assert_eq!(json["oracle"]["program"], "/x/erl");
        assert_eq!(json["oracle"]["source"], "$PATH");
        assert_eq!(json["rust"]["available"], false);
        assert_eq!(json["rust"]["program"], serde_json::Value::Null);
        assert_eq!(json["summary"]["total"], 1);
        assert_eq!(json["summary"]["unsupported"], 1);
        let case = &json["cases"][0];
        assert_eq!(case["case_id"], "etf/term_to_binary_tuple");
        assert_eq!(case["contracts"][0], "A.2");
        assert_eq!(case["verdict"], "unsupported");
        assert!(case["eval"].as_str().unwrap().starts_with("try ({ok,42})"));
        assert_eq!(case["oracle"]["available"], true);
        assert_eq!(case["oracle"]["matched_golden"], true);
        assert_eq!(case["oracle"]["exit_code"], 0);
        assert_eq!(case["oracle"]["stdout_hex"], "6f6b203c3c3133312c39372c34323e3e0a");
        assert_eq!(case["rust"]["available"], false);
        assert!(case["rust"]["reason"].as_str().unwrap().contains("beam_rt"));
        assert_eq!(case["diffs"][0]["field"], "stdout");
    }

    #[test]
    fn optional_side_keys_are_omitted_rather_than_guessed() {
        let report = report(Verdict::Unsupported, SideResult::unsupported("no beam_rt"), None);
        let json: serde_json::Value = serde_json::from_str(&report.to_json_string().unwrap()).unwrap();
        let oracle = &json["cases"][0]["oracle"];
        assert!(
            oracle.get("matched_golden").is_none(),
            "no golden was asserted: {oracle}"
        );
        assert!(oracle.get("reason").is_none(), "the oracle ran: {oracle}");
        let rust = &json["cases"][0]["rust"];
        assert_eq!(rust["available"], false);
        assert!(rust["reason"].as_str().unwrap().contains("beam_rt"));
        assert!(
            rust.get("matched_golden").is_none(),
            "the Rust side never checks a golden: {rust}"
        );
        assert!(rust["stdout"].is_null(), "{rust}");
        assert!(rust["exit_code"].is_null(), "{rust}");
    }

    #[test]
    fn summary_counts_every_verdict() {
        let mut cases = Vec::new();
        for verdict in [
            Verdict::Agree,
            Verdict::Mismatch,
            Verdict::Unsupported,
            Verdict::Skipped,
        ] {
            let mut base = report(verdict, SideResult::unsupported("x"), Some(true));
            cases.append(&mut base.cases);
        }
        let summary = Summary::of(&cases);
        assert_eq!(summary.total, 4);
        assert_eq!(summary.agree, 1);
        assert_eq!(summary.mismatch, 1);
        assert_eq!(summary.unsupported, 1);
        assert_eq!(summary.skipped, 1);
    }

    #[test]
    fn list_report_describes_the_corpus() {
        let list = ListReport::from_corpus();
        assert_eq!(list.schema, "differential/list/1");
        assert_eq!(list.total, crate::corpus::cases().len());
        assert!(list.total > 0);
        let json: serde_json::Value = serde_json::from_str(&to_json_string(&list).unwrap()).unwrap();
        assert_eq!(json["total"], list.total);
        assert_eq!(json["cases"][0]["contracts"][0]["id"], "A.2");
        assert_eq!(json["cases"][0]["contracts"][0]["label"], ".beam binary/ETF bytes");
        assert!(!json["cases"][0]["probe"].as_str().unwrap().is_empty());
    }

    #[test]
    fn write_json_defaults_to_the_gitignored_out_dir() {
        let expected = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("out");
        assert_eq!(default_out_dir(), expected);
    }

    #[test]
    fn write_json_writes_a_file_in_the_requested_directory() {
        let dir = std::env::temp_dir().join(format!(
            "differential-report-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let list = ListReport::from_corpus();
        let path = write_json_value(&list, Some(&dir)).expect("write");
        assert_eq!(path, dir.join("report.json"));
        let written = std::fs::read_to_string(&path).expect("read back");
        assert!(written.contains("\"schema\": \"differential/list/1\""));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn render_human_is_readable_without_a_rust_runtime() {
        let report = report(Verdict::Unsupported, SideResult::unsupported("no beam_rt"), Some(true));
        let mut buf = Vec::new();
        render_human(&report, &mut buf).expect("render");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("schema differential/1"));
        assert!(text.contains("oracle: /x/erl ($PATH)"));
        assert!(text.contains("rust: unavailable"));
        assert!(text.contains("total=1"));
        assert!(text.contains("[UNSUPPORTED] etf/term_to_binary_tuple (A.2)"));
        assert!(text.contains("unsupported: no beam_rt"));
        assert!(text.contains("golden=ok"));
    }

    /// The report of a real (oracle-backed) run must never claim agreement for the stub
    /// side. Skipped loudly when the oracle is unavailable.
    #[test]
    fn a_real_run_never_reports_fabricated_agreement() {
        let case = find_case("etf/term_to_binary_tuple").expect("case");
        let outcome = match run_case(case) {
            Ok(outcome) => outcome,
            Err(err) => {
                eprintln!("SKIP: oracle erl not available ({err})");
                return;
            }
        };
        let report = RunReport::from_case("run", &outcome);
        assert_eq!(report.schema, SCHEMA);
        assert_ne!(report.cases[0].verdict, Verdict::Agree);
        assert_eq!(report.summary.agree, 0);
    }
}
