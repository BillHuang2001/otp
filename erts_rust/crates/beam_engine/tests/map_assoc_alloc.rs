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

//! `put_map_assoc`'s capacity count is **O(1)**: the source map is enumerated
//! exactly once per instruction, and that is pinned by a counting allocator.
//!
//! # The waste this pins
//!
//! `put_map_assoc` sizes its reservation from the source map's key count plus the
//! number of new pairs (`beam_common.c:2002,2017` — the C reads
//! `flatmap_get_size(old_mp)` off the header). The count used to come from
//! `terms::map_entries(map)?.len()`, which materializes a `Vec<(Eterm, Eterm)>` of
//! every existing pair and drops it again **purely for the length**, before the
//! merge builds the same list a second time. On the dominant memory path of an
//! in-process `-eval` compile (`lib/kernel/src/inet_parse.erl`, 69.7 % of its arena
//! words in `put_map_assoc` reservations) that doubled the walk on every update.
//!
//! The count now comes from [`beam_term::map::map_size`] — the head node's own size
//! word, one O(1) read — so the pair list is built once.
//!
//! # Method — a counting global allocator, per-thread armed
//!
//! This integration test is its own binary, so it installs its own
//! `#[global_allocator]`: `System` plus an allocation counter that only counts while
//! the current thread is *armed*. Unlike `tests/bif_alloc.rs` (a process-global flag)
//! the flag here is **per-thread**, so a sibling `#[test]` in this binary — or
//! libtest's own result collection — cannot contaminate a measurement.
//!
//! A `put_map_assoc` run is *not* allocation-free: `Machine::run_label` builds the
//! run's label index and [`beam_term::map::write_map`] allocates a `Box` per HAMT
//! node (see `beam_term/tests/map_words_capacity.rs`). So the bound is not zero — it
//! is **calibrated in this same binary** from the three parts the instruction is made
//! of, all measured with the same counter:
//!
//! * the **harness** (`harness_module`: `Machine::new` + one label index over a
//!   body that allocates nothing);
//! * the **build** (`terms::make_map` over the merged pair list — the reference map
//!   the semantic pin compares against);
//! * **one** source walk (`terms::map_entries(source)`).
//!
//! The instruction must fit `harness + build + one walk + a slack smaller than a
//! walk`, i.e. the source is enumerated once. The measured decomposition is exact
//! (`A == harness + build + walk`), and the old `map_entries(map)?.len()` code sat one
//! full walk above it:
//!
//! | n | harness | build | one walk | instruction | band | **old (two walks)** |
//! |---|---|---|---|---|---|---|
//! | 1 000 | 6 | 1 173 | 14 | 1 193 | 1 199 | **1 207** |
//! | 2 775 | 6 | 3 174 | 17 | 3 197 | 3 203 | **3 214** |
//!
//! (bytes, n = 1 000: 165 264 measured vs 181 904 for the two-walk code; n = 2 775:
//! 455 944 vs 521 992 — the difference is exactly one walk's `Vec<(Eterm, Eterm)>`,
//! `16n` bytes of growth.) The figures are printed (`-- --nocapture`).
//!
//! # The semantic pin (independent of the counter)
//!
//! The capacity only sizes the reservation — the builder deduplicates, so any upper
//! bound yields the same map. The test therefore also pins the **result**: the same
//! instruction run twice must produce equal maps, and the produced map must equal the
//! reference map built for the same merged pair list by the general builder
//! ([`beam_engine::terms::make_map`]), down to `terms::map_entries`' **storage order**
//! (the HAMT shape, which term equality alone would not pin).
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
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{Function, Instr, Label, MapWriteArgs, Module, Operand, NO_FAIL};
use beam_term::{cmp, make_small, Eterm};

// ---------------------------------------------------------------------------
// The counting allocator
// ---------------------------------------------------------------------------

/// Allocation calls observed while the current thread is armed.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Bytes requested by those calls.
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring: the counters are
    /// process-global but *arming* is per-thread, so libtest's own allocations and a
    /// sibling test on another thread can never perturb a measurement.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Records one allocation of `bytes` bytes. Atomics only — it allocates nothing.
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

// SAFETY: every method forwards to the `System` allocator with the same `Layout`, so
// the (ptr, layout) pairing the callers released is preserved exactly; the counters
// are plain atomics and allocate nothing.
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

/// Runs `body` with the counters armed on this thread, returning
/// `(allocations, bytes, body's value)`.
fn measure<T>(body: impl FnOnce() -> T) -> (usize, usize, T) {
    // Force the TLS to initialize *before* the window opens.
    ARMED.with(|armed| armed.set(false));
    let before_alloc = ALLOCATIONS.load(Ordering::Relaxed);
    let before_bytes = BYTES.load(Ordering::Relaxed);
    ARMED.with(|armed| armed.set(true));
    let value = body();
    ARMED.with(|armed| armed.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed).wrapping_sub(before_alloc);
    let bytes = BYTES.load(Ordering::Relaxed).wrapping_sub(before_bytes);
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
/// events here (17 at 2 775 pairs), so no single-walk path can absorb a second walk
/// inside this slack — while the single-walk path needs none of it (measured: exactly
/// `harness + build + one walk`).
const ALLOC_SLACK: usize = 6;

/// The same head-room in bytes — one walk's `Vec<(Eterm, Eterm)>` costs `16 * n`
/// bytes of growth (16 KiB at n = 1 000, 64 KiB at n = 2 775), so a 1 KiB slack is
/// three orders of magnitude away from hiding a second walk.
const BYTE_SLACK: usize = 1_024;

/// The regression test: one `put_map_assoc` per size, with the allocation
/// decomposition and the produced map both pinned.
///
/// The counters are process-global, so every measurement lives in **one** `#[test]`
/// (libtest would otherwise run the sizes concurrently in the same process).
///
/// The whole measurement fits in the `MinimalContext`'s first reserved region
/// (~2.5 MiB of the 4 MiB one: four `map_words_capacity(n)` blocks per size), so no
/// region reservation — a large `mmap` in the counter — can land inside a window; the
/// byte figures are evidence of that, being exactly linear in `n`.
#[test]
fn put_map_assoc_walks_the_source_map_once() {
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
        // (2) The source walk the capacity count must NOT need a second time.
        let (walk_allocs, walk_bytes, walked) = measure(|| {
            terms::map_entries(source).expect("a readable map")
        });
        assert_eq!(walked.len(), n, "n = {n}: one walk sees every pair");
        // (3) The build alone: the reference map for the merged pair list, whose
        //     `map_entries` **storage order** must match too — the HAMT shape, which
        //     term equality alone would not pin.
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
        let produced = terms::map_entries(first).expect("a readable map");
        let expected = terms::map_entries(reference).expect("a readable map");
        assert_eq!(produced.len(), expected.len(), "n = {n}: same key count");
        for (got, want) in produced.iter().zip(expected.iter()) {
            assert!(
                cmp(got.0, want.0, atoms).is_eq() && cmp(got.1, want.1, atoms).is_eq(),
                "n = {n}: storage-order pair mismatch: {got:?} vs {want:?}"
            );
        }

        // The allocation pin: `harness + build + ONE walk`, with a slack smaller than
        // a single walk. The old code (`map_entries(map)?.len()`) walked twice, so it
        // sat at `harness + build + 2 * walk` — above the band on both signals.
        assert!(
            allocs <= band_allocs,
            "n = {n}: {allocs} allocations exceed harness({h_allocs}) + build({build_allocs}) + \
             one walk({walk_allocs}) + {ALLOC_SLACK} = {band_allocs}; the capacity count must \
             come from the map's size word, not from a second `map_entries` walk"
        );
        assert!(
            bytes <= band_bytes,
            "n = {n}: {bytes} bytes exceed harness({h_bytes}) + build({build_bytes}) + one \
             walk({walk_bytes}) + {BYTE_SLACK} = {band_bytes}; a second walk allocates a `Vec` of \
             {n} pairs ({size} bytes) here",
            size = 16 * n
        );
    }
}
