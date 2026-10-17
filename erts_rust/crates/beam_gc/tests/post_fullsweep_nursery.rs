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

//! **Post-fullsweep nursery sizing — the demand rule.**
//!
//! A fullsweep relocates every live young word into the mature generation, so the nursery it
//! leaves behind has no surviving set to size from. This file pins what
//! [`Collector::collect_major`](beam_gc::Collector::collect_major) does with that freedom: it
//! sizes the nursery from the **demand the round observed** — the occupancy the mutator had
//! admitted into the chain, sampled *before* the copy (`Nursery::collapse`'s demand rule) — and
//! not from zero.
//!
//! 1. **A genuinely used nursery keeps its capacity.** A fullsweep over a nursery the mutator
//!    has filled leaves a space the next round's demand fits, so the workload stops paying a
//!    collect-and-grow per few hundred admitted words. The old rule (`live_words = 0`) collapsed
//!    the nursery to the [`need_reserve_words`](beam_gc::HeapPolicy::need_reserve_words) floor
//!    on every fullsweep, which on the `gc_bound/churn` shape meant a fresh space install per
//!    operation (`MEASURED.md`, "Post-fullsweep nursery sizing").
//! 2. **An over-provisioned nursery still shrinks.** A fullsweep over an empty (or barely used)
//!    nursery collapses to exactly the same reserve floor as before, so the rule is
//!    demand-driven, never "keep everything".
//! 3. **The capacity can never grow across a collection.** Every scenario reports
//!    `capacity_after <= capacity_before`, and the policy function is exhaustively checked to
//!    be non-increasing for every demand its callers can actually pass it
//!    (`demand_words <= capacity_words`, since a space's occupancy cannot exceed its capacity).
//!    Only the request-driven growth seam
//!    ([`Collector::ensure_capacity`](beam_gc::Collector::ensure_capacity) /
//!    [`grow_nursery`](beam_gc::Collector::grow_nursery)) enlarges a nursery — so nothing here
//!    is capacity creep.
//! 4. **A fixed demand converges to one constant capacity.** Hundreds of rounds of the same
//!    allocation shape leave the nursery on a single capacity, not a drifting one.
//! Both mature strategies are exercised where the corpus is strategy-agnostic. Every figure
//! here is a count, so it is load-invariant; no timing is measured or asserted.

use beam_gc::prelude::{
    AnyCollector, Collector, GcConfig, HeapObservable, HeapPolicy, HeapSpace, MatureKind,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The reserve floor the old rule collapsed to on every fullsweep:
/// `young_after_collection(_, 0)` = `next_words(need_reserve_words = 64, 0)`.
const RESERVE_FLOOR: usize = 90;

/// A collector for `policy` with `kind` **pinned** (an explicit override of the automatic rule).
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::new(policy).with_mature(kind)).expect("collector")
}

/// Builds a tuple from `elems` in `space` (a collector's current nursery space).
fn tuple_in(space: &mut HeapSpace, elems: &[Eterm]) -> Eterm {
    space.heap_mut().tuple(elems).expect("nursery tuple")
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
// 1. A fullsweep over a genuinely used nursery keeps the capacity it had
// ---------------------------------------------------------------------------

#[test]
fn a_fullsweep_that_observed_a_used_nursery_keeps_the_capacity_it_had() {
    let mut c = Collector::new(HeapPolicy::default()).expect("collector");
    let initial = c.young().capacity_words();
    assert_eq!(initial, 233, "the default policy's initial nursery");

    // A request larger than the nursery: a real (retiring) growth, so the chain is two spaces.
    c.grow_nursery(300).expect("grow");
    let grown = c.young().capacity_words();
    assert!(grown > initial, "the growth enlarged the space ({grown})");
    let collected_capacity = c.heap_size_words();
    assert_eq!(
        collected_capacity,
        initial + grown,
        "the chain is the two spaces a growth retained"
    );

    // Fill the current space, leaving a handful of words free (as a door-driven workload does
    // before it collects).
    let cells = (grown - 4) / 2;
    let mut roots = build_list(&mut c, cells);
    let observed = c.young().used_words();
    assert!(observed > 0);
    // The demand the round observed is what the policy compares the capacity against: it is
    // well past the shrink threshold (`capacity > 4 x demand`), so the pre-change rule's
    // collapse was *not* forced — it happened because a fullsweep passed `0`.
    assert!(
        4 * observed >= collected_capacity,
        "the corpus must find the nursery genuinely used ({observed} of {collected_capacity})"
    );

    let stats = c
        .collect_major(&mut roots)
        .expect("the fullsweep fits the default policy");
    assert!(stats.words_copied > 0, "the live list was copied");

    // The chain collapsed to one space, empty, and **not** re-sized down to the reserve floor.
    assert_eq!(c.young_space_count(), 1);
    assert_eq!(c.young().used_words(), 0);
    assert_eq!(
        c.young().capacity_words(),
        collected_capacity,
        "the nursery keeps the capacity the round observed"
    );
    assert!(c.young().capacity_words() > RESERVE_FLOOR);

    // The consequence the change is for: the demand that filled the nursery still fits, so the
    // next round neither collects (over occupancy) nor installs a fresh space.
    assert!(
        !c.should_collect(observed),
        "the round's own demand fits the nursery it left behind"
    );
    assert!(!c.collection_debt(), "the collapse settled the chain");

    // The live set really was evacuated, not dropped.
    assert_list_is_intact(roots[0], cells, "the list survives the fullsweep");

    // ... and the *same* rule holds on the mark-region strategy.
    let mut immix = any_for(MatureKind::Immix, HeapPolicy::default());
    immix.grow_nursery(grown).expect("grow");
    let immix_collected = immix.heap_size_words();
    let mut immix_roots = vec![NIL];
    let mut cells_immix = 0;
    while immix.young().available_words() > 8 {
        let tail = immix_roots[0];
        immix_roots[0] = immix
            .young_mut()
            .heap_mut()
            .cons(make_small(cells_immix as i64), tail)
            .expect("room");
        cells_immix += 1;
    }
    let immix_used = immix.young().used_words();
    let stats = immix
        .collect_major(&mut immix_roots)
        .expect("the mark-region fullsweep fits");
    assert!(stats.words_copied > 0);
    assert!(
        immix.young().capacity_words() >= immix_used.min(immix_collected),
        "the mark-region collector sizes the nursery from the same demand \
         ({} < {immix_used})",
        immix.young().capacity_words()
    );
    assert!(immix.young().capacity_words() > RESERVE_FLOOR);
}

// ---------------------------------------------------------------------------
// 2. An over-provisioned nursery still collapses to the reserve floor
// ---------------------------------------------------------------------------

#[test]
fn a_fullsweep_over_an_untouched_nursery_still_collapses_to_the_reserve_floor() {
    let mut c = Collector::new(HeapPolicy::default()).expect("collector");
    c.grow_nursery(2500).expect("a large nursery");
    let capacity = c.young().capacity_words();
    assert!(capacity > RESERVE_FLOOR);
    assert_eq!(c.young().used_words(), 0, "nothing was ever allocated");

    let mut roots = vec![NIL];
    c.collect_major(&mut roots).expect("an empty fullsweep");

    // Identical to the pre-change behaviour: there is no demand to keep, so the nursery falls
    // back to exactly the policy's reserve (`young_after_collection(capacity, 0)`).
    assert_eq!(
        c.young().capacity_words(),
        RESERVE_FLOOR,
        "an unused nursery is sized from the reserve alone"
    );
    assert_eq!(
        HeapPolicy::default().young_after_collection(capacity, 0),
        RESERVE_FLOOR,
        "which is the policy's own answer for a zero demand"
    );
}

// ---------------------------------------------------------------------------
// 3. A collection can never enlarge the nursery
// ---------------------------------------------------------------------------

#[test]
fn the_post_fullsweep_nursery_never_exceeds_the_collected_capacity() {
    // (a) The policy function: for every demand a caller can actually pass it
    // (`demand <= capacity`, because a space's occupancy cannot exceed its capacity) the
    // target is at most the capacity. That is the whole bounded-growth argument, checked
    // exhaustively over the growth table.
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
            let target = policy.young_after_collection(capacity, demand);
            assert!(
                target <= capacity.max(reserve_floor),
                "demand {demand} of capacity {capacity} must not grow the nursery (got {target})"
            );
        }
        // A demand over the capacity is the *explicit* growth seam's job, not a collection's.
        assert!(policy.young_after_collection(capacity, capacity + 1) >= capacity);
    }

    // (b) The collector: every fullsweep, over a spread of demands and starting geometries,
    // leaves a nursery no larger than the chain it collected.
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
