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

//! **The `MinimalContext` arena is chunked.**
//!
//! [`MinimalContext::alloc_words`](beam_engine::ExecutionContext::alloc_words)
//! backed every request with its own `Box<[Eterm]>`, which retained a whole boxed
//! handle (plus an allocator header) per allocation. It now hands out sub-slices of
//! a large fixed-size **chunk** (64 KiB), so an allocation costs its payload only
//! and the arena retains **one** `Box` per chunk.
//!
//! The three contracts this suite pins:
//!
//! * **few chunks** — many small allocations are served from a handful of chunks,
//!   and one allocation never straddles a chunk boundary (proved structurally: two
//!   consecutive `n`-word allocations from the same chunk are exactly `n` words
//!   apart, so a chunk boundary is observable from the pointers alone);
//! * **pointer stability** — a pointer handed out by `alloc_words` still points at
//!   its own words after any number of later allocations (the chunks are boxed and
//!   never move, only the `Vec` of chunk handles grows);
//! * **oversized requests** — a request bigger than a chunk still works, and its
//!   words are never handed out again.
//!
//! It also **measures** the change with a counting global allocator and the
//! process's resident-set size: [`the_chunked_arena_retains_payload_only`] runs the
//! same allocation loop against the new chunked arena and against the old
//! one-box-per-allocation shape (reproduced locally) and prints the comparison.
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

/// The process's resident set size in **pages**, from `/proc/self/statm`, or
/// `None` when it cannot be read (the measurement test skips its RSS line then).
fn rss_pages() -> Option<usize> {
    let text = std::fs::read_to_string("/proc/self/statm").ok()?;
    text.split_whitespace().nth(1)?.parse().ok()
}

/// The chunks a sequence of pointers was served from: a boundary is any step that
/// is **not** exactly the requested `words` (same-chunk allocations are contiguous).
fn chunks_of(pointers: &[*mut Eterm], words: usize) -> Vec<usize> {
    let step = words * WORD_BYTES;
    let mut chunk_starts = vec![0usize];
    for i in 1..pointers.len() {
        let delta = (pointers[i] as isize) - (pointers[i - 1] as isize);
        if delta != step as isize {
            chunk_starts.push(i);
        }
    }
    chunk_starts
}

/// The pre-chunking arena shape, reproduced locally: one `Box<[Eterm]>` per
/// allocation. Kept only as the comparison arm of the measurement test.
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

/// Many small allocations come from a few chunks, one allocation never straddles a
/// chunk boundary, and a chunk is the documented 64 KiB.
#[test]
fn small_allocations_are_served_from_a_handful_of_chunks() {
    const WORDS: usize = 4;
    const N: usize = 10_000;
    let mut ctx = MinimalContext::new();
    let pointers: Vec<*mut Eterm> = (0..N)
        .map(|_| ctx.alloc_words(WORDS).expect("the arena allocates"))
        .collect();

    // Write a distinguishing word through every allocation: the payload must never
    // be shared with another allocation, and (see below) never straddle a chunk.
    for (i, ptr) in pointers.iter().enumerate() {
        // SAFETY: `alloc_words(WORDS)` returned `WORDS` exclusively-owned words; a
        // `usize`-sized write stays inside them (`WORDS >= 1`).
        unsafe { ptr.cast::<usize>().write(i) };
    }
    for (i, ptr) in pointers.iter().enumerate() {
        // SAFETY: as above; the read observes this allocation's own word.
        assert_eq!(unsafe { ptr.cast::<usize>().read() }, i, "no aliasing");
    }

    let starts = chunks_of(&pointers, WORDS);
    assert!(
        starts.len() >= 2,
        "10 000 x {WORDS}-word allocations must span more than one chunk"
    );
    assert!(
        starts.len() <= 8,
        "10 000 x {WORDS}-word allocations must come from a handful of chunks, not {}",
        starts.len()
    );

    // The observable chunk capacity: the number of same-chunk allocations before
    // the first boundary (the *first* chunk is filled from zero).
    let first_chunk_words = starts[1] * WORDS;
    assert_eq!(
        first_chunk_words * WORD_BYTES,
        64 * 1024,
        "the chunk is the documented 64 KiB ({first_chunk_words} words)"
    );
    // Every later chunk holds the same capacity; the last may be a partial chunk.
    for pair in starts.windows(2) {
        let words_in_chunk = (pair[1] - pair[0]) * WORDS;
        assert!(
            words_in_chunk <= first_chunk_words,
            "a chunk handed out {words_in_chunk} words, more than its capacity"
        );
    }
}

/// A pointer handed out by `alloc_words` is still valid (and unmoved) after many
/// later allocations, and writing through it is not disturbed.
#[test]
fn pointers_stay_stable_across_later_allocations() {
    let mut ctx = MinimalContext::new();
    let first = ctx.alloc_words(4).expect("the arena allocates");
    let first_addr = first as usize;
    // SAFETY: `alloc_words(4)` returned four exclusively-owned writable words.
    unsafe {
        first.write(Eterm::from_raw(0xAABB_CCDD));
    }

    // Enough later allocations to start several new chunks and to grow the chunk
    // handle `Vec` more than once.
    for _ in 0..5_000 {
        let later = ctx.alloc_words(4).expect("the arena allocates");
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

/// A request larger than a chunk gets its own chunk, is fully writable, and its
/// words are never handed out again.
#[test]
fn an_allocation_larger_than_a_chunk_works_and_does_not_alias() {
    // The chunk capacity is 64 KiB (asserted by the test above); ask for more.
    const CHUNK_WORDS: usize = 64 * 1024 / WORD_BYTES;
    const BIG_WORDS: usize = CHUNK_WORDS + 100;

    let mut ctx = MinimalContext::new();
    let big = ctx.alloc_words(BIG_WORDS).expect("the arena allocates");
    // The whole oversized request is writable.
    for i in 0..BIG_WORDS {
        // SAFETY: `alloc_words(BIG_WORDS)` returned `BIG_WORDS` exclusively-owned
        // words, so index `i` is in bounds.
        unsafe { big.add(i).write(Eterm::from_raw(i + 1)) };
    }
    for i in 0..BIG_WORDS {
        // SAFETY: as above.
        assert_eq!(unsafe { big.add(i).read() }, Eterm::from_raw(i + 1));
    }

    // A following small allocation must fall outside the oversized block.
    let small = ctx.alloc_words(4).expect("the arena allocates");
    let big_range = big as usize..(big as usize + BIG_WORDS * WORD_BYTES);
    assert!(
        !big_range.contains(&(small as usize)),
        "a small allocation aliased into the oversized block"
    );
    // ...and writing it must not disturb the oversized payload's tail.
    // SAFETY: `alloc_words(4)` returned four exclusively-owned writable words.
    unsafe { small.write(Eterm::from_raw(0xDEAD_BEEF)) };
    // SAFETY: still in bounds of the oversized allocation.
    assert_eq!(
        unsafe { big.add(BIG_WORDS - 1).read() },
        Eterm::from_raw(BIG_WORDS)
    );
}

/// A zero-word request still yields a non-null, well-aligned pointer (the
/// documented "dangling but valid" result), including when the current chunk is
/// exactly full.
#[test]
fn a_zero_word_allocation_is_non_null_and_aligned() {
    const CHUNK_WORDS: usize = 64 * 1024 / WORD_BYTES;
    let mut ctx = MinimalContext::new();
    // Fill a whole chunk exactly, so the arena frontier sits on a boundary.
    let filler = ctx.alloc_words(CHUNK_WORDS).expect("the arena allocates");
    assert!(!filler.is_null());
    let ptr = ctx.alloc_words(0).expect("the arena allocates zero words");
    assert!(!ptr.is_null());
    assert_eq!(ptr as usize % core::mem::align_of::<Eterm>(), 0);
}

/// The measurement: the chunked arena retains payload only.
///
/// The counters are process-global, so this is the file's single armed region; the
/// other tests run concurrently and contribute a handful of chunk allocations,
/// which the generous bounds below absorb.
#[test]
fn the_chunked_arena_retains_payload_only() {
    const WORDS: usize = 4;
    const N: usize = 100_000;

    // --- the new chunked arena (kept alive across the second measurement, so the
    // second one cannot reuse the first one's freed pages) -------------------
    let rss0 = rss_pages();
    let (new_allocs, (ctx, pointers)) = measure(|| {
        let mut ctx = MinimalContext::new();
        let pointers = (0..N)
            .map(|_| ctx.alloc_words(WORDS).expect("the arena allocates"))
            .collect::<Vec<_>>();
        (ctx, pointers)
    });
    let rss1 = rss_pages();
    let new_chunks = chunks_of(&pointers, WORDS).len();

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
         \x20 chunked       : {new_allocs} heap allocations ({new_chunks} chunks), RSS {}\n\
         \x20 box-per-alloc : {old_allocs} heap allocations, RSS {}",
        rss_line(rss0, rss1),
        rss_line(rss1, rss2),
    );

    // The chunked arena's allocation count is bounded by its chunk count (plus the
    // `Vec` growth and whatever the concurrently-running tests add), never by N.
    assert!(
        new_allocs < 256,
        "{N} small allocations must cost a handful of chunk allocations, not {new_allocs}"
    );
    assert!(
        old_allocs >= N,
        "the old shape allocated one box per request, so it must be at least {N}, got {old_allocs}"
    );
    assert!(
        new_allocs * 10 < old_allocs,
        "the chunked arena must be dramatically cheaper: {new_allocs} vs {old_allocs}"
    );
}
