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

//! The per-process bump heap: the production term-construction arena.
//!
//! `Heap` is the *production* counterpart of the minimal `beam_term::heap::Heap`
//! test arena. Its geometry is identical (`hp`/`hend`, fixed capacity) but it is
//! backed by a real [`AllocRef`], and it **never reallocates**: handed-out pointers
//! stay valid for the heap's lifetime, so growth happens by *replacement* — the GC
//! allocates a larger heap and copies into it — never by moving live terms
//! (workspace `CONTEXT.md` §2).
//!
//! Exhaustion is a **normal** condition: it is reported as
//! [`BeamError::Alloc`](beam_common::BeamError::Alloc) with
//! [`AllocKind::Heap`](beam_common::AllocKind) so the engine/GC layer can collect
//! and retry (see the crate `CONTEXT.md`, Design Decisions).
//!
//! `unsafe` is confined to this module's marked functions.

use core::alloc::Layout;
use core::mem::size_of;
use core::ptr::NonNull;

use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::allocator::{AllocRef, MIN_ALIGN};
use crate::bump;
use crate::registry::default_allocator;

/// A fixed-capacity bump arena over a carrier-provided block.
pub struct Heap {
    alloc: AllocRef,
    base: *mut Eterm,
    cap: usize,
    used: usize,
    peak: usize,
}

impl Heap {
    /// A new heap of `capacity_words` term words, backed by the default allocator.
    pub fn new(capacity_words: usize) -> Result<Heap> {
        Heap::with_allocator(default_allocator().clone(), capacity_words)
    }

    /// A new heap of `capacity_words` term words, backed by `alloc`.
    pub fn with_allocator(alloc: AllocRef, capacity_words: usize) -> Result<Heap> {
        let bytes = capacity_words.saturating_mul(size_of::<Eterm>());
        let block = alloc.alloc_bytes(bytes)?;
        Ok(Heap {
            alloc,
            base: block.as_ptr().cast::<Eterm>(),
            cap: capacity_words,
            used: 0,
            peak: 0,
        })
    }

    /// Total capacity in term words.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Words handed out so far.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Words still available.
    pub fn available(&self) -> usize {
        self.cap - self.used
    }

    /// High-water mark of [`used`](Heap::used) since the last `reset`.
    pub fn peak_used(&self) -> usize {
        self.peak
    }

    /// Rewinds the bump pointer. This invalidates **all** previously handed-out
    /// pointers (the GC calls it only after copying live terms out).
    pub fn reset(&mut self) {
        self.used = 0;
    }

    /// The current allocation pointer (`hp`).
    pub fn hp(&self) -> *mut Eterm {
        // SAFETY: `base` is valid for `cap` words and `used <= cap`, so the
        // displacement is in-bounds (or exactly one-past-the-end when full).
        unsafe { self.base.add(self.used) }
    }

    /// One past the last word of the heap (`hend`).
    pub fn hend(&self) -> *mut Eterm {
        // SAFETY: `base` is valid for `cap` words, so `cap` is one-past-the-end.
        unsafe { self.base.add(self.cap) }
    }

    /// The allocator backing this heap.
    pub fn allocator(&self) -> &AllocRef {
        &self.alloc
    }

    /// Allocates `n` contiguous, **uninitialised** words.
    ///
    /// # Safety
    ///
    /// The returned words are uninitialised; the caller must write them before they
    /// are read, and must not use them after this heap is dropped. The heap never
    /// reallocates, so the pointer stays valid for the heap's lifetime.
    pub unsafe fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        // SAFETY: `hp()` is a pointer into this exclusively-owned heap's block with
        // `available()` writable words still ahead of it, which is exactly
        // `bump::alloc_words_at`'s contract.
        let p = unsafe { bump::alloc_words_at(self.hp(), self.available(), n)? };
        self.used += n;
        self.peak = self.peak.max(self.used);
        Ok(p)
    }

    /// Allocates a tuple of arity `n`: writes the arityval header and returns the
    /// header pointer, with `n` uninitialised element words following it.
    ///
    /// # Safety
    ///
    /// The `n` element words are uninitialised; the caller must write them before they
    /// are read, and must not use the pointer after this heap is dropped.
    pub unsafe fn alloc_tuple(&mut self, n: usize) -> Result<*mut Eterm> {
        // SAFETY: as for `alloc_words`; `alloc_tuple_at` writes the header word itself
        // and leaves the `n` element words to the caller.
        let p = unsafe { bump::alloc_tuple_at(self.hp(), self.available(), n)? };
        self.used += n + 1;
        self.peak = self.peak.max(self.used);
        Ok(p)
    }

    /// Builds a cons cell `[car, cdr]` and returns the list term.
    pub fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm> {
        // SAFETY: `hp()` has `available()` writable words ahead of it in this
        // exclusively-owned heap, which is `bump::cons_at`'s contract.
        let term = unsafe { bump::cons_at(self.hp(), self.available(), car, cdr)? };
        self.used += 2;
        self.peak = self.peak.max(self.used);
        Ok(term)
    }

    /// Builds a tuple from `elems` and returns the tuple term.
    pub fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        // SAFETY: `hp()` has `available()` writable words ahead of it in this
        // exclusively-owned heap, which is `bump::tuple_at`'s contract.
        let term = unsafe { bump::tuple_at(self.hp(), self.available(), elems)? };
        self.used += 1 + elems.len();
        self.peak = self.peak.max(self.used);
        Ok(term)
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        let bytes = self.cap * size_of::<Eterm>();
        let layout = match Layout::from_size_align(bytes, MIN_ALIGN) {
            Ok(layout) => layout,
            // `with_allocator` built this layout, so it must be valid; there is
            // nothing sensible to do in `Drop` if it somehow is not.
            Err(_) => return,
        };
        let ptr = match NonNull::new(self.base.cast::<u8>()) {
            Some(ptr) => ptr,
            // A null base means nothing was handed out (zero-capacity heap).
            None => return,
        };
        // SAFETY: `base` came from `self.alloc.alloc_bytes(bytes)` in
        // `with_allocator` with this exact size and alignment, and is freed exactly
        // once here (the heap never reallocates).
        unsafe { self.alloc.dealloc(ptr, layout) };
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the bump heap: geometry, `alloc_words`/`alloc_tuple`/`cons`/
    //! `tuple`, exhaustion, pointer stability (no reallocation) and `Drop`.
    //!
    //! The heap is defined over the frozen [`Allocator`] seam, so these tests use a
    //! recording `std::alloc`-backed double instead of the carrier allocator: the
    //! heap's contract stays testable independently of the carrier framework, and
    //! `default_allocator()` is never touched (the seam is what `Heap` depends on).
    //!
    //! `unsafe` below only dereferences pointers this test has just allocated and
    //! written itself.

    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use beam_common::{AllocFailure, AllocKind, BeamError};
    use beam_term::prelude::{
        arityval, car, cdr, is_arity_value, is_list, is_tuple, list_val, make_arityval, make_small,
        make_tuple, signed_value, tuple_element, tuple_size, MAX_ARITYVAL, NIL,
    };

    use crate::allocator::{AllocRef, Allocator};
    use crate::stats::AllocStats;

    use super::*;

    /// A minimal [`Allocator`] over `std::alloc`, used only by these tests.
    ///
    /// It records live blocks/bytes so `Drop` can be asserted to return every byte
    /// to the allocator, and it implements the zero-size rule of the trait: a
    /// zero-sized request yields the conventional dangling-but-aligned non-null
    /// pointer and never reaches `std::alloc`.
    #[derive(Default)]
    struct StdlibAllocator {
        alloc_calls: AtomicUsize,
        free_calls: AtomicUsize,
        live_blocks: AtomicUsize,
        live_bytes: AtomicUsize,
    }

    impl StdlibAllocator {
        fn live_blocks(&self) -> usize {
            self.live_blocks.load(Ordering::SeqCst)
        }

        fn live_bytes(&self) -> usize {
            self.live_bytes.load(Ordering::SeqCst)
        }
    }

    impl Allocator for StdlibAllocator {
        fn alloc(&self, layout: Layout) -> Result<NonNull<u8>> {
            let size = layout.size();
            if size == 0 {
                // SAFETY: `Layout::align()` is a non-zero power of two, so the
                // address is non-null; a zero-sized block owns no memory.
                return Ok(unsafe { NonNull::new_unchecked(layout.align() as *mut u8) });
            }
            let ptr = NonNull::new(unsafe { std::alloc::alloc(layout) })
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
        ) -> Result<NonNull<u8>> {
            if old_layout.size() == 0 || new_size == 0 {
                // `std::alloc::realloc` requires both sizes to be non-zero; this
                // double does not model that corner (the heap never reallocates at
                // all), so it refuses instead of doing something wrong.
                return Err(BeamError::BadArg);
            }
            // SAFETY: the trait contract guarantees `ptr` came from `alloc`/`realloc`
            // on this allocator with `old_layout`, and both sizes are non-zero.
            let ptr =
                NonNull::new(unsafe { std::alloc::realloc(ptr.as_ptr(), old_layout, new_size) })
                    .ok_or_else(|| {
                        BeamError::Alloc(AllocFailure::new(AllocKind::Other, new_size))
                    })?;
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
            // SAFETY: the trait contract guarantees `ptr` came from `alloc`/`realloc`
            // on this allocator with `layout`, and it is freed exactly once.
            unsafe { std::alloc::dealloc(ptr.as_ptr(), layout) };
        }

        fn stats(&self) -> AllocStats {
            AllocStats {
                alloc_calls: self.alloc_calls.load(Ordering::SeqCst) as u64,
                free_calls: self.free_calls.load(Ordering::SeqCst) as u64,
                live_blocks: self.live_blocks.load(Ordering::SeqCst) as u64,
                live_bytes: self.live_bytes.load(Ordering::SeqCst),
                ..AllocStats::default()
            }
        }

        fn name(&self) -> &str {
            "stdlib-test-allocator"
        }
    }

    /// The trait-object handle of `alloc`, i.e. what a dependent crate would hold.
    fn alloc_ref(alloc: &Arc<StdlibAllocator>) -> AllocRef {
        let concrete: Arc<StdlibAllocator> = Arc::clone(alloc);
        concrete
    }

    /// A heap of `words` term words over the recording test allocator.
    fn new_heap(words: usize) -> (Arc<StdlibAllocator>, Heap) {
        let alloc = Arc::new(StdlibAllocator::default());
        let handle = alloc_ref(&alloc);
        let heap = Heap::with_allocator(handle, words).expect("the test allocator never fails");
        (alloc, heap)
    }

    /// `hend - hp` in bytes; must always equal `available() * WORD_SIZE`.
    fn hp_span(heap: &Heap) -> usize {
        (heap.hend() as usize) - (heap.hp() as usize)
    }

    const WORD: usize = size_of::<Eterm>();

    #[test]
    fn capacity_used_available_and_peak_geometry() {
        let (_alloc, mut heap) = new_heap(16);

        assert_eq!(heap.capacity(), 16);
        assert_eq!(heap.used(), 0);
        assert_eq!(heap.available(), 16);
        assert_eq!(heap.peak_used(), 0);
        assert_eq!(hp_span(&heap), 16 * WORD);
        let hp_before = heap.hp();

        // SAFETY: the heap is exclusively owned here and the three words are written
        // below (never read before that) and are not used after the heap is dropped.
        let p = unsafe { heap.alloc_words(3) }.expect("3 of 16 words fit");
        assert_eq!(p, hp_before, "the first allocation starts at hp");
        assert_eq!(heap.used(), 3);
        assert_eq!(heap.available(), 13);
        assert_eq!(heap.peak_used(), 3);
        assert_eq!(hp_span(&heap), 13 * WORD);
        assert_eq!(heap.hp() as usize - p as usize, 3 * WORD);

        // SAFETY: as above; `p` is the base of 3 freshly handed-out words.
        unsafe {
            *p = make_small(1);
            *p.add(1) = make_small(2);
            *p.add(2) = make_small(3);
        }

        // SAFETY: the heap is exclusively owned here.
        unsafe { heap.alloc_words(1) }.expect("one more word fits");
        assert_eq!(heap.used(), 4);
        assert_eq!(heap.available(), 12);
        assert_eq!(hp_span(&heap), 12 * WORD);

        // SAFETY: `p`'s three words were initialised above and are still live.
        unsafe {
            assert_eq!(*p, make_small(1));
            assert_eq!(*p.add(1), make_small(2));
            assert_eq!(*p.add(2), make_small(3));
        }
    }

    #[test]
    fn alloc_words_is_contiguous_and_never_reallocates() {
        let (_alloc, mut heap) = new_heap(64);
        let mut ptrs: Vec<*mut Eterm> = Vec::with_capacity(64);

        for i in 0..64_i64 {
            // SAFETY: each call hands out one exclusively-owned, writable word.
            let p = unsafe { heap.alloc_words(1) }.expect("64 of 64 words fit");
            // SAFETY: `p` owns one word that was just handed out.
            unsafe { *p = make_small(i) };
            ptrs.push(p);
        }

        assert_eq!(heap.used(), 64);
        assert_eq!(heap.available(), 0);
        assert_eq!(heap.peak_used(), 64);

        for (i, p) in ptrs.iter().enumerate() {
            // SAFETY: `*p` is a live word initialised in the loop above.
            assert_eq!(unsafe { **p }, make_small(i as i64));
            if i > 0 {
                // Contiguous and ascending: the bump pointer only ever moves forward
                // within the one block `with_allocator` obtained, so no earlier word
                // was moved or reallocated (workspace `CONTEXT.md` §2).
                assert_eq!(*p as usize - ptrs[i - 1] as usize, WORD);
            }
        }
        assert_eq!(hp_span(&heap), 0);
        assert_eq!(heap.hp(), heap.hend());
    }

    #[test]
    fn alloc_words_zero_consumes_nothing() {
        let (_alloc, mut heap) = new_heap(4);
        let hp_before = heap.hp();

        // SAFETY: a zero-word request hands out no words at all.
        let p = unsafe { heap.alloc_words(0) }.expect("zero words always fit");

        assert_eq!(p, hp_before);
        assert_eq!(heap.hp(), hp_before);
        assert_eq!(heap.used(), 0);
        assert_eq!(heap.available(), 4);
        assert_eq!(heap.peak_used(), 0);
    }

    #[test]
    fn alloc_tuple_writes_the_arityval_header() {
        let (_alloc, mut heap) = new_heap(8);
        let hp_before = heap.hp();

        // SAFETY: the one header word plus three element words are written below.
        let p = unsafe { heap.alloc_tuple(3) }.expect("4 of 8 words fit");

        assert_eq!(p, hp_before);
        // SAFETY: `alloc_tuple` initialised the header word at `p`.
        assert_eq!(unsafe { *p }, make_arityval(3));
        // SAFETY: as above — the header is a valid arityval.
        assert!(unsafe { is_arity_value(*p) });
        // SAFETY: as above.
        assert_eq!(unsafe { arityval(*p) }, 3);
        assert_eq!(heap.used(), 4);

        for i in 0..3_i64 {
            // SAFETY: `alloc_tuple(3)` owns words `1..=3` after the header.
            unsafe { *p.add(1 + i as usize) = make_small(10 * (i + 1)) };
        }

        let term = make_tuple(p);
        // SAFETY: `term` wraps a live 3-arity heap tuple.
        unsafe {
            assert!(is_tuple(term));
            assert_eq!(tuple_size(term), 3);
            assert_eq!(tuple_element(term, 0), make_small(10));
            assert_eq!(tuple_element(term, 1), make_small(20));
            assert_eq!(tuple_element(term, 2), make_small(30));
        }
    }

    #[test]
    fn tuple_round_trips_elements() {
        let (_alloc, mut heap) = new_heap(8);
        let elems = [make_small(7), NIL, make_small(-3)];
        let term = heap.tuple(&elems).expect("4 of 8 words fit");

        // SAFETY: `tuple` wraps a live 3-arity heap tuple.
        unsafe {
            assert!(is_tuple(term));
            assert_eq!(tuple_size(term), 3);
            for (i, e) in elems.iter().enumerate() {
                assert_eq!(tuple_element(term, i), *e);
            }
            assert_eq!(signed_value(tuple_element(term, 2)), -3);
        }
        assert_eq!(heap.used(), 4);
    }

    #[test]
    fn tuple_arity_zero_writes_only_the_header() {
        let (_alloc, mut heap) = new_heap(1);
        let term = heap.tuple(&[]).expect("the header word fits");

        // SAFETY: `tuple` wraps a live 0-arity heap tuple, whose single header word
        // was written by `alloc_tuple`.
        unsafe {
            assert!(is_tuple(term));
            assert_eq!(tuple_size(term), 0);
        }
        assert_eq!(heap.used(), 1);
        assert_eq!(heap.available(), 0);
    }

    #[test]
    fn cons_builds_a_proper_list() {
        let (_alloc, mut heap) = new_heap(4);

        let tail = heap.cons(make_small(2), NIL).expect("2 of 4 words fit");
        assert!(is_list(tail));
        // SAFETY: `tail` is a live cons cell.
        unsafe {
            assert_eq!(car(tail), make_small(2));
            assert_eq!(cdr(tail), NIL);
        }

        let head = heap
            .cons(make_small(1), tail)
            .expect("the last 2 words fit");
        // SAFETY: `head` is a live cons cell whose cdr is `tail`.
        unsafe {
            assert_eq!(car(head), make_small(1));
            assert_eq!(cdr(head), tail);
            assert_eq!(car(cdr(head)), make_small(2));
            assert_eq!(cdr(cdr(head)), NIL);
        }
        // The two cells are adjacent: cons does not reallocate or move the first one
        // (`head` was built after `tail`, so its cell is the one further along).
        assert_eq!(list_val(head) as usize - list_val(tail) as usize, 2 * WORD);
        assert_eq!(heap.used(), 4);
        assert_eq!(heap.available(), 0);
    }

    #[test]
    fn reset_rewinds_the_bump_pointer_and_reuses_the_words() {
        let (_alloc, mut heap) = new_heap(8);

        // SAFETY: `p` owns 2 words that are written in the next statement.
        let p = unsafe { heap.alloc_words(2) }.expect("2 of 8 words fit");
        // SAFETY: `p` owns exactly 2 words.
        unsafe {
            *p = make_small(11);
            *p.add(1) = make_small(22);
        }
        // SAFETY: one more exclusively-owned word.
        unsafe { heap.alloc_words(1) }.expect("1 of 6 remaining words fits");
        assert_eq!(heap.used(), 3);

        heap.reset();
        assert_eq!(heap.used(), 0);
        assert_eq!(heap.available(), 8);
        assert_eq!(heap.peak_used(), 3, "reset keeps the high-water mark");
        assert_eq!(hp_span(&heap), 8 * WORD);

        // The same words are handed out again (the block is never reallocated), and
        // reset does not move or wipe the block.
        // SAFETY: `p2` owns 2 words; `p` reads back words that were initialised above
        // and are still inside the live block.
        unsafe {
            let p2 = heap.alloc_words(2).expect("2 of 8 words fit");
            assert_eq!(p2, p);
            assert_eq!(*p, make_small(11));
            assert_eq!(*p.add(1), make_small(22));
        }
        assert_eq!(heap.peak_used(), 3);
    }

    #[test]
    fn peak_used_is_the_high_water_mark() {
        let (_alloc, mut heap) = new_heap(16);
        assert_eq!(heap.peak_used(), 0);

        // SAFETY: 5 of 16 words.
        unsafe { heap.alloc_words(5) }.expect("5 of 16 words fit");
        assert_eq!(heap.peak_used(), 5);

        heap.reset();
        // SAFETY: 3 of 16 words.
        unsafe { heap.alloc_words(3) }.expect("3 of 16 words fit");
        assert_eq!(heap.used(), 3);
        assert_eq!(heap.peak_used(), 5, "below the high-water mark");

        // SAFETY: 9 more words (3 + 9 = 12 <= 16).
        unsafe { heap.alloc_words(9) }.expect("9 of 13 remaining words fit");
        assert_eq!(heap.used(), 12);
        assert_eq!(heap.peak_used(), 12);

        // SAFETY: 4 more words (12 + 4 = 16, exactly full).
        unsafe { heap.alloc_words(4) }.expect("the last 4 words fit");
        assert_eq!(heap.peak_used(), 16);
        assert_eq!(heap.available(), 0);
        assert_eq!(heap.hp(), heap.hend());
    }

    #[test]
    fn exhaustion_reports_a_heap_failure_and_consumes_nothing() {
        let (_alloc, mut heap) = new_heap(4);
        let hp_before = heap.hp();

        // SAFETY: the request is rejected before anything is handed out.
        let too_much = unsafe { heap.alloc_words(5) };
        match too_much {
            Err(BeamError::Alloc(failure)) => {
                assert_eq!(failure.kind, AllocKind::Heap);
                assert_eq!(failure.requested, 5 * WORD);
            }
            other => panic!("expected a heap allocation failure, got {other:?}"),
        }
        assert_eq!(heap.used(), 0, "a failed request consumes no words");
        assert_eq!(heap.hp(), hp_before);
        assert_eq!(heap.available(), 4);
        assert_eq!(heap.peak_used(), 0);

        // Exactly `capacity` words is *not* exhaustion; one more is.
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
    fn alloc_tuple_needs_arity_plus_one_words_and_checks_arity_first() {
        let (_alloc, mut heap) = new_heap(4);
        let hp_before = heap.hp();

        // An arity beyond `MAX_ARITYVAL` (and an overflowing one) is rejected before
        // any word is consumed, so `n + 1` never overflows.
        // SAFETY: rejected before any word is handed out.
        let bad = unsafe { heap.alloc_tuple(MAX_ARITYVAL + 1) };
        assert!(
            matches!(bad, Err(BeamError::SystemLimit)),
            "expected system_limit, got {bad:?}"
        );
        // SAFETY: as above.
        let huge = unsafe { heap.alloc_tuple(usize::MAX) };
        assert!(
            matches!(huge, Err(BeamError::SystemLimit)),
            "expected system_limit, got {huge:?}"
        );
        assert_eq!(heap.used(), 0);
        assert_eq!(heap.hp(), hp_before);
        assert_eq!(heap.peak_used(), 0);

        // A 3-tuple needs 4 words (header + 3 elements), so it fits exactly...
        // SAFETY: 4 of 4 words.
        unsafe { heap.alloc_tuple(3) }.expect("4 of 4 words fit");
        assert_eq!(heap.used(), 4);
        // ...and even a 0-tuple does not fit any more, because the heap is full.
        // SAFETY: the heap is full, so a single header word is rejected.
        let full = unsafe { heap.alloc_tuple(0) };
        match full {
            Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Heap),
            other => panic!("expected a heap allocation failure, got {other:?}"),
        }
        assert_eq!(heap.used(), 4);
    }

    #[test]
    fn tuple_and_cons_exhaustion_report_heap_failures() {
        let (_alloc, mut heap) = new_heap(2);

        // A 2-tuple needs 3 words.
        assert!(matches!(
            heap.tuple(&[make_small(1), make_small(2)]),
            Err(BeamError::Alloc(_))
        ));
        assert_eq!(heap.used(), 0);

        // A cons needs 2 words of the 2 available: exactly enough.
        let list = heap.cons(make_small(1), NIL).expect("2 of 2 words fit");
        assert!(is_list(list));
        // A second cons does not.
        assert!(matches!(
            heap.cons(make_small(2), list),
            Err(BeamError::Alloc(_))
        ));
        assert_eq!(heap.used(), 2);
        assert_eq!(heap.hp(), heap.hend());
    }

    #[test]
    fn zero_capacity_heap_never_hands_out_a_word() {
        let alloc = Arc::new(StdlibAllocator::default());
        let handle = alloc_ref(&alloc);
        let mut heap = Heap::with_allocator(handle, 0).expect("the test allocator never fails");

        assert_eq!(heap.capacity(), 0);
        assert_eq!(heap.used(), 0);
        assert_eq!(heap.available(), 0);
        assert_eq!(heap.peak_used(), 0);

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

        drop(heap);
        assert_eq!(alloc.live_blocks(), 0);
        assert_eq!(alloc.live_bytes(), 0);
    }

    #[test]
    fn an_unrepresentable_word_count_is_a_typed_failure_never_an_overflow() {
        // `alloc_words` is an `unsafe fn` whose safety contract does not bound `n` (the
        // request is simply checked against the heap's `available()`), so `usize::MAX` is
        // a legal call. Converting that word count into the byte count an `AllocFailure`
        // carries must saturate rather than overflow: a plain `n * size_of::<Eterm>()`
        // panics in a debug build and wraps to a bogus count in a release build.
        let (_alloc, mut heap) = new_heap(8);
        let hp_before = heap.hp();

        // SAFETY: the request is rejected against `available()` before any word is
        // handed out, so no pointer is dereferenced.
        let request = unsafe { heap.alloc_words(usize::MAX) };
        match request {
            Err(BeamError::Alloc(failure)) => {
                assert_eq!(failure.kind, AllocKind::Heap);
                assert_eq!(
                    failure.requested,
                    usize::MAX,
                    "the byte count saturates instead of wrapping"
                );
            }
            other => panic!("expected a heap allocation failure, got {other:?}"),
        }

        // The rejection left no half-built state behind: no word was consumed and the
        // bump pointer stands exactly where it did.
        assert_eq!(heap.used(), 0);
        assert_eq!(heap.peak_used(), 0);
        assert_eq!(heap.available(), 8);
        assert_eq!(heap.hp(), hp_before);
    }

    #[test]
    fn drop_returns_the_whole_block_to_the_allocator() {
        let alloc = Arc::new(StdlibAllocator::default());
        let bytes = 32 * WORD;

        {
            let handle = alloc_ref(&alloc);
            let mut heap =
                Heap::with_allocator(handle, 32).expect("the test allocator never fails");
            assert_eq!(heap.capacity(), 32);
            assert_eq!(alloc.live_blocks(), 1);
            assert_eq!(alloc.live_bytes(), bytes);
            assert_eq!(heap.allocator().name(), "stdlib-test-allocator");
            let handle = alloc_ref(&alloc);
            assert!(Arc::ptr_eq(heap.allocator(), &handle));

            // SAFETY: one exclusively-owned word, never read (the heap is dropped).
            unsafe { heap.alloc_words(1) }.expect("1 of 32 words fits");
            assert_eq!(heap.used(), 1);
            assert_eq!(alloc.live_blocks(), 1, "the heap never reallocates");
        }

        assert_eq!(alloc.live_blocks(), 0, "Drop frees the backing block");
        assert_eq!(alloc.live_bytes(), 0);
        let stats = alloc.stats();
        assert_eq!(stats.alloc_calls, 1);
        assert_eq!(stats.free_calls, 1);
        assert_eq!(stats.live_blocks, 0);
        assert_eq!(stats.live_bytes, 0);
    }
}
