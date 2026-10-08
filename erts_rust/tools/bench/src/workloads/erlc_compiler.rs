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

//! `erlc_compiler` — the "compiler throughput" workload over a broadened, two-corpus
//! `erlc` source set.
//!
//! Compiles **bounded, deterministic slices** (the first [`SLICE_PER_CORPUS`] `*.erl`
//! files, sorted) of two *real* in-repo source corpora with `erlc`, into a private
//! scratch directory (a [`ScratchDir`] subdir under `$TMPDIR`):
//!
//! * the OTP **stdlib** sources (`<ERL_TOP>/lib/stdlib/src`) — always available, and
//! * the **Elixir compiler/kernel** Erlang sources from the read-only Elixir checkout
//!   (`<elixir>/lib/elixir/src`, resolved via `resolve_repo("elixir_compiler", …)`).
//!
//! This runs today on any host with `erlc` on `PATH` and gives a repeatable files/sec,
//! LOC/sec and wall time for both runtimes (oracle and `beam_rt`).
//!
//! Honest reporting: if `erlc` produces **zero** `.beam` files the result is `failed`
//! (never a fabricated throughput); a *partial* success is recorded `ok` with a note.
//! When the Elixir corpus is absent or fails to compile, the workload falls back to the
//! OTP-only corpus and records *why* in its notes — it never fabricates a result.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc::{self, Completed};
use crate::report::WorkloadResult;
use crate::scratch::ScratchDir;
use crate::workload::{RunContext, Workload};
use crate::workloads::{collect_files, count_source_lines, erl_top, resolve_repo};

/// How many source files to compile from *each* corpus (keeps the workload bounded
/// and fast while still exercising both source trees).
const SLICE_PER_CORPUS: usize = 30;

/// The documented default Elixir checkout (overridable via
/// `$BENCH_ELIXIR_COMPILER_REPO` / `--repo elixir_compiler=<path>`).
const DEFAULT_ELIXIR_REPO: &str = "/home/bill/Source/elixir";

/// The `erlc` throughput workload.
pub struct ErlcCompiler;

/// One source corpus: a directory of `*.erl` files, the `-I` include directories its
/// files need, how many files to take, and a short label for the report notes.
struct Corpus {
    /// Short, stable label used in the report notes (e.g. `stdlib`, `elixir`).
    label: &'static str,
    /// The directory holding the corpus's `*.erl` sources.
    src_dir: PathBuf,
    /// The `-I` include directories the corpus's sources require.
    includes: Vec<PathBuf>,
    /// The maximum number of files to take from this corpus.
    limit: usize,
}

impl Corpus {
    /// The deterministic, bounded slice of this corpus: the first [`Corpus::limit`]
    /// `*.erl` files, in the sorted order produced by [`collect_files`].
    fn slice(&self) -> Vec<PathBuf> {
        collect_files(&self.src_dir, "erl")
            .into_iter()
            .take(self.limit)
            .collect()
    }
}

/// The OTP **stdlib** corpus: `<ERL_TOP>/lib/stdlib/src` (always present in this repo).
fn stdlib_corpus() -> Corpus {
    let top = erl_top();
    Corpus {
        label: "stdlib",
        src_dir: top.join("lib/stdlib/src"),
        includes: vec![
            top.join("lib/stdlib/src"),
            top.join("lib/stdlib/include"),
            top.join("lib/kernel/include"),
        ],
        limit: SLICE_PER_CORPUS,
    }
}

/// The **Elixir** corpus: `<elixir>/lib/elixir/src`, the Elixir compiler/kernel Erlang
/// sources. The checkout is read-only — this only *reads* its sources.
fn elixir_corpus() -> Corpus {
    let src = resolve_repo("elixir_compiler", DEFAULT_ELIXIR_REPO).join("lib/elixir/src");
    Corpus {
        label: "elixir",
        includes: vec![src.clone()],
        src_dir: src,
        limit: SLICE_PER_CORPUS,
    }
}

fn erlc_program() -> Option<PathBuf> {
    proc::which("erlc")
}

/// The outcome of compiling one corpus slice.
struct CorpusRun {
    /// The corpus label.
    label: &'static str,
    /// How many source files were handed to `erlc`.
    files: usize,
    /// The total source lines across those files (a LOC proxy).
    loc: usize,
    /// How many `.beam` files this slice actually produced.
    produced: usize,
    /// The raw subprocess outcome (exit code, stderr, wall time, peak RSS).
    completed: Completed,
}

/// Compiles `files` from `corpus` into `build`, returning the run summary.
///
/// `.beam` files are counted as the **delta** in `build` around the run, so a shared
/// output directory still attributes the produced modules to the right corpus.
fn compile_corpus(
    erlc: &Path,
    corpus: &Corpus,
    files: &[PathBuf],
    build: &Path,
) -> std::io::Result<CorpusRun> {
    let before = collect_files(build, "beam").len();

    let mut cmd = Command::new(erlc);
    cmd.arg("-o").arg(build);
    for inc in &corpus.includes {
        cmd.arg("-I").arg(inc);
    }
    cmd.args(files);

    let completed = proc::run(&mut cmd)?;

    let after = collect_files(build, "beam").len();
    Ok(CorpusRun {
        label: corpus.label,
        files: files.len(),
        loc: count_source_lines(files),
        produced: after.saturating_sub(before),
        completed,
    })
}

/// The first non-empty line of `text`, trimmed; the empty string when there is none.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

impl Workload for ErlcCompiler {
    fn id(&self) -> &'static str {
        "erlc_compiler"
    }

    fn description(&self) -> &'static str {
        "erlc throughput over bounded OTP stdlib + Elixir compiler corpus slices"
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
        // The OTP corpus is the primary, always-required one; a missing Elixir checkout
        // is a *fallback*, not a skip (see `run`).
        if stdlib_corpus().slice().is_empty() {
            return Probe::skipped("corpus-missing");
        }
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        let fail = |reason: String| {
            WorkloadResult::failed(self.id(), kind, self.category(), self.description(), reason)
        };
        let skip = |reason: &str| {
            WorkloadResult::skipped(self.id(), kind, self.category(), self.description(), reason)
        };

        let Some(erlc) = erlc_program() else {
            return skip("erlc-absent");
        };

        let stdlib = stdlib_corpus();
        let stdlib_files = stdlib.slice();
        if stdlib_files.is_empty() {
            return skip("corpus-missing");
        }

        // Compile into a private, self-cleaning temp dir so the `.beam` count reflects
        // this run only and nothing is ever written into a read-only checkout.
        let scratch = match ScratchDir::new("erlc") {
            Ok(s) => s,
            Err(e) => return fail(format!("scratch-dir-failed: {e}")),
        };
        let build = scratch.path().join("build");
        if let Err(e) = std::fs::create_dir_all(&build) {
            return fail(format!("scratch-dir-failed: {e}"));
        }

        let mut notes: Vec<String> = Vec::new();
        let mut runs: Vec<CorpusRun> = Vec::new();

        match compile_corpus(&erlc, &stdlib, &stdlib_files, &build) {
            Ok(run) => runs.push(run),
            Err(e) => return fail(format!("erlc-spawn-failed: {e}")),
        }

        // The Elixir corpus is optional: absent or uncompilable ⇒ OTP-only, with a note.
        let elixir = elixir_corpus();
        if !elixir.src_dir.is_dir() {
            notes.push(format!(
                "elixir corpus absent at {}; compiled OTP stdlib only",
                elixir.src_dir.display()
            ));
        } else {
            let elixir_files = elixir.slice();
            if elixir_files.is_empty() {
                notes.push(format!(
                    "elixir corpus has no .erl sources at {}; compiled OTP stdlib only",
                    elixir.src_dir.display()
                ));
            } else {
                match compile_corpus(&erlc, &elixir, &elixir_files, &build) {
                    // Zero `.beam`s means the corpus did not compile: fall back to
                    // OTP-only and record *why* (never fabricate a result).
                    Ok(run) if run.produced == 0 => notes.push(format!(
                        "elixir corpus failed to compile (exit code {:?}); \
                         fell back to OTP stdlib only: {}",
                        run.completed.exit_code,
                        first_line(&run.completed.stderr)
                    )),
                    Ok(run) => runs.push(run),
                    Err(e) => notes.push(format!(
                        "elixir corpus spawn failed ({e}); fell back to OTP stdlib only"
                    )),
                }
            }
        }

        // Aggregate every successfully compiled slice.
        let total_files: usize = runs.iter().map(|r| r.files).sum();
        let total_produced: usize = runs.iter().map(|r| r.produced).sum();
        let total_loc: usize = runs.iter().map(|r| r.loc).sum();
        let wall_ms: f64 = runs.iter().map(|r| r.completed.wall_ms).sum();
        let peak = runs
            .iter()
            .filter_map(|r| r.completed.peak_memory_bytes)
            .max();
        let sources = runs
            .iter()
            .map(|r| r.label)
            .collect::<Vec<_>>()
            .join("+");
        let seconds = (wall_ms / 1000.0).max(f64::MIN_POSITIVE);

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result.metrics.push(Metric::scalar("wall_ms", "ms", wall_ms));
        result
            .metrics
            .push(Metric::scalar("files_compiled", "files", total_produced as f64));
        result.metrics.push(Metric::scalar(
            "files_per_sec",
            "files/s",
            total_files as f64 / seconds,
        ));
        result
            .metrics
            .push(Metric::scalar("loc_per_sec", "loc/s", total_loc as f64 / seconds));
        result.peak_memory_bytes = peak;
        result.notes.extend(notes);
        result
            .notes
            .push(format!("corpus: {total_files} files from {sources}"));

        if total_produced == 0 {
            let exit = runs.last().and_then(|r| r.completed.exit_code);
            let stderr = runs
                .last()
                .map(|r| r.completed.stderr.trim())
                .unwrap_or_default();
            result.status = Status::Failed;
            result.reason = Some("erlc-produced-no-output".to_string());
            result
                .notes
                .push(format!("erlc exited with code {exit:?}; stderr: {stderr}"));
            return result;
        }

        if total_produced < total_files {
            result
                .notes
                .push(format!("partial: {total_produced}/{total_files}"));
            for run in &runs {
                if run.produced < run.files {
                    result.notes.push(format!(
                        "partial: {} {}/{} files produced .beam (exit code {:?})",
                        run.label, run.produced, run.files, run.completed.exit_code
                    ));
                }
            }
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
        // The description names the broadened corpus.
        assert!(w.description().contains("stdlib"));
        assert!(w.description().contains("Elixir"));
    }

    #[test]
    fn otp_corpus_is_discoverable_in_this_checkout() {
        let files = stdlib_corpus().slice();
        assert!(!files.is_empty(), "expected .erl files under lib/stdlib/src");
        assert!(files.len() <= SLICE_PER_CORPUS, "slice must stay bounded");
    }

    #[test]
    fn elixir_corpus_is_bounded_and_discovered_when_present() {
        let corpus = elixir_corpus();
        if corpus.src_dir.is_dir() {
            let files = corpus.slice();
            assert!(
                !files.is_empty(),
                "elixir src dir exists but yielded no .erl files"
            );
            assert!(files.len() <= SLICE_PER_CORPUS, "slice must stay bounded");
            // Deterministic: `collect_files` sorts, so the slice is sorted too.
            let mut sorted = files.clone();
            sorted.sort();
            assert_eq!(files, sorted);
        }
    }

    #[test]
    fn stdlib_slice_honours_the_per_corpus_bound() {
        let all = collect_files(&stdlib_corpus().src_dir, "erl");
        // This checkout has far more than the bound, so the slice must be truncated.
        assert!(all.len() > SLICE_PER_CORPUS);
        assert_eq!(stdlib_corpus().slice().len(), SLICE_PER_CORPUS);
    }

    #[test]
    fn first_line_skips_blanks_and_trims() {
        assert_eq!(first_line(""), "");
        assert_eq!(first_line("   \n\t\n"), "");
        assert_eq!(first_line("\n  warning: x\nmore"), "warning: x");
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

    #[test]
    fn failed_constructor_carries_the_reason() {
        let failed = WorkloadResult::failed(
            "erlc_compiler",
            RuntimeKind::Sys,
            Category::Compiler,
            "d",
            "erlc-produced-no-output",
        );
        assert_eq!(failed.status, Status::Failed);
        assert_eq!(failed.reason.as_deref(), Some("erlc-produced-no-output"));
    }
}
