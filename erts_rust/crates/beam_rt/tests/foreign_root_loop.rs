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
//! **The unscanned-root valve, proved end to end: a loop that holds a foreign-rooted term
//! still collects.**
//!
//! `src/context/gc.rs::root_is_unscanned` declines a collection only when a root (or a
//! root-reachable object) is a container in a region the collector does **not** scan *and*
//! forwards at least one word the collector *would* have to follow or update
//! (`forwards_scanned_word`). A foreign container of immediates therefore forwards nothing a
//! relocation touches, so it is **inert** and must not decline — and the per-pid descent latch
//! (`Runtime::note_foreign_reachable`) that a genuine hazard opens is **cleared again** by the
//! two full admission doors (`ensure_heap_full`, `alloc_words_rooted`) when their descent ran
//! over the machine's whole root set and reached no foreign container
//! (`ValveVerdict::foreign_free` → `Runtime::close_foreign_reachable`).
//!
//! This suite is the acceptance evidence for both halves, through the real runtime:
//!
//! 1. **The collector keeps running.** `vp:hold/1` holds the 3-tuple
//!    `erlang:statistics(garbage_collection)` returns — a `RuntimeHooks`-served BIF's result, so
//!    it lives on the pooled BIF heap, a region the collector does not scan — as its own
//!    accumulator for the whole loop. Its three elements are small integers, so the tuple
//!    forwards no scanned word. The loop must still collect, and collect **more** the longer it
//!    runs.
//! 2. **The count is a derived quantity, not a range.** Every iteration of the loop reserves
//!    exactly [`WORDS_PER_ITERATION`] words for `junk/1`'s abandoned term, so a
//!    [`EMULATOR_MIN_HEAP_WORDS`]-word nursery holds [`ITERATIONS_PER_COLLECTION`] of them and the
//!    run's collection count follows the **exact** fill law [`collections_law`] —
//!    `(N + `[`FILL_OFFSET`]`) / `[`ITERATIONS_PER_COLLECTION`]. Every measured `N` pins that exact
//!    count (the style `src/runner/tests.rs`'s `counter_table` uses for its own growth shapes);
//!    no measured row needs the slack a boundary collection could add, see [`collections_law`].
//! 3. **Memory stays bounded.** The process's `heap_size` (the nursery chain's capacity, the
//!    number `process_info(self(), heap_size)` reports) is flat in `N`: it sits at one of two
//!    collector-geometry plateaus, never on a curve.
//! 4. **The foreign root costs nothing.** `vp:control/1` is the same loop reading only
//!    `element(1, ...)` — a small integer — so no foreign container is ever a root. Its counters
//!    must equal `vp:hold/1`'s **exactly**: an inert foreign container neither declines a
//!    collection nor changes the allocation shape.
//!
//! # What this would catch (measured)
//!
//! With the pre-rework classification — "any container from an unscanned region is a hazard",
//! `forwards_scanned_word` answering unconditionally — `vp:hold/1`'s statistics tuple froze its
//! collection count: the *run's own* delta measured **0** collections at every `N`, while its
//! `heap_size` climbed linearly (121 387 / 318 037 / 2 032 432 words at N = 5 000 / 10 000 /
//! 100 000), and `vp:control/1` was untouched (294 / 588 / 5 882 collections, a 233-word heap).
//! Both tests below fail on that behaviour — the "collections grow with N" and the "heap is
//! bounded" assertions, and the hold-vs-control equality.
//!
//! # Recording discipline
//!
//! Only **deterministic counters** are asserted — collections, their major/failed splits, the
//! arena-fallback count and the collector's own capacity/occupancy figures. No wall time is
//! measured or asserted anywhere (this host is shared), and no figure is compared against the
//! oracle's: the allocation *shapes* differ (the oracle's own `hold` reports a different count at
//! the same `N`), and only the **shape** — collections growing, heap flat — is a property that
//! must hold for both runtimes.
//!
//! # Fixture
//!
//! `tests/fixtures/vp.beam` is a committed `.beam` image produced by the system `erlc`
//! (`+deterministic`; the recipe is in `tests/fixtures/README.md`), so the suite needs no
//! compiler — only the system Erlang installation the boot sequence resolves. A missing
//! installation is a **loud skip**, never a silent pass.
// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;
use std::time::{Duration, Instant};
use beam_engine::terms;
use beam_gc::heap_info::HeapObservable;
use beam_rt::Runtime;
use beam_term::{is_small, make_small, signed_value};
/// The committed fixture: `hold/1` (the loop that keeps the BIF-heap statistics tuple live) and
/// `control/1` (the same loop with an integer accumulator).
const VP_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vp.beam");
/// The iteration counts each measured shape is driven at. A 20× span, so a count that is
/// *constant* in `N` (the frozen pre-fix behaviour) cannot be mistaken for a count that grows;
/// every row is a **multi-slice** run (2, 2, 3, 8 and 26 scheduler slices), so the sample-based
/// heap assertions below always have a live process to read; and `N = 7 000` / `30 000` are the
/// rows whose remainder modulo [`ITERATIONS_PER_COLLECTION`] makes the [`FILL_OFFSET`] observable
/// (`7 006 / 17 = 412` against `7 000 / 17 = 411`, `30 006 / 17 = 1 765` against `30 000 / 17 =
/// 1 764`), so the phase term is load-bearing rather than absorbed by the floor — the three
/// original counts (`5 000`, `10 000`, `100 000`, remainders 2 / 4 / 6) all happen to sit on the
/// same floor with or without it.
const COUNTS: [i64; 5] = [5_000, 7_000, 10_000, 30_000, 100_000];
/// What a release build's `process_info(P, heap_size)` may report over a whole run: the
/// collector's two nursery-capacity plateaus, 233 words (the emulator's `min_heap_size`) and
/// 843 words (that plus a grown space), measured at every slice boundary of every run below.
/// It is a **constant** — the point of the assertion is that it does not move with `N`.
const HEAP_SIZE_BOUND_WORDS: usize = 843;
/// A generous wall-clock safety net for one measured run (the loops are pure interpreter work
/// and finish well inside it; this only bounds a pathological hang, it is never measured).
const RUN_DEADLINE: Duration = Duration::from_secs(120);
/// What one scheduled run of a fixture's loop did.
///
/// Every field is a **deterministic** structural counter: `Runtime::gc_stats()` deltas across
/// the run, the collector geometry sampled at each slice boundary, and the value the loop
/// returned.
#[derive(Debug, Clone, Copy)]
struct LoopRun {
    /// Collections the runtime ran during this run (minor **and** major).
    collections: u64,
    /// How many of [`LoopRun::collections`] were fullsweeps.
    major: u64,
    /// Engine allocations that fell back to the pointer-stable arena during this run.
    fallbacks: u64,
    /// Collections that failed (each one halts its own process).
    errors: u64,
    /// The collection counter the loop's **own last** `erlang:statistics(garbage_collection)`
    /// call read, i.e. the first element of the value the fixture returned.
    returned_collections: i64,
    /// The `process_info(self(), heap_size)` the loop read on its last iteration — the second
    /// element of the returned value.
    returned_heap_size: i64,
    /// The largest `heap_size` (nursery chain capacity, words) observed at a slice boundary.
    peak_heap_size: usize,
    /// The largest young-heap occupancy (words) observed at a slice boundary.
    peak_used: usize,
    /// Scheduler slices the run took.
    slices: usize,
}
/// A runtime with the fixture loaded and the boot sequence already driven to quiescence.
///
/// The drain matters: `Runtime::spawn_process` boots on demand, and the boot script's own
/// processes would otherwise contend with the measured loop for slices (and add their own
/// collections to the counter deltas).
///
/// A tree without a discoverable system Erlang installation cannot resolve the code path, so
/// this suite reports a **loud skip** there ([`support::require_oracle`]), never a silent pass.
fn runtime_with_fixture() -> Option<Runtime> {
    support::require_oracle("foreign_root_loop")?;
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    let mut guard = 0u32;
    while rt.run_one_slice().expect("a boot-drain slice runs") > 0 {
        guard += 1;
        assert!(guard < 100_000, "the boot process tree never quiesced");
    }
    rt.load_module(VP_BEAM).expect("the vp fixture loads");
    Some(rt)
}
/// Runs `vp:function/1` with argument `n` as a **scheduled process**, counting every
/// deterministic counter the runtime exposes and sampling the process's own collector at every
/// slice boundary.
///
/// The sampling window is the only one there is: a terminated process leaves the table, so its
/// collector's geometry is only readable *while* it lives.
fn run_loop(rt: &mut Runtime, function: &str, n: i64) -> LoopRun {
    let before = rt.gc_stats();
    let pid = rt
        .spawn_process("vp", function, vec![make_small(n)])
        .unwrap_or_else(|err| panic!("vp:{function}/1 must spawn: {err}"));
    let mut peak_heap_size = 0usize;
    let mut peak_used = 0usize;
    let mut slices = 0usize;
    let deadline = Instant::now() + RUN_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "vp:{function}({n}) did not terminate within {}s",
            RUN_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
        slices += 1;
        if let Some(process) = rt.processes().get(pid) {
            peak_heap_size = peak_heap_size.max(process.collector().heap_size_words());
            peak_used = peak_used.max(process.heap_used_words());
        }
    }
    let value = rt.process_result(pid).expect("the loop returned a value");
    let (returned_collections, returned_heap_size) = decode_result(value, function, n);
    let after = rt.gc_stats();
    LoopRun {
        collections: after.collections - before.collections,
        major: after.major - before.major,
        fallbacks: after.fallbacks - before.fallbacks,
        errors: after.errors - before.errors,
        returned_collections,
        returned_heap_size,
        peak_heap_size,
        peak_used,
        slices,
    }
}
/// Decodes the `{Collections, HeapSize}` tuple the fixture returns, refusing any other shape
/// with a diagnosable panic (a loop that returned something else is a fixture/engine bug, not a
/// measurable outcome).
fn decode_result(value: beam_term::Eterm, function: &str, n: i64) -> (i64, i64) {
    let arity = terms::tuple_arity(value)
        .unwrap_or_else(|err| panic!("vp:{function}({n}) returned a non-tuple: {err}"));
    assert_eq!(arity, 2, "vp:{function}({n}) returns a {{Collections, HeapSize}} pair");
    let mut ints = [0i64; 2];
    for (slot, element) in ints.iter_mut().enumerate() {
        let word = terms::tuple_element(value, slot)
            .unwrap_or_else(|err| panic!("vp:{function}({n})'s element {slot}: {err}"));
        assert!(
            is_small(word),
            "vp:{function}({n})'s element {slot} must be a small integer"
        );
        *element = signed_value(word);
    }
    (ints[0], ints[1])
}
/// Runs one shape at each iteration count in `counts` (a **fresh** runtime each time, so every row
/// is a per-run delta) and prints the counter table; every counter is reported — including the
/// fill law's prediction and the measured surplus over it — so a reader needs no access to this
/// source to read the numbers.
fn counter_table(function: &str, counts: &[i64]) -> Vec<(i64, LoopRun)> {
    let rows: Vec<(i64, LoopRun)> = counts
        .iter()
        .map(|n| {
            let mut rt = runtime_with_fixture().expect("the fixture runtime");
            let row = run_loop(&mut rt, function, *n);
            assert_eq!(row.errors, 0, "no collection may fail: vp:{function}({n}) {row:?}");
            assert!(
                !rt.gc_stats().disabled,
                "the global GC valve must stay closed: vp:{function}({n})"
            );
            (*n, row)
        })
        .collect();
    eprintln!(
        "counter_table vp:{function} (N : collections major fallbacks errors slices \
         peak_heap peak_used returned | law surplus)"
    );
    for (n, row) in &rows {
        eprintln!(
            "counter_table vp:{function} N={n} : collections={} major={} fallbacks={} \
             errors={} slices={} peak_heap={} peak_used={} returned={:?} | law={} surplus={}",
            row.collections,
            row.major,
            row.fallbacks,
            row.errors,
            row.slices,
            row.peak_heap_size,
            row.peak_used,
            (row.returned_collections, row.returned_heap_size),
            collections_law(*n),
            row.collections as i64 - collections_law(*n) as i64,
        );
    }
    rows
}
/// The words one iteration of the loop's body reserves — and uses — on the process heap.
///
/// Read off the fixture's own Erlang assembly (`erlc +deterministic`; the recipe is in
/// `tests/fixtures/README.md`): `vp:junk/1` emits `test_heap,13,3` and then exactly three
/// `put_list` and two `put_tuple2` instructions, i.e. three 2-word cons cells (6 words) + the
/// 2-tuple `{N, N+1}` (1 header + 2 = 3) + the 3-tuple `{N, [N, N+1, N+2], {N, N+1}}` (1 header +
/// 3 = 4) = **13 words**, `test_heap`'s reserve being exactly that use. Every element is a small
/// integer, hence an immediate, so nothing else is heap resident: `erlang:statistics/1`'s result is
/// built on the pooled BIF heap, and `control/1`'s `element/2` read allocates nothing at all.
const WORDS_PER_ITERATION: u64 = 13;
/// The nursery capacity a fresh process is created with — the emulator's `min_heap_size` in words
/// (`Process::collector().heap_size_words()` on a just-spawned process), and the geometry the fill
/// law is derived from.
const EMULATOR_MIN_HEAP_WORDS: u64 = 233;
/// How many loop iterations one nursery space holds — **derived**, not fitted:
/// `233 / 13 = 17.9`, so 17 iterations consume 221 of the 233 words while an 18th (234) would not
/// fit. A collection therefore falls on every 17th iteration, and that the rate really is the
/// space's capacity and not something larger also proves the space is **drained** each cycle: a
/// collection that kept more than `233 - 221 = 12` words live would hold fewer iterations.
const ITERATIONS_PER_COLLECTION: u64 = EMULATOR_MIN_HEAP_WORDS / WORDS_PER_ITERATION;
/// The measured **phase offset** of the fill law, in iterations — the loop's *first* cycle is
/// `FILL_OFFSET` iterations short of the steady-state 17, because the nursery a freshly spawned
/// process arrives with already carries its entry frame and stack. Pinned directly by
/// `the_fill_law_holds_and_its_phase_is_the_entry_frame` (the loop collects `0` times at `N = 10`
/// and `1` time at `N = 11`, so the first collection lands on iteration
/// `ITERATIONS_PER_COLLECTION - FILL_OFFSET` = 11).
const FILL_OFFSET: u64 = 6;
/// The collection count one measured run of either shape must report at `N` — the **exact** fill
/// law, `(N + `[`FILL_OFFSET`]`) / `[`ITERATIONS_PER_COLLECTION`]`.
///
/// # The derivation
///
/// * **The rate is arithmetic.** Each iteration reserves exactly [`WORDS_PER_ITERATION`] = 13 words
///   for the abandoned `junk/1` term, and the fresh process's nursery is
///   [`EMULATOR_MIN_HEAP_WORDS`] = 233 words, so one space absorbs
///   [`ITERATIONS_PER_COLLECTION`] = `233 / 13` = 17 iterations (`17 × 13 = 221 ≤ 233 < 234 =
///   18 × 13`). Every steady-state cycle is exactly that many iterations long.
/// * **The phase is the entry frame.** The space does not start empty — the spawned process's
///   entry frame and stack are already on it — so the first cycle is short. Measured, not guessed
///   (`the_fill_law_holds_and_its_phase_is_the_entry_frame` pins both sides of the boundary): the
///   first collection lands on **iteration 11**, which is what the `+ `[`FILL_OFFSET`]` term
///   encodes.
/// * **Nothing else contributes.** The reduction/yield-boundary collection door
///   (`src/context/gc/reds_door.rs`) could in principle add a collection at a slice boundary that
///   the heap-admission points would not have run — that is what makes a per-`slices` sandwich the
///   natural fallback. It does not here: at its 7/8 watermark a boundary collection needs ≥ 204 of
///   the 233 words used, a depth this loop only reaches inside the very fill an admission point is
///   about to complete anyway, so a boundary collection can only *coincide* with a cycle and never
///   steal one. Measured surplus over the law is **0** at every surveyed `N`, so the law is exact
///   well beyond the one-slice regime and no sandwich term is needed.
///
/// # Verified against 71 measured rows, all exact (surplus 0)
///
/// `N = 1 … 40` (both sides of every cycle boundary, so [`FILL_OFFSET`] is pinned), `N =
/// 1 000 … 1 034`, `N = 100 000 … 100 010`, `N = 200 000` and `N = 500 000` (126 slices), plus the
/// 13-point ladder `1 000 / 2 000 / 3 000 / 5 000 / 7 000 / 10 000 / 15 000 / 20 000 / 30 000 /
/// 50 000 / 70 000 / 100 000 / 130 000` (up to 33 slices) — every residue class modulo 17 in both
/// the one-slice and the multi-slice regime. This is the relation a foreign root that declined
/// collections, a heap that stopped draining, or a boundary collection that stole a cycle breaks.
fn collections_law(n: i64) -> u64 {
    (n as u64 + FILL_OFFSET) / ITERATIONS_PER_COLLECTION
}
/// **The headline evidence: a loop holding a BIF-heap (foreign) term still collects, more the
/// longer it runs, on a bounded heap.**
///
/// `vp:hold/1` keeps `erlang:statistics(garbage_collection)`'s 3-tuple — an all-immediate
/// container on the pooled BIF heap — as its accumulator across the whole loop, so it is a live
/// root at every one of the loop's heap-admission points. Under the current valve that root is
/// inert: it forwards no word a relocation would follow, so nothing declines, the descent latch
/// never opens, and the counts must be exactly what the scanned control sees.
#[test]
fn a_loop_holding_the_foreign_statistics_tuple_keeps_collecting() {
    let rows = counter_table("hold", &COUNTS);
    // The collector really ran, and it ran more the longer the loop ran: the pre-fix behaviour
    // (the foreign root latching collection off) reports the run's start-up count at every N.
    assert!(
        rows[0].1.collections > 0,
        "vp:hold must collect at all: {:?}",
        rows[0].1
    );
    for pair in rows.windows(2) {
        let (n_a, a) = pair[0];
        let (n_b, b) = pair[1];
        assert!(
            a.collections < b.collections,
            "the collection count must grow with N (N={n_a} → {}, N={n_b} → {})",
            a.collections,
            b.collections,
        );
    }
    // ... and it grows at exactly the derived rate, with exactly the derived phase: the nursery
    // holds ITERATIONS_PER_COLLECTION (233 / 13) iterations of `junk/1`'s allocation, and the
    // first cycle is FILL_OFFSET iterations short because the spawned process's entry frame is
    // already in the space (see `collections_law`).
    for (n, row) in &rows {
        assert_eq!(
            row.collections,
            collections_law(*n),
            "collections must be (N + {FILL_OFFSET}) / {ITERATIONS_PER_COLLECTION} at N={n} \
             ({EMULATOR_MIN_HEAP_WORDS} nursery words / {WORDS_PER_ITERATION} words per \
             iteration): {row:?}"
        );
    }
    // The heap stays bounded: a flat capacity, never a curve in N. (The pre-fix run's
    // `heap_size` climbed to 318 037 words at N = 10 000 and 2 032 432 at N = 100 000.)
    for (n, row) in &rows {
        assert!(
            row.peak_heap_size > 0 && row.peak_heap_size <= HEAP_SIZE_BOUND_WORDS,
            "the heap must stay on its geometry plateau at N={n} (peak {} words, bound \
             {HEAP_SIZE_BOUND_WORDS}): {row:?}",
            row.peak_heap_size,
        );
        assert!(
            row.returned_heap_size > 0
                && (row.returned_heap_size as usize) <= HEAP_SIZE_BOUND_WORDS,
            "the loop's own process_info(heap_size) must stay bounded at N={n}: {row:?}"
        );
    }
    // The run really was the loop (it terminated and returned the loop's value), and every
    // allocation stayed on the process's own young heap — no arena fallback, i.e. no decline.
    for (n, row) in &rows {
        assert_eq!(
            row.fallbacks, 0,
            "an inert foreign root must not decline a collection at N={n}: {row:?}"
        );
        assert!(
            row.returned_collections > 0,
            "the loop's own statistics reading must see collections at N={n}: {row:?}"
        );
        assert!(
            row.returned_collections as u64 <= row.collections,
            "the loop's reading is a snapshot of this run's count at N={n}: {row:?}"
        );
    }
}
/// **The fill law's phase, pinned directly: the first collection lands on iteration 11.**
///
/// The rate of [`collections_law`] is arithmetic (`233 / 13 = 17` iterations per nursery space);
/// its `+ `[`FILL_OFFSET`]` phase is the one measured constant, and this test measures it where it
/// is unambiguous, in the one-slice regime where no boundary collection can be involved. The loop
/// collects `0` times up to `N = 10` and `1` time from `N = 11` on: the first space fills after 11
/// iterations (11 × 13 = 143 words) because the process arrives with its entry frame already in it
/// (the `u0` that satisfies `u0 + 11 × 13 + 13 > 233 ≥ u0 + 10 × 13 + 13` is 78 … 90 words), so
/// the first cycle is `ITERATIONS_PER_COLLECTION - 11 = `[`FILL_OFFSET`] iterations short — which
/// is exactly the `+ FILL_OFFSET` phase term, with `(N + 6) / 17` reproducing every row below
/// including both sides of the next cycle boundary (`N = 27` → 1, `N = 28` → 2).
#[test]
fn the_fill_law_holds_and_its_phase_is_the_entry_frame() {
    // (N, the collections the law predicts and the run must report)
    let ladder: [(i64, u64); 5] = [(0, 0), (10, 0), (11, 1), (27, 1), (28, 2)];
    for (n, expected) in ladder {
        // The law's own prediction, so the table above cannot drift from `collections_law`.
        assert_eq!(collections_law(n), expected, "the law at N={n}");
        let rows = counter_table("hold", &[n]);
        let (_, row) = rows[0];
        assert_eq!(
            row.collections,
            expected,
            "vp:hold({n}) must collect exactly {expected} time(s): the first space fills on \
             iteration {first}, then every {ITERATIONS_PER_COLLECTION} iterations: {row:?}",
            first = ITERATIONS_PER_COLLECTION - FILL_OFFSET,
        );
    }
}
/// **The control: the foreign root costs exactly nothing.**
///
/// `vp:control/1` is the same loop with an integer accumulator (`element(1, ...)`), so no
/// foreign container is ever a root. Every counter must equal `vp:hold/1`'s — same collections,
/// same splits, same heap plateaus, same returned figures. A difference of one collection is
/// the signal that the valve classified the statistics tuple as a hazard.
#[test]
fn the_foreign_held_loop_matches_its_scanned_control() {
    let held = counter_table("hold", &COUNTS);
    let control = counter_table("control", &COUNTS);
    for ((n, held), (_, control)) in held.into_iter().zip(control) {
        assert_eq!(
            (held.collections, held.major, held.fallbacks, held.errors),
            (control.collections, control.major, control.fallbacks, control.errors),
            "vp:hold and vp:control must run identically at N={n} (the foreign root is inert): \
             hold={held:?} control={control:?}"
        );
        assert_eq!(
            (
                held.peak_heap_size,
                held.returned_heap_size,
                held.returned_collections
            ),
            (
                control.peak_heap_size,
                control.returned_heap_size,
                control.returned_collections
            ),
            "both shapes must sit on the same heap plateau and read the same counters at N={n}: \
             hold={held:?} control={control:?}"
        );
    }
}
