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

//! The **tiering policy** — the interpret → baseline → hot escalation ladder.
//!
//! Part II §F ("The JIT") prescribes a three-rung ladder for a function that is
//! already loaded: `Tier 0` the interpreter (fallback/diagnostics) → `Tier 1` the
//! baseline JIT (BeamAsm + block register allocation, "all the *certain* value, no
//! speculation") → `Tier 2` the optional Cranelift hot tier (per-function recompile
//! with runtime type narrowing + deopt, compiled **off the critical path** with a
//! bounded budget). Escalation is driven by a **cheap per-function execution
//! counter** — BEAM has none today (reductions are per-scheduler), so the counter is
//! part of the new design.
//!
//! [`TierPolicy`] models that ladder: it holds the two thresholds and answers which
//! [`Tier`] a function with a given execution count belongs on.
//!
//! ```text
//!   executions:  0   1 ....... baseline_threshold ....... hot_threshold .....
//!   tier:        |------- Interpreter -------|--- BaselineJit ---|--- Cranelift ---|
//! ```
//!
//! # Availability is respected, never assumed
//!
//! The policy never names a tier this build cannot run. [`Tier::Cranelift`] exists
//! only when the **`jit-cranelift`** Cargo feature is enabled, so
//! [`select`](TierPolicy::select) degrades to [`Tier::BaselineJit`] when it is not
//! ([`super::available_tiers`] is the roster of compiled-in tiers). In general the selected
//! tier is the *hottest tier at or below* the one the thresholds ask for that
//! [`Tier::is_available`] — and the interpreter is always available, so a tier is
//! always returned.
//!
//! # This is a *policy*, not yet a wiring
//!
//! [`TierPolicy`] is deliberately **not** consulted anywhere on an execution path:
//! [`Machine::run_with_tier`](crate::Machine::run_with_tier) still runs exactly the
//! tier the caller names for the whole call, the interpreter's semantics are
//! unchanged, and neither the `baseline` nor the `cranelift` backend recompiles or
//! promotes anything mid-run. This
//! module demonstrates the escalation *ladder* (upgrade thresholds, the availability
//! downgrade) that a future per-function call counter and promotion machinery would
//! consult; installing that machinery is separate, later work (Part II §F, Stage 1b).

use super::Tier;

/// The default baseline threshold ([`TierPolicy::default`]): a function is
/// interpreted once, then runs on the baseline JIT.
pub const DEFAULT_BASELINE_THRESHOLD: u32 = 1;

/// The default hot threshold ([`TierPolicy::default`]): a function that has run
/// this many times is hot enough to be worth an off-critical-path Cranelift
/// recompile.
pub const DEFAULT_HOT_THRESHOLD: u32 = 10_000;

/// A measurement-gated tiering policy: escalate a function from the interpreter
/// to the baseline JIT once it is warm, and (when compiled in) to the Cranelift hot
/// tier once it is hot. Counters are per-function in the real design; this type
/// models the escalation ladder and its thresholds.
///
/// See the module docs for the ladder and for why this is a *policy* type that no
/// execution path consults yet.
///
/// ```
/// use beam_engine::jit::{Tier, TierPolicy};
///
/// let policy = TierPolicy::new(2, 100);
/// assert_eq!(policy.select(0), Tier::Interpreter); // cold
/// assert_eq!(policy.select(1), Tier::Interpreter);
/// assert_eq!(policy.select(2), Tier::BaselineJit); // warm
/// assert_eq!(policy.select(99), Tier::BaselineJit);
/// // Hot — Cranelift when it is compiled in, else the baseline JIT.
/// let hot = policy.select(100);
/// assert!(matches!(hot, Tier::BaselineJit | Tier::Cranelift));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierPolicy {
    baseline_threshold: u32,
    hot_threshold: u32,
}

impl TierPolicy {
    /// A policy that keeps a function on the interpreter for
    /// `baseline_threshold` executions, on the baseline JIT from there up to
    /// `hot_threshold`, and on the Cranelift hot tier (when compiled in) from
    /// `hot_threshold` on.
    ///
    /// `hot_threshold` is clamped **up** to `baseline_threshold` so the ladder
    /// never inverts (a hot threshold below the baseline threshold would make the
    /// baseline rung unreachable): `new(10, 3)` behaves as `new(10, 10)`.
    pub fn new(baseline_threshold: u32, hot_threshold: u32) -> TierPolicy {
        TierPolicy {
            baseline_threshold,
            hot_threshold: hot_threshold.max(baseline_threshold),
        }
    }

    /// The execution count at which the baseline JIT takes over from the
    /// interpreter.
    pub fn baseline_threshold(&self) -> u32 {
        self.baseline_threshold
    }

    /// The execution count at which the Cranelift hot tier takes over from the
    /// baseline JIT (as clamped by [`TierPolicy::new`]).
    pub fn hot_threshold(&self) -> u32 {
        self.hot_threshold
    }

    /// The tier a function with `executions` executions of its counter belongs on.
    ///
    /// Below [`baseline_threshold`](TierPolicy::baseline_threshold) the
    /// interpreter; from there up to (but not including)
    /// [`hot_threshold`](TierPolicy::hot_threshold) the baseline JIT; at or above
    /// the hot threshold the Cranelift hot tier — **when it is compiled in** —
    /// otherwise the baseline JIT. The returned tier is always one that
    /// [`super::available_tiers`] lists.
    pub fn select(&self, executions: u32) -> Tier {
        let wanted = if executions < self.baseline_threshold {
            Tier::Interpreter
        } else if executions < self.hot_threshold {
            Tier::BaselineJit
        } else {
            Tier::Cranelift
        };
        hottest_available(wanted)
    }

    /// Whether `executions` puts a function on this build's hottest tier — that is,
    /// whether it has been interpreted and baselined as much as this build can
    /// profit from.
    pub fn is_hot(&self, executions: u32) -> bool {
        executions >= self.hot_threshold
    }
}

impl Default for TierPolicy {
    /// The default policy: interpreted once, then baselined; hot at
    /// [`DEFAULT_HOT_THRESHOLD`] executions. See [`DEFAULT_BASELINE_THRESHOLD`].
    fn default() -> TierPolicy {
        TierPolicy::new(DEFAULT_BASELINE_THRESHOLD, DEFAULT_HOT_THRESHOLD)
    }
}

/// The hottest tier at or below `wanted` that this build can run.
///
/// [`Tier::ALL`] lists the tiers in roster order (interpreter first), so walking it
/// downwards from `wanted` finds the strongest available tier no hotter than what
/// the thresholds asked for. The interpreter is in every roster, so the walk always
/// terminates on an available tier.
fn hottest_available(wanted: Tier) -> Tier {
    let requested = Tier::ALL
        .iter()
        .position(|tier| *tier == wanted)
        .unwrap_or(0);
    Tier::ALL[..=requested]
        .iter()
        .rev()
        .find(|tier| tier.is_available())
        .copied()
        .unwrap_or(Tier::Interpreter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit::available_tiers;
    /// The Cranelift tier is in this build's roster exactly when the feature is on.
    fn cranelift_built() -> bool {
        available_tiers().contains(&Tier::Cranelift.name())
    }

    #[test]
    fn thresholds_are_reported_as_given_when_ordered() {
        let policy = TierPolicy::new(3, 17);
        assert_eq!(policy.baseline_threshold(), 3);
        assert_eq!(policy.hot_threshold(), 17);
    }

    #[test]
    fn a_hot_threshold_below_the_baseline_is_clamped_up() {
        let policy = TierPolicy::new(10, 3);
        assert_eq!(policy.baseline_threshold(), 10);
        assert_eq!(policy.hot_threshold(), 10);
        // The baseline rung is therefore still reachable.
        assert_eq!(policy.select(9), Tier::Interpreter);
        assert_eq!(policy.select(10), policy.select(u32::MAX - 1));
    }

    #[test]
    fn cold_warm_and_hot_boundaries() {
        let policy = TierPolicy::new(2, 100);

        // Cold: below the baseline threshold.
        assert_eq!(policy.select(0), Tier::Interpreter);
        assert_eq!(policy.select(1), Tier::Interpreter);

        // Warm: from the baseline threshold, exclusive of the hot threshold.
        assert_eq!(policy.select(2), Tier::BaselineJit);
        assert_eq!(policy.select(99), Tier::BaselineJit);

        // Hot: at and above the hot threshold.
        let hot = policy.select(100);
        assert!(matches!(hot, Tier::BaselineJit | Tier::Cranelift));
        assert_eq!(policy.select(100), policy.select(u32::MAX));
    }

    #[test]
    fn a_zero_baseline_threshold_starts_on_the_baseline_jit() {
        let policy = TierPolicy::new(0, 5);
        assert_eq!(policy.select(0), Tier::BaselineJit);
        assert_eq!(policy.select(4), Tier::BaselineJit);
        assert!(matches!(
            policy.select(5),
            Tier::BaselineJit | Tier::Cranelift
        ));
    }

    #[test]
    fn equal_thresholds_skip_the_baseline_rung() {
        let policy = TierPolicy::new(4, 4);
        assert_eq!(policy.select(3), Tier::Interpreter);
        assert!(matches!(
            policy.select(4),
            Tier::BaselineJit | Tier::Cranelift
        ));
    }

    #[test]
    fn select_never_names_an_unavailable_tier() {
        let policy = TierPolicy::new(1, 1);
        for executions in [0, 1, 2, 1_000, u32::MAX] {
            let tier = policy.select(executions);
            assert!(
                available_tiers().contains(&tier.name()),
                "select({executions}) = {tier:?}, which is not in {:?}",
                available_tiers()
            );
        }
    }

    #[test]
    fn cranelift_only_escalates_when_it_is_compiled_in() {
        // A hot function: Cranelift when the feature is on, else the baseline JIT —
        // never the unavailable tier and never a downgrade below the baseline.
        let policy = TierPolicy::new(1, 3);
        let hot = policy.select(3);
        if cranelift_built() {
            assert_eq!(hot, Tier::Cranelift);
            assert_eq!(Tier::Cranelift.is_available(), cranelift_built());
        } else {
            assert_eq!(hot, Tier::BaselineJit);
        }
    }

    #[test]
    fn is_hot_tracks_the_hot_threshold() {
        let policy = TierPolicy::new(1, 10);
        assert!(!policy.is_hot(9));
        assert!(policy.is_hot(10));
        assert!(policy.is_hot(u32::MAX));
    }

    #[test]
    fn default_is_baseline_at_one_and_hot_later() {
        let policy = TierPolicy::default();
        assert_eq!(policy, TierPolicy::new(1, DEFAULT_HOT_THRESHOLD));
        assert_eq!(policy.baseline_threshold(), DEFAULT_BASELINE_THRESHOLD);
        assert_eq!(policy.hot_threshold(), DEFAULT_HOT_THRESHOLD);
        assert!(DEFAULT_HOT_THRESHOLD > DEFAULT_BASELINE_THRESHOLD);

        assert_eq!(policy.select(0), Tier::Interpreter);
        assert_eq!(policy.select(1), Tier::BaselineJit);
        assert!(matches!(
            policy.select(DEFAULT_HOT_THRESHOLD),
            Tier::BaselineJit | Tier::Cranelift
        ));
    }
}
