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

//! An mseg-backed carrier: the OS-level slab the allocator carves blocks from.
//!
//! Implementation module (public for testing/reuse, **not** part of the cross-crate
//! seam). A carrier does not map memory itself any more: it obtains one memory
//! **segment** from the process-wide segment allocator ([`crate::mseg`], the
//! `erts_mseg` counterpart) and owns it for its whole life. Releasing the carrier
//! hands the segment back to that allocator, which keeps it in its reuse cache for the
//! next carrier or returns it to the OS with `munmap` when the cache is full or
//! disabled — the legacy `mseg_alloc`/`mseg_dealloc` flow, `+MM` limits and counters
//! included. A carrier therefore never calls `libc::mmap`/`libc::munmap` itself.
//!
//! A multi-block carrier is dedicated to a single size class and subdivided into
//! equal blocks kept on the class's intrusive free list ([`FreeList`]); a large
//! request instead gets a dedicated single-block carrier.
//!
//! # Page alignment and alignment padding
//!
//! The segment layer reserves a private control header ([`crate::mseg`]'s
//! `MSEG_HEADER_SIZE` bytes) in front of every payload and returns the payload
//! itself, aligned to the alignment the caller asked for. A carrier hence requests
//! [`os_page_size`] alignment — the strongest alignment [`Carrier::alloc_map`]
//! accepts — so its base is **page-aligned**, exactly as a direct `mmap` base was.
//!
//! Because the base is page-aligned and the allocator rejects alignments larger than
//! the page, a block's alignment padding is a function of the requested alignment
//! alone (`align - BLOCK_HEADER_SIZE`), which is what lets `dealloc`/`realloc` locate
//! a block header from the caller's layout without storing a back-pointer. The block
//! header of a large block is written at the carrier base and the segment header
//! lives *before* the base, so nothing the allocator writes collides with it.
//!
//! # Zeroing
//!
//! A freshly mapped segment is anonymous private `mmap` memory and therefore zeroed,
//! but a segment served from the reuse cache retains whatever its previous owner
//! wrote. The allocator never relies on carrier memory being zeroed: it writes every
//! block header it later reads ([`Carrier::subdivide_into`] for slabs, the large path
//! in `crate::allocator` for dedicated blocks) and `alloc_zeroed` zeroes its payload
//! explicitly. The large path additionally zeroes the *alignment padding* between the
//! header and the payload, because that is where the unsized-dealloc probe looks for a
//! candidate header and stale bytes there would be indistinguishable from one of ours.
//!
//! `unsafe` is confined to this module's marked functions.

use core::ffi::c_void;
use std::sync::LazyLock;

use beam_common::{BeamError, Result};

use crate::block::{BlockFlags, BlockHdr, BlockHeader, BLOCK_HEADER_SIZE, UNIT_SIZE};
use crate::error;
use crate::free_list::FreeList;
use crate::mseg::{MsegAllocator, Segment};

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

/// The process-wide segment allocator every carrier is obtained from.
///
/// One instance for the whole process, like the legacy `erts_mseg` singleton: it owns
/// the `mmap`ed segments, the reuse cache and the `+MM` limit accounting, so the
/// segment of a released carrier can be handed to the next carrier instead of being
/// returned to the OS. No clone is kept here — the [`Segment`]s it hands out keep its
/// shared state alive on their own.
pub(crate) fn mseg() -> &'static MsegAllocator {
    static MSEG: LazyLock<MsegAllocator> = LazyLock::new(MsegAllocator::with_defaults);
    &MSEG
}

/// One memory slab handed to the allocator by the segment layer.
pub struct Carrier {
    /// The segment backing this carrier. It owns the mapping and releases it on drop,
    /// so a carrier can never leak its address space by being forgotten.
    segment: Segment,
    /// The segment's payload: the carrier base, page-aligned. Cached so `base()` is a
    /// plain load and kept as a raw pointer (which is why `Carrier` keeps its
    /// hand-written `Send` bound and deliberately does not get an automatic `Sync`).
    base: *mut u8,
    class: Option<u16>,
    block_count: usize,
    free_count: usize,
}

// SAFETY: a `Carrier` is an owning handle to a region of raw memory that is only
// ever mutated through `&mut self` (in practice while holding the allocator's
// mutex). The base pointer is not aliased by any other owning value, and the segment
// it points into is owned exclusively by this carrier, so moving a `Carrier` between
// threads is sound. `Sync` is deliberately *not* implemented: sharing `&Carrier`
// across threads is not part of the contract.
unsafe impl Send for Carrier {}

impl Carrier {
    /// Wraps an exclusively-owned [`Segment`] into a carrier.
    ///
    /// The segment must have been mapped with [`os_page_size`] alignment (see
    /// [`Carrier::alloc_map`]); the page-alignment invariant the block-header recovery
    /// in `crate::allocator` depends on is asserted here.
    fn from_segment(segment: Segment) -> Carrier {
        let base = segment.as_ptr();
        debug_assert_eq!(
            base as usize % os_page_size(),
            0,
            "a carrier segment payload is page-aligned"
        );
        Carrier {
            segment,
            base,
            class: None,
            block_count: 0,
            free_count: 0,
        }
    }

    /// The carrier size in bytes: the segment's usable extent, i.e. what
    /// [`Carrier::subdivide_into`] subdivides and what a dedicated large block owns.
    pub fn size(&self) -> usize {
        self.segment.size()
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

    /// Obtains a carrier of `size` bytes (rounded up to [`UNIT_SIZE`]) from the
    /// segment allocator.
    ///
    /// The segment is requested with [`os_page_size`] alignment, the strongest
    /// alignment this function accepts, so the payload it hands back — the carrier
    /// base — is page-aligned and the 64-byte segment header stays *below* the base.
    /// Every alignment the allocator can ask for (any power of two up to the page
    /// size) therefore divides the base, which is what the `padding_for`/`locate`
    /// block-header recovery in `crate::allocator` relies on.
    ///
    /// The segment is classified as a **multi-block** segment (`+M<alloc>mmmbc`, the
    /// unlimited one): this layer cannot tell a single-block carrier from a
    /// multi-block one because `crate::allocator` records the class and the block
    /// count only *after* the mapping exists, and the single-block limit
    /// (`+M<alloc>mmsbc`, 256) would spuriously cap the slab carriers, which are
    /// retained for the allocator's lifetime.
    ///
    /// # Safety
    ///
    /// `align` must be a non-zero power of two and at most the OS page size (the
    /// carrier is page-aligned, so any smaller power-of-two alignment is satisfied).
    /// The returned carrier owns its segment exclusively and must be consumed by
    /// [`Carrier::unmap`] exactly once (dropping it is equivalent and also releases
    /// the segment).
    pub(crate) unsafe fn alloc_map(size: usize, align: usize) -> Result<Carrier> {
        if !align.is_power_of_two() || align > os_page_size() {
            return Err(error::bad_align(align));
        }
        let size = round_up_unit(size).max(UNIT_SIZE);
        // `align <= os_page_size()` here, so requesting the page alignment satisfies
        // `align` too and makes the payload page-aligned.
        let segment = mseg().map(size, os_page_size())?;
        Ok(Carrier::from_segment(segment))
    }

    /// Releases the carrier's segment back to the segment allocator.
    ///
    /// The segment is handed to the segment allocator's reuse cache and is returned to
    /// the OS from there once the cache is full or dropped (legacy `erts_mseg_dealloc`;
    /// a `+MM`-configured allocator may hold it for reuse). Dropping a `Carrier` does
    /// exactly the same thing — the [`Segment`] it owns is RAII — so this is the
    /// explicit form of that release.
    ///
    /// # Safety
    ///
    /// No live pointer into the carrier may remain and all its blocks must be free.
    /// A carrier cannot be released twice: `unmap` consumes it and the segment is
    /// released by this call (or by the drop that follows) only once.
    pub(crate) unsafe fn unmap(self) {
        drop(self);
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
        if block_size < BLOCK_HEADER_SIZE || !block_size.is_multiple_of(UNIT_SIZE) {
            return Err(BeamError::BadArg);
        }
        let count = self.size() / block_size;
        if count == 0 {
            return Err(error::carrier_failure(self.segment.size()));
        }
        self.class = Some(class);
        self.block_count = count;
        self.free_count = count;
        lists.reserve(usize::from(class), count)?;
        for index in 0..count {
            let block_start = (index * block_size) as isize;
            // SAFETY: `(index + 1) * block_size <= self.size() <= capacity()` for
            // every `index < count`, so `block_start` and the header at it are inside
            // the segment; every block start is a multiple of `UNIT_SIZE`, and the
            // blocks do not overlap the intrusive links (written into the payloads by
            // `push`).
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
            // SAFETY: the block start is `UNIT_SIZE`-aligned and inside the segment,
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
    use crate::mseg::SegmentKind;

    #[test]
    fn mapping_is_page_aligned_and_has_the_requested_size() {
        // SAFETY: `alloc_map` takes ownership of the segment it returns and nothing
        // else references it; `UNIT_SIZE` is a valid alignment.
        let carrier = unsafe { Carrier::alloc_map(4096, UNIT_SIZE) }.expect("segment");
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
        let carrier = unsafe { Carrier::alloc_map(1, UNIT_SIZE) }.expect("segment");
        assert_eq!(carrier.size(), UNIT_SIZE);
        // SAFETY: nothing was handed out.
        unsafe { carrier.unmap() };
        // A zero-byte request still owns a usable one-unit segment.
        // SAFETY: as above.
        let carrier = unsafe { Carrier::alloc_map(0, UNIT_SIZE) }.expect("segment");
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
        // SAFETY: `alloc_map` gives exclusive ownership of the segment.
        let mut carrier = unsafe { Carrier::alloc_map(1024, UNIT_SIZE) }.expect("segment");
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
        // SAFETY: `alloc_map` gives exclusive ownership of the segment.
        let mut carrier = unsafe { Carrier::alloc_map(1000, UNIT_SIZE) }.expect("segment");
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
        // SAFETY: `alloc_map` gives exclusive ownership of the segment.
        let mut carrier = unsafe { Carrier::alloc_map(64, UNIT_SIZE) }.expect("segment");
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

    #[test]
    fn every_supported_alignment_yields_a_page_aligned_base() {
        // `allocator.rs` serves requests aligned to any power of two up to the page
        // size by handing out `base + align`, so the base must be page-aligned for all
        // of them — this is the invariant the block-header recovery depends on.
        let page = os_page_size();
        let mut align = 1;
        while align <= page {
            for size in [UNIT_SIZE, 48, page + 1, 4 * page] {
                // SAFETY: `align` is a power of two at most the page size and the
                // carrier owns the segment exclusively until it is unmapped below.
                let carrier = unsafe { Carrier::alloc_map(size, align) }
                    .unwrap_or_else(|e| panic!("alignment {align}, size {size}: {e:?}"));
                assert_eq!(carrier.base() as usize % page, 0, "align {align}, size {size}");
                assert_eq!(carrier.base() as usize % align, 0, "align {align}, size {size}");
                assert_eq!(carrier.size(), round_up_unit(size).max(UNIT_SIZE));
                // SAFETY: nothing was handed out and the carrier is consumed here.
                unsafe { carrier.unmap() };
            }
            align *= 2;
        }
    }

    #[test]
    fn alloc_map_takes_its_segment_from_the_shared_mseg_allocator() {
        // The crate-level segment allocator is shared with every other test in this
        // binary, so only its monotone call counters are compared here; the segment
        // lifecycle itself is pinned by the test-local allocator below.
        let before = mseg().stats();
        // SAFETY: `alloc_map` hands back exclusive ownership of a fresh segment.
        let carrier = unsafe { Carrier::alloc_map(4096, UNIT_SIZE) }.expect("segment");
        // The carrier's own segment was requested with page alignment and classified
        // as a multi-block segment: that is exactly what `alloc_map` asks mseg for.
        assert_eq!(carrier.segment.align(), os_page_size());
        assert_eq!(carrier.segment.kind(), SegmentKind::MultiBlock);
        // SAFETY: no block was ever handed out and the carrier is consumed here.
        unsafe { carrier.unmap() };
        let after = mseg().stats();
        assert!(
            after.calls.alloc > before.calls.alloc,
            "the carrier asked the shared segment allocator for a segment"
        );
        assert!(
            after.calls.dealloc > before.calls.dealloc,
            "and handed it back when it was unmapped"
        );
    }

    #[test]
    fn unmapping_hands_the_segment_back_to_the_segment_allocator() {
        // A test-local segment allocator, so the counters below are touched by nothing
        // else (the crate-level instance is shared by the whole test binary).
        let segments = MsegAllocator::with_defaults();
        let segment = segments
            .map(8192, os_page_size())
            .expect("the segment allocator maps a page-aligned segment");
        assert_eq!(segments.stats().segments, 1, "the segment is handed out");
        assert_eq!(segments.stats().mapped_segments, 1, "and mapped from the OS");

        let carrier = Carrier::from_segment(segment);
        assert_eq!(carrier.size(), 8192);
        assert_eq!(carrier.base() as usize % os_page_size(), 0, "page-aligned");
        // SAFETY: no block was ever handed out of this carrier and it is consumed here.
        unsafe { carrier.unmap() };

        let released = segments.stats();
        assert_eq!(released.segments, 0, "the segment is no longer handed out");
        assert_eq!(released.calls.dealloc, 1, "the release went through mseg");
        assert_eq!(released.cached_segments, 1, "and was cached for reuse");
        assert_eq!(released.mapped_segments, 1, "the mapping itself is retained");

        segments.clear_cache();
        assert_eq!(
            segments.stats().mapped_segments,
            0,
            "the mapping went back to the OS"
        );
    }
}
