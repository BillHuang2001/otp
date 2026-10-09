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

//! **Mature-generation selection curve** — a two-axis measurement sweep that probes the
//! automatic rule's break-even ([`IMMIX_BREAK_EVEN_WORDS`], `src/selection.rs`) along the two
//! axes the existing measurement files do **not** cover.
//!
//! `immix_break_even.rs` sweeps the *live-set size* of one object shape; `immix_workloads.rs`
//! measures three shapes at one size each; `immix_measure.rs` measures one favourable workload.
//! This file asks the two questions the rule's *input* never sees:
//!
//! # Axis 1 — object-size distribution (fixed, in-place mature size)
//!
//! The rule is a pure function of the *configured* mature size in **words**
//! ([`mature_words_of`](beam_gc::prelude::mature_words_of) = `policy.initial_mature_words`). But
//! the mark-region space allocates in whole 16-word **lines**, so the *useful* payload a given
//! word budget can hold depends entirely on the object-size distribution living in it. This axis
//! fixes the mature capacity and the live-set **line** count (8 whole blocks = the break-even)
//! and sweeps five distributions: cons cells (2 words), small tuples (3 words), line-sized
//! tuples (16 words), multi-line tuples (17 words) and a **mixed** corpus that mixes all four in
//! one live set (packed so no object straddles a block boundary). For each it reports, per
//! strategy, the moved words/objects, the resident footprint (line-words for the mark-region
//! space) and the pause of the fastest of [`REPS`] forced fullsweeps.
//!
//! # Axis 2 — configured mature size / occupancy (fixed live set)
//!
//! The rule switches to the mark-region space at `IMMIX_BREAK_EVEN_WORDS` configured words. This
//! axis fixes one long-lived live set (40 one-line objects = 2 whole blocks + a trailing partial
//! block) and sweeps the configured mature capacity `initial_mature_words` from *below* one
//! whole block, through the break-even (~8 blocks), up to a loose/oversized generation, so the
//! occupancy ratio (live line-words ÷ resident capacity) runs from near-full to sparse. For each
//! point it reports how moved words, resident footprint and pause change **for both strategies**,
//! whether the automatic rule (`MatureSelection::Auto`) would select the mark-region space, and
//! whether the fullsweep took the **in-place reclaim** path or the **relocating** path
//! (`GenImmixCollector::last_immix_stats` is `None` for a relocation, which re-sizes — and
//! re-copies — the whole region instead of reclaiming in place).
//!
//! # What is asserted, and what is only printed
//!
//! Every *count* here is deterministic and asserted: the copying fullsweep re-copies the whole
//! live set, the mark-region fullsweep pins exactly the objects of its fully-live blocks and
//! evacuates the trailing partial block (line-rounded), the resident footprint is the
//! line-rounded occupancy, and the in-place-vs-relocating decision is checked against the
//! policy's own [`HeapPolicy::mature_after_fullsweep`] verdict. **Pause is never asserted** — it
//! is printed as the fastest of [`REPS`] samples (noise can only add time) and carries
//! information only as a within-point copying-vs-mark-region ratio.
//!
//! `unsafe` is not used: the corpus is built through the public `beam_alloc::Heap` API and the
//! graphs are compared through `beam_term::compare::eq`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_gc::prelude::{
    lines_for, select_mature, AnyCollector, GcConfig, GcStats, HeapPolicy, ImmixStats, MatureKind,
    MatureSource, BLOCK_WORDS, IMMIX_BREAK_EVEN_WORDS, LINES_PER_BLOCK, LINE_WORDS,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, AtomTable, Eterm};

/// Timed repetitions of the measured forced fullsweep per (point, strategy); the FASTEST sample
/// is reported.
const REPS: usize = 9;

/// Serialises the measurements: the harness runs test functions in parallel threads, and an
/// in-process timing is meaningless while another workload allocates on the same machine. A
/// poisoned lock is deliberately ignored (`unwrap_or_else(into_inner)`): the guard still gives
/// mutual exclusion, and one failing run must not cascade.
static SERIAL: Mutex<()> = Mutex::new(());

/// Takes the measurement lock for the rest of a test.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Nanoseconds of a pause (µs resolution is too coarse: most of these collections are well under
/// 10 µs).
fn nanos(d: Duration) -> u64 {
    d.as_nanos() as u64
}

// ===========================================================================
// The corpus vocabulary: object descriptors and a plan
// ===========================================================================

/// One long-lived object of a corpus, described by its shape only: the words it occupies are
/// what a word-granular copy pays for it, the lines it occupies are what the mark-region space
/// rounds its allocation to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Obj {
    /// A cons cell: 2 words on one line (the smallest object the frozen layout has).
    Cons,
    /// A tuple of `arity` small integers: `arity + 1` words.
    Tuple(usize),
}

impl Obj {
    /// The object's exact size in words: what a word-granular copy pays for it.
    fn words(self) -> usize {
        match self {
            Obj::Cons => 2,
            Obj::Tuple(arity) => arity + 1,
        }
    }

    /// The mark-region lines the object occupies (`lines_for`, i.e. rounded up).
    fn lines(self) -> usize {
        lines_for(self.words())
    }

    /// Writes the object into `heap`; `id` makes every object's contents unique so a structural
    /// comparison across strategies is meaningful.
    fn write(self, heap: &mut Heap, id: i64) -> Eterm {
        match self {
            Obj::Cons => heap
                .cons(make_small(id), make_small(2 * id + 1))
                .expect("a cons cell"),
            Obj::Tuple(arity) => {
                let elems: Vec<Eterm> = (0..arity).map(|k| make_small(id * 64 + k as i64)).collect();
                heap.tuple(&elems).expect("a tuple")
            }
        }
    }
}

/// A corpus: the ordered plan of objects that make up one live set.
///
/// The order **is** the allocation order, so (given line-granular promotion) it is also the
/// mature layout the mark-region space's whole-block pinning law reads.
struct Corpus {
    plan: Vec<Obj>,
}

impl Corpus {
    /// Builds a corpus from an explicit plan.
    fn from_plan(plan: Vec<Obj>) -> Corpus {
        Corpus { plan }
    }

    /// A corpus of `count` repetitions of the same object.
    fn uniform(obj: Obj, count: usize) -> Corpus {
        Corpus::from_plan(vec![obj; count])
    }

    /// The number of live objects.
    fn objects(&self) -> usize {
        self.plan.len()
    }

    /// The live set's exact word count: the copying path's footprint and moved work.
    fn useful_words(&self) -> usize {
        self.plan.iter().map(|o| o.words()).sum()
    }

    /// The live set's line count: the mark-region path's resident footprint (rounded up to whole
    /// lines).
    fn lines(&self) -> usize {
        self.plan.iter().map(|o| o.lines()).sum()
    }

    /// The live set's line-rounded word count — the mark-region space's `used_words`.
    fn resident_line_words(&self) -> usize {
        self.lines() * LINE_WORDS
    }

    /// Writes the whole plan into `heap`, in order, pushing each object onto `roots`.
    fn build(&self, heap: &mut Heap, roots: &mut Vec<Eterm>) {
        for (i, obj) in self.plan.iter().enumerate() {
            roots.push(obj.write(heap, i as i64));
        }
    }
}

// ===========================================================================
// The shared measurement driver
// ===========================================================================

/// One (point, strategy) measurement, plus the live state it produced.
struct Run {
    kind: MatureKind,
    stats: GcStats,
    immix: Option<ImmixStats>,
    /// Live words in the mature space (line-rounded for the mark-region space).
    used_words: usize,
    /// Resident mature capacity in words (whole blocks for the mark-region space).
    capacity_words: usize,
    /// Resident lines, for the mark-region space only.
    used_lines: Option<usize>,
    moved_objects: usize,
    moved_words: usize,
    pinned_objects: usize,
    pause_ns: u64,
    /// Kept alive so `roots` (raw pointers into this collector's own spaces) stay valid — a
    /// lifetime anchor, never read directly.
    #[allow(dead_code)]
    collector: AnyCollector,
    /// The surviving roots, in the order the corpus allocated them.
    roots: Vec<Eterm>,
}

/// The parts of a run that must be identical across repetitions of the same point (everything
/// but the timing).
fn count_signature(r: &Run) -> (MatureKind, GcStats, Option<ImmixStats>, usize, usize, Option<usize>) {
    (
        r.kind,
        r.stats,
        r.immix,
        r.used_words,
        r.capacity_words,
        r.used_lines,
    )
}

/// The moved-work triple of a fullsweep: the objects and words the strategy actually moved
/// (line-rounded to whole lines for the mark-region path's evacuations), plus the objects it left
/// exactly where they were.
fn moved(stats: &GcStats, immix: Option<ImmixStats>) -> (usize, usize, usize) {
    match immix {
        Some(i) => (
            i.objects_evacuated + stats.objects_copied,
            i.lines_evacuated * LINE_WORDS + stats.words_copied,
            i.objects_pinned,
        ),
        None => (stats.objects_copied, stats.words_copied, 0),
    }
}

/// The mature-space accounting view of an [`AnyCollector`]: used words, resident capacity and
/// (mark-region only) resident lines.
fn mature_view(c: &AnyCollector) -> (usize, usize, Option<usize>) {
    match c {
        AnyCollector::Copying(x) => (x.mature().used_words(), x.mature().capacity_words(), None),
        AnyCollector::Immix(x) => (
            x.mature().used_words(),
            x.mature().capacity_words(),
            Some(x.mature().used_lines()),
        ),
    }
}

/// Runs a forced fullsweep and returns its [`GcStats`] plus, for the mark-region path, the
/// in-place reclaim's own accounting (`None` when the fullsweep relocated instead).
fn fullsweep(c: &mut AnyCollector, roots: &mut [Eterm]) -> (GcStats, Option<ImmixStats>) {
    match c {
        AnyCollector::Copying(x) => (x.collect_major(roots).expect("fullsweep"), None),
        AnyCollector::Immix(x) => {
            let stats = x.collect_major(roots).expect("fullsweep");
            (stats, x.last_immix_stats())
        }
    }
}

/// Fills the (empty) nursery with nothing but garbage, so the measured fullsweep has no live
/// nursery object to promote and the two strategies differ only in the mature generation.
fn fill_nursery_with_garbage(c: &mut AnyCollector) {
    let garbage_objects = c.young().available_words() / (3 + 64);
    let heap = c.young_mut().heap_mut();
    for i in 0..garbage_objects as i64 {
        let _ = heap
            .tuple(&[make_small(i), make_small(2 * i)])
            .expect("garbage tuple");
    }
}

/// One full measurement pass over `corpus` under `policy` with the requested strategy.
///
/// The **promotion** fullsweep is unmeasured: it moves the whole corpus out of the nursery, so it
/// is not where the two strategies differ. Its regime is still recorded (`Some(true)` reclaimed
/// in place, `Some(false)` relocated, `None` for the copying path, which always copies) because
/// a configured capacity the live set does not fit forces that first fullsweep to resize. The
/// **measured** fullsweep then sees a mature generation that already holds the whole live set.
fn run_once(corpus: &Corpus, policy: &HeapPolicy, kind: MatureKind) -> (Run, Option<bool>) {
    let config = GcConfig::copying(policy.clone()).with_mature(kind);
    let mut c = AnyCollector::from_config(&config).expect("collector");
    assert_eq!(c.kind(), kind, "the config selected the requested strategy");

    let mut roots: Vec<Eterm> = Vec::with_capacity(corpus.objects());
    {
        let heap = c.young_mut().heap_mut();
        corpus.build(heap, &mut roots);
    }
    assert_eq!(roots.len(), corpus.objects(), "the whole corpus was built");
    assert!(
        c.young().available_words() >= corpus.resident_line_words() / 2,
        "the nursery was sized to hold the whole corpus"
    );

    let (_, promote_immix) = fullsweep(&mut c, &mut roots);
    let promote_in_place = match kind {
        MatureKind::Copying => None,
        MatureKind::Immix => Some(promote_immix.is_some()),
    };
    assert_eq!(roots.len(), corpus.objects(), "the roots survived promotion");
    if let (MatureKind::Immix, Some(false)) = (kind, promote_in_place) {
        // A relocation still copies the whole live set — never a half-forwarded heap; the fresh
        // region is a real, non-empty space that now holds every live object.
        let (used, capacity, _lines) = mature_view(&c);
        assert!(capacity > 0, "a relocated region is a fresh, non-empty space");
        assert!(used > 0, "the relocated region holds the promoted live set");
    }

    fill_nursery_with_garbage(&mut c);

    let start = Instant::now();
    let (stats, immix) = fullsweep(&mut c, &mut roots);
    let pause_ns = nanos(start.elapsed());

    let (used_words, capacity_words, used_lines) = mature_view(&c);
    let (moved_objects, moved_words, pinned_objects) = moved(&stats, immix);

    let run = Run {
        kind,
        stats,
        immix,
        used_words,
        capacity_words,
        used_lines,
        moved_objects,
        moved_words,
        pinned_objects,
        pause_ns,
        collector: c,
        roots,
    };
    (run, promote_in_place)
}

/// Runs [`run_once`] [`REPS`] times, asserting every repetition did identical work, and returns
/// the last run with its pause replaced by the **fastest** sample.
fn measure(corpus: &Corpus, policy: &HeapPolicy, kind: MatureKind) -> (Run, Option<bool>) {
    let (mut last, promote_in_place) = run_once(corpus, policy, kind);
    assert_eq!(last.kind, kind, "the config selected the requested strategy");
    let mut samples = Vec::with_capacity(REPS);
    samples.push(last.pause_ns);
    for rep in 1..REPS {
        let (next, next_promote) = run_once(corpus, policy, kind);
        assert_eq!(
            count_signature(&next),
            count_signature(&last),
            "repetition {rep} must do identical work"
        );
        assert_eq!(
            next_promote, promote_in_place,
            "repetition {rep} must take the same promotion regime"
        );
        samples.push(next.pause_ns);
        last = next;
    }
    last.pause_ns = *samples.iter().min().expect("at least one sample");
    (last, promote_in_place)
}

/// Asserts the reachable graph is identical under both strategies.
fn assert_same_graph(copying: &Run, immix: &Run) {
    let atoms = AtomTable::new();
    assert_eq!(copying.roots.len(), immix.roots.len(), "root count");
    for (i, (a, b)) in copying.roots.iter().zip(&immix.roots).enumerate() {
        assert!(term_eq(*a, *b, &atoms), "root {i} differs across strategies");
    }
}

/// One report line for a single (point, strategy) run.
fn report_line(r: &Run) -> String {
    let resident = match r.used_lines {
        Some(lines) => format!(
            "{} used / {} cap line-words ({lines} lines)",
            r.used_words, r.capacity_words
        ),
        None => format!("{} used / {} cap words", r.used_words, r.capacity_words),
    };
    format!(
        "    {:>7?}: moved {:>4} obj / {:>5} words | pinned {:>4} obj | reclaimed {:>5} words \
         | resident {:>42} | pause {:>7.2} µs",
        r.kind,
        r.moved_objects,
        r.moved_words,
        r.pinned_objects,
        r.stats.words_reclaimed,
        resident,
        r.pause_ns as f64 / 1000.0
    )
}

// ===========================================================================
// Axis 1 — object-size distribution
// ===========================================================================

/// Whole Immix blocks the axis-1 live set **fills**: 8 — the break-even — so the mark-region
/// space always has eight fully-live, pinnable blocks to work with.
const AXIS1_BLOCKS: usize = 8;

/// Extra lines the axis-1 live set occupies past the whole blocks: a **trailing partial block**
/// of 6 lines. A whole-block live set would pin everything (moved words 0, shape-independent);
/// a trailing partial block forces an evacuation whose moved work is measured in whole **lines**
/// — invariant across object-size distributions — which is exactly the axis's question.
const AXIS1_TAIL_LINES: usize = 6;

/// The axis-1 live set's line count (8 blocks + 6 lines = 134 lines). Even, so the 2-line
/// multi-object distribution tiles it exactly.
const AXIS1_LINES: usize = AXIS1_BLOCKS * LINES_PER_BLOCK + AXIS1_TAIL_LINES;

/// The axis-1 mature capacity, in words: 16 whole blocks — deliberately **larger** than the live
/// set, so it is an in-place fixed point (neither `should_grow` nor `should_shrink` fires) and
/// the pinning/evacuation regime is what is measured, never a relocating fullsweep.
const AXIS1_MATURE_WORDS: usize = 4096;

/// The object-size distributions the axis sweeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dist {
    /// Cons cells: 2 words, one line (worst line waste: 8× resident overhead).
    Cons,
    /// Two-element tuples: 3 words, one line (5.3× resident overhead).
    Small,
    /// Fifteen-element tuples: 16 words, exactly one full line (no line waste).
    Line,
    /// Sixteen-element tuples: 17 words, two lines (1.9× resident overhead).
    Multi,
    /// A mix of all four sizes in one live set, packed as a repeated 16-line block pattern.
    Mixed,
}

impl Dist {
    /// Every distribution the sweep covers.
    const ALL: [Dist; 5] = [Dist::Cons, Dist::Small, Dist::Line, Dist::Multi, Dist::Mixed];

    /// The object plan of [`AXIS1_LINES`] lines for this distribution.
    fn plan(self) -> Vec<Obj> {
        match self {
            Dist::Cons => vec![Obj::Cons; AXIS1_LINES],
            Dist::Small => vec![Obj::Tuple(2); AXIS1_LINES],
            Dist::Line => vec![Obj::Tuple(15); AXIS1_LINES],
            // A 17-word tuple spans two lines, so it fills the same 128 lines in half as many
            // objects; eight such tuples fill one 16-line block exactly.
            Dist::Multi => vec![Obj::Tuple(16); AXIS1_LINES / 2],
            Dist::Mixed => {
                // One 16-line block pattern covering all four sizes, with the two 2-line objects
                // at block-relative offsets 0 and 8 (so no object ever straddles a block
                // boundary):
                //   multi(0-1) line(2) small(3) cons(4) line(5) small(6) cons(7)
                //   multi(8-9) line(10) small(11) cons(12) line(13) small(14) cons(15)
                let pattern = [
                    Obj::Tuple(16),
                    Obj::Tuple(15),
                    Obj::Tuple(2),
                    Obj::Cons,
                    Obj::Tuple(15),
                    Obj::Tuple(2),
                    Obj::Cons,
                    Obj::Tuple(16),
                    Obj::Tuple(15),
                    Obj::Tuple(2),
                    Obj::Cons,
                    Obj::Tuple(15),
                    Obj::Tuple(2),
                    Obj::Cons,
                ];
                assert_eq!(
                    pattern.iter().map(|o| o.lines()).sum::<usize>(),
                    LINES_PER_BLOCK,
                    "the mixed pattern is exactly one block"
                );
                // Tile whole 16-line blocks, then append the pattern's leading objects until the
                // trailing partial block is filled (the partial prefix is `multi, line, small,
                // cons, line` — all of it inside the last block, so nothing straddles a
                // boundary).
                let mut plan: Vec<Obj> = Vec::new();
                let mut lines = 0usize;
                for obj in pattern.iter().copied().cycle() {
                    if lines == AXIS1_LINES {
                        break;
                    }
                    assert!(
                        lines + obj.lines() <= AXIS1_LINES,
                        "no object overruns the trailing partial block"
                    );
                    plan.push(obj);
                    lines += obj.lines();
                }
                assert_eq!(lines, AXIS1_LINES, "the mixed plan fills the live set exactly");
                plan
            }
        }
    }

    /// The corpus of this distribution.
    fn corpus(self) -> Corpus {
        let corpus = Corpus::from_plan(self.plan());
        assert_eq!(
            corpus.lines(),
            AXIS1_LINES,
            "{self:?}: the live set occupies exactly {AXIS1_BLOCKS} whole blocks"
        );
        corpus
    }
}

/// The heap policy every axis-1 shape shares: a nursery that holds the whole corpus, the
/// fullsweep cadence disabled, and the fixed [`AXIS1_MATURE_WORDS`] mature capacity.
fn axis1_policy() -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: 65535,
        initial_young_words: 8192,
        initial_mature_words: AXIS1_MATURE_WORDS,
        need_reserve_words: 64,
        ..HeapPolicy::default()
    }
}

/// The **whole-block law** of one axis-1 corpus: the live set is contiguous from the start of the
/// region, so it covers some whole blocks plus one trailing partial block. The mark-region space
/// pins every object that *starts* in a fully-live block and evacuates the objects that start in
/// the trailing partial block (whole lines), which fixes every count [`assert_axis1_point`]
/// asserts — independently of the object-size distribution.
struct Axis1Law {
    /// Lines in whole blocks entirely covered by live objects.
    full_blocks: usize,
    /// Lines in the trailing partial block.
    remainder_lines: usize,
    /// Objects whose first line is inside a fully-live block.
    pinned_objects: usize,
    /// Live words of the pinned objects.
    pinned_words: usize,
    /// Objects that start in the trailing partial block.
    evacuated_objects: usize,
}

impl Axis1Law {
    /// The law's prediction for `corpus`.
    fn of(corpus: &Corpus) -> Axis1Law {
        let full_blocks = corpus.lines() / LINES_PER_BLOCK;
        let remainder_lines = corpus.lines() % LINES_PER_BLOCK;
        let boundary = full_blocks * LINES_PER_BLOCK;
        let mut start = 0usize;
        let (mut pinned_objects, mut pinned_words) = (0usize, 0usize);
        for obj in &corpus.plan {
            if start < boundary {
                pinned_objects += 1;
                pinned_words += obj.words();
            }
            start += obj.lines();
        }
        assert_eq!(start, corpus.lines(), "the plan tiles the live set exactly");
        Axis1Law {
            full_blocks,
            remainder_lines,
            pinned_objects,
            pinned_words,
            evacuated_objects: corpus.objects() - pinned_objects,
        }
    }

    /// The words the trailing partial block's evacuation moves (whole lines, whatever the object
    /// sizes are).
    fn moved_words(&self) -> usize {
        self.remainder_lines * LINE_WORDS
    }
}

/// The deterministic law of one axis-1 distribution point. Every assertion is on an exact count;
/// nothing is derived from a timing.
fn assert_axis1_point(dist: Dist, copying: &Run, immix: &Run) {
    let corpus = dist.corpus();
    let law = Axis1Law::of(&corpus);
    let label = format!(
        "{dist:?} ({} objects, {} useful words, {} lines)",
        corpus.objects(),
        corpus.useful_words(),
        corpus.lines()
    );

    // -- the copying path re-copies the whole live set, word for word -----------------------
    assert_eq!(copying.kind, MatureKind::Copying, "{label}");
    assert_eq!(copying.moved_objects, corpus.objects(), "{label}: copies every live object");
    assert_eq!(
        copying.moved_words,
        corpus.useful_words(),
        "{label}: copies the exact live word count"
    );
    assert_eq!(copying.pinned_objects, 0, "{label}: copying pins nothing");
    assert_eq!(copying.used_words, corpus.useful_words(), "{label}: footprint = live words");

    // -- the mark-region path pins whole blocks and evacuates the trailing partial one ------
    assert_eq!(immix.kind, MatureKind::Immix, "{label}");
    assert_eq!(immix.stats.objects_copied, 0, "{label}: the nursery held only garbage");
    assert_eq!(immix.stats.words_copied, 0, "{label}: nothing was promoted");
    let stats = immix
        .immix
        .unwrap_or_else(|| panic!("{label}: the fullsweep must reclaim in place"));
    assert_eq!(stats.objects_pinned, law.pinned_objects, "{label}: whole blocks are pinned");
    assert_eq!(
        stats.objects_evacuated,
        law.evacuated_objects,
        "{label}: the trailing partial block is evacuated"
    );
    assert_eq!(
        stats.objects_pinned + stats.objects_evacuated,
        corpus.objects(),
        "{label}: every live object is accounted for exactly once"
    );
    assert_eq!(
        stats.lines_pinned,
        law.full_blocks * LINES_PER_BLOCK,
        "{label}: pinned lines"
    );
    assert_eq!(stats.lines_evacuated, law.remainder_lines, "{label}: evacuated lines");
    assert_eq!(stats.words_pinned, law.pinned_words, "{label}: pinned words");
    assert_eq!(stats.words_reclaimed, 0, "{label}: nothing in the mature generation is garbage");
    assert_eq!(stats.blocks_reclaimed, 0, "{label}: the live set is a root");
    assert_eq!(
        immix.moved_objects,
        law.evacuated_objects,
        "{label}: only the partial block's objects move"
    );
    // Moved work is measured in whole LINES — the same count for every object-size distribution.
    assert_eq!(
        immix.moved_words,
        law.moved_words(),
        "{label}: moved words are the line-rounded partial block"
    );
    assert_eq!(immix.pinned_objects, law.pinned_objects, "{label}");

    // -- the mark-region footprint is the line-rounded occupancy ----------------------------
    assert_eq!(immix.used_words, corpus.resident_line_words(), "{label}: resident line-words");
    assert_eq!(immix.used_lines, Some(corpus.lines()), "{label}: resident lines");
    assert!(
        immix.used_words >= copying.used_words,
        "{label}: line rounding can only over-state the live words"
    );
}

/// The axis-1 sweep: every distribution at the fixed, in-place mature size.
fn sweep_axis1() {
    assert_eq!(AXIS1_MATURE_WORDS % BLOCK_WORDS, 0, "the axis-1 capacity is whole blocks");
    let policy = axis1_policy();
    assert_eq!(
        policy.mature_after_fullsweep(AXIS1_MATURE_WORDS, AXIS1_LINES * LINE_WORDS, 0),
        AXIS1_MATURE_WORDS,
        "the axis-1 capacity is an in-place fixed point (the fullsweep neither grows nor shrinks)"
    );

    println!(
        "\n=== axis 1: object-size distribution at a fixed in-place mature size ({AXIS1_MATURE_WORDS} words = {} blocks); live set = {AXIS1_LINES} lines ({AXIS1_BLOCKS} whole blocks + {AXIS1_TAIL_LINES} trailing lines) ===",
        AXIS1_MATURE_WORDS / BLOCK_WORDS,
    );
    for dist in Dist::ALL {
        let corpus = dist.corpus();
        let law = Axis1Law::of(&corpus);
        let (copying, _) = measure(&corpus, &policy, MatureKind::Copying);
        let (immix, _) = measure(&corpus, &policy, MatureKind::Immix);
        assert_same_graph(&copying, &immix);
        assert_axis1_point(dist, &copying, &immix);

        println!(
            "  -- {dist:?}: {} objects, {} useful words, {} lines ({} line-words; law: {} pinned obj / {} evacuated obj, move {} line-words) --",
            corpus.objects(),
            corpus.useful_words(),
            corpus.lines(),
            corpus.resident_line_words(),
            law.pinned_objects,
            law.evacuated_objects,
            law.moved_words(),
        );
        println!("{}", report_line(&copying));
        println!("{}", report_line(&immix));
        let moved_ratio = if immix.moved_words == 0 {
            "immix moved 0 words".to_string()
        } else {
            format!("{:.2}×", copying.moved_words as f64 / immix.moved_words as f64)
        };
        println!(
            "    ratio: moved words copying/immix = {moved_ratio} | resident immix/copying = {:.2}× \
             ({:.1}× line waste) | pause immix/copying = {:.2}× | rule selects {:?} at {} \
             configured words",
            immix.used_words as f64 / copying.used_words.max(1) as f64,
            immix.used_words as f64 / corpus.useful_words() as f64,
            immix.pause_ns as f64 / copying.pause_ns.max(1) as f64,
            select_mature(AXIS1_MATURE_WORDS),
            AXIS1_MATURE_WORDS,
        );
    }
}

#[test]
fn object_size_distribution_axis() {
    let _serial = serial();
    sweep_axis1();
}

// ===========================================================================
// Axis 2 — configured mature size / occupancy
// ===========================================================================

/// The axis-2 live set: 40 one-line objects (15-element tuples, 16 words) = 640 useful words on
/// 40 lines — exactly 2 whole blocks plus a trailing partial block (8 lines), so an in-place
/// reclaim always has both pinned whole blocks and a partial block to evacuate.
const AXIS2_OBJECTS: usize = 40;

/// The axis-2 configured mature capacities, in words: below one whole block (128), one block
/// (256), two (512), four (1024), the break-even (2048), the sparsest generation the policy still
/// **keeps** (`4 × (live line-words + reserve)` = 2816 = 11 whole blocks), sixteen (4096) and an
/// oversized generation (8192) that the policy shrinks back.
const AXIS2_CAPACITIES: [usize; 8] = [128, 256, 512, 1024, 2048, 2816, 4096, 8192];

/// The axis-2 live set.
fn axis2_corpus() -> Corpus {
    Corpus::uniform(Obj::Tuple(15), AXIS2_OBJECTS)
}

/// The heap policy for a configured mature capacity: a nursery that holds the whole live set and
/// the fullsweep cadence disabled.
fn axis2_policy(mature_words: usize) -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: 65535,
        initial_young_words: 8192,
        initial_mature_words: mature_words,
        need_reserve_words: 64,
        ..HeapPolicy::default()
    }
}

/// Asserts the deterministic law of one axis-2 point for the mark-region strategy.
///
/// The live set is 40 lines = 2 whole blocks + 8 lines: an in-place reclaim pins the two whole
/// blocks (32 objects) and evacuates the trailing partial block (8 objects, 8 lines), while a
/// relocating fullsweep re-copies the whole live set (a plain copying collection).
fn assert_axis2_immix(corpus: &Corpus, configured_words: usize, run: &Run) {
    let label = format!("capacity {configured_words} words");
    assert_eq!(run.kind, MatureKind::Immix, "{label}");
    assert_eq!(run.roots.len(), corpus.objects(), "{label}: roots survive");

    let full_blocks = corpus.lines() / LINES_PER_BLOCK;
    let remainder = corpus.lines() % LINES_PER_BLOCK;
    // Every axis-2 object is line-sized, so one line per object and one object per line.
    let objects_per_block = LINES_PER_BLOCK;

    match run.immix {
        Some(stats) => {
            assert_eq!(run.stats.objects_copied, 0, "{label}: nursery held only garbage");
            assert_eq!(run.stats.words_copied, 0, "{label}: nothing was promoted");
            assert_eq!(
                stats.objects_pinned,
                full_blocks * objects_per_block,
                "{label}: whole blocks are pinned"
            );
            assert_eq!(
                stats.objects_evacuated,
                remainder,
                "{label}: the trailing partial block is evacuated"
            );
            assert_eq!(
                stats.objects_pinned + stats.objects_evacuated,
                corpus.objects(),
                "{label}: every live object is accounted for exactly once"
            );
            assert_eq!(stats.lines_pinned, full_blocks * LINES_PER_BLOCK, "{label}: pinned lines");
            assert_eq!(stats.lines_evacuated, remainder, "{label}: evacuated lines");
            assert_eq!(stats.words_reclaimed, 0, "{label}: the live set is a root");
            assert_eq!(stats.blocks_reclaimed, 0, "{label}: the live set is a root");
            assert_eq!(
                run.moved_words,
                remainder * LINE_WORDS,
                "{label}: only the partial block is moved, line-rounded"
            );
            assert_eq!(run.moved_objects, remainder, "{label}");
            assert_eq!(run.pinned_objects, full_blocks * objects_per_block, "{label}");
            assert_eq!(run.used_words, corpus.resident_line_words(), "{label}");
            assert_eq!(run.used_lines, Some(corpus.lines()), "{label}");
        }
        None => {
            assert_eq!(
                run.moved_objects,
                corpus.objects(),
                "{label}: a relocating fullsweep copies every live object"
            );
            assert_eq!(
                run.moved_words,
                corpus.useful_words(),
                "{label}: a relocating fullsweep copies the exact live word count"
            );
            assert_eq!(run.pinned_objects, 0, "{label}: a relocation pins nothing");
            assert_eq!(run.stats.objects_copied, corpus.objects(), "{label}");
        }
    }
}

/// The axis-2 sweep: one fixed live set through a range of configured mature capacities.
fn sweep_axis2() {
    let corpus = axis2_corpus();
    let live_line_words = corpus.resident_line_words();

    println!(
        "\n=== axis 2: configured mature capacity vs occupancy; live set = {} objects = {} useful words = {} lines ({} line-words) ===",
        corpus.objects(),
        corpus.useful_words(),
        corpus.lines(),
        live_line_words
    );
    println!(
        "  (default policy = 233 words; the rule starts selecting the mark-region space at \
         {IMMIX_BREAK_EVEN_WORDS} configured words = {} whole blocks)",
        IMMIX_BREAK_EVEN_WORDS / BLOCK_WORDS
    );

    for &cap in &AXIS2_CAPACITIES {
        let policy = axis2_policy(cap);
        let rule = GcConfig::auto(policy.clone()).mature();
        assert_eq!(
            rule,
            if cap >= IMMIX_BREAK_EVEN_WORDS {
                MatureKind::Immix
            } else {
                MatureKind::Copying
            },
            "the automatic rule's verdict at {cap} configured words"
        );

        let (copying, _) = measure(&corpus, &policy, MatureKind::Copying);
        let (immix, promote_in_place) = measure(&corpus, &policy, MatureKind::Immix);
        assert_same_graph(&copying, &immix);

        // The copying path always pays for the whole live set.
        assert_eq!(copying.moved_objects, corpus.objects(), "capacity {cap}");
        assert_eq!(copying.moved_words, corpus.useful_words(), "capacity {cap}");
        assert_eq!(copying.used_words, corpus.useful_words(), "capacity {cap}");
        assert_axis2_immix(&corpus, cap, &immix);

        let occupancy = live_line_words as f64 / immix.capacity_words.max(1) as f64;
        let regime = |in_place: Option<bool>| match in_place {
            Some(true) => "reclaimed in place",
            Some(false) => "RELOCATED (resized the region)",
            None => "n/a",
        };
        println!(
            "  -- configured {cap} words (rule -> {rule:?}); resident capacity: copying {} words, \
             immix {} words (occupancy {:.1}%) --",
            copying.capacity_words,
            immix.capacity_words,
            occupancy * 100.0,
        );
        println!(
            "    promotion fullsweep: copying always copies; immix {}",
            regime(promote_in_place)
        );
        println!("{}", report_line(&copying));
        println!("{}", report_line(&immix));
        println!(
            "    measured fullsweep: immix {} | moved words copying/immix = {:.2}× | resident \
             immix/copying = {:.2}× | pause immix/copying = {:.2}×",
            regime(Some(immix.immix.is_some())),
            copying.moved_words as f64 / immix.moved_words.max(1) as f64,
            immix.capacity_words as f64 / copying.capacity_words.max(1) as f64,
            immix.pause_ns as f64 / copying.pause_ns.max(1) as f64,
        );
    }
}

#[test]
fn mature_size_axis() {
    let _serial = serial();
    sweep_axis2();
}

// ===========================================================================
// The verdict on the break-even constant
// ===========================================================================

/// The static, arithmetic facts about the rule's break-even — the answer to "does the automatic
/// rule ever select the mark-region space at the default policy?".
#[test]
fn break_even_verdict() {
    let _serial = serial();

    // The constant is expressed in the space's own unit (whole Immix blocks).
    assert_eq!(
        IMMIX_BREAK_EVEN_WORDS,
        8 * BLOCK_WORDS,
        "the break-even is eight whole Immix blocks"
    );
    assert_eq!(BLOCK_WORDS, LINES_PER_BLOCK * LINE_WORDS);

    // The default policy's configured mature generation is 233 words — under one whole block, let
    // alone the eight the rule asks for.
    let default = HeapPolicy::default();
    assert_eq!(default.initial_mature_words, 233);
    assert!(
        default.initial_mature_words < BLOCK_WORDS,
        "the default generation is smaller than a single block"
    );

    // The pure rule, in both directions.
    assert_eq!(select_mature(233), MatureKind::Copying);
    assert_eq!(select_mature(IMMIX_BREAK_EVEN_WORDS - 1), MatureKind::Copying);
    assert_eq!(select_mature(IMMIX_BREAK_EVEN_WORDS), MatureKind::Immix);

    // The default configuration therefore resolves to the copying path, automatically.
    let auto_default = GcConfig::auto(default.clone());
    assert_eq!(auto_default.mature(), MatureKind::Copying);
    assert_eq!(auto_default.mature_choice().mature_words, 233);
    assert_eq!(auto_default.mature_choice().source, MatureSource::Automatic);
    assert_eq!(GcConfig::default().mature(), MatureKind::Copying);

    // At the switch, the rule also decides automatically.
    let at_switch = GcConfig::auto(HeapPolicy {
        initial_mature_words: IMMIX_BREAK_EVEN_WORDS,
        ..HeapPolicy::default()
    });
    assert_eq!(at_switch.mature(), MatureKind::Immix);
    assert_eq!(at_switch.mature_choice().source, MatureSource::Automatic);

    println!(
        "\n=== verdict ===\n\
         break-even = {IMMIX_BREAK_EVEN_WORDS} configured words = {} whole blocks\n\
         default policy mature generation = 233 words -> rule selects {:?} (never Immix)\n\
         the rule first reaches a whole block at {BLOCK_WORDS} words -> still {:?}\n\
         the rule first selects Immix at {IMMIX_BREAK_EVEN_WORDS} configured words (= {} blocks)\n\
         the default configuration resolves to {:?}",
        IMMIX_BREAK_EVEN_WORDS / BLOCK_WORDS,
        select_mature(233),
        select_mature(BLOCK_WORDS),
        IMMIX_BREAK_EVEN_WORDS / BLOCK_WORDS,
        GcConfig::default().mature(),
    );
}
