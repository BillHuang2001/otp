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
//! **The slice budget prices native-tier work: the per-iteration price, the refusing seam that
//! ends a slicing loop, and the shape whose own work can never end one.**
//!
//! Commit `4968e89402` gave [`BudgetProbe::spend`](super::super::budget::BudgetProbe) — a
//! **non-refusing** price — to the four seams a compiled block *does* reach
//! ([`call_bif`](beam_engine::ExecutionContext::call_bif)/
//! [`call_bif_in_machine`](beam_engine::ExecutionContext::call_bif_in_machine) and
//! [`resolve_external_call`](beam_engine::ExecutionContext::resolve_external_call)/
//! [`resolve_local_call`](beam_engine::ExecutionContext::resolve_local_call)), and the baseline
//! driver also takes one **refusing** charge per driver-loop iteration (per block entry, where
//! `Machine::current_code` is published — see `jit/baseline/mod.rs`'s loop head). Before either, a
//! native baseline-JIT block crossed neither the interpreter's per-instruction `reduce` hook nor,
//! on a BIF-and-call-only loop, a heap-admission point — so such a run charged **nothing** and
//! `erlang:statistics(reductions)` / the process's own counter stayed at `0`. This suite pins the
//! four facts that follow, all from deterministic counters (never wall clock — the host is shared
//! and heavily loaded, so only counts and ratios are read).
//!
//! # What was measured (`scripts/cargo.sh test -p beam_rt`, this host — every figure below is
//! reproduced byte-for-byte by the run that pins it, printed by [`report`]; counters, never
//! timings)
//!
//! `native_reds:heap/1` (`tests/fixtures/native_reds.erl`) is `reds_boundary`'s BIF-and-call-only
//! loop — each iteration allocates only through registered BIFs
//! (`list_to_tuple(lists:seq(1, 400))`), so the module contains **no** `test_heap`/
//! `allocate_heap` at all (`erlc -S`: `allocate`/`call_ext`/`call_ext`/`gc_bif`/`call_last`) —
//! plus one in-process read of the running collector's `heap_size` after the loop, which is the
//! only way to observe a shape whose own work never ends a slice (see pin 4).
//!
//! | tier | N | slices | yields | waits | collections | major | peak_young_spaces | reductions | heap_size |
//! |---|---|---|---|---|---|---|---|---|---|
//! | native (default) | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 9 | 233 |
//! | native | 10 | 1 | 0 | 0 | 0 | 0 | 0 | 59 | 28 309 |
//! | native | 100 | 1 | 0 | 0 | 0 | 0 | 0 | 509 | 196 267 |
//! | native | 400 | 1 | 0 | 0 | 0 | 0 | 0 | 2 009 | 514 454 |
//! | native | 700 | 1 | 0 | 0 | 0 | 0 | 0 | 3 509 | 1 347 480 |
//! | native | 798 | 1 | 0 | 0 | 0 | 0 | 0 | 3 999 | 1 347 480 |
//! | native | 799 | 2 | 1 | 0 | 1 | 1 | 1 | 4 004 | 233 |
//! | native | 800 | 2 | 1 | 0 | 1 | 1 | 1 | 4 006 | 233 |
//! | native | 2 000 | 2 | 1 | 0 | 1 | 1 | 1 | 4 006 | 233 |
//! | native | 4 000 | 2 | 1 | 0 | 1 | 1 | 1 | 4 006 | 233 |
//! | native | 20 000 | 2 | 1 | 0 | 1 | 1 | 1 | 4 006 | 233 |
//! | interpreter | 798 | 4 | 3 | 0 | 3 | 1 | 1 | 14 386 | 196 267 |
//! | interpreter | 2 000 | 10 | 9 | 0 | 9 | 1 | 1 | 36 018 | 233 |
//!
//! `heap_growth:run/1` (the allocating engine-native shape — one 401-word `test_heap` per
//! iteration) on the same default native tier:
//!
//! | N | slices | yields | waits | collections | major | peak_young_spaces | reductions |
//! |---|---|---|---|---|---|---|---|
//! | 2 000 | 3 | 2 | 0 | 2 002 | 1 | 1 | 10 005 |
//! | 10 000 | 13 | 12 | 0 | 10 012 | 1 | 1 | 50 005 |
//! | 20 000 | 26 | 25 | 0 | 20 025 | 1 | 1 | 100 005 |
//! | 80 000 | 101 | 100 | 0 | 80 100 | 2 | 1 | 400 005 |
//!
//! # The four pins
//!
//! 1. **The charge really happens on the native tier, and below the budget it is an exact price
//!    per iteration: `reductions == 5N + 9`.** Measured at N = 10 / 100 / 400 / 700 / 798, and
//!    N = 0 pins the constant alone. The loop prices **5 units per iteration**; the run's own
//!    entry/epilogue seams add a constant **9**. What the counters *do not* show is the split of
//!    either figure: it is consistent with the three seam consultations the loop body performs
//!    (`call_ext lists:seq/2`, `call_ext erlang:list_to_tuple/1`, `call_last loop/1`) plus
//!    `lists:seq/2`'s own internal resolutions and a block entry or two — but the pin asserts the
//!    **count**, never a decomposition. The linear form is exact while the total stays under one
//!    slice's budget ([`DEFAULT_REDUCTIONS_PER_SLICE`] = 4 000), and `priced(N) = reductions − 9`
//!    is then an exact multiple of the per-iteration price: `priced(400) == 4·priced(100)` and
//!    `priced(100) == 10·priced(10)` are asserted as equalities. The pre-change figure for the
//!    same runs was **0** (`spend` did not exist), which is what makes "non-zero, and 5 per
//!    iteration" the pin — verified by knocking `spend`'s body out in the working tree: the rows
//!    then report `reductions = 0` and this test fails on its first assertion.
//! 2. **The budget boundary is derived, not calibrated.** The smallest N whose linear total can no
//!    longer fit in one slice is `floor((4 000 − 9) / 5) + 1 = 799`: at N = 798 the run charges
//!    3 999 and stays inside one slice, at N = 799 it charges 4 004 and needs two. Both sides are
//!    measured and pinned, so a change to the per-iteration price or to the budget that moved the
//!    boundary would have to be re-derived here rather than silently accepted.
//! 3. **A native loop that reaches a refusing seam still ends its slice.** `heap_growth` reaches
//!    the heap-admission door (`test_heap` → `ensure_heap_full`, a refusing `charge`) on every
//!    iteration, so the budget is what ends each slice: `yields + 1 == slices` (every slice but
//!    the last ends in the `Yield` arm), `slices == ceil(reductions / 4 000)` (no slice ends with
//!    budget left except the last, which simply runs out of work), and because the shape prices
//!    **5 units per iteration** (`reductions == 5N + 5`) a slice holds at most **800** iterations
//!    — measured as the exact band `800·(slices − 1) ≤ N ≤ 800·slices − 1` on every row, both
//!    halves derived from those two laws rather than calibrated. Its collections are exactly
//!    `N + yields`: one admission collection per iteration plus one boundary collection per
//!    pre-emption (measured 2 002 / 10 012 / 20 025 / 80 100), so the pre-emption rate does not
//!    grow with N (the per-call figures are flat over a 4x step).
//! 4. **The honest residual: this shape's own work can never end a slice.** `native_reds` prices
//!    its iterations through `spend` (a `spend` never refuses, and once the budget is gone it is a
//!    silent no-op — it does not even count), so past the budget the loop keeps running
//!    **uncharged** and its `Yield` is not the loop's at all: it is the *next refusing charge*,
//!    which the run reaches only in its epilogue. The measured consequences, all pinned as
//!    equalities:
//!    * the charge saturates at **exactly 4 006** for *every* N from 800 to 20 000 — the first
//!      slice's whole budget (4 000) plus a constant **6** charged in the resumed slice (per-slice
//!      `BudgetProbe::used` measured with a temporary per-slice print: 4 000 then 6; how the
//!      constant 9 splits into "before the loop" and "epilogue" is *not* derivable from the
//!      totals — only its sum is) — flat in N, so it can never track the work;
//!    * `slices == 2 && yields == 1` on every one of those rows, where a loop-driven pre-emption
//!      cadence would need `ceil((5N + 9) / 4 000)` slices (3 at N = 2 000, 26 at N = 20 000);
//!    * a fixture of the same shape with **no** epilogue seam ends **zero** slices (measured
//!      `slices=1, yields=0, reductions=4 000` at N = 8 000), which is what identifies the
//!      epilogue as the refusing site rather than the loop;
//!    * the single `Yield` *does* collect once (a major at the boundary), so — unlike before this
//!      pin was re-derived — the shape's `heap_size` now sits on the collector's floor (233)
//!      instead of tracking N. The memory half of the old residual has therefore closed; the
//!      **charging** half has not: closing it needs a refusing charge the loop's own seams can
//!      take, i.e. the `beam_engine` handoff that makes a call-seam refusal re-runnable (the
//!      resume address must be the seam's own instruction — see `BudgetProbe::spend`'s docs).
//!      When that lands, this fourth pin is the assertion that must change (`yields` would then
//!      track `slices − 1`).
//!
//! The interpreter control is the same fixture under [`JitMode::Interpreter`] (forced per thread
//! through [`super::super::tier::force`], never through the process environment): it charges per
//! instruction, so its total really does track the work — **36 018** units and 10 slices / 9
//! yields / 9 `collections` at N = 2 000 against the native tier's flat 4 006 / 2 / 1 — and its
//! `heap_size` sits on the same collector's floor. That is the residual's discriminator: the gap
//! is a fact about the **tier**, not about the fixture.
use super::super::tier::{JitMode, clear, force};
use super::{GrowthCounters, growth_runtime, run_growth_function};
use beam_process::DEFAULT_REDUCTIONS_PER_SLICE;

/// The `native_reds` fixture: `heap/1` is the BIF-and-call-only loop of `reds_boundary:run/1`
/// plus an in-process `process_info(self(), heap_size)` report (see the module docs and
/// `tests/fixtures/native_reds.erl`).
fn native_reds_fixture() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/native_reds.beam"
    )
}

/// The charged units one `native_reds` iteration prices — the **measured** constant 5 of
/// `reductions == 5N + 9`, holding at N = 10 / 100 / 400 / 700 / 798.
///
/// The split is not observable from the counters alone; it is *consistent* with the loop body's
/// three seam consultations (two `call_ext` — `lists:seq/2` and `list_to_tuple/1`, the latter a
/// BIF dispatch — plus its local `call_last loop/1`), with `lists:seq/2`'s own two resolutions
/// (`seq/3` and `seq_1/3`), and with the driver's per-block charge. What the pin asserts is the
/// count, not the decomposition.
const NATIVE_REDS_UNITS_PER_ITERATION: u64 = 5;

/// The constant the run's own entry/epilogue seams add to the charge (measured: `reductions` is
/// exactly `9` at N = 0, the only row whose loop body never executes, and `5N + 9` throughout the
/// linear regime N ≤ 798).
const NATIVE_REDS_ENTRY_UNITS: u64 = 9;

/// What a **saturated** `native_reds` run charges beyond its first slice's budget: the constant
/// **6** the resumed slice spends in the epilogue (measured — a temporary per-slice print of
/// `BudgetProbe::used` reads `4 000` then `6`, for every N ≥ 800). It is *not* `ENTRY_UNITS`: the
/// loop's own entry is charged before the budget runs out, so only the epilogue's share lands in
/// the second slice. How the 9 splits between the two is not derivable from the totals — only the
/// sum is — so the split is measured, not assumed.
const NATIVE_REDS_EPILOGUE_UNITS: u64 = 6;

/// The collector's floor under this runtime: the young-nursery capacity a collected process reads
/// back as `process_info(self(), heap_size)` (measured 233 on every saturated `native_reds` row and
/// on the interpreter control).
const COLLECTOR_FLOOR: i64 = 233;

/// The largest `NativeReds` iteration count whose **linear** total (`5N + 9`) still fits in one
/// slice's budget: `floor((4 000 − 9) / 5) = 798`. At N = 799 the total is 4 004 and the run needs a
/// second slice — the boundary is derived from the two measured laws, never calibrated.
fn last_linear_native_reds_n() -> i64 {
    ((u64::from(DEFAULT_REDUCTIONS_PER_SLICE) - NATIVE_REDS_ENTRY_UNITS)
        / NATIVE_REDS_UNITS_PER_ITERATION) as i64
}

/// Pins this thread's execution tier to the interpreter for as long as it is alive — a `Drop`
/// guard, so a failing assertion cannot leave the override behind on the harness thread.
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

/// Runs `native_reds:heap/1` with `n` on a fresh runtime and returns the row (whose `value` is
/// the heap size the process reported for itself), printing the counter-table line for it.
fn native_reds_row(n: i64, label: &str) -> GrowthCounters {
    let mut rt = growth_runtime(native_reds_fixture());
    let row = run_growth_function(&mut rt, "native_reds", "heap", n);
    assert_eq!(row.errors, 0, "no collection may fail: N={n} {row:?}");
    assert_eq!(
        row.waits, 0,
        "the fixture never parks in a receive: {row:?}"
    );
    assert_eq!(
        row.fallbacks, 0,
        "the loop never falls back to the arena: {row:?}"
    );
    report("native_reds", label, n, &row);
    row
}

/// The per-row report line: `{n, slices, yields, waits, collections, major,
/// peak_young_spaces, reductions, value}` — the counter-table shape the neighbouring suites
/// print, so every figure quoted in the module docs is re-derived by the very run that pins it.
/// `value` is whatever the entry returned: the heap size the process reported for itself on the
/// `native_reds` rows (see the module docs' table), the iteration count on the `heap_growth` rows.
fn report(module: &str, label: &str, n: i64, row: &GrowthCounters) {
    eprintln!(
        "slice_charging {module} [{label}] N={n} : slices={} yields={} waits={} collections={} \
         major={} peak_young_spaces={} reductions={} value={}",
        row.slices,
        row.yields,
        row.waits,
        row.collections,
        row.major,
        row.peak_young_spaces,
        row.reductions,
        row.value,
    );
}

/// **The slice budget really is spent by native-tier work, a refusing seam still ends the slice,
/// and the shape whose own seams cannot refuse saturates at a flat ceiling.**
///
/// See the module docs for the measured tables, the derivation of every law, and the
/// `beam_engine` handoff that the fourth pin will need when it lands.
#[test]
fn the_slice_budget_prices_native_work_and_a_refusing_seam_still_ends_the_slice() {
    // The measurements below are the **default** tier's. `BEAM_RT_TIER=interpreter` (or any
    // unknown value) pins the reference tier process-wide, where every count in this suite moves
    // — that is what the control row at the end measures. Fail loudly rather than compare an
    // interpreter run against native-tier figures.
    assert_ne!(
        super::super::tier::env_default(),
        JitMode::Interpreter,
        "this suite measures the default (native baseline) tier; unset BEAM_RT_TIER"
    );
    let budget = u64::from(DEFAULT_REDUCTIONS_PER_SLICE);

    // ---- 1. the charge, on the native tier: an exact price per iteration -------------------
    // N = 0 alone pins the constant: the loop body never runs, so the whole total is the run's
    // own entry/epilogue seams.
    let empty = native_reds_row(0, "linear");
    assert_eq!(
        empty.reductions, NATIVE_REDS_ENTRY_UNITS,
        "N=0: with no iteration the total must be the entry/epilogue constant alone \
         ({NATIVE_REDS_ENTRY_UNITS}), got {}: {empty:?}",
        empty.reductions
    );

    // The linear regime, below the slice budget: the reported total is the loop's own price — 5
    // units per iteration plus the constant. (Before commit 4968e89402 the same runs reported 0:
    // no seam this shape reaches charged anything.)
    let last_linear = last_linear_native_reds_n();
    let linear: Vec<(i64, GrowthCounters)> = [10i64, 100, 400, 700, last_linear]
        .into_iter()
        .map(|n| (n, native_reds_row(n, "linear")))
        .collect();
    for &(n, row) in &linear {
        assert_eq!(
            row.reductions,
            NATIVE_REDS_UNITS_PER_ITERATION * (n as u64) + NATIVE_REDS_ENTRY_UNITS,
            "N={n}: the native tier must charge {} units per iteration + {} for the \
             entry/epilogue seams, got {}: {row:?}",
            NATIVE_REDS_UNITS_PER_ITERATION,
            NATIVE_REDS_ENTRY_UNITS,
            row.reductions
        );
        // Below the budget the whole run fits in ONE slice: nothing pre-empted it, and nothing
        // collected it — which is why its self-reported `heap_size` still tracks N here (the
        // pre-pin-4 memory shape, kept as the control for pin 4's collapse to the floor).
        assert_eq!(
            (row.slices, row.yields, row.waits, row.collections),
            (1, 0, 0, 0),
            "N={n}: a total under one slice's budget must fit in one slice and collect nothing: \
             {row:?}"
        );
        assert!(
            row.value > 1_000,
            "N={n}: an uncollected run's heap_size must track N (the collector's floor is \
             {COLLECTOR_FLOOR}), got {}: {row:?}",
            row.value
        );
    }
    // ... and it is a *price per iteration*, so it grows linearly in N: after the shared constant
    // is taken out, 4x / 7x / 10x the iterations is exactly 4x / 7x / 10x the charge.
    let priced = |row: &GrowthCounters| row.reductions - NATIVE_REDS_ENTRY_UNITS;
    assert_eq!(
        priced(&linear[2].1),
        4 * priced(&linear[1].1),
        "the charge must scale linearly with N (400 iterations = 4x 100): {linear:?}"
    );
    assert_eq!(
        priced(&linear[3].1),
        7 * priced(&linear[1].1),
        "the charge must scale linearly with N (700 iterations = 7x 100): {linear:?}"
    );
    assert_eq!(
        priced(&linear[1].1),
        10 * priced(&linear[0].1),
        "the charge must scale linearly with N (100 iterations = 10x 10): {linear:?}"
    );
    assert_eq!(
        priced(&linear[4].1),
        NATIVE_REDS_UNITS_PER_ITERATION * (last_linear as u64),
        "the last linear row must charge exactly {NATIVE_REDS_UNITS_PER_ITERATION} per iteration: \
         {linear:?}"
    );

    // ---- 2. the budget boundary is derived, not calibrated --------------------------------
    // A run fits in one slice exactly while `5N + 9 <= budget`; the first N that breaks it is the
    // pinned boundary, asserted from the two measured laws before it is measured.
    assert_eq!(last_linear, 798, "the derivation must yield 798");
    assert_eq!(
        NATIVE_REDS_UNITS_PER_ITERATION * (last_linear as u64) + NATIVE_REDS_ENTRY_UNITS,
        budget - 1,
        "the last linear row must land one unit under the budget (that is what makes it linear)"
    );
    let boundary = native_reds_row(last_linear + 1, "boundary");
    assert_eq!(
        boundary.reductions,
        NATIVE_REDS_UNITS_PER_ITERATION * (last_linear as u64 + 1) + NATIVE_REDS_ENTRY_UNITS,
        "N={}: one iteration past the boundary, the total is still the linear total (the loop's \
         spends drain the budget and the epilogue adds its constant): {boundary:?}",
        last_linear + 1
    );
    assert_eq!(
        (boundary.slices, boundary.yields, boundary.collections),
        (2, 1, 1),
        "N={}: the first N past the boundary must need a second slice (and collect once at the \
         yield): {boundary:?}",
        last_linear + 1
    );

    // ---- 3. the saturated regime: a flat ceiling, and ONE yield that is not the loop's ------
    // These are pin 4's rows. Two things are pinned together: the ceiling (the first slice's
    // whole budget plus the epilogue's measured 6) and the *single* pre-emption — a loop-driven
    // cadence would need `ceil((5N + 9) / budget)` slices, which the last assertion derives.
    let saturated: Vec<(i64, GrowthCounters)> = [800i64, 2_000, 4_000, 20_000]
        .into_iter()
        .map(|n| (n, native_reds_row(n, "saturated/residual")))
        .collect();
    for &(n, row) in &saturated {
        assert_eq!(
            row.reductions,
            budget + NATIVE_REDS_EPILOGUE_UNITS,
            "N={n}: a saturated run must charge exactly the first slice's budget ({budget}) plus \
             the epilogue's {NATIVE_REDS_EPILOGUE_UNITS}, got {}: {row:?}",
            row.reductions
        );
        assert_eq!(
            (
                row.slices,
                row.yields,
                row.waits,
                row.collections,
                row.major,
                row.peak_young_spaces
            ),
            (2, 1, 0, 1, 1, 1),
            "N={n}: exactly one pre-emption and one boundary collection, whatever N is: {row:?}"
        );
        // The one boundary collection collapses the run's whole retired chain, so — unlike the
        // pre-pin-4 shape — the heap now sits on the collector's floor instead of tracking N.
        assert_eq!(
            row.value, COLLECTOR_FLOOR,
            "N={n}: the single boundary collection must leave the collector's floor \
             ({COLLECTOR_FLOOR}), got {}: {row:?}",
            row.value
        );
    }
    assert_eq!(
        saturated[0].1.reductions, saturated[3].1.reductions,
        "the saturated charge cannot track N (a flat ceiling, not a cadence): {saturated:?}"
    );
    // The cadence the loop's own work *would* demand if its seams could refuse: the measured
    // slice count is 2 on every saturated row, i.e. 13x below the cadence at N = 20 000.
    let cadence = |n: i64| {
        (NATIVE_REDS_UNITS_PER_ITERATION * (n as u64) + NATIVE_REDS_ENTRY_UNITS + budget - 1)
            / budget
    };
    assert_eq!(cadence(2_000), 3, "the derivation of the cadence at N = 2 000");
    assert_eq!(cadence(20_000), 26, "the derivation of the cadence at N = 20 000");
    assert!(
        saturated[3].1.slices as u64 * 10 < cadence(20_000),
        "N=20 000: the measured slice count ({}) must be an order of magnitude below the \
         loop-driven cadence ({}) — that gap IS the residual: {saturated:?}",
        saturated[3].1.slices,
        cadence(20_000)
    );

    // ---- 4. the refusing seam: a native allocating loop ends its slice --------------------
    let mut growth_rows = Vec::new();
    for n in [2_000i64, 10_000, 20_000, 80_000] {
        let mut rt = growth_runtime(super::heap_growth_fixture());
        let row = run_growth_function(&mut rt, "heap_growth", "run", n);
        assert_eq!(
            row.value, n,
            "heap_growth:run({n}) must return its argument"
        );
        assert_eq!(row.errors, 0, "no collection may fail: N={n} {row:?}");
        assert_eq!(
            row.waits, 0,
            "heap_growth never parks in a receive: {row:?}"
        );
        report("heap_growth", "refusing seam", n, &row);
        growth_rows.push((n, row));
    }
    // The allocating shape's price is 5 units per iteration plus a constant 5 (measured on all
    // three rows) — the door it reaches on every iteration is a *refusing* charge, which is what
    // makes the budget, and not the loop, the thing that ends each slice.
    for &(n, ref row) in &growth_rows {
        assert_eq!(
            row.reductions,
            NATIVE_REDS_UNITS_PER_ITERATION * (n as u64) + 5,
            "N={n}: the allocating loop must price {} units per iteration + a constant 5: {row:?}",
            NATIVE_REDS_UNITS_PER_ITERATION
        );
    }
    let [(n1, g1), (n2, g2)] = [growth_rows[2], growth_rows[3]];
    for (n, row) in [(n1, &g1), (n2, &g2)] {
        // The pre-emption is real, and it is the *only* thing that ended a slice: every slice but
        // the last handed the machine back through the `Yield` arm, and each of those had spent
        // its whole budget (`slices == ceil(reductions / budget)` — a slice that ended for any
        // other reason would need another one).
        assert!(
            row.yields > 0,
            "an admission-door loop must be pre-empted: N={n} {row:?}"
        );
        assert_eq!(
            row.yields + 1,
            row.slices,
            "every slice but the last must end in the `Yield` arm: N={n} {row:?}"
        );
        let slices_from_budget = (row.reductions + budget - 1) / budget;
        assert_eq!(
            row.slices as u64, slices_from_budget,
            "the budget must be what ends each slice: N={n} {row:?}"
        );
        // A bounded number of iterations per slice, *derived*: a slice holds 4 000 priced units
        // and the shape prices 5 per iteration, so at most 800 iterations — and the last slice's
        // tail is what drops the average, giving the exact band below.
        let per_slice = budget / NATIVE_REDS_UNITS_PER_ITERATION;
        assert!(
            n as u64 <= per_slice * row.slices as u64
                && n as u64 >= per_slice * (row.slices as u64 - 1),
            "a {budget}-unit slice must hold between {per_slice}*(slices-1) and {per_slice}*slices \
             iterations of a {NATIVE_REDS_UNITS_PER_ITERATION}-unit iteration: N={n} {row:?}"
        );
        assert!(
            (769.0..=800.0).contains(&(n as f64 / row.slices as f64)),
            "N={n}: measured iterations/slice {}, derived floor {:.4}",
            n as f64 / row.slices as f64,
            per_slice as f64 * (row.slices as f64 - 1.0) / row.slices as f64
        );
        // Its collections are exactly one admission collection per iteration plus one boundary
        // collection per pre-emption.
        assert_eq!(
            row.collections,
            n as u64 + row.yields as u64,
            "N={n}: collections must be N + yields (one admission per iteration, one boundary per \
             pre-emption): {row:?}"
        );
    }
    // ... and the pre-emptions and the collections both scale **linearly** with N: over a 4x
    // step their per-call figures are flat (the ratio only ever falls, because a run's fixed tail
    // is amortized away by the wider run).
    let calls = |n: i64| (n as f64) + 1.0;
    let ratio = |a: f64, b: f64| (b / calls(n2)) / (a / calls(n1));
    assert!(
        ratio(g1.yields as f64, g2.yields as f64) <= 1.5,
        "yields/call must not grow with N: {} vs {}",
        g1.yields as f64 / calls(n1),
        g2.yields as f64 / calls(n2)
    );
    assert!(
        ratio(g1.collections as f64, g2.collections as f64) <= 1.5,
        "collections/call must not grow with N: {} vs {}",
        g1.collections as f64 / calls(n1),
        g2.collections as f64 / calls(n2)
    );
    assert!(
        ratio(g1.reductions as f64, g2.reductions as f64) <= 1.05,
        "reductions/call must be flat in N: {} vs {}",
        g1.reductions as f64 / calls(n1),
        g2.reductions as f64 / calls(n2)
    );
    assert!(
        g2.collections > g1.collections && g2.yields > g1.yields,
        "more iterations => more pre-emptions and more collections: {g1:?} {g2:?}"
    );

    // ---- the control: the same fixture on the interpreter **does** price the work ----------
    let control = {
        let _tier = InterpreterTier::new();
        native_reds_row(2_000, "interpreter control")
    };
    assert!(
        control.yields > 0,
        "the interpreter charges per instruction, so the same shape must be pre-empted: \
         {control:?}"
    );
    assert_eq!(
        control.yields as u64, control.collections,
        "one boundary collection per pre-emption (the shape reaches no admission collection of \
         its own): {control:?}"
    );
    assert_eq!(
        control.slices,
        control.yields + 1,
        "every interpreter slice but the last must end in the `Yield` arm: {control:?}"
    );
    assert_eq!(
        control.value, COLLECTOR_FLOOR,
        "the collected shape's heap_size must sit on the collector's floor ({COLLECTOR_FLOOR}): \
         {control:?}"
    );
    assert_eq!(
        control.reductions, 36_018,
        "the reference tier prices every instruction of the same 2 000 iterations (measured \
         36 018 = 10 slices' worth of work) — the counter that makes the native tier's flat \
         4 006 a residual rather than a property of the fixture: {control:?}"
    );
    assert!(
        control.reductions >= 8 * saturated[1].1.reductions,
        "the reference tier must charge at least 8x the native tier's saturated ceiling for the \
         same work (measured {} vs {}): the gap is the tier's, not the fixture's",
        control.reductions,
        saturated[1].1.reductions
    );
    assert!(
        control.slices > saturated[1].1.slices,
        "and it must need more slices: {} vs {}",
        control.slices,
        saturated[1].1.slices
    );
}
