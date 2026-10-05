//! An mmap-backed carrier: the OS-level slab the allocator carves blocks from.
//!
//! Implementation module (public for testing/reuse, **not** part of the cross-crate
//! seam). A carrier is obtained with anonymous, private `libc::mmap` and is therefore
//! **page-aligned by construction**; it is returned to the OS with `libc::munmap`
//! both on explicit release (a large block's `dealloc`, or the allocator's drop) and,
//! for a large block, with the whole mapping being the block.
//!
//! A multi-block carrier is dedicated to a single size class and subdivided into
//! equal blocks kept on the class's intrusive free list ([`FreeList`]); a large
//! request instead gets a dedicated single-block carrier.
//!
//! # Page alignment and alignment padding
//!
//! Because the `mmap` base is page-aligned and the allocator rejects alignments
//! larger than the page ([`os_page_size`]), a block's alignment padding is a function
//! of the requested alignment alone (`align - BLOCK_HEADER_SIZE`), which is what lets
//! `dealloc`/`realloc` locate a block header from the caller's layout without
//! storing a back-pointer.
//!
//! `unsafe` is confined to this module's marked functions.

use core::ffi::c_void;
use core::ptr;
use std::sync::LazyLock;

use beam_common::{BeamError, Result};

use crate::block::{BlockFlags, BlockHdr, BlockHeader, BLOCK_HEADER_SIZE, UNIT_SIZE};
use crate::error;
use crate::free_list::FreeList;

/// The OS page size, cached.
///
/// Carriers are page-aligned, so this is also the largest alignment the allocator
/// can satisfy without super-aligning a mapping.
pub(crate) fn os_page_size() -> usize {
    static PAGE_SIZE: LazyLock<usize> = LazyLock::new(|| {
        // SAFETY: `sysconf` with `_SC_PAGESIZE` has no preconditions and does not
        // touch caller memory; it returns a positive constant or a negative error.
        let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if value > 0 {
            value as usize
        } else {
            4096
        }
    });
    *PAGE_SIZE
}

/// Rounds `bytes` up to a whole number of [`UNIT_SIZE`] units (saturating).
pub(crate) fn round_up_unit(bytes: usize) -> usize {
    bytes.div_ceil(UNIT_SIZE).saturating_mul(UNIT_SIZE)
}

/// One memory slab handed to the allocator by the OS.
pub struct Carrier {
    base: *mut u8,
    size: usize,
    class: Option<u16>,
    block_count: usize,
    free_count: usize,
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

    /// The number of blocks currently free.
    pub fn free_count(&self) -> usize {
        self.free_count
    }

    /// The carrier base address (page-aligned).
    pub fn base(&self) -> *mut u8 {
        self.base
    }

    /// `mmap`s a new carrier of `size` bytes (rounded up to [`UNIT_SIZE`]).
    ///
    /// The mapping is anonymous and private, hence zeroed and page-aligned; the
    /// returned carrier owns it exclusively.
    ///
    /// # Safety
    ///
    /// `align` must be a non-zero power of two and at most the OS page size (the
    /// carrier is page-aligned, so any smaller power-of-two alignment is satisfied).
    /// The returned carrier owns its mapping exclusively and must be consumed by
    /// [`Carrier::unmap`] exactly once.
    pub(crate) unsafe fn alloc_map(size: usize, align: usize) -> Result<Carrier> {
        if !align.is_power_of_two() || align > os_page_size() {
            return Err(error::bad_align(align));
        }
        let size = round_up_unit(size).max(UNIT_SIZE);
        // SAFETY: an anonymous, private mapping has no preconditions beyond valid
        // flags/length. `mmap` either returns a fresh, exclusively-owned,
        // page-aligned mapping or `MAP_FAILED`, which is handled below.
        let mapped = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(error::carrier_failure(size));
        }
        let base = mapped.cast::<u8>();
        debug_assert_eq!(base as usize % os_page_size(), 0, "mmap is page-aligned");
        Ok(Carrier {
            base,
            size,
            class: None,
            block_count: 0,
            free_count: 0,
        })
    }

    /// Returns the carrier's memory to the OS with `munmap`.
    ///
    /// # Safety
    ///
    /// No live pointer into the carrier may remain and all its blocks must be free;
    /// the carrier must not be unmapped twice.
    pub(crate) unsafe fn unmap(self) {
        if self.base.is_null() || self.size == 0 {
            return;
        }
        // SAFETY: `base`/`size` are exactly the pair a successful `alloc_map`
        // requested, and the contract states no live pointer into the mapping
        // remains, so unmapping the whole region is sound and happens exactly once
        // (a `Carrier` is consumed by this call and has no `Drop`).
        let rc = unsafe { libc::munmap(self.base.cast::<c_void>(), self.size) };
        debug_assert_eq!(rc, 0, "munmap must succeed for a live carrier mapping");
    }

    /// Records the size class this carrier is dedicated to.
    pub(crate) fn set_class(&mut self, class: Option<u16>) {
        self.class = class;
    }

    /// Sets the number of blocks (keeps [`Carrier::block_count`] in sync).
    pub(crate) fn set_block_count(&mut self, n: usize) {
        self.block_count = n;
    }

    /// Sets the number of free blocks (keeps [`Carrier::free_count`] in sync).
    pub(crate) fn set_free_count(&mut self, n: usize) {
        self.free_count = n;
    }

    /// Subdivides this carrier into equal `block_size`-byte blocks of size class
    /// `class`, writing every block's header and pushing its payload onto
    /// `lists[class]`.
    ///
    /// Returns [`BeamError::BadArg`] for a malformed `block_size` and
    /// [`BeamError::Alloc`] when the carrier cannot hold a single block (it must be
    /// at least `block_size` bytes: the allocator sizes carriers accordingly).
    pub(crate) fn subdivide_into(
        &mut self,
        class: u16,
        block_size: usize,
        lists: &mut FreeList,
    ) -> Result<()> {
        if block_size < BLOCK_HEADER_SIZE || block_size % UNIT_SIZE != 0 {
            return Err(BeamError::BadArg);
        }
        let count = self.size / block_size;
        if count == 0 {
            return Err(error::carrier_failure(self.size));
        }
        self.class = Some(class);
        self.block_count = count;
        self.free_count = count;
        lists.reserve(usize::from(class), count)?;
        for index in 0..count {
            let block_start = (index * block_size) as isize;
            // SAFETY: `(index + 1) * block_size <= size` for every `index < count`,
            // so `block_start` and the header at it are inside the mapping; every
            // block start is a multiple of `UNIT_SIZE`, and the blocks do not
            // overlap the intrusive links (written into the payloads by `push`).
            let payload = unsafe { self.base.offset(block_start + BLOCK_HEADER_SIZE as isize) };
            let mut flags = BlockFlags::empty();
            flags.set_free();
            if index + 1 == count {
                flags.set_last_in_carrier();
            }
            let header = BlockHdr {
                size: block_size,
                payload_offset: 0,
                class,
                carrier: self.base.cast::<c_void>(),
                flags: flags.bits(),
            };
            // SAFETY: the block start is `UNIT_SIZE`-aligned and inside the mapping,
            // and no live payload overlaps the header area.
            unsafe { BlockHeader::store(self.base.offset(block_start), header) };
            // SAFETY: `payload` is the first word of a free block with at least
            // `BLOCK_HEADER_SIZE` usable bytes below it, and is not reachable from
            // anywhere else.
            unsafe { lists.push(usize::from(class), payload) };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_is_page_aligned_and_has_the_requested_size() {
        // SAFETY: `alloc_map` takes ownership of the mapping it returns and nothing
        // else references it; `UNIT_SIZE` is a valid alignment.
        let carrier = unsafe { Carrier::alloc_map(4096, UNIT_SIZE) }.expect("mmap");
        assert_eq!(carrier.size(), 4096);
        assert_eq!(carrier.base() as usize % os_page_size(), 0, "page-aligned");
        assert_eq!(carrier.class(), None);
        assert_eq!(carrier.block_count(), 0);
        assert_eq!(carrier.free_count(), 0);
        // SAFETY: no block was ever handed out and the carrier is consumed here.
        unsafe { carrier.unmap() };
    }

    #[test]
    fn mapping_size_is_rounded_up_to_the_allocation_unit() {
        // SAFETY: as above; a non-unit size is rounded up rather than truncated.
        let carrier = unsafe { Carrier::alloc_map(1, UNIT_SIZE) }.expect("mmap");
        assert_eq!(carrier.size(), UNIT_SIZE);
        // SAFETY: nothing was handed out.
        unsafe { carrier.unmap() };
        // A zero-byte request still owns a usable one-unit mapping.
        // SAFETY: as above.
        let carrier = unsafe { Carrier::alloc_map(0, UNIT_SIZE) }.expect("mmap");
        assert_eq!(carrier.size(), UNIT_SIZE);
        // SAFETY: as above.
        unsafe { carrier.unmap() };
    }

    #[test]
    fn unsupported_alignments_are_rejected() {
        // Larger than a page: impossible without super-aligning the mapping.
        // SAFETY: the call returns before mapping anything for this alignment.
        assert!(matches!(
            unsafe { Carrier::alloc_map(4096, os_page_size() * 2) },
            Err(BeamError::BadArg)
        ));
        // Not a power of two.
        // SAFETY: as above.
        assert!(matches!(
            unsafe { Carrier::alloc_map(4096, 3) },
            Err(BeamError::BadArg)
        ));
        // SAFETY: as above.
        assert!(matches!(
            unsafe { Carrier::alloc_map(4096, 0) },
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn subdivision_counts_blocks_and_feeds_the_free_list() {
        // SAFETY: `alloc_map` gives exclusive ownership of the mapping.
        let mut carrier = unsafe { Carrier::alloc_map(1024, UNIT_SIZE) }.expect("mmap");
        let mut lists = FreeList::new();
        // 1024 / (16 header + 48 payload) = 16 whole blocks, with no remainder here.
        carrier
            .subdivide_into(0, BLOCK_HEADER_SIZE + 48, &mut lists)
            .expect("subdivision");
        assert_eq!(carrier.class(), Some(0));
        assert_eq!(carrier.block_count(), 16);
        assert_eq!(carrier.free_count(), 16);
        assert_eq!(lists.class_count(), 1);
        assert_eq!(lists.free_blocks(0), 16);
        // Every block is inside the carrier and carries a header pointing back at it.
        for _ in 0..16 {
            // SAFETY: the list is non-empty; each node is popped once.
            let payload = unsafe { lists.pop(0) }.expect("a free block");
            let address = payload as usize;
            assert!(address >= carrier.base() as usize);
            assert!(address + 48 <= carrier.base() as usize + carrier.size());
            // SAFETY: the payload's header was written by `subdivide_into`.
            let header = unsafe { BlockHeader::load(payload.sub(BLOCK_HEADER_SIZE)) };
            assert_eq!(header.size, BLOCK_HEADER_SIZE + 48);
            assert_eq!(header.class, 0);
            assert_eq!(header.carrier, carrier.base().cast::<c_void>());
            assert!(BlockFlags::from_bits(header.flags).is_free());
        }
        assert_eq!(lists.free_blocks(0), 0);
        // SAFETY: no block is live and the carrier is consumed here.
        unsafe { carrier.unmap() };
    }

    #[test]
    fn subdivision_leaves_the_tail_unused() {
        // SAFETY: `alloc_map` gives exclusive ownership of the mapping.
        let mut carrier = unsafe { Carrier::alloc_map(1000, UNIT_SIZE) }.expect("mmap");
        let mut lists = FreeList::new();
        let block_size = BLOCK_HEADER_SIZE + 64;
        carrier
            .subdivide_into(3, block_size, &mut lists)
            .expect("subdivision");
        assert_eq!(carrier.block_count(), 1000 / block_size);
        assert_eq!(lists.free_blocks(3), 1000 / block_size);
        // SAFETY: no block is live and the carrier is consumed here.
        unsafe { carrier.unmap() };
    }

    #[test]
    fn subdivision_rejects_a_block_larger_than_the_carrier() {
        // SAFETY: `alloc_map` gives exclusive ownership of the mapping.
        let mut carrier = unsafe { Carrier::alloc_map(64, UNIT_SIZE) }.expect("mmap");
        let mut lists = FreeList::new();
        assert!(matches!(
            carrier.subdivide_into(0, 128, &mut lists),
            Err(BeamError::Alloc(_))
        ));
        assert!(matches!(
            carrier.subdivide_into(0, UNIT_SIZE - 1, &mut lists),
            Err(BeamError::BadArg)
        ));
        // SAFETY: nothing was handed out and the carrier is consumed here.
        unsafe { carrier.unmap() };
    }

    #[test]
    fn os_page_size_is_a_power_of_two() {
        let page = os_page_size();
        assert!(page.is_power_of_two());
        assert!(page >= 4096);
        assert_eq!(round_up_unit(17), 32);
        assert_eq!(round_up_unit(32), 32);
        assert_eq!(round_up_unit(0), 0);
    }
}
