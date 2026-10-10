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

//! **Break-even re-measurement** for the automatic mature-generation selection rule.
//!
//! `immix_workloads.rs` compares the two mature strategies on three *shapes* at one size
//! each. The selection rule needs a different axis: **how does each strategy scale with the
//! size of a long-lived mature generation?** This file answers exactly that by driving the
//! *same* long-lived corpus at several size points — from a quarter of an Immix block up to
//! many whole blocks of live data — through both strategies
//! ([`AnyCollector::Copying`] and [`AnyCollector::Immix`], selected through
//! [`GcConfig`]/[`MatureKind`]) and reporting, per size point and per strategy, the three
//! quantities the rule is derived from:
//!
//! * the forced fullsweep's **moved work** (`GcStats::objects_copied`/`words_copied` for the
//!   copying path; the mark-region reclaim's `objects_evacuated`/`lines_evacuated` plus the
//!   nursery promotion for the mark-region path);
//! * the resident mature **footprint** (whole lines for the mark-region space, i.e. its
//!   line-rounded `used_words`);
//! * the **pause** of the fastest of [`REPS`] timed forced fullsweeps.
//!
//! # The corpus
//!
//! The live set is *purely long-lived*: every object is a root, so the mark-region path's
//! `AllMarked` pinning applies to every **full** block while the copying path re-copies the
//! whole live set into a fresh space on every fullsweep. The nursery holds nothing but
//! garbage at the measured collection, so the two strategies differ only in what they do
//! with the mature generation.
//!
//! Three shapes are swept, so that the mark-region space's **line** granularity is exercised
//! both without waste and at its worst:
//!
//! * [`Shape::Line`] — a 16-word tuple, i.e. **exactly one 16-word line** per object: the
//!   shape the mark-region space is designed for. Its line granularity wastes nothing, so
//!   its resident footprint equals the copying path's and the only difference is *how much
//!   it moves*;
//! * [`Shape::Small`] — a 3-word tuple: 3 useful words per 16-word line (5.3× resident
//!   overhead). The mark-region space's **worst case**, and the shape whose benefit/cost
//!   ratio pins the rule's threshold;
//! * [`Shape::Multi`] — a 17-word tuple: **two** lines for 17 useful words (1.9×). The
//!   multi-line analogue of the worst case, which checks that the whole-block law below
//!   also holds, and is strictly dominated below one block, for objects that span more than
//!   one line.
//!
//! # The measured shape of the trade (printed for orientation, counts asserted)
//!
//! Running the sweep prints, per shape and size point, the moved words, the pinned objects,
//! the resident footprint and the pause. The *Counts* below are exact; *pauses* are the
//! fastest of [`REPS`] runs and carry information only as a within-point ratio:
//!
//! * [`Shape::Line`] (no line waste) — at `1/4` and `1/2` block the mark-region path moves
//!   exactly as much as the copying path (64 → 64, 128 → 128 words) at a 1.0× footprint and
//!   ≈ 2× the pause, i.e. it is *never better* below one block; from one whole block up it
//!   moves **0** words (the whole live set is pinned) while the copying path pays the whole
//!   live set (256 words at one block, 16 384 at 64), at a 1.0× footprint.
//! * [`Shape::Small`] (5.3× line waste) — below one whole block it pins nothing and moves
//!   **5.3× more** words (64 vs 12, 128 vs 24) in 5.3× the footprint, i.e. it is *strictly
//!   dominated*; from one block up it pins the whole live set at a 0.19× footprint. Its
//!   avoided-copy-words-per-extra-µs ratio climbs ≈55 → ≈63 at 1 → 2 blocks and ≈73 at 8
//!   blocks, then settles at ≈80–85 out to 64 blocks — the plateau the constant's *value* is
//!   read from.
//! * [`Shape::Multi`] (1.9× line waste, objects spanning two lines) — the same qualitative
//!   picture with the rounding factor of two: below one block it moves 1.9× more words
//!   (128 vs 68) in 1.9× the footprint; from one block up it moves 0 words at a 0.53×
//!   footprint, with the ratio ≈198 → ≈263 at 1 → 2 blocks and ≈274–292 from 8 blocks up.
//!
//! # What is assertion-backed, and what is not
//!
//! The rule has two halves, and this file treats them differently on purpose.
//!
//! **Assertion-backed (deterministic counts).** Every measured size point asserts the exact
//! counts the corpus forces ([`assert_size_point`], built on the whole-block law
//! [`Law::of`]):
//!
//! * the copying fullsweep re-copies the whole live set — `objects_copied == objects` and
//!   `words_copied == objects × object words` — and pins nothing;
//! * the mark-region fullsweep promotes nothing (the nursery holds only garbage at the
//!   measured collection, so `words_copied == 0`) and accounts for every live object exactly
//!   once (`objects_evacuated + objects_pinned == objects`);
//! * a live line set of `lines` lines (contiguous from the start of the region) is pinned
//!   exactly where it fills **whole blocks**: `objects_pinned == objects-per-block × whole
//!   blocks covered` and `moved words == (lines % LINES_PER_BLOCK) × LINE_WORDS`;
//! * therefore **below one whole block nothing can be pinned** — the mark-region path
//!   evacuates the whole live set, line-rounded: never fewer words than the copying path,
//!   and *strictly* more for any shape whose objects do not fill their last line. That is
//!   the deterministic justification for the rule's floor of one whole block;
//! * and **from one whole block up the live set is pinned whole** at every measured
//!   whole-block size point (`moved words == 0`) while the copying path pays a full copy.
//!
//! [`assert_rule_against_the_sweep`] couples those facts to the constant's *units* (a whole
//! number of blocks, at least one) and [`the_break_even_point_is_assertion_checked`] checks
//! the constant at exactly its own size: at the threshold the mark-region path is inside the
//! full-pinning regime (0 words moved against a break-even-sized copy), and one line below it
//! it is not (its trailing partial block must be evacuated).
//!
//! **Timing-derived (not assertable).** The constant's *value* — 8 blocks rather than 1 —
//! comes from the avoided-copy-work per extra µs of pause, which plateaus at 8 blocks (see
//! `src/selection.rs`). Pause times are noisy by construction (machine, allocator, thread
//! scheduling) and every *deterministic* axis is flat across the band `[1, 64]` blocks, so
//! the exact block count cannot be pinned by a count. Pauses are printed only, as the
//! *fastest* of [`REPS`] runs (measurement noise can only add time, so the minimum is the
//! cleanest estimate for a deterministic corpus); absolute µs numbers are meaningless, only
//! the copying-vs-mark-region ratio at one size point carries information.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that a
//! collection just wrote); every block carries a `// SAFETY:` rationale.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use beam_gc::prelude::{
    lines_for, AnyCollector, GcConfig, GcStats, HeapPolicy, ImmixStats, MatureKind, BLOCK_WORDS,
    IMMIX_BREAK_EVEN_BLOCKS, IMMIX_BREAK_EVEN_WORDS, LINES_PER_BLOCK, LINE_WORDS,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, AtomTable, Eterm};

/// Timed repetitions of the forced fullsweep per (size point, strategy); the FASTEST sample
/// is reported.
const REPS: usize = 9;

/// The size points, as object counts. One object occupies [`Shape::lines`] lines, so the
/// points span `1/4 → 64` whole Immix blocks for the single-line shapes and `1/2 → 128` for
/// the two-line one; every shape therefore has sub-block, one-block and many-block points.
const OBJECT_POINTS: [usize; 7] = [4, 8, 16, 32, 128, 512, 1024];

/// Serialises the measurements: the harness runs test functions in parallel threads, and an
/// in-process timing is meaningless while another workload allocates on the same machine. A
/// poisoned lock is deliberately ignored (`unwrap_or_else(into_inner)`): the guard still
/// gives mutual exclusion, and one failing run must not cascade.
static SERIAL: Mutex<()> = Mutex::new(());

/// Takes the measurement lock for the rest of a test.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The long-lived object shape (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A 3-word tuple: one line for 3 useful words (5.3× resident overhead).
    Small,
    /// A 17-word tuple: two lines for 17 useful words (1.9× resident overhead).
    Multi,
    /// A 16-word tuple: exactly one line (no resident overhead).
    Line,
}

impl Shape {
    /// Every shape the sweep covers.
    const ALL: [Shape; 3] = [Shape::Small, Shape::Multi, Shape::Line];

    /// The tuple arity, so the object is `arity + 1` words.
    fn arity(self) -> usize {
        match self {
            Shape::Small => 2,
            Shape::Multi => LINE_WORDS,
            Shape::Line => LINE_WORDS - 1,
        }
    }

    /// The object's exact size in words (a tuple is `1 + arity`): what a word-granular copy
    /// pays for it.
    fn words(self) -> usize {
        self.arity() + 1
    }

    /// The Immix lines the object occupies (`lines_for`, i.e. rounded up).
    fn lines(self) -> usize {
        lines_for(self.words())
    }

    /// Words wasted by line rounding — `0` exactly when the object fills its last line.
    fn line_waste(self) -> usize {
        self.lines() * LINE_WORDS - self.words()
    }
}

/// The **whole-block law** of a size point: the live line set of this corpus is contiguous
/// from the start of the region, so it covers `lines / LINES_PER_BLOCK` **whole** blocks plus
/// one trailing partial block. The mark-region path pins a whole block and evacuates a
/// partial block's objects, which fixes every count [`assert_size_point`] asserts.
struct Law {
    /// Live lines (`objects × shape.lines()`).
    lines: usize,
    /// Whole blocks entirely covered by live lines.
    full_blocks: usize,
    /// Lines in the trailing partial block (`lines % LINES_PER_BLOCK`).
    remainder_lines: usize,
    /// Live objects the whole blocks pin.
    pinned_objects: usize,
    /// Live objects the trailing partial block must evacuate.
    evacuated_objects: usize,
}

impl Law {
    /// The law's prediction for `objects` objects of `shape`.
    fn of(shape: Shape, objects: usize) -> Law {
        // One object starts at a line boundary (line-granular allocation), and the shapes
        // are chosen so that a whole number of them fills one block: the packing is exact.
        assert_eq!(
            LINES_PER_BLOCK % shape.lines(),
            0,
            "{shape:?}: a whole number of objects fills one block"
        );
        let lines = objects * shape.lines();
        let full_blocks = lines / LINES_PER_BLOCK;
        let remainder_lines = lines % LINES_PER_BLOCK;
        let objects_per_block = LINES_PER_BLOCK / shape.lines();
        let pinned_objects = full_blocks * objects_per_block;
        Law {
            lines,
            full_blocks,
            remainder_lines,
            pinned_objects,
            evacuated_objects: objects - pinned_objects,
        }
    }

    /// The words the trailing partial block's evacuation moves (whole lines).
    fn moved_words(&self) -> usize {
        self.remainder_lines * LINE_WORDS
    }

    /// The mark-region path's resident footprint: whole lines.
    fn footprint_words(&self) -> usize {
        self.lines * LINE_WORDS
    }
}

/// The heap policy both strategies share at `objects` long-lived objects of `shape`: the
/// fullsweep cadence disabled (every collection measured is one this file explicitly asked
/// for), a nursery that holds the whole corpus, and a mature capacity `2 × (live lines +
/// reserve)` — the fixed point of [`HeapPolicy::mature_after_fullsweep`] that keeps the
/// mark-region fullsweep **in place** (neither `should_grow` nor `should_shrink` fires), so
/// the pinning (or partial-block evacuation) regime is what is measured, never a relocating
/// fullsweep.
///
/// The fullsweep is now sized from the **exact live word count**, which for a small-object
/// live set is far below the line-rounded region size, so `should_shrink` would otherwise fire
/// and relocate the region. `min_heap_words` is therefore pinned to the (whole-block-aligned)
/// intended region: with `capacity == min_heap_words`, `should_shrink` can never fire, and
/// `should_grow` does not either (the region already covers `live.words + reserve` with room to
/// spare), so the region stays the in-place fixed point the measurement needs.
fn policy(shape: Shape, objects: usize) -> HeapPolicy {
    let need_reserve_words = 64;
    let live_line_words = objects * shape.lines() * LINE_WORDS;
    // Round the intended region up to a whole number of Immix blocks, so the region the
    // collector builds and its `min_heap_words` floor agree exactly.
    let mature_words =
        (2 * (live_line_words + need_reserve_words)).div_ceil(BLOCK_WORDS) * BLOCK_WORDS;
    HeapPolicy {
        fullsweep_after: 65535,
        initial_young_words: live_line_words + 4096,
        initial_mature_words: mature_words,
        min_heap_words: mature_words,
        need_reserve_words,
        ..HeapPolicy::default()
    }
}

/// A collector for `kind`, selected through [`GcConfig`] exactly as a caller would.
fn collector(shape: Shape, objects: usize, kind: MatureKind) -> AnyCollector {
    let config = GcConfig::copying(policy(shape, objects)).with_mature(kind);
    AnyCollector::from_config(&config).expect("collector")
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

/// One (shape, size point, strategy) measurement, plus the live state it produced.
struct Run {
    kind: MatureKind,
    stats: GcStats,
    immix: Option<ImmixStats>,
    footprint_words: usize,
    footprint_lines: Option<usize>,
    moved_objects: usize,
    moved_words: usize,
    pinned_objects: usize,
    pause_ns: u64,
    /// Kept alive so `roots` (raw pointers into this collector's own spaces) stay valid —
    /// a lifetime anchor, never read directly.
    #[allow(dead_code)]
    collector: AnyCollector,
    /// The surviving roots, in the order the corpus allocated them.
    roots: Vec<Eterm>,
}

/// The parts of a run that must be identical across repetitions (everything but the timing).
fn count_signature(r: &Run) -> (MatureKind, GcStats, Option<ImmixStats>, usize, Option<usize>) {
    (r.kind, r.stats, r.immix, r.footprint_words, r.footprint_lines)
}

/// Builds an `objects`-object long-lived corpus of `shape`, promotes it, offers the nursery
/// only garbage, and measures one forced fullsweep.
fn run_once(shape: Shape, objects: usize, kind: MatureKind) -> Run {
    let mut c = collector(shape, objects, kind);
    let mut roots: Vec<Eterm> = Vec::with_capacity(objects);

    // ---- phase 1: the long-lived live set -----------------------------------------------
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..objects as i64 {
            let elems: Vec<Eterm> = (0..shape.arity())
                .map(|k| make_small(i * 100 + k as i64))
                .collect();
            let obj = heap.tuple(&elems).expect("a long-lived object");
            roots.push(obj);
        }
    }
    assert_eq!(roots.len(), objects, "the corpus holds {objects} objects");
    assert!(
        c.young().available_words() >= 4096,
        "the nursery was sized to hold the whole corpus"
    );

    // ---- phase 2: promote (an unmeasured fullsweep) -------------------------------------
    let (_, promote_immix) = fullsweep(&mut c, &mut roots);
    assert_eq!(roots.len(), objects, "the roots survived the promotion");
    if kind == MatureKind::Immix {
        assert!(
            promote_immix.is_some(),
            "the promotion fullsweep must reclaim in place, not relocate"
        );
    }

    // ---- phase 3: fill the (now empty) nursery with nothing but garbage -----------------
    let garbage_objects = c.young().available_words() / (3 + 64);
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..garbage_objects as i64 {
            let _ = heap
                .tuple(&[make_small(i), make_small(2 * i)])
                .expect("garbage tuple");
        }
    }

    // ---- phase 4: the measured fullsweep ------------------------------------------------
    let start = Instant::now();
    let (stats, immix) = fullsweep(&mut c, &mut roots);
    let pause_ns = nanos(start.elapsed());
    if kind == MatureKind::Immix {
        assert!(
            immix.is_some(),
            "the measured fullsweep must reclaim in place, not relocate"
        );
    }

    let (footprint_words, footprint_lines) = match &c {
        AnyCollector::Copying(x) => (x.mature().used_words(), None),
        AnyCollector::Immix(x) => (x.mature().used_words(), Some(x.mature().used_lines())),
    };
    let (moved_objects, moved_words, pinned_objects) = moved(&stats, immix);

    Run {
        kind,
        stats,
        immix,
        footprint_words,
        footprint_lines,
        moved_objects,
        moved_words,
        pinned_objects,
        pause_ns,
        collector: c,
        roots,
    }
}

/// The moved-work triple of a fullsweep: the objects and words the strategy actually moved
/// (line-rounded to whole lines for the mark-region path's evacuations), plus the objects it
/// left exactly where they were.
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

/// Nanoseconds of a pause (µs resolution is too coarse: most of these collections are well
/// under 10 µs).
fn nanos(d: Duration) -> u64 {
    d.as_nanos() as u64
}

/// Runs the corpus [`REPS`] times for `kind`, asserting every repetition did identical work,
/// and returns the last run with its pause replaced by the **fastest** sample.
fn measure(shape: Shape, objects: usize, kind: MatureKind) -> Run {
    let mut last = run_once(shape, objects, kind);
    assert_eq!(last.kind, kind, "the config selected the requested strategy");
    let mut samples = Vec::with_capacity(REPS);
    samples.push(last.pause_ns);
    for rep in 1..REPS {
        let next = run_once(shape, objects, kind);
        assert_eq!(
            count_signature(&next),
            count_signature(&last),
            "{shape:?} {objects} objects {kind:?}: repetition {rep} must do identical work"
        );
        samples.push(next.pause_ns);
        last = next;
    }
    last.pause_ns = *samples.iter().min().expect("at least one sample");
    last
}

/// Asserts the reachable graph is identical under both strategies.
fn assert_same_graph(copying: &Run, immix: &Run, atoms: &AtomTable) {
    assert_eq!(copying.roots.len(), immix.roots.len(), "root count");
    for (i, (a, b)) in copying.roots.iter().zip(&immix.roots).enumerate() {
        assert!(term_eq(*a, *b, atoms), "root {i} differs across strategies");
    }
}

/// The **deterministic law** a size point must satisfy, for a corpus whose every object
/// occupies a whole number of Immix lines (see [`Law`] for the derivation). Every assertion
/// here is on an exact count; nothing is derived from a timing.
fn assert_size_point(shape: Shape, objects: usize, copying: &Run, immix: &Run) {
    let law = Law::of(shape, objects);
    let label = format!("{shape:?} {objects} objects ({} lines)", law.lines);

    // -- the copying path re-copies the whole live set, word for word ---------------------
    assert_eq!(copying.kind, MatureKind::Copying, "{label}");
    assert_eq!(
        copying.moved_objects,
        objects,
        "{label}: the copying path copies every live object"
    );
    assert_eq!(
        copying.moved_words,
        objects * shape.words(),
        "{label}: the copying path copies the exact live word count"
    );
    assert_eq!(copying.pinned_objects, 0, "{label}: copying pins nothing");
    assert_eq!(
        copying.footprint_words,
        objects * shape.words(),
        "{label}: the copying footprint is the exact live word count"
    );
    assert_eq!(copying.footprint_lines, None, "{label}: not a line-granular space");

    // -- the mark-region path promotes nothing: the nursery holds only garbage -----------
    assert_eq!(immix.kind, MatureKind::Immix, "{label}");
    assert_eq!(
        immix.stats.objects_copied, 0,
        "{label}: the nursery held only garbage"
    );
    assert_eq!(immix.stats.words_copied, 0, "{label}: nothing was promoted");
    let stats = immix
        .immix
        .unwrap_or_else(|| panic!("{label}: the fullsweep must reclaim in place"));

    // -- every live object is accounted for exactly once, by the whole-block law ---------
    assert_eq!(
        stats.objects_pinned, law.pinned_objects,
        "{label}: whole blocks are pinned"
    );
    assert_eq!(
        stats.objects_evacuated, law.evacuated_objects,
        "{label}: the trailing partial block is evacuated"
    );
    assert_eq!(
        stats.objects_pinned + stats.objects_evacuated,
        objects,
        "{label}: every live object is accounted for exactly once"
    );
    assert_eq!(
        stats.lines_pinned,
        law.full_blocks * LINES_PER_BLOCK,
        "{label}: pinned lines"
    );
    assert_eq!(
        stats.lines_evacuated, law.remainder_lines,
        "{label}: evacuated lines"
    );
    assert_eq!(
        stats.words_pinned,
        law.pinned_objects * shape.words(),
        "{label}: pinned words are the exact object sizes"
    );
    assert_eq!(stats.blocks_reclaimed, 0, "{label}: the live set is a root");
    assert_eq!(
        stats.words_reclaimed, 0,
        "{label}: nothing in the mature generation is garbage"
    );
    assert_eq!(
        immix.moved_objects,
        law.evacuated_objects,
        "{label}: only the evacuated objects move"
    );
    assert_eq!(
        immix.moved_words,
        law.moved_words(),
        "{label}: moved words are line-rounded"
    );
    // The line-granular footprint is exactly one line per live line.
    assert_eq!(
        immix.footprint_lines,
        Some(law.lines),
        "{label}: resident lines"
    );
    assert_eq!(
        immix.footprint_words,
        law.footprint_words(),
        "{label}: resident line-words"
    );

    if law.full_blocks == 0 {
        // -- below one whole block nothing can be pinned ----------------------------------
        assert_eq!(law.pinned_objects, 0, "{label}: no whole block, nothing to pin");
        if shape.line_waste() > 0 {
            // Line rounding makes the mark-region path strictly dominated, not merely equal.
            assert!(
                immix.moved_words > copying.moved_words,
                "{label}: {} > {} required (line rounding)",
                immix.moved_words,
                copying.moved_words
            );
            assert!(
                immix.footprint_words > copying.footprint_words,
                "{label}: the line-rounded footprint must be strictly larger"
            );
        } else {
            assert_eq!(
                immix.moved_words, copying.moved_words,
                "{label}: whole-line objects are copied either way"
            );
        }
    } else if law.remainder_lines == 0 {
        // -- the live set is a whole number of blocks: it is pinned whole ----------------
        assert_eq!(
            law.pinned_objects, objects,
            "{label}: the whole live set is pinned"
        );
        assert_eq!(
            immix.moved_words, 0,
            "{label}: a fully live block is never moved"
        );
        assert!(
            copying.moved_words > 0,
            "{label}: the copying path still pays a full copy"
        );
    } else {
        // -- a whole block plus a trailing partial block: both halves are exercised -------
        assert!(
            law.pinned_objects > 0 && law.evacuated_objects > 0,
            "{label}: both halves must be non-empty"
        );
    }
}

/// Measures one size point through both strategies and asserts the deterministic law.
fn measure_point(shape: Shape, objects: usize) -> (Run, Run) {
    let copying = measure(shape, objects, MatureKind::Copying);
    let immix = measure(shape, objects, MatureKind::Immix);
    assert_same_graph(&copying, &immix, &AtomTable::new());
    assert_size_point(shape, objects, &copying, &immix);
    (copying, immix)
}

/// One report line for a single (size point, strategy) run.
fn report_line(r: &Run) -> String {
    let footprint = match r.footprint_lines {
        Some(lines) => format!("{} line-words ({lines} lines)", r.footprint_words),
        None => format!("{} words", r.footprint_words),
    };
    format!(
        "  {:>7?}: moved {:>4} obj / {:>5} words | pinned {:>4} obj | reclaimed {:>5} words \
         | footprint {:>24} | pause {:>7.2} µs",
        r.kind,
        r.moved_objects,
        r.moved_words,
        r.pinned_objects,
        r.stats.words_reclaimed,
        footprint,
        r.pause_ns as f64 / 1000.0
    )
}

/// Asserts the sweep's deterministic facts against the rule's constant.
fn assert_rule_against_the_sweep(shape: Shape, points: &[(usize, Run, Run)]) {
    // The constant is expressed in the space's own allocation unit and is at least one whole
    // block — the regime below which `assert_size_point` proves the mark-region path can pin
    // nothing, so selecting it there would be a pure loss.
    assert_eq!(
        IMMIX_BREAK_EVEN_WORDS % BLOCK_WORDS,
        0,
        "the break-even is a whole number of Immix blocks"
    );
    assert_eq!(
        IMMIX_BREAK_EVEN_BLOCKS * BLOCK_WORDS,
        IMMIX_BREAK_EVEN_WORDS,
        "the two constants agree"
    );
    assert!(
        IMMIX_BREAK_EVEN_BLOCKS >= 1,
        "never select Immix below one whole block"
    );

    let mut whole_block_points = 0usize;
    for (objects, copying, immix) in points {
        let law = Law::of(shape, *objects);
        if law.lines < LINES_PER_BLOCK {
            // Every sub-block point is strictly below the rule's threshold, so the rule's
            // floor keeps the copying path exactly where the sweep says it must.
            assert!(
                law.lines * LINE_WORDS < IMMIX_BREAK_EVEN_WORDS,
                "{shape:?} {objects}: a sub-block point must be below the break-even"
            );
            assert_eq!(law.pinned_objects, 0, "{shape:?} {objects}: nothing is pinned");
        } else if law.remainder_lines == 0 {
            whole_block_points += 1;
            // The whole-block points carry the rule's positive half: the mark-region path
            // moves *nothing* while the copying path pays the whole live set.
            assert_eq!(immix.moved_words, 0, "{shape:?} {objects}: pinned whole");
            assert!(
                copying.moved_words > 0,
                "{shape:?} {objects}: the copying path pays"
            );
        }
    }
    // There is at least a point at one block and one at 64, so the "from one block up" claim
    // is checked at more than a single size.
    assert!(
        whole_block_points >= 2,
        "{shape:?}: the sweep must cover several whole-block points"
    );
    println!(
        "  (asserted) {shape:?}: below one whole block nothing is pinned (line-rounded \
         evacuation, never fewer words than copying); at every whole-block point from 1 to 64 \
         blocks the live set is pinned whole (0 words moved). The constant's *value* \
         ({IMMIX_BREAK_EVEN_BLOCKS} blocks / {IMMIX_BREAK_EVEN_WORDS} words) is the \
         timing-derived ratio plateau, not an asserted count."
    );
}

/// Measures one shape at every size point, prints the table, and asserts both the per-point
/// deterministic law and the rule's assertion-backed bounds.
fn sweep(shape: Shape) -> Vec<(usize, Run, Run)> {
    let mut points = Vec::new();
    println!(
        "\n=== break-even sweep: shape {shape:?} ({} words/object, {} lines/object) ===",
        shape.words(),
        shape.lines()
    );
    for &objects in &OBJECT_POINTS {
        let (copying, immix) = measure_point(shape, objects);
        let law = Law::of(shape, objects);
        println!(
            "-- {objects} objects ({:.2} block-equivalents; {} line-words live, {} useful \
             words) --",
            law.lines as f64 / LINES_PER_BLOCK as f64,
            law.lines * LINE_WORDS,
            objects * shape.words()
        );
        println!("{}", report_line(&copying));
        println!("{}", report_line(&immix));
        println!(
            "  law (asserted): pinned {:>4} obj / {:>3} lines | evacuated {:>4} obj / {:>3} \
             lines | moved {:>5} words",
            law.pinned_objects,
            law.full_blocks * LINES_PER_BLOCK,
            law.evacuated_objects,
            law.remainder_lines,
            law.moved_words()
        );
        println!(
            "  ratio: moved words copying/immix = {:.2}× | footprint copying/immix = {:.2}× | \
             pause immix/copying = {:.2}× | words saved per extra µs = {:.0}",
            copying.moved_words as f64 / immix.moved_words.max(1) as f64,
            copying.footprint_words as f64 / immix.footprint_words.max(1) as f64,
            immix.pause_ns as f64 / copying.pause_ns.max(1) as f64,
            (copying.moved_words as f64 - immix.moved_words as f64)
                / ((immix.pause_ns as f64 - copying.pause_ns as f64) / 1000.0).max(0.001),
        );
        points.push((objects, copying, immix));
    }
    assert_rule_against_the_sweep(shape, &points);
    points
}

#[test]
fn break_even_table_line_objects() {
    let _serial = serial();
    sweep(Shape::Line);
}

#[test]
fn break_even_table_small_objects() {
    let _serial = serial();
    sweep(Shape::Small);
}

#[test]
fn break_even_table_multi_line_objects() {
    let _serial = serial();
    sweep(Shape::Multi);
}

/// The constant checked at **its own size**, in the sweep's own units: at the threshold the
/// mark-region path is inside its full-pinning regime (0 words moved while the copying path
/// pays a break-even-sized copy), and one line below it it is not (the trailing partial block
/// must be evacuated).
///
/// This is the only part of the constant's *value* that a count can pin. Its exact size — 8
/// blocks rather than 1 — comes from the timing-derived plateau of avoided copy work per
/// extra µs of pause (see the module docs), which is deliberately never asserted.
#[test]
fn the_break_even_point_is_assertion_checked() {
    let _serial = serial();

    // A whole line of live data is `LINE_WORDS` words, so the rule's word threshold is this
    // many whole lines — exactly `IMMIX_BREAK_EVEN_BLOCKS` whole blocks.
    let break_even_lines = IMMIX_BREAK_EVEN_WORDS / LINE_WORDS;
    assert_eq!(
        break_even_lines,
        IMMIX_BREAK_EVEN_BLOCKS * LINES_PER_BLOCK,
        "the word threshold is a whole number of lines and blocks"
    );

    for shape in Shape::ALL {
        let objects = break_even_lines / shape.lines();
        assert_eq!(
            objects * shape.lines(),
            break_even_lines,
            "{shape:?}: the threshold is an exact size point"
        );

        let (at_copying, at_immix) = measure_point(shape, objects);
        println!(
            "break-even point {shape:?}: {objects} objects = {break_even_lines} lines = \
             {IMMIX_BREAK_EVEN_BLOCKS} blocks -> copying moves {} words, immix {} words \
             ({} pinned)",
            at_copying.moved_words, at_immix.moved_words, at_immix.pinned_objects
        );
        assert!(
            at_copying.moved_words > 0,
            "{shape:?}: the copying path pays for the whole live set"
        );
        assert_eq!(
            at_immix.moved_words, 0,
            "{shape:?}: at the threshold the whole live set is pinned"
        );
        assert_eq!(
            at_immix.pinned_objects, objects,
            "{shape:?}: every live object is pinned at the threshold"
        );

        // One line below the threshold the live set no longer fills whole blocks: the
        // trailing partial block must be evacuated, so the rule's switch point is the point
        // at which the mark-region path reaches its best regime, not one line past it.
        let (below_copying, below_immix) = measure_point(shape, objects - 1);
        println!(
            "  one line below: {} objects -> copying {} words, immix {} words ({} pinned)",
            objects - 1,
            below_copying.moved_words,
            below_immix.moved_words,
            below_immix.pinned_objects
        );
        assert!(
            below_immix.moved_words > 0,
            "{shape:?}: below the threshold the partial block is evacuated"
        );
        assert!(
            below_immix.pinned_objects < below_copying.moved_objects,
            "{shape:?}: below the threshold not every object is pinned"
        );
    }

    // For the one-line shape the threshold is also the copy the rule avoids, word for word.
    let (copying, immix) = measure_point(Shape::Line, break_even_lines);
    assert_eq!(
        copying.moved_words, IMMIX_BREAK_EVEN_WORDS,
        "the copying path pays exactly the break-even size"
    );
    assert_eq!(immix.moved_words, 0);
}
