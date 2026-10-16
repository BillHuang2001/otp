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

//! The `differential` binary: parse the command line, dispatch, print and exit.
//!
//! Everything of substance lives in the library — this is only the shell that turns a
//! [`Command`](differential::cli::Command) into output plus a process exit code
//! (`0` agreed, `1` mismatch, `2` usage/env/IO error).
//!
//! This shell adds exactly one thing the library must not do: before a command whose cases
//! need the external **live-distribution helper** (`dist_probe`, the Rust side of the four
//! `dist/live_*` command cases) runs, it makes that helper available by building it on demand
//! **once** ([`differential::runtime::ensure_helper`]). Without it the cases would report a
//! loud `skipped` in a tree where only `beam_rt` and `differential` were built. The build is a
//! last resort — a resolvable `$BEAM_DIST_PROBE` or a built workspace artifact always wins —
//! and `$DIFFERENTIAL_NO_BUILD` (any non-empty value) switches it off. A failed build is never
//! fatal: the affected cases stay a loud `skipped` naming cargo's diagnosis.

use std::path::{Path, PathBuf};

use differential::cli::{self, Command};
use differential::corpus::{self, CaseKind, CommandProgram};
use differential::error::{Error, Result};
use differential::report::{self, ListReport, RunReport};
use differential::runtime::{self, HelperStatus};
use differential::{allowlist::Allowlist, harness, ratchet};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(run(&args));
}

/// Run the parsed command, returning the process exit code (never panicking).
fn run(args: &[String]) -> i32 {
    let command = match cli::parse(args) {
        Ok(command) => command,
        Err(error) => return report_error(&error),
    };
    match dispatch(command) {
        Ok(code) => code,
        Err(error) => report_error(&error),
    }
}

/// Print a tool-level failure on stderr and return its exit code (2).
fn report_error(error: &Error) -> i32 {
    eprintln!("differential: {error}");
    error.exit_code()
}

fn dispatch(command: Command) -> Result<i32> {
    ensure_helper(&command)?;
    match command {
        Command::Help { topic } => {
            print!("{}", cli::usage(topic.as_deref()));
            Ok(0)
        }
        Command::List {
            by_contract,
            json,
            out,
        } => {
            let list = if by_contract {
                ListReport::from_corpus_grouped()
            } else {
                ListReport::from_corpus()
            };
            emit_list(&list, json, out)?;
            Ok(0)
        }
        Command::Run {
            case_id,
            json,
            out,
            out_file,
        } => {
            let case = corpus::find_case(&case_id).ok_or_else(|| {
                Error::usage(format!(
                    "unknown case id '{case_id}' (see `differential list` for the corpus)"
                ))
            })?;
            let outcome = harness::run_case(case)?;
            emit_report(
                &RunReport::from_case("run", &outcome),
                json,
                out,
                out_file,
            )?;
            Ok(outcome.exit_code())
        }
        Command::Compare {
            expr,
            pa,
            json,
            out,
        } => {
            let outcome = harness::compare_expr(&expr, &pa)?;
            emit_report(&RunReport::from_case("compare", &outcome), json, out, None)?;
            Ok(outcome.exit_code())
        }
        Command::Suite {
            strict,
            allowlist,
            gaps,
            json,
            out,
        } => {
            // Load the allowlist FIRST: a missing/unreadable file is a fast usage/env exit 2
            // with no corpus run.
            let list = match &allowlist {
                Some(path) => Some(Allowlist::load(path)?),
                None => None,
            };
            let suite = harness::run_suite(strict)?;
            let report = match &list {
                Some(list) => ratchet::report_from_suite(&suite, list),
                None => RunReport::from_suite(&suite),
            };
            emit_report(&report, json, out, None)?;
            if let Some(gaps_path) = &gaps {
                let empty = Allowlist::empty();
                let list = list.as_ref().unwrap_or(&empty);
                ratchet::write_gaps(gaps_path, &report, list)?;
                eprintln!("differential: wrote {}", gaps_path.display());
            }
            Ok(ratchet::suite_exit_code(&report, strict))
        }
        Command::Merge {
            from,
            allowlist,
            gaps,
            json,
            out,
            out_file,
        } => {
            // Fold the per-case `run --out-file` reports into ONE suite-shaped report, then
            // classify the gaps and (optionally) regenerate `GAPS.md` with the SAME renderer
            // `suite --gaps` uses, so the generated document is identical. This never runs a
            // case, so it is the cheap route to regenerate the document.
            let reports = read_reports(&from)?;
            let list = match &allowlist {
                Some(path) => Some(Allowlist::load(path)?),
                None => None,
            };
            let report = RunReport::merge(reports, list.as_ref())?;
            emit_report(&report, json, out, out_file)?;
            if let Some(gaps_path) = &gaps {
                let empty = Allowlist::empty();
                let list = list.as_ref().unwrap_or(&empty);
                ratchet::write_gaps(gaps_path, &report, list)?;
                eprintln!("differential: wrote {}", gaps_path.display());
            }
            Ok(ratchet::suite_exit_code(&report, true))
        }
    }
}

/// Read every `*.json` report directly under `dir` and parse each as a run report.
///
/// Files are visited in sorted path order so the fold is deterministic. A `--from` that is
/// not a readable directory, an empty directory, or a `*.json` file that does not parse as a
/// `differential/1` report is a loud usage error (exit 2) naming the offender — never a
/// silently skipped file.
fn read_reports(dir: &Path) -> Result<Vec<RunReport>> {
    let entries = std::fs::read_dir(dir).map_err(|source| {
        Error::usage(format!(
            "--from '{}' is not a readable directory: {source}",
            dir.display()
        ))
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| {
            Error::usage(format!(
                "reading the directory '{}': {source}",
                dir.display()
            ))
        })?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
            && path.is_file()
        {
            paths.push(path);
        }
    }
    if paths.is_empty() {
        return Err(Error::usage(format!(
            "no *.json reports found under '{}' (write them with `run <case-id> --out-file <path>`)",
            dir.display()
        )));
    }
    paths.sort();
    let mut reports = Vec::with_capacity(paths.len());
    for path in paths {
        reports.push(RunReport::read_json(&path)?);
    }
    Ok(reports)
}

/// Make the live-distribution helper (`dist_probe`) available when the command can need it.
///
/// [`needs_helper`] decides whether the command can spawn the helper at all — only the four
/// `dist/live_*` command cases do — because the on-demand build ([`runtime::ensure_helper`])
/// must stay a last resort: a `run` of any other case, a `compare`, a `list` or a `help` never
/// touches the build system, and a **usage** error (an unknown case id) never becomes a slow
/// build either.
///
/// A helper that had to be built is announced on stderr, and one that could not be obtained
/// (a failed build, or the `$DIFFERENTIAL_NO_BUILD` opt-out) is announced loudly here as well
/// as in the affected cases' own `skipped` reason — a skip is never silent. Neither outcome is
/// a tool error: the run continues and reports the cases honestly.
fn ensure_helper(command: &Command) -> Result<()> {
    if !needs_helper(command) {
        return Ok(());
    }
    match runtime::ensure_helper()? {
        HelperStatus::Available {
            spec,
            built_on_demand: true,
        } => eprintln!(
            "differential: built the distribution helper on demand: {}",
            spec.program.display()
        ),
        HelperStatus::Available {
            built_on_demand: false,
            ..
        } => {}
        HelperStatus::Unavailable(reason) => {
            eprintln!("differential: the distribution helper is unavailable: {reason}");
        }
    }
    Ok(())
}

/// Whether `command` can spawn the external `dist_probe` helper.
///
/// The helper is the Rust side of the `dist/live_*` [`CaseKind::Command`] cases (contract A.5),
/// so only a `run` of such a case or a `suite` over a corpus containing one can need it. A
/// `run` whose case id is unknown is a usage error and is deliberately *not* a trigger.
fn needs_helper(command: &Command) -> bool {
    match command {
        Command::Run { case_id, .. } => corpus::find_case(case_id).is_some_and(case_needs_helper),
        Command::Suite { .. } => corpus::cases().iter().any(case_needs_helper),
        Command::Compare { .. } | Command::List { .. } | Command::Merge { .. } | Command::Help { .. } => {
            false
        }
    }
}

/// Whether either side of a command case is the `dist_probe` helper.
fn case_needs_helper(case: &corpus::Case) -> bool {
    match &case.kind {
        CaseKind::Command(run) => [run.oracle.program, run.rust.program]
            .into_iter()
            .any(|program| matches!(program, CommandProgram::Helper)),
        _ => false,
    }
}

/// Write the report and print it (`--json` or human text).
///
/// The report file is always written — `<dir>/report.json` with `--out <dir>` (or, with
/// `--out-file <path>`, **exactly** that path), and the crate's gitignored `out/report.json`
/// otherwise (the documented `--out` default).
fn emit_report(
    report: &RunReport,
    json: bool,
    out: Option<PathBuf>,
    out_file: Option<PathBuf>,
) -> Result<()> {
    let path = match out_file {
        Some(path) => report.write_json_to(&path)?,
        None => {
            let dir = out.unwrap_or_else(report::default_out_dir);
            report::write_json(report, Some(&dir))?
        }
    };
    eprintln!("differential: wrote {}", path.display());
    if json {
        println!("{}", report.to_json_string()?);
    } else {
        let mut stdout = std::io::stdout();
        report::render_human(report, &mut stdout)
            .map_err(|source| Error::io("failed to write the report", source))?;
    }
    Ok(())
}

/// The `list` command: corpus ids, contracts and descriptions (no runtime involved).
///
/// The plain view prints one line per case. When the listing carries the per-contract
/// grouping (`list --by-contract`) the human view is grouped by contract instead.
fn emit_list(list: &ListReport, json: bool, out: Option<PathBuf>) -> Result<()> {
    let dir = out.unwrap_or_else(report::default_out_dir);
    let path = report::write_json_value(list, Some(&dir))?;
    eprintln!("differential: wrote {}", path.display());
    if json {
        println!("{}", report::to_json_string(list)?);
    } else if list.by_contract.is_some() {
        let mut stdout = std::io::stdout();
        report::render_list_grouped(list, &mut stdout)
            .map_err(|source| Error::io("failed to write the listing", source))?;
    } else {
        for entry in &list.cases {
            let contracts: Vec<&str> = entry.contracts.iter().map(|entry| entry.id).collect();
            println!(
                "{} — {} — {}",
                entry.case_id,
                contracts.join(","),
                entry.description
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::needs_helper;
    use differential::cli::Command;

    /// A `run` command for `case_id` (the only field the helper decision reads is the id).
    fn run(case_id: &str) -> Command {
        Command::Run {
            case_id: case_id.to_string(),
            json: false,
            out: None,
            out_file: None,
        }
    }

    fn suite() -> Command {
        Command::Suite {
            strict: false,
            allowlist: None,
            gaps: None,
            json: false,
            out: None,
        }
    }

    /// A `merge` command (a pure report-folding command: it never spawns a runtime).
    fn merge() -> Command {
        Command::Merge {
            from: std::path::PathBuf::from("reports"),
            allowlist: None,
            gaps: None,
            json: false,
            out: None,
            out_file: None,
        }
    }

    /// The on-demand build is triggered **only** by a command that can spawn the helper: the
    /// four `dist/live_*` command cases, or a `suite` over the corpus that contains them. An
    /// expression case, a `compare`, a `list`, a `help` and an unknown id never touch the build
    /// system (an unknown id must stay a fast, pure usage error).
    #[test]
    fn only_helper_backed_commands_can_trigger_the_on_demand_build() {
        for case_id in [
            "dist/live_handshake_ping",
            "dist/live_node_visibility",
            "dist/live_term_delivery",
            "dist/live_utf8_atom_round_trip",
        ] {
            assert!(needs_helper(&run(case_id)), "`{case_id}` spawns the helper");
        }

        assert!(
            !needs_helper(&run("etf/term_to_binary_tuple")),
            "an expression case never needs the helper"
        );
        assert!(
            !needs_helper(&run("no/such-case")),
            "an unknown case id stays a pure usage error"
        );
        assert!(
            needs_helper(&suite()),
            "the live-distribution cases are part of the corpus"
        );
        assert!(!needs_helper(&Command::List {
            by_contract: false,
            json: false,
            out: None,
        }));
        assert!(!needs_helper(&Command::Help { topic: None }));
        assert!(!needs_helper(&Command::Help {
            topic: Some("suite".to_string()),
        }));
        assert!(!needs_helper(&Command::Compare {
            expr: "1 + 1".to_string(),
            pa: Vec::new(),
            json: false,
            out: None,
        }));
        assert!(
            !needs_helper(&merge()),
            "`merge` only folds reports: it can never spawn the helper"
        );
    }
}
