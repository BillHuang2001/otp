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

//! `baseline` — the always-on system-ERTS baseline.
//!
//! Measures three independent quantities against the selected runtime (treated as a
//! drop-in `erl`), each with its own metric and none conflated with another:
//!
//! * **startup latency** — `N` runs of `<erl> -noshell -eval 'halt().'` timed
//!   individually, summarized as `startup_ms` (`--quick` uses 10 runs, `--full` 50);
//! * **a fixed compute loop** — `~5` runs of a deterministic in-VM workload
//!   (`lists:sum(lists:seq(1, 1_000_000))`), summarized as `compute_loop_ms` with a
//!   derived `compute_ops_per_sec` scalar;
//! * **the VM-reported memory** — one `erlang:memory(total)` probe, recorded as the
//!   scalar metric `vm_memory_total_bytes` (its own metric, **never** conflated with
//!   the OS-level `peak_memory_bytes` field, which is the peak RSS of the compute runs).
//!
//! # Capability gaps degrade, they never abort
//!
//! The Rust runtime does not implement the `erlang:memory/0,1` BIF yet, so the memory
//! probe fails there. That failure is recorded as the explicit note
//! `vm_memory_total_bytes: not-run (unsupported: erlang:memory/1)` and **no** metric —
//! the startup summary, the compute loop and every other metric are still produced, so
//! a run always yields numbers or an explicit reason. A runtime that answers the probe
//! records the bytes; any *other* probe failure is surfaced verbatim as `failed: …`.
//! Nothing is ever fabricated, and a probe failure never aborts the workload.

use std::process::Command;

use crate::metrics::{Category, Metric, Probe, Runtimes};
use crate::proc::{self, Completed};
use crate::report::WorkloadResult;
use crate::workload::{RunContext, Workload};

/// Number of additions in the fixed compute loop.
const COMPUTE_OPS: f64 = 1_000_000.0;

/// The deterministic compute expression. It deliberately has **no** VM-memory call so it
/// runs on every runtime; it prints the single integer sum.
const COMPUTE_EVAL: &str =
    r#"S = lists:sum(lists:seq(1,1000000)), io:format("~p~n",[S]), halt()."#;

/// The VM-memory probe: prints the single integer `erlang:memory(total)` in bytes.
/// `erlang:memory/0,1` is not implemented by the Rust runtime yet, so this is the probe
/// that reports a capability gap rather than a number there.
const MEMORY_EVAL: &str = r#"M = erlang:memory(total), io:format("~p~n",[M]), halt()."#;

/// The BIF the memory probe depends on, named in the capability-gap note.
const MEMORY_BIF: &str = "erlang:memory/1";

/// The startup-only expression.
const STARTUP_EVAL: &str = "halt().";

/// The system-ERTS baseline workload.
pub struct Baseline;

impl Workload for Baseline {
    fn id(&self) -> &'static str {
        "baseline"
    }

    fn description(&self) -> &'static str {
        "system-ERTS startup latency + fixed compute loop"
    }

    fn category(&self) -> Category {
        Category::Startup
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.6", "A.7"]
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::Both
    }

    fn probe(&self) -> Probe {
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        let Some(program) = ctx.runtime.program() else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "runtime-unsupported",
            );
        };

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());

        // --- startup latency ---------------------------------------------------
        let runs = ctx.profile.startup_runs();
        let mut startup_ms = Vec::with_capacity(runs);
        for _ in 0..runs {
            match proc::run(Command::new(program).args(["-noshell", "-eval", STARTUP_EVAL])) {
                Ok(out) if out.success => startup_ms.push(out.wall_ms),
                Ok(out) => result.notes.push(format!(
                    "startup run exited with code {:?}",
                    out.exit_code
                )),
                Err(e) => result.notes.push(format!("startup run failed to spawn: {e}")),
            }
        }
        if startup_ms.is_empty() {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "startup-spawn-failed",
            );
        }
        result
            .metrics
            .push(Metric::samples("startup_ms", "ms", &startup_ms));

        // --- fixed compute loop ------------------------------------------------
        let compute_runs = ctx.profile.compute_runs();
        let mut compute_ms = Vec::with_capacity(compute_runs);
        let mut peak: Option<u64> = None;
        let mut last_compute_failure: Option<String> = None;
        for _ in 0..compute_runs {
            match proc::run(Command::new(program).args(["-noshell", "-eval", COMPUTE_EVAL])) {
                Ok(out) if out.success => {
                    compute_ms.push(out.wall_ms);
                    peak = max_peak(peak, out.peak_memory_bytes);
                }
                Ok(out) => {
                    last_compute_failure = Some(format!(
                        "exited with code {:?}: {}",
                        out.exit_code,
                        out.stderr.trim()
                    ))
                }
                Err(e) => last_compute_failure = Some(format!("failed to spawn: {e}")),
            }
        }
        if !compute_ms.is_empty() {
            result
                .metrics
                .push(Metric::samples("compute_loop_ms", "ms", &compute_ms));
            let mean_ms = crate::stats::mean(&compute_ms);
            if mean_ms > 0.0 {
                result.metrics.push(Metric::scalar(
                    "compute_ops_per_sec",
                    "ops/s",
                    COMPUTE_OPS / (mean_ms / 1000.0),
                ));
            }
        } else if let Some(why) = last_compute_failure {
            // Degrade gracefully: the whole workload is not aborted for a compute-loop
            // failure (the startup summary is already recorded); the reason is explicit.
            result
                .notes
                .push(format!("compute_loop: not-run ({why})"));
        }
        result.peak_memory_bytes = peak;

        // --- VM-reported memory (its own probe; capability-gapped on Rust today) ---
        match proc::run(Command::new(program).args(["-noshell", "-eval", MEMORY_EVAL])) {
            Ok(out) if out.success => match parse_leading_u64(&out.stdout) {
                Some(bytes) => result.metrics.push(Metric::scalar(
                    "vm_memory_total_bytes",
                    "bytes",
                    bytes as f64,
                )),
                None => result.notes.push(format!(
                    "vm_memory_total_bytes: unparseable stdout `{}`",
                    out.stdout_trim()
                )),
            },
            Ok(out) if is_capability_gap(&out) => result.notes.push(format!(
                "vm_memory_total_bytes: not-run (unsupported: {MEMORY_BIF})"
            )),
            Ok(out) => result.notes.push(format!(
                "vm_memory_total_bytes: failed: exited {:?}: {}",
                out.exit_code,
                out.stderr.trim()
            )),
            Err(e) => result.notes.push(format!(
                "vm_memory_total_bytes: probe failed to spawn: {e}"
            )),
        }

        result
    }
}

/// Parses the first whitespace-separated unsigned integer of `stdout` (the compute sum
/// and the memory byte count are each printed as a single integer).
fn parse_leading_u64(stdout: &str) -> Option<u64> {
    stdout.split_whitespace().next()?.parse().ok()
}

/// Whether a failed memory probe is a documented **capability gap** rather than a genuine
/// runtime error.
///
/// The Rust runtime reports the missing `erlang:memory/0,1` BIF as a bare `beam_rt: error
/// undef` (exit `1`) today, so the gap is recognised from the runtime's own `undef` /
/// `unsupported` diagnostic as well as from the exit-`3` `unsupported` signal.
fn is_capability_gap(out: &Completed) -> bool {
    if out.exit_code == Some(3) {
        return true;
    }
    let stderr = out.stderr.trim();
    stderr.contains("undef") || stderr.contains("unsupported")
}

fn max_peak(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// The runtime kinds this workload can be scheduled under (documented for tests).
pub fn supported_kind(kind: crate::runtime::RuntimeKind) -> bool {
    Runtimes::Both.allows(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn completed(exit_code: i32, stderr: &str) -> Completed {
        Completed {
            exit_code: Some(exit_code),
            success: exit_code == 0,
            stdout: String::new(),
            stderr: stderr.to_string(),
            wall_ms: 0.0,
            peak_memory_bytes: None,
        }
    }

    #[test]
    fn parses_the_leading_integer_only() {
        assert_eq!(parse_leading_u64("500000500000\n"), Some(500_000_500_000));
        assert_eq!(parse_leading_u64("  34567890 extra"), Some(34_567_890));
        assert_eq!(parse_leading_u64("garbage"), None);
        assert_eq!(parse_leading_u64(""), None);
    }

    #[test]
    fn capability_gap_matches_only_the_documented_signals() {
        // The missing-BIF diagnostic (`undef`) and the exit-3 `unsupported` signal.
        assert!(is_capability_gap(&completed(1, "beam_rt: error undef")));
        assert!(is_capability_gap(&completed(
            3,
            "beam_rt: unsupported: erlang:memory/1 is a known BIF that is not implemented"
        )));
        // A genuine error is NOT a capability gap: it must be surfaced verbatim.
        assert!(!is_capability_gap(&completed(1, "beam_rt: error badarg")));
        assert!(!is_capability_gap(&completed(2, "usage error")));
        assert!(!is_capability_gap(&completed(0, "")));
    }

    #[test]
    fn max_peak_combines_optionals() {
        assert_eq!(max_peak(None, None), None);
        assert_eq!(max_peak(Some(1), None), Some(1));
        assert_eq!(max_peak(None, Some(2)), Some(2));
        assert_eq!(max_peak(Some(1), Some(2)), Some(2));
    }

    #[test]
    fn metadata_is_stable() {
        let w = Baseline;
        assert_eq!(w.id(), "baseline");
        assert_eq!(w.category(), Category::Startup);
        assert!(matches!(w.runtimes(), Runtimes::Both));
        assert!(w.probe().is_ready());
        assert_eq!(w.contracts(), &["A.6", "A.7"]);
    }

    #[test]
    fn unresolved_runtime_skips_with_runtime_unsupported() {
        let rt = crate::runtime::Runtime {
            kind: crate::runtime::RuntimeKind::Rust,
            program: None,
        };
        let ctx = RunContext::new(&rt, crate::report::Profile::Quick, std::path::Path::new("/tmp"));
        let res = Baseline.run(&ctx);
        assert_eq!(res.status, crate::metrics::Status::Skipped);
        assert_eq!(res.reason.as_deref(), Some("runtime-unsupported"));
    }
}
