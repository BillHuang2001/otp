//! **Break-even re-measurement** for the automatic mature-generation selection rule.
//!
//! `immix_workloads.rs` compares the two mature strategies on three *shapes* at one size
//! each. The selection rule needs a different axis: **how does each strategy scale with the
//! size of a long-lived mature generation?** This file answers exactly that by driving the
//! *same* long-lived corpus at several size points — from a quarter of an Immix block up to
//! 64 whole blocks of live data — through both strategies
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
//! Objects are **one whole 16-word line** each ([`Shape::Line`]), which is the shape the
//! mark-region space is designed for: its line granularity wastes nothing, so its resident
//! footprint equals the copying path's and the only difference is *how much it moves*. The
//! same sweep is also run over [`Shape::Small`] (3-word tuples, 3 useful words per 16-word
//! line ⇒ 5.3× line-rounding waste) — the mark-region space's worst case, and the shape that
//! pins the rule's threshold (its benefit/cost ratio is the one that plateaus at 8 blocks).
//!
//! # Determinism, and honesty about the timing column
//!
//! Every *assertion* is on an exact or relational count: both strategies must find the same
//! live set (the reachable graphs are compared with the crate's own `beam_term::compare`),
//! the copying fullsweep's copy is pinned to the corpus's exact word count, and the
//! mark-region path must either pin the whole live set (whole blocks) or evacuate it
//! line-rounded (sub-block live sets). Nothing is asserted about the pause times — they are
//! printed only, as the *fastest* of [`REPS`] runs (measurement noise can only add time, so
//! the minimum is the cleanest estimate for a deterministic corpus); absolute µs numbers are
//! meaningless, only the copying-vs-mark-region ratio at one size point carries information.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that a
//! collection just wrote); every block carries a `// SAFETY:` rationale.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use beam_gc::prelude::{
    AnyCollector, GcConfig, GcStats, HeapPolicy, ImmixStats, MatureKind, LINES_PER_BLOCK,
    LINE_WORDS,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{make_small, AtomTable, Eterm};

/// Timed repetitions of the forced fullsweep per (size point, strategy); the FASTEST sample
/// is reported.
const REPS: usize = 9;

/// The size points, as object counts. `LINES_PER_BLOCK` (16) objects fill one whole Immix
/// block, so the points are 1/4, 1/2, 1, 2, 8, 32 and 64 blocks.
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
    /// A 16-word tuple: exactly one line (no resident overhead).
    Line,
}

impl Shape {
    /// Useful words per object (a tuple is `1 + arity`).
    fn words(self) -> usize {
        match self {
            Shape::Small => 3,
            Shape::Line => LINE_WORDS,
        }
    }
}

/// The heap policy both strategies share at `objects` long-lived objects: the fullsweep
/// cadence disabled (every collection measured is one this file explicitly asked for), a
/// nursery that holds the whole corpus, and a mature capacity `2 × (live + reserve)` — the
/// fixed point of [`HeapPolicy::mature_after_fullsweep`] that keeps the mark-region fullsweep
/// **in place** (neither `should_grow` nor `should_shrink` fires), so the pinning (or
/// partial-block evacuation) regime is what is measured, never a relocating fullsweep.
fn policy(objects: usize) -> HeapPolicy {
    let need_reserve_words = 64;
    let live_line_words = objects * LINE_WORDS;
    HeapPolicy {
        fullsweep_after: 65535,
        initial_young_words: live_line_words + 4096,
        initial_mature_words: 2 * (live_line_words + need_reserve_words),
        need_reserve_words,
        ..HeapPolicy::default()
    }
}

/// A collector for `kind`, selected through [`GcConfig`] exactly as a caller would.
fn collector(objects: usize, kind: MatureKind) -> AnyCollector {
    let config = GcConfig::copying(policy(objects)).with_mature(kind);
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

/// Builds an `objects`-object long-lived corpus, promotes it, offers the nursery only
/// garbage, and measures one forced fullsweep.
fn run_once(shape: Shape, objects: usize, kind: MatureKind) -> Run {
    let mut c = collector(objects, kind);
    let mut roots: Vec<Eterm> = Vec::with_capacity(objects);

    // ---- phase 1: the long-lived live set -----------------------------------------------
    {
        let heap = c.young_mut().heap_mut();
        for i in 0..objects as i64 {
            let obj = match shape {
                Shape::Small => heap.tuple(&[make_small(i), make_small(2 * i)]),
                Shape::Line => {
                    let elems: Vec<Eterm> = (0..(LINE_WORDS as i64 - 1))
                        .map(|k| make_small(i * 100 + k))
                        .collect();
                    heap.tuple(&elems)
                }
            };
            roots.push(obj.expect("a long-lived object"));
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

/// Measures one shape at every size point and prints the table, returning the pairs.
fn sweep(shape: Shape) -> Vec<(Run, Run)> {
    let mut pairs = Vec::new();
    println!("\n=== break-even sweep: shape {shape:?} ({} words/object) ===", shape.words());
    for &objects in &OBJECT_POINTS {
        let copying = measure(shape, objects, MatureKind::Copying);
        let immix = measure(shape, objects, MatureKind::Immix);
        assert_same_graph(&copying, &immix, &AtomTable::new());
        println!(
            "-- {objects} objects ({} block-equivalents; {} line-words live, {} useful words) --",
            objects as f64 / LINES_PER_BLOCK as f64,
            objects * LINE_WORDS,
            objects * shape.words()
        );
        println!("{}", report_line(&copying));
        println!("{}", report_line(&immix));
        println!(
            "  ratio: moved words copying/immix = {:.2}× | footprint copying/immix = {:.2}× | \
             pause immix/copying = {:.2}× | words saved per extra µs = {:.0}",
            copying.moved_words as f64 / immix.moved_words.max(1) as f64,
            copying.footprint_words as f64 / immix.footprint_words.max(1) as f64,
            immix.pause_ns as f64 / copying.pause_ns.max(1) as f64,
            (copying.moved_words as f64 - immix.moved_words as f64)
                / ((immix.pause_ns as f64 - copying.pause_ns as f64) / 1000.0).max(0.001),
        );
        pairs.push((copying, immix));
    }
    pairs
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
