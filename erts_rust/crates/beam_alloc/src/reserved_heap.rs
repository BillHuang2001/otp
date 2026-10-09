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

//! The reservation-capable per-process term heap, [`ReservedHeap`].
//!
//! [`Heap`](crate::heap::Heap) is a *fixed-capacity* bump arena: it can never grow,
//! because growing it would invalidate every term pointer held in registers or on the
//! Erlang stack, so the runtime instead grows a heap by *replacement* (the collector
//! allocates a bigger one and copies into it). [`ReservedHeap`] keeps that same
//! pointer-stability guarantee while making growth the common case: it **reserves** a
//! large address range up front (the carriers come from `mmap` via [`crate::mseg`], so
//! reserved pages fault lazily and reserving address space does not commit RSS),
//! commits only an initial slice of it, and can **raise the committed limit in place,
//! with no move** — until the whole reservation is exhausted, at which point the caller
//! falls back to a real move ([`relocate`](ReservedHeap::relocate)) or to the existing
//! growth-by-replacement path.
//!
//! # Memory model
//!
//! One backing block of `reserved_words` term words, obtained exactly once from the
//! [`AllocRef`] and released exactly once in [`Drop`]. `committed_words` is the part of
//! it that is currently handed out to the bump allocator; `used` is how much of that has
//! been consumed. So `hp() == base() + used()` and `limit() == base() + committed_words()`
//! — the bump allocator may hand out words in `[hp, limit)` and nothing beyond, even
//! though the reservation extends to `reservation_end() == base() + reserved_words()`.
//!
//! # The host-adoption contract
//!
//! A host (the engine/`beam_rt` glue that owns the heap and its root set) drives
//! growth like this:
//!
//! 1. **Fast path — in-place growth.** When a term construction exhausts the committed
//!    region (a [`BeamError::Alloc`] carrying
//!    [`AllocKind::Heap`](beam_common::AllocKind)), the host calls
//!    [`try_grow`](ReservedHeap::try_grow). A
//!    [`GrowOutcome::GrewInPlace`] means the committed limit now covers the request:
//!    retry the allocation. **The bump pointer did not move**, so a cached `hp()` stays
//!    valid — but a cached [`limit`](ReservedHeap::limit) **must be re-fetched**, since
//!    that is exactly what moved. The host owns the growth *policy*
//!    (how much to ask for); `try_grow` only executes it, with no heuristics of its own.
//! 2. **Slow path — a move is required.** A [`GrowOutcome::MoveRequired`] means the
//!    request does not fit inside the reservation, and `try_grow` changed **nothing**.
//!    The host must supply more address space: either [`relocate`](ReservedHeap::relocate)
//!    to a larger reservation, or a fresh, larger `ReservedHeap` plus a pointer-aware copy.
//! 3. **After any move, every cached raw term pointer is invalid.** The move shifts the
//!    whole live block by a **constant delta** (returned by
//!    [`relocate`](ReservedHeap::relocate)), so the caller re-bases by adding that delta
//!    to (a) its external roots and (b) the pointer words *inside* the moved region. The
//!    root set is the caller's live-word slice — `x_regs[0..live]` followed by the Erlang
//!    stack, the same shape the collector takes (`beam_engine`'s
//!    `Machine::{x_mut, fr_mut, stack_mut}` supplies it).
//! 4. **`relocate` cannot fix interior pointers itself.** Distinguishing a pointer word
//!    from raw payload bytes (bitstring or refc-binary data) needs term-structure
//!    knowledge that belongs to `beam_term`/`beam_gc`, not to a bump arena. So the
//!    sanctioned fully-consistent move is the existing pointer-aware copy (the
//!    collector's `copy_struct`/Cheney scan) — growth by *replacement*, which is what
//!    this crate has always documented for [`Heap`](crate::heap::Heap). `relocate` is
//!    the cheap constant-delta primitive for a host that already knows its root set.
//! 5. **Never cache a raw term pointer across a relocation.** `base()`, `hp()` and any
//!    term derived from them must be re-read afterwards; only the returned delta is a
//!    valid carry-over.
//!
//! Exhaustion, like [`Heap`](crate::heap::Heap)'s, is a normal condition: a request
//! that does not fit in the committed region is reported as
//! [`BeamError::Alloc`](beam_common::BeamError::Alloc) with
//! [`AllocKind::Heap`](beam_common::AllocKind) (collect/grow and retry), never a panic.
//!
//! `unsafe` is confined to this module's marked functions.

use core::alloc::Layout;
use core::mem::size_of;
use core::ptr::NonNull;

use beam_common::{BeamError, Result};
use beam_term::prelude::Eterm;

use crate::allocator::{AllocRef, MIN_ALIGN};
use crate::bump;
use crate::registry::default_allocator;

/// What [`ReservedHeap::try_grow`] did with a request for more bump room.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GrowOutcome {
    /// The request is covered: either the committed limit already reached it, or the
    /// limit was raised in place. The bump pointer did **not** move, the backing block
    /// did not change, and the host can simply retry its allocation.
    GrewInPlace,
    /// The request does not fit inside the reservation. Nothing changed at all; the
    /// host must supply more address space (see the module docs' host-adoption
    /// contract) before retrying.
    MoveRequired,
}

/// A per-process term heap that reserves a large address range up front and grows by
/// raising its committed limit **in place** until the reservation is exhausted.
///
/// See the module documentation for the memory model and the host-adoption contract.
/// The geometry mirrors [`Heap`](crate::heap::Heap): one contiguous backing block that is
/// never grown or reallocated in place, so raising the committed limit always keeps every
/// handed-out pointer valid. (A [`relocate`](ReservedHeap::relocate) is a *move* — a
/// separate, larger block — and invalidates them, exactly like the collector's
/// growth-by-replacement.)
pub struct ReservedHeap {
    alloc: AllocRef,
    base: *mut Eterm,
    /// The reservation's size in term words ([`reserved_words`](ReservedHeap::reserved_words)).
    reserved: usize,
    /// How much of the reservation may currently be handed out
    /// ([`committed_words`](ReservedHeap::committed_words)).
    committed: usize,
    /// Words handed out so far ([`used`](ReservedHeap::used)).
    used: usize,
    /// High-water mark of `used` since the last [`reset`](ReservedHeap::reset).
    peak: usize,
}

/// The byte size of a `reserved_words`-word reservation.
///
/// [`BeamError::BadArg`] on a `reserved_words * size_of::<Eterm>()` overflow: the
/// reservation is never silently wrapped or saturated.
fn reservation_bytes(reserved_words: usize) -> Result<usize> {
    reserved_words
        .checked_mul(size_of::<Eterm>())
        .ok_or(BeamError::BadArg)
}

impl ReservedHeap {
    /// A new heap reserving `reserved_words` term words and committing the first
    /// `initial_words` of them, backed by the default allocator.
    ///
    /// [`BeamError::BadArg`] if `initial_words > reserved_words` or if the reservation's
    /// byte size overflows. `reserved_words == 0` is valid (a heap with no address
    /// space at all, which can never hand out a word).
    pub fn new(reserved_words: usize, initial_words: usize) -> Result<ReservedHeap> {
        ReservedHeap::with_allocator(default_allocator().clone(), reserved_words, initial_words)
    }

    /// A new heap reserving `reserved_words` term words and committing the first
    /// `initial_words` of them, backed by `alloc`.
    ///
    /// Exactly one block of `reserved_words * size_of::<Eterm>()` bytes is obtained from
    /// `alloc` and released in [`Drop`]. [`BeamError::BadArg`] if
    /// `initial_words > reserved_words` or if the byte size overflows.
    pub fn with_allocator(
        alloc: AllocRef,
        reserved_words: usize,
        initial_words: usize,
    ) -> Result<ReservedHeap> {
        if initial_words > reserved_words {
            return Err(BeamError::BadArg);
        }
        let bytes = reservation_bytes(reserved_words)?;
        let block = alloc.alloc_bytes(bytes)?;
        Ok(ReservedHeap {
            alloc,
            base: block.as_ptr().cast::<Eterm>(),
            reserved: reserved_words,
            committed: initial_words,
            used: 0,
            peak: 0,
        })
    }

    /// Ensures at least `need_words` further words of bump room.
    ///
    /// * Already covered (`need_words <= available()`) ⇒
    ///   [`GrewInPlace`](GrowOutcome::GrewInPlace) with **no state change** (idempotent).
    /// * Within the reservation but past the committed limit ⇒ the committed limit is
    ///   raised to exactly `used() + need_words` (no growth heuristic — the host owns the
    ///   policy) and [`GrewInPlace`](GrowOutcome::GrewInPlace) is returned. The bump
    ///   pointer is byte-identical before and after, so a cached `hp()` stays valid.
    /// * Beyond the reservation ⇒ [`MoveRequired`](GrowOutcome::MoveRequired) with **no
    ///   state change at all**.
    ///
    /// The `Result` is part of the crate's return-based error contract; this operation
    /// currently cannot fail.
    pub fn try_grow(&mut self, need_words: usize) -> Result<GrowOutcome> {
        if need_words <= self.available() {
            return Ok(GrowOutcome::GrewInPlace);
        }
        // `reserved >= used` is an invariant, so this subtraction cannot underflow; it is
        // taken before the comparison so nothing here can overflow either.
        if need_words > self.reserved - self.used {
            return Ok(GrowOutcome::MoveRequired);
        }
        self.committed = self.used + need_words;
        Ok(GrowOutcome::GrewInPlace)
    }

    /// Moves the whole heap into a **new** reservation of `new_reserved_words` words and
    /// returns the byte delta the live block moved by (`new_base - old_base`).
    ///
    /// The `used()` live words are copied verbatim into the new block, the new block
    /// becomes the heap's backing block (with `committed_words == new_reserved_words` and
    /// `peak_used` preserved), and the old block is released. Allocation happens *first*,
    /// so on failure ([`BeamError::BadArg`] when `new_reserved_words < used()` or the
    /// byte size overflows, or the allocator's typed error) the heap is left **completely
    /// unchanged** — the old block is still live and every pointer into it is still valid.
    ///
    /// # Pointer consequences (read this before calling)
    ///
    /// After a successful call **every** raw term pointer into this heap is invalid.
    /// Because the move is a rigid shift by the returned delta, the caller re-bases by
    /// adding that delta to its external roots *and* to the pointer words inside the
    /// moved region (see the module docs' host-adoption contract, points 3–5).
    /// `relocate` does **not** and **cannot** fix interior pointers itself: telling a
    /// pointer word from raw payload bytes (bitstring/refc-binary data) requires
    /// term-structure knowledge owned by `beam_term`/`beam_gc`. A fully-consistent move
    /// must therefore go through the existing pointer-aware copy (the collector's
    /// `copy_struct`/Cheney scan) — growth by *replacement*.
    pub fn relocate(&mut self, new_reserved_words: usize) -> Result<isize> {
        if new_reserved_words < self.used {
            return Err(BeamError::BadArg);
        }
        let new_bytes = reservation_bytes(new_reserved_words)?;
        // Allocate the replacement FIRST: nothing is mutated before this succeeds, so a
        // failure leaves the heap — and every pointer into it — fully valid.
        let block = self.alloc.alloc_bytes(new_bytes)?;
        let new_base = block.as_ptr().cast::<Eterm>();

        if self.used > 0 {
            // SAFETY: `base` is the start of this heap's live `reserved`-word block and
            // `new_base` the start of a fresh, disjoint `new_reserved_words >= used`-word
            // block, so the regions cannot overlap and `used` words are in-bounds in both.
            unsafe { core::ptr::copy_nonoverlapping(self.base, new_base, self.used) };
        }

        let old_base = self.base;
        let old_reserved = self.reserved;
        self.base = new_base;
        self.reserved = new_reserved_words;
        self.committed = new_reserved_words;
        // `peak` is deliberately preserved: it is a property of the live-word history, not
        // of the block the words currently live in.

        self.free_block(old_base, old_reserved);

        // Both addresses are non-null pointers into the same address space, so their true
        // difference always fits `isize`; `wrapping_sub` states that without a debug-only
        // overflow panic path.
        Ok((new_base as isize).wrapping_sub(old_base as isize))
    }

    /// The start of the reservation.
    pub fn base(&self) -> *mut Eterm {
        self.base
    }

    /// The current allocation pointer (`hp`). It moves only when words are handed out,
    /// never when the committed limit is raised, and is re-based by
    /// [`relocate`](ReservedHeap::relocate).
    pub fn hp(&self) -> *mut Eterm {
        // SAFETY: `used <= committed <= reserved`, so the displacement is in-bounds
        // (or exactly one-past-the-end).
        unsafe { self.base.add(self.used) }
    }

    /// The end of the committed region (`base + committed_words`). The bump allocator
    /// may hand out words up to, but not past, this pointer.
    ///
    /// A host that caches this value **must re-read it after a
    /// [`try_grow`](ReservedHeap::try_grow)**: raising the committed limit is what moves
    /// it (unlike `hp()`, which does not move).
    pub fn limit(&self) -> *mut Eterm {
        // SAFETY: `committed <= reserved`.
        unsafe { self.base.add(self.committed) }
    }

    /// One past the last word of the reservation (`base + reserved_words`); the furthest
    /// [`try_grow`](ReservedHeap::try_grow) can reach in place.
    pub fn reservation_end(&self) -> *mut Eterm {
        // SAFETY: `reserved` is the reservation's word count, so this is exactly
        // one-past-the-end.
        unsafe { self.base.add(self.reserved) }
    }

    /// The reservation's size in term words.
    pub fn reserved_words(&self) -> usize {
        self.reserved
    }

    /// How much of the reservation may currently be handed out. Raised by
    /// [`try_grow`](ReservedHeap::try_grow), reset to the whole reservation by
    /// [`relocate`](ReservedHeap::relocate).
    pub fn committed_words(&self) -> usize {
        self.committed
    }

    /// The words usable right now — [`committed_words`](ReservedHeap::committed_words),
    /// the same meaning [`Heap::capacity`](crate::heap::Heap::capacity) has.
    pub fn capacity(&self) -> usize {
        self.committed
    }

    /// Words handed out so far.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Words still available inside the committed region.
    pub fn available(&self) -> usize {
        self.committed - self.used
    }

    /// High-water mark of [`used`](ReservedHeap::used) since the last
    /// [`reset`](ReservedHeap::reset).
    pub fn peak_used(&self) -> usize {
        self.peak
    }

    /// The allocator backing this heap.
    pub fn allocator(&self) -> &AllocRef {
        &self.alloc
    }

    /// Rewinds the bump pointer. This invalidates **all** previously handed-out
    /// pointers (the GC calls it only after copying live terms out), exactly as
    /// [`Heap::reset`](crate::heap::Heap::reset) does. The reservation, the committed
    /// limit and the peak are untouched.
    pub fn reset(&mut self) {
        self.used = 0;
    }

    /// Allocates `n` contiguous, **uninitialised** words out of the committed region.
    ///
    /// # Safety
    ///
    /// The returned words are uninitialised; the caller must write them before they are
    /// read, and must not use them after this heap is dropped or after a
    /// [`relocate`](ReservedHeap::relocate).
    pub unsafe fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        // SAFETY: `hp()` points into this exclusively-owned reservation with
        // `available()` writable words ahead of it inside the committed region, which is
        // `bump::alloc_words_at`'s contract.
        let p = unsafe { bump::alloc_words_at(self.hp(), self.available(), n)? };
        self.used += n;
        self.peak = self.peak.max(self.used);
        Ok(p)
    }

    /// Allocates a tuple of arity `n`: writes the arityval header and returns the header
    /// pointer, with `n` uninitialised element words following it.
    ///
    /// # Safety
    ///
    /// The `n` element words are uninitialised; the caller must write them before they
    /// are read, and must not use the pointer after this heap is dropped or after a
    /// [`relocate`](ReservedHeap::relocate).
    pub unsafe fn alloc_tuple(&mut self, n: usize) -> Result<*mut Eterm> {
        // SAFETY: as for `alloc_words`; `alloc_tuple_at` writes the header word itself and
        // leaves the `n` element words to the caller.
        let p = unsafe { bump::alloc_tuple_at(self.hp(), self.available(), n)? };
        self.used += n + 1;
        self.peak = self.peak.max(self.used);
        Ok(p)
    }

    /// Builds a cons cell `[car, cdr]` and returns the list term.
    pub fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm> {
        // SAFETY: `hp()` has `available()` writable words ahead of it inside the committed
        // region of this exclusively-owned reservation, which is `bump::cons_at`'s contract.
        let term = unsafe { bump::cons_at(self.hp(), self.available(), car, cdr)? };
        self.used += 2;
        self.peak = self.peak.max(self.used);
        Ok(term)
    }

    /// Builds a tuple from `elems` and returns the tuple term.
    pub fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        // SAFETY: `hp()` has `available()` writable words ahead of it inside the committed
        // region of this exclusively-owned reservation, which is `bump::tuple_at`'s contract.
        let term = unsafe { bump::tuple_at(self.hp(), self.available(), elems)? };
        self.used += 1 + elems.len();
        self.peak = self.peak.max(self.used);
        Ok(term)
    }

    /// Frees a `reserved_words`-word block at `base` through this heap's allocator.
    ///
    /// The size and alignment were validated by [`reservation_bytes`] when the block was
    /// allocated, so the layout always reconstructs here; the impossible failure is
    /// logged rather than panicked on ([`Drop`] must not panic, and there is nothing
    /// sensible to do about it anyway).
    fn free_block(&self, base: *mut Eterm, reserved_words: usize) {
        let bytes = match reservation_bytes(reserved_words) {
            Ok(bytes) => bytes,
            Err(_) => {
                beam_common::beam_debug!("beam_alloc: reserved heap block size overflowed on free");
                return;
            }
        };
        let layout = match Layout::from_size_align(bytes, MIN_ALIGN) {
            Ok(layout) => layout,
            Err(_) => {
                beam_common::beam_debug!("beam_alloc: reserved heap block layout invalid on free");
                return;
            }
        };
        let ptr = match NonNull::new(base.cast::<u8>()) {
            Some(ptr) => ptr,
            // A null base means nothing was handed out (zero-word reservation).
            None => return,
        };
        // SAFETY: `base` came from `self.alloc.alloc_bytes(bytes)` with this exact size and
        // alignment (in the constructor or a previous `relocate`), is no longer referenced
        // by `self`, and is freed exactly once.
        unsafe { self.alloc.dealloc(ptr, layout) };
    }
}

impl Drop for ReservedHeap {
    fn drop(&mut self) {
        // The one block obtained at construction (or by the last `relocate`) is released
        // here, with its exact size and alignment.
        self.free_block(self.base, self.reserved);
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for [`ReservedHeap`]'s geometry and growth arithmetic.
    //!
    //! They drive the **real** default allocator (mseg/carrier-backed) rather than a
    //! recording double: the crate keeps exactly one allocator double per test binary
    //! (`tests/heap.rs`, `tests/reserved_heap.rs`), so the in-module tests stay on the
    //! production path — which is itself worth exercising (the zero-size rule, the
    //! committed-slice reservation, the in-place growth arithmetic).
    //!
    //! `unsafe` below only dereferences pointers these tests have just allocated and
    //! written themselves.

    use core::mem::size_of;

    use beam_common::BeamError;
    use beam_term::prelude::{
        car, cdr, make_small, make_tuple, tuple_element, tuple_size, Eterm, NIL,
    };

    use super::*;

    const WORD: usize = size_of::<Eterm>();

    /// Re-bases a **boxed** term by the constant delta a relocation reports. Adding the
    /// delta to an *immediate* would be wrong — immediates carry no address — so this is
    /// only ever used on pointer-tagged terms.
    fn rebase(term: Eterm, delta: isize) -> Eterm {
        Eterm::from_raw(term.raw().wrapping_add(delta as usize))
    }

    #[test]
    fn geometry_tracks_the_committed_slice_of_the_reservation() {
        let mut heap = ReservedHeap::new(64, 4).expect("a real reservation is served");

        assert_eq!(heap.reserved_words(), 64);
        assert_eq!(heap.committed_words(), 4);
        assert_eq!(heap.capacity(), 4);
        assert_eq!(heap.used(), 0);
        assert_eq!(heap.available(), 4);
        assert_eq!(heap.peak_used(), 0);

        assert_eq!(heap.hp(), heap.base());
        assert_eq!(heap.limit() as usize - heap.base() as usize, 4 * WORD);
        assert_eq!(
            heap.reservation_end() as usize - heap.base() as usize,
            64 * WORD
        );

        // SAFETY: one exclusively-owned word, written and read back.
        let p = unsafe { heap.alloc_words(1) }.expect("1 of 4 committed words fits");
        // SAFETY: `p` owns the word just handed out.
        unsafe { *p = make_small(3) };
        assert_eq!(p, heap.base());
        assert_eq!(heap.hp() as usize - heap.base() as usize, WORD);
        // SAFETY: `p` is a live word initialised above.
        unsafe { assert_eq!(*p, make_small(3)) };
    }

    #[test]
    fn grows_in_place_up_to_the_reservation_then_asks_for_a_move() {
        let mut heap = ReservedHeap::new(16, 2).expect("a real reservation is served");
        // SAFETY: one exclusively-owned word.
        unsafe { heap.alloc_words(1) }.expect("1 of 2 committed words fits");
        assert_eq!(heap.used(), 1);

        // `used + need == reserved`: exactly the furthest in-place growth there is.
        let hp_before = heap.hp();
        let base_before = heap.base();
        assert_eq!(heap.try_grow(15).unwrap(), GrowOutcome::GrewInPlace);
        assert_eq!(heap.committed_words(), 16);
        assert_eq!(heap.capacity(), 16);
        assert_eq!(heap.hp(), hp_before, "in-place growth never moves hp");
        assert_eq!(heap.base(), base_before, "... nor the base");
        assert_eq!(heap.limit() as usize - base_before as usize, 16 * WORD);

        // Already covered: idempotent, no state change.
        assert_eq!(heap.try_grow(15).unwrap(), GrowOutcome::GrewInPlace);
        assert_eq!(heap.committed_words(), 16);

        // SAFETY: the reservation is now fully committed.
        unsafe { heap.alloc_words(15) }.expect("the raised limit covers the request");
        assert_eq!(heap.used(), 16);
        assert_eq!(heap.available(), 0);

        // One more word than the reservation holds.
        let hp_full = heap.hp();
        assert_eq!(heap.try_grow(1).unwrap(), GrowOutcome::MoveRequired);
        assert_eq!(heap.used(), 16, "a required move changes nothing");
        assert_eq!(heap.committed_words(), 16);
        assert_eq!(heap.hp(), hp_full);
    }

    #[test]
    fn try_grow_inside_the_committed_region_is_a_no_op() {
        let mut heap = ReservedHeap::new(32, 8).expect("a real reservation is served");
        let (committed, hp, used, reserve_end) = (
            heap.committed_words(),
            heap.hp(),
            heap.used(),
            heap.reservation_end(),
        );

        assert_eq!(heap.try_grow(8).unwrap(), GrowOutcome::GrewInPlace);
        assert_eq!(heap.try_grow(0).unwrap(), GrowOutcome::GrewInPlace);

        assert_eq!(heap.committed_words(), committed);
        assert_eq!(heap.hp(), hp);
        assert_eq!(heap.used(), used);
        assert_eq!(heap.reservation_end(), reserve_end);
    }

    #[test]
    fn relocate_shifts_the_live_words_by_a_constant_delta() {
        let mut heap = ReservedHeap::new(64, 8).expect("a real reservation is served");

        // Two lists (2 words each) and a 2-tuple (3 words) over them: `tup`'s element 1 is
        // the pointer *word* of `tail`, i.e. an interior pointer inside the moved region.
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
        // SAFETY: `tup` is a live 2-arity heap tuple whose two elements are the list
        // terms built just above.
        unsafe {
            assert_eq!(tuple_size(tup), 2);
            assert_eq!(tuple_element(tup, 0), head);
            assert_eq!(tuple_element(tup, 1), tail);
        }

        let old_base = heap.base();
        let old_hp = heap.hp();

        let delta = heap.relocate(256).expect("the default allocator serves it");

        assert_eq!(heap.reserved_words(), 256);
        assert_eq!(heap.committed_words(), 256);
        assert_eq!(heap.capacity(), 256);
        assert_eq!(heap.used(), 7, "relocate preserves the live word count");
        assert_eq!(heap.peak_used(), 7);
        assert_eq!(
            heap.hp() as usize,
            (old_hp as usize).wrapping_add(delta as usize),
            "the new hp is the old hp plus the delta"
        );
        assert_eq!(
            heap.reservation_end() as usize - heap.base() as usize,
            256 * WORD
        );
        assert_eq!(
            heap.base() as usize,
            (old_base as usize).wrapping_add(delta as usize)
        );

        // The tuple's header sits two cons cells (4 words) into the block; its element
        // words are interior pointer words (the two list terms).
        // SAFETY: the live region holds the 7 initialised words.
        unsafe {
            let new_tup = make_tuple(heap.base().add(4));
            let want_head = rebase(head, delta);
            let want_tail = rebase(tail, delta);
            assert_eq!(tuple_size(new_tup), 2);
            // The copy is verbatim — the interior pointer words still hold the OLD
            // addresses, which is exactly why `relocate` cannot fix them itself...
            assert_eq!(tuple_element(new_tup, 0), head);
            assert_eq!(tuple_element(new_tup, 1), tail);
            // ... and re-basing them by the constant delta names the relocated sub-terms.
            assert_eq!(rebase(tuple_element(new_tup, 0), delta), want_head);
            assert_eq!(rebase(tuple_element(new_tup, 1), delta), want_tail);
            assert_eq!(car(want_head), make_small(1));
            assert_eq!(
                cdr(want_head),
                tail,
                "the cdr word was copied, not re-based"
            );
            assert_eq!(cdr(want_tail), NIL);
        }

        // The heap is fully usable in its new reservation, in place from here on.
        let hp_before = heap.hp();
        assert_eq!(heap.try_grow(1).unwrap(), GrowOutcome::GrewInPlace);
        assert_eq!(heap.hp(), hp_before);
        // SAFETY: one exclusively-owned word in the new reservation.
        let p = unsafe { heap.alloc_words(1) }.expect("the new reservation has room");
        // SAFETY: `p` owns the word just handed out.
        unsafe { *p = make_small(99) };
        assert_eq!(heap.used(), 8);
    }

    #[test]
    fn failed_relocate_leaves_the_heap_valid() {
        let mut heap = ReservedHeap::new(16, 4).expect("a real reservation is served");
        let list = heap
            .cons(make_small(7), NIL)
            .expect("2 of 4 committed words fit");
        let (base, hp, used, committed, reserved) = (
            heap.base(),
            heap.hp(),
            heap.used(),
            heap.committed_words(),
            heap.reserved_words(),
        );

        // Fewer words than are live.
        assert!(matches!(heap.relocate(1), Err(BeamError::BadArg)));
        // A reservation whose byte size cannot be represented.
        assert!(matches!(heap.relocate(usize::MAX), Err(BeamError::BadArg)));

        assert_eq!(heap.base(), base, "a failed move changes nothing");
        assert_eq!(heap.hp(), hp);
        assert_eq!(heap.used(), used);
        assert_eq!(heap.committed_words(), committed);
        assert_eq!(heap.reserved_words(), reserved);
        // SAFETY: `list` is still live in the unchanged reservation.
        unsafe { assert_eq!(car(list), make_small(7)) };
    }

    #[test]
    fn relocate_to_exactly_the_live_words_still_reads_back() {
        let mut heap = ReservedHeap::new(16, 8).expect("a real reservation is served");
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

        // SAFETY: the moved cons cells are live; the copy is verbatim, so the interior
        // pointer word still holds the old address and the caller re-bases it by the delta.
        unsafe {
            let new_head = rebase(head, delta);
            let new_tail = rebase(tail, delta);
            assert_eq!(car(new_head), make_small(1));
            assert_eq!(cdr(new_head), tail, "verbatim interior pointer");
            assert_eq!(rebase(cdr(new_head), delta), new_tail);
            assert_eq!(car(new_tail), make_small(2));
        }
    }

    #[test]
    fn reset_rewinds_only_the_bump_pointer() {
        let mut heap = ReservedHeap::new(16, 8).expect("a real reservation is served");
        // SAFETY: 4 exclusively-owned words, the first written below.
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

        // The same words are handed out again, uninitialised but unmoved.
        // SAFETY: `p2` aliases `p`; its words were initialised above and are still inside
        // the live block.
        unsafe {
            let p2 = heap.alloc_words(4).expect("4 of 8 committed words fit");
            assert_eq!(p2, p);
            assert_eq!(*p, make_small(11));
        }
    }

    #[test]
    fn zero_reservation_and_invalid_construction() {
        let mut zero = ReservedHeap::new(0, 0).expect("a zero-word reservation is valid");
        assert_eq!(zero.reserved_words(), 0);
        assert_eq!(zero.committed_words(), 0);
        assert_eq!(zero.capacity(), 0);
        assert_eq!(zero.available(), 0);
        assert_eq!(zero.hp(), zero.base());
        assert_eq!(zero.limit(), zero.base());
        assert_eq!(zero.reservation_end(), zero.base());

        // No address space at all: every request is a typed heap failure, never a panic.
        assert!(matches!(
            unsafe { zero.alloc_words(1) },
            Err(BeamError::Alloc(_))
        ));
        assert!(matches!(
            unsafe { zero.alloc_tuple(0) },
            Err(BeamError::Alloc(_))
        ));
        assert!(matches!(
            zero.cons(make_small(1), NIL),
            Err(BeamError::Alloc(_))
        ));
        assert!(matches!(
            zero.tuple(&[make_small(1)]),
            Err(BeamError::Alloc(_))
        ));
        // Zero words always fit, and consume nothing.
        let base = zero.base();
        // SAFETY: a zero-word request hands out no words at all.
        assert_eq!(unsafe { zero.alloc_words(0) }.unwrap(), base);
        assert_eq!(zero.used(), 0);
        assert_eq!(zero.peak_used(), 0);
        // An empty reservation cannot grow in place.
        assert_eq!(zero.try_grow(1).unwrap(), GrowOutcome::MoveRequired);

        assert!(matches!(ReservedHeap::new(0, 1), Err(BeamError::BadArg)));
        assert!(matches!(ReservedHeap::new(4, 5), Err(BeamError::BadArg)));
        assert!(matches!(
            ReservedHeap::new(usize::MAX, 0),
            Err(BeamError::BadArg)
        ));
    }
}
