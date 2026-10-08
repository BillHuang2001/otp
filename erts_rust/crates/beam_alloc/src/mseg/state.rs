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

//! The mutex-guarded bookkeeping shared by a segment allocator and every live
//! [`Segment`](super::Segment) it handed out: the reuse cache, the counters and the
//! mapping limits.

use parking_lot::Mutex;

use beam_common::{BeamError, Result};

use super::config::{MsegCalls, MsegConfig, MsegInfo, SegmentKind};
use super::header::{CacheEntry, MsegHeader, Segment};
use super::mapping::os_unmap;
use crate::error;

/// The shared state of a [`MsegAllocator`](super::MsegAllocator).
///
/// It is reference counted because every live [`Segment`] needs it to be able to
/// release itself: the last handle to go away releases the mappings still held in the
/// reuse cache, so dropping an allocator never leaks address space.
pub(super) struct MsegState {
    pub(super) inner: Mutex<MsegInner>,
}

impl Drop for MsegState {
    fn drop(&mut self) {
        // Exclusive access: the `Arc` count reached zero.
        self.inner.get_mut().purge_cache();
    }
}

/// The mutable bookkeeping of a [`MsegAllocator`](super::MsegAllocator), guarded by
/// [`MsegState::inner`].
pub(super) struct MsegInner {
    pub(super) config: MsegConfig,
    /// Cached segments, oldest first.
    cache: Vec<CacheEntry>,
    /// Segments currently handed out.
    pub(super) live: u64,
    pub(super) live_bytes: usize,
    pub(super) max_live: u64,
    max_live_ever: u64,
    pub(super) max_bytes: usize,
    max_bytes_ever: usize,
    /// Monotone high-water mark of the handed-out segment count.
    watermark: u64,
    /// Segments currently mapped from the OS (handed-out plus cached).
    pub(super) mapped: u64,
    mapped_bytes: usize,
    peak_mapped_bytes: usize,
    mapped_sbc: u64,
    mapped_mbc: u64,
    cache_hits: u64,
    pub(super) calls: MsegCalls,
}

impl MsegInner {
    pub(super) fn new(config: MsegConfig) -> MsegInner {
        MsegInner {
            config,
            cache: Vec::new(),
            live: 0,
            live_bytes: 0,
            max_live: 0,
            max_live_ever: 0,
            max_bytes: 0,
            max_bytes_ever: 0,
            watermark: 0,
            mapped: 0,
            mapped_bytes: 0,
            peak_mapped_bytes: 0,
            mapped_sbc: 0,
            mapped_mbc: 0,
            cache_hits: 0,
            calls: MsegCalls::default(),
        }
    }

    /// Records a segment handed to the caller (`ERTS_MSEG_ALLOC_STAT`).
    pub(super) fn note_alloc(&mut self, size: usize) {
        self.live += 1;
        self.live_bytes += size;
        self.max_live = self.max_live.max(self.live);
        self.max_bytes = self.max_bytes.max(self.live_bytes);
        self.watermark = self.watermark.max(self.live);
    }

    /// Records a fresh mapping (`ERTS_MMAP_SIZE_*_INC`, `mseg_create`).
    pub(super) fn note_mapping(&mut self, map_len: usize, kind: SegmentKind) {
        self.mapped += 1;
        self.mapped_bytes += map_len;
        self.peak_mapped_bytes = self.peak_mapped_bytes.max(self.mapped_bytes);
        match kind {
            SegmentKind::SingleBlock => self.mapped_sbc += 1,
            SegmentKind::MultiBlock => self.mapped_mbc += 1,
        }
        self.calls.create += 1;
    }

    /// Returns a segment to the OS (`mseg_destroy`).
    fn destroy(&mut self, entry: CacheEntry) {
        // SAFETY: `entry.base`/`entry.map_len` are exactly the address and length of a
        // mapping this allocator owns, and the segment is no longer reachable (it is
        // either being evicted from the cache or was just handed back). The range is
        // page-aligned by construction.
        if unsafe { os_unmap(entry.base, entry.map_len) } {
            self.calls.destroy += 1;
        } else {
            beam_common::beam_warn!(
                "beam_alloc: mseg could not munmap {:#x}+{}",
                entry.base,
                entry.map_len
            );
        }
        self.mapped = self.mapped.saturating_sub(1);
        self.mapped_bytes = self.mapped_bytes.saturating_sub(entry.map_len);
        match entry.kind {
            SegmentKind::SingleBlock => self.mapped_sbc = self.mapped_sbc.saturating_sub(1),
            SegmentKind::MultiBlock => self.mapped_mbc = self.mapped_mbc.saturating_sub(1),
        }
    }

    /// `mseg_dealloc`: hand a segment back, caching it if there is room.
    pub(super) fn release(&mut self, entry: CacheEntry) {
        self.calls.dealloc += 1;
        debug_assert!(self.live > 0, "releasing a segment that was never handed out");
        self.live = self.live.saturating_sub(1);
        self.live_bytes = self.live_bytes.saturating_sub(entry.size);
        if self.config.cache && self.config.max_cache_size > 0 {
            while self.cache.len() >= self.config.max_cache_size {
                let victim = self.cache.remove(0);
                self.destroy(victim);
            }
            self.cache.push(entry);
            return;
        }
        self.destroy(entry);
    }

    /// `cache_get_segment`: take the smallest cached segment that fits.
    ///
    /// The returned entry is *retargeted* to the request: its usable size becomes
    /// exactly `size` (so the caller is never told it owns more than it asked for; the
    /// mapping's remainder stays visible through [`CacheEntry::capacity`]) and its
    /// alignment becomes exactly `align` (the payload is already a multiple of it —
    /// that is the reuse condition).
    pub(super) fn take_cached(
        &mut self,
        size: usize,
        align: usize,
        kind: SegmentKind,
    ) -> Option<CacheEntry> {
        let mut best: Option<usize> = None;
        for (index, entry) in self.cache.iter().enumerate() {
            if entry.kind != kind || entry.payload % align != 0 || entry.capacity() < size {
                continue;
            }
            match best {
                Some(current) if self.cache[current].capacity() <= entry.capacity() => {}
                _ => best = Some(index),
            }
        }
        let mut entry = self.cache.remove(best?);

        // `+MMamcbf` / `+MMrmcbf`: a cached segment that is a bad fit is trimmed down
        // before it is reused rather than being kept as-is.
        let slack = entry.capacity() - size;
        let relative = self.config.rel_max_cache_bad_fit as u128 * size as u128;
        let good_fit =
            slack < self.config.abs_max_cache_bad_fit && (slack as u128) * 100 < relative;
        if !good_fit {
            if let Some((address, length)) = entry.tail_trim(size) {
                // SAFETY: `(address, length)` is a page-aligned tail of this cached
                // segment's retained mapping, past the usable area; the segment is not
                // handed out, so nothing points into the trimmed range.
                if unsafe { os_unmap(address, length) } {
                    entry.map_len -= length;
                    self.mapped_bytes -= length;
                    self.calls.destroy += 1;
                } else {
                    beam_common::beam_warn!(
                        "beam_alloc: mseg could not trim a reused segment at {address:#x}+{length}"
                    );
                }
            }
        }
        self.cache_hits += 1;
        entry.size = size;
        entry.align = align;
        Some(entry)
    }

    /// Returns every cached segment to the OS (`mseg_clear_cache`).
    pub(super) fn purge_cache(&mut self) {
        while let Some(entry) = self.cache.pop() {
            self.destroy(entry);
        }
    }

    /// Returns the oldest cached segment to the OS (`mseg_check_cache`); returns the
    /// number of segments still cached.
    pub(super) fn drop_one_cached(&mut self) -> u64 {
        if !self.cache.is_empty() {
            let victim = self.cache.remove(0);
            self.destroy(victim);
        }
        self.cache.len() as u64
    }

    /// Refuses a mapping the configured `+MM` limits do not allow.
    pub(super) fn reserve(&mut self, kind: SegmentKind, size: usize) -> Result<()> {
        if self.mapped >= self.config.max_carriers as u64 {
            return Err(error::carrier_failure(size));
        }
        let (limit, used) = match kind {
            SegmentKind::SingleBlock => (self.config.max_sbc_carriers, self.mapped_sbc),
            SegmentKind::MultiBlock => (self.config.max_mbc_carriers, self.mapped_mbc),
        };
        if used >= limit as u64 {
            return Err(error::carrier_failure(size));
        }
        Ok(())
    }

    /// Resizes a live segment in place (`seg.capacity()` already covers `new_size`),
    /// optionally returning the freed tail to the OS.
    pub(super) fn retarget(&mut self, seg: &mut Segment, new_size: usize, trim: bool) -> Result<()> {
        if trim {
            if let Some((address, length)) = seg.entry.tail_trim(new_size) {
                // SAFETY: `(address, length)` is a page-aligned tail of the segment's
                // retained mapping past the bytes that stay usable; the segment is
                // live and exclusively owned, so nothing points into that range.
                if !unsafe { os_unmap(address, length) } {
                    return Err(BeamError::Io(std::io::Error::last_os_error()));
                }
                seg.entry.map_len -= length;
                self.mapped_bytes -= length;
                self.calls.destroy += 1;
            }
        }
        if new_size >= seg.entry.size {
            self.live_bytes += new_size - seg.entry.size;
        } else {
            self.live_bytes -= seg.entry.size - new_size;
        }
        seg.entry.size = new_size;
        self.max_bytes = self.max_bytes.max(self.live_bytes);
        MsegHeader::new(&seg.entry).store(seg.entry.payload);
        Ok(())
    }

    /// The report snapshot; also folds the period maxima into the "ever" maxima,
    /// exactly like the legacy `info_status`.
    pub(super) fn snapshot(&mut self) -> MsegInfo {
        if self.max_live_ever < self.max_live {
            self.max_live_ever = self.max_live;
        }
        if self.max_bytes_ever < self.max_bytes {
            self.max_bytes_ever = self.max_bytes;
        }
        MsegInfo {
            segments: self.live,
            max_segments: self.max_live,
            max_segments_ever: self.max_live_ever,
            bytes: self.live_bytes,
            max_bytes: self.max_bytes,
            max_bytes_ever: self.max_bytes_ever,
            watermark: self.watermark,
            cached_segments: self.cache.len() as u64,
            cache_hits: self.cache_hits,
            mapped_segments: self.mapped,
            mapped_bytes: self.mapped_bytes,
            peak_mapped_bytes: self.peak_mapped_bytes,
            calls: self.calls,
        }
    }
}
