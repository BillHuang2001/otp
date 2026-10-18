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
//! **The BIF-result copy-out, proved end to end: a loop that keeps `erlang:statistics/1`'s
//! result live still collects — because that result now lives on the process's own scanned young
//! heap.**
//!
//! `src/context/calls.rs::call_registered` moves an `Ok` **container** result off the pooled BIF
//! heap and onto the running process's own young heap on **every** process route. The move is
//! deliberately not gated on `safe_to_rewind` (that gate answers a different question — may this
//! call's pool words be *recycled* — and it is `false` for a reentrant call). So the 3-tuple
//! `erlang:statistics(garbage_collection)` returns — a `RuntimeHooks`-served BIF's result — is no
//! longer a term sitting in a region the collector never scans: it is allocated on the young heap
//! and scanned, followed and relocated like any other term.
//!
//! # That is a measurement, not a reading of the code
//!
//! `vp:hold/1` holds that tuple as its accumulator for a whole loop; `vp:control/1` is the same
//! loop reading only `element(1, …)` (a small integer). Both throw away a fresh runtime-valued
//! `junk/1` term per iteration, which is what gives the loop a heap-admission point at all. The
//! words one iteration reserves on the process heap therefore went from **13** (`junk/1` alone,
//! when the tuple stayed on the pool) to **17** — `junk/1`'s 13 plus the tuple's 4 (1 header + 3
//! small integers) — and, one nursery space being 233 words, the fill period went from 17
//! iterations (`233 / 13`) to **13** (`233 / 17`). The measured collection counts moved with it
//! (385 at N = 5 000 where the old tree reported 294), and — the direct statement that the tuple
//! costs the process heap exactly what the `junk/1` beside it costs — `vp:hold/1` and
//! `vp:control/1` now report the **identical** collection count on every measured row (601
//! consecutive rows at `N = 0 … 600` plus the wide ladder `collections_law` tabulates, up to
//! `N = 1 000 000`). [`WORDS_PER_ITERATION`] is where that 17 lives, and [`collections_law`]
//! derives the rate from it.
//!
//! # What the tests below pin
//!
//! 1. **The collector keeps running.** A loop that holds a BIF container result across every
//!    heap-admission point still collects, and collects more the longer it runs — the pre-fix
//!    behaviour (a root in an unscanned region latching collection off) reports the run's start-up
//!    count at every `N`.
//! 2. **The count is a derived quantity, not a range.** The fill law ([`collections_law`]) is an
//!    exact **floor** — every nursery fill is collected — and the run may exceed it only by
//!    [`SURPLUS_BOUND`], a single phase-shifting cycle. Both halves are measured, and the surplus
//!    bound is a constant `1`, not a rate.
//! 3. **The phase is the entry frame.** The loop's *first* collection lands on iteration 8 rather
//!    than 9 (`the_fill_law_holds_and_its_phase_is_the_entry_frame` pins both sides of every early
//!    boundary), because the spawned process arrives with its entry frame already in the space.
//! 4. **Memory stays bounded.** `heap_size` sits on one of two collector-geometry plateaus, never
//!    on a curve in `N`.
//! 5. **The held container costs no collection and is a real live object.** `vp:control/1`
//!    measures the *identical* collection count, fallback count and heap figures; the one counter
//!    that must differ is the **fullsweep** count, because `hold`'s 4-word accumulator survives
//!    every minor collection and is promoted into the mature generation, while `control` retains
//!    only an immediate.
//!
//! # The unscanned-root valve: no longer reachable from the Erlang surface — case (ii), stated
//!
//! This suite was originally built around a different premise: `vp:hold/1`'s statistics tuple was
//! a **foreign** (unscanned-region) root, and the witness was that
//! `src/context/gc.rs::root_is_unscanned` classifies it **inert** — it is a container in a region
//! the collector does not scan *and* it forwards no word a relocation would follow, so nothing
//! declines and the loop keeps collecting. The copy-out change removes that root: the tuple is a
//! scanned heap object now (see the 13 → 17 measurement above), so it can no longer witness the
//! valve's classification at all.
//!
//! **Which case applies: (ii) — the host surface can no longer produce such a root.** Not because
//! the valve changed, but because the *producers* did:
//!
//! * every `Ok` container result on a process route is re-homed onto the young heap
//!   (`call_registered`'s copy-out), so a registered-BIF result can no longer be a root in the
//!   pool. That is the measured 13 → 17 step;
//! * the foreign regions this runtime really produces — the pooled BIF heap, the pointer-stable
//!   arena (`Runtime::alloc_words`), the `-eval` heap and a `beam_message` heap fragment
//!   (`ScannedBounds::definitely_unscanned`'s set) — hold either immediates or words that point
//!   into their own region, so `forwards_scanned_word` is `false` for every one of them;
//! * the two ways a container result can still stay on the pool are inert by construction: a graph
//!   that holds a **reference** (`graph_holds_reference`, kept for the reference's identity) is an
//!   all-immediate/off-heap-leaf shape, and a **failed** move only happens when a nursery cannot be
//!   grown, which a fixture cannot provoke from Erlang code.
//!
//! A hazardous foreign root — a container in an unscanned region that forwards a pointer into the
//! asking process's **young heap** — needs a foreign object holding a live heap word, which no
//! Erlang-level expression on this runtime produces any more. Its coverage therefore lives where
//! such a container can be built by hand, one word away from the collector:
//!
//! * `src/context/gc.rs::a_foreign_leaf_is_not_unscanned_but_a_container_forwarding_a_young_word_is`
//!   — the two-fact classification over every shape the production producers write;
//! * `src/context/gc.rs::only_a_foreign_container_forwarding_a_scanned_word_opens_the_descent_latch`;
//! * `src/context/gc_walk.rs::a_foreign_container_in_every_seeded_root_family_is_detected`;
//! * `src/context/gc/valve_bounds.rs::{the_two_tier_test_matches_the_exact_predicate_in_every_region,
//!   the_filter_does_not_change_a_collections_counters}`;
//! * the copy-out's own suites (`src/context/calls/tests.rs`), which pin that a foreign container
//!   handed to `move_result_into_process_heap` really lands on the process heap.
//!
//! What this file still pins end to end is the *reason* the valve has nothing to decline here: a
//! container result held across every admission point is an ordinary scanned root, so the counts
//! follow the fill law exactly and the heap stays on its plateau (assertions 2–5). No assertion was
//! dropped in the re-derivation; the two that described the old root shape were replaced by
//! stronger ones (the exact fill law, and the identical `hold`/`control` counts).
//!
//! # Recording discipline
//!
//! Only **deterministic counters** are asserted — collections, their major/failed splits, the
//! arena-fallback count and the collector's own capacity/occupancy figures; every row of every
//! table below was reproduced by three consecutive runs of this suite. No wall time is measured or
//! asserted anywhere (this host is shared), and no figure is compared against the oracle's: the
//! allocation *shapes* differ (the oracle's own `hold` reports a different count at the same `N`),
//! and only the **shape** — collections growing, heap flat — is a property that must hold for both
//! runtimes.
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
/// The committed fixture: `hold/1` (the loop that keeps the BIF-specific result tuple live) and
/// `control/1` (the same loop with an integer accumulator).
const VP_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vp.beam");
/// The iteration counts each measured shape is driven at, in the order the docs quote them. A 40×
/// span, so a count that is *constant* in `N` (the frozen pre-fix behaviour) cannot be mistaken for
/// a count that grows; every row is a **multi-slice** run (9, 13, 18, 35, 53, 88, 175, 263 and 350
/// scheduler slices as measured), so the sample-based heap assertions below always have a live
/// process to read; and the rows whose iteration count is `8` modulo
/// [`ITERATIONS_PER_COLLECTION`] (`5 000`, `200 000`) are exactly the ones where the phase term is
/// load-bearing rather than absorbed by the floor — they are the [`SURPLUS_BOUND`] = 1 rows of
/// [`collections_law`]'s measured ladder.
const COUNTS: [i64; 9] = [
    5_000, 7_000, 10_000, 20_000, 30_000, 50_000, 100_000, 150_000, 200_000,
];
/// What a release build's `process_info(P, heap_size)` may report over a whole run: the
/// collector's two nursery-capacity plateaus, 233 words (the emulator's `min_heap_size`) and
/// 843 words (that plus a grown space), measured at every slice boundary of every run below.
/// It is a **constant** — the point of the assertion is that it does not move with `N`. (The
/// sampled peak is 233 on every multi-slice row; the 843 appears only in the process's *exit-time*
/// reading, on the rows whose last iteration lands on the grown space.)
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
/// The words `vp:junk/1` allocates per iteration — **13**, and this half is unchanged by the
/// copy-out rework.
///
/// Read off the fixture's own Erlang assembly (`erlc +deterministic`; the recipe is in
/// `tests/fixtures/README.md`): `vp:junk/1` emits `test_heap,13,3` and then exactly three
/// `put_list` and two `put_tuple2` instructions, i.e. three 2-word cons cells (6 words) + the
/// 2-tuple `{N, N+1}` (1 header + 2 = 3) + the 3-tuple `{N, [N, N+1, N+2], {N, N+1}}` (1 header +
/// 3 = 4) = **13 words**, `test_heap`'s reserve being exactly that use. Every element is a small
/// integer, hence an immediate, so nothing else of `junk/1` is heap resident.
const JUNK_WORDS: u64 = 13;
/// The words `erlang:statistics(garbage_collection)`'s result occupies **on the process heap**:
/// its `{Collections, WordsReclaimed, 0}` 3-tuple is 1 header + 3 elements = **4** words, and all
/// three elements are small integers (they become bignums only past 2^59 collections, far beyond
/// any measurement here).
///
/// That these 4 words are charged to the *process* heap — once per iteration, in both shapes — is
/// what [`WORDS_PER_ITERATION`] is derived from, and it is the measurement that says the result is
/// a scanned object rather than a pool-resident foreign root: while the tuple stayed on the pooled
/// BIF heap (before `call_registered`'s copy-out became unconditional for container results) this
/// term was **0**.
const STATS_TUPLE_WORDS: u64 = 4;
/// The words one iteration of the loop's body reserves — and uses — on the process heap: the
/// abandoned `junk/1` term plus the `erlang:statistics/1` result tuple that the loop keeps.
///
/// This is the quantity that moved when the BIF-result copy-out changed, and every figure in this
/// file follows from it: it is **17**, not the 13 a run on the pre-copy-out tree measured.
const WORDS_PER_ITERATION: u64 = JUNK_WORDS + STATS_TUPLE_WORDS;
/// The nursery capacity a fresh process is created with — the emulator's `min_heap_size` in words
/// (`Process::collector().heap_size_words()` on a just-spawned process), and the geometry the fill
/// law is derived from.
const EMULATOR_MIN_HEAP_WORDS: u64 = 233;
/// How many loop iterations one nursery space holds — **derived**, not fitted:
/// `233 / 17 = 13.7`, so 13 iterations consume 221 of the 233 words while a 14th (238) would not
/// fit. A collection therefore falls on every 13th iteration, and that the rate really is the
/// space's capacity and not something larger also proves the space is **drained** each cycle: a
/// collection that kept more than `233 - 221 = 12` words live would hold fewer iterations.
const ITERATIONS_PER_COLLECTION: u64 = EMULATOR_MIN_HEAP_WORDS / WORDS_PER_ITERATION;
/// The measured **phase offset** of the fill law, in iterations — the one constant in
/// `(N + FILL_OFFSET) / ITERATIONS_PER_COLLECTION` (both [`FILL_OFFSET`] and
/// [`ITERATIONS_PER_COLLECTION`]) that is not arithmetic. It encodes how much of the first,
/// entry-frame-loaded space is already consumed when the loop starts, i.e. how far the fills of a
/// run are shifted left against an empty-space model.
///
/// Pinned directly by `the_fill_law_holds_and_its_phase_is_the_entry_frame`: the loop's first
/// collection lands on **iteration [`FIRST_FILL_ITERATION`] = 8** (0 collections at `N = 7`, 1 at
/// `N = 8`), and thereafter one lands every 13 iterations (`22`, `35`, `48`, `61`, …). The value 4
/// is what makes the floor hold at **every** measured row — the first cycle is one iteration
/// shorter than the offset alone leaves room for, so that row carries a surplus of 1 (see
/// [`SURPLUS_BOUND`]).
const FILL_OFFSET: u64 = 4;
/// The iteration the loop's **first** collection lands on — measured, not derived: `vp:hold(7)`
/// collects `0` times and `vp:hold(8)` collects once, and this is where the entry frame's size is
/// pinned (with [`WORDS_PER_ITERATION`] = 17 and a 233-word space, `u0 + 8 × 17 > 233 ≥
/// u0 + 7 × 17` puts the frame the spawned process arrives with at 98 … 114 words).
///
/// It is one iteration earlier than the steady cycle would suggest, which is why the first row of
/// the law's measured ladder carries a surplus of 1 rather than 0.
const FIRST_FILL_ITERATION: u64 = 8;
/// The largest **pre-emption surcharge** a run over [`collections_law`] may carry: **1**, one
/// phase-shifting cycle, wherever in `N` it lands and however many slices the run took.
///
/// # The mechanism, and why the bound is a constant
///
/// The law counts nursery *fills*. A fill is `ceil((233 − L) / 17)` iterations long, where `L` is
/// the live set the previous collection left in the space (the retained accumulator tuple, the
/// loop's frame and its `junk/1` argument): `L ∈ [13, 29]` gives the steady 13, `L ≤ 12` gives 14
/// and `L ≥ 30` gives 12. `L` moves by a few words between cycles — the accumulator's elements
/// become bignums, `heap()/0`'s `process_info` result lands differently — so a cycle now and then
/// runs one iteration long or short, and the run's total ends up **one count** off the law and
/// stays there (the phase is shifted for the rest of the run). A single cycle of ±1 iteration can
/// never move the total by a second count, because that would take a whole 13-iteration fill of
/// shortening, i.e. ~13 unlucky cycles in a row — which the measured corpus never shows.
///
/// # Measured (deterministic; three identical repetitions of the whole table)
///
/// Over **601 consecutive rows** (`N = 0 … 600`, one fresh runtime each) the surplus from the law
/// is `0` in 598 rows and `1` in exactly three (`N = 8`, `580`, `593`); it is never negative — the
/// law is a floor. Over the wide ladder behind [`collections_law`]'s table (`N = 5 000 … 1 000 000`
/// / up to 1 749 slices) it is `0`, except `1` at `N = 5 000` and `N = 200 000`, the two
/// rows where `N ≡ 8 (mod 13)`. It is a **constant** bound, not a rate: this replaced the old
/// `SURPLUS_FLOOR + slices / 50` (which at 1 749 slices allowed 35 extra counts) because the
/// measured surplus no longer grows with the slice count — with 17 words per iteration the
/// reduction/yield-boundary door (`src/context/gc/reds_door.rs`) reaches its 7/8 watermark
/// (`204` words, iteration 12 of a 13-iteration fill) too late in the fill to waste a whole cycle,
/// where the old 13-word shape left ~2 iterations of slack per firing.
///
/// **Re-derive it** if [`WORDS_PER_ITERATION`], the door's 7/8 watermark or the collector's
/// growth policy changes: print the table (every test that drives [`COUNTS`] prints it) and take
/// the largest surplus over a long ladder.
const SURPLUS_BOUND: u64 = 1;
/// The smallest number of minor collections one fullsweep was measured to cost in the
/// `vp:hold/1` shape (`collections / major`, floored): the mature generation absorbs roughly
/// `32`–`37` promotions of the 4-word accumulator before it can no longer take one.
///
/// This is a **measured band**, not a derived figure: the promotion cadence depends on the
/// collector's grow-or-sweep decision, and the ratio moves inside the band with `N` (measured
/// `35.0` at `N = 5 000`, `36.6` from `N = 20 000` on, and `32.7`–`36.4` across a 201-row
/// consecutive sweep at `N = 5 000 … 5 200`). What the tests pin with it: the fullsweep count is
/// `collections / 36.6` in the large, never the `collections / 513` the integer-accumulator
/// control reads — i.e. the held BIF result really is a live, promoted heap object.
const MINORS_PER_MAJOR: u64 = 32;
/// The largest number of minor collections one fullsweep was measured to cost in the
/// `vp:hold/1` shape — see [`MINORS_PER_MAJOR`].
const MAX_MINORS_PER_MAJOR: u64 = 37;
/// The collection count one measured run of either shape must report at `N`, before the
/// pre-emption surcharge — the **fill law**, `(N + FILL_OFFSET) / ITERATIONS_PER_COLLECTION` (see
/// [`FILL_OFFSET`] and [`ITERATIONS_PER_COLLECTION`]), the exact number of nursery fills, and
/// therefore of admission collections, the loop's allocation forces.
///
/// # The derivation
///
/// * **The rate is arithmetic.** Each iteration reserves exactly [`WORDS_PER_ITERATION`] = 17 words
///   — `junk/1`'s [`JUNK_WORDS`] = 13 plus the [`STATS_TUPLE_WORDS`] = 4 words of the
///   `erlang:statistics/1` result tuple the copy-out now puts on the process heap — and the fresh
///   process's nursery is [`EMULATOR_MIN_HEAP_WORDS`] = 233 words, so one space absorbs
///   [`ITERATIONS_PER_COLLECTION`] = `233 / 17` = 13 iterations (`13 × 17 = 221 ≤ 233 < 238 =
///   14 × 17`). Every steady-state cycle is that many iterations long.
/// * **The phase is the entry frame.** The space does not start empty — the spawned process's
///   entry frame and stack are already on it — so the first cycle is short. Measured, not guessed
///   (`the_fill_law_holds_and_its_phase_is_the_entry_frame` pins both sides of every early
///   boundary): the first collection lands on **iteration 8**, which is what the
///   `+ FILL_OFFSET` term ([`FILL_OFFSET`]) encodes.
/// * **On top sits a surplus of at most [`SURPLUS_BOUND`]** — one phase-shifting cycle whose
///   mechanism and measured distribution are [`SURPLUS_BOUND`]'s.
///
/// # Measured (deterministic; every row printed by the tests that drive [`COUNTS`], three
/// identical repetitions of the whole table)
///
/// `vp:hold/1`:
///
/// | N | slices | collections | law | surplus | major |
/// |---|---|---|---|---|---|
/// | 5 000 | 9 | 385 | 384 | 1 | 11 |
/// | 7 000 | 13 | 538 | 538 | 0 | 15 |
/// | 10 000 | 18 | 769 | 769 | 0 | 21 |
/// | 20 000 | 35 | 1 538 | 1 538 | 0 | 42 |
/// | 30 000 | 53 | 2 308 | 2 308 | 0 | 63 |
/// | 50 000 | 88 | 3 846 | 3 846 | 0 | 105 |
/// | 100 000 | 175 | 7 692 | 7 692 | 0 | 210 |
/// | 150 000 | 263 | 11 538 | 11 538 | 0 | 315 |
/// | 200 000 | 350 | 15 385 | 15 384 | 1 | 420 |
///
/// `vp:control/1` reports the **same** collections at every row (385 / 538 / 769 / 1 538 / 2 308 /
/// 3 846 / 7 692 / 11 538 / 15 385) with `major` = 1 / 1 / 2 / 3 / 5 / 8 / 15 / 22 / 30 — the only
/// counter that differs between the two shapes.
///
/// Three more figures from the same measurement, so the bound can be read at its extremes: `N =
/// 300 000` (525 slices) reports `23 077` collections against a law of `23 077` and `630`
/// fullsweeps, `N = 500 000` reports `38 461` against `38 461`, and `N = 1 000 000` (1 749 slices)
/// reports `76 923` against `76 923` with `2 098` fullsweeps.
fn collections_law(n: i64) -> u64 {
    (n as u64 + FILL_OFFSET) / ITERATIONS_PER_COLLECTION
}

/// **The headline evidence: a loop holding a BIF container result still collects, more the longer
/// it runs, on a bounded heap — at exactly the derived rate.**
///
/// `vp:hold/1` keeps `erlang:statistics(garbage_collection)`'s 3-tuple as its accumulator across
/// the whole loop, so it is a live root at every one of the loop's heap-admission points. Since
/// the BIF-result copy-out, that tuple is an ordinary object on the process's own young heap
/// ([`WORDS_PER_ITERATION`] is the measurement), so it can neither pin the collector nor move the
/// count away from the fill law.
#[test]
fn a_loop_holding_the_statistics_result_keeps_collecting() {
    let rows = counter_table("hold", &COUNTS);
    // The collector really ran, and it ran more the longer the loop ran: the pre-fix behaviour
    // (a foreign root latching collection off) reports the run's start-up count at every N.
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
    // ... and it grows at the derived rate, with the derived phase: each iteration allocates
    // WORDS_PER_ITERATION (13 of `junk/1` + the 4 the BIF's result tuple occupies on this heap),
    // the nursery holds ITERATIONS_PER_COLLECTION (233 / 17) of them, and the first cycle is
    // FILL_OFFSET iterations short because the spawned process's entry frame is already in the
    // space (see `collections_law` for the measured ladder).
    for (n, row) in &rows {
        let law = collections_law(*n);
        // The fill law is an exact **floor**: every nursery fill must be collected, so a run whose
        // root set pinned the collector breaks it outright.
        assert!(
            row.collections >= law,
            "collections must be at least (N + {FILL_OFFSET}) / {ITERATIONS_PER_COLLECTION} at \
             N={n} ({EMULATOR_MIN_HEAP_WORDS} nursery words / {WORDS_PER_ITERATION} words per \
             iteration): {row:?}"
        );
        // ... and the run may exceed it only by the phase shift `SURPLUS_BOUND` derives (measured
        // `1` at N = 5 000 and 200 000, `0` on every other row here).
        let surplus = row.collections - law;
        assert!(
            surplus <= SURPLUS_BOUND,
            "the surplus over the fill law must stay within {SURPLUS_BOUND} at N={n} (measured \
             {surplus} over {law} in {} slices): {row:?}",
            row.slices
        );
    }
    // The heap stays bounded: a flat capacity, never a curve in N. (The pre-fix run's `heap_size`
    // climbed to 318 037 words at N = 10 000 and 2 032 432 at N = 100 000.)
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
    // The run really was the loop (it terminated and returned the loop's value), every allocation
    // stayed on the process's own young heap (no arena fallback, i.e. nothing declined), and the
    // container the loop holds is a live object in the mature generation's promotion stream: a
    // run that kept it off the heap would sweep as rarely as the integer-accumulator control.
    for (n, row) in &rows {
        assert_eq!(
            row.fallbacks, 0,
            "a copied-out BIF result must not decline a collection at N={n}: {row:?}"
        );
        assert!(
            row.returned_collections > 0,
            "the loop's own statistics reading must see collections at N={n}: {row:?}"
        );
        assert!(
            row.returned_collections as u64 <= row.collections,
            "the loop's reading is a snapshot of this run's count at N={n}: {row:?}"
        );
        assert!(
            row.major * MAX_MINORS_PER_MAJOR >= row.collections,
            "at most {MAX_MINORS_PER_MAJOR} minor collections may pass between two fullsweeps at \
             N={n} (the held 4-word accumulator is promoted on every one): {row:?}"
        );
        assert!(
            row.major * MINORS_PER_MAJOR <= row.collections,
            "the mature generation must absorb at least {MINORS_PER_MAJOR} of those promotions \
             before a fullsweep at N={n}: {row:?}"
        );
    }
}

/// **The fill law's phase, pinned directly: the first collection lands on iteration 8, then one
/// every 13.**
///
/// The rate of [`collections_law`] is arithmetic (`233 / 17 = 13` iterations per nursery space);
/// its `+ FILL_OFFSET` phase ([`FILL_OFFSET`]) is the one measured constant, and this test measures
/// it where it is unambiguous, in the one-slice regime where no boundary collection can be
/// involved. The
/// ladder below is the measured transition table of the count — the iteration on which the run's
/// *n*-th collection first appears, on both sides of every early boundary: `0` up to `N = 7`, `1`
/// from `N = 8`, `2` from `22`, `3` from `35`, then every 13 iterations (`48`, `61`, `74`, `87`,
/// `100`, `113`, `126`, `139`).
///
/// The first cycle is short because the process arrives with its entry frame already in the
/// space: with 17 words per iteration and a 233-word space, `u0 + 8 × 17 > 233 ≥ u0 + 7 × 17`
/// puts that frame at 98 … 114 words. The second boundary is one iteration *late* (14 instead of
/// 13) — one of the phase-shifting cycles [`SURPLUS_BOUND`] bounds.
#[test]
fn the_fill_law_holds_and_its_phase_is_the_entry_frame() {
    // The rate really is the space's capacity divided by the iteration's use, not a fitted rate.
    const _: () = assert!(ITERATIONS_PER_COLLECTION * WORDS_PER_ITERATION <= EMULATOR_MIN_HEAP_WORDS);
    const _: () = assert!(
        (ITERATIONS_PER_COLLECTION + 1) * WORDS_PER_ITERATION > EMULATOR_MIN_HEAP_WORDS
    );
    // And the iteration's use really is the abandoned `junk/1` term plus the BIF's result tuple —
    // the two halves the copy-out rework separated.
    const _: () = assert!(JUNK_WORDS + STATS_TUPLE_WORDS == WORDS_PER_ITERATION);
    // (N, the collections the run must report) — both sides of every early transition point.
    let ladder: [(i64, u64); 11] = [
        (0, 0),
        (7, 0),
        (8, 1),
        (21, 1),
        (22, 2),
        (34, 2),
        (35, 3),
        (47, 3),
        (48, 4),
        (60, 4),
        (61, 5),
    ];
    let rows = counter_table("hold", &ladder.map(|(n, _)| n));
    // The ladder's own first boundary *is* `FIRST_FILL_ITERATION`, so the constant cannot drift
    // from the table below.
    assert_eq!(
        ladder[2],
        (FIRST_FILL_ITERATION as i64, 1),
        "the first collection lands on iteration {FIRST_FILL_ITERATION}"
    );
    for ((n, expected), (_, row)) in ladder.into_iter().zip(&rows) {
        assert_eq!(
            row.collections,
            expected,
            "vp:hold({n}) must collect exactly {expected} time(s): the first space fills on \
             iteration {FIRST_FILL_ITERATION}, then every {ITERATIONS_PER_COLLECTION} iterations: \
             {row:?}",
        );
        // The law holds underneath it everywhere, with at most the one-cycle surplus.
        let law = collections_law(n);
        assert!(
            row.collections >= law && row.collections - law <= SURPLUS_BOUND,
            "the fill law must bracket the measured count at N={n} (law {law}, bound \
             {SURPLUS_BOUND}): {row:?}"
        );
    }
}

/// **The control: the held BIF result costs no collection, and shows up only as promotion.**
///
/// `vp:control/1` is the same loop with an integer accumulator (`element(1, …)`) — it calls the
/// very same BIF, so it pays the very same copy-out, and the two shapes must therefore collect
/// **identically**: same collections, same arena-fallback count, same heap plateaus, same figures
/// read back by the loop. A difference of one collection is the signal that holding the container
/// changed the collector's behaviour.
///
/// `major` is the one counter that **must** differ: `hold`'s 4-word accumulator survives every
/// minor collection and is promoted, so its mature generation fills after ~37 minors, while
/// `control` retains an immediate and sweeps after ~513. That is the positive evidence that the
/// statistics tuple is a live heap object — and the band the assertions below pin is
/// [`MINORS_PER_MAJOR`] … [`MAX_MINORS_PER_MAJOR`].
#[test]
fn the_held_loop_collects_exactly_as_its_control_does_and_only_promotes_more() {
    let held = counter_table("hold", &COUNTS);
    let control = counter_table("control", &COUNTS);
    for ((n, held), (_, control)) in held.into_iter().zip(control) {
        assert_eq!(
            (held.collections, held.fallbacks, held.errors),
            (control.collections, control.fallbacks, control.errors),
            "vp:hold and vp:control must collect identically at N={n} (the held BIF result is an \
             ordinary scanned heap object): hold={held:?} control={control:?}"
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
        assert!(
            held.major > control.major,
            "holding the statistics tuple must cost fullsweeps (it is promoted on every minor \
             collection, the control's integer accumulator is not) at N={n}: hold={held:?} \
             control={control:?}"
        );
    }
}
