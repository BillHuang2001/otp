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
pub const HELPER_MODULES: &[&str] = &["error_info_lib", "dict_test_lib"];
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
/// The per-case drive given to **both** sides, with `{suite}`, `{timeout_ms}` and
/// `{data_root}` substituted.
pub const DRIVE: &str = "ct_runner:run('{suite}', {timeout_ms}, \"{data_root}\"), halt(0).";
/// Builds the drive expression for one suite.
pub fn drive(suite: &str, timeout_ms: u64, data_root: &Path) -> String {
    DRIVE
        .replace("{suite}", suite)
        .replace("{timeout_ms}", &timeout_ms.to_string())
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
/// Compiles the selected suites, their helper modules and the runner. Returns the ebin dir.
fn compile(stage_dir: &Path, stage_src: &Path, suites: &[String], erlc: &Path, opts: &TargetOptions) -> Result<(PathBuf, Vec<String>)> {
    let ebin = stage_dir.join("ebin");
    let ctl = stage_dir.join("ctl");
    std::fs::create_dir_all(&ebin)?;
    std::fs::create_dir_all(&ctl)?;
    std::fs::write(ctl.join("ct_runner.erl"), RUNNER)?;
    let mut failures = Vec::new();
    let mut modules: Vec<String> = suites.to_vec();
    modules.extend(HELPER_MODULES.iter().map(|s| s.to_string()));
    modules.push("ct_runner".into());
    for module in &modules {
        let dir = if module == "ct_runner" { &ctl } else { stage_src };
        let source_file = dir.join(format!("{module}.erl"));
        if !source_file.is_file() {
            failures.push(format!("{module}: source not found at {}", source_file.display()));
            continue;
        }
        let mut cmd = Command::new(erlc);
        cmd.arg("-o").arg(&ebin).arg("-I").arg(stage_src).arg(&source_file);
        if opts.verbose {
            eprintln!("[stdlib] compile: {}", proc::cmdline(&cmd));
        }
        let run = proc::run_with_timeout(&mut cmd, Some(std::time::Duration::from_secs(300)))?;
        if !run.success {
            failures.push(format!(
                "{module}: exit {:?}: {}",
                run.exit_code,
                report::one_line(&run.tail(8), 500)
            ));
        }
    }
    Ok((ebin, failures))
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
/// Parses the runner's output into `(case, status)` pairs, in output order.
pub fn parse(stdout: &str) -> (Vec<(String, CaseStatus)>, Option<String>, Option<String>) {
    let mut cases = Vec::new();
    let mut init_failed = None;
    let mut suite_skip = None;
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
        }
    }
    (cases, init_failed, suite_skip)
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
/// Observes one suite on one side: `(cases, failure)`, where `failure` replaces the case rows
/// when the suite could not start at all.
fn observe(
    runtime: &Runtime,
    code_path: &[PathBuf],
    suite: &str,
    data_root: &Path,
    opts: &TargetOptions,
    cwd: &Path,
) -> (Vec<(String, CaseStatus)>, Option<Outcome>) {
    if runtime.program().is_none() {
        return (
            Vec::new(),
            Some(Outcome::bare(
                Status::NotRun,
                runtime.absent_reason.clone().unwrap_or_default(),
            )),
        );
    }
    let timeout_ms = opts.timeout.as_millis() as u64;
    let eval = drive(suite, timeout_ms, data_root);
    match target::invoke(
        runtime,
        code_path,
        &eval,
        opts.timeout + std::time::Duration::from_secs(30),
        &[],
        Some(cwd),
    ) {
        Ok(run) => {
            if run.timed_out {
                return (
                    Vec::new(),
                    Some(Outcome::bare(Status::Timeout, format!("suite killed after {} ms", run.wall_ms.round()))),
                );
            }
            let (cases, init_failed, suite_skip) = parse(&run.stdout);
            if cases.is_empty() {
                return (Vec::new(), Some(target::unclassified(&run)));
            }
            if let Some(reason) = init_failed {
                return (cases, Some(Outcome::bare(Status::InitFail, reason)));
            }
            if let Some(reason) = suite_skip {
                return (cases, Some(Outcome::bare(Status::Skip, reason)));
            }
            (cases, None)
        }
        Err(err) => (Vec::new(), Some(Outcome::bare(Status::Error, err))),
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
    let (ebin, compile_failures) = compile(stage_dir, &stage_src, &suites, &erlc, opts)?;
    let code_path = vec![ebin.clone(), stage_dir.join("ctl")];
    let cwd_root = stage_dir.join("cwd");
    let build = Build {
        command: format!("{} -o <stage>/ebin -I <stage>/src <suite>.erl …", erlc.display()),
        ok: compile_failures.is_empty(),
        reused: !opts.rebuild && stage_dir.join(".build-ok").is_file(),
        detail: if compile_failures.is_empty() {
            format!("compiled {} suites + helpers + runner with the oracle's erlc", suites.len())
        } else {
            compile_failures.join("; ")
        },
    };
    if build.ok {
        std::fs::write(stage_dir.join(".build-ok"), &build.command)?;
    }
    let mut notes = vec![
        format!("drive (identical on both sides): `{}`", DRIVE),
        "each case runs in its own `spawn_monitor` with a per-case timeout; `init_per_group`/`end_per_group`/`init_per_testcase` hooks are NOT executed".to_string(),
        "cases that fail identically on both sides (missing `ct` config/data dirs, group-hook dependencies) are `agree-fail-identical`: excluded from both numerators, never counted as passes".to_string(),
        format!("per-case timeout: {} s", opts.timeout.as_secs()),
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
                &drive(&sample, opts.timeout.as_millis() as u64, &stage_src),
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
        let sys = match oracle {
            Some(rt) => observe(rt, &code_path, suite, &stage_src, opts, &cwd),
            None => (Vec::new(), Some(Outcome::bare(Status::NotRun, "oracle not selected"))),
        };
        let rust = match runtimes.iter().find(|r| r.kind == RuntimeKind::Rust) {
            Some(rt) => observe(rt, &code_path, suite, &stage_src, opts, &cwd),
            None => (Vec::new(), Some(Outcome::bare(Status::NotRun, "rust runtime not selected"))),
        };
        (suite.clone(), sys, rust)
    });
    for (suite, (sys_cases, sys_failure), (rust_cases, rust_failure)) in observed {
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
        for (name, _) in &rust_cases {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        let sys_map = |name: &str| sys_cases.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone());
        let rust_map = |name: &str| rust_cases.iter().find(|(n, _)| n == name).map(|(_, s)| s.clone());
        let mut sys_count = (0u32, 0u32);
        let mut rust_count = (0u32, 0u32);
        for name in &names {
            let sys_outcome = match (sys_map(name), &sys_failure) {
                (Some(status), _) => outcome_of(&status),
                (None, Some(failure)) => failure.clone(),
                (None, None) => Outcome::bare(Status::Error, "case missing from the oracle's output"),
            };
            let rust_outcome = match (rust_map(name), &rust_failure) {
                (Some(status), _) => outcome_of(&status),
                (None, Some(failure)) => failure.clone(),
                (None, None) => Outcome::bare(Status::Error, "case missing from the rust runtime's output"),
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
