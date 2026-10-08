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

//! The [`Allocator`] trait: the object-safe allocation interface every allocation
//! site in the workspace uses, plus its typed (interposition) surface.

use core::alloc::Layout;
use std::ptr::NonNull;

use beam_common::Result;

use crate::block::BLOCK_HEADER_SIZE;
use crate::error;
use crate::stats::{AllocStats, AllocatorInfo, TypeStats};
use crate::types::TypeId;

use super::{
    padding_for, plausible_header_at, probe_alignments, report_name, MIN_ALIGN,
};

/// The allocator interface every allocation site in the workspace uses.
///
/// Implementors must be `Send + Sync + 'static` so the handle can be shared. The
/// trait is object-safe (no generic methods), so it is used as `dyn Allocator`
/// behind [`AllocRef`](super::AllocRef). Failure is return-based: an allocator returns
/// `BeamError::Alloc(..)` on OOM and never panics.
pub trait Allocator: Send + Sync + 'static {
    /// Allocates `layout.size()` bytes aligned to `layout.align()`.
    fn alloc(&self, layout: Layout) -> Result<NonNull<u8>>;

    /// Grows or shrinks an allocation to `new_size` bytes, preserving its contents up
    /// to the smaller of the old and new sizes.
    fn realloc(&self, ptr: NonNull<u8>, old_layout: Layout, new_size: usize)
        -> Result<NonNull<u8>>;

    /// Frees an allocation previously returned by [`alloc`](Allocator::alloc) or
    /// [`realloc`](Allocator::realloc) on *this* allocator.
    ///
    /// # Safety
    ///
    /// `ptr` must have been returned by `alloc`/`realloc` on this allocator, must not
    /// be used afterwards, and `layout` must be the layout it was allocated with.
    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout);

    /// A point-in-time snapshot of this allocator's counters.
    fn stats(&self) -> AllocStats;

    /// The human-readable allocator name (for logs / `system_info`).
    fn name(&self) -> &str;

    /// Allocates `size` bytes with the default alignment ([`MIN_ALIGN`]).
    fn alloc_bytes(&self, size: usize) -> Result<NonNull<u8>> {
        let layout =
            Layout::from_size_align(size, MIN_ALIGN).map_err(|_| error::bad_align(MIN_ALIGN))?;
        self.alloc(layout)
    }

    /// Allocates `layout` and zeroes the whole allocation.
    fn alloc_zeroed(&self, layout: Layout) -> Result<NonNull<u8>> {
        let ptr = self.alloc(layout)?;
        // SAFETY: `alloc` returned a block of at least `layout.size()` bytes that is
        // exclusively ours until it is handed back to `dealloc`, so writing zeros is
        // in-bounds and non-aliasing.
        unsafe { std::ptr::write_bytes(ptr.as_ptr(), 0, layout.size()) };
        Ok(ptr)
    }

    // -- Typed (interposition) surface ----------------------------------------------
    //
    // The methods below are the *typed* counterparts of `alloc`/`realloc`/`dealloc`:
    // they carry the allocation [`TypeId`] the caller is allocating for, so an
    // allocator can attribute the request to the legacy per-type table
    // (`erts_alloc(TYPE, size)` -> the `blocks[<name>]` rows of
    // `erts_alloc:info/0`). Every one of them has a default that forwards to the
    // untyped method, so the trait stays object-safe and an allocator that does not
    // track types (`stats.rs`'s per-type table is optional) needs no change.
    //
    // Untyped and typed calls are *not* interchangeable accounting-wise: an allocator
    // that implements the typed methods must record the type-attributed event once
    // (which, for this crate's carrier allocator, also performs the aggregate
    // accounting), and must never additionally record the untyped aggregate event.

    /// Allocates `layout` for allocation type `ty`.
    ///
    /// The default forwards to [`alloc`](Allocator::alloc) and records nothing about
    /// `ty`; an allocator that keeps a per-type table overrides this (and must then
    /// perform the aggregate accounting itself so an event is recorded exactly once).
    fn alloc_typed(&self, _ty: TypeId, layout: Layout) -> Result<NonNull<u8>> {
        self.alloc(layout)
    }

    /// Grows or shrinks an allocation of type `ty` to `new_size` bytes, preserving its
    /// contents up to the smaller of the old and new sizes.
    ///
    /// The typed counterpart of [`realloc`](Allocator::realloc); the default forwards
    /// to it.
    fn realloc_typed(
        &self,
        _ty: TypeId,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>> {
        self.realloc(ptr, old_layout, new_size)
    }

    /// Frees an allocation of type `ty` previously returned by
    /// [`alloc_typed`](Allocator::alloc_typed) (or [`realloc_typed`](Allocator::realloc_typed))
    /// on *this* allocator.
    ///
    /// The typed counterpart of [`dealloc`](Allocator::dealloc); the default forwards
    /// to it.
    ///
    /// # Safety
    ///
    /// `ptr` must have been returned by `alloc_typed`/`realloc_typed` on this
    /// allocator, must not be used afterwards, and `layout` must be the layout it was
    /// allocated with.
    unsafe fn dealloc_typed(&self, _ty: TypeId, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller upheld `dealloc`'s contract.
        unsafe { self.dealloc(ptr, layout) }
    }

    /// Allocates `size` bytes of allocation type `ty` at the default alignment
    /// ([`MIN_ALIGN`]).
    ///
    /// "Unsized" because the caller supplies only a byte count, not a full
    /// [`Layout`] — the shape the legacy `erts_alloc(TYPE, size)` macro and a driver's
    /// `erl_drv_alloc` have. The default forwards to
    /// [`alloc_bytes`](Allocator::alloc_bytes).
    fn alloc_unsized(&self, _ty: TypeId, size: usize) -> Result<NonNull<u8>> {
        self.alloc_bytes(size)
    }

    /// Frees the block whose payload is `ptr`, knowing only the pointer.
    ///
    /// This is the interposition-friendly inverse of
    /// [`alloc_unsized`](Allocator::alloc_unsized) (`erts_free(TYPE, ptr)` /
    /// `erl_drv_free`): the caller does not — and cannot — pass the original layout,
    /// so the allocator must recover it from the block itself.
    ///
    /// # Default behaviour (a bounded header probe)
    ///
    /// An allocator that keeps a header *in band* (immediately before the payload) can
    /// reconstruct the layout; one that does not cannot identify a bare pointer and
    /// must not free anything. The default therefore probes, in order, the candidate
    /// alignments this crate's carrier model supports — [`MIN_ALIGN`] followed by
    /// every power of two up to and including `os_page_size` — and calls
    /// [`dealloc_typed`](Allocator::dealloc_typed) for the **first** candidate whose
    /// in-band header passes the plausibility check. Two properties make that safe
    /// and cheap:
    ///
    /// * **Bounded.** Only `1 + log2(os_page_size() / MIN_ALIGN)` candidates are
    ///   tried (9 for a 4 KiB page), whatever the alignment the block was allocated
    ///   with.
    /// * **No double free.** The candidate set produces pairwise *distinct* payload
    ///   offsets `padding_for(align)` — `0` for [`MIN_ALIGN`] and `align - UNIT_SIZE`
    ///   for each larger power of two — and the plausibility check requires the
    ///   header's recorded offset to equal the candidate's, so at most one candidate
    ///   can ever describe a given block.
    ///
    /// A pointer that no candidate recognises is left alone (logged, never freed), so
    /// the default never frees a pointer it cannot identify. The probed layout carries
    /// only the recovered alignment (its size is `0`): a layout-less free cannot know
    /// the size, and this crate's [`dealloc`](Allocator::dealloc) derives the whole
    /// block, size included, from the alignment and the recorded header. An allocator
    /// whose `dealloc` needs the exact size must override this method.
    ///
    /// # Safety
    ///
    /// `ptr` must be `null` or a pointer previously returned by
    /// [`alloc_unsized`](Allocator::alloc_unsized)/[`alloc_typed`](Allocator::alloc_typed)
    /// on *this* allocator and not yet freed. A pointer the allocator does not
    /// recognise is refused, but the pointer itself still has to be a value the
    /// allocator could plausibly have produced (in particular, a pointer into memory
    /// this process may read just below).
    unsafe fn dealloc_unsized(&self, ty: TypeId, ptr: NonNull<u8>) {
        let payload = ptr.as_ptr();
        for align in probe_alignments() {
            let offset = padding_for(align);
            let block_start = payload.wrapping_sub(BLOCK_HEADER_SIZE + offset);
            if !plausible_header_at(block_start, offset) {
                continue;
            }
            // SAFETY: `dealloc_typed`'s contract is `ptr` came from a typed allocation
            // on this allocator; the in-band header just identified it as a block
            // allocated with alignment `align`, and it is freed exactly once because
            // no other candidate can share this payload offset.
            if let Ok(layout) = Layout::from_size_align(0, align) {
                unsafe { self.dealloc_typed(ty, ptr, layout) };
            }
            return;
        }
        beam_common::beam_debug!(
            "beam_alloc: refusing to free an unrecognised block at {payload:p}"
        );
    }

    /// The per-*type* counters this allocator has accumulated for allocation type
    /// `ty`.
    ///
    /// The default reports the all-zero value: an allocator that does not keep the
    /// per-type table records nothing. Introspection only — it must not be called on
    /// an allocation hot path.
    fn type_stats(&self, _ty: TypeId) -> TypeStats {
        TypeStats::default()
    }

    /// The allocator's `=allocator:<name>` report section.
    ///
    /// Returns the per-allocator item list of
    /// [`AllocatorInfo::from_stats`](crate::stats::AllocatorInfo::from_stats) over
    /// [`stats`](Allocator::stats). The default adds no policy items and no per-type
    /// rows, because the trait carries neither the configured `+M<alloc><opt>` policy nor
    /// a per-type table; an allocator that has them (this crate's
    /// [`CarrierAllocator`](super::CarrierAllocator)) overrides this to append the option
    /// items ([`push_policy`](crate::stats::AllocatorInfo::push_policy)) and one
    /// [`push_type`](crate::stats::AllocatorInfo::push_type) row set per allocation
    /// type bound to it.
    ///
    /// Introspection only — it snapshots the counters and builds a `Vec`, so it must
    /// never run on an allocation hot path.
    fn allocator_info(&self) -> AllocatorInfo {
        AllocatorInfo::from_stats(report_name(self.name()), &self.stats())
    }
}
