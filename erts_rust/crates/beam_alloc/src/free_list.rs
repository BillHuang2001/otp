//! Segregated free lists: the per-size-class intrusive lists of free blocks.
//!
//! Implementation module (public for testing/reuse, **not** part of the cross-crate
//! seam). Classic segregated/slab fit: each multi-block carrier is dedicated to one
//! size class and its free blocks are threaded onto that class's list using an
//! intrusive next-pointer stored in the block payload itself.
//!
//! `unsafe` is confined to this module's marked functions.

use beam_common::Result;

/// The per-size-class set of free-block lists.
#[derive(Default)]
pub struct FreeList {
    /// Intrusive head of each class's free list (`null` when empty).
    heads: Vec<*mut u8>,
    /// Number of free blocks on each class's list.
    counts: Vec<usize>,
}

// SAFETY: the heads point into carrier memory owned by the allocator; the structure
// is only ever mutated through `&mut self` (in practice while holding the
// allocator's mutex), so moving it between threads is sound. `Sync` is deliberately
// not implemented.
unsafe impl Send for FreeList {}

impl FreeList {
    /// A new, empty free-list set.
    pub fn new() -> FreeList {
        FreeList::default()
    }

    /// The number of size classes tracked.
    pub fn class_count(&self) -> usize {
        self.heads.len()
    }

    /// The number of free blocks currently on `class`'s list (0 for an out-of-range
    /// class).
    pub fn free_blocks(&self, class: usize) -> usize {
        self.counts.get(class).copied().unwrap_or(0)
    }

    /// Ensures `class` has room to track `count` more free blocks.
    pub(crate) fn reserve(&mut self, class: usize, count: usize) -> Result<()> {
        let _ = (class, count);
        unimplemented!("FreeList::reserve: slab growth (allocator implementation pass)")
    }

    /// Pops a free block off `class`'s list, or `None` when it is empty.
    ///
    /// # Safety
    ///
    /// `class` must be a valid class index; the returned block must not be otherwise
    /// reachable, and its intrusive link must be removed by this call.
    pub(crate) unsafe fn pop(&mut self, class: usize) -> Option<*mut u8> {
        let _ = class;
        unimplemented!("FreeList::pop: intrusive pop (allocator implementation pass)")
    }

    /// Pushes `ptr` onto `class`'s free list.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a `UNIT_SIZE`-aligned free block large enough to hold the
    /// intrusive next-pointer, and must not be otherwise reachable.
    pub(crate) unsafe fn push(&mut self, class: usize, ptr: *mut u8) {
        let _ = (class, ptr);
        unimplemented!("FreeList::push: intrusive push (allocator implementation pass)")
    }
}
