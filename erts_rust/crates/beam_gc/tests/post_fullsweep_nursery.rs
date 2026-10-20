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

//! **Post-fullsweep nursery sizing — one live-set target, installed at the fullsweep.**
//!
//! A fullsweep relocates every live young word into the mature generation, so the nursery it
//! leaves behind has no surviving young set of its own to size from. The young heap's
//! post-collection capacity is therefore **one live-set target**, derived from the whole-heap
//! live set the collection measured *exactly*:
//!
//! ```text
//! young_target_words(live) = next_words(2 * live + need_reserve_words, 0)
//! ```
//! ([`HeapPolicy::young_target_words`], `YOUNG_TARGET_LIVE_MULTIPLE` = 2).
//!
//! **A minor only keeps or shrinks.** Its cheap live figure is the mature generation's
//! occupancy — a minor promotes every live young word into it — and that figure still holds the
//! garbage the minors since the last fullsweep promoted, so it is an *over*-estimate. Growing
//! from an over-estimate would close a feedback loop through the nursery (a bigger nursery
//! admits a bigger promotion, which raises the occupancy that sizes the nursery), so the growth
//! decision belongs to the collection that measures the live set exactly
//! ([`HeapPolicy::young_after_fullsweep`] / [`Nursery::collapse_after_fullsweep`], reached only
//! from a fullsweep) or to an explicit request ([`Collector::ensure_capacity`]). A minor's seam
//! is [`HeapPolicy::young_after_collection`] / [`Nursery::collapse`], which are keep-or-shrink.
//!
//! The two branches of the fullsweep rule are one band around the target: the capacity is
//! **grown** to it whenever [`HeapPolicy::should_grow`] holds (`capacity < target`, or
//! `3 * capacity < 4 * target`), **shrunk** back to it once the live set has fallen
//! `YOUNG_SHRINK_FACTOR` = 8 times below it, and otherwise **kept**. So the keep band is
//! `[4/3 * target, 8 * target]`.
//!
//! The properties pinned here:
//!
//! 1. **An over-provisioned nursery shrinks** to the live set's target — exact derived values, no
//!    inequalities ([`a_fullsweep_whose_live_set_is_small_shrinks_to_the_live_set_target`]).
//! 2. **An undersized nursery grows** to the live set's target
//!    ([`a_fullsweep_whose_live_set_outgrows_the_nursery_grows_it`]), and the `gc_bound/churn`
//!    geometry follows its live set instead of keeping a fixed capacity
//!    ([`the_churn_geometry_follows_the_live_set_target`]).
//! 3. **A nursery inside the band keeps its capacity** — the churn protection that stops a
//!    collection from installing a space per round
//!    ([`a_fullsweep_whose_live_set_is_not_far_below_the_capacity_keeps_it`]).
//! 4. **The rule is a fixed point and never ratchets**: every result is either the collected
//!    capacity or the target, re-applying the rule reproduces it, and a **minor never grows the
//!    nursery** ([`the_live_set_target_is_a_fixed_point_and_never_ratchets`],
//!    [`a_minor_never_grows_the_nursery`]).
//! 5. **A fixed demand converges** to one constant capacity
//!    ([`a_fixed_demand_converges_to_one_constant_post_fullsweep_capacity`]).
//! 6. **Both mature strategies answer identically** for the same geometry
//!    ([`both_mature_strategies_size_the_nursery_identically`]).
//!
//! Every figure here is a count, so it is load-invariant; no timing is measured or asserted.

use beam_gc::prelude::{
    AnyCollector, CollectionKind, Collector, GcConfig, HeapObservable, HeapPolicy, HeapSpace,
    MatureKind, YOUNG_SHRINK_FACTOR, YOUNG_TARGET_LIVE_MULTIPLE,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, size_object, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The target of an **empty** live set: `young_target_words(0)` =
/// `next_words(need_reserve_words = 64, 0)` = 90 — the floor of the whole rule (a constant of the
/// policy, never of the workload).
const EMPTY_LIVE_TARGET: usize = 90;

/// A collector for `policy` with `kind` **pinned** (an explicit override of the automatic rule).
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::new(policy).with_mature(kind)).expect("collector")
}

/// Builds a tuple from `elems` in `space` (a collector's current nursery space).
fn tuple_in(space: &mut HeapSpace, elems: &[Eterm]) -> Eterm {
    space.heap_mut().tuple(elems).expect("nursery tuple")
}

/// Allocates 2-word cons cells in the current nursery space, dropping every one of them, until
/// fewer than `spare` words are left — a nursery whose **occupancy** is high and whose **live
/// set** is whatever the caller actually roots.
fn fill_with_garbage(c: &mut Collector, spare: usize) {
    while c.young().available_words() > spare {
        let _ = c
            .young_mut()
            .heap_mut()
            .cons(make_small(1), NIL)
            .expect("the loop leaves room for one cons cell");
    }
}

/// The same fill for the strategy-agnostic collector.
fn fill_with_garbage_any(c: &mut AnyCollector, spare: usize) {
    while c.young().available_words() > spare {
        let _ = c
            .young_mut()
            .heap_mut()
            .cons(make_small(1), NIL)
            .expect("the loop leaves room for one cons cell");
    }
}

/// Conses `cells` cells `[0, 1, … cells-1]` onto `NIL`, growing the nursery **through the
/// no-relocation seam** whenever the current space runs out, so the corpus always ends with the
/// whole list live and addressable. Returns the list head as a one-word root set.
fn build_list(c: &mut Collector, cells: usize) -> Vec<Eterm> {
    let mut roots = vec![NIL];
    for i in 0..cells {
        if c.young().available_words() < 2 {
            c.grow_nursery(2).expect("grow for one more cons cell");
        }
        let tail = roots[0];
        roots[0] = c
            .young_mut()
            .heap_mut()
            .cons(make_small(i as i64), tail)
            .expect("the growth left room for one cons cell");
    }
    roots
}

/// Structurally compares the live `term` against an independently built witness of the
/// [`build_list`] shape.
fn assert_list_is_intact(term: Eterm, cells: usize, what: &str) {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(4 * cells + 16);
    let mut expected = NIL;
    for i in 0..cells {
        expected = heap.cons(make_small(i as i64), expected).expect("witness");
    }
    assert!(term_eq(term, expected, &atoms), "{what}");
}

// ---------------------------------------------------------------------------
// 1. An over-provisioned nursery shrinks to the live set's target
// ---------------------------------------------------------------------------

/// A fullsweep over a nursery that was grown for a burst, filled, and then has almost nothing
/// live in it shrinks to `young_target_words(live)`.
///
/// **The derivation.** The growth seam takes the nursery to the 2586-word curated size, so the
/// chain is `233 + 2586 = 2819` words. The live set is a single 59-element tuple = **60 words**,
/// so the target is `next_words(2 * 60 + 64 = 184) = 233`, and `2819 > 8 * 233 = 1864`: the
/// nursery is more than the whole keep band above its target and gives the memory back.
#[test]
fn a_fullsweep_whose_live_set_is_small_shrinks_to_the_live_set_target() {
    let policy = HeapPolicy::default();
    let mut c = Collector::new(policy.clone()).expect("collector");
    assert_eq!(c.young().capacity_words(), 233, "the initial nursery");

    // A burst allocation: the growth seam installs the curated 2586-word space.
    c.grow_nursery(2_500).expect("the burst's growth");
    let collected_capacity = c.heap_size_words();
    assert_eq!(
        collected_capacity,
        233 + policy.next_words(2_500, 0),
        "the chain is the 233-word space the growth retained plus the 2586-word one it installed"
    );

    // The nursery is filled — the fullsweep will find it full — but almost all of it is garbage:
    // the only live term is a 59-element (60-word) tuple.
    let elems: Vec<Eterm> = (0..59).map(|i| make_small(i as i64)).collect();
    let mut roots = vec![tuple_in(c.young_mut(), &elems)];
    fill_with_garbage(&mut c, 4);
    let occupied = c.young().used_words();
    assert!(
        occupied > collected_capacity / 2,
        "the nursery the fullsweep sees is genuinely full ({occupied} of {collected_capacity})"
    );

    let live = 1 + elems.len();
    assert_eq!(
        size_object(roots[0]),
        live,
        "the live set is the 59-element tuple's 60 words"
    );
    let target = policy.young_target_words(live);
    assert_eq!(target, 233, "next_words(2 * 60 + 64 = 184) = 233");
    assert!(
        collected_capacity > YOUNG_SHRINK_FACTOR * target,
        "the shrink branch must hold: {collected_capacity} > {} * {target}",
        YOUNG_SHRINK_FACTOR
    );
    // The **occupancy** is not what decides: a nursery full of garbage is still full, so a
    // capacity-keyed or fill-keyed rule would keep it. This test therefore really pins the
    // live-set rule.
    assert!(
        occupied > 3 * collected_capacity / 4,
        "the round's occupancy ({occupied}) says nothing about the live set"
    );

    let stats = c.collect_major(&mut roots).expect("the fullsweep fits");
    assert_eq!(stats.words_copied, live, "exactly the live set was copied");
    assert_eq!(c.young_space_count(), 1, "the chain collapsed");
    assert_eq!(c.young().used_words(), 0);

    assert_eq!(
        c.young().capacity_words(),
        target,
        "the over-provisioned nursery shrinks to young_target_words({live}) = {target}"
    );
    assert!(c.young().capacity_words() < collected_capacity);
}

// ---------------------------------------------------------------------------
// 2. An undersized nursery grows to the live set's target
// ---------------------------------------------------------------------------

/// The **growth** half, which only a live-set target can express: a fullsweep that kept more than
/// the nursery is sized for **installs the target**, so the nursery stops re-copying the live set
/// every round.
///
/// **The derivation.** `grow_nursery(400)` installs the 610-word curated space (chain
/// `233 + 610 = 843`), and the live set is a 200-cell list = **400 words**. The target is
/// `next_words(864) = 987`, and `3 * 843 = 2529 < 4 * 987 = 3948`, so `should_grow` holds and the
/// fullsweep grows the nursery to 987 — a nursery that keeps its live set rather than re-copying
/// it every collection (the copy volume per allocated word is `live / nursery`).
#[test]
fn a_fullsweep_whose_live_set_outgrows_the_nursery_grows_it() {
    let policy = HeapPolicy::default();
    let mut c = Collector::new(policy.clone()).expect("collector");
    c.grow_nursery(400).expect("grow for the list");
    let collected_capacity = c.heap_size_words();
    assert_eq!(collected_capacity, 233 + policy.next_words(400, 0));

    let cells = 200;
    let mut roots = build_list(&mut c, cells);
    let live = 2 * cells;
    assert_eq!(size_object(roots[0]), live);
    let target = policy.young_target_words(live);
    assert_eq!(target, 987, "next_words(2 * 400 + 64 = 864) = 987");
    assert!(
        target > collected_capacity,
        "the live set's target ({target}) is larger than the collected chain \
         ({collected_capacity})"
    );
    assert!(
        policy.should_grow(collected_capacity, target),
        "the grow branch must hold: 3 * {collected_capacity} < 4 * {target}"
    );

    let stats = c.collect_major(&mut roots).expect("the fullsweep fits");
    assert_eq!(stats.words_copied, live);
    assert_eq!(c.young_space_count(), 1);
    assert_eq!(
        c.young().capacity_words(),
        target,
        "the fullsweep grows the nursery to young_target_words({live}) = {target}"
    );
    assert_list_is_intact(roots[0], cells, "the list survives the growth");

    // The result is a **fixed point**: the same live set re-collected keeps the capacity, so the
    // growth can never become a per-collection ratchet.
    c.collect_major(&mut roots).expect("the second fullsweep");
    assert_eq!(
        c.young().capacity_words(),
        target,
        "the target is a fixed point of its own rule"
    );
    assert_eq!(c.stats().major_collections, 2);
}

// ---------------------------------------------------------------------------
// 3. A nursery inside the band keeps its capacity
// ---------------------------------------------------------------------------

/// A fullsweep whose live set is **not** far below (nor above) the capacity keeps the capacity —
/// the churn protection: a resize here would reintroduce the per-round space install the
/// hysteresis band exists to remove.
///
/// **The derivation.** `grow_nursery(1_100)` installs the curated 1598-word space (chain
/// `233 + 1598 = 1831`), and the live set is a 200-cell list = **400 words**, so the target is
/// `next_words(864) = 987`. The keep band is `[4/3 * 987, 8 * 987] = [1316, 7896]`, and 1831 is
/// inside it on both sides: `should_grow(1831, 987)` is false (`3 * 1831 = 5493 >= 4 * 987`) and
/// `should_shrink` likewise.
#[test]
fn a_fullsweep_whose_live_set_is_not_far_below_the_capacity_keeps_it() {
    let policy = HeapPolicy::default();
    let mut c = Collector::new(policy.clone()).expect("collector");
    c.grow_nursery(1_100).expect("grow");
    let collected_capacity = c.heap_size_words();
    assert_eq!(collected_capacity, 233 + policy.next_words(1_100, 0));

    let cells = 200;
    let mut roots = build_list(&mut c, cells);
    fill_with_garbage(&mut c, 4);
    let live = 2 * cells;
    assert_eq!(size_object(roots[0]), live);
    let target = policy.young_target_words(live);
    assert!(
        !policy.should_grow(collected_capacity, target),
        "the grow branch must NOT hold: 3 * {collected_capacity} >= 4 * {target}"
    );
    assert!(
        collected_capacity <= YOUNG_SHRINK_FACTOR * target,
        "the shrink branch must NOT hold: {collected_capacity} <= {} * {target}",
        YOUNG_SHRINK_FACTOR
    );

    let stats = c.collect_major(&mut roots).expect("the fullsweep fits");
    assert_eq!(stats.words_copied, live);
    assert_eq!(c.young_space_count(), 1);
    assert_eq!(
        c.young().capacity_words(),
        collected_capacity,
        "a live set of {live} words (target {target}) keeps the {collected_capacity}-word nursery"
    );
    // The consequence the band exists for: the demand that filled the nursery still fits, so the
    // next round neither collects over occupancy nor installs a fresh space.
    assert!(!c.should_collect(live), "the live set still fits");
    assert!(!c.collection_debt(), "the collapse settled the chain");
    assert_list_is_intact(roots[0], cells, "the list survives the fullsweep");
}

/// The **`gc_bound/churn` geometry** itself, at the policy level: the churn shape's settled
/// nursery (609 words) against the fullsweep live sets it measures (464…1353 words).
///
/// Under a live-set target the nursery no longer *keeps* 609 words there — it follows the live
/// set — and the three derived targets are pinned exactly. That is the point of the rule: the
/// capacity is a workload property, so a shape whose live set is three times its nursery gets a
/// nursery of the live set's scale and copies it a third as often per allocated word.
#[test]
fn the_churn_geometry_follows_the_live_set_target() {
    let policy = HeapPolicy::default();
    // The shape's measured extremes (`tests/churn_attribution.rs`, `MEASURED.md`): settled
    // nursery capacity 609 words against a fullsweep live set of 464…1353 words.
    let capacity = 609;
    for (live, want) in [(464usize, 1_598usize), (900, 2_586), (1_353, 4_185)] {
        let target = policy.young_target_words(live);
        assert_eq!(
            target, want,
            "next_words(2 * {live} + 64 = {}) = {want}",
            2 * live + policy.need_reserve_words
        );
        assert!(
            policy.should_grow(capacity, target),
            "the churn shape's live set {live} outgrows its {capacity}-word nursery"
        );
        assert_eq!(
            policy.young_after_fullsweep(capacity, live),
            want,
            "a fullsweep over the churn shape installs the live-set target {want} (live {live})"
        );
    }
    // ... and the rule is *not* a function of the collected capacity: the same live set answers
    // the same target whatever nursery it is measured against, so the nursery a fullsweep leaves
    // is a property of the workload rather than of the space it happened to collect.
    for live in [464usize, 900, 1_353] {
        let target = policy.young_target_words(live);
        for collected in [capacity, 233, 987, 2_819, 4_185] {
            let got = policy.young_after_fullsweep(collected, live);
            assert!(
                got == collected || got == target,
                "live {live} / collected {collected}: the rule answers the capacity or the target"
            );
            if got == collected {
                // A kept capacity is inside the band around the target, never a scale of its own.
                assert!(
                    collected <= YOUNG_SHRINK_FACTOR * target,
                    "live {live}: a kept {collected}-word capacity exceeds {} * {target}",
                    YOUNG_SHRINK_FACTOR
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 4. The rule is a fixed point and never ratchets; a minor never grows
// ---------------------------------------------------------------------------

/// The bounded-growth argument, in three parts.
#[test]
fn the_live_set_target_is_a_fixed_point_and_never_ratchets() {
    let policy = HeapPolicy::default();

    // (a) For every collected capacity and every live set a fullsweep can measure, the result is
    // one of exactly two numbers — the capacity the chain had, or the live set's target. There is
    // no third possibility, so nothing else can leak in (the property is checked exhaustively over
    // the growth table).
    for &capacity in beam_gc::policy::heap_sizes().iter().take(24) {
        for live in [
            0,
            1,
            12,
            policy.need_reserve_words,
            EMPTY_LIVE_TARGET,
            233,
            1_000,
            capacity / 4,
            capacity / 2,
            capacity,
            4 * capacity,
        ] {
            let target = policy.young_target_words(live);
            let got = policy.young_after_fullsweep(capacity, live);
            assert!(
                got == capacity || got == target,
                "capacity {capacity} / live {live}: got {got}, which is neither the capacity nor \
                 the target {target}"
            );
            assert!(
                got <= capacity.max(target),
                "capacity {capacity} / live {live}: {got} exceeds both the capacity and the \
                 target {target}"
            );
            // A *kept* capacity is inside the band; an installed one is exactly the target.
            if got == capacity {
                assert!(
                    capacity <= YOUNG_SHRINK_FACTOR * target,
                    "capacity {capacity} / live {live}: a kept capacity must be within {}x of \
                     the target {target}",
                    YOUNG_SHRINK_FACTOR
                );
                assert!(
                    !policy.should_grow(capacity, target) || target <= capacity,
                    "capacity {capacity} / live {live}: a kept capacity must not want to grow"
                );
            }
            // (b) It is a fixed point: re-applying the rule at its own result reproduces it, so a
            // fixed workload can never climb the growth table one step per collection.
            assert_eq!(
                policy.young_after_fullsweep(got, live),
                got,
                "capacity {capacity} / live {live}: {got} must be a fixed point"
            );
        }
    }

    // The empty live set's target is the policy's reserve floor, and an over-provisioned nursery
    // really does collapse to it.
    assert_eq!(policy.young_target_words(0), EMPTY_LIVE_TARGET);
    assert_eq!(policy.young_after_fullsweep(100_000, 0), EMPTY_LIVE_TARGET);
    assert_eq!(
        policy.next_words(policy.need_reserve_words, 0),
        EMPTY_LIVE_TARGET
    );

    // (c) The collector: every fullsweep, over a spread of demands, live sets and starting
    // geometries, leaves one space holding either the collected chain's capacity or the target —
    // and never a capacity that grows with the round number.
    for need in [0usize, 4, 64, 200, 700, 2_000] {
        let mut c = Collector::new(HeapPolicy::default()).expect("collector");
        let mut roots = vec![NIL];
        for round in 0..40 {
            // Alternate the demand so every branch of the rule is reached (grow-then-collect,
            // keep, shrink).
            let want = need + (round % 3) * 64;
            if c.young().available_words() < want {
                c.grow_nursery(want).expect("grow");
            }
            if want > 1 {
                let elems: Vec<Eterm> = (0..want - 1).map(|i| make_small(i as i64)).collect();
                roots[0] = tuple_in(c.young_mut(), &elems);
            } else {
                roots[0] = NIL;
            }
            let before = c.heap_size_words();
            let live = size_object(roots[0]);
            let stats = c.collect_major(&mut roots).expect("fullsweep");
            assert_eq!(stats.words_copied, live, "a fullsweep copies the live set");
            let after = c.young().capacity_words();
            let target = policy.young_target_words(live);
            assert!(
                after == before || after == target,
                "need {need} round {round}: a fullsweep answered {after}, neither the collected \
                 {before} nor the target {target} (copied {live})"
            );
            assert!(
                after <= before.max(target),
                "need {need} round {round}: a fullsweep grew the nursery past both the chain \
                 ({before}) and the target ({target}) to {after}"
            );
            assert_eq!(c.young_space_count(), 1, "the chain collapsed");
        }
    }
}

/// The complementary half of the rule: **a minor keeps or shrinks, never grows** — its live
/// figure is the mature generation's (garbage-inflated) occupancy, so a growth branch there would
/// feed itself through the nursery.
#[test]
fn a_minor_never_grows_the_nursery() {
    let policy = HeapPolicy::default();

    // (a) The policy function: for every capacity and live set, the minor rule answers a size no
    // larger than the capacity it was given.
    for &capacity in beam_gc::policy::heap_sizes().iter().take(24) {
        for live in [0, 1, 12, 233, 1_000, capacity / 4, capacity, 4 * capacity] {
            let got = policy.young_after_collection(capacity, live);
            assert!(
                got <= capacity,
                "young_after_collection({capacity}, {live}) = {got} grew the nursery"
            );
            assert!(
                got == capacity || got == policy.young_target_words(live),
                "young_after_collection({capacity}, {live}) = {got}: it keeps or installs the \
                 target"
            );
        }
    }

    // (b) The collector: a minor over a live set that is *larger* than the nursery it collects
    // leaves the nursery at its size (the mature occupancy is an over-estimate, and the band
    // absorbs it), while a minor over a live set far below it gives the memory back.
    let mut c = Collector::new(HeapPolicy {
        initial_mature_words: 1_024,
        ..policy.clone()
    })
    .expect("collector");
    c.grow_nursery(400).expect("grow");
    let cells = 200;
    let mut roots = build_list(&mut c, cells);
    let live = 2 * cells;
    // The collapse sizes from the **chain's** summed capacity (`233 + 610 = 843`), not from the
    // current space's: that is the figure a collection has to give back.
    let before = c.heap_size_words();
    let collection = c.collect(&mut roots).expect("minor");
    assert_eq!(collection.kind, CollectionKind::Minor);
    assert_eq!(
        collection.stats.words_copied, live,
        "the minor promoted the whole live set"
    );
    assert_eq!(
        c.young().capacity_words(),
        before,
        "a minor keeps the nursery it collected ({before} words of chain, live {live})"
    );
    assert!(before >= 610, "the grown chain is the capacity in question");

    // A nursery the round left almost entirely dead does shrink — the memory really is given back.
    let mut c = Collector::new(HeapPolicy {
        initial_young_words: 4_185,
        initial_mature_words: 8_192,
        ..policy
    })
    .expect("collector");
    let elems: Vec<Eterm> = (0..2).map(|i| make_small(i as i64)).collect();
    let mut roots = vec![tuple_in(c.young_mut(), &elems)];
    fill_with_garbage(&mut c, 4);
    let collection = c.collect(&mut roots).expect("minor");
    assert_eq!(collection.kind, CollectionKind::Minor);
    assert_eq!(
        c.young().capacity_words(),
        EMPTY_LIVE_TARGET,
        "a nursery holding 3 live words of garbage shrinks to the empty live set's target"
    );
}

// ---------------------------------------------------------------------------
// 5. A fixed demand converges to one constant capacity
// ---------------------------------------------------------------------------

#[test]
fn a_fixed_demand_converges_to_one_constant_post_fullsweep_capacity() {
    const ROUNDS: usize = 300;
    /// The op's allocation — a tuple, abandoned at the end of every round.
    const OP_WORDS: usize = 300;
    /// The tail the plateau is pinned over.
    const TAIL: usize = 100;

    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut c = any_for(kind, HeapPolicy::default());
        let elems: Vec<Eterm> = (0..OP_WORDS - 1).map(|i| make_small(i as i64)).collect();
        // A root the collector keeps: the op's result is live for the round that built it.
        let mut roots = vec![NIL];
        let mut capacities: Vec<usize> = Vec::with_capacity(ROUNDS);
        let mut collections = 0usize;

        for _ in 0..ROUNDS {
            // The heap-admission door: collect (and grow when the request needs it) only when
            // the current space cannot hold the op's allocation — so collections happen
            // because the nursery filled, exactly as in the runtime's `collect_or_grow`.
            if c.should_collect(OP_WORDS) || c.collection_debt() {
                let _ = c.ensure_capacity(OP_WORDS, &mut roots).expect("the door");
                collections += 1;
            }
            roots[0] = tuple_in(c.young_mut(), &elems);
            capacities.push(c.young().capacity_words());
        }

        assert!(collections > 0, "{kind:?}: the door collected");
        let tail = &capacities[ROUNDS - TAIL..];
        assert!(
            tail.iter().all(|cap| *cap == tail[0]),
            "{kind:?}: a fixed demand must converge to ONE nursery capacity, got {:?}",
            tail.iter().copied().collect::<std::collections::BTreeSet<usize>>()
        );
        // ... and that constant is the live set's own target — a property of the workload, not a
        // function of the iteration count (a creep would show up as a rising tail above).
        let live = size_object(roots[0]);
        let target = HeapPolicy::default().young_target_words(live);
        assert_eq!(
            tail[0], target,
            "{kind:?}: the converged capacity {} must be young_target_words({live}) = {target}",
            tail[0]
        );
        assert!(
            tail[0] <= 8 * OP_WORDS,
            "{kind:?}: the converged capacity {} must stay a small multiple of {OP_WORDS}",
            tail[0]
        );
    }
}

// ---------------------------------------------------------------------------
// 6. Both mature strategies answer identically for the same geometry
// ---------------------------------------------------------------------------

/// The live-set rule is a *policy* rule, so the copying and the mark-region collector must agree
/// on it for the same nursery geometry: same collected capacity, same live set, same resulting
/// nursery capacity. (Their mature generations are sized and reclaimed differently — that is the
/// whole difference between the strategies — but the nursery half is shared.)
#[test]
fn both_mature_strategies_size_the_nursery_identically() {
    // The over-provisioned geometry of test 1: 2819 words collected, 60 words live.
    let mut results = Vec::new();
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut c = any_for(kind, HeapPolicy::default());
        c.grow_nursery(2_500).expect("the burst's growth");
        assert_eq!(
            c.heap_size_words(),
            233 + HeapPolicy::default().next_words(2_500, 0),
            "{kind:?}: the chain is the 233-word space the growth retained plus the 2586-word one \
             it installed"
        );
        let elems: Vec<Eterm> = (0..59).map(|i| make_small(i as i64)).collect();
        let mut roots = vec![tuple_in(c.young_mut(), &elems)];
        fill_with_garbage_any(&mut c, 4);
        let stats = c.collect_major(&mut roots).expect("the fullsweep fits");
        assert_eq!(stats.words_copied, 60, "{kind:?}: exactly the live set moved");
        results.push((kind, c.young().capacity_words()));
    }
    assert_eq!(
        results[0].1, results[1].1,
        "both strategies size the over-provisioned nursery identically: {results:?}"
    );
    assert_eq!(
        results[0].1,
        HeapPolicy::default().young_target_words(60),
        "and that size is the live set's target"
    );
    assert_eq!(results[0].1, 233);
}

/// ... and the same for the **growth** half: an undersized nursery is grown to the same target by
/// either mature strategy.
#[test]
fn both_mature_strategies_grow_the_nursery_identically() {
    let mut results = Vec::new();
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut c = any_for(kind, HeapPolicy::default());
        c.grow_nursery(400).expect("grow for the list");
        let mut roots = vec![NIL];
        for i in 0..200 {
            if c.young().available_words() < 2 {
                c.grow_nursery(2).expect("grow for one more cons cell");
            }
            let tail = roots[0];
            roots[0] = c
                .young_mut()
                .heap_mut()
                .cons(make_small(i as i64), tail)
                .expect("room for one cons cell");
        }
        let before = c.heap_size_words();
        let stats = c.collect_major(&mut roots).expect("the fullsweep fits");
        assert_eq!(stats.words_copied, 400, "{kind:?}: the 400-word list");
        assert!(
            c.young().capacity_words() > before,
            "{kind:?}: the fullsweep grew the nursery ({before} -> {})",
            c.young().capacity_words()
        );
        results.push((kind, c.young().capacity_words()));
    }
    assert_eq!(results[0].1, results[1].1, "{results:?}");
    assert_eq!(results[0].1, HeapPolicy::default().young_target_words(400));
    assert_eq!(results[0].1, 987);
}

/// The constants the rule is built from, pinned so a change to either has to come through this
/// file: the multiple of the live set a nursery aims at, and the shrink band around it.
#[test]
fn the_rule_constants_are_pinned() {
    assert_eq!(YOUNG_TARGET_LIVE_MULTIPLE, 2);
    assert_eq!(YOUNG_SHRINK_FACTOR, 8);
    let policy = HeapPolicy::default();
    assert_eq!(policy.need_reserve_words, 64);
    // A target is `next_words(multiple * live + reserve)`.
    for live in [0usize, 1, 60, 400, 1_353] {
        assert_eq!(
            policy.young_target_words(live),
            policy.next_words(
                YOUNG_TARGET_LIVE_MULTIPLE * live + policy.need_reserve_words,
                0
            )
        );
    }
}
