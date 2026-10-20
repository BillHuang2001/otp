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

//! **Nursery boundedness** — the regression gate for runaway young-generation growth.
//!
//! A process's **nursery** (the young semi-space) is a *bounded* buffer: it is the space the
//! mutator churns through between collections, **not** a place for the live set (that is the
//! mature generation's job). Its size is therefore one **live-set target**, installed by the
//! collection that measured the live set it sizes from: a **fullsweep** installs the *whole*
//! live set's target ([`HeapPolicy::young_target_words`] = `next_words(2 * live + reserve)`,
//! it measures the live set exactly), and a **minor** installs the *young* live set's target
//! from the words it evacuated — its copy volume, `GcStats::words_copied`
//! ([`HeapPolicy::young_after_collection`] over [`HeapPolicy::young_minor_target_words`] =
//! `next_words(live + reserve)`). A minor may therefore **grow** the nursery, but only onto a
//! capacity derived from the live set, never from the capacity it had or the fill it reached.
//!
//! The failure this file guards against is a nursery that grows one step per collection: a
//! nursery is only collected when it is **full**, so a "grow when full" rule grows it ~20 %
//! on *every* collection and its capacity ends up scaling with the number of collections —
//! which, for a long-running workload, means memory scaling with the workload size (the
//! `foreign` benchmark's RSS growing with `N`). Sizing the nursery from its *pre-collection*
//! fill is the same bug by another route: that count is ≈ the capacity, so the rule grows on
//! every collection. Deriving it from the live set instead is idempotent — the same live set
//! yields the same capacity — so the nursery converges with the live set and stops; the
//! assertions below therefore hold every capacity to *a live-set target or the capacity it
//! already had*, never to a third value.
//!
//! Every case below is driven through **both** mature strategies
//! ([`AnyCollector::Copying`] and [`AnyCollector::Immix`]) unless the case is about the
//! nursery alone. Every corpus is deterministic (no randomness) and every assertion is on a
//! deterministic count — **no timing is ever asserted**.

use beam_gc::prelude::{
    AnyCollector, CollectionKind, GcConfig, HeapPolicy, MatureKind, YOUNG_SHRINK_FACTOR,
};
use beam_term::prelude::{make_small, Eterm};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The nursery capacity both strategies share: the crate default (233 words), which is
/// intentionally small so a bounded nursery is exercised against workloads that dwarf it.
const NURSERY: usize = 233;

/// A policy with a 233-word nursery, a 4096-word mature generation and the crate default
/// (`min_heap_words == 0`, i.e. *no floor*) — the bounded rule in its natural state, with
/// nothing pinning the nursery to a size.
fn policy() -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: 65535,
        initial_young_words: NURSERY,
        initial_mature_words: 4096,
        ..HeapPolicy::default()
    }
}

/// A collector of the requested mature-generation strategy over [`policy`].
fn collector(kind: MatureKind) -> AnyCollector {
    let config = GcConfig::copying(policy()).with_mature(kind);
    let c = AnyCollector::from_config(&config).expect("collector");
    assert_eq!(c.kind(), kind);
    c
}

/// Allocates a `{a, 2a}` tuple in `c`'s nursery.
fn tuple(c: &mut AnyCollector, a: i64) -> Eterm {
    c.young_mut()
        .heap_mut()
        .tuple(&[make_small(a), make_small(2 * a)])
        .expect("a nursery tuple")
}

/// Every strategy the file exercises.
const STRATEGIES: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

// ---------------------------------------------------------------------------
// 1. The nursery and the total heap plateau under a growing working load
// ---------------------------------------------------------------------------

/// A **bounded** live set that is rebuilt fresh every round, plus a per-round *garbage* batch
/// that grows with the round number: the working load climbs, the live set does not.
#[test]
fn the_nursery_and_total_heap_plateau_under_a_growing_working_load() {
    /// Live tuples re-built every round (a bounded, non-growing live set): 20 * 3 = 60 words.
    const LIVE_TUPLES: usize = 20;
    /// Garbage tuples at the last round: the per-round *churn* grows with the round number.
    /// Live + garbage must stay inside the nursery, so the round's allocation (60 + 120 =
    /// 180 words) fits the 233-word nursery with room to spare.
    ///
    /// The ceiling is also what keeps the **total** heap a plateau once the load saturates: a
    /// fullsweep sizes the mature generation from
    /// [`HeapPolicy::mature_target_words`](beam_gc::HeapPolicy::mature_target_words), i.e.
    /// `next_words(2 * (live (60) + young occupancy (60 + 3 * garbage) + reserve (64)))` =
    /// `next_words(368 + 6 * garbage)`, which sits at 610 words for `2 <= garbage <= 40` — so a
    /// garbage ceiling of 40 keeps every saturated round on the one mature target. A larger
    /// ceiling would step the growth table again inside the run and the total heap would move
    /// with the *working load*, which is exactly what this case must not do.
    const MAX_GARBAGE_TUPLES: usize = 40;
    /// Rounds to run. Long enough that a per-collection creep would be astronomical
    /// (`233 * 1.2^200` is ~10^17 words) and that the measured half is a true steady state.
    const ROUNDS: usize = 200;

    for kind in STRATEGIES {
        // `fullsweep_after == 0` rebuilds the mature generation from the live set on *every*
        // collection, so both halves of the heap accounting are directly comparable from
        // round to round (a minor-only cadence would let the mature generation accumulate
        // garbage between fullsweeps, which is a separate, unrelated effect).
        let full = HeapPolicy {
            fullsweep_after: 0,
            ..policy()
        };
        let config = GcConfig::copying(full).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(c.kind(), kind);
        assert_eq!(c.young().capacity_words(), NURSERY, "{kind:?}");

        let mut young_caps: Vec<usize> = Vec::new();
        let mut total_heaps: Vec<usize> = Vec::new();
        let mut allocated_words: usize = 0;
        let mut roots: Vec<Eterm> = Vec::new();

        for round in 0..ROUNDS {
            // A fresh live set: the previous round's graph is now unreachable garbage.
            roots.clear();
            for i in 0..LIVE_TUPLES as i64 {
                roots.push(tuple(&mut c, round as i64 * 1000 + i));
            }

            // The garbage batch grows with the round number, so the *working load* keeps
            // climbing even though the live set is constant.
            let garbage = (round / 4).min(MAX_GARBAGE_TUPLES);
            for i in 0..garbage as i64 {
                // Dropped immediately: unreachable, so the collection must reclaim it.
                let _ = tuple(&mut c, -i - 1);
            }

            allocated_words += (LIVE_TUPLES + garbage) * 3;

            c.collect(&mut roots).expect("collect");
            assert_eq!(
                c.young().used_words(),
                0,
                "{kind:?}: the nursery is emptied every round (round {round})"
            );

            young_caps.push(c.young().capacity_words());
            total_heaps.push(c.heap_info().total_heap_size);
        }

        // ---- the plateau ----
        // Ignore the first few rounds (allocator/steady-state warm-up) and require the whole
        // measured half to sit at a single value: the capacity must not grow per collection.
        let young = &young_caps[10..];
        let total = &total_heaps[10..];
        let young_min = *young.iter().min().expect("non-empty");
        let young_max = *young.iter().max().expect("non-empty");
        let total_min = *total.iter().min().expect("non-empty");
        let total_max = *total.iter().max().expect("non-empty");

        assert_eq!(
            young_min, young_max,
            "{kind:?}: the nursery capacity must plateau, got {young_caps:?}"
        );
        assert_eq!(
            total_min, total_max,
            "{kind:?}: the total heap must plateau, got {total_heaps:?}"
        );

        // The heap is bounded by the *live set*, not by the volume of allocation that has
        // flowed through it: over the run the mutator allocated `allocated_words` words into
        // a heap that never grew past `total_max`.
        assert!(
            total_max * 8 < allocated_words,
            "{kind:?}: the heap ({total_max} words) must stay far below the allocation volume \
             ({allocated_words} words) -- memory must not scale with the workload"
        );

        eprintln!(
            "{kind:?}: {ROUNDS} rounds, {allocated_words} words allocated -> nursery capacity \
             {young_min} words (steady), total heap {total_min} words (steady)"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. A live-heavy round sizes the nursery to the live set — and no further
// ---------------------------------------------------------------------------

/// Each round fills the whole nursery with **live** data and keeps it, so the live set climbs
/// past the nursery and the nursery is re-sized — but only ever onto a live-set target: the
/// **young** live set's target (`HeapPolicy::young_minor_target_words`) after a minor, the
/// **whole** live set's target ([`HeapPolicy::young_target_words`]) after a fullsweep. Under a
/// "grow when full" rule this is exactly the corpus that ramps the nursery up without bound: it
/// would step the growth table once per round; here every step lands on that round's target, so
/// the capacity is a function of the live set and converges with it.
#[test]
fn a_live_heavy_round_grows_the_nursery_to_the_live_set_target() {
    /// Tuples per round: 77 * 3 = 231 words, i.e. 231 of the 233-word birth nursery — full.
    const BATCH: i64 = 77;
    /// Rounds to run: the live set ends at 30 * 231 = 6930 words, ~30x the birth nursery.
    const ROUNDS: i64 = 30;

    for kind in STRATEGIES {
        let policy = policy();
        let mut c = collector(kind);
        let mut roots: Vec<Eterm> = Vec::new();
        let mut young_caps: Vec<usize> = Vec::new();
        let mut targets: Vec<usize> = Vec::new();

        for round in 0..ROUNDS {
            // The round's batch fits the nursery as it stands: the corpus never overruns the
            // buffer it is pressuring, so a capacity change is always the collector's decision.
            assert!(
                c.young().available_words() >= (BATCH * 3) as usize,
                "{kind:?}: the round's batch must fit the nursery (round {round})"
            );
            for j in 0..BATCH {
                let a = round * 1000 + j;
                roots.push(tuple(&mut c, a));
            }
            // 77 three-word tuples = 231 words: while the nursery is still the birth size the
            // round fills it, which is the pressure that makes a "grow when full" rule ramp.
            if c.young().capacity_words() == NURSERY {
                assert!(
                    c.young().available_words() < 3,
                    "{kind:?}: the round filled the {NURSERY}-word nursery (round {round})"
                );
            }

            let collection = c.collect(&mut roots).expect("collect");
            assert_eq!(c.young().used_words(), 0, "{kind:?}");

            // Every root is a 3-word `{a, 2a}` tuple, so the round's live set is exact and the
            // capacity the collector may install is derivable: one live-set target, of the
            // quantity the kind that ran measured.
            let live = roots.len() * 3;
            let promoted = collection.stats.words_copied;
            let target = match collection.kind {
                CollectionKind::Minor => policy.young_minor_target_words(promoted),
                CollectionKind::Major => policy.young_target_words(promoted),
            };
            let cap = c.young().capacity_words();
            let previous = young_caps.last().copied().unwrap_or(NURSERY);
            assert!(
                cap == previous || cap == target,
                "{kind:?} round {round}: the {:?} nursery capacity {cap} is neither the previous \
                 capacity {previous} nor the live set's target {target} (whole live set {live}, \
                 promoted {promoted})",
                collection.kind
            );
            // The floor is a real lower bound on every installed capacity, and the band bounds
            // how far a kept one may sit above its target.
            assert!(
                cap >= policy.nursery_floor_words(),
                "{kind:?} round {round}: {cap} words is below the policy's {} -word floor",
                policy.nursery_floor_words()
            );
            assert!(
                cap <= YOUNG_SHRINK_FACTOR * target,
                "{kind:?} round {round}: {cap} words is past the {}x band around its \
                 {target}-word target",
                YOUNG_SHRINK_FACTOR
            );
            // The rule is a fixed point at the live set it measured: a collection can never be
            // the start of a per-collection ratchet.
            let reapplied = match collection.kind {
                CollectionKind::Minor => policy.young_after_collection(cap, promoted),
                CollectionKind::Major => policy.young_after_fullsweep(cap, promoted),
            };
            assert_eq!(
                reapplied, cap,
                "{kind:?} round {round}: re-applying the rule at {cap} words / {promoted} live \
                 words answered {reapplied}, so {cap} is not a fixed point"
            );
            young_caps.push(cap);
            targets.push(target);
        }

        // The live set now dwarfs the birth nursery ...
        let live_words = roots.len() * 3;
        assert!(
            live_words > 20 * NURSERY,
            "{kind:?}: the live set ({live_words} words) must far exceed the nursery"
        );

        // ... the nursery followed it to the final round's target — the capacity the rule
        // answers for the live set that round measured ...
        assert_eq!(
            *young_caps.last().expect("non-empty"),
            *targets.last().expect("non-empty"),
            "{kind:?}: the nursery settles on the live set's target, got {young_caps:?}"
        );
        // ... and it never ran ahead of the whole live set's scale: every capacity the nursery
        // ever took is a live-set target of some round (or the birth size).
        let mut distinct = young_caps.clone();
        distinct.sort_unstable();
        distinct.dedup();
        for cap in &distinct {
            assert!(
                *cap == NURSERY || targets.contains(cap),
                "{kind:?}: the nursery took {cap} words, which is neither the birth size \
                 ({NURSERY}) nor any round's live-set target — it came from something other than \
                 the live set ({young_caps:?}, targets {targets:?})"
            );
        }
        // The capacity climbed, but only in live-set steps: a "grow when full" rule would step it
        // ~20 % on *every* one of the 30 rounds (30 distinct capacities). The bound is the number
        // of *targets* the live set itself produced, plus the birth size — a property of the
        // workload, never of the round count.
        let mut distinct_targets = targets.clone();
        distinct_targets.sort_unstable();
        distinct_targets.dedup();
        assert!(
            distinct.len() <= distinct_targets.len() + 1,
            "{kind:?}: the nursery took {} distinct capacities ({distinct:?}) but the live set \
             only ever had {} distinct targets ({distinct_targets:?}) — a per-collection creep",
            distinct.len(),
            distinct_targets.len()
        );
        assert!(
            distinct.len() * 2 < ROUNDS as usize,
            "{kind:?}: the nursery must grow in live-set steps, not once per round, got \
             {young_caps:?}"
        );
        assert!(
            distinct.len() >= 2,
            "{kind:?}: the live set grew past the nursery, so it must have been re-sized, got \
             {young_caps:?}"
        );

        // The mature generation is where that live set had to go.
        assert!(
            c.heap_info().total_heap_size > NURSERY,
            "{kind:?}: the mature generation grew to hold the live set"
        );

        eprintln!(
            "{kind:?}: live set = {live_words} words, nursery capacity = {} words (target), \
             steps {distinct:?}",
            young_caps.last().expect("non-empty")
        );
    }
}

// ---------------------------------------------------------------------------
// 3. A garbage-heavy collection shrinks the nursery back
// ---------------------------------------------------------------------------

/// A nursery a collection leaves almost entirely dead must fall back towards the live set: a
/// large nursery is a *buffered* nursery, and once it turns out to be far bigger than what
/// survives it must give the memory back.
#[test]
fn a_garbage_heavy_collection_shrinks_the_nursery() {
    /// Live tuples (3 words each → 9 live words).
    const LIVE: i64 = 3;
    /// Garbage tuples (3 words each → 90 garbage words), left unreachable.
    const GARBAGE: i64 = 30;
    /// The shrunk nursery the bounded rule settles on: `next_heap_size(need_reserve_words =
    /// 64, 0)` — the smallest curated size that covers the reserve.
    const SHRUNK_TO: usize = 90;

    let big = HeapPolicy {
        initial_young_words: 4096,
        initial_mature_words: 4096,
        ..policy()
    };

    for kind in STRATEGIES {
        let config = GcConfig::copying(big.clone()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(c.young().capacity_words(), 4096, "{kind:?}");

        let mut roots: Vec<Eterm> = Vec::new();
        for i in 0..LIVE {
            roots.push(tuple(&mut c, i));
        }
        for i in 0..GARBAGE {
            // Dropped: 90 words of dead nursery data the collection must reclaim.
            let _ = tuple(&mut c, -i - 1);
        }

        c.collect(&mut roots).expect("collect");
        assert_eq!(
            c.young().used_words(),
            0,
            "{kind:?}: the nursery is emptied"
        );

        assert_eq!(
            c.young().capacity_words(),
            SHRUNK_TO,
            "{kind:?}: a nursery the round left almost entirely dead must shrink back to the \
             live set's curated size"
        );

        eprintln!(
            "{kind:?}: 4096-word nursery after a garbage-heavy collection -> {} words",
            c.young().capacity_words()
        );
    }
}

// ---------------------------------------------------------------------------
// 4. A genuinely larger single request still grows the nursery — but bounded
// ---------------------------------------------------------------------------

/// The *other* half of the contract: the nursery must not be so rigid that a genuinely large
/// single allocation can never be met. [`AnyCollector::ensure_capacity`] is the explicit,
/// request-driven growth path, and it grows the nursery to (and no further than) the smallest
/// curated size that covers the request — after which the nursery is bounded again and the
/// next collection may bring it back down.
#[test]
fn a_larger_single_request_grows_the_nursery_to_just_cover_it() {
    const REQUEST: usize = 4096;

    for kind in STRATEGIES {
        let mut c = collector(kind);
        assert_eq!(c.young().capacity_words(), NURSERY, "{kind:?}");
        assert!(c.young().available_words() < REQUEST);

        let mut roots = vec![tuple(&mut c, 1)];

        // The grown target is exactly the policy's smallest curated size covering the
        // request — not "one step past whatever it had".
        let expected = policy().next_words(REQUEST, 0);
        c.ensure_capacity(REQUEST, &mut roots).expect("grow");
        assert!(
            c.young().available_words() >= REQUEST,
            "{kind:?}: the nursery now satisfies the request"
        );
        assert_eq!(
            c.young().capacity_words(),
            expected,
            "{kind:?}: the growth is bounded by the smallest curated size covering the request"
        );

        // A grown nursery is not a permanent one: a collection that keeps almost nothing
        // brings it straight back down.
        c.collect(&mut roots).expect("collect");
        assert!(
            c.young().capacity_words() < expected,
            "{kind:?}: the nursery shrinks back after a collection"
        );

        eprintln!(
            "{kind:?}: {NURSERY} words -> {} words for a {REQUEST}-word request -> {} words \
             after the next collection",
            expected,
            c.young().capacity_words()
        );
    }
}
