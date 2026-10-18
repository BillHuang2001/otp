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
//! # The measured table (every figure below is printed by the test's own `eprintln!`)
//!
//! | N | slices | yields | collections | major | door checks | door collections | young spaces | young capacity |
//! |---|---|---|---|---|---|---|---|---|
//! | 2 000 | 905 | 904 | 15 346 | 2 514 | 904 | 113 | 1 | 609 |
//! | 4 000 | 1 810 | 1 809 | 30 691 | 4 514 | 1 809 | 230 | 1 | 609 |
//! | 10 000 | 4 525 | 4 524 | 76 728 | 10 514 | 4 524 | 571 | 1 | 609 |
//!
//! Three repetitions of the whole table are byte-identical, so every figure here is a
//! **deterministic counter**, never a timing. The two relations pinned are:
//!
//! * **one check per pre-emption** — the door is handed exactly one boundary per `Yield` park
//!   (`door.checks == yields`), never one per instruction: 904 / 1 809 / 4 524 checks against
//!   904 / 1 809 / 4 524 pre-emptions. That is **0.4518 / 0.4521 / 0.4524 checks per call**
//!   (`N + 1` calls), i.e. a 4 000-reduction slice carries 2 001 / 905 = 2.2110, 4 001 / 1 810 =
//!   2.2105 and 10 001 / 4 525 = 2.2102 calls, so the loop's own price is 4 000 / 2.2110 =
//!   **1 809 reductions per call** (the churn counter table's independent `reductions` column
//!   reads 3 619 770 over the very same 2 001 calls = 1 808.98);
//! * **the seven-eighths watermark rations it to about one boundary in eight** —
//!   `door.checks / 9 <= door.collections <= door.checks / 7`: the shape's own heap-admission
//!   door settles its collection debt on every iteration, so the nursery is nearly exhausted
//!   (seven eighths used, the C's `ERTS_VBUMP_ALLOC_REDS` moment) at only every ~eighth slice
//!   end, and a boundary collection leaves the boundaries after it below the watermark. Measured
//!   **113/904 = one in 8.00** (N = 2 000), **230/1 809 = one in 7.87** (N = 4 000) and
//!   **571/4 524 = one in 7.92** (N = 10 000): the door fires on 0.0565 / 0.0575 / 0.0571 calls
//!   of a shape that collects 7.6692 / 7.6708 / 7.6720 times per call — **0.74 %** of the run's
//!   own collections. It is a *rate*, not an identity: 8.00 / 7.87 / 7.92 are not one another
//!   and none of them is a law (the N = 2 000 row lands on 8.00 only because 113 × 8 = 904
//!   exactly), which is why the assertion is the band and not `checks / 8`.
//!
//! **What this module can no longer verify: the door-ON vs door-OFF A/B it used to quote.**
//! Establishing that the door's collections *substitute for* the heap-admission door's rather
//! than adding to them needs a door-disabled build — a throwaway `#[cfg(test)]` early return
//! inside `collect_at_reds_boundary` (`src/context/gc/reds_door.rs`) — which is an edit outside
//! this measurement's scope. The figures that A/B produced (identical totals at 6.984032 /
//! 6.996002 / 6.999200 collections and 1.497006 / 1.499250 / 1.499850 majors per call at
//! N = 500 / 2 000 / 10 000, against a door-off two young spaces / 376 words) come from an
//! earlier tree state and no longer describe this one — N = 2 000 measures 7.669165 /
//! 1.256372 per call and a **609**-word young space — so the substitution claim is **unverified**
//! here and is not restated as a fact. Three structural facts keep the totals off the closed form
//! `collections == 7N - 1` / `major == 3N / 2`: the door pins a young-only
//! `Process::collect_kind(CollectionKind::Minor)` first and escalates to the unpinned
//! `Process::collect` only when the promotion cannot fit the mature space (so a boundary firing
//! does not force a fullsweep of its own), the native baseline tier charges its blocks per block
//! (which sets the slice and pre-emption counts the per-call rates are expressed against), and
//! the BIF-result copy-out's learned hint grows the nursery **before** each copy
//! (`RuntimeContext::copy_out_hint` + `hint_grow_gate`, `src/context/calls.rs`) — a growth event
//! retires a nursery space, so it adds one unit of collection debt per iteration and therefore
//! sets how often this shape is collected at all. The historical `1/2`-watermark cost (**+0.048
//! collections/call (+0.69 %)** and **+0.134 majors/call (+8.9 %)** against its door-off build) is
//! from that same earlier tree state and is likewise not re-measured here. What the table above
//! *does* pin is the rationing: ~one firing in 7.9 boundaries, 0.74 % of the shape's collections,
//! a per-call collection rate flat to 0.04 % across a 5× range (7.6692 → 7.6720; the majors fall
//! from 1.2564 to 1.0513 per call, since they are one per iteration plus a fixed 514) and a
//! collapsed retired-space chain (one young space, 609 words at every N).
//!
//! Every iteration of this shape reaches the engine's heap-admission door (each churn builds
//! three 256-element structures, one of them through the interpreted `build_list/2` recursion),
//! so unlike `reds_boundary:run/1` — whose loop allocates solely through registered BIFs and so
//! reaches no admission door — this shape is collected whether or not the boundary door exists.
//! That makes it the door's *control*: a boundary firing here can only ever duplicate a
//! collection the iteration was going to take anyway, which is what the rationing is for.
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
    let reductions_before = rt.reductions_statistics(false).0;
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
            reductions: rt.reductions_statistics(false).0 - reductions_before,
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

/// The measured rows this module pins: `(N, collections, major)`.
///
/// These are the run's **exact** totals — the honest per-N form of what used to be quoted as a
/// closed form (`collections == 7N - 1`, `major == 3N / 2`, which this tree does not satisfy:
/// see the module docs for the structural facts that keep it off that form). Every figure the
/// docs quote (slices, yields, the door's checks and collections, the nursery geometry) is
/// printed by the test's own `eprintln!`, and three repetitions of the table are identical.
///
/// **Re-derive this table** from that output after any change to the runner's reduction pricing,
/// the door's gates/watermark, the collector's nursery sizing or the BIF-result copy-out's
/// growth gate — the files consulted are `src/runner/budget.rs`, `src/context/gc/reds_door.rs`,
/// `src/context/calls.rs` and the `beam_gc` collector's sizing.
const MEASURED: [(i64, u64, u64); 3] = [
    (2_000, 15_346, 2_514),
    (4_000, 30_691, 4_514),
    (10_000, 76_728, 10_514),
];

/// **The boundary door is a per-pre-emption, watermarked site on the `gc_bound` churn shape —
/// not a per-call cost.**
///
/// Three derived relations, all structural facts of the runner and the collector (never
/// timings), pinned at the three iteration counts of [`MEASURED`]:
///
/// * **one boundary check per pre-emption** — `door.checks == yields`, exactly, at every row:
///   the door is reached from the runner's `Yield` arm and nowhere else, so its check count *is*
///   the shape's pre-emption count (904 / 1 809 / 4 524 checks against 905 / 1 810 / 4 525
///   slices), i.e. **0.4518 / 0.4521 / 0.4524 per call**. At 4 000 reductions a slice and
///   2.2110 / 2.2105 / 2.2102 calls per slice, that prices the loop at **1 809 reductions per
///   call**;
/// * **the seven-eighths watermark rations it to between one boundary in seven and one in
///   nine** — `door.checks / 9 <= door.collections <= door.checks / 7`. It is a *rate*, not an
///   identity: measured 113/904 = **one in 8.00**, 230/1 809 = **one in 7.87**, 571/4 524 =
///   **one in 7.92**, so the door collects on **0.0565 / 0.0575 / 0.0571 calls** of a shape that
///   collects 7.6692 / 7.6708 / 7.6720 times per call — **0.74 %** of the run's own collections;
/// * **the shape's own totals are a measured table** ([`MEASURED`]), pinned exactly per row:
///   15 346 / 2 514 at N = 2 000, 30 691 / 4 514 at N = 4 000 and 76 728 / 10 514 at N = 10 000,
///   i.e. **7.6692 / 7.6708 / 7.6720 collections and 1.2564 / 1.1282 / 1.0513 majors per call**
///   (7.6730 / 7.6728 / 7.6728 and 1.2570 / 1.1285 / 1.0514 per iteration). The collection rate
///   is therefore flat per iteration (7.6728 across a 5× range) while the majors are **one per
///   iteration plus a fixed 514** (2 514 = 2 000 + 514, 4 514 = 4 000 + 514, 10 514 =
///   10 000 + 514 exactly), so their per-iteration rate falls toward 1 from above rather than
///   converging on the closed form's 1.5. The structural facts that keep the totals off
///   `7N - 1` / `3N / 2` are named in the module docs: the door pins a young-only
///   `CollectionKind::Minor` collection and escalates only when the promotion cannot fit the
///   mature space, the native baseline tier charges its blocks per block (which sets the
///   slice/pre-emption counts), and the BIF-result copy-out's learned hint grows the nursery
///   before each copy (one growth event = one retired space = one unit of collection debt per
///   iteration).
///
/// What the door contributes to those totals **cannot** be measured on this tree (a door-ON/OFF
/// A/B needs a door-disabled build — see the module docs), so nothing here claims a `Δ`; what is
/// pinned is the rationing itself, which is what rules out a per-call door cost.
#[test]
fn the_boundary_door_is_a_per_preemption_watermarked_site_on_the_churn_shape() {
    let mut rows = Vec::new();
    for (n, want_collections, want_major) in MEASURED {
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
        assert_eq!(
            (run.counters.collections, run.counters.major),
            (want_collections, want_major),
            "the measured totals of `MEASURED` must be what this tree runs (re-derive the table \
             from the line above, never relax it): N={n} {run:?}"
        );
        rows.push((n, run));
    }

    // 1. The door is reached exactly once per pre-emption, at every row — and never from a
    //    non-publishing window (churn's loop carries no live float register at a slice end).
    assert!(rows[0].1.door.checks > 0, "the shape must reach the door at all: {:?}", rows[0]);
    for (n, run) in &rows {
        assert_eq!(
            run.door.checks, run.counters.yields,
            "one boundary check per `Yield` park: N={n} {run:?}"
        );
        assert_eq!(
            run.door.unpublished, 0,
            "every churn park publishes its root set: N={n} {run:?}"
        );
    }

    // 2. The per-call rates: one boundary per ~2.2 calls, the watermark's 1-in-7..9 ration, and
    //    never more than one collection per boundary.
    for (n, run) in &rows {
        let calls = Run::calls(*n);
        let checks_per_call = run.door.checks as f64 / calls;
        let door_per_call = run.door.collections as f64 / calls;
        // A 4 000-reduction slice over an iteration that prices its `call`/`call_only`
        // resolutions and BIF dispatches — 1 809 reductions measured (see the module docs).
        assert!(
            (0.40..0.50).contains(&checks_per_call),
            "one boundary per ~2.2 calls (a 4 000-reduction slice over a ~1 809-reduction \
             iteration): N={n} {checks_per_call:.6}/call"
        );
        assert!(
            run.door.collections * 9 >= run.door.checks
                && run.door.collections * 7 <= run.door.checks,
            "the watermark rations the door to one boundary in 7..9 slice ends: N={n} {}/{}",
            run.door.collections,
            run.door.checks
        );
        assert!(
            door_per_call <= checks_per_call,
            "at most one collection per boundary: N={n} {door_per_call:.6} > {checks_per_call:.6}"
        );
    }

    // 3. The nursery stays one space and keeps the collector's plateau capacity: the door's
    //    collections keep the retired-space chain collapsed instead of letting it grow with N.
    for (n, run) in &rows {
        assert_eq!(
            run.counters.peak_young_spaces, 1,
            "the door keeps the nursery chain collapsed: N={n} {run:?}"
        );
        assert!(
            run.counters.peak_young_capacity <= 610,
            "the nursery capacity must not climb with N: N={n} peak={}",
            run.counters.peak_young_capacity
        );
    }
}
