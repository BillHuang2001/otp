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

//! Hot-path micro-benchmarks + a leak/steady-state baseline test for the
//! `beam_alloc` allocator.
//!
//! There is no `criterion`/`cargo bench` wiring in this workspace (the crate's
//! dependency set is frozen and a bench harness is a build-system decision, not a
//! crate decision), so this file is a plain `#[test]` binary that measures with
//! [`std::time::Instant`] and prints ns/op to stdout. Run it with `--nocapture`:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_alloc --test hot_path -- --nocapture
//! ```
//!
//! # What is measured
//!
//! 1. [`bench_size_class_lookup`] — `SizeClasses::class_for` over a boundary-heavy,
//!    pseudo-random size mix (ns/op), including a driver-only baseline so the
//!    lookup's own share of the loop can be read off.
//! 2. [`bench_alloc_free_hot_path`] — `alloc` + `dealloc` of a small block at
//!    [`MIN_ALIGN`], in three shapes: a single immediately-freed block, a 256-block
//!    working set, and a 256-block working set across all size classes of the
//!    allocator's default table.
//! 3. [`bench_bulk_mixed_workload`] — a random-size batch of blocks allocated and
//!    then freed, reported as total wall time and ns/op per block.
//! 4. [`allocator_reaches_a_steady_state_without_leaking`] — the *correctness*
//!    half: repeated full alloc/free cycles on a dedicated allocator must return
//!    `live_blocks`/`live_bytes` to zero and must not ratchet the carrier high-water
//!    mark upwards.
//!
//! # Method / caveats
//!
//! * Timings are **printed only** — no test in this file asserts a wall-clock
//!   threshold (that would be flaky under a shared CI machine). Only the leak test
//!   asserts, and it asserts on counters alone.
//! * The measurements run in the dev profile (`opt-level=1` for the crate itself),
//!   so the absolute numbers are *not* production numbers; they are a regression
//!   baseline for the allocator's own hot path (`Allocator::alloc` → free-list pop
//!   → `Allocator::dealloc` → free-list push, all behind the per-allocator mutex).
//! * [`black_box`] is applied to the inputs *and* the results of every measured
//!   call, and the loop bodies accumulate a checksum that is `black_box`ed after
//!   the loop, so no iteration (and no call) can be optimised away or hoisted.
//! * The timing tests serialise on a process-wide mutex: the Rust test harness
//!   runs tests in threads by default, and measuring several allocators
//!   concurrently would distort every number.
//! * Everything here uses a *dedicated* `CarrierAllocator` instance, never the
//!   process-wide registry ([`beam_alloc::default_allocator`]), because the shared
//!   instance's counters are visible to every caller in the process and would make
//!   the leak assertions racy.
//! * This file consumes the crate **only** through its stable public API
//!   (`Allocator`/`AllocRef`/`AllocatorConfig`/`CarrierAllocator`/`SizeClasses`/
//!   `AllocStats`), so it keeps compiling while `src/carrier.rs`,
//!   `src/free_list.rs`, `src/error.rs` and `src/allocator.rs` are reworked
//!   internally.

use std::alloc::Layout;
use std::hint::black_box;
use std::ptr;
use std::ptr::NonNull;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use beam_alloc::prelude::*;
/// The number of iterations of the tight alloc/free loops.
const ALLOC_ITERS: u64 = 2_000_000;

/// The number of iterations of the size-class lookup loop.
const LOOKUP_ITERS: u64 = 4_000_000;

/// The working-set size of the "several blocks live" variants.
const WORKING_SET: usize = 256;

/// The number of blocks allocated and freed in the bulk workload.
const BULK_BLOCKS: usize = 65_536;

/// The number of bulk rounds (the first round also maps the carriers).
const BULK_ROUNDS: usize = 3;

/// The number of full alloc/free cycles in the leak/steady-state test.
const LEAK_CYCLES: usize = 4;

/// Serialises the timing tests so they do not compete for the machine (and so the
/// printed ns/op describe one loop at a time). Poisoning is irrelevant — the guard
/// protects nothing but the timing, so take the inner value instead of panicking.
static BENCH_LOCK: Mutex<()> = Mutex::new(());

fn bench_guard() -> MutexGuard<'static, ()> {
    BENCH_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A deterministic 64-bit LCG (the same generator shape the sibling
/// `tests/allocator.rs` uses): reproducible workload mixes without adding a
/// random-number dependency to a crate whose dependency set is frozen.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Lcg {
        Lcg(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// A value in `0..bound`; `bound` must be non-zero.
    fn below(&mut self, bound: usize) -> usize {
        assert!(bound > 0, "`Lcg::below` needs a non-empty range");
        (self.next() % bound as u64) as usize
    }
}

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).expect("valid layout")
}

fn ns_per_op(elapsed: Duration, ops: u64) -> f64 {
    elapsed.as_secs_f64() * 1.0e9 / ops as f64
}

/// A dedicated allocator with a small carrier, so the workload has to carve and
/// reuse several carriers instead of living inside one mapping. The size-class
/// table and every other setting stay at their production defaults.
fn hot_path_allocator(name: &str, carrier_size: usize) -> CarrierAllocator {
    let config = AllocatorConfig::builder()
        .name(name)
        .carrier_size(carrier_size)
        .build();
    CarrierAllocator::new(config).expect("allocator construction maps nothing")
}

/// The class sizes of the allocator's default size-class table.
fn default_class_sizes() -> Vec<usize> {
    SizeClasses::default_small_block().iter().collect()
}

/// A boundary-heavy size mix for the lookup benchmark: every class size of the
/// table plus one byte on either side of it (so both the "exact class" and the
/// "round up to the next class" branches are hit), padded with uniform-random
/// sizes across the table's whole accepted range — including sizes above
/// `large_threshold()`, which must return `None` (a dedicated carrier).
fn boundary_size_mix(classes: &SizeClasses, count: usize, seed: u64) -> Vec<usize> {
    let mut rng = Lcg::new(seed);
    let mut mix = Vec::with_capacity(count.max(3 * classes.len() + 1));
    for class in classes.iter() {
        mix.push(class.saturating_sub(1));
        mix.push(class);
        mix.push(class.saturating_add(1));
    }
    mix.push(classes.large_threshold().saturating_add(1));
    while mix.len() < count {
        mix.push(rng.below(classes.large_threshold().saturating_add(2)));
    }
    mix.truncate(count);
    mix
}

// ---------------------------------------------------------------------------
// 1. Size-class lookup
// ---------------------------------------------------------------------------

/// Times `classes.class_for(size)` over `sizes`, returning the elapsed time and a
/// running checksum. The checksum is returned so the caller can `black_box` it;
/// every call takes `black_box`ed input and its result feeds the checksum, so the
/// call cannot be removed.
fn time_class_for(
    classes: &SizeClasses,
    sizes: &[usize],
    iters: u64,
    seed: u64,
) -> (Duration, u64) {
    let mut rng = Lcg::new(seed);
    let mut checksum = 0u64;
    let start = Instant::now();
    for _ in 0..iters {
        let size = black_box(sizes[rng.below(sizes.len())]);
        let class = classes.class_for(size);
        checksum = checksum.wrapping_add(class.unwrap_or(usize::MAX) as u64);
    }
    let elapsed = start.elapsed();
    black_box(checksum);
    (elapsed, iters)
}

/// The same loop with the lookup removed: it isolates the cost of the LCG, the
/// index into `sizes` and the accumulator, i.e. the "driver" overhead that is
/// included in [`time_class_for`]'s number. Subtracting the two gives the
/// lookup's own share of the loop.
fn time_driver_only(sizes: &[usize], iters: u64, seed: u64) -> Duration {
    let mut rng = Lcg::new(seed);
    let mut checksum = 0u64;
    let start = Instant::now();
    for _ in 0..iters {
        let size = black_box(sizes[rng.below(sizes.len())]);
        checksum = checksum.wrapping_add(size as u64);
    }
    let elapsed = start.elapsed();
    black_box(checksum);
    elapsed
}

#[test]
fn bench_size_class_lookup() {
    let _guard = bench_guard();
    let default = SizeClasses::default_small_block();
    let legacy = SizeClasses::default_legacy();

    for (label, classes) in [
        ("default_small_block", &default),
        ("default_legacy", &legacy),
    ] {
        let sizes = boundary_size_mix(classes, 4096, 0x51E5_00D5);
        let (elapsed, iters) = time_class_for(classes, &sizes, LOOKUP_ITERS, 0xC0FF_EE01);
        let driver = time_driver_only(&sizes, LOOKUP_ITERS, 0xC0FF_EE01);
        let per_op = ns_per_op(elapsed, iters);
        let driver_per_op = ns_per_op(driver, iters);
        println!(
            "[beam_alloc hot_path] size-class lookup ({label}: {} classes, \
             large_threshold={} B, {} distinct sizes): {per_op:.2} ns/op \
             (driver-only baseline {driver_per_op:.2} ns/op, lookup share \
             {:.2} ns/op) over {iters} ops",
            classes.len(),
            classes.large_threshold(),
            sizes.len(),
            per_op - driver_per_op,
        );
        // Sanity-check the mix is what this benchmark claims it is: it must reach
        // both the "served from a class" and the "dedicated carrier" outcomes.
        assert!(
            sizes.iter().any(|&s| classes.class_for(s).is_some()),
            "the size mix must contain sizes served from a size class"
        );
        assert!(
            sizes.iter().any(|&s| classes.class_for(s).is_none()),
            "the size mix must contain sizes that need a dedicated carrier"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Alloc / free hot path
// ---------------------------------------------------------------------------

/// Allocates `live` blocks, then repeatedly frees one slot of the working set and
/// refills it with a freshly allocated block of a size drawn from `sizes`. Every
/// iteration is therefore exactly one `alloc` + one `dealloc`, with `live` blocks
/// outstanding at all times (`live == 1` is the "alloc + immediately free"
/// variant).
///
/// `black_box` is applied to the returned pointers (and to the checksum built from
/// them), so neither the allocations nor the writes of the slot table can be
/// elided.
fn time_alloc_free_ring(
    alloc: &dyn Allocator,
    iters: u64,
    live: usize,
    sizes: &[usize],
    seed: u64,
) -> Duration {
    assert!(live > 0, "the working set must be non-empty");
    assert!(!sizes.is_empty(), "the size mix must be non-empty");

    let mut rng = Lcg::new(seed);
    let mut slots: Vec<(NonNull<u8>, Layout)> = Vec::with_capacity(live);
    for index in 0..live {
        let size = sizes[index % sizes.len()];
        let l = layout(size, MIN_ALIGN);
        slots.push((alloc.alloc(l).expect("alloc"), l));
    }

    let mut checksum = 0usize;
    let start = Instant::now();
    for iteration in 0..iters {
        let slot = iteration as usize % live;
        let (old_ptr, old_layout) = slots[slot];
        // SAFETY: the pointer is live, it was handed out by `alloc` on this
        // allocator with exactly `old_layout`, and the slot is overwritten below
        // before it can be used again.
        unsafe { alloc.dealloc(old_ptr, old_layout) };
        let size = sizes[rng.below(sizes.len())];
        let l = layout(size, MIN_ALIGN);
        let ptr = alloc.alloc(l).expect("alloc");
        checksum = checksum.wrapping_add(black_box(ptr.as_ptr()) as usize);
        slots[slot] = (ptr, l);
    }
    let elapsed = start.elapsed();
    black_box(checksum);

    for (ptr, l) in slots.drain(..) {
        // SAFETY: each slot is drained exactly once and holds a live allocation
        // made with exactly `l`.
        unsafe { alloc.dealloc(ptr, l) };
    }
    elapsed
}

#[test]
fn bench_alloc_free_hot_path() {
    let _guard = bench_guard();
    let classes = default_class_sizes();
    let small = layout(64, MIN_ALIGN).size();

    // One immediately-freed block: the pure alloc/free-lock round trip.
    let alloc = hot_path_allocator("hot-path-immediate", 64 * 1024);
    let elapsed = time_alloc_free_ring(&alloc, ALLOC_ITERS, 1, &[small], 0xA11C_0001);
    println!(
        "[beam_alloc hot_path] alloc+free, 1 live block of {small} B at \
         {MIN_ALIGN}-byte alignment (64 KiB carriers): {:.2} ns/op over {ALLOC_ITERS} ops",
        ns_per_op(elapsed, ALLOC_ITERS)
    );
    let after = alloc.stats();
    assert_eq!(
        after.live_blocks, 0,
        "the immediate variant must free everything"
    );
    assert_eq!(after.live_bytes, 0);
    assert!(
        after.carriers >= 1,
        "the workload must have mapped a carrier"
    );

    // A small working set: the same block size, but out of the free list rather
    // than always the just-freed one.
    let alloc = hot_path_allocator("hot-path-working-set", 64 * 1024);
    let elapsed = time_alloc_free_ring(&alloc, ALLOC_ITERS, WORKING_SET, &[small], 0xA11C_0002);
    println!(
        "[beam_alloc hot_path] alloc+free, {WORKING_SET} live blocks of {small} B \
         (64 KiB carriers): {:.2} ns/op over {ALLOC_ITERS} ops",
        ns_per_op(elapsed, ALLOC_ITERS)
    );
    assert_eq!(alloc.stats().live_blocks, 0);
    assert_eq!(alloc.stats().live_bytes, 0);

    // Mixed size classes, the shape the copy engine/ETS actually produce.
    let alloc = hot_path_allocator("hot-path-mixed", 256 * 1024);
    let elapsed = time_alloc_free_ring(&alloc, ALLOC_ITERS, WORKING_SET, &classes, 0xA11C_0003);
    println!(
        "[beam_alloc hot_path] alloc+free, {WORKING_SET} live blocks over {} size \
         classes (16 B..1 KiB, 256 KiB carriers): {:.2} ns/op over {ALLOC_ITERS} ops",
        classes.len(),
        ns_per_op(elapsed, ALLOC_ITERS)
    );
    assert_eq!(alloc.stats().live_blocks, 0);
    assert_eq!(alloc.stats().live_bytes, 0);
}

// ---------------------------------------------------------------------------
// 3. Bulk mixed workload
// ---------------------------------------------------------------------------

/// A batch mix: mostly the default table's class sizes, with one in eight requests
/// large enough to need a dedicated single-block carrier. Returns the mix and the
/// sizes that must take the dedicated path.
fn bulk_size_mix() -> (Vec<usize>, Vec<usize>) {
    let classes = default_class_sizes();
    let mut mix: Vec<usize> = Vec::with_capacity(256);
    let mut dedicated: Vec<usize> = Vec::new();
    for index in 0..256 {
        if index % 8 == 7 {
            // Above `large_threshold()` (1 KiB for the default table).
            let size = 2048 * (1 + index % 4);
            // Only record sizes the table really cannot serve.
            if SizeClasses::default_small_block().class_for(size).is_none() {
                dedicated.push(size);
            }
            mix.push(size);
        } else {
            mix.push(classes[index % classes.len()]);
        }
    }
    (mix, dedicated)
}

#[test]
fn bench_bulk_mixed_workload() {
    let _guard = bench_guard();
    let (mix, dedicated) = bulk_size_mix();
    let alloc = hot_path_allocator("hot-path-bulk", 256 * 1024);
    let mut rng = Lcg::new(0xB01C_0017);

    for round in 0..BULK_ROUNDS {
        let mut live: Vec<(NonNull<u8>, Layout)> = Vec::with_capacity(BULK_BLOCKS);
        let alloc_start = Instant::now();
        for _ in 0..BULK_BLOCKS {
            let size = mix[rng.below(mix.len())];
            let l = layout(size, MIN_ALIGN);
            let ptr = alloc.alloc(l).expect("alloc");
            live.push((black_box(ptr), l));
        }
        let alloc_elapsed = alloc_start.elapsed();

        let during = alloc.stats();
        assert_eq!(
            during.live_blocks, BULK_BLOCKS as u64,
            "round {round}: every block must be accounted for"
        );

        let free_start = Instant::now();
        for (ptr, l) in live.drain(..) {
            // SAFETY: each block is live, was allocated with exactly `l` on this
            // allocator, and is freed exactly once.
            unsafe { alloc.dealloc(black_box(ptr), l) };
        }
        let free_elapsed = free_start.elapsed();

        let total = alloc_elapsed + free_elapsed;
        println!(
            "[beam_alloc hot_path] bulk mixed round {round}: {BULK_BLOCKS} blocks \
             (alloc {:.2} ms + free {:.2} ms = {:.2} ms, {:.2} ns/op per \
             alloc+free pair; {} sizes incl. {} dedicated-carrier sizes)",
            alloc_elapsed.as_secs_f64() * 1.0e3,
            free_elapsed.as_secs_f64() * 1.0e3,
            total.as_secs_f64() * 1.0e3,
            ns_per_op(total, 2 * BULK_BLOCKS as u64),
            mix.len(),
            dedicated.len(),
        );

        let after = alloc.stats();
        assert_eq!(after.live_blocks, 0, "round {round} leaked blocks");
        assert_eq!(after.live_bytes, 0, "round {round} leaked bytes");
    }
}

// ---------------------------------------------------------------------------
// 4. Leak / baseline: steady state over repeated alloc/free cycles
// ---------------------------------------------------------------------------

/// Repeats identical full alloc/free cycles on a dedicated allocator and asserts
/// the allocator is leak-free *and* at a steady state:
///
/// * `live_blocks == 0` and `live_bytes == 0` after every cycle — every block that
///   was handed out (including the 0-byte and `SizeClasses::large_threshold()`-
///   exceeding dedicated-carrier sizes, and the blocks grown and shrunk by
///   `realloc`) was handed back, and none is still accounted for.
/// * `peak_carrier_bytes` is the same after cycle 2..N as after cycle 1 — the
///   allocator does not ratchet its carrier high-water mark upwards for a repeat of
///   the same workload, i.e. it reuses what it mapped (a per-cycle growth would be
///   the signature of a carrier leak).
/// * the number of carriers mapped while the whole batch is live never exceeds what
///   the first cycle needed.
///
/// Nothing here asserts wall-clock time, so the test cannot flake; the counters it
/// prints *are* the baseline. Call counters (`alloc_calls`/`free_calls`/
/// `realloc_calls`) are printed but deliberately not asserted: their exact
/// accounting rules for the `realloc`-moves-a-block and dedicated-carrier paths are
/// the allocator's own business, while `live_blocks`/`live_bytes`/`peak_carrier_bytes`
/// are the leak-relevant observables.
#[test]
fn allocator_reaches_a_steady_state_without_leaking() {
    let alloc = hot_path_allocator("hot-path-leak", 64 * 1024);

    // Carriers are mapped lazily: constructing an allocator must not leak into
    // `live_blocks`/`live_bytes`/`carriers` before the first allocation.
    let baseline = alloc.stats();
    assert_eq!(
        baseline.live_blocks, 0,
        "no block may be live before the first alloc"
    );
    assert_eq!(
        baseline.live_bytes, 0,
        "no byte may be live before the first alloc"
    );
    assert_eq!(baseline.carriers, 0, "carriers must be mapped lazily");

    // The per-cycle batch: 0-byte and 1-byte edge cases, every class size of the
    // default table with its ±1 neighbours (so "fits the class" and "rounds up a
    // class" both happen), and sizes above the table's threshold (2048, 64 KiB and
    // 1 MiB), which need a dedicated single-block carrier.
    let mut sizes: Vec<usize> = vec![0, 1, 8, 16];
    for class in default_class_sizes() {
        sizes.push(class.saturating_sub(1));
        sizes.push(class);
        sizes.push(class.saturating_add(1));
    }
    sizes.extend([2048, 64 * 1024, 1024 * 1024]);

    let mut first_peak_carrier_bytes: Option<usize> = None;
    let mut first_live_carriers: Option<u64> = None;

    for cycle in 0..LEAK_CYCLES {
        let mut live: Vec<(NonNull<u8>, Layout)> = Vec::with_capacity(sizes.len());
        for &size in &sizes {
            let l = layout(size, MIN_ALIGN);
            let ptr = alloc.alloc(l).expect("alloc");
            if size > 0 {
                // Touch the first and the last byte: a block that is too short
                // would corrupt neighbouring blocks here.
                // SAFETY: the allocation holds `size` writable bytes.
                unsafe {
                    ptr::write_bytes(ptr.as_ptr(), 0xA5, 1);
                    ptr::write_bytes(ptr.as_ptr().add(size - 1), 0x5A, 1);
                }
            }
            live.push((black_box(ptr), l));
        }

        let during = alloc.stats();
        assert_eq!(
            during.live_blocks,
            live.len() as u64,
            "cycle {cycle}: every allocated block must be live"
        );
        assert!(
            during.live_bytes > 0,
            "cycle {cycle}: live bytes must be accounted"
        );

        // A `realloc` round trip, on top of the batch: grow a small block across
        // several classes (a real move + copy) and shrink it back in place. The
        // payload must survive both directions, and the call must leave no block
        // behind.
        {
            let l = layout(16, MIN_ALIGN);
            let ptr = alloc.alloc(l).expect("alloc");
            // SAFETY: 16 writable bytes.
            unsafe { ptr::write_bytes(ptr.as_ptr(), 0xC3, 16) };
            let grown = alloc.realloc(ptr, l, 4096).expect("realloc grow");
            assert_eq!(
                grown.as_ptr() as usize % MIN_ALIGN,
                0,
                "cycle {cycle}: realloc must preserve {MIN_ALIGN}-byte alignment"
            );
            for offset in 0..16 {
                // SAFETY: the grown block holds at least the 16 initialised bytes.
                assert_eq!(
                    unsafe { *grown.as_ptr().add(offset) },
                    0xC3,
                    "cycle {cycle}: realloc lost byte {offset} of the payload"
                );
            }
            let shrunk = alloc
                .realloc(grown, layout(4096, MIN_ALIGN), 32)
                .expect("realloc shrink");
            for offset in 0..16 {
                // SAFETY: the shrunk block holds at least the 16 initialised bytes.
                assert_eq!(
                    unsafe { *shrunk.as_ptr().add(offset) },
                    0xC3,
                    "cycle {cycle}: realloc shrink lost byte {offset} of the payload"
                );
            }
            // SAFETY: `shrunk` is live and is freed exactly once here.
            unsafe { alloc.dealloc(black_box(shrunk), layout(32, MIN_ALIGN)) };
        }
        assert_eq!(
            alloc.stats().live_blocks,
            live.len() as u64,
            "cycle {cycle}: the realloc round trip must leave exactly the batch live"
        );

        for (ptr, l) in live.drain(..) {
            // SAFETY: each block in the batch is live, was allocated with exactly
            // `l`, and is freed exactly once.
            unsafe { alloc.dealloc(black_box(ptr), l) };
        }

        let after = alloc.stats();
        assert_eq!(after.live_blocks, 0, "cycle {cycle} leaked blocks");
        assert_eq!(after.live_bytes, 0, "cycle {cycle} leaked bytes");

        let live_words = during.carriers;
        match (first_peak_carrier_bytes, first_live_carriers) {
            (None, None) => {
                // The counters after the first cycle are the baseline the later
                // cycles are compared against.
                assert_eq!(cycle, 0, "the baseline must be taken from cycle 0");
                first_peak_carrier_bytes = Some(after.peak_carrier_bytes);
                first_live_carriers = Some(live_words);
            }
            (Some(peak0), Some(carriers0)) => {
                assert_eq!(
                    after.peak_carrier_bytes, peak0,
                    "cycle {cycle} grew the carrier high-water mark beyond cycle 0's \
                     {peak0} B: the allocator is not reusing its carriers across \
                     identical cycles"
                );
                assert!(
                    live_words <= carriers0,
                    "cycle {cycle} needed {} carriers while the batch was live, more \
                     than cycle 0's {carriers0}",
                    live_words
                );
            }
            _ => unreachable!("the two baselines are set together"),
        }

        println!(
            "[beam_alloc hot_path] leak cycle {cycle}: live {}/{} blocks/bytes after \
             free, peak_live_bytes={}, carriers={} (peak {} B, {} B held), \
             alloc_calls={} free_calls={} realloc_calls={}",
            after.live_blocks,
            after.live_bytes,
            after.peak_live_bytes,
            after.carriers,
            after.peak_carrier_bytes,
            after.carrier_bytes,
            after.alloc_calls,
            after.free_calls,
            after.realloc_calls,
        );
    }

    // The allocator ends where it started, apart from the carriers it keeps for
    // reuse (the documented M1 retention policy).
    let final_stats = alloc.stats();
    assert_eq!(final_stats.live_blocks, 0);
    assert_eq!(final_stats.live_bytes, 0);
    assert!(
        final_stats.carriers >= 1,
        "the workload must have exercised carriers"
    );
    println!(
        "[beam_alloc hot_path] leak summary: {} cycles x {} blocks, steady state at \
         peak_carrier_bytes={} across {} carriers ({} bytes)",
        LEAK_CYCLES,
        sizes.len(),
        final_stats.peak_carrier_bytes,
        final_stats.carriers,
        final_stats.carrier_bytes,
    );
}
