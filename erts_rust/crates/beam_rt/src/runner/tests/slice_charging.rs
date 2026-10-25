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
//! A compiled baseline-JIT block crosses neither the interpreter's per-instruction
//! [`reduce`](beam_engine::ExecutionContext::reduce) hook nor — on a BIF-and-call-only loop — a
//! heap-admission point, so on that tier the budget is drained by the seams the block **does**
//! reach, in two kinds:
//!
//! * **Refusing** charges ([`BudgetProbe::charge`](super::super::budget::BudgetProbe)): one per
//!   driver-loop iteration (a block entry, where `Machine::current_code` is published — see
//!   `jit/baseline/mod.rs`'s loop head), one per **in-block transfer** (the linked
//!   branch/tail-call that replaces a driver round-trip reproduces the skipped iteration's
//!   charge — `jit/baseline/runtime.rs`'s `resume_in_block`), and the two heap-admission doors
//!   (`ensure_heap`/`ensure_heap_full`).
//! * **Spending**, never refusing, charges ([`BudgetProbe::spend`](super::super::budget::BudgetProbe)):
//!   one per call-resolution and per BIF dispatch — the two call seams
//!   ([`resolve_external_call`](beam_engine::ExecutionContext::resolve_external_call),
//!   [`resolve_local_call`](beam_engine::ExecutionContext::resolve_local_call)) and the BIF
//!   doors ([`call_bif`](beam_engine::ExecutionContext::call_bif)/
//!   [`call_bif_in_machine`](beam_engine::ExecutionContext::call_bif_in_machine) and their
//!   outcome-aware forms).
//!
//! **A same-module local call is priced once per distinct callee per run, never per call.** The
//! engine memoizes the host's local-call verdict on the machine
//! (`Machine::local_call_enters_in_machine`, backed by `LocalVerdicts`), because the verdict for
//! a fixed `(module, function, arity)` triple is a pure function of that triple and ERTS itself
//! charges nothing for an intra-module call. The seam behind it is therefore consulted on the
//! run's **first** call to a callee and answered from the table afterwards, so a hot local
//! recursion contributes nothing per iteration — which is why the loop bodies measured below
//! price exactly the seams they reach, and no more.
//!
//! # What is measured (`erts_rust/scripts/cargo.sh test -p beam_rt`, counters only, never
//! # timings: the host is shared and heavily loaded)
//!
//! Every figure a pin reads is reproduced byte-for-byte by the run that pins it and printed by
//! [`report`]; a figure no assertion reads is quoted as *measured*.
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
//! | native | 10 | 1 | 0 | 0 | 0 | 0 | 0 | 49 | 22 148 |
//! | native | 100 | 1 | 0 | 0 | 0 | 0 | 0 | 409 | 190 104 |
//! | native | 400 | 1 | 0 | 0 | 0 | 0 | 0 | 1 609 | 508 290 |
//! | native | 700 | 1 | 0 | 0 | 0 | 0 | 0 | 2 809 | 1 341 315 |
//! | native | 997 | 1 | 0 | 0 | 0 | 0 | 0 | 3 997 | 1 341 315 |
//! | native | 998 | 1 | 0 | 0 | 0 | 0 | 0 | 4 000 | 1 341 315 |
//! | native | 999 | 2 | 1 | 0 | 1 | 0 | 1 | 4 004 | 233 |
//! | native | 2 000 | 2 | 1 | 0 | 1 | 0 | 1 | 4 006 | 233 |
//! | native | 4 000 | 2 | 1 | 0 | 1 | 0 | 1 | 4 006 | 233 |
//! | native | 5 000 | 2 | 1 | 0 | 1 | 0 | 1 | 4 006 | 233 |
//! | native | 20 000 | 2 | 1 | 0 | 1 | 0 | 1 | 4 006 | 233 |
//! | native | 80 000 | 2 | 1 | 0 | 1 | 0 | 1 | 4 006 | 233 |
//! | interpreter | 798 | 4 | 3 | 0 | 3 | 1 | 1 | 13 588 | 117 344 |
//! | interpreter | 2 000 | 9 | 8 | 0 | 8 | 1 | 1 | 34 020 | 190 104 |
//! | interpreter | 20 000 | 85 | 84 | 0 | 84 | 3 | 1 | 339 968 | 312 394 |
//!
//! (A row's `heap_size` is the capacity the nursery held when the loop returned, so on the
//! interpreter tier it is that row's own phase-dependent figure — bounded, never a function of
//! the work, see the interpreter control at the end of the test.)
//!
//! `heap_growth:run/1` (the allocating engine-native shape — one 401-word `test_heap` per
//! iteration through `sprawl/1`) on the same default native tier:
//!
//! | N | slices | yields | waits | collections | major | peak_young_spaces | nursery capacity | reductions |
//! |---|---|---|---|---|---|---|---|---|
//! | 1 | 1 | 0 | 0 | 1 | 0 | 0 | — | 8 |
//! | 2 | 1 | 0 | 0 | 2 | 0 | 0 | — | 11 |
//! | 5 | 1 | 0 | 0 | 3 | 0 | 0 | — | 20 |
//! | 10 | 1 | 0 | 0 | 6 | 0 | 0 | — | 35 |
//! | 2 000 | 2 | 1 | 0 | 1 001 | 0 | 1 | 843 | 6 005 |
//! | 3 000 | 3 | 2 | 0 | 1 501 | 0 | 1 | 843 | 9 008 |
//! | 5 000 | 4 | 3 | 0 | 2 501 | 0 | 1 | 843 | 15 008 |
//! | 20 000 | 16 | 15 | 0 | 10 001 | 1 | 1 | 987 | 60 020 |
//! | 80 000 | 61 | 60 | 0 | 40 001 | 1 | 1 | 987 | 240 065 |
//!
//! (A row that finishes inside its first slice has no slice boundary, so its nursery capacity is
//! never sampled — the sampler reads a collector only between slices — hence the `—`. Its
//! `collections` follow the same `1 + N/K` cadence as the sampled rows, with `K` derived from
//! the capacity the sampled rows measure.)
//!
//! # The four pins
//!
//! 1. **The charge really happens on the native tier, and below the budget it is an exact price
//!    per iteration: `reductions == 4N + 9`.** Measured at N = 10 / 100 / 400 / 700 / 997, and
//!    N = 0 pins the constant alone. The loop prices **4 units per iteration**; the run's own
//!    entry/epilogue seams add a constant **9**. The per-iteration 4 is exactly the seams the
//!    body reaches — two `call_ext` resolutions (`lists:seq/2`, `erlang:list_to_tuple/1`) and
//!    the two BIF dispatches they resolve to — as a per-seam trace of the charge stream shows;
//!    the body's local `call_last loop/1` prices **nothing** after the run's first consultation
//!    of it (the memoized verdict described above), and the driver's block-entry /
//!    in-block-transfer charges fall once per run's block crossing, not per iteration. The
//!    constant 9 is the run's entry/epilogue seams (the two block charges, the two in-block
//!    transfers, the two memoized local-call consultations `loop/1` and `heap_size/0`, and the
//!    seams of the `heap_size/0` report itself); the counters expose only its **sum**, which is
//!    what the pin asserts. The linear form is exact while the total stays under one slice's
//!    budget ([`DEFAULT_REDUCTIONS_PER_SLICE`] = 4 000), and `priced(N) = reductions − 9` is
//!    then an exact multiple of the per-iteration price: `priced(400) == 4·priced(100)` and
//!    `priced(100) == 10·priced(10)` are asserted as equalities. The pin is non-vacuous by
//!    construction: with `spend`'s body knocked out every row reports `reductions = 0` and the
//!    first assertion fails.
//! 2. **The budget boundary is derived, not calibrated.** With `P = 4` the derivation
//!    `floor((4 000 − 9) / 4) = 997` is the largest N whose total is **strictly** under the
//!    budget, and it leaves the integer-division remainder `3 = (4 000 − 9) mod 4` units unspent
//!    — fewer than one more iteration's price, so one more iteration would break the slice. The
//!    **crossing** row (N = 998, linear total 4 001) is the one row that would exceed the budget
//!    and still completes in a single slice: the single unit that lands past the budget is a
//!    `spend`, which is a silent no-op on an exhausted probe, and no refusing charge follows it —
//!    so the row reports exactly the budget (4 000, one unit clipped) and one slice. The first
//!    row that needs a second slice is therefore **999**: its over-charge is `1 + P` units, wide
//!    enough to contain the block entry's refusing charge. All three rows are measured, so a
//!    change to the per-iteration price or to the budget that moved the boundary has to be
//!    re-derived here rather than silently accepted.
//! 3. **A native loop that reaches a refusing seam still ends its slice.** `heap_growth` reaches
//!    the heap-admission door (`test_heap` → `ensure_heap_full`, a refusing `charge`) on every
//!    iteration — plus the two in-block transfers its body crosses — so the budget is what ends
//!    each slice: `yields + 1 == slices` (every slice but the last ends in the `Yield` arm),
//!    `slices == ceil(reductions / 4 000)` (no slice ends with budget left except the last,
//!    which simply runs out of work), and because the shape prices **3 units per iteration**
//!    (`reductions == 3N + 5` on every single-slice row) a slice holds at most `4 000 / 3 = 1 333`
//!    iterations — measured as the exact band `1 333·(slices − 1) ≤ N ≤ 1 333·slices`, both
//!    halves derived from those two laws rather than calibrated. Across slices the total is
//!    `3N + 5` plus one unit per pre-emption, within the ±1 unit `spend`'s no-op can add or
//!    remove at a boundary. Its collections are `1 + N/K`, with `K` iterations carried by the
//!    run's own settled nursery (`K` derived from the measured peak capacity and the fixture's
//!    401-word footprint, see `heap_growth_iterations_per_nursery`), and both of the capacities a
//!    row can settle on carry `K = 2` iterations: **843** words while every boundary's promotion
//!    is empty, and **987** after the row's one fullsweep has re-sized the mature region to 2 586
//!    words (holding the single 401-word iteration that did not fit). So the cadence is the same
//!    `1 + N/2` on every row (measured 1 001 / 1 501 / 2 501 / 10 001 / 40 001). Which of the two
//!    a given row settles on follows from whether any of its boundaries landed on a live
//!    iteration — a phase fact, so the mature-generation pin states the mechanism (a fullsweep is
//!    taken exactly when a boundary's promotion does not fit the region) and pins each of its two
//!    branches, and the sampled rows are asserted to exercise **both**. The pre-emption rate does
//!    not grow with N: the per-call figures are flat over a 4x step.
//!    A pre-emption ends a slice but adds no collection of its own: the N = 20 000 row takes 15
//!    boundaries and still reads exactly `1 + N/2 = 10 001` collections, so the boundary's firing
//!    is not additional to the admission cadence — which is why the same `1 + N/K` law covers the
//!    single-slice rows and the sliced ones.
//! 4. **The honest residual: this shape's own work can never end a slice.** `native_reds` prices
//!    its iterations through `spend` (a `spend` never refuses, and once the budget is gone it is a
//!    silent no-op — it does not even count), so past the budget the loop keeps running
//!    **uncharged** and its `Yield` is not the loop's at all: it is the *next refusing charge*,
//!    which the run reaches only in its epilogue. The measured consequences, all pinned as
//!    equalities:
//!    * the charge saturates at **exactly 4 006** on every sampled N from 2 000 to 80 000 (a
//!      40x span) — the first slice's whole budget (4 000) plus a constant **6** charged in the
//!      resumed slice (per-slice `BudgetProbe::used` measured with a temporary per-slice print:
//!      4 000 then 6; how the constant 9 splits into "before the loop" and "epilogue" is *not*
//!      derivable from the totals — only its sum is) — flat in N, so it can never track the work;
//!    * `slices == 2 && yields == 1` on every one of those rows, where a loop-driven pre-emption
//!      cadence would need `ceil((4N + 9) / 4 000)` slices (3 at N = 2 000, 21 at N = 20 000);
//!    * the single `Yield` *does* collect once — as a **minor**, not a fullsweep: the door pins
//!      `Process::collect_kind(CollectionKind::Minor)` first and escalates only when the live
//!      promotion cannot fit the mature region, and this shape's boundary live set is empty (its
//!      per-iteration tuples are all dead by then), so the minor is promotable and no fullsweep
//!      is taken — leaving the shape's `heap_size` on the collector's floor (233) instead of
//!      tracking N. Whether a boundary has anything to promote is a fact of the iteration phase
//!      the pre-emption lands on, not of N (the `heap_growth` rows do promote a live iteration at
//!      some of their boundaries), so pin 3 states the fullsweep **mechanism** and pins each of
//!      its two branches, never a threshold in N.
//!      The memory half of the residual has therefore closed; the
//!      **charging** half has not: closing it needs a refusing charge the loop's own seams can
//!      take, i.e. the `beam_engine` handoff that makes a call-seam refusal re-runnable (the
//!      resume address must be the seam's own instruction — see `BudgetProbe::spend`'s docs).
//!      When that lands, this fourth pin is the assertion that must change (`yields` would then
//!      track `slices − 1`).
//!
//! The interpreter control is the same fixture under [`JitMode::Interpreter`] (forced per thread
//! through [`super::super::tier::force`], never through the process environment): it charges per
//! instruction, so its total really does track the work — **34 020** units and 9 slices / 8
//! yields / 8 `collections` at N = 2 000, and 339 968 / 85 / 84 at N = 20 000, against the native
//! tier's flat 4 006 / 2 / 1. That is the residual's discriminator: the gap is a fact about the
//! **tier**, not about the fixture.
//! Its `heap_size` is the nursery capacity the last slice's growth left in place, which the
//! live-set-targeted minor rule
//! ([`HeapPolicy::young_minor_target_words`](beam_gc::HeapPolicy::young_minor_target_words))
//! bounds instead of letting it follow N: **190 104** words at N = 2 000 and **312 394** at
//! N = 20 000 — a 10x step in the work moves the reported capacity by less than 2x, and both
//! figures sit far below the 1 341 315 words the native tier's *uncollected* row reports at
//! N = 997, i.e. the figure is amortized over the pre-emptions instead of tracking the work.
//!
//! Every figure above is the **native baseline** tier's unless the row says `interpreter`, so any
//! other mode invalidates them: the control is reached by forcing the interpreter on this thread,
//! never through the process environment, and `BEAM_RT_TIER=cranelift` (with the runtime's
//! `cranelift` Cargo feature) would take every measured row on the hot tier instead — which is why
//! the test opens by asserting the native baseline selection.
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

/// The charged units one `native_reds` iteration prices — the measured constant **4** of
/// `reductions == 4N + 9`, holding at N = 10 / 100 / 400 / 700 / 997.
///
/// It is exactly the priced seams the loop body reaches: the two `call_ext` resolutions
/// (`lists:seq/2` and `erlang:list_to_tuple/1`) and the two BIF dispatches they resolve to.
/// Everything else the body contains prices nothing per iteration: the local
/// `call_last loop/1` reaches no priced seam after the run's first consultation of it (the
/// engine answers a local call from its memoized `LocalVerdicts` verdict, and the host's
/// verdict for a fixed `(module, function, arity)` triple is pure), and the driver's
/// block-entry / in-block-transfer charges are per block crossing, not per iteration. A
/// per-seam trace of the charge stream at N = 2 shows precisely that split; the pin itself
/// asserts the **count**.
const NATIVE_REDS_UNITS_PER_ITERATION: u64 = 4;

/// The constant the run's own entry/epilogue seams add to the charge (measured: `reductions` is
/// exactly `9` at N = 0, the only row whose loop body never executes, and `4N + 9` throughout
/// the linear regime N ≤ 997).
///
/// The seams behind it are the run's two block charges, the two in-block transfers, the two
/// memoized local-call consultations (`loop/1`, `heap_size/0`) and the `heap_size/0` report's
/// own priced seams. Only their sum is observable from the counters, which is what the pin
/// asserts.
const NATIVE_REDS_ENTRY_UNITS: u64 = 9;

/// What a **saturated** `native_reds` run charges beyond its first slice's budget: the constant
/// **6** the resumed slice spends in the epilogue (measured — a temporary per-slice print of
/// `BudgetProbe::used` reads `4 000` then `6`, for every N ≥ 1 000). It is *not* `ENTRY_UNITS`:
/// the loop's own entry is charged before the budget runs out, so only the epilogue's share
/// lands in the second slice. How the 9 splits between the two is not derivable from the totals
/// — only the sum is — so the split is measured, not assumed.
const NATIVE_REDS_EPILOGUE_UNITS: u64 = 6;

/// The charged units one `heap_growth:run/1` iteration prices — the measured constant **3** of
/// the single-slice law `reductions == 3N + 5` (N = 1 / 2 / 5 / 10).
///
/// Its own decomposition differs from `native_reds`': the body reaches the `test_heap`
/// admission door (a **refusing** charge) once per iteration and crosses two in-block transfers
/// (into `sprawl/1`'s block and back into `loop/2`'s), while its two local calls are memoized
/// like every other — so the price is the door plus the transfers, and no call resolution.
const HEAP_GROWTH_UNITS_PER_ITERATION: u64 = 3;

/// The constant a single-slice `heap_growth:run/1` adds to its loop's price (measured: `8` at
/// N = 1 and `3N + 5` at N = 2 / 5 / 10).
const HEAP_GROWTH_ENTRY_UNITS: u64 = 5;

/// The most a **pre-emption** can add to a multi-slice `heap_growth` run's total, beyond
/// `3N + 5 + (slices − 1)`: each boundary re-enters the interrupted block entry, and `spend`'s
/// non-refusing no-op can swallow (or the re-entry can add) one unit at that boundary — so the
/// total sits within this many units of the per-boundary derivation.
const HEAP_GROWTH_BOUNDARY_EXCURSION_UNITS: i64 = 1;

/// The heap policy a process created **without** heap options gets — the emulator defaults this
/// suite's runtimes are built with ([`HeapOptions::new`](crate::config::HeapOptions::new), the
/// `heap` field of the [`RuntimeConfig::default`](crate::config::RuntimeConfig) that
/// [`growth_runtime`] passes to `Runtime::with_defaults`), whose `min_heap_words` is the C
/// `H_DEFAULT_SIZE`.
fn emulator_default_policy() -> beam_gc::HeapPolicy {
    crate::config::HeapOptions::default().policy().clone()
}

/// The collector's floor under this runtime: the young-nursery capacity a collected process with
/// an **empty** live set settles on, i.e.
/// [`HeapPolicy::young_minor_target_words`](beam_gc::HeapPolicy::young_minor_target_words)`(0)`
/// — which is also its
/// [`nursery_floor_words`](beam_gc::HeapPolicy::nursery_floor_words), so no capacity any
/// collection installs can sit below it. It is `beam_gc::DEFAULT_MIN_HEAP_WORDS` (233) for the
/// emulator-default policy, and every saturated `native_reds` row reads it back as
/// `process_info(self(), heap_size)`.
fn collector_floor_words() -> i64 {
    emulator_default_policy().young_minor_target_words(0) as i64
}

/// The largest `NativeReds` iteration count whose **linear** total (`4N + 9`) is still strictly
/// under one slice's budget: `floor((4 000 − 9) / 4) = 997`, leaving the division's remainder of
/// 3 units — fewer than one more iteration's price. At N = 998 the linear total is 4 001 (one
/// unit past the budget, and still one slice — see the crossing row) and at N = 999 the run
/// needs a second slice — the boundary is derived from the two measured laws, never calibrated.
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

/// Runs `heap_growth:run/1` with `n` on a fresh runtime, asserting the shape's own invariants
/// (it returns its argument, never parks, never fails a collection) and printing the
/// counter-table line.
fn heap_growth_row(n: i64, label: &str) -> GrowthCounters {
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
    report("heap_growth", label, n, &row);
    row
}

/// The per-row report line: `{n, slices, yields, waits, collections, major, peak_young_spaces,
/// peak_young_capacity, peak_mature_capacity, peak_mature_used, reductions, value}` — every
/// figure the module docs' tables quote is re-derived by the very run that pins it. `value` is
/// whatever the entry returned: the heap size the process reported for itself on the
/// `native_reds` rows, the iteration count on the `heap_growth` rows.
fn report(module: &str, label: &str, n: i64, row: &GrowthCounters) {
    eprintln!(
        "slice_charging {module} [{label}] N={n} : slices={} yields={} waits={} collections={} \
         major={} peak_young_spaces={} peak_young_capacity={} peak_mature_capacity={} \
         peak_mature_used={} reductions={} value={}",
        row.slices,
        row.yields,
        row.waits,
        row.collections,
        row.major,
        row.peak_young_spaces,
        row.peak_young_capacity,
        row.peak_mature_capacity,
        row.peak_mature_used,
        row.reductions,
        row.value,
    );
}

/// **The slice budget really is spent by native-tier work, a refusing seam still ends the slice,
/// and the shape whose own seams cannot refuse saturates at a flat ceiling.**
///
/// Every figure here is the **native baseline** tier's. The interpreter control — the rows at the
/// end — is reached by forcing [`JitMode::Interpreter`] on this thread, never by the ambient
/// environment, because a process-wide non-baseline mode (the `BEAM_RT_TIER=interpreter` kill
/// switch, or `BEAM_RT_TIER=cranelift` with the runtime's `cranelift` Cargo feature) would move
/// every count in the suite; the test fails loudly instead of comparing such a run against
/// native-tier figures.
///
/// See the module docs for the measured tables, the derivation of every law, and the
/// `beam_engine` handoff that the fourth pin will need when it lands.
#[test]
fn the_slice_budget_prices_native_work_and_a_refusing_seam_still_ends_the_slice() {
    // The measurements below are the **native baseline** tier's. `BEAM_RT_TIER=interpreter` (or
    // any unknown value) pins the reference tier process-wide, and `BEAM_RT_TIER=cranelift`
    // (feature enabled) would take every measured row on the hot tier — either way every count in
    // this suite moves, so the precondition is that the environment default is one of the two
    // native baseline selections (`Auto`, the unset default, and its `Baseline` alias). That is
    // also why the interpreter control rows force their mode per thread
    // (`InterpreterTier`) instead of through the process environment. Fail loudly rather than
    // compare a non-baseline run against native-baseline figures.
    assert!(
        matches!(
            super::super::tier::env_default(),
            JitMode::Auto | JitMode::Baseline
        ),
        "this suite measures the native baseline tier; BEAM_RT_TIER must be unset/auto/baseline, \
         got {:?}",
        super::super::tier::env_default()
    );
    let budget = u64::from(DEFAULT_REDUCTIONS_PER_SLICE);
    // The floor every collected row's own `heap_size` report sits on, derived from the policy the
    // spawned processes are actually created with (never a calibrated literal).
    let collector_floor = collector_floor_words();
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

    // The linear regime, strictly below the slice budget: the reported total is the loop's own
    // price — 4 units per iteration plus the constant.
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
        // collected it — which is why its self-reported `heap_size` still carries the grown
        // (uncollected) nursery here, the control for pin 4's collapse to the floor.
        assert_eq!(
            (row.slices, row.yields, row.waits, row.collections),
            (1, 0, 0, 0),
            "N={n}: a total under one slice's budget must fit in one slice and collect nothing: \
             {row:?}"
        );
        assert!(
            row.value > 1_000,
            "N={n}: an uncollected run's heap_size must still be the grown retail chain (the \
             collector's floor is {collector_floor}), got {}: {row:?}",
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
    // The derivation is `floor((budget - ENTRY) / P)`: the largest N whose total is strictly
    // under the budget. What it leaves unspent is the division's remainder — fewer than one more
    // iteration's price, which is what makes the row the LAST one that fits.
    assert_eq!(last_linear, 997, "the derivation must yield 997");
    let last_linear_total = NATIVE_REDS_UNITS_PER_ITERATION * (last_linear as u64)
        + NATIVE_REDS_ENTRY_UNITS;
    let leftover = budget - last_linear_total;
    assert_eq!(
        leftover,
        (budget - NATIVE_REDS_ENTRY_UNITS) % NATIVE_REDS_UNITS_PER_ITERATION,
        "the unspent units are the derivation's own remainder"
    );
    assert!(
        last_linear_total <= budget,
        "the last linear row must still fit in one slice ({last_linear_total} vs {budget})"
    );
    assert!(
        leftover < NATIVE_REDS_UNITS_PER_ITERATION,
        "and one more iteration must break the slice: {leftover} units are left, an iteration \
         prices {NATIVE_REDS_UNITS_PER_ITERATION}"
    );

    // The **crossing** row: the smallest N whose linear total exceeds the budget by
    // `P - leftover` units. That single unit is a `spend`, which never refuses, so the run keeps
    // going uncharged past the budget and completes inside its first slice with the clipped
    // charge and no collection.
    let crossing = last_linear + 1;
    assert_eq!(
        NATIVE_REDS_UNITS_PER_ITERATION * (crossing as u64) + NATIVE_REDS_ENTRY_UNITS,
        budget + (NATIVE_REDS_UNITS_PER_ITERATION - leftover),
        "the crossing row is one iteration past the last linear one"
    );
    let crossing_row = native_reds_row(crossing, "crossing");
    assert_eq!(
        crossing_row.reductions, budget,
        "N={crossing}: the unit that lands past the budget is a `spend`, which is a silent no-op \
         on an exhausted probe, so the charge stops at the budget itself: {crossing_row:?}"
    );
    assert_eq!(
        (
            crossing_row.slices,
            crossing_row.yields,
            crossing_row.collections
        ),
        (1, 0, 0),
        "N={crossing}: a `spend` never refuses, so no refusing charge follows the exhaustion and \
         the run still ends inside its first slice: {crossing_row:?}"
    );
    assert!(
        crossing_row.value > collector_floor,
        "N={crossing}: nothing collected it, so its nursery is still the grown chain (the floor \
         is {collector_floor}), got {}: {crossing_row:?}",
        crossing_row.value
    );

    // One iteration further the over-charge is `1 + P` units — wide enough to contain the block
    // entry's refusing charge — so that row needs a second slice. The `+1` is the crossing row's
    // one swallowed unit, possible exactly when the derivation's remainder is `P - 1`.
    let first_two_slice = crossing + i64::from(leftover == NATIVE_REDS_UNITS_PER_ITERATION - 1);
    assert_eq!(first_two_slice, 999, "the derivation must yield 999");
    let breaking = native_reds_row(first_two_slice, "boundary");
    assert_eq!(
        breaking.reductions,
        NATIVE_REDS_UNITS_PER_ITERATION * (first_two_slice as u64) + NATIVE_REDS_ENTRY_UNITS - 1,
        "N={first_two_slice}: one iteration past the crossing row, the total is the linear total \
         less the single unit `spend` swallows at the exhausted budget: {breaking:?}"
    );
    assert_eq!(
        (breaking.slices, breaking.yields, breaking.waits, breaking.collections),
        (2, 1, 0, 1),
        "N={first_two_slice}: the first N past the crossing row must need a second slice (and \
         collect once at the yield): {breaking:?}"
    );
    assert_eq!(
        breaking.value, collector_floor,
        "N={first_two_slice}: that boundary collection collapses the retired chain, leaving the \
         collector's floor ({collector_floor}): {breaking:?}"
    );

    // ---- 3. the saturated regime: a flat ceiling, and ONE yield that is not the loop's ------
    // These are pin 4's rows. Two things are pinned together: the ceiling (the first slice's
    // whole budget plus the epilogue's measured 6) and the *single* pre-emption — a loop-driven
    // cadence would need `ceil((4N + 9) / budget)` slices, which the later assertion derives.
    let saturated: Vec<(i64, GrowthCounters)> = [2_000i64, 4_000, 5_000, 20_000, 80_000]
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
            (2, 1, 0, 1, 0, 1),
            "N={n}: exactly one pre-emption and one boundary collection, whatever N is — and that \
             collection is a minor (`major == 0`): the door pins `CollectionKind::Minor` first \
             and only escalates when the live promotion cannot fit the mature region, and this \
             shape's boundary live set is empty, so it fits the 233-word initial region: {row:?}"
        );
        // The two mature figures say the same thing from the collector's side: the region is
        // still the one a spawn policy installs (`initial_mature_words`, never re-sized, which
        // only a fullsweep does) and nothing was ever promoted into it.
        assert_eq!(
            (row.peak_mature_capacity, row.peak_mature_used),
            (emulator_default_policy().initial_mature_words, 0),
            "N={n}: `major == 0` is not an accident of the counter — the mature region is still \
             the spawn policy's initial capacity (only a fullsweep re-sizes it) and holds \
             nothing (the boundary's live promotion is empty): {row:?}"
        );
        // The one boundary collection collapses the run's whole retired chain, so — unlike the
        // uncollected crossing row — the heap now sits on the collector's floor instead of
        // tracking N.
        assert_eq!(
            row.value, collector_floor,
            "N={n}: the single boundary collection must leave the collector's floor \
             ({collector_floor}), got {}: {row:?}",
            row.value
        );
    }
    assert_eq!(
        saturated[0].1.reductions, saturated[4].1.reductions,
        "the saturated charge cannot track N (a flat ceiling, not a cadence): {saturated:?}"
    );
    // The cadence the loop's own work *would* demand if its seams could refuse: the measured
    // slice count is 2 on every saturated row, i.e. 10x below the cadence at N = 20 000.
    let cadence = |n: i64| {
        (NATIVE_REDS_UNITS_PER_ITERATION * (n as u64) + NATIVE_REDS_ENTRY_UNITS + budget - 1)
            / budget
    };
    assert_eq!(cadence(2_000), 3, "the derivation of the cadence at N = 2 000");
    assert_eq!(cadence(20_000), 21, "the derivation of the cadence at N = 20 000");
    assert!(
        saturated[3].1.slices as u64 * 10 <= cadence(20_000),
        "N=20 000: the measured slice count ({}) must be an order of magnitude below the \
         loop-driven cadence ({}) — that gap IS the residual: {saturated:?}",
        saturated[3].1.slices,
        cadence(20_000)
    );

    // ---- 4. the refusing seam: a native allocating loop ends its slice --------------------
    // Single-slice rows pin the shape's own price exactly: the admission door (a refusing
    // charge) plus the body's two in-block transfers, with its local calls memoized.
    let short_rows: Vec<(i64, GrowthCounters)> = [1i64, 2, 5, 10]
        .into_iter()
        .map(|n| (n, heap_growth_row(n, "refusing seam")))
        .collect();
    for &(n, ref row) in &short_rows {
        assert_eq!(
            row.reductions,
            HEAP_GROWTH_UNITS_PER_ITERATION * (n as u64) + HEAP_GROWTH_ENTRY_UNITS,
            "N={n}: the allocating loop must price {} units per iteration + a constant {}: {row:?}",
            HEAP_GROWTH_UNITS_PER_ITERATION,
            HEAP_GROWTH_ENTRY_UNITS
        );
        assert_eq!(
            (row.slices, row.yields, row.collections > 0),
            (1, 0, true),
            "N={n}: an iteration is one 401-word admission request, so the run collects even \
             when it never leaves its first slice: {row:?}"
        );
    }
    let growth_rows: Vec<(i64, GrowthCounters)> = [2_000i64, 3_000, 5_000, 20_000, 80_000]
        .into_iter()
        .map(|n| (n, heap_growth_row(n, "refusing seam")))
        .collect();
    // The iterations a settled nursery carries (`K`), derived from the measured peak capacity on
    // every row that has a sampled boundary — the single-slice rows finish before any sample, so
    // they are checked against the same `K` afterwards.
    let mut settled_k = 0u64;
    for &(n, ref row) in &growth_rows {
        let k = super::heap_growth_iterations_per_nursery(row.peak_young_capacity);
        if settled_k == 0 {
            settled_k = k;
        }
        assert_eq!(
            k, settled_k,
            "N={n}: every row must settle on the same iterations-per-nursery, derived from its \
             own measured capacity {}: {row:?}",
            row.peak_young_capacity
        );
        assert_eq!(
            k, 2,
            "N={n}: a {}-word settled nursery must carry two {}-word iterations (measured on \
             every row): {row:?}",
            row.peak_young_capacity,
            super::HEAP_GROWTH_ITERATION_WORDS
        );
    }
    for &(n, ref row) in short_rows.iter().chain(growth_rows.iter()) {
        assert_eq!(
            row.collections,
            super::heap_growth_expected_collections(n, settled_k),
            "N={n}: collections must be 1 + N/K = 1 + {n}/{settled_k} = {} (one per {settled_k} \
             iterations plus the one before the first): {row:?}",
            super::heap_growth_expected_collections(n, settled_k)
        );
    }
    let per_slice = budget / HEAP_GROWTH_UNITS_PER_ITERATION;
    for &(n, ref row) in &growth_rows {
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
        // The total is the loop's price plus one unit per pre-emption, within the excursion a
        // boundary's `spend` can add or swallow.
        let derived = HEAP_GROWTH_UNITS_PER_ITERATION * (n as u64)
            + HEAP_GROWTH_ENTRY_UNITS
            + (row.slices as u64 - 1);
        let excursion = row.reductions as i64 - derived as i64;
        assert!(
            excursion.abs() <= HEAP_GROWTH_BOUNDARY_EXCURSION_UNITS,
            "N={n}: the total must be the loop's price plus one unit per pre-emption ({derived}), \
             within {} unit(s), measured {excursion} off: {row:?}",
            HEAP_GROWTH_BOUNDARY_EXCURSION_UNITS
        );
        // A bounded number of iterations per slice, *derived*: a slice holds `budget / 3` = 1 333
        // priced iterations, so N lies between `per_slice*(slices-1)` and `per_slice*slices` —
        // and the last slice's tail drags the average down to at least
        // `per_slice*(slices-1)/slices`.
        assert!(
            n as u64 <= per_slice * row.slices as u64
                && n as u64 >= per_slice * (row.slices as u64 - 1),
            "a {budget}-unit slice must hold between {per_slice}*(slices-1) and {per_slice}*slices \
             iterations of a {HEAP_GROWTH_UNITS_PER_ITERATION}-unit iteration: N={n} {row:?}"
        );
        let average = n as f64 / row.slices as f64;
        let floor_average = per_slice as f64 * (row.slices as f64 - 1.0) / row.slices as f64;
        assert!(
            (floor_average..=per_slice as f64).contains(&average),
            "N={n}: measured iterations/slice {average} must lie in \
             [{floor_average}, {per_slice}] — the arithmetic minimum (every slice full except \
             the last) and the per-slice capacity: {row:?}"
        );
        // The mature generation. A fullsweep is taken **exactly when** a boundary minor's
        // promotion does not fit the mature region, so the counter has two possible values and
        // each names what the region must look like:
        //   * none taken (the boundary had nothing live to promote, or its promotion fit): the
        //     region is still the one the spawn policy installed and nothing stands in it;
        //   * one taken: it re-sized the region, and the promotion it could not fit is now
        //     resident — exactly the fixture's one-iteration footprint.
        // At most one, at any N: the fullsweep re-sizes the region to hold what it promotes, so no
        // later boundary needs another (measured on the 80 000-iteration row, 40 001 collections
        // in). Which branch a row takes is decided by the iteration phase its boundaries landed
        // on — not by N — so both branches are pinned, and the sampled rows must hit both.
        assert!(
            row.major <= 1,
            "N={n}: a fullsweep re-sizes the mature region, so no boundary after it can need \
             another: {row:?}"
        );
        if row.major == 0 {
            assert_eq!(
                (row.peak_mature_capacity, row.peak_mature_used),
                (emulator_default_policy().initial_mature_words, 0),
                "N={n}: without a fullsweep the region is the spawn policy's and holds nothing — \
                 every promotion that reached it fitted (here: none was live): {row:?}"
            );
        } else {
            assert_eq!(
                row.peak_mature_used,
                super::HEAP_GROWTH_ITERATION_WORDS,
                "N={n}: the fullsweep's cause is the promotion that did not fit, so the region \
                 now holds exactly one {}-word iteration: {row:?}",
                super::HEAP_GROWTH_ITERATION_WORDS
            );
            assert!(
                row.peak_mature_capacity > emulator_default_policy().initial_mature_words,
                "N={n}: a fullsweep re-sizes the region, so its capacity must be above the spawn \
                 policy's {}: {row:?}",
                emulator_default_policy().initial_mature_words
            );
            assert!(
                row.peak_mature_capacity >= row.peak_mature_used,
                "N={n}: the re-sized region must hold what it was re-sized for: {row:?}"
            );
        }
    }
    // The pin above is only meaningful if both of the mechanism's branches are exercised: the
    // sampled rows must contain a run whose boundaries never promoted a live iteration and one
    // whose fullsweep was forced by a promotion that did not fit.
    assert!(
        growth_rows.iter().any(|(_, r)| r.major == 0) && growth_rows.iter().any(|(_, r)| r.major == 1),
        "both branches of the fullsweep mechanism must be sampled: {growth_rows:?}"
    );
    // ... and the pre-emptions and the collections both scale **linearly** with N: over a 4x
    // step their per-call figures are flat (the ratio only ever falls, because a run's fixed tail
    // is amortized away by the wider run).
    let [(n1, g1), (n2, g2)] = [growth_rows[3], growth_rows[4]];
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
    let control: Vec<(i64, GrowthCounters)> = {
        let _tier = InterpreterTier::new();
        [798i64, 2_000, 20_000]
            .into_iter()
            .map(|n| (n, native_reds_row(n, "interpreter control")))
            .collect()
    };
    for &(n, ref row) in &control {
        assert!(
            row.yields > 0,
            "the interpreter charges per instruction, so the same shape must be pre-empted at \
             N={n}: {row:?}"
        );
        assert_eq!(
            row.yields as u64, row.collections,
            "one boundary collection per pre-emption (the shape reaches no admission collection \
             of its own): N={n} {row:?}"
        );
        assert_eq!(
            row.slices,
            row.yields + 1,
            "every interpreter slice but the last must end in the `Yield` arm: N={n} {row:?}"
        );
        assert_eq!(
            row.peak_young_spaces, 1,
            "the boundary collection keeps the chain at one young space: N={n} {row:?}"
        );
    }
    assert_eq!(
        (
            control[1].1.slices,
            control[1].1.yields,
            control[1].1.collections,
            control[1].1.reductions
        ),
        (9, 8, 8, 34_020),
        "the reference tier prices every instruction of the same 2 000 iterations (measured \
         34 020 = 9 slices' worth of work) — the counter that makes the native tier's flat \
         4 006 a residual rather than a property of the fixture: {control:?}"
    );
    assert_eq!(
        (
            control[2].1.slices,
            control[2].1.yields,
            control[2].1.collections,
            control[2].1.reductions
        ),
        (85, 84, 84, 339_968),
        "and it keeps pricing them: 10x the iterations is 10x the slices and 10x the charge, \
         where the native tier does not move at all: {control:?}"
    );
    assert!(
        control[1].1.reductions >= 8 * saturated[0].1.reductions,
        "the reference tier must charge at least 8x the native tier's saturated ceiling for the \
         same work (measured {} vs {}): the gap is the tier's, not the fixture's",
        control[1].1.reductions,
        saturated[0].1.reductions
    );
    assert!(
        control[2].1.slices > saturated[4].1.slices,
        "and it must need more slices: {} vs {}",
        control[2].1.slices,
        saturated[4].1.slices
    );
    // Its reported capacity is the nursery the last slice's growth left in place, bounded by the
    // live-set-targeted minor rule rather than by the work: a 10x step in N moves it by less than
    // 2x, and it stays far below the figure an *uncollected* run reports for a twentieth of that
    // work.
    assert!(
        control[2].1.value > collector_floor,
        "the interpreter rows end on a grown nursery, not the floor: {control:?}"
    );
    assert!(
        control[2].1.value < 2 * control[1].1.value,
        "10x the iterations must not move the settled capacity by 10x ({} vs {}): the nursery is \
         sized from the live set at every pre-emption, never from the total work",
        control[2].1.value,
        control[1].1.value
    );
    assert!(
        control[2].1.value < linear[4].1.value,
        "and it must stay below what an uncollected run at a twentieth of that work reports ({} \
         vs {}): collecting is what keeps the capacity off the work",
        control[2].1.value,
        linear[4].1.value
    );
}
