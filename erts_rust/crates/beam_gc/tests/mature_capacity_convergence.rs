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

//! **The mature generation stays proportional to the LIVE set** — the regression gate for the
//! fullsweep sizing rule.
//!
//! `tests/steady_state.rs` pins the whole quiescent heap (nursery, mature capacity, mature
//! occupancy and total heap) under the **default** cadence, and `tests/nursery_bounded.rs` /
//! `tests/memory_overhead.rs` / `tests/retained_vs_live.rs` isolate the nursery or the retained
//! word accounting under a *forced* fullsweep-per-round cadence (`fullsweep_after == 0`). This
//! file is the one that isolates the **mature capacity** under the default cadence
//! (`fullsweep_after = 65535`, every other sizing field at its [`HeapPolicy::default`] value)
//! over a long-lived, *bounded* live set rebuilt fresh every round, and asserts the mature
//! generation's capacity converges to a **constant**.
//!
//! The mature *occupancy* is a different story and is pinned per strategy instead of as a
//! constant: a copying minor promotes the nursery's live words *onto the top of* the mature
//! space without reclaiming it, so the occupancy cycles one live set per minor until the
//! escalation test fires a fullsweep ([`COPYING_OCCUPANCY_CYCLE`]); the mark-region region is
//! reclaimed **in place** at every fullsweep and then holds exactly the live set, so its
//! occupancy is the single value [`IMMIX_OCCUPANCY`]. Both are bounded — neither grows without
//! limit — which is the property asserted below.
//!
//! That is exactly the property a fullsweep must have and the one a "size the fresh mature space
//! from the pre-collection occupancy" rule breaks. Between two fullsweeps the mature generation
//! is *not* reclaimed (that is the whole point of a generational collector): the graph a round
//! promoted becomes garbage the moment the next round's root set replaces it, so by the time the
//! cadence fires the mature space is full of dead words. Sizing the fresh space from that
//! occupancy therefore asks the policy for room for the *garbage* as well as the live set, and
//! `HeapPolicy::mature_after_fullsweep`'s grow bracket (`capacity < target`) fires on every single
//! fullsweep once the target is derived from that occupancy — it exceeds the capacity at every
//! round — so the capacity creeps up one growth-table step per fullsweep, without bound, for a
//! live set that never changes. The fullsweep must instead be sized from the **exact live word
//! count** of the graph it evacuates (the C `adjust_after_fullsweep`'s `HEAP_TOP(p) - HEAP_START(p)`).
//!
//! Every corpus here is deterministic (no randomness) and every **assertion** is a deterministic
//! count — capacities and occupancies. **No timing is ever asserted** and none is measured; the
//! converged numbers are printed with `-- --nocapture` for a human reader only.
//!
//! Driven through **both** mature strategies ([`AnyCollector`] `Copying` and `Immix`), which
//! share the nursery and differ only in where the mature generation lives: the copying space
//! sizes itself in whole words, the mark-region space in whole 16-word lines.

use beam_gc::prelude::{
    AnyCollector, GcConfig, HeapPolicy, MatureKind, BLOCK_WORDS, LINE_WORDS,
    MATURE_TARGET_LIVE_MULTIPLE,
};
use beam_term::prelude::{make_small, signed_value, tuple_element, tuple_size, Eterm};

// ---------------------------------------------------------------------------
// The corpus and the policy
// ---------------------------------------------------------------------------

/// The default nursery capacity (`HeapPolicy::default().initial_young_words`), in words.
const DEFAULT_NURSERY: usize = 233;

/// Words per `{a, 2a}` tuple: the frozen layout is `arity + 1` (a header plus two elements).
const TUPLE_WORDS: usize = 3;

/// Live tuples re-built **fresh** every round: a bounded, non-growing live set of 60 words.
const LIVE_TUPLES: i64 = 20;

/// Garbage tuples built every round and dropped immediately (never referenced): 150 words.
///
/// `LIVE_TUPLES + GARBAGE_TUPLES` is 70 tuples = 210 words, which fits the 233-word default
/// nursery, so every round's allocation succeeds before the round's collection.
const GARBAGE_TUPLES: i64 = 50;

/// Rounds run before measurement (warm-up to the sizing fixed point).
const WARMUP: usize = 20;

/// Long-running measured rounds. Under a per-fullsweep creep the capacity would be astronomical
/// by now (`233 * 1.2^200` is ~10^17 words), so a single constant across this half is a genuine
/// steady state.
const MEASURED: usize = 200;

/// The stable live set in words (the graph the converged heap must cope with).
const LIVE_WORDS: usize = LIVE_TUPLES as usize * TUPLE_WORDS;

/// One round's total allocation — the **nursery occupancy** a collection samples and the figure
/// the mature sizing rule folds into `need_after` (`live + young occupancy + reserve`).
///
/// It is *not* the escalation test's input: a minor is admitted while the mature space's room
/// covers the nursery's **live** promotion ([`Collector::collect`](beam_gc::Collector::collect)'s
/// `minor_promotion_fits`, the C `erl_gc.c:1486-1488` room test), which is [`LIVE_WORDS`] here —
/// see [`occupancy_steps`] for why that distinction sets the cycle's length.
const ROUND_WORDS: usize = (LIVE_TUPLES as usize + GARBAGE_TUPLES as usize) * TUPLE_WORDS;

/// [`HeapPolicy::default`]'s `need_reserve_words`; the test asserts the live policy agrees.
const NEED_RESERVE_WORDS: usize = 64;
/// Every mature strategy: the copying space and the mark-region space.
const STRATEGIES: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// The **copying** mature occupancy's converged cycle: one live set per minor since the last
/// fullsweep. A copying minor promotes the nursery's live words onto the top of the mature
/// space without reclaiming it, so the occupancy steps one live set per minor until
/// [`Collector::collect`](beam_gc::Collector::collect)'s escalation test — the mature space's
/// remaining room below the nursery's **live promotion** (the C's `mature_size` room test,
/// `erl_gc.c:1486-1488`) — fires a fullsweep, which evacuates young *and* mature into a fresh
/// space holding exactly the 60-word live set and starts the cycle over at 60.
///
/// The cycle's **length** is the amortisation [`MATURE_TARGET_LIVE_MULTIPLE`] buys, and it is
/// derived rather than listed (see [`occupancy_steps`]): the converged capacity is
/// `mature_target_words(LIVE_WORDS, ROUND_WORDS)` = `next_words(2 * (60 + 210 + 64))` = 987
/// words, the post-collection occupancy takes the values `live, 2 * live, …`, and a minor is
/// admitted while `capacity - used >= live`, so the largest value in the cycle is
/// `capacity / live = 987 / 60 = 16` live sets (960 words, leaving 27 words — below the 60-word
/// live promotion). Sixteen occupancy values, fifteen promotions.
const COPYING_OCCUPANCY_STEPS: usize = occupancy_steps(mature_target(), LIVE_WORDS);
/// The cycle as the `COPYING_OCCUPANCY_STEPS` successive multiples of the live set.
const COPYING_OCCUPANCY_CYCLE: [usize; COPYING_OCCUPANCY_STEPS] = {
    let mut cycle = [0usize; COPYING_OCCUPANCY_STEPS];
    let mut step = 0;
    while step < COPYING_OCCUPANCY_STEPS {
        cycle[step] = (step + 1) * LIVE_WORDS;
        step += 1;
    }
    cycle
};

/// The number of distinct occupancy values in a converged cycle, derived from the rule rather
/// than measured.
///
/// The occupancy starts at one `live` (what the fullsweep left) and grows one `live` per minor,
/// and a minor is admitted while the mature space's room covers the nursery's **live promotion**
/// — [`Collector::collect`](beam_gc::Collector::collect)'s `minor_promotion_fits`, whose second
/// arm is `available >= young_promotion_words`, i.e. `capacity - used >= live`. The cycle is
/// therefore `live, 2 * live, …` up to the largest multiple of `live` that still fits,
/// `capacity / live` — the first value past it would need `capacity - used < live` and the
/// collection escalates to a fullsweep, which resets the occupancy to `live`.
///
/// Note the input is the **live** set, not the round's allocation: a fill-keyed test
/// (`capacity - used >= round`) would stop the cycle `round / live` values early, which is what
/// this test used to pin before the escalation predicate was changed to read the live promotion
/// (`src/collector.rs::minor_promotion_fits`).
const fn occupancy_steps(capacity: usize, live: usize) -> usize {
    if live == 0 {
        // Degenerate input: a live set of no words never fills the space, so the cycle is the
        // single post-fullsweep value. (Guards the division; the corpora here are non-empty.)
        return 1;
    }
    let steps = capacity / live;
    if steps == 0 {
        1
    } else {
        steps
    }
}

/// The converged mature capacity this corpus' fullsweep asks for —
/// `next_words(MATURE_TARGET_LIVE_MULTIPLE * (LIVE_WORDS + ROUND_WORDS + need_reserve_words))`,
/// evaluated here as the growth table's curated size at or above `2 * (60 + 210 + 64) = 668`,
/// i.e. 987 words.
const fn mature_target() -> usize {
    let need_after = LIVE_WORDS + ROUND_WORDS + NEED_RESERVE_WORDS;
    next_curated_size(MATURE_TARGET_LIVE_MULTIPLE * need_after)
}

/// `HeapPolicy::next_words(size, 0)` over the curated growth table, in `const` form: the
/// Fibonacci + 1 stage up to 23 entries, then + 20 % steps. (`beam_gc::heap_sizes()` is the
/// runtime authority; this is the same recurrence, so a table change shows up here as a
/// mismatch of the printed figures rather than a silent drift.)
const fn next_curated_size(size: usize) -> usize {
    // Fibonacci + 1 from 12, 38 as `build_heap_sizes` does it.
    let mut a = 12usize;
    let mut b = 38usize;
    if size <= a {
        return a;
    }
    if size <= b {
        return b;
    }
    let mut i = 2;
    while i < 23 {
        let next = a + b + 1;
        a = b;
        b = next;
        if size <= b {
            return b;
        }
        i += 1;
    }
    // The + 20 % stage.
    let mut last = b;
    loop {
        let next = last + last / 5;
        if size <= next {
            return next;
        }
        last = next;
    }
}

/// One mark-region live set: 20 one-line tuples = 20 lines of 16 words = 320 words.
const IMMIX_LIVE_WORDS: usize = LIVE_TUPLES as usize * LINE_WORDS;

/// The mark-region region's capacity: the policy's curated target rounded up to whole
/// 256-word blocks — `ceil(987 / 256) * 256 = 1024` words (four blocks).
const fn immix_region_words() -> usize {
    let blocks = mature_target().div_ceil(BLOCK_WORDS);
    blocks * BLOCK_WORDS
}

/// The **mark-region** mature occupancy's converged cycle, derived by the same rule: the region
/// is reclaimed **in place** at every fullsweep and then holds exactly the live set (320 words),
/// and it admits another promotion while the region's free **lines** cover the promotion's —
/// `1024 - used >= 320` — so it cycles over `320, 640, 960` (three values, two promotions)
/// instead of growing without bound.
const IMMIX_OCCUPANCY_STEPS: usize = occupancy_steps(immix_region_words(), IMMIX_LIVE_WORDS);
/// The cycle as the `IMMIX_OCCUPANCY_STEPS` successive multiples of the live set.
const IMMIX_OCCUPANCY_CYCLE: [usize; IMMIX_OCCUPANCY_STEPS] = {
    let mut cycle = [0usize; IMMIX_OCCUPANCY_STEPS];
    let mut step = 0;
    while step < IMMIX_OCCUPANCY_STEPS {
        cycle[step] = (step + 1) * IMMIX_LIVE_WORDS;
        step += 1;
    }
    cycle
};

/// The pinned mature-occupancy cycle for a strategy.
fn occupancy_cycle(kind: MatureKind) -> &'static [usize] {
    match kind {
        MatureKind::Copying => &COPYING_OCCUPANCY_CYCLE,
        MatureKind::Immix => &IMMIX_OCCUPANCY_CYCLE,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Builds a `{a, 2a}` tuple (`TUPLE_WORDS` words) in `c`'s nursery.
fn tuple(c: &mut AnyCollector, a: i64) -> Eterm {
    c.young_mut()
        .heap_mut()
        .tuple(&[make_small(a), make_small(2 * a)])
        .expect("a 3-word nursery tuple")
}

/// The mature space's `(used_words, capacity_words)`, uniform across both strategies.
fn mature_words(c: &AnyCollector) -> (usize, usize) {
    match c {
        AnyCollector::Copying(x) => (x.mature().used_words(), x.mature().capacity_words()),
        AnyCollector::Immix(x) => (x.mature().used_words(), x.mature().capacity_words()),
    }
}

/// The min and max of a non-empty slice of counts.
fn min_max(values: &[usize]) -> (usize, usize) {
    (
        *values.iter().min().expect("non-empty"),
        *values.iter().max().expect("non-empty"),
    )
}

/// Reads the two integer elements of a `{a, 2a}` tuple as `(a, 2a)`.
///
/// # Safety
///
/// `t` must be a live, 2-element tuple of small integers resident in a collector heap.
unsafe fn tuple_pair(t: Eterm) -> (i64, i64) {
    assert_eq!(unsafe { tuple_size(t) }, 2, "the term is a 2-tuple");
    // SAFETY: `t` is a live 2-tuple, so elements 0 and 1 are within it (per this function's
    // contract); both are small integers the caller built with `tuple`.
    unsafe {
        (
            signed_value(tuple_element(t, 0)),
            signed_value(tuple_element(t, 1)),
        )
    }
}

// ---------------------------------------------------------------------------
// The regression gate
// ---------------------------------------------------------------------------

/// Under the **default** fullsweep cadence, 200 measured rounds of a fresh-graph workload with a
/// bounded live set must leave the mature generation at a **single constant capacity**.
///
/// The live set never changes: 20 tuples (60 words) are rebuilt every round and the previous
/// round's graph becomes garbage. A fullsweep therefore has to preserve exactly 60 live words
/// every time, so its size decision must be identical every time — any growth is the
/// occupancy-inflated sizing bug. The nursery capacity is asserted to be constant too, the
/// mature occupancy is asserted to follow the strategy's pinned cycle (see
/// [`occupancy_cycle`]), and the live set is checked structurally after every round.
#[test]
fn the_mature_capacity_converges_under_the_default_cadence() {
    for kind in STRATEGIES {
        // The **default** cadence — the one the other steady-state tests have to override:
        // every sizing field is the crate default (`fullsweep_after` 65535, `min_heap_words`
        // 0, the 233-word young/mature initial sizes).
        let config = GcConfig::copying(HeapPolicy::default()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(c.kind(), kind, "{kind:?}: the configured strategy");
        assert_eq!(
            c.young().capacity_words(),
            DEFAULT_NURSERY,
            "{kind:?}: the collector starts at the default nursery"
        );

        let mut roots: Vec<Eterm> = Vec::new();
        let mut mature_caps: Vec<usize> = Vec::with_capacity(MEASURED);
        let mut mature_uses: Vec<usize> = Vec::with_capacity(MEASURED);
        let mut young_caps: Vec<usize> = Vec::with_capacity(MEASURED);

        for round in 0..(WARMUP + MEASURED) {
            // A fresh bounded live graph (the previous round's is now unreachable garbage) ...
            roots.clear();
            for i in 0..LIVE_TUPLES {
                roots.push(tuple(&mut c, round as i64 * 1000 + i));
            }
            // ... plus a garbage batch the collection must reclaim.
            for i in 0..GARBAGE_TUPLES {
                // Dropped immediately: never referenced from `roots`.
                let _ = tuple(&mut c, -i - 1);
            }

            c.collect(&mut roots).expect("collect");
            assert_eq!(
                c.young().used_words(),
                0,
                "{kind:?}: the nursery must be empty after every collection (round {round})"
            );

            // The live set is intact: every root still holds its `{a, 2a}` pair.
            for (i, &t) in roots.iter().enumerate() {
                let a = round as i64 * 1000 + i as i64;
                // SAFETY: each root is a live `{a, 2a}` tuple this test built in the collector.
                assert_eq!(
                    unsafe { tuple_pair(t) },
                    (a, 2 * a),
                    "{kind:?}: live term {i} must survive (round {round})"
                );
            }

            if round >= WARMUP {
                let (used, capacity) = mature_words(&c);
                mature_caps.push(capacity);
                mature_uses.push(used);
                young_caps.push(c.young().capacity_words());
            }
        }

        assert_eq!(
            mature_caps.len(),
            MEASURED,
            "{kind:?}: one sample per measured round"
        );

        // ---- the converged constant: min == max for every sizing observable ----
        let (cap_min, cap_max) = min_max(&mature_caps);
        assert_eq!(
            cap_min,
            cap_max,
            "{kind:?}: the mature capacity must converge to a constant under the default \
             cadence, got min {cap_min} != max {cap_max} (first/last: {} -> {}); the capacity \
             series is {mature_caps:?}",
            mature_caps[0],
            mature_caps[MEASURED - 1]
        );

        // ---- the mature occupancy follows the pinned cycle for the strategy ----
        // A copying minor promotes the nursery's live words onto the top of the mature space
        // without reclaiming it, so the occupancy steps up one live set per minor until a
        // fullsweep resets it (see `COPYING_OCCUPANCY_CYCLE`); the mark-region region is
        // reclaimed in place at every fullsweep and then holds exactly the live set, so its
        // occupancy is a single value. Neither creeps: the observed set of values must be
        // exactly the pinned cycle, no more and no less.
        let cycle = occupancy_cycle(kind);
        let mut observed = mature_uses.clone();
        observed.sort_unstable();
        observed.dedup();
        let mut pinned = cycle.to_vec();
        pinned.sort_unstable();
        assert_eq!(
            observed, pinned,
            "{kind:?}: the mature occupancy must follow the pinned cycle {cycle:?}"
        );

        let (young_min, young_max) = min_max(&young_caps);
        assert_eq!(
            young_min, young_max,
            "{kind:?}: the nursery must stay constant too, got min {young_min} != max {young_max}"
        );

        // ---- non-degenerate: it covers the live set and stays at the default scale ----
        assert!(
            cap_min >= LIVE_WORDS,
            "{kind:?}: the converged mature capacity ({cap_min} words) must cover the live set \
             ({LIVE_WORDS} words)"
        );
        assert!(
            cap_min >= DEFAULT_NURSERY,
            "{kind:?}: the converged mature capacity ({cap_min} words) must not have collapsed \
             below the default scale"
        );
        // The fullsweep path really was exercised: a fullsweep promotes the whole live graph, so
        // the mature occupancy's floor is the live set (word-exact for copying; line-rounded, and
        // therefore at least the live set, for the mark-region space).
        let cycle_min = *cycle.iter().min().expect("a pinned cycle");
        assert!(
            cycle_min >= LIVE_WORDS,
            "{kind:?}: the mature occupancy cycle {cycle:?} must hold the live set"
        );
        assert!(
            c.stats().major_collections > 0,
            "{kind:?}: the default cadence must have run at least one fullsweep"
        );

        eprintln!(
            "{kind:?}: {MEASURED} measured rounds (+{WARMUP} warm-up) under the DEFAULT cadence \
             (`fullsweep_after` 65535), stable live set = {LIVE_WORDS} words, {} fullsweeps:",
            c.stats().major_collections
        );
        eprintln!(
            "  young capacity = {young_min} words (constant), mature capacity = {cap_min} words \
             (constant), mature occupancy cycles over {cycle:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The sizing input: what the fullsweep actually measured
// ---------------------------------------------------------------------------

/// One fullsweep's decision inputs and outcome, sampled by the harness below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fullsweep {
    /// The mature capacity going in.
    capacity_before: usize,
    /// The exact live set the fullsweep evacuated (`crate::live`), read from the fresh space's
    /// occupancy — the figure the sizing rule is given.
    live: usize,
    /// The nursery's occupancy at the collection, which the rule folds into `need_after`. (The
    /// escalation test does **not** read it — that compares the mature space's room against the
    /// nursery's **live** promotion, `minor_promotion_fits`; see [`occupancy_steps`].)
    young_occupancy: usize,
    /// The mature capacity installed.
    capacity_after: usize,
    /// How many collections ran since the previous fullsweep.
    minors_since: usize,
}

/// **The mature generation is sized from the live set a fullsweep measured, and the target is
/// the amortised room that fullsweep must leave.**
///
/// This is the rule's own instrument: it samples, at every collection, the two inputs the sizing
/// rule consumes (the nursery's occupancy going in, the live set the fullsweep kept coming out)
/// and re-derives the decision, so every figure below is a measured counter rather than a reading
/// of the constants. It pins three things the amortisation must not break:
///
/// * **the input is the live set, never the occupancy.** Every fullsweep of this corpus keeps
///   exactly [`LIVE_WORDS`] live words, although the mature space it fills is full of the
///   garbage the minors since the last fullsweep promoted. A rule that fed the occupancy back in
///   would see a figure two orders of magnitude larger and creep the capacity one growth-table
///   step per fullsweep (`MEASURED.md`, the rejected variants);
/// * **the installed capacity is the rule's fixed point** —
///   `mature_after_fullsweep(capacity_before, live, young_occupancy)`, re-derived here and
///   compared exactly, so a sizing change that is not "derive from the live set" cannot pass
///   silently;
/// * **the mature target holds [`MATURE_TARGET_LIVE_MULTIPLE`] `need_after` budgets**, so the
///   space is sized for a *run* of promotions: `capacity_after >= 2 * (live + young_occupancy +
///   reserve)`, and consequently more than one minor runs between fullsweeps. That is the
///   amortisation the multiple buys, and [`occupancy_cycle`] pins its exact length for the two
///   strategies.
///
/// The copying strategy is asserted exactly (its capacity is the policy's size in whole words);
/// the mark-region strategy's capacity is the policy's size rounded up to whole blocks, so only
/// the inequality and the cycle are pinned for it.
#[test]
fn the_fullsweep_sizes_the_mature_generation_from_the_live_set_it_measured() {
    let policy = HeapPolicy::default();
    assert_eq!(
        policy.need_reserve_words, NEED_RESERVE_WORDS,
        "the corpus' reserve constant must be the policy's"
    );

    for kind in STRATEGIES {
        let config = GcConfig::copying(policy.clone()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");
        let mut roots: Vec<Eterm> = Vec::new();
        let mut fullsweeps: Vec<Fullsweep> = Vec::new();
        let mut since = 0usize;

        for round in 0..(WARMUP + MEASURED) {
            roots.clear();
            for i in 0..LIVE_TUPLES {
                roots.push(tuple(&mut c, round as i64 * 1000 + i));
            }
            for i in 0..GARBAGE_TUPLES {
                let _ = tuple(&mut c, -i - 1);
            }

            let capacity_before = mature_words(&c).1;
            let young_occupancy = c.young().used_words();
            let collection = c.collect(&mut roots).expect("collect");
            since += 1;
            if collection.kind == beam_gc::CollectionKind::Major {
                let (live, capacity_after) = mature_words(&c);
                fullsweeps.push(Fullsweep {
                    capacity_before,
                    live,
                    young_occupancy,
                    capacity_after,
                    minors_since: since - 1,
                });
                since = 0;
            }
        }

        assert!(
            fullsweeps.len() > 2,
            "{kind:?}: the corpus must run several fullsweeps, got {}",
            fullsweeps.len()
        );

        // The steady state: the later half of the run, past the warm-up's transients.
        let steady = &fullsweeps[fullsweeps.len() / 2..];
        let counts: Vec<usize> = steady.iter().map(|f| f.minors_since).collect();
        let (minors_min, minors_max) = min_max(&counts);

        for sweep in steady {
            // 1. The sizing input is the live set the fullsweep measured — the same words every
            //    time, for a corpus whose mature space is full of dead promotions. The
            //    mark-region space reports its occupancy in whole lines, so its figure is the
            //    live set rounded up to the line.
            let expected_live = match kind {
                MatureKind::Copying => LIVE_WORDS,
                MatureKind::Immix => IMMIX_LIVE_WORDS,
            };
            assert_eq!(
                sweep.live, expected_live,
                "{kind:?}: every fullsweep keeps exactly the stable live set: {sweep:?}"
            );
            // 2. The copying capacity is the rule's own answer for those inputs: the rule's
            //    target in whole words, which the steady state keeps.
            if kind == MatureKind::Copying {
                let derived = policy.mature_after_fullsweep(
                    sweep.capacity_before,
                    sweep.live,
                    sweep.young_occupancy,
                );
                assert_eq!(
                    sweep.capacity_after, derived,
                    "{kind:?}: the installed mature capacity must be \
                     mature_after_fullsweep({}, {}, {}): {sweep:?}",
                    sweep.capacity_before, sweep.live, sweep.young_occupancy
                );
            }
            // 3. The target covers MATURE_TARGET_LIVE_MULTIPLE `need_after` budgets, so the
            //    space is sized for a run of promotions and not for a single one.
            let need_after = policy.need_after_fullsweep(LIVE_WORDS, sweep.young_occupancy);
            assert!(
                sweep.capacity_after
                    >= MATURE_TARGET_LIVE_MULTIPLE * need_after,
                "{kind:?}: the mature capacity ({}) must hold {} x the {} words a fullsweep \
                 must leave behind: {sweep:?}",
                sweep.capacity_after,
                MATURE_TARGET_LIVE_MULTIPLE,
                need_after
            );
            // 3b. And it *is* that target: the policy's curated size in whole words, or — for
            //     the mark-region space — that size rounded up to whole blocks.
            let target = policy.mature_target_words(LIVE_WORDS, sweep.young_occupancy);
            let block_rounded = target.div_ceil(BLOCK_WORDS) * BLOCK_WORDS;
            match kind {
                MatureKind::Copying => assert_eq!(
                    sweep.capacity_after, target,
                    "{kind:?}: the copying mature capacity is the policy's target: {sweep:?}"
                ),
                MatureKind::Immix => assert!(
                    (target..=block_rounded).contains(&sweep.capacity_after),
                    "{kind:?}: the mark-region capacity is the target ({target}) rounded to \
                     whole {BLOCK_WORDS}-word blocks ({block_rounded}): {sweep:?}"
                ),
            }
        }

        // 4. A run of promotions, not one: more than a single minor runs between fullsweeps, at
        //    the exact length the converged cycle derivation predicts.
        assert!(
            minors_min > 1,
            "{kind:?}: the mature target must admit a run of promotions, got {minors_min} \
             minors per fullsweep: {steady:?}"
        );
        let expected = occupancy_cycle(kind).len() - 1;
        assert_eq!(
            (minors_min, minors_max),
            (expected, expected),
            "{kind:?}: the steady state must run exactly {expected} minors per fullsweep (the \
             converged occupancy cycle's length less the post-fullsweep value)"
        );

        eprintln!(
            "{kind:?}: {} fullsweeps over {} collections, steady state = {} minors per \
             fullsweep (live {} words, young occupancy {} words, need_after {} words):",
            fullsweeps.len(),
            WARMUP + MEASURED,
            minors_min,
            LIVE_WORDS,
            steady[0].young_occupancy,
            policy.need_after_fullsweep(LIVE_WORDS, steady[0].young_occupancy)
        );
        eprintln!(
            "  mature capacity {} -> {} words, target = next_words({} x {} + 0) = {} words, \
             settled occupancy {} words",
            steady[0].capacity_before,
            steady[0].capacity_after,
            MATURE_TARGET_LIVE_MULTIPLE,
            policy.need_after_fullsweep(LIVE_WORDS, steady[0].young_occupancy),
            policy.mature_target_words(LIVE_WORDS, steady[0].young_occupancy),
            steady[0].live
        );
    }
}
