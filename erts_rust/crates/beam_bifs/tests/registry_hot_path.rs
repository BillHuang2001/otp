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

//! Deterministic, dependency-free measurement of the [`BifRegistry`] hot path.
//!
//! `BifRegistry::lookup`/`contains` run on *every* registered BIF dispatch, so their
//! allocation behaviour is a hard performance contract, not an implementation detail.
//! This integration test pins it two ways:
//!
//! 1. **Allocations (the primary, deterministic evidence).** A counting
//!    `#[global_allocator]` wraps `std::alloc::System` with two `AtomicUsize`
//!    counters, armed per-thread so the test harness and the sibling timing test can
//!    never perturb a measurement. The registry is built *before* the measurement
//!    window (its owned `String` keys allocate at boot), then a few thousand hit/miss
//!    `lookup` and `contains` calls and a hit `call` are made inside the window; the
//!    delta must be **exactly 0**. This is host-load independent: an allocation either
//!    happens or it does not.
//! 2. **Cost (the regression tripwire).** `lookup`, `contains` and a hit `call` are
//!    timed over `>=100k` iterations, repeated `>=15` times; the median and p99 ns/call
//!    are printed to stderr (visible with `--nocapture`). Only a very loose sanity
//!    bound is asserted so the test cannot become a flaky timing assertion.
//!
//! `std`-only: no new dependencies. The `beam_bifs` lib is `#![deny(unsafe_code)]`,
//! but this file is its own crate root (the `#[global_allocator]` needs `unsafe impl`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext};
use beam_process::ProcessId;
use beam_term::{make_atom, AtomTable};

// ---------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc` calls made on an armed thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Number of bytes requested on an armed thread.
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring.
    ///
    /// The counters are process-global, but the *arming* is per-thread, so the test
    /// harness and the sibling timing test (which allocate while formatting output)
    /// can never perturb a measurement. That is what makes the allocation evidence
    /// deterministic rather than merely usually-zero.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Records one allocation of `bytes` on the current thread while it is armed.
///
/// `try_with` tolerates a thread whose TLS is being torn down; with a `const`-init
/// `Cell` there is no lazy allocation and no destructor to run, so this is safe to
/// call from inside the allocator.
#[inline]
fn record(bytes: usize) {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(bytes, Ordering::Relaxed);
        }
    });
}

/// `System` plus per-thread opt-in counting.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Count only the growth (a shrink is not an allocation event).
        record(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

// ---------------------------------------------------------------------------
// Measurement scaffolding
// ---------------------------------------------------------------------------

/// Serializes the tests in this binary so the timing test is not perturbed by the
/// allocation test running on another core.
static SERIAL: Mutex<()> = Mutex::new(());

/// Runs `body` with the counters armed on this thread and returns
/// `(allocations, bytes, result)`.
fn allocations_in<T>(body: impl FnOnce() -> T) -> (usize, usize, T) {
    // Force the TLS to initialize *before* the window opens.
    ARMED.with(|armed| armed.set(false));
    let before_allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = BYTES.load(Ordering::Relaxed);
    ARMED.with(|armed| armed.set(true));
    let result = body();
    ARMED.with(|armed| armed.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before_allocations;
    let bytes = BYTES.load(Ordering::Relaxed) - before_bytes;
    (allocations, bytes, result)
}

/// Times `iterations` calls of `op`, repeated `repeats` times, returning the ns/call of
/// each run.
fn time_repeated(repeats: usize, iterations: usize, mut op: impl FnMut()) -> Vec<f64> {
    let mut per_run = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        let start = Instant::now();
        for _ in 0..iterations {
            op();
        }
        let elapsed_ns = start.elapsed().as_nanos() as f64;
        per_run.push(elapsed_ns / iterations as f64);
    }
    per_run
}

/// The median of `samples` (sorts in place).
fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

/// The `p`-quantile of `samples` (sorts in place); `p` in `0.0..=1.0`.
fn percentile(samples: &mut [f64], p: f64) -> f64 {
    samples.sort_by(f64::total_cmp);
    let last = samples.len() - 1;
    let index = ((last as f64) * p).round() as usize;
    samples[index.min(last)]
}

/// The `(median, p99)` ns/call of a timing sample, plus the raw per-run numbers.
fn summarize(label: &str, samples: &mut Vec<f64>, iterations: usize) -> f64 {
    let runs = samples.len();
    let med = median(samples);
    let p99 = percentile(samples, 0.99);
    let raw: Vec<f64> = samples.iter().map(|v| (v * 100.0).round() / 100.0).collect();
    eprintln!(
        "registry_hot_path: {label:<24} median {med:9.2} ns/call   p99 {p99:9.2} ns/call   \
         ({runs} runs x {iterations})"
    );
    eprintln!("registry_hot_path: {label:<24} raw per-run ns/call: {raw:?}");
    med
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// A fixed, known-good registry context: `erlang:is_atom/1` is registered and its body
/// (`BifContext::boolean` → an interned-atom lookup) allocates nothing, so a `call` on
/// it measures the *dispatch* path alone.
fn scenario() -> (beam_bifs::BifRegistry, Heap, AtomTable, beam_term::Eterm) {
    let reg = default_registry();
    let heap = Heap::new(64).expect("test heap");
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false"]);
    let truthy = make_atom(atoms.intern("true"));
    (reg, heap, atoms, truthy)
}

#[test]
fn a_registry_hit_and_miss_allocate_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // Building the registry (637 owned `String` keys) and the context allocate; only
    // the lookups below are measured.
    let (reg, mut heap, atoms, truthy) = scenario();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    const ITERATIONS: usize = 5_000;

    // A HIT: `lookup`/`contains` on a registered key.
    let (hit_allocs, hit_bytes, _) = allocations_in(|| {
        let (module, function, arity) = black_box(("erlang", "is_atom", 1u32));
        for _ in 0..ITERATIONS {
            black_box(reg.lookup(black_box(module), black_box(function), black_box(arity)));
            black_box(reg.contains(black_box(module), black_box(function), black_box(arity)));
        }
    });

    // A MISS: unknown function, unknown module, and a wrong arity.
    let (miss_allocs, miss_bytes, _) = allocations_in(|| {
        let miss_function = black_box(("erlang", "no_such_bif", 1u32));
        let miss_module = black_box(("no_such_module", "is_atom", 1u32));
        let miss_arity = black_box(("erlang", "is_atom", 2u32));
        for _ in 0..ITERATIONS {
            black_box(reg.lookup(miss_function.0, miss_function.1, miss_function.2));
            black_box(reg.contains(miss_function.0, miss_function.1, miss_function.2));
            black_box(reg.lookup(miss_module.0, miss_module.1, miss_module.2));
            black_box(reg.contains(miss_module.0, miss_module.1, miss_module.2));
            black_box(reg.lookup(miss_arity.0, miss_arity.1, miss_arity.2));
            black_box(reg.contains(miss_arity.0, miss_arity.1, miss_arity.2));
        }
    });

    // A hit `call` of a registered BIF whose body allocates nothing.
    let (call_allocs, call_bytes, _) = allocations_in(|| {
        let args = black_box([truthy]);
        for _ in 0..ITERATIONS {
            black_box(reg.call("erlang", "is_atom", 1, &mut ctx, &args).ok());
        }
    });

    eprintln!(
        "registry_hot_path: allocations over {ITERATIONS} iterations \
         => hit lookups {hit_allocs} ({hit_bytes} bytes), \
         miss lookups {miss_allocs} ({miss_bytes} bytes), \
         hit calls {call_allocs} ({call_bytes} bytes)"
    );

    assert_eq!(hit_allocs, 0, "a `lookup`/`contains` hit must not allocate");
    assert_eq!(miss_allocs, 0, "a `lookup`/`contains` miss must not allocate");
    assert_eq!(
        call_allocs, 0,
        "a registered BIF `call` hit (allocation-free body) must not allocate"
    );
}

#[test]
fn registry_lookup_cost_is_reported() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    let (reg, mut heap, atoms, truthy) = scenario();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    const ITERATIONS: usize = 100_000;
    const REPEATS: usize = 15;

    let mut lookup_ns = time_repeated(REPEATS, ITERATIONS, || {
        let (module, function, arity) = black_box(("erlang", "is_atom", 1u32));
        black_box(reg.lookup(module, function, arity));
    });

    let mut contains_ns = time_repeated(REPEATS, ITERATIONS, || {
        let (module, function, arity) = black_box(("erlang", "is_atom", 1u32));
        black_box(reg.contains(module, function, arity));
    });

    let mut call_ns = time_repeated(REPEATS, ITERATIONS, || {
        let args = black_box([truthy]);
        black_box(reg.call("erlang", "is_atom", 1, &mut ctx, &args).ok());
    });

    let lookup_med = summarize("lookup (hit)", &mut lookup_ns, ITERATIONS);
    let contains_med = summarize("contains (hit)", &mut contains_ns, ITERATIONS);
    let call_med = summarize("call (hit)", &mut call_ns, ITERATIONS);

    // A deliberately loose tripwire: this catches an order-of-magnitude regression
    // (e.g. re-introducing an owned-key lookup) without becoming a timing assertion.
    for (label, med) in [
        ("lookup", lookup_med),
        ("contains", contains_med),
        ("call", call_med),
    ] {
        assert!(
            med < 2_000.0,
            "{label} median {med:.2} ns/call exceeds the 2 us sanity bound"
        );
    }
}
