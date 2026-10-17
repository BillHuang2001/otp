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

//! Deterministic, dependency-free pin on the **transient allocation cost** of
//! `erlang:length/1` and `erlang:list_to_tuple/1` in `src/term_ops.rs`.
//!
//! ## Why
//!
//! Both BIFs used to walk the list through one `proper_list` that collected the list's
//! *heads* into a `Vec::new()`, grown by doubling:
//!
//! * `length/1` only needs the **count**, so that whole `Vec` was waste — a list of
//!   `n` elements cost ~`log2(n)` reallocations to compute an integer.
//! * `list_to_tuple/1` does need the elements, but it built them in a `Vec` that grew
//!   from capacity 0 before `ctx.tuple` copied it once onto the caller's heap arena.
//!
//! `length/1` now counts the list (`proper_list_len`) and never allocates, and
//! `proper_list` reserves the list's **exact** length before filling it, so
//! `list_to_tuple/1` allocates its element buffer exactly once.
//!
//! ## The measurement
//!
//! A counting `#[global_allocator]` (the exact pattern of
//! `tests/{binconv_alloc,registry_hot_path,decode_hot_path}.rs`, armed **per thread** so
//! the libtest harness can never perturb a window) reports the number of `alloc`/
//! `realloc` calls and the bytes they requested. Every figure below is therefore
//! host-load independent — an allocation either happens or it does not — and nothing
//! here asserts on wall-clock time.
//!
//! The counts are global-allocator only: the result terms are built on a
//! `beam_alloc::Heap` (a bump allocator over one preallocated arena), so neither the
//! `length/1` small-integer result nor the `list_to_tuple/1` tuple block is charged
//! here — exactly as in the runtime, where those live on the process/BIF heap.
//!
//! | case | before (alloc/bytes) | after (alloc/bytes) |
//! |---|---|---|
//! | `length/1` of a 1 000-element list     | 9 / 8192 | **0 / 0** |
//! | `list_to_tuple/1` of a 100-element list | 6 / 1024 | **1 / 800** |
//! | `length([])`                            | 0 / 0    | 0 / 0     |
//! | `list_to_tuple([])`                     | 0 / 0    | 0 / 0     |
//! | `length([1|2])` (improper, `badarg`)    | 1 / 32   | **0 / 0** |
//! | `list_to_tuple([1|2])` (improper, `badarg`) | 1 / 32 | **0 / 0** |
//! | `length(7)` / `list_to_tuple(7)` (not a list, `badarg`) | 0 / 0 | 0 / 0 |
//!
//! The pre-fix column is what this same harness reported against the unmodified
//! `src/term_ops.rs`, and the doubling is visible in it: a 1 000-element list reached
//! capacity 1024 through capacities 4, 8, …, 1024 (one `alloc` plus eight `realloc`s,
//! 32 + 8160 bytes), and a 100-element one through 4, 8, …, 128 (one `alloc` plus five
//! `realloc`s). The improper-list cases are the same effect one step earlier: the old
//! walk pushed the first head before discovering the non-`nil` tail, so it allocated on
//! a call that only ever returned `badarg`. The assertions below pin the post-fix
//! column exactly (an allocation either happens or it does not — the counts are
//! deterministic), so any regression on either BIF fails loudly.
//!
//! `std`-only: no new dependencies. The `beam_bifs` lib is `#![deny(unsafe_code)]`,
//! but this file is its own crate root (the `#[global_allocator]` needs
//! `unsafe impl`).
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{make_small, AtomTable, Eterm, NIL};

// ---------------------------------------------------------------------------
// The counting allocator (as in `tests/binconv_alloc.rs`)
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

/// Serializes this file's measurements: the counters are process-global, so a window
/// open on one thread would otherwise absorb another test's allocations (the arming is
/// per-thread, but the *totals* are not). Same guard as `tests/binconv_alloc.rs`.
static SERIAL: Mutex<()> = Mutex::new(());

/// Runs `body` with the counters armed on this thread and returns
/// `(allocations, bytes, result)`.
///
/// The window holds [`SERIAL`], so two tests in this binary never measure at once.
fn allocations_in<T>(body: impl FnOnce() -> T) -> (usize, usize, T) {
    // A poisoned lock only means another test panicked; the counters are still
    // meaningful for this window, so recover rather than fail on the poison.
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
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

/// One armed measurement: the number of global allocations, the bytes they requested,
/// and what the measured call returned.
#[derive(Debug)]
struct Measurement {
    allocations: usize,
    bytes: usize,
    result: Result<Eterm>,
}

impl Measurement {
    /// Reports itself in the same one-line form as `tests/binconv_alloc.rs`.
    fn report(&self, label: &str) {
        eprintln!(
            "{label}: {} alloc / {} B -> {:?}",
            self.allocations,
            self.bytes,
            self.result
                .as_ref()
                .map(|_| "ok")
                .map_err(|e| format!("{e:?}"))
        );
    }

    /// Asserts the exact `(allocations, bytes)` cost of the measured call.
    fn assert_cost(&self, label: &str, allocations: usize, bytes: usize) {
        assert_eq!(
            (self.allocations, self.bytes),
            (allocations, bytes),
            "{label} allocated {} times / {} bytes, not the pinned {allocations} / {bytes}",
            self.allocations,
            self.bytes,
        );
    }

    /// Asserts the call raised the argument error the oracle raises.
    fn assert_badarg(&self, label: &str) {
        assert!(
            matches!(self.result, Err(BeamError::BadArg)),
            "{label} must stay `badarg`, got {:?}",
            self.result.as_ref().map(|_| "ok"),
        );
    }
}

// ---------------------------------------------------------------------------
// The workloads
// ---------------------------------------------------------------------------

/// Measures one `erlang` call of arity 1 on the registry, warm-up outside the window.
///
/// `arg` lives on the *machine* heap (`terms`), `dest` is the [`BifContext`]'s heap
/// where a result term is built — **different** arenas, exactly as in the runtime
/// (process heap vs. the pooled BIF heap), so rewinding `dest` between the two calls
/// can never disturb the argument.
fn measure(
    reg: &BifRegistry,
    function: &str,
    atoms: &AtomTable,
    dest: &mut Heap,
    arg: Eterm,
) -> Measurement {
    let pid = ProcessId::new(1, 0);
    // Warm-up (outside the window).
    dest.reset();
    {
        let mut ctx = BifContext::new(dest, atoms, pid);
        let _ = black_box(reg.call("erlang", function, 1, &mut ctx, &[arg]));
    }
    dest.reset();

    let (allocations, bytes, result) = allocations_in(|| {
        let mut ctx = BifContext::new(dest, atoms, pid);
        black_box(reg.call("erlang", function, 1, &mut ctx, &[arg]))
    });
    Measurement {
        allocations,
        bytes,
        result,
    }
}

/// `erlang:tuple_size/1` of the term a measured call built (unarmed), so the shape of
/// the result is checked through the registry rather than a term-layout helper: a
/// successful `tuple_size` *is* the proof that the result is a tuple.
fn tuple_size_of(reg: &BifRegistry, atoms: &AtomTable, dest: &mut Heap, tuple: Eterm) -> i64 {
    let pid = ProcessId::new(1, 0);
    let mut ctx = BifContext::new(dest, atoms, pid);
    let size = reg
        .call("erlang", "tuple_size", 1, &mut ctx, &[tuple])
        .expect("tuple_size/1 of a tuple");
    beam_term::signed_value(size)
}

/// A fresh destination heap, sized for the largest tuple this file builds.
fn destination() -> Heap {
    Heap::new(1 << 16).expect("destination heap")
}

/// A proper list of `n` small integers, on a heap that is never reset.
fn list_of(n: usize, heap: &mut Heap) -> Eterm {
    let elems: Vec<Eterm> = (0..n).map(|i| make_small(i as i64)).collect();
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the workload list fits");
    }
    acc
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// `length/1` needs only the count: the walk must not allocate at all, for any list
/// length. A 1 000-element list is the case the old `proper_list` cost 9 allocations /
/// 8 192 bytes (a `Vec` grown from capacity 4 to 1024) — the count is now free.
#[test]
fn length_of_a_thousand_elements_allocates_nothing() {
    let reg = default_registry();
    let atoms = AtomTable::new();
    let mut terms = Heap::new(1 << 16).expect("machine heap");
    let mut dest = destination();
    let thousand = list_of(1_000, &mut terms);

    let measured = measure(&reg, "length", &atoms, &mut dest, thousand);
    measured.report("length(1000 elements)");
    assert_eq!(
        measured.result.as_ref().expect("length of a proper list"),
        &make_small(1_000),
        "length/1 of a 1 000-element list"
    );
    measured.assert_cost("length/1 of a 1 000-element list", 0, 0);
}

/// `list_to_tuple/1` needs the elements, but only one buffer: counting first lets the
/// `Vec` be reserved at the list's exact length, so the doubling (6 allocations /
/// 1 024 bytes for 100 elements) is gone. `dest.tuple` then copies the elements once
/// onto the `beam_alloc` arena, which is not a global allocation.
#[test]
fn list_to_tuple_of_a_hundred_elements_allocates_once() {
    let reg = default_registry();
    let atoms = AtomTable::new();
    let mut terms = Heap::new(1 << 16).expect("machine heap");
    let mut dest = destination();
    let hundred = list_of(100, &mut terms);

    let measured = measure(&reg, "list_to_tuple", &atoms, &mut dest, hundred);
    measured.report("list_to_tuple(100 elements)");
    let tuple = *measured
        .result
        .as_ref()
        .expect("list_to_tuple of a proper list");
    assert_eq!(
        tuple_size_of(&reg, &atoms, &mut dest, tuple),
        100,
        "the tuple's arity"
    );
    // One allocation: `Vec::with_capacity(100)` of 8-byte `Eterm`s = 800 bytes.
    measured.assert_cost("list_to_tuple/1 of a 100-element list", 1, 800);
}

/// The empty list: `length([])` is `0` and `list_to_tuple([])` is `{}`, and neither
/// allocates (a zero-capacity `Vec` never reaches the allocator).
#[test]
fn the_empty_list_neither_counts_nor_collects() {
    let reg = default_registry();
    let atoms = AtomTable::new();
    let mut dest = destination();

    let len = measure(&reg, "length", &atoms, &mut dest, NIL);
    len.report("length([])");
    assert_eq!(
        len.result.as_ref().expect("length([]) works"),
        &make_small(0)
    );
    len.assert_cost("length([])", 0, 0);

    let tup = measure(&reg, "list_to_tuple", &atoms, &mut dest, NIL);
    tup.report("list_to_tuple([])");
    let empty = *tup.result.as_ref().expect("list_to_tuple([]) works");
    assert_eq!(
        tuple_size_of(&reg, &atoms, &mut dest, empty),
        0,
        "the empty tuple's arity"
    );
    tup.assert_cost("list_to_tuple([])", 0, 0);
}

/// The rejection shapes are untouched (`badarg`, per the module doc's oracle pin) and
/// now cheaper: the count pass discovers the improper tail / the non-list *before* any
/// element buffer exists, so an improper `[1|2]` no longer allocates the way the old
/// push-then-discover walk did (1 allocation / 32 bytes).
#[test]
fn the_badarg_shapes_stay_badarg_and_allocate_nothing() {
    let reg = default_registry();
    let atoms = AtomTable::new();
    let mut terms = Heap::new(1 << 16).expect("machine heap");
    let mut dest = destination();
    // `[1|2]`: a cons cell whose tail is neither a cons nor `nil`.
    let improper = terms
        .cons(make_small(1), make_small(2))
        .expect("improper list");
    // A bare atom is not a list at all.
    let not_a_list = make_small(7);

    for (function, label) in [("length", "length"), ("list_to_tuple", "list_to_tuple")] {
        let bad = measure(&reg, function, &atoms, &mut dest, improper);
        bad.report(&format!("{label}([1|2])"));
        bad.assert_badarg(&format!("{label}/1 of an improper list"));
        bad.assert_cost(&format!("{label}/1 of an improper list"), 0, 0);

        let not = measure(&reg, function, &atoms, &mut dest, not_a_list);
        not.report(&format!("{label}(7)"));
        not.assert_badarg(&format!("{label}/1 of a non-list"));
        not.assert_cost(&format!("{label}/1 of a non-list"), 0, 0);
    }
}
