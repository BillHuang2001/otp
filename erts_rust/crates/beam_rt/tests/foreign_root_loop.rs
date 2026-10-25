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
//! iterations (`233 / 13`) to **13** (`233 / 17`). The measured collection counts followed
//! (384 at `N = 5 000`), and — the direct statement that the tuple
//! costs the process heap exactly what the `junk/1` beside it costs — `vp:hold/1` and
//! `vp:control/1` report the **identical** collection count on every measured row (the wide ladder
//! `collections_law` tabulates, `N = 5 000 … 1 000 000`, and the 601-row low-law sweep
//! `N = 0 … 600`, both driven by the tests below). [`WORDS_PER_ITERATION`] is where that 17 lives,
//! and [`collections_law`] derives the rate from it.
//!
//! # What the tests below pin
//!
//! 1. **The collector keeps running.** A loop that holds a BIF container result across every
//!    heap-admission point still collects, and collects more the longer it runs — the pre-fix
//!    behaviour (a root in an unscanned region latching collection off) reports the run's start-up
//!    count at every `N`.
//! 2. **The count is a derived quantity, not a range.** The fill law ([`collections_law`]) is an
//!    exact **floor** — every nursery fill is collected — and the run may exceed it only by
//!    [`surplus_bound`]`(law)`: one count for the entry frame's phase plus one per
//!    [`COLLECTIONS_PER_SURPLUS_COUNT`] fills for the pre-emption's phase shifts. Both halves are
//!    measured, and the surplus is a bounded **rate in the law**, not a constant.
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
//! follow the fill law exactly and the heap stays on its plateau (assertions 2–5).
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
/// The iteration counts each measured shape is driven at, in the order the docs quote them. A 200×
/// span, so a count that is *constant* in `N` (the frozen pre-fix behaviour) cannot be mistaken for
/// a count that grows, and long enough that [`surplus_bound`]'s rate term is exercised well past
/// its own denominator (the last four rows carry a non-zero surplus). Every row is a
/// **multi-slice** run (7, 9, 13, 26, 38, 63, 126, 188, 251, 376, 626 and 1 251 scheduler slices
/// as measured), so the sample-based heap assertions below always have a live process to read.
const COUNTS: [i64; 12] = [
    5_000, 7_000, 10_000, 20_000, 30_000, 50_000, 100_000, 150_000, 200_000, 300_000, 500_000,
    1_000_000,
];
/// The last iteration count of the **low-law regime** sweep, `N = 0 … LOW_LAW_SWEEP_LAST`, one
/// fresh runtime per row: the region where [`surplus_bound`]'s rate term is `0`, so its base term is
/// the only allowance and [`SURPLUS_BASE`] is measured directly. 600 iterations is 46 nursery fills
/// at every phase alignment the fill law has (each `N mod ITERATIONS_PER_COLLECTION` occurs 46
/// times), and it is one of the lengths within which a single-slice run stays single-slice (the
/// first slice covers ~1 400 iterations), so no boundary collection can be involved.
const LOW_LAW_SWEEP_LAST: i64 = 600;
/// What a release build's `process_info(P, heap_size)` — the summed capacity of the process's
/// nursery chain — may report over a whole run: two plateaus, both arithmetic. **233** words is
/// the emulator's `min_heap_size`, the single space a fresh process is created with; **609** is
/// that space plus the 376-word one a `grow_nursery` adds (`HeapPolicy::young_minor_target_words
/// (233) = 376`). The bound is 843, the growth table's step above 610, i.e. one step of headroom
/// over the largest reading this shape produces.
/// It is a **constant** — the point of the assertion is that it does not move with `N`. (Measured:
/// the peak sampled at a slice boundary is 233 on every multi-slice row; 609 shows up only in the
/// loop's own last reading, on the rows whose final iteration lands in the grown-chain phase.)
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
    /// The largest **mature-generation capacity** (words) observed at a slice boundary — the
    /// region [`MAX_MINORS_PER_MAJOR`] divides, so the per-fullsweep room rule is read from the
    /// run's own geometry rather than assumed. `0` on a single-slice run, where no boundary is
    /// sampled before the process terminates.
    peak_mature: usize,
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
    let mut peak_mature = 0usize;
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
            peak_mature = peak_mature.max(process.collector().mature_capacity_words());
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
        peak_mature,
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
/// Runs one shape at each iteration count in `counts` — a **fresh** runtime each time, so every row
/// is a per-run delta — asserting the two invariants that hold on every row whatever its `N`: no
/// collection may fail, and the global GC valve must stay closed. Returns the rows unprinted, so a
/// caller that does not want a 600-line table ([`the_fill_law_holds_and_its_phase_is_the_entry_frame`]'s
/// low-law sweep) shares the measurement rather than growing its own.
fn measure_rows(function: &str, counts: &[i64]) -> Vec<(i64, LoopRun)> {
    counts
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
        .collect()
}
/// [`measure_rows`] plus the printed counter table; every counter is reported — including the
/// fill law's prediction and the measured surplus over it — so a reader needs no access to this
/// source to read the numbers.
fn counter_table(function: &str, counts: &[i64]) -> Vec<(i64, LoopRun)> {
    let rows = measure_rows(function, counts);
    eprintln!(
        "counter_table vp:{function} (N : collections major fallbacks errors slices \
         peak_heap peak_used peak_mature returned | law surplus)"
    );
    for (n, row) in &rows {
        eprintln!(
            "counter_table vp:{function} N={n} : collections={} major={} fallbacks={} \
             errors={} slices={} peak_heap={} peak_used={} peak_mature={} returned={:?} | law={} \
             surplus={}",
            row.collections,
            row.major,
            row.fallbacks,
            row.errors,
            row.slices,
            row.peak_heap_size,
            row.peak_used,
            row.peak_mature,
            (row.returned_collections, row.returned_heap_size),
            collections_law(*n),
            row.collections as i64 - collections_law(*n) as i64,
        );
    }
    rows
}
/// Asserts [`collections_law`]'s two-sided bracket on one measured row: the law is an exact
/// **floor** (every nursery fill is collected, so a run whose root set pinned the collector breaks
/// it outright), and the run may exceed it only by [`surplus_bound`].
fn assert_law_bracket(n: i64, row: &LoopRun) {
    let law = collections_law(n);
    assert!(
        row.collections >= law,
        "collections must be at least (N + {FILL_OFFSET}) / {ITERATIONS_PER_COLLECTION} at N={n} \
         ({EMULATOR_MIN_HEAP_WORDS} nursery words / {WORDS_PER_ITERATION} words per iteration): \
         {row:?}"
    );
    let surplus = row.collections - law;
    let bound = surplus_bound(law);
    assert!(
        surplus <= bound,
        "the surplus over the fill law must stay within {bound} (= {SURPLUS_BASE} + {law} / \
         {COLLECTIONS_PER_SURPLUS_COUNT}) at N={n} (measured {surplus} over {law} in {} slices): \
         {row:?}",
        row.slices
    );
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
/// [`SURPLUS_BASE`]).
const FILL_OFFSET: u64 = 4;
/// The iteration the loop's **first** collection lands on — measured, not derived: `vp:hold(7)`
/// collects `0` times and `vp:hold(8)` collects once, and this is where the entry frame's size is
/// pinned (with [`WORDS_PER_ITERATION`] = 17 and a 233-word space, `u0 + 8 × 17 > 233 ≥
/// u0 + 7 × 17` puts the frame the spawned process arrives with at 98 … 114 words).
///
/// It is one iteration earlier than the steady cycle would suggest, which is why the first row of
/// the law's measured ladder carries a surplus of 1 rather than 0.
const FIRST_FILL_ITERATION: u64 = 8;
/// The largest **pre-emption surcharge** a run over [`collections_law`] may carry, as a function of
/// the law it is measured against: [`SURPLUS_BASE`] plus one count per
/// [`COLLECTIONS_PER_SURPLUS_COUNT`] nursery fills, i.e. `1 + law / 5 000`.
///
/// # The mechanism: a surplus count is a phase shift, and a phase shift is a rate
///
/// The law counts nursery *fills*. A fill is `ceil((233 − L) / 17)` iterations long, where `L` is
/// the live set the previous collection left in the space (the retained accumulator tuple, the
/// loop's frame and its `junk/1` argument): `L ∈ [13, 29]` gives the steady 13, `L ≤ 12` gives 14
/// and `L ≥ 30` gives 12. `L` moves by a few words between cycles — the accumulator's elements
/// become bignums, `heap()/0`'s `process_info` result lands differently — so a cycle now and then
/// runs one iteration long or short, and the run's total ends up **one count** off the law and
/// stays there (the phase is shifted for the rest of the run).
///
/// What makes that happen at all is the pre-emption: the reduction/yield-boundary door
/// (`src/context/gc/reds_door.rs`) collects at a slice boundary when the allocation area is past
/// its occupancy watermark, and such a collection empties a space that has not *quite* filled — one
/// count the law never charged. Every slice boundary is one such opportunity, so the number of
/// phase shifts is proportional to the **slice count**, and the slice count is proportional to `N`.
/// The surplus is therefore a *rate in the law*, not a constant: `1` catches the entry-frame phase
/// offset (the one count [`FILL_OFFSET`] leaves room for), and the rate term catches the boundary
/// shifts. It is also a **linear** rate — the property the assertion exists to hold: an
/// accumulating root would make the gap superlinear, and 9 counts at `N = 1 000 000` is nowhere
/// near that.
///
/// # Measured (deterministic; the whole ladder reproduced identically by consecutive runs)
///
/// The wide ladder behind [`collections_law`]'s table, as `surplus / law`:
///
/// | N | slices | surplus | law | surplus per collection |
/// |---|---|---|---|---|
/// | 5 000 … 50 000 | 7 … 63 | 0 | 384 … 3 846 | — |
/// | 100 000 | 126 | 1 | 7 692 | 1 in 7 692 |
/// | 150 000 | 188 | 2 | 11 538 | 1 in 5 769 |
/// | 200 000 | 251 | 2 | 15 384 | 1 in 7 692 |
/// | 300 000 | 376 | 2 | 23 077 | 1 in 11 538 |
/// | 500 000 | 626 | 5 | 38 461 | 1 in 7 692 |
/// | 1 000 000 | 1 251 | 9 | 76 923 | 1 in 8 547 |
///
/// The worst row is `N = 150 000` at 1 in 5 769 — 0.017 % of the law — and the long rows settle at
/// 1 in 7 700 … 8 500, so [`COLLECTIONS_PER_SURPLUS_COUNT`] = 5 000 is ~1.15× the worst rate and
/// still ~2× the asymptotic one: a bound that a superlinear surplus leaves in a few hundred
/// thousand iterations, and that the measured ladder sits inside with margin ≥ 1 count on every
/// row. In slice terms the same figure reads ~1 shift per 80 slices (the loops run ~61 collections
/// per slice: 1 251 slices / 76 932 collections at `N = 1 000 000`), against a measured 1 per
/// 94 … 190.
///
/// Over the **low-law regime** (`N = 0 … 600`, i.e. every phase alignment of the first 46 fills,
/// where the rate term is `0` and only [`SURPLUS_BASE`] applies) the surplus is `0` in 600 of the
/// 601 rows and `1` in exactly one — `N = 8`, the entry-frame row. That sweep is run by
/// `the_fill_law_holds_and_its_phase_is_the_entry_frame`, so the base term is measured, not
/// asserted from memory.
///
/// **Re-derive it** if [`WORDS_PER_ITERATION`], the door's occupancy watermark or the collector's
/// growth policy changes: print the table (every test that drives [`COUNTS`] prints it) and take
/// the largest `surplus / law` over a long ladder.
fn surplus_bound(law: u64) -> u64 {
    SURPLUS_BASE + law / COLLECTIONS_PER_SURPLUS_COUNT
}
/// How many nursery fills one surplus count over [`collections_law`] may be charged for — the
/// denominator of [`surplus_bound`]'s rate term, derived from the measured ladder in its own docs:
/// 5 000 sits ~1.15× above the worst measured rate (1 surplus count per 5 769 collections, at
/// `N = 150 000`) and ~2× above the asymptotic one (1 per ~8 500).
const COLLECTIONS_PER_SURPLUS_COUNT: u64 = 5_000;
/// The fixed part of [`surplus_bound`]: the one count the **entry frame**'s phase offset buys.
///
/// [`FILL_OFFSET`] is the constant that makes the law a floor at every measured row, and it is one
/// count *short* of the measured phase: the first collection lands on iteration
/// [`FIRST_FILL_ITERATION`] = 8 while the law charges nothing until `N = 12`, so that row carries a
/// surplus of 1 — the only surplus the low-law regime ever shows (600 of its 601 rows carry 0).
const SURPLUS_BASE: u64 = 1;
/// The mature space a `vp:hold/1` fullsweep installs, in words — the budget every minor
/// collection between two fullsweeps draws on.
///
/// # The derivation (from `beam_gc`'s policy, not from a fit)
///
/// `HeapPolicy::mature_target_words(live, young_occupancy)` = `next_words(mature_target_live_
/// multiple × (live + young_occupancy + need_reserve_words))` with the multiple 2, the reserve 64
/// and this shape's own fullsweep measurement `live = 4` (the held accumulator tuple, all four
/// elements immediates) and `young_occupancy = 221` (`ITERATIONS_PER_COLLECTION ×
/// WORDS_PER_ITERATION`, the nursery's fill) ⇒ `next_words(2 × 289 = 578)` = **610** words, the
/// growth table's step above 376. Every later fullsweep re-derives the same 610 from the same live
/// set, so it is a fixed point rather than a ratchet.
///
/// # Measured
///
/// [`LoopRun::peak_mature`] samples `AnyCollector::mature_capacity_words()` at every slice boundary
/// of every run: on **every** multi-slice `vp:hold/1` row it reads 610 at every boundary — the space
/// neither grows past its target nor shrinks back — and it is the figure that run's `major` counter
/// is divided by. `vp:control/1` reads 233 (the emulator `min_heap_size` floor, the region a fresh
/// process is created with) on the rows where none of its one-to-three fullsweeps has been sampled
/// yet, and 610 on the rows where one has — the target such a fullsweep re-derives from the live
/// set that survived it. `a_loop_holding_the_statistics_result_keeps_collecting` and
/// `the_held_loop_collects_exactly_as_its_control_does_and_only_promotes_more` assert both figures,
/// so this constant is a measurement of the run, not a quotation.
const MATURE_CAPACITY_WORDS: u64 = 610;
/// The smallest number of minor collections one fullsweep must be able to cost in the
/// `vp:hold/1` shape — the **pessimistic** end of the mature generation's own room rule.
///
/// # The derivation (from `beam_gc`'s policy, not from a fit)
///
/// A minor is admitted only while the mature space has room for the nursery's **live promotion** —
/// the exact words the minor will copy — and each minor then consumes exactly those words
/// (`collector.rs::minor_room_words`, the one input of both the kind choice and the minor's own
/// pre-copy check). A fresh [`MATURE_CAPACITY_WORDS`]-word region therefore admits
/// `MATURE_CAPACITY_WORDS / L` minors, where `L` is the live young word count at the admission
/// point; the nursery *fill* plays no part beyond the cheap `available >= fill` short-circuit,
/// because it also counts the words the mutator has already abandoned.
///
/// * `L` = [`WORDS_PER_ITERATION`] = **17** is the pessimistic end — every minor copying the
///   abandoned `junk/1` as well as the held tuple — so one fullsweep still buys
///   `610 / 17` = **35** minors.
/// * `L` = [`STATS_TUPLE_WORDS`] = **4** is the steady-state end (the tuple alone; `junk/1` is
///   already dead at the admission point) — see [`MAX_MINORS_PER_MAJOR`].
///
/// # Measured
///
/// A **single** interval between two fullsweeps is [`FIRST_SWEEP_MINORS`] = **56** minors for the
/// run's first one (the region it draws down is the 233-word floor, not the 610-word target) and
/// then 151 … 152, the optimistic end of the `610 / L` rule with `L` moving by a word between
/// cycles — the whole-run average at `N = 1 000 000` is 76 932 / 509 = 151.1. What the
/// assertion pins is the order of magnitude: the fullsweep count is **hundreds** of minors, never
/// the 15 386 minors the integer-accumulator control's single fullsweep sits behind at
/// `N = 200 000` — i.e. the held BIF result really is a live, promoted heap object.
const MINORS_PER_MAJOR: u64 = MATURE_CAPACITY_WORDS / WORDS_PER_ITERATION;
/// The largest number of minor collections one fullsweep may cost in the `vp:hold/1` shape — the
/// **optimistic** end of the same room rule (`MATURE_CAPACITY_WORDS / STATS_TUPLE_WORDS` = `610 /
/// 4` = 152: a minor that promotes only the held 4-word accumulator).
///
/// See [`MINORS_PER_MAJOR`] for the derivation and the 122 … 152 interval the rule permits
/// (`610 / 5` … `610 / 4`: `L` moves by a word between cycles). The first fullsweep comes sooner
/// than both ends because the victim's *first* mature region is the 233-word floor rather than the
/// 610-word target — [`FIRST_SWEEP_MINORS`] = 56 minors, which
/// `a_loop_holding_the_statistics_result_keeps_collecting` measures from either side of the
/// boundary — which only ever lowers a run's ratio.
///
/// This bounds **one interval**, not a whole-run average: a run's `collections` also contain the
/// interval that is still open at its last iteration — the minors after the final fullsweep, which
/// no fullsweep has closed — so the average `collections / major` can sit *above* 152 (measured 154
/// at `N = 10 000 … 30 000`, where that tail is a large fraction of a short run) while every closed
/// interval stays inside the band. `a_loop_holding_the_statistics_result_keeps_collecting` charges
/// that one open interval explicitly.
const MAX_MINORS_PER_MAJOR: u64 = MATURE_CAPACITY_WORDS / STATS_TUPLE_WORDS;
/// How many minor collections the victim's **first** mature region absorbs before its first
/// fullsweep — measured at **56**, two short of the `floor(233 / 4)` = 58 an accumulator-only
/// promotion stream would allow.
///
/// # Why it is shorter than that ideal
///
/// A fullsweep is what installs the [`MATURE_CAPACITY_WORDS`]-word target; before it the region is
/// the [`EMULATOR_MIN_HEAP_WORDS`]-word `min_heap_size` floor a fresh process is created with, and
/// each minor draws it down by the words it promotes. In the steady state that is
/// [`STATS_TUPLE_WORDS`] = 4 words (the held accumulator alone: `610 / 152` = 4.01 words per
/// minor), but the run's *first* minor also promotes the live part of the entry-frame-loaded space,
/// so the floor is drawn down faster at the start: `233 / 56` = 4.16 words per minor. Both figures
/// are measurements of this fixture.
///
/// # Measured
///
/// `a_loop_holding_the_statistics_result_keeps_collecting` brackets the boundary from either side
/// with [`n_for_law`]: at [`FIRST_SWEEP_MINORS`] − 1 = 55 collections the run has swept **0**
/// times, and by 56 collections it has swept once.
const FIRST_SWEEP_MINORS: u64 = 56;
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
/// * **On top sits a surplus of at most [`surplus_bound`]`(law)`** — the entry-frame count plus the
///   pre-emption rate, whose mechanism and measured ladder are [`surplus_bound`]'s.
///
/// # Measured (deterministic; every row printed by the tests that drive [`COUNTS`])
///
/// `vp:hold/1`:
///
/// | N | slices | collections | law | surplus | major | peak mature |
/// |---|---|---|---|---|---|---|
/// | 5 000 | 7 | 384 | 384 | 0 | 3 | 610 |
/// | 7 000 | 9 | 538 | 538 | 0 | 4 | 610 |
/// | 10 000 | 13 | 769 | 769 | 0 | 5 | 610 |
/// | 20 000 | 26 | 1 538 | 1 538 | 0 | 10 | 610 |
/// | 30 000 | 38 | 2 308 | 2 308 | 0 | 15 | 610 |
/// | 50 000 | 63 | 3 846 | 3 846 | 0 | 26 | 610 |
/// | 100 000 | 126 | 7 693 | 7 692 | 1 | 51 | 610 |
/// | 150 000 | 188 | 11 540 | 11 538 | 2 | 76 | 610 |
/// | 200 000 | 251 | 15 386 | 15 384 | 2 | 102 | 610 |
/// | 300 000 | 376 | 23 079 | 23 077 | 2 | 153 | 610 |
/// | 500 000 | 626 | 38 466 | 38 461 | 5 | 254 | 610 |
/// | 1 000 000 | 1 251 | 76 932 | 76 923 | 9 | 509 | 610 |
///
/// `vp:control/1` reports the **same** collections and the same heap plateaus at every row, with
/// `major` = 0 / 0 / 0 / 0 / 0 / 0 / 0 / 1 / 1 / 1 / 1 / 3 and a peak mature capacity of 233 on
/// its first eight rows and 610 on the last four — 233 is the floor it is created with, 610 the
/// target one of its rare fullsweeps re-derives once one has been sampled. The matures, and the
/// fullsweeps, are therefore the only counters that differ between the two shapes.
///
/// The surplus column is the ladder [`surplus_bound`] is fitted to: `0` up to `N = 50 000`, then a
/// bounded rate (1 in 7 692 … 1 in 11 538 collections), never a constant and never superlinear.
/// The `major` column is `collections / 128 … 154` (a whole-run average, which includes the one
/// interval that is still open at the last iteration — see [`MAX_MINORS_PER_MAJOR`]).
/// The iteration count at which [`collections_law`] first returns `law` — the law inverted, for
/// assertions whose subject is naturally a collection count rather than an iteration count.
fn n_for_law(law: u64) -> i64 {
    (law * ITERATIONS_PER_COLLECTION - FILL_OFFSET) as i64
}

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
        // The fill law is an exact floor and the surplus over it is a bounded rate.
        assert_law_bracket(*n, row);
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
        // The mature generation absorbs at most MAX_MINORS_PER_MAJOR minors between two
        // fullsweeps — and a whole-run count is those closed intervals plus the one interval still
        // open at the last iteration, which no fullsweep has closed yet (so it is shorter than a
        // full one and the average `collections / major` may sit just above the per-interval band).
        assert!(
            row.collections <= row.major * MAX_MINORS_PER_MAJOR + (MAX_MINORS_PER_MAJOR - 1),
            "at most {MAX_MINORS_PER_MAJOR} minor collections may pass between two fullsweeps at \
             N={n}, plus the one interval still open at the run's last iteration (the held 4-word \
             accumulator is promoted on every one): {row:?}"
        );
        assert!(
            row.major * MINORS_PER_MAJOR <= row.collections,
            "the mature generation must absorb at least {MINORS_PER_MAJOR} of those promotions \
             before a fullsweep at N={n}: {row:?}"
        );
        // ... and the mature region the band divides is the one the policy derives: every fullsweep
        // re-derives MATURE_CAPACITY_WORDS from this shape's live set, a fixed point rather than a
        // ratchet, so the sampled capacity sits on that figure at every boundary.
        assert_eq!(
            row.peak_mature, MATURE_CAPACITY_WORDS as usize,
            "the mature generation's capacity is the one the policy derives at N={n} \
             ({MATURE_CAPACITY_WORDS} words): {row:?}"
        );
    }
    // The first fullsweep's position, measured from both sides: the region it replaces is the
    // EMULATOR_MIN_HEAP_WORDS floor rather than the steady target, and each minor draws it down by
    // the live young words it promotes (FIRST_SWEEP_MINORS).
    let first_sweep = measure_rows(
        "hold",
        &[
            n_for_law(FIRST_SWEEP_MINORS - 1),
            n_for_law(FIRST_SWEEP_MINORS),
        ],
    );
    let (n_before, before) = first_sweep[0];
    let (n_at, at) = first_sweep[1];
    assert_eq!(
        before.collections,
        FIRST_SWEEP_MINORS - 1,
        "the bracket is measured in collection counts: vp:hold({n_before}): {before:?}"
    );
    assert_eq!(
        before.major, 0,
        "no fullsweep may have run by {FIRST_SWEEP_MINORS} - 1 collections \
         (vp:hold({n_before})): {before:?}"
    );
    assert_eq!(
        at.collections, FIRST_SWEEP_MINORS,
        "the bracket is measured in collection counts: vp:hold({n_at}): {at:?}"
    );
    assert_eq!(
        at.major, 1,
        "the first fullsweep lands on collection {FIRST_SWEEP_MINORS}, which is what the \
         {EMULATOR_MIN_HEAP_WORDS}-word floor's room rule allows (vp:hold({n_at})): {at:?}"
    );
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
/// 13) — one of the phase-shifting cycles [`surplus_bound`] bounds.
///
/// The test then walks the whole **low-law regime** (`N = 0 … 600`, every phase alignment of the
/// first 46 fills) to pin [`SURPLUS_BASE`] where it is the only term of [`surplus_bound`]: the
/// surplus is `0` in 600 of those 601 rows and `1` in exactly one — this ladder's entry-frame row,
/// `N = 8` — so the base term is one count of measured headroom, not a fitted slack.
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
        // The law holds underneath it everywhere, with at most [`surplus_bound`]'s allowance.
        assert_law_bracket(n, row);
    }
    // ... and the low-law regime's whole surplus ledger is the base term: over `N = 0 … 600`
    // (every phase alignment of the first 46 fills, so the rate term of `surplus_bound` is 0 in
    // every one of them) the surplus is 0 in all rows but the entry-frame one, which carries
    // exactly `SURPLUS_BASE`.
    let sweep = measure_rows("hold", &(0..=LOW_LAW_SWEEP_LAST).collect::<Vec<i64>>());
    assert_eq!(sweep.len(), LOW_LAW_SWEEP_LAST as usize + 1);
    let mut surplus_histogram = [0usize; 4];
    for (n, row) in &sweep {
        assert_law_bracket(*n, row);
        let surplus = (row.collections - collections_law(*n)) as usize;
        assert!(
            surplus < surplus_histogram.len(),
            "a low-law row may not carry {surplus} surplus counts at N={n}: {row:?}"
        );
        surplus_histogram[surplus] += 1;
    }
    eprintln!(
        "low_law_sweep vp:hold N=0..={LOW_LAW_SWEEP_LAST} : surplus histogram \
         {surplus_histogram:?} (index = surplus count, value = rows; bound {})",
        surplus_bound(collections_law(LOW_LAW_SWEEP_LAST))
    );
    assert_eq!(
        surplus_histogram,
        [
            LOW_LAW_SWEEP_LAST as usize,
            SURPLUS_BASE as usize,
            0,
            0
        ],
        "in the low-law regime the surplus is SURPLUS_BASE ({SURPLUS_BASE}) in exactly the \
         entry-frame row and 0 everywhere else"
    );
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
/// minor collection and is promoted, so its mature generation fills after 151 … 152 minors (the
/// measured steady interval — see [`MINORS_PER_MAJOR`] and [`MAX_MINORS_PER_MAJOR`]), while
/// `control` retains an immediate, promotes nothing and sweeps 0 … 3 times over the whole ladder
/// (once in 15 386 collections at `N = 200 000`, once in 25 644 at `N = 1 000 000`). That is the
/// positive evidence that the statistics tuple is a live heap object — and the band the assertions
/// below pin is [`MINORS_PER_MAJOR`] … [`MAX_MINORS_PER_MAJOR`].
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
        // The mature geometry is where that promotion lands. `hold`'s fullsweeps install the
        // 610-word target this shape derives at every one. `control` starts on the 233-word floor a
        // fresh process is created with, and its rare fullsweep re-derives the *same* 610 from the
        // live set that survived it (the loop's frame and stack — an immediate accumulator adds
        // nothing to a promotion), so a sampled boundary sees one figure or the other and never a
        // third: which of the two depends only on whether a fullsweep has been sampled yet.
        assert_eq!(
            held.peak_mature, MATURE_CAPACITY_WORDS as usize,
            "hold's mature generation is the {MATURE_CAPACITY_WORDS}-word target its own live set \
             derives at N={n}: hold={held:?}"
        );
        assert!(
            control.peak_mature == EMULATOR_MIN_HEAP_WORDS as usize
                || control.peak_mature == MATURE_CAPACITY_WORDS as usize,
            "the control's mature generation is the {EMULATOR_MIN_HEAP_WORDS}-word floor it was \
             created with, or the same {MATURE_CAPACITY_WORDS}-word target once one of its rare \
             fullsweeps has re-derived it, at N={n}: control={control:?}"
        );
    }
}
