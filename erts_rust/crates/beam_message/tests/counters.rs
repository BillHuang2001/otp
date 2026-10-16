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

//! **Deterministic counters** for the copy/messaging boundary — exact counts, not timings.
//!
//! [`tests/perf.rs`](perf) measures ns/word for the copy engine and
//! [`tests/send_perf.rs`](send_perf) measures ns/`send` for the same shapes. Timings vary with the
//! host; **counts do not**. This file pins the data-level work the crate does per message, so a
//! future optimisation can be judged against a hard number instead of a noisy one:
//!
//! | quantity | how it is obtained |
//! |---|---|
//! | words copied per message | `size_of_copy(term)` / `size_object(term)`, cross-checked against the destination heap's `used()` advance |
//! | objects copied | a walk of the term graph (cons cells + headed blocks) |
//! | per-object word/header overhead | the same walk, split into header words and payload words |
//! | allocations per message, bytes per message | a counting `#[global_allocator]` |
//! | bytes touched per message | `2 · words · size_of::<Eterm>()` (read once + written once), asserted against the destination `used()` |
//! | copy-vs-enqueue split | the counting allocator (and `Instant`) around `copy_struct` alone vs the whole `beam_message::send` |
//! | the work stack's inline capacity and its spill bracket | a scan of `K`-element tuples, `K` pending fix-ups each (see below) |
//!
//! # The two hard invariants this file now pins
//!
//! 1. **The copy path allocates nothing for a standard shape.** The fix-up worklist keeps its first
//!    entries inline on the copy's own frame (`copy.rs`'s `WORK_INLINE`), so `copy_struct` on all
//!    four shapes below — the 64-key HAMT map and its 21-node trie included — records **0**
//!    allocation events, and so does a whole `send` onto a warm mailbox.
//! 2. **A graph past the inline capacity spills at most once.** The spill probe copies a
//!    `K`-element tuple of compound children (`K` pending fix-ups live at one instant) for every `K`
//!    up to [`MAX_PROBE_CHILDREN`], *derives* the inline capacity as `first allocating K − 1`, and
//!    asserts the bracket: zero allocations up to that capacity, exactly **one** up to twice it.
//!    The capacity is therefore measured, not restated — a change to `WORK_INLINE` moves the
//!    transition and the assertions follow it.
//!//! The write half of the bytes-touched figure is **asserted** (`used()` advances by exactly the
//! flat word count). The read half is a **model**, and its precondition is asserted too: every
//! destination here starts with a fully empty off-heap table, which is exactly
//! `copy.rs::is_fast_path_eligible` — so the copier takes its **single-pass** path (no preflight
//! traversal in front of the writing one) and reads each source word once. A destination whose
//! off-heap table is non-empty would take the sized path and read the graph twice; that is the one
//! case this figure would understate.
//!
//! Run it with output shown:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message \
//!     --test counters -- --nocapture
//! ```
//!
//! # Correctness only, never a timing threshold
//!
//! `std::time::Instant` (no `criterion`, no new dependency) is used for the copy-vs-enqueue split
//! only; **no** assertion here is a timing threshold. The assertions are the deterministic counts
//! plus structural correctness: the enqueued payload is `compare::eq` the original, exactly one
//! message was enqueued, and the destination `used()` advanced by exactly `size_of_copy(term)`.
//!
//! # Why one `#[test]`
//!
//! The counting allocator is process-wide. libtest runs the test functions of a binary on parallel
//! threads, so a second test would interleave its own allocations with a counter window and the
//! exact counts would stop being exact. Everything therefore lives in the single
//! `report_deterministic_counters_per_shape` test — which is also why each counter window is
//! measured `REPEATS` times and asserted **identical**: any concurrent allocation (from a stray
//! thread, a lazy runtime init, …) would break that assertion rather than silently inflate the
//! published table.
//!
//! # The payloads
//!
//! The primary shape is the **exact** payload the `tools/bench` `copy_bound` `send_recv` /
//! `send_recv_plain` workload copies — a literal reproduction of the generated Erlang
//!
//! ```erlang
//! #{a => [1,2,3,4,5,6,7,8,9,10], b => {ok, <<"hello world">>, 3.5},
//!   c => <<1,2,3,4,5,6,7,8,9,10,11,12>>, d => "a char list", e => -42}
//! ```
//!
//! (`erts_rust/tools/bench/src/workloads/copy_bound.rs`, `module_source()`/`PAYLOAD_*`; the same
//! shape is built by `send_perf`'s `send_recv_payload`). Three smaller shapes — a 3-element tuple,
//! a 10-element int list and a 64-key HAMT map — give the per-object overhead contrast. None of
//! them holds an off-heap reference, so every shape crosses the boundary as a pure on-heap flat
//! copy through the copier's single-pass path.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use beam_alloc::Heap;
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, send, size_of_copy, CopyContext, Queue};
use beam_term::prelude::*;
use beam_term::{compare, size_object};

// ---------------------------------------------------------------------------
// 1. The counting global allocator
// ---------------------------------------------------------------------------

/// Every `alloc` event, process-wide.
static ALLOC_EVENTS: AtomicUsize = AtomicUsize::new(0);
/// Every byte an `alloc` event asked the system allocator for.
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Every `dealloc` event, process-wide.
static DEALLOC_EVENTS: AtomicUsize = AtomicUsize::new(0);

/// A `#[global_allocator]` that counts allocation events and requested bytes over
/// [`System`](std::alloc::System).
///
/// Only `alloc`/`dealloc` are overridden, deliberately: `GlobalAlloc`'s provided `realloc` default
/// is defined as `alloc` + copy + `dealloc` and its `alloc_zeroed` default as `alloc` + zeroing, so
/// a growth event is counted as **exactly one** allocation whose requested size is the new size.
/// That is the honest reading of "allocations and bytes the boundary asked the allocator for",
/// without a second bookkeeping path that could disagree with it.
struct CountingAllocator;

// SAFETY: both methods forward the caller's own `layout` to `System` unchanged, so every pointer
// handed out is one `System` produced and every pointer handed back came from `System`; the
// atomics only observe. `System` is a valid `GlobalAlloc` implementation.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_EVENTS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        // SAFETY: the caller's `layout` is forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        DEALLOC_EVENTS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr` was handed out by this allocator's `alloc` for exactly `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// The allocation counters, read at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Counters {
    allocs: usize,
    bytes: usize,
    deallocs: usize,
}

impl Counters {
    fn now() -> Counters {
        Counters {
            allocs: ALLOC_EVENTS.load(Ordering::Relaxed),
            bytes: ALLOC_BYTES.load(Ordering::Relaxed),
            deallocs: DEALLOC_EVENTS.load(Ordering::Relaxed),
        }
    }

    /// The events `self` (read *after*) accumulated since `before`.
    fn since(self, before: Counters) -> Counters {
        Counters {
            allocs: self.allocs - before.allocs,
            bytes: self.bytes - before.bytes,
            deallocs: self.deallocs - before.deallocs,
        }
    }

    /// Per-event cost of the phase, in bytes.
    fn bytes_per_alloc(self) -> f64 {
        if self.allocs == 0 {
            0.0
        } else {
            self.bytes as f64 / self.allocs as f64
        }
    }
}

/// Runs `f` and returns its value together with the allocation events it caused.
///
/// `f` is a plain `FnOnce` closure — no boxing, no `dyn`, so the window is exactly the call.
fn counted<T>(f: impl FnOnce() -> T) -> (T, Counters) {
    let before = Counters::now();
    let value = f();
    (value, Counters::now().since(before))
}

// ---------------------------------------------------------------------------
// 2. The object census (a walk of the copied graph)
// ---------------------------------------------------------------------------

/// What a **flat** copy of a term graph materialises: how many objects, and how their words split
/// into the one-word object header and the payload.
///
/// A cons cell is header-less in the frozen layout (its two words are `[CAR, CDR]`), so it
/// contributes `2` payload words and **no** header word; every other object contributes its
/// `beam_term::layout::object_words` count, of which the first word is the header.
///
/// The header convention is `object_words`'s own — **one** header word per headed block — not a
/// per-shape enumeration of "words before the data". That matters for exactly one shape: an
/// `ErlHeapBits` (`[thing_word, n_bits, data…]`) carries a second bookkeeping word, which this
/// census counts as payload. `header_words` is therefore a lower bound on the true header bytes
/// for a binary-heavy graph, and the `words` total (which is what the copier actually writes) is
/// unaffected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Census {
    /// Cons cells + headed blocks.
    objects: usize,
    /// Of `objects`: the header-less two-word cons cells.
    cons_cells: usize,
    /// Of `objects`: the blocks with a one-word header.
    headed: usize,
    /// One per headed block.
    header_words: usize,
    /// The remaining words of every object.
    payload_words: usize,
    /// `header_words + payload_words` — must equal `size_object(term)`.
    words: usize,
    /// The largest single object's word count (the worst-case per-object cost).
    max_object_words: usize,
}

impl Census {
    fn mean_object_words(&self) -> f64 {
        if self.objects == 0 {
            0.0
        } else {
            self.words as f64 / self.objects as f64
        }
    }

    /// `header_words / words` as a percentage: the flat copy's per-word header overhead.
    fn header_overhead_percent(&self) -> f64 {
        if self.words == 0 {
            0.0
        } else {
            100.0 * self.header_words as f64 / self.words as f64
        }
    }
}

/// Counts the objects of the **flat** graph rooted at `root`, exactly as a flat `copy_struct`
/// materialises them: a subterm occurring twice is counted twice, immediates and the shared
/// zero-arity tuple literal contribute nothing, and every object's words come from the LCA's
/// [`object_words`] — the very function the copier reserves with, so the counts cannot drift from
/// the copy.
///
/// The walk descends [`for_each_sizing_child_slot`], the same child enumeration `beam_term`'s
/// `size`/`layout` halves and the copier's sizing walk use, and mirrors `layout.rs`'s own
/// `sum_object_words` test walk (explicit DFS, an on-path set so an accidental cycle terminates
/// rather than hangs). A cycle can only come from a hand-forged graph; the real shapes here are
/// acyclic trees, where the on-path set never triggers.
///
/// A refc binary counts as **one** 8-word block (`[BinRef][ErlSubBits]`, the count `object_words`
/// reports), not as a `BinRef` plus an `ErlSubBits`: that is what the copier writes and it is the
/// only defensible reading at this level. No measured shape holds one.
fn census(root: Eterm) -> Census {
    enum Step {
        Enter(Eterm),
        Exit(usize),
    }

    let mut c = Census::default();
    let mut on_path: HashSet<usize> = HashSet::new();
    let mut steps: Vec<Step> = vec![Step::Enter(root)];

    while let Some(step) = steps.pop() {
        let term = match step {
            Step::Exit(addr) => {
                on_path.remove(&addr);
                continue;
            }
            Step::Enter(term) => term,
        };
        if !is_term_word(term) || is_immediate(term) {
            continue;
        }
        if is_list(term) {
            let cell = list_val(term);
            if !on_path.insert(cell as usize) {
                continue;
            }
            c.objects += 1;
            c.cons_cells += 1;
            // A cons cell is `[CAR, CDR]` with no header word: both words are payload.
            c.payload_words += 2;
            c.words += 2;
            c.max_object_words = c.max_object_words.max(2);
            steps.push(Step::Exit(cell as usize));
            // SAFETY: `term` is a live, non-`NIL` cons cell, so both its words are readable.
            let (head, tail) = unsafe { (*cell, *cell.add(1)) };
            steps.push(Step::Enter(tail));
            steps.push(Step::Enter(head));
            continue;
        }

        let ptr = boxed_val(term);
        // SAFETY: `term` is a live boxed term, so `ptr` addresses its live header word.
        let header = unsafe { *ptr };
        if is_arity_value(header) && header_arity(header) == 0 {
            // The shared zero-arity tuple literal: never a fresh heap object.
            continue;
        }
        // SAFETY: `header`/`ptr` head a live object of `header`'s shape.
        let words = unsafe { object_words(header, ptr) };
        if !on_path.insert(ptr as usize) {
            continue;
        }
        c.objects += 1;
        c.headed += 1;
        c.header_words += 1;
        c.payload_words += words - 1;
        c.words += words;
        c.max_object_words = c.max_object_words.max(words);
        steps.push(Step::Exit(ptr as usize));
        // SAFETY: `header`/`ptr` head a live object whose payload words are live, so its sizing
        // child slots are readable; the closure only reads them and pushes their terms.
        unsafe {
            for_each_sizing_child_slot(header, ptr, |offset| {
                steps.push(Step::Enter(*ptr.add(offset)));
            });
        }
    }

    c
}

// ---------------------------------------------------------------------------
// 3. The shapes
// ---------------------------------------------------------------------------

/// The source process: the heap the shapes live on, its atom table, and the (empty) off-heap table
/// the copy's source side reads.
struct Src {
    heap: Heap,
    atoms: AtomTable,
    off: OffHeapTable,
}

impl Src {
    fn new() -> Src {
        Src {
            heap: Heap::new(1 << 16).expect("source heap"),
            atoms: AtomTable::new(),
            off: OffHeapTable::new(),
        }
    }
}

/// The free head-room every destination keeps beyond the payload's own words — the same value
/// `tests/send_perf.rs` uses, so a copy here is measured into a destination sized like the real
/// one.
const HEADROOM: usize = 64;

/// How many times each counter window is measured; every repeat must produce identical counts.
const REPEATS: usize = 8;

/// The largest work-stack probe arity the spill scan tries (`1..=MAX_PROBE_CHILDREN`).
///
/// The scan *derives* the work stack's inline capacity from behaviour — the smallest arity whose
/// copy allocates is the first arity past it — and then asserts the "one spill" bracket up to
/// **twice** that capacity, so 96 covers the expected transition (33) and its bracket (64) with
/// room to spare while keeping the scan cheap. A copy path whose inline capacity ever grew past
/// this would make the scan find no transition at all and fail loudly on that expect, rather than
/// silently asserting nothing.
const MAX_PROBE_CHILDREN: usize = 96;
/// Builds the list `[e0, e1, ...]` on `heap`, tail-first.
fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for &elem in elems.iter().rev() {
        acc = heap.cons(elem, acc).expect("the cons fits");
    }
    acc
}

/// Writes a flat map (or HAMT map, past `MAP_SMALL_MAP_LIMIT`) on `heap`.
fn write_map_term(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    // SAFETY: `map_words_capacity` is a sound upper bound on the words `write_map` writes.
    unsafe {
        let hp = heap
            .alloc_words(map_words_capacity(pairs.len()))
            .expect("the map fits");
        write_map(hp, pairs, atoms).expect("the map builds")
    }
}

/// Writes a byte-aligned on-heap bitstring of `bytes` (the shape ERTS keeps on the heap: `<<…>>`
/// up to `ERL_ONHEAP_BINARY_LIMIT` bytes).
fn heap_bits(heap: &mut Heap, bytes: &[u8]) -> Eterm {
    let bits = bytes.len() * 8;
    // SAFETY: `heap_bits_size_words(bits)` words are reserved and `bytes` covers `bits` bits.
    unsafe {
        let hp = heap
            .alloc_words(heap_bits_size_words(bits))
            .expect("the bitstring fits");
        write_heap_bits(hp, bytes, bits).expect("the payload fits")
    }
}

/// Writes a boxed float (`[HEADER_FLONUM, bits]`, 2 words).
fn boxed_float(heap: &mut Heap, value: f64) -> Eterm {
    // SAFETY: `FLOAT_SIZE_WORDS` (2) words are reserved for the block.
    unsafe {
        let hp = heap.alloc_words(FLOAT_SIZE_WORDS).expect("the float fits");
        write_float(hp, value)
    }
}

/// The smallest realistic message: a 3-element tuple (header + 3 immediate elements).
fn small_tuple(s: &mut Src) -> Eterm {
    s.heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the tuple fits")
}

/// A 10-element integer list: 10 cons cells of immediates.
fn int_list(s: &mut Src) -> Eterm {
    build_list(&mut s.heap, &(1..=10).map(make_small).collect::<Vec<_>>())
}

/// A 64-key map: past `MAP_SMALL_MAP_LIMIT` (32), so a HAMT trie of many small nodes.
fn hamt_map(s: &mut Src) -> Eterm {
    let pairs: Vec<(Eterm, Eterm)> = (0..64)
        .map(|i| (make_small(i * 3), make_small(i * 1000 + 7)))
        .collect();
    write_map_term(&mut s.heap, &pairs, &s.atoms)
}

/// The **exact** `copy_bound` `send_recv` benchmark payload
/// (`tools/bench/src/workloads/copy_bound.rs`): a 5-key flat map holding a 10-element int list, an
/// `{ok, <<"hello world">>, 3.5}` tuple, a 12-byte binary, an 11-character char list and `-42`.
fn send_recv_payload(s: &mut Src) -> Eterm {
    let ints = build_list(&mut s.heap, &(1..=10).map(make_small).collect::<Vec<_>>());
    let hello = heap_bits(&mut s.heap, b"hello world");
    let float = boxed_float(&mut s.heap, 3.5);
    let ok = make_atom(s.atoms.intern("ok"));
    let b = s.heap.tuple(&[ok, hello, float]).expect("the tuple fits");
    let c = heap_bits(&mut s.heap, &(1..=12u8).collect::<Vec<u8>>());
    let d = build_list(
        &mut s.heap,
        &"a char list"
            .bytes()
            .map(|ch| make_small(ch as i64))
            .collect::<Vec<_>>(),
    );

    let ka = make_atom(s.atoms.intern("a"));
    let kb = make_atom(s.atoms.intern("b"));
    let kc = make_atom(s.atoms.intern("c"));
    let kd = make_atom(s.atoms.intern("d"));
    let ke = make_atom(s.atoms.intern("e"));
    let pairs = [
        (ka, ints),
        (kb, b),
        (kc, c),
        (kd, d),
        (ke, make_small(-42)),
    ];
    write_map_term(&mut s.heap, &pairs, &s.atoms)
}

/// The **work-stack spill probe**: a tuple of `children` distinct *compound* elements, each a
/// 2-element tuple of small ints.
///
/// A tuple is materialised in one step and then hands its children to the copier's worklist
/// *before any of them is resolved*, so this shape holds exactly `children` pending fix-ups live on
/// the work stack at one instant. That is the lever the probe below uses to turn the stack's inline
/// capacity into a measurable allocation count — and because the shape is otherwise trivial (each
/// popped child is a pair of immediates, which pushes nothing), the copy's total allocation count
/// is the work stack's and nothing else.
fn work_stack_probe(s: &mut Src, children: usize) -> Eterm {
    let elems: Vec<Eterm> = (0..children)
        .map(|i| {
            let value = i as i64;
            s.heap
                .tuple(&[make_small(value), make_small(value + 1)])
                .expect("the child tuple fits")
        })
        .collect();
    s.heap.tuple(&elems).expect("the probe tuple fits")
}

/// A measured payload: how to build it and how many copies to time.
struct Shape {
    name: &'static str,
    build: fn(&mut Src) -> Eterm,
    iterations: u32,
}
/// The measured shapes. The last is the `copy_bound` payload; the first three are the contrast
/// cases (a single small object, a homogeneous cons chain, a many-node trie).
const SHAPES: &[Shape] = &[
    Shape {
        name: "small tuple",
        build: small_tuple,
        iterations: 20_000,
    },
    Shape {
        name: "10-int list",
        build: int_list,
        iterations: 20_000,
    },
    Shape {
        name: "HAMT map (64 keys)",
        build: hamt_map,
        iterations: 20_000,
    },
    Shape {
        name: "send_recv payload",
        build: send_recv_payload,
        iterations: 20_000,
    },
];

// ---------------------------------------------------------------------------
// 4. Staging and the measured phases
// ---------------------------------------------------------------------------

/// A clean destination heap of `words + HEADROOM` words with a fresh, **empty** off-heap table —
/// the destination shape `send_perf` uses, and the one that admits the copier's single-pass path
/// (the gate is the off-heap table's emptiness alone).
fn stage(words: usize) -> (Heap, OffHeapTable) {
    (
        Heap::new(words + HEADROOM).expect("destination heap"),
        OffHeapTable::new(),
    )
}

/// The destination table must be empty in every category for the single-pass path — which is what
/// makes the "each source word is read once" model of the bytes-touched figure true.
fn assert_destination_is_clean(dst_off: &OffHeapTable, label: &str) {
    assert!(
        dst_off.is_empty() && dst_off.binaries().is_empty() && dst_off.nodes().is_empty(),
        "{label}: the destination off-heap table must start empty"
    );
}

/// The counted **copy phase**: `copy_struct` alone into a destination sized like the real one.
fn count_copy(term: Eterm, src: &Src, words: usize) -> Counters {
    let (mut dst, mut dst_off) = stage(words);
    assert_destination_is_clean(&dst_off, "copy phase");
    let before = dst.used();

    let (result, counters) = counted(|| {
        let mut ctx = CopyContext::new(&src.off, &mut dst_off);
        copy_struct(term, &mut dst, &mut ctx)
    });
    let copy = result.expect("the copy succeeds");

    // The deterministic word accounting, asserted against what the copier actually wrote.
    assert_eq!(
        dst.used() - before,
        words,
        "the copy advances `used` by exactly size_of_copy(term)"
    );
    assert!(
        compare::eq(term, copy, &src.atoms),
        "the counted copy must be =:= the original"
    );
    counters
}

/// The counted **full send phase**: `beam_message::send` (copy + off-heap drain + `Message` build
/// + `Queue::enqueue`) into a **fresh** queue, so the queue's first buffer growth is inside the
/// window — the cold-start per-message cost.
fn count_send_cold(term: Eterm, src: &Src, words: usize) -> Counters {
    let (mut dst, mut dst_off) = stage(words);
    let mut queue = Queue::new();
    assert_destination_is_clean(&dst_off, "cold send phase");
    let before = dst.used();

    let (result, counters) = counted(|| {
        let mut ctx = CopyContext::new(&src.off, &mut dst_off);
        send(make_small(1), term, &mut dst, &mut queue, &mut ctx)
    });
    result.expect("the send succeeds");

    assert_eq!(
        dst.used() - before,
        words,
        "the send advances `used` by exactly size_of_copy(term)"
    );
    assert_eq!(queue.len(), 1, "exactly one message is enqueued");
    let payload = queue.peek().expect("one queued message").payload();
    assert!(
        compare::eq(term, payload, &src.atoms),
        "the enqueued payload must be =:= the original"
    );
    counters
}

/// The counted **steady-state send phase**: the same send, but into a queue that already holds one
/// message (staged outside the window), so the queue's own buffer growth is *not* charged —
/// the marginal allocation cost a host pays per message once its mailbox exists.
fn count_send_warm(term: Eterm, src: &Src, words: usize) -> Counters {
    // Warm the queue: one immediate payload, so nothing but the queue's buffer is set up.
    let mut queue = Queue::new();
    {
        let mut warm_heap = Heap::new(8).expect("warm-up heap");
        let mut warm_off = OffHeapTable::new();
        let mut ctx = CopyContext::new(&src.off, &mut warm_off);
        send(make_small(1), make_small(0), &mut warm_heap, &mut queue, &mut ctx)
            .expect("the warm-up send succeeds");
    }
    assert_eq!(queue.len(), 1, "the queue is pre-warmed with one message");

    let (mut dst, mut dst_off) = stage(words);
    assert_destination_is_clean(&dst_off, "warm send phase");
    let before = dst.used();

    let (result, counters) = counted(|| {
        let mut ctx = CopyContext::new(&src.off, &mut dst_off);
        send(make_small(1), term, &mut dst, &mut queue, &mut ctx)
    });
    result.expect("the send succeeds");

    assert_eq!(
        dst.used() - before,
        words,
        "the send advances `used` by exactly size_of_copy(term)"
    );
    assert_eq!(queue.len(), 2, "the warmed message is still queued");
    let payload = queue.iter().last().expect("two queued messages").payload();
    assert!(
        compare::eq(term, payload, &src.atoms),
        "the enqueued payload must be =:= the original"
    );
    counters
}

/// `REPEATS` counter windows of the same phase must agree **exactly** — the determinism claim.
/// Returns the agreed counters and panics naming the phase on any disagreement.
fn repeat<F: FnMut() -> Counters>(label: &str, mut phase: F) -> Counters {
    let first = phase();
    for i in 1..REPEATS {
        let again = phase();
        assert_eq!(
            first, again,
            "{label}: counter window {i} differs from window 0 — the counts are not deterministic"
        );
    }
    first
}

/// Times `iterations` **copy-only** copies of `term`, asserting correctness, and returns
/// `(mean_ns, best_ns)`.
fn time_copy(term: Eterm, src: &Src, words: usize, iterations: u32) -> (f64, u128) {
    let mut total = Duration::ZERO;
    let mut best = Duration::MAX;
    for i in 0..iterations {
        // Staging is outside the timed region: only the copy itself is measured.
        let (mut dst, mut dst_off) = stage(words);
        let start = Instant::now();
        let copy = {
            let mut ctx = CopyContext::new(&src.off, &mut dst_off);
            copy_struct(term, &mut dst, &mut ctx).expect("the copy succeeds")
        };
        let elapsed = start.elapsed();
        total += elapsed;
        best = best.min(elapsed);

        if i == 0 {
            assert!(
                compare::eq(term, copy, &src.atoms),
                "the timed copy must be =:= the original"
            );
        }
        assert_eq!(dst.used(), words, "the timed copy writes the sized count");
        // Keep the copy observable so it cannot be optimised away.
        std::hint::black_box(copy);
    }
    (mean_ns(total, iterations), best.as_nanos())
}

/// Times `iterations` full [`send`]s of `term` into a fresh queue, asserting correctness, and
/// returns `(mean_ns, best_ns)`.
fn time_send(term: Eterm, src: &Src, words: usize, iterations: u32) -> (f64, u128) {
    let mut total = Duration::ZERO;
    let mut best = Duration::MAX;
    for i in 0..iterations {
        let (mut dst, mut dst_off) = stage(words);
        let mut queue = Queue::new();
        let start = Instant::now();
        {
            let mut ctx = CopyContext::new(&src.off, &mut dst_off);
            send(make_small(1), term, &mut dst, &mut queue, &mut ctx).expect("the send succeeds");
        }
        let elapsed = start.elapsed();
        total += elapsed;
        best = best.min(elapsed);

        if i == 0 {
            let payload = queue.peek().expect("one queued message").payload();
            assert!(
                compare::eq(term, payload, &src.atoms),
                "the timed send must enqueue a =:= payload"
            );
        }
        assert_eq!(queue.len(), 1, "exactly one message per timed send");
        assert_eq!(dst.used(), words, "the timed send writes the sized count");
        std::hint::black_box(queue.len());
    }
    (mean_ns(total, iterations), best.as_nanos())
}

fn mean_ns(total: Duration, iterations: u32) -> f64 {
    total.as_nanos() as f64 / f64::from(iterations)
}

/// `/proc/loadavg`, or a marker when the host does not expose it (the timings are meaningless
/// without it, so the report names it).
fn loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string())
}

// ---------------------------------------------------------------------------
// 5. The report
// ---------------------------------------------------------------------------

/// The whole harness: per shape, the graph census, the allocation counters and the copy-vs-enqueue
/// split. See the module docs for the method and for why this is one test.
#[test]
fn report_deterministic_counters_per_shape() {
    println!("counters: /proc/loadavg at start = {}", loadavg());
    println!(
        "counters: {} shapes, {REPEATS} counter windows each (all must agree exactly), \
         Eterm = {} bytes, HEADROOM = {HEADROOM} words",
        SHAPES.len(),
        core::mem::size_of::<Eterm>()
    );

    // (1) The graph census and the per-object overhead.
    println!();
    println!("== per-message work (a flat copy into a clean `words + {HEADROOM}`-word heap) ==");
    println!(
        "{:<22} {:>6} {:>7} {:>6} {:>7} {:>7} {:>8} {:>7} {:>7} {:>7} {:>7}",
        "shape",
        "words",
        "objects",
        "cons",
        "headed",
        "hdr_w",
        "payload_w",
        "w/obj",
        "max_w/o",
        "hdr%",
        "touched_B"
    );

    struct Row {
        name: &'static str,
        words: usize,
        census: Census,
        copy: Counters,
        send_cold: Counters,
        send_warm: Counters,
        copy_ns: f64,
        copy_best: u128,
        send_ns: f64,
        send_best: u128,
    }

    let mut rows: Vec<Row> = Vec::with_capacity(SHAPES.len());
    for shape in SHAPES {
        let mut src = Src::new();
        let term = (shape.build)(&mut src);

        let words = size_of_copy(term);
        assert!(words > 0, "{}: every shape is a compound term", shape.name);
        assert_eq!(
            words,
            size_object(term),
            "{}: `size_of_copy` is the `size_object` forwarder",
            shape.name
        );

        // The census must reproduce the copier's own word count exactly, or the object/overhead
        // columns would be describing a different copy than the one measured.
        let census = census(term);
        assert_eq!(
            census.words, words,
            "{}: the object census must sum to size_of_copy(term)",
            shape.name
        );
        assert_eq!(
            census.objects,
            census.cons_cells + census.headed,
            "{}: every object is a cons cell or a headed block",
            shape.name
        );
        assert!(
            census.max_object_words <= words,
            "{}: no object is larger than the whole graph",
            shape.name
        );

        // The counted phases: copy alone, full send into a cold queue, full send into a warm one.
        let copy = repeat(&format!("{} [copy_struct]", shape.name), || {
            count_copy(term, &src, words)
        });
        let send_cold = repeat(&format!("{} [send, cold queue]", shape.name), || {
            count_send_cold(term, &src, words)
        });
        let send_warm = repeat(&format!("{} [send, warm queue]", shape.name), || {
            count_send_warm(term, &src, words)
        });

        // The send path is the copy plus the enqueue, so the enqueue can never *remove* an
        // allocation the copy made — the split is only meaningful if this holds.
        assert!(
            send_cold.allocs >= copy.allocs,
            "{}: the full send cannot allocate less than the bare copy",
            shape.name
        );
        assert!(
            send_warm.allocs >= copy.allocs,
            "{}: the warmed send cannot allocate less than the bare copy",
            shape.name
        );

        // The allocation-free invariant of the copy path: the fix-up worklist keeps its first
        // entries inline on the copy's own frame (`copy.rs`'s `WORK_INLINE`), so **every** one of
        // these shapes — the 64-key HAMT map with its 21-node trie included — copies without
        // touching the allocator. A graph past the inline capacity spills once; see the spill probe
        // below, which derives the capacity from behaviour instead of restating it here.
        assert_eq!(
            copy.allocs, 0,
            "{}: the copy path must allocate nothing for a standard shape",
            shape.name
        );
        assert_eq!(
            copy.deallocs, 0,
            "{}: an allocation-free copy frees nothing either",
            shape.name
        );

        // ... and the whole per-message `send` is allocation-free once its mailbox exists: the
        // copy, the off-heap drain, the `Message` build and `Queue::enqueue` together ask the
        // allocator for nothing. (The cold-queue row's single allocation is the mailbox's first
        // buffer, which a host pays once per mailbox, not once per message.)
        assert_eq!(
            send_warm.allocs, 0,
            "{}: a send onto a warm mailbox must allocate nothing",
            shape.name
        );
        // Bytes touched = every source word read once + every destination word written once. The
        // `used()` advance asserted in the phases above is the written half.
        let touched_bytes = 2 * words * core::mem::size_of::<Eterm>();

        println!(
            "{:<22} {:>6} {:>7} {:>6} {:>7} {:>7} {:>8} {:>7.2} {:>7} {:>6.1}% {:>7}",
            shape.name,
            words,
            census.objects,
            census.cons_cells,
            census.headed,
            census.header_words,
            census.payload_words,
            census.mean_object_words(),
            census.max_object_words,
            census.header_overhead_percent(),
            touched_bytes
        );

        let (copy_ns, copy_best) = time_copy(term, &src, words, shape.iterations);
        let (send_ns, send_best) = time_send(term, &src, words, shape.iterations);

        rows.push(Row {
            name: shape.name,
            words,
            census,
            copy,
            send_cold,
            send_warm,
            copy_ns,
            copy_best,
            send_ns,
            send_best,
        });
    }

    // (2) The allocation counters, per phase.
    println!();
    println!("== allocations per message (counting #[global_allocator] over System) ==");
    println!(
        "{:<22} {:<20} {:>8} {:>10} {:>11} {:>10}",
        "shape", "phase", "allocs", "bytes", "B/alloc", "deallocs"
    );
    for row in &rows {
        for (phase, counters) in [
            ("copy_struct", row.copy),
            ("send (cold queue)", row.send_cold),
            ("send (warm queue)", row.send_warm),
        ] {
            println!(
                "{:<22} {:<20} {:>8} {:>10} {:>11.1} {:>10}",
                row.name,
                phase,
                counters.allocs,
                counters.bytes,
                counters.bytes_per_alloc(),
                counters.deallocs
            );
        }
        // The enqueue phase is the send path minus the bare copy it performs first.
        let cold = Counters {
            allocs: row.send_cold.allocs - row.copy.allocs,
            bytes: row.send_cold.bytes - row.copy.bytes,
            deallocs: row.send_cold.deallocs - row.copy.deallocs,
        };
        let warm = Counters {
            allocs: row.send_warm.allocs - row.copy.allocs,
            bytes: row.send_warm.bytes - row.copy.bytes,
            deallocs: row.send_warm.deallocs - row.copy.deallocs,
        };
        println!(
            "{:<22} {:<20} {:>8} {:>10} {:>11.1} {:>10}",
            row.name,
            "  sent = copy + enq",
            cold.allocs,
            cold.bytes,
            cold.bytes_per_alloc(),
            cold.deallocs
        );
        println!(
            "{:<22} {:<20} {:>8} {:>10} {:>11.1} {:>10}",
            row.name,
            "  enq (warm queue)",
            warm.allocs,
            warm.bytes,
            warm.bytes_per_alloc(),
            warm.deallocs
        );
    }

    // (2b) The work-stack spill probe. `copy.rs`'s work stack keeps its first `WORK_INLINE` fix-ups
    // inline and spills to a `Vec` (reserved at *twice* that capacity) beyond them; this scan
    // derives that capacity from the allocation counts alone — the smallest probe arity whose copy
    // allocates is the first arity past it — and then pins the bracket it implies: nothing below,
    // exactly one allocation up to twice the capacity, more only past that.
    println!();
    println!(
        "== work-stack spill probe (a K-element tuple of compound children = K pending fix-ups) =="
    );
    {
        let mut probe: Vec<(usize, usize, Counters)> = Vec::new();
        for children in 1..=MAX_PROBE_CHILDREN {
            let mut src = Src::new();
            // Building the probe is outside the counted window: only the copy is measured.
            let term = work_stack_probe(&mut src, children);
            let words = size_of_copy(term);
            probe.push((children, words, count_copy(term, &src, words)));
        }

        let inline_capacity = probe
            .iter()
            .find(|(_, _, counters)| counters.allocs > 0)
            .map(|(children, _, _)| children - 1)
            .expect(
                "a probe arity past the work stack's inline capacity must exist within \
                 MAX_PROBE_CHILDREN — the capacity has grown past the scan",
            );

        for (children, _, counters) in &probe {
            if *children <= inline_capacity {
                assert_eq!(
                    counters.allocs, 0,
                    "a {children}-fix-up copy fits the {inline_capacity}-entry inline stack and \
                     must not allocate"
                );
            } else if *children <= 2 * inline_capacity {
                assert_eq!(
                    counters.allocs, 1,
                    "a {children}-fix-up copy is within twice the {inline_capacity}-entry inline \
                     stack, so it must pay exactly ONE spill allocation"
                );
                assert_eq!(
                    counters.deallocs, 1,
                    "a {children}-fix-up copy frees exactly the one spill buffer"
                );
            }
        }
        // The counts can only grow with the graph, and they never grow faster than the graph's
        // fix-up count (the spill doubles; it does not allocate per entry).
        assert!(
            probe.windows(2).all(|w| w[0].2.allocs <= w[1].2.allocs),
            "the copy's allocation count must not fall as the worklist grows"
        );
        assert!(
            probe
                .iter()
                .all(|(children, _, counters)| counters.allocs <= *children),
            "the copy's allocation count cannot exceed the number of pending fix-ups"
        );

        println!(
            "{:<10} {:>8} {:>9} {:>11} {:>10}",
            "children", "words", "allocs", "bytes", "deallocs"
        );
        // Only the transitions: every other arity repeats its predecessor's row.
        let mut previous: Option<usize> = None;
        for (children, words, counters) in &probe {
            let last = *children == MAX_PROBE_CHILDREN;
            if previous != Some(counters.allocs) || last {
                println!(
                    "{:<10} {:>8} {:>9} {:>11} {:>10}",
                    children, words, counters.allocs, counters.bytes, counters.deallocs
                );
                previous = Some(counters.allocs);
            }
        }
        let first_spill = probe
            .iter()
            .find(|(children, _, _)| *children == inline_capacity + 1)
            .expect("the first allocating arity was found");
        println!(
            "  derived inline capacity = {inline_capacity} fix-ups (the first arity that allocates \
             is {}, its spill is {} B = {} entries x {} B)",
            inline_capacity + 1,
            first_spill.2.bytes,
            2 * inline_capacity,
            core::mem::size_of::<(*mut Eterm, Eterm)>()
        );
    }

    // (3) The copy-vs-enqueue split: deterministic counts first, `Instant` second.
    println!();
    println!("== the copy-vs-enqueue split (counts exact; ns are the split only, no threshold) ==");
    println!(
        "{:<22} {:>6} {:>14} {:>12} {:>14} {:>12} {:>16} {:>9} {:>9} {:>10} {:>10}",
        "shape",
        "words",
        "copy_mean_ns",
        "copy_best_ns",
        "send_mean_ns",
        "send_best_ns",
        "enqueue_mean_ns",
        "copy_all",
        "send_all",
        "enq_all",
        "enqueue_B"
    );
    for row in &rows {
        // The enqueue phase is the send path minus the bare copy it performs first: the difference
        // of two exact counter windows, so it is exact too.
        let enqueue_allocs = row.send_warm.allocs - row.copy.allocs;
        let enqueue_bytes = row.send_warm.bytes - row.copy.bytes;
        println!(
            "{:<22} {:>6} {:>14.1} {:>12} {:>14.1} {:>12} {:>16.1} {:>9} {:>9} {:>10} {:>10}",
            row.name,
            row.words,
            row.copy_ns,
            row.copy_best,
            row.send_ns,
            row.send_best,
            row.send_ns - row.copy_ns,
            row.copy.allocs,
            row.send_cold.allocs,
            enqueue_allocs,
            enqueue_bytes
        );
    }

    // (4) The headline: what the `send_recv` benchmark payload actually costs. The bytes-touched
    // figure is `2 x words x size_of::<Eterm>()` — each source word read once (the single-pass
    // gate asserted in every phase above) and each destination word written once (the `used()`
    // advance asserted in every phase above); the `touched_B` column of table (1) is the same
    // figure per shape.
    let primary = rows
        .iter()
        .find(|row| row.name == "send_recv payload")
        .expect("the primary shape is measured");
    {
        let copy = primary.copy;
        let send = primary.send_cold;
        let warm = primary.send_warm;
        let unit = core::mem::size_of::<Eterm>();
        println!();
        println!("== `send_recv payload` (the tools/bench copy_bound payload) ==");
        println!(
            "  graph: {} words in {} objects ({} cons cells + {} headed), \
             header overhead {:.1}%, mean {:.2} words/object, max {}",
            primary.words,
            primary.census.objects,
            primary.census.cons_cells,
            primary.census.headed,
            primary.census.header_overhead_percent(),
            primary.census.mean_object_words(),
            primary.census.max_object_words
        );
        println!(
            "  bytes touched: {} words ({} read + {} written) = {} bytes",
            2 * primary.words,
            primary.words,
            primary.words,
            2 * primary.words * unit
        );
        println!(
            "  allocations: copy_struct {} ({} B), whole send {} ({} B), \
             warmed-queue send {} ({} B)",
            copy.allocs, copy.bytes, send.allocs, send.bytes, warm.allocs, warm.bytes
        );
        println!(
            "  enqueue phase (warm queue) = send - copy = {} allocations, {} bytes",
            warm.allocs - copy.allocs,
            warm.bytes - copy.bytes
        );
        println!(
            "  fixed destination capacity per message: {} words ({} + {HEADROOM} head-room)",
            primary.words + HEADROOM,
            primary.words
        );
        println!(
            "  ns: copy_struct {:.1} mean / {} best, whole send {:.1} mean / {} best",
            primary.copy_ns, primary.copy_best, primary.send_ns, primary.send_best
        );
    }

    println!();
    println!("counters: /proc/loadavg at end = {}", loadavg());
}
