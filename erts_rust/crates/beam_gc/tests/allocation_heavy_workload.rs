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

//! **Allocation-heavy workload** — the measurement the existing evidence files do *not*
//! cover: does the mature-generation strategy pay off over a **long, allocation-driven run**
//! (many collections, a steady fullsweep cadence, a large volume of short-lived allocations)
//! rather than on a single favourable corpus?
//!
//! `immix_measure.rs`, `immix_workloads.rs`, `immix_selection_curve.rs` and
//! `immix_break_even.rs` all measure *purely long-lived* corpora at fixed shapes, and
//! `retained_vs_live.rs` measures one allocate/collect cycle. This file instead drives a
//! **process-like loop** of [`ROUNDS`] rounds through both strategies — the copying
//! [`AnyCollector::Copying`] and the mark-region [`AnyCollector::Immix`], each built from an
//! explicit `GcConfig::copying`/`GcConfig::immix` pin (asserted through
//! [`AnyCollector::kind`]) at a configured mature generation **above** the automatic rule's
//! break-even ([`IMMIX_BREAK_EVEN_WORDS`]), so the mark-region space is genuinely exercised.
//!
//! # The workload
//!
//! Every round allocates:
//!
//! * [`LONG_LIVED`] tuples of exactly one Immix line each (16 words) — the **bounded
//!   long-lived set**, allocated once in round 0 and kept alive across every round, so the
//!   first collection promotes it and it then occupies [`LONG_LIVED`] whole lines (exactly
//!   [`LONG_LIVED_BLOCKS`] full Immix blocks);
//! * [`TRANSIENTS`] small 3-word tuples — the **short-lived set**: reachable during the
//!   round's collection (so a minor *promotes* them) and dropped immediately afterwards, so
//!   they are **dead before the next collection**;
//! * [`GARBAGE`] further small tuples that are never referenced — the pure **nursery
//!   garbage** a real allocator produces, reclaimed by the round's collection.
//!
//! A fullsweep runs every [`CYCLE`]th collection (`fullsweep_after` = [`FULLSWEEP_AFTER`]),
//! so the mature generation must reclaim the dead promoted batches periodically — the point
//! where the two strategies diverge: a copying fullsweep re-copies the whole live set into a
//! fresh space, while the mark-region fullsweep reclaims **in place** and pins the
//! fully-live blocks.
//!
//! # What is asserted (deterministic counts only) versus printed
//!
//! Every assertion is on an **exact count** that follows from the corpus geometry:
//!
//! * the collection **cadence** (`round % CYCLE == FULLSWEEP_AFTER` is a fullsweep);
//! * the **objects/words copied per round** — the same live nursery set for both strategies,
//!   plus the mature live set on a copying fullsweep;
//! * the **nursery garbage reclaimed** by a minor, which is *identical* for both strategies
//!   (the allocation sequence is);
//! * the **total** objects/words moved over the run, and the exact `copying − immix`
//!   difference: [`MAJOR_ROUNDS`] fullsweeps × the long-lived set;
//! * the mark-region fullsweep's in-place accounting (every long-lived object **pinned**,
//!   **zero** objects evacuated, no block relocated);
//! * the **steady-state capacities** (nursery and mature constant over the measured half);
//! * the **peak resident** words of each space, i.e. the `FULLSWEEP_AFTER + 1` dead
//!   short-lived batches a generation accumulates before the next fullsweep;
//! * the surviving long-lived set after the last collection is **structurally equal** under
//!   both strategies (`beam_term::compare`) and still holds its original values.
//!
//! The **pause column is printed only, never asserted**: it is the median of a few forced
//! fullsweeps timed in-process on a single thread — a load-dependent signal, not a benchmark.
//!
//! Run `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc
//! --test allocation_heavy_workload -- --nocapture` to see the table; the measured numbers
//! are recorded in `MEASURED.md` beside this crate.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use beam_gc::prelude::{
    select_mature, AnyCollector, CollectionKind, GcConfig, HeapPolicy, ImmixStats, MatureKind,
    IMMIX_BREAK_EVEN_WORDS, LINE_WORDS,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, make_small, signed_value, size_object, AtomTable, Eterm,
};

/// Serialises the measurement: the pause column times a single-threaded in-process run, so no
/// other test in this binary may allocate while a sample is taken. A poisoned lock is
/// deliberately ignored — the guard still gives mutual exclusion.
static SERIAL: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Workload geometry (fixed; no randomness anywhere in this file)
// ---------------------------------------------------------------------------

/// The **bounded long-lived set**: [`LONG_LIVED`] tuples, alive across every round.
const LONG_LIVED: usize = 160;
/// Element count of a long-lived tuple: 15 elements + a header = 16 words = exactly one
/// [`LINE_WORDS`] line, so the set tiles whole Immix blocks with no line waste.
const LONG_LIVED_ARITY: usize = 15;
/// Frozen-layout footprint of the long-lived set (`arity + 1` words per tuple).
const LONG_LIVED_WORDS: usize = LONG_LIVED * (LONG_LIVED_ARITY + 1);
/// Immix lines the long-lived set occupies (one per tuple) and the whole blocks it fills.
const LONG_LIVED_LINES: usize = LONG_LIVED;
const LONG_LIVED_BLOCKS: usize = LONG_LIVED_LINES / 16;

/// The **short-lived set** allocated fresh every round and dropped right after the round's
/// collection — dead before the next collection.
const TRANSIENTS: usize = 512;
/// Element count of a transient tuple: 2 elements + a header = 3 words (one line).
const TRANSIENT_ARITY: usize = 2;
/// Frozen-layout footprint of one round's transient batch.
const TRANSIENT_WORDS: usize = TRANSIENTS * (TRANSIENT_ARITY + 1);
/// Immix lines one round's transient batch occupies when promoted (one per tuple).
const TRANSIENT_LINES: usize = TRANSIENTS;

/// The **pure nursery garbage** allocated per round (never referenced): the volume a real
/// allocator produces that dies before the round's own collection.
const GARBAGE: usize = 2048;
/// Frozen-layout footprint of one round's garbage.
const GARBAGE_WORDS: usize = GARBAGE * (TRANSIENT_ARITY + 1);

/// The fullsweep cadence: this many minors, then a fullsweep — so collection `r` is a major
/// iff `r % CYCLE == FULLSWEEP_AFTER`.
const FULLSWEEP_AFTER: u32 = 2;
/// Collections per major/minor cycle.
const CYCLE: usize = FULLSWEEP_AFTER as usize + 1;

/// Rounds run before measurement starts (allocator/carrier warm-up).
const WARMUP: usize = 200;
/// Long-running measured rounds — together a multi-second, multi-million-allocation run.
const MEASURED: usize = 7_800;
/// Rounds in the whole loop.
const ROUNDS: usize = WARMUP + MEASURED;

/// The nursery capacity both strategies start with (pinned: `min_heap_words` is the mature
/// size, so the bounded [`HeapPolicy::young_after_collection`] rule can never shrink it).
const NURSERY_WORDS: usize = 16_384;
/// The mature generation's capacity in words — 128 whole Immix blocks (2048 lines), enough
/// for the long-lived set plus the dead batches a fullsweep cycle accumulates. Also pinned as
/// `min_heap_words`, so it is a fixed point of `mature_after_fullsweep` for **both**
/// strategies and every fullsweep takes the in-place path (never a relocation).
const MATURE_WORDS: usize = 32_768;

/// Timed repetitions of the pause micro-benchmark.
const REPS: usize = 5;

// ---------------------------------------------------------------------------
// Policy and scaffolding
// ---------------------------------------------------------------------------

/// The heap-size policy both strategies share.
///
/// `min_heap_words` is pinned to the mature capacity so the mature generation is a fixed
/// point of [`HeapPolicy::mature_after_fullsweep`] (neither `should_grow` nor `should_shrink`
/// fires: the capacity never exceeds `4 * need_after`, and a shrink can never go below the
/// same floor) and the nursery is never resized — the run therefore reaches a steady state
/// after the first fullsweep and the capacities are an assertable constant.
fn policy() -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: FULLSWEEP_AFTER,
        initial_young_words: NURSERY_WORDS,
        initial_mature_words: MATURE_WORDS,
        min_heap_words: MATURE_WORDS,
        ..HeapPolicy::default()
    }
}

/// The configuration for `kind`, pinned explicitly.
fn config(kind: MatureKind) -> GcConfig {
    match kind {
        MatureKind::Copying => GcConfig::copying(policy()),
        MatureKind::Immix => GcConfig::immix(policy()),
    }
}

/// Whether round `round`'s collection is a fullsweep.
fn is_major_round(round: usize) -> bool {
    round % CYCLE == FULLSWEEP_AFTER as usize
}

/// The mature generation's occupancy, in the strategy's own unit (exact words for the
/// copying space, whole line-words for the mark-region one).
fn mature_used(c: &AnyCollector) -> usize {
    match c {
        AnyCollector::Copying(x) => x.mature().used_words(),
        AnyCollector::Immix(x) => x.mature().used_words(),
    }
}

/// The mature generation's capacity.
fn mature_capacity(c: &AnyCollector) -> usize {
    match c {
        AnyCollector::Copying(x) => x.mature().capacity_words(),
        AnyCollector::Immix(x) => x.mature().capacity_words(),
    }
}

/// The mature region's resident line count (`None` for the copying strategy), so the
/// line-granular rounding of its `used_words` is visible in the report.
fn mature_lines(c: &AnyCollector) -> Option<usize> {
    match c {
        AnyCollector::Copying(_) => None,
        AnyCollector::Immix(x) => Some(x.mature().used_lines()),
    }
}

/// Builds the bounded long-lived set in the nursery.
fn build_long_lived(c: &mut AnyCollector) -> Vec<Eterm> {
    let heap = c.young_mut().heap_mut();
    let mut roots = Vec::with_capacity(LONG_LIVED);
    for i in 0..LONG_LIVED {
        let base = (i as i64) * LONG_LIVED_ARITY as i64;
        let mut elems = [make_small(0); LONG_LIVED_ARITY];
        for (j, e) in elems.iter_mut().enumerate() {
            *e = make_small(base + j as i64);
        }
        roots.push(heap.tuple(&elems).expect("long-lived tuple"));
    }
    roots
}

/// Allocates one round's short-lived set (reachable: returned as roots) and its pure
/// nursery garbage (unreferenced: the returned terms are dropped immediately).
fn build_round(c: &mut AnyCollector, round: usize) -> Vec<Eterm> {
    let mut roots = Vec::with_capacity(TRANSIENTS);
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..TRANSIENTS {
            let v = (round as i64) * 10_000 - i as i64;
            let term = heap
                .tuple(&[make_small(v), make_small(-v)])
                .expect("transient tuple");
            roots.push(term);
        }
    }
    // The garbage is deliberately never stored: the bump pointer moves and the words are
    // unreachable from the roots, so the round's collection reclaims every one of them.
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..GARBAGE {
            let v = -(i as i64) - 1;
            let term = heap
                .tuple(&[make_small(v), make_small(v)])
                .expect("garbage tuple");
            let _ = term;
        }
    }
    roots
}

/// The `i`th element of a live tuple, as a small integer.
fn tuple_element(term: Eterm, i: usize) -> i64 {
    // SAFETY: `term` is a live boxed tuple this test (or a collection) built; element `i` is
    // inside it because every caller passes `i < arity`.
    let header = unsafe { *boxed_val(term) };
    assert!(is_arity_value(header), "not a tuple header");
    assert!(i < arityval(header), "element index inside the tuple");
    // SAFETY: element `i` lies within the `arity`-element tuple just validated.
    signed_value(unsafe { *boxed_val(term).add(1 + i) })
}

// ---------------------------------------------------------------------------
// The measurement
// ---------------------------------------------------------------------------

/// One round's measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sample {
    /// The collection's kind — the cadence is asserted against [`is_major_round`].
    kind: CollectionKind,
    /// Objects the collection copied (the nursery promotion, plus the mature set on a
    /// copying fullsweep).
    objects_copied: usize,
    /// Words the collection copied.
    words_copied: usize,
    /// Words the collection reclaimed.
    words_reclaimed: usize,
    /// The nursery's occupancy just before the collection (transients + garbage).
    young_used_before: usize,
    /// The mature generation's occupancy just before the collection — its high-water.
    mature_used_before: usize,
    /// The mature region's resident line count before the collection (`None` for the copying
    /// strategy, whose unit is exact words).
    mature_lines_before: Option<usize>,
    /// The mature generation's occupancy after the collection.
    mature_used_after: usize,
    /// The nursery's occupancy after the collection (must be zero).
    young_used_after: usize,
    /// The nursery capacity after the collection.
    young_capacity: usize,
    /// The mature capacity after the collection.
    mature_capacity: usize,
    /// The mark-region fullsweep's in-place accounting (the *last* one; only meaningful at a
    /// major — the copying strategy never produces one).
    immix: Option<ImmixStats>,
}

impl Sample {
    /// The words the heap spaces held just before the collection: the round's high-water.
    fn resident_before(&self) -> usize {
        self.young_used_before + self.mature_used_before
    }
}

/// One strategy's whole run.
struct Run {
    kind: MatureKind,
    /// Kept alive: every root points straight into these spaces.
    collector: AnyCollector,
    /// The surviving long-lived roots, in allocation order.
    long_lived: Vec<Eterm>,
    samples: Vec<Sample>,
    /// The **median** wall-clock duration of [`REPS`] forced fullsweeps, in microseconds.
    /// Printed only — never asserted.
    pause_us: u64,
}

impl Run {
    /// Objects and words the run copied, over every collection.
    fn moved(&self) -> (usize, usize) {
        self.samples
            .iter()
            .fold((0, 0), |(o, w), s| (o + s.objects_copied, w + s.words_copied))
    }

    /// Words the run reclaimed, over every collection.
    fn reclaimed(&self) -> usize {
        self.samples.iter().map(|s| s.words_reclaimed).sum()
    }

    /// Collections of each kind.
    fn counts(&self) -> (usize, usize) {
        let minors = self
            .samples
            .iter()
            .filter(|s| s.kind == CollectionKind::Minor)
            .count();
        (minors, self.samples.len() - minors)
    }

    /// The measured half's samples (the steady state).
    fn measured(&self) -> &[Sample] {
        &self.samples[WARMUP..]
    }

    /// The peak mature occupancy the run held.
    fn peak_mature(&self) -> usize {
        self.samples
            .iter()
            .map(|s| s.mature_used_before)
            .max()
            .expect("at least one round")
    }

    /// The peak words the heap spaces held at once (nursery + mature, pre-collection).
    fn peak_resident(&self) -> usize {
        self.samples
            .iter()
            .map(|s| s.resident_before())
            .max()
            .expect("at least one round")
    }

    /// The steady-state `(nursery, mature)` capacities over the measured half.
    fn steady_capacities(&self) -> (usize, usize) {
        let first = self.measured()[0];
        (first.young_capacity, first.mature_capacity)
    }

    /// The peak mature footprint as a strategy-specific string (whole line-words for the
    /// mark-region space, exact words for the copying one).
    fn footprint(&self) -> String {
        match self.kind {
            MatureKind::Copying => format!("{} words", self.peak_mature()),
            MatureKind::Immix => {
                let lines = self
                    .samples
                    .iter()
                    .filter_map(|s| s.mature_lines_before)
                    .max()
                    .unwrap_or(0);
                format!("{} line-words ({lines} lines) [peak]", self.peak_mature())
            }
        }
    }

    /// One report line.
    fn report(&self) -> String {
        let (minors, majors) = self.counts();
        let (objects, words) = self.moved();
        let (young, mature) = self.steady_capacities();
        format!(
            "  {:>8?}: collections {minors} minor / {majors} major | moved {objects} obj / \
             {words} words | reclaimed {} words | peak mature {} | peak resident {} words | \
             steady capacities young {young} / mature {mature} words | pause median {} µs",
            self.kind,
            self.reclaimed(),
            self.footprint(),
            self.peak_resident(),
            self.pause_us,
        )
    }
}

/// Runs the whole workload for `kind` and returns the run.
fn run_workload(kind: MatureKind) -> Run {
    let mut c = AnyCollector::from_config(&config(kind)).expect("collector");
    assert_eq!(c.kind(), kind, "the explicit pin must select {kind:?}");

    let mut long_lived: Vec<Eterm> = Vec::new();
    let mut samples: Vec<Sample> = Vec::with_capacity(ROUNDS);

    for round in 0..ROUNDS {
        if round == 0 {
            long_lived = build_long_lived(&mut c);
        }
        let round_roots = build_round(&mut c, round);

        let mut roots: Vec<Eterm> = Vec::with_capacity(long_lived.len() + round_roots.len());
        roots.extend_from_slice(&long_lived);
        roots.extend_from_slice(&round_roots);

        let young_used_before = c.young().used_words();
        let mature_used_before = mature_used(&c);
        assert_eq!(
            young_used_before,
            if round == 0 {
                LONG_LIVED_WORDS + TRANSIENT_WORDS + GARBAGE_WORDS
            } else {
                TRANSIENT_WORDS + GARBAGE_WORDS
            },
            "round {round}: the round allocated exactly its transients and garbage"
        );

        let collection = c.collect(&mut roots).expect("collect");
        assert_eq!(
            collection.kind == CollectionKind::Major,
            is_major_round(round),
            "round {round}: the fullsweep cadence (fullsweep_after = {FULLSWEEP_AFTER})"
        );

        // The relocation of the long-lived roots is written back; the round's transients are
        // then dropped, so they are dead before the next collection.
        let live = long_lived.len();
        long_lived.copy_from_slice(&roots[..live]);
        drop(round_roots);
        drop(roots);

        samples.push(Sample {
            kind: collection.kind,
            objects_copied: collection.stats.objects_copied,
            words_copied: collection.stats.words_copied,
            words_reclaimed: collection.stats.words_reclaimed,
            young_used_before,
            mature_used_before,
            mature_lines_before: mature_lines(&c),
            mature_used_after: mature_used(&c),
            young_used_after: c.young().used_words(),
            young_capacity: c.young().capacity_words(),
            mature_capacity: mature_capacity(&c),
            immix: match &c {
                AnyCollector::Copying(_) => None,
                AnyCollector::Immix(x) => x.last_immix_stats(),
            },
        });
    }

    let pause_us = measure_pause(&mut c, &mut long_lived);
    Run {
        kind,
        collector: c,
        long_lived,
        samples,
        pause_us,
    }
}

/// Times [`REPS`] forced fullsweeps over the workload's own shape and returns the **median**
/// in microseconds — the load-dependent pause signal, printed but never asserted.
fn measure_pause(c: &mut AnyCollector, long_lived: &mut [Eterm]) -> u64 {
    let mut samples: Vec<Duration> = Vec::with_capacity(REPS);
    for rep in 0..REPS {
        let round_roots = build_round(c, rep);
        let mut roots: Vec<Eterm> = Vec::with_capacity(long_lived.len() + round_roots.len());
        roots.extend_from_slice(long_lived);
        roots.extend_from_slice(&round_roots);

        let start = Instant::now();
        c.collect_major(&mut roots).expect("forced fullsweep");
        samples.push(start.elapsed());

        let live = long_lived.len();
        long_lived.copy_from_slice(&roots[..live]);
        drop(round_roots);
        drop(roots);
    }
    samples.sort_unstable();
    samples[REPS / 2].as_micros() as u64
}

// ---------------------------------------------------------------------------
// Assertions
// ---------------------------------------------------------------------------

/// The per-round counts both strategies must show, from the corpus geometry.
///
/// The **live nursery set is identical** for both strategies — the same long-lived promotion
/// on round 0 and the same transient batch on every round — so the copy counts differ only
/// where the strategies differ: a copying fullsweep also re-copies the whole mature live set
/// (the bounded long-lived set), which the mark-region fullsweep pins instead.
fn assert_per_round(run: &Run) {
    let label = format!("{:?}", run.kind);
    // The mature live set at a fullsweep is exactly the long-lived set: every earlier
    // transient batch is already dead (dropped before the next collection).
    for (round, s) in run.samples.iter().enumerate() {
        let includes_mature = run.kind == MatureKind::Copying && s.kind == CollectionKind::Major;
        let expected_objects = if round == 0 || includes_mature {
            LONG_LIVED + TRANSIENTS
        } else {
            TRANSIENTS
        };
        let expected_words = if round == 0 || includes_mature {
            LONG_LIVED_WORDS + TRANSIENT_WORDS
        } else {
            TRANSIENT_WORDS
        };
        assert_eq!(
            s.objects_copied, expected_objects,
            "{label}: round {round} copied objects"
        );
        assert_eq!(
            s.words_copied, expected_words,
            "{label}: round {round} copied words"
        );
        assert_eq!(
            s.young_used_after, 0,
            "{label}: round {round}: the nursery is emptied by every collection"
        );
        // A minor reclaims exactly the nursery garbage (identical for both strategies); a
        // fullsweep additionally reclaims the dead promoted batches.
        if s.kind == CollectionKind::Minor {
            assert_eq!(
                s.words_reclaimed, GARBAGE_WORDS,
                "{label}: round {round}: a minor reclaims exactly the round's nursery garbage"
            );
        } else {
            assert!(
                s.words_reclaimed > GARBAGE_WORDS,
                "{label}: round {round}: a fullsweep reclaims the garbage *and* the dead \
                 promoted batches ({} words)",
                s.words_reclaimed
            );
        }
    }

    // The mark-region fullsweep reclaims in place and pins the whole long-lived set: it
    // occupies whole Immix blocks, so no object is evacuated and no block is relocated.
    if run.kind == MatureKind::Immix {
        for (round, s) in run.samples.iter().enumerate() {
            if s.kind == CollectionKind::Major {
                let immix = s
                    .immix
                    .expect("round {round}: an in-place mark-region fullsweep");
                assert_eq!(
                    immix.objects_pinned, LONG_LIVED,
                    "round {round}: every long-lived object is pinned in its whole block"
                );
                assert_eq!(
                    immix.objects_evacuated, 0,
                    "round {round}: nothing is evacuated — the dead batches sit in fully-free \
                     blocks"
                );
                assert_eq!(
                    immix.words_pinned, LONG_LIVED_WORDS,
                    "round {round}: the pinned long-lived words"
                );
                assert!(
                    immix.words_reclaimed >= TRANSIENT_WORDS,
                    "round {round}: the dead promoted batches are reclaimed"
                );
            }
        }
    }
}

/// The steady-state, peak and total invariants of one run.
fn assert_run_invariants(run: &Run) {
    let label = format!("{:?}", run.kind);
    // Steady state: the capacities are constant over the measured half — the policy pins both
    // spaces (see [`policy`]), so no collection grows or shrinks them.
    for (i, s) in run.measured().iter().enumerate() {
        assert_eq!(
            (s.young_capacity, s.mature_capacity),
            (NURSERY_WORDS, MATURE_WORDS),
            "{label}: measured round {i}: the capacities are the steady state"
        );
        assert_eq!(
            s.young_used_after, 0,
            "{label}: measured round {i}: the nursery is empty after the collection"
        );
    }

    // Peak: a generation accumulates `fullsweep_after + 1` short-lived batches (the batch the
    // fullsweep itself promotes, plus one per minor) before the next fullsweep reclaims them.
    let batches = FULLSWEEP_AFTER as usize + 1;
    let expected_peak_mature_words = match run.kind {
        MatureKind::Copying => LONG_LIVED_WORDS + batches * TRANSIENT_WORDS,
        // The mark-region space charges every short-lived tuple a whole line.
        MatureKind::Immix => (LONG_LIVED_LINES + batches * TRANSIENT_LINES) * LINE_WORDS,
    };
    assert_eq!(
        run.peak_mature(),
        expected_peak_mature_words,
        "{label}: the peak mature occupancy"
    );
    if run.kind == MatureKind::Immix {
        let peak_lines = run
            .samples
            .iter()
            .filter_map(|s| s.mature_lines_before)
            .max()
            .expect("a mark-region space reports its line count");
        assert_eq!(
            peak_lines,
            LONG_LIVED_LINES + batches * TRANSIENT_LINES,
            "{label}: the peak resident line count"
        );
        assert_eq!(
            run.peak_mature(),
            peak_lines * LINE_WORDS,
            "{label}: the resident line-words agree with `used_words`"
        );
        let counters = match &run.collector {
            AnyCollector::Immix(x) => x.mature().counters(),
            AnyCollector::Copying(_) => unreachable!("the strategy was asserted above"),
        };
        assert_eq!(
            counters.used_words % LINE_WORDS,
            0,
            "{label}: mark-region occupancy is whole lines"
        );
    }
    // The peak resident high-water is the peak mature occupancy plus the round's live
    // nursery (its transients and its garbage).
    assert_eq!(
        run.peak_resident(),
        expected_peak_mature_words + TRANSIENT_WORDS + GARBAGE_WORDS,
        "{label}: the peak resident words"
    );

    // The run really did the allocation-heavy work: every round's garbage was reclaimed.
    assert_eq!(run.samples.len(), ROUNDS);
    assert!(
        run.reclaimed() >= ROUNDS * GARBAGE_WORDS,
        "{label}: every round's garbage was reclaimed: {} words",
        run.reclaimed()
    );
}

/// The cross-strategy verdict: identical live nursery work, a strictly cheaper mark-region
/// fullsweep, and a structurally identical surviving live set.
fn assert_cross_strategy(copying: &Run, immix: &Run, atoms: &AtomTable) {
    assert_eq!(copying.kind, MatureKind::Copying);
    assert_eq!(immix.kind, MatureKind::Immix);

    // The same collection schedule.
    let kinds: Vec<CollectionKind> = copying.samples.iter().map(|s| s.kind).collect();
    let immix_kinds: Vec<CollectionKind> = immix.samples.iter().map(|s| s.kind).collect();
    assert_eq!(kinds, immix_kinds, "the two strategies collect in lockstep");

    // The difference is exactly the long-lived set, re-copied once per copying fullsweep.
    let (c_objects, c_words) = copying.moved();
    let (i_objects, i_words) = immix.moved();
    let (_, majors) = copying.counts();
    assert!(c_objects > i_objects);
    assert_eq!(
        c_objects - i_objects,
        majors * LONG_LIVED,
        "the copying path re-copies the whole long-lived set at each fullsweep"
    );
    assert_eq!(c_words - i_words, majors * LONG_LIVED_WORDS);

    // The mark-region path never moves the long-lived objects: it pins them. It copies the
    // long-lived set exactly once (round 0's promotion) and one transient batch per round.
    assert_eq!(i_objects, LONG_LIVED + ROUNDS * TRANSIENTS);

    // The surviving live set is the same graph, holding the same values, under both.
    assert_eq!(copying.long_lived.len(), LONG_LIVED);
    assert_eq!(immix.long_lived.len(), LONG_LIVED);
    for (i, (a, b)) in copying.long_lived.iter().zip(&immix.long_lived).enumerate() {
        assert!(term_eq(*a, *b, atoms), "long-lived root {i} differs across strategies");
    }
    let words: usize = copying.long_lived.iter().map(|&t| size_object(t)).sum();
    assert_eq!(
        words, LONG_LIVED_WORDS,
        "the independently measured footprint of the surviving set"
    );
    for (i, &root) in immix.long_lived.iter().enumerate() {
        assert_eq!(size_object(root), LONG_LIVED_ARITY + 1, "root {i} is one line");
        let base = (i as i64) * LONG_LIVED_ARITY as i64;
        assert_eq!(tuple_element(root, 0), base, "root {i}: first element");
        assert_eq!(
            tuple_element(root, LONG_LIVED_ARITY - 1),
            base + LONG_LIVED_ARITY as i64 - 1,
            "root {i}: last element"
        );
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// The allocation-heavy workload, measured through **both** mature-generation strategies.
///
/// Run with `-- --nocapture` for the table (also recorded in `MEASURED.md`).
#[test]
fn the_allocation_heavy_workload_is_measured_through_both_strategies() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // The configured mature generation is above the automatic rule's break-even, so the
    // mark-region strategy is a real (not nominal) option at this configuration.
    assert!(
        MATURE_WORDS >= IMMIX_BREAK_EVEN_WORDS,
        "the workload's mature generation must exceed the Immix break-even"
    );
    assert_eq!(select_mature(MATURE_WORDS), MatureKind::Immix);

    let copying = run_workload(MatureKind::Copying);
    let immix = run_workload(MatureKind::Immix);

    assert_per_round(&copying);
    assert_per_round(&immix);
    assert_run_invariants(&copying);
    assert_run_invariants(&immix);
    assert_cross_strategy(&copying, &immix, &AtomTable::new());

    let (c_minors, c_majors) = copying.counts();
    let (i_minors, i_majors) = immix.counts();
    let (c_objects, c_words) = copying.moved();
    let (i_objects, i_words) = immix.moved();

    println!("\n=== allocation-heavy workload: copying vs mark-region mature generation ===");
    println!(
        "workload: {ROUNDS} rounds × ({TRANSIENTS} short-lived {TRANSIENT_ARITY}-element tuples \
         → live for the round's collection then dropped, + {GARBAGE} unreferenced garbage \
         tuples), a bounded {LONG_LIVED}-tuple long-lived set ({LONG_LIVED_WORDS} words = \
         {LONG_LIVED_BLOCKS} whole Immix blocks) alive throughout, and a fullsweep every {CYCLE}rd \
         collection"
    );
    println!(
        "short-lived objects allocated over the run: {} tuples ({} words)",
        ROUNDS * (TRANSIENTS + GARBAGE),
        ROUNDS * (TRANSIENT_WORDS + GARBAGE_WORDS)
    );
    println!("{}", copying.report());
    println!("{}", immix.report());
    println!(
        "  derived: copying − immix = {c_majors} fullsweeps × the long-lived set ({LONG_LIVED} \
         obj / {LONG_LIVED_WORDS} words) = {} obj / {} words moved less by the mark-region path",
        c_objects - i_objects,
        c_words - i_words
    );
    println!(
        "  derived: moved words per strategy = {c_words} (copying: {c_minors} minor + \
         {c_majors} fullsweep) vs {i_words} (immix: {i_minors} minor + {i_majors} fullsweep)"
    );
    println!(
        "  derived: peak mature footprint copying {} words vs immix {} — the line-granular \
         mark-region space charges every {TRANSIENT_ARITY}-element tuple a whole \
         {LINE_WORDS}-word line",
        copying.peak_mature(),
        immix.peak_mature()
    );
    println!(
        "  caveat: the pause column is a single-threaded in-process median of {REPS} forced \
         fullsweeps — a load-dependent signal, NOT a benchmark and NOT reproducible run-to-run"
    );
    println!(
        "  pause medians: copying {} µs vs immix {} µs",
        copying.pause_us, immix.pause_us
    );
}
