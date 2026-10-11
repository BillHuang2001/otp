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

//! **The runtime's mature-GC strategy follows its spawn `HeapPolicy`.**
//!
//! `Runtime::default_spawn_policy` hands `beam_gc` the policy of the runtime's own
//! [`HeapOptions`](beam_rt::config::HeapOptions) for **every** process it creates (the root
//! process, the native file service and every `spawn_process` child), and `beam_gc` resolves
//! the mature-generation strategy from that policy with the **automatic rule**
//! ([`select_mature`](beam_gc::select_mature)): the mark-region (`MatureKind::Immix`) space
//! at/above [`IMMIX_BREAK_EVEN_WORDS`] mature words, the copying space below it. This suite
//! is the acceptance evidence that the *runtime's configuration* — not a hand-built
//! collector — really reaches the strategy of a live, runtime-spawned process.
//!
//! # Why this file exists
//!
//! The runtime cannot pin a mature strategy explicitly: `ProcessTable::spawn` accepts a
//! [`HeapPolicy`](beam_gc::HeapPolicy) and nothing else, so there is no
//! `GcConfig`/`MatureSelection` parameter on the process path at all (see
//! [`HeapOptions::set_gc_strategy`](beam_rt::config::HeapOptions::set_gc_strategy)). The only
//! lever is the policy's mature-generation size, and the honest way to use it is to let the
//! automatic rule resolve it. That also means the strategy a process runs is a consequence of
//! the configuration: *this* is what makes the `BEAM_RT_GC_STRATEGY=immix` host opt-in
//! (applied once in `Runtime::new` to the owned `HeapOptions`) actually select the mark-region
//! space — through the rule, with `MatureSource::Automatic` and **no** override.
//!
//! # What is asserted
//!
//! 1. the break-even constant itself (8 whole Immix blocks);
//! 2. the **runtime default** still resolves to `MatureKind::Copying` — the behaviour of every
//!    `beam_rt` process before the configuration knobs existed;
//! 3. a runtime whose `HeapOptions` carry the Immix policy selects `MatureKind::Immix`, from
//!    the automatic rule (**not** a pin) — the assertion with no pre-change counterpart;
//! 4. the boundary one word below the break-even still selects `MatureKind::Copying`;
//! 5. the config-implied mark-region strategy really **runs** a fullsweep workload;
//! 6. a **live payload** survives real collections on the mark-region path, across a
//!    fullsweep on every collection, with every element verified — the correctness evidence
//!    for the Immix path end to end (see [`a_live_payload_survives_a_fullsweep_on_the_immix_path`]).
//!
//! Nothing here measures time: the work is a spawn, some scheduler slices and a table read,
//! so the suite is deterministic (no wall-clock assertion anywhere).
//!
//! # Fixtures
//!
//! Acceptances 1–4 spawn a process at the committed `tests/fixtures/heap_growth.beam` (`run/1`,
//! the growth loop `tests/heap_growth.rs` uses) and **never** run it: the strategy is fixed when
//! the process is created, so spawning it is the whole workload. Acceptance 5 runs that fixture
//! as a real scheduled process; acceptance 6 runs the committed
//! `tests/fixtures/gc_payload.beam` (`run/1`, a long-lived payload plus short-lived churn — see
//! `tests/fixtures/README.md` for the recipe). Every fixture is loaded through the runtime's own
//! `load_module`. A missing system Erlang installation is a **loud skip**, never a silent pass.

// The shared helper module is compiled into every integration-test binary, so this binary
// would otherwise warn on the entry points it does not call.
#[allow(dead_code)]
mod support;

use std::time::{Duration, Instant};

use beam_gc::{MatureKind, MatureSource, IMMIX_BREAK_EVEN_WORDS};
use beam_rt::config::{strategy_env_default, GcStrategy, HeapOptions};
use beam_rt::{Runtime, RuntimeConfig};
use beam_term::{make_small, signed_value};

/// The fixture `tests/heap_growth.rs` also runs: `run(N)` repeats `sprawl/1` N times.
const HEAP_GROWTH_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/heap_growth.beam"
);

/// The runtime-shaped GC workload: `run(N)` builds a 64-element long-lived payload, churns N
/// rounds of short-lived garbage while keeping it live, and returns the payload.
const GC_PAYLOAD_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/gc_payload.beam"
);

/// [`GC_PAYLOAD_BEAM`]'s payload element count (`gc_payload:payload_length/0`).
const PAYLOAD_LENGTH: i64 = 64;

/// The churn rounds acceptance 6 runs — enough for many collections at the 233-word default
/// nursery, with `fullsweep_after(0)` making every one of them a fullsweep.
const PAYLOAD_ROUNDS: i64 = 200;

/// The emulator's default initial mature-generation size in words — the `beam_gc` default
/// [`HeapOptions::default`] carries, and a value far below the break-even.
const DEFAULT_INITIAL_MATURE_WORDS: usize = 233;

/// The iterations of [`HEAP_GROWTH_BEAM`]'s loop. Each one is a `test_heap 401 1` admission,
/// so the process collects on essentially every iteration; 500 keeps the suite sub-second
/// while driving plenty of collections.
const WORKLOAD_ITERATIONS: i64 = 500;

/// How long a workload run may take before the test declares the runtime hung — a safety net
/// (the loop finishes in milliseconds), never a performance assertion.
const WORKLOAD_DEADLINE: Duration = Duration::from_secs(120);

/// A runtime with `config` and the fixture `beam` loaded, or `None` after a loud skip.
///
/// The runtime is built through the crate's **own** path (`Runtime::new` → `boot` →
/// `load_module`), never from a hand-built process or collector, so what the assertions read
/// is what the real runtime creates.
fn runtime_with_fixture(config: RuntimeConfig, beam: &str) -> Option<Runtime> {
    support::require_oracle("gc_strategy")?;
    let mut rt = Runtime::new(config).expect("the runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(beam).expect("the fixture loads");
    Some(rt)
}

/// [`runtime_with_fixture`] on the strategy-probe fixture.
fn runtime_with(config: RuntimeConfig) -> Option<Runtime> {
    runtime_with_fixture(config, HEAP_GROWTH_BEAM)
}

/// What a live process's collector reports about its mature-generation strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResolvedStrategy {
    /// The strategy the process actually collects with.
    kind: MatureKind,
    /// Whether the automatic rule chose it or someone pinned it.
    source: MatureSource,
    /// The mature-generation size the rule decided from.
    mature_words: usize,
    /// The rule's break-even, as the collector reports it.
    break_even_words: usize,
    /// Whether the choice overrides the rule (it must never, on this path).
    is_override: bool,
}

impl ResolvedStrategy {
    /// Reads `process`'s resolved strategy through the public collector accessors.
    fn of(process: &beam_process::Process) -> ResolvedStrategy {
        let collector = process.collector();
        let choice = collector.mature_choice();
        ResolvedStrategy {
            kind: collector.kind(),
            source: choice.source,
            mature_words: choice.mature_words,
            break_even_words: choice.break_even_words,
            is_override: choice.is_override(),
        }
    }
}

/// Spawns the fixture workload on `rt` and reports the **spawned child's** resolved
/// strategy, read while the process is still in the table.
///
/// The strategy is fixed at `ProcessTable::spawn` time (the collector is created from the
/// spawn policy there), so no scheduler slice is needed — and the process is deliberately
/// never stepped, which keeps the suite independent of the collector running.
fn spawned_strategy(rt: &mut Runtime) -> ResolvedStrategy {
    let pid = rt
        .spawn_process("heap_growth", "run", vec![make_small(1)])
        .expect("the fixture workload is spawned");
    let process = rt
        .processes()
        .get(pid)
        .expect("a just-spawned process is in the process table until it terminates");
    ResolvedStrategy::of(process)
}

/// Skips (loudly) a test whose premise a host-level `BEAM_RT_GC_STRATEGY=immix` would
/// invalidate, rather than reporting a spurious failure.
fn host_opted_into_immix(banner: &str) -> bool {
    if strategy_env_default() == GcStrategy::Immix {
        eprintln!();
        eprintln!("=============== beam_rt {banner} SKIPPED ===============");
        eprintln!(
            "BEAM_RT_GC_STRATEGY=immix is set in this environment, so the runtime default is \
             the mark-region strategy by construction."
        );
        eprintln!("Unset it to run this default-behaviour test. NOT a pass.");
        eprintln!("========================================================");
        eprintln!();
        return true;
    }
    false
}

/// **Acceptance 1 — the rule's break-even is 8 whole Immix blocks.**
///
/// The figure is `beam_gc`'s, not this crate's: the runtime must not restate it, only consume
/// it. Pinned here because every other assertion in this file is expressed in terms of it.
#[test]
fn the_break_even_is_eight_immix_blocks() {
    assert_eq!(IMMIX_BREAK_EVEN_WORDS, 2_048);
    assert_eq!(IMMIX_BREAK_EVEN_WORDS, 8 * beam_gc::BLOCK_WORDS);
    assert_eq!(
        beam_gc::select_mature(IMMIX_BREAK_EVEN_WORDS),
        MatureKind::Immix
    );
    assert_eq!(
        beam_gc::select_mature(IMMIX_BREAK_EVEN_WORDS - 1),
        MatureKind::Copying
    );
}

/// **Acceptance 2 — the runtime default still spawns a copying collector.**
///
/// A bare `Runtime::new(RuntimeConfig::new())` — the configuration the CLI produces with no
/// heap flags — must keep the pre-change behaviour exactly: the copying mature generation,
/// chosen by the **automatic** rule at the default 233-word mature size, with no override.
#[test]
fn the_runtime_default_spawns_a_copying_collector() {
    if host_opted_into_immix("gc_strategy") {
        return;
    }
    let Some(mut rt) = runtime_with(RuntimeConfig::new()) else {
        return;
    };
    // The configuration really is the emulator default …
    assert_eq!(
        rt.config().heap_options().policy().initial_mature_words,
        DEFAULT_INITIAL_MATURE_WORDS
    );
    assert_eq!(
        rt.config().heap_options(),
        &HeapOptions::default(),
        "a bare RuntimeConfig carries the untouched emulator heap policy"
    );
    // … and the child it spawns resolves to the copying space, automatically.
    let spawned = spawned_strategy(&mut rt);
    assert_eq!(
        spawned,
        ResolvedStrategy {
            kind: MatureKind::Copying,
            source: MatureSource::Automatic,
            mature_words: DEFAULT_INITIAL_MATURE_WORDS,
            break_even_words: IMMIX_BREAK_EVEN_WORDS,
            is_override: false,
        }
    );
    // The runtime's own root process carries the very same policy, so a config-implied
    // strategy reaches it too (the spawn policy is the single global default).
    let root = rt
        .processes()
        .get(rt.self_pid())
        .expect("the runtime's root process is always in the table");
    assert_eq!(ResolvedStrategy::of(root), spawned);
}

/// **Acceptance 3 — the config-implied strategy is the one selected.**
///
/// A runtime whose `HeapOptions` carry the Immix policy (`with_initial_mature_words` at the
/// break-even, which is exactly what `BEAM_RT_GC_STRATEGY=immix` applies to the owned
/// configuration in `Runtime::new`) spawns a process whose collector **is** the mark-region
/// one — resolved by the automatic rule, with no explicit pin anywhere on the path (the
/// process path cannot express one).
///
/// This is the assertion with no pre-change counterpart: before the configuration knobs, the
/// runtime had no way to reach `MatureKind::Immix` at all — `initial_mature_words` was
/// private and fixed at the emulator default.
#[test]
fn a_config_implied_immix_policy_selects_the_mark_region_strategy() {
    let config = RuntimeConfig::new().with_heap_options(
        HeapOptions::new().with_initial_mature_words(IMMIX_BREAK_EVEN_WORDS),
    );
    let Some(mut rt) = runtime_with(config) else {
        return;
    };
    // The opt-in reached no init argument: it is a host seam, not a CLI surface.
    assert!(
        rt.config().init_args.is_empty(),
        "the strategy knob must not record an init argument; {:?}",
        rt.config().init_args
    );
    assert_eq!(
        rt.config().heap_options().policy().initial_mature_words,
        IMMIX_BREAK_EVEN_WORDS
    );
    let spawned = spawned_strategy(&mut rt);
    assert_eq!(
        spawned,
        ResolvedStrategy {
            kind: MatureKind::Immix,
            source: MatureSource::Automatic,
            mature_words: IMMIX_BREAK_EVEN_WORDS,
            break_even_words: IMMIX_BREAK_EVEN_WORDS,
            is_override: false,
        },
        "the mark-region strategy must come from the runtime's configured policy, via the rule"
    );
    assert_eq!(
        ResolvedStrategy::of(
            rt.processes()
                .get(rt.self_pid())
                .expect("the root process is in the table")
        ),
        spawned,
        "the root process and a spawned child share the one spawn policy"
    );
}

/// **Acceptance 4 — the boundary is exact: one word below the break-even is copying.**
///
/// The rule is `>=` [`IMMIX_BREAK_EVEN_WORDS`], so the largest non-Immix policy still selects
/// the copying space. This is what keeps a raised size a *deliberate* opt-in.
#[test]
fn one_word_below_the_break_even_still_selects_copying() {
    let config = RuntimeConfig::new().with_heap_options(
        HeapOptions::new().with_initial_mature_words(IMMIX_BREAK_EVEN_WORDS - 1),
    );
    let Some(mut rt) = runtime_with(config) else {
        return;
    };
    let spawned = spawned_strategy(&mut rt);
    assert_eq!(spawned.kind, MatureKind::Copying);
    assert_eq!(spawned.source, MatureSource::Automatic);
    assert_eq!(spawned.mature_words, IMMIX_BREAK_EVEN_WORDS - 1);
    assert!(!spawned.is_override);
}

/// What one scheduled run of the fixture workload did: its value and the runtime's collection
/// counters across the run.
#[derive(Debug, Clone, Copy)]
struct WorkloadRun {
    /// The loop's returned value.
    value: i64,
    /// Collections the runtime ran during the run.
    collections: u64,
    /// How many of them were fullsweeps — the only collection that touches the mature
    /// generation, i.e. the code path the selected strategy changes.
    major: u64,
}

/// Runs `heap_growth:run(N)` as a real scheduled process (one runner slice at a time, bounded
/// by [`WORKLOAD_DEADLINE`]) and reports its value and the run's collection counters.
fn run_workload(rt: &mut Runtime, n: i64) -> WorkloadRun {
    let before = rt.gc_stats();
    let pid = rt
        .spawn_process("heap_growth", "run", vec![make_small(n)])
        .expect("the workload is spawned");
    let deadline = Instant::now() + WORKLOAD_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "heap_growth:run/1 with N={n} did not terminate within {}s",
            WORKLOAD_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
    }
    let after = rt.gc_stats();
    let value = rt.process_result(pid).expect("the loop returned a value");
    WorkloadRun {
        value: signed_value(value),
        collections: after.collections - before.collections,
        major: after.major - before.major,
    }
}

/// Asserts every "something went wrong" counter of the runtime is still clean.
fn assert_no_gc_failures(rt: &Runtime, run: &WorkloadRun) {
    let stats = rt.gc_stats();
    assert_eq!(stats.errors, 0, "no collection failed; {stats:?} {run:?}");
    assert!(!stats.disabled, "no valve tripped; {stats:?} {run:?}");
    assert_eq!(stats.halted, 0, "no process halted; {stats:?} {run:?}");
    assert_eq!(stats.valve_trips, 0, "no escape-store valve; {stats:?} {run:?}");
}

/// **Acceptance 5 — the config-implied strategy really runs a fullsweep workload.**
///
/// A "selected" strategy is only evidence if it runs code. The same fixture is run as a real
/// scheduled process on both runtimes, with `fullsweep_after(0)` so **every** collection is a
/// fullsweep (`HeapPolicy::fullsweep_due`: `minor_gcs >= 0` always holds) — the only
/// collection that touches the mature generation, and therefore the only way a bounded
/// workload can drive the mark-region space at all.
///
/// Both runs must return their argument and finish with a clean counter set: the default
/// (copying) behaviour is unchanged, and the config-implied mark-region strategy is live —
/// its mature space is reached (`major > 0`), and not one collection fails, halts a process or
/// trips a valve.
#[test]
fn the_config_implied_strategy_runs_a_fullsweep_workload() {
    if host_opted_into_immix("gc_strategy") {
        return;
    }
    // The default configuration, forced into a fullsweep on every collection.
    let default_config = RuntimeConfig::new()
        .with_heap_options(HeapOptions::new().with_fullsweep_after(0));
    let Some(mut default_rt) = runtime_with(default_config) else {
        return;
    };
    let default_run = run_workload(&mut default_rt, WORKLOAD_ITERATIONS);
    assert_eq!(default_run.value, WORKLOAD_ITERATIONS);
    assert!(default_run.collections > 0, "the workload collects; {default_run:?}");
    assert_eq!(
        default_run.major, default_run.collections,
        "fullsweep_after 0 makes every collection a fullsweep; {default_run:?}"
    );
    assert_no_gc_failures(&default_rt, &default_run);

    // The same workload under the config-implied mark-region strategy.
    let immix_config = RuntimeConfig::new().with_heap_options(
        HeapOptions::new()
            .with_initial_mature_words(IMMIX_BREAK_EVEN_WORDS)
            .with_fullsweep_after(0),
    );
    let Some(mut immix_rt) = runtime_with(immix_config) else {
        return;
    };
    let immix_run = run_workload(&mut immix_rt, WORKLOAD_ITERATIONS);
    assert_eq!(
        immix_run.value, WORKLOAD_ITERATIONS,
        "the mark-region strategy must run the workload correctly; {immix_run:?}"
    );
    assert!(
        immix_run.major > 0,
        "the workload must really have driven the mature generation; {immix_run:?}"
    );
    assert_eq!(
        immix_run.major, immix_run.collections,
        "fullsweep_after 0 makes every collection a fullsweep; {immix_run:?}"
    );
    assert_no_gc_failures(&immix_rt, &immix_run);
}

/// **Acceptance 6 — a LIVE payload survives real collections on the mark-region path.**
///
/// Acceptances 3–5 prove the *strategy*; this one proves the mark-region path **collects a
/// running process correctly**, which is a different claim: the strategy test never runs a
/// process at all, and the fullsweep test of acceptance 5 runs a workload whose whole live
/// set is a single integer. Here a **live, structurally checkable payload** — the 64-element
/// list `tests/fixtures/gc_payload.beam`'s `run/1` builds and keeps reachable across every
/// round of short-lived churn — is held across real collections driven by the runtime's own
/// GC doors (the engine's heap-admission point), with `fullsweep_after(0)` so **every**
/// collection is a fullsweep — the one collection that touches (and, on this strategy,
/// relocates or pins) the mature generation the payload is promoted into.
///
/// Asserted: the process's own collector really is the mark-region one while it runs; the
/// workload returned its payload; **every element of that payload is intact**; the run drove
/// collections and fullsweeps (`gc_stats().collections > 0`, `major == collections`); and no
/// collection failed, halted a process or tripped a valve.
///
/// The payload check is deliberately structural rather than `x(0) == N`: a collection that
/// relocated the heap against an incomplete root set would leave the payload's X/Y register
/// (or a pointer it holds) dangling, so the term would come back with a wrong element, a
/// wrong field, or not be a proper list at all — the assertion genuinely fails in that case.
///
/// **The assertion with no pre-change counterpart** is `the process's collector is
/// `MatureKind::Immix`` **while it holds a live payload across a fullsweep**: before the
/// configuration knobs, the runtime's own path could not reach the mark-region strategy at
/// all (`initial_mature_words` was fixed at the emulator default, far below the break-even),
/// so no live-payload collection could ever run on the Immix path.
#[test]
fn a_live_payload_survives_a_fullsweep_on_the_immix_path() {
    if host_opted_into_immix("gc_strategy") {
        return;
    }
    let config = RuntimeConfig::new().with_heap_options(
        HeapOptions::new()
            .with_initial_mature_words(IMMIX_BREAK_EVEN_WORDS)
            .with_fullsweep_after(0),
    );
    let Some(mut rt) = runtime_with_fixture(config, GC_PAYLOAD_BEAM) else {
        return;
    };
    let before = rt.gc_stats();
    let pid = rt
        .spawn_process("gc_payload", "run", vec![make_small(PAYLOAD_ROUNDS)])
        .expect("the payload workload is spawned");

    // The strategy is fixed at spawn time; the collection counters are cumulative, so the
    // last sample taken **while the process is still in the table** (the scheduler removes a
    // terminated process at once) reports the whole run.
    let mut kind = None;
    let mut fullsweeps = 0u64;
    let deadline = Instant::now() + WORKLOAD_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "gc_payload:run/1 with N={PAYLOAD_ROUNDS} did not terminate within {}s",
            WORKLOAD_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
        if let Some(process) = rt.processes().get(pid) {
            let collector = process.collector();
            kind = Some(collector.kind());
            fullsweeps = collector.stats().major_collections;
        }
    }

    // 1. The mark-region strategy really ran the process.
    assert_eq!(
        kind,
        Some(MatureKind::Immix),
        "the spawned process must collect with the configured mark-region strategy"
    );
    // 2. It really ran fullsweeps on that strategy.
    assert!(
        fullsweeps > 0,
        "fullsweep_after 0 makes every collection a fullsweep; saw {fullsweeps}"
    );
    // 3. The payload came back intact, element by element.
    let payload = rt.process_result(pid).expect("the workload returned its payload");
    support::check_gc_payload(payload, PAYLOAD_LENGTH);
    // 4. The runtime's own counters agree, and nothing failed.
    let after = rt.gc_stats();
    let collections = after.collections - before.collections;
    assert!(collections > 0, "the workload really collected; {after:?}");
    assert_eq!(
        after.major - before.major,
        collections,
        "fullsweep_after 0 makes every collection a fullsweep; {after:?}"
    );
    assert_eq!(after.errors, 0, "no collection failed; {after:?}");
    assert_eq!(after.halted, 0, "no process halted its collection; {after:?}");
    assert_eq!(after.valve_trips, 0, "no escape-store valve tripped; {after:?}");
    assert!(!after.disabled, "collection was never disabled; {after:?}");
}
