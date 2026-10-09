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

//! Crate-level integration tests for [`beam_alloc::ReservedHeap`].
//!
//! These exercise the heap through its **public** API only (`ReservedHeap`,
//! `GrowOutcome`, the frozen [`Allocator`] seam, `AllocStats`) plus the frozen term
//! inspection helpers of `beam_term`, the way a dependent crate (`beam_engine`,
//! `beam_rt`) will use it.
//!
//! Like `tests/heap.rs`, the heap is defined over the [`Allocator`] seam, so these
//! tests drive it with a recording `std::alloc`-backed allocator double
//! ([`TestAllocator`]) instead of the carrier allocator: growth-in-place ("no extra
//! allocation"), the move (`one block freed, one live`) and `Drop` ("everything
//! returned") are then exact. One test also drives the **real** default allocator so
//! the production (mseg/carrier) path is covered too.
//!
//! `unsafe` below only dereferences pointers that this test allocated and wrote itself.

use std::alloc::{alloc as std_alloc, dealloc as std_dealloc, realloc as std_realloc, Layout};
use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_alloc::{AllocRef, AllocStats, Allocator, GrowOutcome, ReservedHeap};
use beam_common::{AllocFailure, AllocKind, BeamError};
use beam_term::prelude::{
    car, cdr, is_tuple, make_small, make_tuple, signed_value, tuple_element, tuple_size, Eterm, NIL,
};

/// Byte size of one term word.
const WORD: usize = size_of::<Eterm>();

/// A recording allocator over `std::alloc`, used only by these tests.
///
/// It satisfies the [`Allocator`] contract (`Send + Sync + 'static`, return-based
/// failure, no panic), counts live blocks/bytes so growth and `Drop` can be asserted
/// exactly, and implements the zero-size rule: a zero-sized request yields the
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
            // `std::alloc::realloc` requires both sizes to be non-zero. The reserved heap
            // never reallocates, so this double refuses the unsupported corner instead of
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

/// A heap reserving `reserved` words and committing the first `initial` of them, over a
/// fresh recording allocator.
fn new_heap(reserved: usize, initial: usize) -> (Arc<TestAllocator>, ReservedHeap) {
    let alloc = TestAllocator::new();
    let heap = ReservedHeap::with_allocator(alloc_ref(&alloc), reserved, initial)
        .expect("the test allocator never fails");
    (alloc, heap)
}

/// Re-bases a **boxed** term by the constant delta a relocation reports. Adding the
/// delta to an *immediate* would be wrong — immediates carry no address — so this is
/// only ever used on pointer-tagged terms.
fn rebase(term: Eterm, delta: isize) -> Eterm {
    Eterm::from_raw(term.raw().wrapping_add(delta as usize))
}

/// The typed heap-exhaustion failure for a request the committed region cannot serve.
fn assert_heap_exhausted<T: std::fmt::Debug>(result: beam_common::Result<T>, words: usize) {
    match result {
        Err(BeamError::Alloc(failure)) => {
            assert_eq!(failure.kind, AllocKind::Heap);
            assert_eq!(failure.requested, words * WORD);
        }
        other => panic!("expected a heap allocation failure, got {other:?}"),
    }
}

#[test]
fn grows_in_place_across_the_committed_limit_without_moving_the_bump_pointer() {
    let alloc = TestAllocator::new();
    let mut heap =
        ReservedHeap::with_allocator(alloc_ref(&alloc), 64, 4).expect("construction never fails");

    assert_eq!(heap.reserved_words(), 64);
    assert_eq!(heap.committed_words(), 4);
    assert_eq!(heap.capacity(), 4);
    assert_eq!(heap.available(), 4);
    assert_eq!(heap.peak_used(), 0);
    assert_eq!(alloc.live_blocks(), 1);
    assert_eq!(alloc.live_bytes(), 64 * WORD);

    let start = heap.hp();
    assert_eq!(heap.limit() as usize - start as usize, 4 * WORD);
    assert_eq!(heap.reservation_end() as usize - start as usize, 64 * WORD);

    let mut ptrs: Vec<*mut Eterm> = Vec::new();
    let mut committed = heap.committed_words();
    for i in 0..64_i64 {
        if heap.available() == 0 {
            let (base_before, hp_before) = (heap.base(), heap.hp());
            assert_eq!(heap.try_grow(1).unwrap(), GrowOutcome::GrewInPlace);
            assert_eq!(heap.hp(), hp_before, "in-place growth never moves hp");
            assert_eq!(heap.base(), base_before, "... nor the base");
            assert!(
                heap.committed_words() > committed,
                "the committed limit moved up"
            );
            assert_eq!(heap.capacity(), heap.committed_words());
            assert_eq!(
                heap.limit() as usize - heap.base() as usize,
                heap.committed_words() * WORD,
                "the limit is base plus the committed words"
            );
            committed = heap.committed_words();
        }
        let hp_before = heap.hp();
        // SAFETY: each call hands out one exclusively-owned, writable word.
        let p = unsafe { heap.alloc_words(1) }.expect("the raised limit always covers it");
        assert_eq!(p, hp_before);
        // SAFETY: `p` owns the word just handed out.
        unsafe { *p = make_small(i) };
        ptrs.push(p);
    }

    assert_eq!(heap.used(), 64);
    assert_eq!(heap.available(), 0);
    assert_eq!(heap.committed_words(), 64);
    assert_eq!(heap.peak_used(), 64);
    assert_eq!(heap.hp(), heap.limit());

    // Every earlier pointer still reads back its own value, at its original address.
    for (i, p) in ptrs.iter().enumerate() {
        // SAFETY: initialised in the loop above, inside the one never-reallocated block.
        assert_eq!(unsafe { **p }, make_small(i as i64));
        assert_eq!(*p as usize, start as usize + i * WORD);
    }

    // Growing in place allocated nothing new and never reallocated the block.
    assert_eq!(alloc.live_blocks(), 1);
    assert_eq!(alloc.live_bytes(), 64 * WORD);
    let stats = alloc.stats();
    assert_eq!(stats.alloc_calls, 1);
    assert_eq!(stats.realloc_calls, 0);
    assert_eq!(stats.free_calls, 0);
}

#[test]
fn try_grow_inside_the_committed_region_is_a_no_op() {
    let (_alloc, mut heap) = new_heap(32, 8);
    let (base, hp, used, committed, limit) = (
        heap.base(),
        heap.hp(),
        heap.used(),
        heap.committed_words(),
        heap.limit(),
    );

    assert_eq!(heap.try_grow(8).unwrap(), GrowOutcome::GrewInPlace);
    assert_eq!(heap.try_grow(0).unwrap(), GrowOutcome::GrewInPlace);

    assert_eq!(heap.base(), base);
    assert_eq!(heap.hp(), hp);
    assert_eq!(heap.used(), used);
    assert_eq!(heap.committed_words(), committed);
    assert_eq!(heap.limit(), limit);
}

#[test]
fn reservation_exhaustion_reports_move_required_and_touches_nothing() {
    let alloc = TestAllocator::new();
    let mut heap =
        ReservedHeap::with_allocator(alloc_ref(&alloc), 8, 8).expect("construction never fails");

    let mut ptrs: Vec<*mut Eterm> = Vec::new();
    for i in 0..8_i64 {
        // SAFETY: each call hands out one exclusively-owned, writable word.
        let p = unsafe { heap.alloc_words(1) }.expect("8 of 8 committed words fit");
        // SAFETY: `p` owns the word just handed out.
        unsafe { *p = make_small(i) };
        ptrs.push(p);
    }
    assert_eq!(heap.hp(), heap.limit(), "the committed region is full");

    let (base, hp, used, committed, peak) = (
        heap.base(),
        heap.hp(),
        heap.used(),
        heap.committed_words(),
        heap.peak_used(),
    );

    // The reservation itself is exhausted: the host must move, in place is impossible.
    assert_eq!(heap.try_grow(1).unwrap(), GrowOutcome::MoveRequired);
    assert_eq!(heap.base(), base, "a required move changes nothing");
    assert_eq!(heap.hp(), hp);
    assert_eq!(heap.used(), used);
    assert_eq!(heap.committed_words(), committed);
    assert_eq!(heap.peak_used(), peak);
    assert_eq!(alloc.live_blocks(), 1, "no replacement block was allocated");

    // An over-large request is the typed heap failure with no partial write.
    // SAFETY: the request is rejected up front; no pointer is dereferenced.
    assert_heap_exhausted(unsafe { heap.alloc_words(9) }, 9);
    assert_eq!(heap.used(), used);
    assert_eq!(heap.peak_used(), peak);
    // SAFETY: the tuple header request is rejected up front too.
    assert_heap_exhausted(unsafe { heap.alloc_tuple(0) }, 1);
    assert_heap_exhausted(heap.cons(make_small(1), NIL), 2);
    assert_heap_exhausted(heap.tuple(&[make_small(1), make_small(2)]), 3);
    assert_eq!(heap.used(), used);
    assert_eq!(heap.peak_used(), peak);

    // Still fully usable at the old limit: everything written before reads back...
    for (i, p) in ptrs.iter().enumerate() {
        // SAFETY: `*p` is a live word initialised in the loop above.
        assert_eq!(unsafe { **p }, make_small(i as i64));
    }
    // ... and a zero-word request still succeeds without consuming anything.
    let hp = heap.hp();
    // SAFETY: a zero-word request hands out no words at all.
    assert_eq!(unsafe { heap.alloc_words(0) }.unwrap(), hp);
    assert_eq!(heap.used(), used);
}

#[test]
fn need_words_exactly_hitting_the_reservation_boundary_grows_in_place() {
    let (_alloc, mut heap) = new_heap(16, 2);
    // SAFETY: one exclusively-owned word.
    unsafe { heap.alloc_words(1) }.expect("1 of 2 committed words fit");
    assert_eq!(heap.used(), 1);

    // `used + need == reserved`: exactly the furthest in-place growth possible.
    assert_eq!(heap.try_grow(15).unwrap(), GrowOutcome::GrewInPlace);
    assert_eq!(heap.committed_words(), 16);
    assert_eq!(heap.limit() as usize - heap.base() as usize, 16 * WORD);

    // SAFETY: the whole reservation is now committed.
    unsafe { heap.alloc_words(15) }.expect("the raised limit covers the request");
    assert_eq!(heap.used(), 16);

    // One word beyond the reservation: a move is required, and nothing changed.
    let hp = heap.hp();
    assert_eq!(heap.try_grow(1).unwrap(), GrowOutcome::MoveRequired);
    assert_eq!(heap.used(), 16);
    assert_eq!(heap.committed_words(), 16);
    assert_eq!(heap.hp(), hp);
}

#[test]
fn relocate_moves_the_live_region_by_a_constant_delta() {
    let alloc = TestAllocator::new();
    let mut heap =
        ReservedHeap::with_allocator(alloc_ref(&alloc), 8, 8).expect("construction never fails");

    // `tail` and `head` are two cons cells (2 words each); `tup` is a 2-arity tuple
    // (3 words) whose elements are those list terms, i.e. interior pointer words.
    let tail = heap
        .cons(make_small(2), NIL)
        .expect("2 of 8 committed words fit");
    let head = heap
        .cons(make_small(1), tail)
        .expect("the next 2 of 8 words fit");
    let tup = heap
        .tuple(&[head, tail])
        .expect("the last 3 of 8 words fit");
    assert_eq!(heap.used(), 7);
    // SAFETY: `tup` is a live 2-arity heap tuple.
    unsafe {
        assert!(is_tuple(tup));
        assert_eq!(tuple_size(tup), 2);
        assert_eq!(tuple_element(tup, 0), head);
    }

    // Snapshot the live region before the move.
    let old_base = heap.base();
    let old_hp = heap.hp();
    // SAFETY: `[base, hp)` holds the `used` initialised words of this heap.
    let old_live: Vec<Eterm> =
        unsafe { core::slice::from_raw_parts(heap.base(), heap.used()) }.to_vec();

    let delta = heap.relocate(64).expect("the test allocator never fails");

    assert_eq!(heap.reserved_words(), 64);
    assert_eq!(heap.committed_words(), 64);
    assert_eq!(heap.capacity(), 64);
    assert_eq!(heap.used(), 7, "relocate preserves the live word count");
    assert_eq!(
        heap.peak_used(),
        7,
        "relocate preserves the high-water mark"
    );
    assert_eq!(
        heap.base() as usize,
        (old_base as usize).wrapping_add(delta as usize),
        "the base moved by exactly the returned delta"
    );
    assert_eq!(
        heap.hp() as usize,
        (old_hp as usize).wrapping_add(delta as usize),
        "hp moved by exactly the returned delta"
    );
    assert_eq!(
        heap.reservation_end() as usize - heap.base() as usize,
        64 * WORD
    );

    // The live region is byte-identical at the new addresses...
    // SAFETY: `[new base, new base + 7)` holds the 7 copied words.
    let new_live: &[Eterm] = unsafe { core::slice::from_raw_parts(heap.base(), heap.used()) };
    assert_eq!(new_live, old_live.as_slice());
    // ... and every word's address shifted by exactly `delta`.
    for i in 0..old_live.len() {
        // SAFETY: `i < used`, so `base + i` is in-bounds.
        let addr = unsafe { heap.base().add(i) } as usize;
        let want = (old_base as usize)
            .wrapping_add(i * WORD)
            .wrapping_add(delta as usize);
        assert_eq!(addr, want);
    }

    // The constant-delta contract. The copy is *verbatim*: interior pointer words are not
    // rewritten, so the word stored inside the moved region still holds the OLD address
    // (which is exactly why `relocate` cannot fix interior pointers itself). Re-basing
    // such a word by the delta names the relocated sub-term, at `old term + delta`.
    assert_eq!(
        heap.base() as usize - old_base as usize,
        delta as usize,
        "the move is a rigid shift"
    );
    // SAFETY: `new_tup` is the relocated 2-arity heap tuple; its two element words are
    // the verbatim copies of the two interior pointer words.
    let new_tup = unsafe { make_tuple(heap.base().add(4)) };
    // SAFETY: as above.
    unsafe {
        assert!(is_tuple(new_tup));
        assert_eq!(tuple_size(new_tup), 2);
        assert_eq!(
            tuple_element(new_tup, 0),
            head,
            "verbatim: still the OLD address"
        );
        assert_eq!(
            tuple_element(new_tup, 1),
            tail,
            "verbatim: still the OLD address"
        );
        assert_eq!(*heap.base().add(5), head);

        let new_head = rebase(head, delta);
        let new_tail = rebase(tail, delta);
        // Every relocated sub-term is its old address plus the constant delta...
        assert_eq!(new_head, rebase(tuple_element(new_tup, 0), delta));
        assert_eq!(new_tail, rebase(tuple_element(new_tup, 1), delta));
        // ... and reads back correctly at its new address: the immediates are unchanged,
        // while the interior pointer word must be re-based by the caller before use.
        assert_eq!(car(new_head), make_small(1), "immediates need no re-basing");
        assert_eq!(cdr(new_head), tail, "the cdr word was copied, not re-based");
        assert_eq!(rebase(cdr(new_head), delta), new_tail);
        assert_eq!(car(new_tail), make_small(2));
        assert_eq!(cdr(new_tail), NIL);
    }

    // The old block was freed and exactly one block remains live.
    assert_eq!(alloc.live_blocks(), 1, "the old reservation was freed");
    assert_eq!(alloc.live_bytes(), 64 * WORD);
    let stats = alloc.stats();
    assert_eq!(
        stats.alloc_calls, 2,
        "one reservation, then its replacement"
    );
    assert_eq!(stats.free_calls, 1, "the old block, freed once");
    assert_eq!(stats.realloc_calls, 0, "a move is not a realloc");

    // The heap is fully usable afterwards, growing in place inside the new reservation.
    let hp_before = heap.hp();
    assert_eq!(heap.try_grow(1).unwrap(), GrowOutcome::GrewInPlace);
    assert_eq!(heap.hp(), hp_before);
    // SAFETY: one exclusively-owned word inside the new reservation.
    let p = unsafe { heap.alloc_words(1) }.expect("the new reservation has room");
    // SAFETY: `p` owns the word just handed out.
    unsafe { *p = make_small(99) };
    assert_eq!(heap.used(), 8);
    // SAFETY: `p` is a live word initialised just above.
    assert_eq!(unsafe { signed_value(*p) }, 99);
}

#[test]
fn failed_relocate_leaves_the_heap_unchanged_and_valid() {
    let alloc = TestAllocator::new();
    let mut heap =
        ReservedHeap::with_allocator(alloc_ref(&alloc), 8, 4).expect("construction never fails");
    let list = heap
        .cons(make_small(1), NIL)
        .expect("2 of 4 committed words fit");

    let (base, hp, used, committed, reserved, peak) = (
        heap.base(),
        heap.hp(),
        heap.used(),
        heap.committed_words(),
        heap.reserved_words(),
        heap.peak_used(),
    );

    // Fewer words than are live: BadArg, nothing changes.
    assert!(matches!(heap.relocate(1), Err(BeamError::BadArg)));
    // A reservation whose byte size cannot be represented: BadArg, nothing changes.
    assert!(matches!(heap.relocate(usize::MAX), Err(BeamError::BadArg)));
    // A representable-but-unservable reservation: the allocator's typed error, and still
    // nothing changes (the replacement is allocated before anything is mutated).
    assert!(matches!(
        heap.relocate(1usize << 59),
        Err(BeamError::Alloc(_))
    ));

    assert_eq!(heap.base(), base, "a failed move changes nothing");
    assert_eq!(heap.hp(), hp);
    assert_eq!(heap.used(), used);
    assert_eq!(heap.committed_words(), committed);
    assert_eq!(heap.reserved_words(), reserved);
    assert_eq!(heap.peak_used(), peak);
    assert_eq!(alloc.live_blocks(), 1, "the old (only) block is still live");
    assert_eq!(alloc.stats().free_calls, 0);

    // The heap is still valid and usable, in place.
    // SAFETY: `list` is a live cons cell in the unchanged reservation.
    unsafe { assert_eq!(car(list), make_small(1)) };
    let hp_before = heap.hp();
    assert_eq!(heap.try_grow(1).unwrap(), GrowOutcome::GrewInPlace);
    assert_eq!(heap.hp(), hp_before);
}

#[test]
fn relocate_to_exactly_the_live_words_keeps_the_terms() {
    let alloc = TestAllocator::new();
    let mut heap =
        ReservedHeap::with_allocator(alloc_ref(&alloc), 8, 8).expect("construction never fails");
    let tail = heap
        .cons(make_small(2), NIL)
        .expect("2 of 8 committed words fit");
    let head = heap
        .cons(make_small(1), tail)
        .expect("the next 2 of 8 words fit");
    assert_eq!(heap.used(), 4);

    let old_base = heap.base();
    let delta = heap.relocate(4).expect("four words is a valid reservation");

    assert_eq!(heap.reserved_words(), 4);
    assert_eq!(heap.committed_words(), 4);
    assert_eq!(heap.available(), 0);
    assert_eq!(heap.hp(), heap.limit());
    assert_eq!(
        heap.hp() as usize,
        (old_base as usize)
            .wrapping_add(4 * WORD)
            .wrapping_add(delta as usize)
    );

    // SAFETY: the relocated cons cells are live; the copy is verbatim, so the interior
    // pointer word still holds the old address and the caller re-bases it by the delta.
    unsafe {
        assert_eq!(car(rebase(head, delta)), make_small(1));
        assert_eq!(cdr(rebase(head, delta)), tail, "verbatim interior pointer");
        assert_eq!(rebase(cdr(rebase(head, delta)), delta), rebase(tail, delta));
        assert_eq!(car(rebase(tail, delta)), make_small(2));
        assert_eq!(cdr(rebase(tail, delta)), NIL);
    }
    assert_eq!(alloc.live_blocks(), 1);
}

#[test]
fn zero_word_reservation_behaves() {
    let alloc = TestAllocator::new();
    let mut heap =
        ReservedHeap::with_allocator(alloc_ref(&alloc), 0, 0).expect("a zero reservation is valid");

    assert_eq!(heap.reserved_words(), 0);
    assert_eq!(heap.committed_words(), 0);
    assert_eq!(heap.capacity(), 0);
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.available(), 0);
    assert_eq!(heap.peak_used(), 0);
    assert_eq!(heap.hp(), heap.base());
    assert_eq!(heap.limit(), heap.base());
    assert_eq!(heap.reservation_end(), heap.base());

    // Every request is a typed heap failure, never a panic and never a partial write.
    // SAFETY: the request is rejected up front; no pointer is dereferenced.
    assert_heap_exhausted(unsafe { heap.alloc_words(1) }, 1);
    // SAFETY: the header request is rejected up front too.
    assert_heap_exhausted(unsafe { heap.alloc_tuple(0) }, 1);
    assert_heap_exhausted(heap.cons(make_small(1), NIL), 2);
    assert_heap_exhausted(heap.tuple(&[make_small(1)]), 2);
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.peak_used(), 0);

    // Zero words always fit, and consume nothing.
    let base = heap.base();
    // SAFETY: a zero-word request hands out no words at all.
    assert_eq!(unsafe { heap.alloc_words(0) }.unwrap(), base);
    assert_eq!(heap.used(), 0);

    // An empty reservation cannot grow in place.
    assert_eq!(heap.try_grow(1).unwrap(), GrowOutcome::MoveRequired);
    // ... and moving to a real reservation from an empty one works.
    let delta = heap.relocate(4).expect("the test allocator never fails");
    assert_eq!(heap.reserved_words(), 4);
    assert_eq!(heap.committed_words(), 4);
    assert_eq!(heap.used(), 0);
    assert_eq!(
        heap.base() as usize,
        (base as usize).wrapping_add(delta as usize)
    );

    // The zero-sized block never reached `std::alloc`, so only the new reservation is
    // accounted for; dropping frees it.
    drop(heap);
    assert_eq!(alloc.live_blocks(), 0);
    assert_eq!(alloc.live_bytes(), 0);
    let stats = alloc.stats();
    assert_eq!(stats.alloc_calls, 1);
    assert_eq!(stats.free_calls, 1);
}

#[test]
fn invalid_construction_is_bad_arg() {
    let alloc = TestAllocator::new();

    // The initial committed limit is inside the reservation, or construction fails.
    assert!(matches!(
        ReservedHeap::with_allocator(alloc_ref(&alloc), 0, 1),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        ReservedHeap::with_allocator(alloc_ref(&alloc), 4, 5),
        Err(BeamError::BadArg)
    ));
    // The reservation's byte size must be representable.
    assert!(matches!(
        ReservedHeap::with_allocator(alloc_ref(&alloc), usize::MAX, 0),
        Err(BeamError::BadArg)
    ));

    // Nothing was ever allocated for any of the rejected constructions.
    assert_eq!(alloc.live_blocks(), 0);
    assert_eq!(alloc.stats().alloc_calls, 0);
    assert_eq!(alloc.stats().free_calls, 0);
}

#[test]
fn reset_rewinds_only_the_bump_pointer() {
    let (_alloc, mut heap) = new_heap(16, 8);
    // SAFETY: 4 exclusively-owned words; the first is written below.
    let p = unsafe { heap.alloc_words(4) }.expect("4 of 8 committed words fit");
    // SAFETY: `p` owns the 4 words just handed out.
    unsafe { *p = make_small(11) };
    assert_eq!(heap.used(), 4);
    assert_eq!(heap.peak_used(), 4);

    let (base, committed, reserved, limit) = (
        heap.base(),
        heap.committed_words(),
        heap.reserved_words(),
        heap.limit(),
    );

    heap.reset();

    assert_eq!(heap.used(), 0);
    assert_eq!(heap.available(), committed);
    assert_eq!(heap.peak_used(), 4, "reset keeps the high-water mark");
    assert_eq!(heap.base(), base);
    assert_eq!(heap.committed_words(), committed);
    assert_eq!(heap.reserved_words(), reserved);
    assert_eq!(heap.limit(), limit);
    assert_eq!(heap.hp(), base);

    // The same words are handed out again at the same addresses, unmoved.
    // SAFETY: `p2` aliases `p`, whose words were initialised above and are still inside
    // the live reservation.
    unsafe {
        let p2 = heap.alloc_words(4).expect("4 of 8 committed words fit");
        assert_eq!(p2, p);
        assert_eq!(*p, make_small(11));
    }
    assert_eq!(heap.peak_used(), 4);
}

#[test]
fn drop_returns_the_one_reservation_to_the_allocator() {
    let alloc = TestAllocator::new();
    let words = 24;

    {
        let mut heap =
            ReservedHeap::with_allocator(alloc_ref(&alloc), words, 8).expect("construction");
        assert_eq!(heap.allocator().name(), "test-stdlib-allocator");
        assert_eq!(
            alloc.live_blocks(),
            1,
            "one block for the whole reservation"
        );
        assert_eq!(alloc.live_bytes(), words * WORD);

        // SAFETY: one exclusively-owned word, written and unused afterwards.
        let p = unsafe { heap.alloc_words(1) }.expect("1 of 8 committed words fits");
        // SAFETY: `p` owns the word just handed out.
        unsafe { *p = make_small(1) };

        // Growth in place allocates nothing.
        assert_eq!(heap.try_grow(8).unwrap(), GrowOutcome::GrewInPlace);
        assert_eq!(alloc.live_blocks(), 1);
        assert_eq!(alloc.live_bytes(), words * WORD);
    }

    assert_eq!(alloc.live_blocks(), 0, "Drop frees the one backing block");
    assert_eq!(alloc.live_bytes(), 0);
    let stats = alloc.stats();
    assert_eq!(stats.alloc_calls, 1);
    assert_eq!(stats.free_calls, 1);
    assert_eq!(
        stats.realloc_calls, 0,
        "the reserved heap never reallocates"
    );
    assert_eq!(stats.live_blocks, 0);
    assert_eq!(stats.live_bytes, 0);
}

#[test]
fn real_default_allocator_backs_the_reservation() {
    // The default allocator is mseg/carrier-backed; this exercises the zero-size rule,
    // a real committed-slice reservation, in-place growth, a real move and `Drop` all on
    // the production path.
    let mut zero = ReservedHeap::new(0, 0).expect("a zero-word reservation is valid");
    assert_eq!(zero.reserved_words(), 0);
    assert_eq!(zero.try_grow(1).unwrap(), GrowOutcome::MoveRequired);
    // SAFETY: the request is rejected up front; no pointer is dereferenced.
    assert_heap_exhausted(unsafe { zero.alloc_words(1) }, 1);
    drop(zero);

    let mut heap = ReservedHeap::new(1024, 16).expect("a real reservation is served");
    assert_eq!(heap.reserved_words(), 1024);
    assert_eq!(heap.committed_words(), 16);

    // SAFETY: 4 exclusively-owned words inside the committed slice; the first is written.
    let p = unsafe { heap.alloc_words(4) }.expect("4 of 16 committed words fit");
    // SAFETY: `p` owns the 4 words just handed out.
    unsafe { *p = make_small(42) };
    assert_eq!(unsafe { *p }, make_small(42));

    // Grow inside the reservation, in place: `hp` must not move.
    assert_eq!(heap.try_grow(1000).unwrap(), GrowOutcome::GrewInPlace);
    assert_eq!(heap.committed_words(), 1004);
    assert_eq!(heap.hp(), p.wrapping_add(4));

    // Move to a bigger reservation and read the earlier word back at its new home.
    let delta = heap.relocate(4096).expect("a bigger reservation is served");
    assert_eq!(heap.reserved_words(), 4096);
    assert_eq!(heap.committed_words(), 4096);
    assert_eq!(heap.used(), 4);
    let new_p = (p as usize).wrapping_add(delta as usize) as *mut Eterm;
    // SAFETY: relocation copied the live words verbatim, so the same word is at `new_p`.
    unsafe { assert_eq!(*new_p, make_small(42)) };

    drop(heap);
}
