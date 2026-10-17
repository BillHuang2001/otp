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
//! **What the reduction/yield-boundary collection door costs on a hot workload.**
//!
//! `crate::context::gc::collect_at_reds_boundary` — the runner's `Yield`-arm collection door —
//! is the one collection site whose cost cannot be read off `Runtime::gc_stats()` alone: on a
//! shape that also reaches the heap-admission doors the total collection count mixes the two.
//! The runner therefore tallies the door itself (`super::super::run::BoundaryDoorCounters`:
//! how many `Yield` parks were handed to it, and how many of those became a real collection),
//! and this module pins that accounting on the `gc_bound` churn shape — the allocate-and-
//! abandon loop whose benchmark row (`tools/bench/src/workloads/gc_bound.rs`) is one of the
//! hot ones.
//!
//! The measured per-call cost of the door on every hot shape (and on both execution tiers) is
//! recorded in `../CONTEXT.md` *Measured*; the two relations pinned here are its structure:
//!
//! * **one check per pre-emption** — the door is handed exactly one boundary per `Yield` park
//!   (`door.checks == yields`), never one per instruction;
//! * **the occupancy watermark halves it** — of two consecutive boundaries only one collects
//!   (the boundary that just collected left the nursery below the watermark, so the next one
//!   declines), which is what keeps the door's own hit rate at ≈0.1 collections per call while
//!   the shape does ≈7.
//!
//! Together they bound the door's contribution: it runs at most one extra collection per two
//! pre-emptions, which on this shape is **+0.048 collections/call (+0.7 %)** — measured
//! against a door-disabled build of the same tree, and recorded with the rest of the table.
use beam_term::make_small;

use super::super::run::{boundary_door_counters, reset_boundary_door_counters, BoundaryDoorCounters};
use super::{growth_runtime, GrowthCounters, COUNTER_RUN_DEADLINE};
use crate::runtime::Runtime;

/// The committed allocate-and-abandon fixture (`churn:run/1`): a constant live set with one
/// allocation-heavy call per iteration, every binary/tuple it keeps coming from a registered
/// BIF (`list_to_binary/1`, `list_to_tuple/1`).
fn churn_fixture() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/churn.beam")
}

/// One scheduled run's counters, split into the shape's totals and the boundary door's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    /// The shape's counters (the same set `super::GrowthCounters` carries).
    counters: GrowthCounters,
    /// The boundary door's own tallies for the very same run.
    door: BoundaryDoorCounters,
}

impl Run {
    /// The number of loop iterations `n` performs, plus the final clause — the denominator of
    /// every per-call figure.
    fn calls(n: i64) -> f64 {
        (n as f64) + 1.0
    }
}

/// Runs `module:function/1` with argument `n` as a **scheduled process** on `rt` and returns
/// the shape's counters plus the boundary door's own tallies for the same run.
///
/// [`super::run_growth_counted`] is the same harness with the entry function hard-coded to
/// `run/1`; this one takes the function name (`slice:fib/1` has no `run/1`) and reads the
/// boundary door's counters around the run, which is the whole point of the pin below. The
/// nursery geometry is sampled at every slice boundary — the only window a live process's
/// collector is readable from outside the runner.
fn run_counted(rt: &mut Runtime, module: &str, function: &str, n: i64) -> Run {
    let before = rt.gc_stats();
    reset_boundary_door_counters();
    super::super::run::reset_run_slice_counters();
    let pid = rt
        .spawn_process(module, function, vec![make_small(n)])
        .unwrap_or_else(|err| panic!("{module}:{function}/1 must spawn: {err}"));
    let mut peak_young_spaces = 0usize;
    let mut peak_young_capacity = 0usize;
    let mut peak_mature_capacity = 0usize;
    let mut peak_mature_used = 0usize;
    let mut debt_boundaries = 0usize;
    let deadline = std::time::Instant::now() + COUNTER_RUN_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "{module}:{function}/1 with N={n} did not terminate within {}s",
            COUNTER_RUN_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
        if let Some(process) = rt.processes().get(pid) {
            let collector = process.collector();
            peak_young_spaces = peak_young_spaces.max(collector.young_space_count());
            peak_young_capacity = peak_young_capacity.max(collector.young().capacity_words());
            peak_mature_capacity = peak_mature_capacity.max(collector.mature_capacity_words());
            peak_mature_used = peak_mature_used.max(collector.mature_used_words());
            if process.collection_debt() {
                debt_boundaries += 1;
            }
        }
    }
    let slices = super::super::run::run_slice_counters();
    let after = rt.gc_stats();
    Run {
        counters: GrowthCounters {
            value: beam_term::signed_value(rt.process_result(pid).expect("the loop returned")),
            slices: slices.slices,
            yields: slices.yields,
            waits: slices.waits,
            collections: after.collections - before.collections,
            major: after.major - before.major,
            fallbacks: after.fallbacks - before.fallbacks,
            errors: after.errors - before.errors,
            peak_young_spaces,
            peak_young_capacity,
            peak_mature_capacity,
            peak_mature_used,
            debt_boundaries,
        },
        door: boundary_door_counters(),
    }
}

/// **The boundary door is a per-pre-emption, watermarked site on the `gc_bound` churn shape —
/// not a per-call cost.**
///
/// Two derived relations, both structural facts of the runner and the collector (never
/// timings), pinned at two iteration counts:
///
/// * **one boundary check per pre-emption** — `door.checks == yields`, exactly: the door is
///   reached from the runner's `Yield` arm and nowhere else, so its check count is the shape's
///   own pre-emption count (∈ [0.19, 0.20] per call) and cannot grow with anything else;
/// * **the occupancy watermark declines the alternating boundary** —
///   `door.collections == door.checks / 2`, exactly: the boundary that collects leaves the
///   nursery below the watermark, so the very next one is a typed no-op. That is what bounds
///   the door's own hit rate at **0.096 collections per call** on a shape that does ≈7.
///
/// The measured consequence, taken against a door-disabled build of the same tree, is a total
/// of **+0.048 collections/call (+0.69 %)**: less than the door's own hit rate, because each
/// boundary collection lets the neighbouring heap-admission door decline. The full table
/// (every hot shape, both execution tiers, ON vs OFF) is in `../CONTEXT.md` *Measured*.
///
/// Every iteration of this shape already reaches a heap-admission door, so the shape is the
/// *control* for the door: unlike `reds_boundary:run/1` (whose loop reaches no such door) this
/// one is collected with or without it.
#[test]
fn the_boundary_door_is_a_per_preemption_watermarked_site_on_the_churn_shape() {
    const COUNTS: [i64; 2] = [2_000, 10_000];
    let mut rows = Vec::new();
    for n in COUNTS {
        let mut rt = growth_runtime(churn_fixture());
        let run = run_counted(&mut rt, "churn", "run", n);
        assert_eq!(run.counters.value, n, "churn:run({n}) must return its argument");
        assert_eq!(run.counters.errors, 0, "no collection may fail: N={n} {run:?}");
        assert_eq!(run.counters.waits, 0, "churn never parks in a receive: {run:?}");
        assert_eq!(run.counters.fallbacks, 0, "churn never falls back to the arena: {run:?}");
        eprintln!(
            "boundary_cost churn N={n} slices={} yields={} collections={} major={} door_checks={} \
             door_unpublished={} door_collections={} peak_young_spaces={} peak_young_capacity={}",
            run.counters.slices,
            run.counters.yields,
            run.counters.collections,
            run.counters.major,
            run.door.checks,
            run.door.unpublished,
            run.door.collections,
            run.counters.peak_young_spaces,
            run.counters.peak_young_capacity,
        );
        rows.push((n, run));
    }
    let [(_, r1), (n2, r2)] = [rows[0], rows[1]];

    // 1. The door is reached exactly once per pre-emption (and never from a non-publishing
    //    window: churn's loop carries no live float register at a slice end).
    assert!(r1.door.checks > 0, "the shape must reach the door at all: {r1:?}");
    assert_eq!(
        (r1.door.checks, r2.door.checks),
        (r1.counters.yields, r2.counters.yields),
        "one boundary check per `Yield` park: {r1:?} {r2:?}"
    );
    assert_eq!(
        (r1.door.unpublished, r2.door.unpublished),
        (0, 0),
        "every churn park publishes its root set: {r1:?} {r2:?}"
    );
    // 2. The occupancy watermark declines exactly the alternating boundary.
    assert_eq!(
        (r1.door.collections, r2.door.collections),
        (r1.door.checks / 2, r2.door.checks / 2),
        "the boundary that collected leaves the nursery below the watermark, so the next one \
         declines: {r1:?} {r2:?}"
    );
    // The bounds that follow (no per-call cost growth, a hit rate far below the shape's own
    // collection rate) hold at both counts and are what makes the door inert here.
    for (n, run) in [rows[0], rows[1]] {
        let calls = Run::calls(n);
        let checks_per_call = run.door.checks as f64 / calls;
        let door_per_call = run.door.collections as f64 / calls;
        let collections_per_call = run.counters.collections as f64 / calls;
        assert!(
            (0.15..0.25).contains(&checks_per_call),
            "one boundary per ~5 calls (a 4 000-reduction slice over a ~20-reduction \
             iteration): N={n} {checks_per_call:.6}/call"
        );
        assert!(
            door_per_call <= checks_per_call,
            "at most one collection per boundary: N={n} {door_per_call:.6} > {checks_per_call:.6}"
        );
        assert!(
            collections_per_call <= 7.06,
            "the door must not move the shape's own ~7.0 collections/call more than its hit \
             rate can justify (measured 7.044 with the door, 6.996 without): N={n} \
             {collections_per_call:.6}/call"
        );
    }
    // The nursery stays one space and keeps the collector's floor capacity: the door's
    // collections keep the retired-space chain collapsed instead of letting it grow with N.
    assert_eq!(
        (r1.counters.peak_young_spaces, r2.counters.peak_young_spaces),
        (1, 1),
        "the door keeps the nursery chain collapsed: {r1:?} {r2:?}"
    );
    assert!(
        r2.counters.peak_young_capacity <= 610,
        "the nursery capacity must not climb with N: N={n2} peak={}",
        r2.counters.peak_young_capacity
    );
}
