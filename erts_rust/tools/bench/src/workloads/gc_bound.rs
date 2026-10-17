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

//! `gc_bound` — **the GC-bound metric**: allocate-and-abandon garbage in a loop, under
//! *both* runtimes (the system oracle and the Rust runtime `beam_rt`).
//!
//! Every repo-backed workload (Cowboy, Khepri, EMQX, PLB, the compilers) is `sys`-only, so
//! the only cross-runtime signals were `list_walk`/`foreign`, both **compute**-bound and
//! both keeping their live set tiny. The allocation/collection half of the rewrite surface
//! (`ERTS_RUST_REWRITE_TEST_PROJECTS.md`: the **GC** row) had no cross-runtime measurement.
//! This workload supplies it.
//!
//! # The program
//!
//! The embedded module ([`module_source`]) is generated from this crate's constants
//! (`LEN`, `MODULE`, `ARITY`), so Rust and Erlang cannot drift. One churn allocates three
//! `LEN`-sized structures and walks away from them, so each iteration leaves the previous
//! iteration's heap to be reclaimed by the per-process collector:
//!
//! ```erlang
//! churn(N) -> churn(N, 0).
//! churn(0, Acc) -> Acc;
//! churn(N, Acc) ->
//!     _ = build_list(LEN, []),                       % abandoned list
//!     Bin = list_to_binary(build_bytes(LEN, [])),    % abandoned binary
//!     Tup = list_to_tuple(build_list(LEN, [])),      % abandoned tuple
//!     true = (byte_size(Bin) =:= LEN),               % O(1) validity checks, so a
//!     true = (tuple_size(Tup) =:= LEN),              % silently-dropped allocation fails
//!     churn(N - 1, Acc + PER_OP).
//! ```
//!
//! The list and the intermediate char list are built **by interpreted recursion**
//! (`build_list/2`, `build_bytes/2`), so the loop is not dominated by one native allocator
//! call; the only BIFs are the two O(1) validity checks, which make a broken allocator a
//! loud failure rather than a passed-but-empty run.
//!
//! Every churn adds `PER_OP`, so a run at `N` churns prints exactly `N * PER_OP`.
//!
//! # The measurement (paired, interleaved, per op)
//!
//! The two runtimes differ by more than an order of magnitude per op, so one fixed loop
//! count cannot give both a measurable marginal: an `N` that is a real signal for the Rust
//! runtime is below the oracle's own run-to-run startup noise. The program therefore
//! declares a **per-runtime loop count** `N` and the workload runs the identical program
//! twice per round — at `N` churns (the *work* sample, `case_churn_wall_ms`) and at `0`
//! churns (the *startup* sample, `case_churn_startup_ms`: not zero, the Rust runtime loads
//! its preloaded-module set and resolves its system code path with a native filesystem probe,
//! `crates/beam_rt/src/codeserver.rs`) — **back to back inside every round**
//! ([`marginal::sample_pairs`]), and derives the throughput metric from the **median of the
//! paired differences**:
//!
//! ```text
//! case_churn_ns_per_op = median_i(wall_ms@N[i] - wall_ms@0[i]) / N * 1e6
//! ```
//!
//! Each pair's two samples straddle essentially the same host conditions, so the fixed
//! process-startup cost cancels against *its own* baseline run instead of against another
//! pass's mean.
//!
//! # Why the schedule is paired (do not "simplify" it back)
//!
//! An earlier version took **all** `N`-churn samples and then **all** `0`-churn samples and
//! formed `(mean(wall@N) - mean(wall@0)) / N`; only a *negative* marginal was refused. That
//! is valid only if the startup cost is constant over the whole measurement, and on this
//! shared host it is not: boot cost drifts by tens — at times hundreds — of milliseconds
//! between the two passes, so the mean-based difference **absorbs** the drift instead of
//! cancelling it and silently publishes an inflated figure (an independent interleaved
//! measurement of the sibling `copy_bound` workload shows the failure mode is worth a ~56×
//! inflation of a published gap). Sampling both passes inside one round makes drift a
//! per-pair residual of at most one sampling interval rather than a bias of `runs`
//! intervals, and the median ignores the one round a load spike hit. **The sequential
//! all-N-then-all-0 schedule must never come back**; the rules live in [`crate::marginal`],
//! which every per-unit workload shares.
//!
//! The figure is withheld — recorded as `marginal below timer resolution`, with no metric —
//! when the median paired difference does not clear
//! `NOISE_MULTIPLE × max(MIN_NOISE_MS, 1.4826 × MAD)` of the deltas: no number is better than
//! a drift artifact, and every published figure carries its auditable
//! `paired median=… min=… max=… noise=±… over k round(s)` note.
//!
//! Because the *timing* is only as stable as the host, the workload also emits
//! `case_churn_ops = N`, the deterministic number of allocate-and-abandon iterations the
//! figure was derived from. That metric is **contention-immune**: it stays correct and
//! comparable on a loaded machine (a shared benchmark host moves the wall-clock rows
//! several-fold).
//!
//! # Honest reporting
//!
//! * Every runtime's stdout must equal the deterministic `N * PER_OP`; a mismatch is
//!   recorded as a note.
//! * A Rust exit `3` (`beam_rt: unsupported: …`) records the capability gap and emits
//!   **no** timing — the row is a `skipped(unsupported-case)`, never a fake number.
//! * `erlc` unavailable ⇒ an honest `skipped(erlc-absent)`.
//! * The embedded module failing to compile is a **failure**
//!   (`module-compile-failed`), never a skip: the source is ours, so it cannot
//!   legitimately be missing.
//!
//! Whether a given runtime's collector actually runs during the loop is its own admission
//! policy, not something this driver asserts: the measured quantity is the wall time of the
//! allocate-and-abandon shape (which is exactly the workload the collector is on the hook
//! for), and the report says so in its notes rather than claiming a collection count.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::marginal::{self, Marginal, Pass, PassError};
use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc::{self, Completed};
use crate::report::{Profile, WorkloadResult};
use crate::runtime::RuntimeKind;
use crate::scratch::ScratchDir;
use crate::workload::{RunContext, Workload};
use crate::workloads::slice;

/// The embedded module's name.
const MODULE: &str = "bench_gc_bound";

/// The exported benchmark function.
const FUNCTION: &str = "churn";

/// The exported benchmark function's arity.
const ARITY: u32 = 1;

/// The size of each structure one churn allocates and abandons (list elements, binary
/// bytes, tuple elements).
///
/// Large enough that one churn is real heap work rather than call overhead, small enough
/// that a single run stays a few hundred milliseconds. It is the *only* place the size is
/// written down: the Erlang source is generated from it.
const LEN: i64 = 256;

/// How much one churn adds to the accumulator, so a run at `N` churns prints `N * PER_OP`.
/// The Erlang source is generated from it.
const PER_OP: i64 = 1;

/// The one case: the loop counts per runtime.
struct Case {
    /// Stable, short case name (used in the metric names, e.g. `case_<name>_wall_ms`).
    name: &'static str,
    /// Loop count calibrated for the Rust runtime.
    n_rust: i64,
    /// Loop count calibrated for the system-runtime oracle.
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
}

/// The single case.
///
/// `n_rust`/`n_sys` are **constants, not measurements**, calibrated on the reference host
/// (a heavily loaded shared machine — see the crate CONTEXT) under one rule: **a runtime's
/// timed loop must do enough work that its marginal cost stands clear of that runtime's own
/// run-to-run boot/startup jitter**, measured here at ≈±0.1 s on the rust boot (the boot
/// itself is ≈0.7–1.5 s under load). The rust-side floor is therefore **≥0.5 s of loop
/// work**. The measured rust churn cost on that host (2026-10-17, `/proc/loadavg` 5.7–7.7,
/// `--full`, default baseline-JIT tier) is ≈69 µs/churn, so `n_rust = 10_000` is ≈0.69 s of
/// churn work — ≳20× the paired estimator's measured ±29.5 ms residual noise band. The
/// oracle's much smaller startup jitter (tens of ms) needs a correspondingly smaller
/// marginal, so `n_sys = 200_000` is unchanged.
///
/// **Maintenance rule:** when the measured per-op cost changes materially, re-derive `n_rust`
/// so the loop work stays ≥0.5 s; the pinning test
/// (`the_rust_loop_count_clears_the_boot_jitter_band`) enforces the floor against the current
/// cost constant.
///
/// The reported metric is normalized per op, so the loop counts only have to keep both
/// marginals measurable — not to equalize the two runtimes' totals. A loop count whose
/// marginal falls below the timer noise yields **no** per-op metric (an honest note), never a
/// fabricated one.
static CASE: Case = Case {
    name: "churn",
    n_rust: 10_000,
    n_sys: 200_000,
};

/// The `gc_bound` workload.
pub struct GcBound;

/// The number of timed samples per pass for a profile.
fn runs_for(profile: Profile) -> usize {
    match profile {
        Profile::Quick => 3,
        Profile::Full => 10,
    }
}

/// The embedded Erlang module, generated from the constants above.
///
/// Deterministic: `churn(N)` returns `N * PER_OP`, and each churn allocates and abandons a
/// `LEN`-element list, a `LEN`-byte binary and a `LEN`-element tuple.
fn module_source() -> String {
    format!(
        "\
-module({MODULE}).
-export([{FUNCTION}/{ARITY}]).

%% Allocate-and-abandon garbage: one churn builds three {LEN}-sized structures — a list
%% (interpreted recursion), a binary and a tuple — discards them for the collector, and
%% checks the two O(1)-checkable ones so a silently-dropped allocation fails loudly.
%% Each churn adds {PER_OP}, so {FUNCTION}(N) == N * {PER_OP}.
{FUNCTION}(N) -> {FUNCTION}(N, 0).

{FUNCTION}(0, Acc) -> Acc;
{FUNCTION}(N, Acc) ->
    _ = build_list({LEN}, []),
    Bin = list_to_binary(build_bytes({LEN}, [])),
    Tup = list_to_tuple(build_list({LEN}, [])),
    true = (byte_size(Bin) =:= {LEN}),
    true = (tuple_size(Tup) =:= {LEN}),
    {FUNCTION}(N - 1, Acc + {PER_OP}).

build_list(0, Acc) -> Acc;
build_list(N, Acc) -> build_list(N - 1, [N | Acc]).

build_bytes(0, Acc) -> Acc;
build_bytes(N, Acc) -> build_bytes(N - 1, [N rem 256 | Acc]).
"
    )
}

/// The number of allocate-and-abandon iterations one case's `n`-churn run performs.
fn ops(n: i64) -> i64 {
    n
}

/// The stdout a run at `n` churns must print (the deterministic constant).
fn expected_stdout(n: i64) -> String {
    (n * PER_OP).to_string()
}

/// The per-side argv for the case at `n` churns (everything after the program path).
fn argv_for(kind: RuntimeKind, dir: &Path, n: i64) -> Vec<String> {
    let beam = dir.join(format!("{MODULE}.beam"));
    match kind {
        RuntimeKind::Rust => vec![
            "-pa".to_string(),
            dir.display().to_string(),
            "run".to_string(),
            beam.display().to_string(),
            format!("{MODULE}:{FUNCTION}/{ARITY}"),
            n.to_string(),
        ],
        RuntimeKind::Sys => vec![
            "-noshell".to_string(),
            "-pa".to_string(),
            dir.display().to_string(),
            "-eval".to_string(),
            format!("io:format(\"~w~n\",[{MODULE}:{FUNCTION}({n})]), halt(0)."),
        ],
    }
}

/// The argv for one pass of the measurement: the [`Pass::Work`] pass runs `N` churns, the
/// [`Pass::Startup`] pass `0`.
fn pass_argv(kind: RuntimeKind, dir: &Path, n: i64, pass: Pass) -> Vec<String> {
    argv_for(kind, dir, n * pass.units())
}

/// Writes the embedded module into `dir` and compiles it with `erlc`.
///
/// Returns `Err(reason)` — never a silent `false` — when the source cannot be written or
/// the module does not compile, so the caller can report a real failure.
fn stage(dir: &Path, erlc: &Path) -> Result<PathBuf, String> {
    let source = dir.join(format!("{MODULE}.erl"));
    std::fs::write(&source, module_source())
        .map_err(|e| format!("write {}: {e}", source.display()))?;

    let mut cmd = Command::new(erlc);
    cmd.arg("-o").arg(dir).arg(&source);
    match proc::run(&mut cmd) {
        Ok(out) if out.success => {}
        Ok(out) => {
            return Err(format!(
                "erlc exited {:?}: {}",
                out.exit_code,
                out.stderr.trim()
            ))
        }
        Err(e) => return Err(format!("erlc spawn failed: {e}")),
    }

    let beam = dir.join(format!("{MODULE}.beam"));
    if beam.is_file() {
        Ok(beam)
    } else {
        Err(format!("{} missing after erlc exited 0", beam.display()))
    }
}

/// One pass's raw sample: the program ran, so its wall time and peak RSS are real.
struct Sample {
    /// End-to-end process wall time (ms).
    wall_ms: f64,
    /// Peak RSS (bytes) of the run, when the poller could read it.
    peak_memory_bytes: Option<u64>,
    /// The trimmed stdout, validated against the deterministic constant by [`run_pass`].
    stdout: String,
    /// Per-sample notes (a stdout mismatch is a note, never an abort).
    notes: Vec<String>,
}

/// The samples collected under one runtime, plus the marginal resolved from them.
struct Outcome {
    /// The loop count used.
    n: i64,
    /// One `(wall_ms@N, wall_ms@0)` sample pair per completed round, in round order.
    pairs: Vec<(f64, f64)>,
    /// Peak-RSS samples (bytes) of the `n`-churn runs.
    rss: Vec<f64>,
    /// The stdout observed on the first completed `n`-churn run.
    observed: Option<String>,
    /// Whether the runtime reported the case as `unsupported` (Rust exit `3` before any
    /// `n`-churn run ever completed).
    unsupported: bool,
    /// Notes about failures/variations (never silent).
    notes: Vec<String>,
    /// The paired, drift-cancelling per-op marginal (`None` figure inside the noise band).
    marginal: Marginal,
}

impl Outcome {
    /// Whether the case produced at least one usable `(work, startup)` pair.
    fn ok(&self) -> bool {
        !self.pairs.is_empty()
    }
}

/// Runs one pass — the program at `n × pass.units()` churns — and collects its sample.
///
/// A non-zero exit or a killed run is an `Err`; a stdout mismatch is a note on the sample,
/// never an abort (that run's timing is still real).
fn run_pass(
    program: &Path,
    kind: RuntimeKind,
    dir: &Path,
    n: i64,
    pass: Pass,
) -> Result<Sample, PassError> {
    let churns = n * pass.units();
    let mut cmd = Command::new(program);
    cmd.args(pass_argv(kind, dir, n, pass));
    match proc::run(&mut cmd) {
        Ok(completed) if completed.success => {
            let expected = expected_stdout(churns);
            let stdout = completed.stdout_trim().to_string();
            let mut notes = Vec::new();
            if stdout != expected {
                notes.push(format!(
                    "case {}: the {} run printed `{stdout}` (expected `{expected}`)",
                    CASE.name,
                    pass.as_str()
                ));
            }
            Ok(Sample {
                wall_ms: completed.wall_ms,
                peak_memory_bytes: completed.peak_memory_bytes,
                stdout,
                notes,
            })
        }
        Ok(completed) => Err(PassError {
            note: classify_failure(kind, &completed),
            // The Rust runtime's capability gap. `execute` downgrades it to a real failure
            // once the loop has run at least once.
            unsupported: kind == RuntimeKind::Rust && completed.exit_code == Some(3),
        }),
        Err(e) => Err(PassError {
            note: format!("case {} ({}): spawn failed: {e}", CASE.name, kind.as_str()),
            unsupported: false,
        }),
    }
}

/// Classifies a non-zero / killed run into a human note.
fn classify_failure(kind: RuntimeKind, completed: &Completed) -> String {
    let stderr = completed.stderr.trim();
    match (kind, completed.exit_code) {
        (RuntimeKind::Rust, Some(3)) => {
            let reason = stderr
                .strip_prefix("beam_rt: unsupported: ")
                .unwrap_or(stderr);
            format!("case {}: unsupported: {reason}", CASE.name)
        }
        (_, Some(code)) => format!(
            "case {} ({}): exited {code}: {stderr}",
            CASE.name,
            kind.as_str()
        ),
        (_, None) => format!(
            "case {} ({}): terminated by signal: {stderr}",
            CASE.name,
            kind.as_str()
        ),
    }
}

/// Runs `runs` interleaved rounds — the `n`-churn pass and then the `0`-churn pass inside
/// every round — and resolves the paired marginal.
fn execute(program: &Path, kind: RuntimeKind, dir: &Path, runs: usize) -> Outcome {
    let n = CASE.n_for(kind);
    let mut work_samples = 0usize;
    measure(n, runs, |pass| match run_pass(program, kind, dir, n, pass) {
        Ok(sample) => {
            if pass == Pass::Work {
                work_samples += 1;
            }
            Ok(sample)
        }
        // A capability gap classifies as `unsupported` only while the loop has never run: a
        // Rust exit `3` after a completed round is a real failure, not a missing capability.
        Err(err) => Err(PassError {
            note: err.note,
            unsupported: err.unsupported && work_samples == 0,
        }),
    })
}

/// Assembles an [`Outcome`] from `runs` rounds sampled by `sample`.
///
/// `sample` produces one pass's [`Sample`] — in production one process spawn ([`run_pass`]).
/// Keeping it a parameter is what makes the *schedule* and a mid-measurement failure testable
/// without spawning anything. A failure aborts the measurement, but the rounds completed
/// before it are retained, so a case with usable pairs is still reported.
fn measure<F>(n: i64, runs: usize, sample: F) -> Outcome
where
    F: FnMut(Pass) -> Result<Sample, PassError>,
{
    let paired = marginal::sample_pairs(runs, sample);
    let mut notes = Vec::new();
    if let Some(abort) = &paired.aborted {
        notes.push(match abort.pass {
            Pass::Work => abort.note.clone(),
            Pass::Startup => format!("case {}: startup run failed: {}", CASE.name, abort.note),
        });
    }

    let mut pairs = Vec::with_capacity(paired.pairs.len());
    let mut rss = Vec::new();
    let mut observed = None;
    for (work, startup) in &paired.pairs {
        pairs.push((work.wall_ms, startup.wall_ms));
        // Peak RSS is a property of the work run; the `0`-churn run is not attributed to it.
        if let Some(bytes) = work.peak_memory_bytes {
            rss.push(bytes as f64);
        }
        if observed.is_none() {
            observed = Some(work.stdout.clone());
        }
        notes.extend(work.notes.iter().chain(startup.notes.iter()).cloned());
    }

    Outcome {
        n,
        rss,
        observed,
        unsupported: paired.aborted.map(|a| a.unsupported).unwrap_or(false),
        notes,
        marginal: marginal::resolve(&pairs, n),
        pairs,
    }
}

impl Workload for GcBound {
    fn id(&self) -> &'static str {
        "gc_bound"
    }

    fn description(&self) -> &'static str {
        "allocate-and-abandon garbage churn under each runtime (per-op GC cost)"
    }

    fn category(&self) -> Category {
        Category::Gc
    }

    fn contracts(&self) -> &'static [&'static str] {
        // Real Erlang semantics (A.1): the same compiled module on both runtimes.
        &["A.1"]
    }

    fn runtimes(&self) -> Runtimes {
        // The whole point: both runtimes execute the same compiled churn.
        Runtimes::Both
    }

    fn probe(&self) -> Probe {
        if slice::resolve_erlc().is_none() {
            return Probe::skipped("erlc-absent");
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

        // The module source is ours, so a staging/compile failure is a real failure, never
        // a missing prerequisite.
        let staged = match stage(dir, &erlc) {
            Ok(beam) => format!(
                "staged {} (embedded source, LEN={LEN}, per-op increment={PER_OP}, erlc={})",
                beam.display(),
                erlc.display()
            ),
            Err(reason) => {
                let mut result = WorkloadResult::failed(
                    self.id(),
                    kind,
                    self.category(),
                    self.description(),
                    "module-compile-failed",
                );
                result.notes.push(reason);
                return result;
            }
        };

        let outcome = execute(program, kind, dir, runs_for(ctx.profile));
        let mut result = WorkloadResult::new(self.id(), kind, self.category(), self.description());
        result.notes.push(staged);

        if outcome.ok() {
            emit(&mut result, &outcome, kind);
        } else {
            result.notes.extend(outcome.notes);
            if outcome.unsupported {
                result.status = Status::Skipped;
                result.reason = Some("unsupported-case".to_string());
            } else {
                result.status = Status::Failed;
                result.reason = Some("no-gc-bound-run".to_string());
            }
        }

        result
    }
}

/// A sampled metric named for the case: `case_<case>_<suffix>`.
fn case_samples(suffix: &str, unit: &str, samples: &[f64]) -> Metric {
    Metric::samples(format!("case_{}_{suffix}", CASE.name), unit, samples)
}

/// Records one completed case's metrics and notes onto `result`.
///
/// Pure — it reads an [`Outcome`] and spawns nothing — so the emission decision that matters
/// most (a figure inside the noise band is withheld, never published) is directly testable.
fn emit(result: &mut WorkloadResult, outcome: &Outcome, kind: RuntimeKind) {
    let wall: Vec<f64> = outcome.pairs.iter().map(|(work, _)| *work).collect();
    let startup: Vec<f64> = outcome.pairs.iter().map(|(_, startup)| *startup).collect();
    result.metrics.push(case_samples("wall_ms", "ms", &wall));
    result.metrics.push(case_samples("startup_ms", "ms", &startup));
    if !outcome.rss.is_empty() {
        result
            .metrics
            .push(case_samples("peak_rss_bytes", "bytes", &outcome.rss));
        let peak = outcome.rss.iter().fold(0.0_f64, |a, &b| a.max(b));
        result.peak_memory_bytes = Some(peak as u64);
    }
    // The figure, or *why* there is none — never a number inside the noise band.
    match outcome.marginal.unit_cost_ns() {
        Some(ns) => {
            result.metrics.push(Metric::scalar(
                format!("case_{}_ns_per_op", CASE.name),
                "ns",
                ns,
            ));
            result
                .notes
                .push(format!("case {}: {}", CASE.name, outcome.marginal.detail()));
        }
        None => result.notes.push(format!(
            "case {}: {}",
            CASE.name,
            outcome.marginal.below_resolution_note()
        )),
    }
    // The deterministic, contention-immune companion metric: how many allocate-and-abandon
    // iterations the timing above was derived from.
    result.metrics.push(Metric::scalar(
        format!("case_{}_ops", CASE.name),
        "ops",
        ops(outcome.n) as f64,
    ));
    if let Some(value) = &outcome.observed {
        result
            .notes
            .push(format!("observed stdout: {}=N*{PER_OP}={value}", CASE.name));
    }
    result.notes.push(format!(
        "case {}: N={} churns ({}), each allocating and abandoning a {LEN}-element \
         list, a {LEN}-byte binary and a {LEN}-element tuple",
        CASE.name,
        outcome.n,
        kind.as_str()
    ));
    result.notes.push(
        "method: every round runs the program at N churns and then at 0 churns back to back, \
         and ns/op is the median paired difference (wall@N - wall@0) / N * 1e6 — never two \
         separate means, whose difference absorbs a drifting boot cost instead of cancelling \
         it. N is per runtime so both marginals are measurable, the fixed process startup \
         (incl. the Rust runtime's code-path resolution) cancels inside every pair, and a \
         figure inside the noise band is withheld rather than published"
            .to_string(),
    );
    result.notes.push(
        "note: the op count is deterministic and contention-immune; the wall-clock \
         rows move with host load. Whether a runtime's collector actually runs during \
         the loop is its own admission policy — this row measures the \
         allocate-and-abandon wall time, and claims no collection count"
            .to_string(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = GcBound;
        assert_eq!(w.id(), "gc_bound");
        assert_eq!(w.category(), Category::Gc);
        assert_eq!(w.category().as_str(), "gc");
        assert_eq!(w.contracts(), &["A.1"]);
        assert!(matches!(w.runtimes(), Runtimes::Both));
        assert!(w.runtimes().allows(RuntimeKind::Rust));
        assert!(w.runtimes().allows(RuntimeKind::Sys));
        assert!(!w.description().is_empty());
    }

    #[test]
    fn the_case_declares_calibrated_loop_counts() {
        assert_eq!(CASE.name, "churn");
        let len = LEN;
        let per_op = PER_OP;
        assert!(len > 0, "the structure size must be positive");
        assert!(per_op > 0, "the per-op increment must be positive");
        assert!(CASE.n_for(RuntimeKind::Rust) > 0);
        assert!(CASE.n_for(RuntimeKind::Sys) > 0);
        // The oracle is the faster runtime, so it always gets the larger loop count.
        assert!(
            CASE.n_sys > CASE.n_rust,
            "the oracle loop must be the larger one"
        );
    }

    /// The calibration rule from the [`CASE`] doc, pinned: the rust loop must be worth ≥0.5 s
    /// of churn work, computed at the m15-measured rust churn cost (2026-10-17:
    /// ≈69 µs/churn, so the conservative `MIN_CHURN_NS` below is 69 000 ns/op — a slower host
    /// only grows the real figure).
    ///
    /// The paired estimator's residual noise is the MAD of the paired differences (measured
    /// ±29.5 ms on this host), so a ≥0.5 s floor is ≳20× headroom over the 3×
    /// [`crate::marginal`] noise guard.
    #[test]
    fn the_rust_loop_count_clears_the_boot_jitter_band() {
        const BOOT_JITTER_MS: f64 = 100.0;
        const FLOOR_MULTIPLE: f64 = 5.0;
        const MIN_CHURN_NS: f64 = 69_000.0;

        let marginal_ms = CASE.n_rust as f64 * MIN_CHURN_NS / 1_000_000.0;
        let work_s = marginal_ms / 1_000.0;
        assert!(
            (0.5..=5.0).contains(&work_s),
            "n_rust={} ⇒ {work_s:.3} s of churn work, outside the 0.5..=5.0 s band",
            CASE.n_rust
        );
        let floor_ms = FLOOR_MULTIPLE * BOOT_JITTER_MS;
        assert!(
            marginal_ms >= floor_ms,
            "n_rust={} ⇒ {marginal_ms:.0} ms of churn work, below the {floor_ms:.0} ms floor",
            CASE.n_rust
        );
        assert_eq!(CASE.n_rust, 10_000);
    }

    #[test]
    fn argv_carries_the_loop_count_and_the_pa_dir_for_both_runtimes() {
        let dir = Path::new("/tmp/xyz");
        let rust = argv_for(RuntimeKind::Rust, dir, 7);
        assert_eq!(rust[0], "-pa");
        assert_eq!(rust[1], "/tmp/xyz");
        assert_eq!(rust[2], "run");
        assert_eq!(rust[3], format!("/tmp/xyz/{MODULE}.beam"));
        assert_eq!(rust[4], format!("{MODULE}:{FUNCTION}/{ARITY}"));
        assert_eq!(rust[5], "7");

        let sys = argv_for(RuntimeKind::Sys, dir, 9);
        assert_eq!(sys[0], "-noshell");
        assert!(sys.contains(&"-pa".to_string()));
        assert!(sys.contains(&"/tmp/xyz".to_string()));
        assert!(sys
            .iter()
            .any(|a| a.contains(&format!("{MODULE}:{FUNCTION}(9)"))));
    }

    /// A measured [`Outcome`] over `runs` fake rounds whose true per-op cost is `ns_per_op`.
    ///
    /// Nothing is spawned: only the *sample shape* is faked, so the pairing, the marginal and
    /// the emission decision are exercised exactly as in production.
    fn fake_outcome(n: i64, runs: usize, ns_per_op: f64) -> Outcome {
        let work_ms = n as f64 * ns_per_op / 1e6;
        measure(n, runs, |pass| {
            Ok(Sample {
                wall_ms: 100.0 + if pass == Pass::Work { work_ms } else { 0.0 },
                peak_memory_bytes: Some(1_000_000),
                stdout: expected_stdout(n * pass.units()),
                notes: Vec::new(),
            })
        })
    }

    /// The schedule itself: one round is the `N`-churn pass and then the `0`-churn pass,
    /// back to back — never all work passes followed by all startup passes.
    #[test]
    fn rounds_are_paired_work_then_startup() {
        let dir = Path::new("/tmp/gc-bound-pairing");
        let n = 7;
        let mut order = Vec::new();
        let mut loop_counts = Vec::new();
        let outcome = measure(n, 3, |pass| {
            let argv = pass_argv(RuntimeKind::Rust, dir, n, pass);
            order.push(pass.as_str());
            // Rust argv: `-pa <dir> run <beam> mod:fun/1 <n>` — the loop count is last.
            loop_counts.push(argv[argv.len() - 1].clone());
            Ok(Sample {
                wall_ms: 100.0 + n as f64 * pass.units() as f64,
                peak_memory_bytes: None,
                stdout: expected_stdout(n * pass.units()),
                notes: Vec::new(),
            })
        });
        assert_eq!(
            order,
            ["work", "startup", "work", "startup", "work", "startup"],
            "each round must sample the N-churn pass and then the 0-churn pass"
        );
        assert_eq!(
            loop_counts,
            ["7", "0", "7", "0", "7", "0"],
            "the work pass runs N churns, the startup pass 0"
        );
        assert_eq!(outcome.pairs.len(), 3);
        assert!(!outcome.unsupported, "no abort ⇒ nothing is a capability gap");
        assert_eq!(outcome.observed.as_deref(), Some(expected_stdout(n).as_str()));
        // 7 ms over 7 churns, recovered exactly because both passes of a round share the
        // same (here constant) 100 ms boot cost.
        assert_eq!(outcome.marginal.unit_cost_ns(), Some(1_000_000.0));
    }

    /// A failing pass aborts the measurement but keeps the rounds already completed — and a
    /// capability gap that stopped the very first work pass stays `unsupported`.
    #[test]
    fn a_failing_startup_pass_keeps_the_completed_pairs() {
        let n = 7;
        let mut samples = 0;
        let outcome = measure(n, 5, |pass| {
            samples += 1;
            if samples == 4 {
                // Passes run Work, Startup, Work, Startup, … ⇒ sample 4 is round 2's startup.
                return Err(PassError {
                    note: "case churn (sys): exited 1: the case printed nothing".into(),
                    unsupported: false,
                });
            }
            Ok(Sample {
                wall_ms: 100.0 + n as f64 * pass.units() as f64,
                peak_memory_bytes: None,
                stdout: expected_stdout(n * pass.units()),
                notes: Vec::new(),
            })
        });
        // Round 1 completed, so its pair is retained and the case is still reportable.
        assert_eq!(outcome.pairs.len(), 1);
        assert!(outcome.ok());
        assert!(!outcome.unsupported);
        assert!(
            outcome
                .notes
                .iter()
                .any(|note| note.contains("startup run failed")
                    && note.contains("printed nothing")),
            "{:?}",
            outcome.notes
        );

        // A capability gap on the very first work pass: nothing measured, flag survives.
        let gap = measure(n, 5, |_pass| {
            Err::<Sample, PassError>(PassError {
                note: "case churn: unsupported: unsupported_bif".into(),
                unsupported: true,
            })
        });
        assert!(!gap.ok());
        assert!(gap.pairs.is_empty());
        assert!(gap.unsupported);
        assert_eq!(
            gap.notes,
            vec!["case churn: unsupported: unsupported_bif".to_string()]
        );
    }

    /// The row keeps its wall/startup/RSS/ops metrics either way, but `ns_per_op` is
    /// published only for a marginal clearly outside the noise band.
    #[test]
    fn the_metric_is_withheld_inside_the_noise_band() {
        let n = CASE.n_for(RuntimeKind::Rust);
        let emit_for = |ns_per_op: f64| {
            let mut result =
                WorkloadResult::new("gc_bound", RuntimeKind::Rust, Category::Gc, "fake");
            emit(&mut result, &fake_outcome(n, 4, ns_per_op), RuntimeKind::Rust);
            result
        };
        let value = |result: &WorkloadResult, name: &str| {
            result
                .metrics
                .iter()
                .find(|metric| metric.name == name)
                .and_then(|metric| metric.value)
        };
        let note = |result: &WorkloadResult, prefix: &str| {
            result.notes.iter().any(|note| note.starts_with(prefix))
        };

        // 0 and 10 ns/op over `n` churns are a 0 ms and a ~0 ms delta — inside the band.
        for withheld in [0.0, 10.0] {
            let result = emit_for(withheld);
            let mut names: Vec<&str> = result.metrics.iter().map(|m| m.name.as_str()).collect();
            names.sort_unstable();
            assert_eq!(
                names,
                [
                    "case_churn_ops",
                    "case_churn_peak_rss_bytes",
                    "case_churn_startup_ms",
                    "case_churn_wall_ms"
                ],
                "{withheld} ns/op: the figure must be withheld, the rest kept"
            );
            assert!(
                note(&result, "case churn: marginal below timer resolution"),
                "{:?}",
                result.notes
            );
            // The deterministic companion, the peak RSS and the observed stdout survive.
            assert_eq!(result.peak_memory_bytes, Some(1_000_000));
            assert!(
                note(&result, "observed stdout: churn=N*"),
                "{:?}",
                result.notes
            );
        }

        // 100 µs/op over `n` churns is a ~500 ms delta: published, with its evidence.
        let result = emit_for(100_000.0);
        let ns = value(&result, "case_churn_ns_per_op").expect("a published figure");
        assert!((ns - 100_000.0).abs() < 1e-6, "{ns}");
        assert!(
            note(&result, "case churn: paired median="),
            "{:?}",
            result.notes
        );
    }

    #[test]
    fn the_op_count_is_the_deterministic_loop_count() {
        assert_eq!(ops(0), 0);
        assert_eq!(ops(1), 1);
        assert_eq!(ops(CASE.n_for(RuntimeKind::Rust)), CASE.n_rust);
        assert_eq!(ops(CASE.n_for(RuntimeKind::Sys)), CASE.n_sys);
        // The deterministic stdout: a run at N churns printed N * PER_OP.
        assert_eq!(expected_stdout(0), "0");
        assert_eq!(expected_stdout(1), PER_OP.to_string());
        assert_eq!(expected_stdout(3), (3 * PER_OP).to_string());
    }

    #[test]
    fn the_module_source_pins_the_shared_constants() {
        let src = module_source();
        assert!(src.starts_with(&format!("-module({MODULE}).")), "{src}");
        assert!(src.contains(&format!("-export([{FUNCTION}/{ARITY}]).")));
        assert!(src.contains(&format!("build_list({LEN}, [])")));
        assert!(src.contains(&format!("build_bytes({LEN}, [])")));
        assert!(src.contains(&format!("byte_size(Bin) =:= {LEN}")));
        assert!(src.contains(&format!("tuple_size(Tup) =:= {LEN}")));
        assert!(src.contains(&format!("Acc + {PER_OP}")));
        // The list/binaries are built by interpreted recursion, not by one native BIF.
        assert!(src.contains("build_list(N - 1, [N | Acc])"));
        assert!(src.contains("build_bytes(N - 1, [N rem 256 | Acc])"));
    }

    #[test]
    fn stage_reports_a_write_failure_instead_of_silently_failing() {
        let missing = std::env::temp_dir().join(format!(
            "bench-gc-bound-absent-{}-xyzzy",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&missing);
        let err = stage(&missing, Path::new("/nonexistent/erlc"))
            .expect_err("a missing dir must be reported");
        assert!(err.contains("write"), "{err}");
    }

    /// Oracle-gated smoke test: with `erlc` and a resolvable runtime present, the case
    /// must produce real timings (or an honest note). No-op when prerequisites are missing.
    #[test]
    fn smoke_runs_on_each_resolvable_runtime() {
        let Some(erlc) = slice::resolve_erlc() else {
            return;
        };
        let runtimes = [
            crate::runtime::resolve_oracle().ok(),
            Some(crate::runtime::resolve_rust()),
        ];
        for runtime in runtimes.into_iter().flatten() {
            if !runtime.self_check() {
                continue;
            }
            let scratch = ScratchDir::new("bench-gc-bound-smoke").expect("scratch");
            let beam = stage(scratch.path(), &erlc).expect("the embedded module must compile");
            assert!(beam.is_file());
            let program = runtime.program().expect("a self-checking runtime resolves");
            let outcome = execute(program, runtime.kind, scratch.path(), 1);
            // Either a real timing, or an honest note (e.g. the Rust runtime's gap).
            assert!(
                outcome.ok() || !outcome.notes.is_empty(),
                "runtime {:?}: neither samples nor notes",
                runtime.kind
            );
            if outcome.ok() {
                assert_eq!(
                    outcome.observed.as_deref(),
                    Some(expected_stdout(outcome.n).as_str()),
                    "runtime {:?}: the churn must print the deterministic constant",
                    runtime.kind
                );
            }
        }
    }
}
