//! Segregated free lists: the per-size-class intrusive lists of free blocks.
//!
//! Implementation module (public for testing/reuse, **not** part of the cross-crate
//! seam). Classic segregated/slab fit: each multi-block carrier is dedicated to one
//! size class and its free blocks are threaded onto that class's list using an
//! intrusive next-pointer stored in the block payload itself.
//!
//! A list node *is* the block's payload address (what the allocator hands to the
//! caller and what [`push`](FreeList::push)/[`pop`](FreeList::pop) exchange): while
//! a block is free its first payload word holds the link to the next free payload,
//! and the block header directly *below* the payload still records the block's size,
//! class and carrier, so a popped block needs no separate lookup table.
//!
//! `unsafe` is confined to this module's marked functions.

use core::ptr;

use beam_common::{BeamError, Result};

use crate::block::{MAX_CLASS_COUNT, UNIT_SIZE};

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
    ///
    /// An intrusive list needs no backing capacity, so this only enlarges the
    /// per-class bookkeeping to cover `class`; `count` is advisory (kept for the
    /// legacy `reserve`/growth surface) and is not stored. A `class` beyond the
    /// encodable range is rejected with [`BeamError::BadArg`].
    pub(crate) fn reserve(&mut self, class: usize, count: usize) -> Result<()> {
        if class >= MAX_CLASS_COUNT {
            return Err(BeamError::BadArg);
        }
        let _ = count;
        if class >= self.heads.len() {
            self.heads.resize(class + 1, ptr::null_mut());
            self.counts.resize(class + 1, 0);
        }
        Ok(())
    }

    /// Pops a free block off `class`'s list, or `None` when it is empty.
    ///
    /// # Safety
    ///
    /// `class` must be a valid class index; the returned block must not be otherwise
    /// reachable, and its intrusive link must be removed by this call.
    pub(crate) unsafe fn pop(&mut self, class: usize) -> Option<*mut u8> {
        let head = *self.heads.get(class)?;
        if head.is_null() {
            return None;
        }
        // SAFETY: `head` is a payload the matching `push` put on this list, so its
        // first word holds the intrusive link (or null, when it was the last node)
        // and is readable.
        let next = unsafe { head.cast::<*mut u8>().read() };
        self.heads[class] = next;
        self.counts[class] = self.counts[class].saturating_sub(1);
        Some(head)
    }

    /// Pushes `ptr` onto `class`'s free list.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a `UNIT_SIZE`-aligned free block large enough to hold the
    /// intrusive next-pointer, and must not be otherwise reachable.
    pub(crate) unsafe fn push(&mut self, class: usize, ptr: *mut u8) {
        debug_assert_eq!(
            ptr as usize % UNIT_SIZE,
            0,
            "free-list nodes must be UNIT_SIZE-aligned payloads"
        );
        debug_assert!(
            class < self.heads.len(),
            "the class slot must be reserved before pushing"
        );
        if class >= self.heads.len() {
            // Unreachable when the caller reserved the slot; dropping the block here
            // would leak it, so the debug assertion above is the real guard.
            return;
        }
        // SAFETY: per the contract, `ptr` is a free block with room for one word and
        // is not reachable from anywhere else, so writing the link there cannot
        // clobber live data.
        unsafe { ptr.cast::<*mut u8>().write(self.heads[class]) };
        self.heads[class] = ptr;
        self.counts[class] += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backing storage standing in for carrier blocks (each "block" is a payload).
    struct Arena(Vec<Box<[u64; 4]>>);

    impl Arena {
        fn new(n: usize) -> Arena {
            Arena((0..n).map(|_| Box::new([0u64; 4])).collect())
        }

        fn payloads(&mut self) -> Vec<*mut u8> {
            self.0
                .iter_mut()
                .map(|b| b.as_mut_ptr().cast::<u8>())
                .collect()
        }
    }

    #[test]
    fn new_list_is_empty_and_grows_on_reserve() {
        let mut lists = FreeList::new();
        assert_eq!(lists.class_count(), 0);
        assert_eq!(lists.free_blocks(0), 0);
        lists.reserve(2, 1).expect("class 2 is in range");
        assert_eq!(lists.class_count(), 3);
        for class in 0..3 {
            assert_eq!(lists.free_blocks(class), 0);
        }
        // SAFETY: no node was pushed, so popping an empty class yields `None`.
        assert!(unsafe { lists.pop(0) }.is_none());
        // An out-of-range class has no list at all.
        // SAFETY: `pop` checks the class index before touching any node.
        assert!(unsafe { lists.pop(usize::MAX) }.is_none());
    }

    #[test]
    fn reserve_rejects_an_unencodable_class() {
        let mut lists = FreeList::new();
        assert!(matches!(
            lists.reserve(MAX_CLASS_COUNT, 1),
            Err(BeamError::BadArg)
        ));
        assert!(lists.reserve(MAX_CLASS_COUNT - 1, 1).is_ok());
    }

    #[test]
    fn push_pop_is_lifo_and_tracks_counts() {
        let mut arena = Arena::new(4);
        let ptrs = arena.payloads();
        let mut lists = FreeList::new();
        lists.reserve(0, 4).expect("class 0");
        for &ptr in &ptrs {
            // SAFETY: each payload is a distinct, 8-aligned, word-sized free block.
            unsafe { lists.push(0, ptr) };
        }
        assert_eq!(lists.free_blocks(0), 4);
        for &expected in ptrs.iter().rev() {
            // SAFETY: the nodes are on class 0's list and each is popped once.
            let popped = unsafe { lists.pop(0) }.expect("non-empty");
            assert_eq!(popped, expected, "LIFO order");
        }
        assert_eq!(lists.free_blocks(0), 0);
        // SAFETY: the list is empty.
        assert!(unsafe { lists.pop(0) }.is_none());
    }

    #[test]
    fn classes_are_independent() {
        let mut arena = Arena::new(4);
        let ptrs = arena.payloads();
        let (a0, a1) = (ptrs[0], ptrs[1]);
        let (b0, b1) = (ptrs[2], ptrs[3]);
        let mut lists = FreeList::new();
        lists.reserve(1, 2).expect("class 1");
        // SAFETY: distinct, 8-aligned, word-sized free blocks per class.
        unsafe {
            lists.push(0, a0);
            lists.push(0, a1);
            lists.push(1, b0);
            lists.push(1, b1);
        }
        assert_eq!(lists.free_blocks(0), 2);
        assert_eq!(lists.free_blocks(1), 2);
        assert_eq!(lists.free_blocks(2), 0);
        // SAFETY: class 1's list is non-empty.
        assert_eq!(unsafe { lists.pop(1) }, Some(b1));
        assert_eq!(lists.free_blocks(0), 2, "class 0 is untouched");
        assert_eq!(lists.free_blocks(1), 1);
        // SAFETY: both lists are non-empty.
        assert_eq!(unsafe { lists.pop(0) }, Some(a1));
        assert_eq!(unsafe { lists.pop(1) }, Some(b0));
        assert_eq!(unsafe { lists.pop(0) }, Some(a0));
        assert_eq!(lists.free_blocks(0), 0);
        assert_eq!(lists.free_blocks(1), 0);
    }

    #[test]
    fn pop_then_push_reuses_the_same_node() {
        let mut arena = Arena::new(2);
        let ptrs = arena.payloads();
        let mut lists = FreeList::new();
        lists.reserve(0, 2).expect("class 0");
        // SAFETY: two distinct 8-aligned free blocks.
        unsafe {
            lists.push(0, ptrs[0]);
            lists.push(0, ptrs[1]);
        }
        // SAFETY: the list is non-empty.
        let first = unsafe { lists.pop(0) }.expect("non-empty");
        // SAFETY: `first` was just removed from the list and is free.
        unsafe { lists.push(0, first) };
        assert_eq!(lists.free_blocks(0), 2);
        // SAFETY: the list is non-empty, and popping twice empties it.
        assert_eq!(unsafe { lists.pop(0) }, Some(first));
        assert_eq!(unsafe { lists.pop(0) }, Some(ptrs[0]));
        assert_eq!(lists.free_blocks(0), 0);
    }
}
