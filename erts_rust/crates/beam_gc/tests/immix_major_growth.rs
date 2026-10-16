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

//! **Growing the mark-region mature generation on a fullsweep** — the user-facing half of
//! [`GenImmixCollector`]'s capacity handling.
//!
//! A fullsweep of the Immix strategy first reclaims the mature generation **in place** and
//! only then promotes the nursery into it. When that promotion does not fit the reclaimed
//! region the collection must not simply fail: "a real growable heap must never turn a
//! working program into a failing one". This file pins the whole observable spectrum of
//! that contract, over one deterministic fragmented corpus (a one-block = 16-line mature
//! region holding `m` live lines, plus a nursery holding `k` more):
//!
//! * **A cap the promotion cannot fit still reports the typed heap exhaustion.** With
//!   `max_heap_words` pinned to the current capacity the region may not grow, and
//!   `collect_major` returns `BeamError::Alloc(AllocFailure { kind: Heap, .. })` **before any
//!   word is promoted**: every root is byte-identical, still resident in the nursery, the
//!   mature region keeps its capacity, and the failed fullsweep is not counted.
//! * **The identical corpus succeeds as soon as the policy may grow.** Raising/removing the
//!   cap turns the very same `collect_major` into an `Ok` that relocates the whole live set
//!   (nursery *and* mature) into a larger region, with every root structurally intact and
//!   `last_immix_stats()` cleared (a relocation is a plain copying collection).
//! * **An unbounded policy never fails a garbage-free fullsweep.** A sweep over corpus
//!   shapes asserts the reachability property behind the fix: a collection may only report
//!   exhaustion when the policy's ceiling forbids growing the region, never when the policy
//!   permits it.
//!
//! Every corpus is deterministic and every cross-collection comparison is structural
//! (`beam_term::compare::eq`). The *direct* test of the growth fallback itself — the
//! `grow_major` path `run_major` takes when its in-place reclaim leaves too few free lines —
//! lives in `src/immix_collector/grow_tests.rs`, because it needs the collector's private
//! state. Timings are never asserted.

use core::mem::size_of;

use beam_common::{AllocFailure, AllocKind, BeamError};
use beam_gc::prelude::{CollectionKind, GenImmixCollector, HeapPolicy, HeapSpace};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, AtomTable, Eterm};

/// One Immix block: the geometry of the corpus's mature region
/// (`immix::LINES_PER_BLOCK` lines of `immix::LINE_WORDS` words).
const LINES_PER_REGION: usize = 16;

/// The same region in words — the block-aligned capacity of a one-block mature space, and
/// therefore also the ceiling that pins a capped policy to it.
const ONE_BLOCK_WORDS: usize = 256;

/// Allocates a `{a, b}` tuple (three words, one Immix line) in `space`.
fn tuple_in(space: &mut HeapSpace, a: i64, b: i64) -> Eterm {
    space
        .heap_mut()
        .tuple(&[make_small(a), make_small(b)])
        .expect("the tuple fits the nursery it is written into")
}

/// Returns the typed allocation failure carried by `err`, if it carries one at all.
fn heap_failure(err: &BeamError) -> AllocFailure {
    match err {
        BeamError::Alloc(failure) => *failure,
        other => panic!("expected a typed BeamError::Alloc, got {other:?}"),
    }
}

/// Asserts `err` is the typed heap exhaustion for exactly `words` term words of a
/// `AllocKind::Heap` allocation (the only error a full heap may produce).
fn assert_heap_exhausted(err: &BeamError, words: usize) {
    let failure = heap_failure(err);
    assert_eq!(failure.kind, AllocKind::Heap, "the allocation class");
    assert_eq!(
        failure.requested,
        words * size_of::<Eterm>(),
        "the requested byte count ({words} words)"
    );
    assert_eq!(err.as_atom(), Some("out_of_memory"), "the BEAM error atom");
}

/// Asserts every root structurally equals an independently built `{a, b}` tuple.
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

/// The deterministic fragmented corpus plus its collector and roots.
struct Case {
    collector: GenImmixCollector,
    /// The `m` mature roots followed by the `k` nursery roots.
    roots: Vec<Eterm>,
    /// The `{a, b}` pair each root holds, in the same order.
    expected: Vec<(i64, i64)>,
    /// How many of `roots` lived in the mature region when the fullsweep started.
    mature_roots: usize,
}

/// Builds the corpus: `m` live tuples promoted into a **one-block** (16-line) mature region,
/// then `k` fresh live tuples left in the nursery, under a policy capped at `cap` words
/// (`0` = unbounded).
///
/// The promotion of the base is an explicit minor — it only has to fit the *initial* region,
/// so the cap never blocks the setup; the fullsweep under test is the caller's.
fn fragmented(m: usize, k: usize, cap: usize) -> Case {
    let policy = HeapPolicy {
        initial_young_words: 233,
        initial_mature_words: 1, // exactly one block, 16 lines
        max_heap_words: cap,
        ..HeapPolicy::default()
    };
    let mut collector = GenImmixCollector::from_policy(policy).expect("collector");

    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..m {
        let (a, b) = (i as i64, 2 * i as i64);
        roots.push(tuple_in(collector.young_mut(), a, b));
        expected.push((a, b));
    }
    if m != 0 {
        collector
            .collect_minor(&mut roots)
            .expect("the base fits the initial region");
    }
    assert_eq!(
        collector.mature().used_lines(),
        m,
        "one line per promoted tuple"
    );

    for i in 0..k {
        let (a, b) = (1000 + i as i64, 2 * i as i64);
        roots.push(tuple_in(collector.young_mut(), a, b));
        expected.push((a, b));
    }

    Case {
        collector,
        roots,
        expected,
        mature_roots: m,
    }
}

// ---------------------------------------------------------------------------
// 1. A cap the promotion cannot fit: typed exhaustion, nothing promoted
// ---------------------------------------------------------------------------

/// With the cap pinned to the one-block region, the 12-line occupancy plus an 8-line
/// promotion cannot fit after the in-place reclaim and may not grow: the fullsweep reports
/// the typed heap exhaustion and promotes nothing.
#[test]
fn a_capped_region_reports_a_typed_heap_exhaustion_and_promotes_nothing() {
    let mut case = fragmented(12, 8, ONE_BLOCK_WORDS);
    let region = case.collector.mature().capacity_words();
    assert_eq!(
        region, ONE_BLOCK_WORDS,
        "the cap keeps the one-block region"
    );
    assert_eq!(case.collector.mature().free_lines(), LINES_PER_REGION - 12);

    let nursery_before = case.roots[case.mature_roots..].to_vec();
    let before_young_used = case.collector.young().used_words();

    let err = case
        .collector
        .collect_major(&mut case.roots)
        .expect_err("the capped region cannot hold the promotion");

    // The 8-line nursery promotion is what did not fit, and that is what is reported.
    assert_heap_exhausted(&err, 8 * 16);

    // Nothing was promoted: no nursery root word was rewritten, every nursery root is still
    // resident in the (un-forwarded) nursery, and the region kept its capacity.
    assert_eq!(
        &case.roots[case.mature_roots..],
        &nursery_before[..],
        "no nursery root word was rewritten"
    );
    for root in &case.roots[case.mature_roots..] {
        assert!(
            case.collector.young().contains_term(*root),
            "every nursery root is still resident in the nursery"
        );
    }
    assert_eq!(
        case.collector.young().used_words(),
        before_young_used,
        "the nursery is untouched"
    );
    assert_eq!(
        case.collector.mature().capacity_words(),
        region,
        "the region did not grow"
    );
    assert_eq!(
        case.collector.mature().used_lines(),
        12,
        "the live mature lines are still there"
    );
    assert_eq!(
        case.collector.stats().major_collections,
        0,
        "a failed fullsweep is not counted"
    );
    assert_pairs_intact(&case.roots, &case.expected);
}

// ---------------------------------------------------------------------------
// 2. The same corpus, once the policy may grow: Ok, relocated, intact
// ---------------------------------------------------------------------------

/// The identical corpus under an unbounded policy: the fullsweep grows the mature region,
/// moves the whole live set into it, and every root stays structurally intact.
#[test]
fn the_identical_corpus_succeeds_once_the_policy_may_grow() {
    let mut case = fragmented(12, 8, 0);
    let region = case.collector.mature().capacity_words();
    assert_eq!(region, ONE_BLOCK_WORDS, "the initial region is one block");

    let stats = case
        .collector
        .collect_major(&mut case.roots)
        .expect("an unbounded policy grows the region instead of failing");

    assert!(
        case.collector.mature().capacity_words() > region,
        "the mark-region mature space grew"
    );
    assert_eq!(
        case.collector.young().used_words(),
        0,
        "the nursery was evacuated"
    );
    assert_eq!(
        case.collector.minor_gcs(),
        0,
        "the fullsweep resets the counter"
    );
    assert!(
        case.collector.last_immix_stats().is_none(),
        "the region was relocated, so no in-place reclaim ran"
    );
    assert_eq!(
        stats.objects_copied, 20,
        "12 mature + 8 nursery tuples moved"
    );
    assert_eq!(
        case.collector.stats().major_collections,
        1,
        "the successful fullsweep is counted"
    );
    for root in &case.roots {
        assert!(
            case.collector.mature().contains_term(*root),
            "every root was relocated into the grown region"
        );
    }
    assert_pairs_intact(&case.roots, &case.expected);
}

// ---------------------------------------------------------------------------
// 3. The cap is the only difference between the two outcomes
// ---------------------------------------------------------------------------

/// Sweeps `max_heap_words` over one corpus: the failure of case 1 is *the cap*, not the
/// corpus — every cap that leaves room to grow collects the same live set fine, and every
/// failure is a typed exhaustion whose policy forbids growing past the current capacity.
#[test]
fn the_failure_is_the_cap_not_the_corpus() {
    let mut outcomes = Vec::new();
    for cap in [0usize, 256, 512, 1024, 4096] {
        let mut case = fragmented(12, 8, cap);
        let region = case.collector.mature().capacity_words();
        let outcome = case.collector.collect_major(&mut case.roots);
        let ok = match &outcome {
            Ok(_) => {
                for root in &case.roots {
                    assert!(
                        case.collector.mature().contains_term(*root),
                        "cap {cap}: a successful fullsweep leaves every root in mature"
                    );
                }
                assert_pairs_intact(&case.roots, &case.expected);
                true
            }
            Err(err) => {
                // A failure may only come from a ceiling that cannot grow the region.
                assert!(
                    cap != 0 && cap <= region,
                    "cap {cap}: only a cap at/below the current capacity ({region}) may fail"
                );
                heap_failure(err);
                false
            }
        };
        outcomes.push((cap, ok));
    }

    // The sweep is non-vacuous: it really contains both outcomes.
    assert!(
        outcomes.iter().any(|(_, ok)| !ok),
        "the capped case must fail: {outcomes:?}"
    );
    assert!(
        outcomes.iter().any(|(_, ok)| *ok),
        "the growable cases must succeed: {outcomes:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. An unbounded policy never fails a garbage-free fullsweep
// ---------------------------------------------------------------------------

/// The reachability property behind the growth fallback: a fullsweep of a **garbage-free**
/// corpus (every nursery word is live) under an unbounded policy always collects. Every
/// corpus shape in the sweep — occupancy from empty to a full block, promotion from nothing
/// to half a block — therefore either fits the in-place region or is relocated into a larger
/// one; a typed exhaustion is never produced when growth is permitted.
#[test]
fn an_unbounded_policy_never_fails_a_garbage_free_fullsweep() {
    for m in [0usize, 4, 8, 12] {
        for k in [0usize, 4, 8, 12] {
            let mut case = fragmented(m, k, 0);
            match case.collector.collect_major(&mut case.roots) {
                Ok(_) => {}
                Err(err) => panic!("m={m} k={k}: an unbounded policy must not fail ({err:?})"),
            }
            assert_eq!(
                case.collector.young().used_words(),
                0,
                "m={m} k={k}: the nursery is emptied"
            );
            for root in &case.roots {
                assert!(
                    case.collector.mature().contains_term(*root),
                    "m={m} k={k}: every live tuple ended up in the mature region"
                );
            }
            assert_pairs_intact(&case.roots, &case.expected);
        }
    }
}

// ---------------------------------------------------------------------------
// 5. The same contract through the strategy-dispatching seam
// ---------------------------------------------------------------------------

/// The same contract through the strategy-dispatching seam: the mark-region strategy is
/// reached via [`AnyCollector`](beam_gc::prelude::AnyCollector), and a fullsweep on *every*
/// collection (`fullsweep_after = 0`) keeps both collections on the major arm.
///
/// Returns the kind of the **second** fullsweep — the one whose promotion cannot fit the
/// capped one-block region — after the first promoted the 12-tuple base.
fn two_full_sweeps_through_the_seam(cap: usize) -> std::result::Result<CollectionKind, BeamError> {
    use beam_gc::prelude::{AnyCollector, GcConfig, MatureKind};

    let policy = HeapPolicy {
        initial_young_words: 233,
        initial_mature_words: 1,
        max_heap_words: cap,
        fullsweep_after: 0,
        ..HeapPolicy::default()
    };
    let mut c = AnyCollector::from_config(&GcConfig::immix(policy)).expect("immix collector");
    assert_eq!(c.kind(), MatureKind::Immix, "the explicit strategy pin");

    let mut roots = Vec::new();
    let mut expected = Vec::new();
    for i in 0..12i64 {
        roots.push(tuple_in(c.young_mut(), i, 2 * i));
        expected.push((i, 2 * i));
    }
    let first = c.collect(&mut roots)?;
    assert_eq!(first.kind, CollectionKind::Major, "fullsweep_after = 0");
    for i in 0..8i64 {
        roots.push(tuple_in(c.young_mut(), 1000 + i, 2 * i));
        expected.push((1000 + i, 2 * i));
    }
    let second = c.collect(&mut roots)?;

    assert_eq!(
        c.young().used_words(),
        0,
        "the fullsweep emptied the nursery"
    );
    assert_pairs_intact(&roots, &expected);
    Ok(second.kind)
}

#[test]
fn the_strategy_seam_reports_the_same_capped_and_growable_outcomes() {
    let err = two_full_sweeps_through_the_seam(256)
        .expect_err("the capped region cannot hold the second promotion");
    heap_failure(&err);

    assert_eq!(
        two_full_sweeps_through_the_seam(0).expect("an unbounded policy collects"),
        CollectionKind::Major,
        "the second collection is still a fullsweep"
    );
}
