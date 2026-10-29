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
//! Hand-rolled argument parsing (no `clap`, so the crate stays dependency-free).
//!
//! Two actions: `list` (what targets exist and where they are) and `run` (measure).
use std::path::PathBuf;
use std::time::Duration;
use crate::error::{Result, SuitesError};
use crate::runtime::RuntimeKind;
/// Which target(s) a run covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSel {
    /// The Khepri eunit suite.
    Khepri,
    /// The selected OTP `stdlib` `common_test` suites.
    Stdlib,
    /// Both.
    All,
}
impl TargetSel {
    /// Whether Khepri is included.
    pub fn includes_khepri(&self) -> bool {
        matches!(self, TargetSel::Khepri | TargetSel::All)
    }
    /// Whether the stdlib suites are included.
    pub fn includes_stdlib(&self) -> bool {
        matches!(self, TargetSel::Stdlib | TargetSel::All)
    }
}
/// A parsed `suites run` invocation.
#[derive(Debug, Clone)]
pub struct RunArgs {
    /// Target selection.
    pub targets: TargetSel,
    /// Restrict to these suite/module names (empty = the target's default set).
    pub only: Vec<String>,
    /// Which sides to run.
    pub runtimes: Vec<RuntimeKind>,
    /// Per-module / per-case timeout in seconds.
    pub timeout_secs: u64,
    /// Whole-suite budget in seconds (stdlib: bounds one suite's process, so a hung case cannot
    /// swallow the rest of the suite's data).
    pub suite_budget_secs: u64,
    /// Concurrent module runs.
    pub jobs: usize,
    /// Output directory for the artifacts (default: `results/` next to the crate).
    pub out_dir: Option<PathBuf>,
    /// Exact JSON artifact path (default: `<out-dir>/report.json`).
    pub out_file: Option<PathBuf>,
    /// Print the JSON artifact to stdout instead of the Markdown summary.
    pub json: bool,
    /// Stream each run's command line.
    pub verbose: bool,
    /// Ignore a completed staged build and rebuild from scratch.
    pub rebuild: bool,
    /// Keep the staged tree after the run (it is kept by default: re-runs reuse it).
    pub keep_stage: bool,
    /// Override the Khepri checkout.
    pub khepri_src: Option<PathBuf>,
    /// Override the `lib/stdlib/test` directory.
    pub stdlib_src: Option<PathBuf>,
}
impl Default for RunArgs {
    fn default() -> Self {
        RunArgs {
            targets: TargetSel::All,
            only: Vec::new(),
            runtimes: vec![RuntimeKind::Sys, RuntimeKind::Rust],
            timeout_secs: 300,
            suite_budget_secs: 600,
            jobs: 4,
            out_dir: None,
            out_file: None,
            json: false,
            verbose: false,
            rebuild: false,
            keep_stage: true,
            khepri_src: None,
            stdlib_src: None,
        }
    }
}
impl RunArgs {
    /// The per-case / per-module timeout.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }
    /// The whole-suite budget (never below one case timeout).
    pub fn suite_budget(&self) -> Duration {
        Duration::from_secs(self.suite_budget_secs.max(self.timeout_secs.max(1)))
    }
}
/// What the user asked for.
#[derive(Debug, Clone)]
pub enum Action {
    /// `suites list`
    List {
        /// Print JSON.
        json: bool,
    },
    /// `suites run …`
    Run(RunArgs),
    /// `suites help`
    Help,
}
/// Parses `argv` (including `argv[0]`).
pub fn parse(args: &[String]) -> Result<Action> {
    let mut iter = args.iter().skip(1).peekable();
    let Some(command) = iter.next() else {
        return Ok(Action::Help);
    };
    match command.as_str() {
        "help" | "--help" | "-h" => Ok(Action::Help),
        "list" => {
            let mut json = false;
            for arg in iter {
                match arg.as_str() {
                    "--json" => json = true,
                    other => return Err(SuitesError::Usage(format!("list: unknown argument `{other}`"))),
                }
            }
            Ok(Action::List { json })
        }
        "run" => {
            let mut args = RunArgs::default();
            while let Some(arg) = iter.next() {
                let (flag, inline) = match arg.split_once('=') {
                    Some((f, v)) => (f, Some(v.to_string())),
                    None => (arg.as_str(), None),
                };
                let mut value = |name: &str| -> Result<String> {
                    match &inline {
                        Some(v) => Ok(v.clone()),
                        None => iter
                            .next()
                            .cloned()
                            .ok_or_else(|| SuitesError::Usage(format!("{name} needs a value"))),
                    }
                };
                match flag {
                    "--target" => {
                        args.targets = match value("--target")?.as_str() {
                            "khepri" => TargetSel::Khepri,
                            "stdlib" => TargetSel::Stdlib,
                            "all" => TargetSel::All,
                            other => {
                                return Err(SuitesError::Usage(format!(
                                    "--target: expected khepri|stdlib|all, got `{other}`"
                                )))
                            }
                        }
                    }
                    "--suite" | "--module" => {
                        for name in value(flag)?.split(',') {
                            let name = name.trim();
                            if !name.is_empty() {
                                args.only.push(name.to_string());
                            }
                        }
                    }
                    "--runtime" => {
                        let raw = value("--runtime")?;
                        args.runtimes = match raw.as_str() {
                            "all" => vec![RuntimeKind::Sys, RuntimeKind::Rust],
                            "sys" => vec![RuntimeKind::Sys],
                            "rust" => vec![RuntimeKind::Rust],
                            other => {
                                return Err(SuitesError::Usage(format!(
                                    "--runtime: expected sys|rust|all, got `{other}`"
                                )))
                            }
                        };
                        if args.runtimes.contains(&RuntimeKind::Rust) && !args.runtimes.contains(&RuntimeKind::Sys) {
                            // A Rust-only run has no baseline; allowed, but the oracle is then
                            // recorded as not-run in every row.
                        }
                    }
                    "--timeout" => {
                        args.timeout_secs = value("--timeout")?
                            .parse()
                            .map_err(|_| SuitesError::Usage("--timeout expects seconds".into()))?;
                    }
                    "--suite-budget" => {
                        args.suite_budget_secs = value("--suite-budget")?
                            .parse()
                            .map_err(|_| SuitesError::Usage("--suite-budget expects seconds".into()))?;
                    }
                    "--jobs" => {
                        args.jobs = value("--jobs")?
                            .parse::<usize>()
                            .map_err(|_| SuitesError::Usage("--jobs expects a number".into()))?
                            .max(1);
                    }
                    "--out" => args.out_dir = Some(PathBuf::from(value("--out")?)),
                    "--out-file" => args.out_file = Some(PathBuf::from(value("--out-file")?)),
                    "--json" => args.json = true,
                    "--verbose" | "-v" => args.verbose = true,
                    "--rebuild" => args.rebuild = true,
                    "--keep-stage" => args.keep_stage = true,
                    "--khepri-src" => args.khepri_src = Some(PathBuf::from(value("--khepri-src")?)),
                    "--stdlib-src" => args.stdlib_src = Some(PathBuf::from(value("--stdlib-src")?)),
                    other => return Err(SuitesError::Usage(format!("run: unknown argument `{other}`"))),
                }
            }
            Ok(Action::Run(args))
        }
        other => Err(SuitesError::Usage(format!("unknown command `{other}`"))),
    }
}
/// The `--help` text.
pub fn usage() -> &'static str {
    r#"suites — measure real third-party / OTP test suites under both runtimes

USAGE:
    suites list [--json]
    suites run [options]
    suites help

RUN OPTIONS:
    --target khepri|stdlib|all   which target(s) (default: all)
    --suite <name>[,<name>…]     restrict to these suites (stdlib) — alias of --module
    --module <name>[,<name>…]    restrict to these eunit modules (khepri)
    --runtime sys|rust|all       which sides to run (default: all)
    --timeout <secs>             per module/case timeout (default: 300)
    --suite-budget <secs>        whole-suite budget (stdlib, default: 600)
    --jobs <n>                   concurrent module runs (default: 4)
    --out <dir>                  artifact directory (default: erts_rust/tools/suites/results)
    --out-file <path>            exact JSON artifact path
    --json                       print the JSON artifact instead of the Markdown summary
    --verbose                    print every command before running it
    --rebuild                    rebuild the staged target instead of reusing a completed build
    --khepri-src <dir>           Khepri checkout (default: $KHEPRI_SRC or /home/bill/Source/khepri)
    --stdlib-src <dir>           OTP stdlib test dir (default: <repo>/lib/stdlib/test)
    -h, --help                   this text

ENVIRONMENT:
    ERTS_ORACLE   oracle `erl` (default: `erl` on $PATH)
    BEAM_RT       Rust runtime binary (default: erts_rust/target/{release,debug}/beam_rt)
    KHEPRI_SRC    Khepri checkout to stage (the checkout itself is never written to)
    TMPDIR        where staged copies and build caches live

EXIT CODES:
    0  the sweep completed (per-row failures are data, not harness errors)
    1  the harness failed (I/O, an unparsable report, a target that could not be staged)
    2  usage or environment error (no oracle, bad flag)
"#
}
#[cfg(test)]
mod tests {
    use super::*;
    fn argv(items: &[&str]) -> Vec<String> {
        std::iter::once("suites".to_string()).chain(items.iter().map(|s| s.to_string())).collect()
    }
    #[test]
    fn parses_run_defaults() {
        let action = parse(&argv(&["run"])).expect("parse");
        match action {
            Action::Run(args) => {
                assert_eq!(args.targets, TargetSel::All);
                assert_eq!(args.jobs, 4);
                assert_eq!(args.timeout_secs, 300);
                assert_eq!(args.runtimes, vec![RuntimeKind::Sys, RuntimeKind::Rust]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    #[test]
    fn parses_flags_with_and_without_equals() {
        let action = parse(&argv(&[
            "run",
            "--target=khepri",
            "--module",
            "simple_get,simple_put",
            "--runtime",
            "sys",
            "--timeout",
            "42",
            "--jobs=2",
        ]))
        .expect("parse");
        match action {
            Action::Run(args) => {
                assert_eq!(args.targets, TargetSel::Khepri);
                assert_eq!(args.only, vec!["simple_get", "simple_put"]);
                assert_eq!(args.runtimes, vec![RuntimeKind::Sys]);
                assert_eq!(args.timeout_secs, 42);
                assert_eq!(args.jobs, 2);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    #[test]
    fn suite_budget_never_sits_below_the_case_timeout() {
        let action = parse(&argv(&["run", "--timeout", "90", "--suite-budget", "5"])).expect("parse");
        match action {
            Action::Run(args) => assert_eq!(args.suite_budget().as_secs(), 90),
            other => panic!("unexpected {other:?}"),
        }
    }
    #[test]
    fn rejects_bad_values() {
        assert!(parse(&argv(&["run", "--target", "cowboy"])).is_err());
        assert!(parse(&argv(&["run", "--timeout"])).is_err());
        assert!(parse(&argv(&["run", "--nope"])).is_err());
        assert!(parse(&argv(&["bogus"])).is_err());
    }
    #[test]
    fn empty_argv_is_help() {
        assert!(matches!(parse(&argv(&[])).expect("parse"), Action::Help));
    }
}
