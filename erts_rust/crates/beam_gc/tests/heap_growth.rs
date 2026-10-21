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
//! 1. **Sustained allocation grows the mature generation; the nursery follows the live set.** A
//!    live set that keeps climbing forces the *mature* generation past its initial size, while
//!    the nursery's capacity follows it in **live-set target** steps — a fullsweep installs the
//!    **whole** live set's target ([`HeapPolicy::young_target_words`]) and a minor installs the
//!    **young** live set's target from the words it promoted
//!    ([`HeapPolicy::young_minor_target_words`]), never one growth-table step per collection,
//!    which is the runaway `HeapPolicy::young_after_collection` /
//!    `HeapPolicy::young_after_fullsweep` exist to prevent. Every live term stays structurally
//!    intact after every collection (`beam_term::compare::eq`).
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
use beam_gc::prelude::{
    AnyCollector, CollectionKind, Collector, GcConfig, HeapObservable, HeapPolicy, MatureKind,
    YOUNG_SHRINK_FACTOR,
};
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
// 1. Sustained allocation grows the mature generation; the nursery stays bounded
// ---------------------------------------------------------------------------

#[test]
fn sustained_allocation_grows_the_mature_generation_while_the_nursery_follows_the_live_set() {
    // Start tiny; the *live set* climbs every round, so the mature generation must grow past
    // its initial size to keep it alive. The nursery follows that live set at every collection —
    // a fullsweep through the whole live set's target (`HeapPolicy::young_target_words`), a minor
    // through the young live set it evacuated (`HeapPolicy::young_minor_target_words`) — so it
    // moves in *live-set target* steps, never one growth-table step per collection, which is the
    // creep the rule exists to prevent.
    let initial_young = 51;
    let initial_mature = 51;
    let policy = HeapPolicy {
        initial_young_words: initial_young,
        initial_mature_words: initial_mature,
        fullsweep_after: 65535,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy.clone()).expect("collector");
    assert_eq!(collector.young().capacity_words(), initial_young);
    assert_eq!(collector.mature().capacity_words(), initial_mature);

    let mut roots: Vec<Eterm> = Vec::new();
    let mut expected: Vec<(i64, i64)> = Vec::new();
    let mut young_caps: Vec<usize> = Vec::new();
    let mut targets: Vec<usize> = Vec::new();
    let mut kinds: Vec<CollectionKind> = Vec::new();
    let mut lives: Vec<usize> = Vec::new();

    // A **constant** per-round batch, so the allocation itself always fits the (birth-sized)
    // nursery; it is the *accumulated* live set that grows, round after round.
    const BATCH: i64 = 8;
    /// The final rounds whose capacity must be one single value (the live-set plateau).
    const TAIL: usize = 4;
    for round in 0..30i64 {
        for j in 0..BATCH {
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

        // Every root is a 3-word `{a, 2a}` tuple, so the round's live set is exact — and so is
        // the capacity the collector may install, derived from the rule of the kind that ran:
        // a **minor** sizes the nursery from the young live set it evacuated (its copy volume,
        // `stats.words_copied`) through `young_minor_target_words`, a **fullsweep** from the
        // whole live set it measured through `young_target_words`.
        let live = 3 * roots.len();
        let promoted = collection.stats.words_copied;
        let target = match collection.kind {
            CollectionKind::Minor => policy.young_minor_target_words(promoted),
            CollectionKind::Major => {
                assert_eq!(
                    promoted, live,
                    "round {round}: a fullsweep copies exactly the whole live set"
                );
                policy.young_target_words(promoted)
            }
        };
        let cap = collector.young().capacity_words();
        let previous = young_caps.last().copied().unwrap_or(initial_young);
        assert!(
            cap == previous || cap == target,
            "round {round}: the {:?} nursery capacity {cap} is neither the previous capacity \
             {previous} nor the live set's target {target} (whole live set {live}, promoted \
             {promoted})",
            collection.kind
        );
        // The floor is a real lower bound on every capacity the rule installs ...
        assert!(
            cap >= policy.nursery_floor_words(),
            "round {round}: {cap} words is below the policy's {} -word nursery floor",
            policy.nursery_floor_words()
        );
        // ... and a kept capacity is inside the hysteresis band around its target.
        assert!(
            cap <= YOUNG_SHRINK_FACTOR * target,
            "round {round}: {cap} words is past the {}x band around its {target}-word target",
            YOUNG_SHRINK_FACTOR
        );
        // The rule is a fixed point at the live set it measured, so a live set that stopped
        // moving could never climb a growth-table step per collection.
        let reapplied = match collection.kind {
            CollectionKind::Minor => policy.young_after_collection(cap, promoted),
            CollectionKind::Major => policy.young_after_fullsweep(cap, promoted),
        };
        assert_eq!(
            reapplied, cap,
            "round {round}: re-applying the rule at {cap} words / {promoted} live words answered \
             {reapplied}, so {cap} is not a fixed point"
        );
        young_caps.push(cap);
        targets.push(target);
        kinds.push(collection.kind);
        lives.push(live);
    }

    // The mature generation really did grow: the live set (240 tuples = 720 words) far
    // exceeds the 51-word initial size, so it had to expand.
    assert!(
        collector.mature().capacity_words() > initial_mature,
        "the mature generation grew ({initial_mature} -> {})",
        collector.mature().capacity_words()
    );
    assert!(
        collector.mature().used_words() > initial_mature,
        "the live set now occupies the grown mature generation"
    );

    // Every fullsweep in the run installed exactly the **whole** live set's target: that is the
    // collection that measures the live set exactly, so it is where the nursery learns the
    // workload's scale.
    for (round, kind) in kinds.iter().enumerate() {
        if *kind == CollectionKind::Major {
            assert_eq!(
                young_caps[round],
                policy.young_target_words(lives[round]),
                "round {round}: a fullsweep over {}-word live set settles on its target",
                lives[round]
            );
        }
    }
    assert!(
        *young_caps.iter().max().expect("non-empty")
            <= lives
                .iter()
                .map(|live| policy.young_target_words(*live))
                .max()
                .expect("non-empty"),
        "the nursery never exceeds the largest live set's target, got {young_caps:?} vs \
         {lives:?}"
    );

    // The last `TAIL` rounds are minors over the same constant batch, so they promote the same
    // young live set every time and settle on its one target: the nursery **plateaus** there
    // rather than stepping up once per collection (a 20 %-per-collection creep off 51 words would
    // be 51 * 1.2^30 ≈ 12 000 words by now).
    let tail = &young_caps[young_caps.len() - TAIL..];
    let tail_kinds = &kinds[kinds.len() - TAIL..];
    assert!(
        tail_kinds.iter().all(|k| *k == CollectionKind::Minor),
        "the trailing run must be minors over the constant batch, got {tail_kinds:?}"
    );
    assert!(
        tail.iter().all(|cap| *cap == tail[0]),
        "the nursery capacity must plateau over the last {TAIL} rounds, got {young_caps:?}"
    );
    // Every trailing round promotes exactly the constant batch, so every trailing round's target is
    // the same figure — a minor's `young_minor_target_words(BATCH * 3)`, one times the young live
    // set it evacuated.
    let tail_target = policy.young_minor_target_words((BATCH * 3) as usize);
    assert_eq!(
        *targets.last().expect("non-empty"),
        tail_target,
        "the last round is a minor over the same {} -word batch, so its target is \
         `young_minor_target_words({}) = {tail_target}`",
        BATCH * 3,
        BATCH * 3
    );
    // The plateau itself is **not** a target of the tail's own rule: the rule keeps any capacity
    // inside the hysteresis band around the target, and gives memory back only above
    // `YOUNG_SHRINK_FACTOR` times it. A plateau below the target would be grown onto it, so the
    // plateau must sit in `[target, 8 * target]` — and it is a live-set target this run *did*
    // install (the whole-live-set target of the fullsweep that escalated out of the mature
    // space's room), never a growth step of the trailing minors' own.
    assert!(
        tail[0] >= tail_target && tail[0] <= YOUNG_SHRINK_FACTOR * tail_target,
        "the plateau {} words must lie inside the {}x keep band around the {tail_target}-word \
         minor target, which is exactly why the minor rule keeps it instead of re-sizing",
        tail[0],
        YOUNG_SHRINK_FACTOR
    );
    assert!(
        targets.contains(&tail[0]),
        "the plateau {} must be a live-set target some round of this run installed, got \
         {targets:?}",
        tail[0]
    );
    // Every step the capacity took landed on a live-set target, and it took more than one: the
    // growth is real, and each step is a live-set step rather than an arbitrary growth-table step.
    // The steps are exactly the fullsweeps' own targets, in order: the initial fullsweep (round 0,
    // `young_target_words(<round-0 live set>)`) and every later escalation (`collect`'s
    // `minor_promotion_fits`, the collection whose promotion the mature space could no longer
    // receive). A minor never moves the capacity here — its 90-word target is below the capacity
    // every fullsweep installs, so it keeps it.
    let fullsweep_targets: Vec<usize> = kinds
        .iter()
        .enumerate()
        .filter(|(_, kind)| **kind == CollectionKind::Major)
        .map(|(round, _)| policy.young_target_words(lives[round]))
        .collect();
    let steps: Vec<usize> = core::iter::once(initial_young)
        .chain(young_caps.iter().copied())
        .collect::<Vec<usize>>()
        .windows(2)
        .filter(|w| w[0] != w[1])
        .map(|w| w[1])
        .collect();
    assert_eq!(
        steps, fullsweep_targets,
        "every capacity step must be a fullsweep's whole-live-set target, got {steps:?} against \
         {fullsweep_targets:?} ({young_caps:?})"
    );
    assert!(
        steps.len() > 1,
        "the capacity must really move more than once (the initial fullsweep and an escalation), \
         got {steps:?}"
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

    // The initial fullsweep, pinned: the first collection of a process has no old generation to
    // promote into, so it is a major (`Collector::has_old_generation`). Running it up front, on the
    // still-empty nursery, leaves the collector with an old generation in place and
    // `minor_since_major == 0`, so the grow below is the plain minor this test is about — and being
    // a major, it leaves the minor counter below starting from zero.
    collector
        .collect_kind(CollectionKind::Major, &mut [])
        .expect("initial fullsweep");

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
    // The ceiling has to leave room for *one* admissible step, so the two halves of the contract
    // — "an unreachable request is a typed refusal" and "a reachable one still grows" — are
    // exercised by the same policy. A ceiling at the 233-word growth-table step above the birth
    // nursery does that: `next_words(64) = 90` fits in the 182 words of budget the chain has
    // left, while 4096 is three growth-table steps away.
    let policy = HeapPolicy {
        initial_young_words: 51,
        max_heap_words: 233,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy.clone()).expect("collector");
    let mut roots = vec![live_tuple(&mut collector, 1, 2)];

    // 4096 words can never be reached under a 233-word ceiling: a typed error, no panic.
    let err = collector
        .ensure_capacity(4096, &mut roots)
        .expect_err("growth is capped");
    assert!(matches!(err, BeamError::Alloc(_)), "got {err:?}");
    assert_eq!(err.as_atom(), Some("out_of_memory"));
    // The refusal left the nursery exactly as it was: still the birth space, no growth.
    assert_eq!(collector.young().capacity_words(), 51);
    assert_eq!(collector.heap_size_words(), 51);

    // A small request under the same cap still succeeds, and the collector stays usable.
    //
    // The initial fullsweep first, kind-pinned: a fresh collector's first collection is a major
    // (there is no old generation to promote into yet), and pinning it here — after the refusal
    // above, which must see the untouched birth nursery — leaves the grow below a minor. The
    // fullsweep itself installs `young_after_fullsweep(51, 3) = young_target_words(3) = 90` words,
    // which is also the curated size the 64-word request wants (`next_words(64) = 90`), so the
    // grow needs to add nothing.
    collector
        .collect_kind(CollectionKind::Major, &mut roots)
        .expect("initial fullsweep");
    let collection = collector
        .ensure_capacity(64, &mut roots)
        .expect("fits under the cap");
    assert!(collector.young().available_words() >= 64);
    assert!(matches!(collection.kind, CollectionKind::Minor));
    // ... and it grew only as far as the request needed (`next_words(64) = 90`), never past the
    // ceiling the previous request ran into.
    assert_eq!(collector.young().capacity_words(), 90);
    assert!(
        collector.heap_size_words() <= policy.max_heap_words,
        "the chain's summed capacity {} must honour the {}-word ceiling",
        collector.heap_size_words(),
        policy.max_heap_words
    );

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

        // The initial fullsweep, pinned through the wrapper: a fresh collector's first collection
        // is a major (no old generation to promote into), so pinning it leaves the grow below the
        // minor this test forwards.
        c.collect_kind(CollectionKind::Major, &mut [])
            .expect("initial fullsweep");

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
