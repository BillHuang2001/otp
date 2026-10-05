//! `emqx` — the messaging/copy-bound workload (the EMQX broker build).
//!
//! Stages the EMQX checkout into a private scratch copy under `$TMPDIR` (the source
//! repo is configured **read-only**) and runs `rebar3 compile` there, recording wall
//! time (and peak memory when measurable). Repo: `$BENCH_EMQX_REPO`, else
//! `/home/bill/Source/emqx`.
//!
//! `rebar3` is a hard prerequisite — absent ⇒ `skipped` (`rebar3-absent`). The opt-in
//! `nix shell` fallback (and its `$BENCH_TRY_NIX` switch) is shared with the other
//! tool-driven workloads; see [`crate::workloads::tool`]. It too is validated with a
//! bounded `--version` probe and, if it fails, the honest `rebar3-absent` skip is kept
//! (never a fabricated number).

use std::path::PathBuf;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc;
use crate::report::WorkloadResult;
use crate::scratch::{self, ScratchDir};
use crate::workload::{RunContext, Workload};
use crate::workloads::{resolve_repo, tool};

/// The documented default checkout path.
const DEFAULT_REPO: &str = "/home/bill/Source/emqx";

/// The external tool this workload drives.
const TOOL: &str = "rebar3";

/// The `nixpkgs` attribute that supplies [`TOOL`] for the opt-in `nix` fallback.
const TOOL_NIX_ATTR: &str = "rebar3";

/// The honest skip reason reported when [`TOOL`] cannot be resolved.
const TOOL_ABSENT: &str = "rebar3-absent";

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
        // build never writes build artifacts into the source repository.
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

        cmd.current_dir(&work).arg("compile");
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
        if repo().is_dir() && !tool::available(TOOL) {
            assert_eq!(p.reason(), Some(TOOL_ABSENT));
        }
    }
}
