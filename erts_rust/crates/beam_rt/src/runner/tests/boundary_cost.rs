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
//! | N | slices | yields | collections | major | door checks | door collections | young spaces | young capacity | young occupancy | mature capacity | mature used | reductions |
//! |---|---|---|---|---|---|---|---|---|---|---|---|---|
//! | 2 000 | 391 | 390 | 5 307 | 331 | 390 | 44 | 1 | 2 585 | 2 135 | 4 185 | 4 092 | 1 561 925 |
//! | 4 000 | 781 | 780 | 10 747 | 653 | 780 | 83 | 1 | 2 585 | 2 135 | 4 185 | 4 092 | 3 123 825 |
//! | 10 000 | 1 953 | 1 952 | 27 112 | 1 624 | 1 952 | 196 | 1 | 2 585 | 2 135 | 4 185 | 4 092 | 7 809 540 |
//!
//! Three repetitions of the whole table are byte-identical, so every figure here is a
//! **deterministic counter**, never a timing. The relations pinned are:
//!
//! * **one check per pre-emption, and one pre-emption per full slice** — the door is reached from
//!   the runner's `Yield` arm and nowhere else, so its check count *is* the shape's pre-emption
//!   count (`door.checks == yields`), and the shape never blocks, so every slice but the returning
//!   one charges its **whole** 4 000-reduction budget: `slices == ceil(reductions / 4 000)` and
//!   `yields == slices - 1` exactly, at every row (390 / 780 / 1 952 checks against 391 / 781 /
//!   1 953 slices). That is **0.1949 / 0.1950 / 0.1952 checks per call** (`N + 1` calls), i.e. a
//!   slice carries 2 001 / 390 = 5.130, 4 001 / 780 = 5.129 and 10 001 / 1 952 = 5.124 calls, so
//!   the loop's own price is 4 000 / 5.13 = **780 reductions per call** — the run's `reductions`
//!   counter reads 1 561 925 / 3 123 825 / 7 809 540 over the very same 2 001 / 4 001 / 10 001
//!   calls = 780.57 / 780.76 / 780.88, flat to within 0.04 % over a 5× range;
//! * **the seven-eighths watermark rations it to about one boundary in eight** —
//!   `door.collections * 8 <= door.checks` is the structural half: the door's occupancy arm fires
//!   only where a slice end catches the young space **seven eighths** used (the C's
//!   `ERTS_VBUMP_ALLOC_REDS` moment), i.e. inside the top eighth of the occupancy range, so at
//!   most one boundary in eight can be sampled inside that window (`crate::context::gc`'s
//!   `reds_door`, whose gate is that watermark OR the growth seam's collection debt).
//!   Measured **44/390 = one in 8.86** (N = 2 000), **83/780 = one in 9.40** (N = 4 000) and
//!   **196/1 952 = one in 9.96** (N = 10 000) — inside the band at every row, with 38 / 116 / 384
//!   boundaries of slack to the upper edge.
//!   The observed rate sits *below* the window's own 1-in-8 because a firing empties the space:
//!   the ~8 boundaries of refill that follow it cannot re-cross the watermark, and the
//!   heap-admission doors in between reset the phase as well.
//!   The ratio drifts up slightly with N (8.86 → 9.96) because the slice length and the
//!   collection cycle are incommensurate — the phase a slice end samples walks through the cycle
//!   as N grows — which is why the lower edge is the measured refill length plus a margin
//!   (`door.checks <= door.collections * 12`).
//!   There is deliberately **no** target frequency: a thinner ration is strictly cheaper, a door
//!   firing only duplicating a collection the iteration was going to take anyway, so the lower
//!   edge is a non-no-op floor and never a figure the product aims at.
//!   The door collects on **0.0220 / 0.0207 / 0.0196 calls**, i.e. **0.83 / 0.77 / 0.72 %** of a
//!   run that collects **2.6522 / 2.6861 / 2.7109** times per call: under 1 % at every row — and
//!   that is the strongest form of "a constant share" available, because the share's denominator
//!   is the run's own collection count, which itself drifts 2.652 → 2.711 per call over the same
//!   5×, so a ratio of two independently drifting rates cannot be constant;
//! * **the shape's own totals are linear in N, at a fixed per-call rate** — 5 307 / 10 747 /
//!   27 112 collections and 331 / 653 / 1 624 majors over a 5× range is **2.6522 / 2.6861 /
//!   2.7109 collections and 0.1654 / 0.1632 / 0.1624 majors per call** at every row.
//!   They are still pinned as the exact measured table of [`MEASURED`], never as a formula.
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
//! observable at, i.e. a **mean copy volume per collection** — 1 260 638 / 5 301 = **238 words**
//! (N = 2 000), 2 552 344 / 10 737 = **238** (N = 4 000) and 6 448 458 / 27 108 = **238**
//! (N = 10 000), constant as a constant live set must be.
//! It mixes the minors' young-only copy volume with the fullsweeps' whole-heap one, so it can
//! only **over**-state the young-only figure the minor rule consumes — the conservative direction
//! for the band asserted below.
//! That puts the minor target at `young_minor_target_words(238) = next_words(238 + 64) =
//! next_words(302) = 376` words and the keep-band at `[376, max(3 000, 8 x 376 = 3 008) =
//! 3 008]`; the sampled
//! **2 585-word** capacity sits inside it, which is exactly why the capacity is a **fixed point**
//! of the rule and does not move with N (2 585 / 2 585 / 2 585): the plateau is a steady state of
//! `young_after_collection`'s *keeping* branch.
//! 2 585 is **not** a growth-table entry (`…376, 610, 987, 1 598, 2 586, 4 185, 6 772…`) and no
//! word is deducted from one — `beam_alloc::Heap::capacity` returns the constructed capacity
//! verbatim and `HeapSpace::capacity_words` only forwards it.
//! It is a **chain sum materialised into one space**: the growth seam
//! (`RuntimeContext::copy_out_hint` + `hint_grow_gate`) retires the current space and installs a
//! larger one over it, so the capacity a collection is handed is the chain's own sum
//! (`Nursery::capacity_words`, summed over every space), the keeping branch returns that sum
//! unchanged, and `install_collapsed` installs it as a single space — **2 585 = 987 + 1 598**
//! (the settled *single* space therefore need not be a table entry at all; only the spaces the
//! growth seam installs are).
//! What the band bounds is that sum: a chain summing past the upper edge (1 597 + 2 586 = 4 183 >
//! 3 008) makes the rule install the 376-word target instead of keeping the chain.
//! The sampled young **occupancy** peak (2 135 words) is the watermark moment the door fires on:
//! the watermark is `7/8` of the capacity that space had at that boundary, and the chain sum one
//! materialisation below the settled 2 585 is 1 597 — `1 597 x 7/8 = 1 397.4 < 2 135`, so a
//! boundary sampled at that capacity crosses, while `2 585 x 7/8 = 2 261.9` is the headroom the
//! settled space leaves the growth seam (which is what keeps the door from firing on every
//! boundary, next bullet).
//! The mature generation is flat for the same structural reason (**4 185** capacity / 4 092 used
//! at every N): `HeapPolicy::mature_target_words` is `MATURE_TARGET_LIVE_MULTIPLE` (2) x its
//! post-fullsweep need `live + young_occupancy + need_reserve_words`, a property of the live set
//! and of the nursery it has to receive a promotion from, never of the mature occupancy or
//! capacity, so it is a fixed point too.
//! Its **exact** value is deliberately not pinned as a single-row identity, because the occupancy
//! term is `Nursery::used_words()` **clamped** to `young_after_fullsweep(capacity, live)`
//! (`beam_gc::Collector::collect_major`), sampled at the governing fullsweep — a moment a
//! boundary sample does not observe (`peak_young_used` is the *watermark* moment the door fires
//! on, not the fullsweep's occupancy).
//! With `live = 238` the clamp keeps the settled capacity
//! (`young_after_fullsweep(2 585, 238) = 2 585`: its target is `next_words(540) = 610`, the
//! capacity is inside `[610, 8 x 610]` and `should_grow(2 585, 610)` is false), so the occupancy
//! is whatever the nursery held at that fullsweep, bounded above by 2 585.
//! The measured 4 185 is `next_words(2 x (238 + occupancy + 64))` for every occupancy in
//! `(991, 1 790]` — one growth-table entry covering that whole interval, which is exactly what
//! makes the capacity a fixed point of the rule rather than a reading of the nursery's own fill —
//! and the assertion beside the flatness rail re-derives that interval from each row's own
//! figures and requires the measured capacity to be its image.
//!
//! **What this module cannot verify: the door-ON vs door-OFF A/B.**
//! Establishing that the door's collections *substitute for* the heap-admission door's rather
//! than adding to them needs a door-disabled build — a throwaway `#[cfg(test)]` early return
//! inside `collect_at_reds_boundary` (`src/context/gc/reds_door.rs`) — which is an edit outside
//! this measurement's scope, so **no `Δ` is claimed here**.
//! What the table *does* pin is the rationing itself (one firing in 8.86–9.96 boundaries, under
//! 1 % of the run's own collections), a collapsed retired-space chain (one
//! young space of 2 585 words at every N) and the exact per-row totals of [`MEASURED`], which are
//! off any closed form for three structural reasons: the door pins a young-only
//! `Process::collect_kind(CollectionKind::Minor)` first and escalates to the unpinned
//! `Process::collect` only when the promotion cannot fit the mature space (so a boundary firing
//! does not force a fullsweep of its own), the native baseline tier charges its blocks per block
//! (which sets the slice and pre-emption counts the per-call rates are expressed against), and
//! the BIF-result copy-out's learned hint grows the nursery **before** each copy
//! (`RuntimeContext::copy_out_hint` + `hint_grow_gate`, `src/context/calls.rs`).
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

/// The measured rows this module pins: `(N, collections, major)` — the run's **exact** totals,
/// the per-N form of the ~2.65–2.71-collections / ~0.163-majors per call the module docs derive
/// (a fixed per-call rate, which is what a live-set-targeted nursery of constant capacity
/// implies).
/// Every figure the docs quote (slices, yields, the door's checks and collections, the nursery
/// and mature geometry, the copy volume the live figure comes from) is printed by the test's own
/// `eprintln!`, and three repetitions of the table are identical.
///
/// **Re-derive this table** from that output after any change to the runner's reduction pricing,
/// the door's gates/watermark, the collector's nursery **or mature** sizing policy, or the
/// BIF-result copy-out's growth gate — the files consulted are `src/runner/budget.rs`,
/// `src/context/gc/reds_door.rs`, `src/context/calls.rs` and `beam_gc`'s `policy.rs`.
const MEASURED: [(i64, u64, u64); 3] = [
    (2_000, 5_307, 331),
    (4_000, 10_747, 653),
    (10_000, 27_112, 1_624),
];

/// The measured young-nursery **capacity** (words) every row settles on — a MEASURED CONSTANT,
/// not a derived one, pinned exactly so a change to the nursery's geometry cannot pass unnoticed.
///
/// It is *not* a bare literal standing in for the rule: the plateau assertion beside it derives
/// the capacity's own band from [`beam_gc::HeapPolicy`] for each row's reported live figure
/// (`nursery_floor_words() <= capacity <= YOUNG_SHRINK_FACTOR x young_minor_target_words(live)`),
/// which is what makes 2 585 a *fixed point* of `young_after_collection` rather than a number
/// this file happens to have read once.
/// 2 585 is not a growth-table entry (`…376, 610, 987, 1 598, 2 586, 4 185, 6 772…`) and no word
/// is deducted from one: `beam_alloc::Heap::capacity` returns the constructed capacity verbatim
/// and `HeapSpace::capacity_words` only forwards it.
/// It is the capacity the nursery **chain** held, materialised into one space —
/// `Nursery::collapse` sizes `young_after_collection` from `Nursery::capacity_words` (summed over
/// every space), the keeping branch returns that sum unchanged, and `install_collapsed` installs
/// it as a single space: **2 585 = 987 + 1 598**.
/// Only the spaces the copy-out's growth seam installs are table entries, and a chain summing past
/// the band's upper edge (8 x 376 = 3 008) makes the rule install the 376-word target instead.
const MEASURED_YOUNG_CAPACITY: usize = 2_585;

/// **The boundary door is a per-pre-emption, watermarked site on the `gc_bound` churn shape —
/// not a per-call cost.**
///
/// Four relations, all structural facts of the runner, the collector and its heap policy (never
/// timings), pinned at the three iteration counts of [`MEASURED`]:
///
/// * **one boundary check per pre-emption, and one pre-emption per full slice** —
///   `door.checks == yields`, exactly, at every row: the door is reached from the runner's
///   `Yield` arm and nowhere else, so its check count *is* the shape's pre-emption count
///   (390 / 780 / 1 952 checks against 391 / 781 / 1 953 slices), i.e. **0.1949 / 0.1950 /
///   0.1952 per call**. The shape never blocks, so every slice but the returning one charges its
///   whole budget — the two identities `slices == ceil(reductions / 4 000)` and
///   `yields == slices - 1` are asserted beside it, which is what lets the slice count be read as
///   a function of the run's reductions. At 2 001 / 390 = 5.130, 4 001 / 780 = 5.129 and
///   10 001 / 1 952 = 5.124 calls per slice, that prices the loop at **780 reductions per call**
///   (the run's own `reductions` counter: 780.57 / 780.76 / 780.88, flat to 0.04 % over a 5×
///   range);
/// * **the seven-eighths watermark rations it to about one boundary in nine** —
///   `door.collections * 8 <= door.checks` is the structural half: the door may never collect on
///   more than one boundary in eight, because a firing empties the space and `2 585 x 7/8 =
///   2 261.9` words of allocation have to accumulate before the watermark (`crate::context::gc`'s
///   `reds_door`, whose gate is that watermark OR the growth seam's collection debt) can be
///   crossed again.
///   Measured **44/390 = one in 8.86** (N = 2 000), **83/780 = one in 9.40** (N = 4 000) and
///   **196/1 952 = one in 9.96** (N = 10 000), with 38 / 116 / 384 boundaries of slack to that
///   edge.
///   The observed rate sits *below* the window's own 1-in-8 because a firing empties the space:
///   the ~8 boundaries of refill that follow it cannot re-cross the watermark, and the
///   heap-admission doors in between reset the phase as well, so the ratio drifts up with N
///   (8.86 → 9.96) as a slice end's phase walks through the collection cycle.
///   The lower edge is therefore a **non-no-op floor** — `door.checks <= door.collections * 12`,
///   the measured refill length plus a margin — and never a frequency the product aims at: a
///   thinner ration is strictly cheaper, because a door firing only duplicates a collection the
///   iteration was going to take anyway;
/// * **the shape's own totals are a measured table** ([`MEASURED`]), pinned exactly per row:
///   5 307 / 331 at N = 2 000, 10 747 / 653 at N = 4 000 and 27 112 / 1 624 at N = 10 000, i.e.
///   **2.6522 / 2.6861 / 2.7109 collections and 0.1654 / 0.1632 / 0.1624 majors per call** —
///   linear in N at a fixed per-call rate, because the nursery's capacity and the shape's live
///   set are both constant (next bullet).
///   The door's own share of them is **0.83 / 0.77 / 0.72 %** (0.0220 / 0.0207 / 0.0196 firings
///   per call), asserted as an absolute rail under 1 %: the share's *denominator* is the run's
///   own collection count, which itself drifts 2.652 → 2.711 per call over the same 5×, so a
///   ratio of two independently drifting rates cannot be expected to be constant — an absolute
///   bound is both stronger and honest;
/// * **the nursery is one space on one live-set-derived capacity, and that capacity is the
///   policy rule's fixed point** — `peak_young_spaces == 1` at every row and the same
///   [`MEASURED_YOUNG_CAPACITY`] (2 585 words) at every row, with the band *derived* from
///   [`beam_gc::HeapPolicy`] for each row's own reported live figure ([`Run::live_words`]:
///   cumulative `GcStats::words_copied` over cumulative collections, a mean copy volume per
///   collection of **238** words at all three rows): `nursery_floor_words() = 233 <=
///   capacity` and `young_minor_target_words(238) = next_words(302) = 376 <= capacity <=
///   `YOUNG_SHRINK_FACTOR x 376 = 3 008` (`young_after_collection` installs the target below the
///   band and lands on it above the band, so a capacity inside the band is kept — which is what
///   makes the plateau flat in N instead of a ratchet). The mature generation is pinned flat as
///   well (4 185-word capacity at every row), the fixed point of `mature_target_words` —
///   `MATURE_TARGET_LIVE_MULTIPLE` (2) x its post-fullsweep need `live + young occupancy +
///   need_reserve_words`. Its exact value is not a single-row identity (the fullsweep's clamped
///   `young_occupancy` is not a boundary-sampled quantity — see the module docs), so beside the
///   flatness rail the test re-derives from each row's own figures the **occupancy interval** the
///   measured capacity is the image of and asserts the row's figures are consistent with it.
///
/// What the door contributes to those totals **cannot** be measured on this tree (a door-ON/OFF
/// A/B needs a door-disabled build — see the module docs), so nothing here claims a `Δ`; what is
/// pinned is the rationing itself, which is what rules out a per-call door cost.
#[test]
fn the_boundary_door_is_a_per_preemption_watermarked_site_on_the_churn_shape() {
    let mut rows = Vec::new();
    for (n, _want_collections, _want_major) in MEASURED {
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

    // Pin the measured totals of [`MEASURED`] only **after** every row has been gathered and
    // printed, so a re-derivation run always sees the whole table rather than stopping at the
    // first row that moved. Each assertion is still the exact per-row equality (never a band).
    for ((n, want_collections, want_major), (_n, run)) in MEASURED.iter().zip(rows.iter()) {
        assert_eq!(
            (run.counters.collections, run.counters.major),
            (*want_collections, *want_major),
            "the measured totals of `MEASURED` must be what this tree runs (re-derive the table \
             from the line above, never relax it): N={n} {run:?}"
        );
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

    // 2. The slice accounting the door's rate is expressed against, the iteration's own price, and
    //    the watermark's ration — every one of them a structural fact of the runner, never a
    //    timing.
    let (n0, r0) = rows[0];
    let budget = beam_common::CONTEXT_REDS as u64;
    for (n, run) in &rows {
        let calls = Run::calls(*n);
        let price = run.counters.reductions as f64 / calls;
        let checks_per_call = run.door.checks as f64 / calls;
        let door_per_call = run.door.collections as f64 / calls;
        // The shape never blocks, so **every** slice but the returning one charges its whole
        // reduction budget — which makes the slice and pre-emption counts a function of the run's
        // own `reductions` instead of an independent reading.
        assert_eq!(
            run.counters.slices as u64,
            run.counters.reductions.div_ceil(budget),
            "every pre-empted slice charges the whole {budget}-reduction budget (the returning one \
             may charge less), so slices == ceil(reductions / {budget}): N={n} slices={} \
             reductions={}",
            run.counters.slices,
            run.counters.reductions
        );
        assert_eq!(
            run.counters.yields as u64 + 1,
            run.counters.slices as u64,
            "only the last slice returns, so every other slice ends in a `Yield` park: N={n} \
             slices={} yields={}",
            run.counters.slices,
            run.counters.yields
        );
        // One churn iteration prices at ~780 reductions: three 256-element structures, two of them
        // through registered BIFs and one through the interpreted `build_list/2` recursion, at the
        // native tier's per-block and per-dispatch charges. The band is +-10 % — wide enough for a
        // block-boundary change in that charge table, narrow enough to refuse a doubling; the
        // cross-row rail below is the host-load-independent half of the same claim.
        assert!(
            (700.0..=860.0).contains(&price),
            "one churn iteration prices at ~780 reductions, i.e. a {budget}-reduction slice carries \
             ~5.13 calls: N={n} {price:.2} reds/call"
        );
        // Everything else about the boundary rate follows from those identities — one check per
        // pre-emption, one pre-emption per full slice — so the rate is the price over the budget
        // and nothing else. (The slack is the returning slice's partial charge, i.e. one call.)
        assert!(
            (checks_per_call - price / budget as f64).abs() <= 1.0 / calls,
            "the boundary rate is the iteration's price over the slice budget: N={n} \
             {checks_per_call:.6}/call vs {:.6}/call",
            price / budget as f64
        );
        // The watermark rations it. The **upper** edge is the structural half: a firing empties
        // the space, so `7/8` of the settled capacity (`2 585 x 7/8 = 2 261.9` words) has to be
        // allocated again before the gate can re-cross — never more than one firing per eight
        // boundary checks. The **lower** edge is only a non-no-op floor at one in twelve (the
        // measured refill length plus a margin): the observed ratio drifts 8.86 -> 9.96 as N grows
        // because the slice length and the collection cycle are incommensurate, and there is
        // deliberately no target frequency — a thinner ration is strictly cheaper, a door firing
        // only duplicating a collection the iteration was going to take anyway.
        assert!(
            run.door.collections * 8 <= run.door.checks
                && run.door.checks <= run.door.collections * 12,
            "the watermark rations the door to between one boundary in 12 and one in 8: N={n} \
             {}/{}",
            run.door.collections,
            run.door.checks
        );
        assert!(
            door_per_call <= checks_per_call,
            "at most one collection per boundary: N={n} {door_per_call:.6} > {checks_per_call:.6}"
        );
        // The door is a rounding error on the shape's own collection count, and it stays one:
        // measured 0.83 / 0.77 / 0.72 % (0.0220 / 0.0207 / 0.0196 firings per call). An **absolute**
        // rail rather than a relative one, because the share's denominator is the run's own
        // collection rate, which itself drifts 2.652 -> 2.711 per call over the same 5x — a ratio
        // of two independently drifting rates cannot be expected to be constant.
        let share = run.door.collections as f64 / run.counters.collections as f64;
        assert!(
            share < 0.01,
            "the door's share of the run's collections must stay under 1 %: N={n} {share:.5}"
        );
    }

    // 3. The per-call rates are the **same** at every N — and that, rather than a fresh constant,
    //    is what "linear in N at a fixed per-call rate" means: a live-set-targeted nursery of
    //    constant capacity and a loop with a fixed own price may not do more work per iteration
    //    just because the loop runs longer. Each row is a ratio of two of the run's own rates, so
    //    host load and thread count cancel out of it.
    fn spread(rows: &[(i64, Run)], pick: fn(i64, &Run) -> f64) -> f64 {
        let values: Vec<f64> = rows.iter().map(|(n, run)| pick(*n, run)).collect();
        let low = values.iter().copied().fold(f64::INFINITY, f64::min);
        let high = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        (high / low) - 1.0
    }
    let price_spread = spread(&rows, |n, run| run.counters.reductions as f64 / Run::calls(n));
    assert!(
        price_spread <= 0.001,
        "the iteration's reduction price must be identical at every N (a fixed iterations-per-\
         slice, never a per-call work increase): spread {:.5}",
        price_spread
    );
    let checks_spread = spread(&rows, |n, run| run.door.checks as f64 / Run::calls(n));
    assert!(
        checks_spread <= 0.005,
        "the boundary rate per call must be identical at every N: spread {:.5}",
        checks_spread
    );
    // The shape's totals are linear in N to within the collection cycle's phase: the *rate* is
    // fixed (above, and the nursery capacity below), so what is left is where a slice end happens
    // to land in the cycle, worth ~2 % of the rate over a 5x range.
    let collections_spread = spread(&rows, |n, run| run.counters.collections as f64 / Run::calls(n));
    assert!(
        collections_spread <= 0.03,
        "a constant live set on a constant nursery collects at a fixed rate per call: spread {:.5}",
        collections_spread
    );
    let major_spread = spread(&rows, |n, run| run.counters.major as f64 / Run::calls(n));
    assert!(
        major_spread <= 0.03,
        "the fullsweep cadence is `MATURE_TARGET_LIVE_MULTIPLE` promotions per space, so it is a \
         fixed rate per call too: spread {:.5}",
        major_spread
    );

    // 4. The nursery stays one space on **one** live-set-derived capacity — and that capacity is
    //    the policy rule's fixed point, not a number this file happens to have read: flatness (the
    //    plateau may not move with N), the measured constant it settles on, and the rule's own
    //    band for each row's reported live figure (never below `nursery_floor_words()`, never
    //    above `YOUNG_SHRINK_FACTOR` x that live figure's `young_minor_target_words` target).
    //    The policy is the one the measured children were **created** with (a plain `spawn`'s
    //    heap options), not `HeapPolicy::default()`: the two differ in `min_heap_words`, and it
    //    is the spawn policy whose floor and targets the nursery rule ran under.
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
        assert_eq!(
            run.counters.peak_young_capacity, MEASURED_YOUNG_CAPACITY,
            "the measured young-capacity plateau (re-derive it from the line above): N={n} \
             peak={}",
            run.counters.peak_young_capacity
        );
        // The rule's band, from the shape's OWN reported live figure (a mean copy volume per
        // collection — see `Run::live_words`): the capacity a collection installs below the band
        // is the target itself, above the band the target too, and inside the band it is kept.
        // A capacity that is a fixed point therefore has to sit inside it.
        let live = run.live_words();
        let target = policy.young_minor_target_words(live);
        let floor = policy.nursery_floor_words();
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
    //    `MATURE_TARGET_LIVE_MULTIPLE`) plus the **young occupancy** it has to receive a promotion
    //    from — never from its own occupancy or capacity — so a constant live set and a constant
    //    nursery make the capacity a fixed point rather than a ratchet.
    //    Its exact value is not pinned as a single-row identity, because the occupancy term is
    //    `Nursery::used_words()` **clamped** to `young_after_fullsweep(capacity, live)`
    //    (`beam_gc::Collector::collect_major`), sampled at the governing fullsweep — a moment a
    //    slice-boundary sample does not observe (`peak_young_used` is the *watermark* moment the
    //    door fires on). What is derivable is the **interval** of occupancies whose image is the
    //    measured capacity, and it is non-empty, non-degenerate and entirely below the settled
    //    capacity — i.e. the mature space is sized by the nursery it must receive, not by its own
    //    fill. Each element is additionally asserted to be a fixed point of
    //    `mature_after_fullsweep`, which is the property the flatness rail depends on.
    for (n, run) in &rows {
        assert_eq!(
            run.counters.peak_mature_capacity, r0.counters.peak_mature_capacity,
            "the mature generation's settled capacity must not move with N: N={n} peak={} vs \
             N={n0} peak={}",
            run.counters.peak_mature_capacity, r0.counters.peak_mature_capacity
        );
        let live = run.live_words();
        let settled_young = run.counters.peak_young_capacity;
        assert_eq!(
            policy.young_after_fullsweep(settled_young, live),
            settled_young,
            "the fullsweep's clamp keeps this shape's settled nursery (its target is far below \
             it), so the occupancy term is the nursery's own fill bounded by {settled_young}: \
             N={n} live={live}"
        );
        let occ_low = policy.young_minor_target_words(live);
        let admissible: Vec<usize> = (occ_low..=settled_young)
            .filter(|occ| {
                policy.mature_target_words(live, *occ) == run.counters.peak_mature_capacity
            })
            .collect();
        assert!(
            !admissible.is_empty(),
            "the measured mature capacity {} must be `mature_target_words(live = {live}, young \
             occupancy)` for some occupancy the clamp admits ({}..={settled_young}): N={n}",
            run.counters.peak_mature_capacity,
            occ_low
        );
        for occ in &admissible {
            assert_eq!(
                policy.mature_after_fullsweep(run.counters.peak_mature_capacity, live, *occ),
                run.counters.peak_mature_capacity,
                "an occupancy whose target IS the measured capacity must leave the rule keeping \
                 it, or the mature space would ratchet: N={n} occupancy={occ} live={live}"
            );
        }
    }
}
