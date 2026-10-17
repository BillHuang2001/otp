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

//! `foreign` — **real modules from the foreign target repos, run under *both* runtimes**.
//!
//! This is the workload that makes the Rust runtime measurable on *real foreign code*.
//! Where every other repo-backed workload (`cowboy`, `khepri`, `emqx`, `plb`) is
//! `Runtimes::SysOnly` — the Rust runtime cannot boot an OTP release, run `rebar3`, or
//! act as `erlc`/`elixirc` — this one picks **pure, self-contained modules from the
//! Cowboy and EMQX checkouts**, compiles them with the system `erlc` (exactly as each
//! repo's own build would), and executes a fixed loop over one of their exported
//! functions under **whatever runtime it is scheduled on**. Under `--runtime all` that
//! is a genuine apples-to-apples side-by-side of the *same* `.beam` running the *same*
//! code on real target-project sources.
//!
//! Repos are **read-only**: the `.erl` sources are only *read*; every `.beam` is
//! produced into a private, self-cleaning [`ScratchDir`] under `$TMPDIR`.
//!
//! # The measurement (paired, startup-cancelled, per call)
//!
//! A single call is far too short to time, and the two runtimes differ by orders of
//! magnitude, so a single fixed loop count cannot give both a meaningful marginal. Each
//! case therefore declares a **per-runtime loop count** `N` (calibrated so a single run
//! costs a few hundred ms on that runtime) and the workload runs the identical program
//! twice:
//!
//! * at `N` iterations ⇒ `case_<name>_wall_ms` (end-to-end process wall time),
//! * at `0` iterations ⇒ `case_<name>_startup_ms` (the same program's pure
//!   startup/boot cost — the Rust runtime loads its preloaded-module set and resolves its
//!   system code path with a native filesystem probe (`crates/beam_rt/src/codeserver.rs`;
//!   the system `erl` is only *run* as a last resort), so this is not zero),
//!
//! and derives the startup-cancelled marginal
//!
//! ```text
//! case_<name>_ns_per_call = median_i(wall_ms at N - wall_ms at 0) / N * 1e6
//! ```
//!
//! The two samples are taken **interleaved, one `(work, startup)` pair per round**: round
//! `i` runs the `N`-call sample and then the `0`-call sample *back to back*
//! ([`crate::marginal::sample_pairs`]), so both members of a pair see essentially the same
//! host conditions. The statistic is the **median of the paired differences** (one per
//! round), normalized by `N` — the throughput comparison above, with the fixed
//! process-startup cost removed from both sides.
//!
//! # Why paired — the old schedule must never come back
//!
//! This workload used to take **all `N` samples first, then all `0` samples** and divide
//! `mean(wall) - mean(startup)` by `N`. That holds only while the boot cost is constant over
//! the measurement; on this shared, loaded host it drifts by tens — at times hundreds — of
//! milliseconds *between* the two passes, so the mean-based difference **absorbs the drift
//! instead of cancelling it** (the only guard being that a negative marginal was refused)
//! and the published error is dominated by how busy the host was: an interleaved
//! re-measurement of the sibling `copy_bound` workload's `etf_roundtrip` case gave `2.42×`
//! where the sequential schedule reported `136×` — a ~56× inflation. Interleaving leaves
//! only a per-pair residual (at most one sampling interval) and the median keeps a single
//! contended round from moving the estimate, so a sequential all-`N`-then-all-`0` schedule
//! is not a stylistic alternative here, it *is* the regression (mechanism and its four
//! rules: `src/marginal.rs`).
//!
//! The figure is **withheld** — no metric, plus a `marginal below timer resolution` note
//! citing the evidence — whenever the median paired difference fails to clear
//! `NOISE_MULTIPLE × max(MIN_NOISE_MS, 1.4826 × MAD)` of those differences, so load jitter
//! is never published as a number; when it *is* published, its `paired median=… ms min=…
//! ms max=… ms noise=±… ms over k round(s)` detail is recorded as a note too.
//!
//! Every timed case also emits the deterministic, contention-immune companion
//! `case_<name>_calls = N` (mirroring `copy_bound`'s `case_<name>_ops`), so the loop count
//! behind the marginal is **always in the report**.
//!
//! # `ns_per_call` is `N`-dependent on the Rust runtime (read before comparing)
//!
//! **On `beam_rt` the per-call cost itself grows with `N`** (measured super-linear; see
//! `RESULTS.md` §3.4), so `case_<name>_ns_per_call` is only a clean marginal at the declared
//! `N` and is **only comparable across runs that used the SAME `N`**. The `sys` (system
//! ERTS) side is ~flat over the same range. The per-runtime `N` values are unchanged since
//! m10, so the m10↔m11 A/B was taken at **matched `N`** and is a valid comparison; a run
//! that changes a case's `n_rust` is not. Each Rust row therefore carries an explicit note
//! stating this, and every row carries the `N` it used in `case_<name>_calls`. The Rust
//! peak-RSS figure is likewise **not** a like-for-like memory comparison: it is
//! boot-dominated (the resident preloaded-module set dwarfs the loop's own footprint).
//!
//! # Honest reporting
//!
//! * Every runtime's stdout must equal `N * per_call` on the work pass and `0` on the
//!   startup pass (the driver accumulates a value linear in the loop count); a mismatch is
//!   recorded as a note, never silently ignored and never fatal.
//! * A Rust exit `3` (`beam_rt: unsupported: …`) records the capability gap and emits
//!   **no** timing for that case — never a fabricated number.
//! * `erlc` unavailable, or every case's sources missing from the (read-only) checkout ⇒ an
//!   honest `skipped(erlc-absent)` / `skipped(sources-missing)`; `erlc` *failing* on the
//!   sources we did stage ⇒ a real `failed(module-compile-failed)`, never a benign skip.
//! * The Rust row records, in a note, that its `ns_per_call` figures are `N`-dependent
//!   (only comparable at the same `N`) — the caveat is never hidden behind the number.

use std::path::{Path, PathBuf};use std::process::Command;

use crate::marginal::{resolve, sample_pairs, Marginal, PairedRuns, Pass, PassError};
use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc::{self, Completed};
use crate::report::{Profile, WorkloadResult};
use crate::runtime::RuntimeKind;
use crate::scratch::ScratchDir;
use crate::workload::{RunContext, Workload};
use crate::workloads::{resolve_repo, slice};

/// The documented default Cowboy checkout (overridable via `--repo cowboy=<path>` /
/// `$BENCH_COWBOY_REPO`).
const COWBOY_ROOT: &str = "/home/bill/Source/cowboy";
/// The documented default EMQX checkout (overridable via `--repo emqx=<path>` /
/// `$BENCH_EMQX_REPO`).
const EMQX_ROOT: &str = "/home/bill/Source/emqx";

/// The driver module compiled alongside the foreign sources; its functions are the
/// benchmark loops (one per case), each taking the loop count and returning a value
/// linear in it.
const DRIVER_MODULE: &str = "bench_foreign_driver";

/// The embedded driver: one `loop/1`-style function per case, each invoking a **real
/// foreign** function and accumulating a result that advances by the same amount every
/// iteration (so `result == N * per_call` on both runtimes).
const DRIVER_SOURCE: &str = "-module(bench_foreign_driver).\n\
     -export([lower/1, cap/1, enc/1, dec/1]).\n\
     lower(N) -> lower(N, 0).\n\
     lower(0, Acc) -> Acc;\n\
     lower(N, Acc) ->\n\
     \x20   lower(N - 1,\n\
     \x20         Acc + byte_size(cowboy_bstr:to_lower(<<\"The Quick Brown Fox 0123456789\">>))).\n\
     cap(N) -> cap(N, 0).\n\
     cap(0, Acc) -> Acc;\n\
     cap(N, Acc) ->\n\
     \x20   cap(N - 1,\n\
     \x20       Acc + byte_size(cowboy_bstr:capitalize_token(<<\"the-quick-brown-fox-0123456789\">>))).\n\
     enc(N) -> enc(N, 0).\n\
     enc(0, Acc) -> Acc;\n\
     enc(N, Acc) ->\n\
     \x20   enc(N - 1,\n\
     \x20       Acc + byte_size(emqx_base62:encode(<<1,2,3,4,5,6,7,8,9,10,11,12>>))).\n\
     dec(N) -> dec(N, 0).\n\
     dec(0, Acc) -> Acc;\n\
     dec(N, Acc) ->\n\
     \x20   dec(N - 1, Acc + byte_size(emqx_base62:decode(<<\"ABCDEFGHIJKLMNO\">>))).\n";

/// One foreign case: a real module from a target repo plus the driver function that
/// loops over it.
struct Case {
    /// Stable, short case name (used in the metric names, e.g. `case_<name>_wall_ms`).
    name: &'static str,
    /// The driver function to call (arity 1: the loop count).
    function: &'static str,
    /// The repo id used by `--repo <id>=<path>` / `$BENCH_<ID>_REPO`.
    repo: &'static str,
    /// The repo's documented default root.
    default_root: &'static str,
    /// The `*.erl` sources to stage and compile, relative to the repo root.
    sources: &'static [&'static str],
    /// The result increment per iteration (`result == N * per_call`).
    per_call: i64,
    /// Loop count calibrated for the Rust runtime (a few hundred ms per run).
    n_rust: i64,
    /// Loop count calibrated for the system-runtime oracle (a few tens of ms per run).
    n_sys: i64,
}

impl Case {
    /// The loop count this runtime should use.
    fn n_for(&self, kind: RuntimeKind) -> i64 {
        match kind {
            RuntimeKind::Rust => self.n_rust,
            RuntimeKind::Sys => self.n_sys,
        }
    }

    /// The driver + foreign sources, as a short human label for the report notes.
    fn sources_label(&self) -> String {
        let paths: Vec<String> = self
            .sources
            .iter()
            .map(|rel| format!("{}/{rel}", self.repo))
            .collect();
        paths.join(", ")
    }

    /// The absolute path of a staged source in the (read-only) checkout.
    fn checkout_path(&self, rel: &str) -> PathBuf {
        resolve_repo(self.repo, self.default_root).join(rel)
    }
}

/// The fixed case table.
///
/// `n_rust`/`n_sys` were calibrated on the reference host (see the crate CONTEXT) so that
/// a single run costs a few hundred ms on Rust and a **oracle marginal well above the
/// ~±30 ms run-to-run startup noise** on the system runtime (each oracle function costs
/// a few hundred ns, so the oracle loop count is in the 10^5–10^6 range); they are
/// constants, not measurements, and the reported metric is normalized per call. A loop
/// count whose marginal falls below the timer noise yields **no** per-call metric (an
/// honest note), never a fabricated one.
static CASES: &[Case] = &[
    Case {
        name: "bstr_to_lower",
        function: "lower",
        repo: "cowboy",
        default_root: COWBOY_ROOT,
        sources: &["src/cowboy_bstr.erl"],
        per_call: 30,
        n_rust: 20_000,
        n_sys: 1_500_000,
    },
    Case {
        name: "bstr_capitalize_token",
        function: "cap",
        repo: "cowboy",
        default_root: COWBOY_ROOT,
        sources: &["src/cowboy_bstr.erl"],
        per_call: 30,
        n_rust: 15_000,
        n_sys: 1_500_000,
    },
    Case {
        name: "emqx_base62_encode",
        function: "enc",
        repo: "emqx",
        default_root: EMQX_ROOT,
        sources: &["apps/emqx/src/emqx_base62.erl"],
        per_call: 16,
        n_rust: 25_000,
        n_sys: 3_000_000,
    },
    Case {
        name: "emqx_base62_decode",
        function: "dec",
        repo: "emqx",
        default_root: EMQX_ROOT,
        sources: &["apps/emqx/src/emqx_base62.erl"],
        per_call: 11,
        n_rust: 15_000,
        // The oracle's marginal is the smallest of the four, so it needs the largest loop.
        n_sys: 3_000_000,
    },
];

/// The real-foreign-modules workload.
pub struct Foreign;

/// The number of timed samples per case for a profile.
fn runs_for(profile: Profile) -> usize {
    match profile {
        Profile::Quick => 3,
        Profile::Full => 10,
    }
}

/// Whether every source of `case` is present in its (read-only) checkout.
fn sources_available(case: &Case) -> bool {
    case.sources.iter().all(|rel| case.checkout_path(rel).is_file())
}

/// Whether **any** case's sources are available.
fn any_sources_available() -> bool {
    CASES.iter().any(sources_available)
}

/// The per-side argv for one case at `n` iterations (everything after the program path).
fn argv_for(kind: RuntimeKind, dir: &Path, case: &Case, n: i64) -> Vec<String> {
    let beam = dir.join(format!("{DRIVER_MODULE}.beam"));
    let call = format!("{DRIVER_MODULE}:{}/1", case.function);
    match kind {
        RuntimeKind::Rust => vec![
            "-pa".to_string(),
            dir.display().to_string(),
            "run".to_string(),
            beam.display().to_string(),
            call,
            n.to_string(),
        ],
        RuntimeKind::Sys => vec![
            "-noshell".to_string(),
            "-pa".to_string(),
            dir.display().to_string(),
            "-eval".to_string(),
            format!("io:format(\"~w~n\",[{DRIVER_MODULE}:{}({n})]), halt(0).", case.function),
        ],
    }
}

/// Why [`stage`] could not produce a single runnable case.
///
/// The two arms are reported differently on purpose: a checkout that does not contain the
/// foreign `.erl` sources is a **missing prerequisite** (`skipped(sources-missing)`), while a
/// failure to compile sources we *did* stage — our own embedded driver, or a present foreign
/// source — is a **real failure** (`failed(module-compile-failed)`) that must never be
/// disguised as a benign skip.
#[derive(Debug, PartialEq, Eq)]
enum StageError {
    /// No case's `.erl` sources are present in the (read-only) checkout.
    NoSources,
    /// The driver plus the staged foreign sources could not be compiled by `erlc`.
    CompileFailed,
}

/// Maps a [`StageError`] to the honest `(status, reason)` for the `foreign` report row:
/// sources genuinely absent ⇒ `skipped(sources-missing)`; any compile failure of code we
/// staged ⇒ `failed(module-compile-failed)` — never a benign skip.
fn stage_outcome(err: StageError) -> (Status, &'static str) {
    match err {
        StageError::NoSources => (Status::Skipped, "sources-missing"),
        StageError::CompileFailed => (Status::Failed, "module-compile-failed"),
    }
}

/// Stages the driver plus every case's foreign sources into `dir` and compiles them with
/// `erlc`, returning the cases that are ready to run. A compile failure of anything we
/// staged is surfaced as [`StageError::CompileFailed`] (never a missing prerequisite), and
/// every reason is recorded in `notes` — never silently dropped.
fn stage(
    dir: &Path,
    erlc: &Path,
    notes: &mut Vec<String>,
) -> std::result::Result<Vec<&'static Case>, StageError> {
    let driver = dir.join(format!("{DRIVER_MODULE}.erl"));
    if let Err(e) = std::fs::write(&driver, DRIVER_SOURCE) {
        notes.push(format!("driver-write-failed: {e}"));
        return Err(StageError::CompileFailed);
    }

    let mut wanted: Vec<&'static Case> = Vec::new();
    let mut copied: Vec<String> = Vec::new();
    for case in CASES {
        if !sources_available(case) {
            let missing: Vec<String> = case
                .sources
                .iter()
                .filter(|rel| !case.checkout_path(rel).is_file())
                .map(|rel| case.checkout_path(rel).display().to_string())
                .collect();
            notes.push(format!(
                "case {}: source missing ({}); skipped",
                case.name,
                missing.join(", ")
            ));
            continue;
        }
        for rel in case.sources {
            let file_name = Path::new(rel)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| rel.to_string());
            if copied.contains(&file_name) {
                continue;
            }
            if let Err(e) = std::fs::copy(case.checkout_path(rel), dir.join(&file_name)) {
                notes.push(format!(
                    "case {}: copy of {} failed: {e}",
                    case.name,
                    case.checkout_path(rel).display()
                ));
                break;
            }
            copied.push(file_name);
        }
        wanted.push(case);
    }
    if wanted.is_empty() {
        return Err(StageError::NoSources);
    }

    // Compile the driver together with every staged foreign source in one `erlc` call.
    let mut sources: Vec<PathBuf> = Vec::new();
    for name in &copied {
        sources.push(dir.join(name));
    }
    sources.push(driver);
    let mut cmd = Command::new(erlc);
    cmd.arg("-o").arg(dir);
    cmd.args(&sources);
    match proc::run(&mut cmd) {
        Ok(out) if out.success => {}
        Ok(out) => {
            notes.push(format!(
                "erlc failed (exit {:?}): {}",
                out.exit_code,
                out.stderr.trim()
            ));
            return Err(StageError::CompileFailed);
        }
        Err(e) => {
            notes.push(format!("erlc spawn failed: {e}"));
            return Err(StageError::CompileFailed);
        }
    }

    if !dir.join(format!("{DRIVER_MODULE}.beam")).is_file() {
        notes.push("driver .beam missing after erlc".to_string());
        return Err(StageError::CompileFailed);
    }
    notes.push(format!(
        "staged {} foreign case(s) from {} distinct source file(s)",
        wanted.len(),
        copied.len()
    ));
    Ok(wanted)
}

/// The samples collected for one case under one runtime.
struct CaseOutcome {
    /// The loop count used.
    n: i64,
    /// Wall-clock samples (ms) of the `n`-iteration runs, in interleaved round order.
    wall: Vec<f64>,
    /// Wall-clock samples (ms) of the `0`-iteration (startup-only) runs, one per work sample.
    startup: Vec<f64>,
    /// Peak-RSS samples (bytes) of the `n`-iteration (work) runs only.
    rss: Vec<f64>,
    /// The stdout observed on the first successful `n`-iteration run.
    observed: Option<String>,
    /// Whether the runtime reported the case as `unsupported` (Rust exit `3`).
    unsupported: bool,
    /// Notes about failures/variations (never silent).
    notes: Vec<String>,
    /// The paired, drift-cancelling marginal the per-call figure is resolved from.
    marginal: Marginal,
}

impl CaseOutcome {
    /// Whether the case produced at least one usable timing.
    fn ok(&self) -> bool {
        !self.wall.is_empty()
    }
}

/// One completed sample of a case: the wall time, the poller's peak RSS, the trimmed stdout
/// and any note about this particular run.
struct CaseSample {
    /// End-to-end process wall time, in milliseconds.
    wall_ms: f64,
    /// The process's peak RSS, when the poller could read it.
    peak_memory_bytes: Option<u64>,
    /// The run's trimmed stdout.
    stdout: String,
    /// Per-sample notes (e.g. an unexpected stdout on this pass).
    notes: Vec<String>,
}

/// Runs **one** sample of `case` — the `Work` pass (`n` iterations) or the `Startup` pass
/// (`0` iterations, i.e. the same program's pure boot cost) — returning the sample, or a
/// [`PassError`] carrying the classified note when the run fails. The Rust runtime's `exit 3`
/// capability gap is flagged `unsupported` **only** while no work sample has completed: once
/// the case has produced a timing, a later failure is a real one.
fn sample_once(
    program: &Path, kind: RuntimeKind, dir: &Path, case: &Case, n: i64, pass: Pass,
    completed_work_samples: usize,
) -> std::result::Result<CaseSample, PassError> {
    let loops = n * pass.units();
    let mut cmd = Command::new(program);
    cmd.args(argv_for(kind, dir, case, loops));
    let completed = proc::run(&mut cmd).map_err(|e| PassError {
        note: format!("case {} ({}): spawn failed: {e}", case.name, kind.as_str()),
        unsupported: false,
    })?;
    if !completed.success {
        return Err(PassError {
            note: classify_failure(kind, case, &completed),
            unsupported: kind == RuntimeKind::Rust
                && completed.exit_code == Some(3)
                && completed_work_samples == 0,
        });
    }
    // The driver's result is linear in the loop count, so the expected stdout is exact on both
    // passes (`0` on the startup pass). A mismatch is a note, never a fatal error: the timing
    // is still real.
    let stdout = completed.stdout_trim().to_string();
    let expected = (loops * case.per_call).to_string();
    let mut notes = Vec::new();
    if stdout != expected {
        notes.push(format!(
            "case {}: unexpected stdout on the {} pass `{stdout}` (expected `{expected}`)",
            case.name,
            pass.as_str()
        ));
    }
    Ok(CaseSample {
        wall_ms: completed.wall_ms,
        peak_memory_bytes: completed.peak_memory_bytes,
        stdout,
        notes,
    })
}

/// Folds the interleaved runs into a [`CaseOutcome`]: the paired wall times, the work-pass peak
/// RSS, the first observed stdout, every per-sample note, the resolved marginal and the abort
/// (if any — the rounds completed before it are **kept** and still reported).
fn outcome_from(n: i64, sampled: PairedRuns<CaseSample>) -> CaseOutcome {
    let PairedRuns { pairs, aborted } = sampled;
    let (mut wall, mut startup) = (Vec::with_capacity(pairs.len()), Vec::with_capacity(pairs.len()));
    let (mut rss, mut observed, mut notes) = (Vec::new(), None, Vec::new());
    for (work, baseline) in pairs {
        wall.push(work.wall_ms);
        startup.push(baseline.wall_ms);
        if let Some(bytes) = work.peak_memory_bytes {
            rss.push(bytes as f64);
        }
        if observed.is_none() {
            observed = Some(work.stdout);
        }
        notes.extend(work.notes);
        notes.extend(baseline.notes);
    }
    let deltas: Vec<(f64, f64)> = wall.iter().copied().zip(startup.iter().copied()).collect();
    // The closure already decided whether an abort was a capability gap (`exit 3` with no
    // completed work sample), so that classification carries over untouched.
    let unsupported = matches!(aborted.as_ref(), Some(abort) if abort.unsupported);
    if let Some(abort) = aborted {
        notes.push(abort.note);
    }
    CaseOutcome {
        n,
        wall,
        startup,
        rss,
        observed,
        unsupported,
        notes,
        marginal: resolve(&deltas, n),
    }
}

/// Emits the per-call metric family for one case: the `case_<name>_ns_per_call` figure plus its
/// auditable `paired median=…` detail note when the marginal cleared the noise band, or the
/// withholding note (and **no** metric) when it did not.
fn emit_per_call(case: &Case, marginal: &Marginal, metrics: &mut Vec<Metric>, notes: &mut Vec<String>) {
    match marginal.unit_cost_ns() {
        Some(ns) => {
            let m = Metric::scalar(format!("case_{}_ns_per_call", case.name), "ns", ns);
            metrics.push(m);
            notes.push(format!("case {}: per-call marginal {}", case.name, marginal.detail()));
        }
        None => {
            let why = marginal.below_resolution_note();
            notes.push(format!("case {}: {why}; no per-call metric", case.name));
        }
    }
}

/// The deterministic, contention-immune companion metric for a timed case: the loop count `N`
/// the per-call marginal was derived from, emitted for **every** case that produced a timing
/// (mirroring `copy_bound`'s `case_<name>_ops`) so a figure can never be read without it.
fn calls_metric(case: &Case, n: i64) -> Metric {
    Metric::scalar(format!("case_{}_calls", case.name), "calls", n as f64)
}

/// The caveat recorded on a Rust `foreign` row: the per-call metric is `N`-dependent, so it is
/// only comparable across runs that used the same `N` (recorded as a report note so the
/// limitation is machine-visible rather than buried in prose).
const N_DEPENDENCE_NOTE: &str = "caveat: on the Rust runtime this per-call metric is \
     N-DEPENDENT (the measured per-call cost grows with N, super-linearly), so \
     case_<name>_ns_per_call is only comparable across runs that used the SAME N; the m10 \
     vs m11 (HEAD) A/B was run at matched N (the case table's n_rust is unchanged). See \
     RESULTS.md 3.4.";

/// Classifies a non-zero / killed run into a human note.
fn classify_failure(kind: RuntimeKind, case: &Case, completed: &Completed) -> String {
    let stderr = completed.stderr.trim();
    match (kind, completed.exit_code) {
        (RuntimeKind::Rust, Some(3)) => {
            let reason = stderr.strip_prefix("beam_rt: unsupported: ").unwrap_or(stderr);
            format!("case {}: unsupported: {reason}", case.name)
        }
        (_, Some(code)) => format!("case {} ({}): exited {code}: {stderr}", case.name, kind.as_str()),
        (_, None) => format!("case {} ({}): terminated by signal: {stderr}", case.name, kind.as_str()),
    }
}

/// Runs one case `runs` times **interleaved**: every round samples the `n`-iteration pass and
/// then the `0`-iteration pass back to back, so the boot cost drifts *inside* a pair rather than
/// between two passes (see the module docs and `crate::marginal`). A failing sample aborts the
/// measurement; the rounds already completed are kept and reported, and a case whose **first
/// work** pass failed is flagged `unsupported` instead of timed.
fn execute_case(program: &Path, kind: RuntimeKind, dir: &Path, case: &Case, runs: usize) -> CaseOutcome {
    let n = case.n_for(kind);
    let mut completed_work_samples = 0usize;
    let sampled = sample_pairs(runs, |pass| {
        let sample = sample_once(program, kind, dir, case, n, pass, completed_work_samples)?;
        if pass == Pass::Work {
            completed_work_samples += 1;
        }
        Ok(sample)
    });
    outcome_from(n, sampled)
}

impl Workload for Foreign {
    fn id(&self) -> &'static str {
        "foreign"
    }

    fn description(&self) -> &'static str {
        "real Cowboy/EMQX modules run under each runtime (per-call cost + wall latency)"
    }
    fn category(&self) -> Category {
        Category::Compute
    }

    fn contracts(&self) -> &'static [&'static str] {
        // Real `.beam` modules (A.2) executing real Erlang semantics (A.1).
        &["A.1", "A.2"]
    }

    fn runtimes(&self) -> Runtimes {
        // The whole point: both runtimes execute the same compiled modules.
        Runtimes::Both
    }

    fn probe(&self) -> Probe {
        if slice::resolve_erlc().is_none() {
            return Probe::skipped("erlc-absent");
        }
        if !any_sources_available() {
            return Probe::skipped("sources-missing");
        }
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
        let Some(erlc) = slice::resolve_erlc() else {
            return WorkloadResult::skipped(
                self.id(),
                kind,
                self.category(),
                self.description(),
                "erlc-absent",
            );
        };

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
        let dir = scratch.path();

        let mut staging_notes: Vec<String> = Vec::new();
        let ready = match stage(dir, &erlc, &mut staging_notes) {
            Ok(ready) => ready,
            Err(err) => {
                // A checkout that simply lacks the foreign sources is a *missing
                // prerequisite* (skip); a failure to compile the sources we did stage is a
                // real failure, never a benign skip.
                let (status, reason) = stage_outcome(err);
                let mut result =
                    WorkloadResult::new(self.id(), kind, self.category(), self.description());
                result.status = status;
                result.reason = Some(reason.to_string());
                result.notes = staging_notes;
                return result;
            }
        };

        let runs = runs_for(ctx.profile);
        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result.notes = staging_notes;

        let mut overall_peak: Option<u64> = None;
        let mut observed_all: Vec<String> = Vec::new();
        let mut ok_cases = 0usize;
        let mut unsupported_cases = 0usize;

        for case in ready {
            let outcome = execute_case(program, kind, dir, case, runs);
            if outcome.ok() {
                ok_cases += 1;
                result.metrics.push(Metric::samples(
                    format!("case_{}_wall_ms", case.name),
                    "ms",
                    &outcome.wall,
                ));
                if !outcome.rss.is_empty() {
                    result.metrics.push(Metric::samples(
                        format!("case_{}_peak_rss_bytes", case.name),
                        "bytes",
                        &outcome.rss,
                    ));
                    for &bytes in &outcome.rss {
                        let bytes = bytes as u64;
                        overall_peak = Some(overall_peak.map_or(bytes, |peak| peak.max(bytes)));
                    }
                }
                if !outcome.startup.is_empty() {
                    result.metrics.push(Metric::samples(
                        format!("case_{}_startup_ms", case.name),
                        "ms",
                        &outcome.startup,
                    ));
                }
                // The paired marginal figure (or an auditable reason it was withheld).
                emit_per_call(case, &outcome.marginal, &mut result.metrics, &mut result.notes);
                // The deterministic companion: the loop count the marginal above was derived
                // from, so the N behind every per-call figure is in the report.
                result.metrics.push(calls_metric(case, outcome.n));
                if let Some(value) = &outcome.observed {
                    observed_all.push(format!("{}={value}", case.name));
                }
                result.notes.push(format!(
                    "case {}: N={} ({}), sources={}",
                    case.name,
                    outcome.n,
                    kind.as_str(),
                    case.sources_label()
                ));
            } else if outcome.unsupported {
                unsupported_cases += 1;
            }
            result.notes.extend(outcome.notes);
        }

        result.peak_memory_bytes = overall_peak;
        if !observed_all.is_empty() {
            result.notes.push(format!("observed stdout: {}", observed_all.join(" ")));
        }
        // The N-dependence caveat belongs on the Rust row: the sys oracle's per-call cost is
        // ~flat over the same N range, while `beam_rt`'s grows with N. Recorded only when the
        // row actually produced timings, so a skipped/failed row is not annotated as measured.
        if kind == RuntimeKind::Rust && ok_cases > 0 {
            result.notes.push(N_DEPENDENCE_NOTE.to_string());
        }
        result.notes.push(
            "method: per-call cost = median of the paired (wall at N iterations - wall at 0 \
             iterations) differences / N; each round samples the N pass and then the 0 pass \
             back to back so a drifting boot cost cancels inside the pair, N is per runtime so \
             both marginals are measurable, and the fixed process startup (incl. the Rust \
             runtime's code-path resolution) cancels out of both sides"
                .to_string(),
        );
        result.notes.push(format!(
            "cases: {ok_cases} ok, {unsupported_cases} unsupported (of {})",
            CASES.len()
        ));

        if ok_cases == 0 {
            result.status = Status::Failed;
            result.reason = Some("no-foreign-case-ran".to_string());
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = Foreign;
        assert_eq!(w.id(), "foreign");
        assert_eq!(w.category(), Category::Compute);
        assert_eq!(w.contracts(), &["A.1", "A.2"]);
        assert!(matches!(w.runtimes(), Runtimes::Both));
        assert!(!w.description().is_empty());
    }

    #[test]
    fn every_case_declares_sources_and_a_calibrated_n() {
        for case in CASES {
            assert!(!case.sources.is_empty(), "{} has no sources", case.name);
            assert!(case.per_call > 0, "{} has no per-call increment", case.name);
            assert!(case.n_for(RuntimeKind::Rust) > 0, "{}", case.name);
            assert!(case.n_for(RuntimeKind::Sys) > 0, "{}", case.name);
            // The oracle is the faster runtime, so it always gets the larger loop count.
            assert!(
                case.n_sys > case.n_rust,
                "{}: the oracle loop must be the larger one",
                case.name
            );
        }
    }

    #[test]
    fn argv_carries_the_loop_count_and_the_pa_dir_for_both_runtimes() {
        let dir = Path::new("/tmp/xyz");
        let case = &CASES[0];
        let rust = argv_for(RuntimeKind::Rust, dir, case, 7);
        assert_eq!(rust[0], "-pa");
        assert_eq!(rust[1], "/tmp/xyz");
        assert_eq!(rust[2], "run");
        assert_eq!(rust[4], format!("{DRIVER_MODULE}:{}/1", case.function));
        assert_eq!(rust[5], "7");

        let sys = argv_for(RuntimeKind::Sys, dir, case, 9);
        assert_eq!(sys[0], "-noshell");
        assert!(sys.contains(&"-pa".to_string()));
        assert!(sys.iter().any(|a| a.contains(&format!("{DRIVER_MODULE}:{}(9)", case.function))));
    }

    /// A fake sample of `case` on `pass` at `wall_ms`: the deterministic stdout that pass must
    /// print (the driver's result is linear in the loop count) and a peak RSS on work only.
    fn fake_sample(case: &Case, pass: Pass, wall_ms: f64) -> CaseSample {
        let loops = case.n_for(RuntimeKind::Rust) * pass.units();
        CaseSample {
            wall_ms,
            peak_memory_bytes: (pass == Pass::Work).then_some(1_024),
            stdout: (loops * case.per_call).to_string(),
            notes: Vec::new(),
        }
    }

    #[test]
    fn rounds_are_paired_work_then_startup() {
        // The schedule is the whole point: every round must sample the work pass and then the
        // startup pass immediately after — never all work samples followed by all startup ones.
        let (case, runs) = (&CASES[0], 3);
        let mut order: Vec<Pass> = Vec::new();
        let sampled = sample_pairs(runs, |pass| {
            order.push(pass);
            Ok::<CaseSample, PassError>(fake_sample(case, pass, 100.0))
        });
        let round = [Pass::Work, Pass::Startup];
        assert_eq!(order, round.iter().copied().cycle().take(2 * runs).collect::<Vec<_>>());
        let outcome = outcome_from(case.n_for(RuntimeKind::Rust), sampled);
        assert_eq!((outcome.wall.len(), outcome.startup.len()), (runs, runs));
        // Peak RSS comes from the work samples only, one per round.
        assert_eq!(outcome.rss, vec![1_024.0; runs]);
    }

    #[test]
    fn a_failing_startup_pass_keeps_the_completed_pairs() {
        // Round 1 completes, round 2's startup pass fails: the completed round must survive,
        // the failure must be recorded (never swallowed) and it is not a capability gap.
        let (case, n) = (&CASES[0], CASES[0].n_for(RuntimeKind::Rust));
        let mut calls = 0usize;
        let sampled = sample_pairs(3, |pass| {
            calls += 1;
            if calls == 4 {
                let note = format!("case {}: startup run failed: exited 1", case.name);
                return Err(PassError { note, unsupported: false });
            }
            Ok::<CaseSample, PassError>(fake_sample(case, pass, 100.0))
        });
        let outcome = outcome_from(n, sampled);
        assert!(outcome.ok(), "the completed round must be reported");
        assert_eq!((outcome.wall.len(), outcome.startup.len()), (1, 1));
        assert_eq!((outcome.marginal.rounds, outcome.unsupported), (1, false));
        assert!(outcome.notes.iter().any(|n| n.contains("startup run failed")), "{:?}", outcome.notes);

        // A first-work-pass failure carrying the Rust capability gap keeps the flag and retains
        // nothing, so no timing is published for an unsupported case.
        let gap = PassError {
            note: format!("case {}: unsupported: unsupported_bif", case.name),
            unsupported: true,
        };
        let outcome = outcome_from(n, sample_pairs(3, |_pass| Err::<CaseSample, PassError>(gap.clone())));
        assert!(!outcome.ok() && outcome.unsupported);
        assert_eq!(outcome.marginal.unit_cost_ns(), None);
    }

    #[test]
    fn the_metric_is_withheld_inside_the_noise_band() {
        // Resolve a fake paired measurement (work pass at `work_ms`, startup pass at 100 ms) and
        // run the metric-emission step over it.
        let emit = |work_ms: f64| {
            let (case, n) = (&CASES[0], CASES[0].n_for(RuntimeKind::Rust));
            let wall = |pass: Pass| if pass == Pass::Work { work_ms } else { 100.0 };
            let fake = |pass| Ok::<CaseSample, PassError>(fake_sample(case, pass, wall(pass)));
            let outcome = outcome_from(n, sample_pairs(3, fake));
            assert!(outcome.ok(), "the samples themselves are usable");
            let (mut metrics, mut notes) = (Vec::new(), Vec::new());
            emit_per_call(case, &outcome.marginal, &mut metrics, &mut notes);
            (n, metrics, notes)
        };

        // A 0.5 ms paired difference is below `MIN_NOISE_MS` (1 ms): no metric, one note.
        let (_, metrics, notes) = emit(100.5);
        assert!(metrics.is_empty(), "no per-call metric inside the band");
        assert!(notes.len() == 1 && notes[0].contains("marginal below timer resolution"), "{notes:?}");

        // A difference that clears the band publishes the figure *and* its auditable detail.
        let (n, metrics, notes) = emit(200.0);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, format!("case_{}_ns_per_call", CASES[0].name));
        assert_eq!(metrics[0].unit, "ns");
        assert_eq!(metrics[0].value, Some(100.0 / n as f64 * 1e6));
        assert!(notes[0].contains("paired median="), "{}", notes[0]);
    }

    #[test]
    fn every_case_has_a_deterministic_calls_companion_metric() {
        // Each case's loop count must be visible in the report under `case_<name>_calls`,
        // so a `ns_per_call` figure can never be read without the N that produced it.
        for case in CASES {
            let m = calls_metric(case, 12_345);
            assert_eq!(m.name, format!("case_{}_calls", case.name));
            assert_eq!(m.unit, "calls");
            assert_eq!(m.value, Some(12_345.0));
            assert!(m.is_scalar(), "the loop count is a scalar, not a summary");
            m.validate().unwrap();
        }
    }

    #[test]
    fn n_dependence_note_states_the_caveat_and_matched_n() {
        // The note must (a) name the N-dependence, (b) say the metric is only comparable at
        // the same N, and (c) record that the m10↔m11 A/B used matched N.
        assert!(N_DEPENDENCE_NOTE.contains("N-DEPENDENT"), "{N_DEPENDENCE_NOTE}");
        assert!(N_DEPENDENCE_NOTE.contains("SAME N"), "{N_DEPENDENCE_NOTE}");
        assert!(N_DEPENDENCE_NOTE.contains("matched N"), "{N_DEPENDENCE_NOTE}");
    }
    #[test]
    fn stage_in_a_missing_dir_is_a_compile_failure_flag_not_a_skip() {
        // A non-existent scratch dir makes the driver write fail — a real failure of code
        // we staged, which must classify as `CompileFailed`, never as `NoSources`.
        let missing =
            std::env::temp_dir().join(format!("bench-foreign-absent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let mut notes = Vec::new();
        let err = match stage(&missing, Path::new("/nonexistent/erlc"), &mut notes) {
            Ok(_) => panic!("a missing driver dir must not report success"),
            Err(e) => e,
        };
        assert_eq!(err, StageError::CompileFailed);
        assert!(!notes.is_empty(), "the failure must be recorded, not swallowed");
    }

    #[test]
    fn stage_failure_maps_sources_absent_to_skip_and_compile_failure_to_failed() {
        // Genuinely-absent sources are a missing prerequisite…
        assert_eq!(
            stage_outcome(StageError::NoSources),
            (Status::Skipped, "sources-missing")
        );
        // …whereas a compile failure of staged code is a real failure, never a benign skip.
        assert_eq!(
            stage_outcome(StageError::CompileFailed),
            (Status::Failed, "module-compile-failed")
        );
    }

    /// Oracle-gated smoke test: with the real checkouts + `erlc` present, one case must
    /// produce real timings (or an honest note). No-op when prerequisites are missing.
    #[test]
    fn smoke_runs_one_case_on_each_resolvable_runtime() {
        let Some(erlc) = slice::resolve_erlc() else { return };
        if !any_sources_available() {
            return;
        }
        let runtimes = [crate::runtime::resolve_oracle().ok(), Some(crate::runtime::resolve_rust())];
        for runtime in runtimes.into_iter().flatten() {
            if !runtime.self_check() {
                continue;
            }
            let scratch = ScratchDir::new("bench-foreign-smoke").expect("scratch");
            let mut notes = Vec::new();
            let Ok(ready) = stage(scratch.path(), &erlc, &mut notes) else {
                continue;
            };
            let Some(case) = ready.first() else { continue };
            let program = runtime.program().expect("a self-checking runtime resolves");
            let outcome = execute_case(program, runtime.kind, scratch.path(), case, 1);
            // Either a real timing, or an honest note (e.g. the Rust runtime's gap).
            assert!(
                outcome.ok() || !outcome.notes.is_empty(),
                "runtime {:?}: neither samples nor notes",
                runtime.kind
            );
        }
    }
}
