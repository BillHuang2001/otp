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

//! Hand-rolled command-line parsing and help text.
//!
//! No `clap`, no `structopt` — the crate deliberately stays within its
//! `serde`/`serde_json`/`beam_common` dependency budget. The parser is small, explicit
//! and fully unit-tested; every subcommand supports `--help`/`-h` (and the top-level
//! `help [<sub>]`), which always yields [`Command::Help`] so the binary can print the
//! relevant text and exit `0`.

use std::path::PathBuf;

use crate::error::{BenchError, Result};
use crate::report::Profile;

/// Which runtime(s) a `run` invocation should exercise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeSel {
    /// The system oracle only (the default).
    Sys,
    /// The Rust runtime only.
    Rust,
    /// Both, in that order.
    All,
}

impl RuntimeSel {
    /// Whether the system oracle is part of the selection.
    pub fn wants_sys(&self) -> bool {
        matches!(self, RuntimeSel::Sys | RuntimeSel::All)
    }

    /// Whether the Rust runtime is part of the selection.
    pub fn wants_rust(&self) -> bool {
        matches!(self, RuntimeSel::Rust | RuntimeSel::All)
    }

    fn parse(value: &str) -> Result<RuntimeSel> {
        match value {
            "sys" => Ok(RuntimeSel::Sys),
            "rust" => Ok(RuntimeSel::Rust),
            "all" => Ok(RuntimeSel::All),
            other => Err(BenchError::usage(format!(
                "invalid --runtime `{other}` (expected sys|rust|all)"
            ))),
        }
    }
}

/// The arguments of a `bench run` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    /// The profile (default `Quick`).
    pub profile: Profile,
    /// The explicit workload id list, or `None` for "all".
    pub workloads: Option<Vec<String>>,
    /// Which runtime(s) to schedule.
    pub runtime: RuntimeSel,
    /// Output directory override.
    pub out: Option<PathBuf>,
    /// Exact output file path override (the report is written here verbatim).
    pub out_file: Option<PathBuf>,
    /// Emit the JSON report to stdout as well.
    pub json: bool,
    /// Extra progress output.
    pub verbose: bool,
    /// Per-workload external-repo overrides (`--repo <id>=<path>`).
    pub repos: Vec<(String, PathBuf)>,
}

impl Default for RunArgs {
    fn default() -> Self {
        RunArgs {
            profile: Profile::Quick,
            workloads: None,
            runtime: RuntimeSel::Sys,
            out: None,
            out_file: None,
            json: false,
            verbose: false,
            repos: Vec::new(),
        }
    }
}

/// A parsed command-line invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `bench list [--json]`.
    List {
        /// Emit machine-readable JSON.
        json: bool,
    },
    /// `bench run [flags]`.
    Run(RunArgs),
    /// `bench baseline [--quick|--full] [--json] [--out <dir>] [--out-file <path>]`.
    Baseline {
        /// The profile.
        profile: Profile,
        /// Emit JSON.
        json: bool,
        /// Output directory override.
        out: Option<PathBuf>,
        /// Exact output file path override.
        out_file: Option<PathBuf>,
    },
    /// `bench compare <a.json> <b.json> [--json]` (two-file), or
    /// `bench compare <report.json> [--json]` (cross-runtime, `sys` vs `rust` within
    /// that one report).
    Compare {
        /// The first report; in one-path mode, the only report.
        a: PathBuf,
        /// The second report, or `None` for the cross-runtime (one-report) mode.
        b: Option<PathBuf>,
        /// Emit JSON.
        json: bool,
    },
    /// `bench help [<subcommand>]` / `--help`.
    Help(Option<String>),
}

/// Parses the full process argument vector (**including** `argv[0]`).
///
/// An empty/one-element vector, or a bare `--help`/`-h`/`help`, yields
/// [`Command::Help`] (never an error) so the binary can print usage and exit `0`.
pub fn parse(args: &[String]) -> Result<Command> {
    if args.len() <= 1 {
        return Ok(Command::Help(None));
    }
    let rest = &args[1..];
    let head = rest[0].as_str();
    match head {
        "--help" | "-h" => Ok(Command::Help(None)),
        "help" => Ok(Command::Help(rest.get(1).cloned())),
        "list" => parse_list(&rest[1..]),
        "run" => parse_run(&rest[1..]),
        "baseline" => parse_baseline(&rest[1..]),
        "compare" => parse_compare(&rest[1..]),
        other => Err(BenchError::usage(format!(
            "unknown subcommand `{other}` (expected list|run|baseline|compare|help)"
        ))),
    }
}

fn is_help(arg: &str) -> bool {
    arg == "--help" || arg == "-h"
}

fn parse_list(args: &[String]) -> Result<Command> {
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            other if is_help(other) => return Ok(Command::Help(Some("list".into()))),
            other => return Err(BenchError::usage(format!("unexpected argument `{other}` for `list`"))),
        }
    }
    Ok(Command::List { json })
}

fn parse_baseline(args: &[String]) -> Result<Command> {
    let mut profile = Profile::Quick;
    let mut json = false;
    let mut out = None;
    let mut out_file = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--quick" => profile = Profile::Quick,
            "--full" => profile = Profile::Full,
            "--json" => json = true,
            "--out" => {
                out = Some(PathBuf::from(take_value(args, &mut i, "--out")?));
            }
            "--out-file" => {
                out_file = Some(PathBuf::from(take_value(args, &mut i, "--out-file")?));
            }
            other if is_help(other) => return Ok(Command::Help(Some("baseline".into()))),
            other => {
                return Err(BenchError::usage(format!(
                    "unexpected argument `{other}` for `baseline`"
                )))
            }
        }
        i += 1;
    }
    Ok(Command::Baseline { profile, json, out, out_file })
}

fn parse_run(args: &[String]) -> Result<Command> {
    let mut run = RunArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--quick" => run.profile = Profile::Quick,
            "--full" => run.profile = Profile::Full,
            "--json" => run.json = true,
            "--verbose" => run.verbose = true,
            "--workloads" => {
                let raw = take_value(args, &mut i, "--workloads")?;
                let ids: Vec<String> = raw
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                if ids.is_empty() {
                    return Err(BenchError::usage("--workloads requires a non-empty id list"));
                }
                run.workloads = Some(ids);
            }
            "--runtime" => {
                let raw = take_value(args, &mut i, "--runtime")?;
                run.runtime = RuntimeSel::parse(&raw)?;
            }
            "--out" => {
                run.out = Some(PathBuf::from(take_value(args, &mut i, "--out")?));
            }
            "--out-file" => {
                run.out_file = Some(PathBuf::from(take_value(args, &mut i, "--out-file")?));
            }
            "--repo" => {
                let raw = take_value(args, &mut i, "--repo")?;
                run.repos.push(parse_repo(&raw)?);
            }
            other if is_help(other) => return Ok(Command::Help(Some("run".into()))),
            other => {
                return Err(BenchError::usage(format!("unexpected argument `{other}` for `run`")))
            }
        }
        i += 1;
    }
    Ok(Command::Run(run))
}

fn parse_compare(args: &[String]) -> Result<Command> {
    let mut positional: Vec<PathBuf> = Vec::new();
    let mut json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json = true,
            other if is_help(other) => return Ok(Command::Help(Some("compare".into()))),
            other if other.starts_with("--") => {
                return Err(BenchError::usage(format!(
                    "unexpected flag `{other}` for `compare`"
                )))
            }
            other => positional.push(PathBuf::from(other)),
        }
    }
    match positional.len() {
        1 => Ok(Command::Compare {
            a: positional[0].clone(),
            b: None,
            json,
        }),
        2 => Ok(Command::Compare {
            a: positional[0].clone(),
            b: Some(positional[1].clone()),
            json,
        }),
        n => Err(BenchError::usage(format!(
            "`compare` needs one report (cross-runtime sys vs rust) or two reports \
             (a.json b.json); got {n}"
        ))),
    }
}

/// Parses `--repo <id>=<path>` into its `(id, path)` pair.
fn parse_repo(raw: &str) -> Result<(String, PathBuf)> {
    match raw.split_once('=') {
        Some((id, path)) if !id.trim().is_empty() && !path.trim().is_empty() => {
            Ok((id.trim().to_string(), PathBuf::from(path.trim())))
        }
        _ => Err(BenchError::usage(format!(
            "invalid --repo `{raw}` (expected <id>=<path>)"
        ))),
    }
}

/// Consumes the value of a flag, advancing the cursor; a missing value is a usage error.
fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String> {
    let next = args.get(*i + 1);
    match next {
        Some(v) if !v.starts_with("--") => {
            *i += 1;
            Ok(v.clone())
        }
        _ => Err(BenchError::usage(format!("`{flag}` requires a value"))),
    }
}

/// The top-level usage text.
pub fn usage() -> String {
    format!(
        "bench {} — Rust ERTS benchmark driver\n\
         \n\
         USAGE:\n  bench <SUBCOMMAND> [OPTIONS]\n\
         \n\
         SUBCOMMANDS:\n\
         \x20 list      List workloads, their categories/contracts and probe status\n\
         \x20 run       Run workloads and write a JSON report\n\
         \x20 baseline  Measure the system-ERTS baseline only\n\
         \x20 compare   Diff two reports, or compare sys vs rust within one report\n\
         \x20 help      Show this help, or `help <subcommand>`\n\
         \n\
         Run `bench help <SUBCOMMAND>` for subcommand options.\n\
         Exit codes: 0 = report produced, 2 = usage/env/IO/schema error.",
        env!("CARGO_PKG_VERSION")
    )
}

/// Per-subcommand help text.
pub fn subcommand_help(name: &str) -> Option<String> {
    let text = match name {
        "list" => "bench list [--json]\n\n  List workload ids, category, contract coverage and probe status.",
        "run" => "bench run [--quick|--full] [--workloads <ids,csv>] [--runtime <sys|rust|all>]\n\
                  \x20        [--out <dir>] [--out-file <path>] [--repo <id>=<path>]... [--json] [--verbose]\n\
                  \n  Defaults: --quick, --runtime sys, all workloads.\n\
                  \x20 --out       Output directory for the timestamped report (default: out/).\n\
                  \x20 --out-file  Exact output path for the report (overrides --out naming).\n\
                  \x20 --repo  Override an external workload's repo path (e.g. --repo cowboy=/src/cowboy).",
        "baseline" => "bench baseline [--quick|--full] [--json] [--out <dir>] [--out-file <path>]\n\
                       \n  System-ERTS baseline only.\n\
                       \x20 --out-file  Exact output path for the report (overrides --out naming).",
        "compare" => "bench compare <report.json> [--json]          # cross-runtime: sys vs rust within one report\n\
                      bench compare <a.json> <b.json> [--json]   # two-file: diff two reports\n\
                      \n  One report  Compare its `sys` rows against its `rust` rows, per (workload, metric)\n\
                      \x20                (as produced by `bench run --runtime all`).\n\
                      \x20 Two reports Diff them per (workload, runtime, metric); `a` is the baseline.",
        "help" => "bench help [<subcommand>]\n\n  Show top-level help, or the help for <subcommand>.",
        _ => return None,
    };
    Some(text.to_string())
}

/// The full help text for an optional subcommand: top-level usage, plus that
/// subcommand's options when given a known name.
pub fn help_text(sub: Option<&str>) -> String {
    match sub.and_then(subcommand_help) {
        Some(specific) => format!("{specific}\n\n{}", usage()),
        None => usage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(rest: &[&str]) -> Vec<String> {
        let mut v = vec!["bench".to_string()];
        v.extend(rest.iter().map(|s| s.to_string()));
        v
    }

    #[test]
    fn no_args_is_help() {
        assert_eq!(parse(&["bench".to_string()]).unwrap(), Command::Help(None));
        assert_eq!(parse(&[]).unwrap(), Command::Help(None));
    }

    #[test]
    fn top_level_help_flags() {
        assert_eq!(parse(&args(&["--help"])).unwrap(), Command::Help(None));
        assert_eq!(parse(&args(&["-h"])).unwrap(), Command::Help(None));
        assert_eq!(parse(&args(&["help"])).unwrap(), Command::Help(None));
        assert_eq!(
            parse(&args(&["help", "run"])).unwrap(),
            Command::Help(Some("run".into()))
        );
    }

    #[test]
    fn list_defaults_and_json() {
        assert_eq!(parse(&args(&["list"])).unwrap(), Command::List { json: false });
        assert_eq!(parse(&args(&["list", "--json"])).unwrap(), Command::List { json: true });
        assert_eq!(
            parse(&args(&["list", "--help"])).unwrap(),
            Command::Help(Some("list".into()))
        );
    }

    #[test]
    fn run_defaults() {
        let cmd = parse(&args(&["run"])).unwrap();
        let Command::Run(run) = cmd else { panic!("expected run") };
        assert_eq!(run.profile, Profile::Quick);
        assert_eq!(run.runtime, RuntimeSel::Sys);
        assert!(run.workloads.is_none());
        assert!(!run.json);
        assert!(!run.verbose);
        assert!(run.repos.is_empty());
    }

    #[test]
    fn run_all_flags() {
        let cmd = parse(&args(&[
            "run",
            "--full",
            "--workloads",
            "baseline, erlc_compiler",
            "--runtime",
            "all",
            "--out",
            "/tmp/x",
            "--repo",
            "cowboy=/src/cowboy",
            "--json",
            "--verbose",
        ]))
        .unwrap();
        let Command::Run(run) = cmd else { panic!("expected run") };
        assert_eq!(run.profile, Profile::Full);
        assert_eq!(run.runtime, RuntimeSel::All);
        assert_eq!(
            run.workloads,
            Some(vec!["baseline".to_string(), "erlc_compiler".to_string()])
        );
        assert_eq!(run.out, Some(PathBuf::from("/tmp/x")));
        assert_eq!(run.repos, vec![("cowboy".to_string(), PathBuf::from("/src/cowboy"))]);
        assert!(run.json && run.verbose);
    }

    #[test]
    fn run_out_file_parses() {
        let cmd = parse(&args(&["run", "--out-file", "out/x.json"])).unwrap();
        let Command::Run(run) = cmd else { panic!("expected run") };
        assert_eq!(run.out_file, Some(PathBuf::from("out/x.json")));
        assert_eq!(run.out, None);

        // `--out` and `--out-file` are independent (dir vs exact file).
        let cmd = parse(&args(&["run", "--out", "d", "--out-file", "d/x.json"])).unwrap();
        let Command::Run(run) = cmd else { panic!("expected run") };
        assert_eq!(run.out, Some(PathBuf::from("d")));
        assert_eq!(run.out_file, Some(PathBuf::from("d/x.json")));
    }

    #[test]
    fn baseline_defaults_and_flags() {
        assert_eq!(
            parse(&args(&["baseline"])).unwrap(),
            Command::Baseline {
                profile: Profile::Quick,
                json: false,
                out: None,
                out_file: None
            }
        );
        assert_eq!(
            parse(&args(&["baseline", "--full", "--json", "--out", "o"])).unwrap(),
            Command::Baseline {
                profile: Profile::Full,
                json: true,
                out: Some(PathBuf::from("o")),
                out_file: None
            }
        );
    }

    #[test]
    fn baseline_out_file_parses() {
        assert_eq!(
            parse(&args(&["baseline", "--out-file", "p"])).unwrap(),
            Command::Baseline {
                profile: Profile::Quick,
                json: false,
                out: None,
                out_file: Some(PathBuf::from("p"))
            }
        );
    }

    #[test]
    fn compare_one_path_is_cross_runtime_mode() {
        assert_eq!(
            parse(&args(&["compare", "report.json"])).unwrap(),
            Command::Compare {
                a: PathBuf::from("report.json"),
                b: None,
                json: false
            }
        );
        assert_eq!(
            parse(&args(&["compare", "report.json", "--json"])).unwrap(),
            Command::Compare {
                a: PathBuf::from("report.json"),
                b: None,
                json: true
            }
        );
    }

    #[test]
    fn compare_two_paths_keeps_two_file_mode() {
        assert_eq!(
            parse(&args(&["compare", "a.json", "b.json"])).unwrap(),
            Command::Compare {
                a: PathBuf::from("a.json"),
                b: Some(PathBuf::from("b.json")),
                json: false
            }
        );
        assert_eq!(
            parse(&args(&["compare", "a.json", "b.json", "--json"])).unwrap(),
            Command::Compare {
                a: PathBuf::from("a.json"),
                b: Some(PathBuf::from("b.json")),
                json: true
            }
        );
    }

    #[test]
    fn compare_rejects_zero_or_three_paths() {
        assert!(parse(&args(&["compare"])).is_err());
        assert!(parse(&args(&["compare", "a.json", "b.json", "c.json"])).is_err());
        assert!(parse(&args(&["compare", "--nope", "a.json"])).is_err());
    }

    #[test]
    fn subcommand_help_flags() {
        for sub in ["run", "baseline", "compare"] {
            assert_eq!(
                parse(&args(&[sub, "--help"])).unwrap(),
                Command::Help(Some(sub.into()))
            );
        }
    }

    #[test]
    fn error_paths() {
        assert!(parse(&args(&["bogus"])).is_err());
        assert!(parse(&args(&["run", "--runtime", "bogus"])).is_err());
        assert!(parse(&args(&["run", "--runtime"])).is_err());
        assert!(parse(&args(&["run", "--out"])).is_err());
        assert!(parse(&args(&["run", "--out-file"])).is_err());
        assert!(parse(&args(&["run", "--workloads", ""])).is_err());
        assert!(parse(&args(&["run", "--repo", "no-equals"])).is_err());
        assert!(parse(&args(&["run", "--nope"])).is_err());
        assert!(parse(&args(&["list", "--verbose"])).is_err());
    }

    #[test]
    fn help_text_contains_usage_and_subcommands() {
        let t = help_text(None);
        assert!(t.contains("USAGE"));
        assert!(t.contains("baseline"));
        let t = help_text(Some("run"));
        assert!(t.contains("--workloads"));
        assert!(t.contains("--out"));
        assert!(t.contains("--out-file"));
        let t = help_text(Some("compare"));
        assert!(t.contains("cross-runtime"));
        assert!(t.contains("<report.json>"));
        assert!(help_text(Some("nope")).contains("USAGE"));
    }
}
