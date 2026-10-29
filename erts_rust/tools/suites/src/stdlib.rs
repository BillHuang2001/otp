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
//! The **OTP `stdlib`** target: real `common_test` suites driven per case.
//!
//! stdlib's suites are `common_test` suites, so there is no eunit summary to parse. A small
//! committed runner (`data/ct_runner.erl`, embedded with `include_str!`) expands
//! `Suite:all()` (recursively through `groups/0`), runs **each case in its own
//! `spawn_monitor` with a timeout**, and prints one canonical line per case:
//!
//! ```text
//! CASE <name> OK | SKIP <reason> | EXC <reason> | TIMEOUT | MISSING
//! ```
//!
//! Both sides get the identical `-pa` set and the identical drive expression, so the
//! comparison is per case, not per suite. The read-only `lib/stdlib/test` tree is staged into
//! `$TMPDIR` and compiled there with the oracle's own `erlc` (so the beams are loadable by
//! both sides); group-level `init_per_group`/`end_per_group` and `init_per_testcase` hooks are
//! **not** executed (documented in the report) — a case that needs them fails identically on
//! both sides and lands in the `agree-fail-identical` bucket, excluded from both numerators.
use std::path::{Path, PathBuf};
use std::process::Command;
use crate::error::{Result, SuitesError};
use crate::proc;
use crate::report::{self, Build, Invocation, Outcome, Row, RowKind, Source, Status, TargetReport};
use crate::runtime::{self, Runtime, RuntimeKind};
use crate::target::{self, TargetOptions};
/// The suites measured when `--suite` is not given: self-contained, no `ct` data dir needed.
pub const DEFAULT_SUITES: &[&str] = &["lists_SUITE", "maps_SUITE", "proplists_SUITE", "dict_SUITE"];
/// Local helper modules some stdlib suites call; compiled alongside the suites.
///
/// These are the small non-`SUITE` modules of `lib/stdlib/test` that suites call directly
/// (`error_info_lib:test/…`, `dict_test_lib:…`, `binref:encode_unsigned/1`,
/// `random_iolist:…`, `sets_test_lib:…`). A helper that fails to compile is *not* fatal: it is
/// recorded in the build detail, and the cases that need it simply fail (identically on both
/// sides, so they land in `agree-fail-identical`).
pub const HELPER_MODULES: &[&str] = &[
    "error_info_lib",
    "dict_test_lib",
    "binref",
    "random_iolist",
    "random_unicode_list",
    "sets_test_lib",
];
/// The committed runner source.
pub const RUNNER: &str = include_str!("../data/ct_runner.erl");
/// The `lib/stdlib/test` directory of this repository.
pub fn source(opts: &TargetOptions) -> PathBuf {
    if opts.source.as_os_str().is_empty() {
        runtime::repo_root().join("lib").join("stdlib").join("test")
    } else {
        opts.source.clone()
    }
}
/// The per-case drive given to **both** sides, with the suite name, the per-case timeout, the
/// suite budget and the data root substituted.
pub const DRIVE: &str =
    "ct_runner:run('{suite}', {case_timeout_ms}, {suite_budget_ms}, \"{data_root}\"), halt(0).";
/// Builds the drive expression for one suite.
pub fn drive(suite: &str, case_timeout_ms: u64, suite_budget_ms: u64, data_root: &Path) -> String {
    DRIVE
        .replace("{suite}", suite)
        .replace("{case_timeout_ms}", &case_timeout_ms.to_string())
        .replace("{suite_budget_ms}", &suite_budget_ms.to_string())
        .replace("{data_root}", &data_root.display().to_string())
}
/// `erlc`: the oracle's sibling binary when present (so the compiler matches the oracle),
/// else `$ERLC`, else `erlc` on `$PATH`.
fn find_erlc(oracle: Option<&Runtime>) -> Result<PathBuf> {
    if let Some(program) = oracle.and_then(Runtime::program) {
        if let Some(dir) = program.parent() {
            let sibling = dir.join("erlc");
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }
    if let Some(v) = std::env::var_os("ERLC").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(v);
        if p.is_file() {
            return Ok(p);
        }
        return Err(SuitesError::Env(format!("$ERLC={} is not a file", p.display())));
    }
    proc::which("erlc")
        .ok_or_else(|| SuitesError::Env("no erlc: set $ERLC or put it on $PATH".into()))
}
/// Stages the selected suites (sources, headers and `<suite>_data` trees) plus the runner.
fn stage(suites: &[String], src: &Path, stage_dir: &Path, opts: &TargetOptions) -> Result<PathBuf> {
    let stage_src = stage_dir.join("src");
    if opts.rebuild || !stage_dir.join(".build-ok").is_file() {
        target::remove_tree(stage_dir)?;
        let wanted: Vec<String> = suites.iter().map(|s| format!("{s}_data")).collect();
        target::copy_filtered(src, &stage_src, &|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.ends_with(".erl") || name.ends_with(".hrl") {
                return true;
            }
            path.is_dir() && wanted.iter().any(|w| w == name)
        })?;
    }
    Ok(stage_src)
}
/// Compiles the selected suites, their helper modules and the runner.
///
/// Returns `(ebin, required_failures, optional_failures)`: a *selected suite* (or the runner)
/// that does not compile makes the build fail; a helper module that does not compile is only
/// recorded, because the cases needing it fail identically on both sides.
fn compile(
    stage_dir: &Path,
    stage_src: &Path,
    suites: &[String],
    erlc: &Path,
    opts: &TargetOptions,
) -> Result<(PathBuf, Vec<String>, Vec<String>)> {
    let ebin = stage_dir.join("ebin");
    let ctl = stage_dir.join("ctl");
    std::fs::create_dir_all(&ebin)?;
    std::fs::create_dir_all(&ctl)?;
    std::fs::write(ctl.join("ct_runner.erl"), RUNNER)?;
    let mut required_failures = Vec::new();
    let mut optional_failures = Vec::new();
    let mut modules: Vec<(String, bool)> = suites.iter().map(|s| (s.clone(), true)).collect();
    modules.extend(HELPER_MODULES.iter().map(|s| (s.to_string(), false)));
    modules.push(("ct_runner".to_string(), true));
    for (module, required) in &modules {
        let dir = if module == "ct_runner" { &ctl } else { stage_src };
        let source_file = dir.join(format!("{module}.erl"));
        if !source_file.is_file() {
            let message = format!("{module}: source not found at {}", source_file.display());
            if *required {
                required_failures.push(message);
            } else {
                optional_failures.push(message);
            }
            continue;
        }
        let mut cmd = Command::new(erlc);
        cmd.arg("-o").arg(&ebin).arg("-I").arg(stage_src).arg(&source_file);
        if opts.verbose {
            eprintln!("[stdlib] compile: {}", proc::cmdline(&cmd));
        }
        let run = proc::run_with_timeout(&mut cmd, Some(std::time::Duration::from_secs(300)))?;
        if !run.success {
            let message = format!(
                "{module}: exit {:?}: {}",
                run.exit_code,
                report::one_line(&run.tail(8), 500)
            );
            if *required {
                required_failures.push(message);
            } else {
                optional_failures.push(message);
            }
        }
    }
    Ok((ebin, required_failures, optional_failures))
}
/// One case's status as printed by the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseStatus {
    /// Ran and returned normally.
    Ok,
    /// Skipped by the suite itself.
    Skip(String),
    /// Crashed (or returned `{fail,_}`/`{error,_}`).
    Exc(String),
    /// Killed at the per-case timeout.
    Timeout,
    /// Not exported by the suite (malformed `all/0`).
    Missing,
}
/// Parses the runner's output into `(case, status)` pairs, in output order, plus the
/// suite-level markers: `init_per_suite` failure, suite skip, and the count of cases the
/// runner never launched because the suite budget was exhausted.
pub fn parse(
    stdout: &str,
) -> (Vec<(String, CaseStatus)>, Option<String>, Option<String>, Option<u64>) {
    let mut cases = Vec::new();
    let mut init_failed = None;
    let mut suite_skip = None;
    let mut deadline: Option<u64> = None;
    for line in stdout.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("CASE ") {
            let mut parts = rest.splitn(3, ' ');
            let name = parts.next().unwrap_or("").to_string();
            let status = parts.next().unwrap_or("");
            let detail = parts.next().unwrap_or("").to_string();
            let status = match status {
                "OK" => CaseStatus::Ok,
                "SKIP" => CaseStatus::Skip(detail),
                "EXC" => CaseStatus::Exc(detail),
                "TIMEOUT" => CaseStatus::Timeout,
                "MISSING" => CaseStatus::Missing,
                _ => continue,
            };
            cases.push((name, status));
        } else if let Some(rest) = t.strip_prefix("SUITE_INIT_FAILED ") {
            init_failed = Some(rest.to_string());
        } else if let Some(rest) = t.strip_prefix("SUITE_SKIP ") {
            suite_skip = Some(rest.to_string());
        } else if let Some(rest) = t.strip_prefix("SUITE_DEADLINE ") {
            deadline = rest.trim().parse().ok();
        }
    }
    (cases, init_failed, suite_skip, deadline)
}
/// A case that the side never reported: a `SUITE_DEADLINE` explains it (the case was *not
/// measured*), otherwise the output is inconsistent with the other side's case list.
fn missing_case(side: &str, deadline: Option<u64>) -> Outcome {
    match deadline {
        Some(not_run) => Outcome::bare(
            Status::Timeout,
            format!("not measured: the suite budget expired before this case ({not_run} cases not run)"),
        ),
        None => Outcome::bare(Status::Error, format!("case missing from the {side}'s output")),
    }
}

/// The outcome of one case on one side.
fn outcome_of(status: &CaseStatus) -> Outcome {
    match status {
        CaseStatus::Ok => Outcome::pass(1),
        CaseStatus::Skip(reason) => Outcome::bare(Status::Skip, reason.clone()),
        CaseStatus::Exc(reason) => Outcome::fail(0, 1, 0, reason.clone()),
        CaseStatus::Timeout => Outcome::fail(0, 1, 0, "timeout"),
        CaseStatus::Missing => Outcome::bare(Status::Error, "case not exported"),
    }
}
/// One side's observation of one suite.
struct SideRun {
    /// The cases the runner actually reported, in output order.
    cases: Vec<(String, CaseStatus)>,
    /// A suite-level failure (`init_per_suite`, load error, whole-process timeout, unparsable
    /// output) that explains why cases are missing.
    suite_failure: Option<Outcome>,
    /// How many cases the runner refused to launch because the suite budget ran out.
    deadline: Option<u64>,
}
impl SideRun {
    fn failed(outcome: Outcome) -> Self {
        SideRun { cases: Vec::new(), suite_failure: Some(outcome), deadline: None }
    }
}
/// Observes one suite on one side.
fn observe(
    runtime: &Runtime,
    code_path: &[PathBuf],
    suite: &str,
    data_root: &Path,
    opts: &TargetOptions,
    cwd: &Path,
) -> SideRun {
    if runtime.program().is_none() {
        return SideRun::failed(Outcome::bare(
            Status::NotRun,
            runtime.absent_reason.clone().unwrap_or_default(),
        ));
    }
    let case_timeout_ms = opts.timeout.as_millis() as u64;
    let budget_ms = opts.suite_budget.as_millis() as u64;
    let eval = drive(suite, case_timeout_ms, budget_ms, data_root);
    // The process budget sits *above* the in-runner suite budget: the runner stops launching
    // cases when its own budget expires, so the process should normally finish on its own. The
    // extra margin covers `end_per_suite` and a case that overruns its own timeout.
    let process_budget = opts.suite_budget + std::time::Duration::from_secs(180);
    match target::invoke(runtime, code_path, &eval, process_budget, &[], Some(cwd)) {
        Ok(run) => {
            if run.timed_out {
                return SideRun::failed(Outcome::bare(
                    Status::Timeout,
                    format!("suite process killed after {} ms (process budget)", run.wall_ms.round()),
                ));
            }
            let (cases, init_failed, suite_skip, deadline) = parse(&run.stdout);
            if cases.is_empty() && init_failed.is_none() && suite_skip.is_none() && deadline.is_none() {
                return SideRun::failed(target::unclassified(&run));
            }
            let suite_failure = if let Some(reason) = init_failed {
                Some(Outcome::bare(Status::InitFail, reason))
            } else {
                suite_skip.map(|reason| Outcome::bare(Status::Skip, reason))
            };
            SideRun { cases, suite_failure, deadline }
        }
        Err(err) => SideRun::failed(Outcome::bare(Status::Error, err)),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_case_lines_and_markers() {
        let out = "\nSUITE maps_SUITE\n\nCASE a OK\n\nCASE b EXC badarg\n\nCASE c SKIP nope\n\nCASE d TIMEOUT\n\nSUITE_DEADLINE 7\n\nDONE maps_SUITE\n";
        let (cases, init_failed, suite_skip, deadline) = parse(out);
        assert_eq!(cases.len(), 4);
        assert_eq!(cases[0], ("a".to_string(), CaseStatus::Ok));
        assert_eq!(cases[1], ("b".to_string(), CaseStatus::Exc("badarg".to_string())));
        assert_eq!(cases[2], ("c".to_string(), CaseStatus::Skip("nope".to_string())));
        assert_eq!(cases[3], ("d".to_string(), CaseStatus::Timeout));
        assert!(init_failed.is_none());
        assert!(suite_skip.is_none());
        assert_eq!(deadline, Some(7));
    }
    #[test]
    fn parses_suite_level_failures() {
        let (cases, init_failed, suite_skip, deadline) =
            parse("\nSUITE_INIT_FAILED {badmatch,enoent}\n");
        assert!(cases.is_empty());
        assert_eq!(init_failed.as_deref(), Some("{badmatch,enoent}"));
        assert!(suite_skip.is_none());
        assert!(deadline.is_none());
    }
    #[test]
    fn a_case_that_printed_without_a_newline_does_not_swallow_the_next_line() {
        // The runner prefixes every protocol line with `\n`, so a case's unfinished output can
        // never glue itself onto `CASE …` (that is what made one oracle case look like a failure
        // before the fix).
        let out = "some log line\nCASE ok_one OK\nCASE ok_two OK\n";
        let (cases, _, _, _) = parse(out);
        assert_eq!(cases.len(), 2);
    }
    #[test]
    fn missing_case_distinguishes_a_budget_expiry_from_bad_output() {
        let expired = missing_case("rust runtime", Some(9));
        assert_eq!(expired.status, Status::Timeout);
        assert!(expired.detail.contains("9 cases not run"), "{}", expired.detail);
        let inconsistent = missing_case("rust runtime", None);
        assert_eq!(inconsistent.status, Status::Error);
    }
    #[test]
    fn drive_embeds_both_timeouts_identically() {
        let text = drive("lists_SUITE", 10_000, 600_000, Path::new("/staged/src"));
        assert_eq!(
            text,
            "ct_runner:run('lists_SUITE', 10000, 600000, \"/staged/src\"), halt(0)."
        );
    }
}

/// Runs the stdlib target end to end.
pub fn run(runtimes: &[Runtime], opts: &TargetOptions, stage_dir: &Path) -> Result<TargetReport> {
    let src = source(opts);
    if !src.is_dir() {
        return Err(SuitesError::Env(format!(
            "no OTP stdlib test directory at {}",
            src.display()
        )));
    }
    let mut suites: Vec<String> = if opts.only.is_empty() {
        DEFAULT_SUITES.iter().map(|s| s.to_string()).collect()
    } else {
        opts.only.iter().map(|s| s.trim_end_matches(".erl").to_string()).collect()
    };
    suites.sort();
    suites.dedup();
    let oracle = runtimes.iter().find(|r| r.kind == RuntimeKind::Sys);
    let erlc = find_erlc(oracle)?;
    let stage_src = stage(&suites, &src, stage_dir, opts)?;
    let (ebin, compile_failures, helper_failures) = compile(stage_dir, &stage_src, &suites, &erlc, opts)?;
    let code_path = vec![ebin.clone(), stage_dir.join("ctl")];
    let cwd_root = stage_dir.join("cwd");
    let build = Build {
        command: format!("{} -o <stage>/ebin -I <stage>/src <suite>.erl …", erlc.display()),
        ok: compile_failures.is_empty(),
        reused: !opts.rebuild && stage_dir.join(".build-ok").is_file(),
        detail: {
            let mut parts = vec![if compile_failures.is_empty() {
                format!(
                    "compiled {} suites + {} helpers + runner with the oracle's erlc",
                    suites.len(),
                    HELPER_MODULES.len()
                )
            } else {
                compile_failures.join("; ")
            }];
            if !helper_failures.is_empty() {
                parts.push(format!("helper compile failures (non-fatal): {}", helper_failures.join("; ")));
            }
            parts.join(" | ")
        },
    };
    if build.ok {
        std::fs::write(stage_dir.join(".build-ok"), &build.command)?;
    }
    let mut notes = vec![
        format!("drive (identical on both sides): `{}`", DRIVE),
        "each case runs in its own `spawn_monitor` with a per-case timeout; `init_per_group`/`end_per_group`/`init_per_testcase` hooks are NOT executed".to_string(),
        "cases that fail identically on both sides (missing `ct` config/data dirs, group-hook dependencies) are `agree-fail-identical`: excluded from both numerators, never counted as passes".to_string(),
        format!(
            "per-case timeout: {} s · suite budget: {} s (the runner stops launching cases when the budget expires and prints `SUITE_DEADLINE <not-run>`, so the remaining cases are reported as *not measured*, never as failures)",
            opts.timeout.as_secs(),
            opts.suite_budget.as_secs()
        ),
    ];
    let mut invocations = Vec::new();
    for runtime in runtimes {
        let sample = suites.first().cloned().unwrap_or_default();
        invocations.push(Invocation {
            side: runtime.kind.as_str().to_string(),
            purpose: format!("ct {sample}"),
            command: target::describe(
                runtime,
                &code_path,
                &drive(
                    &sample,
                    opts.timeout.as_millis() as u64,
                    opts.suite_budget.as_millis() as u64,
                    &stage_src,
                ),
                &[],
                Some(&cwd_root.join(&sample)),
            ),
        });
    }
    invocations.push(Invocation {
        side: "build".into(),
        purpose: "erlc".into(),
        command: build.command.clone(),
    });
    let mut rows: Vec<Row> = Vec::new();
    let work: Vec<String> = suites.clone();
    let observed = target::par_map(opts.jobs, &work, |suite| {
        let cwd = cwd_root.join(suite);
        if let Err(err) = target::fresh_dir(&cwd) {
            let out = Outcome::bare(Status::Error, err);
            return (suite.clone(), SideRun::failed(out.clone()), SideRun::failed(out));
        }
        let sys = match oracle {
            Some(rt) => observe(rt, &code_path, suite, &stage_src, opts, &cwd),
            None => SideRun::failed(Outcome::bare(Status::NotRun, "oracle not selected")),
        };
        let rust = match runtimes.iter().find(|r| r.kind == RuntimeKind::Rust) {
            Some(rt) => observe(rt, &code_path, suite, &stage_src, opts, &cwd),
            None => SideRun::failed(Outcome::bare(Status::NotRun, "rust runtime not selected")),
        };
        (suite.clone(), sys, rust)
    });
    for (suite, sys_run, rust_run) in observed {
        let sys_cases = &sys_run.cases;
        let rust_cases = &rust_run.cases;
        let sys_failure = &sys_run.suite_failure;
        let rust_failure = &rust_run.suite_failure;
        if sys_cases.is_empty() && rust_cases.is_empty() {
            rows.push(Row {
                id: suite.clone(),
                suite: Some(suite.clone()),
                kind: RowKind::Notice,
                sys: sys_failure.clone().unwrap_or_else(|| Outcome::bare(Status::Error, "no cases")),
                rust: rust_failure.clone().unwrap_or_else(|| Outcome::bare(Status::Error, "no cases")),
                note: "the suite could not be started on either side — no per-case rows".into(),
            });
            continue;
        }
        // Union of case names, in the oracle's order (grouped by suite, so the row table reads
        // like `ct`'s own case list), then anything only the Rust side reached.
        let mut names: Vec<String> = sys_cases.iter().map(|(n, _)| n.clone()).collect();
        for (name, _) in rust_cases {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        let sys_map = |name: &str| sys_cases.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone());
        let rust_map = |name: &str| rust_cases.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone());
        let mut sys_count = (0u32, 0u32);
        let mut rust_count = (0u32, 0u32);
        for name in &names {
            let sys_outcome = match (sys_map(name), sys_failure) {
                (Some(status), _) => outcome_of(&status),
                (None, Some(failure)) => failure.clone(),
                (None, None) => missing_case("oracle", sys_run.deadline),
            };
            let rust_outcome = match (rust_map(name), rust_failure) {
                (Some(status), _) => outcome_of(&status),
                (None, Some(failure)) => failure.clone(),
                (None, None) => missing_case("rust runtime", rust_run.deadline),
            };
            if matches!(sys_outcome.status, Status::Pass | Status::Fail) {
                if sys_outcome.failed > 0 { sys_count.1 += 1 } else { sys_count.0 += 1 }
            }
            if matches!(rust_outcome.status, Status::Pass | Status::Fail) {
                if rust_outcome.failed > 0 { rust_count.1 += 1 } else { rust_count.0 += 1 }
            }
            rows.push(Row {
                id: format!("{suite}:{name}"),
                suite: Some(suite.clone()),
                kind: RowKind::CtCase,
                sys: sys_outcome,
                rust: rust_outcome,
                note: String::new(),
            });
        }
        notes.push(format!(
            "{suite}: {} cases — sys {}/{} pass/fail, rust {}/{} pass/fail",
            names.len(),
            sys_count.0,
            sys_count.1,
            rust_count.0,
            rust_count.1
        ));
        if let Some(failure) = &sys_failure {
            notes.push(format!("{suite}: oracle-side suite failure: {}", report::one_line(&failure.detail, 200)));
        }
        if let Some(failure) = &rust_failure {
            notes.push(format!("{suite}: rust-side suite failure: {}", report::one_line(&failure.detail, 200)));
        }
    }
    Ok(TargetReport {
        id: "stdlib".into(),
        kind: "ct".into(),
        source: Source {
            repo: "otp".into(),
            path: src.display().to_string(),
            sha: runtime::git_sha(&runtime::repo_root()),
            staged: Some(stage_src.display().to_string()),
        },
        build,
        code_path: code_path.iter().map(|p| p.display().to_string()).collect(),
        invocations,
        rows,
        notes,
    })
}
