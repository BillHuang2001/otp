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

//! Variable-size multi-block carriers: split-on-allocation and free-block coalescing.
//!
//! Implementation module (public for testing/reuse, **not** part of the cross-crate
//! seam). A multi-block carrier (MBC) is one `mseg` segment that the allocator carves
//! **on demand**: instead of being subdivided up front into equal blocks of one size
//! class — what [`Carrier::subdivide_into`] does for the segregated/slab path — its
//! free space is one or more free blocks of *arbitrary* size that grow and shrink as
//! requests arrive and are released.
//!
//! Ground truth: the legacy `mbc_alloc` / `mbc_alloc_finalize` / `mbc_free` trio in
//! `../../erts/emulator/beam/erl_alloc_util.c` (read it; never modify it). This module
//! reproduces its algorithm faithfully:
//!
//! * [`MbcCarrier::alloc`] takes a free block of at least the request's size — the
//!   good-fit search of [`FreeList::take_fit`], not a second search — and, when the
//!   block would still leave a *usable* residue behind, **splits**: the request's worth
//!   is handed out and the tail is filed back as a new free block (the legacy
//!   `mbc_alloc_finalize` "shrink block" arm). When the residue would be smaller than
//!   the minimum block size the whole block is handed out instead — the rule that stops
//!   the allocator littering the carrier with unusable slivers (legacy `adjust_sbct`:
//!   "we avoid having to split off free 'residue blocks' smaller than min_block_size").
//! * [`MbcCarrier::dealloc`] marks the block free and then **coalesces it with its
//!   physically adjacent free neighbours** — previous first, then next, exactly the
//!   order `mbc_free` uses — so a carrier fragmented by an alloc/free sequence ends up
//!   as one free block spanning the whole carrier again ([`MbcCarrier::is_fully_free`],
//!   the condition the allocator uses to destroy the carrier).
//!
//! # Interior layout: boundary tags (a free-block footer) + one state bit
//!
//! Each block is `[2-word header][payload]`, contiguous, covering the carrier exactly
//! once (the geometry [`crate::block`] and [`crate::free_list`] already assume). A
//! block's header records its **total** size ([`crate::block::BlockHdr::size`]), a free
//! marker and a last-in-carrier marker. That is not enough to coalesce: a block must
//! also be able to find the *start* of its physical predecessor, and a predecessor's
//! size is not recoverable from the successor's own header.
//!
//! Two designs can supply it:
//!
//! * an **address-ordered side structure** (e.g. a sorted map from block start to
//!   size), or
//! * **boundary tags** — a one-word *footer* written into the last word of every free
//!   block, holding that block's size, plus a per-block `prev_free` state bit saying
//!   "the footer before me is valid".
//!
//! This module takes the boundary-tag design, because it is what the legacy does
//! (`SET_BLK_SZ_FTR` / `PREV_BLK_SZ` / `PREV_FREE_BLK_HDR_FLG`) and because its geometry
//! stays **entirely address-based**: a block's neighbours are recoverable from the block
//! pointer alone, so the scheme allocates nothing, needs no side table to be grown (an
//! allocator *inside* the allocator), and cannot leak bookkeeping. Its cost is exactly
//! one word per **free** block — never per allocated block — and it is that cost which
//! sets this module's minimum block size:
//!
//! ```text
//! block_start + 0                     .. + 16   header (bhdr, carrier)
//! block_start + 16                    .. + 32   the free block's two link words
//!                                                 (FreeList's in-block links)
//! block_start + size - WORD_SIZE      .. + size the free block's footer
//! ```
//!
//! The links and the footer must not collide, so `size >= 16 + 16 + 8 = 40`; block
//! sizes are [`UNIT_SIZE`]-granular, hence [`MIN_MBC_BLOCK_SIZE`] = **48** bytes.
//!
//! ## Why this cannot disturb the existing per-class slab path
//!
//! * The slab path ([`Carrier::subdivide_into`] + [`FreeList::push`]/[`FreeList::pop`])
//!   writes headers with `payload_offset == 0`, [`BlockFlags::is_free`] and (last block)
//!   [`BlockFlags::last_in_carrier`]; it never sets `prev_free` and never writes a
//!   footer. Only this module sets that bit and writes footers.
//! * [`FreeList`] reads a block's size from its **header** (the legacy `MBC_FBLK_SZ`
//!   model), never from the footer, so filing and unfiling a free block is unchanged
//!   whichever path created it.
//! * [`crate::free_list::MIN_FREE_BLOCK_SIZE`] stays **32**: this module's minimum is a
//!   local constant, and every block it creates is ≥ 48 ≥ 32, so `FreeList::insert`'s
//!   existing lower bound is satisfied rather than relaxed.
//! * [`ALL_FLAGS`](crate::block::ALL_FLAGS) and its compile-time assertion are
//!   unchanged: `prev_free` is bit 7, a block-*state* bit kept out of the taxonomy
//!   union, exactly as the legacy `PREV_FREE_BLK_HDR_FLG` sits beside the legacy
//!   carrier-kind combination.
//!
//! # Deliberately NOT reproduced (legacy behaviour this module omits)
//!
//! The legacy multi-block allocator carries a great deal of machinery that is not part
//! of "split and coalesce" and is out of scope here:
//!
//! * `mem_discard` / `madvise` regions, the pooled-carrier discard limits.
//! * Allocation tags (`ATAG`) and `BLK_ATAG_SZ`, and therefore tagged block sizing.
//! * Carrier pools / the "homecoming" protocol, abandoned-carrier thresholds, and the
//!   delayed-dealloc queue.
//! * `mbc_realloc` — the in-place grow/shrink-with-next-free path. Reallocation is the
//!   caller's concern (allocate, copy, free).
//! * Super-aligned carriers' carrier-offset bits in the block header
//!   (`MBC_ABLK_OFFSET_BITS`): this crate's header always stores the carrier *base*.
//! * `create_carrier`'s emergency single-block-carrier fallback, the "main carrier"
//!   special case (the main carrier is never destroyed) and `mbc_alloc_block`'s
//!   `get_free_block`-then-`create_carrier` *allocation policy*: this module owns one
//!   carrier and never creates another. Choosing among carriers, and releasing a fully
//!   free one ([`MbcCarrier::is_fully_free`] + [`MbcCarrier::into_carrier`]), is the
//!   allocator wiring's job.
//! * `struct`-level page trimming of a coalesced free block.
//!
//! # Unsafe
//!
//! `unsafe` is confined to the marked functions: the raw block-header/footer/link
//! reads and writes, the carrier mapping, and the calls into [`FreeList`]'s unsafe
//! API. Every `unsafe fn` has a `# Safety` section and every `unsafe` block a
//! `// SAFETY:` rationale (`beam_common::safety`). All size arithmetic, the decisions
//! (split or not, which neighbour is free, merge or not) and every introspection query
//! are safe Rust.
//!
//! # Tests
//!
//! Tests live in `src/mbc/tests.rs` (mirroring `src/mseg/tests.rs`), so this file stays
//! focused on the implementation.

use core::ffi::c_void;

use beam_common::Result;

use crate::block::{
    BlockFlags, BlockHdr, BlockHeader, BLOCK_HEADER_SIZE, MAX_BLOCK_SIZE, UNIT_SIZE, WORD_SIZE,
};
use crate::carrier::{round_up_unit, Carrier};
use crate::free_list::{FreeList, LINK_WORDS};

/// The free-list class a variable-size MBC files its free blocks under.
///
/// One carrier's free space is a single *variable-size* bucket — that is the whole
/// point of the path — so it needs exactly one class index. The value is also recorded
/// in every block header this module writes, so the header stays consistent with the
/// allocator's `(class == CLASS_NONE) == is_large` plausibility rule (`is_large` is
/// clear: an MBC block is not a dedicated single-block-carrier block).
pub const MBC_CLASS: u16 = 0;

/// The smallest block this module creates: the block header, the two intrusive link
/// words [`FreeList`] threads through a free block's payload, and the free-block footer.
///
/// `16 + 2*8 + 8 = 40` rounded up to [`UNIT_SIZE`] granularity = **48** bytes (see the
/// module docs' layout diagram). This is an **MBC-local** constant: it does not change
/// [`crate::free_list::MIN_FREE_BLOCK_SIZE`] (32), which guards the slab path.
pub const MIN_MBC_BLOCK_SIZE: usize =
    (BLOCK_HEADER_SIZE + LINK_WORDS * WORD_SIZE + WORD_SIZE).div_ceil(UNIT_SIZE) * UNIT_SIZE;

// The footer must be a machine word and the link words must be payload-sized, so the
// layout diagram's arithmetic is about the same units on any target.
const _: () = assert!(WORD_SIZE == core::mem::size_of::<*mut u8>());
// The MBC minimum must be able to carry both intrusive link words *and* the footer.
const _: () = assert!(MIN_MBC_BLOCK_SIZE - WORD_SIZE >= BLOCK_HEADER_SIZE + LINK_WORDS * WORD_SIZE);
// A block this module creates is always large enough for `FreeList::insert`.
const _: () = assert!(MIN_MBC_BLOCK_SIZE >= crate::free_list::MIN_FREE_BLOCK_SIZE);
// `MIN_FREE_BLOCK_SIZE` is UNIT_SIZE-granular.
const _: () = assert!(MIN_MBC_BLOCK_SIZE.is_multiple_of(UNIT_SIZE));

/// One variable-size multi-block carrier: a carrier plus the free list of the free
/// blocks carved out of it.
///
/// The carrier is owned exclusively; all of its space is covered by a gapless chain of
/// blocks from [`MbcCarrier::base`] to `base + carrier_size()`, each block's header
/// recording its total size. That is what makes `free_bytes() + live_bytes() ==
/// carrier_size()` an invariant rather than an estimate.
pub struct MbcCarrier {
    /// The owned mseg-backed segment. Dropping (or releasing) it hands the segment back.
    carrier: Carrier,
    /// The size class recorded in every block header this carrier creates, and the
    /// [`FreeList`] bucket its free blocks are filed under.
    class: u16,
    /// The carrier's free blocks, one variable-size bucket.
    lists: FreeList,
    /// Total bytes handed out (block sizes, headers included).
    live_bytes: usize,
    /// Number of blocks currently handed out.
    live_blocks: usize,
}

// The allocator wiring (a follow-up task) adopts this module; until then only this
// module's own tests drive it, so the operational API is not reachable from a
// non-test build's dead-code analysis yet.
#[allow(dead_code)]
impl MbcCarrier {
    /// Maps a carrier of `size` bytes (rounded up to [`UNIT_SIZE`], at least
    /// [`MIN_MBC_BLOCK_SIZE`]) and turns the whole of it into one free block.
    ///
    /// `class` is the size class recorded in every block header and used as the free
    /// list's bucket index; it must be a valid class index (<
    /// [`crate::block::MAX_CLASS_COUNT`]) or the call fails with [`BeamError::BadArg`]
    /// before anything is mapped. `align` is the carrier base's alignment, validated by
    /// [`Carrier::alloc_map`] (a non-zero power of two up to the OS page size).
    ///
    /// # Safety
    ///
    /// The returned carrier owns its segment exclusively and must be released —
    /// dropping it, or [`MbcCarrier::into_carrier`] followed by `Carrier::unmap` — with
    /// no live block outstanding.
    ///
    /// [`BeamError::BadArg`]: beam_common::BeamError::BadArg
    pub(crate) unsafe fn new(size: usize, align: usize, class: u16) -> Result<MbcCarrier> {
        let mut lists = FreeList::new();
        // Validate the class *before* mapping, so a bad class costs no segment.
        lists.reserve(usize::from(class), 1)?;
        let request = round_up_unit(size.max(MIN_MBC_BLOCK_SIZE));
        // SAFETY: `align` is validated by `alloc_map`; the caller accepts exclusive
        // ownership of the mapping it returns.
        let carrier = unsafe { Carrier::alloc_map(request, align)? };
        let mut this = MbcCarrier {
            carrier,
            class,
            lists,
            live_bytes: 0,
            live_blocks: 0,
        };
        let carrier_size = this.carrier.size();
        debug_assert_eq!(carrier_size, request, "the segment is the exact usable size");
        debug_assert!(carrier_size >= MIN_MBC_BLOCK_SIZE);
        let mut flags = BlockFlags::empty();
        flags.set_free();
        // One block covers the whole carrier, so it is also the last one.
        flags.set_last_in_carrier();
        // SAFETY: the carrier is exclusively owned and `carrier_size` is its whole
        // extent, so the header at its base is inside the mapping, `UNIT_SIZE`-aligned
        // and overlaps no live payload; no footer is needed for a last block.
        unsafe { this.store_block(this.carrier.base(), carrier_size, flags) };
        // SAFETY: the block at the base is a free block whose header was just written
        // and whose payload (the whole carrier after the header) holds the link words.
        unsafe {
            this.lists
                .insert(usize::from(class), payload_of(this.carrier.base()), carrier_size)
        };
        Ok(this)
    }

    /// Hands out a block whose payload is at least `size` bytes, or `None` when this
    /// carrier holds no big-enough free block.
    ///
    /// Splits the chosen free block when the residue would still be a usable block
    /// (≥ [`MIN_MBC_BLOCK_SIZE`]); otherwise the whole block is handed out, so `size` is
    /// a lower bound on the payload rather than its exact extent — the caller learns the
    /// real block size through [`crate::free_list::FreeList::block_size`] or, at the
    /// allocator level, from the block header.
    ///
    /// # Safety
    ///
    /// The carrier must be exclusively owned (no other pointer into it is being read or
    /// written concurrently) and the returned pointer is a live allocation that must be
    /// released exactly once through [`MbcCarrier::dealloc`].
    pub(crate) unsafe fn alloc(&mut self, size: usize) -> Option<*mut u8> {
        let want = Self::want_block_size(size)?;
        // SAFETY: `class` is reserved (see `new`) and every block filed on it is an
        // intact free block this module maintains.
        let payload = unsafe { self.lists.take_fit(usize::from(self.class), want) }?;
        let block = block_of_payload(payload);
        // SAFETY: `payload` was just taken off the free list, so it names a block whose
        // header this module wrote.
        let org_size = unsafe { FreeList::block_size(payload) };
        debug_assert!(org_size >= want, "a good fit is at least the requested size");
        // SAFETY: as above.
        let flags = unsafe { self.load_flags(block) };
        debug_assert!(flags.is_free(), "a free-list node is a free block");

        let prev_free = flags.is_prev_free();
        let was_last = flags.last_in_carrier();
        // The legacy `mbc_alloc_finalize` split rule, verbatim: split only when the
        // residue would still be a usable block.
        let (block_size, residue) = if org_size - MIN_MBC_BLOCK_SIZE >= want {
            (want, org_size - want)
        } else {
            (org_size, 0)
        };

        let mut handed_out = BlockFlags::empty();
        if prev_free {
            handed_out.set_prev_free();
        }
        if residue == 0 && was_last {
            handed_out.set_last_in_carrier();
        }
        // SAFETY: `block` is the taken block's start; the header is this block's own and
        // its payload is not live (it was free a moment ago).
        unsafe { self.store_block(block, block_size, handed_out) };

        if residue > 0 {
            let nxt = block.wrapping_add(block_size);
            let mut free_flags = BlockFlags::empty();
            free_flags.set_free();
            // The residue ends where the taken block ended, so it inherits its lastness.
            // Its own predecessor is the block just handed out, so `prev_free` is clear.
            if was_last {
                free_flags.set_last_in_carrier();
            }
            // SAFETY: the residue starts inside the carrier (`block_size < org_size`) and
            // does not overlap the block just handed out.
            unsafe { self.store_block(nxt, residue, free_flags) };
            if !was_last {
                // SAFETY: the residue is not the last block, so its footer (the word
                // before its successor) is inside the carrier.
                unsafe { write_footer(nxt, residue) };
                // The successor already carries `prev_free` (its predecessor, the taken
                // block, was free and still is — as the residue) and its footer now holds
                // the residue's size, so nothing else needs fixing up: the legacy
                // `valid_blk_info = 1` case.
            }
            // SAFETY: `nxt` is a free block of `residue >= MIN_MBC_BLOCK_SIZE` bytes with
            // a header just written and link words inside its payload.
            unsafe {
                self.lists
                    .insert(usize::from(self.class), payload_of(nxt), residue)
            };
        } else if !was_last {
            // The whole block was handed out, so the successor's predecessor is no
            // longer free: clear its marker (its stale footer is never read again).
            let nxt = block.wrapping_add(block_size);
            // SAFETY: `block` is not the last block, so its successor starts inside the
            // carrier and its header was written by this module.
            unsafe { self.clear_prev_free(nxt) };
        }

        self.live_bytes += block_size;
        self.live_blocks += 1;
        Some(payload)
    }

    /// Releases a block handed out by [`MbcCarrier::alloc`] and coalesces it with its
    /// physically adjacent free neighbours (previous first, then next).
    ///
    /// # Safety
    ///
    /// `payload` must be a block currently handed out of *this* carrier and must not be
    /// released twice.
    pub(crate) unsafe fn dealloc(&mut self, payload: *mut u8) {
        let class = usize::from(self.class);
        let base = self.carrier.base();
        let mut block = block_of_payload(payload);
        // SAFETY: `payload` is a block this module handed out, so its header was written
        // by `store_block`.
        let mut size = unsafe { self.block_size(block) };
        let released = size;
        // SAFETY: as above.
        let flags = unsafe { self.load_flags(block) };
        let mut is_last = flags.last_in_carrier();

        if flags.is_prev_free() {
            debug_assert_ne!(block, base, "the first block cannot have a free predecessor");
            // SAFETY: `prev_free` says the word before `block` is the predecessor's
            // footer, holding its (free) block size.
            let prev_size = unsafe { read_prev_size(block) };
            debug_assert!(
                prev_size >= MIN_MBC_BLOCK_SIZE && prev_size % UNIT_SIZE == 0,
                "a free block is at least the MBC minimum and UNIT_SIZE-granular"
            );
            let prev = block.wrapping_sub(prev_size);
            // SAFETY: the predecessor is a free block this module filed on `class`.
            let removed = unsafe { self.lists.remove(class, payload_of(prev)) };
            debug_assert!(removed, "a free neighbour is filed on the free list");
            block = prev;
            size += prev_size;
            // The merged block *is* `prev`: keep its own `prev_free` (it still describes
            // the block before `prev`) and record the grown free size.
            // SAFETY: `block` is a free block with a header this module wrote.
            unsafe { self.store_size(block, size) };
        } else {
            // SAFETY: `block` is the block just released.
            unsafe { self.mark_free(block) };
        }

        if is_last {
            // SAFETY: `block` is a free block with a header this module wrote.
            unsafe { self.set_last(block) };
        } else {
            let nxt = block.wrapping_add(size);
            // SAFETY: `block` is not last, so its successor starts inside the carrier
            // and is a block this module created.
            if unsafe { self.load_flags(nxt) }.is_free() {
                // SAFETY: as above.
                let nxt_size = unsafe { self.block_size(nxt) };
                debug_assert!(
                    nxt_size >= MIN_MBC_BLOCK_SIZE && nxt_size % UNIT_SIZE == 0,
                    "a free block is at least the MBC minimum and UNIT_SIZE-granular"
                );
                // SAFETY: the successor is a free block this module filed on `class`.
                let removed = unsafe { self.lists.remove(class, payload_of(nxt)) };
                debug_assert!(removed, "a free neighbour is filed on the free list");
                size += nxt_size;
                // SAFETY: `block` is a free block with a header this module wrote.
                unsafe {
                    self.store_size(block, size);
                    is_last = self.load_flags(nxt).last_in_carrier();
                }
                if is_last {
                    // SAFETY: as above.
                    unsafe { self.set_last(block) };
                } else {
                    // SAFETY: as above — the merged block is still followed by a block,
                    // so it needs its own footer.
                    unsafe {
                        self.clear_last(block);
                        write_footer(block, size);
                    }
                }
            } else {
                // The successor stays allocated: it must now know that its predecessor
                // is free (and how big it is).
                // SAFETY: `nxt` is a block inside the carrier with a header this module
                // wrote; `block` is the merged free block and is not last, so it needs a
                // footer.
                unsafe {
                    self.set_prev_free(nxt);
                    self.clear_last(block);
                    write_footer(block, size);
                }
            }
        }

        // SAFETY: `block` is a free block of `size` bytes (≥ MIN_MBC_BLOCK_SIZE) — the
        // merged extent — with a header this module wrote and link words inside its
        // payload, and it is on no free list (its neighbours were unlinked above).
        unsafe {
            self.lists.insert(class, payload_of(block), size)
        };
        self.live_bytes -= released;
        self.live_blocks -= 1;
    }

    /// The carrier size in bytes (its whole usable extent).
    pub fn carrier_size(&self) -> usize {
        self.carrier.size()
    }

    /// The carrier base address (page-aligned).
    pub fn base(&self) -> *mut u8 {
        self.carrier.base()
    }

    /// The size class recorded in this carrier's block headers.
    pub fn class(&self) -> u16 {
        self.class
    }

    /// Total free bytes: the carrier's size minus the blocks currently handed out.
    ///
    /// Exact, not an estimate: the carrier is covered by a gapless chain of blocks whose
    /// sizes sum to its whole extent, so every byte is either inside a handed-out block
    /// or inside a free one.
    pub fn free_bytes(&self) -> usize {
        self.carrier.size() - self.live_bytes
    }

    /// Total bytes currently handed out (block sizes, headers included), so
    /// `free_bytes() + live_bytes() == carrier_size()`.
    pub fn live_bytes(&self) -> usize {
        self.live_bytes
    }

    /// The number of blocks currently handed out.
    pub fn allocated_blocks(&self) -> usize {
        self.live_blocks
    }

    /// The number of free blocks currently filed.
    pub fn free_blocks(&self) -> usize {
        self.lists.free_blocks(usize::from(self.class))
    }

    /// The largest free block currently filed, in bytes (0 when the carrier is full).
    ///
    /// Exact: it is [`FreeList::largest_free`]'s maintained record, so
    /// `largest_free() < size` proves this carrier cannot serve a request of `size`.
    pub fn largest_free(&self) -> usize {
        self.lists.largest_free(usize::from(self.class))
    }

    /// Whether the carrier is entirely free: exactly one free block, spanning it all.
    ///
    /// This is the coalescing payoff and the allocator's "destroy this carrier" test
    /// (the legacy `is_first_blk && is_last_blk` arm of `mbc_free`).
    pub fn is_fully_free(&self) -> bool {
        let class = usize::from(self.class);
        self.lists.free_blocks(class) == 1 && self.lists.largest_free(class) == self.carrier.size()
    }

    /// Consumes the sub-allocator and returns the carrier it owns, so the caller can
    /// release the segment (`Carrier::unmap`) or keep the mapping.
    pub(crate) fn into_carrier(self) -> Carrier {
        self.carrier
    }

    /// The total block size a payload request of `size` bytes needs, or `None` when no
    /// block could ever be big enough to be encoded ([`MAX_BLOCK_SIZE`]) or when the
    /// request's byte count overflows.
    ///
    /// The legacy `UMEMSZ2BLKSZ`: the request's size grown by the block header and
    /// rounded up to [`UNIT_SIZE`], but never below [`MIN_MBC_BLOCK_SIZE`].
    fn want_block_size(size: usize) -> Option<usize> {
        let total = BLOCK_HEADER_SIZE.checked_add(size)?;
        if total <= MIN_MBC_BLOCK_SIZE {
            return Some(MIN_MBC_BLOCK_SIZE);
        }
        let want = total.div_ceil(UNIT_SIZE).checked_mul(UNIT_SIZE)?;
        if want > MAX_BLOCK_SIZE {
            return None;
        }
        Some(want)
    }

    /// The total size of the block whose header starts at `block`.
    ///
    /// # Safety
    ///
    /// `block` must be a block start inside this carrier whose header was written by
    /// [`MbcCarrier::store_block`].
    unsafe fn block_size(&self, block: *mut u8) -> usize {
        // SAFETY: per the contract `block` holds a header written by `store_block`.
        unsafe { BlockHeader::load(block) }.size
    }

    /// The flag byte of the block whose header starts at `block`.
    ///
    /// # Safety
    ///
    /// As for [`MbcCarrier::block_size`].
    unsafe fn load_flags(&self, block: *mut u8) -> BlockFlags {
        // SAFETY: per the contract `block` holds a header written by `store_block`.
        BlockFlags::from_bits(unsafe { BlockHeader::load(block) }.flags)
    }

    /// Writes a complete block header (`size` + `flags`) for the block at `block`,
    /// stamping this carrier's class and base address.
    ///
    /// # Safety
    ///
    /// `block` must be a [`UNIT_SIZE`]-aligned block start inside this carrier with room
    /// for a whole header, and no live payload may overlap it.
    unsafe fn store_block(&self, block: *mut u8, size: usize, flags: BlockFlags) {
        let hdr = BlockHdr {
            size,
            payload_offset: 0,
            class: self.class,
            carrier: self.carrier.base().cast::<c_void>(),
            flags: flags.bits(),
        };
        // SAFETY: the caller guarantees `block` is a header-sized, `UNIT_SIZE`-aligned
        // region inside the carrier that no live payload overlaps.
        unsafe { BlockHeader::store(block, hdr) };
    }

    /// Rewrites the block size of the block at `block`, leaving the rest of its header
    /// (flags, class, carrier word) untouched.
    ///
    /// # Safety
    ///
    /// As for [`MbcCarrier::block_size`]; `size` must be the block's new
    /// `UNIT_SIZE`-granular total size.
    unsafe fn store_size(&self, block: *mut u8, size: usize) {
        // SAFETY: per the contract `block` holds a header written by `store_block`.
        let mut hdr = unsafe { BlockHeader::load(block) };
        hdr.size = size;
        // SAFETY: as above — the header words belong to this block.
        unsafe { BlockHeader::store(block, hdr) };
    }

    /// Sets the block's free marker.
    ///
    /// # Safety
    ///
    /// As for [`MbcCarrier::block_size`].
    unsafe fn mark_free(&self, block: *mut u8) {
        // SAFETY: per the contract `block` holds a header written by `store_block`.
        let mut flags = unsafe { self.load_flags(block) };
        flags.set_free();
        // SAFETY: as above.
        unsafe { self.store_flags(block, flags) };
    }

    /// Sets the block's last-in-carrier marker.
    ///
    /// # Safety
    ///
    /// As for [`MbcCarrier::block_size`].
    unsafe fn set_last(&self, block: *mut u8) {
        // SAFETY: per the contract `block` holds a header written by `store_block`.
        let mut flags = unsafe { self.load_flags(block) };
        flags.set_last_in_carrier();
        // SAFETY: as above.
        unsafe { self.store_flags(block, flags) };
    }

    /// Clears the block's last-in-carrier marker.
    ///
    /// # Safety
    ///
    /// As for [`MbcCarrier::block_size`].
    unsafe fn clear_last(&self, block: *mut u8) {
        // SAFETY: per the contract `block` holds a header written by `store_block`.
        let mut flags = unsafe { self.load_flags(block) };
        flags.clear_last_in_carrier();
        // SAFETY: as above.
        unsafe { self.store_flags(block, flags) };
    }

    /// Marks the block's predecessor as free.
    ///
    /// # Safety
    ///
    /// As for [`MbcCarrier::block_size`].
    unsafe fn set_prev_free(&self, block: *mut u8) {
        // SAFETY: per the contract `block` holds a header written by `store_block`.
        let mut flags = unsafe { self.load_flags(block) };
        flags.set_prev_free();
        // SAFETY: as above.
        unsafe { self.store_flags(block, flags) };
    }

    /// Marks the block's predecessor as allocated.
    ///
    /// # Safety
    ///
    /// As for [`MbcCarrier::block_size`].
    unsafe fn clear_prev_free(&self, block: *mut u8) {
        // SAFETY: per the contract `block` holds a header written by `store_block`.
        let mut flags = unsafe { self.load_flags(block) };
        flags.clear_prev_free();
        // SAFETY: as above.
        unsafe { self.store_flags(block, flags) };
    }

    /// Rewrites only the flag byte of the block at `block`.
    ///
    /// # Safety
    ///
    /// As for [`MbcCarrier::block_size`].
    unsafe fn store_flags(&self, block: *mut u8, flags: BlockFlags) {
        // SAFETY: per the contract `block` holds a header written by `store_block`.
        let mut hdr = unsafe { BlockHeader::load(block) };
        hdr.flags = flags.bits();
        // SAFETY: as above — the header words belong to this block and no live payload
        // overlaps them.
        unsafe { BlockHeader::store(block, hdr) };
    }
}

/// The payload address of the block whose header starts at `block`.
///
/// Pure address arithmetic (the game geometry `crate::block`/`crate::free_list` share:
/// the payload starts [`BLOCK_HEADER_SIZE`] bytes into the block), so it is safe Rust.
fn payload_of(block: *mut u8) -> *mut u8 {
    block.wrapping_add(BLOCK_HEADER_SIZE)
}

/// The block start whose payload is `payload`.
///
/// The inverse of [`payload_of`]; pure address arithmetic, hence safe Rust.
fn block_of_payload(payload: *mut u8) -> *mut u8 {
    payload.wrapping_sub(BLOCK_HEADER_SIZE)
}

/// Reads the one-word footer immediately before the block at `block`: the **size of the
/// physically previous block** (the legacy `PREV_BLK_SZ`).
///
/// # Safety
///
/// The block at `block` must have its `prev_free` marker set, so the footer word before
/// it was written by [`write_footer`] (and `block` must not be the carrier's first
/// block).
unsafe fn read_prev_size(block: *mut u8) -> usize {
    // SAFETY: the caller guarantees the footer immediately before `block` holds the
    // predecessor's size — `UNIT_SIZE`-aligned payloads make the footer word aligned.
    unsafe { block.sub(WORD_SIZE).cast::<usize>().read() }
}

/// Writes the one-word footer at the **end** of the free block at `block`, recording its
/// total size (the legacy `SET_BLK_SZ_FTR`); the footered word is the one immediately
/// before the block's successor.
///
/// # Safety
///
/// `block` must be a free block inside this carrier that is not the last block, with a
/// `UNIT_SIZE`-granular total size `size >= MIN_MBC_BLOCK_SIZE`, so the footer word is
/// inside the block and above the intrusive link words.
unsafe fn write_footer(block: *mut u8, size: usize) {
    debug_assert!(size >= MIN_MBC_BLOCK_SIZE && size.is_multiple_of(UNIT_SIZE));
    // SAFETY: per the contract `size` is the block's own extent, so the word at
    // `block + size - WORD_SIZE` is inside the block and inside the carrier.
    unsafe { block.add(size - WORD_SIZE).cast::<usize>().write(size) };
}

#[cfg(test)]
mod tests;