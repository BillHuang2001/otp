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

//! Deterministic, dependency-free pin on the **host-side ETF decode** allocation behaviour: it must
//! be independent of the size of the shared `AtomTable`.
//!
//! The driver host's `binary_to_term` (`DriverPort`'s [`DriverHost`] hook — the C's
//! `erl_drv_binary_to_term` / the term-data machine's `ERL_DRV_EXT2TERM` operand, `io.c:5824`) used
//! to rebuild a private copy of the caller's atom table on **every** call (`AtomTable::clone()`,
//! `O(table)` allocations, ~127 us for a ~3000-atom table — the whole cost of the decode, whose term
//! itself decodes in ~1 us). The decode now interns into a **memoized per-thread image** of the
//! caller's table (`with_decode_scratch` in `src/nif.rs`, shared with the NIF host
//! `NifHostImpl::binary_to_term` behind `enif_binary_to_term`), revalidated index by index
//! (`prefix_matches`, allocation-free) before each reuse. The memo's *semantics* — the stale-image
//! shapes it must reject — are pinned by the colocated units
//! `nif::tests::{the_same_table_twice_reuses_the_memo_without_moving_the_atom,
//! the_scratch_is_rebuilt_when_the_table_moves_a_name,
//! a_memo_left_ahead_does_not_shift_a_later_novel_atom}`; this file pins its *allocation behaviour*,
//! which is what a re-introduced per-call clone would violate.
//!
//! ## The measurement
//!
//! A counting `#[global_allocator]` (the exact pattern of `beam_bifs/tests/decode_hot_path.rs`)
//! wraps `std::alloc::System` with two `AtomicUsize` counters armed **per-thread**, so the libtest
//! harness and any sibling test can never perturb a measurement. This is host-load independent: an
//! allocation either happens or it does not, so nothing here asserts on wall-clock time.
//!
//! For **two very different shared-table sizes** (~137 and ~4105 atoms, seeded exactly as
//! `beam_bifs/tests/decode_hot_path.rs` seeds them) the same canonical image — whose only atom (`ok`)
//! is already interned, so the decode interns nothing — is decoded once **outside** the window
//! (building and validating the per-thread memo, so the measured call is the steady state) and once
//! **inside** it. Every heap block is allocated once and never reallocated. The host is built once
//! per case, outside the window.
//!
//! ## What is asserted
//!
//! 1. The two steady-state counts must be **identical** — the hard `== 0` on the table-attributable
//!    delta, and the observable form of "independent of the size of the shared `AtomTable`". A
//!    per-call `AtomTable::clone()` would allocate on the order of the table size and could not
//!    survive this comparison.
//! 2. The measurement is proven **sensitive** by two positive controls in the same harness, each
//!    measured per table size: an actual `AtomTable::clone()`, and the **exact counterfactual** this
//!    pin exists for — the pre-fix `binary_to_term` shape (`atoms.clone()` +
//!    `beam_codec::decode_term`). Both must scale with the table and sit an order of magnitude above
//!    the hot path. Without them, an accidentally-too-narrow window would make assertion (1)
//!    vacuous.
//! 3. The absolute count is bounded by a small, payload-intrinsic budget (see
//!    [`STEADY_STATE_ALLOCATION_BUDGET`]), far below even the smallest table clone here.
//!
//! `std`-only: no new dependencies. `beam_rt` is `#![deny(unsafe_code)]`, but this file is its own
//! crate root (the `#[global_allocator]` needs `unsafe impl`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_abi::DriverHost;
use beam_alloc::Heap;
use beam_process::ProcessId;
use beam_rt::port::driver::DriverPort;
use beam_term::AtomTable;

// ---------------------------------------------------------------------------
// The counting allocator (as in `beam_bifs/tests/decode_hot_path.rs`)
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc` calls made on an armed thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Number of bytes requested on an armed thread.
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring.
    ///
    /// The counters are process-global, but the *arming* is per-thread, so the test harness (which
    /// allocates while formatting output) can never perturb a measurement. That is what makes the
    /// allocation evidence deterministic rather than merely usually-zero.
    static ARMED: Cell<bool> = const { Cell::new(false) };
}

/// Records one allocation of `bytes` on the current thread while it is armed.
///
/// `try_with` tolerates a thread whose TLS is being torn down; with a `const`-init `Cell` there is no
/// lazy allocation and no destructor to run, so this is safe to call from inside the allocator.
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

/// Runs `body` with the counters armed on this thread and returns `(allocations, bytes, result)`.
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

/// A bound on the decode's own **payload-proportional** transients — the codec's `Vec` accumulators
/// for the tuple's three elements and the list's three elements. Measured value for this payload is
/// small; the bound leaves room for a differently-sized accumulator without admitting a table clone
/// (a clone of the *smallest* table here is hundreds of allocations).
const STEADY_STATE_ALLOCATION_BUDGET: usize = 16;
/// The byte half of [`STEADY_STATE_ALLOCATION_BUDGET`].
const STEADY_STATE_BYTE_BUDGET: usize = 1024;

/// The canonical image the measurement decodes: `{ok, [1,2,3], <<"hello">>}` — its only atom (`ok`)
/// is seeded into both tables below, so the decode interns nothing and the recorded-name path stays
/// empty.
fn payload_image() -> Vec<u8> {
    const VERSION_MAGIC: u8 = 131;
    const SMALL_TUPLE: u8 = 104;
    const SMALL_ATOM_UTF8: u8 = 119;
    const LIST: u8 = 108;
    const SMALL_INTEGER: u8 = 97;
    const NIL: u8 = 106;
    const BINARY: u8 = 109;

    let mut out = vec![VERSION_MAGIC, SMALL_TUPLE, 3];
    out.extend_from_slice(&[SMALL_ATOM_UTF8, 2, b'o', b'k']);
    out.extend_from_slice(&[LIST, 0, 0, 0, 3]);
    for n in 1..=3u8 {
        out.extend_from_slice(&[SMALL_INTEGER, n]);
    }
    out.push(NIL);
    out.extend_from_slice(&[BINARY, 0, 0, 0, 5]);
    out.extend_from_slice(b"hello");
    out
}

/// The shared table seeded exactly as `beam_bifs/tests/decode_hot_path.rs` seeds it: the runtime's
/// own atoms and the payload's, then `table_size` synthetic atoms. The synthetic names are appended,
/// so a smaller table is an exact index-prefix of a larger one — which is what lets a warm memo be
/// *extended* (rather than rebuilt) between the two sizes and keeps the measured call on the memo.
fn seeded_table(table_size: usize) -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "nonode@nohost", "a", "b", "c", "d", "e"]);
    let synthetic: Vec<String> = (0..table_size).map(|n| format!("synthetic_atom_{n}")).collect();
    atoms.seed(synthetic.iter().map(String::as_str));
    atoms
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

/// One shared-table size's evidence: the steady-state cost of a warm decode and two positive
/// controls over the very table whose size must not matter.
#[derive(Debug, Clone, Copy)]
struct Case {
    /// Number of atoms in this case's shared table.
    table_atoms: usize,
    /// Steady-state allocation of one warm `DriverPort::binary_to_term`.
    hot: Measurement,
    /// Allocation of one `AtomTable::clone()` of the same table — the operation the pre-fix code
    /// performed per call.
    clone: Measurement,
    /// Allocation of the **exact counterfactual** (one private table clone plus the codec's decode
    /// of the same image), i.e. what the hook would cost if the per-call clone came back.
    clone_decode: Measurement,
}

/// Builds the full scenario for one shared-table size (all outside the window) and returns its
/// measurements.
fn run_case(table_size: usize, image: &[u8]) -> Case {
    let atoms = Rc::new(seeded_table(table_size));
    let table_atoms = atoms.len();
    // The host owns its (never-reset) arena, so it is built once; the decodes are tiny and the block
    // is large enough for them all.
    let mut host = DriverPort::new(
        Heap::new(1 << 16).expect("destination heap"),
        Rc::clone(&atoms),
        ProcessId::new(1, 0),
    );

    // Warm-up: the per-thread memo is built (or extended/revalidated) here, never in the window.
    let warmed = host
        .binary_to_term(image)
        .expect("the warm-up decode succeeds");
    black_box(warmed);

    let hot = Measurement::of(|| {
        black_box(host.binary_to_term(image).expect("the decode succeeds"));
    });

    // The image introduces no name: the shared table is untouched and nothing was recorded for a
    // later commit.
    assert_eq!(
        atoms.len(),
        table_atoms,
        "the decode must not mutate the shared table"
    );
    let mut committed = (*atoms).clone();
    let dropped = host.commit_atoms(&mut committed);
    assert!(dropped.is_empty(), "no name was dropped: {dropped:?}");
    assert_eq!(
        committed.len(),
        table_atoms,
        "the payload image interned a novel atom — the steady-state measurement would no longer \
         describe a run that stays on the memo"
    );

    // Positive control 1: exactly the operation the pre-fix code performed per call. Note the
    // `(*atoms).clone()` deref — `atoms.clone()` on the `Rc<AtomTable>` would only bump the refcount
    // and allocate nothing, which is precisely what the pre-fix code did *not* do.
    let clone = Measurement::of(|| {
        black_box((*atoms).clone());
    });

    // Positive control 2: the regression verbatim — rebuild a private table copy per call and decode
    // into it. Its own heap is allocated outside the window.
    let mut legacy_heap = Heap::new(1 << 16).expect("counterfactual heap");
    let clone_decode = Measurement::of(|| {
        let mut local = (*atoms).clone();
        black_box(beam_codec::decode_term(image, &mut legacy_heap, &mut local).ok());
    });

    Case {
        table_atoms,
        hot,
        clone,
        clone_decode,
    }
}

/// The host-side decode's allocation behaviour is the steady state's, not the table's size.
#[test]
fn the_driver_decode_allocates_o_term_not_o_table() {
    // Named host-load context for the record (this pin does not depend on it).
    let loadavg = std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string());
    eprintln!("decode_scratch_alloc: /proc/loadavg = {loadavg}");

    let image = payload_image();

    // The small table first, so the large case *extends* the memo the small one left (still outside
    // every window).
    let small = run_case(128, &image);
    let large = run_case(4096, &image);

    eprintln!(
        "decode_scratch_alloc: steady state => {} atoms: {} allocations / {} bytes; \
         {} atoms: {} allocations / {} bytes",
        small.table_atoms,
        small.hot.allocations,
        small.hot.bytes,
        large.table_atoms,
        large.hot.allocations,
        large.hot.bytes,
    );
    eprintln!(
        "decode_scratch_alloc: controls => clone: {} / {} (small), {} / {} (large); \
         clone+decode: {} / {} (small), {} / {} (large)",
        small.clone.allocations,
        small.clone.bytes,
        large.clone.allocations,
        large.clone.bytes,
        small.clone_decode.allocations,
        small.clone_decode.bytes,
        large.clone_decode.allocations,
        large.clone_decode.bytes,
    );

    // (1) The hard `== 0`: no allocation is attributable to the shared table's size. The pre-fix
    // per-call `AtomTable::clone()` would make the larger table allocate ~30x as many times (see the
    // controls below), not the same number.
    assert_eq!(
        small.hot.allocations, large.hot.allocations,
        "the host decode's allocation behaviour must not scale with the shared AtomTable \
         (a {} atom table cost {} allocations, a {} atom table {})",
        small.table_atoms, small.hot.allocations, large.table_atoms, large.hot.allocations
    );
    assert_eq!(
        small.hot.bytes, large.hot.bytes,
        "the host decode's allocated byte total must not scale with the shared AtomTable \
         (a {} atom table cost {} bytes, a {} atom table {})",
        small.table_atoms, small.hot.bytes, large.table_atoms, large.hot.bytes
    );

    // (2) The controls: the measurement must be sensitive enough to have caught the clone the
    // pre-fix code performed. Both controls scale with the table...
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
