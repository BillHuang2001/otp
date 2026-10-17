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
//! 2. **The count is a derived quantity, not a range.** The measured table (see
//!    [`counter_table`]) is an exact affine relation in `N` — every iteration's abandoned
//!    allocation fills the nursery at a constant rate — so the tests pin the **exact** count at
//!    each measured `N` (the style `src/runner/tests.rs`'s `counter_table` uses for its own
//!    growth shapes).
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
/// The iteration counts each measured shape is driven at: a 20× span, so a count that is
/// *constant* in `N` (the frozen pre-fix behaviour) cannot be mistaken for a count that grows.
const COUNTS: [i64; 3] = [5_000, 10_000, 100_000];
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
/// Runs one shape at three increasing iteration counts (a **fresh** runtime each time, so every
/// row is a per-run delta) and prints the counter table; every counter is reported so a reader
/// needs no access to this source to read the numbers.
fn counter_table(function: &str, counts: [i64; 3]) -> [(i64, LoopRun); 3] {
    let rows = counts.map(|n| {
        let mut rt = runtime_with_fixture().expect("the fixture runtime");
        let row = run_loop(&mut rt, function, n);
        assert_eq!(row.errors, 0, "no collection may fail: vp:{function}({n}) {row:?}");
        assert!(
            !rt.gc_stats().disabled,
            "the global GC valve must stay closed: vp:{function}({n})"
        );
        (n, row)
    });
    eprintln!(
        "counter_table vp:{function} (N : collections major fallbacks errors slices \
         peak_heap peak_used returned | collections/N)"
    );
    for (n, row) in &rows {
        eprintln!(
            "counter_table vp:{function} N={n} : collections={} major={} fallbacks={} \
             errors={} slices={} peak_heap={} peak_used={} returned={:?} | collections/N={:.6}",
            row.collections,
            row.major,
            row.fallbacks,
            row.errors,
            row.slices,
            row.peak_heap_size,
            row.peak_used,
            (row.returned_collections, row.returned_heap_size),
            row.collections as f64 / *n as f64,
        );
    }
    rows
}
/// The collection count each shape must report at a measured `N`, derived from the measured
/// table and pinned exactly.
///
/// **The derivation (not a fitted curve).** Every iteration builds the same abandoned term
/// (`vp:junk/1`: a 3-tuple holding a 3-element list and a 2-tuple) on the process's own young
/// heap, and the compiler's `test_heap 13 3` reserves exactly **13 words** for it. A fresh
/// process's nursery holds [`EMULATOR_MIN_HEAP_WORDS`] = **233** words
/// (`Process::collector().heap_size_words()`, the emulator's `min_heap_size`), and 17
/// iterations of 13 words fill 221 of them while an 18th (234) would not fit — so a collection
/// runs every **17** iterations, and the run's count is `N / 17` with **no** additive offset
/// (the measurement starts from a freshly spawned process, whose collector counters are taken
/// before its first allocation; `Runtime::gc_stats()` is cumulative for the whole node, so the
/// per-run figure is the delta across the run).
///
/// That reproduces all six measured rows exactly at `N = 5 000 / 10 000 / 100 000` — none of
/// which is a multiple of 17, so the integer division's floor is pinned too — and it is the
/// relation a foreign root that declined collections, or a heap that stopped draining, breaks
/// at one `N` but not the other. (A CLI run of the same fixture reports the same slope with the
/// node's own start-up collections as a constant offset, which is why only the **per-run**
/// delta is a clean `N / 17`.)
const COLLECTIONS_PER_ITERATIONS: u64 = 17;
/// The nursery capacity a fresh process is created with — the emulator's `min_heap_size` in
/// words, and the geometry that turns [`COLLECTIONS_PER_ITERATIONS`] into a per-iteration rate.
const EMULATOR_MIN_HEAP_WORDS: usize = 233;
/// `N / COLLECTIONS_PER_ITERATIONS` — the derived collection count of one measured run.
fn collections_slope(n: i64) -> u64 {
    (n as u64) / COLLECTIONS_PER_ITERATIONS
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
    let rows = counter_table("hold", COUNTS);
    let [(n1, r1), (n2, r2), (n3, r3)] = rows;
    // The collector really ran, and it ran more the longer the loop ran: the pre-fix behaviour
    // (the foreign root latching collection off) reports the run's start-up count at every N.
    assert!(r1.collections > 0, "vp:hold must collect at all: {r1:?}");
    assert!(
        r1.collections < r2.collections && r2.collections < r3.collections,
        "the collection count must grow with N (N={n1} → {}, N={n2} → {}, N={n3} → {})",
        r1.collections,
        r2.collections,
        r3.collections,
    );
    // ... and it grows *linearly*, at the derived rate: exactly one collection every
    // COLLECTIONS_PER_ITERATIONS iterations, at every measured N.
    for (n, row) in rows {
        assert_eq!(
            row.collections,
            collections_slope(n),
            "collections must be N/{COLLECTIONS_PER_ITERATIONS} at N={n} \
             ({EMULATOR_MIN_HEAP_WORDS} nursery words / 13 words per iteration): {row:?}"
        );
    }
    // The heap stays bounded: a flat capacity, never a curve in N. (The pre-fix run's
    // `heap_size` climbed to 318 037 words at N = 10 000 and 2 032 432 at N = 100 000.)
    for (n, row) in rows {
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
    for (n, row) in rows {
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
/// **The control: the foreign root costs exactly nothing.**
///
/// `vp:control/1` is the same loop with an integer accumulator (`element(1, ...)`), so no
/// foreign container is ever a root. Every counter must equal `vp:hold/1`'s — same collections,
/// same splits, same heap plateaus, same returned figures. A difference of one collection is
/// the signal that the valve classified the statistics tuple as a hazard.
#[test]
fn the_foreign_held_loop_matches_its_scanned_control() {
    let held = counter_table("hold", COUNTS);
    let control = counter_table("control", COUNTS);
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
