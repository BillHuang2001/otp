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

//! **Multi-workload measurement** of the GenImmix mark-region mature generation: does the
//! option pay off across the *shapes* a real process shows, or only on the one shape
//! `immix_measure.rs` pins down?
//!
//! `immix_measure.rs` answers the question for a single, deliberately favourable corpus (one
//! long-lived set, one fullsweep). A default-strategy decision needs more than that:
//! `ERTS_RUST_REWRITE_DEEPDIVE.md` §D/§H makes GenImmix **measurement-gated**, and its own
//! Fit matrix warns that Immix alone is "worse for small heaps (per-line metadata)", so the
//! decision has to rest on a shape-by-shape comparison. This file therefore drives **three
//! representative workload shapes** through *both* strategies — [`AnyCollector::Copying`]
//! and [`AnyCollector::Immix`], selected through [`GcConfig`]/[`MatureKind`] — and reports,
//! per shape and per strategy:
//!
//! * the forced fullsweep's [`GcStats`] (`copied`/`promoted`, `reclaimed`, garbage);
//! * the mark-region reclaim's own [`ImmixStats`] (`evacuated` vs `pinned`, blocks/words
//!   reclaimed);
//! * the resident mature footprint (whole lines for the mark-region space);
//! * the **median pause** of [`REPS`] timed forced fullsweeps.
//!
//! # The three shapes
//!
//! 1. [`long_lived_shared`] — **the regime GenImmix targets**: one large shared long-lived
//!    subterm referenced from many roots. Both live in full Immix blocks, so the mark-region
//!    path pins everything and moves *nothing* where the copying path re-copies the whole
//!    live set.
//! 2. [`garbage_heavy`] — **copying's regime**: thousands of short-lived words against a
//!    tiny live set that occupies a *partial* block. There is almost nothing to pin, so the
//!    mark-region path pays its line rounding for nothing — the shape where Immix loses.
//! 3. [`large_heap_many_objects`] — a large live set spread over many full blocks **plus
//!    promotion pressure**: a live young set (including a >32-key HAMT map, a `Local`
//!    closure and a refc-binary pair) that the measured fullsweep still has to promote.
//!
//! # Determinism, and honesty about the timing column
//!
//! Every assertion below is on an exact or relational **count**: both strategies must find
//! the same live set, the copying path's whole-live-set copy is pinned to the corpus's exact
//! word count, the garbage reclaimed must be identical, and the reachable graphs must be
//! structurally identical (checked with the crate's own `beam_term::compare`). Nothing is
//! asserted about the timings — they are printed only.
//!
//! The pause time is an **in-process** median over [`REPS`] runs of the same deterministic
//! corpus, timed with [`std::time::Instant`] around the forced fullsweep alone. It is a
//! rough signal for a direction of magnitude, **not a benchmark**: no warm-up control, no
//! cross-process isolation, allocator and scheduler noise, and each run builds its spaces
//! from scratch. It is reported as µs because the absolute numbers are meaningless; only
//! the copying-vs-mark-region ratio on the same shape carries information.
//!
//! # Measured outcome (run with `-- --nocapture` for the full table)
//!
//! The counts are exact and reproduce on every run; the medians below are the stable
//! 10 µs-scale readings this sandbox gives, quoted as ranges observed over repeated runs.
//!
//! | workload | moved (copying) | moved (immix) | footprint (copying) | footprint (immix) | pause (copying) | pause (immix) |
//! |---|---|---|---|---|---|---|
//! | 1 long-lived shared | 129 obj / 639 words | **0 obj / 0 words** (129 pinned) | 639 words | 2 304 line-words | 8–10 µs | 11–12 µs |
//! | 2 garbage-heavy | 10 obj / 20 words | 10 obj / **160 line-words** | **20 words** | 160 line-words | 4–5 µs | **2 µs** |
//! | 3 large heap + promotion | 633 obj / 1 824 words | **121 obj / 288 words** (512 pinned) | **1 824 words** | 10 128 line-words | **19–27 µs** | 47–55 µs |
//!
//! Read it as: the mark-region path does **strictly less copying work on the long-lived
//! shapes** (5.2× fewer objects on workload 3, *all* of the mature set pinned on workload 1)
//! but pays for it with **3.6×–8× the resident footprint** (line-granular allocation: a
//! 2-word cons cell occupies a whole 16-word line) and, in this Milestone-1 implementation,
//! a **higher per-collection pause on every workload where it saves work** — its reclaim
//! marks the whole root set, scans every line of the region and walks the nursery twice
//! (once in `scan_young`, once in the promotion), while a copying fullsweep only touches the
//! live words it moves. Workload 2 is the shape where it *loses* outright: the live set is
//! tiny and sits in a partial block, so nothing is pinned and the same 10 objects are
//! evacuated at 8× the volume and 8× the footprint.
//!
//! So on these numbers the option does **not** clear the bar for becoming the default: it
//! trades a real reduction in copy work for a real increase in memory and pause. That is the
//! decision this file exists to inform; the crate's `CONTEXT.md` records the call.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that a
//! collection just wrote); every block carries a `// SAFETY:` rationale.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use beam_alloc::{Heap, RefcBinary};
use beam_gc::prelude::{
    AnyCollector, GcConfig, GcStats, HeapPolicy, ImmixStats, MatureKind, LINE_WORDS,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{
    fun_words, make_small, map_words_capacity, refc_bytes, refc_size, write_fun_local, write_map,
    write_refc_binary, AtomTable, Eterm, FunKind, NIL, ERL_REFC_BITS_SIZE,
};

// ---------------------------------------------------------------------------
// Workload geometry (fixed; no randomness anywhere in this file)
// ---------------------------------------------------------------------------

/// Timed repetitions of the forced fullsweep per (workload, strategy); the MEDIAN is
/// reported.
const REPS: usize = 5;

/// Serialises the workloads. The test harness runs test functions in parallel threads, and a
/// microsecond-resolution in-process timing is meaningless while another workload's
/// allocation runs concurrently on the same machine. Holding this for a whole measurement
/// makes the pause column comparable; at this scale it costs nothing. A poisoned lock is
/// deliberately ignored (`unwrap_or_else(into_inner)`): the guard still gives mutual
/// exclusion, and one failing workload must not cascade into the others.
static SERIAL: Mutex<()> = Mutex::new(());

/// Takes the measurement lock for the rest of a test.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// **Workload 1** — the shared long-lived tuple: 254 elements ⇒ 255 words, which is exactly
/// one 16-line Immix block.
const SHARED_ARITY: usize = 254;
/// **Workload 1** — the rows referencing the shared subterm: one 3-word tuple each, i.e. one
/// line, so 128 rows fill exactly eight Immix blocks.
const SHARED_ROWS: usize = 128;
/// **Workload 1** — short-lived 3-word tuples allocated just before the measured fullsweep.
const GARBAGE_A: usize = 400;

/// **Workload 2** — the whole live set: ten 1-line cons cells, i.e. a *partial* block (10 of
/// its 16 lines), so the mark-region reclaim has nothing to pin.
const LIVE_B: usize = 10;
/// **Workload 2** — short-lived 3-word tuples: 6 000 words of garbage against 20 live words.
const GARBAGE_B: usize = 2000;

/// **Workload 3** — the long-lived set: 512 one-line tuples, i.e. exactly 32 full blocks.
const LIVE_C: usize = 512;
/// **Workload 3** — live young tuples the measured fullsweep must promote (64 lines).
const PROMO_C: usize = 64;
/// **Workload 3** — map keys, above `MAP_SMALL_MAP_LIMIT` (32) so the HAMT interior is used.
const HAMT_KEYS: usize = 40;
/// **Workload 3** — short-lived 3-word tuples: 4 500 words of garbage.
const GARBAGE_C: usize = 1500;

// ---------------------------------------------------------------------------
// Shared scaffolding
// ---------------------------------------------------------------------------

/// The heap policy both strategies share: identical sizes, and the fullsweep cadence
/// disabled so every collection is one this file explicitly asked for — the measurement must
/// not depend on which collection a policy happened to pick.
///
/// The fullsweep now sizes the fresh region from the **exact live word count**, so
/// workload 3's 64-block region (larger than its ~1536-word live set) would otherwise be shrunk
/// and relocated out from under the measurement. `min_heap_words` is pinned to the same
/// 64 blocks, so `capacity == min_heap_words` and `should_shrink` can never fire: the region
/// keeps all 32 live blocks **and** room for the promotion the workload is about.
fn policy() -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: 65535,
        initial_young_words: 8192,
        // 64 Immix blocks: enough for workload 3's 32 live blocks *and* its promotion.
        initial_mature_words: 16384,
        min_heap_words: 16384,
        ..HeapPolicy::default()
    }
}

/// A collector for `kind`, selected through [`GcConfig`] exactly as a caller would.
fn collector(kind: MatureKind) -> AnyCollector {
    let config = GcConfig::copying(policy()).with_mature(kind);
    AnyCollector::from_config(&config).expect("collector")
}

/// Runs a forced fullsweep and returns its [`GcStats`] plus, for the mark-region path, the
/// in-place reclaim's own accounting.
fn fullsweep(c: &mut AnyCollector, roots: &mut [Eterm]) -> (GcStats, Option<ImmixStats>) {
    match c {
        AnyCollector::Copying(x) => (
            x.collect_major(roots).expect("the measured fullsweep"),
            None,
        ),
        AnyCollector::Immix(x) => {
            let stats = x.collect_major(roots).expect("the measured fullsweep");
            (stats, x.last_immix_stats())
        }
    }
}

/// Promotes the corpus's pre-phase set into the mature generation before the measured
/// fullsweep: the same forced fullsweep, whose numbers the pre-phases do not measure.
fn promote(c: &mut AnyCollector, roots: &mut [Eterm]) {
    let _ = fullsweep(c, roots);
}

/// The mature generation's resident footprint after the fullsweep, in the strategy's own
/// accounting (whole lines for the mark-region space: its `used_words` *is* line-rounded).
fn mature_words(c: &AnyCollector) -> usize {
    match c {
        AnyCollector::Copying(x) => x.mature().used_words(),
        AnyCollector::Immix(x) => x.mature().used_words(),
    }
}

/// The mark-region space's resident line count (`None` for the copying strategy), so the
/// internal fragmentation its `used_words` over-states is visible in the report.
fn mature_lines(c: &AnyCollector) -> Option<usize> {
    match c {
        AnyCollector::Copying(_) => None,
        AnyCollector::Immix(x) => Some(x.mature().used_lines()),
    }
}

/// Microseconds of `d`, for the reported pause column.
fn micros(d: Duration) -> u64 {
    d.as_micros() as u64
}

/// Reserves [`map_words_capacity`]`(n)` words — the sound upper bound — and writes the
/// `i -> 3*i` map there. (Test-local scaffolding, mirroring `tests/immix_parity.rs`, whose
/// corpus this file extends; `tests/` are separate crates, so there is no shared module.)
fn write_map_n(heap: &mut Heap, atoms: &AtomTable, n: usize) -> Eterm {
    let cap = map_words_capacity(n);
    let input: Vec<(Eterm, Eterm)> = (0..n as i64)
        .map(|i| (make_small(i), make_small(3 * i)))
        .collect();
    // SAFETY: `cap` exclusively-owned, writable words are reserved; `write_map` writes a
    // contiguous prefix of them with well-formed terms and returns a root into that prefix.
    unsafe {
        let hp = heap.alloc_words(cap).expect("reserve the map block");
        write_map(hp, &input, atoms).expect("build the map")
    }
}

/// Writes a `Local` fun (`[header][uniq][index][env…]`) into `heap`.
fn write_local(heap: &mut Heap, arity: usize, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
    let words = fun_words(FunKind::Local, env.len());
    // SAFETY: `words` exclusively-owned, writable words are reserved and all of them are
    // written by `write_fun_local`; every `env` entry is a live term of this heap.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve the local fun");
        write_fun_local(hp, arity, uniq, index, env).expect("write the local fun")
    }
}

/// Writes the 8-word `[BinRef][ErlSubBits]` refc-binary pair for `payload` into `heap`.
fn write_refc(heap: &mut Heap, payload: &RefcBinary) -> Eterm {
    let size_bits = payload.len() * 8;
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved;
    // `payload.data_ptr()` is a live, word-aligned pointer to `size_bits` bits of off-heap
    // storage kept alive by `payload` (held by the caller for the whole run), and `null` is
    // the (empty) off-heap chain link.
    unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).expect("8 words for the pair");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            0,
            size_bits,
        )
    }
}

// ---------------------------------------------------------------------------
// The measured result
// ---------------------------------------------------------------------------

/// The measured result of one forced fullsweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Measurement {
    kind: MatureKind,
    /// `GcStats` of the forced fullsweep. For the copying strategy `objects_copied` /
    /// `words_copied` are the **whole live set** (a fullsweep re-copies the mature
    /// generation into a fresh space); for the mark-region path they are only the
    /// **nursery promotion**.
    stats: GcStats,
    /// The mark-region reclaim's own accounting (`None` for the copying strategy).
    immix: Option<ImmixStats>,
    /// The mature generation's resident footprint after the fullsweep.
    mature_words: usize,
    /// The mark-region space's resident line count (`None` for the copying strategy).
    mature_lines: Option<usize>,
    /// Nursery words that were dead at the fullsweep (never copied). Measured *before* the
    /// collection, so it is identical for both strategies — the allocation sequence is.
    nursery_garbage: usize,
    /// The MEDIAN wall-clock duration of the forced fullsweep, in microseconds. Reported
    /// only; never asserted on.
    pause_us: u64,
}

impl Measurement {
    /// Whether two runs did exactly the same *work* (everything except the timing sample).
    fn same_work(&self, other: &Measurement) -> bool {
        self.kind == other.kind
            && self.stats == other.stats
            && self.immix == other.immix
            && self.mature_words == other.mature_words
            && self.mature_lines == other.mature_lines
            && self.nursery_garbage == other.nursery_garbage
    }

    /// Objects this strategy actually **moved** during the fullsweep: the copying path's
    /// copies, or the mark-region reclaim's evacuations *plus* the nursery promotion it
    /// still had to copy.
    fn objects_moved(&self) -> usize {
        match self.immix {
            Some(immix) => immix.objects_evacuated + self.stats.objects_copied,
            None => self.stats.objects_copied,
        }
    }

    /// Words this strategy actually **moved**. The mark-region figure is line-rounded
    /// (`lines_evacuated * LINE_WORDS`), which is an upper bound on the real volume — a
    /// 2-word cons cell occupies a whole 16-word line.
    fn words_moved(&self) -> usize {
        match self.immix {
            Some(immix) => immix.lines_evacuated * LINE_WORDS + self.stats.words_copied,
            None => self.stats.words_copied,
        }
    }

    /// Objects the strategy **left where they were**.
    fn objects_left_in_place(&self) -> usize {
        match self.immix {
            Some(immix) => immix.objects_pinned,
            None => 0,
        }
    }

    /// The strategy's own footprint unit suffix: exact words, or line-rounded words with the
    /// line count it rounds to.
    fn footprint(&self) -> String {
        match self.mature_lines {
            Some(lines) => format!("{} line-words ({} lines)", self.mature_words, lines),
            None => format!("{} words", self.mature_words),
        }
    }

    /// The strategy-specific half of the report line.
    fn detail(&self) -> String {
        match self.immix {
            Some(i) => format!(
                "promoted {} obj / {} words | evacuated {} obj / {} lines / {} line-words | \
                 pinned {} obj / {} lines / {} words | blocks_reclaimed {}",
                self.stats.objects_copied,
                self.stats.words_copied,
                i.objects_evacuated,
                i.lines_evacuated,
                i.lines_evacuated * LINE_WORDS,
                i.objects_pinned,
                i.lines_pinned,
                i.words_pinned,
                i.blocks_reclaimed
            ),
            None => format!(
                "copied {} obj / {} words",
                self.stats.objects_copied, self.stats.words_copied
            ),
        }
    }

    /// One compact report line.
    fn report(&self) -> String {
        format!(
            "  {:?}: {} | reclaimed {} words | mature footprint {} | nursery garbage {} words \
             | moved {} obj / {} words | left in place {} obj | pause {} µs",
            self.kind,
            self.detail(),
            self.stats.words_reclaimed,
            self.footprint(),
            self.nursery_garbage,
            self.objects_moved(),
            self.words_moved(),
            self.objects_left_in_place(),
            self.pause_us
        )
    }
}

/// One workload run: the measurement plus the live state it produced.
struct WorkloadRun {
    measurement: Measurement,
    /// Kept alive so `roots` stay valid: they are raw `Eterm` pointers straight into this
    /// collector's own spaces, so it must outlive every use of them.
    collector: AnyCollector,
    /// The surviving roots, in the order the workload allocated them.
    roots: Vec<Eterm>,
    /// Off-heap refc-binary payloads the roots reference. An on-heap refc term does not own
    /// its payload, so the payload must outlive both the collector and the comparison.
    held: Vec<RefcBinary>,
    /// Index into `roots` of the refc-binary root, when the workload has one — compared
    /// field by field rather than with `beam_term::compare` (see [`assert_same_graph`]).
    refc_root: Option<usize>,
}

impl WorkloadRun {
    /// The strategy this run was measured on.
    fn kind(&self) -> MatureKind {
        self.collector.kind()
    }
}

/// Runs the workload `REPS` times for `kind` and returns the **last** run, with its pause
/// replaced by the median of the samples — after asserting that every repetition did exactly
/// the same work (the corpus is deterministic; only the timing may vary).
fn measure_reps(kind: MatureKind, run: impl Fn(MatureKind) -> WorkloadRun) -> WorkloadRun {
    let mut last = run(kind);
    assert_eq!(last.kind(), kind, "the config selected the requested strategy");
    let mut samples: Vec<u64> = Vec::with_capacity(REPS);
    samples.push(last.measurement.pause_us);
    for rep in 1..REPS {
        let next = run(kind);
        assert_eq!(next.kind(), kind, "the config selected {kind:?}");
        assert!(
            next.measurement.same_work(&last.measurement),
            "{kind:?}: repetition {rep} must do identical work:\n  {:?}\n  {:?}",
            last.measurement,
            next.measurement
        );
        samples.push(next.measurement.pause_us);
        last = next;
    }
    samples.sort_unstable();
    last.measurement.pause_us = samples[REPS / 2];
    last
}

/// Asserts the reachable graph is **identical** under both strategies, using the crate's own
/// structural term comparison.
///
/// The refc-binary root is compared field by field instead: `beam_term::compare` classifies
/// a refc binary as `BinRef`, for which it falls back to a *raw-pointer* comparison
/// (`compare::fallback_cmp`) — which can never hold across two different spaces, even when
/// the two terms are the same binary.
fn assert_same_graph(copying: &WorkloadRun, immix: &WorkloadRun, atoms: &AtomTable) {
    assert_eq!(copying.roots.len(), immix.roots.len(), "root count");
    for (i, (a, b)) in copying.roots.iter().zip(&immix.roots).enumerate() {
        if Some(i) == copying.refc_root {
            // SAFETY: both roots are live refc-binary terms whose off-heap payloads are kept
            // alive by the `held` field of the run that owns them.
            let (sa, ba) = unsafe { (refc_size(*a), refc_bytes(*a)) };
            // SAFETY: as above, for the other strategy's root.
            let (sb, bb) = unsafe { (refc_size(*b), refc_bytes(*b)) };
            assert_eq!(sa, sb, "root {i}: refc binary size");
            assert_eq!(ba, bb, "root {i}: refc binary payload bytes");
        } else {
            assert!(term_eq(*a, *b, atoms), "root {i} differs across strategies");
        }
    }
}

/// Prints one workload's measurement table.
fn report(title: &str, copying: &Measurement, immix: &Measurement) {
    println!("\n=== {title} ===");
    println!("{}", copying.report());
    println!("{}", immix.report());
    println!(
        "  comparison: copying moved {} obj / {} words vs immix {} obj / {} words (immix left \
         {} obj in place); footprint {} vs {}",
        copying.objects_moved(),
        copying.words_moved(),
        immix.objects_moved(),
        immix.words_moved(),
        immix.objects_left_in_place(),
        copying.footprint(),
        immix.footprint()
    );
}

// ---------------------------------------------------------------------------
// Workload 1 — a large shared long-lived subterm referenced from many roots
// ---------------------------------------------------------------------------

/// The regime GenImmix targets: one big shared structure plus a row per reference, both
/// living in *full* Immix blocks.
///
/// Phase 1 promotes the long-lived set; phase 2 offers the nursery only garbage; phase 3 is
/// the measured fullsweep. Every mature block is full, so the mark-region path pins the whole
/// live set while the copying path re-copies all of it into a fresh space.
fn long_lived_shared(kind: MatureKind) -> WorkloadRun {
    let mut c = collector(kind);
    let mut roots: Vec<Eterm> = Vec::with_capacity(1 + SHARED_ROWS);

    // ---- phase 1: the shared subterm and the rows that reference it --------------------
    {
        let heap = c.young_mut().heap_mut();
        let elems: Vec<Eterm> = (0..SHARED_ARITY as i64).map(make_small).collect();
        roots.push(heap.tuple(&elems).expect("the shared long-lived tuple"));
    }
    {
        let heap = c.young_mut().heap_mut();
        let shared = roots[0];
        for i in 0..SHARED_ROWS as i64 {
            roots.push(
                heap.tuple(&[shared, make_small(i)])
                    .expect("a row referencing the shared subterm"),
            );
        }
    }
    promote(&mut c, &mut roots);
    let shared_after_promotion = roots[0].raw();

    // ---- phase 2: nothing but garbage in the nursery -----------------------------------
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..GARBAGE_A as i64 {
            let _ = heap
                .tuple(&[make_small(i), make_small(2 * i)])
                .expect("garbage tuple");
        }
    }
    let nursery_garbage = c.young().used_words();

    // ---- phase 3: the measured fullsweep ------------------------------------------------
    let start = Instant::now();
    let (stats, immix) = fullsweep(&mut c, &mut roots);
    let pause_us = micros(start.elapsed());

    let measurement = Measurement {
        kind,
        stats,
        immix,
        mature_words: mature_words(&c),
        mature_lines: mature_lines(&c),
        nursery_garbage,
        pause_us,
    };
    assert_eq!(c.young().used_words(), 0, "the nursery is emptied");

    // The pinning claim in address form: every mature block is full, so the mark-region path
    // leaves the shared subterm exactly where it was, while the copying fullsweep re-copies
    // every live word into a fresh space (allocated while the old one is still live, so the
    // address cannot be the same).
    let shared_after_fullsweep = roots[0].raw();
    match kind {
        MatureKind::Immix => assert_eq!(
            shared_after_fullsweep, shared_after_promotion,
            "a full block pins the shared subterm: it must not move"
        ),
        MatureKind::Copying => assert_ne!(
            shared_after_fullsweep, shared_after_promotion,
            "the copying fullsweep re-copies every live word"
        ),
    }

    WorkloadRun {
        measurement,
        collector: c,
        roots,
        held: Vec::new(),
        refc_root: None,
    }
}

#[test]
fn long_lived_shared_the_mark_region_pins_the_whole_live_set() {
    let _serial = serial();
    let copying = measure_reps(MatureKind::Copying, long_lived_shared);
    let immix = measure_reps(MatureKind::Immix, long_lived_shared);
    // Both collectors are alive at once, so both root slices are valid and comparable.
    assert_same_graph(&copying, &immix, &AtomTable::new());
    report(
        "workload 1: long_lived_shared — one large shared subterm referenced from 128 roots",
        &copying.measurement,
        &immix.measurement,
    );

    // Every long-lived object occupies one full line, so the live set spans exactly the
    // shared tuple's lines plus one line per row.
    let live_lines = (SHARED_ARITY + 1).div_ceil(LINE_WORDS) + SHARED_ROWS;
    let live_words = (SHARED_ARITY + 1) + SHARED_ROWS * 3;
    let live_objects = 1 + SHARED_ROWS;
    assert_eq!(live_lines, 16 + 128, "the corpus blocks as designed");
    assert_eq!(live_words, 255 + 384);

    // --- the copying fullsweep re-copies the entire live set ---------------------------
    assert_eq!(copying.measurement.stats.objects_copied, live_objects);
    assert_eq!(copying.measurement.stats.words_copied, live_words);
    assert_eq!(
        copying.measurement.stats.words_reclaimed,
        GARBAGE_A * 3,
        "the copying fullsweep reclaims exactly the nursery garbage"
    );

    // --- the mark-region fullsweep moves nothing at all --------------------------------
    let immix_stats = immix.measurement.immix.expect("an immix fullsweep");
    assert_eq!(
        immix_stats.objects_evacuated + immix_stats.objects_pinned,
        copying.measurement.stats.objects_copied,
        "both strategies find the same live mature set"
    );
    assert_eq!(immix_stats.objects_evacuated, 0, "no partial block exists");
    assert_eq!(immix_stats.objects_pinned, live_objects);
    assert_eq!(immix_stats.lines_pinned, live_lines);
    assert_eq!(immix_stats.words_pinned, live_words);
    assert_eq!(immix_stats.blocks_reclaimed, 0, "every block stays live");
    assert_eq!(immix.measurement.stats.objects_copied, 0, "nothing to promote");
    assert_eq!(immix.measurement.objects_moved(), 0);

    // --- the comparison the decision rests on ------------------------------------------
    assert!(
        immix.measurement.objects_moved() < copying.measurement.objects_moved(),
        "immix must move strictly fewer objects on this shape: {} vs {}",
        immix.measurement.objects_moved(),
        copying.measurement.objects_moved()
    );
    assert!(immix.measurement.words_moved() < copying.measurement.words_moved());
    assert_eq!(
        immix.measurement.stats.words_reclaimed, copying.measurement.stats.words_reclaimed,
        "both strategies reclaim the same garbage"
    );
    assert_eq!(
        immix.measurement.nursery_garbage, copying.measurement.nursery_garbage
    );

    // --- the honest caveat: line-granular allocation over-states the footprint ----------
    assert_eq!(copying.measurement.mature_words, live_words);
    assert_eq!(copying.measurement.mature_lines, None);
    assert_eq!(immix.measurement.mature_words, live_lines * LINE_WORDS);
    assert_eq!(immix.measurement.mature_lines, Some(live_lines));
    assert!(
        immix.measurement.mature_words > live_words,
        "the pinned lines are whole 16-word lines, so the resident footprint is larger"
    );
}

// ---------------------------------------------------------------------------
// Workload 2 — garbage-heavy allocation with a tiny live set
// ---------------------------------------------------------------------------

/// Copying's regime: 6 000 words of short-lived garbage against a 20-word live set that
/// occupies a *partial* block, so the mark-region path has nothing to pin and pays its line
/// rounding for nothing.
fn garbage_heavy(kind: MatureKind) -> WorkloadRun {
    let mut c = collector(kind);
    let mut roots: Vec<Eterm> = Vec::with_capacity(LIVE_B);

    // ---- phase 1: the small live set survives to the mature generation -----------------
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..LIVE_B as i64 {
            roots.push(heap.cons(make_small(i), NIL).expect("a live cons cell"));
        }
    }
    promote(&mut c, &mut roots);

    // ---- phase 2: heavy short-lived allocation -----------------------------------------
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..GARBAGE_B as i64 {
            let _ = heap
                .tuple(&[make_small(i), make_small(2 * i)])
                .expect("garbage tuple");
        }
    }
    let nursery_garbage = c.young().used_words();

    // ---- phase 3: the measured fullsweep ------------------------------------------------
    let start = Instant::now();
    let (stats, immix) = fullsweep(&mut c, &mut roots);
    let pause_us = micros(start.elapsed());

    let measurement = Measurement {
        kind,
        stats,
        immix,
        mature_words: mature_words(&c),
        mature_lines: mature_lines(&c),
        nursery_garbage,
        pause_us,
    };
    assert_eq!(c.young().used_words(), 0, "the nursery is emptied");

    WorkloadRun {
        measurement,
        collector: c,
        roots,
        held: Vec::new(),
        refc_root: None,
    }
}

#[test]
fn garbage_heavy_the_mark_region_pays_its_line_rounding_for_nothing() {
    let _serial = serial();
    let copying = measure_reps(MatureKind::Copying, garbage_heavy);
    let immix = measure_reps(MatureKind::Immix, garbage_heavy);
    assert_same_graph(&copying, &immix, &AtomTable::new());
    report(
        "workload 2: garbage_heavy — 6 000 words of short-lived garbage, a 20-word live set",
        &copying.measurement,
        &immix.measurement,
    );

    let live_words = LIVE_B * 2;
    let live_lines = LIVE_B; // one 1-line cons cell each

    // --- both paths move the same tiny live set ----------------------------------------
    assert_eq!(copying.measurement.stats.objects_copied, LIVE_B);
    assert_eq!(copying.measurement.stats.words_copied, live_words);
    assert_eq!(copying.measurement.stats.words_reclaimed, GARBAGE_B * 3);

    let immix_stats = immix.measurement.immix.expect("an immix fullsweep");
    assert_eq!(immix_stats.objects_evacuated, LIVE_B);
    assert_eq!(
        immix_stats.objects_pinned, 0,
        "the live set is in a partial block: nothing is pinned"
    );
    assert_eq!(immix_stats.lines_evacuated, live_lines);
    assert_eq!(immix.measurement.stats.objects_copied, 0, "nothing to promote");
    assert_eq!(
        immix_stats.objects_evacuated + immix_stats.objects_pinned,
        copying.measurement.stats.objects_copied,
        "both strategies find the same live set"
    );

    // --- the honest result: the same object count, but far more moved *volume* ---------
    assert_eq!(
        immix.measurement.objects_moved(),
        copying.measurement.objects_moved(),
        "neither strategy saves an object on this shape"
    );
    assert!(
        immix.measurement.words_moved() > copying.measurement.words_moved(),
        "line rounding makes the mark-region path move more volume: {} vs {}",
        immix.measurement.words_moved(),
        copying.measurement.words_moved()
    );
    assert_eq!(immix.measurement.words_moved(), live_lines * LINE_WORDS);

    // --- and a much larger resident footprint ------------------------------------------
    assert_eq!(copying.measurement.mature_words, live_words);
    assert_eq!(copying.measurement.mature_lines, None);
    assert_eq!(immix.measurement.mature_words, live_lines * LINE_WORDS);
    assert_eq!(immix.measurement.mature_lines, Some(live_lines));
    assert!(
        immix.measurement.mature_words > copying.measurement.mature_words,
        "10 live lines cost {} line-words against {} exact words",
        immix.measurement.mature_words,
        copying.measurement.mature_words
    );

    assert_eq!(
        immix.measurement.stats.words_reclaimed, copying.measurement.stats.words_reclaimed,
        "both strategies reclaim the same 6 000 words of nursery garbage"
    );
    assert_eq!(
        immix.measurement.nursery_garbage, copying.measurement.nursery_garbage
    );
}

// ---------------------------------------------------------------------------
// Workload 3 — a large live set over many blocks, plus promotion pressure
// ---------------------------------------------------------------------------

/// A large live object set (32 full Immix blocks) *plus* promotion pressure: a live young
/// set — including a >32-key HAMT map, a `Local` closure and a refc-binary pair — that the
/// measured fullsweep still has to copy, whichever strategy runs.
fn large_heap_many_objects(kind: MatureKind) -> WorkloadRun {
    let mut c = collector(kind);
    let mut roots: Vec<Eterm> = Vec::with_capacity(LIVE_C + PROMO_C + 3);

    // ---- phase 1: the large long-lived set survives to the mature generation -----------
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..LIVE_C as i64 {
            roots.push(
                heap.tuple(&[make_small(i), make_small(i + 1)])
                    .expect("a long-lived tuple"),
            );
        }
    }
    promote(&mut c, &mut roots);

    // ---- phase 2: promotion pressure + heavy garbage ------------------------------------
    let atoms = AtomTable::new();
    let payload = RefcBinary::new(b"immix-workloads refc payload").expect("refc payload");
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..PROMO_C as i64 {
            roots.push(
                heap.tuple(&[make_small(10_000 + i)])
                    .expect("a live young tuple"),
            );
        }
        roots.push(write_map_n(heap, &atoms, HAMT_KEYS));
        roots.push(write_local(heap, 2, 0xABCD_1234, 7, &[make_small(42)]));
        roots.push(write_refc(heap, &payload));
    }
    let refc_root = Some(roots.len() - 1);
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..GARBAGE_C as i64 {
            let _ = heap
                .tuple(&[make_small(i), make_small(2 * i)])
                .expect("garbage tuple");
        }
    }
    let nursery_garbage = c.young().used_words();

    // ---- phase 3: the measured fullsweep ------------------------------------------------
    let start = Instant::now();
    let (stats, immix) = fullsweep(&mut c, &mut roots);
    let pause_us = micros(start.elapsed());

    let measurement = Measurement {
        kind,
        stats,
        immix,
        mature_words: mature_words(&c),
        mature_lines: mature_lines(&c),
        nursery_garbage,
        pause_us,
    };
    assert_eq!(c.young().used_words(), 0, "the nursery is emptied");

    WorkloadRun {
        measurement,
        collector: c,
        roots,
        held: vec![payload],
        refc_root,
    }
}

#[test]
fn large_heap_many_objects_pins_the_live_blocks_and_still_promotes() {
    let _serial = serial();
    let copying = measure_reps(MatureKind::Copying, large_heap_many_objects);
    let immix = measure_reps(MatureKind::Immix, large_heap_many_objects);
    assert_same_graph(&copying, &immix, &AtomTable::new());
    report(
        "workload 3: large_heap_many_objects — 512 long-lived tuples (32 full blocks) + a live \
         young set the fullsweep must promote",
        &copying.measurement,
        &immix.measurement,
    );

    // The off-heap payload is owned by the *run*, not by the collector: the roots' `orig`
    // pointers are only valid while it is held.
    assert_eq!(copying.held.len(), 1);
    assert_eq!(immix.held.len(), 1);
    assert!(copying.refc_root.is_some() && copying.refc_root == immix.refc_root);

    let live_words = LIVE_C * 3;
    assert_eq!(live_words, 1536);

    // --- the copying fullsweep copies the mature set *and* the promotion -----------------
    let copying_stats = copying.measurement.stats;
    assert!(
        copying_stats.objects_copied > LIVE_C + PROMO_C,
        "{} live mature objects plus the promoted young set (and the map/fun/refc objects)",
        LIVE_C
    );
    assert!(copying_stats.words_copied > live_words + PROMO_C * 2);

    // --- the mark-region fullsweep pins all 32 blocks and only promotes the nursery ------
    let immix_stats = immix.measurement.immix.expect("an immix fullsweep");
    assert_eq!(immix_stats.objects_evacuated, 0, "every block is full");
    assert_eq!(immix_stats.objects_pinned, LIVE_C);
    assert_eq!(immix_stats.lines_pinned, LIVE_C, "one line per long-lived tuple");
    assert_eq!(immix_stats.words_pinned, live_words);
    assert_eq!(immix_stats.blocks_reclaimed, 0);

    // The nursery live set is *identical* for both strategies, so the copying path's copy
    // count minus the mature set must equal the mark-region path's promotion exactly.
    assert_eq!(
        immix.measurement.stats.objects_copied + LIVE_C,
        copying_stats.objects_copied,
        "both strategies copy the same live nursery graph"
    );
    assert_eq!(
        immix.measurement.stats.words_copied + live_words,
        copying_stats.words_copied,
        "and it is the same volume of live words"
    );
    assert!(immix.measurement.stats.objects_copied > 0, "promotion pressure is real");

    // --- the comparison the decision rests on ------------------------------------------
    assert!(
        immix.measurement.objects_moved() < copying.measurement.objects_moved(),
        "immix must move strictly fewer objects: {} vs {}",
        immix.measurement.objects_moved(),
        copying.measurement.objects_moved()
    );
    assert_eq!(
        immix.measurement.stats.words_reclaimed, copying_stats.words_reclaimed,
        "both strategies reclaim the same garbage"
    );
    assert_eq!(
        immix.measurement.nursery_garbage, copying.measurement.nursery_garbage
    );

    // --- the footprint trade -------------------------------------------------------------
    assert_eq!(copying.measurement.mature_words, copying_stats.words_copied);
    assert_eq!(copying.measurement.mature_lines, None);
    assert!(
        immix.measurement.mature_words > copying.measurement.mature_words,
        "the pinned blocks plus the line-rounded promotion occupy more than the exact copy: \
         {} vs {}",
        immix.measurement.mature_words,
        copying.measurement.mature_words
    );
    assert!(
        immix.measurement.mature_words >= live_words + immix.measurement.stats.words_copied,
        "line-granular allocation can only over-state the live words"
    );
    assert!(
        immix.measurement.mature_lines.expect("an immix mature space") > LIVE_C,
        "the 512 pinned lines plus the promotion's lines are resident"
    );
}
