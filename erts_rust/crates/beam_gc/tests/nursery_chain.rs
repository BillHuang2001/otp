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

//! **Nursery-chain** evidence for `beam_gc` — the no-relocation growth seam.
//!
//! A [`HeapSpace`](beam_gc::HeapSpace) never reallocates, so growing the nursery cannot
//! move live terms: [`Collector::grow_nursery`](beam_gc::Collector::grow_nursery) instead
//! **retains** the space it replaces and points the allocator at a fresh, larger one. The
//! nursery becomes a **chain** of spaces, and this file pins the consequences a runtime
//! relies on when it holds no root set (so cannot collect) but must still enlarge the heap
//! so a large allocation can proceed:
//!
//! 1. **Growth never relocates and never collects.** A live term keeps its exact address, and
//!    the accumulated statistics and the minor-collection counter are untouched — every live
//!    word is still addressable at its original address, and the collection the growth
//!    avoided simply has not run.
//! 2. **A graph spanning the growth boundary survives a collection.** Terms built in the
//!    retired space and the fresh space, with references **both** ways across the boundary,
//!    are evacuated whole and read back structurally intact on **both** mature strategies
//!    (the copying collector and the mark-region promotion).
//! 3. **A growth-retained space is still part of the nursery.** The range test
//!    ([`young_contains`](beam_gc::Collector::young_contains)) covers the retired and the
//!    current space (and neither a foreign nor an out-of-space address), the summed nursery
//!    capacity
//!    ([`heap_size_words`](beam_gc::HeapObservable::heap_size_words)) reports the whole chain
//!    while it is live, and a collection collapses it back to a single space.
//! 4. **A capped heap leaves the collector untouched.** A request a
//!    [`max_heap_words`](beam_gc::HeapPolicy::max_heap_words) ceiling cannot reach is the
//!    typed heap-exhaustion error, and the collector is exactly as it was (no space retired,
//!    no words handed out, no collection run); a request under the same cap still succeeds.
//! 5. **A grow-then-collect loop reaches a bounded steady state.** A long deterministic loop
//!    that grows and collects every round plateaus its nursery and mature capacities — the
//!    growth path is exercised, nothing creeps without bound.
//!
//! Every corpus here is deterministic (no randomness), and every cross-collection comparison
//! is structural ([`beam_term::compare::eq`] against an independently built witness). Timings
//! are never asserted.

use beam_common::BeamError;
use beam_gc::prelude::{
    AnyCollector, Collector, GcConfig, HeapObservable, HeapPolicy, HeapSpace, MatureKind,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{boxed_val, make_small, ptr_val, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A collector for `policy` with `kind` **pinned** (an explicit override of the automatic
/// rule), so a test drives the same corpus through both mature strategies.
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::new(policy).with_mature(kind)).expect("collector")
}

/// Builds a tuple from `elems` in `space` (a collector's current nursery space).
fn tuple_in(space: &mut HeapSpace, elems: &[Eterm]) -> Eterm {
    space.heap_mut().tuple(elems).expect("nursery tuple")
}

/// Structurally compares the live `term` against an **independently constructed** witness: the
/// `build` closure writes the expected term into a fresh [`beam_term::heap::Heap`] (kept alive
/// for the comparison) and a fresh [`AtomTable`] resolves atoms. This is the single witness
/// helper every test uses, so no term shape is re-derived per test.
fn assert_witness(
    term: Eterm,
    build: impl FnOnce(&mut beam_term::heap::Heap) -> Eterm,
    what: &str,
) {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(4096);
    let expected = build(&mut heap);
    assert!(term_eq(term, expected, &atoms), "{what}");
}

// ---------------------------------------------------------------------------
// 1. Growth preserves live terms at their original address, without collecting
// ---------------------------------------------------------------------------

#[test]
fn growth_preserves_live_terms_at_their_original_address_without_collecting() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let policy = HeapPolicy {
            initial_young_words: 38,
            ..HeapPolicy::default()
        };
        let mut c = any_for(kind, policy);
        assert_eq!(c.kind(), kind, "the strategy was selected");

        // A small graph in the nursery: `{{7,8},9}`.
        let inner = tuple_in(c.young_mut(), &[make_small(7), make_small(8)]);
        let root = tuple_in(c.young_mut(), &[inner, make_small(9)]);

        let addr = ptr_val(root) as usize;
        let used_before = c.young().used_words();
        let stats_before = c.stats();
        let minor_before = c.minor_gcs();
        let cap_before = c.young().capacity_words();
        assert!(used_before > 0, "{kind:?}: the graph is live");
        assert_eq!(c.young_space_count(), 1, "{kind:?}: a single space to begin with");

        let need = 4096usize;
        assert!(
            c.young().available_words() < need,
            "{kind:?}: the request outgrows the nursery"
        );
        c.grow_nursery(need).expect("grow");

        // Nothing moved: the live word keeps its exact address.
        assert_eq!(
            ptr_val(root) as usize,
            addr,
            "{kind:?}: growth must not relocate a live term"
        );
        // Nothing collected: the accumulated statistics and the minor counter are untouched.
        assert_eq!(
            c.stats(),
            stats_before,
            "{kind:?}: grow_nursery ran no collection"
        );
        assert_eq!(c.minor_gcs(), minor_before, "{kind:?}: no minor ran");
        // The chain grew, and the old space is still part of the nursery.
        assert!(
            c.young_space_count() >= 2,
            "{kind:?}: a growth retains the old space ({})",
            c.young_space_count()
        );
        assert!(
            c.young_contains(ptr_val(root) as *const Eterm),
            "{kind:?}: young_contains covers the retired space"
        );
        assert!(
            !c.young().contains_term(root),
            "{kind:?}: the term is not in the fresh current space"
        );
        assert!(
            c.young().capacity_words() > cap_before,
            "{kind:?}: the current space is larger than the one it replaced"
        );
        // The current space can now satisfy the request.
        assert!(
            c.young().available_words() >= need,
            "{kind:?}: the grown current space satisfies the request"
        );

        // The term is still readable and structurally intact.
        assert_witness(
            root,
            |h| {
                let inner = h.tuple(&[make_small(7), make_small(8)]).expect("inner");
                h.tuple(&[inner, make_small(9)]).expect("root")
            },
            &format!("{kind:?}: the retired term must read back intact"),
        );
    }
}

// ---------------------------------------------------------------------------
// 2. A graph spanning the growth boundary survives a collection
// ---------------------------------------------------------------------------

#[test]
fn a_graph_spanning_the_growth_boundary_survives_a_collection_on_both_strategies() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let policy = HeapPolicy {
            initial_young_words: 90,
            initial_mature_words: 512,
            ..HeapPolicy::default()
        };
        let mut c = any_for(kind, policy);

        // Space A (the current space, soon retired): `a1 = {nil}`, `a2 = {5}`.
        let a1 = tuple_in(c.young_mut(), &[NIL]);
        let a2 = tuple_in(c.young_mut(), &[make_small(5)]);

        // Grow: A is retired, B is the fresh current space.
        c.grow_nursery(4096).expect("grow");
        assert!(c.young_space_count() >= 2, "{kind:?}: the chain is live");

        // Space B: `b1 = {a2, 2}` is a B->A edge; `b2 = {6}`.
        let b1 = tuple_in(c.young_mut(), &[a2, make_small(2)]);
        let b2 = tuple_in(c.young_mut(), &[make_small(6)]);

        // Close the A->B edge by writing `b2` into `a1`'s element 0.
        // SAFETY: `a1` is a live 1-element tuple this test allocated in the retained nursery
        // space A, and a tuple's element 0 lives at `boxed_val(a1).add(1)`; the write stays
        // inside the tuple's two words.
        unsafe {
            *boxed_val(a1).add(1) = b2;
        }

        let mut roots = vec![a1, b1];
        c.collect(&mut roots).expect("collect");

        // Both roots are mature-resident (the A->B root too, via the B-space term) and the
        // chain has collapsed.
        assert!(
            c.mature_contains(ptr_val(roots[0]) as *const Eterm),
            "{kind:?}: the A-space root was promoted"
        );
        assert!(
            c.mature_contains(ptr_val(roots[1]) as *const Eterm),
            "{kind:?}: the B-space root was promoted"
        );
        assert_eq!(
            c.young_space_count(),
            1,
            "{kind:?}: the collection collapsed the chain"
        );

        // The A->B reference (a1 -> b2 == {6}) and the B->A reference (b1 -> a2 == {5}) both
        // survived the move into mature.
        assert_witness(
            roots[0],
            |h| {
                let inner = h.tuple(&[make_small(6)]).expect("inner");
                h.tuple(&[inner]).expect("outer")
            },
            &format!("{kind:?}: the A->B reference must survive"),
        );
        assert_witness(
            roots[1],
            |h| {
                let inner = h.tuple(&[make_small(5)]).expect("inner");
                h.tuple(&[inner, make_small(2)]).expect("outer")
            },
            &format!("{kind:?}: the B->A reference must survive"),
        );
    }
}

// ---------------------------------------------------------------------------
// 3. The chain collapses to one space; young_contains covers every space
// ---------------------------------------------------------------------------

#[test]
fn the_chain_collapses_to_one_space_and_young_contains_covers_every_space() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let policy = HeapPolicy {
            initial_young_words: 90,
            initial_mature_words: 512,
            ..HeapPolicy::default()
        };
        let mut c = any_for(kind, policy);

        // A term in the space a growth will retire, and the single space's capacity.
        let retired_term = tuple_in(c.young_mut(), &[make_small(1), make_small(2)]);
        let cap_single = c.young().capacity_words();

        c.grow_nursery(4096).expect("grow");

        // A term in the fresh current space.
        let current_term = tuple_in(c.young_mut(), &[make_small(3), make_small(4)]);

        // The range test covers BOTH spaces of the chain.
        assert!(c.young_space_count() >= 2, "{kind:?}");
        assert!(
            c.young_contains(ptr_val(retired_term) as *const Eterm),
            "{kind:?}: a term in the retired space is in the nursery"
        );
        assert!(
            c.young_contains(ptr_val(current_term) as *const Eterm),
            "{kind:?}: a term in the current space is in the nursery"
        );

        // ... and neither a foreign (stack) address nor an out-of-space address.
        let local = make_small(0);
        let foreign = core::ptr::addr_of!(local) as *const Eterm;
        assert!(!c.young_contains(foreign), "{kind:?}: a foreign address is not");
        let just_outside = c.young().limit() as *const Eterm;
        assert!(
            !c.young_contains(just_outside),
            "{kind:?}: a pointer at the end of the current space is outside"
        );

        // The summed nursery capacity is the whole chain: the retained space plus the current
        // one — strictly more than the current space alone while the chain is live.
        let current_cap = c.young().capacity_words();
        assert_eq!(
            c.heap_size_words(),
            cap_single + current_cap,
            "{kind:?}: heap_size_words is the summed nursery"
        );
        assert!(
            c.heap_size_words() > current_cap,
            "{kind:?}: the chain is larger than its current space"
        );

        // A collection collapses the chain back to exactly one space, and the summed nursery
        // is then exactly that one space's capacity.
        let mut roots = vec![retired_term, current_term];
        c.collect(&mut roots).expect("collect");
        assert_eq!(
            c.young_space_count(),
            1,
            "{kind:?}: the collection collapsed the chain"
        );
        assert_eq!(
            c.heap_size_words(),
            c.young().capacity_words(),
            "{kind:?}: one space, one capacity"
        );

        // `should_collect(need)` is exactly "the current space cannot satisfy `need`".
        let avail = c.young().available_words();
        assert!(c.should_collect(avail + 1), "{kind:?}: one more word does not fit");
        assert!(!c.should_collect(avail), "{kind:?}: an exact fit still fits");
        assert!(!c.should_collect(0), "{kind:?}: zero words always fit");
    }
}

// ---------------------------------------------------------------------------
// 4. A capped heap leaves the collector untouched
// ---------------------------------------------------------------------------

#[test]
fn grow_nursery_leaves_the_collector_untouched_when_the_ceiling_cannot_reach() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let policy = HeapPolicy {
            initial_young_words: 51,
            max_heap_words: 100,
            ..HeapPolicy::default()
        };
        let mut c = any_for(kind, policy);

        let root = tuple_in(c.young_mut(), &[make_small(1), make_small(2)]);

        let spaces_before = c.young_space_count();
        let used_before = c.young().used_words();
        let stats_before = c.stats();

        // 4096 words can never be reached with a 100-word ceiling: a typed error, no panic.
        let err = c.grow_nursery(4096).expect_err("growth is capped");
        assert!(matches!(err, BeamError::Alloc(_)), "{kind:?}: got {err:?}");
        assert_eq!(err.as_atom(), Some("out_of_memory"), "{kind:?}");

        // The collector is exactly as it was: no space retired, no words handed out, no
        // collection run.
        assert_eq!(
            c.young_space_count(),
            spaces_before,
            "{kind:?}: nothing was retired"
        );
        assert_eq!(c.young().used_words(), used_before, "{kind:?}: nothing allocated");
        assert_eq!(c.stats(), stats_before, "{kind:?}: nothing collected");

        // The live term is untouched.
        assert_witness(
            root,
            |h| h.tuple(&[make_small(1), make_small(2)]).expect("root"),
            &format!("{kind:?}: the live term is intact after a refused growth"),
        );

        // A small request under the same cap still succeeds.
        c.grow_nursery(64).expect("fits under the cap");
        assert!(
            c.young().available_words() >= 64,
            "{kind:?}: the small request succeeded"
        );
        assert_eq!(c.stats(), stats_before, "{kind:?}: still no collection");
        assert_witness(
            root,
            |h| h.tuple(&[make_small(1), make_small(2)]).expect("root"),
            &format!("{kind:?}: the live term is intact after a successful growth"),
        );
    }
}

// ---------------------------------------------------------------------------
// 5. A grow-then-collect loop reaches a bounded steady state
// ---------------------------------------------------------------------------

#[test]
fn a_grow_then_collect_loop_reaches_a_bounded_steady_state() {
    let policy = HeapPolicy {
        initial_young_words: 51,
        initial_mature_words: 51,
        ..HeapPolicy::default()
    };
    let mut c = Collector::new(policy).expect("collector");

    const ROUNDS: usize = 200;
    // The request rises over the first few rounds to exercise the growth path (each round
    // outgrows the previous nursery), then settles on a constant far larger than the start.
    // The rise stays *below* the steady request so the mature generation's size is set by the
    // steady request alone and the loop converges to a fixed period.
    const STEADY_NEED: usize = 700;
    let need_for = |round: usize| -> usize {
        if round < 10 {
            64 + round * 70
        } else {
            STEADY_NEED
        }
    };

    let mut recorded: Vec<(usize, usize, usize)> = Vec::with_capacity(ROUNDS);
    let mut grew = false;

    for round in 0..ROUNDS {
        let need = need_for(round);

        if c.young().available_words() < need {
            c.grow_nursery(need).expect("grow");
            grew = true;
        }

        // A `need`-word tuple: `need - 1` element words plus the header.
        let elems: Vec<Eterm> = (0..need - 1).map(|i| make_small((i % 1000) as i64)).collect();
        let tuple = tuple_in(c.young_mut(), &elems);

        let mut roots = vec![tuple];
        c.collect(&mut roots).expect("collect");

        // The live root survived the round, structurally.
        assert_witness(
            roots[0],
            |h| h.tuple(&elems).expect("witness"),
            &format!("round {round}: the live root must be intact"),
        );

        recorded.push((
            c.young_space_count(),
            c.young().capacity_words(),
            c.mature().capacity_words(),
        ));
    }

    // The growth path really was exercised.
    assert!(grew, "the loop must have grown the nursery at least once");

    // The capacities plateau: the last 50 triples form a bounded **period-2** cycle.
    //
    // After the growth path has run, a round's *collection* collapses the chain and re-sizes
    // the nursery from what it left alive in it: the round's `STEADY_NEED`-word object is
    // promoted out, so the surviving set is empty and `young_after_collection` falls the
    // nursery back to a small size; the *next* round's request then outgrows that and
    // `grow_nursery` steps it back up. The loop therefore alternates deterministically between
    // a grown and a collapsed nursery rather than sitting at a single constant.
    //
    // What matters is that the cycle is **bounded** — a fixed period over a fixed, finite set
    // of states, each capacity pinned — not that it is a single value. A creep would show as a
    // third (or drifting) state, which the exact-set assertion below rejects.
    const PERIOD: usize = 2;
    let tail = &recorded[ROUNDS - 50..];
    assert!(
        tail.iter().enumerate().all(|(i, t)| *t == tail[i % PERIOD]),
        "the last 50 triples must form a bounded period-{PERIOD} cycle, got {tail:?}"
    );
    let distinct: std::collections::BTreeSet<(usize, usize, usize)> =
        tail.iter().copied().collect();
    let pinned: std::collections::BTreeSet<(usize, usize, usize)> =
        [(1, 90, 1598), (1, 1077, 1598)].into_iter().collect();
    assert_eq!(
        distinct, pinned,
        "the converged cycle must be exactly the pinned grown/collapsed alternation"
    );

    // Nothing creeps without bound.
    for (round, (spaces, young_cap, mature_cap)) in recorded.iter().enumerate() {
        assert!(
            *spaces < 100_000 && *young_cap < 100_000 && *mature_cap < 100_000,
            "round {round}: an unbounded capacity grew ({spaces}, {young_cap}, {mature_cap})"
        );
    }
}