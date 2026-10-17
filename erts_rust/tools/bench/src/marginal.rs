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

//! The **paired, drift-cancelling** startup-cancelled marginal measurement.
//!
//! Every per-unit metric the driver publishes (`case_<name>_ns_per_op`,
//! `case_<name>_ns_per_element`, `case_<name>_ns_per_call`, …) is a *marginal*: the
//! program is run twice, once doing `N` units of work and once doing `0` units, and the
//! fixed process-startup cost — which is **not** zero on either runtime (`beam_rt` loads
//! its preloaded-module set and resolves its system code path) — is assumed to cancel in
//! the difference:
//!
//! ```text
//! unit_cost_ns = (wall at N units - wall at 0 units) / N * 1e6
//! ```
//!
//! # The drift problem (why the obvious schedule is wrong)
//!
//! The original schedule ran **all `N`-unit samples first, then all `0`-unit samples**,
//! and formed the figure from `mean(all work) - mean(all startup)`. That is only valid if
//! the process-startup cost is *constant* over the whole measurement. On this benchmark
//! host it is not: the host is shared and its load drifts, so boot cost moves by tens — at
//! times hundreds — of milliseconds between the two passes. The mean-based difference then
//! absorbs that drift **instead of cancelling it**, and the only guard was that a
//! *negative* marginal was refused. A silently inflated figure was still published: a
//! reported `136×` `etf_roundtrip` gap where an interleaved min-of-5 measurement gives
//! `2.42×` is exactly this failure mode.
//!
//! # The fix — four rules, all implemented here
//!
//! 1. **Interleave the two passes within every round.** [`sample_pairs`] runs the `Work`
//!    pass and the `Startup` pass *back to back* (`runs` rounds, `Work` then `Startup`
//!    each round), so a drifting boot cost hits both members of a **pair** almost
//!    equally. Drift across the whole measurement is then a per-pair residual of at most
//!    one sampling interval, not a bias of `runs` intervals.
//! 2. **Form the figure from paired differences, never from two separate means.**
//!    [`resolve`] works on `deltas[i] = work_ms[i] - startup_ms[i]` — one delta per round
//!    — so the boot cost of round `i` cancels against *its own* baseline run.
//! 3. **Use robust statistics.** The point estimate is the **median** of the deltas (one
//!    contended round cannot move it, whereas it drags a mean), and the spread is the
//!    **MAD** scaled to a σ-equivalent (`1.4826 × MAD`), floored at [`MIN_NOISE_MS`].
//! 4. **Never publish a figure inside the noise band.** The median delta must be at least
//!    [`NOISE_MULTIPLE`] × the noise estimate, otherwise [`Marginal::unit_cost_ns`] is
//!    `None` and the caller records [`Marginal::below_resolution_note`] — no number rather
//!    than a drift artifact. A negative or non-finite marginal is never published either.
//!
//! **Do not revert to the sequential schedule.** It is not a stylistic choice: on a loaded
//! host it publishes a figure whose error is dominated by how busy the host was while the
//! two passes ran. New per-unit workloads must take their samples through
//! [`sample_pairs`] and resolve them with [`resolve`].
//!
//! # Example
//!
//! ```
//! use bench::marginal::{resolve, sample_pairs, Pass, PassError};
//!
//! // A fake runtime whose 0-unit run costs 100 ms and whose 25 000-unit run costs 25 ms
//! // more, with the host drifting by 40 ms every round — both passes of a round see the
//! // same drift, exactly as the interleaved schedule intends.
//! let mut round = 0;
//! let runs = sample_pairs(5, |pass| {
//!     let work_ms = if pass == Pass::Work { 25.0 } else { 0.0 };
//!     let wall = 100.0 + 40.0 * round as f64 + work_ms;
//!     if pass == Pass::Startup {
//!         round += 1;
//!     }
//!     Ok::<f64, PassError>(wall)
//! });
//! let marginal = resolve(&runs.pairs, 25_000);
//! // 25 ms over 25 000 units = 1000 ns per unit, despite the drift.
//! let ns = marginal.unit_cost_ns().expect("clearly above the noise band");
//! assert!((ns - 1_000.0).abs() < 1e-9, "{ns}");
//! ```

use crate::stats::percentile;

/// The smallest noise band, in milliseconds, the median delta must clear.
///
/// A perfectly quiet host yields a MAD of exactly `0`, which would otherwise accept a
/// sub-millisecond "marginal" as real. One millisecond is above the resolution of the
/// wall-clock figures the driver records (`f64` milliseconds from `Instant`), so nothing
/// below it is trustworthy.
pub const MIN_NOISE_MS: f64 = 1.0;

/// How many noise bands the median delta must clear, i.e. the publication threshold.
///
/// `3 ×` a σ-equivalent is the classical "clearly outside the noise" band: a delta smaller
/// than this is as likely to be load jitter as work.
pub const NOISE_MULTIPLE: f64 = 3.0;

/// Scale factor from the median absolute deviation to a standard-deviation equivalent.
///
/// For normally distributed data `σ ≈ 1.4826 × MAD`; unlike the standard deviation the MAD
/// is not inflated by a single outlier, which is what a contended round is.
const MAD_TO_SIGMA: f64 = 1.4826;

/// Which pass of a measurement round is being sampled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    /// The measured pass: the program is run with `N` units of work.
    Work,
    /// The baseline pass: the same program with `0` units, timed for its startup cost.
    Startup,
}

impl Pass {
    /// The loop count to pass to the program: `1` for [`Pass::Work`], `0` for
    /// [`Pass::Startup`].
    ///
    /// The workload multiplies this by the case's `N` to build the argv of the pass (the
    /// program itself multiplies its result by the count, so run-at-`0` prints `0`).
    pub fn units(self) -> i64 {
        match self {
            Pass::Work => 1,
            Pass::Startup => 0,
        }
    }

    /// `"work"` / `"startup"` — the spelling used in notes and abort reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Pass::Work => "work",
            Pass::Startup => "startup",
        }
    }
}

/// A failure reported by a sampling closure.
///
/// The closure has already classified the failure (a compile/run error, a capability gap,
/// a stdout mismatch, …) and owns the wording; this type only carries that note plus the
/// one classification the marginal measurement itself must honour: whether it was a
/// **capability gap** (the Rust runtime exits `3`, `unsupported: …`) rather than a real
/// failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassError {
    /// Human-readable, already-classified note (the caller owns the wording).
    pub note: String,
    /// Whether this was a Rust-runtime capability gap (exit `3`), not a real failure.
    pub unsupported: bool,
}

/// The samples collected from one interleaved measurement, plus the abort (if any).
#[derive(Debug, Clone)]
pub struct PairedRuns<T> {
    /// One `(work, startup)` sample pair per completed round, in round order.
    pub pairs: Vec<(T, T)>,
    /// The first failure that aborted the measurement: its note, whether it was a
    /// capability gap, and the pass it happened on.
    ///
    /// `None` when every requested round completed. When it is `Some`, [`Self::pairs`]
    /// still holds every round that completed *before* the failing pass, so a case with
    /// usable samples can still be reported.
    pub aborted: Option<Abort>,
}

/// Where and why a measurement stopped early.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Abort {
    /// The pass that failed.
    pub pass: Pass,
    /// The note the sampling closure attached to the failure.
    pub note: String,
    /// Whether the failure was a Rust-runtime capability gap rather than a real one.
    pub unsupported: bool,
}

/// Samples `runs` rounds, each round running the `Work` pass then the `Startup` pass
/// back to back (interleaved so a drifting boot cost cancels inside every pair).
///
/// This is the *only* sanctioned sampling schedule for a per-unit metric — see the module
/// docs for the regression the interleaving prevents.
///
/// A failure aborts the measurement; the pairs completed before it are retained so a case
/// can still be reported when it has usable samples. Aborting is deliberate: a partially
/// failed measurement is not a reason to burn further runtime, and the caller decides from
/// the returned pairs/abort whether it has enough to publish.
pub fn sample_pairs<T, F>(runs: usize, mut sample: F) -> PairedRuns<T>
where
    F: FnMut(Pass) -> Result<T, PassError>,
{
    let mut pairs = Vec::with_capacity(runs);
    for _ in 0..runs {
        let work = match sample(Pass::Work) {
            Ok(sample) => sample,
            Err(err) => return aborted(pairs, Pass::Work, err),
        };
        let startup = match sample(Pass::Startup) {
            Ok(sample) => sample,
            Err(err) => return aborted(pairs, Pass::Startup, err),
        };
        pairs.push((work, startup));
    }
    PairedRuns {
        pairs,
        aborted: None,
    }
}

/// Assembles the early-return value of [`sample_pairs`].
fn aborted<T>(pairs: Vec<(T, T)>, pass: Pass, err: PassError) -> PairedRuns<T> {
    PairedRuns {
        pairs,
        aborted: Some(Abort {
            pass,
            note: err.note,
            unsupported: err.unsupported,
        }),
    }
}

/// The resolved startup-cancelled marginal.
///
/// Every field is finite for every input: an empty, degenerate or non-finite sample set
/// yields `unit_cost_ns: None` with zeroed statistics rather than a `NaN`/`inf` that would
/// poison the report JSON. `unit_cost_ns` is `None` exactly when the marginal is not
/// clearly above the measured noise band (or when there is no usable input at all) — never
/// a fabricated or negative number.
#[derive(Debug, Clone, PartialEq)]
pub struct Marginal {
    /// The per-unit cost in nanoseconds, `None` when the marginal is not clearly above
    /// the noise band (never a fabricated or negative number).
    pub unit_cost_ns: Option<f64>,
    /// Median paired difference (ms) — the point estimate.
    pub median_delta_ms: f64,
    /// Smallest paired difference (ms).
    pub min_delta_ms: f64,
    /// Largest paired difference (ms).
    pub max_delta_ms: f64,
    /// The estimated noise band (ms) the median had to clear.
    pub noise_ms: f64,
    /// Number of paired rounds behind the figure.
    pub rounds: usize,
}

impl Marginal {
    /// The published per-unit cost in nanoseconds — the same value as the `unit_cost_ns`
    /// field, provided as an accessor so a caller may use either form.
    ///
    /// `Some(ns)` only for a published figure; `None` when the marginal is inside the
    /// noise band (see [`Self::below_resolution_note`]).
    pub fn unit_cost_ns(&self) -> Option<f64> {
        self.unit_cost_ns
    }

    /// A one-line, auditable note describing the figure behind the number:
    /// `paired median=… ms min=… ms max=… ms noise=±… ms over k round(s)`.
    ///
    /// Always emitted, so a published `ns`-per-unit figure can always be traced back to
    /// its deltas and the noise it had to clear.
    pub fn detail(&self) -> String {
        format!(
            "paired median={:.3} ms min={:.3} ms max={:.3} ms noise=±{:.3} ms over {} round(s)",
            self.median_delta_ms, self.min_delta_ms, self.max_delta_ms, self.noise_ms, self.rounds
        )
    }

    /// The note to record when [`Self::unit_cost_ns`] is `None` — *why* no figure was
    /// published, with the full [`Self::detail`] behind the decision.
    pub fn below_resolution_note(&self) -> String {
        format!("marginal below timer resolution: {}", self.detail())
    }
}

/// Resolves `(work_ms, startup_ms)` pairs into a per-unit marginal over `units` units.
///
/// Resolution rules (see the module docs for the reasoning):
///
/// * `deltas[i] = work_ms[i] - startup_ms[i]`; `rounds = deltas.len()`. Non-finite deltas
///   are dropped (they can only come from a broken sample; they must never reach the
///   report as `NaN`/`inf`).
/// * The point estimate is the **median** of the deltas, with the min/max recorded
///   alongside it.
/// * The noise band is `max(MIN_NOISE_MS, 1.4826 × MAD(deltas))` — the median absolute
///   deviation scaled to a σ-equivalent and floored at [`MIN_NOISE_MS`], so a perfectly
///   quiet host cannot accept a sub-millisecond "marginal".
/// * The figure is published (`unit_cost_ns = median / units × 1e6`) only when
///   `units > 0`, `rounds > 0`, the median delta is positive **and** it is at least
///   [`NOISE_MULTIPLE`] × the noise band. Otherwise `unit_cost_ns` is `None` and the
///   caller records [`Marginal::below_resolution_note`].
pub fn resolve(pairs: &[(f64, f64)], units: i64) -> Marginal {
    let mut deltas: Vec<f64> = pairs
        .iter()
        .map(|(work_ms, startup_ms)| work_ms - startup_ms)
        .filter(|delta| delta.is_finite())
        .collect();
    if deltas.is_empty() {
        // Degenerate input: report the (finite) noise floor and no figure.
        return Marginal {
            unit_cost_ns: None,
            median_delta_ms: 0.0,
            min_delta_ms: 0.0,
            max_delta_ms: 0.0,
            noise_ms: MIN_NOISE_MS,
            rounds: 0,
        };
    }
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rounds = deltas.len();
    let median_delta_ms = median(&deltas);
    let min_delta_ms = deltas[0];
    let max_delta_ms = deltas[rounds - 1];
    let mut deviations: Vec<f64> = deltas
        .iter()
        .map(|delta| (delta - median_delta_ms).abs())
        .collect();
    deviations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let noise_ms = (MAD_TO_SIGMA * median(&deviations)).max(MIN_NOISE_MS);
    let candidate = median_delta_ms / units as f64 * 1e6;
    let publishes = units > 0
        && median_delta_ms > 0.0
        && median_delta_ms >= NOISE_MULTIPLE * noise_ms
        && candidate.is_finite();
    Marginal {
        unit_cost_ns: if publishes { Some(candidate) } else { None },
        median_delta_ms,
        min_delta_ms,
        max_delta_ms,
        noise_ms,
        rounds,
    }
}

/// The median of an **ascending-sorted** slice (the interpolated 50th percentile, so an
/// even count averages its two middle values; `0.0` for an empty slice).
fn median(sorted: &[f64]) -> f64 {
    percentile(sorted, 50.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::mean;

    /// The fake clock's fixed boot cost, in ms (the right size for a booted `beam_rt`).
    const TRUE_STARTUP_MS: f64 = 1_100.0;
    /// How far the fake clock's boot cost moves per pass-sample interval, in ms.
    const DRIFT_STEP_MS: f64 = 200.0;
    /// How far the boot cost moves per sample under the harshest *smooth* ramp (ms).
    const SMOOTH_DRIFT_MS: f64 = 20.0;
    /// Rounds per fake measurement.
    const RUNS: usize = 5;
    /// Loop count of the fake program's work pass.
    const N: i64 = 200_000;
    /// The fake program's true per-op cost, in ns.
    const TRUE_NS_PER_OP: f64 = 1_000.0;
    /// The fake program's true work-pass cost, in ms (`N × 1000 ns` = 200 ms).
    const TRUE_WORK_MS: f64 = N as f64 * TRUE_NS_PER_OP / 1e6;
    /// A one-off load step of `RUNS` drift steps, i.e. everything the old schedule lets
    /// accumulate between its two passes.
    const LOAD_STEP_MS: f64 = RUNS as f64 * DRIFT_STEP_MS;

    /// A fake clock modelling a host whose process-startup cost drifts while a measurement
    /// runs. Every sample costs `TRUE_STARTUP_MS + drift(sample_index) + work`, where
    /// `drift(i) = per_sample_ms × i`, plus a one-off step of `step_ms` from sample `step_at`
    /// onwards. The sample index is the *global* position in the measurement, so the two
    /// schedules order the very same samples differently:
    ///
    /// * interleaved — indices `0…2×RUNS-1`, `Work` then `Startup` inside each round;
    /// * sequential — indices `0…RUNS-1` are all `Work`, then `RUNS…2×RUNS-1` all `Startup`.
    struct FakeClock {
        per_sample_ms: f64,
        step_at: usize,
        step_ms: f64,
        sample_index: usize,
    }

    impl FakeClock {
        fn new(per_sample_ms: f64, step_at: usize, step_ms: f64) -> FakeClock {
            FakeClock {
                per_sample_ms,
                step_at,
                step_ms,
                sample_index: 0,
            }
        }

        /// The host quiets by one full pass-worth of drift (`LOAD_STEP_MS`) exactly where
        /// the old schedule switches from its work pass to its startup pass.
        fn load_step() -> FakeClock {
            FakeClock::new(0.0, RUNS, -LOAD_STEP_MS)
        }

        /// The harshest smooth ramp: the boot cost moves on *every* sample.
        fn smooth_ramp() -> FakeClock {
            FakeClock::new(-SMOOTH_DRIFT_MS, usize::MAX, 0.0)
        }

        /// The wall time (ms) of one sample of `pass`.
        fn sample(&mut self, pass: Pass) -> f64 {
            let index = self.sample_index;
            self.sample_index += 1;
            let step = if index >= self.step_at {
                self.step_ms
            } else {
                0.0
            };
            let drift = self.per_sample_ms * index as f64 + step;
            let work_ms = match pass {
                Pass::Work => TRUE_WORK_MS,
                Pass::Startup => 0.0,
            };
            TRUE_STARTUP_MS + drift + work_ms
        }
    }

    /// The interleaved measurement over a fake clock.
    fn paired_marginal(mut clock: FakeClock) -> Marginal {
        let runs = sample_pairs(RUNS, |pass| Ok::<f64, PassError>(clock.sample(pass)));
        assert!(runs.aborted.is_none(), "the fake clock never fails");
        assert_eq!(runs.pairs.len(), RUNS);
        resolve(&runs.pairs, N)
    }

    /// The **old** schedule over the same fake clock: all work samples first, then all
    /// startup samples, and the marginal as `mean(work) - mean(startup)` — exactly what
    /// `copy_bound`/`list_walk`/`gc_bound`/`foreign` did before this module existed.
    fn sequential_marginal_ms(mut clock: FakeClock) -> f64 {
        let work: Vec<f64> = (0..RUNS).map(|_| clock.sample(Pass::Work)).collect();
        let startup: Vec<f64> = (0..RUNS).map(|_| clock.sample(Pass::Startup)).collect();
        mean(&work) - mean(&startup)
    }

    /// Asserts every published field is finite — a `NaN`/`inf` would poison the report.
    fn assert_finite(m: &Marginal) {
        for (name, value) in [
            ("median_delta_ms", m.median_delta_ms),
            ("min_delta_ms", m.min_delta_ms),
            ("max_delta_ms", m.max_delta_ms),
            ("noise_ms", m.noise_ms),
        ] {
            assert!(value.is_finite(), "{name} = {value}");
        }
        if let Some(ns) = m.unit_cost_ns() {
            assert!(ns.is_finite(), "unit_cost_ns = {ns}");
        }
    }

    /// **The pinning test.** Interleaving must cancel a drifting boot cost; the sequential
    /// schedule the four workloads used to run must not.
    #[test]
    fn interleaving_cancels_a_drifting_boot_cost() {
        // (a) A single load step: the host quiets by `LOAD_STEP_MS` = 1000 ms exactly where
        // the old schedule switches from its work pass to its startup pass. Interleaved,
        // that step lands *inside* one round — an outlier pair that the median ignores —
        // and both members of every other pair share the same boot cost, so the true
        // per-op cost is recovered exactly.
        let paired = paired_marginal(FakeClock::load_step());
        assert_eq!(paired.rounds, RUNS);
        assert_eq!(paired.median_delta_ms, TRUE_WORK_MS);
        let got = paired
            .unit_cost_ns()
            .expect("a clean paired marginal must publish");
        assert!(
            (got - TRUE_NS_PER_OP).abs() / TRUE_NS_PER_OP < 1e-9,
            "paired {got} ns/op vs true {TRUE_NS_PER_OP}: {}",
            paired.detail()
        );
        // The straddling round is visible in the extrema — it just does not move the estimate.
        assert_eq!(paired.min_delta_ms, TRUE_WORK_MS);
        assert_eq!(paired.max_delta_ms, TRUE_WORK_MS + LOAD_STEP_MS);

        // (b) The old schedule over the SAME fake clock puts the whole step *between* its
        // two passes, so the bias lands wholly in the marginal: it reports
        // `TRUE_WORK_MS + LOAD_STEP_MS` = 1200 ms of "work" where the work pass costs
        // 200 ms — a ~6× inflated per-op figure. That is the regression this module
        // prevents (the published 136× `etf_roundtrip` gap was this artifact).
        let sequential_ms = sequential_marginal_ms(FakeClock::load_step());
        let excess_ms = sequential_ms - TRUE_WORK_MS;
        assert!(
            (excess_ms - LOAD_STEP_MS).abs() < 1e-9,
            "the sequential bias must be RUNS × drift_step ({LOAD_STEP_MS} ms), got {excess_ms} ms"
        );
        let inflation = sequential_ms / TRUE_WORK_MS;
        assert!(inflation > 5.0, "sequential inflation {inflation}×");
        let sequential_ns = sequential_ms / N as f64 * 1e6;
        assert!(
            sequential_ns > 5.0 * got,
            "sequential {sequential_ns} ns/op vs paired {got} ns/op"
        );

        // (c) The harshest model — a smooth ramp where the boot cost moves on *every*
        // sample — still leaves a pair with a residual of exactly one sampling interval,
        // while the old schedule absorbs `RUNS` of them: an `RUNS`-fold improvement, and
        // the reason the guard compares against the *paired* noise band, not a raw mean.
        let ramp = paired_marginal(FakeClock::smooth_ramp());
        let ramp_ns = ramp.unit_cost_ns().expect("the ramp model still publishes");
        let paired_error_ms = (ramp_ns - TRUE_NS_PER_OP).abs() * N as f64 / 1e6;
        assert!(
            (paired_error_ms - SMOOTH_DRIFT_MS).abs() < 1e-9,
            "paired residual {paired_error_ms} ms must be one sampling interval"
        );
        let sequential_error_ms =
            (sequential_marginal_ms(FakeClock::smooth_ramp()) - TRUE_WORK_MS).abs();
        assert!(
            (sequential_error_ms - RUNS as f64 * SMOOTH_DRIFT_MS).abs() < 1e-9,
            "sequential error {sequential_error_ms} ms must be RUNS sampling intervals"
        );
        assert!(
            paired_error_ms * RUNS as f64 <= sequential_error_ms + 1e-9,
            "the paired residual must be RUNS-fold smaller: {paired_error_ms} vs \
             {sequential_error_ms} ms"
        );
    }

    /// One contended round must not move the published figure.
    #[test]
    fn median_ignores_one_contended_round() {
        // Seven rounds of 10 ms of work, except round 3, which was contended and cost 300 ms.
        let deltas_ms = [10.0, 10.0, 10.0, 300.0, 10.0, 10.0, 10.0];
        let pairs = |deltas: &[f64]| -> Vec<(f64, f64)> {
            deltas.iter().map(|delta| (100.0 + delta, 100.0)).collect()
        };
        let clean = resolve(&pairs(&[10.0; 7]), 1_000);
        let contended = resolve(&pairs(&deltas_ms), 1_000);
        assert_eq!(clean.unit_cost_ns(), Some(10_000.0));
        let got = contended
            .unit_cost_ns()
            .expect("one bad round must not suppress the figure");
        assert!(
            (got - 10_000.0).abs() < 1e-9,
            "median moved to {got} ns/op: {}",
            contended.detail()
        );
        assert_eq!(contended.median_delta_ms, 10.0);
        // ... while the extrema still expose it, and the old mean-based estimate would
        // have been dragged ~5× high by it.
        assert_eq!(contended.min_delta_ms, 10.0);
        assert_eq!(contended.max_delta_ms, 300.0);
        assert!(mean(&deltas_ms) > 3.0 * contended.median_delta_ms);
        assert_finite(&contended);
    }

    /// A scattered, noisy measurement whose median sits inside the noise band publishes
    /// nothing and says why.
    #[test]
    fn noise_band_rejects_a_marginal_inside_it() {
        // Deltas 0, 1, 4, 9, 10 ms ⇒ median 4 ms, MAD 4 ms ⇒ noise 5.93 ms, so the
        // threshold is 17.8 ms: the 4 ms median is well inside the noise.
        let pairs: Vec<(f64, f64)> = [0.0, 1.0, 4.0, 9.0, 10.0]
            .iter()
            .map(|delta| (100.0 + delta, 100.0))
            .collect();
        let m = resolve(&pairs, 1_000);
        assert_eq!(m.rounds, 5);
        assert_eq!(m.median_delta_ms, 4.0);
        assert!((m.noise_ms - 5.9304).abs() < 1e-3, "noise {}", m.noise_ms);
        assert!(m.median_delta_ms < NOISE_MULTIPLE * m.noise_ms);
        assert_eq!(m.unit_cost_ns(), None);
        let note = m.below_resolution_note();
        assert!(note.contains("below timer resolution"), "{note}");
        assert!(note.contains("noise=±"), "{note}");
        assert_finite(&m);
    }

    /// Zero/negative medians, a non-positive unit count and no input at all are all
    /// unresolvable — and finite.
    #[test]
    fn zero_and_negative_marginals_are_unresolvable() {
        let zero = resolve(&[(100.0, 100.0); 5], 1_000);
        assert_eq!(zero.unit_cost_ns(), None);
        assert_eq!(zero.median_delta_ms, 0.0);
        assert_eq!(zero.rounds, 5);
        assert_finite(&zero);

        // Deltas -5, -4, -1 ms ⇒ median -4 ms: the startup pass was dearer than the work
        // pass, so no figure may be published.
        let negative = resolve(&[(95.0, 100.0), (96.0, 100.0), (99.0, 100.0)], 1_000);
        assert_eq!(negative.unit_cost_ns(), None);
        assert_eq!(negative.median_delta_ms, -4.0);
        assert_finite(&negative);

        // A real delta but no units to divide it by.
        assert_eq!(resolve(&[(110.0, 100.0); 5], 0).unit_cost_ns(), None);
        assert_eq!(resolve(&[(110.0, 100.0); 5], -7).unit_cost_ns(), None);

        // No input at all.
        let empty = resolve(&[], 1_000);
        assert_eq!(empty.unit_cost_ns(), None);
        assert_eq!(empty.rounds, 0);
        assert_eq!(empty.noise_ms, MIN_NOISE_MS);
        assert_finite(&empty);

        // NaN/inf samples are dropped, never propagated into the report.
        let broken = resolve(&[(f64::NAN, 100.0), (f64::INFINITY, 100.0)], 1_000);
        assert_eq!(broken.unit_cost_ns(), None);
        assert_eq!(broken.rounds, 0);
        assert_finite(&broken);
    }

    /// Identical deltas ⇒ MAD 0 ⇒ the noise band is the [`MIN_NOISE_MS`] floor, so a delta
    /// above `NOISE_MULTIPLE × MIN_NOISE_MS` publishes the exact expected ns/op.
    #[test]
    fn identical_deltas_resolve_off_the_noise_floor() {
        let m = resolve(&[(110.0, 100.0); 5], 1_000);
        assert_eq!(m.rounds, 5);
        assert_eq!(m.noise_ms, MIN_NOISE_MS);
        assert_eq!(m.median_delta_ms, 10.0);
        assert_eq!(m.min_delta_ms, 10.0);
        assert_eq!(m.max_delta_ms, 10.0);
        assert_eq!(m.unit_cost_ns(), Some(10_000.0));

        // Exactly on the threshold (3 × 1 ms) still publishes ...
        assert_eq!(
            resolve(&[(103.0, 100.0); 5], 1_000).unit_cost_ns(),
            Some(3_000.0)
        );
        // ... one millisecond under it does not.
        assert_eq!(resolve(&[(102.0, 100.0); 5], 1_000).unit_cost_ns(), None);
    }

    /// A later failure retains the pairs already measured and reports where it stopped.
    #[test]
    fn pairs_are_retained_when_a_later_pass_aborts() {
        // Sample 6 is round 3's `Startup` pass (passes run Work, Startup, Work, ...
        // so sample indices 1..=6 are W,S,W,S,W,S).
        let mut sample_index = 0;
        let runs = sample_pairs(RUNS, |_pass| {
            sample_index += 1;
            if sample_index == 6 {
                return Err(PassError {
                    note: "runtime exited 1: the case printed nothing".into(),
                    unsupported: false,
                });
            }
            Ok(sample_index as f64)
        });
        assert_eq!(runs.pairs, vec![(1.0, 2.0), (3.0, 4.0)]);
        let aborted = runs.aborted.expect("the failure must be reported");
        assert_eq!(aborted.pass, Pass::Startup);
        assert_eq!(aborted.note, "runtime exited 1: the case printed nothing");
        assert!(!aborted.unsupported);

        // A failure on the very first `Work` pass retains nothing and carries the
        // capability-gap flag through untouched.
        let runs = sample_pairs(RUNS, |_pass| {
            Err::<f64, PassError>(PassError {
                note: "unsupported: unsupported_bif".into(),
                unsupported: true,
            })
        });
        assert!(runs.pairs.is_empty());
        let aborted = runs.aborted.expect("the failure must be reported");
        assert_eq!(aborted.pass, Pass::Work);
        assert_eq!(aborted.note, "unsupported: unsupported_bif");
        assert!(aborted.unsupported);

        // A complete measurement reports no abort at all.
        let runs = sample_pairs(RUNS, |pass| Ok::<f64, PassError>(pass.units() as f64));
        assert_eq!(runs.pairs.len(), RUNS);
        assert!(runs.aborted.is_none());
    }

    /// `Pass` maps to the loop count and the note spelling the workloads rely on.
    #[test]
    fn pass_round_trips_its_units() {
        assert_eq!(Pass::Work.units(), 1);
        assert_eq!(Pass::Startup.units(), 0);
        assert_eq!(Pass::Work.as_str(), "work");
        assert_eq!(Pass::Startup.as_str(), "startup");
        assert_ne!(Pass::Work, Pass::Startup);
        assert_eq!(Pass::Work, Pass::Work);
    }

    /// `detail` carries every number behind the decision.
    #[test]
    fn detail_reports_the_full_paired_evidence() {
        let m = resolve(&[(110.0, 100.0), (120.0, 100.0), (130.0, 100.0)], 1_000);
        let detail = m.detail();
        assert!(detail.contains("median=20.000 ms"), "{detail}");
        assert!(detail.contains("min=10.000 ms"), "{detail}");
        assert!(detail.contains("max=30.000 ms"), "{detail}");
        assert!(detail.contains("noise=±"), "{detail}");
        assert!(detail.contains("over 3 round(s)"), "{detail}");
        assert_eq!(
            m.below_resolution_note(),
            format!("marginal below timer resolution: {detail}"),
            "the note must cite the detail verbatim"
        );
    }
}
