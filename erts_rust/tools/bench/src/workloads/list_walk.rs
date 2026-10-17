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

//! `list_walk` — **the interpreter-throughput metric**: a BIF-free recursive
//! `sum/1` over a fixed-length list, run under *both* runtimes.
//!
//! `beam_engine`'s owners measure the interpreter's per-instruction loop overhead
//! with `crates/beam_engine/tests/engine_bench.rs::bench_list_walk` (a body-recursive
//! `sum([H|T]) -> H + sum(T)` walk whose every element costs ~7 dispatched
//! instructions). That test lives *inside* the engine crate and prints, it does not
//! `assert`; nothing outside `beam_engine` can see it. This workload makes the same
//! shape a **permanent, reproducible regression signal of the driver**: the identical
//! program runs under the Rust runtime and under the system oracle, and the report
//! carries nanoseconds per walked element for both.
//!
//! # The program
//!
//! The embedded module ([`module_source`]) is generated from the crate's own constants
//! (`LEN`, `MODULE`, `ARITY`), so Rust and Erlang cannot drift:
//!
//! ```erlang
//! walk(N) -> walk(N, build(LEN, []), 0).
//! walk(0, _L, Acc) -> Acc;
//! walk(N, L, Acc) -> walk(N - 1, L, Acc + sum(L)).
//! build(0, Acc) -> Acc;
//! build(N, Acc) -> build(N - 1, [N | Acc]).
//! sum([]) -> 0;
//! sum([H | T]) -> H + sum(T).
//! ```
//!
//! It is **entirely BIF-free**: no `lists:seq/2`, no `length/1`, no `iolist` — the only
//! arithmetic is one small-int `'+'/2` per element on the way back out. The list is
//! built by `build/2` (also purely interpreted) **once per process**, so what the timed
//! loop measures is the interpreter walking `LEN` elements `N` times: ~7 instructions
//! per element going down (`test`/`allocate`/`get_list`/`call`) and coming back
//! (`gc_bif`/`deallocate`/`return`).
//!
//! Every walk returns the deterministic constant `LEN * (LEN + 1) / 2`, so a run at `N`
//! walks must print exactly `N * LEN * (LEN + 1) / 2` — an independently checkable
//! agreement signal, not a self-reported timing.
//!
//! # The measurement (paired, startup-cancelled, per element)
//!
//! The two runtimes differ by orders of magnitude, so one fixed loop count cannot give
//! both a measurable marginal: the same `N` that costs the Rust runtime hundreds of
//! milliseconds costs the oracle a few milliseconds (i.e. below its own startup noise).
//! The program therefore declares a **per-runtime loop count** `N` and is sampled in
//! **interleaved rounds** ([`marginal::sample_pairs`]): each round runs the `N`-walk pass
//! and then the `0`-walk pass **back to back**, so both members of a pair straddle
//! essentially the same host conditions.
//!
//! * the `N`-walk pass ⇒ `case_list_walk_wall_ms` (end-to-end process wall time) and the
//!   work samples' `case_list_walk_peak_rss_bytes`,
//! * the `0`-walk pass ⇒ `case_list_walk_startup_ms` (the same program's pure startup/boot
//!   cost — the Rust runtime loads its preloaded-module set and resolves its system code
//!   path with a native filesystem probe (`crates/beam_rt/src/codeserver.rs`; the system
//!   `erl` is only *run* as a last resort), so this is far from zero),
//!
//! and derives the throughput metric from the **median of the per-round paired
//! differences** ([`marginal::resolve`]), with the min/max difference reported alongside in
//! a note:
//!
//! ```text
//! case_list_walk_ns_per_element = median(wall_i at N - wall_i at 0) / (N * LEN) * 1e6
//! ```
//!
//! which cancels the fixed process-startup cost **inside every pair**. The figure is
//! withheld — no metric, just a `marginal below timer resolution` note — whenever the
//! median difference does not clear `3 × max(1 ms, 1.4826 × MAD)` of the differences, so
//! a noisy or negative marginal is never published as a number.
//!
//! ## Why the passes are interleaved (the sequential schedule must never come back)
//!
//! The previous schedule ran **all `N`-walk samples first and all `0`-walk samples
//! afterwards**, then took `mean(all work) - mean(all startup)`. On a shared benchmark host
//! that difference does not cancel the boot cost: the host's load drifts *between* the two
//! passes, so the drift lands in the difference as if it were work — a published gap
//! inflated several-fold, with only "the marginal was negative" as a guard. Running the two
//! passes in adjacent pairs makes that drift a per-pair residual (at most one sampling
//! interval) instead of a bias of `runs` intervals, and taking the *median* difference
//! keeps one contended round from moving the figure. Do not reintroduce the sequential
//! order: see [`marginal`]'s module docs for the full reasoning and the regression it
//! caused.
//!
//! Because the *timing* is only as stable as the host, the workload also emits
//! `case_list_walk_elements = N * LEN`, the deterministic number of elements walked.
//! That metric is **contention-immune**: it stays correct and comparable on a loaded
//! machine (a shared benchmark host moves the wall-clock rows several-fold), and it
//! documents exactly how much work the ns/element figure was derived from.
//!
//! # Honest reporting
//!
//! * Every runtime's stdout must equal the deterministic expected value; a mismatch is
//!   recorded as a note.
//! * A Rust exit `3` (`beam_rt: unsupported: …`) records the capability gap and emits
//!   **no** timing — the row is a `skipped(unsupported-case-set)`, never a fake number.
//! * `erlc` unavailable ⇒ an honest `skipped(erlc-absent)`.
//! * The embedded module failing to compile is a **failure** (`module-compile-failed`),
//!   never a skip: the source is ours, so it cannot legitimately be missing.
//!
//! Everything is staged in a private, self-cleaning [`ScratchDir`] under `$TMPDIR`; the
//! workload never writes outside it and never panics.

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
const MODULE: &str = "bench_list_walk";

/// The exported benchmark function.
const FUNCTION: &str = "walk";

/// The exported benchmark function's arity.
const ARITY: u32 = 1;

/// The length of the list each walk traverses.
///
/// Small enough that the oracle can afford many walks (its marginal has to clear its own
/// startup noise) and large enough that a single walk is real interpreter work rather
/// than call overhead. It is the *only* place the list length is written down: the
/// Erlang source is generated from it.
const LEN: i64 = 1_000;

/// The value one `sum/1` over a `LEN`-element list `[1, .., LEN]` returns.
const PER_SUM: i64 = LEN * (LEN + 1) / 2;

/// The one case: the loop counts per runtime.
///
/// `n_rust`/`n_sys` are **constants, not measurements**, calibrated on the reference host
/// (a heavily loaded shared machine — see the crate CONTEXT) under one rule: **a runtime's
/// timed loop must do enough work that its marginal cost stands clear of that runtime's own
/// run-to-run boot/startup jitter** (measured at ≈±0.1 s on the rust side's ≈0.7–1.5 s
/// boot). The rust-side target is **≳1.5 s of loop work**; the oracle's much smaller
/// startup jitter (tens of ms) needs a correspondingly smaller marginal.
///
/// Measured per-element cost on that host by direct `beam_rt`/`erl` ladders (2026-10-16,
/// `/proc/loadavg` 45–100, min-of-reps):
///
/// * Rust, **default (baseline-JIT) tier**: **~31–33 ns/element** ⇒ `n_rust = 50_000` walks
///   (5×10^7 elements) is ≈1.6 s;
/// * Rust with `BEAM_RT_TIER=interpreter`: ~400 ns/element (a Rust-vs-Rust tier fact, never
///   a `sys` comparison);
/// * oracle: **~5–7 ns/element** ⇒ `n_sys = 100_000` walks (10^8 elements) is ≈0.5–1.0 s.
///
/// The reported metric is normalized **per element**, so the loop counts only have to keep
/// both marginals measurable — not to equalize the two runtimes' totals. Never compare raw
/// `wall_ms` across two sides with different `N`: the `case_list_walk_elements` companion
/// records the `N` behind every figure.
struct Case {
    /// Stable, short case name (used in the metric names, e.g. `case_<name>_wall_ms`).
    name: &'static str,
    /// Loop count calibrated for the Rust runtime (target: ≳1.5 s of loop work).
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
static CASE: Case = Case {
    name: "list_walk",
    n_rust: 50_000,
    n_sys: 100_000,
};

/// The `list_walk` workload.
pub struct ListWalk;

/// The number of timed samples per pass for a profile.
fn runs_for(profile: Profile) -> usize {
    match profile {
        Profile::Quick => 3,
        Profile::Full => 10,
    }
}

/// The embedded Erlang module, generated from the constants above.
///
/// Body-recursive, BIF-free and deterministic: `walk(N)` returns `N * PER_SUM`.
fn module_source() -> String {
    format!(
        "\
-module({MODULE}).
-export([{FUNCTION}/{ARITY}]).

%% A BIF-free, body-recursive walk of a {LEN}-element list: nothing but the interpreter.
%% walk(N) -> N * {PER_SUM} (the sum of 1..{LEN}), so the result is a deterministic
%% constant that both runtimes must print.
{FUNCTION}(N) -> {FUNCTION}(N, build({LEN}, []), 0).

{FUNCTION}(0, _L, Acc) -> Acc;
{FUNCTION}(N, L, Acc) -> {FUNCTION}(N - 1, L, Acc + sum(L)).

build(0, Acc) -> Acc;
build(N, Acc) -> build(N - 1, [N | Acc]).

sum([]) -> 0;
sum([H | T]) -> H + sum(T).
"
    )
}

/// The number of elements one case's `n` walks traverse.
fn elements(n: i64) -> i64 {
    n * LEN
}

/// The stdout a run at `n` walks must print (the deterministic constant).
fn expected_stdout(n: i64) -> String {
    (n * PER_SUM).to_string()
}

/// The per-side argv for the case at `n` walks (everything after the program path).
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

/// One sample of one pass: exactly what a single process run contributes.
///
/// It carries its own `notes` because a stdout mismatch is a **non-fatal** observation —
/// the sample is still usable, the note just has to survive into the report.
struct Sample {
    /// End-to-end process wall time (ms).
    wall_ms: f64,
    /// Peak RSS (bytes) of this run, when `/proc` was readable.
    peak_memory_bytes: Option<u64>,
    /// The run's stdout, trailing whitespace trimmed.
    stdout: String,
    /// Non-fatal notes about this sample (e.g. a deterministic-stdout mismatch).
    notes: Vec<String>,
}

/// The samples collected under one runtime, in interleaved `(work, startup)` pairs.
struct Outcome {
    /// The loop count used.
    n: i64,
    /// One `(work, startup)` sample pair per completed round, in round order.
    pairs: Vec<(Sample, Sample)>,
    /// Peak-RSS samples (bytes) of the **work** passes only.
    rss: Vec<f64>,
    /// The stdout observed on the first successful `n`-walk run.
    observed: Option<String>,
    /// Whether the runtime reported the case as `unsupported` (Rust exit `3`).
    unsupported: bool,
    /// Notes about failures/variations (never silent), in sampling order.
    notes: Vec<String>,
    /// The resolved paired marginal over `elements(n)` elements.
    marginal: Marginal,
}

impl Outcome {
    /// Whether the case produced at least one complete `(work, startup)` pair.
    fn ok(&self) -> bool {
        !self.pairs.is_empty()
    }

    /// The work-pass wall-clock samples (ms), one per completed round.
    fn wall(&self) -> Vec<f64> {
        self.pairs.iter().map(|(work, _)| work.wall_ms).collect()
    }

    /// The startup-pass wall-clock samples (ms), one per completed round.
    fn startup(&self) -> Vec<f64> {
        self.pairs.iter().map(|(_, startup)| startup.wall_ms).collect()
    }
}

/// The note for a failed (or killed) pass; a startup failure is labelled as such.
fn failure_note(kind: RuntimeKind, pass: Pass, completed: &Completed) -> String {
    let note = classify_failure(kind, completed);
    match pass {
        Pass::Work => note,
        Pass::Startup => format!("case {}: startup run failed: {note}", CASE.name),
    }
}
/// Gathers `runs` interleaved rounds through [`marginal::sample_pairs`] into an
/// [`Outcome`].
///
/// `sample` performs one pass — it is the **only** place a process is spawned, so the
/// schedule and everything derived from it can be tested with a fake sampling function.
/// Per round the `Work` pass is sampled immediately before the `Startup` pass; the pairs
/// completed before a failure are retained (a partial measurement is still reportable),
/// and the marginal is resolved from the paired differences over `elements(n)` elements.
fn gather(n: i64, runs: usize, sample: impl FnMut(Pass) -> Result<Sample, PassError>) -> Outcome {
    let sampled = marginal::sample_pairs(runs, sample);
    let mut rss = Vec::new();
    let mut observed: Option<String> = None;
    let mut notes = Vec::new();
    for (work, startup) in &sampled.pairs {
        if observed.is_none() {
            observed = Some(work.stdout.clone());
        }
        if let Some(bytes) = work.peak_memory_bytes {
            rss.push(bytes as f64);
        }
        notes.extend(work.notes.iter().cloned());
        notes.extend(startup.notes.iter().cloned());
    }
    let (unsupported, aborted) = match &sampled.aborted {
        Some(abort) => (abort.unsupported, Some(abort.note.clone())),
        None => (false, None),
    };
    notes.extend(aborted);
    let deltas: Vec<(f64, f64)> = sampled
        .pairs
        .iter()
        .map(|(work, startup)| (work.wall_ms, startup.wall_ms))
        .collect();
    Outcome {
        n,
        pairs: sampled.pairs,
        rss,
        observed,
        unsupported,
        notes,
        marginal: marginal::resolve(&deltas, elements(n)),
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

/// Runs the program in `runs` interleaved `(N walks, 0 walks)` rounds, collecting samples.
fn execute(program: &Path, kind: RuntimeKind, dir: &Path, runs: usize) -> Outcome {
    let n = CASE.n_for(kind);
    // Whether any work pass has completed: a capability gap is only reported as
    // `unsupported` when it is the *first* thing the case does (matching a runtime that
    // cannot run the program at all), not as an escape hatch from a broken measurement.
    let mut work_seen = false;
    gather(n, runs, |pass| {
        let loops = n * pass.units();
        let expected = expected_stdout(loops);
        let mut cmd = Command::new(program);
        cmd.args(argv_for(kind, dir, loops));
        let completed = proc::run(&mut cmd).map_err(|e| PassError {
            note: format!(
                "case {} ({}): {} spawn failed: {e}",
                CASE.name,
                kind.as_str(),
                pass.as_str()
            ),
            unsupported: false,
        })?;
        if !completed.success {
            return Err(PassError {
                note: failure_note(kind, pass, &completed),
                unsupported: kind == RuntimeKind::Rust
                    && completed.exit_code == Some(3)
                    && !work_seen,
            });
        }
        let stdout = completed.stdout_trim().to_string();
        let mut notes = Vec::new();
        if stdout != expected {
            notes.push(match pass {
                Pass::Work => format!(
                    "case {}: unexpected stdout `{stdout}` (expected `{expected}`)",
                    CASE.name
                ),
                Pass::Startup => format!(
                    "case {}: the startup run printed `{stdout}` (expected `{expected}`)",
                    CASE.name
                ),
            });
        }
        if pass == Pass::Work {
            work_seen = true;
        }
        Ok(Sample {
            wall_ms: completed.wall_ms,
            peak_memory_bytes: completed.peak_memory_bytes,
            stdout,
            notes,
        })
    })
}
impl Workload for ListWalk {
    fn id(&self) -> &'static str {
        "list_walk"
    }

    fn description(&self) -> &'static str {
        "a BIF-free recursive list walk on both runtimes (interpreter ns/element)"
    }

    fn category(&self) -> Category {
        Category::Compute
    }

    fn contracts(&self) -> &'static [&'static str] {
        // A real `.beam` module (A.2) executing real Erlang semantics (A.1).
        &["A.1", "A.2"]
    }

    fn runtimes(&self) -> Runtimes {
        // The whole point: both runtimes execute the same compiled module.
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

        // The module source is ours, so a staging/compile failure is a real failure,
        // never a missing prerequisite.
        let staged = match stage(dir, &erlc) {
            Ok(beam) => format!(
                "staged {} (embedded source, LEN={LEN}, erlc={})",
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
        // Per-sample observations (a stdout mismatch is a note, never an abort) and the
        // abort note when the measurement stopped early — reported either way.
        result.notes.extend(outcome.notes.iter().cloned());

        if outcome.ok() {
            result.metrics.push(Metric::samples(
                format!("case_{}_wall_ms", CASE.name),
                "ms",
                &outcome.wall(),
            ));
            result.metrics.push(Metric::samples(
                format!("case_{}_startup_ms", CASE.name),
                "ms",
                &outcome.startup(),
            ));
            if !outcome.rss.is_empty() {
                result.metrics.push(Metric::samples(
                    format!("case_{}_peak_rss_bytes", CASE.name),
                    "bytes",
                    &outcome.rss,
                ));
                let peak = outcome.rss.iter().fold(0.0_f64, |a, &b| a.max(b));
                result.peak_memory_bytes = Some(peak as u64);
            }
            push_ns_per_element(&mut result, &outcome.marginal);
            // The deterministic, contention-immune companion metric: how many elements the
            // timing above was derived from.
            result.metrics.push(Metric::scalar(
                format!("case_{}_elements", CASE.name),
                "elements",
                elements(outcome.n) as f64,
            ));

            if let Some(value) = &outcome.observed {
                result.notes.push(format!(
                    "observed stdout: {}=N*{PER_SUM}={value}",
                    CASE.name
                ));
            }
            result.notes.push(format!(
                "case {}: N={} walks ({}) at LEN={LEN}, i.e. {} elements walked",
                CASE.name,
                outcome.n,
                kind.as_str(),
                elements(outcome.n)
            ));
            result.notes.push(
                "method: paired startup-cancelled marginal — every round runs the N-walk pass \
                 then the 0-walk pass back to back, and ns/element = median(paired difference) \
                 / (N * LEN) * 1e6, withheld below 3x max(1 ms, 1.4826*MAD) of the paired \
                 differences; N is per runtime so both marginals are measurable, and the fixed \
                 process startup (incl. the Rust runtime's code-path resolution) cancels inside \
                 every pair; the module is BIF-free and builds no term"
                    .to_string(),
            );
            result.notes.push(
                "note: element count is deterministic and contention-immune; the wall-clock \
                 rows move with host load"
                    .to_string(),
            );
        } else {
            if outcome.unsupported {
                result.status = Status::Skipped;
                result.reason = Some("unsupported-case-set".to_string());
            } else {
                result.status = Status::Failed;
                result.reason = Some("no-list-walk-run".to_string());
            }
        }

        result
    }
}

/// Emits the per-element metric from a resolved marginal: the figure **and** the auditable
/// `detail` line behind it when published, otherwise the below-resolution note (and no
/// metric at all) — never a fabricated or negative number.
fn push_ns_per_element(result: &mut WorkloadResult, marginal: &Marginal) {
    match marginal.unit_cost_ns() {
        Some(ns) => {
            result.metrics.push(Metric::scalar(
                format!("case_{}_ns_per_element", CASE.name),
                "ns",
                ns,
            ));
            result
                .notes
                .push(format!("case {}: {}", CASE.name, marginal.detail()));
        }
        None => result.notes.push(format!(
            "case {}: {}; no ns_per_element metric",
            CASE.name,
            marginal.below_resolution_note()
        )),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_is_stable() {
        let w = ListWalk;
        assert_eq!(w.id(), "list_walk");
        assert_eq!(w.category(), Category::Compute);
        assert_eq!(w.contracts(), &["A.1", "A.2"]);
        assert!(matches!(w.runtimes(), Runtimes::Both));
        assert!(w.runtimes().allows(RuntimeKind::Rust));
        assert!(w.runtimes().allows(RuntimeKind::Sys));
        assert!(!w.description().is_empty());
    }

    #[test]
    fn the_case_declares_calibrated_loop_counts() {
        assert_eq!(CASE.name, "list_walk");
        assert!(LEN > 0, "the list length must be positive");
        assert_eq!(PER_SUM, LEN * (LEN + 1) / 2);
        assert!(CASE.n_for(RuntimeKind::Rust) > 0);
        assert!(CASE.n_for(RuntimeKind::Sys) > 0);
        // The oracle is the faster runtime, so it always gets the larger loop count.
        assert!(
            CASE.n_sys > CASE.n_rust,
            "the oracle loop must be the larger one"
        );
    }

    /// The m14 recalibration: the rust loop count must be large enough that the loop's
    /// marginal stands clear of the rust side's ≈±0.1 s run-to-run boot jitter. At the
    /// measured ≈32 ns/element (default baseline-JIT tier, 2026-10-16) the old `N` = 1 000
    /// was only ≈32 ms of work — inside the jitter band, which is exactly why the metric
    /// stopped resolving; the new `N` is ≳1.5 s of work and still only a few seconds.
    #[test]
    fn the_rust_loop_count_clears_the_boot_jitter_band() {
        const RUST_NS_PER_ELEMENT: f64 = 32.0; // measured; see the `Case` doc comment
        assert_eq!(CASE.n_rust, 50_000, "the recalibrated rust loop count");
        let work_s = CASE.n_rust as f64 * LEN as f64 * RUST_NS_PER_ELEMENT / 1e9;
        assert!(work_s >= 1.5, "the rust loop must do ≳1.5 s of work: {work_s} s");
        assert!(work_s <= 5.0, "the rust loop must stay a few seconds: {work_s} s");
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

    /// A fake sample: a startup pass costing 100 ms, a work pass costing `work_ms` more.
    fn fake_sample(pass: Pass, work_ms: f64) -> Sample {
        let work = pass == Pass::Work;
        Sample {
            wall_ms: if work { 100.0 + work_ms } else { 100.0 },
            peak_memory_bytes: work.then_some(4096),
            stdout: pass.units().to_string(),
            notes: Vec::new(),
        }
    }

    /// The paired schedule: every startup sample sits immediately after its own work
    /// sample, so a drifting boot cost cancels inside the pair instead of landing in the
    /// marginal (the sequential all-N-then-all-0 order must never come back).
    #[test]
    fn rounds_are_paired_work_then_startup() {
        let mut order = Vec::new();
        let outcome = gather(LEN, 3, |pass| {
            order.push(pass);
            Ok::<Sample, PassError>(fake_sample(pass, 10.0))
        });
        let round = [Pass::Work, Pass::Startup];
        assert_eq!(order, [round; 3].concat(), "each round is work then startup");
        assert_eq!(outcome.pairs.len(), 3);
        assert!(outcome.ok());
        assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
        // 10 ms of walking over N=LEN walks × LEN elements ⇒ 10 ns per element.
        assert_eq!(outcome.marginal.unit_cost_ns(), Some(10.0));
        assert_eq!(outcome.marginal.rounds, 3);
        assert_eq!(outcome.observed.as_deref(), Some("1"));
        assert_eq!(outcome.rss, vec![4096.0; 3], "RSS from the work samples only");
        assert_eq!(outcome.wall(), vec![110.0; 3]);
        assert_eq!(outcome.startup(), vec![100.0; 3]);
    }

    /// A failure keeps every round that already completed (a partial measurement is still
    /// reportable), and a capability gap on the very first pass is flagged, not timed.
    #[test]
    fn a_failing_startup_pass_keeps_the_completed_pairs() {
        let mut startups = 0;
        let outcome = gather(LEN, 3, |pass| {
            if pass == Pass::Startup {
                startups += 1;
                if startups == 2 {
                    return Err(PassError {
                        note: "case list_walk: startup run failed: exited 1: boom".into(),
                        unsupported: false,
                    });
                }
            }
            Ok::<Sample, PassError>(fake_sample(pass, 10.0))
        });
        assert_eq!(outcome.pairs.len(), 1, "the completed round is retained");
        assert!(outcome.ok(), "one complete pair is still usable");
        assert_eq!(outcome.marginal.rounds, 1);
        assert_eq!(
            (outcome.wall(), outcome.startup()),
            (vec![110.0], vec![100.0])
        );
        assert!(
            outcome.notes.iter().any(|n| n.contains("boom")),
            "the abort must be reported: {:?}",
            outcome.notes
        );
        assert!(!outcome.unsupported);

        // A capability gap on the first work pass: nothing retained, `unsupported` flagged.
        let outcome = gather(LEN, 3, |_pass| {
            Err::<Sample, PassError>(PassError {
                note: "case list_walk: unsupported: unsupported_bif".into(),
                unsupported: true,
            })
        });
        assert!(!outcome.ok() && outcome.pairs.is_empty() && outcome.unsupported);
        assert_eq!(outcome.marginal.unit_cost_ns(), None);
        assert_eq!(outcome.notes.len(), 1, "{:?}", outcome.notes);
    }

    /// The published figure carries its auditable detail; inside the noise band no metric
    /// is emitted at all (never a fabricated or negative number).
    #[test]
    fn the_metric_is_withheld_inside_the_noise_band() {
        let emit = |pairs: &[(f64, f64)]| {
            let mut result =
                WorkloadResult::new("list_walk", RuntimeKind::Sys, Category::Compute, "test");
            push_ns_per_element(&mut result, &marginal::resolve(pairs, elements(CASE.n_sys)));
            result
        };
        // Differences straddling zero ⇒ inside the noise band ⇒ no figure, just the note.
        let withheld = emit(&[(101.0, 100.0), (99.0, 100.0)]);
        assert!(withheld.metrics.is_empty(), "no figure inside the noise band");
        assert!(
            withheld.notes[0].starts_with("case list_walk: marginal below timer resolution"),
            "{:?}",
            withheld.notes
        );

        let published = emit(&[(110.0, 100.0); 3]);
        assert_eq!(published.metrics.len(), 1);
        assert_eq!(published.metrics[0].name, "case_list_walk_ns_per_element");
        assert_eq!(published.metrics[0].unit, "ns");
        // 10 ms over n_sys walks × LEN elements.
        let expected = 10.0 / elements(CASE.n_sys) as f64 * 1e6;
        assert_eq!(published.metrics[0].value, Some(expected));
        assert!(
            published.notes[0].starts_with("case list_walk: paired median=10.000 ms"),
            "{:?}",
            published.notes
        );
    }

    #[test]    fn the_element_count_is_the_deterministic_product() {
        assert_eq!(elements(0), 0);
        assert_eq!(elements(1), LEN);
        assert_eq!(elements(CASE.n_for(RuntimeKind::Rust)), CASE.n_rust * LEN);
        assert_eq!(elements(CASE.n_for(RuntimeKind::Sys)), CASE.n_sys * LEN);
        // The deterministic stdout: `walk(N)` returns `N` times the sum of `1..=LEN`.
        assert_eq!(expected_stdout(0), "0");
        assert_eq!(expected_stdout(1), PER_SUM.to_string());
        assert_eq!(expected_stdout(3), (3 * PER_SUM).to_string());
    }

    #[test]
    fn the_module_source_pins_the_shared_constants_and_stays_bif_free() {
        let src = module_source();
        assert!(src.starts_with(&format!("-module({MODULE}).")), "{src}");
        assert!(src.contains(&format!("-export([{FUNCTION}/{ARITY}]).")));
        assert!(src.contains(&format!("{FUNCTION}(N, build({LEN}, []), 0).")));
        assert!(
            src.contains(&format!("N * {PER_SUM}")),
            "the doc comment pins the sum"
        );
        // BIF-free: no call into any other module at all (so no `lists:seq/2`).
        assert!(
            !src.contains("lists:"),
            "the module must not call the `lists` module"
        );
        assert!(!src.contains("-import"), "the module must import nothing");
        for line in src.lines().filter(|l| !l.trim_start().starts_with('%')) {
            assert!(
                !line.contains("erlang:"),
                "the walk must not call a BIF directly: {line}"
            );
        }
    }

    #[test]
    fn stage_reports_a_write_failure_instead_of_silently_failing() {
        let missing = std::env::temp_dir().join(format!(
            "bench-list-walk-absent-{}-xyzzy",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&missing);
        let err = stage(&missing, Path::new("/nonexistent/erlc"))
            .expect_err("a missing dir must be reported");
        assert!(err.contains("write"), "{err}");
    }

    /// Oracle-gated smoke test: with `erlc` and a resolvable runtime present, the case
    /// must produce real timings (or an honest note). No-op when prerequisites are
    /// missing.
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
            let scratch = ScratchDir::new("bench-list-walk-smoke").expect("scratch");
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
                    "runtime {:?}: the walk must print the deterministic constant",
                    runtime.kind
                );
            }
        }
    }
}
