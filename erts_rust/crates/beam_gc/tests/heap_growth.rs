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

//! **On-demand heap growth** evidence for `beam_gc` — Stage-4 heap growth.
//!
//! `tests/heap_sizing.rs` pins the *policy arithmetic* (the curated table, the growth/shrink
//! rules, the fullsweep cadence). This file pins the two behaviours a runtime relies on when
//! the mutator outgrows its heap:
//!
//! 1. **Sustained allocation grows the spaces.** A live set that keeps climbing forces the
//!    nursery and the mature generation past their initial sizes, and every live term stays
//!    structurally intact after every collection (`beam_term::compare::eq`).
//! 2. **A collection never leaves a half-forwarded from-space.** A policy whose
//!    `max_heap_words` cannot fit the live set makes `collect` / `collect_major` /
//!    `collect_minor` return a **typed** heap-exhaustion error *before* a single word is
//!    copied — the from-space is untouched, so reading the roots still yields the correct
//!    live terms and re-attempting the collection is idempotent.
//! 3. **An unbounded policy never errors.** The same growing workload keeps almost all of the
//!    heap live and grows the space on demand.
//! 4. **[`ensure_capacity`](beam_gc::Collector::ensure_capacity)** (the on-demand growth
//!    seam) grows the nursery so a request far larger than its current size fits, and returns
//!    a typed error when the policy's ceiling makes the request impossible. [`AnyCollector`]
//!    forwards it to either mature-generation strategy.
//!
//! Every corpus here is deterministic (no randomness), and every cross-collection comparison
//! is structural. Timings are never asserted.

use beam_common::BeamError;
use beam_gc::prelude::{AnyCollector, CollectionKind, Collector, GcConfig, HeapPolicy, MatureKind};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Allocates a `{a, b}` tuple in the nursery.
fn live_tuple(c: &mut Collector, a: i64, b: i64) -> Eterm {
    c.young_mut()
        .heap_mut()
        .tuple(&[make_small(a), make_small(b)])
        .expect("live tuple")
}

/// Asserts every live root still holds its `(a, b)` pair, structurally, via
/// `beam_term::compare::eq` against an independently built witness.
fn assert_pairs_intact(roots: &[Eterm], expected: &[(i64, i64)]) {
    assert_eq!(roots.len(), expected.len(), "root/expectation count");
    let atoms = AtomTable::new();
    let mut witness = beam_term::heap::Heap::new(8);
    for (i, (root, (a, b))) in roots.iter().zip(expected).enumerate() {
        witness.reset();
        let w = witness
            .tuple(&[make_small(*a), make_small(*b)])
            .expect("witness tuple");
        assert!(
            term_eq(*root, w, &atoms),
            "root {i} was corrupted (expected {{{a},{b}}})"
        );
    }
}

// ---------------------------------------------------------------------------
// 1. Sustained allocation grows the spaces and preserves the live set
// ---------------------------------------------------------------------------

#[test]
fn sustained_allocation_grows_the_spaces_and_preserves_every_live_term() {
    // Start tiny; the live set climbs every round, so the collector must grow the nursery
    // and the mature generation past these initial sizes to keep it alive.
    let initial_young = 51;
    let initial_mature = 51;
    let policy = HeapPolicy {
        initial_young_words: initial_young,
        initial_mature_words: initial_mature,
        fullsweep_after: 65535,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");
    assert_eq!(collector.young().capacity_words(), initial_young);
    assert_eq!(collector.mature().capacity_words(), initial_mature);

    let mut roots: Vec<Eterm> = Vec::new();
    let mut expected: Vec<(i64, i64)> = Vec::new();

    for round in 0..30i64 {
        // A growing batch of long-lived tuples, so the live set keeps climbing.
        let batch = 2 + round;
        for j in 0..batch {
            let a = round * 1000 + j;
            roots.push(live_tuple(&mut collector, a, 2 * a));
            expected.push((a, 2 * a));
        }

        let collection = collector.collect(&mut roots).expect("collect");
        assert_eq!(collector.young().used_words(), 0, "the nursery is emptied");
        assert!(
            matches!(
                collection.kind,
                CollectionKind::Minor | CollectionKind::Major
            ),
            "a collection ran"
        );

        // After *every* collection the whole live set is structurally intact.
        assert_pairs_intact(&roots, &expected);
    }

    // The spaces really did grow: the live set (495 tuples = 1485 words) far exceeds the
    // 51-word initial sizes, so both the nursery and the mature generation had to expand.
    assert!(
        collector.young().capacity_words() > initial_young,
        "the nursery grew ({} -> ...)",
        initial_young
    );
    assert!(
        collector.mature().capacity_words() > initial_mature,
        "the mature generation grew ({} -> ...)",
        initial_mature
    );
    assert!(
        collector.mature().used_words() > initial_mature,
        "the live set now occupies the grown mature generation"
    );

    // A final fullsweep keeps everything alive and correct.
    collector.collect_major(&mut roots).expect("fullsweep");
    assert_pairs_intact(&roots, &expected);
}

// ---------------------------------------------------------------------------
// 2. A capped heap reports a typed error and leaves the from-space untouched
// ---------------------------------------------------------------------------

/// A collector whose `max_heap_words` forbids holding `live` words, with `live` already
/// resident in the nursery.
fn capped_collector_with_live_set() -> (Collector, Vec<Eterm>, Vec<(i64, i64)>) {
    let policy = HeapPolicy {
        initial_young_words: 233,
        initial_mature_words: 12,
        max_heap_words: 90,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");

    // 34 two-element tuples = 34 * 3 = 102 live words, comfortably over the 90-word cap.
    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..34i64 {
        roots.push(live_tuple(&mut collector, i, 2 * i));
        expected.push((i, 2 * i));
    }
    (collector, roots, expected)
}

#[test]
fn a_full_sweep_that_cannot_fit_the_live_set_leaves_the_from_space_intact() {
    let (mut collector, mut roots, expected) = capped_collector_with_live_set();
    let live = collector.young().used_words();
    assert_eq!(live, 102, "34 tuples of 3 words each");
    assert!(
        collector.mature().available_words() < live,
        "the mature space cannot hold the live set"
    );

    // A fullsweep cannot size a fresh space to hold the live set: a typed error, before any
    // word is copied.
    let err = collector
        .collect_major(&mut roots)
        .expect_err("must not fit");
    assert!(matches!(err, BeamError::Alloc(_)), "got {err:?}");
    assert_eq!(err.as_atom(), Some("out_of_memory"));

    // Nothing was copied: both from-spaces are exactly as they were.
    assert_eq!(
        collector.young().used_words(),
        live,
        "the nursery is untouched"
    );
    assert_eq!(
        collector.mature().used_words(),
        0,
        "the mature space is untouched"
    );
    assert_eq!(
        collector.stats().major_collections,
        0,
        "the failed fullsweep is not counted"
    );
    assert!(
        roots.iter().all(|r| collector.young().contains_term(*r)),
        "every root is still resident in the (un-forwarded) nursery"
    );
    assert_pairs_intact(&roots, &expected);

    // A direct minor is guarded the same way (its destination is too small too).
    let minor_err = collector
        .collect_minor(&mut roots)
        .expect_err("must not fit");
    assert!(matches!(minor_err, BeamError::Alloc(_)));
    assert_eq!(collector.stats().minor_collections, 0);
    assert_pairs_intact(&roots, &expected);

    // `collect` escalates a would-not-fit minor to a fullsweep, which is capped as well.
    let collect_err = collector.collect(&mut roots).expect_err("must not fit");
    assert!(matches!(collect_err, BeamError::Alloc(_)));

    // Re-attempting the fullsweep is idempotent: it errors again and the graph is still
    // intact — which it could not be if the first attempt had half-forwarded the nursery.
    let again = collector
        .collect_major(&mut roots)
        .expect_err("still must not fit");
    assert!(matches!(again, BeamError::Alloc(_)));
    assert_pairs_intact(&roots, &expected);
}

#[test]
fn the_same_corpus_collects_successfully_under_a_workable_policy() {
    // The corpus that the capped policy cannot collect is collectible under an unbounded one
    // — isolating the failure to the cap, not to the corpus.
    let policy = HeapPolicy {
        initial_young_words: 233,
        initial_mature_words: 12,
        max_heap_words: 0,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");

    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..34i64 {
        roots.push(live_tuple(&mut collector, i, 2 * i));
        expected.push((i, 2 * i));
    }

    collector
        .collect_major(&mut roots)
        .expect("unbounded fullsweep fits");
    assert_eq!(collector.stats().major_collections, 1);
    assert!(
        roots.iter().all(|r| collector.mature().contains_term(*r)),
        "every root was promoted"
    );
    assert_pairs_intact(&roots, &expected);
}

// ---------------------------------------------------------------------------
// 3. An unbounded policy grows on demand and never errors
// ---------------------------------------------------------------------------

#[test]
fn an_unbounded_policy_keeps_a_growing_live_set_and_never_errors() {
    // A bounded-but-large live set with `max_heap_words == 0`: growth is unlimited, so no
    // collection may ever fail. `fullsweep_after: 3` exercises both minors and majors.
    let policy = HeapPolicy {
        initial_young_words: 51,
        initial_mature_words: 51,
        max_heap_words: 0,
        fullsweep_after: 3,
        ..HeapPolicy::default()
    };
    assert_eq!(policy.max_heap_words, 0, "unbounded growth");
    let mut collector = Collector::new(policy).expect("collector");

    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for round in 0..60i64 {
        for j in 0..6i64 {
            let a = round * 100 + j;
            roots.push(live_tuple(&mut collector, a, 2 * a));
            expected.push((a, 2 * a));
        }
        // The whole point: an unbounded policy always finds room for the growing live set.
        collector
            .collect(&mut roots)
            .expect("unbounded growth must never fail");
        assert_pairs_intact(&roots, &expected);
    }

    assert!(
        collector.mature().capacity_words() > 51,
        "the mature generation grew on demand"
    );
    assert!(
        collector.mature().used_words() > 51,
        "the grown space holds the (large) live set"
    );
    assert!(
        collector.stats().major_collections > 0 && collector.stats().minor_collections > 0,
        "the cadence exercised both collection kinds"
    );
}

// ---------------------------------------------------------------------------
// 4. `ensure_capacity` — the on-demand growth seam
// ---------------------------------------------------------------------------

#[test]
fn ensure_capacity_grows_the_nursery_to_fit_a_large_request() {
    let policy = HeapPolicy {
        initial_young_words: 51,
        initial_mature_words: 90,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");

    // A little live data, then a request far larger than the nursery starts with.
    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..4i64 {
        roots.push(live_tuple(&mut collector, i, 2 * i));
        expected.push((i, 2 * i));
    }
    let need = 4096usize;
    assert!(
        collector.young().available_words() < need,
        "the nursery starts too small for the request"
    );

    let collection = collector.ensure_capacity(need, &mut roots).expect("grow");

    assert_eq!(
        collection.kind,
        CollectionKind::Minor,
        "a plain minor made room (the mature generation held the nursery)"
    );
    assert_eq!(
        collector.stats().minor_collections,
        1,
        "exactly one collection ran"
    );
    assert!(
        collector.young().available_words() >= need,
        "the nursery now satisfies the request"
    );
    assert!(collector.young().capacity_words() >= need);
    // The live data survived the collection and the growth.
    assert!(
        roots.iter().all(|r| collector.mature().contains_term(*r)),
        "the live tuples were promoted"
    );
    assert_pairs_intact(&roots, &expected);
}

#[test]
fn ensure_capacity_reports_a_typed_error_when_growth_is_capped() {
    let policy = HeapPolicy {
        initial_young_words: 51,
        max_heap_words: 100,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");
    let mut roots = vec![live_tuple(&mut collector, 1, 2)];

    // 4096 words can never be reached with a 100-word ceiling: a typed error, no panic.
    let err = collector
        .ensure_capacity(4096, &mut roots)
        .expect_err("growth is capped");
    assert!(matches!(err, BeamError::Alloc(_)), "got {err:?}");
    assert_eq!(err.as_atom(), Some("out_of_memory"));

    // A small request under the same cap still succeeds, and the collector stays usable.
    let collection = collector
        .ensure_capacity(64, &mut roots)
        .expect("fits under the cap");
    assert!(collector.young().available_words() >= 64);
    assert!(matches!(collection.kind, CollectionKind::Minor));

    // The live root is untouched throughout.
    assert_pairs_intact(&roots, &[(1, 2)]);
}

#[test]
fn any_collector_forwards_ensure_capacity_to_either_strategy() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let policy = HeapPolicy {
            initial_young_words: 51,
            initial_mature_words: 90,
            ..HeapPolicy::default()
        };
        let mut c = AnyCollector::from_config(&GcConfig::copying(policy).with_mature(kind))
            .expect("collector");
        assert_eq!(c.kind(), kind);

        let mut roots = Vec::new();
        for i in 0..4i64 {
            roots.push(
                c.young_mut()
                    .heap_mut()
                    .tuple(&[make_small(i), make_small(2 * i)])
                    .expect("live tuple"),
            );
        }

        let need = 4096usize;
        assert!(c.young().available_words() < need, "{kind:?}");
        let collection = c.ensure_capacity(need, &mut roots).expect("grow");
        assert!(matches!(collection.kind, CollectionKind::Minor), "{kind:?}");
        assert!(
            c.young().available_words() >= need,
            "{kind:?}: the nursery grew to fit the request"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. The growth knob is explicit
// ---------------------------------------------------------------------------

#[test]
fn unbounded_and_growable_express_unlimited_growth() {
    // `max_heap_words == 0` is the "unbounded growth" encoding, and both named constructors
    // express exactly it.
    assert_eq!(HeapPolicy::unbounded().max_heap_words, 0);
    assert_eq!(HeapPolicy::unbounded(), HeapPolicy::default());
    let config = GcConfig::growable();
    assert_eq!(config.policy().max_heap_words, 0);
    assert_eq!(config, GcConfig::default());
    assert_eq!(config.mature(), MatureKind::Copying);

    // A bounded policy is bounded; the min floor still raises a shrunk target.
    let bounded = HeapPolicy::from_min_max(500, 1_000);
    assert_eq!(bounded.max_heap_words, 1_000);
    assert_eq!(bounded.next_words(12, 0), 500, "the min floors the target");
}
