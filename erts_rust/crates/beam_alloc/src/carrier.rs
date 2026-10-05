//! An mmap-backed carrier: the OS-level slab the allocator carves blocks from.
//!
//! Implementation module (public for testing/reuse, **not** part of the cross-crate
//! seam). A carrier is obtained with anonymous, private `libc::mmap` and is therefore
//! **page-aligned by construction**; it is returned to the OS with `libc::munmap`
//! once its class's carrier list is drained. The `mmap` base *is* the carrier base,
//! which makes block→carrier recovery cheap and gives huge-page-eligible memory.
//!
//! A multi-block carrier is dedicated to a single size class and subdivided into
//! equal blocks kept on an intrusive free list; the free-list head lives here. A
//! large request instead gets a dedicated single-block carrier.
//!
//! `unsafe` is confined to this module's marked functions.

use beam_common::Result;

/// One memory slab handed to the allocator by the OS.
pub struct Carrier {
    base: *mut u8,
    size: usize,
    class: Option<u16>,
    block_count: usize,
    free_count: usize,
    /// Intrusive head of this carrier's free-block list (`null` when none free).
    free_head: *mut u8,
}

// SAFETY: a `Carrier` is an owning handle to a region of raw memory that is only
// ever mutated through `&mut self` (in practice while holding the allocator's
// mutex). The base pointer is not aliased by any other owning value, so moving a
// `Carrier` between threads is sound. `Sync` is deliberately *not* implemented:
// sharing `&Carrier` across threads is not part of the contract.
unsafe impl Send for Carrier {}

impl Carrier {
    /// The carrier size in bytes.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The size class this carrier serves (`None` for a dedicated large carrier).
    pub fn class(&self) -> Option<u16> {
        self.class
    }

    /// The number of equal blocks this carrier was subdivided into.
    pub fn block_count(&self) -> usize {
        self.block_count
    }

    /// The number of blocks currently on the free list.
    pub fn free_count(&self) -> usize {
        self.free_count
    }

    /// The carrier base address (page-aligned).
    pub fn base(&self) -> *mut u8 {
        self.base
    }

    /// `mmap`s a new carrier of `size` bytes, aligned so that payloads respect
    /// `align`.
    ///
    /// # Safety
    ///
    /// `align` must be a non-zero power of two and a divisor of the OS page size (the
    /// carrier is page-aligned, so any smaller power-of-two alignment is satisfied).
    /// The returned carrier owns its mapping exclusively.
    pub(crate) unsafe fn alloc_map(size: usize, align: usize) -> Result<Carrier> {
        let _ = (size, align);
        unimplemented!("Carrier::alloc_map: mmap-backed creation (allocator implementation pass)")
    }

    /// Returns the carrier's memory to the OS with `munmap`.
    ///
    /// # Safety
    ///
    /// No live pointer into the carrier may remain and all its blocks must be free;
    /// the carrier must not be unmapped twice.
    pub(crate) unsafe fn unmap(self) {
        let _ = self;
        unimplemented!("Carrier::unmap: munmap-backed teardown (allocator implementation pass)")
    }

    /// The intrusive free-list head (`null` when the carrier has no free blocks).
    pub(crate) fn free_head(&self) -> *mut u8 {
        self.free_head
    }

    /// Sets the intrusive free-list head.
    pub(crate) fn set_free_head(&mut self, head: *mut u8) {
        self.free_head = head;
    }

    /// Sets the number of free blocks (keeps [`Carrier::free_count`] in sync).
    pub(crate) fn set_free_count(&mut self, n: usize) {
        self.free_count = n;
    }
}
