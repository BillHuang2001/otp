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

//! `elixir_compiler` — the Elixir-compiler throughput workload (runs in **both** profiles).
//!
//! Measures the **installed** Elixir compiler (`elixirc` on `PATH`) compiling a fixed,
//! bounded corpus of real `*.ex` files. The corpus is discovered in the Elixir checkout
//! (`$BENCH_ELIXIR_COMPILER_REPO`, else `/home/bill/Source/elixir`, else `--repo
//! elixir_compiler=<path>`) and is used *purely as a source of `.ex` files*: the checkout
//! is **unbuilt and read-only**, so it is never built, never `make`d, and never used as
//! the compiler. Every source file is copied into a private scratch directory under
//! `$TMPDIR` (see [`crate::scratch`]) before it is handed to `elixirc`, so nothing is
//! written into the checkout.
//!
//! # Compilation strategy
//!
//! A single **batch** invocation (`elixirc -o <out> <files…>`) is attempted first. If it
//! fails — or produces fewer beams than the number of inputs it was given — the workload
//! **falls back to per-file compilation** (one `elixirc -o <out> <file>` per file). That
//! fallback is what makes the measurement meaningful across the *version skew* between
//! the installed Elixir and the checkout: `elixirc` is all-or-nothing per invocation, so
//! a single skewed stdlib source would otherwise hide every good file behind zero beams.
//!
//! # Metrics
//!
//! `wall_ms` (total compile time across every invocation used), `files_attempted`,
//! `files_compiled` (`.beam` files actually produced), `files_per_sec`, `loc_per_sec`,
//! `compiler_startup_ms` (see `measure_startup`) and `peak_memory_bytes` (the largest
//! peak RSS observed across the `elixirc` invocations this workload ran).
//!
//! Honest reporting: zero beams ⇒ `failed(elixirc-produced-no-output)`; a partial result
//! is `ok` with an `N/M compiled` note — never a fabricated throughput.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc;
use crate::report::{Profile, WorkloadResult};
use crate::runtime::RuntimeKind;
use crate::scratch::ScratchDir;
use crate::workload::{RunContext, Workload};
use crate::workloads::{collect_files, count_source_lines, resolve_repo};

/// The stable workload id (the `--workloads` selector and the `--repo` key).
const ID: &str = "elixir_compiler";

/// The one-line description recorded in every result.
const DESCRIPTION: &str =
    "installed system Elixir compiler (elixirc) throughput over the checkout's stdlib corpus";

/// The benchmark category (compiler throughput).
const CATEGORY: Category = Category::Compiler;

/// The compatibility contracts this workload exercises.
const CONTRACTS: &[&str] = &["A.1", "A.9"];

/// The documented default checkout path (overridable via `$BENCH_ELIXIR_COMPILER_REPO`
/// or `--repo elixir_compiler=<path>`).
const DEFAULT_REPO: &str = "/home/bill/Source/elixir";

/// The stdlib source directory inside the checkout.
const CORPUS_SUBDIR: &str = "lib/elixir/lib";

/// Corpus bound for the fast default profile.
const QUICK_FILES: usize = 25;

/// Corpus bound for the milestone-only profile.
const FULL_FILES: usize = 200;

/// How much of a failed invocation's stderr is kept for the diagnostic note.
const DIAG_CHARS: usize = 400;

/// The source of the trivial one-module program used to measure `compiler_startup_ms`.
const STARTUP_SOURCE: &str = "\
defmodule BenchElixircStartupProbe do
  @moduledoc false
  def probe, do: :ok
end
";

/// The Elixir-compiler workload.
pub struct ElixirCompiler;

/// The system `elixirc` on `PATH`, if any.
fn elixirc_program() -> Option<PathBuf> {
    proc::which("elixirc")
}

/// The resolved Elixir checkout root.
fn repo() -> PathBuf {
    resolve_repo("elixir_compiler", DEFAULT_REPO)
}

/// The checkout's stdlib source directory.
fn corpus_root() -> PathBuf {
    repo().join(CORPUS_SUBDIR)
}

/// The sorted, recursively-collected `*.ex` corpus of the checkout.
fn corpus() -> Vec<PathBuf> {
    collect_files(&corpus_root(), "ex")
}

/// The number of corpus files a profile compiles.
fn corpus_bound(profile: Profile) -> usize {
    match profile {
        Profile::Quick => QUICK_FILES,
        Profile::Full => FULL_FILES,
    }
}

/// The larger of two optional byte counts (`None` only when both are `None`).
fn max_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// A short, single-line snippet of a failed invocation's stderr (`DIAG_CHARS` characters
/// at most, read from the end so the actual error survives).
fn diag_snippet(stderr: &str) -> String {
    let mut chars: Vec<char> = stderr.trim().chars().collect();
    if chars.len() > DIAG_CHARS {
        chars.drain(..chars.len() - DIAG_CHARS);
    }
    chars.into_iter().collect::<String>().replace('\n', " | ")
}

/// Copies `files` (all below `root`) into `dst`, **mirroring each file's path relative to
/// `root`**, and returns the staged paths (sorted).
///
/// The mirroring is deliberate: the corpus contains distinct sources that share a file
/// name (e.g. `typespec.ex` at the top level and under `code/`), so a flat copy would
/// silently overwrite — and therefore stop measuring — some of them.
///
/// Failures are surfaced unchanged; nothing is swallowed.
fn stage_corpus(root: &Path, files: &[PathBuf], dst: &Path) -> io::Result<Vec<PathBuf>> {
    let mut staged = Vec::with_capacity(files.len());
    for file in files {
        let rel = file.strip_prefix(root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not below {}", file.display(), root.display()),
            )
        })?;
        let target = dst.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(file, &target)?;
        staged.push(target);
    }
    staged.sort();
    Ok(staged)
}

/// Removes the `.beam` files directly produced by an earlier invocation, so the beam
/// count of the next phase reflects only that phase.
fn remove_beams(dir: &Path) -> Result<(), String> {
    for beam in collect_files(dir, "beam") {
        fs::remove_file(&beam).map_err(|e| format!("cleanup-failed: {} ({e})", beam.display()))?;
    }
    Ok(())
}

/// Measures `compiler_startup_ms`: the wall time of a **fresh** `elixirc` invocation
/// compiling a single, trivial one-line module.
///
/// This isolates the compiler's fixed per-invocation cost — VM boot plus Elixir kernel
/// load — which otherwise hides inside the corpus throughput numbers, because a
/// per-file fallback pays it once per file. The probe module is written into the private
/// scratch directory and compiled into its own output directory, so it can never inflate
/// `files_compiled`.
///
/// Returns `Err(reason)` (never a fabricated number) when the probe could not be run.
fn measure_startup(elixirc: &Path, scratch: &Path) -> Result<(f64, Option<u64>), String> {
    let src = scratch.join("startup_probe.ex");
    fs::write(&src, STARTUP_SOURCE).map_err(|e| format!("startup-probe-write-failed: {e}"))?;
    let out = scratch.join("startup_out");
    fs::create_dir_all(&out).map_err(|e| format!("startup-probe-out-failed: {e}"))?;

    let mut cmd = Command::new(elixirc);
    cmd.arg("-o").arg(&out).arg(&src);
    let done = proc::run(&mut cmd).map_err(|e| format!("startup-probe-spawn-failed: {e}"))?;
    if !done.success {
        return Err(format!(
            "startup-probe-failed: {}",
            diag_snippet(&done.stderr)
        ));
    }
    Ok((done.wall_ms, done.peak_memory_bytes))
}

/// The installed compiler's version banner (the first `Elixir …` line of `elixirc
/// --version`), recorded as a note so a report is self-describing about the version skew
/// between the compiler and the checkout's sources.
fn elixirc_version(elixirc: &Path) -> Result<String, String> {
    let mut cmd = Command::new(elixirc);
    cmd.arg("--version");
    let done = proc::run(&mut cmd).map_err(|e| format!("version-probe-spawn-failed: {e}"))?;
    done.stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("Elixir"))
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "version-banner-unrecognised: {}",
                diag_snippet(&done.stdout)
            )
        })
}

/// Compiles the profile-bounded slice of `corpus_root`'s `*.ex` files with `elixirc`.
///
/// Split out of [`Workload::run`] so tests can drive the whole measurement hermetically
/// against a tiny synthetic corpus, instead of the external (and read-only) checkout.
fn compile_corpus(
    elixirc: &Path,
    corpus_root: &Path,
    limit: usize,
    kind: RuntimeKind,
) -> WorkloadResult {
    let slice: Vec<PathBuf> = collect_files(corpus_root, "ex")
        .into_iter()
        .take(limit)
        .collect();
    if slice.is_empty() {
        return WorkloadResult::skipped(ID, kind, CATEGORY, DESCRIPTION, "corpus-missing");
    }

    let temp = match ScratchDir::new("elixirc") {
        Ok(dir) => dir,
        Err(e) => {
            return WorkloadResult::failed(
                ID,
                kind,
                CATEGORY,
                DESCRIPTION,
                format!("scratch-dir-failed: {e}"),
            )
        }
    };

    // Stage the sources into $TMPDIR (the checkout is read-only) before compiling.
    let src = temp.path().join("src");
    if let Err(e) = stage_corpus(corpus_root, &slice, &src) {
        return WorkloadResult::failed(
            ID,
            kind,
            CATEGORY,
            DESCRIPTION,
            format!("stage-failed: {e}"),
        );
    }
    let staged = collect_files(&src, "ex");
    if staged.is_empty() {
        return WorkloadResult::skipped(ID, kind, CATEGORY, DESCRIPTION, "corpus-missing");
    }

    let out = temp.path().join("out");
    if let Err(e) = fs::create_dir_all(&out) {
        return WorkloadResult::failed(
            ID,
            kind,
            CATEGORY,
            DESCRIPTION,
            format!("scratch-dir-failed: {e}"),
        );
    }

    let mut wall_ms = 0.0f64;
    let mut peak: Option<u64> = None;
    let mut diag = String::new();

    // Phase 1: one batch invocation over the whole slice.
    let mut batch = Command::new(elixirc);
    batch.arg("-o").arg(&out).args(&staged);
    let batch_out = match proc::run(&mut batch) {
        Ok(o) => o,
        Err(e) => {
            return WorkloadResult::failed(
                ID,
                kind,
                CATEGORY,
                DESCRIPTION,
                format!("elixirc-spawn-failed: {e}"),
            )
        }
    };
    wall_ms += batch_out.wall_ms;
    peak = max_opt(peak, batch_out.peak_memory_bytes);

    let mut produced = collect_files(&out, "beam").len();
    // A successful batch invocation compiled every input it was given.
    let mut succeeded = staged.len();
    let mut note = String::from("corpus compiled in a single batch elixirc invocation");

    if !batch_out.success || produced < staged.len() {
        // `elixirc` is all-or-nothing per invocation: one skewed or broken source hides
        // every good one. Recompile each file on its own so the good ones still count.
        note = "batch compile failed; per-file fallback (version skew)".to_string();
        diag = diag_snippet(&batch_out.stderr);
        if let Err(e) = remove_beams(&out) {
            return WorkloadResult::failed(ID, kind, CATEGORY, DESCRIPTION, e);
        }
        let mut ok_files = 0usize;
        for file in &staged {
            let mut one = Command::new(elixirc);
            one.arg("-o").arg(&out).arg(file);
            let done = match proc::run(&mut one) {
                Ok(o) => o,
                Err(e) => {
                    return WorkloadResult::failed(
                        ID,
                        kind,
                        CATEGORY,
                        DESCRIPTION,
                        format!("elixirc-spawn-failed: {e}"),
                    )
                }
            };
            wall_ms += done.wall_ms;
            peak = max_opt(peak, done.peak_memory_bytes);
            if done.success {
                ok_files += 1;
            } else if diag.is_empty() {
                diag = diag_snippet(&done.stderr);
            }
        }
        succeeded = ok_files;
        produced = collect_files(&out, "beam").len();
    }

    let seconds = (wall_ms / 1000.0).max(f64::MIN_POSITIVE);
    let loc = count_source_lines(&staged);

    let mut result = WorkloadResult::new(ID, kind, CATEGORY, DESCRIPTION);
    result
        .metrics
        .push(Metric::scalar("wall_ms", "ms", wall_ms));
    result.metrics.push(Metric::scalar(
        "files_attempted",
        "files",
        staged.len() as f64,
    ));
    result
        .metrics
        .push(Metric::scalar("files_compiled", "files", produced as f64));
    result.metrics.push(Metric::scalar(
        "files_per_sec",
        "files/s",
        produced as f64 / seconds,
    ));
    result
        .metrics
        .push(Metric::scalar("loc_per_sec", "loc/s", loc as f64 / seconds));
    result.peak_memory_bytes = peak;
    result.notes.push(note);

    match measure_startup(elixirc, temp.path()) {
        Ok((ms, probe_peak)) => {
            result
                .metrics
                .push(Metric::scalar("compiler_startup_ms", "ms", ms));
            result.peak_memory_bytes = max_opt(result.peak_memory_bytes, probe_peak);
        }
        Err(reason) => result.notes.push(reason),
    }

    match elixirc_version(elixirc) {
        Ok(version) => result.notes.push(format!("compiler: {version}")),
        Err(reason) => result.notes.push(reason),
    }
    result.notes.push(
        "corpus = the checkout's unbuilt, read-only .ex sources staged into $TMPDIR; \
         compiler = the installed system elixirc, so a version skew with the checkout is expected"
            .to_string(),
    );

    if produced == 0 {
        result.status = Status::Failed;
        result.reason = Some("elixirc-produced-no-output".to_string());
        result.notes.push(format!(
            "elixirc produced no .beam for {} staged file(s); stderr: {}",
            staged.len(),
            if diag.is_empty() { "(none)".to_string() } else { diag }
        ));
    } else {
        if succeeded < staged.len() {
            result.notes.push(format!(
                "{succeeded}/{} compiled; {produced} .beam produced",
                staged.len()
            ));
        }
        if produced > staged.len() {
            // Elixir sources may define several modules, so beams outnumber sources.
            result.notes.push(format!(
                "{produced} .beam file(s) produced from {} source file(s) \
                 (an Elixir source may define several modules)",
                staged.len()
            ));
        }
    }

    result
}

impl Workload for ElixirCompiler {
    fn id(&self) -> &'static str {
        ID
    }

    fn description(&self) -> &'static str {
        DESCRIPTION
    }

    fn category(&self) -> Category {
        CATEGORY
    }

    fn contracts(&self) -> &'static [&'static str] {
        CONTRACTS
    }

    fn runtimes(&self) -> Runtimes {
        Runtimes::SysOnly
    }

    fn probe(&self) -> Probe {
        if elixirc_program().is_none() {
            return Probe::skipped("elixirc-absent");
        }
        if corpus().is_empty() {
            return Probe::skipped("corpus-missing");
        }
        Probe::Ready
    }

    fn run(&self, ctx: &RunContext) -> WorkloadResult {
        let kind = ctx.runtime.kind;
        let Some(elixirc) = elixirc_program() else {
            return WorkloadResult::skipped(ID, kind, CATEGORY, DESCRIPTION, "elixirc-absent");
        };
        compile_corpus(
            &elixirc,
            &corpus_root(),
            corpus_bound(ctx.profile),
            kind,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scalar metric's value, panicking loudly when it is missing or not a scalar.
    fn metric_value(res: &WorkloadResult, name: &str) -> f64 {
        res.metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("metric `{name}` missing; notes: {:?}", res.notes))
            .value
            .unwrap_or_else(|| panic!("metric `{name}` is not a scalar"))
    }

    /// Writes a syntactically valid, uniquely-named one-module `.ex` source.
    fn write_module(dir: &Path, name: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(
            dir.join(format!("{name}.ex")),
            format!("defmodule {name} do\n  @moduledoc false\n  def probe(x), do: x\nend\n"),
        )
        .unwrap();
    }

    #[test]
    fn metadata_is_stable() {
        let w = ElixirCompiler;
        assert_eq!(w.id(), "elixir_compiler");
        assert_eq!(w.category(), Category::Compiler);
        assert!(matches!(w.runtimes(), Runtimes::SysOnly));
        assert_eq!(w.contracts(), &["A.1", "A.9"]);
        assert!(!w.description().is_empty());
    }

    #[test]
    fn quick_profile_compiles_less_than_full() {
        let quick = corpus_bound(Profile::Quick);
        let full = corpus_bound(Profile::Full);
        assert_eq!(quick, QUICK_FILES);
        assert_eq!(full, FULL_FILES);
        assert!(quick < full, "quick={quick} full={full}");
    }

    #[test]
    fn corpus_is_ex_only_and_sorted() {
        let files = corpus();
        assert!(
            files.windows(2).all(|w| w[0] <= w[1]),
            "corpus must be sorted for determinism"
        );
        assert!(
            files
                .iter()
                .all(|p| p.extension().and_then(|e| e.to_str()) == Some("ex")),
            "corpus must contain only .ex sources"
        );
    }

    #[test]
    fn quick_mode_is_no_longer_a_skip_reason() {
        // `quick` used to skip this workload (`quick-mode`); it is now a real, cheap run,
        // so the probe must never report that reason.
        assert_ne!(ElixirCompiler.probe().reason(), Some("quick-mode"));
    }

    #[test]
    fn probe_is_honest_about_prerequisites() {
        let p = ElixirCompiler.probe();
        if elixirc_program().is_none() {
            assert_eq!(p.reason(), Some("elixirc-absent"));
        } else if corpus().is_empty() {
            assert_eq!(p.reason(), Some("corpus-missing"));
        } else {
            assert!(p.is_ready(), "prerequisites present but probe said {}", p.label());
        }
    }

    #[test]
    fn diag_snippet_flattens_and_truncates() {
        assert_eq!(diag_snippet("  a\nb  "), "a | b");
        let long = "x".repeat(DIAG_CHARS * 10);
        assert_eq!(diag_snippet(&long).chars().count(), DIAG_CHARS);
    }

    #[test]
    fn max_opt_keeps_the_larger_value() {
        assert_eq!(max_opt(None, None), None);
        assert_eq!(max_opt(Some(1), None), Some(1));
        assert_eq!(max_opt(None, Some(2)), Some(2));
        assert_eq!(max_opt(Some(1), Some(2)), Some(2));
        assert_eq!(max_opt(Some(3), Some(2)), Some(3));
    }

    #[test]
    fn stage_corpus_preserves_distinct_files_with_the_same_name() {
        let temp = ScratchDir::new("elixirc-stage").unwrap();
        let root = temp.path().join("corpus");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("dup.ex"), "top\n").unwrap();
        fs::write(root.join("nested/dup.ex"), "nested\n").unwrap();
        let files = collect_files(&root, "ex");
        assert_eq!(files.len(), 2);

        let dst = temp.path().join("staged");
        let staged = stage_corpus(&root, &files, &dst).unwrap();

        assert_eq!(staged.len(), 2, "both same-named sources must survive staging");
        assert!(fs::read_to_string(dst.join("dup.ex"))
            .unwrap()
            .contains("top"));
        assert!(fs::read_to_string(dst.join("nested/dup.ex"))
            .unwrap()
            .contains("nested"));
    }

    #[test]
    fn stage_corpus_refuses_a_file_outside_the_root() {
        let temp = ScratchDir::new("elixirc-stage-bad").unwrap();
        let root = temp.path().join("corpus");
        fs::create_dir_all(&root).unwrap();
        let outside = vec![temp.path().join("elsewhere.ex")];
        let err = stage_corpus(&root, &outside, &temp.path().join("dst")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn compile_corpus_with_an_empty_corpus_is_skipped() {
        let temp = ScratchDir::new("elixirc-empty").unwrap();
        let root = temp.path().join("corpus");
        fs::create_dir_all(&root).unwrap();
        let res = compile_corpus(
            Path::new("/nonexistent/elixirc"),
            &root,
            10,
            RuntimeKind::Sys,
        );
        assert_eq!(res.status, Status::Skipped);
        assert_eq!(res.reason.as_deref(), Some("corpus-missing"));
    }

    #[test]
    fn compile_corpus_batches_a_synthetic_corpus_and_reports_real_metrics() {
        let Some(elixirc) = elixirc_program() else {
            return;
        };
        let temp = ScratchDir::new("elixirc-batch").unwrap();
        let root = temp.path().join("corpus");
        write_module(&root, "BenchBatchAlpha");
        write_module(&root, "BenchBatchBeta");
        write_module(&root, "BenchBatchGamma");

        // The bound is honoured: only the first two (sorted) files are staged.
        let res = compile_corpus(&elixirc, &root, 2, RuntimeKind::Sys);

        assert_eq!(res.status, Status::Ok, "notes: {:?}", res.notes);
        assert!(
            res.notes.iter().any(|n| n.contains("single batch")),
            "notes: {:?}",
            res.notes
        );
        assert_eq!(metric_value(&res, "files_attempted"), 2.0);
        assert_eq!(metric_value(&res, "files_compiled"), 2.0);
        assert!(metric_value(&res, "wall_ms") > 0.0);
        assert!(metric_value(&res, "files_per_sec") > 0.0);
        assert!(metric_value(&res, "loc_per_sec") > 0.0);
        assert!(metric_value(&res, "compiler_startup_ms") > 0.0);
        assert!(res.peak_memory_bytes.map(|b| b > 0).unwrap_or(true));
        for metric in &res.metrics {
            metric.validate().unwrap();
        }
    }

    #[test]
    fn compile_corpus_falls_back_per_file_when_a_source_is_broken() {
        let Some(elixirc) = elixirc_program() else {
            return;
        };
        let temp = ScratchDir::new("elixirc-fallback").unwrap();
        let root = temp.path().join("corpus");
        write_module(&root, "BenchFallbackGood");
        fs::write(root.join("zzz_broken.ex"), "defmodule 42 do\n").unwrap();

        let res = compile_corpus(&elixirc, &root, 10, RuntimeKind::Sys);

        assert_eq!(res.status, Status::Ok, "notes: {:?}", res.notes);
        assert!(
            res.notes
                .iter()
                .any(|n| n.contains("per-file fallback")),
            "notes: {:?}",
            res.notes
        );
        assert!(res.notes.iter().any(|n| n.contains("1/2 compiled")));
        assert_eq!(metric_value(&res, "files_attempted"), 2.0);
        assert_eq!(metric_value(&res, "files_compiled"), 1.0);
        for metric in &res.metrics {
            metric.validate().unwrap();
        }
    }

    #[test]
    fn compile_corpus_fails_loudly_when_nothing_compiles() {
        let Some(elixirc) = elixirc_program() else {
            return;
        };
        let temp = ScratchDir::new("elixirc-broken").unwrap();
        let root = temp.path().join("corpus");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("broken.ex"), "defmodule 42 do\n").unwrap();

        let res = compile_corpus(&elixirc, &root, 10, RuntimeKind::Sys);

        assert_eq!(res.status, Status::Failed);
        assert_eq!(res.reason.as_deref(), Some("elixirc-produced-no-output"));
        assert_eq!(metric_value(&res, "files_compiled"), 0.0);
    }
}
