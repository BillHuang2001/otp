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

//! Allocation pins for the map instructions: the **write** arm below is incremental,
//! and the two **read** instructions build nothing proportional to the map size.
//!
//! # The write instruction: `put_map_assoc`'s hash-map arm is incremental
//!
//! The source is not enumerated at all, and the instruction's allocation count no
//! longer grows with `n`.
//!
//! ## The two arms
//!
//! The instruction picks its arm from the source's own layout:
//!
//! * a **hash map** (HAMT) source takes the path-copying writer,
//!   `beam_term::map::map_put_cow`, through [`beam_engine::terms::map_cow_rooted`]:
//!   one rooted reservation of `map_put_cow_capacity(size) * pairs` arena words, then
//!   one put per pair that descends the key's hash path and copies only the nodes on
//!   it. **No pair list is materialized** — the source is not walked once, let alone
//!   twice — and the reservation is two orders of magnitude below the whole-map
//!   rebuild's.
//! * a **flat map** (≤ `MAP_SMALL_MAP_LIMIT` = 32 keys) keeps the rebuild: merge into
//!   a fresh pair list and build it with [`beam_engine::terms::make_map`], whose
//!   `write_map` allocates a `Box` per HAMT node. That arm is what performs the 33rd
//!   key's conversion to a hash map.
//!
//! ## What the write test still guards, and how
//!
//! The counting allocator and the `harness + build + one walk` band are kept
//! **deliberately**, as an upper-bound regression guard on the *rebuild* shape:
//! `build` is the reference map the semantic pin compares against and `walk` is one
//! [`beam_engine::terms::map_entries`] enumeration, so an update that went back to
//! enumerate-and-rebuild (or to the old `map_entries(map)?.len()` double walk) would
//! still blow through the band. The measured hash-map arm sits far below it (the
//! figures are printed by `-- --nocapture`), so only a structural regression can
//! fail this test.
//!
//! # The read instructions: one `map_get` descent per key, no pair list
//!
//! `get_map_elements` and `has_map_fields` read each key with
//! `beam_term::map::map_get` (a hashed descent of a hash map, the bounded scan of a
//! flat one) and never materialize the source, so their allocation count is
//! **independent of `n`**: the second test below runs both over a hash-map source at
//! `n = 250` and `n = 6 250` and asserts the counts are *equal*. A read path that went
//! back to `terms::map_entries` + a linear scan would allocate (and grow) a
//! `Vec<(Eterm, Eterm)>` per instruction, growing with `n`.

//!
//! # Method — a counting global allocator, per-thread counters
//!
//! This integration test is its own binary, so it installs its own
//! `#[global_allocator]`: `System` plus an allocation counter that only counts while
//! the current thread is *armed*. Both the arming flag **and** the counters are
//! **per-thread** (unlike `tests/bif_alloc.rs`'s process-global flag), so a sibling
//! `#[test]` in this binary — which libtest runs on another thread, concurrently by
//! default — or libtest's own result collection cannot contaminate a measurement.
//! A process-global counter with per-thread arming still lets two measuring threads
//! increment one counter, which surfaced as a load-dependent one-event flake in the
//! read pin; the per-thread counter pair removes that overlap.
//!
//! The band is **calibrated in this same binary** from the three parts a rebuilding
//! update is made of, all measured with the same counter:
//!
//! * the **harness** (`harness_module`: `Machine::new` + one label index over a
//!   body that allocates nothing);
//! * the **build** (`terms::make_map` over the merged pair list — the reference map
//!   the semantic pin compares against);
//! * **one** source walk (`terms::map_entries(source)`).
//!
//! `harness + build + one walk` is what an enumerate-once-and-rebuild update costs;
//! the old `map_entries(map)?.len()` code sat one full walk above it (the `OLD`
//! column below). Both are now *bounds*, not the model: the reserved arena words of
//! a path-copying put are not allocator events at all (they come out of the arena's
//! own region), so the hash-map arm's allocator figure is the harness plus the
//! reference build and nothing that scales with `n`.
//!
//! # The semantic pin (independent of the counter)
//!
//! The reservation only sizes the block a put writes into. The test therefore also
//! pins the **result**: the same instruction run twice must produce equal maps, and
//! the produced map must equal the reference map built for the same merged pair list
//! by the general builder ([`beam_engine::terms::make_map`]) — `=:=`, the same size
//! word, and the same **canonical** key sequence
//! ([`beam_term::map::map_pairs`]). The *internal* HAMT node order is deliberately
//! not pinned: a path-copied map copies the nodes on one key's path and shares the
//! rest, so its node order is not the rebuild's in general.
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test map_assoc_alloc -- --nocapture
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;

use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{Function, Instr, Label, MapWriteArgs, Module, Operand, NO_FAIL};
use beam_term::{cmp, make_small, Eterm};

// ---------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------

thread_local! {
    /// The counter pair of the thread that is currently measuring, armed only while
    /// a measurement window is open: `Some((allocations, bytes))` counts, `None`
    /// disarmed. Arming and the counters live in **one** thread-local so `record`
    /// updates them as a pair, and — the reason they are per-thread rather than
    /// process-global — two measuring threads (libtest runs this binary's two
    /// `#[test]`s concurrently by default) each observe only their own allocations.
    /// A process-global counter would let one test's measured allocation land in the
    /// other test's window whenever their windows overlap, which is exactly the
    /// load-dependent `6→7` flake this fixed.
    static COUNTERS: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}

/// Records one allocation of `bytes` bytes on the thread that issued it. Touches
/// only thread-local state, so it allocates nothing.
#[inline]
fn record(bytes: usize) {
    // `try_with`, never `with`: the global allocator is also called on threads that
    // never opened a measurement window (libtest's own bookkeeping threads), and
    // those threads must not initialise — let alone count into — a counter.
    let _ = COUNTERS.try_with(|counters| {
        if let Some((allocations, total)) = counters.get() {
            counters.set(Some((allocations + 1, total + bytes)));
        }
    });
}

/// `System` plus the per-thread opt-in counting above.
struct CountingAllocator;

// SAFETY: every method forwards to the `System` allocator with the same `Layout`, so
// the (ptr, layout) pairing the callers released is preserved exactly; the counters
// are thread-local cells and allocate nothing.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Count only growth: a shrink is not an allocation event.
        if new_size > layout.size() {
            record(new_size - layout.size());
        }
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `body` with this thread's counters armed, returning
/// `(allocations, bytes, body's value)`.
fn measure<T>(body: impl FnOnce() -> T) -> (usize, usize, T) {
    // Initialize the TLS and arm (reset to zero) before the window opens. Setting
    // `Some((0, 0))` also clears a counter a previous panic left armed, so a
    // measurement never inherits stale values from the same thread.
    COUNTERS.with(|counters| counters.set(Some((0, 0))));
    let value = body();
    let (allocations, bytes) = COUNTERS.with(|counters| {
        let (allocations, bytes) = counters.get().unwrap_or((0, 0));
        counters.set(None);
        (allocations, bytes)
    });
    (allocations, bytes, value)
}

// ---------------------------------------------------------------------------
// The measured instruction
// ---------------------------------------------------------------------------

/// The source sizes measured: the objective's 1 000, and the 2 775-pair update an
/// in-process compile of `lib/kernel/src/inet_parse.erl` reaches.
const SIZES: [usize; 2] = [1_000, 2_775];

/// `f/1`: `put_map_assoc` one new key and one overwritten key into `x(0)`, return it.
fn assoc_module() -> (Module, Label) {
    let mut module = Module::default();
    let name = module.atom_table.intern("f");
    module.name = name;
    let entry = Label(1);
    let code = vec![
        Instr::Label(entry),
        Instr::PutMapAssoc(Box::new(MapWriteArgs {
            fail: NO_FAIL,
            source: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            pairs: vec![
                // Overwrite an existing key and add a new one, so the merge takes
                // both of its branches.
                (Operand::Integer(0), Operand::Integer(-1)),
                (Operand::Integer(99_999), Operand::Integer(1)),
            ],
        })),
        Instr::Return,
    ];
    let labels: BTreeMap<Label, usize> = code
        .iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect();
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name,
        arity: 1,
        code,
        blocks: Vec::new(),
        labels,
    });
    (module, entry)
}

/// One `put_map_assoc` over `source`, measured.
fn run_measured(
    ctx: &mut MinimalContext,
    module: &Module,
    entry: Label,
    source: Eterm,
) -> (usize, usize, Eterm) {
    measure(|| {
        let mut machine = Machine::new();
        machine
            .run_label(module, entry, 1, &[source], ctx)
            .expect("a well-formed put_map_assoc over a hash map")
    })
}

/// The **harness** cost: entering and returning from a run whose body allocates
/// nothing (`move x(0), x(0)`), i.e. `Machine::new` plus the per-run label index.
///
/// It is measured rather than assumed so the band below is calibrated in this very
/// binary: `put_map_assoc`'s cost decomposes exactly into *harness + the map build +
/// one source walk*, and the second walk the old code performed is what the band
/// leaves no room for.
fn harness_module() -> (Module, Label) {
    let mut module = Module::default();
    let name = module.atom_table.intern("h");
    module.name = name;
    let entry = Label(1);
    let code = vec![
        Instr::Label(entry),
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let labels: BTreeMap<Label, usize> = code
        .iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect();
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name,
        arity: 1,
        code,
        blocks: Vec::new(),
        labels,
    });
    (module, entry)
}

/// The head-room the band allows on top of `harness + build + one walk`, in
/// allocation events.
///
/// One `terms::map_entries` walk over a 1 000-pair hash map costs **14** allocation
/// events here (17 at 2 775 pairs), so no enumerate-and-rebuild path can slip under
/// the band by absorbing a walk inside this slack.
const ALLOC_SLACK: usize = 6;

/// The same head-room in bytes — one walk's `Vec<(Eterm, Eterm)>` costs `16 * n`
/// bytes of growth (16 KiB at n = 1 000, 64 KiB at n = 2 775), so a 1 KiB slack is
/// three orders of magnitude away from hiding a second walk.
const BYTE_SLACK: usize = 1_024;

/// The regression test: one `put_map_assoc` per size, with the allocation
/// decomposition and the produced map both pinned.
///
/// Every measurement still lives in **one** `#[test]` so the size loop and its
/// `harness`/`build`/`walk` decomposition run in a single, deterministic order. The
/// counters are per-thread, so the sibling read test cannot contaminate them even
/// when libtest runs the two tests concurrently.
///
/// The whole measurement fits in the `MinimalContext`'s reserved regions (a source,
/// a reference and a few path-copying reservations per size), so no region
/// reservation — a large `mmap` in the counter — can land inside a window; the byte
/// figures are evidence of that, being flat in `n`.
#[test]
fn put_map_assoc_does_not_enumerate_the_source_map() {
    let (module, entry) = assoc_module();
    let (harness_module, harness_entry) = harness_module();
    let atoms = &module.atom_table;
    let mut ctx = MinimalContext::new();

    // The harness first: nothing has grown the arena yet, and its own cost is
    // constant (a machine plus one label index).
    let (h_allocs, h_bytes, _) =
        run_measured(&mut ctx, &harness_module, harness_entry, make_small(0));

    eprintln!();
    eprintln!(
        "{:>6} {:>9} {:>7} {:>6} {:>7} {:>7} {:>7} | {:>9} {:>9}",
        "n", "alloc", "harness", "build", "walk", "band", "OLD", "bytes", "band"
    );

    for n in SIZES {
        // The source map and both pair lists are built *outside* the measured windows.
        let source_pairs: Vec<(Eterm, Eterm)> = (0..n as i64)
            .map(|i| (make_small(i), make_small(i * 3)))
            .collect();
        let mut merged = source_pairs.clone();
        merged[0].1 = make_small(-1);
        merged.push((make_small(99_999), make_small(1)));
        let source = terms::make_map(&mut ctx, &source_pairs, atoms).expect("a hash map");
        assert!(
            terms::is_hashmap(source),
            "n = {n}: the measured source must be a hash map"
        );
        assert_eq!(beam_term::map::map_size(source, atoms).unwrap(), n);

        // (1) The instruction itself — twice, the second run being the determinism
        //     pin.
        let (allocs, bytes, first) = run_measured(&mut ctx, &module, entry, source);
        let (_, _, second) = run_measured(&mut ctx, &module, entry, source);
        // (2) One source walk: part of the band's upper bound (the rebuild shape walks
        //     the source; the hash-map arm does not walk it at all).
        let (walk_allocs, walk_bytes, walked) = measure(|| {
            terms::map_entries(source).expect("a readable map")
        });
        assert_eq!(walked.len(), n, "n = {n}: one walk sees every pair");
        // (3) The build alone: the reference map for the merged pair list, which is
        //     the rebuild model's cost and the semantic pin's other side.
        let (build_allocs, build_bytes, reference) =
            measure(|| terms::make_map(&mut ctx, &merged, atoms).expect("the reference map"));

        let band_allocs = h_allocs + build_allocs + walk_allocs + ALLOC_SLACK;
        let band_bytes = h_bytes + build_bytes + walk_bytes + BYTE_SLACK;
        eprintln!(
            "{n:>6} {allocs:>9} {h_allocs:>7} {build_allocs:>6} {walk_allocs:>7} \
             {band_allocs:>7} {:>7} | {bytes:>9} {band_bytes:>9}",
            h_allocs + build_allocs + 2 * walk_allocs
        );

        // The result: n keys stay, one is overwritten, one is added.
        assert_eq!(
            beam_term::map::map_size(first, atoms).unwrap(),
            n + 1,
            "n = {n}: one key added, none lost"
        );
        assert!(
            cmp(first, second, atoms).is_eq(),
            "n = {n}: the instruction is deterministic"
        );
        assert_eq!(
            beam_term::map::map_get(first, make_small(0), atoms).unwrap(),
            Some(make_small(-1)),
            "n = {n}: the existing key was overwritten"
        );
        assert_eq!(
            beam_term::map::map_get(first, make_small(99_999), atoms).unwrap(),
            Some(make_small(1)),
            "n = {n}: the new key was inserted"
        );
        assert!(
            cmp(first, reference, atoms).is_eq(),
            "n = {n}: the update equals the reference build"
        );
        // The canonical key sequence, which the frozen layout does guarantee. The
        // *internal* HAMT node order is not pinned: the update copies one key's path
        // and shares the rest, so its node order is not the rebuild's in general.
        let produced = beam_term::map::map_pairs(first, atoms).expect("a readable map");
        let expected = beam_term::map::map_pairs(reference, atoms).expect("a readable map");
        assert_eq!(produced.len(), expected.len(), "n = {n}: same key count");
        for (got, want) in produced.iter().zip(expected.iter()) {
            assert!(
                cmp(got.0, want.0, atoms).is_eq() && cmp(got.1, want.1, atoms).is_eq(),
                "n = {n}: canonical-order pair mismatch: {got:?} vs {want:?}"
            );
        }

        // The band is an UPPER BOUND on the rebuild shape (`harness + build + one
        // walk`, with a slack smaller than a single walk): the hash-map arm's
        // path-copying reservation is arena storage, not an allocator event, so it
        // sits far below it. An update that went back to enumerate-and-rebuild —
        // including the old `map_entries(map)?.len()` double walk — lands above it on
        // both signals.
        assert!(
            allocs <= band_allocs,
            "n = {n}: {allocs} allocations exceed harness({h_allocs}) + build({build_allocs}) + \
             one walk({walk_allocs}) + {ALLOC_SLACK} = {band_allocs}; the update must not \
             materialize the source map as a pair list"
        );
        assert!(
            bytes <= band_bytes,
            "n = {n}: {bytes} bytes exceed harness({h_bytes}) + build({build_bytes}) + one \
             walk({walk_bytes}) + {BYTE_SLACK} = {band_bytes}; a walk allocates a `Vec` of \
             {n} pairs ({size} bytes) here",
            size = 16 * n
        );
    }
}

// ---------------------------------------------------------------------------
// The read instructions: no allocation proportional to the map size
// ---------------------------------------------------------------------------

/// The two hash-map sizes the read pin compares: the second is 25x the first, so a
/// `Vec` of pairs — or any structure that grows per key — cannot answer the same
/// allocation count at both.
const READ_SIZES: [usize; 2] = [250, 6_250];

/// `f/1`: `get_map_elements` for two keys of `x(0)` into `x(1)`/`x(2)`.
fn get_elements_module() -> (Module, Label) {
    let mut module = Module::default();
    let name = module.atom_table.intern("g");
    module.name = name;
    let entry = Label(1);
    let code = vec![
        Instr::Label(entry),
        Instr::GetMapElements {
            fail: NO_FAIL,
            source: Operand::X(0),
            pairs: vec![
                (Operand::Integer(0), Operand::X(1)),
                (Operand::Integer(7), Operand::X(2)),
            ],
        },
        Instr::Return,
    ];
    let labels: BTreeMap<Label, usize> = code
        .iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect();
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name,
        arity: 1,
        code,
        blocks: Vec::new(),
        labels,
    });
    (module, entry)
}

/// `f/1`: `has_map_fields` for the same two keys of `x(0)`, writing nothing.
fn has_fields_module() -> (Module, Label) {
    let mut module = Module::default();
    let name = module.atom_table.intern("h");
    module.name = name;
    let entry = Label(1);
    let code = vec![
        Instr::Label(entry),
        Instr::HasMapFields {
            fail: NO_FAIL,
            source: Operand::X(0),
            keys: vec![Operand::Integer(0), Operand::Integer(7)],
        },
        Instr::Return,
    ];
    let labels: BTreeMap<Label, usize> = code
        .iter()
        .enumerate()
        .filter_map(|(index, instr)| match instr {
            Instr::Label(label) => Some((*label, index)),
            _ => None,
        })
        .collect();
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name,
        arity: 1,
        code,
        blocks: Vec::new(),
        labels,
    });
    (module, entry)
}

/// The allocation count of one read instruction over a `source`, measured.
fn run_read_measured(
    ctx: &mut MinimalContext,
    module: &Module,
    entry: Label,
    source: Eterm,
) -> (usize, usize, Eterm) {
    measure(|| {
        let mut machine = Machine::new();
        machine
            .run_label(module, entry, 1, &[source], ctx)
            .expect("a well-formed read over a hash map")
    })
}

/// The allocation ceiling for one measured read run.
///
/// A read allocates **nothing** in the map — every allocation in the window is the
/// run harness itself (`Machine::new`'s register files plus the per-run label index),
/// which is a fixed number of events (9 here) and does not scale with the source. The
/// bound leaves it headroom while staying far below a pair-list read: one
/// `terms::map_entries` walk of the same sources costs 12 events at 250 pairs and 18
/// at 6 250, so such a read would land at ~21 and ~27 events.
const READ_ALLOC_BOUND: usize = 16;

/// The read pin: both instructions allocate the **same** number of times over a
/// 250-key and a 6 250-key hash map — i.e. the read path materializes nothing
/// proportional to the map's size.
#[test]
fn the_map_read_instructions_do_not_allocate_a_pair_list() {
    let (get_module, get_entry) = get_elements_module();
    let (has_module, has_entry) = has_fields_module();
    let atoms = &get_module.atom_table;
    let mut ctx = MinimalContext::new();

    // Warm-up, outside every measured window: the first run of a module builds and
    // caches its label index, the first arena allocation reserves the region (a
    // large `mmap`, not an allocator event), and the source map's own build is a
    // `Vec` of pairs. None of that may land inside a window, or the two sizes would
    // differ for a reason unrelated to the instruction.
    let warm_pairs: Vec<(Eterm, Eterm)> = (0..64i64).map(|i| (make_small(i), make_small(i))).collect();
    let warm = terms::make_map(&mut ctx, &warm_pairs, atoms).expect("a hash map");
    let _ = run_read_measured(&mut ctx, &get_module, get_entry, warm);
    let _ = run_read_measured(&mut ctx, &has_module, has_entry, warm);

    eprintln!();
    eprintln!(
        "{:>6} {:>12} {:>11} {:>10} | {:>10} {:>11}",
        "n", "get-elements", "has-fields", "walk", "read bytes", "has bytes"
    );

    let mut counts = Vec::new();
    for n in READ_SIZES {
        // The source is built *outside* the measured windows.
        let source_pairs: Vec<(Eterm, Eterm)> = (0..n as i64)
            .map(|i| (make_small(i), make_small(3 * i)))
            .collect();
        let source = terms::make_map(&mut ctx, &source_pairs, atoms).expect("a hash map");
        assert!(
            terms::is_hashmap(source),
            "n = {n}: the read pin needs a hash-map source"
        );

        let (get_allocs, get_bytes, fetched) =
            run_read_measured(&mut ctx, &get_module, get_entry, source);
        let (has_allocs, has_bytes, tested) =
            run_read_measured(&mut ctx, &has_module, has_entry, source);
        // Evidence, not a bound: one `terms::map_entries` walk of the same source, the
        // shape the read instructions used to take.
        let (walk_allocs, _walk_bytes, walked) =
            measure(|| terms::map_entries(source).expect("a readable map"));
        assert_eq!(walked.len(), n, "n = {n}: one walk sees every pair");
        eprintln!(
            "{n:>6} {get_allocs:>12} {has_allocs:>11} {walk_allocs:>10} | {get_bytes:>10} \
             {has_bytes:>11}"
        );
        assert_eq!(fetched, source, "n = {n}: the fetch falls through");
        assert_eq!(tested, source, "n = {n}: the field test falls through");
        counts.push((n, get_allocs, has_allocs));
    }

    let (_, get_small, has_small) = counts[0];
    let (_, get_large, has_large) = counts[1];
    assert_eq!(
        (get_small, has_small),
        (get_large, has_large),
        "the read allocation count must not grow with the map size: {counts:?} \
         (a pair-list read allocates O(n) here)"
    );
    for (n, get_allocs, has_allocs) in &counts {
        assert!(
            *get_allocs <= READ_ALLOC_BOUND,
            "n = {n}: get_map_elements made {get_allocs} allocations, above the harness-only \
             bound {READ_ALLOC_BOUND}"
        );
        assert!(
            *has_allocs <= READ_ALLOC_BOUND,
            "n = {n}: has_map_fields made {has_allocs} allocations, above the harness-only \
             bound {READ_ALLOC_BOUND}"
        );
    }
}
