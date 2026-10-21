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

//! **The mature generation's capacity is a function of the LIVE set — never of the allocation
//! history** — the regression gate for the fullsweep's *young-occupancy* input.
//!
//! `tests/mature_capacity_convergence.rs` pins that a **constant** live set converges to a
//! constant mature capacity (it is the gate against feeding the *mature* occupancy back in).
//! This file pins the other half of the same rule: the fullsweep also folds the **young heap's
//! occupancy** into the C's `need_after` (`erl_gc.c:2072`), so the room the fresh mature space
//! gets still has to be a function of the live set. Unclamped, that input is the *chain-summed*
//! occupancy of every nursery space ([`HeapObservable::heap_used_words`]), which counts every
//! **dead** word the mutator — or a host's unrooted BIF copy-out, which bumps the process heap
//! and never rewinds it — accumulated since the last collection. That figure is unbounded and
//! has nothing to do with the live set, so baking it into the next mature space makes the
//! capacity climb with the run's allocation: the feedback the measured
//! `3.8 M → 26 M → 590 M`-word walk and the `{crash,lint_module,system_limit,…}` fullsweep are.
//!
//! The rule restored here is the C's own premise for that term: `HEAP_TOP(p) - HEAP_START(p)`
//! is the usage of **one** young block, so it can never exceed that block's `HEAP_SIZE` — and
//! the C re-sizes the young block from the live set at every collection (a fullsweep frees the
//! old block, `erl_gc.c:1875-1888`; `adjust_after_fullsweep` sizes the next one from the
//! post-copy live set, `erl_gc.c:2061-2098`). The Rust equivalent is
//! `min(nursery.used_words(), HeapPolicy::young_target_words(live))`, the target the nursery's
//! own collapse installs immediately after (`Nursery::collapse_after_fullsweep`).
//!
//! Four contracts, each a deterministic **word count** (never a clock: nothing here is timed):
//!
//! 1. **Flat in the dead occupancy.** The mature capacity a fullsweep installs is identical
//!    whether the nursery carried K or 4K words of dead occupancy going in — and, below the
//!    nursery's own target, it still *does* include the occupancy (the term is bounded, not
//!    dropped: without it the next minor has no room and the cadence degenerates).
//! 2. **No feedback.** Successive fullsweeps over an unchanged live set stay at the policy's own
//!    fixed point — the capacity series is constant across a *cumulative* dead-burst shape
//!    (K, 2K, 4K), so it cannot be `2 × previous`.
//! 3. **Both strategies agree**, because `AnyCollector` must keep them converging on the same
//!    live-set-derived figure: the copying space installs that figure, the mark-region space
//!    installs it rounded up to whole [`BLOCK_WORDS`]-word blocks.
//! 4. **The shrink still happens.** After a big live set is dropped, the next fullsweep gives the
//!    capacity back down to the small live set's own target — the figure is released, not merely
//!    capped.
//!
//! Every corpus is deterministic (no randomness) and both strategies are driven through
//! [`AnyCollector`].
//!
//! # Scope of the flatness claim
//!
//! The bound is the policy's nursery target for the live set
//! ([`HeapPolicy::young_target_words`]), which the corpus' dead batches all exceed or all fall
//! below — so the capacity is measured to be flat exactly where the unclamped input was
//! unbounded. When the nursery's collapse instead **keeps** a capacity within its hysteresis band
//! (the keep branch, `4/3..=8` times the target — a live set that shrank less than
//! [`YOUNG_SHRINK_FACTOR`]-fold), the kept capacity is larger than the bound and one collection
//! may escalate to a fullsweep until the chain passes `8 * target` and the collapse installs the
//! target. That is bounded by the live set (it is not this feedback), so it is not pinned as a
//! figure here; the mature capacity itself is asserted against the rule's own derivation in
//! every case below.

use beam_gc::prelude::{
    AnyCollector, CollectionKind, GcConfig, HeapObservable, HeapPolicy, MatureKind,
    MATURE_TARGET_LIVE_MULTIPLE, BLOCK_WORDS, YOUNG_SHRINK_FACTOR,
};
use beam_term::prelude::{make_small, signed_value, tuple_element, tuple_size, Eterm};

// ---------------------------------------------------------------------------
// The corpus and the policy
// ---------------------------------------------------------------------------

/// Words per `{a, 2a}` tuple: the frozen layout is `arity + 1` (a header plus two elements).
const TUPLE_WORDS: usize = 3;

/// The live set: 20 tuples re-built **fresh** every round, i.e. a bounded 60-word graph.
const LIVE_TUPLES: i64 = 20;

/// The live set in words — the only input the mature capacity may depend on.
const LIVE_WORDS: usize = LIVE_TUPLES as usize * TUPLE_WORDS;

/// A **big** live set (1 800 words) for the shrink case: large enough that the mature capacity it
/// asks for is more than [`YOUNG_SHRINK_FACTOR`] times the small live set's target, so the
/// policy's shrink bracket really fires afterwards.
const BIG_TUPLES: i64 = 600;

/// [`HeapPolicy::default`]'s `need_reserve_words`; the test asserts the live policy agrees.
const NEED_RESERVE_WORDS: usize = 64;

/// A dead occupancy comfortably **above** the nursery's live-set target, so the clamp bites.
///
/// The nursery's target for a 60-word live set is `young_target_words(60) = 233` words, so a
/// 4 000-word dead batch is ~17 times it: the shape a host's unrooted BIF copy-out produces.
const DEAD_ABOVE_TARGET_WORDS: usize = 4_000;

/// The same burst four times larger — the "history grew" perturbation. The mature capacity must
/// not see the difference.
const DEAD_ABOVE_TARGET_X4_WORDS: usize = 4 * DEAD_ABOVE_TARGET_WORDS;

/// A dead occupancy **below** the nursery's target: here the clamp is inert and the occupancy is
/// a genuine, bounded input to `need_after` — the term must not be dropped.
const DEAD_BELOW_TARGET_WORDS: usize = 100;

/// Every mature strategy: the copying space and the mark-region space.
const STRATEGIES: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

// ---------------------------------------------------------------------------
// The derived figures
// ---------------------------------------------------------------------------

/// The capacity the nursery will have after the collapse that follows a fullsweep keeping
/// [`LIVE_WORDS`] — the bound the fullsweep's young-occupancy term must be clamped to.
fn nursery_bound(policy: &HeapPolicy) -> usize {
    policy.young_target_words(LIVE_WORDS)
}

/// The mature capacity a fullsweep over the [`LIVE_WORDS`]-word live set must aim at, derived
/// from the **live set alone**: the term the policy's own rule produces when the nursery's
/// occupancy is bounded by the capacity that nursery is about to have.
fn live_set_target(policy: &HeapPolicy) -> usize {
    policy.mature_target_words(LIVE_WORDS, nursery_bound(policy))
}

/// Asserts a mature capacity is the live-set-derived figure for [`LIVE_WORDS`], per strategy: the
/// copying space installs the policy's size in whole words, the mark-region space that size
/// rounded up to whole [`BLOCK_WORDS`]-word blocks (it allocates by line).
fn assert_is_live_set_target(kind: MatureKind, policy: &HeapPolicy, capacity: usize, what: &str) {
    let target = live_set_target(policy);
    match kind {
        MatureKind::Copying => assert_eq!(
            capacity, target,
            "{kind:?}: {what} must be the live-set target mature_target_words({LIVE_WORDS}, \
             young_target_words({LIVE_WORDS}) = {}) = {target} words, got {capacity}",
            nursery_bound(policy)
        ),
        MatureKind::Immix => {
            let block_rounded = target.div_ceil(BLOCK_WORDS) * BLOCK_WORDS;
            assert!(
                (target..=block_rounded).contains(&capacity),
                "{kind:?}: {what} must be the live-set target {target} (nursery bound {}) \
                 rounded up to whole {BLOCK_WORDS}-word blocks ({block_rounded}), got {capacity}",
                nursery_bound(policy)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Builds a `{a, 2a}` tuple ([`TUPLE_WORDS`] words) in `c`'s nursery.
fn tuple(c: &mut AnyCollector, a: i64) -> Eterm {
    c.young_mut()
        .heap_mut()
        .tuple(&[make_small(a), make_small(2 * a)])
        .expect("a 3-word nursery tuple")
}

/// Builds the live set: [`LIVE_TUPLES`] fresh `{a, 2a}` tuples held in `roots`.
fn build_live(c: &mut AnyCollector, roots: &mut Vec<Eterm>, tag: i64) {
    build_tuples(c, roots, LIVE_TUPLES, tag);
}

/// Builds `count` fresh `{a, 2a}` tuples into `roots`, growing the nursery (without collecting)
/// whenever the current space is full — the escape hatch a caller needs for a graph larger than
/// one space.
fn build_tuples(c: &mut AnyCollector, roots: &mut Vec<Eterm>, count: i64, tag: i64) {
    for i in 0..count {
        if c.young().available_words() < TUPLE_WORDS {
            c.grow_nursery(TUPLE_WORDS)
                .expect("the default policy lets the nursery grow");
        }
        roots.push(tuple(c, tag * 1000 + i));
    }
}

/// Bump-allocates about `words`-worth of **dead** tuples into the nursery, growing it without
/// collecting whenever the current space is full — exactly what a host's unrooted BIF copy-out
/// does when it bumps the process heap and never rewinds. Nothing here is reachable from a root,
/// so every word it builds is garbage the next collection reclaims.
///
/// Returns the number of words it built: `words` rounded up to whole [`TUPLE_WORDS`].
fn fill_dead(c: &mut AnyCollector, words: usize) -> usize {
    let mut built = 0usize;
    while built < words {
        if c.young().available_words() < TUPLE_WORDS {
            // A no-relocation growth: it retains the space it replaces, so the words built so far
            // stay addressable and the chain's summed occupancy keeps counting them.
            c.grow_nursery(TUPLE_WORDS)
                .expect("the default policy lets the nursery grow");
        }
        let _ = tuple(c, -(built as i64) - 1);
        built += TUPLE_WORDS;
    }
    built
}

/// Runs a **pinned** fullsweep (never a minor, never an escalation) and returns the mature
/// capacity the collection installed.
fn fullsweep(c: &mut AnyCollector, roots: &mut [Eterm]) -> usize {
    let collection = c
        .collect_kind(CollectionKind::Major, roots)
        .expect("the pinned fullsweep fits");
    assert_eq!(
        collection.kind,
        CollectionKind::Major,
        "collect_kind(Major) must run exactly a fullsweep"
    );
    assert_eq!(
        c.heap_used_words(),
        0,
        "a fullsweep evacuates the whole nursery chain"
    );
    c.mature_capacity_words()
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

/// A collector for `kind` under the **default** policy — the cadence the runtime lives at
/// (`fullsweep_after` 65535, no `min_heap_words`, the 233-word initial young/mature spaces).
fn collector(kind: MatureKind, policy: &HeapPolicy) -> AnyCollector {
    let config = GcConfig::copying(policy.clone()).with_mature(kind);
    let c = AnyCollector::from_config(&config).expect("collector");
    assert_eq!(c.kind(), kind, "{kind:?}: the configured strategy");
    c
}

// ---------------------------------------------------------------------------
// 1. Flat in the dead occupancy (and bounded, not dropped, below the target)
// ---------------------------------------------------------------------------

/// **The mature capacity does not follow the dead occupancy the nursery accumulated.**
///
/// Three identical collectors see the same 60-word live set and three different dead batches:
/// one *below* the nursery's own target (100 words) and two *above* it (4 000 and 16 000). If the
/// fullsweep fed the chain-summed occupancy into `need_after` unclamped, the capacity would be a
/// function of the dead words — three different, growing figures (the feedback). Clamped to the
/// capacity the nursery is about to have, all three sweeps install the same live-set figure:
///
/// * the **4 000**- and **16 000**-word batches are indistinguishable — that is the flatness the
///   fix buys, and it is a word count, not a timing;
/// * the **100**-word batch is *not* folded away: it is below the bound, so it is a genuine input
///   and the target is the larger `mature_target_words(60, 160)` — the bound bounds, it does not
///   pin. This is what keeps the term's purpose (a whole nursery's worth of promotion room, so a
///   *run* of minors fits between fullsweeps) intact.
#[test]
fn the_mature_capacity_is_flat_in_the_dead_occupancy() {
    let policy = HeapPolicy::default();
    assert_eq!(
        policy.need_reserve_words, NEED_RESERVE_WORDS,
        "the corpus' reserve constant must be the policy's"
    );
    let bound = nursery_bound(&policy);

    for kind in STRATEGIES {
        let mut capacities: Vec<usize> = Vec::new();
        let mut occupancies: Vec<usize> = Vec::new();

        for dead in [
            DEAD_BELOW_TARGET_WORDS,
            DEAD_ABOVE_TARGET_WORDS,
            DEAD_ABOVE_TARGET_X4_WORDS,
        ] {
            let mut c = collector(kind, &policy);
            let mut roots: Vec<Eterm> = Vec::new();
            build_live(&mut c, &mut roots, 1);
            let built = fill_dead(&mut c, dead);
            assert_eq!(
                built,
                dead.div_ceil(TUPLE_WORDS) * TUPLE_WORDS,
                "{kind:?}: the dead batch is whole tuples"
            );

            // The premise: the fullsweep really sees the dead words, and (for the two big
            // batches) they really exceed the capacity the nursery will be given back.
            let used_before = c.heap_used_words();
            assert_eq!(
                used_before,
                LIVE_WORDS + built,
                "{kind:?}: the chain-summed occupancy counts every built word"
            );
            if dead > DEAD_BELOW_TARGET_WORDS {
                assert!(
                    used_before > bound,
                    "{kind:?}: the {dead}-word dead batch must exceed the nursery's bound \
                     ({bound} words) for the clamp to bite, occupancy was {used_before}"
                );
            }

            let capacity = fullsweep(&mut c, &mut roots);
            // The live set survived the fullsweep intact.
            for (i, &t) in roots.iter().enumerate() {
                let a = 1000 + i as i64;
                // SAFETY: each root is a live `{a, 2a}` tuple this test built in the collector.
                assert_eq!(
                    unsafe { tuple_pair(t) },
                    (a, 2 * a),
                    "{kind:?}: live term {i} must survive the fullsweep"
                );
            }

            if dead == DEAD_BELOW_TARGET_WORDS {
                // Below the bound the occupancy is a real input, so this case is a *different*
                // target than the clamped ones — it pins that the fix bounds the term rather than
                // replacing it with a constant.
                let expected = policy.mature_target_words(LIVE_WORDS, used_before);
                match kind {
                    MatureKind::Copying => assert_eq!(
                        capacity, expected,
                        "{kind:?}: below the bound the occupancy is still folded into \
                         need_after, so the target is mature_target_words({LIVE_WORDS}, \
                         {used_before}) = {expected}, got {capacity}"
                    ),
                    MatureKind::Immix => {
                        let block_rounded = expected.div_ceil(BLOCK_WORDS) * BLOCK_WORDS;
                        assert!(
                            (expected..=block_rounded).contains(&capacity),
                            "{kind:?}: below the bound the target is {expected} (block-rounded \
                             {block_rounded}), got {capacity}"
                        );
                    }
                }
                assert_ne!(
                    expected,
                    live_set_target(&policy),
                    "{kind:?}: the below-bound case must be distinguishable from the clamped one, \
                     otherwise this test could not see a constant"
                );
            } else {
                assert_is_live_set_target(kind, &policy, capacity, "a fullsweep over a dead burst");
            }

            capacities.push(capacity);
            occupancies.push(used_before);
        }

        // The flatness: the two dead batches above the bound are irrelevant to the capacity.
        assert_eq!(
            capacities[1], capacities[2],
            "{kind:?}: the mature capacity must not depend on the dead occupancy — {} words of \
             dead garbage and {} words both answer {} vs {} words (occupancies {occupancies:?})",
            DEAD_ABOVE_TARGET_WORDS, DEAD_ABOVE_TARGET_X4_WORDS, capacities[1], capacities[2]
        );
        assert!(
            capacities[2] < occupancies[2],
            "{kind:?}: the mutable figure ({}) must be far below the dead occupancy it saw \
             ({})", capacities[2], occupancies[2]
        );

        eprintln!(
            "{kind:?}: dead occupancy {occupancies:?} words -> mature capacity {capacities:?} \
             words (nursery bound {bound}, live {LIVE_WORDS} words, live-set target {})",
            live_set_target(&policy)
        );
    }
}

// ---------------------------------------------------------------------------
// 2. No feedback: consecutive fullsweeps stay at the fixed point
// ---------------------------------------------------------------------------

/// **Two consecutive fullsweeps over an unchanged live set leave the mature capacity where the
/// policy's convergence rule puts it** — never `2 × previous`.
///
/// The shape is the *cumulative* one a runaway host produces: each round accumulates a dead
/// batch twice the previous round's (K, 2K, 4K) with the same 60-word live set. The mature
/// capacity is asserted, after every sweep, to be exactly the rule's own re-derivation
/// `mature_after_fullsweep(capacity_before, live, young_target_words(live))` and to be the
/// same live-set figure the first sweep installed. Under the feedback each round's capacity would
/// grow with that round's dead words (the measured `3.8 M → 26 M → 590 M` walk); here the series
/// is constant, so no round can double the previous one.
#[test]
fn consecutive_fullsweeps_stay_at_the_mature_fixed_point() {
    let policy = HeapPolicy::default();

    for kind in STRATEGIES {
        let mut c = collector(kind, &policy);
        let mut roots: Vec<Eterm> = Vec::new();
        let mut series: Vec<(usize, usize, usize)> = Vec::new(); // (before, after, occupancy)

        for (round, dead) in [1usize, 2, 4]
            .into_iter()
            .map(|k| k * DEAD_ABOVE_TARGET_WORDS)
            .enumerate()
        {
            // The **same** live set every round: 20 fresh `{a, 2a}` tuples, the previous round's
            // graph becoming garbage.
            roots.clear();
            build_live(&mut c, &mut roots, round as i64 + 1);
            fill_dead(&mut c, dead);

            let capacity_before = c.mature_capacity_words();
            let occupancy = c.heap_used_words();
            let capacity_after = fullsweep(&mut c, &mut roots);

            // The rule's own answer for what it was given, with the occupancy bounded by the
            // capacity the nursery is about to have. The copying space installs that answer in
            // whole words; the mark-region space installs it rounded up to whole blocks, which
            // `assert_is_live_set_target` pins for both strategies.
            let bound = occupancy.min(nursery_bound(&policy));
            if kind == MatureKind::Copying {
                let derived =
                    policy.mature_after_fullsweep(capacity_before, LIVE_WORDS, bound);
                assert_eq!(
                    capacity_after, derived,
                    "{kind:?}: round {round} (dead {dead}, occupancy {occupancy}) must install \
                     mature_after_fullsweep({capacity_before}, {LIVE_WORDS}, {bound}) = \
                     {derived}, got {capacity_after}"
                );
            }
            assert_is_live_set_target(
                kind,
                &policy,
                capacity_after,
                "every fullsweep of the cumulative-burst series",
            );
            series.push((capacity_before, capacity_after, occupancy));
        }

        // No feedback: the capacity series is constant, so no round doubles its predecessor.
        let afters: Vec<usize> = series.iter().map(|&(_, after, _)| after).collect();
        assert_eq!(
            afters[0], afters[1],
            "{kind:?}: the second consecutive fullsweep must not move the capacity: {series:?}"
        );
        assert_eq!(
            afters[1], afters[2],
            "{kind:?}: the third consecutive fullsweep must not move the capacity: {series:?}"
        );
        for (i, &(_, after, _)) in series.iter().enumerate() {
            if i > 0 {
                let previous = series[i - 1].1;
                assert!(
                    after <= previous,
                    "{kind:?}: the capacity must never ratchet (round {i}: {previous} -> \
                     {after}): {series:?}"
                );
            }
        }
        // And it is a real, useful capacity: it covers the live set and leaves room for a *run*
        // of promotions (that is what `MATURE_TARGET_LIVE_MULTIPLE` buys).
        let target = live_set_target(&policy);
        assert!(
            afters[2] >= target,
            "{kind:?}: the fixed point ({}) must cover the live-set target ({target})",
            afters[2]
        );
        assert!(
            afters[2]
                >= MATURE_TARGET_LIVE_MULTIPLE
                    * policy.need_after_fullsweep(LIVE_WORDS, nursery_bound(&policy)),
            "{kind:?}: the fixed point must hold {MATURE_TARGET_LIVE_MULTIPLE} `need_after` \
             budgets so a run of minors fits"
        );
        assert!(
            afters[2] <= YOUNG_SHRINK_FACTOR * target,
            "{kind:?}: the fixed point must stay inside the policy band around {target}, got {}",
            afters[2]
        );

        eprintln!(
            "{kind:?}: (capacity before, capacity after, young occupancy) per fullsweep = \
             {series:?}; fixed point = {} words, live-set target = {target} words",
            afters[2]
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Both strategies agree on the live-set-derived figure
// ---------------------------------------------------------------------------

/// **`AnyCollector`'s two strategies converge on the same live-set-derived figure.**
///
/// The clamp is one rule that both fullsweeps apply (`Collector::collect_major` and
/// `GenImmixCollector::run_major`), so a dead burst must land them on the same *target*: the
/// copying space's capacity is that target in whole words, the mark-region capacity is the same
/// target rounded up to whole blocks (its line granularity), and neither moves when the dead
/// occupancy goes from K to 4K. The assertions are on the two capacities and on the shared
/// derivation, so a divergence between the strategies — the failure mode a fix applied to only
/// one of them would produce — is red.
#[test]
fn both_strategies_agree_on_the_live_set_figure() {
    let policy = HeapPolicy::default();
    let target = live_set_target(&policy);
    let block_rounded = target.div_ceil(BLOCK_WORDS) * BLOCK_WORDS;

    let mut converged: Vec<(MatureKind, usize, usize)> = Vec::new(); // (kind, K, 4K)
    for kind in STRATEGIES {
        let mut capacities: Vec<usize> = Vec::new();
        for dead in [DEAD_ABOVE_TARGET_WORDS, DEAD_ABOVE_TARGET_X4_WORDS] {
            let mut c = collector(kind, &policy);
            let mut roots: Vec<Eterm> = Vec::new();
            build_live(&mut c, &mut roots, 1);
            fill_dead(&mut c, dead);
            capacities.push(fullsweep(&mut c, &mut roots));
        }
        assert_eq!(
            capacities[0], capacities[1],
            "{kind:?}: the strategy must itself be flat in the dead occupancy: {capacities:?}"
        );
        converged.push((kind, capacities[0], capacities[1]));
    }

    let (copying, immix) = (converged[0], converged[1]);
    assert_eq!(copying.0, MatureKind::Copying);
    assert_eq!(immix.0, MatureKind::Immix);
    // The shared derivation: the copying capacity *is* the live-set target and the mark-region
    // capacity is that same target rounded up to whole blocks — one figure, two granularities.
    assert_eq!(
        copying.1, target,
        "the copying capacity ({}) must be the live-set target ({target})",
        copying.1
    );
    assert!(
        (target..=block_rounded).contains(&immix.1),
        "the mark-region capacity ({}) must be the same target {target} rounded up to whole \
         {BLOCK_WORDS}-word blocks ({block_rounded})",
        immix.1
    );
    // And therefore the two strategies are inside one block of each other, not merely both
    // "bounded".
    assert!(
        immix.1.abs_diff(copying.1) < BLOCK_WORDS,
        "the two strategies must agree up to the mark-region block granularity: copying {} vs \
         immix {}",
        copying.1,
        immix.1
    );

    eprintln!(
        "both strategies: K/4K = {} / {} words (copying), {} / {} words (immix), live-set target \
         {target} words",
        copying.1, copying.2, immix.1, immix.2
    );
}

// ---------------------------------------------------------------------------
// 4. The shrink still happens
// ---------------------------------------------------------------------------

/// **A dead burst's capacity is genuinely given back, not merely capped.**
///
/// A big live set (1 800 words) is built and fullswept, which asks the policy for its own large
/// mature capacity; the live set is then dropped and replaced by the 60-word one, with a big dead
/// batch on top. The next fullsweep must **shrink** the capacity back onto the small live set's
/// target (`mature_after_fullsweep`'s `should_shrink` bracket) — the oracle's own behaviour, where
/// a fullsweep frees the old block and the reported total comes back down after a compile
/// (`erl_gc.c:1875-1888`). So the figure tracks the live set in *both* directions.
#[test]
fn a_dropped_live_set_gives_the_mature_capacity_back() {
    let policy = HeapPolicy::default();

    for kind in STRATEGIES {
        let mut c = collector(kind, &policy);

        // The big phase: a 1 800-word live set the mature space has to be sized for.
        let mut roots: Vec<Eterm> = Vec::new();
        build_tuples(&mut c, &mut roots, BIG_TUPLES, 1);
        let big_live = BIG_TUPLES as usize * TUPLE_WORDS;
        let big_capacity = fullsweep(&mut c, &mut roots);
        assert!(
            big_capacity > big_live,
            "{kind:?}: the big-phase capacity ({big_capacity}) must exceed the live set \
             ({big_live}) so the shrink below is observable"
        );
        // The premise of the case: the policy's shrink bracket will really be due at the small
        // live set's target, so the drop below is the shrink branch and not "already small".
        assert!(
            policy.should_shrink(big_capacity, live_set_target(&policy)),
            "{kind:?}: the big-phase capacity ({big_capacity}) must be more than \
             {YOUNG_SHRINK_FACTOR}x the small live set's target ({}) for the shrink to fire",
            live_set_target(&policy)
        );

        // The small phase: the big graph is dropped, the 60-word live set replaces it, and a
        // dead burst is on top — precisely the shape whose capacity must come back down.
        roots.clear();
        build_live(&mut c, &mut roots, 9);
        fill_dead(&mut c, DEAD_ABOVE_TARGET_X4_WORDS);
        let small_capacity = fullsweep(&mut c, &mut roots);

        assert!(
            small_capacity < big_capacity,
            "{kind:?}: the capacity must shrink after the big live set is dropped: {big_capacity} \
             -> {small_capacity} words"
        );
        assert_is_live_set_target(
            kind,
            &policy,
            small_capacity,
            "the fullsweep after the big live set was dropped",
        );
        // The live set is still intact after the shrink.
        for (i, &t) in roots.iter().enumerate() {
            let a = 9000 + i as i64;
            // SAFETY: each root is a live `{a, 2a}` tuple this test built in the collector.
            assert_eq!(
                unsafe { tuple_pair(t) },
                (a, 2 * a),
                "{kind:?}: live term {i} must survive the shrinking fullsweep"
            );
        }

        eprintln!(
            "{kind:?}: big live {big_live} words -> capacity {big_capacity}, dropped to \
             {LIVE_WORDS} words -> capacity {small_capacity} (live-set target {})",
            live_set_target(&policy)
        );
    }
}
