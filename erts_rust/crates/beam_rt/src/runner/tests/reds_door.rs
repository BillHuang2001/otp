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
//! With the door live, the same three runs read **9 / 89 / 899** collections, **8 / 86 / 876**
//! minors (the rest are the door's escalating fullsweeps) and a **flat `heap_size` of 987 words**
//! with a one-space nursery throughout — the live set the boundary window holds is the single
//! in-flight 400-element tuple, so the capacity is the **fullsweep** rule's target for it
//! ([`HeapPolicy::young_target_words`](beam_gc::HeapPolicy::young_target_words)) at every N,
//! never a quantity that follows the loop count. The test below drives all three N, so the
//! figures are reproducible from it.
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

/// The young-nursery capacity the **fullsweep** rule installs for [`BOUNDARY_LIVE_WORDS`] —
/// `young_target_words(live)` = `next_words(2 x live + need_reserve_words)`, floored by
/// `nursery_floor_words()` — under the emulator-default policy a plain `spawn` child is created
/// with ([`HeapOptions::new`](crate::config::HeapOptions::new), the `heap` field of the
/// [`RuntimeConfig`](crate::config::RuntimeConfig) [`growth_runtime`] builds its runtime from).
///
/// This is the plateau every row must read, so the pin is the rule rather than a measured
/// integer: at 401 live words `next_words(866)` is **987**. It is the *fullsweep* target and not
/// the minor one (`next_words(465)` = 610) because the run's **first** collection is the door's
/// escalating fullsweep — the mature region a spawn policy starts at holds 233 words, far less
/// than the ~800-word live promotion the first pre-emption holds, so the kind-pinned minor
/// refuses and `Process::collect` runs a fullsweep, whose `collapse_after_fullsweep` installs
/// this target. Every later minor then *keeps* it, because 987 sits inside
/// `young_after_collection`'s band `[young_minor_target_words(401) = 610, 8 x 610 = 4 880]`.
/// The two fixed-point assertions below pin exactly that: the plateau is a steady state of
/// **both** rules, which is what makes it flat in N instead of a ratchet.
fn plateau_words() -> usize {
    let policy = crate::config::HeapOptions::default().policy().clone();
    let target = policy.young_target_words(BOUNDARY_LIVE_WORDS);
    assert!(
        target >= policy.nursery_floor_words(),
        "the fullsweep target can never sit below the policy's floor: {target} < {}",
        policy.nursery_floor_words()
    );
    assert_eq!(
        policy.young_after_fullsweep(target, BOUNDARY_LIVE_WORDS),
        target,
        "the fullsweep rule must reproduce its own target (a next fullsweep keeps it)"
    );
    let minor = policy.young_minor_target_words(BOUNDARY_LIVE_WORDS);
    assert!(
        target >= minor && target <= beam_gc::YOUNG_SHRINK_FACTOR * minor,
        "a kept nursery lives inside [young_minor_target_words({BOUNDARY_LIVE_WORDS}) = {minor}, \
         YOUNG_SHRINK_FACTOR x that = {}] — that band is why the fullsweep's {target} survives \
         every later minor",
        beam_gc::YOUNG_SHRINK_FACTOR * minor
    );
    assert_eq!(
        policy.young_after_collection(target, BOUNDARY_LIVE_WORDS),
        target,
        "hence the minor rule keeps it: the plateau is a fixed point of both rules"
    );
    target
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
/// * **`minor_gcs` follows the same relation minus the door's escalating fullsweeps** —
///   `collections - major` at every N, measured **(8, 86, 876)** for the three rows below. A
///   fullsweep is what the door falls back to when its kind-pinned minor cannot be promoted: the
///   mature region a spawn policy starts at holds 233 words (`HeapPolicy::initial_mature_words`)
///   while the live promotion the **first** pre-emption holds is ~800 words (`lists:seq(1, 400)`
///   consed plus the 401-word tuple), so that first minor refuses and `Process::collect` runs a
///   fullsweep — which is what sizes the region from the live set and makes the cadence minors
///   afterwards. Later fullsweeps are the same refusal recurring as that region fills (the
///   promotion of each minor is counted against its room by `minor_room_words`, the **live**
///   promotion, `beam_gc`'s `collector.rs`), so a run takes `1 / 3 / 23` of them: at least one
///   (the first, always) and never more than one per two minors — `MATURE_TARGET_LIVE_MULTIPLE`
///   (2) is exactly the number of promotions a fullsweep sizes the region for, so a fullsweep
///   can never be the common outcome. This is what makes the `garbage_collection` item's counter
///   move with real collections instead of sitting at the pre-fix `0`.
/// * **`heap_size` is bounded**: the peak chain capacity **converges** — one young space of
///   `young_target_words(401)` words (987, [`plateau_words`]) at every N measured. The
///   fixture's live set at a boundary is the single in-flight 400-element `list_to_tuple/1`
///   result (401 words), the fullsweep sizes one nursery at twice that plus the reserve
///   (`next_words(866)`), and the minor rule then keeps it because 987 lies inside
///   `[610, 4 880]` — so the capacity is a function of the iteration's footprint and never of the
///   loop count. What the pin is *for* — flatness in N and a single retired space — is unchanged
///   (with the door disabled: 3 074 841 / 29 485 327 / 264 963 824 words for N = 2 000 /
///   20 000 / 200 000).
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
    // Every collection here is the boundary door's: the workload reaches no other site. A slice of
    // this loop holds a **fixed** number of iterations, so the count is a constant rate per
    // iteration at every N (never a per-call work increase); the rate is the loop's per-iteration
    // reduction price over the slice's 4 000 — measured 17.8–18.0 reds/iteration at all three N,
    // against ~14 before the call/BIF seams were priced. Pinned as that measured band.
    for (n, row) in [(n1, &r1), (n2, &r2), (n3, &r3)] {
        assert!(
            row.collections > 0,
            "the door must fire at all at N={n} (it is the only collection site this workload \
             reaches): {row:?}"
        );
        assert_eq!(
            row.yields as u64,
            row.collections,
            "one boundary per pre-emption at N={n}: {row:?}"
        );
        let reds_per_iteration = row.collections * 4_000 / n as u64;
        assert!(
            (17..=20).contains(&reds_per_iteration),
            "N={n} must collect once per 4 000-reduction slice over a ~17.8-reds iteration \
             (17..=20 reds/iteration), got {reds_per_iteration}: {row:?}"
        );
    }
    assert!(
        r1.collections < r2.collections && r2.collections < r3.collections,
        "more iterations ⇒ more collections: {r1:?} {r2:?} {r3:?}"
    );
    // `minor_gcs` is the collections minus the door's escalating fullsweeps: the first boundary's
    // kind-pinned minor cannot be promoted into the 233-word mature region a spawn policy starts
    // with (~800 live words), so it escalates once, and later escalations recur only as that
    // region fills — 1 / 3 / 23 fullsweeps over 9 / 89 / 899 collections.
    let minor = |r: &super::GrowthCounters| r.collections - r.major;
    assert_eq!(
        (minor(&r1), minor(&r2), minor(&r3)),
        (8, 86, 876),
        "the garbage_collection item's counter must move with the collections: \
         {r1:?} {r2:?} {r3:?}"
    );
    // ... and the split is the door's own: the minors really are the pre-emptions' collections
    // minus the escalations (the `yields == collections` identity above makes this exact), every
    // run escalates at least once (the first boundary, the derivation above), and a fullsweep can
    // never be the common outcome — one fullsweep sizes the mature region for
    // `MATURE_TARGET_LIVE_MULTIPLE` (2) promotions, so a minor cannot be outnumbered by one.
    for (n, row) in [(n1, &r1), (n2, &r2), (n3, &r3)] {
        assert!(
            row.major > 0,
            "the first boundary's minor always refuses the 233-word initial mature region, so \
             every run escalates at least once: N={n} {row:?}"
        );
        assert_eq!(
            minor(row),
            row.yields as u64 - row.major,
            "minors = pre-emptions - escalations at N={n}: {row:?}"
        );
        assert!(
            row.major * beam_gc::MATURE_TARGET_LIVE_MULTIPLE as u64 <= minor(row),
            "a fullsweep sizes the mature region for {} promotions, so minors must outnumber \
             fullsweeps by at least that factor: N={n} {row:?}",
            beam_gc::MATURE_TARGET_LIVE_MULTIPLE
        );
    }
    // The point of the door: a flat `heap_size` instead of a linear climb with N (the
    // door-disabled figures are 3 074 841 / 29 485 327 / 264 963 824 words — ≈ 1 300 retained
    // words per iteration). The converged capacity is the minor rule's target for the boundary
    // window's live set (`young_minor_target_words(401)`, 610 words — see the doc comment), and it
    // is the same at every N — flatness, not the absolute figure, is the property.
    let plateau = plateau_words();
    for (n, heap) in [(n1, heap1), (n2, heap2), (n3, heap3)] {
        assert_eq!(heap, plateau, "the nursery must converge, not climb: N={n}");
    }
    assert_eq!(
        (r1.peak_young_spaces, r2.peak_young_spaces, r3.peak_young_spaces),
        (1, 1, 1),
        "heap_size must NOT climb with N, and the chain must stay one space at every N"
    );
}
