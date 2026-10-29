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

//! `cowboy` — the HTTP I/O-bound workload (the Cowboy `hello_world` example), measured
//! under **both** runtimes.
//!
//! The Cowboy checkout is a **read-only** foreign repository, so the workload never
//! builds it in place: [`cowboy_serve`](super::cowboy_serve) stages a copy into a private
//! [`ScratchDir`](crate::scratch::ScratchDir) under `$TMPDIR`, builds `cowboy` +
//! `cowlib` + `ranch` there, compiles the `examples/hello_world` sources together with a
//! tiny harness, and serves the documented route (`{'_', [{"/", toppage_h, []}]}`) on
//! port 8080 — **the same `.beam`s and the same dispatch on both sides**, so the only
//! difference between the two rows is which runtime executes them.
//!
//! This module owns the `Workload` surface (probe, metric assembly, notes); the staging,
//! build/compile recipe, per-runtime argv, spawn/ready/drain/peak-RSS/stop lifecycle and
//! the port hygiene all live in `cowboy_serve`.
//!
//! The load is driven by the crate's own dependency-free HTTP/1.1 load generator
//! ([`crate::http_load`]) — no external `wrk`/`h2load`/`bombardier`. The row records
//! throughput (req/s), the p50/p99 latency distribution, the boot-to-ready time, the
//! server process's peak RSS, the build/compile times and the transport-error count; the
//! scratch tree (and the untouched Cowboy source) is left clean afterwards.
//!
//! Repo: `$BENCH_COWBOY_REPO`, else `/home/bill/Source/cowboy`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::metrics::{Category, Metric, Probe, Runtimes};
use crate::proc;
use crate::report::{Profile, WorkloadResult};
use crate::workload::{RunContext, Workload};
use crate::workloads::{cowboy_serve, resolve_repo, slice};

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/cowboy";
/// Concurrency (connections) and duration of the `--quick` load run.
const QUICK_CONNECTIONS: usize = 8;
const QUICK_DURATION: Duration = Duration::from_secs(2);
/// Concurrency (connections) and duration of the `--full` load run.
const FULL_CONNECTIONS: usize = 16;
const FULL_DURATION: Duration = Duration::from_secs(5);
/// Upper bound on stored latency samples (keeps the report bounded).
const MAX_LATENCY_SAMPLES: usize = 20_000;
/// How much of a crash report to quote in a note.
const CRASH_SAMPLE_CHARS: usize = 300;

/// The Cowboy HTTP workload.
pub struct Cowboy;

fn repo() -> PathBuf {
    resolve_repo("cowboy", DEFAULT_REPO)
}

impl Workload for Cowboy {
    fn id(&self) -> &'static str {
        "cowboy"
    }

    fn description(&self) -> &'static str {
        "Cowboy hello_world HTTP server (built from source) driven by the in-process \
         HTTP/1.1 load generator under both runtimes"
    }

    fn category(&self) -> Category {
        Category::Io
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.1", "A.6"]
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::Both
    }

    fn probe(&self) -> Probe {
        if !repo().is_dir() {
            return Probe::skipped("repo-missing");
        }
        if proc::which("make").is_none() {
            return Probe::skipped("make-absent");
        }
        // erlang.mk fetches cowlib/ranch with `git`, so the build needs it too.
        if proc::which("git").is_none() {
            return Probe::skipped("git-absent");
        }
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        let desc = self.description();
        let r = repo();
        if !r.is_dir() {
            return WorkloadResult::skipped(self.id(), kind, self.category(), desc, "repo-missing");
        }
        let Some(make) = proc::which("make") else {
            return WorkloadResult::skipped(self.id(), kind, self.category(), desc, "make-absent");
        };
        if proc::which("git").is_none() {
            return WorkloadResult::skipped(self.id(), kind, self.category(), desc, "git-absent");
        }
        let Some(erlc) = slice::resolve_erlc() else {
            return WorkloadResult::skipped(self.id(), kind, self.category(), desc, "erlc-absent");
        };
        // `run_one` already gates an unsupported runtime; an unresolved program is still
        // reported honestly rather than turning into a spawn failure.
        let Some(program) = ctx.runtime.program() else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                desc,
                "runtime-unsupported",
            );
        };

        // Stage → build → compile the harness. Every outcome keeps what was measured.
        let built = match cowboy_serve::stage_and_build(&r, &erlc, &make) {
            Ok(built) => built,
            Err(e) => {
                let mut result = if e.skipped {
                    WorkloadResult::skipped(self.id(), kind, self.category(), desc, e.reason)
                } else {
                    WorkloadResult::failed(self.id(), kind, self.category(), desc, e.reason)
                };
                if let Some(ms) = e.build_ms {
                    result.metrics.push(Metric::scalar("build_ms", "ms", ms));
                }
                if let Some(ms) = e.compile_ms {
                    result.metrics.push(Metric::scalar("compile_ms", "ms", ms));
                }
                result.notes = e.notes;
                return result;
            }
        };

        let (connections, duration) = match ctx.profile {
            Profile::Quick => (QUICK_CONNECTIONS, QUICK_DURATION),
            Profile::Full => (FULL_CONNECTIONS, FULL_DURATION),
        };
        let plan = cowboy_serve::LoadPlan {
            connections,
            duration,
            max_latency_samples: MAX_LATENCY_SAMPLES,
        };

        let (server_program, argv) =
            cowboy_serve::server_argv(kind, program, &built.pa, &built.harness_beam);
        let loadavg_before = cowboy_serve::loadavg();
        let served = cowboy_serve::serve(
            &server_program,
            &argv,
            plan,
            cowboy_serve::run_deadline_ms(kind),
        );

        let outcome = match served {
            Ok(outcome) => outcome,
            Err(e) => {
                let mut result =
                    WorkloadResult::failed(self.id(), kind, self.category(), desc, e.reason());
                result
                    .metrics
                    .push(Metric::scalar("build_ms", "ms", built.build_ms));
                result
                    .metrics
                    .push(Metric::scalar("compile_ms", "ms", built.compile_ms));
                result.notes = built.notes;
                push_host_notes(&mut result, loadavg_before.as_deref());
                return result;
            }
        };

        if outcome.load.requests == 0 {
            let mut result = WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                desc,
                "load-no-responses",
            );
            result
                .metrics
                .push(Metric::scalar("build_ms", "ms", built.build_ms));
            result
                .metrics
                .push(Metric::scalar("compile_ms", "ms", built.compile_ms));
            result
                .metrics
                .push(Metric::scalar("errors", "count", outcome.load.errors as f64));
            result.notes = built.notes;
            result.notes.push(format!(
                "0 responses in {:.0} ms of load ({} transport error(s))",
                outcome.load.wall_ms, outcome.load.errors
            ));
            push_run_notes(
                &mut result,
                &server_program,
                ctx,
                connections,
                &outcome,
                loadavg_before.as_deref(),
            );
            result.peak_memory_bytes = outcome.peak_memory_bytes;
            return result;
        }

        let seconds = (outcome.load.wall_ms / 1000.0).max(f64::MIN_POSITIVE);
        let requests_per_sec = outcome.load.requests as f64 / seconds;

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), desc);
        result
            .metrics
            .push(Metric::scalar("build_ms", "ms", built.build_ms));
        result
            .metrics
            .push(Metric::scalar("compile_ms", "ms", built.compile_ms));
        result
            .metrics
            .push(Metric::scalar("ready_ms", "ms", outcome.ready_ms));
        result
            .metrics
            .push(Metric::scalar("requests", "count", outcome.load.requests as f64));
        result
            .metrics
            .push(Metric::scalar("errors", "count", outcome.load.errors as f64));
        result
            .metrics
            .push(Metric::scalar("requests_per_sec", "req/s", requests_per_sec));
        result.metrics.push(Metric::samples(
            "latency_ms",
            "ms",
            &outcome.load.latencies_ms,
        ));
        result.peak_memory_bytes = outcome.peak_memory_bytes;
        result.notes = built.notes;
        if outcome.load.latencies_ms.is_empty() {
            result
                .notes
                .push("no latency samples recorded".to_string());
        }
        push_run_notes(
            &mut result,
            &server_program,
            ctx,
            connections,
            &outcome,
            loadavg_before.as_deref(),
        );
        result
    }
}

/// Adds the server/dispatch/window/keep-alive/crash-report notes of a completed run.
fn push_run_notes(
    result: &mut WorkloadResult,
    server_program: &Path,
    ctx: &RunContext,
    connections: usize,
    outcome: &cowboy_serve::ServeOutcome,
    loadavg_before: Option<&str>,
) {
    let version = ctx
        .runtime
        .version()
        .unwrap_or_else(|| "unknown".to_string());
    result.notes.push(format!(
        "server: {} (runtime {} version {version}), {} keep-alive connection(s)",
        server_program.display(),
        ctx.runtime.kind.as_str(),
        connections
    ));
    result.notes.push(
        "dispatch: cowboy_router:compile([{'_', [{\"/\", toppage_h, []}]}]) — the hello_world \
         example's own handler serves every request"
            .to_string(),
    );
    result.notes.push(format!(
        "window: {:.0} ms of load inside a {:.0} ms driver wall; boot-to-ready {:.0} ms",
        outcome.load_duration.as_secs_f64() * 1000.0,
        outcome.load.wall_ms,
        outcome.ready_ms
    ));
    if let Some(note) = &outcome.clamp_note {
        result.notes.push(note.clone());
    }

    let crashes = cowboy_serve::count_crash_reports(&outcome.stdout);
    let bound = if outcome.transcript_truncated {
        " (a lower bound: the retained transcript was truncated)"
    } else {
        ""
    };
    let sample = cowboy_serve::crash_report_sample(&outcome.stdout, CRASH_SAMPLE_CHARS)
        .map(|s| format!("; first block: {s}"))
        .unwrap_or_default();
    result.notes.push(format!(
        "crash reports: {crashes} `=ERROR REPORT====` block(s) on the server's stdout{bound}{sample}"
    ));
    let stderr_tail = cowboy_serve::tail(&outcome.stderr, cowboy_serve::OUTPUT_TAIL_CHARS);
    if !stderr_tail.is_empty() {
        result
            .notes
            .push(format!("server stderr tail: {stderr_tail}"));
    }
    if outcome.transcript_truncated {
        result.notes.push(
            "the drained server output exceeded the driver's retained cap; only the first \
             slice was examined"
                .to_string(),
        );
    }
    if !outcome.port_released {
        result.notes.push(
            "the port was still not bindable 5 s after the server was killed — a following \
             rep could see `{error, eaddrinuse}`"
                .to_string(),
        );
    }
    if outcome.worker_panicked {
        result.notes.push(
            "a drain/poller thread panicked, so its captured output and the peak-RSS figure \
             are incomplete"
                .to_string(),
        );
    }
    result
        .notes
        .push(cowboy_serve::keepalive_close_note(outcome.load.requests, outcome.load.errors));
    push_host_notes(result, loadavg_before);
}

/// Adds the `/proc/loadavg` bookends: the host is shared, so read the ratio.
fn push_host_notes(result: &mut WorkloadResult, loadavg_before: Option<&str>) {
    let before = loadavg_before.unwrap_or("<unreadable>");
    let after = cowboy_serve::loadavg().unwrap_or_else(|| "<unreadable>".to_string());
    result.notes.push(format!(
        "host load before the run: {before}; after: {after} — the host is shared, so read the \
         cross-runtime ratio and the request/error counts, not the absolute wall time"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimeKind;

    #[test]
    fn metadata_is_stable_and_the_row_is_cross_runtime() {
        let w = Cowboy;
        assert_eq!(w.id(), "cowboy");
        assert_eq!(w.category(), Category::Io);
        assert_eq!(w.contracts(), &["A.1", "A.6"]);
        assert_eq!(w.runtimes(), Runtimes::Both);
        assert!(w.runtimes().allows(RuntimeKind::Sys));
        assert!(w.runtimes().allows(RuntimeKind::Rust));
    }

    #[test]
    fn probe_is_honest_about_a_missing_repo() {
        let p = Cowboy.probe();
        if p.reason() == Some("repo-missing") {
            assert!(!repo().is_dir());
        }
    }

    #[test]
    fn probe_requires_the_build_tools_the_staged_build_needs() {
        if !repo().is_dir() || proc::which("make").is_none() {
            return; // the earlier prerequisites already gate this row
        }
        if proc::which("git").is_some() {
            // No external load generator is a prerequisite any more: the crate drives
            // the load itself, so the probe is ready with just repo + make + git.
            assert!(Cowboy.probe().is_ready(), "probe: {:?}", Cowboy.probe());
        } else {
            assert_eq!(Cowboy.probe().reason(), Some("git-absent"));
        }
    }
}
