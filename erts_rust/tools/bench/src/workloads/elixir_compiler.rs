//! `elixir_compiler` — the Elixir-compiler throughput workload (`--full` only).
//!
//! [`Probe`]s the Elixir checkout (`$BENCH_ELIXIR_COMPILER_REPO`, else
//! `/home/bill/Source/elixir`) plus `make`/`erl`, then (in a `--full` run) `make`s the
//! checkout and drives `bin/elixirc` over a fixed, bounded corpus of `lib/elixir/lib`
//! `*.ex` files, recording files/sec, LOC/sec, wall time and peak memory.
//!
//! In the default `--quick` profile this is skipped with `quick-mode` — the checkout
//! build is a milestone-only, expensive operation.

use std::path::PathBuf;
use std::process::Command;

use crate::metrics::{Category, Metric, Probe, Runtimes};
use crate::proc;
use crate::report::{Profile, WorkloadResult};
use crate::workload::{RunContext, Workload};
use crate::workloads::{collect_files, count_source_lines, resolve_repo};

/// The documented default checkout path (overridable via `$BENCH_ELIXIR_COMPILER_REPO`).
const DEFAULT_REPO: &str = "/home/bill/Source/elixir";

/// Upper bound on the compiled corpus size, keeping the workload bounded.
const MAX_FILES: usize = 200;

/// The Elixir-compiler workload.
pub struct ElixirCompiler;

fn repo() -> PathBuf {
    resolve_repo("elixir_compiler", DEFAULT_REPO)
}

fn corpus() -> Vec<PathBuf> {
    collect_files(&repo().join("lib/elixir/lib"), "ex")
}

impl Workload for ElixirCompiler {
    fn id(&self) -> &'static str {
        "elixir_compiler"
    }

    fn description(&self) -> &'static str {
        "Elixir compiler (bin/elixirc) throughput over the checkout's stdlib"
    }

    fn category(&self) -> Category {
        Category::Compiler
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
        if proc::which("make").is_none() {
            return Probe::skipped("make-absent");
        }
        if proc::which("erl").is_none() {
            return Probe::skipped("erl-absent");
        }
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        if ctx.profile == Profile::Quick {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "quick-mode",
            );
        }
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
        let Some(make) = proc::which("make") else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "make-absent",
            );
        };
        let files: Vec<_> = corpus().into_iter().take(MAX_FILES).collect();
        if files.is_empty() {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "corpus-missing",
            );
        }

        // Build the checkout (writes build artifacts inside the repo; never commits).
        let mut build_cmd = Command::new(&make);
        build_cmd.current_dir(&r);
        let build = match proc::run(&mut build_cmd) {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("make-spawn-failed: {e}"),
                )
            }
        };
        if !build.success {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                format!("make-failed: {}", build.stderr.trim()),
            );
        }

        let elixirc = r.join("bin/elixirc");
        if !elixirc.is_file() {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "elixirc-missing-after-build",
            );
        }
        let out_beams = ctx.out_dir.join("elixirc_build");
        if let Err(e) = std::fs::create_dir_all(&out_beams) {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                format!("scratch-dir-failed: {e}"),
            );
        }
        for stale in collect_files(&out_beams, "beam") {
            let _ = std::fs::remove_file(stale);
        }

        let mut cmd = Command::new(&elixirc);
        cmd.current_dir(&r);
        cmd.arg("-o").arg(&out_beams).args(&files);

        let out = match proc::run(&mut cmd) {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("elixirc-spawn-failed: {e}"),
                )
            }
        };

        let produced = collect_files(&out_beams, "beam").len();
        let loc = count_source_lines(&files);
        let seconds = (out.wall_ms / 1000.0).max(f64::MIN_POSITIVE);

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result
            .metrics
            .push(Metric::scalar("build_ms", "ms", build.wall_ms));
        result.metrics.push(Metric::scalar("wall_ms", "ms", out.wall_ms));
        result
            .metrics
            .push(Metric::scalar("files_compiled", "files", produced as f64));
        result.metrics.push(Metric::scalar(
            "files_per_sec",
            "files/s",
            files.len() as f64 / seconds,
        ));
        result
            .metrics
            .push(Metric::scalar("loc_per_sec", "loc/s", loc as f64 / seconds));
        result.peak_memory_bytes = out.peak_memory_bytes;
        if produced < files.len() {
            result
                .notes
                .push(format!("{produced}/{} files produced .beam", files.len()));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = ElixirCompiler;
        assert_eq!(w.id(), "elixir_compiler");
        assert_eq!(w.category(), Category::Compiler);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
    }

    #[test]
    fn quick_mode_skips_before_touching_the_repo() {
        let rt = crate::runtime::Runtime {
            kind: crate::runtime::RuntimeKind::Sys,
            program: Some(PathBuf::from("/bin/true")),
        };
        let ctx = RunContext::new(&rt, Profile::Quick, std::path::Path::new("/tmp"));
        let res = ElixirCompiler.run(&ctx);
        assert_eq!(res.status, crate::metrics::Status::Skipped);
        assert_eq!(res.reason.as_deref(), Some("quick-mode"));
    }

    #[test]
    fn probe_is_honest_about_missing_repo() {
        let p = ElixirCompiler.probe();
        // Either the checkout is absent (repo-missing) or present and ready.
        if p.reason() == Some("repo-missing") {
            assert!(!repo().is_dir());
        }
    }
}
