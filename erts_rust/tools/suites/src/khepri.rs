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
//! The **Khepri** target: 44 eunit modules of a real third-party project.
//!
//! The checkout (`/home/bill/Source/khepri`, or `$KHEPRI_SRC`) is **read-only**: it is
//! copied into `$TMPDIR/erts-suites-stage/khepri/proj`, and `rebar3 as test compile` runs
//! only inside that copy. `rebar3` is usually not on `$PATH` in this sandbox, so it is
//! resolved from `$REBAR3`, then `$PATH`, then the Nix store. Because the real `$HOME` is
//! read-only, `HOME`, `REBAR_CACHE_DIR`, `XDG_CACHE_HOME` and `HEX_HOME` are redirected into
//! the stage (hex is reachable, so the `test` profile's deps — `ra`, `horus`, `aten`,
//! `gen_batch_server`, `seshat`, `meck`, `proper`, `cf`, `cth_readable` — are fetched once
//! and reused by later runs).
//!
//! `rebar3 as test compile` is used for the **build only**: the comparison numbers come from
//! running `eunit:test(Module, [verbose])` directly on both sides, never from `rebar3 eunit`.
//! The compiled test beams land in `_build/test/lib/khepri/test`, so the `-pa` set is every
//! `_build/test/lib/*/ebin` plus that directory.
use std::path::{Path, PathBuf};
use std::process::Command;
use crate::error::{Result, SuitesError};
use crate::eunit;
use crate::proc;
use crate::report::{self, Build, Invocation, Outcome, Row, RowKind, Source, Status, TargetReport};
use crate::runtime::{self, Runtime, RuntimeKind};
use crate::target::{self, TargetOptions};
/// The default checkout, when `$KHEPRI_SRC` is unset.
pub const DEFAULT_SOURCE: &str = "/home/bill/Source/khepri";
/// How long the one-time `rebar3 as test compile` may take (dependency fetch included).
const BUILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);
/// The `eunit` drive given to **both** sides, with `{module}` replaced.
///
/// `halt(0)` guarantees the run terminates even when the module under test leaves processes
/// behind; the per-run timeout is the backstop.
pub const DRIVE: &str = "eunit:test('{module}',[verbose]), halt(0).";
/// Builds the drive expression for one module.
pub fn drive(module: &str) -> String {
    DRIVE.replace("{module}", module)
}
/// The source checkout this run will stage.
pub fn source(opts: &TargetOptions) -> PathBuf {
    if opts.source.as_os_str().is_empty() {
        std::env::var_os("KHEPRI_SRC")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SOURCE))
    } else {
        opts.source.clone()
    }
}
/// Finds `rebar3`: `$REBAR3`, then `$PATH`, then the Nix store.
pub fn find_rebar3() -> Result<PathBuf> {
    if let Some(v) = std::env::var_os("REBAR3").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(v);
        if p.is_file() {
            return Ok(p);
        }
        return Err(SuitesError::Env(format!("$REBAR3={} is not a file", p.display())));
    }
    if let Some(p) = proc::which("rebar3") {
        return Ok(p);
    }
    if let Some(p) = proc::find_in_nix_store("rebar3", "rebar3") {
        return Ok(p);
    }
    Err(SuitesError::Env(
        "no rebar3: set $REBAR3, put it on $PATH, or install it in /nix/store".into(),
    ))
}
/// Stages the checkout and builds the `test` profile. Returns `(project_dir, Build)`.
fn stage_and_build(
    src: &Path,
    stage: &Path,
    opts: &TargetOptions,
    rebar3: &Path,
) -> Result<(PathBuf, Build)> {
    let project = stage.join("proj");
    let marker = stage.join(".build-ok");
    let home = stage.join("home");
    let build_command = format!("{} as test compile", rebar3.display());
    if marker.is_file() && !opts.rebuild {
        return Ok((
            project,
            Build {
                command: build_command,
                ok: true,
                reused: true,
                detail: "reused a completed `rebar3 as test compile` in the staged copy".into(),
            },
        ));
    }
    target::remove_tree(stage)?;
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(stage.join("tmp"))?;
    target::copy_tree(src, &project)?;
    let mut cmd = Command::new(rebar3);
    cmd.arg("as").arg("test").arg("compile");
    cmd.current_dir(&project);
    cmd.env("HOME", &home)
        .env("REBAR_CACHE_DIR", home.join(".cache").join("rebar3"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local").join("share"))
        .env("HEX_HOME", home.join(".hex"))
        .env("TMPDIR", stage.join("tmp"));
    if opts.verbose {
        eprintln!("[khepri] build: {}", proc::cmdline(&cmd));
    }
    let run = proc::run_with_timeout(&mut cmd, Some(BUILD_TIMEOUT))?;
    let ok = run.success;
    if ok {
        std::fs::write(&marker, proc::cmdline(&cmd))?;
    }
    let detail = if ok {
        format!("built in {} ms", run.wall_ms.round())
    } else {
        format!(
            "exit {:?}{}: {}",
            run.exit_code,
            if run.timed_out { " (timed out)" } else { "" },
            report::one_line(&run.tail(12), 900)
        )
    };
    Ok((project, Build { command: build_command, ok, reused: false, detail }))
}
/// The `-pa` set: every dependency/test ebin plus the project's compiled test directory.
pub fn code_path(project: &Path) -> Vec<PathBuf> {
    let lib = project.join("_build").join("test").join("lib");
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&lib) {
        for entry in entries.flatten() {
            let ebin = entry.path().join("ebin");
            if ebin.is_dir() {
                dirs.push(ebin);
            }
        }
    }
    let test_dir = lib.join("khepri").join("test");
    if test_dir.is_dir() {
        dirs.push(test_dir);
    }
    dirs.sort();
    dirs
}
/// The candidate modules: every beam compiled into the test directory.
///
/// Modules without eunit tests (helpers, `proper` properties, the `common_test` suite) are
/// *kept* in the list and reported as `no_tests` rather than silently dropped — that is how
/// `cluster_SUITE` shows up as "a `common_test` suite, 0 eunit tests" instead of vanishing.
pub fn discover(project: &Path) -> Vec<String> {
    let test_dir = project.join("_build").join("test").join("lib").join("khepri").join("test");
    let mut modules: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&test_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("beam") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    modules.push(stem.to_string());
                }
            }
        }
    }
    if modules.is_empty() {
        // Fall back to the sources if the build layout ever changes: the candidate set is the
        // set of test modules, not a hard-coded list.
        if let Ok(entries) = std::fs::read_dir(project.join("test")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("erl") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        modules.push(stem.to_string());
                    }
                }
            }
        }
    }
    modules.sort();
    modules
}
/// Observes one module on one side.
fn observe(
    runtime: &Runtime,
    code_path: &[PathBuf],
    module: &str,
    opts: &TargetOptions,
    cwd: &Path,
) -> Outcome {
    if runtime.program().is_none() {
        return Outcome::bare(Status::NotRun, runtime.absent_reason.clone().unwrap_or_default());
    }
    let eval = drive(module);
    match target::invoke(runtime, code_path, &eval, opts.timeout, &[], Some(cwd)) {
        Ok(run) => {
            if run.timed_out {
                return Outcome::bare(
                    Status::Timeout,
                    format!("killed after {} ms", run.wall_ms.round()),
                );
            }
            let summary = eunit::parse(&run.stdout);
            if summary.status == eunit::EunitStatus::Unknown {
                return target::unclassified(&run);
            }
            summary.outcome()
        }
        Err(err) => Outcome::bare(Status::Error, err),
    }
}
/// Runs the Khepri target end to end.
pub fn run(runtimes: &[Runtime], opts: &TargetOptions, stage: &Path) -> Result<TargetReport> {
    let src = source(opts);
    if !src.is_dir() {
        return Err(SuitesError::Env(format!(
            "no Khepri checkout at {} (set $KHEPRI_SRC)",
            src.display()
        )));
    }
    let rebar3 = find_rebar3()?;
    let (project, build) = stage_and_build(&src, stage, opts, &rebar3)?;
    let code_path = code_path(&project);
    let cwd_root = stage.join("cwd");
    let mut notes = vec![
        format!(
            "drive (identical on both sides): `{}` — the module name is substituted per row",
            DRIVE
        ),
        "`rebar3 as test compile` is used for the build only; the numbers come from running `eunit:test/2` directly on both runtimes".to_string(),
        format!("code path: {} entries (every `_build/test/lib/*/ebin` plus `_build/test/lib/khepri/test`)", code_path.len()),
        format!("jobs: {} concurrent module runs (each in its own OS process and working directory)", opts.jobs),
        format!("per-module working directory: `{}` (khepri's tests create `_test.<name>` dirs relative to the cwd; both sides of a module share one)", cwd_root.display()),
    ];
    let mut rows = Vec::new();
    if !build.ok {
        rows.push(Row {
            id: "build".into(),
            suite: None,
            kind: RowKind::Notice,
            sys: Outcome::bare(Status::Error, format!("build failed: {}", build.detail)),
            rust: Outcome::bare(Status::NotRun, ""),
            note: "the target could not be built, so no module was measured".into(),
        });
        notes.push("build failed — see `build.detail` in the JSON artifact".into());
    } else {
        let mut modules = discover(&project);
        if !opts.only.is_empty() {
            modules.retain(|m| opts.only.iter().any(|o| o == m));
        }
        if modules.is_empty() {
            return Err(SuitesError::Data(format!(
                "--module filter matched no compiled test module ({} candidates)",
                discover(&project).len()
            )));
        }
        // One representative invocation per side is recorded; every row uses exactly this
        // shape with its own module name.
        let sample = modules.first().cloned().unwrap_or_default();
        for runtime in runtimes {
            rows_invocations(&mut notes, runtime, &code_path, &sample, &cwd_root.join(&sample));
        }
        rows = modules_rows(runtimes, &code_path, &modules, opts, &cwd_root)?;
        for row in &mut rows {
            if row.kind == RowKind::Notice && row.note.is_empty() {
                row.note = "no eunit tests (common_test suite or helper module)".into();
            }
        }
    }
    let mut invocations = Vec::new();
    let sample = discover(&project).first().cloned().unwrap_or_else(|| "simple_get".into());
    for runtime in runtimes {
        invocations.push(Invocation {
            side: runtime.kind.as_str().to_string(),
            purpose: format!("eunit {sample}"),
            command: target::describe(runtime, &code_path, &drive(&sample), &[], Some(&cwd_root.join(&sample))),
        });
    }
    invocations.push(Invocation {
        side: "build".into(),
        purpose: "rebar3 as test compile".into(),
        command: format!("cd <stage>/proj && {}", build.command),
    });
    Ok(TargetReport {
        id: "khepri".into(),
        kind: "eunit".into(),
        source: Source {
            repo: "khepri".into(),
            path: src.display().to_string(),
            sha: runtime::git_sha(&src),
            staged: Some(project.display().to_string()),
        },
        build,
        code_path: code_path.iter().map(|p| p.display().to_string()).collect(),
        invocations,
        rows,
        notes,
    })
}
/// Placeholder so the per-side documentation note is produced once per resolved side.
fn rows_invocations(
    notes: &mut Vec<String>,
    runtime: &Runtime,
    code_path: &[PathBuf],
    module: &str,
    cwd: &Path,
) {
    let side = runtime.kind.as_str();
    match runtime.program() {
        Some(_) => notes.push(format!(
            "{side}: `{}`",
            target::describe(runtime, code_path, &drive(module), &[], Some(cwd))
        )),
        None => notes.push(format!(
            "{side}: not run ({})",
            runtime.absent_reason.clone().unwrap_or_else(|| "absent".into())
        )),
    }
}
/// Observes every module on every side, in parallel, and pairs the results into rows.
fn modules_rows(
    runtimes: &[Runtime],
    code_path: &[PathBuf],
    modules: &[String],
    opts: &TargetOptions,
    cwd: &Path,
) -> Result<Vec<Row>> {
    let sys = runtimes.iter().find(|r| r.kind == RuntimeKind::Sys);
    let rust = runtimes.iter().find(|r| r.kind == RuntimeKind::Rust);
    let observed: Vec<(String, Outcome, Outcome)> = target::par_map(opts.jobs, modules, |module| {
        // Each module gets its own working directory: khepri's tests create `_test.<name>`
        // directories *relative to the cwd*, so a shared cwd would make concurrent modules
        // (and re-runs) interfere. Both sides of one module still share one cwd, which is
        // what makes the comparison fair.
        let cwd = cwd.join(module);
        if let Err(err) = target::fresh_dir(&cwd) {
            return (module.clone(), Outcome::bare(Status::Error, err.clone()), Outcome::bare(Status::Error, err));
        }
        let sys_outcome = match sys {
            Some(rt) => observe(rt, code_path, module, opts, &cwd),
            None => Outcome::bare(Status::NotRun, "oracle not selected"),
        };
        let rust_outcome = match rust {
            Some(rt) => observe(rt, code_path, module, opts, &cwd),
            None => Outcome::bare(Status::NotRun, "rust runtime not selected"),
        };
        (module.clone(), sys_outcome, rust_outcome)
    });
    let mut rows = Vec::new();
    for (module, sys_outcome, rust_outcome) in observed {
        let has_tests = matches!(sys_outcome.status, Status::Pass | Status::Fail)
            || matches!(rust_outcome.status, Status::Pass | Status::Fail);
        let mut note = String::new();
        if module.ends_with("_SUITE") && !has_tests {
            note = "a common_test suite: it carries no eunit tests (0/0 would be a lie)".into();
        }
        rows.push(Row {
            id: module,
            suite: None,
            kind: if has_tests { RowKind::Eunit } else { RowKind::Notice },
            sys: sys_outcome,
            rust: rust_outcome,
            note,
        });
    }
    Ok(rows)
}
