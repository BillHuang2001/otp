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
//! 5. **(e) `AtomTable::fork()` allocates nothing at any table size** — it shares the whole prefix
//!    (both storage directions) by `Arc`, so it copies no name handle and no bucket. This is the
//!    copy the per-module load of a `beam_rt` boot uses on every module, and its allocation count
//!    is *flat* in the table size while `clone`'s is linear: (d) and (e) are the two halves of
//!    that contrast — one pinning that `clone` still scales, the other that `fork` does not.
//! 6. **(f) the copy-on-write step a fork pays on its first append is bounded and flat** — one
//!    name chunk (≤ `NAMES_PER_CHUNK` handles), the shard array (its `8 · INDEX_SHARDS` bytes move
//!    bytes, not allocation *count*), one index shard (about `len / INDEX_SHARDS` entries) and the
//!    new string, at 300 atoms exactly as at 3 000. Without this the fork would only move the
//!    whole-table copy from fork time to the first append.


use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_term::atom::store::{INDEX_SHARDS, NAMES_PER_CHUNK};
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

/// How far a bulk seed may exceed "one allocation per distinct name".
///
/// The store behind [`AtomTable`] keeps the name list in [`NAMES_PER_CHUNK`]-sized chunks and the
/// name→index map in [`INDEX_SHARDS`] shards, so a seed pays, on top of one string per name:
///
/// * the chunk containers — one `Arc<NameChunk>` plus one `Vec<Arc<str>>` buffer per chunk
///   (`NAMES.div_ceil(NAMES_PER_CHUNK)` of them), plus the chunk list's `Arc` and its one
///   reserved buffer;
/// * the shard containers — the shard array (`Arc` + buffer), one `Arc<IndexShard>` per shard a
///   name lands in, and up to six map-growth events per such shard.
///
/// This is a deliberate **upper bound**, counted in containers rather than names so it stays put
/// when the name count grows. It is an *over*-allowance by construction, not a derivation to be
/// re-fitted: it charges every shard the six growth events of a densely loaded map, while at
/// `INDEX_SHARDS = 1024` a `NAMES`-name seed leaves only ~3 names in each (one or two events), and
/// at any shard count a map that holds `n` names grows `O(log n)` times, never `O(n)`. The property
/// the budget states is the shape — one string per name plus a bounded number of *containers*.
fn seed_slack() -> usize {
    let chunks = NAMES.div_ceil(NAMES_PER_CHUNK);
    let chunk_containers = chunks * 2 + 2;
    let shard_containers = INDEX_SHARDS + INDEX_SHARDS * 6 + 2;
    chunk_containers + shard_containers
}

/// How many allocations the first append after a fork may cost.
///
/// Every mutating path detaches its shared container with `Arc::make_mut`, and one such step is
/// **two** allocations (the cloned payload and the `Arc` block that owns it): one for the chunk
/// list, one for the chunk the append lands in, one for the shard array, one for the shard. On top
/// of that: the interned string itself, a fresh chunk when the index falls on a
/// `NAMES_PER_CHUNK` boundary (its `Arc` and its buffer), and one shard-map growth event.
const FORK_APPEND_BUDGET: usize = 4 * 2 + 1 + 2 + 1;

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
    let (seed, mut seeded) = allocations_in(|| {
        let mut t = AtomTable::new();
        t.seed(names.iter().map(String::as_str));
        t
    });
    assert_eq!(seeded.len(), NAMES, "the seed really interned every name");

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
    // (e) `fork()` at the same two sizes, plus the empty table: nothing at all, at any size. The
    // empty table is built *outside* the window, so the measurement is the fork and only the fork.
    let empty = AtomTable::new();
    let (fork_small, _f1) = allocations_in(|| small_table.fork());
    let (fork_large, _f2) = allocations_in(|| seeded.fork());
    let (fork_empty, _f3) = allocations_in(|| empty.fork());
    // (f) the copy-on-write step a fork pays on its first append — the property the whole design
    // exists for. `seeded`/`small_table` still hold their containers, so the fork's append must
    // detach (a chunk + a shard), and the parent's own next append (with the fork alive) must too.
    let (fork_intern_large, _g1) = allocations_in(|| {
        let mut f = seeded.fork();
        f.intern("only_in_a_literal");
        f
    });
    let (fork_intern_small, _g2) = allocations_in(|| {
        let mut f = small_table.fork();
        f.intern("only_in_a_literal");
        f
    });
    let held_fork = seeded.fork();
    let seeded_len = seeded.len();
    let (parent_intern, _g3) = allocations_in(|| seeded.intern("after_the_fork"));
    assert_eq!(
        held_fork.len(),
        seeded_len,
        "the fork held across the parent's append did not move"
    );

    // Print everything *before* asserting, so a pre-fix run still reports every figure.
    for line in [
        seed.line(&format!("(a) seed({NAMES})")),
        from_source.line(&format!("(b) seed_from_atom_names({NAMES})")),
        lookup.line("(c) lookup() x1000"),
        name_of.line("(c) name() x1000"),
        reintern.line("(c) intern(existing)"),
        clone_small.line(&format!("(d) clone({SMALL})")),
        clone_large.line(&format!("(d) clone({NAMES})")),
        fork_small.line(&format!("(e) fork({SMALL})")),
        fork_large.line(&format!("(e) fork({NAMES})")),
        fork_empty.line("(e) fork(empty)"),
        fork_intern_small.line(&format!("(f) fork({SMALL}) + 1 intern")),
        fork_intern_large.line(&format!("(f) fork({NAMES}) + 1 intern")),
        parent_intern.line(&format!("(f) parent intern() with a live fork ({NAMES})")),
    ] {
        println!("{line}");
    }

    // The measured calls really did the work, so the figures are not vacuous.
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

    // (a)+(b) one heap string per name, plus a bounded number of *container* allocations that
    // grows with the container count (chunks, shards), never with the name count.
    let budget = NAMES + seed_slack();
    assert!(
        seed.allocations <= budget,
        "seeding {NAMES} names cost {} allocations (budget {}): one heap string per distinct name, \
         plus a bounded number of container allocations that grows with the container count, never \
         with the name count",
        seed.allocations,
        budget
    );
    assert!(
        from_source.allocations <= budget,
        "seed_from_atom_names over {NAMES} names cost {} allocations (budget {}): the parser must \
         intern each printname directly, with no throw-away buffer",
        from_source.allocations,
        budget
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
    // (e) `fork` shares both storage directions by `Arc`: no string, no name handle, no bucket —
    // so it allocates *nothing* at any table size, and it is the contrast with (d) that makes
    // each half meaningful: `clone` still scales, `fork` does not.
    assert_eq!(
        fork_large.allocations, 0,
        "a fork of a {NAMES}-atom table must allocate nothing (it shares the chunk list and the \
         shard array by `Arc`), got {}",
        fork_large.allocations
    );
    assert_eq!(
        fork_small.allocations, 0,
        "a fork of a {SMALL}-atom table must allocate nothing, got {}",
        fork_small.allocations
    );
    assert_eq!(
        fork_empty.allocations, 0,
        "a fork of an empty table allocates nothing"
    );
    assert_eq!(
        fork_small.allocations, fork_large.allocations,
        "a fork must not scale with the table: {SMALL} atoms cost {} allocations, {NAMES} atoms {}",
        fork_small.allocations, fork_large.allocations
    );
    assert!(
        fork_large.bytes < clone_large.bytes,
        "a fork must move fewer bytes than a deep clone (it copies nothing at all): {} vs {} \
         bytes for {NAMES} atoms",
        fork_large.bytes,
        clone_large.bytes
    );

    // (f) the first append after a fork pays ONE chunk, ONE shard and the new string — a bounded,
    // size-independent number of allocations, not a table copy.
    for (label, measured) in [
        (format!("fork({SMALL}) + 1 intern"), fork_intern_small),
        (format!("fork({NAMES}) + 1 intern"), fork_intern_large),
        (
            format!("parent intern() with a live fork ({NAMES})"),
            parent_intern,
        ),
    ] {
        assert!(
            measured.allocations <= FORK_APPEND_BUDGET,
            "{label} cost {} allocations (budget {FORK_APPEND_BUDGET}): an append after a fork may \
             pay one name chunk and one index shard (two allocations each, payload + `Arc` block) \
             plus the string, but never the table",
            measured.allocations
        );
    }
    assert!(
        fork_intern_large.allocations <= fork_intern_small.allocations + 2,
        "the fork-then-append cost must be flat, not proportional: {SMALL} atoms cost {} \
         allocations, {NAMES} atoms {} (one extra shard-map growth event is all the slack there is)",
        fork_intern_small.allocations,
        fork_intern_large.allocations
    );
    assert!(
        fork_intern_large.bytes * 10 < clone_large.bytes,
        "fork-then-append must be under a tenth of a deep clone: {} vs {} bytes for {NAMES} atoms",
        fork_intern_large.bytes,
        clone_large.bytes
    );
    // the append really landed, and the parent's did too (on its own side)
    assert_eq!(seeded_len, NAMES);
    assert_eq!(seeded.len(), NAMES + 1);
    assert_eq!(
        seeded
            .lookup("after_the_fork")
            .map(|atom| atom.raw() as usize),
        Some(NAMES)
    );
}
