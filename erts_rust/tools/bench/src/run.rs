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

//! The runner and the top-level command dispatcher.
//!
//! [`dispatch`] is the single entry point the binary calls: it executes a parsed
//! [`Command`] and returns the process exit code (`0` on success; errors bubble up as
//! [`BenchError`], which the binary maps to `2`). The core runner rule — *every
//! scheduled `(workload × runtime)` pair yields exactly one [`WorkloadResult`]* — is
//! implemented in [`run_all`].

use std::path::Path;

use serde::Serialize;

use crate::cli::{self, Command, RunArgs, RuntimeSel};
use crate::compare;
use crate::error::{BenchError, Result};
use crate::metrics::{Metric, Probe};
use crate::report::{Profile, Report, WorkloadResult};
use crate::runtime::{self, Runtime, RuntimeInfo};
use crate::workload::{self, RunContext};
use crate::workloads;

/// Executes a parsed command and returns the process exit code.
pub fn dispatch(command: &Command) -> Result<i32> {
    match command {
        Command::Help(sub) => {
            print!("{}", cli::help_text(sub.as_deref()));
            Ok(0)
        }
        Command::List { json } => list(*json),
        Command::Run(args) => {
            run_all(args)?;
            Ok(0)
        }
        Command::Baseline { profile, json, out, out_file } => {
            let args = RunArgs {
                profile: *profile,
                workloads: Some(vec!["baseline".to_string()]),
                runtime: RuntimeSel::Sys,
                out: out.clone(),
                out_file: out_file.clone(),
                json: *json,
                verbose: false,
                repos: Vec::new(),
            };
            run_all(&args)?;
            Ok(0)
        }
        Command::Compare { a, b, json } => match b {
            Some(b) => compare_reports(a, b, *json),
            None => cross_compare_report(a, *json),
        },
    }
}

/// A machine-readable listing entry for `bench list --json`.
#[derive(Debug, Serialize)]
struct ListEntry {
    id: &'static str,
    description: &'static str,
    category: &'static str,
    contracts: &'static [&'static str],
    runtimes: &'static str,
    probe_ready: bool,
    probe_reason: Option<String>,
}

/// `bench list`.
fn list(json: bool) -> Result<i32> {
    let entries: Vec<ListEntry> = workload::registry()
        .into_iter()
        .map(|w| {
            let probe = w.probe();
            ListEntry {
                id: w.id(),
                description: w.description(),
                category: w.category().as_str(),
                contracts: w.contracts(),
                runtimes: match w.runtimes() {
                    crate::metrics::Runtimes::Both => "both",
                    crate::metrics::Runtimes::SysOnly => "sys-only",
                },
                probe_ready: matches!(probe, Probe::Ready),
                probe_reason: probe.reason().map(str::to_string),
            }
        })
        .collect();

    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else {
        for e in &entries {
            let probe = match &e.probe_reason {
                Some(r) => format!("skipped({r})"),
                None => "ready".to_string(),
            };
            println!(
                "{:<16} {:<10} {:<9} probe={:<28} contracts={}",
                e.id,
                e.category,
                e.runtimes,
                probe,
                e.contracts.join(",")
            );
            println!("{:16} {}", "", e.description);
        }
    }
    Ok(0)
}

/// The core runner: resolve runtimes, iterate `(runtime × workload)`, write the report.
fn run_all(args: &RunArgs) -> Result<Report> {
    // Apply `--repo` overrides (validated against the repo-backed workload ids).
    for (id, path) in &args.repos {
        if !workloads::REPO_WORKLOADS.contains(&id.as_str()) {
            return Err(BenchError::usage(format!(
                "--repo `{id}` is not a repo-backed workload (expected one of: {})",
                workloads::REPO_WORKLOADS.join(", ")
            )));
        }
        workloads::set_repo_override(id, path);
    }

    // Resolve the workload selection.
    let selected: Vec<Box<dyn workload::Workload>> = match &args.workloads {
        None => workload::registry(),
        Some(ids) => {
            let mut chosen = Vec::with_capacity(ids.len());
            for id in ids {
                match workload::find(id) {
                    Some(w) => chosen.push(w),
                    None => {
                        return Err(BenchError::usage(format!(
                            "unknown workload `{id}` (run `bench list` to see the ids)"
                        )))
                    }
                }
            }
            chosen
        }
    };

    // Resolve the runtimes. The oracle is mandatory whenever it is in the selection.
    let mut runtimes: Vec<Runtime> = Vec::new();
    if args.runtime.wants_sys() {
        runtimes.push(runtime::resolve_oracle()?);
    }
    if args.runtime.wants_rust() {
        runtimes.push(runtime::resolve_rust());
    }

    let out_dir = args
        .out
        .clone()
        .unwrap_or_else(workloads::default_out_dir);
    std::fs::create_dir_all(&out_dir)?;

    let infos: Vec<RuntimeInfo> = runtimes.iter().map(|r| r.info()).collect();

    let mut results: Vec<WorkloadResult> = Vec::new();
    for (runtime, info) in runtimes.iter().zip(infos.iter()) {
        for w in &selected {
            results.push(run_one(w.as_ref(), runtime, info.supported, &out_dir, args.profile));
        }
    }

    let report = Report::new(args.profile, infos, results);
    let out_path = match &args.out_file {
        Some(path) => {
            // Write to the exact requested path; make sure its parent exists.
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            path.clone()
        }
        None => report.resolve_out_path(&out_dir),
    };
    report.save_to(&out_path)?;

    if args.json {
        println!("{}", report.to_json_pretty()?);
    } else {
        print_report(&report, &out_path, args.verbose);
    }
    Ok(report)
}

/// Runs (or skips) a single `(workload × runtime)` pair, always yielding one result.
fn run_one(
    w: &dyn workload::Workload,
    rt: &Runtime,
    runtime_supported: bool,
    out_dir: &Path,
    profile: Profile,
) -> WorkloadResult {
    let kind = rt.kind;
    if !w.runtimes().allows(kind) || !runtime_supported {
        return WorkloadResult::skipped(
            w.id(),
            kind,
            w.category(),
            w.description(),
            "runtime-unsupported",
        );
    }
    match w.probe() {
        Probe::Skipped(reason) => {
            WorkloadResult::skipped(w.id(), kind, w.category(), w.description(), reason)
        }
        Probe::Ready => {
            let ctx = RunContext::new(rt, profile, out_dir);
            w.run(&ctx)
        }
    }
}

/// Prints the human-readable report table.
fn print_report(report: &Report, out_path: &Path, verbose: bool) {
    println!(
        "bench {} — profile {} — {} runtime(s), {} result(s)",
        report.tool_version,
        report.profile.as_str(),
        report.runtimes.len(),
        report.results.len()
    );
    for info in &report.runtimes {
        let program = info.program.as_deref().unwrap_or("<unresolved>");
        let version = info.version.as_deref().unwrap_or("-");
        println!(
            "  runtime {:<4} supported={:<5} version={:<8} {}",
            info.kind.as_str(),
            info.supported,
            version,
            program
        );
        if verbose {
            for note in &info.notes {
                println!("        note: {note}");
            }
        }
    }
    for r in &report.results {
        let reason = r
            .reason
            .as_deref()
            .map(|x| format!(" ({x})"))
            .unwrap_or_default();
        println!(
            "  [{}] {:<16} {:<4} {}",
            r.status.as_str(),
            r.workload,
            r.runtime.as_str(),
            reason
        );
        for m in &r.metrics {
            println!("        {}", format_metric(m));
        }
        if let Some(bytes) = r.peak_memory_bytes {
            println!("        peak_memory_bytes={bytes}");
        }
        if verbose {
            for note in &r.notes {
                println!("        note: {note}");
            }
        }
    }
    println!("wrote {}", out_path.display());
}

/// Formats one metric for the human table.
fn format_metric(m: &Metric) -> String {
    if let Some(v) = m.value {
        return format!("{}={v:.4} {}", m.name, m.unit);
    }
    match &m.summary {
        Some(s) => format!(
            "{}: n={} min={:.4} mean={:.4} p50={:.4} p99={:.4} {}",
            m.name, s.count, s.min, s.mean, s.p50, s.p99, m.unit
        ),
        None => format!("{}=<none>", m.name),
    }
}

/// `bench compare`.
fn compare_reports(a: &Path, b: &Path, json: bool) -> Result<i32> {
    let ra = Report::load_from(a)?;
    let rb = Report::load_from(b)?;
    let label_a = file_label(a);
    let label_b = file_label(b);
    let cmp = compare::compare_labeled(&ra, &rb, &label_a, &label_b);

    if json {
        println!("{}", cmp.to_json_pretty()?);
    } else {
        println!(
            "compare {} -> {}  ({} row(s), {} numeric)",
            cmp.a,
            cmp.b,
            cmp.rows.len(),
            cmp.numeric_rows()
        );
        for row in &cmp.rows {
            let metric = row.metric.as_deref().unwrap_or("-");
            match (&row.a_value, &row.b_value) {
                (Some(av), Some(bv)) => {
                    let pct = row
                        .pct_delta
                        .map(|p| format!("{p:+.2}%"))
                        .unwrap_or_else(|| "n/a".to_string());
                    println!(
                        "  {:<16} {:<4} {:<22} {av:.4} -> {bv:.4}  Δ={:+.4} ({pct})",
                        row.workload,
                        row.runtime,
                        metric,
                        row.abs_delta.unwrap_or(0.0)
                    );
                }
                _ => {
                    let note = row.note.as_deref().unwrap_or("no-comparable-value");
                    println!(
                        "  {:<16} {:<4} {:<22} {note}",
                        row.workload, row.runtime, metric
                    );
                }
            }
        }
    }
    Ok(0)
}

/// `bench compare <report.json>` — the cross-runtime (`sys` vs `rust`) comparison
/// *within* a single report (typically produced by `bench run --runtime all`).
fn cross_compare_report(path: &Path, json: bool) -> Result<i32> {
    let report = Report::load_from(path)?;
    let label = file_label(path);
    let cmp = compare::cross_compare_labeled(&report, &label);

    if json {
        println!("{}", cmp.to_json_pretty()?);
    } else {
        println!(
            "cross-runtime compare {} — {} workload(s), {} numeric row(s)",
            cmp.report,
            cmp.workload_count(),
            cmp.numeric_rows()
        );
        print!("{}", cmp.render_table());
    }
    Ok(0)
}

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{Category, Status};
    use crate::runtime::RuntimeKind;
    use crate::workload::Workload;
    use std::path::PathBuf;

    #[test]
    fn format_scalar_metric() {
        let m = Metric::scalar("startup_ms", "ms", 12.34567);
        assert_eq!(format_metric(&m), "startup_ms=12.3457 ms");
    }

    #[test]
    fn format_summary_metric() {
        let m = Metric::samples("startup_ms", "ms", &[1.0, 2.0, 3.0]);
        let s = format_metric(&m);
        assert!(s.contains("n=3"));
        assert!(s.contains("p99="));
    }

    #[test]
    fn run_one_gates_rust_for_sysonly_workloads() {
        let rt = Runtime {
            kind: RuntimeKind::Rust,
            program: Some(PathBuf::from("/bin/true")),
        };
        let w = workloads::khepri::Khepri;
        let res = run_one(&w, &rt, true, Path::new("/tmp"), Profile::Quick);
        assert_eq!(res.status, Status::Skipped);
        assert_eq!(res.reason.as_deref(), Some("runtime-unsupported"));
    }

    #[test]
    fn run_one_gates_unsupported_runtime() {
        let rt = Runtime {
            kind: RuntimeKind::Rust,
            program: None,
        };
        let w = workloads::baseline::Baseline;
        let res = run_one(&w, &rt, false, Path::new("/tmp"), Profile::Quick);
        assert_eq!(res.status, Status::Skipped);
        assert_eq!(res.reason.as_deref(), Some("runtime-unsupported"));
    }

    #[test]
    fn run_one_reports_probe_skips_without_running() {
        let rt = Runtime {
            kind: RuntimeKind::Sys,
            program: Some(PathBuf::from("/bin/true")),
        };
        // A workload whose probe fails in this environment must be skipped with its
        // reason verbatim; `khepri` is skipped for `rebar3-absent` here.
        let w = workloads::khepri::Khepri;
        if let Probe::Skipped(reason) = w.probe() {
            let res = run_one(&w, &rt, true, Path::new("/tmp"), Profile::Quick);
            assert_eq!(res.status, Status::Skipped);
            assert_eq!(res.reason.as_deref(), Some(reason.as_str()));
        }
    }

    #[test]
    fn skipped_result_has_a_reason_and_never_ok() {
        let r = WorkloadResult::skipped(
            "x",
            RuntimeKind::Sys,
            Category::Io,
            "d",
            "no-http-load-generator",
        );
        assert_ne!(r.status, Status::Ok);
        assert!(r.reason.is_some());
    }
}
