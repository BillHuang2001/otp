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

//! The mmap-backed memory-segment allocator (the legacy `erts_mseg` surface): the
//! public [`MsegAllocator`] handle, its map/free/realloc entry points and the
//! `+MM`-aware cache/limit bookkeeping they drive.

use core::ptr;
use std::sync::Arc;

use beam_common::{BeamError, Result};
use parking_lot::Mutex;

use super::config::{MsegConfig, MsegInfo, SegmentKind};
use super::header::{entry_for_payload, CacheEntry, MsegHeader, Segment};
use super::mapping::{map_aligned, SUPER_ALIGNED_SIZE};
use super::state::{MsegInner, MsegState};
use crate::error;

/// The version string the legacy segment allocator reports (`ERTS_MSEG_VSN_STR`).
pub const MSEG_VERSION: &str = "0.9";

/// The mmap-backed memory-segment allocator (legacy `erts_mseg`).
///
/// Cheap to clone: clones share one set of counters, one reuse cache and one mutex,
/// so all of them hand out (and take back) segments of the same pool.
#[derive(Clone)]
pub struct MsegAllocator {
    state: Arc<MsegState>,
}

impl core::fmt::Debug for MsegAllocator {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let info = self.stats();
        f.debug_struct("MsegAllocator")
            .field("config", &self.config())
            .field("segments", &info.segments)
            .field("mapped_segments", &info.mapped_segments)
            .field("mapped_bytes", &info.mapped_bytes)
            .finish()
    }
}

impl MsegAllocator {
    /// The version string the legacy segment allocator reports
    /// (`ERTS_MSEG_VSN_STR`).
    pub const VERSION: &'static str = MSEG_VERSION;

    /// Creates an allocator from `config` (normalising it the way `erts_mseg_init`
    /// does).
    ///
    /// This never touches the OS: no memory is mapped until the first
    /// [`map`](MsegAllocator::map).
    pub fn new(config: MsegConfig) -> MsegAllocator {
        MsegAllocator {
            state: Arc::new(MsegState {
                inner: Mutex::new(MsegInner::new(config.normalised())),
            }),
        }
    }

    /// Creates an allocator with the legacy default configuration.
    pub fn with_defaults() -> MsegAllocator {
        MsegAllocator::new(MsegConfig::default())
    }

    /// The effective configuration (after normalisation).
    pub fn config(&self) -> MsegConfig {
        self.state.inner.lock().config.clone()
    }

    /// Maps a segment of at least `size` bytes whose start is `align`-aligned,
    /// classified as a multi-block carrier segment.
    ///
    /// `align` must be a non-zero power of two; any power of two is supported, the
    /// ones above the page size by over-mapping and trimming (see [`map_aligned`]).
    /// A `size` of 0 is valid: the segment then has a page-granular capacity but
    /// reports a usable size of 0.
    ///
    /// # Errors
    ///
    /// * [`BeamError::BadArg`] for an alignment of zero or a non-power-of-two.
    /// * `BeamError::Alloc` when the extent overflows, the OS cannot map the memory,
    ///   or a configured `+MM` limit would be exceeded.
    pub fn map(&self, size: usize, align: usize) -> Result<Segment> {
        self.map_kind(size, align, SegmentKind::MultiBlock)
    }

    /// Like [`map`](MsegAllocator::map), classified as a single-block carrier
    /// segment (bounded by `max_sbc_carriers`).
    pub fn map_sbc(&self, size: usize, align: usize) -> Result<Segment> {
        self.map_kind(size, align, SegmentKind::SingleBlock)
    }

    /// Like [`map`](MsegAllocator::map), with an explicit [`SegmentKind`].
    ///
    /// This is the legacy `erts_mseg_alloc_opt` entry point: it runs the same
    /// cache/limit checks and honours the same configuration.
    ///
    /// # Errors
    ///
    /// As [`map`](MsegAllocator::map).
    pub fn map_kind(&self, size: usize, align: usize, kind: SegmentKind) -> Result<Segment> {
        if !align.is_power_of_two() {
            return Err(error::bad_align(align));
        }
        let mut inner = self.state.inner.lock();
        inner.calls.alloc += 1;

        if inner.config.cache && inner.config.max_cache_size > 0 {
            if let Some(entry) = inner.take_cached(size, align, kind) {
                inner.note_alloc(entry.size);
                MsegHeader::new(&entry).store(entry.payload);
                return Ok(Segment::new(Arc::clone(&self.state), entry));
            }
        }

        inner.reserve(kind, size)?;
        let mapped = map_aligned(size, align)?;
        inner.note_mapping(mapped.map_len, kind);
        let entry = CacheEntry {
            base: mapped.base,
            map_len: mapped.map_len,
            payload: mapped.payload,
            size,
            align,
            kind,
        };
        inner.note_alloc(size);
        MsegHeader::new(&entry).store(entry.payload);
        Ok(Segment::new(Arc::clone(&self.state), entry))
    }

    /// Returns `seg` to the allocator (legacy `erts_mseg_dealloc`): into the reuse
    /// cache if it has room, otherwise straight to the OS.
    ///
    /// Equivalent to dropping the segment; it exists because the legacy API has an
    /// explicit deallocation entry point.
    pub fn free(&self, seg: Segment) {
        drop(seg);
    }

    /// Resizes `seg` to `new_size` usable bytes, preserving its contents (legacy
    /// `erts_mseg_realloc`).
    ///
    /// The segment is grown or shrunk **in place** when its retained mapping already
    /// covers the new size; otherwise a fresh segment is mapped, the contents are
    /// copied and the old segment is released. Shrinking additionally returns the
    /// freed pages to the OS when the configured shrink thresholds say the shrink is
    /// worth it (`abs_shrink_threshold` / `rel_shrink_threshold`; the legacy defaults
    /// shrink whenever a whole page can be released).
    ///
    /// A shrink always takes effect logically — [`Segment::size`] becomes `new_size` —
    /// but pages the thresholds consider "not worth releasing" stay mapped (the legacy
    /// `mseg_realloc` instead refuses the shrink and reports the old size; keeping the
    /// caller's requested size while retaining the address space is the behaviour this
    /// module documents, and [`Segment::capacity`] still exposes the retained room).
    ///
    /// `old_size` is the size the caller believes `seg` has; it must match
    /// [`Segment::size`], otherwise the call is a caller bug and the segment is left
    /// untouched.
    ///
    /// # Errors
    ///
    /// [`BeamError::BadArg`] on a size mismatch, and the same errors as
    /// [`map`](MsegAllocator::map) when the growth needs a fresh mapping. On error
    /// `seg` is unchanged and still owns its memory.
    pub fn realloc(&self, seg: &mut Segment, old_size: usize, new_size: usize) -> Result<()> {
        if old_size != seg.entry.size {
            return Err(BeamError::BadArg);
        }
        self.state.inner.lock().calls.realloc += 1;

        if new_size <= seg.entry.capacity() {
            let trim = new_size < seg.entry.size
                && self.shrink_is_worthwhile(old_size, old_size - new_size);
            return self.state.inner.lock().retarget(seg, new_size, trim);
        }

        // Growth beyond the mapping: map a fresh segment, copy, release the old one.
        // `map_kind` cannot return `seg` itself (a live segment is never cached), so
        // the copy can never overlap its source.
        let fresh = self.map_kind(new_size, seg.entry.align, seg.entry.kind)?;
        let copy_len = seg.entry.size.min(fresh.entry.size);
        // SAFETY: both segments own their mappings exclusively and are distinct, so
        // the two ranges of `copy_len` bytes are valid, initialised and disjoint.
        unsafe {
            ptr::copy_nonoverlapping(seg.as_ptr() as *const u8, fresh.as_ptr(), copy_len);
        }
        self.state.inner.lock().calls.recreate += 1;
        let old = core::mem::replace(seg, fresh);
        drop(old);
        Ok(())
    }

    /// Whether the legacy shrink thresholds want a shrink of `shrink` bytes on a
    /// segment of `old_size` bytes to actually release memory (`+M<alloc>rsbcst`).
    fn shrink_is_worthwhile(&self, old_size: usize, shrink: usize) -> bool {
        let config = self.state.inner.lock().config.clone();
        shrink >= config.abs_shrink_threshold
            || (shrink as u128) * 100 >= config.rel_shrink_threshold as u128 * old_size as u128
    }

    /// Returns `seg` to the allocator, reconstructing it from its payload pointer
    /// (legacy `erts_mseg_dealloc`, which takes a bare `void *`).
    ///
    /// # Safety
    ///
    /// `payload` must be the payload of a *live* segment mapped by this (or a cloned)
    /// allocator and must not be used afterwards. The segment handle for it must not
    /// be alive any more (otherwise the mapping is released twice), and the header
    /// bytes in front of `payload` must still be intact.
    pub unsafe fn free_ptr(&self, payload: *mut u8) -> Result<()> {
        if payload.is_null() {
            return Ok(());
        }
        let entry = unsafe { entry_for_payload(payload as usize)? };
        self.state.inner.lock().release(entry);
        Ok(())
    }

    /// Rebuilds the [`Segment`] handle of a live segment from its payload pointer.
    ///
    /// # Safety
    ///
    /// `payload` must be the payload of a *live* segment mapped by this (or a cloned)
    /// allocator; the returned handle takes ownership and must be the only one (see
    /// [`free_ptr`](MsegAllocator::free_ptr)).
    pub unsafe fn from_raw(&self, payload: *mut u8) -> Result<Segment> {
        let entry = unsafe { entry_for_payload(payload as usize)? };
        Ok(Segment::new(Arc::clone(&self.state), entry))
    }

    /// A point-in-time snapshot of the counters.
    pub fn stats(&self) -> MsegInfo {
        self.state.inner.lock().snapshot()
    }

    /// Starts a new "max period": the period maxima are set to the current values,
    /// the "ever" maxima are left alone (legacy `begin_new_max_period`).
    pub fn reset_max(&self) {
        let mut inner = self.state.inner.lock();
        inner.max_live = inner.live;
        inner.max_bytes = inner.live_bytes;
    }

    /// Returns every cached segment to the OS (legacy `erts_mseg_clear_cache`).
    pub fn clear_cache(&self) {
        let mut inner = self.state.inner.lock();
        inner.calls.clear_cache += 1;
        inner.purge_cache();
    }

    /// Returns the oldest cached segment to the OS (legacy `erts_mseg_check_cache`)
    /// and reports how many segments are still cached.
    pub fn check_cache(&self) -> u64 {
        let mut inner = self.state.inner.lock();
        inner.calls.check_cache += 1;
        inner.drop_one_cached()
    }

    /// The number of segments currently mapped from the OS (handed out plus cached),
    /// the legacy `erts_mseg_no()`.
    pub fn segment_count(&self) -> u64 {
        self.state.inner.lock().mapped
    }

    /// The byte size the legacy `erts_mseg_unit_size()` reports: the super-alignment
    /// granularity.
    pub fn unit_size() -> usize {
        SUPER_ALIGNED_SIZE
    }
}
