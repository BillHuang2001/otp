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

//! The `mseg` allocator: the mmap-backed memory-segment manager.
//!
//! `mseg` sits *below* the carrier allocator: it hands out page-granular memory
//! segments obtained straight from the operating system, takes them back when they
//! are no longer needed and keeps a few of them in a small **reuse cache** so that a
//! churn of carriers does not turn into a churn of `mmap`/`munmap` syscalls. It is
//! the Rust counterpart of the legacy segment layer
//! `../../erts/emulator/sys/common/erl_mseg.c` together with the OS half of
//! `../../erts/emulator/sys/common/erl_mmap.c` (`os_mmap` / `os_munmap` /
//! `os_mmap_aligned`). The legacy *super carrier* machinery (a large pre-reserved
//! virtual range with a free-segment descriptor map) is deliberately not modelled:
//! every segment is one plain anonymous mapping, which is what the legacy falls back
//! to whenever no super carrier is configured.
//!
//! # What a segment is
//!
//! A segment is one `mmap`-ed range whose base is [`MSEG_HEADER_SIZE`] bytes in front
//! of the *payload* pointer handed to the caller. The payload is aligned to the
//! alignment the caller asked for. Requests at or below the page size are satisfied
//! directly by `mmap` (whose base is page-aligned anyway); larger — *super-aligned* —
//! requests (`ERTS_HAVE_MSEG_SUPER_ALIGNED` / `ERTS_SUPER_ALIGN_BITS` in the legacy)
//! are satisfied by over-mapping and trimming the leading and trailing slack, exactly
//! like the legacy `os_mmap_aligned()`. After trimming, the retained mapping starts at
//! a page boundary at most one page (plus the header) in front of the payload and ends
//! at a page boundary at most one page past the last usable byte.
//!
//! # Segment header
//!
//! The legacy caller keeps the segment size itself and passes it back to
//! `erts_mseg_dealloc`. This implementation additionally records a small control
//! header in the [`MSEG_HEADER_SIZE`] bytes immediately *before* the payload, so a
//! segment can be described — and released — from a bare payload pointer
//! ([`MsegAllocator::free_ptr`], [`MsegAllocator::from_raw`]). The layout is private
//! to this module (it never crosses a C ABI or the ETF wire); it is
//! `#[repr(C)]` and is written with unaligned stores, so the header address itself
//! carries no alignment requirement.
//!
//! # Errors and `unsafe`
//!
//! Every failure is a typed `beam_common::{Result, BeamError}`: a bad alignment is
//! [`beam_common::BeamError::BadArg`], an exhausted address space or an exceeded `+MM`
//! limit is `BeamError::Alloc(AllocFailure { kind: Carrier, .. })`, and a failed
//! `munmap` while a live segment shrinks is [`beam_common::BeamError::Io`]. A `munmap`
//! that fails while a segment is being handed back cannot be reported to anyone — the
//! caller has already given the segment up — so it is logged and the counters are kept
//! consistent. Nothing panics on runtime data.
//!
//! `unsafe` is confined to this subtree's mapping primitives (the `os_map`/`os_unmap`
//! pair, plus the header stores/loads and the bare-pointer entry points); every
//! `unsafe fn` carries a `# Safety` section and every `unsafe` block a preceding
//! `// SAFETY:` rationale (`../beam_common/src/safety.rs` convention).
//!
//! # Layout of this module
//!
//! `mseg.rs` is the module facade: the mapping primitives live in [`mapping`], the
//! segment control header / cache entry / [`Segment`] handle in [`header`], the
//! `+MM`/`+M` configuration and report types in [`config`], the mutex-guarded
//! bookkeeping in [`state`], and the public [`MsegAllocator`] in [`allocator`].

mod allocator;
mod config;
mod header;
mod mapping;
mod state;

pub use allocator::{MsegAllocator, MSEG_VERSION};
pub use config::{
    MsegCalls, MsegConfig, MsegConfigBuilder, MsegInfo, SegmentKind, MAX_CACHE_SIZE,
};
pub use header::{Segment, MSEG_HEADER_SIZE};
pub use mapping::{is_super_aligned, page_size, SUPER_ALIGN_BITS, SUPER_ALIGNED_SIZE};

#[cfg(test)]
mod tests;
