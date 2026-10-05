//! `baseline` — the always-on system-ERTS baseline.
//!
//! Measures two things against the selected runtime (treated as a drop-in `erl`):
//!
//! * **startup latency** — `N` runs of `<erl> -noshell -eval 'halt().'` timed
//!   individually, summarized (`startup_ms`); `--quick` uses 10 runs, `--full` 50;
//! * **a fixed compute loop** — `~5` runs of a deterministic in-VM workload
//!   (`lists:sum(lists:seq(1, 1_000_000))`, plus `erlang:memory(total)`), summarized as
//!   `compute_loop_ms` with a derived `compute_ops_per_sec` scalar.
//!
//! Erlang-reported `erlang:memory(total)` is recorded as its own metric
//! (`vm_memory_total_bytes`) and is **never** conflated with the OS-level
//! `peak_memory_bytes` field.

use std::process::Command;

use crate::metrics::{Category, Probe, Runtimes};
use crate::proc;
use crate::report::WorkloadResult;
use crate::runtime::RuntimeKind;
use crate::workload::{RunContext, Workload};

/// Number of additions in the fixed compute loop.
const COMPUTE_OPS: f64 = 1_000_000.0;

/// The deterministic compute expression; prints `<sum> <memory_bytes>`.
const COMPUTE_EVAL: &str =
    r#"S = lists:sum(lists:seq(1,1000000)), M = erlang:memory(total), io:format("~p ~p~n",[S,M]), halt()."#;

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
            .push(crate::metrics::Metric::samples("startup_ms", "ms", &startup_ms));

        // --- fixed compute loop ------------------------------------------------
        let compute_runs = ctx.profile.compute_runs();
        let mut compute_ms = Vec::with_capacity(compute_runs);
        let mut vm_memory = Vec::with_capacity(compute_runs);
        let mut peak: Option<u64> = None;
        for _ in 0..compute_runs {
            match proc::run(Command::new(program).args(["-noshell", "-eval", COMPUTE_EVAL])) {
                Ok(out) if out.success => {
                    compute_ms.push(out.wall_ms);
                    if let Some(bytes) = parse_compute_memory(&out.stdout) {
                        vm_memory.push(bytes as f64);
                    }
                    peak = max_peak(peak, out.peak_memory_bytes);
                }
                Ok(out) => result.notes.push(format!(
                    "compute run exited with code {:?}: {}",
                    out.exit_code,
                    out.stderr.trim()
                )),
                Err(e) => result.notes.push(format!("compute run failed to spawn: {e}")),
            }
        }

        if !compute_ms.is_empty() {
            result
                .metrics
                .push(crate::metrics::Metric::samples("compute_loop_ms", "ms", &compute_ms));
            let mean_ms = crate::stats::mean(&compute_ms);
            if mean_ms > 0.0 {
                result.metrics.push(crate::metrics::Metric::scalar(
                    "compute_ops_per_sec",
                    "ops/s",
                    COMPUTE_OPS / (mean_ms / 1000.0),
                ));
            }
        }
        if !vm_memory.is_empty() {
            result.metrics.push(crate::metrics::Metric::scalar(
                "vm_memory_total_bytes",
                "bytes",
                crate::stats::mean(&vm_memory),
            ));
        }
        result.peak_memory_bytes = peak;
        result
    }
}

/// Parses the `<sum> <memory_bytes>` line the compute expression prints.
fn parse_compute_memory(stdout: &str) -> Option<u64> {
    let mut it = stdout.split_whitespace();
    let _sum: u64 = it.next()?.parse().ok()?;
    it.next()?.parse().ok()
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
pub fn supported_kind(kind: RuntimeKind) -> bool {
    Runtimes::Both.allows(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_compute_output() {
        assert_eq!(parse_compute_memory("500000500000 34567890\n"), Some(34_567_890));
        assert_eq!(parse_compute_memory("  1   2  "), Some(2));
        assert_eq!(parse_compute_memory("garbage"), None);
        assert_eq!(parse_compute_memory(""), None);
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
            kind: RuntimeKind::Rust,
            program: None,
        };
        let ctx = RunContext::new(&rt, crate::report::Profile::Quick, std::path::Path::new("/tmp"));
        let res = Baseline.run(&ctx);
        assert_eq!(res.status, crate::metrics::Status::Skipped);
        assert_eq!(res.reason.as_deref(), Some("runtime-unsupported"));
    }
}
