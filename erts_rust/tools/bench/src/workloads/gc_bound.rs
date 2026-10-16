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
//! # The measurement (startup-cancelled, per op)
//!
//! The two runtimes differ by more than an order of magnitude per op, so one fixed loop
//! count cannot give both a measurable marginal: an `N` that is a real signal for the Rust
//! runtime is below the oracle's own run-to-run startup noise. The program therefore
//! declares a **per-runtime loop count** `N` and the workload runs the identical program
//! twice per sample:
//!
//! * at `N` churns ⇒ `case_churn_wall_ms` (end-to-end process wall time),
//! * at `0` churns ⇒ `case_churn_startup_ms` (the same program's pure startup/boot cost —
//!   the Rust runtime loads its preloaded-module set and resolves its system code path with
//!   a native filesystem probe (`crates/beam_rt/src/codeserver.rs`; the system `erl` is only
//!   *run* as a last resort), so this is not zero),
//!
//! and derives the throughput metric
//!
//! ```text
//! case_churn_ns_per_op = (mean(wall_ms) - mean(startup_ms)) / N * 1e6
//! ```
//!
//! which cancels the fixed process-startup cost from both sides. It is emitted only when
//! the marginal is positive; otherwise a note records that the marginal is below timer
//! resolution — never a fabricated or negative number.
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

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc::{self, Completed};
use crate::report::{Profile, WorkloadResult};
use crate::runtime::RuntimeKind;
use crate::scratch::ScratchDir;
use crate::stats::mean;
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
/// work** (≈5× that jitter); measured rust churn cost on that host (2026-10-16,
/// `/proc/loadavg` 45–100, min-of-reps) is ≈130–180 µs/churn, so `n_rust = 5_000` (raised
/// from 2 000 by the m14 recalibration, whose ≈0.26 s marginal sat inside the band) is
/// ≈0.7–1.6 s. The oracle's much smaller startup jitter (tens of ms) needs a correspondingly
/// smaller marginal.
///
/// The reported metric is normalized per op, so the loop counts only have to keep both
/// marginals measurable — not to equalize the two runtimes' totals. A loop count whose
/// marginal falls below the timer noise yields **no** per-op metric (an honest note), never a
/// fabricated one.
static CASE: Case = Case {
    name: "churn",
    n_rust: 5_000,
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

/// The samples collected under one runtime.
struct Outcome {
    /// The loop count used.
    n: i64,
    /// Wall-clock samples (ms) of the `n`-churn runs.
    wall: Vec<f64>,
    /// Wall-clock samples (ms) of the `0`-churn (startup-only) runs.
    startup: Vec<f64>,
    /// Peak-RSS samples (bytes) of the `n`-churn runs.
    rss: Vec<f64>,
    /// The stdout observed on the first successful `n`-churn run.
    observed: Option<String>,
    /// Whether the runtime reported the case as `unsupported` (Rust exit `3`).
    unsupported: bool,
    /// Notes about failures/variations (never silent).
    notes: Vec<String>,
}

impl Outcome {
    /// Whether the case produced at least one usable timing.
    fn ok(&self) -> bool {
        !self.wall.is_empty()
    }
}

/// The startup-cancelled marginal cost per churn, in nanoseconds — `None` when either
/// sample set is unusable or the marginal is not positive (below timer resolution).
fn ns_per_op(wall: &[f64], startup: &[f64], n: i64) -> Option<f64> {
    if wall.is_empty() || startup.is_empty() || n <= 0 {
        return None;
    }
    let marginal = mean(wall) - mean(startup);
    if marginal <= 0.0 {
        None
    } else {
        Some(marginal / n as f64 * 1_000_000.0)
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

/// Runs the program at `N` churns and at `0` churns, `runs` times each, collecting samples.
fn execute(program: &Path, kind: RuntimeKind, dir: &Path, runs: usize) -> Outcome {
    let n = CASE.n_for(kind);
    let mut outcome = Outcome {
        n,
        wall: Vec::new(),
        startup: Vec::new(),
        rss: Vec::new(),
        observed: None,
        unsupported: false,
        notes: Vec::new(),
    };
    let expected = expected_stdout(n);

    // Pass 1: the `N`-churn runs (the measured work).
    for _ in 0..runs {
        let mut cmd = Command::new(program);
        cmd.args(argv_for(kind, dir, n));
        match proc::run(&mut cmd) {
            Ok(completed) if completed.success => {
                outcome.wall.push(completed.wall_ms);
                if let Some(bytes) = completed.peak_memory_bytes {
                    outcome.rss.push(bytes as f64);
                }
                let got = completed.stdout_trim().to_string();
                if got != expected {
                    outcome.notes.push(format!(
                        "case {}: unexpected stdout `{got}` (expected `{expected}`)",
                        CASE.name
                    ));
                }
                if outcome.observed.is_none() {
                    outcome.observed = Some(got);
                }
            }
            Ok(completed) => {
                if kind == RuntimeKind::Rust
                    && completed.exit_code == Some(3)
                    && outcome.wall.is_empty()
                {
                    outcome.unsupported = true;
                }
                outcome.notes.push(classify_failure(kind, &completed));
                break;
            }
            Err(e) => {
                outcome.notes.push(format!(
                    "case {} ({}): spawn failed: {e}",
                    CASE.name,
                    kind.as_str()
                ));
                break;
            }
        }
    }

    // Pass 2: the `0`-churn runs (startup only). Skipped when pass 1 already failed — a
    // broken case must not be timed at all.
    if outcome.ok() {
        let expected_startup = expected_stdout(0);
        for _ in 0..runs {
            let mut cmd = Command::new(program);
            cmd.args(argv_for(kind, dir, 0));
            match proc::run(&mut cmd) {
                Ok(completed) if completed.success => {
                    outcome.startup.push(completed.wall_ms);
                    let got = completed.stdout_trim().to_string();
                    if got != expected_startup {
                        outcome.notes.push(format!(
                            "case {}: the startup run printed `{got}` (expected `{expected_startup}`)",
                            CASE.name
                        ));
                    }
                }
                Ok(completed) => {
                    outcome.notes.push(format!(
                        "case {}: startup run failed: {}",
                        CASE.name,
                        classify_failure(kind, &completed)
                    ));
                    break;
                }
                Err(e) => {
                    outcome
                        .notes
                        .push(format!("case {}: startup spawn failed: {e}", CASE.name));
                    break;
                }
            }
        }
    }

    outcome
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
            result.metrics.push(Metric::samples(
                format!("case_{}_wall_ms", CASE.name),
                "ms",
                &outcome.wall,
            ));
            if !outcome.startup.is_empty() {
                result.metrics.push(Metric::samples(
                    format!("case_{}_startup_ms", CASE.name),
                    "ms",
                    &outcome.startup,
                ));
            }
            if !outcome.rss.is_empty() {
                result.metrics.push(Metric::samples(
                    format!("case_{}_peak_rss_bytes", CASE.name),
                    "bytes",
                    &outcome.rss,
                ));
                let peak = outcome.rss.iter().fold(0.0_f64, |a, &b| a.max(b));
                result.peak_memory_bytes = Some(peak as u64);
            }
            match ns_per_op(&outcome.wall, &outcome.startup, outcome.n) {
                Some(ns) => result.metrics.push(Metric::scalar(
                    format!("case_{}_ns_per_op", CASE.name),
                    "ns",
                    ns,
                )),
                None => result.notes.push(format!(
                    "case {}: marginal below timer resolution; no ns_per_op metric",
                    CASE.name
                )),
            }
            // The deterministic, contention-immune companion metric: how many
            // allocate-and-abandon iterations the timing above was derived from.
            result.metrics.push(Metric::scalar(
                format!("case_{}_ops", CASE.name),
                "ops",
                ops(outcome.n) as f64,
            ));

            if let Some(value) = &outcome.observed {
                result.notes.push(format!(
                    "observed stdout: {}=N*{PER_OP}={value}",
                    CASE.name
                ));
            }
            result.notes.push(format!(
                "case {}: N={} churns ({}), each allocating and abandoning a {LEN}-element \
                 list, a {LEN}-byte binary and a {LEN}-element tuple",
                CASE.name,
                outcome.n,
                kind.as_str()
            ));
            result.notes.push(
                "method: ns/op = (mean wall at N churns - mean wall at 0 churns) / N * 1e6; N \
                 is per runtime so both marginals are measurable, and the fixed process \
                 startup (incl. the Rust runtime's code-path resolution) cancels out of both \
                 sides"
                    .to_string(),
            );
            result.notes.push(
                "note: the op count is deterministic and contention-immune; the wall-clock \
                 rows move with host load. Whether a runtime's collector actually runs during \
                 the loop is its own admission policy — this row measures the \
                 allocate-and-abandon wall time, and claims no collection count"
                    .to_string(),
            );
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

    #[test]
    fn ns_per_op_cancels_startup_and_rejects_unusable_samples() {
        // 100 ms at N=1000 churns, 10 ms at 0 ⇒ 90 ms of churn over 1000 ops ⇒ 90 µs/op.
        assert_eq!(ns_per_op(&[100.0], &[10.0], 1_000), Some(90_000.0));
        // Equal or inverted (noise) ⇒ no metric, never a negative number.
        assert_eq!(ns_per_op(&[100.0], &[100.0], 1_000), None);
        assert_eq!(ns_per_op(&[90.0], &[100.0], 1_000), None);
        // Missing sample sets / a non-positive count ⇒ no metric.
        assert_eq!(ns_per_op(&[], &[1.0], 1_000), None);
        assert_eq!(ns_per_op(&[1.0], &[], 1_000), None);
        assert_eq!(ns_per_op(&[1.0], &[0.0], -1), None);
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
