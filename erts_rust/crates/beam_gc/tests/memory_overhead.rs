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

//! Memory-usage evidence for `beam_gc`: **per-object overhead** and a **no-leak RSS
//! plateau**.
//!
//! The rewrite's mandate is improved performance "both speed and memory usage" (workspace
//! `CONTEXT.md`), but no test in this crate measures how much memory the collector actually
//! *consumes* per object or whether repeated allocation/collection cycles leak. This file
//! closes that gap with two independent, **deterministic** measurements:
//!
//! 1. [`per_object_overhead_is_bounded`] builds representative patterns (N cons cells, N
//!    2-tuples, N small boxed values, N small flat maps) in a [`HeapSpace`] and compares the
//!    words the bump allocator actually handed out against the objects' **frozen-layout
//!    footprint** (`beam_term::size_object`, cross-checked against `beam_term`'s own layout
//!    formulas). The asserted overhead bound is *zero words per object*: terms are whole,
//!    word-aligned `Eterm`s, so a bump allocator must not pad.
//!
//! 2. [`rss_and_word_accounting_plateau_across_cycles`] drives the two-generation
//!    [`Collector`] through many bounded allocate/collect cycles (a fresh live graph plus
//!    garbage each round, with `fullsweep_after == 0` so every cycle rebuilds the mature
//!    generation) and asserts that the process's resident set from `/proc/self/statm`
//!    *plateaus* rather than growing, and that the collector's own word accounting returns to
//!    a steady state (constant live words, bounded capacities).
//!
//! ## Determinism
//!
//! Every **assertion** is on a deterministic count (used words, frozen words, live words,
//! capacity). Wall-clock time and RSS are **printed only** for a human reading run with
//! `-- --nocapture`; they are never asserted except for the *bounded-factor* plateau check,
//! which compares two RSS snapshots of the same process and is robust to page-granularity
//! noise. RSS is measured from `/proc/self/statm`; when that is unavailable (non-Linux) the
//! test **skips loudly** and still runs the deterministic half.
//!
//! `unsafe` below only writes words this test just reserved itself; every block carries a
//! `// SAFETY:` rationale.

use std::sync::Mutex;

use beam_alloc::default_allocator;
use beam_gc::prelude::{CollectionKind, Collector, HeapPolicy, HeapSpace};
use beam_term::prelude::{
    flatmap_size_words, make_small, map_words_capacity, size_object, write_float, write_map,
    AtomTable, Eterm, FLOAT_SIZE_WORDS, NIL,
};

/// Serialises this binary's tests: the RSS measurement reads process-wide resident pages, so
/// no other test in this file may allocate while it runs.
static SERIAL: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Allocates an empty `words`-word semi-space over the default allocator.
fn space(words: usize) -> HeapSpace {
    HeapSpace::with_allocator(default_allocator().clone(), words).expect("heap allocation")
}

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

// ---------------------------------------------------------------------------
// 1. Per-object overhead (deterministic)
// ---------------------------------------------------------------------------

/// Objects built per pattern.
const N: usize = 4096;
/// Pairs per "small map" (a flat map; the frozen layout sizes it exactly).
const MAP_PAIRS: usize = 4;

/// One allocation pattern's measured words versus its frozen-layout footprint.
struct Overhead {
    /// A human-readable pattern name.
    label: &'static str,
    /// How many objects the pattern built.
    count: usize,
    /// The words the bump allocator actually handed out.
    words_consumed: usize,
    /// The objects' frozen-layout footprint, summed from `beam_term::size_object`.
    frozen_words: usize,
}

impl Overhead {
    /// Words consumed *beyond* the objects' frozen footprint.
    ///
    /// A negative value would mean the measurement somehow under-counted the frozen layout;
    /// a positive value is genuine per-object overhead (padding). The test requires exactly
    /// zero.
    fn overhead_words(&self) -> isize {
        self.words_consumed as isize - self.frozen_words as isize
    }
}

/// Prints the measured table and asserts the per-object overhead bound.
///
/// The bound is the tightest possible: the bump allocator hands out whole, word-aligned term
/// words, so there must be **no** per-object padding (overhead `== 0`, i.e. strictly under one
/// word per object).
fn assert_and_report_overhead(rows: &[Overhead]) {
    eprintln!("per-object overhead (N = {N} objects per pattern):");
    eprintln!(
        "  {:<16} {:>8} {:>12} {:>12} {:>10} {:>14}",
        "pattern", "objects", "consumed", "frozen", "overhead", "overhead/object"
    );

    let mut total_objects = 0usize;
    let mut total_consumed = 0usize;
    let mut total_frozen = 0usize;

    for row in rows {
        let overhead = row.overhead_words();
        let per_object = overhead as f64 / row.count as f64;
        eprintln!(
            "  {:<16} {:>8} {:>12} {:>12} {:>10} {:>14.4}",
            row.label, row.count, row.words_consumed, row.frozen_words, overhead, per_object
        );

        assert!(
            overhead >= 0,
            "{}: consumed {} words but the frozen layout is only {} -- under-counted",
            row.label,
            row.words_consumed,
            row.frozen_words
        );
        assert!(
            per_object < 1.0,
            "{}: {per_object:.4} words of overhead per object is not bounded below one word",
            row.label
        );
        assert_eq!(
            overhead, 0,
            "{}: {} words of overhead over {} objects ({} per object) -- the bump allocator \
             must not pad a word-aligned term",
            row.label,
            overhead,
            row.count,
            per_object
        );

        total_objects += row.count;
        total_consumed += row.words_consumed;
        total_frozen += row.frozen_words;
    }

    let total_overhead = total_consumed as isize - total_frozen as isize;
    eprintln!(
        "  total: {total_objects} objects, {total_consumed} words consumed, {total_frozen} \
         words frozen, {total_overhead} overhead words"
    );
    assert_eq!(
        total_overhead, 0,
        "the total per-object overhead must be exactly zero words"
    );
}

#[test]
fn per_object_overhead_is_bounded() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let mut rows: Vec<Overhead> = Vec::new();

    // (1) Cons cells: a header-less 2-word `[CAR, CDR]` object.
    {
        let mut heap = space(2 * N + 8);
        let mut roots = Vec::with_capacity(N);
        for i in 0..N {
            roots.push(
                heap.heap_mut()
                    .cons(make_small(i as i64), NIL)
                    .expect("cons cell"),
            );
        }
        assert_eq!(heap.used_words(), 2 * N, "a cons consumes exactly 2 words");
        assert_eq!(size_object(roots[0]), 2, "the frozen cons size is 2 words");
        rows.push(Overhead {
            label: "cons cell",
            count: N,
            words_consumed: heap.used_words(),
            frozen_words: roots.iter().map(|&t| size_object(t)).sum(),
        });
    }

    // (2) 2-tuples: a header word plus two arity words.
    {
        let mut heap = space(3 * N + 8);
        let mut roots = Vec::with_capacity(N);
        for i in 0..N {
            let i = i as i64;
            roots.push(
                heap.heap_mut()
                    .tuple(&[make_small(i), make_small(i + 1)])
                    .expect("2-tuple"),
            );
        }
        assert_eq!(
            heap.used_words(),
            3 * N,
            "a 2-tuple consumes exactly 3 words"
        );
        assert_eq!(
            size_object(roots[0]),
            3,
            "the frozen 2-tuple size is arity + 1 = 3 words"
        );
        rows.push(Overhead {
            label: "2-tuple",
            count: N,
            words_consumed: heap.used_words(),
            frozen_words: roots.iter().map(|&t| size_object(t)).sum(),
        });
    }

    // (3) Small boxed values: floats, the smallest boxed leaf (`[HEADER_FLONUM, f64 bits]`).
    {
        let mut heap = space(FLOAT_SIZE_WORDS * N + 8);
        let mut roots = Vec::with_capacity(N);
        for i in 0..N {
            // SAFETY: `heap` owns `FLOAT_SIZE_WORDS` writable words reserved here; the float
            // block is fully written by `write_float` and stays live in `heap`.
            let float = unsafe {
                let hp = heap
                    .heap_mut()
                    .alloc_words(FLOAT_SIZE_WORDS)
                    .expect("reserve a float block");
                write_float(hp, i as f64)
            };
            roots.push(float);
        }
        assert_eq!(
            heap.used_words(),
            FLOAT_SIZE_WORDS * N,
            "a float consumes exactly [HEADER_FLONUM, bits]"
        );
        assert_eq!(
            size_object(roots[0]),
            FLOAT_SIZE_WORDS,
            "the frozen float size is 2 words"
        );
        rows.push(Overhead {
            label: "float (boxed)",
            count: N,
            words_consumed: heap.used_words(),
            frozen_words: roots.iter().map(|&t| size_object(t)).sum(),
        });
    }

    // (4) Small flat maps: `MAP_PAIRS` pairs, each a flat map sized *exactly* by
    //     `flatmap_size_words` (which `map_words_capacity` returns for `<= 32` pairs).
    {
        let atoms = AtomTable::new();
        let cap = map_words_capacity(MAP_PAIRS);
        assert_eq!(
            cap,
            flatmap_size_words(MAP_PAIRS),
            "a small map is a flat map sized exactly by the frozen formula"
        );
        assert_eq!(cap, 4 + 2 * MAP_PAIRS, "the frozen flat-map formula is 4 + 2n");

        let mut heap = space(cap * N + 8);
        let mut roots = Vec::with_capacity(N);
        for i in 0..N {
            let base = (i as i64) * MAP_PAIRS as i64;
            let pairs = [
                (make_small(base), make_small(base)),
                (make_small(base + 1), make_small(base + 1)),
                (make_small(base + 2), make_small(base + 2)),
                (make_small(base + 3), make_small(base + 3)),
            ];
            // SAFETY: `heap` owns `cap` writable words reserved here; `write_map` writes the
            // whole flat-map block and the term stays live in `heap`.
            let map = unsafe {
                let hp = heap
                    .heap_mut()
                    .alloc_words(cap)
                    .expect("reserve a map block");
                write_map(hp, &pairs, &atoms).expect("write a small map")
            };
            roots.push(map);
        }
        assert_eq!(
            heap.used_words(),
            cap * N,
            "a small map consumes its exact reservation"
        );
        assert_eq!(
            size_object(roots[0]),
            flatmap_size_words(MAP_PAIRS),
            "the frozen flat-map size agrees with the layout formula"
        );
        rows.push(Overhead {
            label: "small flat map",
            count: N,
            words_consumed: heap.used_words(),
            frozen_words: roots.iter().map(|&t| size_object(t)).sum(),
        });
    }

    assert_and_report_overhead(&rows);
}

// ---------------------------------------------------------------------------
// 2. Peak RSS / no-leak plateau
// ---------------------------------------------------------------------------

/// Live tuples per cycle (a bounded, non-growing live set).
const LIVE_TUPLES: usize = 256;
/// Element count of each live tuple.
const LIVE_ARITY: usize = 8;
/// The live graph's frozen-layout footprint: `LIVE_TUPLES` tuples of `LIVE_ARITY` elements.
const LIVE_WORDS: usize = LIVE_TUPLES * (LIVE_ARITY + 1);
/// Cycles run before measurement starts (allocator/carrier warm-up).
const RSS_WARMUP: usize = 20;
/// Measured cycles.
const RSS_CYCLES: usize = 120;

/// One cycle's result: the RSS snapshot (when available) and the collector's word accounting.
struct CycleSample {
    rss: Option<usize>,
    mature_used: usize,
    mature_capacity: usize,
    young_capacity: usize,
}

/// Builds a fresh `LIVE_TUPLES`-tuple live graph in the nursery and returns its roots.
fn build_live_graph(collector: &mut Collector) -> Vec<Eterm> {
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
    live
}

#[test]
fn rss_and_word_accounting_plateau_across_cycles() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let rss_available = read_rss_bytes().is_some();
    let rss_before = read_rss_bytes();

    // Every collection is a fullsweep (`fullsweep_after == 0`), so the mature generation is
    // rebuilt from the live set on every cycle: a collector that failed to free the previous
    // mature space would show up as monotonic growth in both RSS and the word accounting.
    let policy = HeapPolicy {
        fullsweep_after: 0,
        initial_young_words: 4096,
        initial_mature_words: 4096,
        ..HeapPolicy::default()
    };
    let mut collector = Collector::new(policy).expect("collector");

    let mut samples: Vec<CycleSample> = Vec::with_capacity(RSS_CYCLES);

    for cycle in 0..(RSS_WARMUP + RSS_CYCLES) {
        // A fresh bounded live graph; a new root set each cycle means the previous cycle's
        // graph is unreachable and must be reclaimed by this collection.
        let mut roots = build_live_graph(&mut collector);
        let _garbage = collector
            .young_mut()
            .heap_mut()
            .tuple(&[make_small(-1)])
            .expect("garbage tuple");

        let collection = collector.collect(&mut roots).expect("collect");
        assert_eq!(
            collection.kind,
            CollectionKind::Major,
            "fullsweep_after == 0 must force a fullsweep on every collection"
        );
        assert_eq!(
            collector.young().used_words(),
            0,
            "the nursery is emptied every cycle (round {cycle})"
        );

        if cycle >= RSS_WARMUP {
            samples.push(CycleSample {
                rss: if rss_available { read_rss_bytes() } else { None },
                mature_used: collector.mature().used_words(),
                mature_capacity: collector.mature().capacity_words(),
                young_capacity: collector.young().capacity_words(),
            });
        }
    }

    // ---- deterministic word-accounting steady state ----
    assert_eq!(samples.len(), RSS_CYCLES, "one sample per measured cycle");

    let mature_used: Vec<usize> = samples.iter().map(|s| s.mature_used).collect();
    let mature_capacity: Vec<usize> = samples.iter().map(|s| s.mature_capacity).collect();
    let young_capacity: Vec<usize> = samples.iter().map(|s| s.young_capacity).collect();

    assert_eq!(
        mature_used[0], LIVE_WORDS,
        "the mature space holds exactly the live graph (no survivor garbage)"
    );
    assert!(
        mature_used.iter().all(|&w| w == LIVE_WORDS),
        "the live set must be constant across cycles: {mature_used:?}"
    );
    let cap_min = *mature_capacity.iter().min().expect("non-empty");
    let cap_max = *mature_capacity.iter().max().expect("non-empty");
    assert_eq!(
        cap_min, cap_max,
        "the mature capacity must reach a steady state: {mature_capacity:?}"
    );
    let young_min = *young_capacity.iter().min().expect("non-empty");
    let young_max = *young_capacity.iter().max().expect("non-empty");
    assert_eq!(
        young_min, young_max,
        "the nursery capacity must reach a steady state: {young_capacity:?}"
    );

    eprintln!(
        "collector word accounting over {RSS_CYCLES} cycles: live = {} words (constant), \
         mature capacity = {cap_min} words (constant), nursery capacity = {young_min} words \
         (constant)",
        LIVE_WORDS
    );

    // ---- RSS plateau ----
    if !rss_available {
        eprintln!(
            "SKIP RSS plateau: `/proc/self/statm` is unavailable, so RSS cannot be measured \
             (the deterministic word-accounting half above already passed)"
        );
        return;
    }

    let rss: Vec<usize> = samples.iter().filter_map(|s| s.rss).collect();
    assert_eq!(
        rss.len(),
        RSS_CYCLES,
        "every measured cycle must yield an RSS snapshot"
    );

    let half = rss.len() / 2;
    let first_half_max = *rss[..half].iter().max().expect("non-empty");
    let first_half_min = *rss[..half].iter().min().expect("non-empty");
    let second_half_max = *rss[half..].iter().max().expect("non-empty");
    let last = *rss.last().expect("non-empty");
    let plateau_factor = second_half_max as f64 / first_half_max as f64;

    eprintln!(
        "rss (bytes): before = {}, first-half min/max = {first_half_min}/{first_half_max}, \
         second-half max = {second_half_max}, last = {last}, plateau factor = \
         {plateau_factor:.3}x",
        rss_before.unwrap_or(0)
    );

    // The second half must stay within a bounded factor (1.5x) of the first half: after
    // warm-up a bounded, non-growing live set may not push RSS up without bound.
    assert!(
        second_half_max <= first_half_max + first_half_max / 2,
        "RSS did not plateau: first-half max {first_half_max} bytes, second-half max \
         {second_half_max} bytes ({plateau_factor:.3}x > 1.5x) -- an allocation cycle is leaking"
    );
    assert!(
        last <= first_half_max + first_half_max / 2,
        "the final RSS {last} bytes exceeds 1.5x the first-half max {first_half_max} bytes"
    );
}
