//! `emqx` — the messaging/copy-bound workload (the EMQX broker build).
//!
//! Builds the EMQX checkout with `rebar3 compile` and records wall time (and peak
//! memory when measurable). Repo: `$BENCH_EMQX_REPO`, else `/home/bill/Source/emqx`.
//! `rebar3` is a hard prerequisite — absent ⇒ `skipped` (`rebar3-absent`).

use std::path::PathBuf;
use std::process::Command;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc;
use crate::report::WorkloadResult;
use crate::workload::{RunContext, Workload};
use crate::workloads::resolve_repo;

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/emqx";

/// The EMQX workload.
pub struct Emqx;

fn repo() -> PathBuf {
    resolve_repo("emqx", DEFAULT_REPO)
}

impl Workload for Emqx {
    fn id(&self) -> &'static str {
        "emqx"
    }

    fn description(&self) -> &'static str {
        "EMQX broker build (rebar3 compile) — messaging/copy bound"
    }

    fn category(&self) -> Category {
        Category::Messaging
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.1", "A.9"]
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::SysOnly
    }

    fn probe(&self) -> Probe {
        if !repo().is_dir() {
            return Probe::skipped("repo-missing");
        }
        if proc::which("rebar3").is_none() {
            return Probe::skipped("rebar3-absent");
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
        let Some(rebar3) = proc::which("rebar3") else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "rebar3-absent",
            );
        };

        let mut cmd = Command::new(&rebar3);
        cmd.current_dir(&r).arg("compile");
        let out = match proc::run(&mut cmd) {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("rebar3-spawn-failed: {e}"),
                )
            }
        };

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result
            .metrics
            .push(Metric::scalar("wall_ms", "ms", out.wall_ms));
        result.peak_memory_bytes = out.peak_memory_bytes;
        if !out.success {
            result.status = Status::Failed;
            result.reason = Some("rebar3-compile-failed".to_string());
            result.notes.push(format!("stderr: {}", out.stderr.trim()));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = Emqx;
        assert_eq!(w.id(), "emqx");
        assert_eq!(w.category(), Category::Messaging);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
    }

    #[test]
    fn probe_reports_rebar3_absence() {
        let p = Emqx.probe();
        if repo().is_dir() && proc::which("rebar3").is_none() {
            assert_eq!(p.reason(), Some("rebar3-absent"));
        }
    }
}
