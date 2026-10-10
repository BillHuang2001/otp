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

//! **A scheduled process's own heap GROWS to fit a request the nursery cannot hold.**
//!
//! `src/context/gc.rs` is the engine's heap-admission point (`ensure_heap`). It is armed
//! only for a scheduler slice's own machine (`src/runner/budget.rs`), and it is the one
//! place that owns both the *root set* a collection relocates and the running process's
//! heap policy. This suite is the acceptance evidence for the second half of that seam:
//! the admission point drives the collector's explicit **growth** step
//! ([`Collector::ensure_capacity`](beam_gc::Collector::ensure_capacity), reached through
//! `beam_process::Process::ensure_capacity`), not only `Process::collect`.
//!
//! 1. **The nursery grows.** `heap_growth:sprawl/1` builds a 400-element tuple of a
//!    *runtime* value in a single block, so the compiler emits `test_heap 401 1` — a
//!    401-word request, far above the 233-word nursery a fresh process is created with.
//!    Running it as a real scheduled process grows the nursery to cover the request
//!    (`peak_young_capacity >= 401`), the loop returns the right answer, and **not one
//!    allocation falls back to the pointer-stable arena** (`gc_stats().fallbacks == 0`).
//! 2. **It stays grown.** A long loop of the same request never falls back either — the
//!    growth is durable, not a one-off — while the runtime reports no failed collection,
//!    no halted process and no tripped valve.
//! 3. **The bounded policy is untouched.** A runtime whose process-heap policy genuinely
//!    cannot reach the request (`+hmax N` below it) keeps the pre-change behaviour exactly:
//!    the collection still runs, the allocation falls back to the arena, and the program
//!    still completes — a request the policy makes unreachable is never a fabricated
//!    collection and never a new failure mode.
//!
//! # Fixture
//!
//! `tests/fixtures/heap_growth.beam` is a committed `.beam` image produced by the system
//! `erlc` in a `$TMPDIR` scratch dir (the recipe lives in `tests/fixtures/README.md`), so
//! the suite needs no compiler — only the system Erlang installation the boot sequence
//! resolves. A missing installation is a **loud skip**, never a silent pass.

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::time::{Duration, Instant};

use beam_rt::config::HeapOptions;
use beam_rt::{Runtime, RuntimeConfig};
use beam_term::{make_small, signed_value};

/// The growth fixture: `run(N)` repeats `sprawl/1` N times and returns `N`, and
/// `sprawl(X)` builds a 400-element tuple of `X`.
const HEAP_GROWTH_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/heap_growth.beam"
);

/// The words `heap_growth:sprawl/1`'s single block reserves: the tuple has 400 elements,
/// so the system compiler emits `test_heap 401 1` (verified with `beam_disasm`). The
/// request the admission point is handed is therefore exactly this many words.
const SPRAWL_WORDS: usize = 401;

/// How long a growth loop may take before the test declares the runtime hung. Generous:
/// the loops are pure interpreter work and finish in well under a second, so this is a
/// safety net rather than the normal path.
const LOOP_DEADLINE: Duration = Duration::from_secs(120);

/// A runtime with the fixture loaded and the boot sequence run.
///
/// `boot` (and therefore every `spawn_process`, which boots on demand) resolves the system
/// code path natively from the filesystem (the system `erl` only when no installation is
/// discoverable), so a tree without the system installation cannot run this suite —
/// reported as a **loud skip** by [`support::require_oracle`], never a silent pass.
fn runtime_with(config: RuntimeConfig) -> Option<Runtime> {
    support::require_oracle("heap_growth")?;
    let mut rt = Runtime::new(config).expect("the runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(HEAP_GROWTH_BEAM).expect("the fixture loads");
    Some(rt)
}

/// What one scheduled run of the growth loop did: its value, the young heap's peak
/// occupancy/capacity, and the runtime counters the run moved.
#[derive(Debug, Clone, Copy)]
struct GrowthRun {
    /// The loop's returned value.
    value: i64,
    /// The largest young-heap occupancy observed at a slice boundary.
    peak_young_used: usize,
    /// The largest young-heap capacity observed at a slice boundary.
    peak_young_capacity: usize,
    /// Collections the **runtime** ran during this run.
    collections: u64,
    /// Engine allocations that fell back to the pointer-stable arena during this run.
    fallbacks: u64,
}

/// Runs `heap_growth:run/1` as a **scheduled process** and samples its own collector while
/// it lives.
///
/// The sampling matters: the scheduler removes a process from the table the moment it
/// terminates, so the pid's heap/capacity figures are only readable *during* the run. One
/// slice is run between samples (the runner pre-empts at the reduction budget), which is
/// the same step granularity `tests/gc_collect.rs` uses.
fn run_growth(rt: &mut Runtime, n: i64) -> GrowthRun {
    let before = rt.gc_stats();
    let pid = rt
        .spawn_process("heap_growth", "run", vec![make_small(n)])
        .expect("the growth loop is spawned");

    let mut peak_young_used = 0usize;
    let mut peak_young_capacity = 0usize;
    let deadline = Instant::now() + LOOP_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "heap_growth:run/1 with N={n} did not terminate within {}s",
            LOOP_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
        if let Some(process) = rt.processes().get(pid) {
            peak_young_used = peak_young_used.max(process.heap_used_words());
            peak_young_capacity =
                peak_young_capacity.max(process.collector().young().capacity_words());
        }
    }

    let value = rt.process_result(pid).expect("the loop returned a value");
    let after = rt.gc_stats();
    GrowthRun {
        value: signed_value(value),
        peak_young_used,
        peak_young_capacity,
        collections: after.collections - before.collections,
        fallbacks: after.fallbacks - before.fallbacks,
    }
}

/// Asserts the three "nothing failed" counters of a run are clean.
fn assert_no_failures(rt: &Runtime, run: &GrowthRun) {
    let stats = rt.gc_stats();
    assert_eq!(stats.errors, 0, "no collection failed; {stats:?} {run:?}");
    assert!(
        !stats.disabled,
        "the escape-store valve did not trip; {stats:?} {run:?}"
    );
    assert_eq!(
        stats.halted, 0,
        "no process had its collection halted; {stats:?} {run:?}"
    );
}

/// **Acceptance 1 — the nursery really grows to fit a request the nursery cannot hold.**
///
/// Before the growth seam was adopted, the running process fell back to the pointer-stable
/// arena on every iteration (the 233-word nursery can never hold a 401-word request), so
/// `gc_stats().fallbacks` moved and the peak nursery capacity stayed at its initial
/// figure. Growing the nursery at the admission point is what makes both observations flip.
#[test]
fn the_nursery_grows_to_fit_a_request_larger_than_the_initial_nursery() {
    let Some(mut rt) = runtime_with(RuntimeConfig::new()) else {
        return;
    };
    let run = run_growth(&mut rt, 5_000);

    assert_eq!(run.value, 5_000, "heap_growth:run/1 returns its argument");
    assert!(
        run.collections > 0,
        "the collector must have run at the admission point; {run:?}"
    );
    assert!(
        run.peak_young_capacity >= SPRAWL_WORDS,
        "the nursery must have grown to cover the {SPRAWL_WORDS}-word request; {run:?}"
    );
    assert!(
        run.peak_young_used <= run.peak_young_capacity,
        "the young occupancy can never exceed its capacity; {run:?}"
    );
    assert_eq!(
        run.fallbacks, 0,
        "every allocation of a {SPRAWL_WORDS}-word request must be served by the \
         process's own grown young heap, never by the pointer-stable arena; {run:?}"
    );
    assert_no_failures(&rt, &run);
}

/// **Acceptance 2 — the growth is durable.**
///
/// The same request repeated many times is still served by the grown nursery (a
/// collection re-sizes the nursery from what it left alive, so it can shrink back between
/// rounds — the admission point grows it again), and nothing falls back to the arena for
/// the whole run.
#[test]
fn a_long_growth_loop_never_falls_back_to_the_arena() {
    let Some(mut rt) = runtime_with(RuntimeConfig::new()) else {
        return;
    };
    let run = run_growth(&mut rt, 20_000);

    assert_eq!(run.value, 20_000, "heap_growth:run/1 returns its argument");
    assert!(
        run.collections > 0,
        "the collector must have run many times; {run:?}"
    );
    assert!(
        run.peak_young_capacity >= SPRAWL_WORDS,
        "the nursery grew to cover the request; {run:?}"
    );
    assert_eq!(
        run.fallbacks, 0,
        "20,000 requests larger than the initial nursery must all be served locally; {run:?}"
    );
    assert_no_failures(&rt, &run);
}

/// **Acceptance 3 — a request the policy makes unreachable keeps today's behaviour.**
///
/// `+hmax 200` puts a hard ceiling of 200 words on the process heap policy — below the
/// 401-word request — so `HeapPolicy::next_words(401, 0) < 401` and the growth seam is
/// genuinely unreachable. That case must keep the pre-change behaviour exactly: the
/// collection still runs, the allocation falls back to the pointer-stable arena, and the
/// program still completes. Never a fabricated collection, never a new failure mode.
#[test]
fn a_bounded_heap_policy_still_falls_back_to_the_arena_instead_of_growing() {
    let bounded = RuntimeConfig::new()
        .with_heap_options(HeapOptions::new().with_max_heap_words(200));
    let Some(mut rt) = runtime_with(bounded) else {
        return;
    };
    let run = run_growth(&mut rt, 50);

    assert_eq!(run.value, 50, "a bounded policy still completes the loop");
    assert!(
        run.fallbacks > 0,
        "a request above the policy ceiling can only be met by the arena; {run:?}"
    );
    assert!(
        run.peak_young_capacity <= 200,
        "the policy ceiling bounds the nursery; {run:?}"
    );
    assert!(
        run.collections > 0,
        "the collection still runs — the growth is an addition, not a replacement; {run:?}"
    );
    assert_no_failures(&rt, &run);
}
