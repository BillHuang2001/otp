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

//! **Fill-barrier / young-heap-exhaustion** evidence for `beam_gc`.
//!
//! `tests/heap_growth.rs` pins the growth seam end-to-end. This file pins the *contract*
//! around it — the "fill barrier": a collection must decide whether it can hold its whole
//! result **before** it copies a single word, so a young heap that cannot be evacuated
//! reports a **typed** heap exhaustion instead of leaving the from-space half-forwarded
//! (root words rewritten to point at copies that were never made).
//!
//! What the code does, in one place (verified here, not just read):
//!
//! * A **direct** [`Collector::collect_minor`] whose mature space cannot hold the whole used
//!   nursery returns `Err(BeamError::Alloc(AllocFailure::new(AllocKind::Heap, words *
//!   size_of::<Eterm>())))` with `words == young.used_words()` — the conservative upper bound
//!   on the promotion — *before* the copier is constructed. [`Collector::collect_major`]
//!   sizes its fresh space from the policy up front and reports the same typed error with
//!   `words == young.used_words() + mature.used_words()` when `max_heap_words` clamps that
//!   size below the live set.
//! * [`GenImmixCollector`] guards both the same way, but its pre-check is *line*-granular:
//!   `collect_minor` reports `lines * LINE_WORDS` words, and a fullsweep that has to relocate
//!   the mark-region mature space checks `free_lines()` and reports
//!   `needed_lines * LINE_WORDS`.
//! * A failed collection leaves **observable state bit-identical**: the young base/top, the
//!   capacity and used words of both spaces, `minor_gcs` and the accumulated [`GcStats`] are
//!   unchanged, every root word is byte-identical, and every root still resolves inside the
//!   from-space (i.e. nothing was forwarded). Re-attempting the identical collection errors
//!   identically — the "half-forwarded then retried" corruption is unobservable because no
//!   forward ever happened.
//! * [`Collector::ensure_capacity`] / [`GenImmixCollector::ensure_capacity`] / the
//!   strategy-dispatching [`AnyCollector::ensure_capacity`] add the mutator-facing growth
//!   step: a request the policy's ceiling makes unreachable fails **fast** (no collection at
//!   all, collector untouched); an reachable one runs exactly one collection and then leaves
//!   `young().available_words() >= need_words`, so a term the mutator writes in the window
//!   between the grow and the next collection is not lost.
//!
//! Every corpus is deterministic (no randomness) and every cross-collection comparison is
//! structural (`beam_term::compare::eq`) or an exact word comparison. Timings are never
//! asserted.

use core::mem::size_of;

use beam_common::{AllocFailure, AllocKind, BeamError};
use beam_gc::prelude::{
    heap_sizes, AnyCollector, CollectionKind, Collector, GcConfig, GcStats, GenImmixCollector,
    HeapPolicy, HeapSpace, MatureKind,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Allocates a `{a, b}` tuple in `space` (three words).
fn tuple_in(space: &mut HeapSpace, a: i64, b: i64) -> Eterm {
    space
        .heap_mut()
        .tuple(&[make_small(a), make_small(b)])
        .expect("the tuple fits the space it is written into")
}

/// The typed allocation failure carried by `err`, if it carries one at all.
fn heap_failure(err: &BeamError) -> Option<AllocFailure> {
    match err {
        BeamError::Alloc(failure) => Some(*failure),
        _ => None,
    }
}

/// Asserts `err` is the typed heap exhaustion for exactly `words` term words of a
/// `AllocKind::Heap` allocation — the *only* error a full heap may produce, reported as the
/// BEAM `out_of_memory` atom.
fn assert_heap_exhausted(err: &BeamError, words: usize) {
    let failure = heap_failure(err).expect("a typed BeamError::Alloc");
    assert_eq!(failure.kind, AllocKind::Heap, "the allocation class");
    assert_eq!(
        failure.requested,
        words * size_of::<Eterm>(),
        "the requested byte count ({words} words)"
    );
    assert_eq!(err.as_atom(), Some("out_of_memory"), "the BEAM error atom");
}

/// Everything a *failed* collection must leave untouched on the copying [`Collector`].
#[derive(Debug, PartialEq, Eq)]
struct CopyingState {
    young_base: usize,
    young_top: usize,
    young_capacity: usize,
    young_used: usize,
    mature_capacity: usize,
    mature_used: usize,
    minor_gcs: u32,
    stats: GcStats,
}

/// Snapshots the copying collector's observable state.
fn copying_state(c: &Collector) -> CopyingState {
    CopyingState {
        young_base: c.young().base() as usize,
        young_top: c.young().top() as usize,
        young_capacity: c.young().capacity_words(),
        young_used: c.young().used_words(),
        mature_capacity: c.mature().capacity_words(),
        mature_used: c.mature().used_words(),
        minor_gcs: c.minor_gcs(),
        stats: c.stats(),
    }
}

/// Everything a *failed* collection must leave untouched on the [`GenImmixCollector`].
#[derive(Debug, PartialEq, Eq)]
struct ImmixState {
    young_base: usize,
    young_top: usize,
    young_capacity: usize,
    young_used: usize,
    mature_capacity: usize,
    mature_used_words: usize,
    mature_free_lines: usize,
    minor_gcs: u32,
    stats: GcStats,
}

/// Snapshots the mark-region collector's observable state.
fn immix_state(c: &GenImmixCollector) -> ImmixState {
    ImmixState {
        young_base: c.young().base() as usize,
        young_top: c.young().top() as usize,
        young_capacity: c.young().capacity_words(),
        young_used: c.young().used_words(),
        mature_capacity: c.mature().capacity_words(),
        mature_used_words: c.mature().used_words(),
        mature_free_lines: c.mature().free_lines(),
        minor_gcs: c.minor_gcs(),
        stats: c.stats(),
    }
}

/// Asserts the root slice is *un-forwarded*: every word is byte-identical to `before` and
/// still resolves inside the from-space. A half-forwarded nursery would rewrite the root
/// words to point at copies that do not exist.
fn assert_roots_untouched(roots: &[Eterm], young: &HeapSpace, before: &[Eterm]) {
    assert_eq!(roots.len(), before.len(), "the root count");
    for (i, (root, prev)) in roots.iter().zip(before).enumerate() {
        assert_eq!(root, prev, "root {i} was rewritten — a forwarded root word");
    }
    assert!(
        roots.iter().all(|r| young.contains_term(*r)),
        "every root is still resident in the un-forwarded nursery"
    );
}

/// Asserts every root structurally equals an independently built `{a, b}` tuple.
fn assert_tuples_intact(roots: &[Eterm], expected: &[(i64, i64)]) {
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

/// Asserts every root structurally equals an independently built `[a | b]` cons cell.
fn assert_conses_intact(roots: &[Eterm], expected: &[(i64, i64)]) {
    assert_eq!(roots.len(), expected.len(), "root/expectation count");
    let atoms = AtomTable::new();
    let mut witness = beam_term::heap::Heap::new(8);
    for (i, (root, (a, b))) in roots.iter().zip(expected).enumerate() {
        witness.reset();
        let w = witness
            .cons(make_small(*a), make_small(*b))
            .expect("witness cons cell");
        assert!(
            term_eq(*root, w, &atoms),
            "root {i} was corrupted (expected [{a}|{b}])"
        );
    }
}

/// Whether `term` is resident in the selected strategy's mature generation.
fn in_mature(c: &AnyCollector, term: Eterm) -> bool {
    match c {
        AnyCollector::Copying(x) => x.mature().contains_term(term),
        AnyCollector::Immix(x) => x.mature().contains_term(term),
    }
}

/// Builds the [`AnyCollector`] for `kind` under `policy`, bypassing the automatic rule (the
/// explicit pin wins in both directions, so the strategy is the one asked for).
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::copying(policy).with_mature(kind)).expect("collector")
}

// ---------------------------------------------------------------------------
// 1. The copying collector: the typed pre-check, before any copy
// ---------------------------------------------------------------------------

#[test]
fn collect_minor_reports_typed_heap_exhaustion_before_any_copy() {
    // A 12-word mature space cannot hold a 30-word nursery: the promotion's conservative
    // upper bound is `young.used_words()`.
    let policy = HeapPolicy {
        initial_young_words: 233,
        initial_mature_words: 12,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");

    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..10i64 {
        roots.push(tuple_in(collector.young_mut(), i, 2 * i));
        expected.push((i, 2 * i));
    }
    let live = collector.young().used_words();
    assert_eq!(live, 30, "10 three-word tuples");
    assert!(
        collector.mature().available_words() < live,
        "the mature space cannot hold the nursery"
    );

    let state = copying_state(&collector);
    let before = roots.clone();

    let err = collector
        .collect_minor(&mut roots)
        .expect_err("the mature space cannot hold the live nursery");
    assert_heap_exhausted(&err, live);

    // Nothing was copied: both from-spaces are bit-identical and no counter moved.
    assert_eq!(
        copying_state(&collector),
        state,
        "a failed minor leaves no observable trace"
    );
    assert_roots_untouched(&roots, collector.young(), &before);
    assert_tuples_intact(&roots, &expected);

    // A second identical attempt errors identically — no state drift between attempts.
    let again = collector
        .collect_minor(&mut roots)
        .expect_err("still cannot hold the nursery");
    assert_eq!(format!("{again:?}"), format!("{err:?}"), "the error is repeatable");
    assert_eq!(copying_state(&collector), state, "still untouched");
    assert_roots_untouched(&roots, collector.young(), &before);
    assert_tuples_intact(&roots, &expected);
}

/// A collector whose `max_heap_words` forbids holding the 102-word live set, with that live
/// set already resident in the nursery.
fn capped_corpus(max_heap_words: usize) -> (Collector, Vec<Eterm>, Vec<(i64, i64)>) {
    let policy = HeapPolicy {
        initial_young_words: 233,
        initial_mature_words: 12,
        max_heap_words,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");

    // 34 two-element tuples = 34 * 3 = 102 live words.
    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..34i64 {
        roots.push(tuple_in(collector.young_mut(), i, 2 * i));
        expected.push((i, 2 * i));
    }
    (collector, roots, expected)
}

#[test]
fn collect_major_reports_typed_heap_exhaustion_before_any_copy() {
    let policy = HeapPolicy {
        initial_young_words: 233,
        initial_mature_words: 12,
        max_heap_words: 90,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");

    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..34i64 {
        roots.push(tuple_in(collector.young_mut(), i, 2 * i));
        expected.push((i, 2 * i));
    }
    let live = collector.young().used_words();
    assert_eq!(live, 102, "34 three-word tuples");

    let state = copying_state(&collector);
    let before = roots.clone();

    let err = collector
        .collect_major(&mut roots)
        .expect_err("the capped fresh space cannot hold the live set");
    // The policy clamps the fresh space to 90 words; the live set is 102.
    assert_heap_exhausted(&err, live);

    assert_eq!(
        copying_state(&collector),
        state,
        "a doomed fullsweep leaves both from-spaces untouched"
    );
    assert_roots_untouched(&roots, collector.young(), &before);
    assert_tuples_intact(&roots, &expected);

    let again = collector
        .collect_major(&mut roots)
        .expect_err("still capped");
    assert_eq!(format!("{again:?}"), format!("{err:?}"));
    assert_eq!(copying_state(&collector), state);
    assert_tuples_intact(&roots, &expected);
}

// ---------------------------------------------------------------------------
// 2. The mark-region collector: the same barrier, line-granular
// ---------------------------------------------------------------------------

#[test]
fn immix_collect_minor_reports_typed_heap_exhaustion_before_any_copy() {
    let policy = HeapPolicy {
        initial_young_words: 64,
        initial_mature_words: 12,
        ..HeapPolicy::default()
    };
    let mut collector = GenImmixCollector::from_policy(policy).expect("collector");
    assert_eq!(collector.mature().block_count(), 1);
    assert_eq!(
        collector.mature().free_lines(),
        16,
        "one 256-word block offers 16 lines"
    );

    // 17 one-line tuples need 17 lines; only 16 are free.
    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..17i64 {
        roots.push(tuple_in(collector.young_mut(), i, 2 * i));
        expected.push((i, 2 * i));
    }
    assert_eq!(collector.young().used_words(), 51);

    let state = immix_state(&collector);
    let before = roots.clone();

    let err = collector
        .collect_minor(&mut roots)
        .expect_err("the promotion has no room");
    assert_heap_exhausted(&err, 17 * 16);

    assert_eq!(immix_state(&collector), state, "nothing was copied");
    assert_roots_untouched(&roots, collector.young(), &before);
    assert_tuples_intact(&roots, &expected);

    let again = collector
        .collect_minor(&mut roots)
        .expect_err("still no room");
    assert_eq!(format!("{again:?}"), format!("{err:?}"));
    assert_eq!(immix_state(&collector), state);
    assert_roots_untouched(&roots, collector.young(), &before);
}

/// The immix corpus: 100 three-word tuples (300 nursery words) under the given cap.
fn immix_corpus(max_heap_words: usize) -> (GenImmixCollector, Vec<Eterm>, Vec<(i64, i64)>) {
    let policy = HeapPolicy {
        initial_young_words: 400,
        initial_mature_words: 12,
        max_heap_words,
        ..HeapPolicy::default()
    };
    let mut collector = GenImmixCollector::from_policy(policy).expect("collector");
    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..100i64 {
        roots.push(tuple_in(collector.young_mut(), i, 2 * i));
        expected.push((i, 2 * i));
    }
    (collector, roots, expected)
}

#[test]
fn immix_collect_major_reports_typed_heap_exhaustion_before_any_copy() {
    // A fullsweep must relocate the mark-region mature space when the policy asks for a
    // different size; the cap clamps the fresh region below what the live set needs.
    let (mut collector, mut roots, expected) = immix_corpus(12);
    assert_eq!(collector.young().used_words(), 300);
    assert_eq!(collector.mature().free_lines(), 16);

    let state = immix_state(&collector);
    let before = roots.clone();

    let err = collector
        .collect_major(&mut roots)
        .expect_err("the clamped region cannot hold the live set");
    // The fullsweep now sizes from the **exact live line need**: line-granular allocation
    // charges one whole 16-word line per live object (a 3-word tuple still occupies a line), so
    // the 100 live tuples need 100 lines and the fresh region offers only 16.
    assert_heap_exhausted(&err, 100 * 16);

    assert_eq!(immix_state(&collector), state, "nothing was copied");
    assert_roots_untouched(&roots, collector.young(), &before);
    assert_tuples_intact(&roots, &expected);

    let again = collector
        .collect_major(&mut roots)
        .expect_err("still cannot fit");
    assert_eq!(format!("{again:?}"), format!("{err:?}"));
    assert_eq!(immix_state(&collector), state);
    assert_tuples_intact(&roots, &expected);
}

#[test]
fn the_same_immix_corpus_collects_under_an_unbounded_policy() {
    // Isolates the failure to the cap, not to the corpus: the identical tuples collect fine
    // once the policy may size the fresh region to fit them.
    let (mut collector, mut roots, expected) = immix_corpus(0);

    collector
        .collect_major(&mut roots)
        .expect("an unbounded fullsweep fits");
    assert_eq!(collector.stats().major_collections, 1);
    assert!(
        roots.iter().all(|r| collector.mature().contains_term(*r)),
        "every root was promoted"
    );
    assert_eq!(collector.young().used_words(), 0, "the nursery was emptied");
    assert_tuples_intact(&roots, &expected);
}

// ---------------------------------------------------------------------------
// 3. `ensure_capacity`: fail fast, or grow by exactly one collection
// ---------------------------------------------------------------------------

#[test]
fn ensure_capacity_fails_fast_and_leaves_the_collector_untouched() {
    // `max_heap_words` 100 makes a 4096-word request unreachable: `next_words(4096, 0)` is
    // clamped to 100, so there is no point collecting at all.
    let policy = HeapPolicy::from_min_max(0, 100);
    let need = 4096usize;
    assert!(
        policy.next_words(need, 0) < need,
        "the cap makes the request unreachable"
    );

    // ... the copying collector, directly.
    let mut collector = Collector::new(policy.clone()).expect("collector");
    let mut roots = vec![tuple_in(collector.young_mut(), 1, 2)];
    let state = copying_state(&collector);
    let before = roots.clone();

    let err = collector
        .ensure_capacity(need, &mut roots)
        .expect_err("unreachable under the cap");
    assert_heap_exhausted(&err, need);
    assert_eq!(
        copying_state(&collector),
        state,
        "an unreachable request does not even collect"
    );
    assert_roots_untouched(&roots, collector.young(), &before);
    assert_tuples_intact(&roots, &[(1, 2)]);

    // ... the mark-region collector, directly.
    let mut collector = GenImmixCollector::from_policy(policy.clone()).expect("collector");
    let mut roots = vec![tuple_in(collector.young_mut(), 1, 2)];
    let state = immix_state(&collector);
    let before = roots.clone();

    let err = collector
        .ensure_capacity(need, &mut roots)
        .expect_err("unreachable under the cap");
    assert_heap_exhausted(&err, need);
    assert_eq!(immix_state(&collector), state, "the collector is untouched");
    assert_roots_untouched(&roots, collector.young(), &before);

    // ... and through the strategy-dispatching wrapper.
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut any = any_for(kind, policy.clone());
        let mut roots = vec![tuple_in(any.young_mut(), 1, 2)];
        let capacity = any.young().capacity_words();
        let used = any.young().used_words();
        let stats = any.stats();
        let before = roots.clone();

        let err = any
            .ensure_capacity(need, &mut roots)
            .expect_err("unreachable under the cap");
        assert_heap_exhausted(&err, need);
        assert_eq!(
            any.young().capacity_words(),
            capacity,
            "{kind:?}: the nursery did not grow"
        );
        assert_eq!(any.young().used_words(), used, "{kind:?}: nothing moved");
        assert_eq!(any.stats(), stats, "{kind:?}: no collection ran");
        assert_roots_untouched(&roots, any.young(), &before);
    }
}

#[test]
fn ensure_capacity_grows_the_nursery_for_every_strategy_and_policy() {
    let need = 4096usize;

    // `unbounded()` and a large `from_min_max` cap both admit the request, and the curated
    // growth table covers it in either case.
    for policy in [HeapPolicy::unbounded(), HeapPolicy::from_min_max(0, 100_000)] {
        assert!(
            policy.next_words(need, 0) >= need,
            "the curated table covers the request"
        );

        // ... the copying collector, directly.
        let mut collector = Collector::new(policy.clone()).expect("collector");
        let mut roots = vec![
            tuple_in(collector.young_mut(), 1, 2),
            tuple_in(collector.young_mut(), 3, 4),
        ];
        assert!(
            collector.young().available_words() < need,
            "the nursery starts too small"
        );

        let collection = collector
            .ensure_capacity(need, &mut roots)
            .expect("growth is reachable");
        assert_eq!(collection.kind, CollectionKind::Minor);
        assert!(collector.young().available_words() >= need);
        assert!(collector.young().capacity_words() >= need);
        assert_eq!(collector.stats().minor_collections, 1, "exactly one collection ran");
        assert_eq!(collector.stats().major_collections, 0);
        assert!(
            roots.iter().all(|r| collector.mature().contains_term(*r)),
            "the live set was promoted by that one collection"
        );
        assert_tuples_intact(&roots, &[(1, 2), (3, 4)]);

        // ... the mark-region collector, directly.
        let mut collector = GenImmixCollector::from_policy(policy.clone()).expect("collector");
        let mut roots = vec![
            tuple_in(collector.young_mut(), 1, 2),
            tuple_in(collector.young_mut(), 3, 4),
        ];
        assert!(collector.young().available_words() < need);

        let collection = collector
            .ensure_capacity(need, &mut roots)
            .expect("growth is reachable");
        assert_eq!(collection.kind, CollectionKind::Minor);
        assert!(collector.young().available_words() >= need);
        assert_eq!(collector.stats().minor_collections, 1, "exactly one collection ran");
        assert_eq!(collector.stats().major_collections, 0);
        assert!(
            roots.iter().all(|r| collector.mature().contains_term(*r)),
            "the live set was promoted by that one collection"
        );
        assert_tuples_intact(&roots, &[(1, 2), (3, 4)]);

        // ... and through the strategy-dispatching wrapper, for both strategies.
        for kind in [MatureKind::Copying, MatureKind::Immix] {
            let mut any = any_for(kind, policy.clone());
            let mut roots = vec![tuple_in(any.young_mut(), 1, 2)];
            let collection = any
                .ensure_capacity(need, &mut roots)
                .expect("growth is reachable");
            assert_eq!(collection.kind, CollectionKind::Minor, "{kind:?}");
            assert!(
                any.young().available_words() >= need,
                "{kind:?}: the nursery grew to fit the request"
            );
            assert_eq!(
                any.stats().minor_collections + any.stats().major_collections,
                1,
                "{kind:?}: exactly one collection ran"
            );
            assert_tuples_intact(&roots, &[(1, 2)]);
        }
    }
}

#[test]
fn the_curated_growth_table_covers_every_request_ensure_capacity_makes() {
    // `ensure_capacity` targets `policy.next_words(need, 0)` — the smallest curated size at or
    // above the request, floored by `min_heap_words` and capped by `max_heap_words`.
    let unbounded = HeapPolicy::unbounded();
    for need in [12usize, 13, 38, 39, 51, 90, 142, 233, 234, 1206, 4096] {
        let target = unbounded.next_words(need, 0);
        assert!(target >= need, "the curated size covers {need}");
        assert!(heap_sizes().contains(&target), "{target} is a curated size");
    }

    // A floor raises the target even for a trivial request; a cap clamps it even for a huge
    // one — which is exactly what makes the fail-fast branch reachable.
    assert_eq!(HeapPolicy::from_min_max(500, 0).next_words(12, 0), 500);
    assert_eq!(HeapPolicy::from_min_max(0, 100).next_words(4096, 0), 100);
    assert!(HeapPolicy::from_min_max(0, 100).next_words(4096, 0) < 4096);
}

// ---------------------------------------------------------------------------
// 4. Adversarial boundaries
// ---------------------------------------------------------------------------

#[test]
fn an_exact_fit_allocation_still_fits_and_one_word_over_does_not() {
    // "The nursery is full" is `available_words() < need_words`, so an exact-fit allocation
    // still fits: the boundary is exclusive.
    let policy = HeapPolicy {
        initial_young_words: 32,
        initial_mature_words: 233,
        ..HeapPolicy::default()
    };

    let mut collector = Collector::new(policy.clone()).expect("collector");
    // 4 three-word tuples occupy 12 of the 32 words.
    for i in 0..4i64 {
        let _ = tuple_in(collector.young_mut(), i, 2 * i);
    }
    let available = collector.young().available_words();
    assert_eq!(available, 20);
    assert!(!collector.should_collect(0), "a zero-word request always fits");
    assert!(
        !collector.should_collect(available),
        "an exact-fit request still fits"
    );
    assert!(!collector.should_collect(available - 1));
    assert!(
        collector.should_collect(available + 1),
        "one word over the available space does not fit"
    );

    // The mark-region strategy answers the identical question identically (exact-fit only).
    let mut collector = GenImmixCollector::from_policy(policy).expect("collector");
    for i in 0..4i64 {
        let _ = tuple_in(collector.young_mut(), i, 2 * i);
    }
    let available = collector.young().available_words();
    assert_eq!(available, 20);
    assert!(!collector.should_collect(available));
    assert!(collector.should_collect(available + 1));
}

#[test]
fn a_fullsweep_fits_exactly_at_the_cap_and_one_word_below_is_a_typed_error() {
    let live = 102usize; // 34 three-word tuples

    // A `max_heap_words` of exactly the live set sizes the fresh space to exactly the live
    // set: the copy fits, word for word.
    let (mut collector, mut roots, expected) = capped_corpus(live);
    assert_eq!(collector.young().used_words(), live);
    collector
        .collect_major(&mut roots)
        .expect("an exact-fit fullsweep");
    assert_eq!(
        collector.mature().capacity_words(),
        live,
        "the cap clamped the fresh space to exactly the live set"
    );
    assert_eq!(collector.stats().major_collections, 1);
    assert_eq!(collector.young().used_words(), 0);
    assert!(
        roots.iter().all(|r| collector.mature().contains_term(*r)),
        "every root was promoted into the exact-fit space"
    );
    assert_tuples_intact(&roots, &expected);

    // One word below the live set: the same corpus cannot fit — a typed error, nothing copied.
    let (mut collector, mut roots, expected) = capped_corpus(live - 1);
    let state = copying_state(&collector);
    let before = roots.clone();
    let err = collector
        .collect_major(&mut roots)
        .expect_err("one word below the live set");
    assert_heap_exhausted(&err, live);
    assert_eq!(
        copying_state(&collector),
        state,
        "nothing was copied and no counter moved"
    );
    assert_roots_untouched(&roots, collector.young(), &before);
    assert_tuples_intact(&roots, &expected);

    let again = collector
        .collect_major(&mut roots)
        .expect_err("still one word short");
    assert_eq!(format!("{again:?}"), format!("{err:?}"));
    assert_eq!(copying_state(&collector), state);
}

/// A nursery grown to its curated size, then filled to its exact word boundary with 2-word
/// cons cells, still collects — for both strategies.
fn grown_nursery_filled_to_its_exact_boundary(kind: MatureKind) {
    let policy = HeapPolicy {
        initial_young_words: 51,
        initial_mature_words: 233,
        ..HeapPolicy::default()
    };
    let mut c = any_for(kind, policy);

    let mut roots = Vec::new();
    let mut tuple_expected = Vec::new();
    for i in 0..2i64 {
        roots.push(tuple_in(c.young_mut(), i, 2 * i));
        tuple_expected.push((i, 2 * i));
    }

    // 1206 words rounds up the table to 1598 (a whole number of 2-word cons cells).
    let need = 1206usize;
    let collection = c.ensure_capacity(need, &mut roots).expect("grow");
    assert_eq!(collection.kind, CollectionKind::Minor, "{kind:?}");
    assert!(c.young().available_words() >= need, "{kind:?}");
    assert_eq!(c.policy().next_words(need, 0), 1598, "{kind:?}");
    let capacity = c.young().capacity_words();
    assert_eq!(capacity, 1598, "{kind:?}: the curated size covering 1206 words");
    assert_eq!(capacity % 2, 0, "the grown nursery divides into 2-word cons cells");

    // Fill the grown nursery to its exact boundary.
    let mut cons_expected = Vec::new();
    while c.young().available_words() >= 2 {
        let i = 1000 + cons_expected.len() as i64;
        let cell = c
            .young_mut()
            .heap_mut()
            .cons(make_small(i), make_small(2 * i))
            .expect("the cons cell fits the remaining nursery");
        roots.push(cell);
        cons_expected.push((i, 2 * i));
    }
    assert_eq!(
        c.young().available_words(),
        0,
        "{kind:?}: the grown nursery is filled to its exact boundary"
    );
    assert_eq!(cons_expected.len(), capacity / 2);

    // The collection of a nursery with zero words to spare still succeeds.
    let collection = c.collect(&mut roots).expect("collect the full nursery");
    assert_eq!(collection.kind, CollectionKind::Major, "{kind:?}");
    assert_eq!(c.young().used_words(), 0, "{kind:?}: the nursery is emptied");
    assert!(
        roots.iter().all(|r| in_mature(&c, *r)),
        "{kind:?}: every root was relocated into the mature generation"
    );
    assert_tuples_intact(&roots[..2], &tuple_expected);
    assert_conses_intact(&roots[2..], &cons_expected);
}

#[test]
fn a_nursery_grown_then_filled_to_its_exact_boundary_still_collects() {
    grown_nursery_filled_to_its_exact_boundary(MatureKind::Copying);
    grown_nursery_filled_to_its_exact_boundary(MatureKind::Immix);
}

// ---------------------------------------------------------------------------
// 5. A term written into the grown nursery's window is not lost
// ---------------------------------------------------------------------------

/// `ensure_capacity` grows the nursery and returns; a term the mutator writes in the window
/// between that grow and the next collection survives, together with the pre-existing set.
fn a_term_written_in_the_window_survives(kind: MatureKind) {
    let policy = HeapPolicy {
        initial_young_words: 51,
        initial_mature_words: 233,
        ..HeapPolicy::default()
    };
    let mut c = any_for(kind, policy);

    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..2i64 {
        roots.push(tuple_in(c.young_mut(), i, 2 * i));
        expected.push((i, 2 * i));
    }

    let need = 1024usize;
    let collection = c.ensure_capacity(need, &mut roots).expect("grow");
    assert_eq!(collection.kind, CollectionKind::Minor, "{kind:?}");
    assert!(c.young().available_words() >= need, "{kind:?}");
    assert!(
        roots.iter().all(|r| in_mature(&c, *r)),
        "{kind:?}: the grow's collection promoted the pre-existing live set"
    );

    // The window: the mutator writes a new term into the freshly grown nursery.
    let fresh = tuple_in(c.young_mut(), 7, 8);
    assert!(
        c.young().contains_term(fresh),
        "{kind:?}: the new term landed in the grown nursery"
    );
    assert!(!in_mature(&c, fresh), "{kind:?}: it starts out young");
    roots.push(fresh);
    expected.push((7, 8));

    // The next collection keeps both the new term and the pre-existing live set.
    let collection = c.collect(&mut roots).expect("collect after the window");
    assert_eq!(collection.kind, CollectionKind::Minor, "{kind:?}");
    assert_eq!(c.young().used_words(), 0, "{kind:?}: the nursery is emptied");
    assert!(
        roots.iter().all(|r| in_mature(&c, *r)),
        "{kind:?}: the new term and the old live set are both in mature"
    );
    assert_tuples_intact(&roots, &expected);
}

#[test]
fn a_term_written_into_the_grown_nursery_survives_the_next_collection() {
    a_term_written_in_the_window_survives(MatureKind::Copying);
    a_term_written_in_the_window_survives(MatureKind::Immix);
}
