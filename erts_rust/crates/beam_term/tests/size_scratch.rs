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

//! `SizingScratch` — the caller-owned, reusable working memory of the sizing walk.
//!
//! The scratch changes *where a walk's buffers come from*, never what it counts. This
//! file pins all three halves of that contract:
//!
//! 1. **(a) equivalence** — [`size_object_with_scratch`] /
//!    [`size_object_bounded_with_scratch`] answer exactly what the scratch-free
//!    [`size_object_with`] / [`size_object_bounded_with`] answer, over a broad corpus
//!    (immediates, lists, tuples, boxed leaves, flat/HAMT maps, all three fun kinds, refc
//!    binaries, a match-context sub-binary, external pid/port/ref, a native record, a
//!    literal arena, a cyclic cell, a 100 000-element list) × four [`SizingPolicy`]s ×
//!    every interesting budget — including the two typed errors, in both directions
//!    (`TermGraphTooLarge` and the priority of `TermGraphCyclic` over it);
//! 2. **(b) allocation** — a counting `#[global_allocator]` (the per-thread-armed pattern
//!    of `tests/intern_alloc.rs`) proves a **warm** scratch makes a call allocation-free,
//!    while the scratch-free entry point allocates per call and **scales with the graph**
//!    it is given;
//! 3. **(c) no state** — one scratch reused across *different* graphs (cyclic, then
//!    acyclic, then cyclic) answers each one correctly, and a scratch left dirty by an
//!    early return (`Cyclic`/`OverLimit` leave entries behind) does not perturb the next
//!    call.
//!
//! Allocation counts are host-load independent — an allocation either happens or it does
//! not — which is why nothing here asserts on wall-clock time; the timing evidence lives
//! in `examples/size_walk_bench.rs`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use beam_term::prelude::*;

// ---------------------------------------------------------------------------
// The counting allocator (the `tests/intern_alloc.rs` pattern)
// ---------------------------------------------------------------------------

/// Number of `alloc`/`realloc`(growth) calls made on an armed thread (monotonic).
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
/// Bytes requested on an armed thread (monotonic).
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Set only on the thread that is currently measuring: the counters are process-global
    /// but *arming* is per-thread, so the libtest harness and sibling tests cannot
    /// perturb a measurement.
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
            "size_scratch: {label} => {} allocations / {} bytes",
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
// Policies under test
// ---------------------------------------------------------------------------

/// The canonical [`object_words`] answer (the trait default, spelled out so the overrides
/// below stay readable).
fn layout_words(header: Eterm, payload: *const Eterm) -> Option<usize> {
    // SAFETY: the walk only calls a policy with a live, initialized object of `header`'s
    // shape — `object_words`'s documented caller contract.
    Some(unsafe { object_words(header, payload) })
}

/// The sharing axis: a literal-area subterm is reused by identity, so it costs nothing
/// (`litopt` in `copy.c:99`/`:119`).
fn shared_literal(t: Eterm) -> bool {
    (is_boxed(t) || is_list(t)) && is_literal(t)
}

/// The pricing axis: a refc binary whose payload no source off-heap handle owns is
/// materialised as its flattened window (`copy.c:232-239`) instead of the 8-word pair.
fn flatten_refc_window(header: Eterm, payload: *const Eterm) -> Option<usize> {
    if header.raw() & TAG_HEADER_MASK != SUB_BITS_SUBTAG {
        return None;
    }
    // SAFETY: `header` heads a live `ErlSubBits`, so words 2..4 (`start`, `end`, `orig`)
    // are initialized.
    let (start, end, orig) = unsafe {
        (
            *payload.add(SUB_BITS_START_WORD),
            *payload.add(SUB_BITS_END_WORD),
            *payload.add(SUB_BITS_ORIG_WORD),
        )
    };
    if !is_boxed_term(orig) {
        return None;
    }
    // SAFETY: `orig` is a boxed term, so its header word is a live term word.
    if unsafe { *boxed_val(orig) } != HEADER_BIN_REF {
        return None;
    }
    Some(heap_bits_size_words(end.raw().saturating_sub(start.raw())))
}

/// Shares literal-area subterms only.
struct LiteralSharing;
impl SizingPolicy for LiteralSharing {
    fn is_shared(&self, t: Eterm) -> bool {
        shared_literal(t)
    }
}

/// Both axes at once — the shape a copier's policy has.
struct CopyLike;
impl SizingPolicy for CopyLike {
    fn is_shared(&self, t: Eterm) -> bool {
        shared_literal(t)
    }

    fn object_words(&self, header: Eterm, payload: *const Eterm) -> Option<usize> {
        flatten_refc_window(header, payload).or_else(|| layout_words(header, payload))
    }
}

/// Prices no boxed object at all (`object_words` is `None` for every shape).
struct Priceless;
impl SizingPolicy for Priceless {
    fn object_words(&self, header: Eterm, payload: *const Eterm) -> Option<usize> {
        let _ = (header, payload);
        None
    }
}

/// Every policy the agreement sweep runs: the canonical one plus each axis and both.
fn policies() -> [(&'static str, &'static dyn SizingPolicy); 4] {
    static DEFAULT: DefaultSizingPolicy = DefaultSizingPolicy;
    static LITERAL: LiteralSharing = LiteralSharing;
    static COPY_LIKE: CopyLike = CopyLike;
    static PRICELESS: Priceless = Priceless;
    [
        ("default", &DEFAULT),
        ("literal-sharing", &LITERAL),
        ("copy-like", &COPY_LIKE),
        ("priceless", &PRICELESS),
    ]
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// The non-term words (`tests/non_term.rs` pins them): bare `HEADER`-tagged words.
fn non_terms() -> [Eterm; 5] {
    [
        THE_NON_VALUE,
        make_header(0, FLOAT_SUBTAG),
        HEADER_FLONUM,
        Eterm::from_raw(0x3C),
        Eterm::from_raw(0x100),
    ]
}

/// Every corpus term, plus the storage that has to outlive it.
///
/// `Heap`/`Vec` are moved but their *buffers* are not, so the interior pointers the terms
/// carry stay valid for the whole test.
struct Corpus {
    _heap: Heap,
    /// `write_map` layouts and the literal arena.
    _buffers: Vec<Vec<Eterm>>,
    /// The off-heap payload the refc binary points at (`Box`ed so its address is fixed
    /// before the term is built).
    _payload: Box<[u64; 1]>,
    terms: Vec<(&'static str, Eterm)>,
}

impl Corpus {
    fn terms(&self) -> &[(&'static str, Eterm)] {
        &self.terms
    }

    fn find(&self, name: &str) -> Eterm {
        self.terms
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| *t)
            .unwrap_or_else(|| panic!("the corpus must contain {name}"))
    }
}

/// Builds one generous arena per test and every shape the sizing walk knows.
fn corpus() -> Corpus {
    // Generous: the corpus ends with a 100 000-cell list (200 000 words).
    let mut heap = Heap::new(260_000);
    let mut buffers: Vec<Vec<Eterm>> = Vec::new();
    let mut terms: Vec<(&'static str, Eterm)> = Vec::new();
    let mut table = AtomTable::new();
    let atom = make_atom(table.intern("an_atom"));

    // --- immediates ---------------------------------------------------------
    terms.push(("small int", make_small(42)));
    terms.push(("nil", NIL));
    terms.push(("atom", atom));
    terms.push(("internal pid", make_internal_pid(make_pid_data(3, 5))));
    terms.push(("internal port", make_internal_port(make_port_data(5))));
    terms.push(("tuple0", tuple0()));

    // --- lists --------------------------------------------------------------
    let one = heap.cons(make_small(1), NIL).unwrap();
    terms.push(("one-cell list", one));
    let cell3 = heap.cons(make_small(3), NIL).unwrap();
    let cell23 = heap.cons(make_small(2), cell3).unwrap();
    let three = heap.cons(make_small(1), cell23).unwrap();
    terms.push(("three-cell list", three));
    let inner_cell = heap.cons(make_small(7), NIL).unwrap();
    let nested_tail = heap.cons(make_small(8), NIL).unwrap();
    let nested_list = heap.cons(inner_cell, nested_tail).unwrap();
    terms.push(("nested list", nested_list));

    // --- tuples -------------------------------------------------------------
    let inner_tuple = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
    let nested_tuple = heap.tuple(&[inner_tuple, make_small(3)]).unwrap();
    terms.push(("nested tuple", nested_tuple));

    // --- boxed leaves -------------------------------------------------------
    // SAFETY: `FLOAT_SIZE_WORDS` (2) words are reserved; the heap owns them.
    let float = unsafe { write_float(heap.alloc_words(FLOAT_SIZE_WORDS).unwrap(), 3.25) };
    terms.push(("float", float));
    // SAFETY: `big_size_words(2)` (3) words are reserved; the heap owns them.
    let big = unsafe {
        write_big(
            heap.alloc_words(big_size_words(2)).unwrap(),
            false,
            &[0xDEAD_BEEF_1234_5678, 1],
        )
    };
    terms.push(("2-digit bignum", big));

    // --- maps: flat at the boundary, HAMT past it ---------------------------
    for n in [0i64, 1, 3, 32, 33, 100] {
        let name: &'static str = match n {
            0 => "flat map (0 keys)",
            1 => "flat map (1 key)",
            3 => "flat map (3 keys)",
            32 => "flat map (32 keys)",
            33 => "HAMT map (33 keys)",
            _ => "HAMT map (100 keys)",
        };
        let pairs: Vec<(Eterm, Eterm)> = (1..=n)
            .map(|k| (make_small(k), make_small(k * 7)))
            .collect();
        let atoms = AtomTable::new();
        let mut buf = vec![Eterm::from_raw(0xDEAD_BEEF); map_words_capacity(pairs.len())];
        // SAFETY: `buf` owns exactly the `map_words_capacity` words `write_map` needs and
        // outlives the term (it is kept in `buffers`).
        let map = unsafe { write_map(buf.as_mut_ptr(), &pairs, &atoms) }.unwrap();
        terms.push((name, map));
        buffers.push(buf);
    }

    // --- funs ---------------------------------------------------------------
    let free = [make_small(1), make_small(2)];
    // SAFETY: `fun_words` sizes the block exactly; the heap owns it.
    let local_fun = unsafe {
        let hp = heap
            .alloc_words(fun_words(FunKind::Local, free.len()))
            .unwrap();
        write_fun_local(hp, 2, 0x1234, 5, &free).unwrap()
    };
    terms.push(("local fun (2 free vars)", local_fun));
    // SAFETY: `fun_words(External, 0)` (4) words are reserved; the heap owns them.
    let external_fun = unsafe {
        let hp = heap.alloc_words(fun_words(FunKind::External, 0)).unwrap();
        write_fun_external(hp, 2, Atom::from_raw(1), Atom::from_raw(2)).unwrap()
    };
    terms.push(("external fun", external_fun));
    let env = [make_small(9)];
    // SAFETY: `fun_words(Imported, 1)` sizes the block exactly; the heap owns it.
    let imported_fun = unsafe {
        let hp = heap
            .alloc_words(fun_words(FunKind::Imported, env.len()))
            .unwrap();
        write_fun_imported(
            hp,
            0,
            Atom::from_raw(1),
            &[0u8; 16],
            1,
            2,
            3,
            FunPid {
                node: Atom::from_raw(2),
                number: 4,
                serial: 5,
                creation: 6,
            },
            &env,
        )
        .unwrap()
    };
    terms.push(("imported fun (1 free var)", imported_fun));

    // --- binaries -----------------------------------------------------------
    let payload: Box<[u64; 1]> = Box::new([0x0102_0304_0506_0708]);
    // SAFETY: 8 words are reserved for the `[BinRef][ErlSubBits]` pair; the payload is
    // word-aligned and outlives the term (the box is kept in the returned `Corpus`).
    let refc = unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).unwrap();
        write_refc_binary(
            hp,
            payload.as_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.as_ptr() as *const u8,
            0,
            64,
        )
    };
    terms.push(("refc binary", refc));
    // SAFETY: `heap_bits_size_words(520)` (11) words are reserved; the heap owns them.
    let heap_bits = unsafe {
        let hp = heap.alloc_words(heap_bits_size_words(520)).unwrap();
        write_heap_bits(hp, &[0xAA; 65], 520).unwrap()
    };
    // SAFETY: 5 words for the `ErlSubBits`; `heap_bits` is a live heap bitstring.
    let match_ctx = unsafe {
        let hp = heap.alloc_words(ERL_SUB_BITS_SIZE).unwrap();
        write_sub_bits(
            hp,
            Eterm::from_raw(0),
            Eterm::from_raw(0),
            Eterm::from_raw(520),
            heap_bits,
        )
    };
    terms.push(("match context (520-bit window)", match_ctx));

    // --- external identifiers ----------------------------------------------
    // SAFETY: `external_pid_size_words()` (4) words are reserved.
    let ext_pid = unsafe {
        let hp = heap.alloc_words(external_pid_size_words()).unwrap();
        write_external_pid(hp, make_atom(Atom::from_raw(1)), 7, 8, 9)
    };
    terms.push(("external pid", ext_pid));
    // SAFETY: `external_port_size_words()` (4) words are reserved.
    let ext_port = unsafe {
        let hp = heap.alloc_words(external_port_size_words()).unwrap();
        write_external_port(hp, make_atom(Atom::from_raw(1)), 0x1122_3344, 5)
    };
    terms.push(("external port", ext_port));
    let ids = [1u32, 2, 3];
    // SAFETY: `external_ref_size_words(ids.len())` words are reserved.
    let ext_ref = unsafe {
        let hp = heap
            .alloc_words(external_ref_size_words(ids.len()))
            .unwrap();
        write_external_ref(hp, make_atom(Atom::from_raw(1)), &ids, 5)
    };
    terms.push(("external ref", ext_ref));

    // --- a native record ----------------------------------------------------
    let fields = [
        (make_atom(table.intern("a")), make_small(1)),
        (make_atom(table.intern("b")), make_small(2)),
    ];
    // SAFETY: `record_block_words(2)` sizes the whole construction block.
    let record = unsafe {
        let hp = heap.alloc_words(record_block_words(fields.len())).unwrap();
        write_record(
            hp,
            table.intern("m"),
            table.intern("r"),
            false,
            &fields,
            make_small(0),
        )
        .unwrap()
    };
    terms.push(("native record (2 fields)", record));

    // --- a literal area -----------------------------------------------------
    // Arena: `{ {7}, [9] }` laid out header-first, exactly as the C literal-area walk
    // expects (`tag_words` steps over a header's covered words).
    let mut words = vec![NIL; 7];
    words[0] = make_arityval(2); // the outer tuple's header
    words[3] = make_arityval(1); // the nested tuple's header
    words[4] = make_small(7); // ... its single element
    words[5] = make_small(9); // the cons cell's CAR
    words[6] = NIL; // ... and its CDR
    let base = words.as_mut_ptr();
    // SAFETY: `words` owns 7 words and is never resized afterwards, so `base` stays
    // valid; the two pointer slots are written before the arena is tagged.
    unsafe {
        words[1] = make_boxed(base.add(3));
        words[2] = make_list(base.add(5));
    }
    let mut literal = make_tuple(base);
    tag_literal_area(&mut words, core::slice::from_mut(&mut literal));
    assert!(is_literal(literal), "the root must be literal-tagged");
    terms.push(("literal-tagged tuple", literal));
    buffers.push(words);

    // --- a composite of several shapes --------------------------------------
    let flat3 = terms
        .iter()
        .find(|(n, _)| *n == "flat map (3 keys)")
        .map(|(_, t)| *t)
        .unwrap();
    let composite = heap
        .tuple(&[float, refc, local_fun, one, flat3, big])
        .unwrap();
    terms.push(("composite tuple", composite));

    // --- a cyclic cell (`L = [1 | L]`) --------------------------------------
    // SAFETY: 2 words are reserved for the cons cell and both are written before the term
    // is handed to any accessor.
    let p = unsafe { heap.alloc_words(2).unwrap() };
    let cell = make_list(p);
    // SAFETY: as above — the `cdr` slot is live and this only rewrites it.
    unsafe {
        *p = make_small(1);
        *p.add(1) = cell;
    }
    terms.push(("cyclic cell", cell));

    // --- a 100 000-element list --------------------------------------------
    let mut list = NIL;
    for i in 0..100_000i64 {
        list = heap.cons(make_small(i), list).unwrap();
    }
    terms.push(("100000-cell list", list));

    Corpus {
        _heap: heap,
        _buffers: buffers,
        _payload: payload,
        terms,
    }
}

/// The budgets a term is swept at: every boundary that can change the verdict, skipped
/// down to the three that matter for a graph too large to walk six times cheaply.
fn limits(n: usize) -> Vec<usize> {
    if n > 10_000 {
        vec![n.saturating_sub(1), n, usize::MAX]
    } else {
        vec![0, 1, n.saturating_sub(1), n, n + 1, usize::MAX]
    }
}

// ---------------------------------------------------------------------------
// (a) Equivalence
// ---------------------------------------------------------------------------

#[test]
fn scratch_backed_and_scratch_free_entry_points_agree() {
    let c = corpus();
    // Two scratches: one fresh per term, one carried across the whole sweep in reverse
    // order, so every call also inherits whatever the previous graph left behind.
    let mut carried = SizingScratch::new();
    let mut reversed: Vec<&(&'static str, Eterm)> = c.terms().iter().collect();
    reversed.reverse();

    for (name, term) in c.terms() {
        let n = size_object(*term);
        for (policy_name, policy) in policies() {
            // Unbounded: same count, and the same count on a *reused* scratch.
            assert_eq!(
                size_object_with_scratch(*term, policy, &mut SizingScratch::new()),
                size_object_with(*term, policy),
                "{name} / {policy_name}: a fresh scratch is the scratch-free count"
            );
            assert_eq!(
                size_object_with_scratch(*term, policy, &mut carried),
                size_object_with(*term, policy),
                "{name} / {policy_name}: a carried scratch is the scratch-free count"
            );
            // Bounded: same verdict, at every boundary.
            for limit in limits(n) {
                let expected = size_object_bounded_with(*term, limit, policy);
                assert_eq!(
                    size_object_bounded_with_scratch(
                        *term,
                        limit,
                        policy,
                        &mut SizingScratch::new()
                    ),
                    expected,
                    "{name} / {policy_name} at limit {limit} (n = {n}): a fresh scratch"
                );
                assert_eq!(
                    size_object_bounded_with_scratch(*term, limit, policy, &mut carried),
                    expected,
                    "{name} / {policy_name} at limit {limit} (n = {n}): a carried scratch"
                );
            }
        }
    }

    // The reverse sweep on one scratch: each term sees a scratch dirtied by the term
    // after it (and, for the cyclic cell, by an early-returning walk).
    for (name, term) in reversed {
        for (policy_name, policy) in policies() {
            assert_eq!(
                size_object_with_scratch(*term, policy, &mut carried),
                size_object_with(*term, policy),
                "{name} / {policy_name}: reverse sweep"
            );
        }
    }

    // Non-terms contribute 0 through the scratch too, and are never dereferenced.
    for nt in non_terms() {
        for (policy_name, policy) in policies() {
            assert_eq!(
                size_object_with_scratch(nt, policy, &mut carried),
                0,
                "{policy_name}: a non-term is 0 words"
            );
            assert_eq!(
                size_object_bounded_with_scratch(nt, 0, policy, &mut carried),
                Ok(0),
                "{policy_name}: a non-term fits a 0-word budget"
            );
        }
    }
}

#[test]
fn the_scratch_keeps_the_cycle_verdict_and_its_priority() {
    let c = corpus();
    let cyclic = c.find("cyclic cell");
    let list = c.find("three-cell list");
    let mut scratch = SizingScratch::new();

    // `Cyclic` outranks `TooLarge` at every budget, exactly as the scratch-free form does.
    for limit in [0usize, 1, 2, 1_000, usize::MAX] {
        assert_eq!(
            size_object_bounded_with_scratch(cyclic, limit, &DefaultSizingPolicy, &mut scratch),
            Err(TermError::TermGraphCyclic),
            "limit {limit}: cyclic outranks too-large"
        );
        assert_eq!(
            size_object_bounded_with_scratch(cyclic, limit, &DefaultSizingPolicy, &mut scratch),
            size_object_bounded_with(cyclic, limit, &DefaultSizingPolicy),
            "limit {limit}: the two forms agree"
        );
    }
    // `size_object_with` keeps the documented total partial count (the cell itself).
    assert_eq!(
        size_object_with_scratch(cyclic, &DefaultSizingPolicy, &mut scratch),
        2
    );
    // ... and the same scratch sizes a valid graph right afterwards.
    assert_eq!(
        size_object_bounded_with_scratch(list, 6, &DefaultSizingPolicy, &mut scratch),
        Ok(6)
    );
}

#[test]
fn with_capacity_changes_only_the_reservation() {
    let c = corpus();
    // A deliberately tiny hint: growth is amortized, so the answers cannot differ.
    let mut tiny = SizingScratch::with_capacity(1);
    let mut generous = SizingScratch::with_capacity(1 << 16);
    for (name, term) in c.terms() {
        assert_eq!(
            size_object_with_scratch(*term, &DefaultSizingPolicy, &mut tiny),
            size_object_with_scratch(*term, &DefaultSizingPolicy, &mut generous),
            "{name}: the capacity hint must not change the count"
        );
        let n = size_object(*term);
        assert_eq!(
            size_object_bounded_with_scratch(*term, n, &CopyLike, &mut tiny),
            size_object_bounded_with_scratch(*term, n, &CopyLike, &mut generous),
            "{name}: the capacity hint must not change the verdict"
        );
    }
}

// ---------------------------------------------------------------------------
// (b) Allocation
// ---------------------------------------------------------------------------

/// The cell count of the "large" workload (deep enough that the path set must grow
/// several times) and of the "small" control.
const LARGE_CELLS: usize = 20_000;
const SMALL_CELLS: usize = 3;

#[test]
fn a_warm_scratch_is_allocation_free_while_the_plain_entry_point_scales() {
    // The workloads: a tuple of two lists of 4-word tuples (a message-shaped payload).
    // Two lists of `LARGE_CELLS` cells (7 words each) plus the wrapping tuples.
    let mut heap = Heap::new(16 * LARGE_CELLS + 64);
    let small_list = Workloads::build_into(&mut heap, SMALL_CELLS);
    let large_list = Workloads::build_into(&mut heap, LARGE_CELLS);
    let large = heap.tuple(&[large_list, large_list]).unwrap();
    let small = heap.tuple(&[small_list]).unwrap();

    // One cell is a 2-word cons over a 5-word tuple (arity 4), so a `cells`-cell list is
    // `7 * cells` words; the wrapping tuples cost their `arity + 1`.
    let large_words = size_object(large);
    let small_words = size_object(small);
    assert_eq!(small_words, 2 + 7 * SMALL_CELLS);
    assert_eq!(large_words, 3 + 2 * 7 * LARGE_CELLS);

    let mut scratch = SizingScratch::new();

    // Warm-up *outside* the measurement windows: one unbounded walk on the large graph
    // (sizes the stack and the path set) and one bounded-too-large walk on it (sizes the
    // acyclicity pass's `done` set).
    black_box(size_object_with_scratch(large, &DefaultSizingPolicy, &mut scratch));
    assert_eq!(
        size_object_bounded_with_scratch(large, 10, &DefaultSizingPolicy, &mut scratch),
        Err(TermError::TermGraphTooLarge)
    );
    assert_eq!(
        size_object_bounded_with_scratch(large, large_words, &DefaultSizingPolicy, &mut scratch),
        Ok(large_words)
    );

    // The measurements.
    const CALLS: usize = 64;
    let (warm_large, _) = allocations_in(|| {
        for _ in 0..CALLS {
            black_box(size_object_with_scratch(
                large,
                &DefaultSizingPolicy,
                &mut scratch,
            ));
        }
    });
    let (warm_small, _) = allocations_in(|| {
        for _ in 0..CALLS {
            black_box(size_object_with_scratch(
                small,
                &DefaultSizingPolicy,
                &mut scratch,
            ));
        }
    });
    let (warm_bounded, _) = allocations_in(|| {
        for _ in 0..CALLS {
            // The `OverLimit` arm: one sizing walk plus the acyclicity re-pass.
            let _ = black_box(size_object_bounded_with_scratch(
                large,
                10,
                &DefaultSizingPolicy,
                &mut scratch,
            ));
        }
    });
    // The control: the same call with a scratch built inside it — i.e. today's entry point.
    let (fresh_large, _) = allocations_in(|| black_box(size_object_with(large, &DefaultSizingPolicy)));
    let (fresh_small, _) = allocations_in(|| black_box(size_object_with(small, &DefaultSizingPolicy)));
    let (fresh_bounded, _) =
        allocations_in(|| black_box(size_object_bounded_with(large, 10, &DefaultSizingPolicy)));

    // Print everything before asserting, so a regression still reports every figure.
    for line in [
        warm_large.line(&format!("(b) warm scratch, large graph x{CALLS}")),
        warm_small.line(&format!("(b) warm scratch, small graph x{CALLS}")),
        warm_bounded.line(&format!("(b) warm scratch, bounded-too-large x{CALLS}")),
        fresh_large.line("(b) scratch-free, large graph x1"),
        fresh_small.line("(b) scratch-free, small graph x1"),
        fresh_bounded.line("(b) scratch-free, bounded-too-large x1"),
    ] {
        println!("{line}");
    }

    // A warm scratch at least as large as the graph: no allocation at all, for either
    // graph size and for the bounded arm.
    assert_eq!(
        warm_large.allocations, 0,
        "a warm scratch must make a sizing call allocation-free"
    );
    assert_eq!(
        warm_small.allocations, 0,
        "a warm scratch must not allocate for a smaller graph either"
    );
    assert_eq!(
        warm_bounded.allocations, 0,
        "the bounded arm's acyclicity re-pass must reuse the same scratch"
    );

    // The scratch-free entry point allocates per call, and *scales with the graph*: the
    // buffers grow with the number of objects on the DFS path.
    assert!(
        fresh_large.allocations > 0 && fresh_small.allocations > 0,
        "the scratch-free entry point builds its buffers per call"
    );
    assert!(
        fresh_large.allocations > fresh_small.allocations,
        "the scratch-free entry point must scale with the graph: {LARGE_CELLS} cells cost {} \
         allocations, {SMALL_CELLS} cells {}",
        fresh_large.allocations,
        fresh_small.allocations
    );
    assert!(
        fresh_bounded.allocations > fresh_large.allocations,
        "the bounded-too-large arm additionally builds the acyclicity pass's sets: {} vs {}",
        fresh_bounded.allocations,
        fresh_large.allocations
    );
    assert!(fresh_large.bytes > 0);
    assert_eq!(warm_large.bytes, 0, "no bytes requested means no allocation");
}

/// Heap-backed workloads for the allocation test (the arena is owned by the caller so the
/// terms stay addressable for the whole test).
struct Workloads;

impl Workloads {
    /// Builds a `cells`-cell list of 4-word tuples into `heap`.
    fn build_into(heap: &mut Heap, cells: usize) -> Eterm {
        let mut list = NIL;
        for i in 0..cells {
            let tuple = heap
                .tuple(&[
                    make_small(i as i64),
                    make_small((i * 3) as i64),
                    make_small((i * 5) as i64),
                    make_small((i * 7) as i64),
                ])
                .unwrap();
            list = heap.cons(tuple, list).unwrap();
        }
        list
    }
}

// ---------------------------------------------------------------------------
// (c) No state between calls
// ---------------------------------------------------------------------------

#[test]
fn one_scratch_serves_unrelated_graphs_without_leaking_state() {
    let c = corpus();
    let cyclic = c.find("cyclic cell");
    let list = c.find("three-cell list");
    let map = c.find("HAMT map (100 keys)");
    let big = c.find("100000-cell list");
    let mut scratch = SizingScratch::new();

    let expected_list = size_object(list);
    let expected_map = size_object(map);
    let expected_big = size_object(big);

    // Cyclic → acyclic → cyclic → big → acyclic, all on one scratch, each answer checked
    // against the scratch-free form. `size_object_bounded_with` returns early on both the
    // `Cyclic` and `OverLimit` verdicts, so the scratch is left dirty every time.
    for _ in 0..2 {
        assert_eq!(
            size_object_bounded_with_scratch(cyclic, usize::MAX, &DefaultSizingPolicy, &mut scratch),
            Err(TermError::TermGraphCyclic)
        );
        assert_eq!(
            size_object_with_scratch(list, &DefaultSizingPolicy, &mut scratch),
            expected_list
        );
        assert_eq!(
            size_object_bounded_with_scratch(map, expected_map, &DefaultSizingPolicy, &mut scratch),
            Ok(expected_map)
        );
        assert_eq!(
            size_object_bounded_with_scratch(map, 1, &DefaultSizingPolicy, &mut scratch),
            Err(TermError::TermGraphTooLarge)
        );
        assert_eq!(
            size_object_with_scratch(big, &DefaultSizingPolicy, &mut scratch),
            expected_big
        );
        assert_eq!(
            size_object_with_scratch(cyclic, &DefaultSizingPolicy, &mut scratch),
            2
        );
        assert_eq!(
            size_object_with_scratch(list, &DefaultSizingPolicy, &mut scratch),
            expected_list
        );
    }

    // `shrink_to_fit` drops the retained buffers; the next call still answers correctly
    // (and simply re-grows them).
    scratch.shrink_to_fit();
    assert_eq!(
        size_object_with_scratch(big, &DefaultSizingPolicy, &mut scratch),
        expected_big
    );
    assert_eq!(
        size_object_bounded_with_scratch(cyclic, 10, &DefaultSizingPolicy, &mut scratch),
        Err(TermError::TermGraphCyclic)
    );
}

#[test]
fn a_scratch_is_debug_printable() {
    // The buffers are an implementation detail; the `Debug` output reports their
    // capacities, which is what a caller holding one wants to know.
    let mut s = SizingScratch::with_capacity(8);
    let shown = format!("{s:?}");
    assert!(shown.contains("SizingScratch"), "{shown}");
    assert!(shown.contains("path_capacity"), "{shown}");
    s.shrink_to_fit();
    assert_eq!(format!("{:?}", SizingScratch::default()), shown_capacity_zero());
}

/// The `Debug` rendering of an empty scratch (no capacity anywhere).
fn shown_capacity_zero() -> String {
    "SizingScratch { stack_capacity: 0, path_capacity: 0, done_capacity: 0 }".to_string()
}
