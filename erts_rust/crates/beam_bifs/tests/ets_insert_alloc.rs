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

//! Deterministic pin on the **transient allocation cost** of the `ets:insert/2`
//! and `ets:insert_new/2` entry points in `src/ets.rs`.
//!
//! ## Why
//!
//! `selection_objects` used to return a `Vec<Eterm>` unconditionally, so the
//! single-object form — `ets:insert(T, {k, V})`, the overwhelmingly common one —
//! built a `vec![object]` only to iterate it as a one-element slice: one
//! `malloc`/`free` per insert. It now answers a two-variant [`Selection`]
//! (`Single`/`List`) and hands the single form to the table through
//! `core::slice::from_ref`, which allocates nothing. The list form is unchanged
//! (it genuinely needs the `Vec` that `proper_list` builds).
//!
//! This is a `beam_bifs`-side cost only: the table's own copy into its term store
//! is `beam_ets`' business, and the figures below are measured *with* it, so the
//! assertion is the delta between the two forms rather than an absolute zero.
//!
//! ## The measurement
//!
//! The counting `#[global_allocator]` of `tests/binconv_alloc.rs` (armed
//! **per thread**, so the libtest harness can never perturb a window) reports the
//! number of `alloc`/`realloc` calls and the bytes they requested. Every figure is
//! host-load independent — an allocation either happens or it does not — and
//! nothing here asserts on wall-clock time.
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
use beam_bifs::{default_registry, BifContext, BifRegistry, TableRegistry};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

// ---------------------------------------------------------------------------
// The counting allocator (as in `tests/binconv_alloc.rs`)
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc` calls made on an armed thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Number of bytes requested on an armed thread.
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring.
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
        // Count only the growth (a shrink is not an allocation event).
        record(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Serializes this file's tests: the counters are process-global, so a window open
/// on one thread would otherwise absorb another test's allocations.
static SERIAL: Mutex<()> = Mutex::new(());

/// One armed measurement: the number of global allocations and the bytes they
/// requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Measurement {
    allocations: usize,
    bytes: usize,
}

impl Measurement {
    /// Measures `body` under the armed counters.
    fn of(body: impl FnOnce()) -> Measurement {
        // A poisoned lock only means another test panicked; the counters are still
        // meaningful for this window.
        let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Force the TLS to initialize *before* the window opens.
        ARMED.with(|armed| armed.set(false));
        let before_allocations = ALLOCATIONS.load(Ordering::Relaxed);
        let before_bytes = BYTES.load(Ordering::Relaxed);
        ARMED.with(|armed| armed.set(true));
        body();
        ARMED.with(|armed| armed.set(false));
        Measurement {
            allocations: ALLOCATIONS.load(Ordering::Relaxed) - before_allocations,
            bytes: BYTES.load(Ordering::Relaxed) - before_bytes,
        }
    }
}

// ---------------------------------------------------------------------------
// The fixture: a registry, a table and the terms to insert
// ---------------------------------------------------------------------------

/// A registry, a heap, an atom table and the live `TableRegistry` the `ets` BIFs
/// drive, plus the object terms (kept on a separate, never-rewound heap, because
/// the table copies them out of it).
struct Fixture {
    registry: BifRegistry,
    /// The `BifContext`'s destination heap: rewound between measured calls, never
    /// reallocated — so a rewound heap cannot appear as an allocation.
    dest: Heap,
    /// The heap the inserted objects live on; never rewound.
    terms: Heap,
    atoms: AtomTable,
    tables: TableRegistry,
    /// The `ets:new/2` handle of the table under test.
    handle: Eterm,
    /// A `{key, 1}` object whose key is present in the table.
    present: Eterm,
    /// A `{key, N}` object with a fresh key, for a `insert_new/2` that succeeds.
    fresh: Eterm,
}

impl Fixture {
    fn new() -> Fixture {
        let registry = default_registry();
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "set", "size", "key", "other"]);
        let mut dest = Heap::new(1 << 16).expect("destination heap");
        let mut terms = Heap::new(1 << 16).expect("terms heap");
        let mut tables = TableRegistry::new();
        // Every setup step is outside any measured window.
        let (handle, present, fresh) = {
            let mut ctx =
                BifContext::new(&mut dest, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
            let name = ctx.atom("key").expect("seeded");
            let kind = ctx.atom("set").expect("seeded");
            let options = ctx.list(&[kind]).expect("fits");
            let handle = registry
                .call("ets", "new", 2, &mut ctx, &[name, options])
                .expect("ets:new");
            // The table copies its objects, so where they were built is immaterial;
            // building them on `terms` keeps them alive across every rewind of `dest`.
            let mut tctx = BifContext::new(&mut terms, &atoms, ProcessId::new(1, 0));
            let k = tctx.atom("key").expect("seeded");
            let one = tctx.int(1).expect("fits");
            let present = tctx.tuple(&[k, one]).expect("fits");
            let other = tctx.atom("other").expect("seeded");
            let two = tctx.int(2).expect("fits");
            let fresh = tctx.tuple(&[other, two]).expect("fits");
            registry
                .call("ets", "insert", 2, &mut ctx, &[handle, present])
                .expect("the seed insert");
            (handle, present, fresh)
        };
        Fixture {
            registry,
            dest,
            terms,
            atoms,
            tables,
            handle,
            present,
            fresh,
        }
    }

    /// Calls `ets:function(Tab, Arg)` through the real registry on this fixture's
    /// table.
    fn call(&mut self, function: &str, arg: Eterm) -> Result<Eterm, BeamError> {
        let handle = self.handle;
        let mut ctx = BifContext::new(&mut self.dest, &self.atoms, ProcessId::new(1, 0))
            .with_tables(&mut self.tables);
        self.registry.call("ets", function, 2, &mut ctx, &[handle, arg])
    }

    /// Measures one `ets:function(Tab, Arg)` call: a warm-up call outside the window
    /// (so any first-call table state settles) and then the armed one.
    ///
    /// The workload is an upsert of an **already-present** key, so the table's
    /// interior only replaces a stored object — nothing index-shaped is allocated by
    /// `beam_ets`, and what the window sees is this crate's selection cost.
    fn measure_replace(&mut self, function: &str, arg: Eterm) -> Measurement {
        self.dest.reset();
        black_box(self.call(function, arg).expect("the warm-up call succeeds"));
        self.dest.reset();
        Measurement::of(|| {
            black_box(self.call(function, arg).expect("the measured call succeeds"));
        })
    }

    /// The proper list `[items…]` on the terms heap.
    fn list(&mut self, items: &[Eterm]) -> Eterm {
        let mut ctx = BifContext::new(&mut self.terms, &self.atoms, ProcessId::new(1, 0));
        ctx.list(items).expect("the list fits")
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// The single-object form of `ets:insert/2` / `ets:insert_new/2` must not allocate
/// inside `beam_bifs`; the list form still builds the `Vec` its selection needs, so
/// the two forms differ by exactly that one allocation.
#[test]
fn the_single_object_insert_selects_without_allocating() {
    let mut fixture = Fixture::new();

    // The single-object form. `present` is inserted: its key exists, so the table
    // replaces the stored object in place.
    let single_insert = fixture.measure_replace("insert", fixture.present);
    let single_insert_new = fixture.measure_replace("insert_new", fixture.present);

    // The list form of the same insert: `proper_list` builds a one-element `Vec`.
    let one = fixture.present;
    let list = fixture.list(&[one]);
    let list_insert = fixture.measure_replace("insert", list);

    eprintln!(
        "ets:insert/2 single {} alloc/{} B, insert_new/2 single {} alloc/{} B, \
         insert/2 [Object] {} alloc/{} B",
        single_insert.allocations,
        single_insert.bytes,
        single_insert_new.allocations,
        single_insert_new.bytes,
        list_insert.allocations,
        list_insert.bytes,
    );

    // The whole single-object `ets:insert/2` — selection *and* the replacement —
    // allocates nothing: before the two-variant `Selection` this was one allocation
    // (8 bytes) worse, the `vec![selection]` the selection built.
    assert_eq!(
        single_insert,
        Measurement {
            allocations: 0,
            bytes: 0
        },
        "ets:insert/2 of one object must not allocate; the selection's `vec![object]` \
         is back"
    );

    // `ets:insert_new/2` carries **one** allocation of its own, and it is not the
    // selection: `insert_new_into` builds a `keys` vector (`Vec::with_capacity(1)`:
    // one `Eterm` slot) to validate every key before touching the table. The
    // selection contributes nothing on top of `insert/2`'s zero, so the two forms
    // are pinned separately rather than by a shared bound.
    assert_eq!(
        single_insert_new,
        Measurement {
            allocations: 1,
            bytes: 8
        },
        "ets:insert_new/2 of one object must cost exactly its key vector"
    );

    // The list form pays the `proper_list` vector: an empty `Vec` grown by one push
    // (4 `Eterm` slots, 32 B). That is the shape the single form must *not* have, so
    // the pair pins both.
    assert_eq!(
        list_insert,
        Measurement {
            allocations: 1,
            bytes: 32
        },
        "the list form of ets:insert/2 should still cost exactly the selection's Vec"
    );
}

/// The selection's *semantics* are unchanged by the representation: `[]` selects no
/// object, a single non-tuple object still reaches the table's own `badarg`, and a
/// proper list is one object per element.
#[test]
fn the_single_and_list_forms_keep_their_semantics() {
    let mut fixture = Fixture::new();
    let nil = beam_term::NIL;

    // `ets:insert(T, [])` is `true` and inserts nothing (the oracle's behaviour).
    let inserted = fixture.call("insert", nil).expect("insert([]) is true");
    assert_eq!(fixture.atoms.name(beam_term::atom_val(inserted)), Some("true"));

    // A single non-tuple object is the table's `badarg` — the `Single` variant must
    // still hand it to the table rather than rejecting it on shape.
    let dest_ctx = BifContext::new(&mut fixture.dest, &fixture.atoms, ProcessId::new(1, 0));    let scalar = dest_ctx.int(7).expect("fits");
    drop(dest_ctx);
    assert!(
        matches!(fixture.call("insert", scalar), Err(BeamError::BadArg)),
        "a non-tuple single object is badarg"
    );

    // A two-object list inserts both.
    let present = fixture.present;
    let fresh = fixture.fresh;
    let both = fixture.list(&[present, fresh]);
    let inserted = fixture.call("insert", both).expect("the list form inserts");
    assert_eq!(fixture.atoms.name(beam_term::atom_val(inserted)), Some("true"));
    let size = {
        let handle = fixture.handle;
        let mut ctx = BifContext::new(&mut fixture.dest, &fixture.atoms, ProcessId::new(1, 0))
            .with_tables(&mut fixture.tables);
        let item = ctx.atom("size").expect("seeded");
        fixture.registry.call("ets", "info", 2, &mut ctx, &[handle, item])
    };
    let size = size.expect("ets:info(T, size)");
    assert_eq!(beam_term::signed_value(size), 2, "both objects are present");
}
