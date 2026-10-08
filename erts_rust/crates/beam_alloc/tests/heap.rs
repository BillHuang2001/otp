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

//! Crate-level integration tests for the per-process [`beam_alloc::Heap`].
//!
//! These exercise the heap through its **public** API only (`Heap`, the frozen
//! [`Allocator`] seam, `AllocStats`) plus the frozen term inspection helpers of
//! `beam_term`, the way a dependent crate (`beam_message`, `beam_gc`, `beam_ets`)
//! would use it.
//!
//! The heap is defined over the [`Allocator`] seam, so the tests drive it with a
//! recording `std::alloc`-backed allocator double ([`TestAllocator`]) instead of the
//! carrier allocator: the heap's contract (bump geometry, exhaustion, pointer
//! stability, `Drop`) is independent of the carrier framework, and the double makes
//! the "everything was returned" assertion exact.
//!
//! `unsafe` below only dereferences pointers that this test allocated and wrote
//! itself.

use std::alloc::{alloc as std_alloc, dealloc as std_dealloc, realloc as std_realloc, Layout};
use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_alloc::{AllocRef, AllocStats, Allocator, Heap};
use beam_common::{AllocFailure, AllocKind, BeamError};
use beam_term::prelude::{
    arityval, car, cdr, is_arity_value, is_list, is_small, is_tuple, list_val, make_arityval,
    make_small, make_tuple, signed_value, tuple_element, tuple_size, Eterm, MAX_ARITYVAL, NIL,
};

/// Byte size of one term word.
const WORD: usize = size_of::<Eterm>();

/// A recording allocator over `std::alloc`, used only by these tests.
///
/// It satisfies the [`Allocator`] contract (`Send + Sync + 'static`, return-based
/// failure, no panic), counts live blocks/bytes so `Drop` can be asserted to return
/// everything, and implements the zero-size rule: a zero-sized request yields the
/// conventional dangling-but-aligned non-null pointer and never reaches `std::alloc`.
#[derive(Default)]
struct TestAllocator {
    alloc_calls: AtomicUsize,
    free_calls: AtomicUsize,
    realloc_calls: AtomicUsize,
    live_blocks: AtomicUsize,
    live_bytes: AtomicUsize,
}

impl TestAllocator {
    fn new() -> Arc<TestAllocator> {
        Arc::new(TestAllocator::default())
    }

    fn live_blocks(&self) -> usize {
        self.live_blocks.load(Ordering::SeqCst)
    }

    fn live_bytes(&self) -> usize {
        self.live_bytes.load(Ordering::SeqCst)
    }
}

impl Allocator for TestAllocator {
    fn alloc(&self, layout: Layout) -> beam_common::Result<NonNull<u8>> {
        let size = layout.size();
        if size == 0 {
            // SAFETY: `Layout::align()` is a non-zero power of two, so the resulting
            // address is non-null; a zero-sized block owns no memory.
            return Ok(unsafe { NonNull::new_unchecked(layout.align() as *mut u8) });
        }
        // SAFETY: `size` is non-zero and `layout` describes a valid allocation.
        let ptr = NonNull::new(unsafe { std_alloc(layout) })
            .ok_or_else(|| BeamError::Alloc(AllocFailure::new(AllocKind::Other, size)))?;
        self.alloc_calls.fetch_add(1, Ordering::SeqCst);
        self.live_blocks.fetch_add(1, Ordering::SeqCst);
        self.live_bytes.fetch_add(size, Ordering::SeqCst);
        Ok(ptr)
    }

    fn realloc(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> beam_common::Result<NonNull<u8>> {
        if old_layout.size() == 0 || new_size == 0 {
            // `std::alloc::realloc` requires both sizes to be non-zero. `Heap` never
            // reallocates, so this double refuses the unsupported corner instead of
            // doing something wrong.
            return Err(BeamError::BadArg);
        }
        self.realloc_calls.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the trait contract guarantees `ptr` came from `alloc`/`realloc` on
        // this allocator with `old_layout`; both sizes are non-zero.
        let ptr = NonNull::new(unsafe { std_realloc(ptr.as_ptr(), old_layout, new_size) })
            .ok_or_else(|| BeamError::Alloc(AllocFailure::new(AllocKind::Other, new_size)))?;
        self.live_bytes.fetch_add(new_size, Ordering::SeqCst);
        self.live_bytes
            .fetch_sub(old_layout.size(), Ordering::SeqCst);
        Ok(ptr)
    }

    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            // Nothing was allocated for a zero-sized block (see `alloc`).
            return;
        }
        self.free_calls.fetch_add(1, Ordering::SeqCst);
        self.live_blocks.fetch_sub(1, Ordering::SeqCst);
        self.live_bytes.fetch_sub(layout.size(), Ordering::SeqCst);
        // SAFETY: the trait contract guarantees `ptr` came from `alloc`/`realloc` on
        // this allocator with `layout`, and is freed exactly once.
        unsafe { std_dealloc(ptr.as_ptr(), layout) };
    }

    fn stats(&self) -> AllocStats {
        AllocStats {
            alloc_calls: self.alloc_calls.load(Ordering::SeqCst) as u64,
            free_calls: self.free_calls.load(Ordering::SeqCst) as u64,
            realloc_calls: self.realloc_calls.load(Ordering::SeqCst) as u64,
            live_blocks: self.live_blocks.load(Ordering::SeqCst) as u64,
            live_bytes: self.live_bytes.load(Ordering::SeqCst),
            ..AllocStats::default()
        }
    }

    fn name(&self) -> &str {
        "test-stdlib-allocator"
    }
}

/// The trait-object handle of `alloc` — what a dependent crate would hold.
fn alloc_ref(alloc: &Arc<TestAllocator>) -> AllocRef {
    let concrete: Arc<TestAllocator> = Arc::clone(alloc);
    concrete
}

/// A `words`-word heap over a fresh recording allocator.
fn new_heap(words: usize) -> (Arc<TestAllocator>, Heap) {
    let alloc = TestAllocator::new();
    let heap =
        Heap::with_allocator(alloc_ref(&alloc), words).expect("the test allocator never fails");
    (alloc, heap)
}

/// `hend - hp` in bytes; must always equal `available() * WORD`.
fn hp_span(heap: &Heap) -> usize {
    (heap.hend() as usize) - (heap.hp() as usize)
}

#[test]
fn geometry_tracks_the_bump_pointer_exactly() {
    let (_alloc, mut heap) = new_heap(10);

    assert_eq!(heap.capacity(), 10);
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.available(), 10);
    assert_eq!(heap.peak_used(), 0);
    assert_eq!(hp_span(&heap), 10 * WORD);

    // SAFETY: `p` owns the 2 words that are written in the next statement.
    let p = unsafe { heap.alloc_words(2) }.expect("2 of 10 words fit");
    // SAFETY: `p` is the base of 2 exclusively-owned words.
    unsafe {
        *p = make_small(1);
        *p.add(1) = make_small(2);
    }

    assert_eq!(heap.used(), 2);
    assert_eq!(heap.available(), 8);
    assert_eq!(heap.peak_used(), 2);
    assert_eq!(hp_span(&heap), 8 * WORD);
    assert_eq!(heap.hp() as usize - p as usize, 2 * WORD);
    assert_eq!(
        heap.hend() as usize - heap.hp() as usize,
        heap.available() * WORD
    );

    // SAFETY: the two words written above are still live.
    unsafe {
        assert_eq!(signed_value(*p), 1);
        assert_eq!(signed_value(*p.add(1)), 2);
    }
}

#[test]
fn alloc_words_hands_out_contiguous_and_stable_words() {
    let (_alloc, mut heap) = new_heap(16);
    let start = heap.hp();
    let mut ptrs: Vec<*mut Eterm> = Vec::new();

    for i in 0..16_i64 {
        // SAFETY: each call hands out one exclusively-owned, writable word.
        let p = unsafe { heap.alloc_words(1) }.expect("16 of 16 words fit");
        // SAFETY: `p` owns the word that was just handed out.
        unsafe { *p = make_small(i) };
        ptrs.push(p);
    }

    assert_eq!(ptrs[0], start, "allocation starts at hp");
    assert_eq!(heap.used(), 16);
    assert_eq!(heap.available(), 0);
    assert_eq!(hp_span(&heap), 0);

    // Every pointer is still valid and unchanged after all 16 allocations: the heap
    // never reallocates or moves a live word (workspace `CONTEXT.md` §2), so the
    // words are strictly ascending and exactly one word apart.
    for (i, p) in ptrs.iter().enumerate() {
        // SAFETY: `*p` is a word initialised in the loop above and still inside the
        // live heap block.
        assert_eq!(unsafe { **p }, make_small(i as i64));
        assert_eq!(*p as usize, start as usize + i * WORD);
    }
}

#[test]
fn alloc_tuple_writes_an_arityval_header_for_a_walkable_tuple() {
    let (_alloc, mut heap) = new_heap(8);

    // SAFETY: `alloc_tuple` writes the header word; the 3 element words are written
    // in the loop below and never read before that.
    let p = unsafe { heap.alloc_tuple(3) }.expect("4 of 8 words fit");

    // SAFETY: `alloc_tuple` initialised the header word at `p`.
    unsafe {
        assert_eq!(*p, make_arityval(3));
        assert!(is_arity_value(*p));
        assert_eq!(arityval(*p), 3);
    }
    for i in 0..3_i64 {
        // SAFETY: `alloc_tuple(3)` owns the 3 element words at `p + 1 .. p + 4`.
        unsafe { *p.add(1 + i as usize) = make_small(100 + i) };
    }

    let term = make_tuple(p);
    // SAFETY: `term` wraps the live 3-arity heap tuple built above.
    unsafe {
        assert!(is_tuple(term));
        assert_eq!(tuple_size(term), 3);
        for i in 0..3 {
            assert_eq!(signed_value(tuple_element(term, i)), 100 + i as i64);
        }
    }
    assert_eq!(heap.used(), 4);
    assert_eq!(heap.available(), 4);
}

#[test]
fn tuple_builder_round_trips_its_elements() {
    let (_alloc, mut heap) = new_heap(8);

    let term = heap
        .tuple(&[make_small(1), make_small(-2), NIL])
        .expect("4 of 8 words fit");

    // SAFETY: `tuple` wraps a live 3-arity heap tuple.
    unsafe {
        assert!(is_tuple(term));
        assert_eq!(tuple_size(term), 3);
        assert!(is_small(tuple_element(term, 0)));
        assert_eq!(signed_value(tuple_element(term, 0)), 1);
        assert_eq!(signed_value(tuple_element(term, 1)), -2);
        assert_eq!(tuple_element(term, 2), NIL);
    }
    assert_eq!(heap.used(), 4);
}

#[test]
fn cons_builds_a_proper_list_that_reads_back() {
    let (_alloc, mut heap) = new_heap(6);

    let list = heap.cons(make_small(1), NIL).expect("2 of 6 words fit");
    let list = heap.cons(make_small(2), list).expect("2 of 4 words fit");
    let list = heap
        .cons(make_small(3), list)
        .expect("the last 2 words fit");

    assert_eq!(heap.used(), 6);
    assert_eq!(heap.available(), 0);

    // Walk the list back: 3, 2, 1, [].
    let mut cur = list;
    let mut seen = Vec::new();
    while is_list(cur) {
        // SAFETY: `cur` is a live cons cell here (`is_list` excludes `NIL`).
        let (head, tail) = unsafe { (car(cur), cdr(cur)) };
        assert!(is_small(head));
        seen.push(signed_value(head));
        cur = tail;
    }
    assert_eq!(cur, NIL);
    assert_eq!(seen, vec![3, 2, 1]);

    // A cons needs two words, so the heap is full: the next one is refused.
    assert!(matches!(
        heap.cons(make_small(4), list),
        Err(BeamError::Alloc(_))
    ));
}

#[test]
fn a_term_tree_is_built_entirely_on_the_heap() {
    // A 3-element proper list of 2-tuples: each tuple costs 3 words, each cons 2,
    // so 3 * (3 + 2) = 15 words.
    let (_alloc, mut heap) = new_heap(15);

    let mut list = NIL;
    for pair in [(1_i64, 2_i64), (3, 4), (5, 6)] {
        let tuple = heap
            .tuple(&[make_small(pair.0), make_small(pair.1)])
            .expect("3 of the remaining words fit");
        list = heap
            .cons(tuple, list)
            .expect("2 of the remaining words fit");
    }

    assert_eq!(heap.used(), 15, "the heap is exactly full");
    assert_eq!(heap.available(), 0);
    assert_eq!(heap.peak_used(), 15);
    // The tree is intact at the end: nothing was moved or reallocated.
    let mut cur = list;
    let mut pairs = Vec::new();
    while is_list(cur) {
        // SAFETY: `cur` is a live cons cell; its head is a live tuple built above.
        let (head, tail) = unsafe { (car(cur), cdr(cur)) };
        // SAFETY: `head` is a live 2-arity heap tuple built in this test.
        let pair = unsafe {
            assert!(is_tuple(head));
            assert_eq!(tuple_size(head), 2);
            (
                signed_value(tuple_element(head, 0)),
                signed_value(tuple_element(head, 1)),
            )
        };
        pairs.push(pair);
        cur = tail;
    }
    assert_eq!(cur, NIL);
    assert_eq!(pairs, vec![(5, 6), (3, 4), (1, 2)]);
}

#[test]
fn reset_rewinds_and_peak_is_a_high_water_mark() {
    let (_alloc, mut heap) = new_heap(8);

    // SAFETY: `p` owns the 5 words written in the next statement.
    let p = unsafe { heap.alloc_words(5) }.expect("5 of 8 words fit");
    // SAFETY: `p` is the base of 5 exclusively-owned words.
    unsafe {
        for i in 0..5 {
            *p.add(i) = make_small(i as i64);
        }
    }
    assert_eq!(heap.peak_used(), 5);

    heap.reset();
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.available(), 8);
    assert_eq!(heap.peak_used(), 5, "reset keeps the high-water mark");
    assert_eq!(hp_span(&heap), 8 * WORD);

    // The bump pointer is rewound onto the very same words — the block is never
    // reallocated (growth is by replacement; workspace `CONTEXT.md` §2).
    // SAFETY: `p2` owns 5 words; `*p` is still live, initialised memory.
    unsafe {
        let p2 = heap.alloc_words(5).expect("5 of 8 words fit");
        assert_eq!(p2, p);
        assert_eq!(signed_value(*p), 0);
        assert_eq!(signed_value(*p.add(4)), 4);
    }
    assert_eq!(heap.used(), 5);
    assert_eq!(heap.peak_used(), 5, "never above the peak");
}

#[test]
fn exhaustion_is_a_heap_failure_that_consumes_nothing() {
    let (_alloc, mut heap) = new_heap(4);
    let hp_before = heap.hp();

    // SAFETY: rejected before any word is handed out.
    let too_many = unsafe { heap.alloc_words(5) };
    match too_many {
        Err(BeamError::Alloc(failure)) => {
            assert_eq!(failure.kind, AllocKind::Heap);
            assert_eq!(failure.requested, 5 * WORD);
        }
        other => panic!("expected a heap allocation failure, got {other:?}"),
    }
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.hp(), hp_before);
    assert_eq!(heap.peak_used(), 0);

    // Exactly the capacity is fine; one word more is not.
    // SAFETY: exactly the 4 words of capacity.
    unsafe { heap.alloc_words(4) }.expect("4 of 4 words fit");
    assert_eq!(heap.hp(), heap.hend());
    // SAFETY: the heap is full, so this is rejected without consuming a word.
    let overflow = unsafe { heap.alloc_words(1) };
    match overflow {
        Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Heap),
        other => panic!("expected a heap allocation failure, got {other:?}"),
    }
    assert_eq!(heap.used(), 4);
    assert_eq!(heap.hp(), heap.hend());
}

#[test]
fn builders_report_exhaustion_instead_of_panicking() {
    let (_alloc, mut heap) = new_heap(3);

    // A 3-tuple needs 4 words (header + 3 elements).
    assert!(matches!(
        heap.tuple(&[make_small(1), make_small(2), make_small(3)]),
        Err(BeamError::Alloc(failure)) if failure.kind == AllocKind::Heap
    ));
    assert_eq!(heap.used(), 0, "the failed builder consumed no words");

    // A 2-tuple needs 3: exactly enough.
    assert!(heap.tuple(&[make_small(1), make_small(2)]).is_ok());
    assert_eq!(heap.used(), 3);
    // A cons needs 2 more.
    assert!(matches!(
        heap.cons(make_small(0), NIL),
        Err(BeamError::Alloc(failure)) if failure.kind == AllocKind::Heap
    ));
    assert_eq!(heap.used(), 3);
}

#[test]
fn alloc_tuple_rejects_an_arity_beyond_arityval() {
    let (_alloc, mut heap) = new_heap(4);
    let hp_before = heap.hp();

    // SAFETY: the arity is rejected before any word is handed out.
    let too_big = unsafe { heap.alloc_tuple(MAX_ARITYVAL + 1) };
    assert!(
        matches!(too_big, Err(BeamError::SystemLimit)),
        "expected system_limit, got {too_big:?}"
    );
    // SAFETY: as above; also guards the internal `n + 1` against overflow.
    let huge = unsafe { heap.alloc_tuple(usize::MAX) };
    assert!(
        matches!(huge, Err(BeamError::SystemLimit)),
        "expected system_limit, got {huge:?}"
    );
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.hp(), hp_before);
    assert_eq!(heap.peak_used(), 0);
}

#[test]
fn zero_capacity_heap_is_immediately_and_repeatably_exhausted() {
    let alloc = TestAllocator::new();
    let mut heap = Heap::with_allocator(alloc_ref(&alloc), 0).expect("construction never fails");

    assert_eq!(heap.capacity(), 0);
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.available(), 0);
    assert_eq!(heap.peak_used(), 0);

    for _ in 0..3 {
        // SAFETY: the request is rejected up front; no pointer is dereferenced.
        let one_word = unsafe { heap.alloc_words(1) };
        match one_word {
            Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Heap),
            other => panic!("expected a heap allocation failure, got {other:?}"),
        }
        // SAFETY: the tuple header request is rejected up front too.
        let header = unsafe { heap.alloc_tuple(0) };
        match header {
            Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Heap),
            other => panic!("expected a heap allocation failure, got {other:?}"),
        }
        assert_eq!(heap.used(), 0);
        assert_eq!(heap.peak_used(), 0);
    }

    drop(heap);
    assert_eq!(alloc.live_blocks(), 0);
    assert_eq!(alloc.live_bytes(), 0);
}

#[test]
fn drop_returns_every_byte_of_the_block_to_the_allocator() {
    let alloc = TestAllocator::new();
    let words = 24;
    let hp_before;

    {
        let mut heap =
            Heap::with_allocator(alloc_ref(&alloc), words).expect("construction never fails");
        assert_eq!(heap.capacity(), words);
        assert_eq!(heap.allocator().name(), "test-stdlib-allocator");
        assert_eq!(alloc.live_blocks(), 1, "one block for the whole heap");
        assert_eq!(alloc.live_bytes(), words * WORD);
        assert_eq!(hp_span(&heap), words * WORD);
        hp_before = heap.hp();

        // SAFETY: one exclusively-owned word, written and unused afterwards.
        let p = unsafe { heap.alloc_words(1) }.expect("1 of 24 words fits");
        assert_eq!(p, hp_before);
        // SAFETY: `p` owns the word just handed out.
        unsafe { *p = make_small(1) };
        // The heap never reallocates: the backing block is unchanged after use.
        assert_eq!(alloc.live_blocks(), 1);
        assert_eq!(alloc.live_bytes(), words * WORD);
    }

    assert_eq!(alloc.live_blocks(), 0, "Drop frees the backing block");
    assert_eq!(alloc.live_bytes(), 0);
    let stats = alloc.stats();
    assert_eq!(stats.alloc_calls, 1);
    assert_eq!(stats.free_calls, 1);
    assert_eq!(stats.realloc_calls, 0, "the heap never reallocates");
    assert_eq!(stats.live_blocks, 0);
    assert_eq!(stats.live_bytes, 0);
}

#[test]
fn a_list_survives_a_later_heap_replacement() {
    // Growth in this runtime is by *replacement*: the collector allocates a bigger
    // heap and copies into it, leaving the old block untouched. This test pins the
    // building blocks of that flow — a fully populated heap keeps reading back
    // correctly while a second, independent heap is created and used.
    let (alloc_a, mut old_heap) = new_heap(2);
    let old_list = old_heap.cons(make_small(1), NIL).expect("2 of 2 words fit");
    assert_eq!(hp_span(&old_heap), 0);

    let (alloc_b, mut new_heap) = new_heap(8);
    let copied = new_heap.cons(make_small(1), NIL).expect("2 of 8 words fit");

    // The copy is a term in the new heap with identical contents...
    // SAFETY: `old_list` and `copied` are live cons cells in their own heaps.
    unsafe {
        assert_eq!(car(old_list), car(copied));
        assert_eq!(cdr(old_list), cdr(copied));
    }
    // ...but it lives in the other block (a replacement, never a move in place).
    assert_ne!(list_val(old_list) as usize, list_val(copied) as usize);
    // SAFETY: `old_list` is still live: dropping the old heap is what invalidates it,
    // not the new allocation.
    unsafe { assert_eq!(car(old_list), make_small(1)) };

    drop(old_heap);
    assert_eq!(alloc_a.live_blocks(), 0);
    assert_eq!(alloc_b.live_blocks(), 1, "the new heap is unaffected");
    // SAFETY: `copied` belongs to `new_heap`, which is still alive.
    unsafe { assert_eq!(car(copied), make_small(1)) };
    drop(new_heap);
    assert_eq!(alloc_b.live_blocks(), 0);
}
