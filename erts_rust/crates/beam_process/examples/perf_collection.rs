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
//! **Micro-benchmark: the per-collection bookkeeping cost (§ `PERF.md`).**
//!
//! One rooted collection (`Process::collect` → `run_rooted_collection`) gathers the process root
//! set, runs the collector over it, writes it back and then reclaims off-heap binaries. This
//! harness measures **only that scaffolding**, from a process whose live set is small and
//! constant, so the figure is the mutant-side cost of a collection rather than the copier's
//! (which is `beam_gc`'s node).
//!
//! It reports two kinds of number, and they answer different questions:
//!
//! * **Deterministic allocation counts** — a counting `#[global_allocator]` wraps
//!   `std::alloc::System`, so `allocs_per_collection` / `bytes_per_collection` are exact
//!   integers, reproducible on any host, and immune to load. This is what makes a claim about
//!   *reused buffers* checkable: a collection that reuses its buffers allocates strictly fewer
//!   times than one that rebuilds them.
//! * **Wall time per collection** (`ns_per_collection`) and the retired-instruction marginal a
//!   caller takes by differencing two runs at different `collections` counts — noisy on this
//!   shared host, so read it only through `/proc/loadavg` (printed before and after) and only as
//!   a ratio between two builds of this same file.
//!
//! The live set is deliberately non-trivial, because the reused buffers are seeded from it: four
//! queued messages (each with a three-word envelope root), two process-dictionary entries, one
//! saved argument and one host-published extra root
//! ([`Registers::extra_roots_mut`]). Every collection drains and re-installs the mailbox, so the
//! `drained` buffer is exercised as well as the root slice.
//!
//! ```text
//! erts_rust/scripts/cargo.sh run --manifest-path erts_rust/Cargo.toml --release \
//!     -p beam_process --example perf_collection -- [collections] [warmup]
//! ```
//!
//! Usage note: the file must stay **identical between the two builds it compares**, and it only
//! touches the crate's public API, so it compiles unchanged against an older `beam_process`.
use beam_alloc::Heap;
use beam_process::prelude::*;
use beam_term::prelude::make_small;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
/// Allocations performed so far (every `alloc`).
static ALLOCS: AtomicU64 = AtomicU64::new(0);
/// `realloc` calls so far (a growing `Vec` on the collection path shows up here).
static REALLOCS: AtomicU64 = AtomicU64::new(0);
/// Bytes requested so far, by `alloc` and by `realloc`.
static BYTES: AtomicU64 = AtomicU64::new(0);
/// The counting allocator: `System` plus three counters, so a run reports exact figures.
struct Counting;
// SAFETY: every method forwards to `System` unchanged (the same layout, the same pointer) and
// only adds relaxed counter increments on the side, so the `GlobalAlloc` contract — the returned
// block satisfies the layout and is freed by the matching `dealloc`/`realloc` — is exactly
// `System`'s. No `unsafe` escapes this impl.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: forwarded unchanged — the caller's layout is `System`'s precondition.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        REALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        // SAFETY: forwarded unchanged — the caller guarantees `ptr` came from this allocator with
        // `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged.
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;
/// `/proc/loadavg`'s first three fields — the host caveat every wall-clock figure carries.
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|_| "unknown".to_string())
}
/// Builds the measured process: four queued messages, two dictionary entries, one saved argument
/// and one extra root — the *shape* of a collection's root set, not its size.
///
/// The mailbox payloads are real heap terms (built on a scratch arena and **copied in** by the
/// delivery, so each collection relocates a live graph), while the dictionary / saved-argument /
/// extra-root slots hold immediates: they contribute root words — which is what sizes the root
/// slice — without inflating the copier's share of the row.
fn fixture() -> Process {
    let mut process = Process::new(ProcessId::new(1, 0)).expect("a fresh process");
    let from = ProcessId::new(2, 0).to_eterm();
    let mut arena = Heap::new(64).expect("the source arena allocates");
    for i in 0..4i64 {
        let payload = arena
            .tuple(&[
                make_small(i),
                make_small(i + 1),
                make_small(i + 2),
                make_small(i + 3),
            ])
            .expect("the payload fits the arena");
        process
            .deliver_simple(from, payload)
            .expect("the message fits the fresh nursery");
    }
    process
        .dict_mut()
        .put(make_small(900), make_small(901));
    process
        .dict_mut()
        .put(make_small(902), make_small(903));
    process.saved_args_mut().push(make_small(904));
    process
        .registers_mut()
        .extra_roots_mut()
        .push(make_small(905));
    process
}
/// One measured iteration's *live and garbage* shape: twelve abandoned tuples (garbage the next
/// collection reclaims) and one heap tuple published as an extra root, so the collection really
/// relocates a live young term — and its root slice is what the scratch buffers are seeded from.
///
/// The published root is popped again after the collection, so the live set does not grow.
fn iteration(process: &mut Process) {
    let words = [make_small(1), make_small(2), make_small(3), make_small(4)];
    for _ in 0..12 {
        process
            .heap_mut()
            .tuple(&words)
            .expect("the nursery has room for one more tuple");
    }
    let live = process
        .heap_mut()
        .tuple(&words)
        .expect("the nursery has room for the live tuple");
    process.registers_mut().extra_roots_mut().push(live);
    process.collect().expect("the collection");
    process.registers_mut().extra_roots_mut().pop();
}
fn main() {
    let mut args = std::env::args().skip(1);
    let collections: usize = args
        .next()
        .map(|a| a.parse().expect("a collection count"))
        .unwrap_or(2_000);
    let warmup: usize = args
        .next()
        .map(|a| a.parse().expect("a warm-up count"))
        .unwrap_or(200);
    println!("loadavg before: {}", loadavg());
    let mut process = fixture();
    // Warm up: capacity is reached on the first collection, so the measured window is steady
    // state (no first-touch growth of either the scratch or the nursery).
    for _ in 0..warmup {
        iteration(&mut process);
    }
    let allocs_before = ALLOCS.load(Ordering::Relaxed);
    let reallocs_before = REALLOCS.load(Ordering::Relaxed);
    let bytes_before = BYTES.load(Ordering::Relaxed);
    let started = Instant::now();
    let mut words_reclaimed = 0usize;
    for _ in 0..collections {
        iteration(&mut process);
        words_reclaimed = words_reclaimed.wrapping_add(process.collector().stats().words_reclaimed);
    }
    let elapsed = started.elapsed();
    let allocs = ALLOCS.load(Ordering::Relaxed) - allocs_before;
    let reallocs = REALLOCS.load(Ordering::Relaxed) - reallocs_before;
    let bytes = BYTES.load(Ordering::Relaxed) - bytes_before;
    let per = |n: u64| n as f64 / collections as f64;
    println!("collections: {collections} (warm-up {warmup})");
    println!(
        "allocs_per_collection: {:.2} (total {allocs})",
        per(allocs)
    );
    println!(
        "reallocs_per_collection: {:.2} (total {reallocs})",
        per(reallocs)
    );
    println!("bytes_per_collection: {:.1} (total {bytes})", per(bytes));
    println!(
        "ns_per_collection: {:.1}",
        elapsed.as_nanos() as f64 / collections as f64
    );
    println!("words_reclaimed_total: {words_reclaimed}");
    println!("minor_gcs: {}", process.collector().minor_gcs());
    println!(
        "young_capacity_words: {}",
        process.collector().young().capacity_words()
    );
    println!("loadavg after: {}", loadavg());
}
