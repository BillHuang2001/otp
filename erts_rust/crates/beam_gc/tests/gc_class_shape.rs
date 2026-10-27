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

//! **The compile-shaped collection census** — the deterministic, load-invariant model of the
//! in-process-compile workload class (`GC_STRATEGY_MEASURED.md`'s C4), driven straight against
//! the collector so it is contention-immune.
//!
//! This is the **pre-change baseline** for the GC-cadence work: it records the *collection mix*
//! (how many collections, split minor/major) and the *copy work* (words/objects copied and
//! reclaimed) of the current collector on a shape that stands in for `compile:file/2`'s process,
//! over both mature strategies and two run lengths. A later change re-runs **this same file** and
//! diffs the printed counters; nothing here asserts a policy value, so the file is the witness,
//! not the verdict.
//!
//! # The shape (and why these three ingredients)
//!
//! `compile:file/2` in one process is not the churn shape: its per-operation allocation is small,
//! but it carries a **retained** live set that grows as the compiler accumulates state, keeps a few
//! intermediates alive for a step or two, and every so often allocates one **large one-off
//! transient** (a BIF result copied out into the process heap and then dropped). So the fixture
//! models exactly three things:
//!
//! * **A retained live set that grows in steps** ([`STEP_CELLS`] cons cells every
//!   [`STEP_PERIOD`] ops, prepended to one rooted list and kept reachable) — the compiler's
//!   accumulating state. It never shrinks, so the mature generation must keep up with it.
//! * **A ring of per-op intermediates** ([`TRANSIENT_SLOTS`] × [`SMALL_WORDS`] words, each built
//!   and dropped [`TRANSIENT_SLOTS`] ops later) — the ordinary intermediate terms a compile builds,
//!   consumes a step later, and abandons. They are live across a collection, so they are
//!   *promoted* and only then become mature-generation garbage: the shape that gives a generational
//!   collector fullsweeps to run.
//! * **A periodic one-off copy-out the nursery cannot hold** ([`BURST_WORDS`] words every
//!   [`BURST_PERIOD`] ops, allocated in one lump and abandoned immediately) — the copy-out a big
//!   BIF result forces. It is deliberately far above the nursery the ordinary allocations settle
//!   at, so it can only be admitted by growing the nursery; how often it exceeded the free room it
//!   found is printed with the census (the exceptions are the copy-outs that follow a fullsweep,
//!   whose whole-heap live-set target installs a nursery larger than one lump).
//!
//! The ordinary allocations go through the **same admission door `beam_rt` drives**
//! (`src/context/gc.rs::collect_or_grow`): ask
//! [`should_collect`](AnyCollector::should_collect) OR
//! [`collection_debt`](AnyCollector::collection_debt), and on `true` call
//! [`ensure_capacity`](AnyCollector::ensure_capacity) with the **current root set** — a real
//! `&mut [Eterm]` slice the run holds, because a collection rewrites the roots it is handed.
//! The copy-out does **not** go through the door, because the real one cannot: at
//! `beam_rt/src/context/calls.rs::hint_grow_gate` the machine's roots are unpublished, so the
//! collector cannot relocate — the copy-out calls
//! [`grow_nursery`](AnyCollector::grow_nursery), the no-relocation seam that retires the space and
//! records the request, and the **next** door admission then collects on `collection_debt()`.
//!
//! # What is deterministic, and what is not
//!
//! Everything printed is a counter or a capacity: collection counts, copy/reclaim volumes, and
//! the nursery/mature sizes a fixed policy settles on. Nothing here measures time, so a diff of
//! two runs of this file on the same build is a diff of behaviour, not of host load. The test
//! asserts only structural invariants (an identity, a non-empty census, and that the retained
//! root really lives in a region the collector scans) — never a sizing figure.
//!
//! Run it with
//! `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc --test gc_class_shape -- --nocapture`.

use beam_gc::prelude::{
    AnyCollector, Collection, GcConfig, GcStats, HeapObservable, HeapPolicy, MatureKind,
};
use beam_term::prelude::{make_small, ptr_val, Eterm, NIL};

// ---------------------------------------------------------------------------
// The shape
// ---------------------------------------------------------------------------

/// Cons cells the retained set gains at each step — the compiler's accumulator growing.
const STEP_CELLS: usize = 8;
/// Ops between steps of the retained set (so the live set grows ~1 word/op).
const STEP_PERIOD: usize = 8;
/// Elements of the per-op transient tuple — an ordinary compiled intermediate.
const SMALL_ELEMS: usize = 15;
/// Its frozen-layout footprint, header included.
const SMALL_WORDS: usize = SMALL_ELEMS + 1;
/// How many per-op intermediates a compile keeps live at once (each one is consumed a few ops
/// after it is built, so it is promoted by the next collection and dies in the mature generation).
const TRANSIENT_SLOTS: usize = 4;
/// Elements of the periodic one-off copy-out. Deliberately far above the default nursery, so the
/// burst can only be admitted by growing the nursery — the "BIF result copied into the process
/// heap" shape.
const BURST_ELEMS: usize = 2_999;
/// The burst's footprint, header included.
const BURST_WORDS: usize = BURST_ELEMS + 1;
/// Ops between bursts.
const BURST_PERIOD: usize = 32;
/// Run lengths the census is taken over.
const RUN_LENGTHS: [usize; 2] = [2_000, 4_000];

/// Root slots the run holds across every admission. A collection may rewrite any of them.
const RETAINED: usize = 0;
const BURST: usize = 1;
/// The ring of live per-op intermediates.
const TRANSIENT: usize = 2;
const ROOTS: usize = TRANSIENT + TRANSIENT_SLOTS;

/// The mature strategies the census is taken over, in report order.
const STRATEGIES: [MatureKind; 2] = [MatureKind::Copying, MatureKind::Immix];

/// The runtime's default spawn policy — what `beam_rt` gives a plain `run` process, so the
/// recurrence under measurement is the one a real compile actually pays.
fn policy() -> HeapPolicy {
    HeapPolicy::default()
}

/// The configuration for `kind`, pinned **explicitly** so the automatic rule cannot substitute
/// one strategy for the other (the comparison is like-for-like by construction).
fn config(kind: MatureKind) -> GcConfig {
    match kind {
        MatureKind::Copying => GcConfig::copying(policy()),
        MatureKind::Immix => GcConfig::immix(policy()),
    }
}

/// The strategy's report name.
fn strategy_name(kind: MatureKind) -> &'static str {
    match kind {
        MatureKind::Copying => "copying",
        MatureKind::Immix => "immix",
    }
}

// ---------------------------------------------------------------------------
// The admission door and the counters
// ---------------------------------------------------------------------------

/// The admission door: the runtime's `should_collect(need) || collection_debt()` predicate, and
/// `ensure_capacity` over the current roots when it answers `true`.
///
/// This is `beam_rt`'s `context/gc.rs::collect_or_grow` in miniature, so the census records the
/// runtime's cadence rather than a test-only one. It returns the collection it ran, if any.
fn door(
    c: &mut AnyCollector,
    roots: &mut Vec<Eterm>,
    need: usize,
    k: &mut Counters,
) -> Option<Collection> {
    if !c.should_collect(need) && !c.collection_debt() {
        return None;
    }
    let collection = c
        .ensure_capacity(need, roots)
        .expect("the compile-shaped run always fits the default policy");
    k.record(collection);
    Some(collection)
}

/// The counters of one `(N, strategy)` row.
#[derive(Debug, Default, Clone, Copy)]
struct Counters {
    collections: u64,
    minors: u64,
    majors: u64,
    words_copied: u64,
    words_reclaimed: u64,
    objects_copied: u64,
    /// Peak chain-summed nursery capacity and whole-heap capacity.
    peak_heap_size: usize,
    peak_total_heap_size: usize,
    /// Peak nursery chain length (spaces).
    peak_young_spaces: usize,
    /// Peak mature-generation capacity.
    peak_mature_capacity: usize,
    /// One-off copy-outs issued, and how many of them found a nursery whose free room could not
    /// hold the lump (the shape's own fidelity counter, not a collection statistic).
    bursts: u64,
    bursts_over_room: u64,
}

impl Counters {
    /// Folds one collection's statistics in, splitting it by the returned `CollectionKind`.
    fn record(&mut self, collection: Collection) {
        self.collections += 1;
        match collection.kind {
            beam_gc::CollectionKind::Minor => self.minors += 1,
            beam_gc::CollectionKind::Major => self.majors += 1,
        }
        let stats: GcStats = collection.stats;
        self.words_copied += stats.words_copied as u64;
        self.words_reclaimed += stats.words_reclaimed as u64;
        self.objects_copied += stats.objects_copied as u64;
    }

    /// Samples the collector's capacities after one op.
    fn observe(&mut self, c: &AnyCollector) {
        self.peak_heap_size = self.peak_heap_size.max(c.heap_size_words());
        self.peak_total_heap_size = self.peak_total_heap_size.max(c.total_heap_size_words());
        self.peak_young_spaces = self.peak_young_spaces.max(c.young_space_count());
        self.peak_mature_capacity = self.peak_mature_capacity.max(c.mature_capacity_words());
    }
}

// ---------------------------------------------------------------------------
// The three ingredients
// ---------------------------------------------------------------------------

/// The retained set grows by [`STEP_CELLS`] cons cells, each admitted through the door.
fn step_retained(c: &mut AnyCollector, roots: &mut Vec<Eterm>, k: &mut Counters) {
    for _ in 0..STEP_CELLS {
        door(c, roots, 3, k);
        let tail = roots[RETAINED];
        roots[RETAINED] = c
            .young_mut()
            .heap_mut()
            .cons(make_small(1), tail)
            .expect("the door left room for one cons cell");
    }
}

/// One ordinary compiled intermediate: allocated into the ring, replacing the one
/// [`TRANSIENT_SLOTS`] ops older — which is the point at which a compile's intermediate has been
/// consumed and the words are garbage (typically already promoted, so the mature generation is
/// where they are reclaimed).
fn small_transient(c: &mut AnyCollector, roots: &mut Vec<Eterm>, k: &mut Counters, op: usize) {
    door(c, roots, SMALL_WORDS, k);
    roots[TRANSIENT + op % TRANSIENT_SLOTS] = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(0); SMALL_ELEMS])
        .expect("the door left room for the per-op tuple");
}

/// One large one-off copy-out: a single lump the nursery cannot hold, abandoned immediately.
///
/// The admission is the **copy-out route**, not the door's: a BIF handing back a big result holds
/// no published root set at that point, so it cannot collect. It grows the nursery through the
/// **no-relocation seam** [`grow_nursery`](AnyCollector::grow_nursery) — which retires the space
/// and records the room request — and the **next** door admission collects on the
/// `collection_debt()` that growth created. The request is counted against the nursery's free room
/// (`should_collect`, a side-effect-free query) before the growth, so how often the copy-out really
/// exceeded the room it found is part of the census rather than an assumption: the exceptions are
/// the copy-outs that follow a **fullsweep**, whose whole-heap live-set target deliberately
/// installs a nursery larger than one copy-out.
fn burst(c: &mut AnyCollector, roots: &mut Vec<Eterm>, k: &mut Counters) {
    k.bursts += 1;
    if c.should_collect(BURST_WORDS) {
        k.bursts_over_room += 1;
    }
    c.grow_nursery(BURST_WORDS)
        .expect("the no-relocation growth seam always finds room for the copy-out");
    roots[BURST] = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(7); BURST_ELEMS])
        .expect("the grown nursery holds the copy-out");
    // Inspected once and dropped: the whole lump is a transient, so it is garbage before any
    // collection sizes the nursery — the next door admission collects on the debt this growth
    // recorded, which is why the copy-out after that must grow the nursery again.
    roots[BURST] = NIL;
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

/// One `(N, strategy)` row's final report.
struct Row {
    n: usize,
    strategy: &'static str,
    counters: Counters,
    /// The settled capacities at the end of the run.
    heap_size: usize,
    total_heap_size: usize,
    young_spaces: usize,
    mature_capacity: usize,
}

/// Runs the compile-shaped workload for `n` ops on the mature strategy `kind`.
fn run(n: usize, kind: MatureKind) -> Row {
    let mut c = AnyCollector::from_config(&config(kind)).expect("collector");
    let mut roots: Vec<Eterm> = vec![NIL; ROOTS];
    let mut k = Counters::default();
    k.observe(&c);
    for op in 0..n {
        if op % STEP_PERIOD == 0 {
            step_retained(&mut c, &mut roots, &mut k);
        }
        small_transient(&mut c, &mut roots, &mut k, op);
        if (op + 1) % BURST_PERIOD == 0 {
            burst(&mut c, &mut roots, &mut k);
        }
        k.observe(&c);
    }
    // The retained set must still be reachable, and it must be the thing the collector keeps: a
    // root pointing into neither scanned region would be a run that lost its own live set.
    let retained = ptr_val(roots[RETAINED]) as *const Eterm;
    assert!(
        c.young_contains(retained) || c.mature_contains(retained),
        "the retained root lives in a region the collector scans"
    );
    Row {
        n,
        strategy: strategy_name(kind),
        counters: k,
        heap_size: c.heap_size_words(),
        total_heap_size: c.total_heap_size_words(),
        young_spaces: c.young_space_count(),
        mature_capacity: c.mature_capacity_words(),
    }
}

impl Row {
    /// Prints one `(N, strategy)` block — the unit a later change diffs.
    fn print(&self) {
        let k = &self.counters;
        let ops = self.n as f64;
        println!(
            "\n=== compile-shaped census: N={} strategy={} ===",
            self.n, self.strategy
        );
        println!("collections                {:>12}", k.collections);
        println!("  minor                    {:>12}", k.minors);
        println!("  major                    {:>12}", k.majors);
        println!("words copied (total)       {:>12}", k.words_copied);
        println!(
            "words copied/op            {:>12.3}",
            k.words_copied as f64 / ops
        );
        println!("words reclaimed (total)    {:>12}", k.words_reclaimed);
        println!(
            "words reclaimed/op         {:>12.3}",
            k.words_reclaimed as f64 / ops
        );
        println!("objects copied (total)     {:>12}", k.objects_copied);
        println!(
            "heap_size_words      final {:>8}  peak {:>8}",
            self.heap_size, k.peak_heap_size
        );
        println!(
            "total_heap_size_words final {:>7}  peak {:>8}",
            self.total_heap_size, k.peak_total_heap_size
        );
        println!(
            "young_space_count    final {:>8}  peak {:>8}",
            self.young_spaces, k.peak_young_spaces
        );
        println!(
            "mature capacity (words) final {:>5}  peak {:>8}",
            self.mature_capacity, k.peak_mature_capacity
        );
        println!(
            "copy-outs issued / over room  {:>5} / {}",
            k.bursts, k.bursts_over_room
        );
    }
}

#[test]
fn the_compile_shape_census_records_the_collection_mix_and_copy_work() {
    println!(
        "\n=== gc_class shape: retained set +{STEP_CELLS} cells every {STEP_PERIOD} ops, \
         per-op transient {SMALL_WORDS} words, one-off copy-out {BURST_WORDS} words every \
         {BURST_PERIOD} ops; policy = HeapPolicy::default() ==="
    );
    let mut rows = Vec::new();
    for n in RUN_LENGTHS {
        for kind in STRATEGIES {
            let row = run(n, kind);
            // The one identity that must hold whatever the policy does.
            assert_eq!(
                row.counters.collections,
                row.counters.minors + row.counters.majors,
                "every collection is counted exactly once, as a minor or as a major"
            );
            assert!(
                row.counters.collections > 0,
                "the shape forces at least one collection"
            );
            row.print();
            rows.push(row);
        }
    }
    assert_eq!(rows.len(), RUN_LENGTHS.len() * STRATEGIES.len());
}
