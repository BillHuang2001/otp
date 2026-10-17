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

//! **Post-fullsweep nursery sizing — the C's two-step rule.**
//!
//! A fullsweep relocates every live young word into the mature generation, so the nursery it
//! leaves behind has no surviving young set of its own to size from. The C answers that with
//! **two** steps, and this file pins both:
//!
//! 1. `major_collection` (`erl_gc.c:1866`) sizes the receiving heap from the **pre-copy**
//!    occupancy (`new_sz = next_heap_size(stack_size + size_before + S_RESERVED, 0)`). Here that
//!    is [`HeapPolicy::young_after_collection`] over the occupancy the round sampled *before* the
//!    copy, so a nursery the mutator has genuinely filled keeps its capacity and the workload
//!    stops paying a collect-and-grow per few hundred admitted words (`MEASURED.md`,
//!    "Post-fullsweep nursery sizing").
//! 2. `adjust_after_fullsweep` (`erl_gc.c:2062-2098`) then runs **after** the copy and re-sizes
//!    off the **post-copy live set**: when `4 * need_after < HEAP_SIZE` (and `HEAP_SIZE >
//!    H_MIN_SIZE`) it shrinks to `wanted = 2 * need_after`, where `need_after = live +
//!    need_reserve_words`. Here that is [`HeapPolicy::young_after_fullsweep`]'s second step.
//!
//! Step 2 is the half step 1 cannot see: a fullsweep fires *when the nursery is full*, so the
//! pre-copy occupancy is ~the capacity and would never trigger a shrink. Only the size of what
//! **survived** says whether the nursery is over-provisioned — the case a heap the growth seam
//! stepped up for a burst and then stopped using lands in, where step 1 alone keeps a nursery so
//! large that the minors the generational cadence relies on stop happening.
//!
//! The properties pinned:
//!
//! 1. **An over-provisioned nursery shrinks** toward `next_words(2 * (live + reserve))` — exact
//!    derived values, no inequalities ([`a_fullsweep_whose_live_set_is_small_shrinks_toward_twice_need_after`]).
//! 2. **A nursery that is not far over-provisioned keeps its capacity** — the churn protection,
//!    with the real `gc_bound/churn` geometry pinned too
//!    ([`a_fullsweep_whose_live_set_is_not_far_below_the_capacity_keeps_it`]).
//! 3. **Step 2 never grows the nursery and is a fixed point of itself**, so a fixed workload
//!    converges instead of creeping or oscillating
//!    ([`the_post_fullsweep_nursery_never_exceeds_the_collected_capacity`],
//!    [`a_fixed_demand_converges_to_one_constant_post_fullsweep_capacity`]).
//! 4. **Both mature strategies answer identically** for the same geometry
//!    ([`both_mature_strategies_size_the_nursery_identically`]).
//!
//! Every figure here is a count, so it is load-invariant; no timing is measured or asserted.

use beam_gc::prelude::{
    AnyCollector, Collector, GcConfig, HeapObservable, HeapPolicy, HeapSpace, MatureKind,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, size_object, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The reserve floor the *first* step collapses an untouched nursery to:
/// `young_after_collection(_, 0)` = `next_words(need_reserve_words = 64, 0)` = 90.
const RESERVE_FLOOR: usize = 90;

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
// 1. Step 2: an over-provisioned nursery shrinks toward 2 * need_after
// ---------------------------------------------------------------------------

/// A fullsweep over a nursery that was grown for a burst, filled, and then has almost nothing
/// live in it shrinks to `next_words(2 * (live + need_reserve_words))` — the C
/// `adjust_after_fullsweep`'s `wanted = 2 * need_after` (`erl_gc.c:2087`).
///
/// **The derivation.** The growth seam takes the nursery to the 2586-word curated size, so the
/// chain is `233 + 2586 = 2819` words and the round observes ~2586 words of occupancy — step 1
/// therefore keeps 2819 (`should_shrink(2819, 2586)` is false). The live set is a single
/// 59-element tuple = **60 words**, so `need_after = 60 + 64 = 124`, the gate
/// `4 * 124 = 496 < 2819` holds and step 2 shrinks to `next_words(2 * 124 = 248, 0) = 376`.
#[test]
fn a_fullsweep_whose_live_set_is_small_shrinks_toward_twice_need_after() {
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
    let observed = c.young().used_words();
    assert!(
        observed > collected_capacity / 2,
        "the nursery the fullsweep sees is genuinely full ({observed} of {collected_capacity})"
    );

    let live = 1 + elems.len();
    assert_eq!(
        size_object(roots[0]),
        live,
        "the live set is the 59-element tuple's 60 words"
    );
    let need_after = live + policy.need_reserve_words;
    assert!(
        4 * need_after < collected_capacity,
        "the shrink gate must hold: 4 * {need_after} < {collected_capacity}"
    );
    // Step 1 on its own keeps the capacity (the round observed it full) — the shrink below is
    // step 2's, so this test really pins the new rule.
    assert_eq!(
        policy.young_after_collection(collected_capacity, observed),
        collected_capacity,
        "step 1 (the pre-copy occupancy sizing) keeps the nursery"
    );

    let stats = c.collect_major(&mut roots).expect("the fullsweep fits");
    assert_eq!(stats.words_copied, live, "exactly the live set was copied");
    assert_eq!(c.young_space_count(), 1, "the chain collapsed");
    assert_eq!(c.young().used_words(), 0);

    // Step 2's exact target: `next_words(2 * need_after, 0)`.
    assert_eq!(policy.next_words(2 * need_after, 0), 376);
    assert_eq!(
        c.young().capacity_words(),
        376,
        "the over-provisioned nursery shrinks to next_words(2 * ({live} + 64)) = 376"
    );
    // ... and the shrink landed within the capacity step 1 chose, never above it.
    assert!(c.young().capacity_words() < collected_capacity);
}

// ---------------------------------------------------------------------------
// 2. Step 2 does not fire when the live set protects the nursery
// ---------------------------------------------------------------------------

/// A fullsweep whose live set is **not** far below the capacity keeps the capacity — the churn
/// protection: a shrink here would reintroduce the per-round space install the pre-copy sizing
/// removed.
///
/// **The derivation.** `grow_nursery(900)` installs the curated 987-word space (chain
/// `233 + 987 = 1220`), and the live set is a 200-cell list = **400 words**, so
/// `need_after = 400 + 64 = 464` and `4 * 464 = 1856 > 1220`: the gate fails and the nursery
/// keeps all 1220 words.
#[test]
fn a_fullsweep_whose_live_set_is_not_far_below_the_capacity_keeps_it() {
    let policy = HeapPolicy::default();
    let mut c = Collector::new(policy.clone()).expect("collector");
    c.grow_nursery(900).expect("grow");
    let collected_capacity = c.heap_size_words();
    assert_eq!(collected_capacity, 233 + policy.next_words(900, 0));

    let cells = 200;
    let mut roots = build_list(&mut c, cells);
    fill_with_garbage(&mut c, 4);
    let live = 2 * cells;
    assert_eq!(size_object(roots[0]), live);
    let need_after = live + policy.need_reserve_words;
    assert!(
        4 * need_after >= collected_capacity,
        "the shrink gate must NOT hold: 4 * {need_after} >= {collected_capacity}"
    );

    let stats = c.collect_major(&mut roots).expect("the fullsweep fits");
    assert_eq!(stats.words_copied, live);
    assert_eq!(c.young_space_count(), 1);
    assert_eq!(
        c.young().capacity_words(),
        collected_capacity,
        "a live set of {live} words protects the {collected_capacity}-word nursery from the \
         shrink (4 * {need_after} >= {collected_capacity})"
    );
    // The consequence the rule exists for: the demand that filled the nursery still fits, so the
    // next round neither collects over occupancy nor installs a fresh space.
    assert!(!c.should_collect(live), "the live set still fits");
    assert!(!c.collection_debt(), "the collapse settled the chain");
    assert_list_is_intact(roots[0], cells, "the list survives the fullsweep");
}

/// The **`gc_bound/churn` geometry** itself, at the policy level: the churn shape's settled
/// nursery (609 words) against the fullsweep live set it measures (464 words at the low end)
/// keeps its capacity, so the shape's `growths/op ≈ 0` survives the new rule.
#[test]
fn the_churn_geometry_is_protected_from_the_shrink() {
    let policy = HeapPolicy::default();
    // The shape's measured extremes (`tests/churn_attribution.rs`, `MEASURED.md`): peak nursery
    // capacity 609 words against a fullsweep live set of 464…1353 words.
    let capacity = 609;
    for live in [464usize, 900, 1_353] {
        let need_after = live + policy.need_reserve_words;
        assert!(
            4 * need_after >= capacity,
            "the churn shape's live set {live} must protect its {capacity}-word nursery"
        );
        assert_eq!(
            policy.young_after_fullsweep(capacity, capacity, live),
            capacity,
            "a fullsweep over the churn shape keeps its {capacity}-word nursery (live {live})"
        );
    }
    // ... and the shape at the *low* end of what a fullsweep can observe is still protected,
    // which is what `growths/op` depends on.
    assert_eq!(policy.young_after_fullsweep(capacity, 461, 464), capacity);
}

// ---------------------------------------------------------------------------
// 3. A collection can never enlarge the nursery, and step 2 is a fixed point
// ---------------------------------------------------------------------------

#[test]
fn the_post_fullsweep_nursery_never_exceeds_the_collected_capacity() {
    // (a) The policy function: for every occupancy and live set a caller can actually pass it
    // (`observed <= capacity`, because a space's occupancy cannot exceed its capacity) the target
    // is at most the capacity. That is the whole bounded-growth argument, checked exhaustively
    // over the growth table.
    let policy = HeapPolicy::default();
    for &capacity in beam_gc::policy::heap_sizes().iter().take(24) {
        // The one size a collection may reach *without* a larger demand: the policy's own
        // reserve floor, which a nursery smaller than the reserve is raised to (a constant of
        // the policy, not of the workload).
        let reserve_floor = policy.young_after_collection(capacity, 0);
        for demand in [
            0,
            1,
            policy.need_reserve_words,
            policy.need_reserve_words + 1,
            capacity / 4,
            capacity / 4 + 1,
            capacity / 2,
            capacity.saturating_sub(1),
            capacity,
        ] {
            for live in [0, 1, 12, 233, 1_000, capacity / 4, capacity] {
                let target = policy.young_after_fullsweep(capacity, demand, live);
                assert!(
                    target <= capacity.max(reserve_floor),
                    "demand {demand} / live {live} of capacity {capacity} must not grow the \
                     nursery (got {target})"
                );
                // Step 2 never returns more than step 1 chose.
                assert!(
                    target <= policy.young_after_collection(capacity, demand),
                    "step 2 must not exceed step 1's size for demand {demand} / live {live} of \
                     capacity {capacity} (got {target})"
                );
            }
        }
        // The one size a collection may *reach* for an empty live set is step 2's own target,
        // `next_words(2 * need_reserve_words, 0)` — the reserve is part of `need_after`, so an
        // over-provisioned nursery is never collapsed below twice the documented slack.
        let empty_target = policy.next_words(2 * policy.need_reserve_words, 0);
        let kept = policy.young_after_collection(capacity, capacity);
        let expected = if policy.should_shrink(kept, policy.need_reserve_words) {
            empty_target.min(kept)
        } else {
            kept
        };
        assert_eq!(policy.young_after_fullsweep(capacity, capacity, 0), expected);
        assert_eq!(RESERVE_FLOOR, policy.next_words(policy.need_reserve_words, 0));
    }

    // (b) Step 2 is a fixed point of itself: a nursery it just shrank to, collected again over a
    // full occupancy with the same live set, is left alone. That is what rules out oscillation.
    for live in [0usize, 10, 60, 200, 500, 2_000] {
        let need_after = live + policy.need_reserve_words;
        let target = policy.next_words(2 * need_after, 0);
        assert_eq!(
            policy.young_after_fullsweep(target, target, live),
            target,
            "a nursery step 2 shrank to {target} for live {live} must be a fixed point"
        );
        // The reason it is: every curated step is under 4x, so the very next gate is false.
        assert!(
            target < 4 * need_after,
            "next_words(2 * {need_after}) = {target} must stay under the 4x gate"
        );
    }

    // (c) The collector: every fullsweep, over a spread of demands, live sets and starting
    // geometries, leaves a nursery no larger than the chain it collected.
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
            let stats = c.collect_major(&mut roots).expect("fullsweep");
            let after = c.young().capacity_words();
            assert!(
                after <= before,
                "need {need} round {round}: a fullsweep grew the nursery ({before} -> {after}) \
                 (copied {})",
                stats.words_copied
            );
            assert_eq!(c.young_space_count(), 1, "the chain collapsed");
        }
    }
}

// ---------------------------------------------------------------------------
// 4. A fixed demand converges to one constant capacity
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
        // ... and that constant is a small multiple of the demand, not a function of the
        // iteration count (a creep would show up as a rising tail above).
        assert!(
            tail[0] <= 8 * OP_WORDS,
            "{kind:?}: the converged capacity {} must stay a small multiple of {OP_WORDS}",
            tail[0]
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Both mature strategies answer identically for the same geometry
// ---------------------------------------------------------------------------

/// The two-step rule is a *policy* rule, so the copying and the mark-region collector must agree
/// on it for the same nursery geometry: same occupancy, same live set, same resulting nursery
/// capacity. (Their mature generations are sized and reclaimed differently — that is the whole
/// difference between the strategies — but the nursery half is shared.)
#[test]
fn both_mature_strategies_size_the_nursery_identically() {
    // The over-provisioned geometry of test 1: 2819 words collected, 60 words live.
    let mut results = Vec::new();
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut c = any_for(kind, HeapPolicy::default());
        c.grow_nursery(2_500).expect("the burst's growth");
        assert_eq!(c.heap_size_words(), 233 + HeapPolicy::default().next_words(2_500, 0));
        let elems: Vec<Eterm> = (0..59).map(|i| make_small(i as i64)).collect();
        let mut roots = vec![tuple_in(c.young_mut(), &elems)];
        fill_with_garbage_any(&mut c, 4);
        let stats = c.collect_major(&mut roots).expect("the fullsweep fits");
        assert_eq!(stats.words_copied, 60, "{kind:?}: exactly the live set moved");
        results.push((kind, c.young().capacity_words()));
    }
    assert_eq!(
        results[0].1, results[1].1,
        "both strategies shrink the over-provisioned nursery to the same size: {results:?}"
    );
    assert_eq!(results[0].1, 376, "and that size is step 2's derived target");
}
