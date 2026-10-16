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

//! **Mark-region fragmentation** — a promote/reclaim cycle of multi-line objects.
//!
//! Line-granular allocation ([`ImmixSpace`](beam_gc::prelude::ImmixSpace)) needs a
//! *contiguous run* of free lines **inside one block** for every object it places, while the
//! region-wide `free_lines()` only counts the total. A region can therefore report room for
//! a promotion and still have no run to put an object in: two 7-line objects fill 14 of a
//! block's 16 lines, so a region of fully-packed blocks has free lines no object can use.
//!
//! This file drives exactly that shape — a fixed live set of 100-word (7-line) tuples, plus a
//! larger 9-line variant, re-promoted every round against a 2048-word (8-block) region — and
//! pins the three halves of the contract that keep it a *bounded steady state* instead of a
//! failure:
//!
//! * **the shape**: a fully packed region has `free_lines() == 16` and yet
//!   `can_reserve(100) == false`, so the region-wide pre-check cannot see the shortfall;
//! * **the promotion is atomic**: a [minor](GenImmixCollector::collect_minor) whose
//!   reservation cannot place an object reports the typed heap exhaustion with the nursery
//!   **byte-identical** (no marker written, no root rewritten) and a retry reproduces it, so
//!   a half-forwarded heap is never observable;
//! * **the collector recovers**: [`GenImmixCollector::collect`] escalates such a minor to a
//!   fullsweep, whose in-place reclaim (and, if needed, a freshly packed region) makes room —
//!   so the round completes, the live set stays structurally intact, and every count this
//!   file asserts is deterministic.
//!
//! Everything asserted here is a count or a structural comparison (`beam_term::compare`);
//! nothing is timed.

use beam_common::BeamError;
use beam_gc::prelude::{
    AnyCollector, CollectionKind, GcConfig, GenImmixCollector, HeapPolicy, HeapSpace,
    LINES_PER_BLOCK, LINE_WORDS,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, AtomTable, Eterm};

/// The mature region the cycles run against: 2048 words = 8 blocks = 128 lines.
const REGION_WORDS: usize = 2048;

/// The nursery the cycles allocate in, and its floor: big enough for one round's fresh
/// objects however far a collection shrinks it (a fullsweep's collapse sizes the nursery
/// through `young_after_collection`, whose only floor is `min_heap_words`).
const NURSERY_WORDS: usize = 1024;

/// A 100-word tuple: a header plus 99 elements, i.e. exactly **7** Immix lines.
const ELEMS: usize = 99;

/// A 129-word tuple: a header plus 128 elements, i.e. exactly **9** Immix lines.
const BIG_ELEMS: usize = 128;

/// How many live tuples the cycles keep alive from round to round.
const LIVE: usize = 2;

/// The lines one `elems`-element tuple occupies — the run length a block must have free.
const fn lines_per_object(elems: usize) -> usize {
    (elems + 1).div_ceil(LINE_WORDS)
}

/// The heap-size policy every collector in this file uses: the region under test, and a
/// nursery no collection can shrink below one round's corpus.
fn policy(initial_mature_words: usize) -> HeapPolicy {
    HeapPolicy {
        min_heap_words: NURSERY_WORDS,
        initial_young_words: NURSERY_WORDS,
        initial_mature_words,
        ..HeapPolicy::default()
    }
}

/// Allocates an `elems`-element tuple whose elements are `tag * 1000 + i` in the nursery of
/// `c`.
fn tuple_for(c: &mut GenImmixCollector, elems: usize, tag: i64) -> Eterm {
    let values: Vec<Eterm> = (0..elems as i64)
        .map(|i| make_small(tag * 1000 + i))
        .collect();
    c.young_mut()
        .heap_mut()
        .tuple(&values)
        .expect("the tuple fits the nursery")
}

/// Asserts `term` is an `elems`-element tuple whose elements are exactly the `tag` sequence.
fn assert_tuple(term: Eterm, elems: usize, tag: i64) {
    // SAFETY: `term` is a live tuple of `elems + 1` words the caller's graph holds.
    let words = beam_term::prelude::boxed_val(term);
    for i in 0..elems as i64 {
        // SAFETY: elements 1..=elems of a live `elems + 1`-word tuple.
        let value = unsafe { *words.add(1 + i as usize) };
        assert_eq!(
            beam_term::prelude::signed_value(value),
            tag * 1000 + i,
            "element {i} of the tuple tagged {tag}"
        );
    }
}

/// The `round`-tagged corpus: `LIVE` fresh `elems`-element tuples in the nursery.
fn corpus(c: &mut GenImmixCollector, elems: usize, round: i64) -> Vec<Eterm> {
    (0..LIVE as i64)
        .map(|i| tuple_for(c, elems, round * 100 + i))
        .collect()
}

/// The nursery's **live region word for word** — the byte-identity witness an atomicity pin
/// needs. A forwarding marker written into a from-space object, or a rewritten reference word
/// inside one, changes exactly one of these words.
fn nursery_snapshot(c: &GenImmixCollector) -> Vec<Eterm> {
    let space: &HeapSpace = c.young();
    let base = space.base();
    let used = space.used_words();
    // SAFETY: `[base, base + used)` is exactly the region this nursery handed the mutator, so
    // every word in it is initialised.
    (0..used).map(|i| unsafe { *base.add(i) }).collect()
}

/// Drives `c` with one `LIVE`-tuple-per-round corpus until the region's occupancy reaches
/// `target_lines` — the point a *fresh* round's promotion must fragment it. Returns how many
/// rounds ran, and asserts each one was a minor that left every root intact.
fn pack_region(c: &mut GenImmixCollector, elems: usize, target_lines: usize) -> i64 {
    let mut round = 0i64;
    while c.mature().used_lines() < target_lines {
        let mut roots = corpus(c, elems, round);
        let collection = c
            .collect(&mut roots)
            .unwrap_or_else(|err| panic!("round {round}: {err:?}"));
        assert_eq!(collection.kind, CollectionKind::Minor, "round {round}");
        for (i, root) in roots.iter().enumerate() {
            assert_tuple(*root, elems, round * 100 + i as i64);
        }
        round += 1;
    }
    round
}

/// What one steady-state sweep observed.
struct SteadyState {
    /// The highest occupancy any round left behind, in lines.
    peak_used_lines: usize,
    /// The capacity the region converged to, in words.
    capacity_words: usize,
    /// How many of the rounds were fullsweeps.
    majors: u64,
}

/// Drives `rounds` collections of `elems`-element tuples behind `collect`, comparing every
/// root against an independently built witness (`beam_term::compare::eq`) after every round —
/// so a corrupted or mis-placed live object fails the sweep wherever it happens.
///
/// `build` allocates a round's corpus (the two collectors below reach their nursery
/// differently), `capacity`/`used_lines` read the region through the type's own accessors.
fn drive<C>(
    c: &mut C,
    elems: usize,
    rounds: i64,
    build: impl Fn(&mut C, usize, i64) -> Vec<Eterm>,
    collect: impl Fn(&mut C, &mut [Eterm]) -> Result<CollectionKind, BeamError>,
    capacity: impl Fn(&C) -> usize,
    used_lines: impl Fn(&C) -> usize,
) -> SteadyState {
    let atoms = AtomTable::new();
    let mut witness = beam_term::heap::Heap::new(elems + 8);
    let mut state = SteadyState {
        peak_used_lines: 0,
        capacity_words: 0,
        majors: 0,
    };
    for round in 0..rounds {
        let mut roots = build(c, elems, round);
        let collection =
            collect(c, &mut roots).unwrap_or_else(|err| panic!("round {round}: {err:?}"));
        if collection == CollectionKind::Major {
            state.majors += 1;
        }
        for (i, root) in roots.iter().enumerate() {
            let tag = round * 100 + i as i64;
            assert_tuple(*root, elems, tag);
            witness.reset();
            let expected: Vec<Eterm> = (0..elems as i64)
                .map(|j| make_small(tag * 1000 + j))
                .collect();
            let w = witness.tuple(&expected).expect("witness tuple");
            assert!(term_eq(*root, w, &atoms), "round {round}: root {i}");
        }
        state.peak_used_lines = state.peak_used_lines.max(used_lines(c));
        state.capacity_words = capacity(c);
    }
    state
}

// ---------------------------------------------------------------------------
// 1. The shape: a fully packed region has free lines no run can use
// ---------------------------------------------------------------------------

#[test]
fn the_two_line_tail_of_a_fully_packed_region_reproduces_the_gap() {
    let mut c = GenImmixCollector::from_policy(policy(REGION_WORDS)).expect("collector");
    assert_eq!(c.mature().capacity_words(), REGION_WORDS);
    let blocks = c.mature().block_count();
    assert_eq!(blocks, 8);
    // Two 7-line objects per block fill 14 of 16 lines; 8 blocks therefore hold 16.
    let per_block = LINES_PER_BLOCK / lines_per_object(ELEMS);
    assert_eq!(per_block, 2);
    let capacity_in_objects = blocks * per_block;
    assert_eq!(capacity_in_objects, 16);

    let rounds = pack_region(&mut c, ELEMS, capacity_in_objects * lines_per_object(ELEMS));
    assert_eq!(rounds, 8, "16 objects, two per round");
    assert_eq!(c.mature().used_lines(), 112, "8 blocks x 14 live lines");
    assert_eq!(
        c.mature().free_lines(),
        16,
        "two unusable lines per block remain"
    );
    assert!(
        !c.mature().can_reserve(ELEMS + 1),
        "the region has 16 free lines but no 7-line run"
    );
    println!("gap reproduced after {rounds} rounds");
}

// ---------------------------------------------------------------------------
// 2. Atomicity: a refused promotion leaves both spaces byte-identical
// ---------------------------------------------------------------------------

#[test]
fn a_refused_promotion_leaves_both_spaces_byte_identical() {
    let mut c = GenImmixCollector::from_policy(policy(REGION_WORDS)).expect("collector");
    let blocks = c.mature().block_count();
    let target_lines =
        blocks * (LINES_PER_BLOCK / lines_per_object(ELEMS)) * lines_per_object(ELEMS);
    let round = pack_region(&mut c, ELEMS, target_lines);
    assert_eq!(c.mature().used_lines(), 112);
    assert_eq!(c.mature().free_lines(), 16);

    // A fresh 7-line promotion: the region has 16 free lines (so the region-wide pre-check
    // passes) but no 7-line run anywhere.
    let mut roots = corpus(&mut c, ELEMS, round);
    assert_eq!(roots.len(), LIVE);
    let roots_before = roots.clone();
    let nursery_before = nursery_snapshot(&c);
    let mature_words_before = c.mature().used_words();
    let minors_before = c.stats().minor_collections;

    let err = c
        .collect_minor(&mut roots)
        .expect_err("the region has no 7-line run for the promotion");
    assert!(matches!(err, BeamError::Alloc(_)), "got {err:?}");

    // Nothing was copied, no marker was written and no counter advanced: the from-space and
    // the destination are exactly as they were, so no half-forwarded heap is observable.
    assert_eq!(roots, roots_before, "no root word was rewritten");
    assert_eq!(
        nursery_snapshot(&c),
        nursery_before,
        "the nursery is byte-identical: no forwarding marker, no rewritten slot"
    );
    assert_eq!(c.mature().used_words(), mature_words_before);
    assert_eq!(
        c.mature().used_lines(),
        112,
        "the mature region is untouched"
    );
    assert_eq!(c.mature().free_lines(), 16);
    assert_eq!(c.stats().minor_collections, minors_before, "not counted");
    assert!(
        roots.iter().all(|r| c.young().contains_term(*r)),
        "every root is still resident in the un-forwarded nursery"
    );

    // A retry is a pure function of the region: the same outcome, still nothing written.
    let err = c
        .collect_minor(&mut roots)
        .expect_err("the refusal is reproducible");
    assert!(matches!(err, BeamError::Alloc(_)), "got {err:?}");
    assert_eq!(nursery_snapshot(&c), nursery_before, "still byte-identical");
    assert_eq!(c.mature().used_words(), mature_words_before);

    // `collect` — the recovering entry point — takes the fullsweep instead and completes.
    let collection = c.collect(&mut roots).expect("the fullsweep recovers");
    assert_eq!(collection.kind, CollectionKind::Major);
    assert_eq!(
        c.mature().used_lines(),
        LIVE * lines_per_object(ELEMS),
        "exactly the new round's live set is resident"
    );
    for (i, root) in roots.iter().enumerate() {
        assert_tuple(*root, ELEMS, round * 100 + i as i64);
    }
}

// ---------------------------------------------------------------------------
// 3. Recovery: a fragmented region reaches a bounded steady state
// ---------------------------------------------------------------------------

#[test]
fn a_promote_reclaim_cycle_reaches_a_bounded_steady_state() {
    let mut c = GenImmixCollector::from_policy(policy(REGION_WORDS)).expect("collector");
    let state = drive(
        &mut c,
        ELEMS,
        64,
        |c, elems, round| corpus(c, elems, round),
        |c, roots| c.collect(roots).map(|collection| collection.kind),
        |c| c.mature().capacity_words(),
        |c| c.mature().used_lines(),
    );

    // Bounded and deterministic: the region converges to the four-block size the fullsweep
    // policy asks for (once below the packed 8-block region, the policy keeps it), and the
    // peak occupancy is the packed 2048-word region's own 112 lines — never more.
    assert_eq!(
        state.capacity_words, 1024,
        "the region converged to 1024 words (four blocks)"
    );
    assert_eq!(
        state.peak_used_lines, 112,
        "the peak occupancy is the packed 2048-word region's 8 x 14 lines"
    );
    assert_eq!(
        state.majors, 14,
        "the fragmented rounds escalated to fullsweeps"
    );
    assert_eq!(c.stats().minor_collections, 50, "the rest stayed minors");
    println!(
        "steady state: capacity={} peak_used_lines={} majors={} minors={}",
        state.capacity_words,
        state.peak_used_lines,
        c.stats().major_collections,
        c.stats().minor_collections
    );
}

// ---------------------------------------------------------------------------
// 4. The larger variant: a 9-line object (one per block) fragments the same way
// ---------------------------------------------------------------------------

#[test]
fn larger_objects_reach_the_same_bounded_steady_state() {
    // Nine lines per object means one object per block with a seven-line tail, so the region
    // packs after four rounds and the fifth round's promotion has no 9-line run left — the
    // same refusal, from a different geometry.
    assert_eq!(lines_per_object(BIG_ELEMS), 9);
    assert_eq!(LINES_PER_BLOCK / lines_per_object(BIG_ELEMS), 1);

    let mut c = GenImmixCollector::from_policy(policy(REGION_WORDS)).expect("collector");
    let state = drive(
        &mut c,
        BIG_ELEMS,
        48,
        |c, elems, round| corpus(c, elems, round),
        |c, roots| c.collect(roots).map(|collection| collection.kind),
        |c| c.mature().capacity_words(),
        |c| c.mature().used_lines(),
    );
    // Deterministic and bounded: the policy keeps the 8-block region (its occupancy stays
    // below the shrink threshold) and the peak is the packed region's own 8 x 9 lines.
    assert_eq!(
        state.capacity_words, 2048,
        "the region is kept at its starting size"
    );
    assert_eq!(
        state.peak_used_lines, 72,
        "the peak occupancy is the packed 2048-word region's 8 x 9 lines"
    );
    assert_eq!(
        state.majors, 11,
        "the fragmented rounds escalated to fullsweeps"
    );
    println!(
        "large-object steady state: capacity={} peak_used_lines={} majors={}",
        state.capacity_words, state.peak_used_lines, state.majors
    );
}

// ---------------------------------------------------------------------------
// 5. The same two steady states through the strategy-dispatching seam
// ---------------------------------------------------------------------------

#[test]
fn the_strategy_seam_reaches_the_same_steady_state() {
    for (elems, rounds) in [(ELEMS, 64i64), (BIG_ELEMS, 48i64)] {
        let mut c = AnyCollector::from_config(&GcConfig::immix(policy(REGION_WORDS)))
            .expect("immix collector");
        let state = drive(
            &mut c,
            elems,
            rounds,
            |c, elems, round| {
                (0..LIVE as i64)
                    .map(|i| {
                        let tag = round * 100 + i;
                        let values: Vec<Eterm> = (0..elems as i64)
                            .map(|j| make_small(tag * 1000 + j))
                            .collect();
                        c.young_mut()
                            .heap_mut()
                            .tuple(&values)
                            .expect("the tuple fits the nursery")
                    })
                    .collect()
            },
            |c, roots| c.collect(roots).map(|collection| collection.kind),
            |c| c.mature_capacity_words(),
            |c| c.mature_used_words() / LINE_WORDS,
        );
        assert!(
            state.capacity_words <= REGION_WORDS,
            "elems {elems}: the region stayed bounded at {}",
            state.capacity_words
        );
        assert!(
            state.peak_used_lines <= REGION_WORDS / LINE_WORDS,
            "elems {elems}: the occupancy stayed bounded"
        );
        assert!(state.majors > 0, "elems {elems}: fullsweeps were reached");
        // The seam reaches the very same steady state as the concrete collector: the region
        // sizes and the peak occupancies are identical on both routes.
        let expected = match elems {
            ELEMS => (1024usize, 112usize),
            BIG_ELEMS => (2048, 72),
            other => panic!("unexpected corpus {other}"),
        };
        assert_eq!(
            (state.capacity_words, state.peak_used_lines),
            expected,
            "elems {elems}: the seam must match the concrete collector's steady state"
        );
        println!(
            "seam steady state (elems {elems}): capacity={} peak_used_lines={} majors={}",
            state.capacity_words, state.peak_used_lines, state.majors
        );
    }
}
