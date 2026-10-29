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

//! **The live heap-sizing-flag seam** — `process_flag(min_heap_size |
//! min_bin_vheap_size | fullsweep_after, N)` applied to the policy of a *running* collector
//! (`AnyCollector::set_min_heap_words` / `set_min_bin_vheap_words` / `set_fullsweep_after`,
//! `src/flags.rs`, forwarding to the [`HeapPolicy`] setters).
//!
//! A process's [`HeapPolicy`] is built at spawn and every sizing decision reads it: the two
//! young-capacity rules, the fullsweep cadence and the binary virtual-heap floor. The flag seam
//! is a **live write** onto that policy, and the contract this file pins is the oracle's
//! *deferred* model — the flag's effect is visible at the next event that consumes the field,
//! never at the setter.
//!
//! # The contract (asserted below)
//!
//! 1. **The setter is a policy write and nothing else.** `heap_size_words`,
//!    `total_heap_size_words`, `minor_gcs`, `stats` and the nursery chain's length
//!    (`young_space_count`) are byte-identical across the call, no collection runs, and each
//!    setter returns the value it replaced (`HeapPolicy::default` answers `0` / `46_422` /
//!    `65_535`).
//! 2. **`min_heap_size` lands at the next collection.** The nursery's capacity does not move at
//!    the setter, and the first collection afterwards — a **minor** at the default cadence, or a
//!    pinned **fullsweep** in the twin — installs exactly `next_heap_size(N, 0)`, the figure the
//!    oracle reports (`process_flag(min_heap_size, 100_000)` → `heap_size` 121 536). It does not
//!    reset `minor_gcs` and does not force a fullsweep.
//! 3. **The floor bounds a later growth**, and a collection that follows gives the capacity back
//!    to the live set's target, floored at the new minimum — never below
//!    [`HeapPolicy::nursery_floor_words`].
//! 4. **Lowering the minimum shrinks the nursery back** onto the live set's target.
//! 5. **`fullsweep_after` moves the cadence only.** `0` makes every `collect` a
//!    [`CollectionKind::Major`] while `collect_kind(Minor)` still runs a minor; `3` gives exactly
//!    three minors and then a major, after which `minor_gcs()` is `0`; and a cadence at or below
//!    the live `minor_gcs` makes the very next `collect` a major **without** the setter having
//!    reset the counter.
//! 6. **`min_bin_vheap_size` lands at the next `BinVheap::record_collection`.** A live budget is
//!    not lifted retroactively, and after the refit an overhead between the old and the new limit
//!    is inside the budget.
//! 7. **The crate default is untouched** by any of it: a fresh `HeapPolicy::default()` still
//!    answers `0` / `46_422` / `65_535`.
//!
//! Every case runs for **both** mature strategies (`GcConfig::copying` and `GcConfig::immix`
//! through [`AnyCollector::from_config`]): the policy and the nursery chain are shared, so a
//! flag changes the same capacity on either. Every corpus is deterministic (an allocation
//! sequence, never a clock) and every figure asserted below is a **derived expression** — a
//! policy rule evaluated here — except one explicitly named oracle pin.

use beam_gc::prelude::{
    next_heap_size, AnyCollector, BinVheap, CollectionKind, GcConfig, HeapObservable, HeapPolicy,
    HeapSpace, MatureKind, DEFAULT_MIN_HEAP_WORDS,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Both mature strategies. The flag seam is a policy write, and both strategies read the one
/// [`HeapPolicy`] and size the one shared nursery from it.
const KINDS: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// A collector with `kind` **pinned** through the public configuration seam, so a case does not
/// depend on the automatic mature-generation rule.
fn any_for(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
    let config = match kind {
        MatureKind::Copying => GcConfig::copying(policy),
        MatureKind::Immix => GcConfig::immix(policy),
    };
    AnyCollector::from_config(&config).expect("collector")
}

/// A policy whose mature generation has room for the minors these cases run, so `collect` is
/// always the kind the cadence chose and never an escalation a case did not intend.
fn roomy_policy() -> HeapPolicy {
    HeapPolicy {
        initial_mature_words: 32_768,
        ..HeapPolicy::default()
    }
}

/// Builds a `{a, 2a}` tuple (3 words) in the nursery and hands it back as a one-element root set.
///
/// It is the whole live set of these cases: tiny, so a floored target is exactly the floor.
fn build_live(c: &mut AnyCollector, a: i64) -> Vec<Eterm> {
    let term = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(a), make_small(2 * a)])
        .expect("a 3-word nursery tuple");
    vec![term]
}

/// Writes 3-word tuples into `space` until it has **fewer than** `keep_free` words available —
/// "the mutator filled the current space", the state a growth event answers. The tuples are
/// unrooted garbage.
fn fill_to(space: &mut HeapSpace, keep_free: usize) -> usize {
    let filler = [make_small(1), make_small(2)];
    let mut tuples = 0;
    while space.available_words() >= keep_free {
        if space.heap_mut().tuple(&filler).is_err() {
            break;
        }
        tuples += 1;
    }
    tuples
}

/// Whether `term` is structurally identical to a freshly built `{a, 2a}` tuple — the witness is
/// written into a fresh [`beam_term::heap::Heap`] (kept alive for the comparison) and a fresh
/// [`AtomTable`] resolves atoms, so the check reads no raw heap word (the pattern
/// `tests/nursery_trim.rs` established).
fn is_pair(term: Eterm, a: i64) -> bool {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(64);
    let expected = heap
        .tuple(&[make_small(a), make_small(2 * a)])
        .expect("the witness tuple");
    term_eq(term, expected, &atoms)
}

/// The figure `process_flag(min_heap_size, 100_000)` makes the *next* collection install:
/// `process_flag/2`'s own rounding, `erts_next_heap_size(i, 0)` (`bif.c:2121-2126`).
fn raised_floor() -> usize {
    next_heap_size(100_000, 0)
}

// ---------------------------------------------------------------------------
// 2. `min_heap_size` lands at the next collection
// ---------------------------------------------------------------------------

/// The growth-table figure the oracle's measured row stands on: an unrounded 100 000-word
/// request is stored as `erts_next_heap_size(100_000, 0)` = **121 536**, and the collection that
/// follows reports exactly that as `heap_size`. The neighbouring rows the oracle measured are the
/// same lookup (`200 000` → 318 187, `500 000` → 514 838), so this one figure pins the rule rather
/// than an accident of the table's density.
#[test]
fn the_oracles_measured_min_heap_size_figure_is_the_growth_tables_next_entry() {
    assert_eq!(
        raised_floor(),
        121_536,
        "the oracle's `process_flag(min_heap_size, 100000)` -> `heap_size` 121536"
    );
    assert_eq!(next_heap_size(200_000, 0), 318_187, "oracle: 200000 -> 318187");
    assert_eq!(next_heap_size(500_000, 0), 514_838, "oracle: 500000 -> 514838");
    assert!(raised_floor() > 100_000);
}

/// A raise is a policy write; the **first collection afterwards** installs the floor, and it is a
/// minor at the default cadence (the flag forces no fullsweep and resets no counter).
#[test]
fn the_minimum_grows_the_heap_at_the_next_collection_not_at_the_setter() {
    let floor = raised_floor();
    for kind in KINDS {
        let mut c = any_for(kind, roomy_policy());
        let mut roots = build_live(&mut c, 1);
        let born = c.heap_size_words();

        let previous = c.set_min_heap_words(floor);
        assert_eq!(previous, 0, "{kind:?}: the policy had no floor");
        assert_eq!(
            c.heap_size_words(),
            born,
            "{kind:?}: the setter itself must not grow the heap"
        );
        assert_eq!(c.policy().min_heap_words, floor, "{kind:?}: stored verbatim");
        assert_eq!(c.minor_gcs(), 0, "{kind:?}: no collection ran");

        let collection = c.collect(&mut roots).expect("the first collection");
        assert_eq!(
            collection.kind,
            CollectionKind::Minor,
            "{kind:?}: the default cadence keeps the first collection a minor"
        );
        assert_eq!(
            c.heap_size_words(),
            floor,
            "{kind:?}: the minor installs the new minimum as the young capacity"
        );
        assert_eq!(c.young_space_count(), 1, "{kind:?}: one nursery space");
        assert_eq!(c.minor_gcs(), 1, "{kind:?}: exactly one minor, not a fullsweep");
        // The live set survived the relocation intact — the root was forwarded, not lost.
        assert!(
            is_pair(roots[0], 1),
            "{kind:?}: the rooted term must come through the collection unchanged"
        );
    }
}

/// The same raise on a **pinned fullsweep**: the floor is a property of the policy's sizing
/// rules, so the fullsweep's own (whole-heap, live-set-derived) nursery target is floored too.
#[test]
fn a_fullsweep_installs_the_same_floor_as_a_minor() {
    let floor = raised_floor();
    for kind in KINDS {
        let mut c = any_for(kind, roomy_policy());
        let mut roots = build_live(&mut c, 1);
        assert_eq!(c.set_min_heap_words(floor), 0);

        let collection = c
            .collect_kind(CollectionKind::Major, &mut roots)
            .expect("a pinned fullsweep");
        assert_eq!(
            collection.kind,
            CollectionKind::Major,
            "{kind:?}: collect_kind never escalates"
        );
        assert_eq!(
            c.heap_size_words(),
            floor,
            "{kind:?}: the fullsweep's live-set target is floored at the new minimum"
        );
        assert_eq!(c.minor_gcs(), 0, "{kind:?}: a fullsweep resets the counter");
    }
}

// ---------------------------------------------------------------------------
// 3. The floor bounds a later growth, and a collection gives the capacity back
// ---------------------------------------------------------------------------

/// A no-relocation growth after the raise never installs a capacity below the floor, and the
/// collection that follows sizes the nursery onto the live set's target — which, for a tiny live
/// set, **is** the floor (`trim_retired` installs it unconditionally, so the figure is exact and
/// not a banded third value).
#[test]
fn the_floor_bounds_a_later_growth_and_the_next_collection_lands_on_it() {
    let floor = raised_floor();
    for kind in KINDS {
        let mut c = any_for(kind, roomy_policy());
        let mut roots = build_live(&mut c, 3);
        assert_eq!(c.set_min_heap_words(floor), 0);

        // Outgrow the current space, then grow: the space the seam installs is floored.
        assert!(
            fill_to(c.young_mut(), 64) > 0,
            "{kind:?}: the current space must hold room to fill"
        );
        assert!(
            c.young().available_words() < 64,
            "{kind:?}: the request must outgrow the current space, or the growth is a no-op"
        );
        c.grow_nursery(64)
            .expect("the growth seam grants a small request");
        assert_eq!(c.young_space_count(), 2, "{kind:?}: a growth retires a space");
        assert!(
            c.heap_size_words() >= floor,
            "{kind:?}: the chain sums to at least the floor ({} < {floor})",
            c.heap_size_words()
        );
        assert!(
            c.young().capacity_words() >= floor,
            "{kind:?}: the space the growth installed is itself floored"
        );

        // The collection that follows gives the surplus back onto the live set's target.
        let collection = c.trim_retired(&mut roots).expect("the give-back collection");
        assert_eq!(
            collection.kind,
            CollectionKind::Minor,
            "{kind:?}: a tiny promotion is a minor"
        );
        assert_eq!(
            c.heap_size_words(),
            floor,
            "{kind:?}: the live set's target for a tiny live set is the floor"
        );
        assert_eq!(
            c.heap_size_words(),
            c.policy().nursery_floor_words(),
            "{kind:?}: the figure is the policy's own lower bound here"
        );
        assert!(!c.collection_debt(), "{kind:?}: the chain collapsed");
    }
}

// ---------------------------------------------------------------------------
// 4. Lowering the minimum shrinks back
// ---------------------------------------------------------------------------

/// Lowering the flag back and collecting installs the live set's target again — the nursery
/// shrinks, and never below [`HeapPolicy::nursery_floor_words`].
#[test]
fn lowering_the_minimum_shrinks_the_nursery_back_onto_the_live_set_target() {
    let floor = raised_floor();
    for kind in KINDS {
        let mut c = any_for(kind, roomy_policy());
        let mut roots = build_live(&mut c, 5);

        // Raise, and let the next collection install the floor.
        assert_eq!(c.set_min_heap_words(floor), 0);
        c.collect(&mut roots).expect("the collection that installs the floor");
        assert_eq!(c.heap_size_words(), floor, "{kind:?}: raised");

        // Lower it back to the spawn default: the setter is still a pure write.
        assert_eq!(c.set_min_heap_words(DEFAULT_MIN_HEAP_WORDS), floor);
        assert_eq!(
            c.heap_size_words(),
            floor,
            "{kind:?}: the setter does not resize the nursery"
        );

        // The root is already in the mature generation, so the young live set the next minor
        // evacuates is empty — a near-empty live set, its target floored by the new minimum.
        let collection = c.collect(&mut roots).expect("the shrinking collection");
        let expected = c
            .policy()
            .young_minor_target_words(collection.stats.words_copied);
        assert_eq!(
            collection.kind,
            CollectionKind::Minor,
            "{kind:?}: the cadence is unchanged"
        );
        assert_eq!(
            c.heap_size_words(),
            expected,
            "{kind:?}: the minor's live-set target"
        );
        assert!(
            c.heap_size_words() < floor,
            "{kind:?}: the capacity really shrank ({} vs {floor})",
            c.heap_size_words()
        );
        assert!(
            c.heap_size_words() >= c.policy().nursery_floor_words(),
            "{kind:?}: never below the policy's floor"
        );
    }
}

// ---------------------------------------------------------------------------
// 5. `fullsweep_after` moves the cadence, nothing else
// ---------------------------------------------------------------------------

/// `fullsweep_after = 0` is due immediately: every `collect` is a fullsweep, while
/// `collect_kind(Minor)` — which never chooses and never escalates — still runs a minor.
#[test]
fn fullsweep_after_zero_makes_every_collect_a_fullsweep() {
    for kind in KINDS {
        let mut c = any_for(kind, roomy_policy());
        let mut roots = build_live(&mut c, 7);

        assert_eq!(c.set_fullsweep_after(0), 65_535, "{kind:?}: the default cadence");
        assert_eq!(c.policy().fullsweep_after, 0);
        assert_eq!(c.minor_gcs(), 0, "{kind:?}: the setter resets nothing");

        for round in 0..3 {
            let collection = c.collect(&mut roots).expect("a cadence-driven collection");
            assert_eq!(
                collection.kind,
                CollectionKind::Major,
                "{kind:?} round {round}: `0 >= 0` is always due"
            );
            assert_eq!(c.minor_gcs(), 0, "{kind:?} round {round}: still no minor run");
        }

        let collection = c
            .collect_kind(CollectionKind::Minor, &mut roots)
            .expect("a pinned minor");
        assert_eq!(
            collection.kind,
            CollectionKind::Minor,
            "{kind:?}: collect_kind never escalates, whatever the cadence"
        );
        assert_eq!(c.minor_gcs(), 1, "{kind:?}: the pinned minor counted");
    }
}

/// `fullsweep_after = 3` gives exactly three minors and then a fullsweep, after which the
/// counter is back to `0`.
#[test]
fn fullsweep_after_three_gives_three_minors_then_a_fullsweep() {
    for kind in KINDS {
        let mut c = any_for(kind, roomy_policy());
        let mut roots = build_live(&mut c, 11);
        assert_eq!(c.set_fullsweep_after(3), 65_535);

        for round in 1..=3u32 {
            let collection = c.collect(&mut roots).expect("a cadence-driven collection");
            assert_eq!(collection.kind, CollectionKind::Minor, "{kind:?} round {round}");
            assert_eq!(c.minor_gcs(), round, "{kind:?} round {round}: the counter ran");
        }
        let collection = c.collect(&mut roots).expect("the due fullsweep");
        assert_eq!(collection.kind, CollectionKind::Major, "{kind:?}: 3 >= 3");
        assert_eq!(c.minor_gcs(), 0, "{kind:?}: the fullsweep reset the counter");
    }
}

/// A cadence lowered **below** the live `minor_gcs` makes the very next collection a fullsweep,
/// and the setter itself does not reset the counter — the same non-reset the oracle shows.
#[test]
fn a_cadence_below_the_live_counter_forces_a_fullsweep_without_resetting_it() {
    for kind in KINDS {
        let mut c = any_for(kind, roomy_policy());
        let mut roots = build_live(&mut c, 13);

        for round in 1..=2u32 {
            let collection = c.collect(&mut roots).expect("a default-cadence minor");
            assert_eq!(collection.kind, CollectionKind::Minor, "{kind:?} round {round}");
        }
        assert_eq!(c.minor_gcs(), 2, "{kind:?}: two minors behind us");

        assert_eq!(c.set_fullsweep_after(1), 65_535);
        assert_eq!(
            c.minor_gcs(),
            2,
            "{kind:?}: the setter does not reset the counter"
        );
        assert!(
            c.policy().fullsweep_due(c.minor_gcs()),
            "{kind:?}: 2 >= 1 is due"
        );

        let collection = c.collect(&mut roots).expect("the forced fullsweep");
        assert_eq!(collection.kind, CollectionKind::Major, "{kind:?}");
        assert_eq!(c.minor_gcs(), 0, "{kind:?}: the fullsweep reset it");
    }
}

// ---------------------------------------------------------------------------
// 6. `min_bin_vheap_size` lands at the next refit
// ---------------------------------------------------------------------------

/// A raised binary-virtual-heap floor does not lift the **live** budget: it lands at the next
/// [`BinVheap::record_collection`], and afterwards an overhead between the old and the new limit
/// is inside the budget.
#[test]
fn the_binary_vheap_floor_lands_at_the_next_refit_not_at_the_setter() {
    /// A floor far above the BEAM default (46 422 words) so both limits are unambiguous.
    const BIG: usize = 1_000_000;
    /// An overhead between the two limits: past the old budget, well inside the new one.
    const BETWEEN: usize = 500_000;

    for kind in KINDS {
        let mut c = any_for(kind, roomy_policy());
        let default_floor = c.policy().min_bin_vheap_words;
        assert_eq!(
            BinVheap::new(c.policy()).limit_words(),
            default_floor,
            "{kind:?}: a fresh budget is the policy's floor"
        );
        // The live budget this process already holds, and a second one to observe the deferral.
        let mut budget = BinVheap::new(c.policy());
        assert!(budget.triggered(BETWEEN), "{kind:?}: past the old budget");

        assert_eq!(c.set_min_bin_vheap_words(BIG), default_floor);
        assert_eq!(c.policy().min_bin_vheap_words, BIG, "{kind:?}: stored verbatim");
        assert_eq!(
            budget.limit_words(),
            default_floor,
            "{kind:?}: a raise does not lift the live budget"
        );
        assert!(
            budget.triggered(BETWEEN),
            "{kind:?}: the live budget is unchanged, so it still triggers"
        );

        // The refit that follows a collection applies the new floor.
        budget.record_collection(c.policy(), 0);
        assert_eq!(
            budget.limit_words(),
            BIG,
            "{kind:?}: the refit floors at the new minimum"
        );
        assert!(
            !budget.triggered(BETWEEN),
            "{kind:?}: an overhead between the old and the new limit is inside the budget"
        );
        assert!(!budget.triggered(BIG), "{kind:?}: exactly at the limit is not a trigger");
        assert!(budget.triggered(BIG + 1), "{kind:?}: one word past triggers");
        // A budget constructed later honours the live policy directly.
        assert_eq!(BinVheap::new(c.policy()).limit_words(), BIG, "{kind:?}");
    }
}

// ---------------------------------------------------------------------------
// 1 & 7. The setters are pure policy writes; the default is untouched
// ---------------------------------------------------------------------------

/// Nothing observable about the heap, the counters or the chain moves across a setter call, each
/// setter returns the value it replaced, and the policy is left as the old one with only the three
/// named fields replaced.
#[test]
fn the_setters_are_pure_policy_writes() {
    for kind in KINDS {
        let mut c = any_for(kind, roomy_policy());
        let mut roots = build_live(&mut c, 17);
        // One commit, so the counters, the capacity and the mature generation are non-trivial.
        c.collect(&mut roots).expect("the establishing collection");

        let heap = c.heap_size_words();
        let total = c.total_heap_size_words();
        let minors = c.minor_gcs();
        let stats = c.stats();
        let spaces = c.young_space_count();

        assert_eq!(c.set_min_heap_words(1_000), 0, "{kind:?}: previous minimum");
        assert_eq!(
            c.set_min_bin_vheap_words(1_000),
            46_422,
            "{kind:?}: previous min_bin_vheap_size"
        );
        assert_eq!(
            c.set_fullsweep_after(7),
            65_535,
            "{kind:?}: previous cadence"
        );

        assert_eq!(
            c.heap_size_words(),
            heap,
            "{kind:?}: no space was allocated, freed, resized or retired"
        );
        assert_eq!(c.total_heap_size_words(), total, "{kind:?}");
        assert_eq!(c.minor_gcs(), minors, "{kind:?}: no collection ran");
        assert_eq!(c.stats(), stats, "{kind:?}");
        assert_eq!(c.young_space_count(), spaces, "{kind:?}: the chain is untouched");
        // Only the three named policy fields moved.
        assert_eq!(
            *c.policy(),
            HeapPolicy {
                min_heap_words: 1_000,
                min_bin_vheap_words: 1_000,
                fullsweep_after: 7,
                ..roomy_policy()
            },
            "{kind:?}: the rest of the policy is inherited unchanged"
        );
    }
}

/// The crate default is a value constructor, so no live write can reach it: a fresh
/// `HeapPolicy::default()` still answers the C's three figures after every case above.
#[test]
fn the_default_policy_is_untouched_by_any_live_write() {
    let mut c = any_for(MatureKind::Copying, HeapPolicy::default());
    let mut roots = build_live(&mut c, 19);
    c.set_min_heap_words(raised_floor());
    c.set_min_bin_vheap_words(1_000_000);
    c.set_fullsweep_after(0);
    c.collect(&mut roots).expect("a collection under the live policy");

    let fresh = HeapPolicy::default();
    assert_eq!(fresh.min_heap_words, 0, "no floor");
    assert_eq!(fresh.min_bin_vheap_words, 46_422, "the C BIN_VH_MIN_SIZE");
    assert_eq!(fresh.fullsweep_after, 65_535, "the C (Uint16) -1");
    assert_eq!(fresh, HeapPolicy::default(), "the constructor is a fixed value");
}
