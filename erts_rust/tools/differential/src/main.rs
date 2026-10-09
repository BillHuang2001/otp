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

use std::path::PathBuf;

use differential::cli::{self, Command};
use differential::error::{Error, Result};
use differential::report::{self, ListReport, RunReport};
use differential::{allowlist::Allowlist, corpus, harness, ratchet};

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
        Command::Run { case_id, json, out } => {
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
            emit_report(&RunReport::from_case("compare", &outcome), json, out)?;
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
            emit_report(&report, json, out)?;
            if let Some(gaps_path) = &gaps {
                let empty = Allowlist::empty();
                let list = list.as_ref().unwrap_or(&empty);
                ratchet::write_gaps(gaps_path, &report, list)?;
                eprintln!("differential: wrote {}", gaps_path.display());
            }
            Ok(ratchet::suite_exit_code(&report, strict))
        }
    }
}

/// Write the report and print it (`--json` or human text).
///
/// The report file is always written — `<dir>/report.json` with `--out <dir>`, and the
/// crate's gitignored `out/report.json` otherwise (the documented `--out` default).
fn emit_report(report: &RunReport, json: bool, out: Option<PathBuf>) -> Result<()> {
    let dir = out.unwrap_or_else(report::default_out_dir);
    let path = report::write_json(report, Some(&dir))?;
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
