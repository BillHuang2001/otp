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

//! **Structural traversal counter** for the copy path: what a `send` *writes into the destination*
//! versus what the source graph *contains*.
//!
//! # Why this instrument exists
//!
//! A measurement attributed ≈45 % of the `tools/bench` `copy_bound` `send_recv` gap to
//! "literal→heap payload residency", quoting **1 object / 3 words** for the bench's request send
//! and **28 objects / 73 words** for "the heap map `binary_to_term` produces". Those two numbers
//! describe **different questions**, and this file makes that provable structurally:
//!
//! * `28 objects / 73 words` is a `size_object`-style **census of a graph** — the walk descends
//!   *into* literal-area subterms, exactly as the C's `size_object_x(obj, NULL)` does
//!   (`erts/emulator/beam/copy.c:105,119`; `size_object(t)` is that same `litopt == NULL` form,
//!   `global.h:1135`).
//! * `1 object / 3 words` is what a **copy** writes — the copier *shares* a literal-area subterm by
//!   identity, so it costs **0** destination words (`copy.c:720,744,767`; the send path carries a
//!   live literal area, `erl_message.c:722-723,771,797,839,860`, so `litopt != NULL` there and
//!   `copy.c:99`'s literal skip applies).
//!
//! Both readings are *correct about their own question*. So on the same 5-key `copy_bound` payload
//! graph (27 objects / 70 words, the very builder `tests/counters.rs` uses) the `{From, Payload}`
//! term census is **28 objects / 73 words** while the copier writes **1 object / 3 words** — the
//! outer 2-tuple only. That is a *pricing* consequence of literal sharing, not a term-shape
//! divergence: `tests/identity.rs` proves the copied term is `=:=` the source, encodes to the same
//! ETF bytes and is still the same literal pointer. Clearing the single `TAG_LITERAL_PTR` bit
//! (`clear_literal`) makes the same copier write the **whole 29-object / 73-word graph** — asserted
//! below as the control, so the delta is attributable to the literal bit and to nothing else.
//!
//! # What is measured, per shape
//!
//! One row per shape, over the **copy/send window** only (destination staging is outside it):
//!
//! | column | how it is obtained |
//! |---|---|
//! | blocks, bytes | a counting `#[global_allocator]` (modeled on `tests/counters.rs`) |
//! | size histogram, top 4 | the same allocator, bucketed at 8-byte granularity |
//! | words written | `Heap::used()` advance, or the fragment's `used()` on the fragment path |
//! | objects written | the copier's own **write census** (`write_census`, the literal-sharing walk) |
//! | census of the source graph | the `size_object`-style **census** (`census`, the literal-descending walk) |
//! | residency | `on-heap`, `fragment`, or `refused` |
//!
//! The five shapes: (i) a `{From, LiteralPayload}` send where the payload graph is literal-tagged
//! (crafted the way `tests/destination_pricing.rs:126,181` does it: build the graph, then OR the
//! literal bit into its root, so the whole interior lives outside the destination); (ii) the same
//! 5-key payload as a plain heap graph; (iii) that payload into a destination one word short (a
//! refusal); (iv) the same payload through `send_fragmented` (the fragment path); (v) the bench's
//! `ets_copy` shape — the 73-word `{From, Payload}` term refused against a
//! `DEFAULT_MIN_HEAP_WORDS` (233-word) nursery whose live occupancy leaves one word too few.
//!
//! # Invariants only — these figures are "before" numbers
//!
//! A concurrent change collapses copier traversals (the destination-pricing preflight's hashing and
//! the copy's own walk). That change legitimately moves the **blocks/bytes/histogram** figures, so
//! **no assertion here pins a walk count, an allocation count or a byte count**. What is asserted
//! holds before *and* after it:
//!
//! * every destination starts with an **empty** off-heap table in every category;
//! * a fitted copy writes exactly the copier's own price — `used()` advance == `write_census`
//!   words — and its payload is `=:=` the source;
//! * a refused `send` is a **total no-op**: `used() == peak_used() == 0` (nothing written, not even
//!   transiently), the queue is empty, the off-heap table is still empty, the error is
//!   `Alloc(Message)`;
//! * the fragment path's `used()` equals the payload's exact word count, the receiver heap is
//!   untouched, and `Message::heap_words()` reports exactly that;
//! * for a non-literal payload the copy's own census **equals** the source census (nothing is
//!   shared) — so the literal row's asymmetry is attributable to the literal bit alone, which the
//!   `clear_literal` control asserts directly;
//! * every counter window is **identical** across `REPEATS` windows, and the size histogram sums to
//!   the block count, so a stray concurrent allocation fails the run instead of silently inflating
//!   a published figure.
//!
//! Wall-clock appears as one *sanity* column (the minimum over the repeats) and is **never**
//! asserted — these are shared-host numbers; read the counts.
//!
//! # Repro
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message \
//!     --test traversals -- --nocapture
//! ```
//!
//! # Why the tests here serialise
//!
//! The counting allocator is process-wide, so libtest's parallel test threads would interleave
//! their allocations with a counter window (the same reason `tests/counters.rs` is one test). The
//! two tests in this file therefore both hold [`MEASUREMENT_LOCK`] for their whole body: the
//! structural table and the warm-preflight window each need the process to themselves, and holding
//! one lock is what keeps them from measuring each other.
//!
//! The `Census` walk, the counting allocator and the payload builder mirror `tests/counters.rs`
//! deliberately: each test file of this crate is its own binary, so a helper here cannot be shared
//! with it, and the count this file prints must be the *same* count that file publishes.

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use beam_alloc::Heap;
use beam_gc::policy::DEFAULT_MIN_HEAP_WORDS;
use beam_message::off_heap::OffHeapTable;
use beam_message::{
    is_message_heap_exhaustion, send, send_fragmented, shrink_warm_scratch, CopyContext, Queue,
};
use beam_term::prelude::*;
use beam_term::{compare, size_object};

/// Serialises the tests in this file: every one of them measures **global** allocation counters, so
/// two running concurrently would each land in the other's counter window. A poisoned lock (another
/// test panicked while holding it) is still taken — the measurements are independent, so there is
/// nothing for a poison to invalidate.
static MEASUREMENT_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// 1. The counting global allocator (allocation events, bytes, size histogram)
// ---------------------------------------------------------------------------

/// Every `alloc` event, process-wide.
static ALLOC_EVENTS: AtomicUsize = AtomicUsize::new(0);
/// Every byte an `alloc` event asked the system allocator for.
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

/// The histogram's size granularity: bucket `b` covers the `alloc` sizes `[8b, 8b + 7]`.
const HIST_GRANULE: usize = 8;
/// Exact-bucket count: sizes below `HIST_BUCKETS * HIST_GRANULE` (1 KiB) get their own bucket, so
/// a 64-byte `Vec` of walk steps and a 1 KiB SipHash `HashSet` table land in **different** buckets.
const HIST_BUCKETS: usize = 128;
/// The bucket count including the single "everything larger" overflow bucket.
const HIST_LEN: usize = HIST_BUCKETS + 1;

/// The allocation-size histogram, allocation-free by construction: the allocator cannot allocate.
static HIST: [AtomicUsize; HIST_LEN] = [const { AtomicUsize::new(0) }; HIST_LEN];

/// The histogram bucket a `size`-byte allocation lands in.
const fn bucket_index(size: usize) -> usize {
    if size / HIST_GRANULE < HIST_BUCKETS {
        size / HIST_GRANULE
    } else {
        HIST_BUCKETS
    }
}

/// A `#[global_allocator]` that counts allocation events and requested bytes over
/// [`System`](std::alloc::System) and records the event's size in [`HIST`].
///
/// Only `alloc`/`dealloc` are overridden, deliberately: `GlobalAlloc`'s provided `realloc` default
/// is `alloc` + copy + `dealloc` and its `alloc_zeroed` default is `alloc` + zeroing, so a growth
/// event counts as **exactly one** allocation of the new size — the honest reading of "what the
/// boundary asked the allocator for", with no second bookkeeping path that could disagree.
struct CountingAllocator;

// SAFETY: both methods forward the caller's own `layout` to `System` unchanged, so every pointer
// handed out is one `System` produced and every pointer handed back came from `System`; the
// atomics only observe. `System` is a valid `GlobalAlloc` implementation.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_EVENTS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        HIST[bucket_index(layout.size())].fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller's `layout` is forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was handed out by this allocator's `alloc` for exactly `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// The allocation counters plus the size histogram, read at one instant.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Counters {
    blocks: usize,
    bytes: usize,
    hist: [usize; HIST_LEN],
}

impl Counters {
    /// A snapshot. The histogram is read **before** the event atomics so this call's own work (it
    /// allocates nothing, but the ordering must not even look racy) cannot enter the window it
    /// delimits.
    fn now() -> Counters {
        let mut hist = [0usize; HIST_LEN];
        for (slot, atomic) in hist.iter_mut().zip(HIST.iter()) {
            *slot = atomic.load(Ordering::Relaxed);
        }
        Counters {
            blocks: ALLOC_EVENTS.load(Ordering::Relaxed),
            bytes: ALLOC_BYTES.load(Ordering::Relaxed),
            hist,
        }
    }

    /// The events `self` (read *after*) accumulated since `before`.
    fn since(self, before: Counters) -> Counters {
        let mut hist = [0usize; HIST_LEN];
        for i in 0..HIST_LEN {
            hist[i] = self.hist[i] - before.hist[i];
        }
        Counters {
            blocks: self.blocks - before.blocks,
            bytes: self.bytes - before.bytes,
            hist,
        }
    }

    /// Per-block cost of the window, in bytes.
    fn bytes_per_block(self) -> f64 {
        if self.blocks == 0 {
            0.0
        } else {
            self.bytes as f64 / self.blocks as f64
        }
    }

    /// The `n` most frequent size buckets as `(bucket, events)`, count DESC then bucket ASC.
    fn top_buckets(&self, n: usize) -> Vec<(usize, usize)> {
        let mut non_zero: Vec<(usize, usize)> = self
            .hist
            .iter()
            .enumerate()
            .filter(|(_, &count)| count > 0)
            .map(|(bucket, &count)| (bucket, count))
            .collect();
        non_zero.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        non_zero.truncate(n);
        non_zero
    }
}

impl core::fmt::Debug for Counters {
    /// Prints the totals plus only the non-zero histogram buckets (the raw array is 129 entries).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Counters {{ blocks: {}, bytes: {}, hist: {:?} }}",
            self.blocks,
            self.bytes,
            self.top_buckets(8)
        )
    }
}

/// The human label of a histogram bucket: an exact 8-byte range, or the overflow bucket.
fn bucket_label(bucket: usize) -> String {
    if bucket >= HIST_BUCKETS {
        format!(">={}B", HIST_BUCKETS * HIST_GRANULE)
    } else {
        let lo = bucket * HIST_GRANULE;
        format!("{lo}-{}B", lo + HIST_GRANULE - 1)
    }
}

/// `"64-71B:3 128-135B:2"` — the top-`n` allocation sizes of a window.
fn histogram_summary(counters: &Counters, n: usize) -> String {
    let top = counters.top_buckets(n);
    if top.is_empty() {
        return "-".to_string();
    }
    top.iter()
        .map(|(bucket, count)| format!("{}:{count}", bucket_label(*bucket)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Runs `f` and returns its value together with the allocation events it caused. `f` is a plain
/// `FnOnce` — no boxing, no `dyn` — so the window is exactly the call.
fn counted<T>(f: impl FnOnce() -> T) -> (T, Counters) {
    let before = Counters::now();
    let value = f();
    (value, Counters::now().since(before))
}

// ---------------------------------------------------------------------------
// 2. The two censuses (source graph vs what the copier writes)
// ---------------------------------------------------------------------------

/// What a term graph is made of: how many objects, and how their words split into the one-word
/// object header and the payload.
///
/// A cons cell is header-less in the frozen layout (its two words are `[CAR, CDR]`), so it
/// contributes `2` payload words and **no** header word; every other object contributes its
/// `beam_term::layout::object_words` count, of which the first word is the header. This is exactly
/// `tests/counters.rs`'s census — same fields, same walk, same numbers.
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
    /// `header_words + payload_words` — equals `size_object` for the descending walk.
    words: usize,
    /// The largest single object's word count.
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
}

/// The shared walk behind both censuses: an explicit DFS over `for_each_sizing_child_slot` (the
/// same child enumeration `beam_term`'s `size` walk uses) with an on-path set, so a hand-forged
/// cycle terminates instead of hanging.
///
/// `share_literals` is the **only** difference between the two readings, and it is the whole point
/// of this file:
///
/// * `false` — the `size_object_x(t, NULL)` reading: descend *into* a literal-area subterm and
///   count its interior (`copy.c:105,119`).
/// * `true` — the copier's reading (`size_object_x(t, litopt)`, `copy.c:99`, and the send path's
///   live literal area, `erl_message.c:771,797`): a literal-area compound word is **shared by
///   identity**, so it costs `0` destination words and is not descended into. The `is_shared`
///   predicate also shares immediates (skipped before this point) and the zero-arity tuple literal
///   (skipped in both readings, `copy.c:124-140`).
fn census_with(root: Eterm, share_literals: bool) -> Census {
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
        // The literal-area share: a compound word carrying `TAG_LITERAL_PTR` crosses the boundary
        // by identity, so a copy writes none of its words and never reaches its interior.
        // (`is_literal` is a bare bit test, so it is only meaningful on a genuine term word — the
        // `is_term_word` guard above is what makes that sound, exactly as `copy.rs::is_shared`.)
        if share_literals && (is_boxed(term) || is_list(term)) && is_literal(term) {
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
            // The shared zero-arity tuple literal: never a fresh heap object, in either reading.
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

/// The **source graph** reading: `size_object`'s question ("how many words does this graph
/// occupy?"), descending into literal-area subterms. Its `words` equals `size_object(root)` for
/// every graph with no off-heap reference, which is asserted at every use site.
fn census(root: Eterm) -> Census {
    census_with(root, false)
}

/// The **copier** reading: "how many objects and words does `copy_struct` write into the
/// destination?", sharing literal-area subterms by identity. Its `words` equals the destination
/// heap's `used()` advance — the invariant every fitted row asserts.
fn write_census(root: Eterm) -> Census {
    census_with(root, true)
}

// ---------------------------------------------------------------------------
// 3. The source process and the payloads
// ---------------------------------------------------------------------------

/// The free head-room every fitted destination keeps beyond the payload's own words.
const HEADROOM: usize = 64;

/// How many times each counter window is measured; every repeat must produce identical counts.
const REPEATS: usize = 8;

/// The sender identity every shape sends from — an immediate, so the enqueue tripwire accepts it
/// and the copy charges it as a shared word (`tests/counters.rs` uses the same value).
const FROM: i64 = 1;

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

/// Writes a byte-aligned on-heap bitstring of `bytes`.
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

/// The **exact** `copy_bound` benchmark payload (`tools/bench/src/workloads/copy_bound.rs`): a
/// 5-key flat map holding a 10-element int list, an `{ok, <<"hello world">>, 3.5}` tuple, a 12-byte
/// binary, an 11-character char list and `-42`.
///
/// Byte-for-byte the builder `tests/counters.rs::send_recv_payload` uses, so the figures here are
/// directly comparable with that file's.
fn copy_bound_payload(s: &mut Src) -> Eterm {
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

// ---------------------------------------------------------------------------
// 4. The measured phases
// ---------------------------------------------------------------------------

/// Which messaging path a row exercises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// `send` into a destination with room for the whole write price.
    Fits,
    /// `send` into a destination **one word short** of the write price ⇒ `Alloc(Message)`.
    Refused,
    /// `send_fragmented` into a destination with no room at all ⇒ the fragment path.
    Fragment,
}

/// One measured phase.
struct Phase {
    counters: Counters,
    /// The smallest window of the repeats (a shared-host sanity figure — never asserted).
    ns_min: u128,
    /// Words the copier wrote (the destination heap's `used()` advance, or the fragment's `used()`).
    written_words: usize,
    /// Objects the copier wrote (the write census — the literal-sharing walk).
    written_objects: usize,
    /// `on-heap`, `fragment` or `refused`.
    residency: &'static str,
}

/// The destination must start empty in **every** off-heap category — that is
/// `copy.rs::is_fast_path_eligible`, and it is what makes the copy a single-pass write.
fn assert_destination_is_clean(dst_off: &OffHeapTable, label: &str) {
    assert!(
        dst_off.is_empty() && dst_off.binaries().is_empty() && dst_off.nodes().is_empty(),
        "{label}: the destination off-heap table must start empty"
    );
}

/// One run of one phase: stage the destination, run the send inside a counter window, then assert
/// the structural invariants of the mode and report what was written.
///
/// `dest_capacity`/`precharge` model a destination whose live occupancy is `precharge` words, so
/// `available() == dest_capacity - precharge` is the room the copy actually sees.
fn one_phase(
    mode: Mode,
    term: Eterm,
    src: &Src,
    dest_capacity: usize,
    precharge: usize,
    fragment_hint: usize,
) -> Phase {
    let mut dst = Heap::new(dest_capacity).expect("destination heap");
    if precharge > 0 {
        // SAFETY: the heap owns these words and the test never reads them.
        unsafe { dst.alloc_words(precharge).expect("the pre-charge fits") };
    }
    assert_eq!(dst.available(), dest_capacity - precharge, "the modelled free room");
    let mut dst_off = OffHeapTable::new();
    let mut queue = Queue::new();
    assert_destination_is_clean(&dst_off, "phase");
    let before = dst.used();

    let start = Instant::now();
    let (result, counters) = counted(|| {
        let mut ctx = CopyContext::new(&src.off, &mut dst_off);
        match mode {
            Mode::Fits | Mode::Refused => {
                send(make_small(FROM), term, &mut dst, &mut queue, &mut ctx)
            }
            Mode::Fragment => {
                send_fragmented(make_small(FROM), term, &mut dst, &mut queue, &mut ctx, fragment_hint)
            }
        }
    });
    let ns = start.elapsed().as_nanos();

    // The copier's own price for this term: the same walk, with literal sharing applied.
    let priced = write_census(term);

    match (mode, result) {
        (Mode::Fits, Ok(())) => {
            let written = dst.used() - before;
            assert_eq!(written, priced.words, "a fitted copy writes the copier's price");
            assert_eq!(queue.len(), 1, "exactly one message is enqueued");
            assert!(dst_off.is_empty() && dst_off.binaries().is_empty(), "nothing off-heap is owned");
            let message = queue.peek().expect("one queued message");
            assert!(message.fragments().is_none(), "a fitted payload stays on the heap");
            assert_eq!(message.heap_words(), size_object(message.payload()), "its flat charge");
            assert!(compare::eq(term, message.payload(), &src.atoms), "payload =:= original");
            Phase { counters, ns_min: ns, written_words: written,
                written_objects: priced.objects, residency: "on-heap" }
        }
        (Mode::Refused, Err(error)) => {
            assert!(is_message_heap_exhaustion(&error), "a message-heap exhaustion: {error:?}");
            assert_eq!(dst.used(), before, "a refused send writes no destination word");
            assert_eq!(dst.peak_used(), before, "not even transiently: the gate is up front");
            assert!(queue.is_empty(), "a refused send enqueues nothing");
            assert!(dst_off.is_empty() && dst_off.binaries().is_empty(), "no off-heap object");
            Phase { counters, ns_min: ns, written_words: 0,
                written_objects: 0, residency: "refused" }
        }
        (Mode::Fragment, Ok(())) => {
            assert_eq!(dst.used(), before, "the fragment path never touches the receiver heap");
            assert_eq!(queue.len(), 1, "exactly one message is enqueued");
            let message = queue.peek().expect("one queued message");
            let fragments = message.fragments().expect("the payload lands in a fragment");
            let used = fragments.total_used_words();
            assert_eq!(used, priced.words, "the fragment holds the payload's written words");
            assert_eq!(message.heap_words(), used, "a fragmented charge is its chain");
            assert_eq!(message.logical_binary_bytes(), 0, "no refc binary in the envelope");
            assert!(compare::eq(term, message.payload(), &src.atoms), "payload =:= original");
            Phase { counters, ns_min: ns, written_words: used,
                written_objects: priced.objects, residency: "fragment" }
        }
        (mode, result) => panic!("{mode:?}: unexpected send outcome {result:?}"),
    }
}

/// `REPEATS` runs of the same phase: their counter windows and their written-word/object counts
/// must agree **exactly** — the determinism claim that makes these figures quotable. Returns the
/// first measured run, with `ns_min` being the fastest of the repeats.
///
/// One warm-up run is discarded first: the very first send in the process may allocate something
/// one-time (a lazily initialised static inside a dependency), which is not a property of the
/// shape. Discarding it does not weaken the claim — all `REPEATS` measured windows must still be
/// identical to each other.
fn measure(
    label: &str,
    mode: Mode,
    term: Eterm,
    src: &Src,
    dest_capacity: usize,
    precharge: usize,
    fragment_hint: usize,
) -> Phase {
    let _warm_up = one_phase(mode, term, src, dest_capacity, precharge, fragment_hint);
    let mut first = one_phase(mode, term, src, dest_capacity, precharge, fragment_hint);
    for i in 1..REPEATS {
        let again = one_phase(mode, term, src, dest_capacity, precharge, fragment_hint);
        assert_eq!(
            first.counters, again.counters,
            "{label}: counter window {i} differs from window 0 — the counts are not deterministic"
        );
        let wrote = (again.written_words, again.written_objects);
        assert_eq!((first.written_words, first.written_objects), wrote, "{label}: window {i}");
        assert_eq!(first.residency, again.residency, "{label}: window {i} residency");
        first.ns_min = first.ns_min.min(again.ns_min);
    }
    first
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

/// One row of the structural table.
struct Row {
    name: &'static str,
    /// The `size_object`-style census of the graph **sent** (descends into literals).
    source: Census,
    /// What the copier wrote (the write census).
    written_objects: usize,
    written_words: usize,
    residency: &'static str,
    counters: Counters,
    ns_min: u128,
}

/// The whole instrument: the literal-sharing itemisation, the five-row structural table with the
/// allocation histogram, and the size-histogram detail. See the module docs for the method and for
/// why this is one test.
#[test]
fn report_structural_traversals_per_shape() {
    let _guard = MEASUREMENT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    println!(
        "traversals: /proc/loadavg at start = {}",
        loadavg()
    );
    println!(
        "traversals: Eterm = {} bytes, {REPEATS} counter windows per row (all must agree exactly), \
         histogram buckets are {HIST_GRANULE}-byte wide below {} B",
        core::mem::size_of::<Eterm>(),
        HIST_BUCKETS * HIST_GRANULE
    );

    // --- The two graphs the rows send -------------------------------------------------------
    let mut src = Src::new();
    let payload = copy_bound_payload(&mut src);
    let literal_payload = make_literal(payload);
    assert!(
        is_literal(literal_payload),
        "the literal payload carries TAG_LITERAL_PTR"
    );
    assert_eq!(
        boxed_val(literal_payload),
        boxed_val(payload),
        "the literal bit does not move the object: `boxed_val` masks it off"
    );

    let from = make_small(FROM);
    let literal_term = src.heap.tuple(&[from, literal_payload]).expect("the request fits");
    let heap_term = src.heap.tuple(&[from, payload]).expect("the request fits");

    // --- The two censuses, and the whole point of the file --------------------------------
    let payload_census = census(payload);
    assert_eq!(
        payload_census.words,
        size_object(payload),
        "the descending census IS `size_object` for an on-heap graph"
    );
    let literal_source = census(literal_term);
    assert_eq!(
        literal_source.words,
        size_object(literal_term),
        "`size_object` descends INTO literal-area subterms (`copy.c:105,119`, `litopt == NULL`)"
    );
    let literal_written = write_census(literal_term);
    let heap_source = census(heap_term);
    let heap_written = write_census(heap_term);
    assert_eq!(
        heap_written, heap_source,
        "with no literal subterm the copier's price IS the source census — nothing is shared"
    );
    assert_eq!(
        heap_written.words,
        size_object(heap_term),
        "and both equal `size_object` for the heap twin"
    );

    println!();
    println!("== literal sharing, itemised (the '1 object / 3 words vs 28 / 73' asymmetry) ==");
    println!(
        "  payload graph (the 5-key copy_bound map):  {} objects / {} words  \
         ({} cons cells + {} headed, mean {:.2} words/object)",
        payload_census.objects,
        payload_census.words,
        payload_census.cons_cells,
        payload_census.headed,
        payload_census.mean_object_words()
    );
    println!(
        "  sent term {{From, Payload}}, census (size_object, descends into the literal): \
         {} objects / {} words",
        literal_source.objects, literal_source.words
    );
    println!(
        "  sent term {{From, Payload}}, copier price (literal shared by identity):      \
         {} objects / {} words",
        literal_written.objects, literal_written.words
    );
    println!(
        "  => the copier writes {} of the {} words the census counts ({} of {} objects); \
         the difference is the literal area, not a shape change",
        literal_written.words,
        literal_source.words,
        literal_written.objects,
        literal_source.objects
    );
    println!(
        "  the same payload with `clear_literal` applied: {} objects / {} words written \
         (the whole graph) — the delta is the single TAG_LITERAL_PTR bit",
        heap_written.objects, heap_written.words
    );

    // The literal bit is the ONLY difference between the two terminals: same builder, same
    // graph, same copier.
    assert_ne!(
        clear_literal(literal_payload),
        literal_payload,
        "the control really differs by exactly the literal bit"
    );
    assert_eq!(
        clear_literal(literal_payload),
        payload,
        "and clearing it recovers the heap twin's payload word-for-word"
    );
    assert!(
        literal_source.words == heap_source.words,
        "both terminals hold the same graph, so their censuses agree"
    );
    assert!(
        literal_written.words < literal_source.words,
        "the literal copy writes strictly less than its census"
    );
    // The C's rule for the outer 2-tuple: one header word + the `From` immediate + the shared
    // literal word. This is a *copy-semantics* figure (what a shared literal costs), not a walk
    // count — no traversal collapse can change it.
    assert_eq!(
        literal_written.words, 3,
        "a {{From, Literal}} send writes the outer 2-tuple only (header + 2 elements)"
    );
    assert_eq!(literal_written.objects, 1, "exactly one object is materialised");

    // --- The measured rows ----------------------------------------------------------------
    let mut rows: Vec<Row> = Vec::with_capacity(5);

    // (i) The literal request: fits — because it only has to hold the outer tuple.
    let lit = measure("literal {From,Lit} (fits)", Mode::Fits, literal_term, &src,
        literal_written.words + HEADROOM, 0, 0);
    rows.push(row("literal {From,Lit} (fits)", literal_term, &literal_source, lit));

    // (ii) The heap-payload twin: fits at the source price.
    let twin = measure("heap twin {From,Map} (fits)", Mode::Fits, heap_term, &src,
        heap_written.words + HEADROOM, 0, 0);
    rows.push(row("heap twin {From,Map} (fits)", heap_term, &heap_source, twin));

    // (iii) The same payload into a destination one word short: a total no-op refusal.
    let refused = measure("heap twin, 1 word short", Mode::Refused, heap_term, &src,
        heap_written.words - 1, 0, 0);
    rows.push(row("heap twin, 1 word short", heap_term, &heap_source, refused));

    // (iv) The same payload through the fragment path: the receiver heap is full.
    let frag = measure("heap twin, full heap (frag)", Mode::Fragment, heap_term, &src, 1, 1, 0);
    rows.push(row("heap twin, full heap (frag)", heap_term, &heap_source, frag));

    // (v) The bench's `ets_copy` shape: the 73-word `{From, Payload}` term against a
    // `DEFAULT_MIN_HEAP_WORDS` nursery whose live occupancy leaves one word too few.
    assert!(
        heap_source.words < DEFAULT_MIN_HEAP_WORDS,
        "the payload itself fits an empty nursery — the refusal is the live occupancy"
    );
    let occupied = DEFAULT_MIN_HEAP_WORDS - (heap_source.words - 1);
    let nursery = measure("ets_copy 233-w nursery", Mode::Refused, heap_term, &src,
        DEFAULT_MIN_HEAP_WORDS, occupied, 0);
    rows.push(row("ets_copy 233-w nursery", heap_term, &heap_source, nursery));

    // --- The table -------------------------------------------------------------------------
    println!();
    println!(
        "== structural traversals: one send per row, allocation window around the send only =="
    );
    println!(
        "{:<27} {:>8} {:>8} {:>8} {:>8} {:>9} {:>8} {:>11} {:<28} {:>9}",
        "shape", "src_objs", "src_words", "w_objs", "w_words", "residency", "blocks", "bytes",
        "top-4 alloc sizes", "ns_min"
    );
    for row in &rows {
        println!(
            "{:<27} {:>8} {:>8} {:>8} {:>8} {:>9} {:>8} {:>11} {:<28} {:>9}",
            row.name, row.source.objects, row.source.words, row.written_objects,
            row.written_words, row.residency, row.counters.blocks, row.counters.bytes,
            histogram_summary(&row.counters, 4), row.ns_min
        );
    }

    // --- The allocation histogram in full, per row -------------------------------------------
    println!();
    println!("== allocation size histogram per row (bucket: events; every non-zero bucket) ==");
    for row in &rows {
        let detail = row.counters.top_buckets(HIST_LEN).iter()
            .map(|(bucket, count)| format!("{}:{count}", bucket_label(*bucket)))
            .collect::<Vec<_>>().join(" ");
        println!(
            "  {:<27} {:>3} blocks {:>7} B ({:>5.1} B/block)  {}",
            row.name,
            row.counters.blocks,
            row.counters.bytes,
            row.counters.bytes_per_block(),
            if detail.is_empty() { "-" } else { detail.as_str() }
        );
        // Invariant: the per-block totals are the histogram's own sum. A bucket the allocator
        // forgot (or double-counted) would break the two accumulators apart.
        assert_eq!(
            row.counters.hist.iter().sum::<usize>(),
            row.counters.blocks,
            "{}: the histogram must account for every allocation event",
            row.name
        );
        assert!(
            row.counters.blocks > 0 || row.counters.bytes == 0,
            "{}: bytes without blocks is impossible",
            row.name
        );
    }

    // --- The cross-row invariants that must hold before and after a traversal collapse --------
    println!();
    println!("== cross-row invariants ==");
    // The literal row's whole reason to exist: it writes strictly less than the heap twin.
    assert!(
        rows[0].written_words * 4 < rows[1].written_words,
        "the literal request writes less than a quarter of the heap twin's price \
         ({} vs {})",
        rows[0].written_words,
        rows[1].written_words
    );
    // Their *source* censuses are the same graph, so they must be equal.
    assert_eq!(
        rows[0].source.words, rows[1].source.words,
        "both rows send the same graph: the difference is the literal bit alone"
    );
    assert_eq!(
        rows[0].source.objects, rows[1].source.objects,
        "and the same object count"
    );
    // A refusal writes nothing; the fragment path writes the whole payload.
    assert_eq!(rows[2].written_words, 0, "the refused row writes nothing");
    assert_eq!(rows[4].written_words, 0, "the nursery row refuses too");
    assert_eq!(
        rows[3].written_words, rows[1].written_words,
        "the fragment path writes exactly what the on-heap path would have"
    );
    assert_eq!(
        rows[3].residency, "fragment",
        "and it is the fragment path that carried it"
    );
    assert_eq!(rows[1].residency, "on-heap", "the twin stays on the heap");
    assert_eq!(rows[2].residency, "refused", "the short destination refuses");
    assert_eq!(rows[4].residency, "refused", "the nursery refuses");
    println!(
        "  literal sharing: {} words written vs {} censused ({}x smaller); \
         heap twin: {} written == {} censused",
        rows[0].written_words,
        rows[0].source.words,
        rows[0].source.words / rows[0].written_words,
        rows[1].written_words,
        rows[1].source.words
    );
    println!(
        "  refusals: rows 3 and 5 wrote 0 words (used() == peak_used() == 0, empty queues); \
         fragment row wrote {} words into the fragment",
        rows[3].written_words
    );
    println!();
    println!("traversals: /proc/loadavg at end = {}", loadavg());
}

/// Pairs a row's static description with its measured phase.
fn row(name: &'static str, term: Eterm, source: &Census, phase: Phase) -> Row {
    // The row's `source` census must describe the term actually sent, or the `src_words` column
    // would be describing a different graph than the measured send.
    assert_eq!(
        phase.written_words.min(source.words),
        phase.written_words,
        "{name}: a copy can never write more than the source graph occupies"
    );
    assert!(
        !is_immediate(term),
        "{name}: every row sends a compound term"
    );
    Row {
        name,
        source: *source,
        written_objects: phase.written_objects,
        written_words: phase.written_words,
        residency: phase.residency,
        counters: phase.counters,
        ns_min: phase.ns_min,
    }
}

// ---------------------------------------------------------------------------
// 6. The warm-preflight window (the scratch's own allocation behaviour)
// ---------------------------------------------------------------------------

/// **A warm preflight allocates nothing** — the claim the copier's destination-pricing gate rests on.
///
/// The gate prices the whole graph before writing a word, so it runs on *every* refused send and
/// again on every retry; the walk's two buffers (the DFS `Vec` and the path `HashSet`) come from the
/// per-thread `SizingScratch` `beam_message::preflight` keeps, which every walk **clears** rather than
/// drops. This test pins that mechanism at the boundary the crate exposes:
///
/// * with the scratch **cold** (`beam_message::shrink_warm_scratch()`), a refused send's window holds
///   the walk's own allocations on top of the copy's fix-up worklist;
/// * **warm**, the same window holds the copy's worklist only — strictly fewer blocks;
/// * shrinking again returns the window to the *identical* cold figure, so the difference is the
///   retained buffers and nothing else (no growth step, no one-time initialisation).
///
/// It cannot move onto the structural table's rows: those discard a warm-up run (so they measure the
/// warm shape) and must agree across `REPEATS`, which is exactly the state this test must vary.
#[test]
fn report_a_warm_preflight_allocates_nothing() {
    let _guard = MEASUREMENT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    println!("warm preflight: /proc/loadavg at start = {}", loadavg());

    let mut src = Src::new();
    let payload = copy_bound_payload(&mut src);
    let term = src
        .heap
        .tuple(&[make_small(FROM), payload])
        .expect("the request fits");
    let price = write_census(term);
    assert_eq!(
        price.words,
        size_object(term),
        "the payload is the heap twin: its written price IS its census"
    );
    // One word short of the price: the send is refused, which is the shape that runs the gate.
    let capacity = price.words - 1;

    // Cold, warm, cold again. The first `shrink_warm_scratch` also initialises this thread's scratch,
    // so the one-time thread-local setup cannot be mistaken for the walk's own allocations.
    shrink_warm_scratch();
    let cold_a = one_phase(Mode::Refused, term, &src, capacity, 0, 0);
    let warm = one_phase(Mode::Refused, term, &src, capacity, 0, 0);
    shrink_warm_scratch();
    let cold_b = one_phase(Mode::Refused, term, &src, capacity, 0, 0);

    println!();
    println!("== refused send: allocation window, cold vs warm pricing scratch ==");
    for (label, phase) in [
        ("cold (fresh scratch)", &cold_a),
        ("warm (retained)", &warm),
        ("cold again (shrunk)", &cold_b),
    ] {
        println!(
            "  {:<22} {:>3} blocks {:>7} B ({:>5.1} B/block)  {}",
            label,
            phase.counters.blocks,
            phase.counters.bytes,
            phase.counters.bytes_per_block(),
            histogram_summary(&phase.counters, 6)
        );
    }

    // The three windows are the same refusal, so they write the same (nothing).
    for (label, phase) in [
        ("cold (fresh scratch)", &cold_a),
        ("warm (retained)", &warm),
        ("cold again (shrunk)", &cold_b),
    ] {
        assert_eq!(phase.written_words, 0, "{label}: a refusal writes no word");
        assert_eq!(phase.residency, "refused", "{label}: the send was refused");
    }

    // Shrinking restores the cold state *exactly* — same blocks, bytes and histogram — so the
    // cold-to-warm delta is the retained buffers and nothing else.
    assert_eq!(
        cold_a.counters, cold_b.counters,
        "a shrunk scratch must price exactly like a never-warmed one"
    );
    assert!(
        warm.counters.blocks < cold_a.counters.blocks,
        "a warm preflight must allocate strictly less than a cold one \
         (warm {} blocks, cold {} blocks)",
        warm.counters.blocks,
        cold_a.counters.blocks
    );
    assert_eq!(
        warm.counters.hist.iter().sum::<usize>(),
        warm.counters.blocks,
        "the warm histogram must account for every allocation event"
    );
    println!(
        "  => the pricing walk's own {} block(s) / {} B are gone once warm; the {} warm block(s) \
         are the copy's fix-up worklist",
        cold_a.counters.blocks - warm.counters.blocks,
        cold_a.counters.bytes - warm.counters.bytes,
        warm.counters.blocks
    );
    println!();
    println!(
        "warm preflight: /proc/loadavg at end = {}",
        loadavg()
    );
}
