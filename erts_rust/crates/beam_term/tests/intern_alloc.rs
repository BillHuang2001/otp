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

//! Deterministic allocation evidence for [`AtomTable`] interning.
//!
//! A counting `#[global_allocator]` (the exact pattern of `tests/atom_witness.rs` and
//! `beam_rt/tests/atom_table_commit_alloc.rs`) wraps `std::alloc::System` with counters armed
//! **per thread**, so the libtest harness can never perturb a measurement. Allocation counts are
//! host-load independent — an allocation either happens or it does not — which is why nothing
//! here asserts on wall-clock time.
//!
//! ## What is pinned
//!
//! 1. **(a) `seed`ing N distinct names allocates one heap string per name, plus a bounded
//!    number of container reservations** — not two strings per name and not a growth storm.
//!    A novel name must be stored **once** and referenced by both the ordered `names` view and
//!    the `index` map.
//! 2. **(b) `seed_from_atom_names` costs the same** — the parser must intern each printname
//!    *directly*, with no throw-away `Vec<String>` per `atom …` line.
//! 3. **(c) `lookup`/`name` on an existing atom allocate nothing.**
//! 4. **(d) `AtomTable::clone()` is proportional to the table** — a clone owns a *fresh* name
//!    string per entry (it is a deep copy, never a shared-buffer copy). This is the property
//!    `beam_rt/tests/atom_table_commit_alloc.rs` relies on: it uses `rt.atoms().clone()` as its
//!    positive control and requires the control to scale with the table. A shared-`Arc` clone
//!    would allocate O(1) and silently make that sibling pin vacuous.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_term::AtomTable;

// ---------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc`(growth) calls made on an armed thread (monotonic).
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Bytes requested on an armed thread (monotonic).
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring. The counters are process-global, but
    /// *arming* is per-thread, so the harness and any sibling test can never perturb a
    /// measurement.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Records one allocation of `bytes` on the current thread while it is armed.
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
        // Count only growth (a shrink is not an allocation event).
        if new_size > layout.size() {
            record(new_size - layout.size());
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// One armed measurement: the number of global allocations and the bytes they requested.
#[derive(Debug, Clone, Copy)]
struct Measured {
    allocations: usize,
    bytes: usize,
}

impl Measured {
    fn line(self, label: &str) -> String {
        format!(
            "intern_alloc: {label} => {} allocations / {} bytes",
            self.allocations, self.bytes
        )
    }
}

/// Runs `body` with the counters armed on this thread and returns what it measured.
fn allocations_in<T>(body: impl FnOnce() -> T) -> (Measured, T) {
    // Force the TLS to initialize *before* the window opens.
    ARMED.with(|armed| armed.set(false));
    let before_allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = BYTES.load(Ordering::Relaxed);
    ARMED.with(|armed| armed.set(true));
    let result = body();
    ARMED.with(|armed| armed.set(false));
    let measured = Measured {
        allocations: ALLOCATIONS.load(Ordering::Relaxed) - before_allocations,
        bytes: BYTES.load(Ordering::Relaxed) - before_bytes,
    };
    (measured, result)
}

// ---------------------------------------------------------------------------
// The workload
// ---------------------------------------------------------------------------

/// The table size the invariants are pinned at.
const NAMES: usize = 3_000;
/// A tenth of it, for the clone-scaling half of invariant (d).
const SMALL: usize = 300;
/// How far a bulk seed may exceed "one allocation per distinct name": the `Vec` reservation and
/// the `HashMap` reservation, plus slack for one extra growth event.
const SEED_SLACK: usize = 8;

/// `n` distinct, realistically-shaped names.
fn synthetic_names(n: usize) -> Vec<String> {
    (0..n)
        .map(|i| format!("intern_alloc_synthetic_{i:04}"))
        .collect()
}

/// A ~3000-name `atom.names` source: `atom …` lines of twelve names each, with comment and blank
/// lines interleaved so the parser's line handling is exercised.
fn atom_names_source(names: &[String]) -> String {
    let mut src = String::from("# synthetic atom.names source\n\n");
    for (line, chunk) in names.chunks(12).enumerate() {
        src.push_str("atom ");
        for (slot, name) in chunk.iter().enumerate() {
            if slot > 0 {
                src.push(' ');
            }
            src.push_str(name);
        }
        src.push('\n');
        if line % 16 == 0 {
            src.push_str("# periodic comment\n");
        }
    }
    src
}

/// The switch from `Vec<String>`/`HashMap<String, Atom>` to `Arc<str>` must **not** drop an
/// auto-trait: siblings hold the shared table across threads, so a regression to `Rc` would break
/// them at compile time. This pins `Send + Sync` (and, for good measure, `UnwindSafe`).
#[test]
fn atom_table_stays_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_unwind_safe<T: std::panic::UnwindSafe>() {}
    assert_send_sync::<AtomTable>();
    assert_unwind_safe::<AtomTable>();
}

#[test]
fn interning_allocates_one_string_per_novel_name_and_a_clone_scales() {
    let names = synthetic_names(NAMES);
    let small_names = synthetic_names(SMALL);
    let source = atom_names_source(&names);

    // (a) `seed` of NAMES distinct names.
    let (seed, seeded) = allocations_in(|| {
        let mut t = AtomTable::new();
        t.seed(names.iter().map(String::as_str));
        t
    });

    // (b) `seed_from_atom_names` over the ~NAMES-name source.
    let (from_source, parsed) = allocations_in(|| {
        let mut t = AtomTable::new();
        t.seed_from_atom_names(&source).expect("valid source");
        t
    });

    // (c) `lookup`/`name`/re-intern on an already-interned atom: zero.
    let existing = parsed.lookup(&names[NAMES / 2]).expect("interned");
    let (lookup, lookup_hits) = allocations_in(|| {
        let mut hits = 0usize;
        for _ in 0..1_000 {
            hits += black_box(parsed.lookup(black_box(names[NAMES / 2].as_str()))).is_some()
                as usize;
        }
        hits
    });
    let (name_of, name_hits) = allocations_in(|| {
        let mut hits = 0usize;
        for _ in 0..1_000 {
            hits += black_box(parsed.name(black_box(existing))).is_some() as usize;
        }
        hits
    });
    // A mutable copy is taken *outside* the window; only the re-intern of a known name is inside.
    let mut warm = parsed.clone();
    let (reintern, _again) = allocations_in(|| warm.intern(&names[0]));

    // (d) `clone()` at two table sizes: it must be proportional to the table.
    let mut small_table = AtomTable::new();
    small_table.seed(small_names.iter().map(String::as_str));
    let (clone_small, _c1) = allocations_in(|| small_table.clone());
    let (clone_large, _c2) = allocations_in(|| seeded.clone());

    // Print everything *before* asserting, so a pre-fix run still reports every figure.
    for line in [
        seed.line(&format!("(a) seed({NAMES})")),
        from_source.line(&format!("(b) seed_from_atom_names({NAMES})")),
        lookup.line("(c) lookup() x1000"),
        name_of.line("(c) name() x1000"),
        reintern.line("(c) intern(existing)"),
        clone_small.line(&format!("(d) clone({SMALL})")),
        clone_large.line(&format!("(d) clone({NAMES})")),
    ] {
        println!("{line}");
    }

    // The measured calls really did the work, so the figures are not vacuous.
    assert_eq!(seeded.len(), NAMES);
    assert_eq!(parsed.len(), NAMES);
    assert_eq!(
        parsed.lookup(&names[NAMES / 2]),
        Some(existing),
        "the source really interned the names"
    );
    assert_eq!(lookup_hits, 1_000);
    assert_eq!(name_hits, 1_000);
    assert_eq!(
        warm.lookup(&names[0]),
        parsed.lookup(&names[0]),
        "the re-intern hit an already-interned name"
    );

    // (c) lookup/name/re-intern on an existing atom allocate nothing.
    assert_eq!(lookup.allocations, 0, "`lookup` must not allocate");
    assert_eq!(name_of.allocations, 0, "`name` must not allocate");
    assert_eq!(
        reintern.allocations, 0,
        "re-interning a known name must not allocate"
    );

    // (a)+(b) one heap string per name, plus bounded container reservations.
    assert!(
        seed.allocations <= NAMES + SEED_SLACK,
        "seeding {NAMES} names cost {} allocations (budget {}): the name must be stored once, \
         not once per container",
        seed.allocations,
        NAMES + SEED_SLACK
    );
    assert!(
        from_source.allocations <= NAMES + SEED_SLACK,
        "seed_from_atom_names over {NAMES} names cost {} allocations (budget {}): the parser must \
         intern each printname directly, with no throw-away buffer",
        from_source.allocations,
        NAMES + SEED_SLACK
    );

    // (d) the clone is a deep copy: it allocates a fresh name string per entry, and so scales.
    assert!(
        clone_large.allocations >= NAMES,
        "cloning a {NAMES}-atom table cost only {} allocations: the clone must own a fresh name \
         string per entry (the `beam_rt` commit-allocation pin uses this as its positive control)",
        clone_large.allocations
    );
    assert!(
        clone_large.allocations > clone_small.allocations * 5,
        "the clone must scale with the table: {NAMES} atoms cost {} allocations, {SMALL} atoms {}",
        clone_large.allocations,
        clone_small.allocations
    );
}
