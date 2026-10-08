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

//! **Automatic mature-generation selection** — the rule, its override, and the correctness
//! of both collectors across the break-even.
//!
//! `tests/immix_break_even.rs` re-measures where the mark-region mature generation starts to
//! pay off (see `src/selection.rs`); this file verifies that the *wiring* built on that
//! measurement behaves:
//!
//! 1. the automatic rule selects [`MatureKind::Copying`] below
//!    [`IMMIX_BREAK_EVEN_WORDS`] and [`MatureKind::Immix`] at/above it, both as a pure
//!    function and through [`AnyCollector::from_config`] on a real collector;
//! 2. the explicit override wins in **both** directions, regardless of size, and is reported
//!    as an override;
//! 3. a workload that **crosses** the threshold (a long-lived mature generation of eight
//!    whole Immix blocks) is collected correctly by **both** strategies: the reachable object
//!    sets are structurally identical, so the strategy choice changes no observable term
//!    semantics.
//!
//! Every corpus here is deterministic (no randomness), and every comparison is structural
//! (`beam_term::compare::eq`), not pointer-based.

use beam_gc::prelude::{
    select_mature, AnyCollector, CollectionKind, GcConfig, HeapPolicy, MatureKind, MatureSource,
    BLOCK_WORDS, IMMIX_BREAK_EVEN_BLOCKS, IMMIX_BREAK_EVEN_WORDS, LINES_PER_BLOCK, LINE_WORDS,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, AtomTable, Eterm};

/// A policy with `initial_mature_words` set to `mature_words` and everything else at the
/// crate defaults.
fn policy(mature_words: usize) -> HeapPolicy {
    HeapPolicy {
        initial_mature_words: mature_words,
        ..HeapPolicy::default()
    }
}

/// A policy for the threshold-crossing corpus: a mature generation of exactly the break-even
/// size, a nursery that holds the whole corpus, and a fullsweep cadence that forces both
/// minors and majors so both collectors are exercised on both collection kinds.
fn crossing_policy() -> HeapPolicy {
    let objects = IMMIX_BREAK_EVEN_BLOCKS * LINES_PER_BLOCK;
    HeapPolicy {
        fullsweep_after: 2,
        initial_young_words: objects * LINE_WORDS + 4096,
        initial_mature_words: IMMIX_BREAK_EVEN_WORDS,
        ..HeapPolicy::default()
    }
}

// ---------------------------------------------------------------------------
// 1. The rule selects by size
// ---------------------------------------------------------------------------

#[test]
fn the_rule_selects_copying_below_and_immix_at_or_above_the_break_even() {
    assert_eq!(IMMIX_BREAK_EVEN_BLOCKS, 8);
    assert_eq!(IMMIX_BREAK_EVEN_WORDS, 8 * BLOCK_WORDS);

    for words in [0, 1, BLOCK_WORDS, IMMIX_BREAK_EVEN_WORDS / 2, IMMIX_BREAK_EVEN_WORDS - 1] {
        assert_eq!(
            select_mature(words),
            MatureKind::Copying,
            "{words} words is below the break-even and must keep the copying path"
        );
    }
    for words in [IMMIX_BREAK_EVEN_WORDS, IMMIX_BREAK_EVEN_WORDS + 1, 4 * IMMIX_BREAK_EVEN_WORDS] {
        assert_eq!(
            select_mature(words),
            MatureKind::Immix,
            "{words} words is at or above the break-even and must select the mark-region path"
        );
    }
}

#[test]
fn from_config_follows_the_rule_for_a_real_collector() {
    // Below the break-even → the copying path.
    let below = GcConfig::auto(policy(IMMIX_BREAK_EVEN_WORDS - 1));
    let any = AnyCollector::from_config(&below).expect("collector");
    assert_eq!(any.kind(), MatureKind::Copying);
    assert!(matches!(any, AnyCollector::Copying(_)));
    assert_eq!(any.mature_choice().source, MatureSource::Automatic);

    // At the break-even → the mark-region path, chosen automatically.
    let at = GcConfig::auto(policy(IMMIX_BREAK_EVEN_WORDS));
    let any = AnyCollector::from_config(&at).expect("collector");
    assert_eq!(any.kind(), MatureKind::Immix);
    assert!(matches!(any, AnyCollector::Immix(_)));
    let choice = any.mature_choice();
    assert_eq!(choice.source, MatureSource::Automatic);
    assert_eq!(choice.mature_words, IMMIX_BREAK_EVEN_WORDS);
    assert_eq!(choice.break_even_words, IMMIX_BREAK_EVEN_WORDS);
    assert!(!choice.is_override());

    // The default configuration (233-word mature generation) still resolves to copying.
    let any = AnyCollector::from_config(&GcConfig::default()).expect("collector");
    assert_eq!(any.kind(), MatureKind::Copying);
}

// ---------------------------------------------------------------------------
// 2. The override wins, both ways
// ---------------------------------------------------------------------------

#[test]
fn an_explicit_override_beats_the_rule_in_both_directions() {
    // A large mature generation the rule would give to Immix — pinned back to Copying.
    let pinned_copying = GcConfig::auto(policy(4 * IMMIX_BREAK_EVEN_WORDS))
        .with_mature(MatureKind::Copying);
    assert_eq!(pinned_copying.mature(), MatureKind::Copying);
    let choice = pinned_copying.mature_choice();
    assert_eq!(choice.source, MatureSource::Explicit);
    assert!(choice.is_override());
    assert_eq!(choice.rule_kind(), MatureKind::Immix, "the rule wanted Immix here");
    let any = AnyCollector::from_config(&pinned_copying).expect("collector");
    assert_eq!(any.kind(), MatureKind::Copying);
    assert!(any.mature_choice().is_override());

    // A tiny mature generation the rule would give to Copying — pinned to Immix.
    let pinned_immix = GcConfig::auto(policy(8)).with_mature(MatureKind::Immix);
    assert_eq!(pinned_immix.mature(), MatureKind::Immix);
    let choice = pinned_immix.mature_choice();
    assert_eq!(choice.source, MatureSource::Explicit);
    assert!(choice.is_override());
    assert_eq!(choice.rule_kind(), MatureKind::Copying, "the rule wanted Copying here");
    let any = AnyCollector::from_config(&pinned_immix).expect("collector");
    assert_eq!(any.kind(), MatureKind::Immix);
    assert!(any.mature_choice().is_override());

    // The constructor shortcuts are just explicit pins.
    assert_eq!(GcConfig::copying(policy(1_000_000)).mature(), MatureKind::Copying);
    assert_eq!(GcConfig::immix(policy(1)).mature(), MatureKind::Immix);
}

// ---------------------------------------------------------------------------
// 3. Both collectors are correct across the break-even
// ---------------------------------------------------------------------------

/// Drives the threshold-crossing corpus through `config`'s collector and returns the
/// surviving roots (kept alive by the returned collector) plus the collection kinds it ran.
///
/// The corpus is a long-lived mature generation of exactly [`IMMIX_BREAK_EVEN_BLOCKS`] whole
/// Immix blocks (line-sized tuples, so the mark-region path's resident footprint matches the
/// copying path's), plus a nested list, plus deliberate garbage the collections must drop.
fn drive(config: &GcConfig) -> (AnyCollector, Vec<Eterm>, Vec<CollectionKind>) {
    let mut c = AnyCollector::from_config(config).expect("collector");
    let long_lived = IMMIX_BREAK_EVEN_BLOCKS * LINES_PER_BLOCK;

    // ---- long-lived live set, in the nursery ---------------------------------------------
    let mut roots: Vec<Eterm> = Vec::with_capacity(long_lived + 1);
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..long_lived as i64 {
            let elems: Vec<Eterm> = (0..(LINE_WORDS as i64 - 1))
                .map(|k| make_small(i * 100 + k))
                .collect();
            roots.push(heap.tuple(&elems).expect("a long-lived line-sized object"));
        }
    }
    // A nested structure, so the copier follows references inside a live object.
    let nested = {
        let heap = c.young_mut().heap_mut();
        let mut node = beam_term::prelude::NIL;
        for i in (0..8i64).rev() {
            node = heap.cons(make_small(i), node).expect("a list cell");
        }
        heap.tuple(&[node, roots[0]]).expect("a nested holder")
    };
    roots.push(nested);

    // ---- promote (minor), then grow the nursery and fullsweep ----------------------------
    let mut kinds = Vec::new();
    kinds.push(c.collect(&mut roots).expect("first collection").kind);

    // New roots after the first collection: a young->mature edge the next fullsweep must
    // resolve, plus garbage.
    {
        let heap = c.young_mut().heap_mut();
        roots.push(heap.tuple(&[roots[0], make_small(7)]).expect("a holder"));
        for i in 0..64i64 {
            let _ = heap.tuple(&[make_small(i), make_small(2 * i)]).expect("garbage");
        }
    }
    kinds.push(c.collect(&mut roots).expect("second collection").kind);

    // ---- a few more rounds, so both minors and majors run --------------------------------
    for round in 0..3i64 {
        {
            let heap = c.young_mut().heap_mut();
            let _ = heap.tuple(&[make_small(round), make_small(round + 1)]).expect("garbage");
        }
        kinds.push(c.collect(&mut roots).expect("later collection").kind);
    }

    (c, roots, kinds)
}

#[test]
fn both_collectors_produce_the_same_reachable_objects_across_the_threshold() {
    let config = crossing_policy();
    // The corpus is exactly at the break-even, so the automatic rule selects the mark-region
    // path; the same corpus is then driven through a pinned copying collector.
    let auto = GcConfig::auto(config.clone());
    let copying = GcConfig::copying(config.clone());
    assert_eq!(
        auto.mature(),
        MatureKind::Immix,
        "the crossing corpus must select the mark-region path automatically"
    );
    assert_eq!(copying.mature(), MatureKind::Copying);

    let (auto_collector, auto_roots, auto_kinds) = drive(&auto);
    let (copying_collector, copying_roots, copying_kinds) = drive(&copying);

    // The choice was really exercised: one collector is mark-region, the other copying.
    assert!(matches!(auto_collector, AnyCollector::Immix(_)));
    assert!(matches!(copying_collector, AnyCollector::Copying(_)));

    // Both ran the same collection sequence and kept the same number of roots ...
    assert_eq!(
        auto_kinds, copying_kinds,
        "the strategy choice must not change the collection sequence"
    );
    assert_eq!(auto_roots.len(), copying_roots.len(), "root count");
    assert!(
        auto_kinds.contains(&CollectionKind::Major) && auto_kinds.contains(&CollectionKind::Minor),
        "the corpus must exercise both minors and majors, got {auto_kinds:?}"
    );

    // ... and every root is the same term, read from the collected heaps. This is the
    // "equal reachable-object sets" evidence: a strategy change cannot alter what survives.
    let atoms = AtomTable::new();
    for (i, (a, b)) in auto_roots.iter().zip(&copying_roots).enumerate() {
        assert!(
            term_eq(*a, *b, &atoms),
            "root {i} differs between the automatic (immix) and pinned-copying collectors"
        );
    }

    // The long-lived set really is the whole break-even generation, not a truncated corpus.
    assert_eq!(auto_roots.len(), IMMIX_BREAK_EVEN_BLOCKS * LINES_PER_BLOCK + 2);
}

#[test]
fn the_strategy_choice_does_not_change_the_collectors_observables() {
    // `system_info`-style observables must be strategy-agnostic apart from the strategy
    // itself: both collectors report the same policy, the same minor-gc counter and the same
    // nursery size for the same corpus.
    let config = crossing_policy();
    let (auto, _, _) = drive(&GcConfig::auto(config.clone()));
    let (copying, _, _) = drive(&GcConfig::copying(config.clone()));
    let a = auto.mature_choice();
    let c = copying.mature_choice();
    assert_eq!(a.mature_words, c.mature_words, "same mature baseline the rule read");
    assert_eq!(a.break_even_words, c.break_even_words);
    assert_eq!(auto.kind(), MatureKind::Immix);
    assert_eq!(copying.kind(), MatureKind::Copying);
    assert_eq!(auto.minor_gcs(), copying.minor_gcs());
    assert_eq!(auto.young().capacity_words(), copying.young().capacity_words());
    // Both descriptions name a decision; the automatic one names the rule.
    assert!(a.describe().contains("automatic"));
    assert!(c.describe().contains("explicit"));
}
