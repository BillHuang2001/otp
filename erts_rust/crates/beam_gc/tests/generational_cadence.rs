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

//! **The generational cadence** — the regression gate for how a fullsweep sizes the fresh mature
//! space.
//!
//! A fullsweep leaves behind a mature space sized from the **exact live word count** it
//! evacuates plus the **young heap's occupancy** — a whole nursery's worth of promotion (the C
//! `adjust_after_fullsweep`'s `need_after = (HEAP_TOP(p) - HEAP_START(p)) + need + …`,
//! `erl_gc.c:2072`; see [`Collector::collect_major`](beam_gc::Collector::collect_major) and
//! [`GenImmixCollector`](beam_gc::GenImmixCollector)). That is the whole point of a generational
//! collector: the fresh mature space must be able to **receive** the next collection's
//! promotion, or the very next collection cannot be a minor.
//!
//! This file is the regression gate for that property. It drives a churn workload — a bounded
//! live set rebuilt every round plus a batch of garbage — through [`AnyCollector`] and asserts,
//! per strategy, that the **minor cadence actually runs**:
//!
//! * [`GcStats::minor_collections`](beam_gc::GcStats) is **greater than zero**, and
//!   [`minor_gcs`](beam_gc::Collector::minor_gcs) strictly advances on a minor and resets to `0`
//!   at a fullsweep, so at least one minor occurs between two fullsweeps;
//! * survivors are **resident in the mature generation** (`mature_used_words() > 0`) and every
//!   live term is **structurally intact after every collection** (compared against an
//!   independently built witness — no `unsafe` heap read);
//! * the round's dead data is **reclaimed** (`words_reclaimed > 0`);
//! * the nursery is empty after every collection; and
//! * the **young and mature capacities converge** — each is a single constant across the
//!   measured second half of the run, so no sizing observable creeps without bound.
//!
//! Before the young occupancy was folded into the fullsweep's `need_after`, the fresh mature
//! space held only the live set, so it could never receive a promotion: every collection after
//! the first escalated to a fullsweep and `minor_collections` stayed at its initial value. The
//! **first** assertion below (a non-zero minor count) is exactly what that bug made fail.
//!
//! ## The policies
//!
//! The **copying** path runs under the **literal default** policy
//! ([`HeapPolicy::default`] — the 233-word young/mature initial sizes, `fullsweep_after` 65535,
//! no floor, no cap): the strongest form of the assertion, since a plain process runs exactly
//! that configuration.
//!
//! The **mark-region** path runs under a policy whose region is given promotion room. At the
//! default policy its region converges to 512 words (two whole 256-word blocks = 32 lines),
//! which holds the 20-line live set and leaves only 12 free lines — fewer than the 20 lines a
//! round promotes — so at the default **every** round escalates to a fullsweep and no minor
//! runs. That is a pre-existing consequence of the region's line granularity, not of the
//! fullsweep sizing rule, so it is documented here rather than asserted; the mark-region cadence
//! *is* pinned, under a region (4 whole blocks = 64 lines) that leaves the round's 20-line
//! promotion room.
//!
//! ## Determinism
//!
//! The corpus is deterministic (no randomness) and every assertion is on a deterministic count
//! (collection kinds, `minor_gcs`, capacities, word counts). No timing is measured or asserted.
//! Nothing here reads a raw heap word: integrity is checked with
//! [`beam_term::compare::eq`] against a witness in a fresh [`beam_term::heap::Heap`].

use beam_gc::prelude::{AnyCollector, CollectionKind, GcConfig, HeapPolicy, MatureKind};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, size_object, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// The churn corpus
// ---------------------------------------------------------------------------

/// The default nursery capacity (`HeapPolicy::default().initial_young_words`), in words.
const DEFAULT_NURSERY: usize = 233;

/// Words per `{a, 2a}` tuple: the frozen layout is `arity + 1` (a header plus two elements).
const TUPLE_WORDS: usize = 3;

/// Live tuples re-built **fresh** every round: a bounded, non-growing live set of 60 words.
const LIVE_TUPLES: i64 = 20;

/// Garbage tuples built every round and dropped immediately (never referenced): 150 words.
///
/// `LIVE_TUPLES + GARBAGE_TUPLES` is 70 tuples = 210 words, which fits the 233-word default
/// nursery, so every round's allocation succeeds before the round's collection.
const GARBAGE_TUPLES: i64 = 50;

/// Rounds run before measurement (warm-up to the sizing fixed point).
const WARMUP: usize = 20;

/// Long-running measured rounds: with a per-collection creep the capacities would be
/// astronomical by now, so a single constant across this second half is a genuine steady state.
const MEASURED: usize = 200;

/// The stable live set in words (the bounded graph the converged heap must cope with).
const LIVE_WORDS: usize = LIVE_TUPLES as usize * TUPLE_WORDS;

/// The lines a round's live set needs in a line-granular region: 20 one-line tuples.
const PROMOTION_LINES: usize = LIVE_TUPLES as usize;

/// The mark-region capacity the cadence runs under: four whole Immix blocks = 64 lines. That
/// holds the 20-line live set and still leaves 44 free lines, so a round's 20-line promotion
/// fits and the rounds between fullsweeps really are minors (see the module docs).
const IMMIX_REGION_WORDS: usize = 1024;

/// Every mature strategy: the copying space and the mark-region space.
const STRATEGIES: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The policy a strategy runs under: the literal [`HeapPolicy::default`] for the copying path,
/// and a promotion-room region for the mark-region path (see the module docs).
fn policy_for(kind: MatureKind) -> HeapPolicy {
    match kind {
        MatureKind::Copying => HeapPolicy::default(),
        MatureKind::Immix => HeapPolicy {
            initial_mature_words: IMMIX_REGION_WORDS,
            ..HeapPolicy::default()
        },
    }
}

/// Builds a `{a, 2a}` tuple (`TUPLE_WORDS` words) in `c`'s nursery.
fn tuple(c: &mut AnyCollector, a: i64) -> Eterm {
    c.young_mut()
        .heap_mut()
        .tuple(&[make_small(a), make_small(2 * a)])
        .expect("a 3-word nursery tuple")
}

/// Whether `term` is structurally identical to a freshly built `{a, 2a}` tuple.
///
/// The witness is written into a fresh [`beam_term::heap::Heap`] (kept alive for the
/// comparison) and a fresh [`AtomTable`] resolves atoms, so the check reads no raw heap word and
/// re-derives no term shape.
fn is_pair(term: Eterm, a: i64) -> bool {
    let atoms = AtomTable::new();
    let mut heap = beam_term::heap::Heap::new(64);
    let expected = heap
        .tuple(&[make_small(a), make_small(2 * a)])
        .expect("the witness tuple");
    term_eq(term, expected, &atoms)
}

/// The mature space's `(used_words, capacity_words)`, uniform across both strategies.
fn mature_words(c: &AnyCollector) -> (usize, usize) {
    match c {
        AnyCollector::Copying(x) => (x.mature().used_words(), x.mature().capacity_words()),
        AnyCollector::Immix(x) => (x.mature().used_words(), x.mature().capacity_words()),
    }
}

/// The min and max of a non-empty slice of counts.
fn min_max(values: &[usize]) -> (usize, usize) {
    (
        *values.iter().min().expect("non-empty"),
        *values.iter().max().expect("non-empty"),
    )
}

// ---------------------------------------------------------------------------
// The regression gate
// ---------------------------------------------------------------------------

/// **The generational cadence runs** at the default policy (copying) and at a policy whose
/// mark-region region leaves promotion room.
///
/// Each of `WARMUP + MEASURED` rounds allocates a fresh bounded live graph plus a garbage batch
/// and collects. The collection must really be a *generational* one: minors run, `minor_gcs`
/// advances per minor and resets per fullsweep, survivors accumulate in the mature generation
/// (and stay structurally intact), the round's garbage is reclaimed, and both capacities
/// converge to a constant across the measured second half.
#[test]
fn the_generational_cadence_runs() {
    for kind in STRATEGIES {
        let config = GcConfig::copying(policy_for(kind)).with_mature(kind);
        let mut c = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(c.kind(), kind, "{kind:?}: the configured strategy");
        assert_eq!(
            c.young().capacity_words(),
            DEFAULT_NURSERY,
            "{kind:?}: the collector starts at the default nursery"
        );

        let mut roots: Vec<Eterm> = Vec::new();
        let mut young_caps: Vec<usize> = Vec::with_capacity(MEASURED);
        let mut mature_caps: Vec<usize> = Vec::with_capacity(MEASURED);
        let mut minors = 0usize;
        let mut majors = 0usize;
        // The longest run of minors observed between two fullsweeps — the cadence itself.
        let mut minors_since_major = 0u32;
        let mut longest_minor_run = 0u32;

        for round in 0..(WARMUP + MEASURED) {
            // A fresh bounded live graph (the previous round's is now unreachable garbage) ...
            roots.clear();
            for i in 0..LIVE_TUPLES {
                roots.push(tuple(&mut c, round as i64 * 1000 + i));
            }
            // ... plus a garbage batch the collection must reclaim.
            for i in 0..GARBAGE_TUPLES {
                // Dropped immediately: never referenced from `roots`.
                let _ = tuple(&mut c, -i - 1);
            }

            let collection = c.collect(&mut roots).expect("collect");

            // ---- the cadence: `minor_gcs` advances per minor, resets per fullsweep ----
            match collection.kind {
                CollectionKind::Major => {
                    assert_eq!(
                        c.minor_gcs(),
                        0,
                        "{kind:?}: a fullsweep must reset `minor_gcs` (round {round})"
                    );
                    longest_minor_run = longest_minor_run.max(minors_since_major);
                    minors_since_major = 0;
                    majors += 1;
                }
                CollectionKind::Minor => {
                    minors_since_major += 1;
                    assert_eq!(
                        c.minor_gcs(),
                        minors_since_major,
                        "{kind:?}: `minor_gcs` must strictly advance on a minor (round {round})"
                    );
                    minors += 1;
                }
            }

            // ---- dead data is reclaimed every round ----
            assert!(
                collection.stats.words_reclaimed > 0,
                "{kind:?}: a fresh graph plus {} garbage words must reclaim something \
                 (round {round})",
                GARBAGE_TUPLES * TUPLE_WORDS as i64
            );

            // ---- the nursery is empty after every collection ----
            assert_eq!(
                c.young().used_words(),
                0,
                "{kind:?}: the nursery must be empty after every collection (round {round})"
            );

            // ---- survivors are resident in the mature generation ... ----
            let (used, _) = mature_words(&c);
            assert!(
                used > 0,
                "{kind:?}: the live set must be resident in the mature generation after every \
                 collection (round {round})"
            );

            // ---- ... and every live term is structurally intact ----
            for (i, &t) in roots.iter().enumerate() {
                let a = round as i64 * 1000 + i as i64;
                assert_eq!(
                    size_object(t),
                    TUPLE_WORDS,
                    "{kind:?}: live term {i} must keep its shape (round {round})"
                );
                assert!(
                    is_pair(t, a),
                    "{kind:?}: live term {i} must survive intact (round {round})"
                );
            }

            if round >= WARMUP {
                let (_, capacity) = mature_words(&c);
                young_caps.push(c.young().capacity_words());
                mature_caps.push(capacity);
            }
        }

        // ---- the generational cadence really ran ----
        // The failing case of the sizing bug this file guards: every collection after the first
        // escalated to a fullsweep, so `minors` stayed 0.
        assert!(
            minors > 0,
            "{kind:?}: the cadence must run minors, got {minors} minors / {majors} fullsweeps"
        );
        assert!(
            majors > 0,
            "{kind:?}: escalation must still reach a fullsweep, got {majors}"
        );
        assert!(
            longest_minor_run >= 1,
            "{kind:?}: at least one minor must occur between two fullsweeps, got \
             {longest_minor_run}"
        );

        // ---- the capacities converge (a single constant over the measured second half) ----
        assert_eq!(
            young_caps.len(),
            MEASURED,
            "{kind:?}: one sample per measured round"
        );
        let (young_min, young_max) = min_max(&young_caps);
        assert_eq!(
            young_min, young_max,
            "{kind:?}: the nursery capacity must converge to a constant, got min {young_min} != \
             max {young_max}"
        );
        let (cap_min, cap_max) = min_max(&mature_caps);
        assert_eq!(
            cap_min, cap_max,
            "{kind:?}: the mature capacity must converge to a constant, got min {cap_min} != \
             max {cap_max}"
        );

        // ---- non-degenerate: the converged heap covers the live set ----
        assert!(
            cap_min >= LIVE_WORDS,
            "{kind:?}: the converged mature capacity ({cap_min} words) must cover the live set \
             ({LIVE_WORDS} words)"
        );
        assert!(
            young_min >= DEFAULT_NURSERY,
            "{kind:?}: the nursery must not have degenerated below the default scale"
        );
        // The mark-region cadence needs the region to keep its promotion room.
        if kind == MatureKind::Immix {
            assert!(
                cap_min >= (PROMOTION_LINES + PROMOTION_LINES) * 16,
                "{kind:?}: the region ({cap_min} words) must leave at least the round's \
                 {PROMOTION_LINES}-line promotion room"
            );
        }

        eprintln!(
            "{kind:?}: the generational cadence ran — {minors} minors / {majors} fullsweeps \
             (longest minor run {longest_minor_run}); over the {MEASURED} measured rounds \
             (+{WARMUP} warm-up) young capacity = {young_min} words, mature capacity = \
             {cap_min} words, live set = {LIVE_WORDS} words"
        );
    }
}
