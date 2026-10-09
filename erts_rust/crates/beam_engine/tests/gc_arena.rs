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

//! **The `MinimalContext` heap is a region heap.**
//!
//! [`MinimalContext::alloc_words`](beam_engine::ExecutionContext::alloc_words) used to
//! back every request with its own `Box<[Eterm]>`, which retained a whole boxed handle
//! (plus an allocator header) per allocation. It now reserves a **large address range**
//! per region (4 MiB) and commits a slice of it up front (64 KiB), raising the visible
//! slice **in place** by committing more words as requests arrive — the bump pointer
//! does not move, so consecutive allocations stay contiguous. A region is abandoned for
//! a fresh, **doubled** one only when its whole *reservation* is exhausted; the old
//! region is left exactly as it was, so every pointer it handed out stays valid and
//! unmoved for the context's lifetime.
//!
//! The contracts this suite pins:
//!
//! * **one region, no boundaries** — many small allocations are served from a single
//!   region grown in place, so two consecutive `n`-word allocations are **exactly** `n`
//!   words apart with no boundary between them (proved structurally from the pointers
//!   alone);
//! * **pointer stability** — a pointer handed out by `alloc_words` still points at its
//!   own words after any number of later allocations (a region only ever grows in place
//!   or is left behind for a fresh one, never moved);
//! * **reservation exhaustion** — a request too large for a region's *remaining*
//!   reservation is served from a fresh, larger region that does not overlap the old
//!   one, and the old region — and every pointer into it — survives.
//!
//! It also **measures** the change with a counting global allocator and the process's
//! resident-set size: [`the_region_heap_retains_payload_only`] runs the same allocation
//! loop against the new region heap and against the old one-box-per-allocation shape
//! (reproduced locally) and prints the comparison.
//!
//! # Running it
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml \
//!     -p beam_engine --test gc_arena -- --nocapture
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use beam_engine::{ExecutionContext, MinimalContext};
use beam_term::Eterm;

// ---------------------------------------------------------------------------
// The counting allocator (as in tests/bif_alloc.rs)
// ---------------------------------------------------------------------------

struct Counting;

/// Whether the counters are currently armed.
static COUNTING: AtomicBool = AtomicBool::new(false);
/// `alloc`/`alloc_zeroed`/`realloc` calls observed while armed.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn note() {
    if COUNTING.load(Ordering::Relaxed) {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to the `System` allocator with the same
// `Layout`, so the (ptr, layout) pairing the callers released is preserved
// exactly; the counter is a plain atomic and allocates nothing.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded verbatim to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Run `f` with the counters armed, returning `(allocations, f's value)`.
fn measure<T>(f: impl FnOnce() -> T) -> (usize, T) {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
    let value = f();
    COUNTING.store(false, Ordering::Relaxed);
    (ALLOCATIONS.load(Ordering::Relaxed), value)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The size of one heap word in bytes (what a small allocation advances by).
const WORD_BYTES: usize = core::mem::size_of::<Eterm>();

/// The first region's **reservation** in `Eterm` words (4 MiB), matching
/// `beam_engine::context`'s `HEAP_RESERVE_WORDS`. It is the address range one region
/// can grow in place before a fresh region is needed, so it bounds what a single run of
/// contiguous allocations can cover.
const RESERVE_WORDS: usize = 4 * 1024 * 1024 / WORD_BYTES;

/// The process's resident set size in **pages**, from `/proc/self/statm`, or
/// `None` when it cannot be read (the measurement test skips its RSS line then).
fn rss_pages() -> Option<usize> {
    let text = std::fs::read_to_string("/proc/self/statm").ok()?;
    text.split_whitespace().nth(1)?.parse().ok()
}

/// The region starts a sequence of pointers was served from: a boundary is any step
/// that is **not** exactly the requested `words` (allocations inside one region are
/// contiguous, so a boundary marks the switch to a fresh region). A run served from a
/// single region therefore yields exactly one element.
fn region_boundaries_of(pointers: &[*mut Eterm], words: usize) -> Vec<usize> {
    let step = words * WORD_BYTES;
    let mut region_starts = vec![0usize];
    for i in 1..pointers.len() {
        let delta = (pointers[i] as isize) - (pointers[i - 1] as isize);
        if delta != step as isize {
            region_starts.push(i);
        }
    }
    region_starts
}

/// The old one-box-per-allocation arena shape, reproduced locally: one `Box<[Eterm]>`
/// per allocation. Kept only as the comparison arm of the measurement test.
struct BoxPerAllocArena {
    blocks: Vec<Box<[Eterm]>>,
}

impl BoxPerAllocArena {
    fn new() -> BoxPerAllocArena {
        BoxPerAllocArena { blocks: Vec::new() }
    }

    fn alloc(&mut self, n: usize) -> *mut Eterm {
        let mut block = vec![Eterm::default(); n].into_boxed_slice();
        let ptr = block.as_mut_ptr();
        self.blocks.push(block);
        ptr
    }
}

// ---------------------------------------------------------------------------
// The proof
// ---------------------------------------------------------------------------

/// Many small allocations are served from a **single** region grown in place: they are
/// contiguous, word for word, with no boundary anywhere in the run.
#[test]
fn small_allocations_grow_one_region_in_place() {
    const WORDS: usize = 4;
    const N: usize = 10_000;
    let mut ctx = MinimalContext::new();
    let pointers: Vec<*mut Eterm> = (0..N)
        .map(|_| ctx.alloc_words(WORDS).expect("the region heap allocates"))
        .collect();

    // Write a distinguishing word through every allocation: the payload must never
    // be shared with another allocation.
    for (i, ptr) in pointers.iter().enumerate() {
        // SAFETY: `alloc_words(WORDS)` returned `WORDS` exclusively-owned words; a
        // `usize`-sized write stays inside them (`WORDS >= 1`).
        unsafe { ptr.cast::<usize>().write(i) };
    }
    for (i, ptr) in pointers.iter().enumerate() {
        // SAFETY: as above; the read observes this allocation's own word.
        assert_eq!(unsafe { ptr.cast::<usize>().read() }, i, "no aliasing");
    }

    // Every consecutive pair is EXACTLY `WORDS` apart: the region grew in place, so
    // there is not a single boundary in the run.
    for i in 1..N {
        let delta = (pointers[i] as isize) - (pointers[i - 1] as isize);
        assert_eq!(
            delta,
            (WORDS * WORD_BYTES) as isize,
            "allocation {i} must follow allocation {} contiguously",
            i - 1
        );
    }

    // ...which is exactly "one region": the boundary scan finds only the start.
    assert_eq!(
        region_boundaries_of(&pointers, WORDS).len(),
        1,
        "10 000 x {WORDS}-word allocations must be served from a single region"
    );

    // The whole payload is well inside one region's reservation — the reason no fresh
    // region was ever required.
    assert!(
        N * WORDS < RESERVE_WORDS,
        "the payload ({N} x {WORDS} words) must fit one reservation ({RESERVE_WORDS} words)"
    );
}

/// A pointer handed out by `alloc_words` is still valid (and unmoved) after many
/// later allocations, and writing through it is not disturbed.
#[test]
fn pointers_stay_stable_across_later_allocations() {
    let mut ctx = MinimalContext::new();
    let first = ctx.alloc_words(4).expect("the region heap allocates");
    let first_addr = first as usize;
    // SAFETY: `alloc_words(4)` returned four exclusively-owned writable words.
    unsafe {
        first.write(Eterm::from_raw(0xAABB_CCDD));
    }

    // Enough later allocations to grow the region's committed slice several times over:
    // the region only ever commits more words in place, so `first` must not move.
    for _ in 0..5_000 {
        let later = ctx.alloc_words(4).expect("the region heap allocates");
        // SAFETY: `alloc_words(4)` returned four exclusively-owned writable words.
        unsafe {
            later.write(Eterm::from_raw(0x1122_3344));
        }
    }

    assert_eq!(first as usize, first_addr, "the pointer never moves");
    // SAFETY: the allocation is still live and exclusively owned by this test.
    assert_eq!(
        unsafe { first.read() },
        Eterm::from_raw(0xAABB_CCDD),
        "the words the pointer owns are untouched by later allocations"
    );
}

/// A request too large for a region's *remaining* reservation forces a fresh, larger
/// region (`MoveRequired`): the oversized block is fully writable, it is disjoint from
/// the earlier region, and the earlier region — and every pointer into it — survives.
#[test]
fn an_exhausted_reservation_gets_a_fresh_region_and_does_not_alias() {
    const WORDS: usize = 4;
    // More words than a whole region reservation: a region that has already handed out
    // the first two allocations cannot grow its reservation to cover this request.
    const BIG_WORDS: usize = RESERVE_WORDS + 8;

    let mut ctx = MinimalContext::new();

    // Region 0 hands out `first` at its base (offset 0).
    let first = ctx
        .alloc_words(WORDS)
        .expect("region 0 serves the first allocation");
    // SAFETY: `alloc_words(WORDS)` returned `WORDS` exclusively-owned words.
    unsafe { first.write(Eterm::from_raw(0xAAAA_0001)) };
    // SAFETY: as above.
    assert_eq!(unsafe { first.read() }, Eterm::from_raw(0xAAAA_0001));

    // ...and `middle` contiguously right after it, inside the same region.
    let middle = ctx
        .alloc_words(WORDS)
        .expect("region 0 serves the second allocation");
    assert_eq!(
        middle as isize - first as isize,
        (WORDS * WORD_BYTES) as isize,
        "the two small allocations are contiguous inside region 0"
    );
    // SAFETY: `alloc_words(WORDS)` returned `WORDS` exclusively-owned words.
    unsafe { middle.write(Eterm::from_raw(0xBBBB_0002)) };

    // Region 0 has used `2 * WORDS = 8` words, so this request exceeds what is left of
    // its *reservation*: only a fresh region can serve it.
    let big = ctx
        .alloc_words(BIG_WORDS)
        .expect("a fresh region serves the oversized request");

    // A fresh region is a separate reservation, so the oversized block starts OUTSIDE
    // region 0's address span `[first, first + RESERVE_WORDS words)` — proving the
    // fresh-region path ran rather than an in-place grow.
    let region0_end = first as usize + RESERVE_WORDS * WORD_BYTES;
    assert!(
        !((first as usize)..region0_end).contains(&(big as usize)),
        "the oversized block must be served from a fresh reservation, not region 0"
    );

    // The whole oversized block is writable and readable, word for word.
    for i in 0..BIG_WORDS {
        // SAFETY: `alloc_words(BIG_WORDS)` returned `BIG_WORDS` exclusively-owned
        // words, so index `i` is in bounds.
        unsafe { big.add(i).write(Eterm::from_raw(i + 1)) };
    }
    for i in 0..BIG_WORDS {
        // SAFETY: as above.
        assert_eq!(unsafe { big.add(i).read() }, Eterm::from_raw(i + 1));
    }

    // No aliasing: the oversized block's range excludes both earlier allocations...
    let big_range = big as usize..(big as usize + BIG_WORDS * WORD_BYTES);
    assert!(
        !big_range.contains(&(first as usize)),
        "the oversized block aliased `first`"
    );
    assert!(
        !big_range.contains(&(middle as usize)),
        "the oversized block aliased `middle`"
    );

    // ...and a following small allocation is disjoint from it too.
    let small = ctx
        .alloc_words(WORDS)
        .expect("the frontier region keeps allocating");
    assert!(
        !big_range.contains(&(small as usize)),
        "a small allocation aliased into the oversized block"
    );
    // SAFETY: `alloc_words(WORDS)` returned `WORDS` exclusively-owned writable words.
    unsafe { small.write(Eterm::from_raw(0xDEAD_BEEF)) };
    // SAFETY: still in bounds of the oversized allocation.
    assert_eq!(
        unsafe { big.add(BIG_WORDS - 1).read() },
        Eterm::from_raw(BIG_WORDS)
    );

    // The OLD region is kept alive and unmoved: its markers survive the fresh region,
    // read back through the ORIGINAL pointers.
    // SAFETY: `first`/`middle` still point into the live region 0.
    unsafe {
        assert_eq!(first.read(), Eterm::from_raw(0xAAAA_0001));
        assert_eq!(middle.read(), Eterm::from_raw(0xBBBB_0002));
    }
}

/// A zero-word request still yields a non-null, well-aligned pointer (the documented
/// "dangling but valid" result) — on a fresh context that has reserved no region yet,
/// and again after some real allocations.
#[test]
fn a_zero_word_allocation_is_non_null_and_aligned() {
    let mut ctx = MinimalContext::new();
    // Fresh context: no region has been reserved.
    let fresh = ctx
        .alloc_words(0)
        .expect("the region heap allocates zero words");
    assert!(!fresh.is_null());
    assert_eq!(fresh as usize % core::mem::align_of::<Eterm>(), 0);

    // ...and again once a region exists and is in use.
    let _ = ctx.alloc_words(8).expect("the region heap allocates");
    let _ = ctx.alloc_words(64).expect("the region heap allocates");
    let ptr = ctx
        .alloc_words(0)
        .expect("the region heap allocates zero words");
    assert!(!ptr.is_null());
    assert_eq!(ptr as usize % core::mem::align_of::<Eterm>(), 0);
}

/// The measurement: the region heap retains payload only.
///
/// The counters are process-global, so this is the file's single armed region; the
/// other tests run concurrently and contribute a handful of region allocations,
/// which the generous bounds below absorb.
#[test]
fn the_region_heap_retains_payload_only() {
    const WORDS: usize = 4;
    const N: usize = 100_000;

    // --- the new region heap (kept alive across the second measurement, so the second
    // one cannot reuse the first one's freed pages) -------------------------
    let rss0 = rss_pages();
    let (new_allocs, (ctx, pointers)) = measure(|| {
        let mut ctx = MinimalContext::new();
        let pointers = (0..N)
            .map(|_| ctx.alloc_words(WORDS).expect("the region heap allocates"))
            .collect::<Vec<_>>();
        (ctx, pointers)
    });
    let rss1 = rss_pages();
    let new_regions = region_boundaries_of(&pointers, WORDS).len();

    // --- the old one-box-per-allocation shape (reproduced locally) ----------
    let (old_allocs, old_arena) = measure(|| {
        let mut arena = BoxPerAllocArena::new();
        for _ in 0..N {
            let _ = arena.alloc(WORDS);
        }
        arena
    });
    let rss2 = rss_pages();
    drop(old_arena);
    drop(pointers);
    drop(ctx);

    let payload_bytes = N * WORDS * WORD_BYTES;
    let rss_line = |before: Option<usize>, after: Option<usize>| match (before, after) {
        (Some(b), Some(a)) => format!("+{} pages", a.saturating_sub(b)),
        _ => "n/a (no /proc/self/statm)".to_string(),
    };
    println!(
        "gc_arena | N={N} x {WORDS} words ({payload_bytes} bytes of payload):\n\
         \x20 region heap   : {new_allocs} heap allocations ({new_regions} region), RSS {}\n\
         \x20 box-per-alloc : {old_allocs} heap allocations, RSS {}",
        rss_line(rss0, rss1),
        rss_line(rss1, rss2),
    );

    // `N x WORDS` words fit inside one reservation, so the whole loop is served from a
    // single region grown in place — no boundary anywhere in the run.
    assert!(
        N * WORDS < RESERVE_WORDS,
        "the payload must fit one reservation for this to pin the single-region shape"
    );
    assert_eq!(
        new_regions, 1,
        "the payload fits one reservation, so it must be served from a single region"
    );

    // The region heap's allocation count is bounded by its region count (plus whatever
    // the concurrently-running tests add), never by N.
    assert!(
        new_allocs < 256,
        "{N} small allocations must cost a handful of region allocations, not {new_allocs}"
    );
    assert!(
        old_allocs >= N,
        "the old shape allocated one box per request, so it must be at least {N}, got {old_allocs}"
    );
    assert!(
        new_allocs * 10 < old_allocs,
        "the region heap must be dramatically cheaper: {new_allocs} vs {old_allocs}"
    );
}
