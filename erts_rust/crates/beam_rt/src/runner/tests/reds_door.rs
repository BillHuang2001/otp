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
//! (`RuntimeContext::move_result_into_process_heap`) can only **grow** the nursery, so before
//! this door existed the process was never collected at all: `heap_size`
//! (`beam_gc`'s nursery-chain capacity, the `process_info(P, garbage_collection)`
//! `heap_size` figure) climbed with the loop count — measured here at **3 074 841 / 29 485 327
//! / 264 963 824** words for N = 2 000 / 20 000 / 200 000 and **0** collections, i.e. ≈ 1 300
//! words retained per iteration.
//!
//! With the door live, the same three runs read **6 / 60 / 600** collections, `minor_gcs`
//! **5 / 59 / 599** and a **flat `heap_size` of 233 words** (the collector's `min_heap_words`
//! floor) with a one-space nursery throughout — the live set really is a handful of words, so
//! the capacity stops growing after the first boundary.
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
/// **A loop that reaches no heap-admission point is collected at the yield boundary, and its
/// `heap_size` stops climbing with N.**
///
/// Three derived relations, each pinned exactly (the counters are structural facts of the
/// runner and the collector — never timings — so they are deterministic on a loaded host):
///
/// * **the collections come from this door.** The fixture contains no `test_heap`/
///   `allocate_heap` at all, so with the door disabled the same runs measure **0** collections
///   (measured; see the module docs). Any non-zero count is therefore the boundary door, and a
///   slice of this loop holds a **fixed** number of iterations, so the count is a constant rate
///   per iteration at every N (and never a per-call work increase). The loop's per-iteration
///   reduction price grew when the slice began **pricing** its `call_ext` resolutions, its
///   `call`/`call_only` resolutions and its BIF dispatches (one unit each, in
///   [`crate::runner::BudgetProbe::spend`], on top of the interpreter's per-instruction charge):
///   measured **17.8–18.0 reds per iteration**, i.e. ~200–225 iterations per 4 000-reduction
///   slice ([`crate::context::gc::REDS_BOUNDARY_MIN_REDS`]), against ~14 before that pricing.
///   The band below is that measured rate, not a calibrated constant.
/// * **`minor_gcs` follows the same relation minus the one fullsweep** the policy's
///   `fullsweep_after` forces during the run (`collections - 1` at every N measured, hence
///   `(8, 88)` for the two rows below), which is what makes the `garbage_collection` item's
///   counter move with real collections instead of sitting at the pre-fix `0`.
/// * **`heap_size` is bounded**: the peak chain capacity **converges** — one young space
///   ([`heap_size`] 610 words at every N measured, the collector's sizing of the live set the
///   boundary window actually holds: with the loop priced more finely the door now fires with
///   the in-flight 400-element list/tuple still live, where it used to fire after the iteration
///   had abandoned it and settled on the 233-word floor). What the pin is *for* — flatness in N
///   and a single retired space — is unchanged (before the door: 3 074 841 / 29 485 327 /
///   264 963 824 words for N = 2 000 / 20 000 / 200 000).
///
/// No collection may fail (`errors`, `halted`) and no run may park in a `receive`.
#[test]
fn a_bif_only_loop_is_collected_at_the_yield_boundary_and_its_heap_size_plateaus() {
    let _tier = InterpreterTier::new();
    const COUNTS: [i64; 2] = [2_000, 20_000];
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
    let [(n1, r1, heap1), (n2, r2, heap2)] = [rows[0], rows[1]];
    // Every collection here is the boundary door's: the workload reaches no other site.
    assert!(
        r1.collections > 0 && r2.collections > 0,
        "the door must fire at all (it is the only collection site this workload reaches): \
         {r1:?} {r2:?}"
    );
    assert_eq!(
        r1.yields as u64,
        r1.collections,
        "one boundary per pre-emption: {r1:?}"
    );
    assert_eq!(
        r2.yields as u64,
        r2.collections,
        "one boundary per pre-emption: {r2:?}"
    );
    // The derived relation: a constant pre-emption rate per iteration (a slice holds a fixed
    // number of iterations), so the boundary's count is linear in N with no per-call work
    // increase. The rate is the loop's per-iteration reduction price over the slice's 4 000:
    // measured 17.8 reds/iteration at N=20 000 (89 slices) and 18.0 at N=2 000 (9), against
    // ~14 before the call/BIF seams were priced. Pinned as that measured band.
    for (n, row) in [(n1, &r1), (n2, &r2)] {
        let reds_per_iteration = row.collections * 4_000 / n as u64;
        assert!(
            (17..=20).contains(&reds_per_iteration),
            "N={n} must collect once per 4 000-reduction slice over a ~17.8-reds iteration \
             (17..=20 reds/iteration), got {reds_per_iteration}: {row:?}"
        );
    }
    assert!(r2.collections > r1.collections, "more iterations ⇒ more collections: {r2:?}");
    // `minor_gcs` is the minors since the last fullsweep, so it is the collection count minus
    // the one fullsweep the policy's `fullsweep_after` forces during each run.
    let minor = |r: &super::GrowthCounters| r.collections - r.major;
    assert_eq!(
        (minor(&r1), minor(&r2)),
        (8, 88),
        "the garbage_collection item's counter must move with the collections: {r1:?} {r2:?}"
    );
    // The point of the door: a flat `heap_size` instead of a linear climb with N (the pre-door
    // figures are 3 074 841 and 29 485 327 words — ≈ 1 300 retained words per iteration). The
    // converged capacity is 610 words at this cadence (see the doc comment), and it is the same
    // at both N — flatness, not the absolute floor, is the property.
    assert_eq!(heap1, 610, "the nursery must converge, not climb: N={n1}");
    assert_eq!(
        (heap2, r1.peak_young_spaces, r2.peak_young_spaces),
        (610, 1, 1),
        "heap_size must NOT climb with N, and the chain must stay one space: N={n2}"
    );
}
