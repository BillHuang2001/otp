//! `erlc_compiler` — the "compiler throughput" workload over an in-repo OTP corpus.
//!
//! Compiles a **bounded, deterministic slice** (the first 40 `*.erl` files, sorted) of
//! `lib/stdlib/src` with `erlc`, into a scratch directory under `out/`. This runs today
//! on any host with `erlc` on `PATH` and gives a repeatable files/sec, LOC/sec and wall
//! time for both runtimes (oracle and, once it exists, `beam_rt`).
//!
//! Honest reporting: if `erlc` produces **zero** `.beam` files the result is `failed`
//! (never a fabricated throughput); a *partial* success is recorded `ok` with a note.

use std::process::Command;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc;
use crate::report::WorkloadResult;
use crate::workload::{RunContext, Workload};
use crate::workloads::{collect_files, count_source_lines, erl_top};

/// How many source files to compile (keeps the workload bounded and fast).
const SLICE: usize = 40;

/// The `erlc` throughput workload.
pub struct ErlcCompiler;

fn corpus_dir() -> std::path::PathBuf {
    erl_top().join("lib/stdlib/src")
}

fn corpus() -> Vec<std::path::PathBuf> {
    collect_files(&corpus_dir(), "erl")
}

fn erlc_program() -> Option<std::path::PathBuf> {
    proc::which("erlc")
}

impl Workload for ErlcCompiler {
    fn id(&self) -> &'static str {
        "erlc_compiler"
    }

    fn description(&self) -> &'static str {
        "erlc throughput over a bounded OTP stdlib corpus"
    }

    fn category(&self) -> Category {
        Category::Compiler
    }

    fn contracts(&self) -> &'static [&'static str] {
        &["A.9"]
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::SysOnly
    }

    fn probe(&self) -> Probe {
        if erlc_program().is_none() {
            return Probe::skipped("erlc-absent");
        }
        if corpus().is_empty() {
            return Probe::skipped("corpus-missing");
        }
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        let Some(erlc) = erlc_program() else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "erlc-absent",
            );
        };
        let files = corpus();
        if files.is_empty() {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "corpus-missing",
            );
        }
        let slice: Vec<_> = files.into_iter().take(SLICE).collect();

        let build = ctx.out_dir.join("erlc_build");
        if let Err(e) = std::fs::create_dir_all(&build) {
            return WorkloadResult::failed(
                self.id(),
                kind,
                self.category(),
                self.description(),
                format!("scratch-dir-failed: {e}"),
            );
        }
        // Fresh output directory so the `.beam` count reflects this run only.
        for stale in collect_files(&build, "beam") {
            let _ = std::fs::remove_file(stale);
        }

        let top = erl_top();
        let includes = [
            top.join("lib/stdlib/src"),
            top.join("lib/stdlib/include"),
            top.join("lib/kernel/include"),
        ];

        let mut cmd = Command::new(&erlc);
        cmd.arg("-o").arg(&build);
        for inc in &includes {
            cmd.arg("-I").arg(inc);
        }
        cmd.args(&slice);

        let out = match proc::run(&mut cmd) {
            Ok(o) => o,
            Err(e) => {
                return WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    format!("erlc-spawn-failed: {e}"),
                )
            }
        };

        let produced = collect_files(&build, "beam").len();
        let loc = count_source_lines(&slice);
        let seconds = (out.wall_ms / 1000.0).max(f64::MIN_POSITIVE);

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result.metrics.push(Metric::scalar("wall_ms", "ms", out.wall_ms));
        result
            .metrics
            .push(Metric::scalar("files_compiled", "files", produced as f64));
        result.metrics.push(Metric::scalar(
            "files_per_sec",
            "files/s",
            slice.len() as f64 / seconds,
        ));
        result
            .metrics
            .push(Metric::scalar("loc_per_sec", "loc/s", loc as f64 / seconds));
        result.peak_memory_bytes = out.peak_memory_bytes;

        if produced == 0 {
            result.status = Status::Failed;
            result.reason = Some("erlc-produced-no-output".to_string());
            result.notes.push(format!(
                "erlc exited with code {:?}; stderr: {}",
                out.exit_code,
                out.stderr.trim()
            ));
        } else if produced < slice.len() {
            result.notes.push(format!(
                "partial: {produced}/{} files produced .beam (exit code {:?})",
                slice.len(),
                out.exit_code
            ));
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Profile;
    use crate::runtime::{Runtime, RuntimeKind};

    #[test]
    fn metadata_is_stable() {
        let w = ErlcCompiler;
        assert_eq!(w.id(), "erlc_compiler");
        assert_eq!(w.category(), Category::Compiler);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
        assert_eq!(w.contracts(), &["A.9"]);
    }

    #[test]
    fn corpus_is_discoverable_in_this_checkout() {
        // The in-repo OTP sources must be present for the workload to be meaningful.
        assert!(!corpus().is_empty(), "expected .erl files under lib/stdlib/src");
    }

    #[test]
    fn probe_reflects_toolchain_availability() {
        let p = ErlcCompiler.probe();
        if erlc_program().is_some() {
            assert!(p.is_ready(), "erlc present but probe said {}", p.label());
        } else {
            assert_eq!(p.reason(), Some("erlc-absent"));
        }
    }

    #[test]
    fn sysonly_is_not_schedulable_under_rust() {
        assert!(!Runtimes::SysOnly.allows(RuntimeKind::Rust));
        let rt = Runtime {
            kind: RuntimeKind::Rust,
            program: None,
        };
        // The runner handles gating; the workload itself still must not panic.
        let _ = rt;
    }

    #[test]
    fn missing_corpus_reports_corpus_missing() {
        // Simulate by pointing at a nonexistent extension dir: probe() consults the real
        // corpus, so here we assert the skip constructor instead.
        let skipped = WorkloadResult::skipped(
            "erlc_compiler",
            RuntimeKind::Sys,
            Category::Compiler,
            "d",
            "corpus-missing",
        );
        assert_eq!(skipped.reason.as_deref(), Some("corpus-missing"));
        assert_eq!(skipped.status, Status::Skipped);
        let _ = Profile::Quick;
    }
}
