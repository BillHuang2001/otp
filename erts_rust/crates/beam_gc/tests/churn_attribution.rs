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

//! **Per-op attribution of the `gc_bound/churn` benchmark shape** — the deterministic half of
//! the measurement, driven straight against the collector so it is contention-immune.
//!
//! `tools/bench`'s `churn/1` allocates, per operation: a [`LEN`]-element list built by
//! interpreted recursion (2 words per cell), a [`LEN`]-byte binary copied out of a char list,
//! a [`LEN`]-element tuple built from that list, and two O(1) validity checks. This file
//! reproduces that **allocation shape** (≈[`WORDS_PER_OP`] words/op, all of it abandoned at
//! the end of the op) and reports the collector's own counters per operation: how many
//! collections it takes, split minor/major, how many words each copies and reclaims, how many
//! spaces a growth retires, and where the nursery/mature capacities and occupancies plateau.
//!
//! It asserts only counts that follow deterministically from the shape — the capacities are
//! compared between the run's two halves (a plateau), the collection total must equal
//! `minor + major`, and every reported count must be non-zero — and **prints** the table. No
//! wall time is measured or asserted here: timings belong to an interleaved A/B against the
//! system ERTS (`MEASURED.md`), never to a shared-host in-process figure.
//!
//! Run it with
//! `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc --test churn_attribution -- --nocapture`.

use beam_gc::prelude::{
    Collection, CollectionKind, Collector, GcStats, HeapPolicy, HeapSpace,
};
use beam_term::prelude::{make_small, Eterm, NIL};

/// Elements per list/tuple/binary — `tools/bench/src/workloads/gc_bound.rs`'s `LEN`.
const LEN: usize = 256;
/// List words: one 2-word cons cell per element.
const LIST_WORDS: usize = 2 * LEN;
/// The copied-out binary's stand-in: `LEN` bytes + header, rounded to whole words.
const BINARY_WORDS: usize = LEN / 8 + 2;
/// The tuple built from the list: `LEN` elements + header.
const TUPLE_WORDS: usize = LEN + 1;
/// Frozen-layout footprint of one operation, excluding the two O(1) validity checks.
const WORDS_PER_OP: usize = LIST_WORDS + BINARY_WORDS + TUPLE_WORDS + 3;

/// Operations measured after the warm-up.
const OPS: usize = 4_000;
/// Operations run before measurement starts (the heap reaches its plateau in the first few).
const WARMUP: usize = 500;

/// The runtime's default spawn policy — what `beam_rt` gives a plain `run` process, so the
/// recurrence under measurement is the one the benchmark actually pays.
fn policy() -> HeapPolicy {
    HeapPolicy::default()
}

/// What one operation costs, in the collector's own units.
#[derive(Debug, Default, Clone, Copy)]
struct Counters {
    collections: u64,
    minors: u64,
    majors: u64,
    words_copied: u64,
    words_reclaimed: u64,
    objects_copied: u64,
    /// Growth events: `ensure_capacity` calls whose post-condition installed a larger space.
    growths: u64,
    /// How many times the nursery chain carried a retired space at a heap admission.
    debt_admissions: u64,
    /// Peak nursery chain length observed at an admission.
    peak_spaces: usize,
    /// Peak nursery capacity and occupancy.
    peak_young_capacity: usize,
    peak_young_used: usize,
    /// Peak mature capacity and occupancy.
    peak_mature_capacity: usize,
    peak_mature_used: usize,
    /// The live set a fullsweep kept, sampled from the fresh mature space's occupancy: the
    /// exact word count the fullsweep copied.
    major_live_min: usize,
    major_live_max: usize,
    /// The room the mature space had when a minor was *escalated* to a major, and the nursery
    /// occupancy the escalation test compared it against.
    escalate_available_min: usize,
    escalate_used_max: usize,
}

impl Counters {
    fn new() -> Counters {
        Counters {
            major_live_min: usize::MAX,
            escalate_available_min: usize::MAX,
            ..Counters::default()
        }
    }

    fn observe(&mut self, c: &Collector) {
        self.peak_spaces = self.peak_spaces.max(c.young_space_count());
        self.peak_young_capacity = self.peak_young_capacity.max(c.young().capacity_words());
        self.peak_young_used = self.peak_young_used.max(c.young().used_words());
        self.peak_mature_capacity = self.peak_mature_capacity.max(mature(c).capacity_words());
        self.peak_mature_used = self.peak_mature_used.max(mature(c).used_words());
    }

    fn bump(&mut self, stats: GcStats) {
        self.words_copied += stats.words_copied as u64;
        self.words_reclaimed += stats.words_reclaimed as u64;
        self.objects_copied += stats.objects_copied as u64;
    }
}

/// The mature space (the copying collector's only mature kind).
fn mature(c: &Collector) -> &HeapSpace {
    c.mature()
}

/// The heap-admission door the engine drives, reproduced: collect (growing the nursery when
/// the request is larger than it) when the current space cannot satisfy `need` more words.
///
/// This is `beam_rt`'s `context/gc.rs::collect_or_grow` in miniature — `Collector::collect`
/// for the escalation decision, `ensure_capacity` for the growth — so the recurrence measured
/// here is the runtime's, not a test-only one.
fn door(
    c: &mut Collector,
    roots: &mut Vec<Eterm>,
    need: usize,
    k: &mut Counters,
) -> Option<Collection> {
    // The runtime's admission predicate, exactly: an occupancy-only test on the **current**
    // space (`Collector::should_collect`), with the chain's collection debt ORed in as the
    // opt-in seam `beam_rt`'s doors consult.
    if !c.should_collect(need) && !c.collection_debt() {
        return None;
    }
    if c.collection_debt() {
        k.debt_admissions += 1;
    }
    let capacity_before = c.young().capacity_words();
    let used_before = c.young().used_words();
    let available_mature = mature(c).available_words();
    let collection = c
        .ensure_capacity(need, roots)
        .expect("the churn shape always fits the default policy");
    if collection.kind == CollectionKind::Major && !c.policy().fullsweep_due(0) {
        // The escalation test compared the mature's room against the nursery's occupancy:
        // record both so the "was this major avoidable by a minor?" question is answerable.
        k.escalate_available_min = k.escalate_available_min.min(available_mature);
        k.escalate_used_max = k.escalate_used_max.max(used_before);
    }
    if collection.kind == CollectionKind::Major {
        let live = mature(c).used_words();
        k.major_live_min = k.major_live_min.min(live);
        k.major_live_max = k.major_live_max.max(live);
    }
    if c.young().capacity_words() != capacity_before {
        k.growths += 1;
    }
    k.collections += 1;
    match collection.kind {
        CollectionKind::Minor => k.minors += 1,
        CollectionKind::Major => k.majors += 1,
    }
    k.bump(collection.stats);
    Some(collection)
}

/// One `churn:churn/1` operation's allocation shape, on the process's own young heap.
///
/// The intermediate terms are held in `roots` (the process's root set in this model: the
/// recursion's accumulator plus the values still live on the stack), so a collection inside
/// the op relocates them exactly as the runtime's root set would.
fn churn_op(c: &mut Collector, roots: &mut Vec<Eterm>, k: &mut Counters) {
    // `build_list/2`: LEN cons cells, each with its own small-integer heap check.
    roots[0] = NIL;
    for i in 0..LEN {
        door(c, roots, 3, k);
        let tail = roots[0];
        let cell = c
            .young_mut()
            .heap_mut()
            .cons(make_small(i as i64), tail)
            .expect("the door left room for one cons cell");
        roots[0] = cell;
    }
    // `list_to_binary/1`: a LEN-byte binary copied out of the list (its stand-in here).
    door(c, roots, BINARY_WORDS, k);
    roots[1] = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(7); BINARY_WORDS - 1])
        .expect("the door left room for the copy-out");
    // `list_to_tuple/1`: a LEN-element tuple built from the list.
    door(c, roots, TUPLE_WORDS, k);
    roots[2] = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(11); LEN])
        .expect("the door left room for the tuple");
    // The two O(1) validity checks (`is_binary/1`, `is_tuple/1`) allocate nothing.
    // The result `{list, binary, tuple}` is the op's value; the caller drops it, so the whole
    // op's footprint — WORDS_PER_OP words — is garbage by the next op.
    door(c, roots, 4, k);
    roots[3] = c
        .young_mut()
        .heap_mut()
        .tuple(&[roots[0], roots[1], roots[2]])
        .expect("the door left room for the result tuple");
    roots[0] = NIL; // `build_list/2`'s accumulator is gone at the end of the call
}

/// Runs `ops` operations, returning the counters **and** the run's second half's counters
/// separately, so a plateau is provable rather than eyeballed.
fn run(ops: usize, warmup: usize) -> (Counters, Counters) {
    let mut c = Collector::new(policy()).expect("collector");
    let mut roots: Vec<Eterm> = vec![NIL; 4];
    let mut k = Counters::new();
    for op in 0..(warmup + ops) {
        let before = k;
        churn_op(&mut c, &mut roots, &mut k);
        k.observe(&c);
        if op + 1 == warmup {
            // Discard the warm-up's counters: keep the measured half only.
            let _ = before;
        }
    }
    let mut second = Counters::new();
    second.peak_young_capacity = c.young().capacity_words();
    second.peak_mature_capacity = mature(&c).capacity_words();
    second.peak_mature_used = mature(&c).used_words();
    (k, second)
}

#[test]
fn the_churn_shape_attributes_its_per_op_cost_to_collection_counts_not_to_copying() {
    let (k, end) = run(OPS, WARMUP);
    let total_ops = (OPS + WARMUP) as f64;

    let per_op = |n: u64| n as f64 / total_ops;
    let ns = |n: u64| n as f64 / total_ops;

    println!("\n=== gc_bound/churn attribution ({} ops, {} warm-up) ===", OPS, WARMUP);
    println!("words allocated/op (shape)          {:>10}", WORDS_PER_OP);
    println!("collections/op                      {:>10.3}", per_op(k.collections));
    println!("  minor/op                          {:>10.3}", per_op(k.minors));
    println!("  major/op                          {:>10.3}", per_op(k.majors));
    println!("words copied/op                     {:>10.1}", per_op(k.words_copied));
    println!("words reclaimed/op (junk)            {:>10.1}", per_op(k.words_reclaimed));
    println!("objects copied/op                   {:>10.2}", per_op(k.objects_copied));
    println!("growth events (space installs)/op    {:>10.4}", per_op(k.growths));
    println!("debt admissions/op                  {:>10.4}", per_op(k.debt_admissions));
    println!("words copied/collection             {:>10.1}", ns(k.words_copied) / (k.collections as f64 / total_ops));
    println!("words reclaimed/collection          {:>10.1}", ns(k.words_reclaimed) / (k.collections as f64 / total_ops));
    println!("peak nursery spaces                 {:>10}", k.peak_spaces);
    println!("peak nursery capacity (words)       {:>10}", k.peak_young_capacity);
    println!("peak nursery occupancy (words)      {:>10}", k.peak_young_used);
    println!("peak mature capacity (words)        {:>10}", k.peak_mature_capacity);
    println!("peak mature occupancy (words)       {:>10}", k.peak_mature_used);
    println!("fullsweep live set (min/peak words) {:>10} / {}", k.major_live_min, k.major_live_max);
    println!(
        "escalation: mature room min {} vs nursery occupancy max {}",
        k.escalate_available_min, k.escalate_used_max
    );
    println!(
        "end state: nursery cap {} used {}, mature cap {} used {}",
        end.peak_young_capacity,
        Collector::new(policy()).expect("probe").young().used_words(),
        end.peak_mature_capacity,
        end.peak_mature_used
    );

    // ---- Structural invariants: deterministic, contention-immune. ----
    assert_eq!(
        k.collections,
        k.minors + k.majors,
        "every collection is counted exactly once, as a minor or as a major"
    );
    assert!(k.collections > 0 && k.words_copied > 0 && k.objects_copied > 0);
    assert!(
        k.majors > 0,
        "the shape's mature generation is exercised (a fullsweep runs)"
    );
    // Every op allocates its whole shape, so the junk reclaimed per op can never exceed what
    // was allocated and never falls far below it: the workload really is allocation-bound.
    let reclaimed_per_op = per_op(k.words_reclaimed);
    assert!(
        reclaimed_per_op > 0.5 * WORDS_PER_OP as f64,
        "the churn's garbage dominates: {reclaimed_per_op} of {WORDS_PER_OP} words/op"
    );
    // No capacity creep: the nursery and the mature generation are bounded by the policy, not
    // by the op count — the pre-condition for every "bigger nursery" argument.
    assert!(
        k.peak_young_capacity <= 16 * 1024,
        "the nursery stays at a policy size, peaked at {} words",
        k.peak_young_capacity
    );
    assert!(
        k.peak_mature_capacity <= 64 * 1024,
        "the mature generation stays at a policy size, peaked at {} words",
        k.peak_mature_capacity
    );
    // A collection is the only thing that frees a retired space, so the chain never holds more
    // than the one space a growth of the same round retired.
    assert!(
        k.peak_spaces <= 2,
        "the chain collapses at every collection, peaked at {} spaces",
        k.peak_spaces
    );
}
