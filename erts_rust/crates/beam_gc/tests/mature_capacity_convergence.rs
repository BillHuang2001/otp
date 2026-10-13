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
//! `HeapPolicy::mature_after_fullsweep`'s `should_grow` fires on every single fullsweep — the
//! capacity creeps up one growth-table step per fullsweep, without bound, for a live set that
//! never changes. The fullsweep must instead be sized from the **exact live word count** of the
//! graph it evacuates (the C `adjust_after_fullsweep`'s `HEAP_TOP(p) - HEAP_START(p)`).
//!
//! Every corpus here is deterministic (no randomness) and every **assertion** is a deterministic
//! count — capacities and occupancies. **No timing is ever asserted** and none is measured; the
//! converged numbers are printed with `-- --nocapture` for a human reader only.
//!
//! Driven through **both** mature strategies ([`AnyCollector`] `Copying` and `Immix`), which
//! share the nursery and differ only in where the mature generation lives: the copying space
//! sizes itself in whole words, the mark-region space in whole 16-word lines.

use beam_gc::prelude::{AnyCollector, GcConfig, HeapPolicy, MatureKind};
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

/// Every mature strategy: the copying space and the mark-region space.
const STRATEGIES: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// The **copying** mature occupancy's converged cycle: one live set per minor since the last
/// fullsweep (`60 -> 120 -> 180 -> 60`). A copying minor promotes the nursery's live words onto
/// the top of the mature space without reclaiming it; once the mature space can no longer
/// receive the nursery's occupancy the escalation test fires a fullsweep, which evacuates young
/// *and* mature into a fresh space holding exactly the 60-word live set — so the occupancy
/// starts over at 60 rather than growing without bound.
const COPYING_OCCUPANCY_CYCLE: [usize; 3] = [LIVE_WORDS, 2 * LIVE_WORDS, 3 * LIVE_WORDS];

/// The **mark-region** mature occupancy: the region is reclaimed **in place** at every
/// fullsweep and then holds exactly the live set — 20 one-line tuples = 20 lines = 320 words.
const IMMIX_OCCUPANCY: usize = LIVE_TUPLES as usize * 16;

/// The pinned mature-occupancy cycle for a strategy (a single value for the mark-region space).
fn occupancy_cycle(kind: MatureKind) -> &'static [usize] {
    match kind {
        MatureKind::Copying => &COPYING_OCCUPANCY_CYCLE,
        MatureKind::Immix => core::slice::from_ref(&IMMIX_OCCUPANCY),
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
    unsafe { (signed_value(tuple_element(t, 0)), signed_value(tuple_element(t, 1))) }
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
            cap_min, cap_max,
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
