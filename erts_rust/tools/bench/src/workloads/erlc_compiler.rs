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
//! (never a fabricated throughput); a *partial* success is recorded `ok` with a note. The
//! `files_per_sec` / `loc_per_sec` rates are computed from the files/LOC `erlc` actually
//! **produced**, not from what was attempted, so a partial run cannot overstate them next
//! to the `files_compiled` metric. When the Elixir corpus is absent or fails to compile, the
//! workload falls back to the OTP-only corpus and records *why* in its notes — it never
//! fabricates a result.

use std::collections::BTreeSet;
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
    /// The total source lines across those files (a LOC proxy for what was attempted).
    loc: usize,
    /// How many `.beam` files this slice actually produced.
    produced: usize,
    /// The source lines of the files that actually produced a `.beam` (the *produced* LOC,
    /// as opposed to [`CorpusRun::loc`]'s attempted LOC) — the honest base for the rate.
    produced_loc: usize,
    /// The raw subprocess outcome (exit code, stderr, wall time, peak RSS).
    completed: Completed,
}

/// The subset of `files` that produced a `.beam` in `build`, matched by file stem.
///
/// `erlc` emits `<stem>.beam` for each source that compiled, so matching the newly written
/// `.beam` files back to their sources identifies exactly which sources the run compiled —
/// without trusting the exit code or assuming every attempted file succeeded.
fn compiled_sources(files: &[PathBuf], build: &Path, new_beams: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|src| {
            src.file_stem().is_some_and(|stem| {
                new_beams.contains(&build.join(format!("{}.beam", stem.to_string_lossy())))
            })
        })
        .cloned()
        .collect()
}

/// Compiles `files` from `corpus` into `build`, returning the run summary.
///
/// `.beam` files are counted as the **delta** in `build` around the run, so a shared
/// output directory still attributes the produced modules to the right corpus. The produced
/// count (and the produced LOC) therefore drive the throughput rates — never the attempted
/// counts.
fn compile_corpus(
    erlc: &Path,
    corpus: &Corpus,
    files: &[PathBuf],
    build: &Path,
) -> std::io::Result<CorpusRun> {
    let before: BTreeSet<PathBuf> = collect_files(build, "beam").into_iter().collect();

    let mut cmd = Command::new(erlc);
    cmd.arg("-o").arg(build);
    for inc in &corpus.includes {
        cmd.arg("-I").arg(inc);
    }
    cmd.args(files);

    let completed = proc::run(&mut cmd)?;

    let after: BTreeSet<PathBuf> = collect_files(build, "beam").into_iter().collect();
    let new_beams: BTreeSet<PathBuf> = after.difference(&before).cloned().collect();
    let compiled = compiled_sources(files, build, &new_beams);
    Ok(CorpusRun {
        label: corpus.label,
        files: files.len(),
        loc: count_source_lines(files),
        produced: new_beams.len(),
        produced_loc: count_source_lines(&compiled),
        completed,
    })
}

/// The throughput rates, computed from the **produced** file/LOC counts — so a partial
/// `erlc` run cannot overstate throughput next to the `files_compiled` metric. The wall time
/// is floored at a positive epsilon so a zero/negative duration yields a large finite rate,
/// never an infinity.
fn rates_per_second(produced: usize, produced_loc: usize, wall_ms: f64) -> (f64, f64) {
    // A real `erlc` run always takes milliseconds, so this floor only guards the degenerate
    // zero/negative duration: it keeps the rate finite instead of overflowing to infinity.
    let seconds = if wall_ms > 0.0 { wall_ms / 1000.0 } else { 1e-9 };
    (produced as f64 / seconds, produced_loc as f64 / seconds)
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
        let total_produced_loc: usize = runs.iter().map(|r| r.produced_loc).sum();
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

        // The rates are computed from what `erlc` actually **produced**, so a partial run
        // cannot overstate throughput next to the `files_compiled` metric.
        let (files_per_sec, loc_per_sec) =
            rates_per_second(total_produced, total_produced_loc, wall_ms);

        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result.metrics.push(Metric::scalar("wall_ms", "ms", wall_ms));
        result
            .metrics
            .push(Metric::scalar("files_compiled", "files", total_produced as f64));
        result
            .metrics
            .push(Metric::scalar("files_per_sec", "files/s", files_per_sec));
        result
            .metrics
            .push(Metric::scalar("loc_per_sec", "loc/s", loc_per_sec));
        result.peak_memory_bytes = peak;
        result.notes.extend(notes);
        result.notes.push(format!(
            "corpus: {total_files} files ({total_loc} LOC) attempted from {sources}; \
             {total_produced} produced .beam ({total_produced_loc} LOC)"
        ));

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

    #[test]
    fn rates_are_computed_from_the_produced_counts_not_the_attempted_ones() {
        // 3 produced files and 120 produced LOC over exactly one second.
        let (files_per_sec, loc_per_sec) = rates_per_second(3, 120, 1000.0);
        assert!((files_per_sec - 3.0).abs() < 1e-9, "{files_per_sec}");
        assert!((loc_per_sec - 120.0).abs() < 1e-9, "{loc_per_sec}");
        // The attempted-count rate (30 files / 1 s) must be strictly larger, i.e. the
        // produced-based rate can never overstate a partial run.
        assert!(files_per_sec < 30.0);
    }

    #[test]
    fn rates_stay_finite_for_a_zero_or_negative_wall_time() {
        for wall_ms in [0.0, -5.0] {
            let (files_per_sec, loc_per_sec) = rates_per_second(5, 50, wall_ms);
            assert!(files_per_sec.is_finite() && files_per_sec > 0.0, "{files_per_sec}");
            assert!(loc_per_sec.is_finite() && loc_per_sec > 0.0, "{loc_per_sec}");
        }
        // Zero produced ⇒ zero rate (no divide-by-zero, no invented number).
        assert_eq!(rates_per_second(0, 0, 1000.0), (0.0, 0.0));
    }

    #[test]
    fn compiled_sources_matches_produced_beams_by_file_stem() {
        let base = std::env::temp_dir().join(format!("bench-erlc-produced-{}", std::process::id()));
        let build = base.join("build");
        std::fs::create_dir_all(&build).unwrap();
        let a = base.join("a.erl");
        let b = base.join("b.erl");
        let c = base.join("c.erl");
        for f in [&a, &b, &c] {
            std::fs::write(f, "line1\nline2\n").unwrap();
        }
        // Only `a` and `b` compiled (produced a `.beam`); `c` did not.
        let new_beams: BTreeSet<PathBuf> = [build.join("a.beam"), build.join("b.beam")]
            .into_iter()
            .collect();
        let compiled = compiled_sources(&[a.clone(), b.clone(), c.clone()], &build, &new_beams);
        assert_eq!(compiled, vec![a, b]);
        // Its produced LOC is the two compiled files' lines, not all three.
        assert_eq!(count_source_lines(&compiled), 4);
        let _ = std::fs::remove_dir_all(&base);
    }
}
