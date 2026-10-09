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
//! # The measurement (startup-cancelled, per element)
//!
//! The two runtimes differ by orders of magnitude, so one fixed loop count cannot give
//! both a measurable marginal: the same `N` that costs the Rust runtime hundreds of
//! milliseconds costs the oracle a few milliseconds (i.e. below its own startup noise).
//! The program therefore declares a **per-runtime loop count** `N` and is run twice per
//! sample:
//!
//! * at `N` walks ⇒ `case_list_walk_wall_ms` (end-to-end process wall time),
//! * at `0` walks ⇒ `case_list_walk_startup_ms` (the same program's pure startup/boot
//!   cost — the Rust runtime resolves its system code path by spawning `erl` once, so
//!   this is far from zero),
//!
//! and derives the throughput metric
//!
//! ```text
//! case_list_walk_ns_per_element = (mean(wall_ms) - mean(startup_ms)) / (N * LEN) * 1e6
//! ```
//!
//! which cancels the fixed process-startup cost from both sides. It is emitted only when
//! the marginal is positive; otherwise a note records that the marginal is below timer
//! resolution — never a fabricated or negative number.
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

use crate::metrics::{Category, Metric, Probe, Runtimes, Status};
use crate::proc::{self, Completed};
use crate::report::{Profile, WorkloadResult};
use crate::runtime::RuntimeKind;
use crate::scratch::ScratchDir;
use crate::stats::mean;
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
/// (a heavily loaded shared machine — see the crate CONTEXT) so that a single run's
/// *marginal* cost stays a few hundred milliseconds on both sides. Measured per-element
/// cost on that host, as `/proc/loadavg` moved from ~80 to ~270:
///
/// * Rust: ~250–1200 ns/element ⇒ `n_rust = 1_000` walks (10^6 elements) is 0.25–1.2 s;
/// * oracle: ~3–12 ns/element ⇒ `n_sys = 100_000` walks (10^8 elements) is 0.3–1.2 s,
///   i.e. far above its run-to-run startup noise (tens of ms).
///
/// The reported metric is normalized per element, so the loop counts only have to keep
/// both marginals measurable — not to equalize the two runtimes' totals.
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
static CASE: Case = Case {
    name: "list_walk",
    n_rust: 1_000,
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

/// The samples collected under one runtime.
struct Outcome {
    /// The loop count used.
    n: i64,
    /// Wall-clock samples (ms) of the `n`-walk runs.
    wall: Vec<f64>,
    /// Wall-clock samples (ms) of the `0`-walk (startup-only) runs.
    startup: Vec<f64>,
    /// Peak-RSS samples (bytes) of the `n`-walk runs.
    rss: Vec<f64>,
    /// The stdout observed on the first successful `n`-walk run.
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

/// The startup-cancelled marginal cost per walked element, in nanoseconds — `None` when
/// either sample set is unusable or the marginal is not positive (below timer
/// resolution).
fn ns_per_element(wall: &[f64], startup: &[f64], n: i64) -> Option<f64> {
    if wall.is_empty() || startup.is_empty() || n <= 0 {
        return None;
    }
    let marginal = mean(wall) - mean(startup);
    if marginal <= 0.0 {
        None
    } else {
        Some(marginal / elements(n) as f64 * 1_000_000.0)
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

/// Runs the program at `N` walks and at `0` walks, `runs` times each, collecting samples.
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

    // Pass 1: the `N`-walk runs (the measured work).
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

    // Pass 2: the `0`-walk runs (startup only). Skipped when pass 1 already failed — a
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
            match ns_per_element(&outcome.wall, &outcome.startup, outcome.n) {
                Some(ns) => result.metrics.push(Metric::scalar(
                    format!("case_{}_ns_per_element", CASE.name),
                    "ns",
                    ns,
                )),
                None => result.notes.push(format!(
                    "case {}: marginal below timer resolution; no ns_per_element metric",
                    CASE.name
                )),
            }
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
                "method: ns/element = (mean wall at N walks - mean wall at 0 walks) \
                 / (N * LEN) * 1e6; N is per runtime so both marginals are measurable, and \
                 the fixed process startup (incl. the Rust runtime's code-path resolution) \
                 cancels out of both sides; the module is BIF-free and builds no term"
                    .to_string(),
            );
            result.notes.push(
                "note: element count is deterministic and contention-immune; the wall-clock \
                 rows move with host load"
                    .to_string(),
            );
        } else {
            result.notes.extend(outcome.notes);
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
    fn ns_per_element_cancels_startup_and_rejects_unusable_samples() {
        // 100 ms at N=1000 walks, 10 ms at 0 ⇒ 90 ms of walking over 10^6 elements
        // ⇒ 90 ns/element.
        assert_eq!(ns_per_element(&[100.0], &[10.0], 1_000), Some(90.0));
        // Equal or inverted (noise) ⇒ no metric, never a negative number.
        assert_eq!(ns_per_element(&[100.0], &[100.0], 1_000), None);
        assert_eq!(ns_per_element(&[90.0], &[100.0], 1_000), None);
        // Missing sample sets / a non-positive count ⇒ no metric.
        assert_eq!(ns_per_element(&[], &[1.0], 1_000), None);
        assert_eq!(ns_per_element(&[1.0], &[], 1_000), None);
        assert_eq!(ns_per_element(&[1.0], &[0.0], 0), None);
        assert_eq!(ns_per_element(&[1.0], &[0.0], -1), None);
    }

    #[test]
    fn the_element_count_is_the_deterministic_product() {
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
