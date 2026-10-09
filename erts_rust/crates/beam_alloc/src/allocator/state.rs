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

//! The mutable, mutex-guarded state of a
//! [`CarrierAllocator`](super::CarrierAllocator): the mapped carriers, the two
//! multi-block carrier pools (slab + variable-size) and the instrumentation counters.

use std::collections::HashMap;

use beam_common::Result;

use crate::block::{BlockHeader, BLOCK_HEADER_SIZE};
use crate::carrier::Carrier;
use crate::free_list::FreeList;
use crate::mbc::MbcCarrier;
use crate::stats::Counters;
use crate::types::TypeId;

/// The mutable, mutex-guarded state of a
/// [`CarrierAllocator`](super::CarrierAllocator).
#[derive(Default)]
pub(super) struct AllocatorState {
    /// Carriers currently mapped. Elements are held **by value**: a block records its
    /// carrier's *segment base* (an `mmap` pointer recovered through `by_base` below),
    /// never the address of the `Carrier` value itself, so nothing depends on an
    /// element's address surviving a later `Vec` reallocation.
    pub(super) carriers: Vec<Carrier>,
    /// `carrier base address -> index in `carriers``, for O(1) block→carrier
    /// recovery on the alloc/free paths.
    by_base: HashMap<usize, usize>,
    /// The per-size-class free-block lists.
    pub(super) free_lists: FreeList,
    /// The pooled **variable-size** multi-block carriers ([`MbcCarrier`]) that serve
    /// requests above the size-class ladder and below the policy's single-block-carrier
    /// threshold. Held by value like [`AllocatorState::carriers`]: a block records the
    /// carrier's *segment base*, not the value's own address, so a `Vec` reallocation is
    /// harmless (`mbc_by_base` below recovers the index from that base).
    pub(super) mbcs: Vec<MbcCarrier>,
    /// `variable-size carrier base -> index in `mbcs``, for the O(1) dispatch key the
    /// free path needs.
    ///
    /// This exact-base lookup is the **only** correct discriminator between the two
    /// multi-block pools: a variable-size carrier's blocks record `class == MBC_CLASS`,
    /// which is also a valid size-class index, so the class word cannot tell them
    /// apart.
    mbc_by_base: HashMap<usize, usize>,
    /// Instrumentation counters.
    pub(super) counters: Counters,
    /// Total bytes held in mapped carriers (the footprint that `max_footprint`
    /// caps; mirrors `Counters::carrier_bytes`).
    pub(super) mapped_bytes: usize,
}

impl AllocatorState {
    /// Installs `carrier` and returns its index.
    pub(super) fn insert_carrier(&mut self, carrier: Carrier) -> usize {
        let index = self.carriers.len();
        self.by_base.insert(carrier.base() as usize, index);
        self.carriers.push(carrier);
        index
    }

    /// The index of the carrier whose base address is `base`.
    pub(super) fn carrier_index(&self, base: usize) -> Option<usize> {
        self.by_base.get(&base).copied()
    }

    /// Installs the variable-size multi-block carrier `mbc` in the pool and returns its
    /// index.
    pub(super) fn insert_mbc(&mut self, mbc: MbcCarrier) -> usize {
        let index = self.mbcs.len();
        self.mbc_by_base.insert(mbc.base() as usize, index);
        self.mbcs.push(mbc);
        index
    }

    /// The index of the pooled variable-size carrier whose base address is `base`.
    pub(super) fn mbc_index(&self, base: usize) -> Option<usize> {
        self.mbc_by_base.get(&base).copied()
    }

    /// Removes and returns the pooled variable-size carrier whose base address is
    /// `base`.
    pub(super) fn take_mbc(&mut self, base: usize) -> Option<MbcCarrier> {
        let index = self.mbc_by_base.remove(&base)?;
        if index >= self.mbcs.len() {
            return None;
        }
        let removed = self.mbcs.swap_remove(index);
        debug_assert_eq!(removed.base() as usize, base, "MBC index table out of sync");
        if index < self.mbcs.len() {
            // `swap_remove` moved the last carrier into `index`; fix its record.
            let moved_base = self.mbcs[index].base() as usize;
            self.mbc_by_base.insert(moved_base, index);
        }
        Some(removed)
    }

    /// Carves a block with at least `size` payload bytes out of one of the pooled
    /// variable-size carriers, or `None` when none of them has a big-enough free block.
    ///
    /// The scan is linear in the pool size and takes the **first** carrier that fits.
    /// The legacy instead keeps its carriers on the `mbcgs` size ladder and walks that
    /// ladder (`mbc_alloc` → `get_free_block`); modelling the ladder (and its
    /// grow-a-carrier policy) is a documented gap. With one shared 1 MiB carrier
    /// serving many mid-size requests the pool stays tiny in practice.
    pub(super) fn take_mbc_fit(&mut self, size: usize) -> Option<*mut u8> {
        for mbc in self.mbcs.iter_mut() {
            // SAFETY: every pooled carrier is exclusively owned by this state (it is
            // only reachable through this pool) and the block it hands back is not
            // reachable from anywhere else; the caller releases it exactly once,
            // through the same carrier, from the free path.
            if let Some(payload) = unsafe { mbc.alloc(size) } {
                return Some(payload);
            }
        }
        None
    }
    /// Removes and returns the carrier whose base address is `base`.
    pub(super) fn take_carrier(&mut self, base: usize) -> Option<Carrier> {
        let index = self.by_base.remove(&base)?;
        if index >= self.carriers.len() {
            return None;
        }
        let removed = self.carriers.swap_remove(index);
        debug_assert_eq!(
            removed.base() as usize,
            base,
            "carrier index table out of sync"
        );
        if index < self.carriers.len() {
            // `swap_remove` moved the last carrier into `index`; fix its record.
            let moved_base = self.carriers[index].base() as usize;
            self.by_base.insert(moved_base, index);
        }
        Some(removed)
    }

    /// Records a newly mapped *single-block* carrier (the dedicated carrier of a large
    /// request), which the report counts as an SBC and as part of the aggregate.
    pub(super) fn note_sbc_carrier_up(&mut self, bytes: usize) {
        self.counters.note_sbc_carrier_up(bytes);
        self.mapped_bytes = self.mapped_bytes.saturating_add(bytes);
    }

    /// Records a single-block carrier of `bytes` returned to the OS.
    pub(super) fn note_sbc_carrier_down(&mut self, bytes: usize) {
        self.counters.note_sbc_carrier_down(bytes);
        self.mapped_bytes = self.mapped_bytes.saturating_sub(bytes);
    }

    /// Records a newly mapped *multi-block* carrier (a slab carrier subdivided into one
    /// class's blocks), which the report counts as an MBC and as part of the aggregate.
    pub(super) fn note_mbc_carrier_up(&mut self, bytes: usize) {
        self.counters.note_mbc_carrier_up(bytes);
        self.mapped_bytes = self.mapped_bytes.saturating_add(bytes);
    }

    /// Records a multi-block carrier of `bytes` returned to the OS.
    ///
    /// A slab carrier is retained for the allocator's lifetime (the M1 policy), so the
    /// only live caller is the free path releasing a pooled **variable-size**
    /// multi-block carrier once every block of it has coalesced back into one free
    /// block.
    pub(super) fn note_mbc_carrier_down(&mut self, bytes: usize) {
        self.counters.note_mbc_carrier_down(bytes);
        self.mapped_bytes = self.mapped_bytes.saturating_sub(bytes);
    }
    /// Records a successful allocation of `bytes`, attributing it to `ty` when the
    /// caller asked for per-type accounting.
    ///
    /// The typed counter method performs the aggregate accounting itself, so exactly
    /// one of the two branches runs for a given event: the
    /// `stats.carriers == stats.sbc_carriers + stats.mbc_carriers` invariant and the
    /// `live_blocks`/`live_bytes` totals stay exact.
    pub(super) fn note_alloc_for(&mut self, ty: Option<TypeId>, bytes: usize) {
        match ty {
            Some(ty) => self.counters.note_alloc_typed(ty.index(), bytes),
            None => self.counters.note_alloc(bytes),
        }
    }

    /// Records a free of `bytes`, attributing it to `ty` when the caller asked for
    /// per-type accounting.
    pub(super) fn note_free_for(&mut self, ty: Option<TypeId>, bytes: usize) {
        match ty {
            Some(ty) => self.counters.note_free_typed(ty.index(), bytes),
            None => self.counters.note_free(bytes),
        }
    }

    /// Bumps the per-carrier free-block tally for `base`.
    fn add_free_block(&mut self, base: usize) {
        if let Some(index) = self.carrier_index(base) {
            let free = self.carriers[index].free_count().saturating_add(1);
            self.carriers[index].set_free_count(free);
        }
    }

    /// Drops the per-carrier free-block tally for `base`.
    fn remove_free_block(&mut self, base: usize) {
        if let Some(index) = self.carrier_index(base) {
            let free = self.carriers[index].free_count().saturating_sub(1);
            self.carriers[index].set_free_count(free);
        }
    }

    /// Returns a free block of `class` to its class list and updates the owning
    /// carrier's tally.
    pub(super) fn return_block(
        &mut self,
        class: usize,
        payload: *mut u8,
        carrier_base: usize,
    ) -> Result<()> {
        self.free_lists.reserve(class, 1)?;
        self.add_free_block(carrier_base);
        // SAFETY: `payload` is the payload of a free block of `class` (its header is
        // directly below and intact, since the intrusive link lives in the payload),
        // and the caller guarantees it is not reachable from anywhere else.
        unsafe { self.free_lists.push(class, payload) };
        Ok(())
    }

    /// Takes a free block of `class` off its list, or `None` when it is empty.
    pub(super) fn take_block(&mut self, class: usize) -> Option<*mut u8> {
        // SAFETY: `class` is a class index the caller reserved; the popped node is
        // removed from the list by this call.
        let payload = unsafe { self.free_lists.pop(class) }?;
        // SAFETY: every block this allocator creates gets a header when it is
        // subdivided or freed, and the header sits `BLOCK_HEADER_SIZE` below the
        // payload (the free-list link overwrites the payload, not the header).
        let header = unsafe { BlockHeader::load(payload.sub(BLOCK_HEADER_SIZE)) };
        self.remove_free_block(header.carrier as usize);
        Some(payload)
    }
}

impl Drop for AllocatorState {
    fn drop(&mut self) {
        for mbc in self.mbcs.drain(..) {
            // SAFETY: as for `carriers` below — this state exclusively owns every
            // carrier it mapped and is being dropped, so no block is used afterwards;
            // each variable-size carrier's segment is released exactly once here
            // (through the same `Carrier::unmap` the slab/dedicated carriers go
            // through).
            let carrier = mbc.into_carrier();
            unsafe { carrier.unmap() };
        }
        self.mbc_by_base.clear();
        for carrier in self.carriers.drain(..) {
            // SAFETY: this state exclusively owns every carrier it mapped and is
            // being dropped, so no live block is used afterwards; each mapping is
            // unmapped exactly once here (a `Carrier` has no `Drop` of its own).
            unsafe { carrier.unmap() };
        }
        self.by_base.clear();
    }
}
