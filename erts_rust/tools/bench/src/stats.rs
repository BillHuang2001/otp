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

//! Summary statistics (`Summary`) and the percentile helper.
//!
//! The benchmark driver records *distributions* (startup latency, compile wall time,
//! …), so the report schema carries a fixed [`Summary`] per sampled metric:
//! `{count, min, max, mean, p50, p90, p99}`. This module is the single source of truth
//! for how those numbers are computed, keeping it testable in isolation.

use serde::{Deserialize, Serialize};

/// A fixed-shape statistical summary of a sample set.
///
/// Serialized verbatim into every sampled [`Metric`](crate::metrics::Metric).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// Number of samples summarized.
    pub count: usize,
    /// Smallest sample.
    pub min: f64,
    /// Largest sample.
    pub max: f64,
    /// Arithmetic mean of the samples.
    pub mean: f64,
    /// 50th percentile.
    pub p50: f64,
    /// 90th percentile.
    pub p90: f64,
    /// 99th percentile.
    pub p99: f64,
}

impl Summary {
    /// The all-zero summary used for an empty sample set.
    ///
    /// We deliberately use `0.0` (not `NaN`) so the JSON stays finite and comparable;
    /// `count: 0` is the honest signal that there was nothing to summarize.
    pub const EMPTY: Summary = Summary {
        count: 0,
        min: 0.0,
        max: 0.0,
        mean: 0.0,
        p50: 0.0,
        p90: 0.0,
        p99: 0.0,
    };
}

/// Arithmetic mean; returns `0.0` for an empty slice (see [`Summary::EMPTY`]).
pub fn mean(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.iter().sum::<f64>() / samples.len() as f64
}

/// Linear-interpolated percentile of an **ascending-sorted** slice.
///
/// Uses the *linear interpolation* definition (the NumPy default, R `type = 7`):
/// for a probability `p` in `[0, 100]` and `n` samples the fractional rank is
/// `rank = p / 100 * (n - 1)`; the result is `sorted[floor(rank)]` when `rank` is an
/// integer, otherwise linearly interpolated between `sorted[floor(rank)]` and
/// `sorted[ceil(rank)]`.
///
/// Edge cases: an empty slice returns `0.0` (finite, unlike `NaN`); a single sample
/// returns that sample for every `p`. `p` is clamped to `[0, 100]`.
///
/// The caller must pass `sorted` in ascending order — this function does not sort
/// (it is called up to three times per [`summarize`] on the same vec).
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let p = p.clamp(0.0, 100.0);
    let rank = p / 100.0 * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = rank - lo as f64;
    sorted[lo] + (sorted[hi] - sorted[lo]) * frac
}

/// Summarizes a sample set into a [`Summary`].
///
/// An empty input yields [`Summary::EMPTY`]. `NaN` samples are ordered last (via a
/// total-order fallback) rather than panicking; the driver never generates them, but
/// the harness must not crash on unexpected input.
pub fn summarize(samples: &[f64]) -> Summary {
    if samples.is_empty() {
        return Summary::EMPTY;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Summary {
        count: sorted.len(),
        min: sorted[0],
        max: sorted[sorted.len() - 1],
        mean: mean(&sorted),
        p50: percentile(&sorted, 50.0),
        p90: percentile(&sorted, 90.0),
        p99: percentile(&sorted, 99.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mean_of_empty_is_zero() {
        assert_eq!(mean(&[]), 0.0);
    }

    #[test]
    fn mean_of_values() {
        assert_eq!(mean(&[1.0, 2.0, 3.0, 4.0]), 2.5);
    }

    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile(&[], 50.0), 0.0);
    }

    #[test]
    fn percentile_single_sample_is_that_sample() {
        assert_eq!(percentile(&[42.0], 0.0), 42.0);
        assert_eq!(percentile(&[42.0], 50.0), 42.0);
        assert_eq!(percentile(&[42.0], 99.9), 42.0);
    }

    #[test]
    fn percentile_interpolates_linearly() {
        // n = 5, p50 => rank = 0.5 * 4 = 2.0 => exactly sorted[2].
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 50.0), 3.0);
        // p25 => rank = 0.25 * 4 = 1.0 => sorted[1].
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 25.0), 2.0);
        // p10 => rank = 0.4 => between sorted[0]=1 and sorted[1]=2 => 1.4.
        assert!((percentile(&[1.0, 2.0, 3.0, 4.0, 5.0], 10.0) - 1.4).abs() < 1e-9);
        // p0 / p100 clamp to the extremes.
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 0.0), 1.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 100.0), 3.0);
    }

    #[test]
    fn percentile_clamps_out_of_range_probabilities() {
        let s = [1.0, 2.0, 3.0];
        assert_eq!(percentile(&s, -5.0), 1.0);
        assert_eq!(percentile(&s, 200.0), 3.0);
    }

    #[test]
    fn summarize_empty_is_the_empty_summary() {
        assert_eq!(summarize(&[]), Summary::EMPTY);
        assert_eq!(summarize(&[]).count, 0);
    }

    #[test]
    fn summarize_single_sample_has_flat_statistics() {
        let s = summarize(&[7.0]);
        assert_eq!(s.count, 1);
        assert_eq!(s.min, 7.0);
        assert_eq!(s.max, 7.0);
        assert_eq!(s.mean, 7.0);
        assert_eq!(s.p50, 7.0);
        assert_eq!(s.p90, 7.0);
        assert_eq!(s.p99, 7.0);
    }

    #[test]
    fn summarize_known_values() {
        let s = summarize(&[10.0, 20.0, 30.0, 40.0]);
        assert_eq!(s.count, 4);
        assert_eq!(s.min, 10.0);
        assert_eq!(s.max, 40.0);
        assert_eq!(s.mean, 25.0);
        // p50 => rank 1.5 => 20 + (30-20)*0.5 = 25.
        assert_eq!(s.p50, 25.0);
        // p90 => rank 2.7 => 30 + (40-30)*0.7 = 37.
        assert!((s.p90 - 37.0).abs() < 1e-9);
        // p99 => rank 2.97 => 30 + 10*0.97 = 39.7.
        assert!((s.p99 - 39.7).abs() < 1e-9);
    }

    #[test]
    fn summarize_does_not_require_presorted_input() {
        let a = summarize(&[3.0, 1.0, 2.0]);
        let b = summarize(&[1.0, 2.0, 3.0]);
        assert_eq!(a, b);
    }
}
