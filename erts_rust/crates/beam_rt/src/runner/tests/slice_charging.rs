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
//! **The slice budget prices native-tier work: the BIF/call seam charges, the refusing seam
//! that still ends a slice, and the residual the first of those two leaves behind.**
//!
//! Commit `4968e89402` gave [`BudgetProbe::spend`](super::super::budget::BudgetProbe) — a
//! **non-refusing** price — to the four seams a compiled block *does* reach
//! ([`call_bif`](beam_engine::ExecutionContext::call_bif)/
//! [`call_bif_in_machine`](beam_engine::ExecutionContext::call_bif_in_machine) and
//! [`resolve_external_call`](beam_engine::ExecutionContext::resolve_external_call)/
//! [`resolve_local_call`](beam_engine::ExecutionContext::resolve_local_call)). Before it a
//! native baseline-JIT block crossed neither the interpreter's per-instruction `reduce` hook nor,
//! on a BIF-and-call-only loop, a heap-admission point — so such a run charged **nothing** and
//! `erlang:statistics(reductions)` / the process's own counter stayed at `0`. This suite pins the
//! three facts that follow, all from deterministic counters (never wall clock — the host is
//! shared and heavily loaded, so only counts and ratios are read).
//!
//! # What was measured (`scripts/cargo.sh test -p beam_rt`, `loadavg` 3.5–26 on this host —
//! every figure below was reproduced byte-for-byte across that whole range, which is the point of
//! reading counters instead of timings)
//!
//! `native_reds:heap/1` (`tests/fixtures/native_reds.erl`) is `reds_boundary`'s BIF-and-call-only
//! loop — each iteration allocates only through registered BIFs
//! (`list_to_tuple(lists:seq(1, 400))`), so the module contains **no** `test_heap`/
//! `allocate_heap` at all (`erlc -S`: `allocate`/`call_ext`/`call_ext`/`gc_bif`/`call_last`) —
//! plus one in-process read of the running collector's `heap_size` after the loop, which is the
//! only way to observe a shape that never ends a slice (see the residual below).
//!
//! | tier | N | slices | yields | waits | collections | major | peak_young_spaces | reductions | heap_size |
//! |---|---|---|---|---|---|---|---|---|---|
//! | native (default) | 10 | 1 | 0 | 0 | 0 | 0 | 0 | 55 | 28 309 |
//! | native | 100 | 1 | 0 | 0 | 0 | 0 | 0 | 505 | 196 267 |
//! | native | 400 | 1 | 0 | 0 | 0 | 0 | 0 | 2 005 | 514 454 |
//! | native | 2 000 | 1 | 0 | 0 | 0 | 0 | 0 | 4 000 | 3 074 841 |
//! | native | 4 000 | 1 | 0 | 0 | 0 | 0 | 0 | 4 000 | 6 656 694 |
//! | interpreter | 2 000 | 10 | 9 | 0 | 9 | 1 | 1 | 36 018 | 233 |
//!
//! (A native row's `peak_young_spaces` is `0` for the same reason its `heap_size` is only
//! visible from *inside* the process: the boundary sampler sees a live collector only between
//! slices, and this shape never ends one.)
//!
//! `heap_growth:run/1` (the allocating engine-native shape) on the same default native tier:
//!
//! | N | slices | yields | waits | collections | major | peak_young_spaces | reductions |
//! |---|---|---|---|---|---|---|---|
//! | 20 000 | 15 | 14 | 0 | 20 014 | 1 | 1 | 59 974 |
//! | 80 000 | 60 | 59 | 0 | 80 059 | 2 | 1 | 239 884 |
//!
//! # The three pins
//!
//! 1. **The charge really happens on the native tier, and it is `min(price, budget)`.** The loop
//!    prices **exactly 5 reduction units per iteration** (plus a constant 5 for the entry/exit
//!    seams): `reductions == 5N + 5` at N = 10, 100 and 400. Past the slice budget the charge
//!    saturates at [`DEFAULT_REDUCTIONS_PER_SLICE`] exactly (`4 000` at N = 2 000 / 4 000, and
//!    measured too at 800 / 1 000 / 8 000) — a `spend` never refuses, so the probe stops counting
//!    once its slice is spent. **The linear form is therefore only observable below the budget**:
//!    a native run of this shape is one slice long (`slices == 1`), so the reported total cannot
//!    grow past the budget however large N is. The pre-change figure for that same run was **0**
//!    (`spend` did not exist), which is what makes "non-zero, and 5 per iteration" the pin —
//!    verified by knocking `spend`'s body out in the working tree: the same rows then report
//!    `reductions = 0` and this test fails on that first assertion.
//! 2. **A native loop that reaches a refusing seam still ends its slice.** `heap_growth` reaches
//!    the heap-admission door (`test_heap` → `ensure_heap_full`, a refusing `charge`) on every
//!    iteration, so the budget is what ends each slice: `yields + 1 == slices` (every slice but
//!    the last ends in the `Yield` arm), `slices == ceil(reductions / 4 000)` (no slice ends with
//!    budget left except the last, which simply runs out of work), and the shape holds a bounded
//!    **1 333.3 iterations per slice** at both N — derived, not calibrated: 3 charged units per
//!    iteration over a 4 000-unit slice. `yields` and `collections` scale linearly with N (their
//!    per-call ratios are flat over a 4x step), so the pre-emption rate does not grow with N.
//! 3. **The honest residual: a non-allocating BIF-and-call shape still ends ZERO slices on the
//!    native tier.** `slices == 1 && yields == 0 && collections == 0` at every `native_reds` row,
//!    so its `heap_size` still tracks N — ≈ 1 537 / 1 664 retained words per iteration at
//!    N = 2 000 / 4 000 (a real collection leaves the collector's 233-word floor, which is what
//!    the forced-interpreter control run reads). The residual is a `beam_engine` handoff, not a
//!    `beam_rt` bug: a `spend` at a call seam can only drain the budget, and the *refusal* that
//!    would end the slice cannot be taken there, because the native tier publishes
//!    `machine.current_code` at **block** granularity (`jit/baseline/runtime.rs:993`
//!    `jit_set_position`) and its local-call paths consult the seam *before* storing a position
//!    of their own (`jit/baseline/call.rs:210` seam vs `:232` store; `runtime.rs:948`
//!    `probe_local_call` stores nothing; `runtime.rs:818` `resolve_ext_entry`). A **refusing**
//!    charge at those seams therefore parks the runner at the block entry, and the resume
//!    re-executes the block prefix — measured with the draft's `charge()?` verbatim:
//!    `beam_rt -eval 'halt(0).'` and `beam_rt run slice.beam` both exited 3 with
//!    `beam_rt: unsupported: opcode: run: process blocked (no message available)`. Closing it
//!    needs the instruction-exact store at those three `beam_engine` sites; **when that lands,
//!    this third pin is the assertion that must change** (the shape should then yield at the
//!    budget, `yields > 0`, and the `heap_size` band below should collapse to the floor).
//!
//! The interpreter control is the same fixture under [`JitMode::Interpreter`] (forced per thread
//! through [`super::super::tier::force`], never through the process environment): it yields
//! (`collections == yields == 9` at N = 2 000, the boundary door) and its `heap_size` sits on the
//! collector's floor — so pin 3 is a fact about the **tier**, not about the fixture.
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
/// `reductions == 5N + 5`.
///
/// The split is not observable from the counters alone; it is *consistent* with the loop body's
/// three seam consultations (two `call_ext` — `lists:seq/2` and `list_to_tuple/1`, the latter a
/// BIF dispatch — plus its local `call_last loop/1`) and `lists:seq/2`'s own two resolutions
/// (`seq/3` and `seq_1/3`). What the pin asserts is the count, not the decomposition.
const NATIVE_REDS_UNITS_PER_ITERATION: u64 = 5;

/// The constant the loop's entry/exit seams add to the charge (measured: `reductions` is `5` at
/// N = 0 and `5N + 5` throughout the linear regime).
const NATIVE_REDS_ENTRY_UNITS: u64 = 9;

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

/// TEMPORARY measurement ladder (removed before commit).
#[test]
fn zz_measure_ladder() {
    for n in [
        790i64, 795, 796, 797, 798, 799, 800, 801, 900, 2_000, 4_000, 20_000,
    ] {
        native_reds_row(n, "ladder");
    }
    for n in [2_000i64, 10_000, 20_000, 80_000] {
        let mut rt = growth_runtime(super::heap_growth_fixture());
        let row = run_growth_function(&mut rt, "heap_growth", "run", n);
        report("heap_growth", "ladder", n, &row);
    }
    let _tier = InterpreterTier::new();
    for n in [798i64, 2_000] {
        native_reds_row(n, "ladder-interp");
    }
}

/// **The slice budget really is spent by native-tier work, a refusing seam still ends the slice,
/// and the shape that reaches none is still never pre-empted.**
///
/// See the module docs for the measured table, the derivation of every bound and the
/// `beam_engine` handoff that the third pin will need when it lands.
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

    // ---- 1. the charge, on the native tier ------------------------------------------------
    // The linear regime, below the slice budget: the reported reduction total is the loop's own
    // price — 5 units per iteration plus the entry/exit constant. (Before commit 4968e89402 the
    // same runs reported 0: no seam this shape reaches charged anything.)
    let linear: Vec<(i64, GrowthCounters)> = [10i64, 100, 400]
        .into_iter()
        .map(|n| (n, native_reds_row(n, "linear")))
        .collect();
    for &(n, row) in &linear {
        assert_eq!(
            row.reductions,
            NATIVE_REDS_UNITS_PER_ITERATION * (n as u64) + NATIVE_REDS_ENTRY_UNITS,
            "N={n}: the native tier must charge 5 units per iteration + 5 for the entry/exit \
             seams, got {}: {row:?}",
            row.reductions
        );
    }
    // ... and it is a *price per iteration*, so it grows linearly in N: 10x the iterations is
    // 10x the charge once the shared constant is taken out.
    let priced = |row: &GrowthCounters| row.reductions - NATIVE_REDS_ENTRY_UNITS;
    assert_eq!(
        priced(&linear[2].1),
        4 * priced(&linear[1].1),
        "the charge must scale linearly with N (400 iterations = 4x 100): {linear:?}"
    );
    assert_eq!(
        priced(&linear[1].1),
        10 * priced(&linear[0].1),
        "the charge must scale linearly with N (100 iterations = 10x 10): {linear:?}"
    );

    // The saturated regime: a `spend` never refuses, so past the budget the total is exactly the
    // slice's budget — not N, and never more than the budget — for every larger N. These are
    // also the residual's two rows (pin 3), so they are measured once.
    let saturated: Vec<(i64, GrowthCounters)> = [2_000i64, 4_000]
        .into_iter()
        .map(|n| (n, native_reds_row(n, "saturated/residual")))
        .collect();
    for &(n, row) in &saturated {
        assert_eq!(
            row.reductions,
            u64::from(DEFAULT_REDUCTIONS_PER_SLICE),
            "N={n}: a big N must charge exactly the slice's budget \
             ({DEFAULT_REDUCTIONS_PER_SLICE}) and no more, got {}: {row:?}",
            row.reductions
        );
    }
    assert_eq!(
        saturated[0].1.reductions, saturated[1].1.reductions,
        "the saturated charge cannot track N (one slice = one budget): {saturated:?}"
    );

    // ---- 2. the refusing seam: a native allocating loop ends its slice --------------------
    let mut growth_rows = Vec::new();
    for n in [20_000i64, 80_000] {
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
    let [(n1, g1), (n2, g2)] = [growth_rows[0], growth_rows[1]];
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
        let slices_from_budget = (row.reductions + u64::from(DEFAULT_REDUCTIONS_PER_SLICE) - 1)
            / u64::from(DEFAULT_REDUCTIONS_PER_SLICE);
        assert_eq!(
            row.slices as u64, slices_from_budget,
            "the budget must be what ends each slice: N={n} {row:?}"
        );
        // A bounded number of iterations per slice — 4 000 charged units over the shape's 3
        // units per iteration, derived from the measured rows rather than calibrated.
        let iterations_per_slice = n as f64 / row.slices as f64;
        assert!(
            (1_333.0..=1_334.0).contains(&iterations_per_slice),
            "a 4 000-unit slice must hold ~1 333 iterations of a 3-unit iteration: N={n} \
             {iterations_per_slice:.4}/slice"
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

    // ---- 3. the honest residual ----------------------------------------------------------
    // The non-allocating BIF-and-call shape ends **no** slice on the native tier, so nothing
    // collects it and its `heap_size` still tracks N (the rows measured above). This is the
    // KNOWN RESIDUAL: the `spend` charges drain the budget (pin 1) but a refusal cannot be taken
    // at a call seam until `beam_engine` stores the instruction address before consulting it (see
    // the module docs). When that handoff lands this assertion is the one that must change: the
    // shape will then yield at the budget (yields > 0), collect at the boundary, and the
    // `heap_size` band below must collapse to the collector's floor.
    let [(rn1, r1), (rn2, r2)] = [saturated[0], saturated[1]];
    for &(n, row) in &[(rn1, r1), (rn2, r2)] {
        assert_eq!(
            (
                row.slices,
                row.yields,
                row.waits,
                row.collections,
                row.major
            ),
            (1, 0, 0, 0, 0),
            "the residual: a BIF-and-call-only shape must still end ZERO slices on the native \
             tier (one slice, no pre-emption, nothing collected): N={n} {row:?}"
        );
        // The consequence: the collector's chain capacity tracks N at ~1.5k words per iteration
        // (a collected shape sits on the 233-word floor — the control row below).
        let retained_per_iteration = row.value as f64 / n as f64;
        assert!(
            (1_400.0..=1_800.0).contains(&retained_per_iteration),
            "N={n}: an uncollected run retains ~1 537-1 664 words per iteration, got \
             {retained_per_iteration:.1} (heap_size {}): {row:?}",
            row.value
        );
    }
    assert!(
        r2.value > r1.value,
        "heap_size must track N while nothing collects: {} vs {}",
        r1.value,
        r2.value
    );
    let heap_ratio = (r2.value as f64 / rn2 as f64) / (r1.value as f64 / rn1 as f64);
    assert!(
        heap_ratio <= 1.5,
        "heap_size/N must be flat in N (a linear climb at a constant rate, not a blow-up): \
         {heap_ratio:.3}"
    );

    // ---- the control: the same fixture on the interpreter **is** pre-empted and collected ---
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
    assert!(
        control.value <= 610,
        "the collected shape's heap_size must sit on the collector's floor (measured 233, the \
         boundary peak 610), not track N: {control:?}"
    );
    assert!(
        r2.value > 10 * control.value,
        "the residual is the tier's: the uncollected native heap ({}) must dwarf the collected \
         interpreter heap ({})",
        r2.value,
        control.value
    );
}
