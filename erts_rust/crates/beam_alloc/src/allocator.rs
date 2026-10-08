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

//! The allocator seam: the [`Allocator`] trait, its shared handle [`AllocRef`], the
//! [`AllocatorConfig`] builder and the concrete [`CarrierAllocator`].
//!
//! This is the **frozen cross-crate contract** of the crate. Dependents
//! (`beam_codec`, `beam_ets`, `beam_gc`, `beam_message`, `beam_process`) allocate
//! through an [`AllocRef`] and never touch carriers, blocks or free lists directly,
//! so the carrier framework can evolve behind this interface.
//!
//! # The carrier model
//!
//! All mutation is serialised through one [`parking_lot::Mutex`]. Carriers come from
//! the OS (`mmap`) and are page-aligned; there are three allocation paths, split the
//! way the legacy `mb_alloc`/`sb_alloc` split them — "this request may share a
//! carrier" versus "this request gets a carrier of its own":
//!
//! * **Small / slab.** A request at or below
//!   [`SizeClasses::large_threshold`](crate::size_class::SizeClasses::large_threshold)
//!   whose alignment is at most [`MIN_ALIGN`] is served from its size class. Each
//!   multi-block carrier is dedicated to one class, subdivided into equal blocks
//!   (each preceded by a [`crate::block::BlockHdr`]) and threaded on that class's
//!   intrusive free list. When a class's list runs dry a new carrier is mapped;
//!   drained carriers are retained (a documented M1 retention policy).
//! * **Mid / variable-size multi-block carrier.** A request above the size-class
//!   ladder, no more aligned than [`MIN_ALIGN`] and *below* the single-block-carrier
//!   threshold
//!   ([`CarrierPolicy::small_block_carrier_threshold`](crate::policy::CarrierPolicy::small_block_carrier_threshold),
//!   `+M<alloc>sbct`, 512 KiB by default) is carved out of a
//!   [`crate::mbc::MbcCarrier`]: a carrier whose free space is split on demand and
//!   whose blocks coalesce again on free. The allocator keeps a pool of them, so many
//!   mid-size requests share one carrier (the legacy `mb_alloc`). A carrier that has
//!   coalesced back into one free block spanning it is released to the segment
//!   allocator.
//! * **Large / dedicated.** A request at or above the single-block-carrier threshold,
//!   or one more aligned than [`MIN_ALIGN`], gets a dedicated single-block carrier
//!   released again when the block is freed (the legacy `sb_alloc`).
//!
//! Which request takes which path is the configurable carrier policy
//! ([`AllocatorConfig::policy`], the legacy `+M<alloc><opt>` options); it defaults to
//! [`CarrierPolicy::default`](crate::policy::CarrierPolicy::default).
//!
//! Because carriers are page-aligned and alignments above the page size are
//! rejected, a block's alignment padding is a function of the requested alignment
//! alone, so `dealloc`/`realloc` can locate a block's header from the caller's
//! layout. Header errors are *never* trusted blindly: a header that does not look
//! like one this allocator wrote is reported and left alone rather than freed.
//!
//! # Module layout
//!
//! The seam is split into responsibility submodules behind this facade, which keeps
//! the crate paths (`crate::allocator::X`) and the re-exports below unchanged:
//! `trait` carries the [`Allocator`] trait, `config` the [`AllocatorConfig`]
//! builder, `state` the mutex-guarded `AllocatorState`, `core` the concrete
//! [`CarrierAllocator`], and `tests` the in-module test module. The shared shape
//! helpers (`padding_for`, `probe_alignments`, `plausible_header`) and the
//! report-name helpers stay here, because both the trait's defaults and the concrete
//! allocator use them.
//!
//! `unsafe` is confined to explicitly-marked functions in the `core`/`state`
//! submodules and in the header-plausibility helper below.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;

use crate::block::{
    BlockFlags, BlockHdr, BlockHeader, ALL_FLAGS, BLOCK_HEADER_SIZE, CLASS_NONE, MAX_BLOCK_SIZE,
    MAX_CLASS_COUNT, UNIT_SIZE,
};
use crate::carrier::os_page_size;
use crate::types::AllocatorId;

mod config;
mod core;
mod state;
mod r#trait;

#[cfg(test)]
mod tests;

pub use self::config::{AllocatorConfig, AllocatorConfigBuilder};
pub use self::core::CarrierAllocator;
pub use self::r#trait::Allocator;

/// Byte alignment of a [`beam_term::prelude::Eterm`] word
/// (`align_of::<Eterm>()`).
pub const WORD_ALIGN: usize = 8;

/// The minimum alignment guaranteed for every allocation (`UNIT_SIZE`).
pub const MIN_ALIGN: usize = UNIT_SIZE;

/// A cheaply-clonable shared handle to an [`Allocator`].
///
/// Dependents hold this (not a borrow) so an allocator can outlive the call that
/// created it and be shared across threads without a lifetime parameter on every
/// structure.
pub type AllocRef = Arc<dyn Allocator>;

/// The alignment padding between the fixed block header and the payload for a
/// request aligned to `align`.
///
/// Only meaningful for `align <= os_page_size()` (which the allocator enforces):
/// a carrier base is page-aligned, so any smaller power-of-two alignment divides
/// it and the padding depends on `align` alone — `align - BLOCK_HEADER_SIZE` for a
/// larger-than-header alignment, and none otherwise. That is what makes a block
/// locatable from the caller's layout on free/realloc.
pub(super) fn padding_for(align: usize) -> usize {
    if align <= UNIT_SIZE {
        0
    } else {
        align - UNIT_SIZE
    }
}

/// The alignments [`Allocator::dealloc_unsized`] probes, in order.
///
/// [`MIN_ALIGN`] comes first and stands in for every alignment at or below it (they
/// all have a zero payload offset); each following entry is the next power of two up
/// to and including [`os_page_size`], the largest alignment the carrier model serves.
/// The sequence is finite — `1 + log2(os_page_size() / MIN_ALIGN)` items — and its
/// entries have pairwise distinct payload offsets ([`padding_for`]), which is what
/// keeps the probe from ever matching one block twice.
pub(super) fn probe_alignments() -> impl Iterator<Item = usize> {
    let page = os_page_size();
    let mut next = MIN_ALIGN;
    std::iter::from_fn(move || {
        if next > page {
            return None;
        }
        let align = next;
        next = next.saturating_mul(2);
        Some(align)
    })
}

/// The `prev_free` block-*state* bit ([`BlockFlags::set_prev_free`]), read through
/// `block.rs`'s public accessor pair rather than re-declaring the literal here.
///
/// It is deliberately **outside** [`ALL_FLAGS`] (see `crate::block`'s module docs):
/// "my physical predecessor is free" is a per-block *state*, not block taxonomy. The
/// variable-size multi-block carrier path (`crate::mbc`) sets it on a **live** block
/// whenever that block's predecessor is free (it is what lets the block find the
/// predecessor's footer), so [`plausible_header`] has to tolerate it — otherwise
/// `Self::locate` would refuse to recognise a legitimately live mid-size block and
/// [`Allocator::dealloc`] would refuse to free it, leaking it.
pub(super) const PREV_FREE_BIT: u8 = {
    let mut flags = BlockFlags::empty();
    flags.set_prev_free();
    flags.bits()
};

/// Whether `header` looks like one this crate's carrier allocator wrote for a payload
/// at padding `offset`.
pub(super) fn plausible_header(header: &BlockHdr, offset: usize) -> bool {
    let flags = BlockFlags::from_bits(header.flags);
    header.size % UNIT_SIZE == 0
        && header.size >= BLOCK_HEADER_SIZE + offset
        && header.size <= MAX_BLOCK_SIZE
        && header.payload_offset == offset
        // Exactly one flag bit beyond the taxonomy union is tolerated: `prev_free`,
        // the block-state bit a live variable-size-carrier block carries when its
        // predecessor is free (see [`PREV_FREE_BIT`]). This widens acceptance by
        // exactly that one bit and leaves [`ALL_FLAGS`] (and its compile-time
        // assertion) untouched.
        && header.flags & !(ALL_FLAGS | PREV_FREE_BIT) == 0
        && (header.class == CLASS_NONE) == flags.is_large()
        && (header.class == CLASS_NONE || usize::from(header.class) < MAX_CLASS_COUNT)
}

/// The module-level form of the header plausibility check, reading the header word at
/// `block_start`.
///
/// Used by the default [`Allocator::dealloc_unsized`] probe (the concrete
/// [`CarrierAllocator`] reaches the same predicate through its own `locate`).
pub(super) fn plausible_header_at(block_start: *mut u8, offset: usize) -> bool {
    // SAFETY: `block_start` is where this crate's carrier allocator puts the header of
    // a block allocated with the candidate alignment, so the 16-byte read is in-bounds
    // for any block *this crate* created. The caller's contract is that `block_start`
    // is a plausible address (a pointer this allocator could have produced); a header
    // that is not one of ours is rejected by the predicate below instead of trusted,
    // and a caller that violates the contract yields an unverifiable address that the
    // predicate merely declines to recognise.
    let header = unsafe { BlockHeader::load(block_start) };
    plausible_header(&header, offset)
}

/// The [`AllocatorId`] whose declared name is `name`, if any.
///
/// The registry names every allocator after its id ([`AllocatorId::as_str`]), so this
/// is how an allocator recovers "which declared allocator am I?" for the
/// `=allocator:<name>` report and for its allocation-type listing.
pub(super) fn allocator_id_for_name(name: &str) -> Option<AllocatorId> {
    AllocatorId::ALL.into_iter().find(|id| id.as_str() == name)
}

/// Resolves an allocator's runtime `name` to the `&'static str` an
/// [`AllocatorInfo`](crate::stats::AllocatorInfo) carries.
///
/// The report stores its allocator name as a `&'static str` (it is compared against
/// the declared allocator identifiers), while an allocator's configured name is a
/// runtime `String`. A name that matches a declared [`AllocatorId`] — every allocator
/// the registry builds — resolves to that id's already-`'static` name and allocates
/// nothing. Any other name (only a directly-constructed test allocator) is interned
/// **once** in a process-global table, so repeated report snapshots neither leak
/// repeatedly nor grow without bound.
pub(super) fn report_name(name: &str) -> &'static str {
    if let Some(id) = allocator_id_for_name(name) {
        return id.as_str();
    }
    static INTERNED: LazyLock<Mutex<HashSet<&'static str>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    let mut interned = INTERNED.lock();
    if let Some(&known) = interned.get(name) {
        return known;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    interned.insert(leaked);
    leaked
}
