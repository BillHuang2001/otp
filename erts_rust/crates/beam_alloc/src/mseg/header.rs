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

//! The private segment control header, the cache entry describing one mapping, and
//! the owned [`Segment`] handle handed to callers.

use core::ptr;
use core::slice;
use std::sync::Arc;

use beam_common::{BeamError, Result};

use super::config::SegmentKind;
use super::mapping::{align_up, page_size};
use super::state::MsegState;

/// The number of bytes reserved in front of every segment payload for the segment
/// control header ([`MsegHeader`]).
///
/// It is a multiple of 8 (so a header placed below a payload is word-aligned) and
/// large enough for the whole header struct.
pub const MSEG_HEADER_SIZE: usize = 64;

/// The private control header stored immediately in front of every segment payload.
///
/// It records everything needed to release a segment from its payload pointer alone:
/// the retained mapping base/length, the payload, the usable size, the requested
/// alignment and the segment kind. The struct is smaller than [`MSEG_HEADER_SIZE`]
/// so the reserved area has room to grow.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MsegHeader {
    magic: u64,
    base: usize,
    map_len: usize,
    payload: usize,
    size: usize,
    align: usize,
    kind: u64,
}

/// The magic word of a segment header (`"MSEG\0\0\0|1"`); a mismatch means the pointer
/// handed to a bare-pointer entry point is not an mseg payload.
const MSEG_MAGIC: u64 = 0x4D53_4547_0000_0001;

// The header must fit in the space reserved for it in front of every payload.
const _: () = assert!(core::mem::size_of::<MsegHeader>() <= MSEG_HEADER_SIZE);

impl MsegHeader {
    pub(super) fn new(entry: &CacheEntry) -> MsegHeader {
        MsegHeader {
            magic: MSEG_MAGIC,
            base: entry.base,
            map_len: entry.map_len,
            payload: entry.payload,
            size: entry.size,
            align: entry.align,
            kind: entry.kind.to_header(),
        }
    }

    /// Writes this header into the [`MSEG_HEADER_SIZE`] bytes in front of
    /// [`MsegHeader::payload`].
    pub(super) fn store(self, payload: usize) {
        debug_assert_eq!(self.payload, payload);
        // SAFETY: `payload` is a live segment payload, so `payload - MSEG_HEADER_SIZE`
        // is inside that segment's retained mapping and is reserved for exactly this
        // header; the store is unaligned-safe and stays inside the reserved bytes.
        unsafe { ptr::write_unaligned((payload - MSEG_HEADER_SIZE) as *mut MsegHeader, self) };
    }

    /// Reads the header recorded in front of the payload at `payload`.
    ///
    /// # Safety
    ///
    /// `payload` must point at (or just past) a live mapping owned by a
    /// [`MsegAllocator`](super::MsegAllocator); the header bytes are dereferenced.
    unsafe fn load(payload: usize) -> Result<MsegHeader> {
        // SAFETY: the caller guarantees `payload` points into a live mseg segment,
        // hence the header bytes in front of it are readable; the load is
        // unaligned-safe.
        let header = unsafe { ptr::read_unaligned((payload - MSEG_HEADER_SIZE) as *const MsegHeader) };
        if header.magic != MSEG_MAGIC || header.payload != payload {
            beam_common::beam_debug!("beam_alloc: mseg header magic mismatch at {payload:#x}");
            return Err(BeamError::BadArg);
        }
        Ok(header)
    }
}

/// One mapping held by the allocator: a live segment, or a cached one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CacheEntry {
    /// The retained mapping base (page-aligned); the address handed to `munmap`.
    pub(super) base: usize,
    /// The retained mapping length; a multiple of the page size.
    pub(super) map_len: usize,
    /// The aligned pointer handed to the caller.
    pub(super) payload: usize,
    /// The usable byte count of the segment.
    pub(super) size: usize,
    /// The alignment the caller requested.
    pub(super) align: usize,
    /// The kind of carrier the segment was obtained for.
    pub(super) kind: SegmentKind,
}

impl CacheEntry {
    /// Bytes of usable memory between the payload and the end of the mapping; always
    /// at least [`size`](CacheEntry::size).
    pub(super) fn capacity(&self) -> usize {
        self.map_len - (self.payload - self.base)
    }

    /// The page-aligned tail of this mapping that lies past `size` usable bytes and
    /// can be returned to the OS, as `(address, length)`; `None` when nothing (a whole
    /// number of pages) can be trimmed.
    pub(super) fn tail_trim(&self, size: usize) -> Option<(usize, usize)> {
        let usable_end = self.payload.checked_add(size)?;
        let keep_end = align_up(usable_end.checked_sub(self.base)?, page_size())?;
        if keep_end >= self.map_len {
            return None;
        }
        Some((self.base + keep_end, self.map_len - keep_end))
    }
}

/// A mapped memory segment: an owned, page-backed range of `size()` usable bytes.
///
/// Dropping the segment returns it to the allocator (and from there to the reuse
/// cache or to the OS), so a segment can never be leaked by an early return. The
/// segment is `Send`: it owns its mapping exclusively and the allocator serialises
/// cache/bookkeeping behind a mutex.
#[derive(Clone)]
pub struct Segment {
    state: Arc<MsegState>,
    pub(super) entry: CacheEntry,
}

// The `Clone` above is deliberately not a public API: see `Segment::clone_for_check`.
impl core::fmt::Debug for Segment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Segment")
            .field("payload", &format_args!("{:#x}", self.entry.payload))
            .field("size", &self.entry.size)
            .field("capacity", &self.entry.capacity())
            .field("align", &self.entry.align)
            .field("kind", &self.entry.kind)
            .finish()
    }
}

impl Segment {
    pub(super) fn new(state: Arc<MsegState>, entry: CacheEntry) -> Segment {
        Segment { state, entry }
    }

    /// The aligned start of the segment: the pointer the caller uses.
    pub fn as_ptr(&self) -> *mut u8 {
        self.entry.payload as *mut u8
    }

    /// The number of usable bytes in the segment (exactly what was requested, except
    /// that a request of 0 is reported as 0 while the mapping still has capacity).
    ///
    /// The legacy `erts_mseg_alloc` reports the *cached* segment's full size when it
    /// reuses one that fits "for free" (`*size_p = csize`); this module always reports
    /// the requested size and leaves the unused remainder visible through
    /// [`capacity`](Segment::capacity) — the caller decides how much of it to use.
    pub fn size(&self) -> usize {
        self.entry.size
    }

    /// The bytes actually available from [`as_ptr`](Segment::as_ptr) to the end of the
    /// retained mapping.
    ///
    /// It is at least [`size`](Segment::size) (page rounding keeps one partial page)
    /// and is what [`MsegAllocator::realloc`](super::MsegAllocator::realloc) can grow
    /// into without remapping.
    pub fn capacity(&self) -> usize {
        self.entry.capacity()
    }

    /// The alignment the segment's payload honours: [`as_ptr`](Segment::as_ptr) is a
    /// multiple of it.
    ///
    /// It is the alignment requested when the segment was obtained, so a segment that
    /// was mapped with a weaker alignment and reused for a stronger one reports the
    /// stronger one (the payload satisfies it — that is the reuse condition).
    pub fn align(&self) -> usize {
        self.entry.align
    }

    /// The kind of carrier this segment was obtained for (see [`SegmentKind`]).
    pub fn kind(&self) -> SegmentKind {
        self.entry.kind
    }

    /// The base address of the retained mapping that backs this segment.
    pub fn mapping_base(&self) -> usize {
        self.entry.base
    }

    /// The length in bytes of the retained mapping that backs this segment.
    pub fn mapping_len(&self) -> usize {
        self.entry.map_len
    }

    /// The number of bytes reserved in front of every payload for the segment header.
    pub const fn header_size() -> usize {
        MSEG_HEADER_SIZE
    }

    /// The first [`size`](Segment::size) bytes of the segment.
    ///
    /// The bytes are readable for the whole lifetime of the segment: `mmap` hands out
    /// zeroed pages and only this segment's owner can write them.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `entry.payload` starts a live mapping of at least `entry.capacity()`
        // bytes that this segment owns exclusively, and `entry.size <= capacity`, so
        // the described range is in-bounds and fully initialised (`mmap` zeroes it).
        unsafe { slice::from_raw_parts(self.entry.payload as *const u8, self.entry.size) }
    }

    /// The first [`size`](Segment::size) bytes of the segment, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as `as_slice`, and `&mut self` proves there is no other borrow of
        // the segment.
        unsafe { slice::from_raw_parts_mut(self.entry.payload as *mut u8, self.entry.size) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        self.state.inner.lock().release(self.entry);
    }
}

/// Reconstructs the cache entry of a live segment from its payload pointer.
///
/// # Safety
///
/// `payload` must point at a live segment payload owned by a
/// [`MsegAllocator`](super::MsegAllocator); the header bytes in front of it are
/// dereferenced.
pub(super) unsafe fn entry_for_payload(payload: usize) -> Result<CacheEntry> {
    let header = unsafe { MsegHeader::load(payload)? };
    let kind = SegmentKind::from_header(header.kind).ok_or(BeamError::BadArg)?;
    Ok(CacheEntry {
        base: header.base,
        map_len: header.map_len,
        payload: header.payload,
        size: header.size,
        align: header.align,
        kind,
    })
}
