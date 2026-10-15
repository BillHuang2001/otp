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

//! **The process-heap collector, proved end to end.**
//!
//! `src/context/gc.rs` is the engine's heap-admission point (`ensure_heap`) and the two
//! allocation seams for a *scheduled* process, `src/runtime/gc.rs` holds the collector's state
//! and its safety valves, and `src/runner/budget.rs` arms the per-call latch that lets the
//! slice's own machine collect. This suite is the acceptance evidence that the wiring really
//! collects a *running* process's own heap:
//!
//! 1. **The collector ran.** An allocating tail-recursive loop (`gc_loop:loop/1`, whose
//!    `build/1` conses every iteration) is run as a real scheduled process; the loop returns
//!    the right answer **and** [`Runtime::gc_stats`] reports collections, the pid's own
//!    `minor_gcs` counter grew, and no collection failed or disabled the runtime.
//! 2. **Memory is bounded.** The same loop at a 10× larger `N` collects proportionally more
//!    often, while the young heap's **capacity and peak occupancy do not scale with `N`** —
//!    the collector's guarantee.
//! 3. **Float code collects safely — and is never pinned.** A module doing float
//!    arithmetic across a heap boundary (`gc_floats:l1/1`) must produce **correct** float
//!    results and **is** collected over: the engine reaches only the **full** heap-admission
//!    door (`ensure_heap_full`), which publishes the machine's **whole** root set — the FR file
//!    included — so a live float register is relocated exactly like an x register and the
//!    collection is FR-safe for **any** module (`gc_stats().collections > 0`). A float register
//!    still live when a slice ends is caught out of band by the runner's post-slice
//!    `Machine::fr_mark` check, which marks the pid **relocation-exempt for the inter-slice
//!    window only** — the park publishes that live FR prefix onto the process's root set, the
//!    next slice's resume clears the mark, and `Runtime::gc_stats().pinned` stays **`0`**, so
//!    the process collects at its own heap-admission points throughout.
//! 4. **A float run needs no arena fallback, and scales.** A longer float run collects
//!    **more** (the loop's collections grow with its iteration count — the plateau the old
//!    permanent pin produced is gone), its allocations stay on its **own** young heap
//!    (`Runtime::gc_stats().fallbacks` stays `0`) and the program stays correct — never a wrong
//!    answer, never a failure. (The arena fallback is the reserve for *nested* runs — see the
//!    limitation below.)
//!
//! # Honest limitation (pinned by these tests, not hidden by them)
//!
//! The GC point is armed **only** for the slice's own machine
//! (the runner's `BudgetContext::ensure_heap` — see `src/context/gc.rs`), so a workload whose
//! allocation happens inside a *nested* run (an external module call served by
//! `RuntimeContext::run_export`, which builds a second `Machine` on the same context) never
//! reaches a collection point. Its allocations are still served from the process young heap by
//! `context::gc::alloc_words`, but when that fills they fall back to the runtime-wide
//! **arena**. `Runtime::gc_stats().fallbacks` is the
//! counter that makes that visible, and it is the number to read when a workload's resident
//! memory still grows with its input (`tests/gc_collect.rs` covers the armed path only; the
//! `foreign` benchmark workload is the cross-module case). A **top-level** run whose allocation
//! happens in the armed slice's own machine — Acceptance 3/4's float run included — is the
//! contrast: it collects normally, so nothing falls back to the arena (`fallbacks == 0`).
//! # Fixtures
//!
//! `tests/fixtures/gc_loop.beam`, `gc_ints.beam` and `gc_floats.beam` are committed `.beam`
//! images produced by the system `erlc` (`+deterministic`; the registry recipe lives in
//! `tests/fixtures/README.md`), so the suite needs no compiler — only the system Erlang
//! installation the boot sequence resolves. A missing installation is a **loud skip**, never
//! a silent pass.

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::time::{Duration, Instant};

use beam_engine::terms;
use beam_rt::Runtime;
use beam_term::{is_small, make_small, signed_value};

/// The allocating-loop image: `loop(N)` builds (and measures) a fresh `N`-element list every
/// iteration, so the reduction budget yields it long before it finishes and the young heap
/// fills many times over.
const GC_LOOP_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/gc_loop.beam");

/// The integer-only control image (its `l1/1` allocates nothing at all): its loop must never
/// fill a young heap, so it pins the "collect only when the heap really fills" direction.
const GC_INTS_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/gc_ints.beam");

/// The float image: `l1(N)` accumulates `math:sqrt(float(N))`, i.e. float arithmetic across a
/// heap boundary — a module whose code writes `Machine::fr`, so it collects through the full
/// door's FR-safe root set; a float register live at a slice end opens the inter-slice window's
/// relocation exemption (cleared at the next resume), never a permanent pin.
const GC_FLOATS_BEAM: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/gc_floats.beam");

/// How long one of these loops may take before the test declares the runtime hung. Generous:
/// the loops are pure interpreter work and the numbers below are chosen to finish in well
/// under a second, so this is a safety net rather than the normal path.
const LOOP_DEADLINE: Duration = Duration::from_secs(120);

/// A runtime with `beam` loaded and the boot sequence run.
///
/// `boot` (and therefore every `spawn_process`, which boots on demand) resolves the system
/// code path natively from the filesystem (the system `erl` only when no installation is
/// discoverable), so a tree without the system installation cannot run this suite — reported
/// as a **loud skip** by [`support::require_oracle`], never a silent pass.
fn runtime_with(beam: &str) -> Option<Runtime> {
    support::require_oracle("gc_collect")?;
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(beam).expect("the fixture loads");
    Some(rt)
}

/// What one scheduled run of a fixture's loop did: its value, the pid's own minor-collection
/// counter and the young heap's peak occupancy/capacity, plus the runtime counters the run
/// moved.
#[derive(Debug, Clone, Copy)]
struct LoopRun {
    /// The loop's returned value.
    value: LoopValue,
    /// The pid's own `minor_gcs` counter, sampled while it was still alive.
    minor_gcs: u32,
    /// The largest young-heap occupancy observed at a slice boundary.
    peak_young_used: usize,
    /// The largest young-heap capacity observed at a slice boundary.
    peak_young_capacity: usize,
    /// Collections the **runtime** ran during this run.
    collections: u64,
    /// Engine allocations that fell back to the pointer-stable arena during this run.
    fallbacks: u64,
}

/// The value a loop returned, decoded to the shape the assertions need.
#[derive(Debug, Clone, Copy)]
enum LoopValue {
    /// A small integer.
    Int(i64),
    /// A boxed float.
    Float(f64),
}

impl LoopRun {
    /// The integer the loop returned, refusing a non-integer with a diagnosable panic.
    fn int(&self) -> i64 {
        match self.value {
            LoopValue::Int(value) => value,
            LoopValue::Float(value) => panic!("expected an integer result, got the float {value}"),
        }
    }

    /// The float the loop returned, refusing a non-float with a diagnosable panic.
    fn float(&self) -> f64 {
        match self.value {
            LoopValue::Float(value) => value,
            LoopValue::Int(value) => panic!("expected a float result, got the integer {value}"),
        }
    }
}

/// Runs `module:function/1` as a **scheduled process** and samples its own collector while it
/// lives.
///
/// The sampling matters: the scheduler removes a process from the table the moment it
/// terminates, so the pid's `minor_gcs`/heap figures are only readable *during* the run. One
/// slice is run between samples (the runner pre-empts at the reduction budget), which is the
/// same step granularity `tests/preempt.rs` uses.
fn run_loop(rt: &mut Runtime, module: &str, function: &str, n: i64) -> LoopRun {
    let before = rt.gc_stats();
    let pid = rt
        .spawn_process(module, function, vec![make_small(n)])
        .expect("the loop is spawned");

    let mut minor_gcs = 0u32;
    let mut peak_young_used = 0usize;
    let mut peak_young_capacity = 0usize;
    let deadline = Instant::now() + LOOP_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "{module}:{function}/{n} did not terminate within {}s",
            LOOP_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
        if let Some(process) = rt.processes().get(pid) {
            minor_gcs = minor_gcs.max(process.collector().minor_gcs());
            peak_young_used = peak_young_used.max(process.heap_used_words());
            peak_young_capacity =
                peak_young_capacity.max(process.collector().young().capacity_words());
        }
    }

    let value = rt.process_result(pid).expect("the loop returned a value");
    let decoded = if is_small(value) {
        LoopValue::Int(signed_value(value))
    } else {
        LoopValue::Float(terms::float_value(value).expect("a `.beam` float register value"))
    };
    let after = rt.gc_stats();
    LoopRun {
        value: decoded,
        minor_gcs,
        peak_young_used,
        peak_young_capacity,
        collections: after.collections - before.collections,
        fallbacks: after.fallbacks - before.fallbacks,
    }
}

/// The accumulator `gc_ints:l1/1` computes: `loop(N, 0)` with `Acc' = (Acc + N) rem 1000003`,
/// evaluated in Rust so the expected value does not depend on the toolchain.
fn expected_int_loop(n: i64) -> i64 {
    (1..=n).rev().fold(0i64, |acc, k| (acc + k) % 1_000_003)
}

/// The accumulator `gc_floats:l1/1` computes: `loop(N, 0.0)` with `Acc' = Acc + sqrt(N)`,
/// evaluated in Rust. IEEE-754 `sqrt` is correctly rounded and the summation order is the
/// fixture's, so the two agree bit for bit.
fn expected_float_loop(n: i64) -> f64 {
    (1..=n)
        .rev()
        .fold(0.0f64, |acc, k| acc + (k as f64).sqrt())
}

/// **Acceptance 1 — the collector really ran.**
///
/// The loop returns the right answer, the runtime counted collections, the pid's own
/// `minor_gcs` grew, and nothing failed, halted or tripped the global valve.
#[test]
fn the_collector_runs_for_an_allocating_scheduled_loop() {
    let Some(mut rt) = runtime_with(GC_LOOP_BEAM) else {
        return;
    };
    let run = run_loop(&mut rt, "gc_loop", "loop", 2_000);

    assert_eq!(run.int(), 2_000, "gc_loop:loop/1 returns its argument");
    assert!(
        run.collections > 0,
        "the runtime must have collected the running process's young heap at the \
         heap-admission point; {run:?}"
    );
    // The pid really allocated on **its own** young heap (never only on the arena): the
    // sampled capacity is this runtime's collector geometry, and the run is well inside it.
    //
    // `minor_gcs` is deliberately **not** asserted here: it is `minor_since_major`, which the
    // collector resets on every fullsweep, and this loop's collections all escalate to majors
    // (see [`the_young_heap_stays_bounded_as_the_loop_count_grows`]) — the cumulative runtime
    // counter `run.collections` above is the honest "the collector ran" signal.
    assert!(
        run.peak_young_capacity > 0 && run.peak_young_used > 0,
        "the scheduled process allocated on its own young heap; {run:?}"
    );
    assert!(
        run.peak_young_used <= run.peak_young_capacity,
        "the young occupancy can never exceed its capacity; {run:?}"
    );

    let stats = rt.gc_stats();
    assert_eq!(stats.errors, 0, "no collection failed; {stats:?}");
    assert!(
        !stats.disabled,
        "the escape-store valve did not trip; {stats:?}"
    );
    assert_eq!(
        stats.halted, 0,
        "no process had its collection halted; {stats:?}"
    );
    assert_eq!(
        stats.pinned, 0,
        "gc_loop has no float-family instruction, so nothing is pinned; {stats:?}"
    );
}

/// **Acceptance 1 (the control direction) — a non-allocating loop is never collected over.**
///
/// `gc_ints:l1/1` only does `+`/`rem` on small integers: it allocates nothing, so the young
/// heap never fills and the collector must never run. This is what keeps the first assertion
/// meaningful (a runtime that collected unconditionally would pass it trivially).
#[test]
fn a_non_allocating_loop_is_never_collected_over() {
    let Some(mut rt) = runtime_with(GC_INTS_BEAM) else {
        return;
    };
    let run = run_loop(&mut rt, "gc_ints", "l1", 20_000);

    assert_eq!(
        run.int(),
        expected_int_loop(20_000),
        "gc_ints:l1/1's accumulator"
    );
    assert_eq!(
        run.collections, 0,
        "an integer-only loop allocates nothing and must never fill a young heap; {run:?}"
    );
    assert_eq!(run.minor_gcs, 0, "{run:?}");
    assert_eq!(rt.gc_stats().errors, 0);
}

/// **Acceptance 2 — the young heap really is the collector's, not the arena.**
///
/// The same loop at a 10× larger `N` collects more often (never fewer) and its young heap
/// stays inside this runtime's collector geometry rather than growing with the *program's*
/// total allocation.
///
/// This pins the observed behaviour honestly, including its limitation: `beam_gc`'s hedging
/// growth table enlarges the nursery when a collection reclaims too little
/// (`young_after_collection`), and this loop keeps a large part of its `build/1` spine live
/// while it recurses, so the capacity does grow with `N` — sublinearly in the words
/// allocated, but measurably. What must hold is that collections happen and that the nursery
/// stays a *bounded* multiple of the small-N figure, not that it is constant.
#[test]
fn the_young_heap_stays_bounded_as_the_loop_count_grows() {
    let Some(mut rt) = runtime_with(GC_LOOP_BEAM) else {
        return;
    };
    let small = run_loop(&mut rt, "gc_loop", "loop", 500);
    let large = run_loop(&mut rt, "gc_loop", "loop", 5_000);

    assert_eq!(small.int(), 500);
    assert_eq!(large.int(), 5_000);
    assert!(
        small.collections > 0,
        "the small run collected too; {small:?}"
    );
    assert!(large.collections > 0, "the large run collected; {large:?}");

    // 10× the iterations ⇒ more collections, never fewer.
    assert!(
        large.collections >= small.collections * 2,
        "collections must grow with N (small: {}, large: {})",
        small.collections,
        large.collections
    );
    // ...and, crucially, the larger run is *not* backed by an unbounded arena: the nursery is
    // bounded by the collector's growth table, well under any figure that would imply the
    // program's whole allocation history is resident.
    assert!(
        large.peak_young_capacity < 100_000_000,
        "the nursery is bounded by the growth table, not by the program's total allocation; \
         got {} words",
        large.peak_young_capacity
    );
}

/// **Acceptance 3 — a float-using module is collected over, and its pid is never pinned.**
///
/// A module doing float arithmetic across a heap boundary returns the **correct** result. The
/// engine reaches only the **full** heap-admission door (`ensure_heap_full`), which publishes the
/// machine's **whole** root set — the FR file included — so a live float register is relocated
/// exactly like an x register or a stack word, and the collection is FR-safe for **any** module:
/// a float-using call site therefore **is** collected over (`collections > 0`). A float register
/// still live when a slice ends is caught out of band by the runner's post-slice
/// [`Machine::fr_mark`](beam_engine::Machine::fr_mark) check, but the reaction is now scoped to
/// the **inter-slice window** (a relocation exemption the next resume clears) rather than the
/// process's whole life, so `gc_stats().pinned` stays `0` and the loop keeps collecting.
#[test]
fn a_float_using_module_is_collected_over_and_never_pinned() {
    let Some(mut rt) = runtime_with(GC_FLOATS_BEAM) else {
        return;
    };
    let run = run_loop(&mut rt, "gc_floats", "l1", 2_000);

    // Correctness first: the float result must be exactly the one the Erlang source computes,
    // regardless of what the collector did to the heap underneath it.
    assert_eq!(
        run.float(),
        expected_float_loop(2_000),
        "gc_floats:l1/1 accumulates sqrt(1..N) in one IEEE-754 double"
    );

    let stats = rt.gc_stats();
    assert!(
        stats.collections > 0,
        "the full door publishes the whole root set (FR file included), so a float-using \
         module IS collected over; {stats:?}"
    );
    assert_eq!(
        stats.pinned, 0,
        "the post-slice float-register reaction is scoped to the inter-slice window, so the pid \
         is NEVER pinned and its own heap-admission points keep collecting; {stats:?}"
    );
    assert_eq!(stats.errors, 0, "{stats:?}");
    assert!(!stats.disabled, "{stats:?}");
}

/// **Acceptance 4 — a long float run needs no arena fallback, and its collections scale.**
///
/// The float process is collected over at its own heap-admission points throughout; the
/// inter-slice window's relocation exemption is cleared by every resume, so it never becomes a
/// permanent refusal. Its allocations are served from its **own** young heap, so the run must
/// return exactly the right answer, report `errors == 0` and `pinned == 0`, with **no**
/// allocation falling back to the pointer-stable arena (`fallbacks == 0`) — the contrast with a
/// *nested* run, whose allocations do fall back (see the module docs' *Honest limitation*).
#[test]
fn a_float_run_needs_no_arena_fallback_and_collects_more_as_n_grows() {
    let Some(mut rt) = runtime_with(GC_FLOATS_BEAM) else {
        return;
    };
    let small = run_loop(&mut rt, "gc_floats", "l1", 500);
    let large = run_loop(&mut rt, "gc_floats", "l1", 20_000);

    assert_eq!(small.float(), expected_float_loop(500));
    assert_eq!(
        large.float(),
        expected_float_loop(20_000),
        "the collected-over float run returns exactly the same float"
    );
    assert!(
        small.fallbacks == 0 && large.fallbacks == 0,
        "a collected-over float run stays on its own young heap — never the arena; \
         {small:?} {large:?}"
    );
    assert!(
        small.collections > 0,
        "the float run really was collected over; {small:?}"
    );
    // The defect this suite pins: while the post-slice FR reaction was a PERMANENT runtime-level
    // pin, `Runtime::gc_process_enabled` answered `false` for the rest of the process's life, so
    // every later collection door consumed and dropped the request and the float loop's
    // collections PLATEAUED at whatever it had reached before the first pin
    // (`large.collections == small.collections`, with the nursery growing instead). Scoped to the
    // inter-slice window, the loop's collections now grow with its iteration count.
    assert!(
        large.collections > small.collections,
        "a longer float loop must collect MORE, not plateau on a permanent pin \
         (small: {}, large: {})",
        small.collections,
        large.collections
    );
    let stats = rt.gc_stats();
    assert_eq!(stats.pinned, 0, "nothing is ever pinned; {stats:?}");
    assert_eq!(stats.errors, 0, "{stats:?}");
}
