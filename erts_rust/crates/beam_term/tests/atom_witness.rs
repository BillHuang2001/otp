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

//! Evidence for the O(1) append-only witness API of [`AtomTable`]: an
//! [`AtomSnapshot`] plus [`AtomTable::appended_since`] must validate a memoized image
//! of the table in **constant time** and **without allocating**, at any table size.
//!
//! Three properties are pinned, in the order they matter:
//!
//! 1. **Zero allocation (deterministic evidence).** A counting `#[global_allocator]`
//!    wraps `std::alloc::System` and arms its counters *per thread*, so the test
//!    harness and the sibling timing test can never perturb a measurement. The table is
//!    built outside the window; `snapshot` and a few hundred thousand
//!    `appended_since` calls run inside it. An allocation either happens or it does
//!    not — this evidence is host-load independent.
//! 2. **Length independence (the regression tripwire).** The same three scalar reads are
//!    timed on a 4-atom table and on a 200 000-atom table; the big-table figure must
//!    stay within a deliberately generous factor of the small-table figure. A scan over
//!    `names` (the `prefix_matches` shape this API replaces) would be ~50 000× at that
//!    size, so an accidental O(table) regression is caught immediately while the
//!    assertion stays far too loose to be flaky.
//! 3. **Soundness at scale.** A clone that diverges, a clone that does not, and an
//!    independently constructed table with identical names must all fail to validate,
//!    at 50 000 atoms — the size at which the property actually matters.
//!
//! `std`-only: no new dependencies, and the `beam_term` library itself stays free of
//! `unsafe` (this file is its own crate root, and only the `#[global_allocator]` needs
//! `unsafe impl`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use beam_term::{Atom, AtomSnapshot, AtomTable};

// ---------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc` calls made on an armed thread (monotonic).
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Number of `dealloc` calls made on an armed thread (monotonic).
static DEALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Bytes requested minus bytes released on an armed thread.
static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring. The counters are
    /// process-global, but *arming* is per-thread, so the test harness (which allocates
    /// while collecting results and formatting output) and the sibling timing test can
    /// never perturb a measurement. That is what makes the allocation evidence
    /// deterministic rather than merely usually-zero.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Records one allocation of `bytes` on the current thread while it is armed.
///
/// `try_with` tolerates a thread whose TLS is being torn down; with a `const`-init
/// `Cell` there is no lazy allocation and no destructor to run, so this is safe to call
/// from inside the allocator.
#[inline]
fn record_alloc(bytes: usize) {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed);
        }
    });
}

/// Records one release of `bytes` on the current thread while it is armed.
#[inline]
fn record_dealloc(bytes: usize) {
    let _ = ARMED.try_with(|armed| {
        if armed.get() {
            DEALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            LIVE_BYTES.fetch_sub(bytes, Ordering::Relaxed);
        }
    });
}

/// `System` plus per-thread opt-in counting.
struct CountingAllocator;

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_alloc(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record_dealloc(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_alloc(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if new_size > layout.size() {
            record_alloc(new_size - layout.size());
        } else {
            record_dealloc(layout.size() - new_size);
        }
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
/// `(allocations, deallocations, live bytes, result)`.
fn allocations_in<T>(body: impl FnOnce() -> T) -> (usize, usize, usize, T) {
    // Force the TLS to initialize *before* the window opens.
    ARMED.with(|armed| armed.set(false));
    let before_alloc = ALLOCATIONS.load(Ordering::Relaxed);
    let before_dealloc = DEALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = LIVE_BYTES.load(Ordering::Relaxed);
    ARMED.with(|armed| armed.set(true));
    let result = body();
    ARMED.with(|armed| armed.set(false));
    let allocs = ALLOCATIONS.load(Ordering::Relaxed).wrapping_sub(before_alloc);
    let deallocs = DEALLOCATIONS.load(Ordering::Relaxed).wrapping_sub(before_dealloc);
    let bytes = LIVE_BYTES.load(Ordering::Relaxed).wrapping_sub(before_bytes);
    (allocs, deallocs, bytes, result)
}

/// Times `iterations` calls of `op`, `repeats` times, returning the ns/call of each run.
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

/// The current 1-minute load average (the sampler this host exposes), for the report.
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or("?")
        .to_string()
}

/// A table holding `n` synthetic atoms.
fn table_with(n: usize) -> AtomTable {
    let mut t = AtomTable::new();
    t.seed(["true", "false", "ok", "error"][..n.min(4)].iter().copied());
    if n > 4 {
        let rest: Vec<String> = (4..n).map(|i| format!("synthetic_atom_{i}")).collect();
        t.seed(rest.iter().map(String::as_str));
    }
    t
}

// ---------------------------------------------------------------------------
// 1. Zero allocation
// ---------------------------------------------------------------------------

#[test]
fn snapshot_and_appended_since_allocate_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    // Built *outside* the window: 30 009 names is a table where a `names` scan would
    // allocate nothing either, but it makes the measured call the real one.
    let mut table = table_with(30_009);
    let other = table_with(30_009);

    const ITERATIONS: usize = 200_000;

    let (snap_allocs, snap_frees, snap_bytes, snapshot) =
        allocations_in(|| black_box(table.snapshot()));
    let (hit_allocs, hit_frees, hit_bytes, hit) = allocations_in(|| {
        let mut hits = 0usize;
        for _ in 0..ITERATIONS {
            hits += black_box(table.appended_since(black_box(snapshot))) as usize;
        }
        black_box(hits)
    });
    let (miss_allocs, miss_frees, miss_bytes, miss) = allocations_in(|| {
        let mut hits = 0usize;
        for _ in 0..ITERATIONS {
            hits += black_box(table.appended_since(black_box(other.snapshot()))) as usize;
        }
        black_box(hits)
    });
    // A snapshot, then appends, then a validating check — the real protocol. Only the
    // check itself is inside the window.
    let before = table.snapshot();
    table.seed(["late_one", "late_two"]);
    let (extend_allocs, extend_frees, extend_bytes, extended) = allocations_in(|| {
        let mut hits = 0usize;
        for _ in 0..ITERATIONS {
            hits += black_box(table.appended_since(black_box(before))) as usize;
        }
        black_box(hits)
    });

    assert_eq!(snap_allocs, 0, "`snapshot()` must not allocate");
    assert_eq!(hit_allocs, 0, "a hit `appended_since` must not allocate");
    assert_eq!(miss_allocs, 0, "a miss `appended_since` must not allocate");
    assert_eq!(
        extend_allocs, 0,
        "an `appended_since` after an append must not allocate"
    );
    for (label, frees, bytes) in [
        ("snapshot", snap_frees, snap_bytes),
        ("hit", hit_frees, hit_bytes),
        ("miss", miss_frees, miss_bytes),
        ("extend", extend_frees, extend_bytes),
    ] {
        assert_eq!(frees, 0, "`{label}` must not free");
        assert_eq!(bytes, 0, "`{label}` must have zero net live bytes");
    }

    // The measured calls did what they claim, so the zero above is not vacuous.
    assert_eq!(snapshot.len(), 30_009);
    assert_eq!(hit, ITERATIONS);
    assert_eq!(miss, 0);
    assert_eq!(extended, ITERATIONS);
    eprintln!(
        "atom_witness: arm check over {ITERATIONS} iterations => \
         snapshot {snap_allocs} allocs, hit {hit_allocs}, miss {miss_allocs}, \
         extended {extend_allocs} (all 0)"
    );
}

// ---------------------------------------------------------------------------
// 2. Length independence
// ---------------------------------------------------------------------------

#[test]
fn appended_since_cost_is_independent_of_table_size() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    const SMALL_ATOMS: usize = 4;
    const BIG_ATOMS: usize = 200_000;
    const ITERATIONS: usize = 20_000_000;
    const REPEATS: usize = 5;

    let small = table_with(SMALL_ATOMS);
    let big = table_with(BIG_ATOMS);
    let small_snap = small.snapshot();
    let big_snap = big.snapshot();
    assert_eq!(small.len(), SMALL_ATOMS);
    assert_eq!(big.len(), BIG_ATOMS);

    let mut small_ns = time_repeated(REPEATS, ITERATIONS, || {
        let t = black_box(&small);
        let s = black_box(small_snap);
        black_box(t.appended_since(s));
    });
    let mut big_ns = time_repeated(REPEATS, ITERATIONS, || {
        let t = black_box(&big);
        let s = black_box(big_snap);
        black_box(t.appended_since(s));
    });

    let small_ns_per_op = median(&mut small_ns);
    let big_ns_per_op = median(&mut big_ns);
    let ratio = big_ns_per_op / small_ns_per_op;

    eprintln!(
        "atom_witness: appended_since {} atoms {small_ns_per_op:.3} ns/op | \
         {} atoms {big_ns_per_op:.3} ns/op | ratio {ratio:.3} | loadavg {} \
         ({REPEATS} runs x {ITERATIONS})",
        SMALL_ATOMS,
        BIG_ATOMS,
        loadavg()
    );
    eprintln!("atom_witness: small raw ns/op {small_ns:?}");
    eprintln!("atom_witness: big   raw ns/op {big_ns:?}");

    // A `names`-scanning validation would be ~50 000x here. 100x is a tripwire for an
    // accidental O(table) regression that is still far too loose to be flaky.
    assert!(
        big_ns_per_op < 100.0 * small_ns_per_op,
        "appended_since on a {BIG_ATOMS}-atom table is {big_ns_per_op:.3} ns/op vs \
         {small_ns_per_op:.3} ns/op on a {SMALL_ATOMS}-atom table ({ratio:.1}x): the \
         witness is no longer O(1)"
    );
}

// ---------------------------------------------------------------------------
// 3. Soundness at scale
// ---------------------------------------------------------------------------

#[test]
fn soundness_holds_at_fifty_thousand_atoms() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());

    const ATOMS: usize = 50_000;
    let mut a = table_with(ATOMS);
    let snap = a.snapshot();

    // (a) a pure append-only extension validates
    a.intern("appended_after_snapshot");
    assert!(a.appended_since(snap));
    assert_eq!(a.len(), ATOMS + 1);
    // ... and the guarantee it stands for: every prefix index is untouched and the new
    // name occupies an index >= snapshot.len()
    assert_eq!(a.name(Atom::from_raw(0)), Some("true"));
    assert_eq!(a.name(Atom::from_raw(ATOMS as u32 - 1)), Some("synthetic_atom_49999"));
    assert_eq!(a.lookup("appended_after_snapshot").map(Atom::raw), Some(ATOMS as u32));

    // (b) an untouched clone is still a *different* identity
    let untouched_clone = a.clone();
    assert!(!untouched_clone.appended_since(snap));
    assert!(untouched_clone.appended_since(untouched_clone.snapshot()));

    // (c) a clone that diverges never validates, in either direction
    let mut b = a.clone();
    b.intern("only_in_b");
    a.intern("only_in_a");
    let snap_a = a.snapshot();
    let snap_b = b.snapshot();
    assert!(!b.appended_since(snap));
    assert!(!b.appended_since(snap_a));
    assert!(!a.appended_since(snap_b));
    assert!(a.appended_since(snap));
    assert!(b.appended_since(snap_b));

    // (d) an independently built table holding the identical names (same length, same
    // generation, same order) must not validate: the witness checks identity, not content
    let mut c = table_with(ATOMS);
    c.intern("appended_after_snapshot");
    c.intern("only_in_a");
    assert_eq!(c.len(), a.len());
    assert_eq!(c.generation(), a.generation());
    assert_eq!(c.name(Atom::from_raw(0)), a.name(Atom::from_raw(0)));
    assert_eq!(
        c.name(Atom::from_raw(ATOMS as u32)),
        a.name(Atom::from_raw(ATOMS as u32))
    );
    assert_eq!(c.snapshot().len(), snap_a.len());
    assert_ne!(c.snapshot(), snap_a);
    assert!(!c.appended_since(snap));
    assert!(!c.appended_since(snap_a));
    assert!(!a.appended_since(c.snapshot()));

    // (e) a shorter table never validates against a longer snapshot (no underflow)
    let short = table_with(10);
    assert!(!short.appended_since(snap));
    assert!(!a.appended_since(short.snapshot()));

    // (f) `AtomSnapshot` is `Copy` and cheap at this scale
    let copied: AtomSnapshot = snap;
    assert_eq!(copied, snap);
    assert_eq!(copied.len(), ATOMS);
    assert_eq!(copied.generation(), ATOMS as u64);
    assert_eq!(a.generation(), ATOMS as u64 + 2);
}
