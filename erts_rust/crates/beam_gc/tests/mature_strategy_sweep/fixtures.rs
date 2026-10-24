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

//! **The class shapes of `tests/mature_strategy_sweep.rs`** — each workload class's geometry,
//! its [`HeapPolicy`], and the driver that turns (class, configuration, N) into one measured
//! [`Tally`].
//!
//! Included from the test-crate root with `#[path]` (a bare `mod fixtures;` would be resolved in
//! `tests/` — a crate root's submodules live in the root's own directory — and Cargo would then
//! build this file as a test target of its own). Nothing here is `#[test]`-annotated: this module
//! is the *shape* half of the sweep, the harness half is the parent file.
//!
//! # The three classes
//!
//! Every class is **one collection per round**, and the round's whole allocation is sized up front
//! so admission can be pre-checked. That is what makes the class a *shape* rather than a race
//! between the mutator and the strategy's own post-collection sizing: the two arms allocate the
//! identical sequence of terms and run the identical number of collections, so the paired
//! difference between them is the mature generation's policy and nothing else.
//!
//! | class | live set | per-round churn | the shape |
//! |---|---|---|---|
//! | [`CHURN`] (C1) | [`CHURN_PERSISTENT`] tuples, **bounded** | 128 promoted transients + 576 never-referenced tuples | low live set, high churn |
//! | [`GROWTH`] (C2) | grows by [`GROWTH_KEEP`] tuples per round, **kept forever** | 250 never-referenced tuples | high live set that grows monotonically |
//! | [`COPY_OUT`] (C3-shaped) | a [`TABLE_ROWS`]-row stand-in "table", bounded | one structural copy of the table | copy-out only, no other churn |
//!
//! Both arms are born from the **identical** [`policy`] for a class (the strategy is the only
//! difference, and it is an explicit [`GcConfig::copying`]/[`GcConfig::immix`] pin), and every
//! class's mature generation is configured **at or above**
//! [`IMMIX_BREAK_EVEN_WORDS`](beam_gc::IMMIX_BREAK_EVEN_WORDS) so the mark-region space is
//! genuinely exercised rather than nominally pinned.
//!
//! # The geometry is a fixed point, deliberately
//!
//! Each class's `initial_young_words` is chosen so that the policy's own post-collection rules
//! **keep** it: the young live set a minor evacuates gives a
//! `young_minor_target_words` far below the capacity, and the capacity is inside the
//! `YOUNG_SHRINK_FACTOR` (8×) keep band around that target, so no collection resizes the nursery
//! and the class's cadence does not drift. The figures are stated per class below; each is a
//! derived consequence of the class's own live set, never a tuned constant.
//!
//! Nothing here reads a collector internal: the shapes are built with the public
//! [`AnyCollector`] API only, so this module is also the dependent's view of the crate.

use beam_gc::prelude::{
    AnyCollector, CollectionKind, GcConfig, GcStats, HeapFootprint, HeapObservable, HeapPolicy,
    ImmixCounters, MatureKind, OffHeap,
};
use beam_term::compare::eq as term_eq;
use beam_term::heap::Heap as WitnessHeap;
use beam_term::prelude::{make_small, size_object, AtomTable, Eterm};

/// C1 — the low-live, high-churn class.
pub const CHURN: &str = "churn";
/// C2 — the high-live, monotonically growing class.
pub const GROWTH: &str = "growth";
/// C3-shaped — the copy-out-only class (the `ets_copy` allocation geometry, with no ETS).
pub const COPY_OUT: &str = "copy_out";

/// Every class this sweep measures, in report order.
pub const CLASSES: &[&str] = &[CHURN, GROWTH, COPY_OUT];

/// The fullsweep cadence **both** arms of **every** class share.
///
/// One shared value is part of the "identical geometry, one strategy apart" discipline: the
/// emulator default (65 535) would never reach a fullsweep in a run this short, so the mature
/// generation — the thing being measured — would not be exercised at all.
pub const FULLSWEEP_AFTER: u32 = 2;

// ---------------------------------------------------------------------------------------------
// C1 — churn: a small bounded live set under a large volume of short-lived allocation
// ---------------------------------------------------------------------------------------------

/// The bounded long-lived set: tuples alive across the whole run (`32 × 4 = 128` words).
pub const CHURN_PERSISTENT: usize = 32;
/// Element count of a long-lived tuple (`3 elements + header = 4 words`).
pub const CHURN_PERSISTENT_ARITY: usize = 3;
/// Frozen-layout footprint of the long-lived set.
pub const CHURN_PERSISTENT_WORDS: usize = CHURN_PERSISTENT * (CHURN_PERSISTENT_ARITY + 1);

/// The short-lived set allocated fresh every round and dropped right after that round's
/// collection — reachable **at** the collection, so a minor promotes it (`128 × 3 = 384` words).
pub const CHURN_TRANSIENTS: usize = 128;
/// Element count of a transient tuple (`2 elements + header = 3 words`).
pub const CHURN_TRANSIENT_ARITY: usize = 2;
/// Frozen-layout footprint of one round's transient batch.
pub const CHURN_TRANSIENT_WORDS: usize = CHURN_TRANSIENTS * (CHURN_TRANSIENT_ARITY + 1);

/// The pure nursery garbage allocated per round and never referenced (`576 × 3 = 1 728` words).
pub const CHURN_GARBAGE: usize = 576;
/// Frozen-layout footprint of one round's garbage.
pub const CHURN_GARBAGE_WORDS: usize = CHURN_GARBAGE * (CHURN_TRANSIENT_ARITY + 1);

/// The nursery the churn class is born with: `4 096` words, comfortably above one round's
/// `2 112` words of allocation.
///
/// It is a fixed point of both post-collection rules: a minor's target is
/// `next_words(512 + 64) = 610` and `4 096 < 8 × 610 = 4 880`, so the keep branch holds the
/// capacity; the first fullsweep's target is `next_words(2 × 1 280 + 64) = 4 185`, which the
/// collapse installs, and every later minor's `610` keeps `4 185` inside its own band too.
pub const CHURN_YOUNG_WORDS: usize = 4_096;
/// The mature generation the churn class is configured with: `4 096` words = 16 whole Immix
/// blocks, i.e. **above** the automatic rule's 2 048-word break-even, and identical for both arms.
pub const CHURN_MATURE_WORDS: usize = 4_096;

// ---------------------------------------------------------------------------------------------
// C2 — growth: a live set that grows monotonically, so the mature generation must grow
// ---------------------------------------------------------------------------------------------

/// The tuples each round allocates **and keeps**: the live set's growth step (`48 × 3 = 144`
/// words per round).
pub const GROWTH_KEEP: usize = 48;
/// Element count of a kept tuple.
pub const GROWTH_KEEP_ARITY: usize = 2;
/// Frozen-layout footprint of one round's kept batch.
pub const GROWTH_KEEP_WORDS: usize = GROWTH_KEEP * (GROWTH_KEEP_ARITY + 1);

/// The nursery garbage each round allocates and drops (`250 × 3 = 750` words): it is what keeps
/// the nursery filling round after round while the live set grows only slowly, so the class
/// collects at a steady cadence instead of on a geometric one.
pub const GROWTH_GARBAGE: usize = 250;
/// Frozen-layout footprint of one round's garbage.
pub const GROWTH_GARBAGE_WORDS: usize = GROWTH_GARBAGE * (GROWTH_KEEP_ARITY + 1);

/// The nursery the growth class is born with: `1 598` words, above one round's `894` words.
///
/// A minor evacuates the round's `144` kept words, so its target is `next_words(208) = 233` and
/// `1 598 < 8 × 233 = 1 864`, so the keep branch holds the capacity — the nursery does not grow
/// with the live set here, which is what forces the **mature** generation to carry the growth.
pub const GROWTH_YOUNG_WORDS: usize = 1_598;
/// The mature generation the growth class is configured with, above the 2 048-word break-even.
pub const GROWTH_MATURE_WORDS: usize = 4_096;

// ---------------------------------------------------------------------------------------------
// C3-shaped — copy-out: a persistent payload copied out once per round, nothing else
// ---------------------------------------------------------------------------------------------

/// The rows of the `Vec`-backed stand-in "table": the class's whole live set (`64 × 3 = 192`
/// words), held as roots across every round.
pub const TABLE_ROWS: usize = 64;
/// Element count of a table row.
pub const TABLE_ROW_ARITY: usize = 2;
/// Frozen-layout footprint of the table.
pub const TABLE_WORDS: usize = TABLE_ROWS * (TABLE_ROW_ARITY + 1);

/// The nursery the copy-out class is born with: `987` words, above one round's `192`-word copy.
///
/// A minor evacuates only the round's copy, so its target is `next_words(256) = 376` and
/// `987 < 8 × 376 = 3 008`, so the keep branch holds the capacity.
pub const COPY_OUT_YOUNG_WORDS: usize = 987;
/// The mature generation the copy-out class is configured with, above the 2 048-word break-even.
pub const COPY_OUT_MATURE_WORDS: usize = 4_096;

// ---------------------------------------------------------------------------------------------
// Per-class accessors
// ---------------------------------------------------------------------------------------------

/// Whether `class` is one of this module's classes.
pub fn is_known_class(class: &str) -> bool {
    CLASSES.contains(&class)
}

/// The words one round of `class` allocates — the quantity its heap admission must reserve.
pub fn batch_words(class: &str) -> usize {
    match class {
        CHURN => CHURN_TRANSIENT_WORDS + CHURN_GARBAGE_WORDS,
        GROWTH => GROWTH_KEEP_WORDS + GROWTH_GARBAGE_WORDS,
        COPY_OUT => TABLE_WORDS,
        other => panic!("no batch geometry for class `{other}`"),
    }
}

/// The heap-size policy `class` is driven under — **identical for both arms** of the class.
///
/// The only levers set are the class's own initial geometries and the shared cadence; every
/// bound stays at the emulator default (`min_heap_words` 0, `max_heap_words` 0 = unbounded), so a
/// collection may size both generations from the live set without a ceiling in play.
pub fn policy(class: &str) -> HeapPolicy {
    let (initial_young_words, initial_mature_words) = match class {
        CHURN => (CHURN_YOUNG_WORDS, CHURN_MATURE_WORDS),
        GROWTH => (GROWTH_YOUNG_WORDS, GROWTH_MATURE_WORDS),
        COPY_OUT => (COPY_OUT_YOUNG_WORDS, COPY_OUT_MATURE_WORDS),
        other => panic!("no policy for class `{other}`"),
    };
    HeapPolicy {
        fullsweep_after: FULLSWEEP_AFTER,
        initial_young_words,
        initial_mature_words,
        ..HeapPolicy::default()
    }
}

/// The configuration of one arm of `class`: the class's policy with `kind` **pinned**.
///
/// An explicit pin is the whole point — [`GcConfig::copying`]/[`GcConfig::immix`] set
/// `MatureSelection::Explicit`, so the arm cannot be silently resolved by the automatic rule.
pub fn config(class: &str, kind: MatureKind) -> GcConfig {
    let policy = policy(class);
    match kind {
        MatureKind::Copying => GcConfig::copying(policy),
        MatureKind::Immix => GcConfig::immix(policy),
    }
}

// ---------------------------------------------------------------------------------------------
// The measurement of one arm of one class
// ---------------------------------------------------------------------------------------------

/// Everything one (class, arm) measurement reports.
///
/// The counters come from three places, each named where it is read: the collector's
/// [`GcStats`] (`words_copied`/`objects_copied`/`words_reclaimed`, summed per collection so the
/// sum can be checked against the cumulative figure), the live mature/young spaces sampled after
/// every collection (`mature_peak_*`, the class's unit), and the final
/// [`HeapFootprint`] of the collector plus an **empty** off-heap roster.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Tally {
    /// Rounds the class ran (= the collections it drove; asserted).
    pub rounds: usize,
    /// Collections observed, counted by this driver (`minors + majors`).
    pub collections: usize,
    /// Minor collections, tallied from each `Collection::kind`.
    pub minors: u64,
    /// Fullsweeps, tallied from each `Collection::kind`.
    pub majors: u64,
    /// `words_copied`, **summed over the per-collection stats**.
    pub copied_words: usize,
    /// `objects_copied`, summed the same way.
    pub copied_objects: usize,
    /// `words_reclaimed`, summed the same way.
    pub reclaimed_words: usize,
    /// The collector's own cumulative [`GcStats`] (must equal the three sums above).
    pub stats: GcStats,
    /// Peak mature occupancy **after** a collection, in the strategy's own unit (exact words for
    /// the copying space, whole `LINE_WORDS` line-words for the mark-region one).
    pub mature_peak_words: usize,
    /// The same peak in whole Immix **lines** (`0` for the copying arm, whose unit is words).
    pub mature_peak_lines: usize,
    /// The mature generation's capacity once the run ended.
    pub mature_capacity_words: usize,
    /// The nursery **chain-summed** capacity once the run ended
    /// ([`HeapObservable::heap_size_words`], the figure [`HeapFootprint`] accounts).
    pub young_capacity_words: usize,
    /// The **current** nursery space's capacity once the run ended (a growth retires a space no
    /// collection drains, so this and the chain sum differ exactly while a growth is un-collapsed).
    pub young_current_capacity_words: usize,
    /// Peak nursery occupancy, sampled **before** each collection.
    pub young_peak_used_words: usize,
    /// The final [`HeapFootprint`] over an empty off-heap roster.
    pub footprint: HeapFootprint,
    /// The mark-region space's own accounting view (`None` for the copying arm).
    pub immix: Option<ImmixCounters>,
    /// How many roots the live set ended with.
    pub live_roots: usize,
    /// Its **independently measured** frozen-layout footprint (`Σ size_object`).
    pub live_words: usize,
    /// Whether every live root matched its independently built witness.
    pub intact: bool,
}

impl Tally {
    /// Folds one collection in: its kind, its own (per-collection) stats, and a sample of the
    /// live spaces **after** it ran.
    fn record(&mut self, kind: CollectionKind, stats: GcStats, collector: &AnyCollector) {
        self.collections += 1;
        match kind {
            CollectionKind::Minor => self.minors += 1,
            CollectionKind::Major => self.majors += 1,
        }
        self.copied_words += stats.words_copied;
        self.copied_objects += stats.objects_copied;
        self.reclaimed_words += stats.words_reclaimed;
        self.mature_peak_words = self.mature_peak_words.max(collector.mature_used_words());
        if let AnyCollector::Immix(immix) = collector {
            self.mature_peak_lines = self.mature_peak_lines.max(immix.mature().used_lines());
        }
    }

    /// The live mature generation's unit, for the printed row.
    pub fn mature_unit(&self) -> &'static str {
        if self.immix.is_some() {
            "line-words"
        } else {
            "words"
        }
    }

    /// The whole run's collections, as the collector itself reports them.
    pub fn reported_collections(&self) -> u64 {
        self.stats.minor_collections + self.stats.major_collections
    }
}

// ---------------------------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------------------------

/// Runs `class` on `collector` for `rounds` rounds and answers the measurement.
///
/// **One collection per round, and nothing else collects.** The round's admission is
/// [`AnyCollector::grow_nursery`] — a growth moves no term, so it is legal for a caller holding
/// every root, and it keeps the cadence a property of the class's geometry (`rounds` collections)
/// instead of a function of the strategy's own post-collection sizing. A `should_collect`-keyed
/// door would collect twice in the rounds where the post-collection nursery is below the batch,
/// and how often that happens is exactly the quantity under measurement.
pub fn run(class: &'static str, collector: &mut AnyCollector, rounds: usize) -> Tally {
    let atoms = AtomTable::new();
    let batch = batch_words(class);
    let mut tally = Tally::default();
    let mut roots = setup(class, collector);
    for round in 0..rounds {
        if collector.young().available_words() < batch {
            collector.grow_nursery(batch).unwrap_or_else(|error| {
                panic!("{class}: round {round}: the nursery cannot admit the batch: {error:?}")
            });
        }
        let survivors = allocate_round(class, collector, round, &mut roots, &atoms);
        tally.young_peak_used_words = tally
            .young_peak_used_words
            .max(collector.young().used_words());
        let collection = collector
            .collect(&mut roots)
            .unwrap_or_else(|error| panic!("{class}: round {round}: the collection failed: {error:?}"));
        tally.record(collection.kind, collection.stats, collector);
        // The round's short-lived terms die here; a growth class keeps everything it allocated.
        roots.truncate(survivors);
    }

    let stats = collector.stats();
    let footprint = HeapFootprint::of(collector, &OffHeap::new());
    let immix = match collector {
        AnyCollector::Immix(immix) => Some(immix.mature().counters()),
        AnyCollector::Copying(_) => None,
    };
    tally.rounds = rounds;
    tally.stats = stats;
    tally.mature_capacity_words = collector.mature_capacity_words();
    tally.young_capacity_words = collector.heap_size_words();
    tally.young_current_capacity_words = collector.young().capacity_words();
    tally.footprint = footprint;
    tally.immix = immix;
    tally.live_roots = roots.len();
    tally.live_words = roots.iter().map(|&root| size_object(root)).sum();

    // The live set is checked against an **independently built** witness the collector never saw.
    tally.intact = verify(class, &roots, &atoms);
    assert!(
        tally.intact,
        "{class}: every live root must be structurally equal to its independently built witness"
    );
    tally
}

/// Builds the class's **persistent** root set in the nursery.
fn setup(class: &str, collector: &mut AnyCollector) -> Vec<Eterm> {
    match class {
        CHURN => {
            let mut roots = Vec::with_capacity(CHURN_PERSISTENT);
            let heap = collector.young_mut().heap_mut();
            for index in 0..CHURN_PERSISTENT {
                roots.push(tuple(heap, index, CHURN_PERSISTENT_ARITY));
            }
            roots
        }
        GROWTH => Vec::with_capacity(GROWTH_KEEP),
        COPY_OUT => {
            let mut roots = Vec::with_capacity(TABLE_ROWS);
            let heap = collector.young_mut().heap_mut();
            for index in 0..TABLE_ROWS {
                roots.push(tuple(heap, index, TABLE_ROW_ARITY));
            }
            roots
        }
        other => panic!("no setup for class `{other}`"),
    }
}

/// Allocates one round of `class` into `roots` and answers **how many roots survive the round**.
///
/// The survivors are the persistent prefix (`CHURN`'s long-lived set, `COPY_OUT`'s table) plus,
/// for the growth class, everything kept so far — its live set only ever grows.
fn allocate_round(
    class: &str,
    collector: &mut AnyCollector,
    round: usize,
    roots: &mut Vec<Eterm>,
    atoms: &AtomTable,
) -> usize {
    match class {
        CHURN => {
            let survivors = roots.len();
            // The garbage is deliberately never stored: the bump pointer moves and the words are
            // unreachable from the roots, so the round's collection reclaims every one of them.
            {
                let heap = collector.young_mut().heap_mut();
                for index in 0..CHURN_GARBAGE {
                    let base = -(index as i64) - 1;
                    let term = heap
                        .tuple(&[make_small(base), make_small(base)])
                        .expect("churn: garbage tuple");
                    let _ = term;
                }
            }
            let heap = collector.young_mut().heap_mut();
            for index in 0..CHURN_TRANSIENTS {
                let value = (round as i64) * 10_000 - index as i64;
                let term = heap
                    .tuple(&[make_small(value), make_small(-value)])
                    .expect("churn: transient tuple");
                roots.push(term);
            }
            survivors
        }
        GROWTH => {
            {
                let heap = collector.young_mut().heap_mut();
                for index in 0..GROWTH_GARBAGE {
                    let value = -(index as i64) - 1;
                    let term = heap
                        .tuple(&[make_small(value), make_small(value)])
                        .expect("growth: garbage tuple");
                    let _ = term;
                }
            }
            let heap = collector.young_mut().heap_mut();
            for slot in 0..GROWTH_KEEP {
                let kept = roots.len();
                debug_assert_eq!(kept, round * GROWTH_KEEP + slot);
                roots.push(tuple(heap, kept, GROWTH_KEEP_ARITY));
            }
            // Nothing dies: the growth class keeps every tuple it has ever kept.
            roots.len()
        }
        COPY_OUT => {
            let survivors = roots.len();
            // The copy-out: rebuild every row from the row **stored in the table**, so the
            // round's allocation is a structural copy of a persistent payload and nothing else.
            let mut copies = Vec::with_capacity(TABLE_ROWS);
            {
                let heap = collector.young_mut().heap_mut();
                for row in 0..TABLE_ROWS {
                    let stored = roots[row];
                    let elements = [
                        make_small(tuple_element(stored, 0)),
                        make_small(tuple_element(stored, 1)),
                    ];
                    copies.push(heap.tuple(&elements).expect("copy_out: copied row"));
                }
            }
            // The copy is a faithful copy of the table, row by row — checked before it is dropped.
            for (row, &copied) in copies.iter().enumerate() {
                assert!(
                    term_eq(copied, roots[row], atoms),
                    "copy_out: round {round}: copied row {row} must equal the stored row"
                );
            }
            roots.extend_from_slice(&copies);
            survivors
        }
        other => panic!("no round allocation for class `{other}`"),
    }
}

/// Allocates a tuple whose elements are `index, index + 1, …, index + arity - 1`.
fn tuple(heap: &mut beam_alloc::Heap, index: usize, arity: usize) -> Eterm {
    let base = index as i64;
    let mut elements = vec![make_small(0); arity];
    for (offset, element) in elements.iter_mut().enumerate() {
        *element = make_small(base + offset as i64);
    }
    heap.tuple(&elements)
        .expect("the class's persistent tuple allocates")
}

/// Whether every root of `class`'s final live set is `beam_term::compare`-equal to a witness
/// tuple built in a **separate** arena the collector never touches.
///
/// The witness is a second, independent materialisation of the same graph — allocated in a
/// `beam_term::heap::Heap` test arena, never a root of the collector, never relocated by any
/// collection — so a collector that lost, duplicated or flattened a live object fails here rather
/// than in a later consumer. It doubles as the class's `size_object` footprint cross-check.
fn verify(class: &str, roots: &[Eterm], atoms: &AtomTable) -> bool {
    let (expected_roots, arity) = match class {
        CHURN => (CHURN_PERSISTENT, CHURN_PERSISTENT_ARITY),
        GROWTH => (roots.len(), GROWTH_KEEP_ARITY),
        COPY_OUT => (TABLE_ROWS, TABLE_ROW_ARITY),
        other => panic!("no witness for class `{other}`"),
    };
    assert!(
        expected_roots > 0,
        "{class}: the live set must not be empty"
    );
    assert_eq!(
        roots.len(),
        expected_roots,
        "{class}: the live set's cardinality"
    );
    let words: usize = roots.iter().map(|&root| size_object(root)).sum();
    assert_eq!(
        words,
        expected_roots * (arity + 1),
        "{class}: the live set's independently measured frozen-layout footprint"
    );
    let mut witness = WitnessHeap::new(expected_roots * (arity + 1) + 64);
    let mut intact = true;
    for (index, &root) in roots.iter().enumerate() {
        let expected = {
            let mut elements = vec![make_small(0); arity];
            for (offset, element) in elements.iter_mut().enumerate() {
                *element = make_small(index as i64 + offset as i64);
            }
            witness.tuple(&elements).expect("witness tuple")
        };
        if !term_eq(root, expected, atoms) {
            intact = false;
        }
    }
    intact
}

/// The `i`th element of a live tuple, as a small integer — the read the copy-out class takes of
/// the row it stored.
fn tuple_element(term: Eterm, index: usize) -> i64 {
    // SAFETY: `term` is a live boxed tuple this module (or a collection) built, so the word at
    // its pointer is its header; `index` is below the arity every caller uses (`TABLE_ROW_ARITY`),
    // so the element slot read below lies inside that tuple.
    let header = unsafe { *beam_term::prelude::boxed_val(term) };
    assert!(
        beam_term::prelude::is_arity_value(header),
        "not a tuple header"
    );
    assert!(
        index < beam_term::prelude::arityval(header),
        "element index inside the tuple"
    );
    // SAFETY: element `index` lies within the `arity`-element tuple the header just validated.
    beam_term::prelude::signed_value(unsafe { *beam_term::prelude::boxed_val(term).add(1 + index) })
}
