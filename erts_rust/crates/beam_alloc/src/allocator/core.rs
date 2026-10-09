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

//! The concrete carrier-backed multi-block allocator: the three allocation paths
//! (slab, variable-size multi-block, dedicated single-block), the block locator, the
//! free/realloc paths and the [`Allocator`] implementation.

use core::alloc::Layout;
use core::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Arc;

use beam_common::Result;
use parking_lot::Mutex;

use crate::block::{
    BlockFlags, BlockHdr, BlockHeader, BLOCK_HEADER_SIZE, CLASS_NONE, MAX_BLOCK_SIZE,
    MAX_CLASS_COUNT, UNIT_SIZE,
};
use crate::carrier::{os_page_size, round_up_unit, Carrier};
use crate::error;
use crate::free_list::FreeList;
use crate::mbc::{MbcCarrier, MBC_CLASS};
use crate::policy::CarrierPolicy;
use crate::stats::{AllocStats, AllocatorInfo, TypeStats};
use crate::types::TypeId;

use super::state::AllocatorState;
use super::{
    allocator_id_for_name, padding_for, plausible_header, probe_alignments, report_name,
    Allocator, AllocatorConfig, AllocRef, MIN_ALIGN,
};

/// The concrete carrier-backed multi-block allocator.
///
/// Construction maps no memory; carriers are mapped lazily on the first allocation.
/// All mutation is serialised through a single [`parking_lot::Mutex`] (the M1
/// concurrency model — thread-specific allocators are a documented gap).
pub struct CarrierAllocator {
    config: AllocatorConfig,
    state: Mutex<AllocatorState>,
}

impl CarrierAllocator {
    /// Creates an allocator from `config`.
    pub fn new(config: AllocatorConfig) -> Result<CarrierAllocator> {
        Ok(CarrierAllocator {
            config,
            state: Mutex::new(AllocatorState::default()),
        })
    }

    /// Consumes this allocator into a shared [`AllocRef`] handle.
    pub fn share(self) -> AllocRef {
        Arc::new(self)
    }

    /// The number of carriers currently mapped (redundant with
    /// [`Allocator::stats`]).
    pub fn carrier_count(&self) -> u64 {
        self.state.lock().counters.snapshot().carriers
    }

    /// The total bytes held in mapped carriers (redundant with
    /// [`Allocator::stats`]).
    pub fn carrier_bytes(&self) -> usize {
        self.state.lock().counters.snapshot().carrier_bytes
    }

    /// Locates the header of the block whose payload is `payload`, given the
    /// alignment the allocation was made with.
    ///
    /// Returns `None` when `align` cannot be served by this allocator (so the block
    /// cannot be located at all) or when the header does not describe a block this
    /// allocator could have written. Callers must then *refuse* to free — the
    /// alternative would be `munmap`ing an address that is not a mapping.
    pub(super) fn locate(payload: *mut u8, align: usize) -> Option<(*mut u8, BlockHdr)> {
        if !align.is_power_of_two() || align > os_page_size() {
            return None;
        }
        let offset = padding_for(align);
        let block_start = payload.wrapping_sub(BLOCK_HEADER_SIZE + offset);
        // SAFETY: `block_start` is where this allocator puts the header of a block
        // handed out with this alignment, so reading the 16-byte header is in-bounds
        // for any block it created. A caller that passes a wrong layout yields an
        // unverifiable address; the plausibility test below rejects such a header
        // instead of trusting it.
        let header = unsafe { BlockHeader::load(block_start) };
        if Self::plausible(&header, offset) {
            Some((block_start, header))
        } else {
            None
        }
    }

    /// Whether `header` looks like one this allocator wrote for a payload at
    /// padding `offset`.
    pub(super) fn plausible(header: &BlockHdr, offset: usize) -> bool {
        plausible_header(header, offset)
    }

    /// Rejects an allocation that would push the mapped footprint past the
    /// configured cap.
    fn check_footprint(&self, state: &AllocatorState, bytes: usize) -> Result<()> {
        if let Some(cap) = self.config.max_footprint {
            if state.mapped_bytes.saturating_add(bytes) > cap {
                return Err(error::carrier_failure(bytes));
            }
        }
        Ok(())
    }

    /// The slab path: serves `size` bytes from the `size <= large_threshold` class
    /// with `MIN_ALIGN`-or-weaker alignment, recording the allocation for `ty`.
    fn alloc_small(
        &self,
        state: &mut AllocatorState,
        size: usize,
        ty: Option<TypeId>,
    ) -> Result<NonNull<u8>> {
        let classes = &self.config.classes;
        let Some(class) = classes.class_for(size) else {
            return Err(error::carrier_failure(size));
        };
        let Some(payload_capacity) = classes.class_size(class) else {
            return Err(error::carrier_failure(size));
        };
        debug_assert!(
            class < MAX_CLASS_COUNT,
            "class index must fit the block header"
        );
        let block_size = BLOCK_HEADER_SIZE.saturating_add(payload_capacity);
        if block_size > MAX_BLOCK_SIZE {
            return Err(error::carrier_failure(block_size));
        }
        state.free_lists.reserve(class, 1)?;
        if state.free_lists.free_blocks(class) == 0 {
            self.map_slab_carrier(state, class, block_size)?;
        }
        let Some(payload) = state.take_block(class) else {
            return Err(error::carrier_failure(block_size));
        };
        let block_start = payload.wrapping_sub(BLOCK_HEADER_SIZE);
        // SAFETY: every slab block has a header, written when the carrier was
        // subdivided or when the block was last freed.
        let previous = unsafe { BlockHeader::load(block_start) };
        let mut flags = BlockFlags::from_bits(previous.flags);
        flags.clear_free();
        let header = BlockHdr {
            size: block_size,
            payload_offset: 0,
            class: class as u16,
            carrier: previous.carrier,
            flags: flags.bits(),
        };
        // SAFETY: `block_start` is this block's header address: inside a live carrier
        // mapping, `UNIT_SIZE`-aligned, and not overlapping the payload handed out
        // below.
        unsafe { BlockHeader::store(block_start, header) };
        state.note_alloc_for(ty, block_size);
        // SAFETY: `payload` points into a live carrier mapping and is non-null (it is
        // derived from a successful `mmap` base).
        Ok(unsafe { NonNull::new_unchecked(payload) })
    }

    /// Maps a new carrier dedicated to `class` and subdivides it into `block_size`
    /// blocks pushed onto the class's free list.
    fn map_slab_carrier(
        &self,
        state: &mut AllocatorState,
        class: usize,
        block_size: usize,
    ) -> Result<()> {
        // A carrier must hold at least one block, so a carrier size smaller than the
        // block is raised to the block size (this is what makes the subdivision
        // below infallible).
        let want = round_up_unit(self.config.carrier_size.max(block_size)).max(UNIT_SIZE);
        self.check_footprint(state, want)?;
        // SAFETY: `alloc_map` hands back exclusive ownership of a fresh mapping;
        // `MIN_ALIGN` is a supported alignment.
        let mut carrier = unsafe { Carrier::alloc_map(want, MIN_ALIGN)? };
        carrier.set_class(None);
        carrier.set_block_count(0);
        carrier.set_free_count(0);
        let mapped = carrier.size();
        state.note_mbc_carrier_up(mapped);
        let index = state.insert_carrier(carrier);
        let class = class as u16;
        let AllocatorState {
            carriers,
            free_lists,
            ..
        } = state;
        carriers[index].subdivide_into(class, block_size, free_lists)
    }

    /// The large path: a dedicated single-block carrier for a request at or above the
    /// single-block-carrier threshold (`+M<alloc>sbct`) or more aligned than
    /// [`MIN_ALIGN`], recording the allocation for `ty`. The alignment padding in front
    /// of the payload is zeroed, so a segment reused from the `mseg` cache cannot leave
    /// a previous owner's bytes where the unsized-dealloc probe would read a candidate
    /// block header.
    fn alloc_large(
        &self,
        state: &mut AllocatorState,
        size: usize,
        align: usize,
        ty: Option<TypeId>,
    ) -> Result<NonNull<u8>> {
        let payload_offset = padding_for(align);
        let needed = BLOCK_HEADER_SIZE
            .saturating_add(payload_offset)
            .saturating_add(size);
        if needed > MAX_BLOCK_SIZE {
            return Err(error::carrier_failure(size));
        }
        // The whole block (header + padding + payload) is one mapping that `munmap`s
        // as a unit when the block is freed, and its size is what the header records.
        let block_size = round_up_unit(needed);
        self.check_footprint(state, block_size)?;
        // SAFETY: `alloc_map` hands back exclusive ownership of a fresh mapping;
        // `align` is a power of two at most the page size (checked by `alloc`).
        let mut carrier = unsafe { Carrier::alloc_map(block_size, align)? };
        debug_assert_eq!(carrier.size(), block_size);
        let base = carrier.base();
        carrier.set_class(None);
        carrier.set_block_count(1);
        carrier.set_free_count(0);
        state.note_sbc_carrier_up(block_size);
        state.insert_carrier(carrier);
        let mut flags = BlockFlags::empty();
        flags.set_large();
        flags.set_last_in_carrier();
        let header = BlockHdr {
            size: block_size,
            payload_offset,
            class: CLASS_NONE,
            carrier: base.cast::<c_void>(),
            flags: flags.bits(),
        };
        // SAFETY: `base` is the start of the mapping this block owns, is
        // `UNIT_SIZE`-aligned, and the header occupies its first 16 bytes only.
        unsafe { BlockHeader::store(base, header) };
        if payload_offset > 0 {
            // Zero the alignment padding between the block header and the payload.
            //
            // `dealloc_unsized` locates a block's header by *probing* candidate
            // alignments and returning on the **first** plausibility match. For an
            // over-aligned block every candidate below the real alignment reads its
            // 16-byte header from `[base + BLOCK_HEADER_SIZE, payload)` — the padding
            // this allocator never writes. Carriers come from the process-wide segment
            // (`mseg`) reuse cache, which hands a released segment back at the *same*
            // address *unzeroed*, so a previous owner's block header (e.g. a size-80
            // slab block of another allocator) can survive in this padding. The probe's
            // first candidate (`align = MIN_ALIGN`, offset 0) would then decode that
            // stale-but-plausible header, "free" the live payload through it (wrong
            // byte accounting, a live payload pushed onto a size-class free list) and
            // leave the real dedicated carrier mapped. Zeroed padding can never pass
            // `plausible_header` (a zero header has `size == 0 < BLOCK_HEADER_SIZE`),
            // so only the real header at `base` can ever match.
            //
            // SAFETY: the padding is the `payload_offset` bytes starting at
            // `base + BLOCK_HEADER_SIZE`, entirely inside the `block_size`-byte mapping
            // this block owns and disjoint from both the 16-byte header (which starts at
            // `base`) and the payload (which starts at `base + BLOCK_HEADER_SIZE +
            // payload_offset`); it is never handed to the caller and never written
            // again for the block's lifetime.
            unsafe { core::ptr::write_bytes(base.add(BLOCK_HEADER_SIZE), 0, payload_offset) };
        }
        state.note_alloc_for(ty, block_size);
        // SAFETY: the payload starts `BLOCK_HEADER_SIZE + payload_offset` bytes into a
        // mapping of `block_size >=` that plus `size` bytes, so it is in-bounds; it is
        // non-null because it derives from a page-aligned mapping base.
        let payload = unsafe { base.add(BLOCK_HEADER_SIZE + payload_offset) };
        Ok(unsafe { NonNull::new_unchecked(payload) })
    }

    /// The shared allocation path behind [`Allocator::alloc`] and
    /// [`Allocator::alloc_typed`].
    ///
    /// `ty` is `None` for the untyped entry point (aggregate accounting only) and
    /// `Some(..)` when the caller asked for per-type attribution; the chosen branch
    /// records the allocation exactly once.
    ///
    /// # Routing (the legacy `mb_alloc` / `sb_alloc` split)
    ///
    /// | request | path |
    /// |---|---|
    /// | `size <= classes.large_threshold()` and `align <= UNIT_SIZE` | [`alloc_small`](Self::alloc_small): the segregated per-class slab carrier |
    /// | `align > UNIT_SIZE` **or** `size >= policy.small_block_carrier_threshold()` | [`alloc_large`](Self::alloc_large): a dedicated single-block carrier |
    /// | anything else (above the ladder, below `sbct`, `MIN_ALIGN`-aligned) | [`alloc_mid`](Self::alloc_mid): a shared variable-size multi-block carrier |
    ///
    /// The first test comes first on purpose: a small, weakly-aligned request is a slab
    /// request even when it is also below `sbct`. Over-aligned requests can never share
    /// a carrier (a carrier's alignment is its *base*'s), and requests at or above
    /// `sbct` are the ones the legacy deliberately gives a carrier of their own.
    fn alloc_impl(&self, layout: Layout, ty: Option<TypeId>) -> Result<NonNull<u8>> {
        let size = layout.size();
        let align = layout.align();
        if !align.is_power_of_two() || align > os_page_size() {
            return Err(error::bad_align(align));
        }
        let mut state = self.state.lock();
        if size <= self.config.classes.large_threshold() && align <= UNIT_SIZE {
            self.alloc_small(&mut state, size, ty)
        } else if align > UNIT_SIZE || self.config.policy.uses_single_block_carrier(size) {
            self.alloc_large(&mut state, size, align, ty)
        } else {
            self.alloc_mid(&mut state, size, ty)
        }
    }

    /// The mid path: serves `size` bytes out of a **variable-size** multi-block carrier
    /// (the legacy `mb_alloc`), recording the allocation for `ty`.
    ///
    /// Reached for a request above the size-class ladder, below the policy's
    /// single-block-carrier threshold and no more aligned than [`MIN_ALIGN`] — the
    /// requests that may share a carrier with their neighbours. A pooled carrier that
    /// can serve the request is reused first; only when none of them can is a fresh
    /// [`MbcCarrier`] mapped (and pooled for the next request).
    fn alloc_mid(
        &self,
        state: &mut AllocatorState,
        size: usize,
        ty: Option<TypeId>,
    ) -> Result<NonNull<u8>> {
        if let Some(payload) = state.take_mbc_fit(size) {
            // SAFETY: `take_mbc_fit` returned the payload of a block carved out of a
            // pooled carrier, so its header (written by `crate::mbc`) sits
            // `BLOCK_HEADER_SIZE` below it and records the block's **total** size — the
            // exact extent the block accounts for, which may exceed `size` (the split
            // rule hands out the whole block when the residue would be unusable).
            let block_size = unsafe { FreeList::block_size(payload) };
            state.note_alloc_for(ty, block_size);
            // SAFETY: the payload lies inside a live carrier mapping and is non-null
            // (it derives from a successful `mmap` base).
            return Ok(unsafe { NonNull::new_unchecked(payload) });
        }

        let carrier_size = self.mbc_size(size);
        if carrier_size > MAX_BLOCK_SIZE {
            return Err(error::carrier_failure(size));
        }
        let cap = self.config.policy.multi_block_carrier_cap();
        if !CarrierPolicy::is_unlimited(cap) && state.mbcs.len() >= cap {
            // `+M<alloc>mmmbc`: the policy caps the number of `mseg_alloc` multi-block
            // carriers. The legacy would fall back to `sys_alloc` beyond it; this crate
            // has no second segment source, so the request fails with the legacy's
            // carrier failure instead of over-mapping.
            return Err(error::carrier_failure(carrier_size));
        }
        self.check_footprint(state, carrier_size)?;
        // SAFETY: `alloc_map` (inside `MbcCarrier::new`) hands back exclusive ownership
        // of a fresh segment; the carrier releases it exactly once, through
        // `into_carrier`/`Drop`. `MIN_ALIGN` is a valid carrier alignment and
        // `MBC_CLASS` is validated before anything is mapped.
        let mut mbc = unsafe { MbcCarrier::new(carrier_size, MIN_ALIGN, MBC_CLASS)? };
        // A fresh carrier is a single free block spanning it and `carrier_size` is at
        // least the block the request needs (see `mbc_size`), so this cannot fail; it is
        // still handled as a typed failure rather than an `unwrap` (never a panic). On
        // failure `mbc` is dropped, handing its segment straight back.
        let Some(payload) = (unsafe { mbc.alloc(size) }) else {
            return Err(error::carrier_failure(carrier_size));
        };
        // SAFETY: as for the pooled case above — the block's header is immediately below
        // the payload and records the block's own total size.
        let block_size = unsafe { FreeList::block_size(payload) };
        state.note_mbc_carrier_up(mbc.carrier_size());
        state.insert_mbc(mbc);
        state.note_alloc_for(ty, block_size);
        // SAFETY: the payload lies inside the carrier just mapped and is non-null.
        Ok(unsafe { NonNull::new_unchecked(payload) })
    }

    /// The size a fresh variable-size multi-block carrier is mapped with for a request
    /// of `size` bytes.
    ///
    /// The preferred size is the configured [`AllocatorConfig::carrier_size`]
    /// (`+M<alloc>mmbcs`) clamped into the policy's `smbcs`..`lmbcs` range
    /// ([`CarrierPolicy::minimum_multi_block_carrier_size`] /
    /// [`CarrierPolicy::effective_largest_mbc_size`]), so with the defaults a 1 MiB
    /// carrier serves many mid-size blocks; a request *larger* than that preferred size
    /// sizes the carrier to the request instead, rounded up to [`UNIT_SIZE`].
    ///
    /// The result is never below the block the request needs (`size` plus the block
    /// header — the `UMEMSZ2BLKSZ` rule `crate::mbc::MbcCarrier` applies internally),
    /// because a fresh carrier holds one free block spanning it: a request at least as
    /// large as the preferred size would otherwise get a carrier one header short of
    /// serving it. The legacy instead grows carriers along its `mbcgs` ladder, which
    /// this crate does not model.
    fn mbc_size(&self, size: usize) -> usize {
        let policy = &self.config.policy;
        let preferred = self.config.carrier_size.clamp(
            policy.minimum_multi_block_carrier_size(),
            policy.effective_largest_mbc_size(),
        );
        round_up_unit(size.max(preferred))
            .max(round_up_unit(size.saturating_add(BLOCK_HEADER_SIZE)))
    }
    /// The shared reallocation path behind [`Allocator::realloc`] and
    /// [`Allocator::realloc_typed`]; `ty` selects the accounting of the alloc + free
    /// pair a growing move performs.
    fn realloc_impl(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
        ty: Option<TypeId>,
    ) -> Result<NonNull<u8>> {
        let old_size = old_layout.size();
        let align = old_layout.align();
        let Some((_block_start, header)) = Self::locate(ptr.as_ptr(), align) else {
            return Err(error::bad_align(align));
        };
        // `locate` guarantees `header.size >= BLOCK_HEADER_SIZE + header.payload_offset`.
        let capacity = header.size - BLOCK_HEADER_SIZE - header.payload_offset;
        if new_size <= capacity {
            // The block's payload capacity is unchanged, so the header (and hence the
            // byte accounting) stays as it is: the caller simply uses less of it. The
            // block keeps its allocation type, so no per-type event is due either.
            self.state.lock().counters.note_realloc();
            return Ok(ptr);
        }
        let new_layout =
            Layout::from_size_align(new_size, align).map_err(|_| error::bad_align(align))?;
        // NOTE: the move performs a real alloc + free, so `alloc_calls`/`free_calls`
        // also advance; `live_bytes` stays exact because both sides use the block's
        // own recorded size.
        let new_ptr = self.alloc_impl(new_layout, ty)?;
        // SAFETY: both blocks are live and owned exclusively by the caller, the copy
        // length fits both (it is at most the old block's capacity and the new
        // block's), and the two blocks are distinct allocations, hence disjoint.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr.as_ptr(), new_ptr.as_ptr(), old_size.min(new_size));
        }
        self.state.lock().counters.note_realloc();
        // SAFETY: `ptr`/`old_layout` are the caller's live allocation on this
        // allocator and it is not used after this call.
        unsafe { self.dealloc_impl(ptr, old_layout, ty) };
        Ok(new_ptr)
    }

    /// The shared free path behind [`Allocator::dealloc`] and
    /// [`Allocator::dealloc_typed`].
    ///
    /// # Dispatch order
    ///
    /// Once the header is located, the block's owning carrier decides the path, and the
    /// **variable-size multi-block pool is checked first**:
    ///
    /// 1. **Variable-size multi-block carrier** — the header's recorded carrier base is
    ///    in [`AllocatorState::mbc_index`]. This lookup has to come first: a
    ///    variable-size carrier's blocks record `class == MBC_CLASS` (0), which is also
    ///    a valid *size-class* index, so the class word cannot discriminate between the
    ///    two multi-block pools. The key is the exact base address, and a base address
    ///    belongs to exactly one mapping, so a slab or dedicated block can never be
    ///    found here and a variable-size block can never fall through to the slab path.
    /// 2. **Dedicated single-block carrier** (`flags.is_large()`): the whole mapping is
    ///    handed back (`take_carrier` + `unmap`).
    /// 3. **Slab carrier**: the block goes back on its size class's free list (with the
    ///    existing class-range and registered-carrier checks).
    ///
    /// # Safety
    ///
    /// Same contract as [`Allocator::dealloc`]: `ptr` must be a live allocation of
    /// this allocator made with `layout`, and must not be used afterwards.
    unsafe fn dealloc_impl(&self, ptr: NonNull<u8>, layout: Layout, ty: Option<TypeId>) {
        let payload = ptr.as_ptr();
        let Some((block_start, header)) = Self::locate(payload, layout.align()) else {
            beam_common::beam_debug!(
                "beam_alloc: refusing to free an unrecognised block at {payload:p}"
            );
            return;
        };
        let flags = BlockFlags::from_bits(header.flags);
        let mut state = self.state.lock();

        // -- 1. The variable-size multi-block carrier pool --------------------------
        if let Some(index) = state.mbc_index(header.carrier as usize) {
            // The block's **own** size, read *before* the free: releasing it rewrites
            // the block's header (and possibly its predecessor's) while coalescing, so
            // this is the only point at which the released block's extent is still
            // recoverable — and the block's own size, never the merged one, is what the
            // accounting owes.
            let block_size = header.size;
            // SAFETY: `locate` found a plausible header at `payload - BLOCK_HEADER_SIZE`
            // whose carrier word names this exact pooled carrier, so `payload` is a live
            // block of it; the caller's contract says it is released exactly once.
            unsafe { state.mbcs[index].dealloc(payload) };
            state.note_free_for(ty, block_size);
            if state.mbcs[index].is_fully_free() {
                // Every block has coalesced back into one free block spanning the
                // carrier — the legacy `is_first_blk && is_last_blk` arm of `mbc_free`.
                // Destroying it is the *only* carrier release modelled: the legacy's
                // partial abandonment (`acul`/`rsbcmt`, which needs live-block
                // relocation) is not implemented.
                if let Some(mbc) = state.take_mbc(header.carrier as usize) {
                    let carrier = mbc.into_carrier();
                    state.note_mbc_carrier_down(carrier.size());
                    // SAFETY: the caller is done with `payload`, the carrier holds no
                    // live block (it is fully free), and it was just removed from the
                    // pool, so its segment is released exactly once here.
                    unsafe { carrier.unmap() };
                }
            }
            return;
        }

        // -- 2. A dedicated single-block carrier -----------------------------------
        if flags.is_large() {
            match state.take_carrier(block_start as usize) {
                Some(carrier) => {
                    // SAFETY: this block owns the whole mapping (nothing else was
                    // carved from it), the caller is done with `ptr`, and the carrier
                    // was just removed from the allocator's table, so the mapping is
                    // unmapped exactly once.
                    unsafe { carrier.unmap() };
                    state.note_sbc_carrier_down(header.size);
                    state.note_free_for(ty, header.size);
                }
                None => beam_common::beam_debug!(
                    "beam_alloc: large block without a registered carrier at {payload:p}"
                ),
            }
            return;
        }
        // -- 3. A slab block -------------------------------------------------------
        let class = usize::from(header.class);
        if class >= self.config.classes.len() {
            beam_common::beam_debug!("beam_alloc: block carries an out-of-range size class");
            return;
        }
        if state.carrier_index(header.carrier as usize).is_none() {
            // A plausible-looking header that names a carrier this allocator does not
            // own: returning the block would put foreign memory on a free list.
            beam_common::beam_debug!("beam_alloc: block claims an unregistered carrier");
            return;
        }
        let mut returned_flags = flags;
        returned_flags.set_free();
        let returned = BlockHdr {
            flags: returned_flags.bits(),
            ..header
        };
        // SAFETY: `block_start` is this block's header location, inside a live carrier
        // mapping, and the block is the caller's exclusively until it is returned here.
        unsafe { BlockHeader::store(block_start, returned) };
        match state.return_block(class, payload, header.carrier as usize) {
            Ok(()) => state.note_free_for(ty, header.size),
            Err(_) => beam_common::beam_debug!(
                "beam_alloc: could not return a block of class {class} to its free list"
            ),
        }
    }
}

impl Allocator for CarrierAllocator {
    fn alloc(&self, layout: Layout) -> Result<NonNull<u8>> {
        self.alloc_impl(layout, None)
    }

    fn alloc_typed(&self, ty: TypeId, layout: Layout) -> Result<NonNull<u8>> {
        self.alloc_impl(layout, Some(ty))
    }

    fn realloc(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>> {
        self.realloc_impl(ptr, old_layout, new_size, None)
    }

    fn realloc_typed(
        &self,
        ty: TypeId,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>> {
        self.realloc_impl(ptr, old_layout, new_size, Some(ty))
    }

    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: forwarded unchanged from the caller.
        unsafe { self.dealloc_impl(ptr, layout, None) };
    }

    unsafe fn dealloc_typed(&self, ty: TypeId, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: forwarded unchanged from the caller.
        unsafe { self.dealloc_impl(ptr, layout, Some(ty)) };
    }

    fn alloc_unsized(&self, ty: TypeId, size: usize) -> Result<NonNull<u8>> {
        let layout =
            Layout::from_size_align(size, MIN_ALIGN).map_err(|_| error::bad_align(MIN_ALIGN))?;
        self.alloc_impl(layout, Some(ty))
    }

    unsafe fn dealloc_unsized(&self, ty: TypeId, ptr: NonNull<u8>) {
        let payload = ptr.as_ptr();
        // The same bounded probe the trait default describes, driven by this
        // allocator's own `locate`/`plausible` header check: `MIN_ALIGN` (payload
        // offset 0, covering every alignment at or below it) followed by each power of
        // two up to the page size. The candidates have pairwise distinct payload
        // offsets, so at most one can match and the block is freed at most once; a
        // pointer nothing recognises is refused.
        for align in probe_alignments() {
            if Self::locate(payload, align).is_some() {
                if let Ok(layout) = Layout::from_size_align(0, align) {
                    // SAFETY: `ptr` is a live block of this allocator (the caller's
                    // contract) whose header `locate` has just accepted for `align`.
                    unsafe { self.dealloc_impl(ptr, layout, Some(ty)) };
                }
                return;
            }
        }
        beam_common::beam_debug!(
            "beam_alloc: refusing to free an unrecognised block at {payload:p}"
        );
    }

    fn type_stats(&self, ty: TypeId) -> TypeStats {
        // The per-type table lives in the allocator's counters (a mismatched typed
        // free saturates at zero rather than corrupting it), so the snapshot is the
        // only reader: `AllocStats::type_stats` is the accessor for one slot.
        self.state.lock().counters.snapshot().type_stats(ty.index())
    }

    fn allocator_info(&self) -> AllocatorInfo {
        let stats = self.stats();
        let mut info = AllocatorInfo::from_stats(report_name(self.name()), &stats);
        // The legacy `=allocator:<name>` section reports the allocator's option items
        // (`option sbct: 524288`, the `options` proplist of
        // `erlang:system_info({allocator, A})`) before its per-type rows. They describe
        // the configured policy (`+M<alloc><opt>`), not the counters — see
        // `AllocatorInfo::push_policy` for which of them this implementation really
        // honours.
        info.push_policy(self.config.policy());
        // A registry allocator is named after its `AllocatorId`, which is what tells it
        // which allocation types are bound to it; a directly-constructed allocator has
        // no id and reports only its per-allocator items.
        if let Some(id) = allocator_id_for_name(self.name()) {
            for ty in crate::types::types_for_allocator(id) {
                info.push_type(ty.name(), stats.type_stats(ty.index()));
            }
        }
        info
    }

    fn stats(&self) -> AllocStats {
        self.state.lock().counters.snapshot()
    }

    fn name(&self) -> &str {
        &self.config.name
    }
}
