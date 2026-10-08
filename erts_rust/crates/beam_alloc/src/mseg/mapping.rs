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

//! The OS mapping primitives under the segment layer: the super-alignment constants,
//! the page size, and the `mmap`/`munmap` calls (with the alignment trimming the legacy
//! `os_mmap_aligned()` performs).
//!
//! This is the only place in the `mseg` subtree that talks to `libc`'s mapping calls;
//! every failure is a typed [`beam_common::BeamError`] and every `unsafe fn` carries a
//! `# Safety` section.

use core::ffi::c_void;
use core::ptr;

use beam_common::Result;

use super::header::MSEG_HEADER_SIZE;
use crate::carrier::os_page_size;
use crate::error;

/// The legacy super-alignment granularity in bits (`ERTS_SUPER_ALIGN_BITS`,
/// which is `ERTS_MMAP_SUPERALIGNED_BITS`: 14 on 64-bit hosts, 18 on 32-bit).
pub const SUPER_ALIGN_BITS: u32 = if usize::BITS == 64 { 14 } else { 18 };

/// The legacy super-alignment granularity in bytes (`ERTS_SUPERALIGNED_SIZE`).
pub const SUPER_ALIGNED_SIZE: usize = 1 << SUPER_ALIGN_BITS;

/// The alignment a cached/fresh segment of a given size always has.
///
/// Mirrors the legacy `ERTS_IS_SUPERALIGNED(X)` predicate: an address that is a whole
/// multiple of [`SUPER_ALIGNED_SIZE`].
pub fn is_super_aligned(address: usize) -> bool {
    address % SUPER_ALIGNED_SIZE == 0
}

/// The OS page size, cached (shared with the carrier layer's `mmap` handling).
pub fn page_size() -> usize {
    os_page_size()
}

/// Rounds `value` up to the next multiple of `align` (`align` must be a power of two);
/// `None` on overflow.
///
/// The caller has to turn `None` into a typed error: a request whose page-rounded
/// extent overflows `usize` cannot be mapped.
pub(super) fn align_up(value: usize, align: usize) -> Option<usize> {
    debug_assert!(align.is_power_of_two());
    value.checked_add(align - 1).map(|v| v & !(align - 1))
}

/// Maps `len` bytes of anonymous, private, read/write memory.
///
/// Returns the base address of a fresh, exclusively-owned, page-aligned mapping, or
/// `BeamError::Alloc`-classified [`AllocKind::Carrier`](beam_common::AllocKind).
fn os_map(len: usize) -> Result<usize> {
    debug_assert!(len > 0);
    // SAFETY: an anonymous, private mapping has no preconditions beyond valid flags
    // and a non-zero length; the result is either a fresh mapping owned by this
    // module or `MAP_FAILED`, which is handled below.
    let mapped = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        return Err(error::carrier_failure(len));
    }
    Ok(mapped as usize)
}

/// Returns `[address, address + len)` to the OS; `true` on success.
///
/// # Safety
///
/// The range must be a page-aligned sub-range of a *live* mapping created by
/// [`os_map`], must not be used afterwards, and no live pointer may point into it.
pub(super) unsafe fn os_unmap(address: usize, len: usize) -> bool {
    debug_assert_eq!(address % page_size(), 0);
    debug_assert_eq!(len % page_size(), 0);
    // SAFETY: the caller guarantees the range is a page-aligned part of one of this
    // module's live anonymous mappings and is not used afterwards.
    unsafe { libc::munmap(address as *mut c_void, len) == 0 }
}

/// A freshly mapped, already trimmed segment.
pub(super) struct MappedSegment {
    pub(super) base: usize,
    pub(super) map_len: usize,
    pub(super) payload: usize,
}

/// Maps `size` usable bytes whose payload is `align`-aligned, over-mapping and then
/// trimming the leading and trailing slack.
///
/// This is the Rust counterpart of the legacy `os_mmap_aligned()`: one `mmap` of
/// `size + align + page + MSEG_HEADER_SIZE` bytes guarantees that an `align`-aligned
/// address with room for the segment header in front of it and `size` usable bytes
/// behind it exists inside the mapping; the slack before and after that window is
/// then `munmap`-ed again so the segment does not pin `align` bytes of address space.
/// Trimming is best-effort: if the OS refuses, the slack simply stays mapped (the
/// segment stays correct, only its footprint is larger).
pub(super) fn map_aligned(size: usize, align: usize) -> Result<MappedSegment> {
    let page = page_size();
    let over = size
        .checked_add(align)
        .and_then(|value| value.checked_add(page))
        .and_then(|value| value.checked_add(MSEG_HEADER_SIZE))
        .ok_or_else(|| error::carrier_failure(size))?;
    let map_len = align_up(over, page).ok_or_else(|| error::carrier_failure(size))?;
    let base = os_map(map_len)?;

    let payload = align_up(base + MSEG_HEADER_SIZE, align)
        .filter(|payload| *payload + size.max(1) <= base + map_len)
        .ok_or_else(|| error::carrier_failure(size))?;
    debug_assert_eq!(payload % align, 0, "payload honours the requested alignment");

    // Leading slack: everything before the payload except the header's own bytes. Only
    // whole pages can be unmapped, so at most one page (and the header) is retained.
    let front_bytes = payload - MSEG_HEADER_SIZE - base;
    let front_trim = front_bytes - (front_bytes % page);
    let mut base = base;
    let mut map_len = map_len;
    if front_trim > 0 && unsafe { os_unmap(base, front_trim) } {
        base += front_trim;
        map_len -= front_trim;
    }

    // Trailing slack: everything past the last page that still contains usable bytes.
    let usable_end = payload + size.max(1);
    let keep_end = align_up(usable_end - base, page).unwrap_or(map_len).min(map_len);
    let back_trim = map_len - keep_end;
    if back_trim > 0 && unsafe { os_unmap(base + keep_end, back_trim) } {
        map_len = keep_end;
    }

    Ok(MappedSegment {
        base,
        map_len,
        payload,
    })
}
