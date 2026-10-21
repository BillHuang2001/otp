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
//! The **reduction/yield-boundary** collection point
//! ([`crate::context::gc::collect_at_reds_boundary`]) and its call site in the runner's
//! pre-emption arm — the slice-end door for a workload that reaches **no** heap-admission
//! point.
//!
//! The fixture (`tests/fixtures/reds_boundary.erl`, compiled with the system `erlc`) is the
//! minimal such shape: its loop body allocates only through registered BIFs
//! (`list_to_tuple(lists:seq(1, 400))`), so `erlc -S` emits **no** `test_heap`/`allocate_heap`
//! anywhere in the module — the loop is `call_ext`/`call`/`gc_bif`/`is_eq_exact`/`move` only.
//! `RuntimeContext::alloc_words` and the BIF-result copy-out
//! (`RuntimeContext::move_result_into_process_heap`) can only **grow** the nursery, so with the
//! door disabled the process is never collected at all: `heap_size`
//! (`beam_gc`'s nursery-chain capacity, the `process_info(P, garbage_collection)`
//! `heap_size` figure) climbs with the loop count — measured at **3 074 841 / 29 485 327
//! / 264 963 824** words for N = 2 000 / 20 000 / 200 000 and **0** collections, i.e. ≈ 1 300
//! words retained per iteration.
//!
//! With the door live, the same three runs read **9 / 89 / 899** collections (**1 / 3 / 23** of
//! them fullsweeps) and a **flat `heap_size` of 987 words** with a one-space nursery throughout.
//! The live set a collection of this shape measures is the single in-flight 400-element
//! `list_to_tuple/1` result (401 words, [`BOUNDARY_LIVE_WORDS`]), so the capacity is a **policy
//! function of that live set**, never a quantity that follows the loop count: 987 is
//! [`HeapPolicy::young_target_words`](beam_gc::HeapPolicy::young_target_words)`(401)`, the
//! whole-heap target the door's first boundary installs by escalating its refused minor to a
//! fullsweep, and every later minor then keeps it because it sits inside the young-only rule's
//! keep band
//! (`[young_minor_target_words(401), YOUNG_SHRINK_FACTOR x that]` = `[610, 4 880]`, see
//! [`HeapPolicy::young_after_collection`](beam_gc::HeapPolicy::young_after_collection)).
//! The test below drives all three N, so the figures are reproducible from it.
//!
//! # Why the run is forced onto the **interpreter**
//!
//! Only the interpreter is pre-empted: it charges the reduction budget once per interpreted
//! instruction, so a slice ends at the budget and the door gets its boundary. The native
//! baseline tier links this self-tail-recursive loop into one natively executed block that
//! never crosses the per-instruction hook, so it would run to completion inside a single
//! slice and the counters below would be meaningless. The tier is pinned per thread through
//! [`super::super::tier::force`] (a `#[cfg(test)]`-only override, cleared again by the guard
//! below), because the runtime builds its [`RunnerState`](super::super::entry::RunnerState)
//! internally and so cannot be reached through the per-state `jit_mode` field.
use std::time::Instant;
use beam_term::make_small;
use super::super::tier::{clear, force, JitMode};
use super::{growth_runtime, run_growth_counted, COUNTER_RUN_DEADLINE};
/// The fixture this suite drives: `reds_boundary:run/1`, a BIF-only allocation loop whose
/// module contains no heap-admission instruction (see the module docs).
fn boundary_fixture() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/reds_boundary.beam"
    )
}
/// Pins this thread's execution tier to the interpreter for as long as it is alive.
///
/// A `Drop` guard rather than a bare `force`/`clear` pair, so a failing assertion cannot leave
/// the override behind for the next test the harness runs on this thread.
struct InterpreterTier;
impl InterpreterTier {
    /// Forces the interpreter on this thread.
    fn new() -> InterpreterTier {
        force(JitMode::Interpreter);
        InterpreterTier
    }
}
impl Drop for InterpreterTier {
    fn drop(&mut self) {
        clear();
    }
}
/// The largest `heap_size` (the nursery **chain's** summed capacity, in words) observed at any
/// slice boundary of one run of `reds_boundary:run/1`.
///
/// This is the metric [`super::GrowthCounters`] cannot carry: its `peak_young_capacity` samples
/// [`beam_gc::AnyCollector::young`] — the **current** space only — while `heap_size` is
/// [`beam_gc::HeapInfo::heap_size`], the sum over the current space **and** every space a
/// growth retired. That sum is exactly what climbs when a workload is never collected (each
/// growth retires the space it replaced), so it is the figure the door exists to bound.
///
/// A terminated process is removed from the table, so the peak can only be seen **during** the
/// run: the loop samples at every slice boundary, the one window a live process's collector is
/// readable from outside the runner.
fn peak_heap_size(module: &str, n: i64) -> usize {
    let mut rt = growth_runtime(boundary_fixture());
    let pid = rt
        .spawn_process(module, "run", vec![make_small(n)])
        .unwrap_or_else(|err| panic!("{module}:run/1 must spawn: {err}"));
    let mut peak = 0usize;
    let deadline = Instant::now() + COUNTER_RUN_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "{module}:run/1 with N={n} did not terminate within {}s",
            COUNTER_RUN_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
        if let Some(process) = rt.processes().get(pid) {
            peak = peak.max(process.collector().heap_info().heap_size);
        }
    }
    assert_eq!(
        beam_term::signed_value(rt.process_result(pid).expect("the loop returned a value")),
        n,
        "{module}:run({n}) must return its argument"
    );
    peak
}
/// The words one `reds_boundary` iteration leaves live in the boundary window: the 400-element
/// `list_to_tuple/1` result (one header word plus 400 element words) the pre-emption catches
/// before the iteration abandons it.
const BOUNDARY_LIVE_WORDS: usize = 401;

/// The heap policy the measured runs' `spawn_process` children are created with — the
/// emulator-default policy ([`HeapOptions::new`](crate::config::HeapOptions::new), the `heap`
/// field of the [`RuntimeConfig`](crate::config::RuntimeConfig) [`growth_runtime`] builds its
/// runtime from), whose `min_heap_words` is the C `H_DEFAULT_SIZE`.
///
/// The two capacities asserted below are **derived from it** and from the boundary window's live
/// set, never pasted: the policy is the rule that installs a nursery capacity in the first place.
fn boundary_policy() -> beam_gc::HeapPolicy {
    crate::config::HeapOptions::default().policy().clone()
}

/// **A loop that reaches no heap-admission point is collected at the yield boundary, and its
/// `heap_size` stops climbing with N.**
///
/// Three groups of relations, each pinned as an **invariant** of the runner, the collector and its
/// heap policy — never as a calibrated integer, and never as a timing (every counter below is a
/// deterministic count, so the figures are reproducible on a loaded host):
///
/// * **the collections come from this door, once per pre-emption.** The fixture contains no
///   `test_heap`/`allocate_heap` at all, so with the door disabled the same runs measure **0**
///   collections (measured; see the module docs). Any non-zero count is therefore the boundary
///   door, and `yields == collections` says it fires exactly once per pre-emption — never once per
///   instruction and never twice for one park. Because a slice prices its `call_ext`/`call`
///   resolutions and its BIF dispatches (one unit each, in
///   [`crate::runner::BudgetProbe::spend`], on top of the interpreter's per-instruction charge), a
///   slice holds a **fixed** number of iterations and the loop's per-iteration price is constant in
///   N: measured **18.0 reductions per iteration** at every row (36 006 / 359 966 / 3 599 562 over
///   2 000 / 20 000 / 200 000), i.e. ~222 iterations per 4 000-reduction slice
///   ([`crate::context::gc::REDS_BOUNDARY_MIN_REDS`]). The assertion is that rate as a band, never
///   the integer a `collections * 4 000 / n` division truncates to.
/// * **`minor_gcs` tracks the real collections.** The `garbage_collection` item's `minor_gcs` is
///   the run's `collections - major`, so it must be positive and grow with N, and the run's
///   fullsweeps must stay a **minority** of its collections (measured 1 of 9, 3 of 89 and 23 of
///   899). A fullsweep here is the door's own escalation — its kind-pinned minor escalating when
///   the promotion cannot fit the mature region — and the region a fullsweep installs is sized far
///   above the young live set each later boundary promotes, so the cadence is minors throughout.
///   The policy's `fullsweep_after` cadence (65 535) never fires at these N. This is what makes the
///   `garbage_collection` item's counter move with real collections instead of sitting at `0`.
/// * **`heap_size` is bounded, and the bound is a policy function of the live set.** The peak
///   chain capacity **converges** at every N — one young space of **987** words, which is
///   `young_target_words(BOUNDARY_LIVE_WORDS)`: a capacity the rule *derives* from the iteration's
///   footprint, kept since by the young-only rule because it lies inside that rule's band
///   (`nursery_floor_words() = 233 <= 987 <= 8 x 610 = 4 880`). The pins are that derivation (the
///   plateau is one of the two capacities the policy produces for this live set), that the plateau
///   is a **fixed point** of `young_after_collection` (so it can neither ratchet nor oscillate),
///   and that it does not move with N. What the pin is *for* is the contrast with the door disabled
///   (3 074 841 / 29 485 327 / 264 963 824 words for N = 2 000 / 20 000 / 200 000): a plateau, not
///   a climb.
///
/// No collection may fail (`errors`, `halted`) and no run may park in a `receive`.
#[test]
fn a_bif_only_loop_is_collected_at_the_yield_boundary_and_its_heap_size_plateaus() {
    let _tier = InterpreterTier::new();
    const COUNTS: [i64; 3] = [2_000, 20_000, 200_000];
    let mut rows = Vec::new();
    for n in COUNTS {
        let mut rt = growth_runtime(boundary_fixture());
        let row = run_growth_counted(&mut rt, "reds_boundary", n);
        assert_eq!(row.value, n, "reds_boundary:run({n}) must return its argument");
        assert_eq!(row.errors, 0, "no collection may fail: N={n} {row:?}");
        assert_eq!(row.waits, 0, "the fixture never parks in a receive: {row:?}");
        let peak = peak_heap_size("reds_boundary", n);
        eprintln!(
            "reds_boundary N={n} slices={} yields={} collections={} major={} heap_size={peak}",
            row.slices, row.yields, row.collections, row.major
        );
        rows.push((n, row, peak));
    }
    let [(n1, r1, heap1), (n2, r2, heap2), (n3, r3, heap3)] = [rows[0], rows[1], rows[2]];
    // 1. Every collection here is the boundary door's, and there is exactly one per pre-emption.
    //    The mean per-iteration price is the loop's own (the slice's 4 000 over the iterations it
    //    holds), so it is constant in N and the counts follow N linearly.
    for (n, row) in [(n1, &r1), (n2, &r2), (n3, &r3)] {
        assert!(
            row.collections > 0,
            "the door must fire at all at N={n} (it is the only collection site this workload \
             reaches): {row:?}"
        );
        assert_eq!(
            row.yields as u64,
            row.collections,
            "one boundary collection per pre-emption at N={n}: {row:?}"
        );
        let reds_per_iteration = row.reductions as f64 / n as f64;
        assert!(
            (17.0..=19.0).contains(&reds_per_iteration),
            "N={n}: the loop's per-iteration price must stay the ~18 reductions a compiled \
             iteration costs (the slice's 4 000 over the ~222 iterations it holds), got \
             {reds_per_iteration:.4}: {row:?}"
        );
    }
    assert!(
        r1.collections < r2.collections && r2.collections < r3.collections,
        "more iterations ⇒ more collections: {r1:?} {r2:?} {r3:?}"
    );
    // 2. `garbage_collection`'s `minor_gcs` item is the run's `collections - major`, so it must
    //    move with the real collections — and the fullsweeps must stay a minority of them.
    let minor = |r: &super::GrowthCounters| r.collections - r.major;
    for (n, row) in [(n1, &r1), (n2, &r2), (n3, &r3)] {
        assert!(
            minor(row) > 0,
            "the garbage_collection item's counter must move with the collections at N={n}: \
             {row:?}"
        );
        assert!(
            minor(row) > row.major,
            "the run's fullsweeps must be a minority of its collections at N={n}: {row:?}"
        );
    }
    assert!(
        minor(&r1) < minor(&r2) && minor(&r2) < minor(&r3),
        "the minors must grow with N: {} {} {}",
        minor(&r1),
        minor(&r2),
        minor(&r3)
    );
    // 3. The point of the door: a flat `heap_size` instead of a linear climb with N, on a capacity
    //    the policy derives from the boundary window's live set and the minor rule then keeps.
    let policy = boundary_policy();
    let minor_target = policy.young_minor_target_words(BOUNDARY_LIVE_WORDS);
    let fullsweep_target = policy.young_target_words(BOUNDARY_LIVE_WORDS);
    let floor = policy.nursery_floor_words();
    assert!(
        minor_target >= floor && fullsweep_target >= minor_target,
        "the policy's two targets for a {BOUNDARY_LIVE_WORDS}-word live set must be ordered \
         ({floor} <= {minor_target} <= {fullsweep_target})"
    );
    for (n, heap) in [(n1, heap1), (n2, heap2), (n3, heap3)] {
        assert_eq!(
            heap, heap1,
            "the nursery must converge, not climb: N={n} peak={heap} vs N={n1} peak={heap1}"
        );
        assert!(
            heap == minor_target || heap == fullsweep_target,
            "the plateau must be a capacity the policy derives from the boundary window's \
             {BOUNDARY_LIVE_WORDS}-word live set — a minor's target ({minor_target}) or a \
             fullsweep's ({fullsweep_target}) — never one that follows the fill, the capacity it \
             replaced or the loop count: N={n} peak={heap}"
        );
        assert_eq!(
            policy.young_after_collection(heap, BOUNDARY_LIVE_WORDS),
            heap,
            "the plateau must be a fixed point of the minor rule, so that no collection resizes it \
             and none ratchets it: N={n} peak={heap}"
        );
        assert!(
            heap >= floor && heap <= beam_gc::YOUNG_SHRINK_FACTOR * minor_target,
            "the plateau must sit inside the minor rule's keep band, \
             {floor} <= capacity <= {} x {minor_target} = {}: N={n} peak={heap}",
            beam_gc::YOUNG_SHRINK_FACTOR,
            beam_gc::YOUNG_SHRINK_FACTOR * minor_target
        );
    }
    // ... and the retired-space chain stays collapsed: one young space at every N is what makes
    // `heap_size` a plateau rather than a sum of the spaces each growth left behind.
    assert_eq!(
        (r1.peak_young_spaces, r2.peak_young_spaces, r3.peak_young_spaces),
        (1, 1, 1),
        "heap_size must NOT climb with N, and the chain must stay one space at every N"
    );
}
