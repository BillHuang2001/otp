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
//! | N | slices | yields | collections | major | door checks | door collections | young spaces | young capacity | young occupancy | mature capacity | mature used |
//! |---|---|---|---|---|---|---|---|---|---|---|---|
//! | 2 000 | 905 | 904 | 4 728 | 325 | 904 | 105 | 1 | 2 585 | 1 551 | 6 772 | 6 712 |
//! | 4 000 | 1 810 | 1 809 | 9 479 | 645 | 1 809 | 210 | 1 | 2 585 | 1 551 | 6 772 | 6 712 |
//! | 10 000 | 4 525 | 4 524 | 23 757 | 1 617 | 4 524 | 532 | 1 | 2 585 | 1 551 | 6 772 | 6 712 |
//!
//! Three repetitions of the whole table are byte-identical, so every figure here is a
//! **deterministic counter**, never a timing. The relations pinned are:
//!
//! * **one check per pre-emption** — the door is handed exactly one boundary per `Yield` park
//!   (`door.checks == yields`), never one per instruction: 904 / 1 809 / 4 524 checks against
//!   904 / 1 809 / 4 524 pre-emptions.
//!   That is **0.4518 / 0.4521 / 0.4524 checks per call** (`N + 1` calls), i.e. a 4 000-reduction
//!   slice carries 2 001 / 905 = 2.2110, 4 001 / 1 810 = 2.2105 and 10 001 / 4 525 = 2.2102 calls,
//!   so the loop's own price is 4 000 / 2.2110 = **1 809 reductions per call** (the run's own
//!   `reductions` counter reads 3 619 770 / 7 239 547 / 18 098 860 over the very same
//!   2 001 / 4 001 / 10 001 calls = 1 808.98 / 1 809.43 / 1 809.70);
//! * **the seven-eighths watermark rations it to about one boundary in eight** —
//!   `door.checks / 9 <= door.collections <= door.checks / 7`: the nursery is nearly exhausted
//!   (seven eighths used, the C's `ERTS_VBUMP_ALLOC_REDS` moment) at only every ~eighth slice
//!   end, and a boundary collection leaves the boundaries after it below the watermark.
//!   Measured **105/904 = one in 8.61** (N = 2 000), **210/1 809 = one in 8.61** (N = 4 000) and
//!   **532/4 524 = one in 8.50** (N = 10 000) — inside the band on both edges, which is why the
//!   assertion is the 7..9 band and never a nominal `checks / 8`.
//!   The door collects on **0.0525 / 0.0525 / 0.0532 calls**, i.e. **2.22 / 2.22 / 2.24 %** of a
//!   run that collects 2.3628 / 2.3692 / 2.3755 times per call: a **constant** share, not a
//!   rising one — the nursery's own capacity is constant (next section), so the allocation each
//!   slice performs fills it at the same rate at every N;
//! * **the shape's own totals are linear in N at a fixed per-call rate** — 4 728 / 9 479 /
//!   23 757 collections and 325 / 645 / 1 617 majors over a 5x range is **2.3628 / 2.3692 /
//!   2.3755 collections per call** (a 0.54 % spread over the whole range) and **0.0687 / 0.0680 /
//!   0.0681 majors per collection** (a 0.9 % spread), so the totals follow N and nothing else.
//!   The pin is that flatness, never a curated total: a per-call rate that rose with N is what a
//!   nursery or mature generation growing with the run would look like.
//!
//! # The nursery plateau, and the live figure the policy sizes it from
//!
//! `beam_gc`'s nursery policy is **live-set targeted**: a minor's live figure is its own copy
//! volume (`GcStats::words_copied`), the capacity it installs is
//! `HeapPolicy::young_minor_target_words(live) = next_words(live + need_reserve_words)` — floored
//! by `HeapPolicy::nursery_floor_words()` — and `HeapPolicy::young_after_collection` keeps an
//! existing capacity only inside `[target, YOUNG_SHRINK_FACTOR (8) x target]`, landing on the
//! target otherwise.
//! The shape's own live figure is read off its collector: the process's cumulative
//! `GcStats::words_copied` over its cumulative collection count at the last boundary it was
//! observable at, i.e. a **mean copy volume per collection** — 945 124 / 4 724 = **201 words**
//! (N = 2 000), 1 904 206 / 9 472 = **202** (N = 4 000) and 4 782 467 / 23 752 = **202**
//! (N = 10 000), constant as a constant live set must be.
//! It mixes the minors' young-only copy volume with the fullsweeps' whole-heap one, so it can
//! only **over**-state the young-only figure the minor rule consumes — the conservative direction
//! for the band asserted below.
//! That puts the minor target at `young_minor_target_words(201) = next_words(265) = 376` words and
//! the keep-band at `[376, 8 x 376 = 3 008]`; the sampled **2 585-word** capacity sits inside it,
//! so `young_after_collection(2 585, 201) == 2 585` — the capacity is a **fixed point** of the
//! rule and does not move with N (2 585 / 2 585 / 2 585).
//! It is not a capacity the rule *installs*: it is the space the BIF-result copy-out's
//! learned-hint growth gate left in place just before a copy-out
//! (`RuntimeContext::copy_out_hint` + `hint_grow_gate`, `src/context/calls.rs`), which every
//! collection since has kept — which is exactly why the pin is the fixed-point law rather than a
//! policy target for this live set.
//! The sampled young **occupancy** peak (1 551 words) is a slice-end reading, not the capacity it
//! is measured against: it sits below `2 585 x 7/8 = 2 262`, the watermark the door fires on,
//! because a sample is taken either just under that trigger or just after a collection emptied the
//! space — and the door's other gate, the growth seam's `collection_debt`, can fire it below the
//! watermark too.
//! The mature generation is flat for the same structural reason (6 772 capacity / 6 712 used at
//! every N): `HeapPolicy::mature_target_words` is `MATURE_TARGET_LIVE_MULTIPLE` (2) x its
//! post-fullsweep need — a property of the live set, never of the mature occupancy or capacity —
//! so it is a fixed point too, and the flatness rail is what guards that fixed point.
//!
//! **What this module cannot verify: the door-ON vs door-OFF A/B.**
//! Establishing that the door's collections *substitute for* the heap-admission door's rather
//! than adding to them needs a door-disabled build — a throwaway `#[cfg(test)]` early return
//! inside `collect_at_reds_boundary` (`src/context/gc/reds_door.rs`) — which is an edit outside
//! this measurement's scope, so **no `Δ` is claimed here**.
//! What the table *does* pin is the rationing itself (one firing in 7..9 boundaries, a constant
//! ~2.2 % share of the run's own collections), a collapsed retired-space chain (one young space of
//! 2 585 words at every N), the per-call linearity of the totals and the two capacities' fixed
//! points.
//! The per-call rates are off any closed form for three structural reasons: the door pins a
//! young-only `Process::collect_kind(CollectionKind::Minor)` first and escalates to the unpinned
//! `Process::collect` only when the promotion cannot fit the mature space (so a boundary firing
//! does not force a fullsweep of its own), the native baseline tier charges its blocks per block
//! (which sets the slice and pre-emption counts the per-call rates are expressed against), and
//! the BIF-result copy-out's learned hint grows the nursery **before** each copy — which is where
//! the plateau's own capacity comes from.
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

/// The iteration counts this module measures, spanning a 5x range so a per-call rate that grows
/// with N is visible as a drift rather than hidden in one row.
const COUNTS: [i64; 3] = [2_000, 4_000, 10_000];

/// How far a row's per-call collection rate may sit from the first row's before the shape stops
/// being **linear in N**.
///
/// The rate is a deterministic counter ratio, so it is flat by construction while the nursery and
/// the shape's live set are constant in N; the tolerance only absorbs the run's fixed tail (the
/// final clause, and the last slice's partial work) being amortized differently at each N.
/// Measured drift: **0.54 %** across the 5x range.
const PER_CALL_DRIFT: f64 = 0.03;

/// How far a row's `major / collections` share may sit from the first row's — the fullsweep
/// share is the mature generation's own fixed-point rail, so it too must be constant in N.
/// Measured drift: **0.9 %**.
const MAJOR_SHARE_DRIFT: f64 = 0.05;

/// One scheduled run's counters, split into the shape's totals and the boundary door's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Run {
    /// The shape's counters (the same set `super::GrowthCounters` carries).
    counters: GrowthCounters,
    /// The boundary door's own tallies for the very same run.
    door: BoundaryDoorCounters,
    /// The largest young-nursery **occupancy** (words) sampled at a slice boundary — the
    /// seven-eighths watermark moment the door fires on, never the capacity it is measured
    /// against.
    peak_young_used: usize,
    /// The process collector's cumulative copy volume (`GcStats::words_copied`) and its
    /// cumulative collection count at the last boundary the process was observable at — the two
    /// figures [`Run::live_words`] is the ratio of. (The run's own total is
    /// `counters.collections`, which is runtime-wide and can exceed this last sample.)
    copy_words: usize,
    process_collections: u64,
}

impl Run {
    /// The number of loop iterations `n` performs, plus the final clause — the denominator of
    /// every per-call figure.
    fn calls(n: i64) -> f64 {
        (n as f64) + 1.0
    }

    /// The shape's own **live figure** (words): its collector's cumulative copy volume over its
    /// cumulative collection count, rounded **up** — a mean copy volume per collection, which is
    /// the quantity [`beam_gc::HeapPolicy::young_minor_target_words`] is fed.
    ///
    /// It mixes a minor's young-only copy volume with a fullsweep's whole-heap one, so it can
    /// only over-state the young-only figure the minor rule consumes: rounding up therefore makes
    /// a target derived from it too large, never too small.
    fn live_words(&self) -> usize {
        assert!(
            self.process_collections > 0,
            "a measured row must have collected at least once to report a live figure: {self:?}"
        );
        self.copy_words.div_ceil(self.process_collections as usize)
    }
}

/// Runs `module:function/1` with argument `n` as a **scheduled process** on `rt` and returns
/// the shape's counters plus the boundary door's own tallies for the same run.
///
/// [`super::run_growth_counted`] is the same harness with the entry function hard-coded to
/// `run/1`; this one takes the function name (`slice:fib/1` has no `run/1`) and reads the
/// boundary door's counters around the run, which is the whole point of the pin below. The
/// nursery geometry **and** the collector's cumulative copy volume are sampled at every slice
/// boundary — the only window a live process's collector is readable from outside the runner (a
/// terminated process is removed from the table).
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
    let mut peak_young_used = 0usize;
    let mut copy_words = 0usize;
    let mut process_collections = 0u64;
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
            peak_young_used = peak_young_used.max(collector.young().used_words());
            let stats = collector.stats();
            copy_words = stats.words_copied;
            process_collections = stats.minor_collections + stats.major_collections;
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
        peak_young_used,
        copy_words,
        process_collections,
    }
}

/// The relative distance between two positive rates, as a fraction of the reference.
fn drift(reference: f64, value: f64) -> f64 {
    (value - reference).abs() / reference
}

/// **The boundary door is a per-pre-emption, watermarked site on the `gc_bound` churn shape —
/// not a per-call cost.**
///
/// Four relations, all structural facts of the runner, the collector and its heap policy (never
/// timings), pinned at the three iteration counts of [`COUNTS`]:
///
/// * **one boundary check per pre-emption** — `door.checks == yields`, exactly, at every row:
///   the door is reached from the runner's `Yield` arm and nowhere else, so its check count *is*
///   the shape's pre-emption count (904 / 1 809 / 4 524 checks against 905 / 1 810 / 4 525
///   slices), i.e. **0.4518 / 0.4521 / 0.4524 per call**. At 4 000 reductions a slice and
///   2.2110 / 2.2105 / 2.2102 calls per slice, that prices the loop at **1 809 reductions per
///   call**;
/// * **the seven-eighths watermark rations it to between one boundary in seven and one in
///   nine** — `door.checks / 9 <= door.collections <= door.checks / 7`. It is a *rate*, not an
///   identity: measured 105/904 = **one in 8.61**, 210/1 809 = **one in 8.61**, 532/4 524 =
///   **one in 8.50** — inside the band — so the door collects on **0.0525 / 0.0525 / 0.0532
///   calls** of a shape that collects 2.3628 / 2.3692 / 2.3755 times per call, i.e. **2.22 /
///   2.22 / 2.24 %** of the run's own collections;
/// * **the shape's own totals are linear in N at a fixed per-call rate** — each row's
///   `collections / calls` within [`PER_CALL_DRIFT`] of the first row's and each row's
///   `major / collections` within [`MAJOR_SHARE_DRIFT`] of the first row's, with the totals
///   strictly increasing in N. Measured **2.3628 / 2.3692 / 2.3755 collections per call** and
///   **0.0687 / 0.0680 / 0.0681 majors per collection**, a 0.54 % and a 0.9 % spread — the totals
///   follow N and nothing else, because the nursery's capacity and the shape's live set are both
///   constant (next bullet). A curated total would fail on any tuning change; a per-call rate that
///   *rose* with N is what a capacity growing with the run looks like, and that is what this pin
///   catches;
/// * **the nursery is one space on one live-set-derived capacity, and that capacity is the
///   policy rule's fixed point** — `peak_young_spaces == 1` at every row and one plateau at every
///   row (2 585 words), which the rule reproduces unchanged:
///   `policy.young_after_collection(peak, live) == peak`. The band is *derived* from
///   [`beam_gc::HeapPolicy`] for each row's own reported live figure ([`Run::live_words`]:
///   cumulative `GcStats::words_copied` over cumulative collections, a mean copy volume per
///   collection of **201 / 202 / 202** words): `nursery_floor_words() = 233 <= capacity` and
///   `young_minor_target_words(201) = next_words(265) = 376 <= capacity <=
///   YOUNG_SHRINK_FACTOR x 376 = 3 008` (`young_after_collection` installs the target below the
///   band and lands on it above the band, so a capacity inside the band is kept — which is what
///   makes the plateau flat in N instead of a ratchet). The mature generation is pinned flat as
///   well (6 772-word capacity at every row), the fixed point of `mature_target_words` —
///   `MATURE_TARGET_LIVE_MULTIPLE` (2) x its post-fullsweep need, a property of the live set and
///   never of the mature occupancy or capacity.
///
/// What the door contributes to those totals **cannot** be measured on this tree (a door-ON/OFF
/// A/B needs a door-disabled build — see the module docs), so nothing here claims a `Δ`; what is
/// pinned is the rationing itself, which is what rules out a per-call door cost.
#[test]
fn the_boundary_door_is_a_per_preemption_watermarked_site_on_the_churn_shape() {
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
             door_unpublished={} door_collections={} peak_young_spaces={} peak_young_capacity={} \
             peak_young_used={} copy_words={} process_collections={} live_words={} \
             peak_mature_capacity={} peak_mature_used={} reductions={}",
            run.counters.slices,
            run.counters.yields,
            run.counters.collections,
            run.counters.major,
            run.door.checks,
            run.door.unpublished,
            run.door.collections,
            run.counters.peak_young_spaces,
            run.counters.peak_young_capacity,
            run.peak_young_used,
            run.copy_words,
            run.process_collections,
            run.live_words(),
            run.counters.peak_mature_capacity,
            run.counters.peak_mature_used,
            run.counters.reductions,
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

    // 3. The totals are **linear in N**: the per-call collection rate and the fullsweep share are
    //    both constant across a 5x range, so each row's rate sits within its drift of the first
    //    row's, and the totals only ever grow.
    let (n0, r0) = rows[0];
    assert_eq!(n0, COUNTS[0], "the reference row must be the first count measured");
    let reference_rate = r0.counters.collections as f64 / Run::calls(n0);
    let reference_share = r0.counters.major as f64 / r0.counters.collections as f64;
    for (n, run) in &rows {
        let rate = run.counters.collections as f64 / Run::calls(*n);
        let share = run.counters.major as f64 / run.counters.collections as f64;
        assert!(
            drift(reference_rate, rate) <= PER_CALL_DRIFT,
            "the collections-per-call rate must stay flat in N (the nursery's capacity and the \
             shape's live set are constant): N={n} {rate:.4}/call vs N={n0} \
             {reference_rate:.4}/call, a {:.2} % drift (limit {:.0} %)",
            drift(reference_rate, rate) * 100.0,
            PER_CALL_DRIFT * 100.0
        );
        assert!(
            drift(reference_share, share) <= MAJOR_SHARE_DRIFT,
            "the fullsweep share must stay flat in N (the mature generation is a fixed point of \
             its own rule): N={n} {share:.4} majors/collection vs N={n0} {reference_share:.4}, a \
             {:.2} % drift (limit {:.0} %)",
            drift(reference_share, share) * 100.0,
            MAJOR_SHARE_DRIFT * 100.0
        );
    }
    for pair in rows.windows(2) {
        let [(n_a, a), (n_b, b)] = [pair[0], pair[1]];
        assert!(
            b.counters.collections > a.counters.collections
                && b.counters.major > a.counters.major
                && b.door.collections > a.door.collections,
            "more iterations ⇒ more collections, fullsweeps and door firings: N={n_a} vs \
             N={n_b} {a:?} {b:?}"
        );
    }

    // 4. The nursery stays one space on **one** live-set-derived capacity — and that capacity is
    //    the policy rule's fixed point, not a number this file happens to have read: flatness (the
    //    plateau may not move with N), the fixed-point law, and the rule's own band for each row's
    //    reported live figure (never below `nursery_floor_words()`, never above
    //    `YOUNG_SHRINK_FACTOR` x that live figure's `young_minor_target_words` target).
    //    The policy is the one these processes were spawned with — the runtime's heap
    //    configuration's ([`crate::config::HeapOptions::default`], whose `min_heap_words` is the C
    //    `H_DEFAULT_SIZE`) — so the floor and the targets are the rule that really ran.
    let policy = crate::config::HeapOptions::default().policy().clone();
    for (n, run) in &rows {
        assert_eq!(
            run.counters.peak_young_spaces, 1,
            "the door keeps the nursery chain collapsed: N={n} {run:?}"
        );
        assert_eq!(
            run.counters.peak_young_capacity, r0.counters.peak_young_capacity,
            "the nursery's settled capacity must not move with N: N={n} peak={} vs N={n0} \
             peak={}",
            run.counters.peak_young_capacity, r0.counters.peak_young_capacity
        );
        let live = run.live_words();
        let target = policy.young_minor_target_words(live);
        let floor = policy.nursery_floor_words();
        // The law the plateau must satisfy: the minor rule *reproduces* the capacity unchanged.
        // A ratchet (each collection installing a larger space) and an oscillation (each
        // collection installing a different one) both fail here, and neither is visible in a
        // single row's figure.
        assert_eq!(
            policy.young_after_collection(run.counters.peak_young_capacity, live),
            run.counters.peak_young_capacity,
            "the plateau must be a fixed point of the minor rule at live = {live} words: N={n} \
             peak={} is resized to {}",
            run.counters.peak_young_capacity,
            policy.young_after_collection(run.counters.peak_young_capacity, live)
        );
        // The rule's band, from the shape's OWN reported live figure: the capacity a collection
        // installs below the band is the target itself, above the band the target too, and inside
        // the band it is kept. A capacity that is a fixed point therefore has to sit inside it.
        assert!(
            run.counters.peak_young_capacity >= floor,
            "no capacity the nursery rule installs is below nursery_floor_words() = {floor}: \
             N={n} peak={}",
            run.counters.peak_young_capacity
        );
        assert!(
            run.counters.peak_young_capacity >= target,
            "a capacity below young_minor_target_words(live = {live}) = {target} is enlarged onto \
             it at the next collection, so the plateau cannot sit under it: N={n} peak={}",
            run.counters.peak_young_capacity
        );
        assert!(
            run.counters.peak_young_capacity <= beam_gc::YOUNG_SHRINK_FACTOR * target,
            "a kept nursery stays within YOUNG_SHRINK_FACTOR x young_minor_target_words(live = \
             {live}) ({target} x {} = {}): N={n} peak={}",
            beam_gc::YOUNG_SHRINK_FACTOR,
            beam_gc::YOUNG_SHRINK_FACTOR * target,
            run.counters.peak_young_capacity
        );
    }

    // 5. The mature generation is flat too, and for the same structural reason: its own rule
    //    sizes it from the live set a fullsweep measured (`mature_target_words`, whose multiple is
    //    `MATURE_TARGET_LIVE_MULTIPLE`), never from its occupancy or its capacity — so a constant
    //    live set makes the capacity a fixed point rather than a ratchet.
    for (n, run) in &rows {
        assert_eq!(
            run.counters.peak_mature_capacity, r0.counters.peak_mature_capacity,
            "the mature generation's settled capacity must not move with N: N={n} peak={} vs \
             N={n0} peak={}",
            run.counters.peak_mature_capacity, r0.counters.peak_mature_capacity
        );
    }
}
