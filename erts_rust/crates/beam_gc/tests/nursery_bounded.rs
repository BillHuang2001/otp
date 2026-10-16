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
//! mature generation's job). `HeapPolicy::young_after_collection` therefore never grows the
//! nursery merely because a collection ran — it grows it only when the words a collection
//! *left alive in it* exceed its capacity, shrinks it back towards the live set when a
//! collection kept only a small part of it, and otherwise keeps its size.
//!
//! The failure this file guards against is a nursery that grows one step per collection: a
//! nursery is only collected when it is **full**, so a "grow when full" rule grows it ~20 %
//! on *every* collection and its capacity ends up scaling with the number of collections —
//! which, for a long-running workload, means memory scaling with the workload size (the
//! `foreign` benchmark's RSS growing with `N`). Sizing the nursery from its *pre-collection*
//! fill is the same bug by another route: that count is ≈ the capacity, so the rule grows on
//! every collection.
//!
//! Every case below is driven through **both** mature strategies
//! ([`AnyCollector::Copying`] and [`AnyCollector::Immix`]) unless the case is about the
//! nursery alone. Every corpus is deterministic (no randomness) and every assertion is on a
//! deterministic count — **no timing is ever asserted**.

use beam_gc::prelude::{AnyCollector, GcConfig, HeapPolicy, MatureKind};
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
    /// Live + garbage must stay inside the nursery, so the round's allocation (60 + 150 =
    /// 210 words) fits the 233-word nursery with room to spare.
    const MAX_GARBAGE_TUPLES: usize = 50;
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
// 2. A live-heavy round does not blow the nursery up
// ---------------------------------------------------------------------------

/// Each round fills (almost) the whole nursery with **live** data and keeps it: the live set
/// is what grows, and the nursery's capacity must still not move. Under a "grow when full"
/// rule this is exactly the corpus that ramps the nursery up without bound.
#[test]
fn a_live_heavy_round_does_not_grow_the_nursery() {
    /// Tuples per round: 77 * 3 = 231 words, i.e. 231 of the 233-word nursery — full.
    const BATCH: i64 = 77;
    /// Rounds to run: the live set ends at 30 * 231 = 6930 words, ~30x the nursery.
    const ROUNDS: i64 = 30;

    for kind in STRATEGIES {
        let mut c = collector(kind);
        let mut roots: Vec<Eterm> = Vec::new();
        let mut young_caps: Vec<usize> = Vec::new();

        for round in 0..ROUNDS {
            for j in 0..BATCH {
                let a = round * 1000 + j;
                roots.push(tuple(&mut c, a));
            }
            // 77 three-word tuples = 231 of the 233 words: the round filled it.
            assert!(
                c.young().available_words() < 3,
                "{kind:?}: the round filled the nursery (round {round})"
            );

            c.collect(&mut roots).expect("collect");
            assert_eq!(c.young().used_words(), 0, "{kind:?}");

            young_caps.push(c.young().capacity_words());
        }

        // The live set now dwarfs the nursery ...
        let live_words = roots.len() * 3;
        assert!(
            live_words > 20 * NURSERY,
            "{kind:?}: the live set ({live_words} words) must far exceed the nursery"
        );

        // ... and the nursery is still exactly the size it started at: a round that filled it
        // to capacity *with live data* did not talk the collector into growing it.
        assert!(
            young_caps.iter().all(|&cap| cap == NURSERY),
            "{kind:?}: the nursery must not grow when a round fills it, got {young_caps:?}"
        );

        // The mature generation is where that live set had to go.
        assert!(
            c.heap_info().total_heap_size > NURSERY,
            "{kind:?}: the mature generation grew to hold the live set"
        );

        eprintln!(
            "{kind:?}: live set = {live_words} words, nursery capacity = {NURSERY} words \
             (steady)"
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
