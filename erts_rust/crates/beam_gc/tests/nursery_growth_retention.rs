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

//! **Growth-retention** evidence for `beam_gc` — the *collection debt* a nursery growth
//! accumulates and the opt-in signal that lets a host claim it.
//!
//! [`Collector::grow_nursery`](beam_gc::Collector::grow_nursery) is the **no-relocation**
//! growth seam: it never collects and never moves a term, so a space it supersedes may still be
//! named by a live term (its words were handed to the mutator) and is kept addressable as a new
//! link of the nursery chain. Nothing inside `beam_gc` ever frees such a space outside a
//! collection's chain **collapse** — the conservative rule this file pins.
//!
//! Two facts follow, and this file pins both:
//!
//! 1. **A chain holding a retired space is collection debt.**
//!    [`collection_debt`](beam_gc::Collector::collection_debt) is the **opt-in** seam a host's
//!    heap-admission point consults: a growth can never reclaim anything, so only a collection
//!    (which collapses the chain) reclaims a retired space, and a `true` answer means "collect,
//!    exactly as for a full nursery". It is deliberately **not** folded into
//!    [`should_collect`](beam_gc::Collector::should_collect), which stays occupancy-only.
//! 2. **Driving the debt keeps the chain and its retention bounded.** A grow + "debt ⇒ collect"
//!    admission loop keeps the chain at **at most one** retired space (never one per cycle), so
//!    the retained capacity cannot scale with the iteration count and the mature generation
//!    converges.
//!
//! The **capacity** side of the same bound is the heap-size policy's live-set rule, and this file
//! pins that too. A collection settles the nursery on
//! [`HeapPolicy::young_target_words`] of the live set it measured — `next_words(2 × live + 64)` —
//! so a settled capacity is a function of the **live set** and never of the iteration count. A
//! collection only gives memory back once the capacity exceeds `YOUNG_SHRINK_FACTOR` (8) × that
//! target, so a settled capacity stays inside the hysteresis band `[.., 8 × target]` that
//! [`HeapPolicy::young_after_collection`] / [`HeapPolicy::young_after_fullsweep`] document, and a
//! [`grow_nursery`](beam_gc::Collector::grow_nursery) step installs at most one growth-table step
//! **or** one doubling above the space it replaces. The peak therefore has to be expressible as a
//! function of the live set (`8 × target`, plus one doubling) — never as a multiple of `ROUNDS`.
//!
//! Every corpus here is deterministic (no randomness) and is driven through **both** mature
//! strategies (the copying collector and the mark-region promotion). Timings are never
//! asserted.

use beam_gc::prelude::{
    AnyCollector, GcConfig, HeapObservable, HeapPolicy, HeapSpace, MatureKind, YOUNG_SHRINK_FACTOR,
};
use beam_term::prelude::{make_small, ptr_val, Eterm};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A collector for `policy` with `kind` **pinned** (an explicit override of the automatic
/// rule), so a test drives the same corpus through both mature strategies.
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::new(policy).with_mature(kind)).expect("collector")
}

/// Both mature strategies, so every pin below is strategy-independent.
const KINDS: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// Builds a tuple from `elems` in `space` (a collector's current nursery space).
fn tuple_in(space: &mut HeapSpace, elems: &[Eterm]) -> Eterm {
    space.heap_mut().tuple(elems).expect("nursery tuple")
}

/// The **retained** (retired) nursery capacity: the chain's summed capacity minus the space the
/// mutator allocates in.
fn retained_words(c: &AnyCollector) -> usize {
    c.heap_size_words() - c.young().capacity_words()
}

/// The upper edge of the heap-size policy's **hysteresis band** for a live set of `live_words`: the
/// largest nursery capacity a collection may settle on without giving the memory back
/// (`YOUNG_SHRINK_FACTOR` × [`HeapPolicy::young_target_words`]).
///
/// It is the quantity the nursery's capacity is *bounded by*, and every term of it is a workload
/// property — the live set — so a capacity checked against it cannot scale with the run length.
fn band_edge(policy: &HeapPolicy, live_words: usize) -> usize {
    YOUNG_SHRINK_FACTOR * policy.young_target_words(live_words)
}

// ---------------------------------------------------------------------------
// 1. A superseded space is retained while it holds words (the deliberate conservatism)
// ---------------------------------------------------------------------------

#[test]
fn a_superseded_space_is_retained_and_never_freed_by_a_growth() {
    for kind in KINDS {
        let mut c = any_for(kind, HeapPolicy::default());
        // The mutator has really built a term in the space about to be superseded.
        let _live = tuple_in(c.young_mut(), &[make_small(1), make_small(2)]);
        assert!(
            c.young().used_words() > 0,
            "{kind:?}: the mutator has words in the space"
        );
        let before = c.young().capacity_words();
        let addr = c.young().base();

        c.grow_nursery(4096).expect("grow");

        // A growth never frees a superseded space (it may hold live words, and only a
        // collection proves them dead): the chain holds it and the allocation is still there.
        assert_eq!(
            c.young_space_count(),
            2,
            "{kind:?}: the superseded space is retained, not freed"
        );
        assert!(
            retained_words(&c) > 0,
            "{kind:?}: the chain carries the retained capacity"
        );
        // Exactly the superseded space is retained — nothing more.
        assert_eq!(
            retained_words(&c),
            before,
            "{kind:?}: the retained capacity is the space that was superseded"
        );
        // The growth **replaced** the space; it did not move it. The old region is still part of
        // the nursery's range test (`young_contains`), which is what keeps a live word in it
        // addressable.
        assert!(
            c.young_contains(addr as *const Eterm),
            "{kind:?}: the retained space is still part of the nursery's range"
        );
        assert!(
            c.young().capacity_words() > before,
            "{kind:?}: the request did enlarge the heap"
        );
        // And the chain is now real collection debt.
        assert!(
            c.collection_debt(),
            "{kind:?}: a retained space is collection debt"
        );

        // Even a superseded space that **never handed out a word** is retained: dropping it
        // would be sound but changes `young_space_count()` and is deliberately not done.
        let mut fresh = any_for(kind, HeapPolicy::default());
        assert_eq!(
            fresh.young().used_words(),
            0,
            "{kind:?}: nothing handed out"
        );
        fresh.grow_nursery(4096).expect("grow");
        assert_eq!(
            fresh.young_space_count(),
            2,
            "{kind:?}: the empty superseded space is retained too"
        );
        assert!(
            fresh.collection_debt(),
            "{kind:?}: an empty superseded space is collection debt as well"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The debt is true while a space is retained and false after a collection
// ---------------------------------------------------------------------------

#[test]
fn the_debt_is_true_while_a_space_is_retained_and_false_after_a_collection() {
    for kind in KINDS {
        let mut c = any_for(kind, HeapPolicy::default());
        assert!(!c.collection_debt(), "{kind:?}: a single space is no debt");
        let live = tuple_in(c.young_mut(), &[make_small(1), make_small(2)]);

        c.grow_nursery(4096).expect("grow");
        assert!(c.collection_debt(), "{kind:?}: the retained space is debt");
        assert!(
            retained_words(&c) > 0,
            "{kind:?}: the chain carries the retained capacity"
        );

        let mut roots = vec![live];
        c.collect(&mut roots)
            .expect("the next collection collapses the chain");
        assert_eq!(
            c.young_space_count(),
            1,
            "{kind:?}: the collection released the chain"
        );
        assert!(
            !c.collection_debt(),
            "{kind:?}: the debt is settled by the collection"
        );
        assert_eq!(
            retained_words(&c),
            0,
            "{kind:?}: the retained capacity is gone after the collection"
        );
        assert_eq!(
            c.heap_size_words(),
            c.young().capacity_words(),
            "{kind:?}: one space, one capacity"
        );
        // The live term was evacuated into the mature generation, not dropped.
        assert!(
            c.mature_contains(ptr_val(roots[0]) as *const Eterm),
            "{kind:?}: the retired space's live term was promoted"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. `should_collect` stays occupancy-only; the debt is a separate, opt-in query
// ---------------------------------------------------------------------------

#[test]
fn should_collect_reports_occupancy_only_and_ignores_the_debt() {
    for kind in KINDS {
        let mut c = any_for(kind, HeapPolicy::default());
        let _live = tuple_in(c.young_mut(), &[make_small(1), make_small(2)]);
        c.grow_nursery(4096).expect("grow");
        assert!(c.collection_debt(), "{kind:?}: the chain carries debt");

        // The fresh space has room for everything up to its own capacity; the retained space
        // must **not** leak into the occupancy signal, so an exact fit still fits and only one
        // word over the free room fires it.
        let available = c.young().available_words();
        assert!(
            !c.should_collect(0),
            "{kind:?}: a zero-word request fits the current space"
        );
        assert!(
            !c.should_collect(available),
            "{kind:?}: an exact fit still fits — occupancy only"
        );
        assert!(
            c.should_collect(available + 1),
            "{kind:?}: one word over the free room does not fit"
        );
        assert!(
            c.collection_debt(),
            "{kind:?}: the debt is reported independently of the occupancy signal"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. N grow + "debt ⇒ collect" admission cycles keep the chain and its retention bounded
// ---------------------------------------------------------------------------

/// The host's heap-admission hint — a small `test_heap` prefix that always fits the current
/// space, so the door's **occupancy** half of the signal never fires on its own.
const ADMISSION_HINT: usize = 16;

/// The op's own result — a tuple the op builds **after** its admission point, so it arrives
/// through the un-rooted result-allocation seam, which may only **grow** the nursery (it holds no
/// root set, so it cannot collect). Each round replaces it as the published root, so what the
/// previous round built is garbage by the time the next admission point runs.
///
/// Deliberately chosen so the object's mark-region footprint is **exactly one whole block**: 256
/// words is `BLOCK_WORDS` — 16 whole `LINE_WORDS` — so a copy destination of this size is always
/// found in a *completely free* block, and the space's total free-line count can never
/// over-report what its per-object run search will hand out. (A request whose lines leave a
/// per-block hole — e.g. 7 lines in a 16-line block — can make `free_lines()` claim room no
/// single run can use; that independent pre-existing gap of the mark-region space is documented
/// in `CONTEXT.md` and is *not* what this file pins.)
const OP_WORDS: usize = 256;
/// The number of `grow + admit` cycles the loop runs. Sized so a *linear* retention would be
/// unmistakable (hundreds of whole spaces) while a bounded one is flat.
const ROUNDS: usize = 400;

/// The last `TAIL` states the harness reports — the window the plateau is pinned over.
const TAIL: usize = 20;

/// What one loop round left the collector in: `(chain length, current young capacity, retained
/// (retired) words, mature capacity, live words)`.
///
/// `live words` is the collector's whole-heap live-set figure — the mature generation's occupancy,
/// which a minor promotes every live young word into, and therefore the quantity the nursery's
/// target is derived from.
type State = (usize, usize, usize, usize, usize);

/// What one full run of the loop observed: the peak chain length, the peak *current* nursery
/// capacity, the peak retained capacity, the largest live set the run measured, and the last
/// [`TAIL`] states.
struct Observed {
    peak_spaces: usize,
    peak_young: usize,
    peak_retained: usize,
    max_live: usize,
    tail: Vec<State>,
}

/// Drives the two seams a runtime alternates in a loop — the machine's heap-admission point
/// (the occupancy signal **and** the opt-in debt query, then `collect`) and an op's own
/// un-rooted result allocation (which may only **grow**) — and reports what the collector did.
///
/// The admission hint is deliberately smaller than the nursery, so the **only** thing that can
/// make the admission point collect is the reported collection debt.
///
/// Every round also checks the rule's two **capacity** invariants, so the bound the caller asserts
/// afterwards is a summary of what the run actually did:
///
/// * a collection settles on a capacity inside the live set's band
///   ([`band_edge`] — `young_after_collection` installs the target or keeps a capacity the band
///   already covers, and `young_after_fullsweep` does the same against the exact live set), and
/// * a growth installs at most **one growth-table step or one doubling** above the space it
///   replaces (`Nursery::grow`'s third step), never a compounding of the two.
fn drive_loop(c: &mut AnyCollector, elems: &[Eterm], kind: MatureKind) -> Observed {
    let mut roots = vec![make_small(0)];
    let mut peak_spaces = 0usize;
    let mut peak_young = 0usize;
    let mut peak_retained = 0usize;
    let mut max_live = 0usize;
    let mut tail: Vec<State> = Vec::with_capacity(TAIL);

    for round in 0..ROUNDS {
        // The host's heap-admission point: the engine's `test_heap` prefix asks whether the
        // young heap needs a collection (occupancy) **or** whether the chain carries debt, and
        // collects over its published roots when either holds.
        if c.should_collect(ADMISSION_HINT) || c.collection_debt() {
            let spaces = c.young_space_count();
            let young = c.young().capacity_words();
            let used = c.young().used_words();
            let retained = retained_words(c);
            let mature = c.mature_capacity_words();
            c.collect(&mut roots).unwrap_or_else(|e| {
                panic!(
                    "{kind:?} round {round}: collect failed {e:?} (spaces={spaces} young={young} \
                     used={used} retained={retained} mature={mature})"
                )
            });
            // A collection sizes the nursery from the live set it measured, and only gives the
            // memory back past the band's upper edge: settle inside the band or not at all.
            let live = c.mature_used_words();
            let settled = c.young().capacity_words();
            let band = band_edge(c.heap_policy(), live);
            assert!(
                settled <= band,
                "{kind:?} round {round}: a collection must settle inside the live set's band \
                 (8 × young_target_words({live}) = {band}), got {settled}"
            );
            max_live = max_live.max(live);
        }
        // The op's own result: the un-rooted allocation seam, which may only grow.
        if c.young().available_words() < OP_WORDS {
            let before = c.young().capacity_words();
            c.grow_nursery(OP_WORDS)
                .unwrap_or_else(|e| panic!("{kind:?} round {round}: grow failed {e:?}"));
            let after = c.young().capacity_words();
            let one_step = c.heap_policy().next_words(OP_WORDS, 0).max(before);
            let bound = c.heap_policy().next_words(2 * before, 0).max(one_step);
            assert!(
                after <= bound,
                "{kind:?} round {round}: a growth is at most one growth-table step or one \
                 doubling of the {before}-word space it replaces, got {after} > {bound}"
            );
        }
        roots[0] = tuple_in(c.young_mut(), elems);

        peak_spaces = peak_spaces.max(c.young_space_count());
        peak_young = peak_young.max(c.young().capacity_words());
        peak_retained = peak_retained.max(retained_words(c));
        if tail.len() == TAIL {
            tail.remove(0);
        }
        tail.push((
            c.young_space_count(),
            c.young().capacity_words(),
            retained_words(c),
            c.mature_capacity_words(),
            c.mature_used_words(),
        ));
    }
    Observed {
        peak_spaces,
        peak_young,
        peak_retained,
        max_live,
        tail,
    }
}

#[test]
fn a_grow_and_admit_loop_keeps_the_chain_and_its_retention_bounded_on_both_strategies() {
    // A mature generation with comfortable room: the corpus is about the **nursery** chain, and
    // the mark-region strategy charges whole lines per object (`LINE_WORDS` 16), so a cramped
    // region would test its line budget rather than the retention this file pins.
    let policy = HeapPolicy {
        initial_mature_words: 2048,
        ..HeapPolicy::default()
    };
    assert!(
        policy.next_words(OP_WORDS, 0) > OP_WORDS,
        "the grown space covers the op's result"
    );
    let elems: Vec<Eterm> = (0..OP_WORDS - 1)
        .map(|i| make_small((i % 1000) as i64))
        .collect();

    for kind in KINDS {
        let mut c = any_for(kind, policy.clone());
        let Observed {
            peak_spaces,
            peak_young,
            peak_retained,
            max_live,
            tail,
        } = drive_loop(&mut c, &elems, kind);
        // The leak this pins: without the debt query the admission point's `should_collect(16)`
        // reads only the current space, the grown space always has room, and every growth
        // lengthens the chain (and retains a whole space) forever.
        assert_eq!(
            peak_spaces, 2,
            "{kind:?}: the chain never holds more than one retired space over {ROUNDS} cycles"
        );
        assert!(
            peak_retained > 0,
            "{kind:?}: the corpus must really exercise the debt (a space was retained)"
        );
        // The retention is always one space at most: it can never exceed the largest single
        // nursery space the run ever installed, so it cannot scale with the iteration count.
        assert!(
            peak_retained <= peak_young,
            "{kind:?}: the retained capacity must be one nursery space at most \
             ({peak_retained} > {peak_young})"
        );

        // The debt is claimed: the admission point really does collect, so nothing is left
        // unclaimed.
        assert!(
            c.stats().major_collections + c.stats().minor_collections > 0,
            "{kind:?}: the admission point collected over the run"
        );

        // **The peak capacity is a function of the live set.** A collection settles the nursery on
        // `young_target_words(live)` (or keeps a capacity the band already covers), and a growth
        // adds at most one growth-table step or one doubling on top of that — so the run's
        // high-water mark is bounded by one doubling above the band's edge for the largest live set
        // the run measured:
        //
        //     peak_young <= next_words(2 × 8 × young_target_words(max_live))
        //
        // Every factor is a property of the workload (the live multiple 2, the band factor 8, the
        // live set itself), and `drive_loop` checks both per-round invariants as it goes, so this
        // is a summary of the rule — not a numeric guard. The live set here is *not* tiny: each
        // round's 256-word tuple is promoted by the next minor and stays live until the following
        // fullsweep, so `max_live` reaches 1792 words (Copying) / 2560 (Immix) and the target
        // 4185 / 6772 — which is why a capacity scaled to the op's own object cannot be expected
        // (and why an `n × OP_WORDS` guard was never the right shape for this bound).
        assert!(
            max_live > 0,
            "{kind:?}: the run must have measured a live set"
        );
        let band_max = band_edge(c.heap_policy(), max_live);
        let peak_bound = c.heap_policy().next_words(2 * band_max, 0);
        assert!(
            peak_young <= peak_bound,
            "{kind:?}: the peak nursery is bounded by the live set — one doubling above \
             8 × young_target_words({max_live}) = {band_max}, i.e. {peak_bound} — got {peak_young} \
             over {ROUNDS} cycles"
        );

        // A converged, bounded tail: the chain and both capacities sit on a small fixed set of
        // states instead of drifting, and nothing creeps with the iteration count.
        let distinct: std::collections::BTreeSet<State> = tail.iter().copied().collect();
        assert!(
            distinct.len() <= 12,
            "{kind:?}: the tail must sit on a few bounded states, got {distinct:?}"
        );
        // The peaks above are the run's global high-water marks, which need not be the plateau's.
        // What the converged cycle must show is that the chain holds at most one retired space,
        // that the capacity stays inside the live set's band, and that the retention never exceeds
        // a single nursery space.
        let tail_spaces = tail.iter().map(|s| s.0).max().expect("a tail");
        let tail_young = tail.iter().map(|s| s.1).max().expect("a tail");
        let tail_retained = tail.iter().map(|s| s.2).max().expect("a tail");
        let tail_live = tail.iter().map(|s| s.4).max().expect("a tail");
        assert!(
            tail_spaces <= 2,
            "{kind:?}: the plateau holds at most one retired space, got {tail_spaces}"
        );
        let tail_band = band_edge(c.heap_policy(), tail_live);
        assert!(
            tail_young <= tail_band,
            "{kind:?}: the plateau's nursery sits inside the live set's band (8 × \
             young_target_words({tail_live}) = {tail_band}), got {tail_young}"
        );
        assert!(
            tail_retained <= tail_young,
            "{kind:?}: the plateau's retention is one nursery space at most \
             ({tail_retained} > {tail_young})"
        );
        // The mature generation has converged: one fixed capacity over the whole tail.
        let mature = tail[0].3;
        assert!(
            tail.iter().all(|s| s.3 == mature),
            "{kind:?}: the mature capacity must converge, got {tail:?}"
        );
        assert_eq!(
            tail[tail.len() - 1].0,
            c.young_space_count(),
            "{kind:?}: the tail's last state is the collector's current state"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. A collection collapses the chain to one space (the bounded-cycle guard)
// ---------------------------------------------------------------------------

#[test]
fn a_repeated_grow_and_collect_cycle_never_accumulates_spaces() {
    for kind in KINDS {
        let mut c = any_for(kind, HeapPolicy::default());
        let mut max_spaces = 0usize;
        let mut max_retained = 0usize;
        let mut max_young = 0usize;
        let mut grew = 0usize;
        for round in 0..ROUNDS {
            let live = tuple_in(c.young_mut(), &[make_small(round as i64)]);
            // A request 4096 words large. The rule's **target** for this corpus is far below that
            // (one round's live set is a 2-word tuple plus whatever a fullsweep has not reclaimed
            // yet, so `young_target_words` answers at most 610 and the band's top 4880), so a
            // growth is a real replacement whenever the settled capacity does not already cover
            // the request — and the rounds where it is a no-op are exactly the ones whose settled
            // capacity does. Both arms are pinned below.
            let before = c.young().capacity_words();
            let room = c.young().available_words();
            c.grow_nursery(4096)
                .unwrap_or_else(|e| panic!("{kind:?} round {round}: grow failed {e:?}"));

            // **The chain holds at most one retired space** (`src/nursery.rs`: a growth retires
            // the space it supersedes and keeps it addressable, and nothing else is ever appended
            // — `young_space_count() <= 2` for the whole life of the nursery). The retained
            // capacity is then exactly the superseded space, so it can never ratchet: one space,
            // never one per growth.
            let spaces = c.young_space_count();
            let young = c.young().capacity_words();
            assert!(
                spaces <= 2,
                "{kind:?} round {round}: the chain holds at most one retired space, got {spaces}"
            );
            if spaces == 2 {
                grew += 1;
                assert_eq!(
                    retained_words(&c),
                    before,
                    "{kind:?} round {round}: a growth retains exactly the {before}-word space it \
                     superseded"
                );
                assert!(
                    young > before,
                    "{kind:?} round {round}: the request did enlarge the heap ({young} <= {before})"
                );
            } else {
                // No retired space: the current space already had the room, so the growth was the
                // rule's no-op arm — nothing was replaced and nothing was retained.
                assert!(
                    room >= 4096,
                    "{kind:?} round {round}: a no-op growth needs the room it asked for \
                     ({room} < 4096)"
                );
                assert_eq!(
                    young, before,
                    "{kind:?} round {round}: a no-op growth keeps the space it had"
                );
                assert_eq!(
                    retained_words(&c),
                    0,
                    "{kind:?} round {round}: a one-space chain retains nothing"
                );
            }
            max_spaces = max_spaces.max(spaces);
            max_retained = max_retained.max(retained_words(&c));
            max_young = max_young.max(young);

            let mut roots = vec![live];
            c.collect(&mut roots).expect("collect");
            assert_eq!(
                c.young_space_count(),
                1,
                "{kind:?} round {round}: a collection collapses the chain"
            );
            assert!(
                !c.collection_debt(),
                "{kind:?} round {round}: the debt is settled by the collection"
            );
            assert_eq!(
                retained_words(&c),
                0,
                "{kind:?} round {round}: the collection gives the retained capacity back"
            );
            // And the capacity it settles on is the live set's, inside the documented band.
            let live_after = c.mature_used_words();
            let band = band_edge(c.heap_policy(), live_after);
            assert!(
                c.young().capacity_words() <= band,
                "{kind:?} round {round}: a collection settles inside the live set's band \
                 (8 × young_target_words({live_after}) = {band}), got {}",
                c.young().capacity_words()
            );
        }
        // **Nothing accumulates over the run**: the chain never exceeded two spaces, the retention
        // never exceeded one nursery space, and the growth seam really was exercised — so the
        // two-space guard is not vacuous.
        assert!(
            max_spaces <= 2,
            "{kind:?}: the chain never exceeded one retired space, got {max_spaces}"
        );
        assert!(
            max_retained <= max_young,
            "{kind:?}: the retention never exceeded one nursery space \
             ({max_retained} > {max_young})"
        );
        assert!(
            grew > ROUNDS / 4,
            "{kind:?}: the loop must really replace a space (grew {grew} of {ROUNDS} rounds)"
        );
        // After the last collapse the chain is one space and carries no debt.
        assert_eq!(c.young_space_count(), 1, "{kind:?}: one space at the end");
        assert_eq!(retained_words(&c), 0, "{kind:?}: no retained capacity left");
    }
}
