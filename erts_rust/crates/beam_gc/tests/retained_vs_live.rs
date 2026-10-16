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

//! Retained-vs-live memory accounting for a **long-running allocate/collect loop**.
//!
//! `tests/memory_overhead.rs` proves *per-object* overhead is zero and that the RSS of a
//! bounded cycle *plateaus*. It does not answer the complementary runtime question this file
//! closes: **how much memory the heap spaces retain versus how many words are truly live**,
//! and whether that retained-minus-live surplus stays *bounded* (never growing per cycle) over
//! a long run — the property a long-running node depends on.
//!
//! Each of the `WARMUP + MEASURED` cycles builds a **fresh** bounded live graph plus garbage
//! in the nursery and drives a full collection (`fullsweep_after == 0`, so *every* collection
//! is a fullsweep and the mature generation is rebuilt from the live set each cycle — a leak
//! or a retained-garbage bug shows up immediately as growth). For the loop we report and
//! assert four independent numbers:
//!
//! * the **true live set** in words, computed **independently** of the collector from the
//!   built graph's frozen-layout footprint (`beam_term::size_object` over the roots — no
//!   collector internal is consulted);
//! * the words **retained by the heap spaces**: `young().capacity_words()`,
//!   `mature().used_words()`, `mature().capacity_words()`, and the compound
//!   `total_heap_size_words()` (via `HeapInfo`/`HeapObservable`);
//! * for the mark-region ([`AnyCollector::Immix`]) strategy, the **resident line-words** of
//!   the mature Immix space (`GenImmixCollector::last_immix_stats` and
//!   [`ImmixCounters`](beam_gc::prelude::ImmixCounters)) — the line-granular footprint
//!   multiplier is where the two strategies genuinely differ;
//! * a process-wide **RSS plateau** from `/proc/self/statm` (SKIPPED LOUDLY off Linux).
//!
//! The loop runs through **both** strategies — [`AnyCollector::Copying`] and
//! [`AnyCollector::Immix`], built from [`GcConfig::copying`]/[`GcConfig::immix`] — behind one
//! helper so the two are measured and asserted identically.
//!
//! ## What is asserted (deterministic) versus printed
//!
//! Every **assertion** is on a deterministic count: the live words, the per-cycle mature
//! occupancy and capacities, and the surplus.
//!
//! * the nursery (`young().used_words()`) is **zero** after *every* collection;
//! * the mature occupancy is **at least** the live set always, is **exactly** the live set
//!   for the copying strategy (the collector reclaims to the live set — no survivor
//!   garbage), and is **bounded by a constant factor** of the live set for the line-granular
//!   mark-region strategy;
//! * the capacities (`young().capacity_words()`, `mature().capacity_words()`) and the
//!   occupancy reach a **steady state** over the measured cycles;
//! * the **surplus** (retained − live) does **not grow**: the second half's maximum is no
//!   larger than the first half's.
//!
//! Wall-clock time and RSS are **printed only**; the single RSS assertion compares two
//! snapshots of the same process and tolerates page-granularity noise (a hard 1.5x plateau
//! bound), exactly the discipline of `tests/memory_overhead.rs`.
//!
//! The only `unsafe` below is the `sysconf` call the RSS reading needs (the same pattern as
//! `tests/memory_overhead.rs`), and it carries a `// SAFETY:` rationale. The collector is
//! driven entirely through its safe public API — this test reserves no term words by hand.

use std::sync::Mutex;

use beam_gc::prelude::{
    AnyCollector, CollectionKind, GcConfig, HeapInfo, HeapPolicy, MatureKind, LINE_WORDS,
};
use beam_term::prelude::{make_small, size_object, Eterm};

/// Serialises this binary's tests: the RSS measurement reads process-wide resident pages, so
/// no other test in this file may allocate while it runs.
static SERIAL: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// The workload: a fresh bounded live graph plus garbage, each cycle
// ---------------------------------------------------------------------------

/// Live tuples built per cycle (a bounded, non-growing live set).
const LIVE_TUPLES: usize = 256;
/// Element count of each live tuple (all immediates, so no sharing).
const LIVE_ARITY: usize = 8;

/// Garbage tuples built per cycle and left unreferenced (the collection must reclaim them).
const GARBAGE_TUPLES: usize = 256;
/// Element count of each garbage tuple.
const GARBAGE_ARITY: usize = 8;

/// Cycles run before measurement starts (allocator/carrier warm-up).
const WARMUP: usize = 20;
/// Long-running measured cycles (the loop itself runs `WARMUP + MEASURED` = 220 rounds).
const MEASURED: usize = 200;

/// Words per tuple of immediates: the frozen layout is `arity + 1` (a header plus one word
/// per element).
const fn tuple_words(arity: usize) -> usize {
    arity + 1
}

/// The live graph's frozen-layout footprint, from the tuple formula (cross-checked against
/// `beam_term::size_object` inside [`build_live_graph`] every cycle).
const LIVE_WORDS: usize = LIVE_TUPLES * tuple_words(LIVE_ARITY);

/// The line-granular surplus bound for the mark-region strategy.
///
/// A mark-region object occupies whole `LINE_WORDS`(16)-word lines: a 9-word live tuple
/// claims a 16-word line, so the resident footprint is at most `ceil(16 / 9) = 2x` the live
/// words. A generous constant factor of 4 leaves room for the block/line geometry while
/// still proving the surplus is bounded (and, crucially, never grows).
const IMMIX_SURPLUS_FACTOR: usize = 4;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Bytes per memory page, from the C library (`sysconf(_SC_PAGESIZE)`).
fn page_size_bytes() -> usize {
    // SAFETY: `sysconf(_SC_PAGESIZE)` has no preconditions, takes no pointer and has no side
    // effects; it returns a positive page size on every supported platform.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as usize
    } else {
        // A conservative fallback; every mainstream target uses 4 KiB.
        4096
    }
}

/// The process's resident set size in bytes, read from `/proc/self/statm` (field 2, the
/// resident-page count).
///
/// Returns `None` when the file cannot be read or parsed (e.g. a non-Linux platform), so the
/// caller can skip the RSS half loudly instead of failing.
fn read_rss_bytes() -> Option<usize> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(resident_pages.saturating_mul(page_size_bytes()))
}

/// Builds a fresh `LIVE_TUPLES`-tuple live graph in the collector's nursery and returns its
/// roots together with the graph's **independently measured** frozen-layout footprint.
///
/// The footprint is computed from `beam_term::size_object` over the returned roots, so it
/// never consults a collector internal; it is cross-checked against [`LIVE_WORDS`].
fn build_live_graph(collector: &mut AnyCollector) -> (Vec<Eterm>, usize) {
    let mut live = Vec::with_capacity(LIVE_TUPLES);
    for i in 0..LIVE_TUPLES {
        let base = (i as i64) * LIVE_ARITY as i64;
        let mut elems = [make_small(0); LIVE_ARITY];
        for (j, e) in elems.iter_mut().enumerate() {
            *e = make_small(base + j as i64);
        }
        live.push(
            collector
                .young_mut()
                .heap_mut()
                .tuple(&elems)
                .expect("live tuple"),
        );
    }

    let words: usize = live.iter().map(|&t| size_object(t)).sum();
    assert_eq!(
        words, LIVE_WORDS,
        "the independently measured frozen footprint must match the layout formula"
    );
    (live, words)
}

/// Allocates `GARBAGE_TUPLES` tuples in the nursery and returns how many words they occupy.
///
/// The tuples are deliberately dropped (unreferenced) — they are garbage the collection must
/// reclaim, so the previous cycle's graph and this cycle's garbage both have to disappear.
fn build_garbage(collector: &mut AnyCollector) -> usize {
    // The returned terms are held only in this local sink, which is dropped on return: they
    // are never referenced from the roots, so the collection reclaims every one of them.
    let mut sink: Vec<Eterm> = Vec::with_capacity(GARBAGE_TUPLES);
    for i in 0..GARBAGE_TUPLES {
        let base = -(i as i64) - 1;
        let mut elems = [make_small(0); GARBAGE_ARITY];
        for (j, e) in elems.iter_mut().enumerate() {
            *e = make_small(base - j as i64);
        }
        sink.push(
            collector
                .young_mut()
                .heap_mut()
                .tuple(&elems)
                .expect("garbage tuple"),
        );
    }
    sink.len() * tuple_words(GARBAGE_ARITY)
}

/// The mark-region mature space's accounting at one snapshot — `None` for the copying
/// strategy. Captures both the `ImmixCounters` resident footprint and the most recent
/// in-place reclaim's `ImmixStats`.
struct ImmixView {
    /// Resident line-words — `ImmixCounters::used_words`, the line-granular footprint.
    resident_words: usize,
    /// Resident lines — `ImmixCounters::lines_allocated`.
    resident_lines: usize,
    /// Lines covered by a live object at the last mark.
    marked_lines: usize,
    /// Live objects the most recent **in-place** major pinned (`None` when the last major
    /// relocated the region instead, or before any major has run).
    pinned_objects: Option<usize>,
    /// Live objects the most recent in-place major evacuated.
    evacuated_objects: Option<usize>,
}

/// One cycle's retained-vs-live snapshot.
struct Sample {
    /// The RSS snapshot in bytes, when `/proc/self/statm` is readable.
    rss: Option<usize>,
    /// The true live set in words (the independent frozen-layout footprint).
    live_words: usize,
    /// The nursery's occupancy after the collection (must be zero).
    young_used: usize,
    /// The nursery's capacity in words.
    young_capacity: usize,
    /// The words the mature generation holds after the collection.
    mature_used: usize,
    /// The mature generation's capacity in words.
    mature_capacity: usize,
    /// `young_capacity + mature_capacity` — the C `heap_sz + old heap` (`HeapInfo::total_heap_size`).
    total_heap_size: usize,
    /// The mark-region accounting (`ImmixSpace` only; `None` for copying).
    immix: Option<ImmixView>,
}

impl Sample {
    /// The words the heap spaces **reserve** beyond the live set: `total_heap_size − live`.
    fn capacity_surplus(&self) -> usize {
        self.total_heap_size.saturating_sub(self.live_words)
    }

    /// The words the mature generation **holds** beyond the live set: `mature_used − live`
    /// (zero for the copying strategy; the line-granular rounding for the mark-region one).
    fn occupancy_surplus(&self) -> usize {
        self.mature_used.saturating_sub(self.live_words)
    }
}

/// Reads one collector's retained-vs-live accounting — the four requested numbers plus the
/// mark-region resident footprint when the Immix strategy is in use.
fn snapshot(collector: &AnyCollector, live_words: usize) -> Sample {
    let info: HeapInfo = collector.heap_info();
    assert_eq!(
        info.heap_size,
        collector.young().capacity_words(),
        "`HeapInfo::heap_size` is the nursery capacity"
    );

    let (mature_used, mature_capacity, immix) = match collector {
        AnyCollector::Copying(c) => (c.mature().used_words(), c.mature().capacity_words(), None),
        AnyCollector::Immix(c) => {
            let counters = c.mature().counters();
            let stats = c.last_immix_stats();
            (
                c.mature().used_words(),
                c.mature().capacity_words(),
                Some(ImmixView {
                    resident_words: counters.used_words,
                    resident_lines: counters.lines_allocated(),
                    marked_lines: counters.lines_marked,
                    pinned_objects: stats.map(|s| s.objects_pinned),
                    evacuated_objects: stats.map(|s| s.objects_evacuated),
                }),
            )
        }
    };

    Sample {
        rss: None,
        live_words,
        young_used: collector.young().used_words(),
        young_capacity: collector.young().capacity_words(),
        mature_used,
        mature_capacity,
        total_heap_size: info.total_heap_size,
        immix,
    }
}

/// The min/max of a slice (the caller guarantees it is non-empty).
fn min_max(values: &[usize]) -> (usize, usize) {
    let min = *values.iter().min().expect("non-empty");
    let max = *values.iter().max().expect("non-empty");
    (min, max)
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// Runs the long allocate/collect loop for `config` and asserts the retained-vs-live
/// discipline for the resolved `kind`.
///
/// `label` names the strategy in every message; `kind` must be what `config` resolves to.
fn run_loop(label: &str, config: &GcConfig, kind: MatureKind) {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let rss_available = read_rss_bytes().is_some();
    let rss_before = read_rss_bytes();

    let mut collector = AnyCollector::from_config(config).expect("collector");
    assert_eq!(
        collector.kind(),
        kind,
        "{label}: the config must resolve to the expected strategy"
    );

    let garbage_words = GARBAGE_TUPLES * tuple_words(GARBAGE_ARITY);
    let mut samples: Vec<Sample> = Vec::with_capacity(MEASURED);

    for cycle in 0..(WARMUP + MEASURED) {
        // A fresh bounded live graph; a new root set each cycle makes the previous cycle's
        // graph unreachable, and the garbage is unreferenced from the start.
        let (mut roots, live_words) = build_live_graph(&mut collector);
        let allocated_garbage = build_garbage(&mut collector);
        assert_eq!(allocated_garbage, garbage_words);

        let collection = collector.collect(&mut roots).expect("collect");
        assert_eq!(
            collection.kind,
            CollectionKind::Major,
            "{label}: fullsweep_after == 0 must force a fullsweep (round {cycle})"
        );
        assert!(
            collection.stats.words_reclaimed > 0,
            "{label}: a fresh live graph plus {} garbage words must reclaim something \
             (round {cycle})",
            garbage_words
        );

        let mut snap = snapshot(&collector, live_words);

        // ---- per-cycle invariants ----
        assert_eq!(
            snap.young_used, 0,
            "{label}: the nursery must be empty after every collection (round {cycle})"
        );
        assert!(
            snap.mature_used >= snap.live_words,
            "{label}: the mature generation must retain at least the live set \
             ({} < {}) (round {cycle})",
            snap.mature_used,
            snap.live_words
        );
        assert_eq!(
            snap.total_heap_size,
            snap.young_capacity + snap.mature_capacity,
            "{label}: `total_heap_size` is the nursery plus mature capacity (round {cycle})"
        );
        match kind {
            MatureKind::Copying => assert_eq!(
                snap.mature_used, snap.live_words,
                "{label}: the copying mature space must hold exactly the live set — no \
                 survivor garbage (round {cycle})"
            ),
            MatureKind::Immix => {
                assert!(
                    snap.mature_used <= snap.live_words * IMMIX_SURPLUS_FACTOR,
                    "{label}: the mark-region mature footprint ({} words) must stay within \
                     {IMMIX_SURPLUS_FACTOR}x the live set ({} words) (round {cycle})",
                    snap.mature_used,
                    snap.live_words
                );
                let iv = snap
                    .immix
                    .as_ref()
                    .expect("the immix strategy must expose the mark-region counters");
                assert_eq!(
                    iv.resident_words, snap.mature_used,
                    "{label}: the Immix counters' resident words must agree with \
                     `mature().used_words()` (round {cycle})"
                );
                assert_eq!(
                    iv.resident_lines * LINE_WORDS,
                    iv.resident_words,
                    "{label}: the resident words are whole lines (round {cycle})"
                );
                assert!(
                    iv.resident_words >= snap.live_words,
                    "{label}: the resident line-words must cover the live set ({} < {}) \
                     (round {cycle})",
                    iv.resident_words,
                    snap.live_words
                );
            }
        }

        if cycle >= WARMUP {
            snap.rss = if rss_available {
                read_rss_bytes()
            } else {
                None
            };
            samples.push(snap);
        }
    }

    assert_eq!(samples.len(), MEASURED, "one sample per measured cycle");

    // ---- steady state (deterministic counts) ----
    let young_caps: Vec<usize> = samples.iter().map(|s| s.young_capacity).collect();
    let mature_caps: Vec<usize> = samples.iter().map(|s| s.mature_capacity).collect();
    let mature_used: Vec<usize> = samples.iter().map(|s| s.mature_used).collect();
    let totals: Vec<usize> = samples.iter().map(|s| s.total_heap_size).collect();

    let (young_min, young_max) = min_max(&young_caps);
    let (mature_cap_min, mature_cap_max) = min_max(&mature_caps);
    let (mature_used_min, mature_used_max) = min_max(&mature_used);
    let (total_min, total_max) = min_max(&totals);

    assert_eq!(
        young_min, young_max,
        "{label}: the nursery capacity must reach a steady state: min {young_min} != max \
         {young_max}"
    );
    assert_eq!(
        mature_cap_min, mature_cap_max,
        "{label}: the mature capacity must reach a steady state: min {mature_cap_min} != max \
         {mature_cap_max}"
    );
    assert_eq!(
        mature_used_min, mature_used_max,
        "{label}: the mature occupancy must be constant (the live set is constant and the \
         collector reclaims to it): min {mature_used_min} != max {mature_used_max}"
    );
    assert_eq!(
        total_min, total_max,
        "{label}: `total_heap_size` must reach a steady state: min {total_min} != max \
         {total_max}"
    );

    // The mark-region resident line count must be a steady state too (it is the line-granular
    // footprint, so a growing value would be a retained-line leak).
    let immix_lines: Vec<usize> = samples
        .iter()
        .filter_map(|s| s.immix.as_ref().map(|v| v.resident_lines))
        .collect();
    if kind == MatureKind::Immix {
        assert_eq!(
            immix_lines.len(),
            MEASURED,
            "{label}: every measured cycle must expose the mark-region line count"
        );
        let (lines_min, lines_max) = min_max(&immix_lines);
        assert_eq!(
            lines_min, lines_max,
            "{label}: the resident line count must reach a steady state: min {lines_min} != \
             max {lines_max}"
        );
    }

    // ---- the surplus must not grow between the first and second half ----
    let capacity_surplus: Vec<usize> = samples.iter().map(Sample::capacity_surplus).collect();
    let occupancy_surplus: Vec<usize> = samples.iter().map(Sample::occupancy_surplus).collect();
    let half = capacity_surplus.len() / 2;

    let (cap_first_max, cap_second_max) = (
        *capacity_surplus[..half].iter().max().expect("non-empty"),
        *capacity_surplus[half..].iter().max().expect("non-empty"),
    );
    assert!(
        cap_second_max <= cap_first_max,
        "{label}: the reserved surplus must not grow per cycle: first-half max \
         {cap_first_max} words, second-half max {cap_second_max} words"
    );

    let (occ_first_max, occ_second_max) = (
        *occupancy_surplus[..half].iter().max().expect("non-empty"),
        *occupancy_surplus[half..].iter().max().expect("non-empty"),
    );
    assert!(
        occ_second_max <= occ_first_max,
        "{label}: the mature occupancy surplus must not grow per cycle: first-half max \
         {occ_first_max} words, second-half max {occ_second_max} words"
    );

    // ---- headline numbers ----
    let live = samples[0].live_words;
    let retained = total_max;
    let reserved_surplus = retained - live;
    let occupancy = mature_used_max;
    eprintln!("[{label}] retained-vs-live over {MEASURED} measured cycles ({WARMUP} warm-up):");
    eprintln!("  true live set (independent frozen footprint) = {live} words");
    eprintln!(
        "  young capacity = {young_min} words (steady), mature occupancy = {occupancy} words \
         (steady)"
    );
    eprintln!(
        "  mature capacity = {mature_cap_min} words (steady), total_heap_size = {retained} \
         words (steady)"
    );
    eprintln!(
        "  reserved surplus (total_heap_size - live) = {reserved_surplus} words \
         ({reserved_ratio:.3}x live)",
        reserved_ratio = retained as f64 / live as f64
    );
    eprintln!(
        "  occupancy surplus (mature_used - live) = {} words ({:.3}x live)",
        occupancy - live,
        occupancy as f64 / live as f64
    );
    match kind {
        MatureKind::Copying => eprintln!(
            "  strategy = Copying: the mature space holds exactly the live set every cycle \
             (surplus = 0)"
        ),
        MatureKind::Immix => {
            let iv = samples[MEASURED - 1]
                .immix
                .as_ref()
                .expect("the immix strategy must expose the mark-region counters");
            eprintln!(
                "  strategy = Immix: resident line-words = {} ({} lines), i.e. the {:.3}x \
                 line-granular multiplier over the {live} live words; last reclaim pinned/\
                 evacuated objects = {:?}/{:?}, marked lines = {}",
                iv.resident_words,
                iv.resident_lines,
                iv.resident_words as f64 / live as f64,
                iv.pinned_objects,
                iv.evacuated_objects,
                iv.marked_lines,
            );
        }
    }

    // ---- RSS plateau ----
    if !rss_available {
        eprintln!(
            "[{label}] SKIP RSS plateau: `/proc/self/statm` is unavailable, so RSS cannot be \
             measured (the deterministic retained-vs-live half above already passed)"
        );
        return;
    }

    let rss: Vec<usize> = samples.iter().filter_map(|s| s.rss).collect();
    assert_eq!(
        rss.len(),
        MEASURED,
        "{label}: every measured cycle must yield an RSS snapshot"
    );

    let rss_half = rss.len() / 2;
    let first_half_max = *rss[..rss_half].iter().max().expect("non-empty");
    let first_half_min = *rss[..rss_half].iter().min().expect("non-empty");
    let second_half_max = *rss[rss_half..].iter().max().expect("non-empty");
    let last = *rss.last().expect("non-empty");
    let plateau_factor = second_half_max as f64 / first_half_max as f64;

    eprintln!(
        "[{label}] rss (bytes): before = {}, first-half min/max = {first_half_min}/\
         {first_half_max}, second-half max = {second_half_max}, last = {last}, plateau factor \
         = {plateau_factor:.3}x",
        rss_before.unwrap_or(0)
    );

    // The second half must stay within a bounded factor (1.5x) of the first half: after
    // warm-up a bounded, non-growing live set may not push RSS up without bound.
    assert!(
        second_half_max <= first_half_max + first_half_max / 2,
        "{label}: RSS did not plateau: first-half max {first_half_max} bytes, second-half max \
         {second_half_max} bytes ({plateau_factor:.3}x > 1.5x) -- an allocation cycle is \
         leaking"
    );
    assert!(
        last <= first_half_max + first_half_max / 2,
        "{label}: the final RSS {last} bytes exceeds 1.5x the first-half max \
         {first_half_max} bytes"
    );
}

/// The heap-size policy both strategies share.
///
/// `fullsweep_after == 0` forces a fullsweep on **every** collection, so the mature
/// generation is rebuilt from the live set each cycle — a retained-garbage or leak bug shows
/// up immediately as growth. The initial sizes are chosen inside each strategy's
/// growth/shrink fixed point so the steady state is reached during warm-up:
///
/// * a 8192-word nursery holds the live graph plus the garbage and neither grows nor shrinks —
///   it is **pinned** with `min_heap_words` because the bounded
///   [`HeapPolicy::young_after_collection`] shrinks a nursery a fullsweep leaves empty, and
///   every cycle re-allocates the same corpus;
/// * a 12288-word mature generation (48 whole mark-region blocks) is a fixed point for both
///   strategies — the copying space shrinks it to the curated fixed point above the live set
///   once and then keeps it, while the mark-region space keeps it in place.
fn shared_policy() -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: 0,
        initial_young_words: 8192,
        min_heap_words: 8192,
        initial_mature_words: 12_288,
        ..HeapPolicy::default()
    }
}

/// The **copying** strategy: the mature space must hold *exactly* the live set every cycle.
#[test]
fn retained_vs_live_copying_is_bounded_and_does_not_grow() {
    let config = GcConfig::copying(shared_policy());
    assert_eq!(config.mature(), MatureKind::Copying);
    run_loop("copying", &config, MatureKind::Copying);
}

/// The **mark-region** strategy: the mature footprint is line-granular, so it retains a
/// bounded constant multiple of the live set — but never grows per cycle.
#[test]
fn retained_vs_live_immix_is_bounded_and_does_not_grow() {
    let config = GcConfig::immix(shared_policy());
    assert_eq!(config.mature(), MatureKind::Immix);
    run_loop("immix", &config, MatureKind::Immix);
}
