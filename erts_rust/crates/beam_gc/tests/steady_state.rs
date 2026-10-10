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

//! **Steady-state quiescent heap** — the single combined long-run gate for the property the
//! per-piece tests only cover separately.
//!
//! `tests/nursery_bounded.rs` proves the *nursery* is bounded, `tests/retained_vs_live.rs`
//! proves the retained-minus-live surplus does not grow, and `tests/memory_overhead.rs` proves
//! per-object overhead is zero and RSS plateaus. None of them asserts the **combined** runtime
//! property a long-lived process depends on: after a warm-up, a process running a *bounded*
//! workload through `collect` in a loop reaches a **quiescent heap** — every sizing observable
//! (`young` capacity, `mature` capacity, `mature` occupancy, `total_heap_size`) settles at a
//! **single constant value** and stays there for the rest of the run. That is the property of
//! [`HeapPolicy::default`] this file pins.
//!
//! Two tests, the two opposite failure modes of the heap policy:
//!
//! 1. [`the_quiescent_heap_converges_to_a_constant`] — a bounded live set re-built fresh every
//!    round plus garbage. The heap must **converge** (no unbounded growth) to a constant that
//!    still covers the live set and is *not* degenerate (the loop must not be surviving by
//!    collecting on every allocation down to a tiny fixed size).
//! 2. [`a_monotonically_growing_live_set_does_not_grow_the_nursery`] — a live set that grows
//!    every round. The **nursery** must stay bounded (a constant) while the **mature**
//!    generation absorbs the growth, with every live term intact.
//!
//! ## The policy
//!
//! Both tests run under the **default sizing policy** — every sizing field at its
//! [`HeapPolicy::default`] value, so there is **no `min_heap_words` pin**, no `max_heap_words`
//! cap, the crate-default 233-word young / 233-word mature initial sizes and the default
//! fullsweep cadence (`fullsweep_after = 65535`). Neither test overrides a sizing knob.
//!
//! Test 1 deliberately runs that **default cadence** rather than forcing a fullsweep per round:
//! the fullsweep now sizes its fresh mature space from the **exact reachable live word count**
//! (`collect_major` → `crate::live`), not from the (garbage-inflated) pre-collection occupancy,
//! so the mature capacity converges under the very cadence a plain process runs — the strongest
//! form of the regression assertion. Sizing it from the occupancy instead makes
//! [`HeapPolicy::should_grow`] fire on every fullsweep and creeps the capacity up one
//! growth-table step per fullsweep, without bound, for a live set that never changes. The same
//! property, isolated from the nursery, is pinned by `tests/mature_capacity_convergence.rs`.
//!
//! ## Determinism
//!
//! Every corpus is deterministic (no randomness) and every **assertion** is on a deterministic
//! count — capacities, occupancies and word counts. **No timing is ever asserted** and none is
//! even measured; the converged numbers are pinned with `assert_eq!` and printed with
//! `-- --nocapture` for a human reader only.

use beam_gc::prelude::{AnyCollector, CollectionKind, GcConfig, HeapPolicy, MatureKind};
use beam_term::prelude::{make_small, size_object, signed_value, tuple_element, tuple_size, Eterm};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The default nursery capacity (`HeapPolicy::default().initial_young_words`), in words.
const DEFAULT_NURSERY: usize = 233;

/// Words per `{a, 2a}` tuple: the frozen layout is `arity + 1` (a header plus two elements).
const TUPLE_WORDS: usize = 3;

/// Every mature strategy this file drives: the copying space and the mark-region space.
const STRATEGIES: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

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
// TEST 1 — the quiescent heap converges to a constant
// ---------------------------------------------------------------------------

/// Live tuples re-built **fresh** every round (a bounded, non-growing live set): 20 * 3 = 60
/// words.
const LIVE_TUPLES: i64 = 20;

/// Garbage tuples built every round and dropped immediately (never referenced): 50 * 3 = 150
/// words.
///
/// `LIVE_TUPLES + GARBAGE_TUPLES` = 70 tuples = 210 words, which fits the 233-word default
/// nursery, so every round's allocation succeeds before the round's collection.
const GARBAGE_TUPLES: i64 = 50;

/// Rounds run before measurement (warm-up to the sizing fixed point).
const WARMUP: usize = 20;

/// Long-running measured rounds: with a per-collection creep the heap would be astronomical
/// (`233 * 1.2^200` is ~10^17 words), so a constant over this half is a genuine steady state.
const MEASURED: usize = 200;

/// The stable live set in words (the graph with which the converged heap must cope).
const LIVE_WORDS: usize = LIVE_TUPLES as usize * TUPLE_WORDS;

/// One round's quiescent-heap observables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Steady {
    /// `young().capacity_words()` — the nursery capacity (the C `HEAP_SIZE`).
    young_capacity: usize,
    /// `mature().capacity_words()` — the mature generation's capacity.
    mature_capacity: usize,
    /// `mature().used_words()` — the words the mature generation holds.
    mature_occupancy: usize,
    /// `heap_info().total_heap_size` = nursery + mature capacity (the C `heap_sz + old heap`).
    total_heap_size: usize,
}

/// The converged steady state for the **copying** strategy. The fullsweep now sizes the fresh
/// mature space from the **exact** reachable live word count (60 words) plus the reserve — not
/// from the round's occupancy — so `mature_after_fullsweep(233, 60, 0)` neither grows nor
/// shrinks the default 233-word mature capacity; the nursery stays at the default 233.
const COPYING_STEADY: Steady = Steady {
    young_capacity: DEFAULT_NURSERY,
    mature_capacity: 233,
    mature_occupancy: LIVE_WORDS,
    total_heap_size: 233 + 233,
};

/// The converged steady state for the **mark-region** strategy. The exact-live sizing asks the
/// policy for `next_words(2 * (60 + need_reserve(64))) = 376` words; the line-granular region
/// rounds that request up to two whole 256-word blocks (512), and the 20 live tuples occupy 20
/// lines = 320 resident words. The nursery stays at the default 233.
const IMMIX_STEADY: Steady = Steady {
    young_capacity: DEFAULT_NURSERY,
    mature_capacity: 512,
    mature_occupancy: LIVE_TUPLES as usize * 16,
    total_heap_size: 233 + 512,
};

/// The pinned expectation for a strategy.
fn expected(kind: MatureKind) -> Steady {
    match kind {
        MatureKind::Copying => COPYING_STEADY,
        MatureKind::Immix => IMMIX_STEADY,
    }
}

/// The **default** sizing policy, verbatim — no sizing override at all, and the default
/// fullsweep cadence (`fullsweep_after = 65535`). The quiescent heap must converge under the
/// exact configuration a plain `HeapPolicy::default()` process runs (see the module header).
fn steady_policy() -> HeapPolicy {
    HeapPolicy::default()
}

/// **TEST 1** — the quiescent heap converges to a constant and stays there.
///
/// Each of `WARMUP + MEASURED` rounds allocates a fresh bounded live graph plus garbage and
/// collects. After the warm-up, across **all** measured rounds, `young` capacity, `mature`
/// capacity, `mature` occupancy and `total_heap_size` must each attain `min == max` (a single
/// constant — no unbounded growth) and the constant must equal the pinned expectation (which
/// covers the 60-word live set and is far from a degenerate "collect every allocation" size).
#[test]
fn the_quiescent_heap_converges_to_a_constant() {
    for kind in STRATEGIES {
        let config = GcConfig::copying(steady_policy()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(
            c.kind(),
            kind,
            "{kind:?}: the config must resolve to the requested strategy"
        );
        assert_eq!(
            c.young().capacity_words(),
            DEFAULT_NURSERY,
            "{kind:?}: the collector starts at the default nursery"
        );

        let mut roots: Vec<Eterm> = Vec::new();
        let mut samples: Vec<Steady> = Vec::with_capacity(MEASURED);
        let mut kinds: Vec<CollectionKind> = Vec::with_capacity(WARMUP + MEASURED);

        for round in 0..(WARMUP + MEASURED) {
            // A fresh bounded live graph (the previous round's is now unreachable garbage)...
            roots.clear();
            for i in 0..LIVE_TUPLES {
                roots.push(tuple(&mut c, round as i64 * 1000 + i));
            }
            // ... plus a garbage batch the collection must reclaim.
            for i in 0..GARBAGE_TUPLES {
                // Dropped immediately: never referenced from `roots`.
                let _ = tuple(&mut c, -i - 1);
            }

            let collection = c.collect(&mut roots).expect("collect");
            kinds.push(collection.kind);
            assert!(
                collection.stats.words_reclaimed > 0,
                "{kind:?}: a fresh graph plus {} garbage words must reclaim something \
                 (round {round})",
                GARBAGE_TUPLES * TUPLE_WORDS as i64
            );

            // The live set really is the stable, bounded footprint.
            let live: usize = roots.iter().map(|&t| size_object(t)).sum();
            assert_eq!(
                live, LIVE_WORDS,
                "{kind:?}: the live set must stay at the bounded footprint (round {round})"
            );
            assert_eq!(
                c.young().used_words(),
                0,
                "{kind:?}: the nursery must be empty after every collection (round {round})"
            );

            if round >= WARMUP {
                let (mature_occupancy, mature_capacity) = mature_words(&c);
                samples.push(Steady {
                    young_capacity: c.young().capacity_words(),
                    mature_capacity,
                    mature_occupancy,
                    total_heap_size: c.heap_info().total_heap_size,
                });
            }
        }

        assert_eq!(samples.len(), MEASURED, "{kind:?}: one sample per measured round");

        // ---- the default cadence really exercised both collection kinds ----
        // The capacity converges only because the default cadence *does* escalate to a
        // fullsweep once the mature space can no longer receive the nursery (a fresh graph per
        // round fills it), and the rounds in between run minors. Asserting both happened keeps
        // the convergence honest: it is the sizing of the real fullsweep, not a run that never
        // reached one.
        assert!(
            kinds.contains(&CollectionKind::Minor),
            "{kind:?}: the default cadence must run minors"
        );
        assert!(
            kinds.contains(&CollectionKind::Major),
            "{kind:?}: the default cadence must escalate to a fullsweep"
        );

        // ---- the converged constant: min == max for all four observables ----
        let young: Vec<usize> = samples.iter().map(|s| s.young_capacity).collect();
        let mature_cap: Vec<usize> = samples.iter().map(|s| s.mature_capacity).collect();
        let mature_occ: Vec<usize> = samples.iter().map(|s| s.mature_occupancy).collect();
        let totals: Vec<usize> = samples.iter().map(|s| s.total_heap_size).collect();

        for (label, series) in [
            ("young capacity", &young),
            ("mature capacity", &mature_cap),
            ("mature occupancy", &mature_occ),
            ("total_heap_size", &totals),
        ] {
            let (min, max) = min_max(series);
            assert_eq!(
                min, max,
                "{kind:?}: {label} must converge to a constant over all {MEASURED} measured \
                 rounds, got min {min} != max {max}"
            );
        }

        // ---- and the constant is exactly the pinned expectation ----
        let want = expected(kind);
        assert_eq!(
            samples[MEASURED - 1],
            want,
            "{kind:?}: the converged quiescent heap must equal the pinned expectation"
        );

        // ---- non-trivial: it covers the stable live set and is not a degenerate minimum ----
        assert!(
            want.mature_occupancy >= LIVE_WORDS,
            "{kind:?}: the converged mature occupancy ({} words) must cover the stable live set \
             ({LIVE_WORDS} words)",
            want.mature_occupancy
        );
        assert!(
            want.total_heap_size >= LIVE_WORDS + DEFAULT_NURSERY,
            "{kind:?}: the converged total heap ({} words) must be far above one round's \
             allocation",
            want.total_heap_size
        );
        assert!(
            want.young_capacity > steady_policy().need_reserve_words,
            "{kind:?}: the nursery must not have degenerated to the bare reserve"
        );
        assert!(
            want.mature_capacity > LIVE_WORDS,
            "{kind:?}: the mature generation must exceed the live set, not be a tight fit"
        );

        eprintln!(
            "{kind:?}: converged quiescent heap over {MEASURED} measured rounds (+{WARMUP} \
             warm-up), stable live set = {LIVE_WORDS} words:"
        );
        eprintln!(
            "  young capacity = {}, mature capacity = {}, mature occupancy = {}, \
             total_heap_size = {}",
            want.young_capacity, want.mature_capacity, want.mature_occupancy, want.total_heap_size
        );
    }
}

// ---------------------------------------------------------------------------
// TEST 2 — a growing live set does not grow the nursery
// ---------------------------------------------------------------------------

/// Live tuples appended per round: 77 * 3 = 231 of the 233-word nursery — the round fills it to
/// (almost) capacity with **live** data, the exact corpus a "grow when full" rule would ramp up.
const BATCH: i64 = 77;

/// Rounds of monotonic growth; the live set ends at 30 * 231 = 6930 words, ~30x the nursery.
const GROWTH_ROUNDS: i64 = 30;

/// **TEST 2** — the nursery stays bounded while the mature generation absorbs the growth.
///
/// Every round appends `BATCH` live tuples (filling the default 233-word nursery) and collects.
/// The live set grows monotonically to ~30x the nursery, so this is the *opposite* failure mode
/// to TEST 1: a policy that grew the nursery on every collection would ramp it up without bound.
/// The nursery must stay exactly at its default capacity throughout, the mature generation must
/// take on the growth, and every live term must survive intact.
#[test]
fn a_monotonically_growing_live_set_does_not_grow_the_nursery() {
    for kind in STRATEGIES {
        // The literal default policy — no sizing override at all.
        let config = GcConfig::copying(HeapPolicy::default()).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(c.kind(), kind);
        assert_eq!(c.young().capacity_words(), DEFAULT_NURSERY, "{kind:?}");

        let mut roots: Vec<Eterm> = Vec::new();
        let mut young_caps: Vec<usize> = Vec::new();
        let mut mature_caps: Vec<usize> = Vec::new();

        for round in 0..GROWTH_ROUNDS {
            for j in 0..BATCH {
                // Alive keys the tuple stays reachable through `roots` and is verified at the end.
                roots.push(tuple(&mut c, round * 1000 + j));
            }
            assert!(
                c.young().available_words() < TUPLE_WORDS,
                "{kind:?}: the round filled the nursery (round {round})"
            );

            c.collect(&mut roots).expect("collect");
            assert_eq!(c.young().used_words(), 0, "{kind:?}");

            young_caps.push(c.young().capacity_words());
            mature_caps.push(mature_words(&c).1);
        }

        // The live set now dwarfs the nursery ...
        let live_words = roots.len() * TUPLE_WORDS;
        assert!(
            live_words > 25 * DEFAULT_NURSERY,
            "{kind:?}: the live set ({live_words} words) must far exceed the nursery"
        );

        // ... yet the nursery never moved off its default: a round that filled it to capacity
        // *with live data* did not talk the collector into growing it.
        let (young_min, young_max) = min_max(&young_caps);
        assert_eq!(
            young_min, young_max,
            "{kind:?}: the nursery capacity must stay constant, got {young_caps:?}"
        );
        assert_eq!(
            young_min, DEFAULT_NURSERY,
            "{kind:?}: the nursery must stay at the default capacity"
        );

        // The mature generation took on the whole growth ...
        let (mature_min, mature_max) = min_max(&mature_caps);
        assert!(
            mature_max > mature_min,
            "{kind:?}: the mature generation must grow to absorb the live set, got \
             {mature_caps:?}"
        );
        assert!(
            mature_max > DEFAULT_NURSERY,
            "{kind:?}: the mature capacity ({mature_max} words) must exceed the nursery"
        );

        // ... and every live term survived intact.
        for (idx, &t) in roots.iter().enumerate() {
            let round = idx as i64 / BATCH;
            let j = idx as i64 % BATCH;
            let a = round * 1000 + j;
            // SAFETY: each root is a live `{a, 2a}` tuple this test built in the collector.
            let (e0, e1) = unsafe { tuple_pair(t) };
            assert_eq!(
                (e0, e1),
                (a, 2 * a),
                "{kind:?}: live term {idx} must survive the growth intact"
            );
        }

        eprintln!(
            "{kind:?}: {GROWTH_ROUNDS} rounds of growth, live set = {live_words} words \
             ({:.0}x the nursery): nursery capacity = {young_min} words (constant), mature \
             capacity {mature_min} -> {mature_max} words",
            live_words as f64 / DEFAULT_NURSERY as f64
        );
    }
}
