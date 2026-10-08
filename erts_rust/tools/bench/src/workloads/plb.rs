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

//! `plb` — the Programming-Language-Benchmarks ("Benchmarks Game") compute kernels.
//!
//! Stages the checkout into a private scratch copy under `$TMPDIR` (the source repo is
//! configured **read-only**) and drives the repo's `dotnet`-based tool there
//! (`dotnet run -c Release --project tool -- --task bench --langs elixir`), reading the
//! machine-readable results it emits under `bench/build/_results/**/*.json` (`timeMS`,
//! `memBytes`) — from the **scratch** copy, so nothing is written back to the source.
//!
//! Repo: `$BENCH_PLB_REPO`, else `/home/bill/Source/Programming-Language-Benchmarks`.
//! `dotnet` is a hard prerequisite — when it is absent the workload is `skipped`
//! (`dotnet-absent`), never a fabricated number. The opt-in `nix shell` fallback (and its
//! `$BENCH_TRY_NIX` switch) is shared with the other tool-driven workloads; see
//! [`crate::workloads::tool`]. It too is validated with a bounded `--version` probe and,
//! if it fails, the honest `dotnet-absent` skip is kept.
//!
//! A tool run that yields **no** result JSON is recorded `failed` (`plb-tool-failed` when
//! `dotnet` exited non-zero, `plb-no-results` when it exited `0` while benching nothing) —
//! never `ok`, because the only other number on such a row is the tool's own wall time,
//! which is not a benchmark result.

use std::path::PathBuf;

use serde_json::Value;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc;
use crate::report::WorkloadResult;
use crate::scratch::{self, ScratchDir};
use crate::workload::{RunContext, Workload};
use crate::workloads::{collect_files, resolve_repo, tool};

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/Programming-Language-Benchmarks";

/// The external tool this workload drives.
const TOOL: &str = "dotnet";

/// The `nixpkgs` attribute that supplies [`TOOL`] for the opt-in `nix` fallback.
const TOOL_NIX_ATTR: &str = "dotnet-sdk";

/// The honest skip reason reported when [`TOOL`] cannot be resolved.
const TOOL_ABSENT: &str = "dotnet-absent";

/// The PLB compute-kernel workload.
pub struct Plb;

fn repo() -> PathBuf {
    resolve_repo("plb", DEFAULT_REPO)
}

impl Workload for Plb {
    fn id(&self) -> &'static str {
        "plb"
    }

    fn description(&self) -> &'static str {
        "Programming-Language-Benchmarks kernels via the dotnet tool"
    }

    fn category(&self) -> Category {
        Category::Compute
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.1", "A.6"]
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::SysOnly
    }

    fn probe(&self) -> Probe {
        if !repo().is_dir() {
            return Probe::skipped("repo-missing");
        }
        if !tool::available(TOOL) {
            return Probe::skipped(TOOL_ABSENT);
        }
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        let r = repo();
        if !r.is_dir() {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "repo-missing",
            );
        }
        let Some(mut cmd) = tool::command_for(TOOL, TOOL_NIX_ATTR) else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                TOOL_ABSENT,
            );
        };

        // Stage the read-only checkout into a private scratch copy and run there, so the
        // `dotnet` build never writes build artifacts (or `_results`) into the source repo.
        let scratch = match ScratchDir::new(self.id()) {
            Ok(dir) => dir,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("scratch-dir-failed: {e}"),
                )
            }
        };
        let work = scratch.path().join(self.id());
        if let Err(e) = scratch::copy_tree(&r, &work, tool::REPO_SKIP_NAMES) {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                format!("scratch-copy-failed: {e}"),
            );
        }

        let bench_dir = work.join("bench");
        if !bench_dir.is_dir() {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "bench-dir-missing",
            );
        }

        cmd.current_dir(&bench_dir)
            .args(["run", "-c", "Release", "--project", "tool", "--"])
            .args(["--task", "bench", "--langs", "elixir"]);

        let out = match proc::run(&mut cmd) {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("dotnet-spawn-failed: {e}"),
                )
            }
        };

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result
            .metrics
            .push(Metric::scalar("wall_ms", "ms", out.wall_ms));
        result.peak_memory_bytes = out.peak_memory_bytes;

        // Read the result JSON from the *scratch* copy, never the (read-only) source repo.
        let (times, mems) = read_results(&bench_dir.join("build/_results"));
        if times.is_empty() {
            // No measurement was produced, so the `wall_ms` recorded above is the *tool's*
            // own runtime and not a benchmark result: the row must never read as `ok`.
            // A non-zero exit is the tool failing; a zero exit with no results means it
            // benched nothing (a silent no-op), which is just as honest a failure.
            result.status = Status::Failed;
            result.reason = Some(no_results_reason(out.success).to_string());
            result.notes.push(format!(
                "no result JSON found (dotnet exit {:?})",
                out.exit_code
            ));
            if !out.success {
                result.notes.push(format!("stderr: {}", out.stderr.trim()));
            }
        } else {
            result
                .metrics
                .push(Metric::samples("test_time_ms", "ms", &times));
            if let Some(max_mem) = mems.iter().copied().fold(None, |a: Option<f64>, b| {
                Some(match a {
                    Some(a) => a.max(b),
                    None => b,
                })
            }) {
                result
                    .metrics
                    .push(Metric::scalar("test_mem_bytes", "bytes", max_mem));
            }
        }
        result
    }
}

/// The honest failure reason when the tool produced **no** benchmark results.
///
/// A non-zero exit is the tool failing outright; a zero exit that still yields no result
/// JSON is a silent no-op (e.g. a `dotnet` build that "succeeded" without benching
/// anything). Either way there is no measurement, so the row is `failed` — never `ok`.
fn no_results_reason(success: bool) -> &'static str {
    if success {
        "plb-no-results"
    } else {
        "plb-tool-failed"
    }
}

/// Reads `timeMS`/`memBytes` fields from every result JSON under `dir`.
fn read_results(dir: &std::path::Path) -> (Vec<f64>, Vec<f64>) {
    let mut times = Vec::new();
    let mut mems = Vec::new();
    for path in collect_files(dir, "json") {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        collect_number(&value, "timeMS", &mut times);
        collect_number(&value, "memBytes", &mut mems);
    }
    (times, mems)
}

/// Recursively collects numeric values stored under `key`.
fn collect_number(value: &Value, key: &str, out: &mut Vec<f64>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                if k == key {
                    if let Some(n) = v.as_f64() {
                        out.push(n);
                    }
                }
                collect_number(v, key, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_number(item, key, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = Plb;
        assert_eq!(w.id(), "plb");
        assert_eq!(w.category(), Category::Compute);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
    }

    #[test]
    fn probe_reports_dotnet_absence() {
        let p = Plb.probe();
        if repo().is_dir() && !tool::available(TOOL) {
            assert_eq!(p.reason(), Some(TOOL_ABSENT));
        }
    }

    #[test]
    fn extracts_numbers_recursively() {
        let v: Value = serde_json::from_str(
            r#"{"results":[{"timeMS":12.5,"memBytes":100},{"timeMS":7.5,"nested":{"timeMS":3.0}}]}"#,
        )
        .unwrap();
        let mut times = Vec::new();
        collect_number(&v, "timeMS", &mut times);
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(times, vec![3.0, 7.5, 12.5]);
        let mut mems = Vec::new();
        collect_number(&v, "memBytes", &mut mems);
        assert_eq!(mems, vec![100.0]);
    }

    #[test]
    fn read_results_handles_missing_dir() {
        let (t, m) = read_results(std::path::Path::new("/nonexistent/xyzzy"));
        assert!(t.is_empty() && m.is_empty());
    }

    /// A run that produced no result JSON is a **failure** — the recorded `wall_ms` is the
    /// tool's own runtime, so it must never be reported as an `ok` benchmark row. The
    /// reason distinguishes "the tool exited non-zero" from "it exited 0 and benched
    /// nothing".
    #[test]
    fn a_run_without_results_is_a_failed_row_not_an_ok_one() {
        assert_eq!(no_results_reason(false), "plb-tool-failed");
        assert_eq!(no_results_reason(true), "plb-no-results");
        assert_ne!(no_results_reason(true), no_results_reason(false));
    }
}
