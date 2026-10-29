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
//! Command dispatch: `list` inspects the environment, `run` performs a sweep and writes the
//! artifacts.
use std::path::PathBuf;
use crate::cli::{Action, RunArgs, TargetSel};
use crate::error::{Result, SuitesError};
use crate::proc;
use crate::report::{HostInfo, RunReport, RustInfo};
use crate::runtime::{self, Runtime, RuntimeKind};
use crate::target::{self, TargetOptions};
use crate::{khepri, stdlib};
/// Runs one parsed action, returning the process exit code.
pub fn dispatch(action: Action) -> Result<i32> {
    match action {
        Action::Help => {
            println!("{}", crate::cli::usage());
            Ok(0)
        }
        Action::List { json } => list(json),
        Action::Run(args) => run(args),
    }
}
/// `suites list`: which targets exist, where their sources are, and what runtime would be
/// used for each side (a missing oracle is reported here too, not swallowed).
fn list(json: bool) -> Result<i32> {
    let sys = Runtime::resolve(RuntimeKind::Sys)?;
    let rust = Runtime::resolve(RuntimeKind::Rust)?;
    let sys_check = sys.self_check();
    let rust_check = rust.self_check();
    let khepri_src = std::env::var_os("KHEPRI_SRC")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(khepri::DEFAULT_SOURCE));
    let stdlib_src = runtime::repo_root().join("lib").join("stdlib").join("test");
    if json {
        let value = crate::json::obj(vec![
            ("runtimes", crate::json::arr(vec![
                runtime_json(&sys, &sys_check),
                runtime_json(&rust, &rust_check),
            ])),
            ("targets", crate::json::arr(vec![
                crate::json::obj(vec![
                    ("id", crate::json::s("khepri")),
                    ("kind", crate::json::s("eunit")),
                    ("source", crate::json::s(khepri_src.display().to_string())),
                    ("present", crate::json::Json::Bool(khepri_src.is_dir())),
                ]),
                crate::json::obj(vec![
                    ("id", crate::json::s("stdlib")),
                    ("kind", crate::json::s("ct")),
                    ("source", crate::json::s(stdlib_src.display().to_string())),
                    ("present", crate::json::Json::Bool(stdlib_src.is_dir())),
                    ("default_suites", crate::json::arr(
                        stdlib::DEFAULT_SUITES.iter().map(|s| crate::json::s(*s)).collect())),
                ]),
            ])),
        ]);
        println!("{}", value.render());
    } else {
        for (rt, check) in [(&sys, &sys_check), (&rust, &rust_check)] {
            match check {
                Ok(version) => println!("runtime {}: {} ({version})", rt.kind.as_str(), rt.program().map(|p| p.display().to_string()).unwrap_or_default()),
                Err(err) => println!("runtime {}: NOT USABLE — {err}", rt.kind.as_str()),
            }
        }
        println!();
        println!("target   kind   source");
        println!("khepri   eunit  {} ({})", khepri_src.display(), if khepri_src.is_dir() { "present" } else { "MISSING" });
        println!("stdlib   ct     {} ({})", stdlib_src.display(), if stdlib_src.is_dir() { "present" } else { "MISSING" });
        println!("         default suites: {}", stdlib::DEFAULT_SUITES.join(", "));
    }
    Ok(0)
}
fn runtime_json(rt: &Runtime, check: &std::result::Result<String, String>) -> crate::json::Json {
    crate::json::obj(vec![
        ("kind", crate::json::s(rt.kind.as_str())),
        ("program", crate::json::opt_str(rt.program().map(|p| p.to_string_lossy().into_owned()).as_deref())),
        ("note", crate::json::opt_str(rt.absent_reason.as_deref())),
        (
            "version",
            crate::json::s(match check {
                Ok(v) => v.clone(),
                Err(e) => format!("unusable: {e}"),
            }),
        ),
    ])
}
/// `suites run`: the sweep.
fn run(args: RunArgs) -> Result<i32> {
    let mut runtimes: Vec<Runtime> = Vec::new();
    for kind in &args.runtimes {
        let rt = Runtime::resolve(*kind)?;
        if rt.program().is_none() && *kind == RuntimeKind::Rust && !args.runtimes.contains(&RuntimeKind::Sys) {
            return Err(SuitesError::Env(
                rt.absent_reason.clone().unwrap_or_else(|| "no Rust runtime".into()),
            ));
        }
        runtimes.push(rt);
    }
    if runtimes.iter().any(|r| r.kind == RuntimeKind::Sys && r.program().is_none()) {
        return Err(SuitesError::Env("the oracle runtime is required".into()));
    }
    let sys = runtimes.iter().find(|r| r.kind == RuntimeKind::Sys);
    let rust = runtimes.iter().find(|r| r.kind == RuntimeKind::Rust);
    let host = HostInfo {
        loadavg_before: proc::loadavg(),
        loadavg_after: None,
        nproc: proc::nproc(),
        oracle: sys
            .and_then(Runtime::program)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<none>".into()),
        oracle_version: sys.and_then(Runtime::version_line),
    };
    let rust_info = RustInfo {
        path: rust.and_then(Runtime::program).map(|p| p.display().to_string()),
        sha: runtime::git_sha(&runtime::workspace_root()),
        version: rust.and_then(Runtime::version_line),
        tier: std::env::var("BEAM_RT_TIER").ok().filter(|v| !v.is_empty()),
    };
    let stage_root = target::stage_root();
    let mut targets = Vec::new();
    if args.targets.includes_khepri() {
        let opts = TargetOptions {
            source: args.khepri_src.clone().unwrap_or_default(),
            only: args.only.clone(),
            timeout: args.timeout(),
            suite_budget: args.suite_budget(),
            jobs: args.jobs,
            verbose: args.verbose,
            rebuild: args.rebuild,
            keep_stage: args.keep_stage,
        };
        targets.push(khepri::run(&runtimes, &opts, &stage_root.join("khepri"))?);
    }
    if args.targets.includes_stdlib() {
        let opts = TargetOptions {
            source: args.stdlib_src.clone().unwrap_or_default(),
            only: args.only.clone(),
            timeout: args.timeout(),
            suite_budget: args.suite_budget(),
            jobs: args.jobs,
            verbose: args.verbose,
            rebuild: args.rebuild,
            keep_stage: args.keep_stage,
        };
        targets.push(stdlib::run(&runtimes, &opts, &stage_root.join("stdlib"))?);
    }
    let mut host = host;
    host.loadavg_after = proc::loadavg();
    let report = RunReport {
        generated_at: proc::epoch_secs(),
        run_command: format!("suites {}", std::env::args().skip(1).collect::<Vec<_>>().join(" ")),
        host,
        rust: rust_info,
        targets,
    };
    write_artifacts(&args, &report)?;
    if args.json {
        println!("{}", report.to_json().render());
    } else {
        print!("{}", report.to_markdown());
    }
    Ok(0)
}
/// Writes `<out>/report.json` and `<out>/summary.md` (or the exact `--out-file`).
fn write_artifacts(args: &RunArgs, report: &RunReport) -> Result<()> {
    let out_dir = args
        .out_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("results"));
    let json_path = args.out_file.clone().unwrap_or_else(|| out_dir.join("report.json"));
    target::write_file(&json_path, &format!("{}\n", report.to_json().render()))?;
    if args.out_file.is_none() {
        target::write_file(&out_dir.join("summary.md"), &report.to_markdown())?;
    }
    eprintln!("suites: wrote {}", json_path.display());
    Ok(())
}
/// The target ids a selection covers (used by the tests).
pub fn selected_targets(sel: TargetSel) -> Vec<&'static str> {
    let mut ids = Vec::new();
    if sel.includes_khepri() {
        ids.push("khepri");
    }
    if sel.includes_stdlib() {
        ids.push("stdlib");
    }
    ids
}
