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

//! Deterministic, dependency-free pin on the `erlang:binary_to_term/1,2` **steady-state
//! allocation behaviour**: it must be independent of the size of the shared
//! `AtomTable`.
//!
//! `binary_to_term` used to rebuild a private copy of the caller's atom table on every
//! call (`AtomTable::clone()`, `O(table)` allocations, ~127 us for a ~3000-atom table —
//! the whole cost of the BIF). The decode now interns into a **memoized per-thread
//! image** of the caller's table (`DECODE_SCRATCH` in `src/binconv.rs`), revalidated
//! index by index (`prefix_matches`, allocation-free) before each reuse, via
//! `ctx.atom_and_heap()`. That memo's *semantics* are pinned by `tests/decode_scratch.rs`;
//! this file pins its *allocation behaviour*, which is what a re-introduced per-call
//! clone would violate.
//!
//! ## The measurement
//!
//! A counting `#[global_allocator]` (the exact pattern of `tests/registry_hot_path.rs`)
//! wraps `std::alloc::System` with two `AtomicUsize` counters armed **per-thread**, so the
//! libtest harness and any sibling test can never perturb a measurement. This is
//! host-load independent: an allocation either happens or it does not, so nothing here
//! asserts on wall-clock time.
//!
//! For **two very different shared-table sizes** (~137 and ~4105 atoms, seeded exactly as
//! `examples/etf_bench.rs` seeds them) the same canonical image — whose atoms are all
//! already interned, so the decode interns nothing and the `fresh` path stays empty — is
//! decoded once **outside** the window (building and validating the per-thread memo, so
//! the measured call is the steady state) and once **inside** it. Every `HEAP` block is
//! allocated once and never reallocated, and every heap write is a bump into it, so the
//! in-window call touches the global allocator only for the decode's own transients.
//!
//! ## What is asserted
//!
//! 1. The two steady-state counts must be **identical** — the hard `== 0` on the
//!    table-attributable delta, and the observable form of "independent of the size of
//!    the shared `AtomTable`" (`AtomTable::len()` scaling cannot be observed on the hot
//!    path). A per-call `AtomTable::clone()` would allocate on the order of the table size
//!    and could not survive this comparison.
//! 2. The measurement is proven **sensitive** by two positive controls in the same
//!    harness, each measured per table size: an actual `AtomTable::clone()`, and the
//!    **exact counterfactual** this pin exists for — a decode that rebuilds a private
//!    table copy per call (`atoms.clone()` + `beam_codec::decode_term_used`). Both must
//!    scale with the table and sit an order of magnitude above the hot path. Without them,
//!    an accidentally-too-narrow window would make assertion (1) vacuous.
//! 3. The absolute count is bounded by a small, payload-intrinsic budget (see
//!    [`STEADY_STATE_ALLOCATION_BUDGET`]), far below even the smallest table clone here.
//!
//! The steady state is **not zero**, and cannot be, without changing `src/`: the decode has
//! payload-proportional transients that have nothing to do with the atom table —
//! `bytes_arg`'s input byte copy (exactly one allocation of `image.len()` bytes: images of
//! a small integer, an atom and a binary each measure exactly that) plus the codec's `Vec`
//! accumulators for a list's elements and a map's pairs. With the same image the count is
//! the same at both table sizes, which is the property being pinned; a
//! *table-proportional* allocation is what must never reappear.
//!
//! `std`-only: no new dependencies. The `beam_bifs` lib is `#![deny(unsafe_code)]`, but
//! this file is its own crate root (the `#[global_allocator]` needs `unsafe impl`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_alloc::Heap;
use beam_bifs::{default_registry, AtomInterner, BifContext, BifRegistry};
use beam_process::ProcessId;
use beam_term::{Atom, AtomTable, Eterm};

// ---------------------------------------------------------------------------
// The counting allocator (as in `tests/registry_hot_path.rs`)
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc` calls made on an armed thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Number of bytes requested on an armed thread.
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring.
    ///
    /// The counters are process-global, but the *arming* is per-thread, so the test
    /// harness (which allocates while formatting output) can never perturb a
    /// measurement. That is what makes the allocation evidence deterministic rather
    /// than merely usually-zero.
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

// ---------------------------------------------------------------------------
// The scenario
// ---------------------------------------------------------------------------

/// A bound on the decode's own **payload-proportional** transients: `bytes_arg`'s input
/// byte copy plus the codec's `Vec` accumulators for the map's five pairs and the list's
/// ten elements. Measured value for this payload is 9 allocations / 602 bytes; the bound
/// leaves room for a differently-sized accumulator without admitting a table clone (a
/// clone of the *smallest* table here is 276 allocations / 16198 bytes).
const STEADY_STATE_ALLOCATION_BUDGET: usize = 16;
/// The byte half of [`STEADY_STATE_ALLOCATION_BUDGET`].
const STEADY_STATE_BYTE_BUDGET: usize = 1024;

/// The `payload/0` term of the `etf_roundtrip` bench case as an ETF image (verbatim from
/// `examples/etf_bench.rs`): a map with five keys holding a 10-element list, a
/// `{ok, <<"hello world">>, 3.5}` tuple, a 12-byte binary, a char-list string and `-42`.
/// Every atom it carries (`a`..`e`, `ok`) is seeded into both tables below, so the decode
/// interns nothing and the `fresh` path stays empty.
fn payload_image() -> Vec<u8> {
    const SMALL_ATOM_UTF8: u8 = 119;
    const SMALL_INTEGER: u8 = 97;
    const INTEGER: u8 = 98;
    const SMALL_TUPLE: u8 = 104;
    const BINARY: u8 = 109;
    const NEW_FLOAT: u8 = 70;
    const STRING: u8 = 107;
    const LIST: u8 = 108;
    const NIL: u8 = 106;
    const MAP: u8 = 116;

    fn key(out: &mut Vec<u8>, name: &str) {
        out.push(SMALL_ATOM_UTF8);
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
    }

    let mut out = vec![131u8, MAP];
    out.extend_from_slice(&5u32.to_be_bytes());
    // a => [1, ..., 10]
    key(&mut out, "a");
    out.push(LIST);
    out.extend_from_slice(&10u32.to_be_bytes());
    for n in 1..=10u8 {
        out.push(SMALL_INTEGER);
        out.push(n);
    }
    out.push(NIL);
    // b => {ok, <<"hello world">>, 3.5}
    key(&mut out, "b");
    out.push(SMALL_TUPLE);
    out.push(3);
    key(&mut out, "ok");
    out.push(BINARY);
    out.extend_from_slice(&11u32.to_be_bytes());
    out.extend_from_slice(b"hello world");
    out.push(NEW_FLOAT);
    out.extend_from_slice(&3.5f64.to_be_bytes());
    // c => <<1..12>>
    key(&mut out, "c");
    out.push(BINARY);
    out.extend_from_slice(&12u32.to_be_bytes());
    out.extend(1..=12u8);
    // d => "a char list"
    key(&mut out, "d");
    out.push(STRING);
    out.extend_from_slice(&11u16.to_be_bytes());
    out.extend_from_slice(b"a char list");
    // e => -42
    key(&mut out, "e");
    out.push(INTEGER);
    out.extend_from_slice(&(-42i32).to_be_bytes());
    out
}

/// The shared table seeded exactly as `examples/etf_bench.rs` seeds it: the runtime's own
/// atoms and the payload's, then `table_size` synthetic atoms. The synthetic names are
/// appended, so a smaller table is an exact index-prefix of a larger one — which is what
/// lets a warm-up memo be *extended* (rather than rebuilt) between the two sizes and keeps
/// the measured call on the memo.
fn seeded_table(table_size: usize) -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "nonode@nohost", "a", "b", "c", "d", "e"]);
    let synthetic: Vec<String> = (0..table_size).map(|n| format!("synthetic_atom_{n}")).collect();
    atoms.seed(synthetic.iter().map(String::as_str));
    atoms
}

/// An [`AtomInterner`] that records every novel name, predicting the context table's next
/// indices exactly as `beam_rt`'s `PendingInterner` does (its `base` is the table's length
/// when the call started).
///
/// The payload image carries no novel atom, so `names` must stay empty: a non-empty one
/// would mean the decode went down the interning path, and the measurement would no longer
/// describe a run that stays on the memo.
struct RecordingInterner {
    base: usize,
    names: Vec<String>,
}

impl RecordingInterner {
    fn new(base: usize) -> Self {
        RecordingInterner {
            base,
            names: Vec::new(),
        }
    }
}

impl AtomInterner for RecordingInterner {
    fn intern(&mut self, name: &str) -> Atom {
        let index = self.base + self.names.len();
        self.names.push(name.to_string());
        Atom::from_raw(index as u32)
    }
}

/// One armed measurement: the number of global allocations and the bytes they requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Measurement {
    allocations: usize,
    bytes: usize,
}

impl Measurement {
    /// Measures `body` under the armed counters.
    fn of(body: impl FnOnce()) -> Measurement {
        let (allocations, bytes, ()) = allocations_in(body);
        Measurement { allocations, bytes }
    }
}

/// Decodes `image` once **outside** the armed window (building and validating the
/// per-thread `DECODE_SCRATCH` memo, so the measured call is the steady state), then once
/// **inside** it, returning the armed allocation count and byte total.
///
/// Everything measured against — the table, the heaps, the registry, the image term, the
/// interner — is built by the caller. Only `BifContext::new` (a struct of borrowed
/// references) is constructed inside the window, and it allocates nothing. The heap reset
/// between the two calls is outside the window too: it only rewinds a bump pointer into a
/// block that is allocated once and never reallocated.
fn measure_steady_state(
    reg: &BifRegistry,
    atoms: &AtomTable,
    image: Eterm,
    heap: &mut Heap,
    interner: &mut RecordingInterner,
) -> Measurement {
    let pid = ProcessId::new(1, 0);

    // Warm-up: the memo is built (or extended/revalidated) here, never in the window.
    heap.reset();
    {
        let mut ctx = BifContext::new(heap, atoms, pid).with_atom_interner(interner);
        let term = reg
            .call("erlang", "binary_to_term", 1, &mut ctx, &[image])
            .expect("the warm-up decode succeeds");
        black_box(term);
    }
    // Rewind the destination heap so the measured call starts from the same bump position
    // (outside the armed window).
    heap.reset();

    Measurement::of(|| {
        let mut ctx = BifContext::new(heap, atoms, pid).with_atom_interner(interner);
        let term = reg
            .call("erlang", "binary_to_term", 1, &mut ctx, &[image])
            .expect("the decode succeeds");
        // Keep the decoded term observable so nothing above can be optimised away.
        black_box(term);
    })
}/// One shared-table size's evidence: the steady-state cost of a warm `binary_to_term/1`
/// and two positive controls over the very table whose size must not matter.
#[derive(Debug, Clone, Copy)]
struct Case {
    /// Number of atoms in this case's shared table.
    table_atoms: usize,
    /// Steady-state allocation of one warm `binary_to_term/1`.
    hot: Measurement,
    /// Allocation of one `AtomTable::clone()` of the same table — the operation the stale
    /// claim says happens per call.
    clone: Measurement,
    /// Allocation of the **exact counterfactual** (one private table clone plus the
    /// codec's decode of the same image), i.e. what the hot path would cost if the
    /// per-call clone came back.
    clone_decode: Measurement,
}

/// Builds the full scenario for one shared-table size (all outside the window) and returns
/// its measurements.
fn run_case(reg: &BifRegistry, table_size: usize, image: Eterm, image_bytes: &[u8]) -> Case {
    let atoms = seeded_table(table_size);
    let table_atoms = atoms.len();
    let mut heap = Heap::new(1 << 16).expect("destination heap");
    let mut interner = RecordingInterner::new(table_atoms);

    let hot = measure_steady_state(reg, &atoms, image, &mut heap, &mut interner);

    assert!(
        interner.names.is_empty(),
        "the payload image interned {} novel atom(s) — the steady-state measurement would \
         no longer describe a run that stays on the memo",
        interner.names.len()
    );
    assert_eq!(
        atoms.len(),
        table_atoms,
        "the decode must not mutate the shared table (novel names go through the interner)"
    );

    // Positive control 1: exactly the operation a per-call clone would perform.
    let clone = Measurement::of(|| {
        black_box(atoms.clone());
    });

    // Positive control 2: the regression verbatim — rebuild a private table copy per call
    // and decode into it (`beam_codec::decode_term_used` is the codec entry point the
    // pre-fix `binconv` used). Its own heap is allocated outside the window and only
    // rewound inside it.
    let mut legacy_heap = Heap::new(1 << 16).expect("counterfactual heap");
    let clone_decode = Measurement::of(|| {
        legacy_heap.reset();
        let mut local = atoms.clone();
        black_box(beam_codec::decode_term_used(image_bytes, &mut legacy_heap, &mut local).ok());
    });

    Case {
        table_atoms,
        hot,
        clone,
        clone_decode,
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// Steady state: the *same* canonical image, decoded with the memo already warm, must
/// allocate **exactly** as much for a ~4105-atom shared table as for a ~137-atom one — and
/// that shared cost must be a small payload-intrinsic constant, not the table.
///
/// The hard claim is the zero delta between the two sizes; the two controls in the same
/// harness prove the comparison would have caught a per-call `AtomTable::clone()`.
#[test]
fn binary_to_term_steady_state_allocates_independently_of_the_shared_table_size() {
    /// The smaller shared table: the nine fixed names plus 128 synthetic atoms.
    const SMALL: usize = 128;
    /// The larger shared table: the nine fixed names plus 4096 synthetic atoms — the scale
    /// `examples/etf_bench.rs` uses to stand in for a booted runtime's table.
    const LARGE: usize = 4096;

    let reg = default_registry();
    let image_bytes = payload_image();
    // The image is built on its own heap — it must outlive every measured call, and it is
    // built outside every window.
    let mut image_heap = Heap::new(4096).expect("image heap");
    let image = beam_codec::term::make_binary(&mut image_heap, &image_bytes).expect("image binary");

    let small = run_case(&reg, SMALL, image, &image_bytes);
    let large = run_case(&reg, LARGE, image, &image_bytes);

    eprintln!(
        "decode_hot_path: steady-state binary_to_term/1 over the {} byte image \
         => {} atoms: hot {} allocations ({} bytes) | AtomTable::clone() {} allocations \
         ({} bytes) | clone+decode {} allocations ({} bytes); \
         {} atoms: hot {} allocations ({} bytes) | clone {} allocations ({} bytes) | \
         clone+decode {} allocations ({} bytes)",
        image_bytes.len(),
        small.table_atoms,
        small.hot.allocations,
        small.hot.bytes,
        small.clone.allocations,
        small.clone.bytes,
        small.clone_decode.allocations,
        small.clone_decode.bytes,
        large.table_atoms,
        large.hot.allocations,
        large.hot.bytes,
        large.clone.allocations,
        large.clone.bytes,
        large.clone_decode.allocations,
        large.clone_decode.bytes,
    );

    // (1) The hard `== 0`: no allocation is attributable to the shared table's size. A
    // per-call `AtomTable::clone()` would make the larger table allocate ~30x as many
    // times (see the controls below), not the same number.
    assert_eq!(
        small.hot.allocations, large.hot.allocations,
        "the decode's allocation behaviour must not scale with the shared AtomTable \
         (a {} atom table cost {} allocations, a {} atom table {})",
        small.table_atoms, small.hot.allocations, large.table_atoms, large.hot.allocations
    );
    assert_eq!(
        small.hot.bytes, large.hot.bytes,
        "the decode's allocated byte total must not scale with the shared AtomTable \
         (a {} atom table cost {} bytes, a {} atom table {})",
        small.table_atoms, small.hot.bytes, large.table_atoms, large.hot.bytes
    );

    // (2) The controls: the measurement must be sensitive enough to have caught the clone
    // the stale claim describes. Both controls scale with the table...
    assert!(
        large.clone.allocations > small.clone.allocations,
        "the clone control must scale with the table: cloning {} atoms allocated {} times, \
         cloning {} atoms {} times",
        small.table_atoms,
        small.clone.allocations,
        large.table_atoms,
        large.clone.allocations
    );
    assert!(
        large.clone_decode.allocations > small.clone_decode.allocations,
        "the counterfactual (per-call clone + decode) must scale with the table: {} atoms \
         cost {} allocations, {} atoms {} allocations",
        small.table_atoms,
        small.clone_decode.allocations,
        large.table_atoms,
        large.clone_decode.allocations
    );
    // ...and both dwarf the hot path at both sizes (an order of magnitude at the smallest).
    for case in [small, large] {
        for (label, control) in [("clone", case.clone), ("clone+decode", case.clone_decode)] {
            assert!(
                case.hot.allocations * 10 < control.allocations,
                "the steady-state path ({} allocations over {} atoms) is not an order of \
                 magnitude below the {label} control ({} allocations over the same table)",
                case.hot.allocations,
                case.table_atoms,
                control.allocations
            );
            assert!(
                case.hot.bytes * 4 < control.bytes,
                "the steady-state path ({} bytes over {} atoms) is not far below the {label} \
                 control ({} bytes over the same table)",
                case.hot.bytes,
                case.table_atoms,
                control.bytes
            );
        }
    }

    // (3) The absolute count is the decode's payload-intrinsic transients, bounded.
    for case in [small, large] {
        assert!(
            case.hot.allocations <= STEADY_STATE_ALLOCATION_BUDGET,
            "the steady state over {} atoms allocated {} times, past the payload-intrinsic \
             budget of {STEADY_STATE_ALLOCATION_BUDGET}",
            case.table_atoms,
            case.hot.allocations
        );
        assert!(
            case.hot.bytes <= STEADY_STATE_BYTE_BUDGET,
            "the steady state over {} atoms allocated {} bytes, past the payload-intrinsic \
             budget of {STEADY_STATE_BYTE_BUDGET}",
            case.table_atoms,
            case.hot.bytes
        );
    }
}
