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

//! Off-heap, refcounted large binaries: [`RefcBinary`] (the ERTS `binary_alloc`
//! analogue).
//!
//! This is the **allocation and ownership bookkeeping** half of the workspace's
//! reference-counted binary feature. The *bit-exact on-heap term layout* — the
//! `BinRef` header, `SubBits`, and the `ERL_ONHEAP_BINARY_LIMIT` /
//! `ERL_ONHEAP_BITS_LIMIT` thresholds that decide whether a binary is stored on the
//! process heap or off it — is owned by `beam_term`. What lives here is the off-heap
//! *payload*: an immutable, reference-counted byte buffer the `BinRef.val` word
//! points at, plus the small control header that carries its reference count.
//!
//! The off-heap interior is **ours**: it is never serialized and never crosses the
//! NIF/driver ABI or the ETF wire, so its layout is an implementation detail (only
//! the [`RefcBinary::data_ptr`] first-byte contract is observable, and that contract
//! is precisely "the pointer value stored in the on-heap `BinRef.val` word").
//!
//! The payload allocation is accounted the way the legacy VM accounts it: it is
//! attributed to the `BINARY` allocation type of the registry's `binary_alloc`
//! instance, so it shows up in that allocator's per-type breakdown (the
//! `allocator`/`binary_alloc` section of the `system_info` report) rather than only
//! in the aggregate counters. See the private `payload_type` helper.
//!
//! # Ownership and thread-safety
//!
//! A [`RefcBinary`] is a handle to a shared, immutable buffer. Construction starts
//! the reference count at 1; [`Clone`] bumps it through
//! [`RefCount::inc`](crate::refcount::RefCount::inc); the final [`Drop`] — the one
//! that drives the count to 0 — frees the buffer through the **same** [`AllocRef`]
//! and the **exact** [`Layout`] used at construction. There is no `Sync` handle
//! because the on-heap `BinRef` ownership model is single-threaded per process, but
//! the handles are [`Send`]: the buffer is never mutated after construction (only
//! the atomic reference count changes), so moving a handle to another thread cannot
//! race.
//!
//! `unsafe` is confined to this module's marked blocks (raw payload memory).

use core::alloc::Layout;
use core::ptr::NonNull;
use core::slice;
use std::sync::LazyLock;

use beam_common::{BeamError, Result};

use crate::allocator::{AllocRef, MIN_ALIGN};
use crate::block::UNIT_SIZE;
use crate::error;
use crate::refcount::RefCount;
use crate::registry::{allocator, AllocType};
use crate::types::TypeId;

/// The control header stored immediately before the payload bytes.
///
/// It carries the atomic reference count and the payload length; `RefcBinary`
/// recovers it deterministically from the data pointer (`base`) without an extra
/// indirection. The `#[repr(C)]` + `len` fields make the header exactly [`UNIT_SIZE`]
/// bytes, so the payload begins `UNIT_SIZE` bytes into the allocation and is
/// therefore `UNIT_SIZE`-aligned (the allocator's minimum alignment).
#[repr(C)]
struct BinaryHeader {
    /// The shared reference count (starts at 1 on construction).
    refc: RefCount,
    /// The payload length in bytes (also cached in the handle for O(1) `len`).
    len: usize,
}

/// The number of bytes of control header between the allocation base and the first
/// payload byte.
const HEADER_SIZE: usize = core::mem::size_of::<BinaryHeader>();

// The header must exactly fill one allocation unit, so `data_ptr = base + HEADER_SIZE`
// lands on the allocator's minimum-alignment boundary and no padding bytes are left
// in an unspecified state.
const _: () = assert!(HEADER_SIZE == UNIT_SIZE);

/// The allocation type a [`RefcBinary`] payload is attributed to.
///
/// The registry's `binary_alloc` instance ([`AllocType::Binary`], i.e.
/// [`AllocatorId::Binary`](crate::types::AllocatorId::Binary)) has exactly **two**
/// types bound to it: `BINARY` and `DRV_BINARY`. The legacy C allocates a *normal*
/// (non-driver) off-heap binary payload with `ERTS_ALC_T_BINARY`
/// (`erts_bin_nrml_alloc`, `../../erts/emulator/beam/erl_binary.h:306`) — the
/// `type BINARY BINARY BINARIES binary` row of
/// `../../erts/emulator/beam/erl_alloc.types` — so that is the type chosen here. Its
/// sibling `DRV_BINARY` (`ERTS_ALC_T_DRV_BINARY`) is deliberately *not* used: the C
/// reserves it for driver-owned binaries (`BIN_FLAG_DRV`, see `erts_bin_realloc_fnf`).
///
/// One function decides the type for the whole module, so the allocation in
/// [`RefcBinary::with_allocator`] and the matching release in
/// [`Drop for RefcBinary`](RefcBinary) can never disagree. The `LazyLock` resolves the
/// lookup once (the type inventory is a compiled-in constant table, so a miss is an
/// unreachable build invariant, and caching keeps the linear table scan off the
/// per-allocation path).
fn payload_type() -> TypeId {
    static PAYLOAD_TYPE: LazyLock<TypeId> = LazyLock::new(|| {
        TypeId::from_name("BINARY")
            .expect("the `BINARY` allocation type is part of the compiled-in type inventory")
    });
    *PAYLOAD_TYPE
}

/// An immutable, reference-counted, off-heap byte buffer.
///
/// See the module documentation for the ownership model and the [`RefcBinary::data_ptr`]
/// contract. The buffer is allocated through the [`AllocType::Binary`] allocator (the
/// registry's `binary_alloc` instance) unless a caller supplies one via
/// [`RefcBinary::with_allocator`].
pub struct RefcBinary {
    alloc: AllocRef,
    /// The allocation base (the start of the control header). `data_ptr` is
    /// `base + HEADER_SIZE`.
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: a `RefcBinary` owns (jointly with its clones) an off-heap byte buffer
// reached through `base`, and the only shared mutable state is the atomic `RefCount`
// in the header. The payload bytes are written once at construction and never
// mutated afterwards, so moving a handle to another thread cannot observe a data
// race; the `AllocRef` handle is `Send + Sync` by the `Allocator` contract, and every
// clone participates in the same atomic refcount protocol. (`Sync` is deliberately
// not implemented: the on-heap `BinRef` ownership model is single-threaded.)
unsafe impl Send for RefcBinary {}

impl RefcBinary {
    /// Allocates an off-heap binary holding a copy of `bytes`.
    ///
    /// The reference count starts at 1; the buffer is allocated through the registry's
    /// [`AllocType::Binary`] allocator.
    pub fn new(bytes: &[u8]) -> Result<RefcBinary> {
        RefcBinary::with_allocator(allocator(AllocType::Binary).clone(), bytes)
    }

    /// Allocates an off-heap binary holding a copy of `bytes`, backed by `alloc`.
    ///
    /// The reference count starts at 1. Every clone of the result (and, eventually,
    /// the final drop) uses *this* allocator and the *same* layout, so the caller may
    /// supply an injected allocator for tests. The payload is obtained through the
    /// typed allocation API (`alloc_typed`), attributed to the `BINARY` allocation type
    /// (see the private `payload_type` helper).
    pub fn with_allocator(alloc: AllocRef, bytes: &[u8]) -> Result<RefcBinary> {
        let len = bytes.len();
        let total = HEADER_SIZE
            .checked_add(len)
            .ok_or_else(|| error::large_object_failure(len))?;
        let layout =
            Layout::from_size_align(total, MIN_ALIGN).map_err(|_| error::bad_align(MIN_ALIGN))?;
        // Map *any* allocation failure onto the `LargeObject` class: to the workspace,
        // a `RefcBinary` payload is an off-heap large object, regardless of whether the
        // allocator served it from a slab or a dedicated carrier.
        // Attribute the payload to its allocation type (`BINARY` of the `binary_alloc`
        // allocator — see `payload_type`), so the per-type breakdown sees these bytes
        // and not just the aggregate counters. On an allocator that does not track
        // types the trait's default forwards to the untyped `alloc`.
        let base = match alloc.alloc_typed(payload_type(), layout) {
            Ok(ptr) => ptr,
            Err(BeamError::Alloc(_)) => return Err(error::large_object_failure(total)),
            Err(other) => return Err(other),
        };
        let binary = RefcBinary { alloc, base, len };
        // SAFETY: `base` is a live allocation of exactly `total >= HEADER_SIZE` bytes
        // owned by `binary`; the header occupies `[base, base + HEADER_SIZE)` and the
        // payload `[base + HEADER_SIZE, base + total)`, so the two writes are in-bounds
        // and disjoint, and `base` is at least `UNIT_SIZE`-aligned (the allocator's
        // guarantee), which suffices for `BinaryHeader` (align 8).
        unsafe {
            binary
                .base
                .cast::<BinaryHeader>()
                .as_ptr()
                .write(BinaryHeader {
                    refc: RefCount::new(1),
                    len,
                });
            if len > 0 {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), binary.data_ptr(), len);
            }
        }
        Ok(binary)
    }

    /// The payload length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The payload bytes.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: the payload is `self.len` initialised bytes starting at `data_ptr`
        // (written at construction) and the buffer is immutable and owned — jointly
        // with its clones — by `self`, so the borrow cannot observe a mutation. For an
        // empty payload `data_ptr` is a valid, aligned (one-past-the-header) pointer
        // and the zero-length slice reads nothing.
        unsafe { slice::from_raw_parts(self.data_ptr(), self.len) }
    }

    /// The current reference count.
    pub fn refcount(&self) -> usize {
        self.header().refc.count()
    }

    /// A pointer to the **first payload byte**.
    ///
    /// This is the value stored in the on-heap `BinRef.val` word: it deliberately
    /// points *past* the beam_alloc-internal control header, so consumers treat it as
    /// a raw byte pointer to the binary's data. The pointer is `UNIT_SIZE`-aligned and
    /// valid for [`len`](RefcBinary::len) readable bytes for as long as any handle to
    /// this binary is alive.
    pub fn data_ptr(&self) -> *mut u8 {
        // SAFETY: `base` is the start of an allocation of `total = HEADER_SIZE + len`
        // bytes, so `base + HEADER_SIZE` is in-bounds (or exactly one-past-the-end for
        // an empty payload, which is never dereferenced).
        unsafe { self.base.as_ptr().add(HEADER_SIZE) }
    }

    /// The control header of this binary.
    fn header(&self) -> &BinaryHeader {
        // SAFETY: `base` is the start of a live allocation owned (jointly with clones)
        // by this binary, and `with_allocator` is the only place that hands out a
        // `RefcBinary`, always after writing a `BinaryHeader` at `base`. The header is
        // therefore initialised, and no `&mut` to it is ever formed (only the atomic
        // `refc` changes, through a shared reference).
        unsafe { &*self.base.cast::<BinaryHeader>().as_ptr() }
    }
}

impl Clone for RefcBinary {
    /// Clones the *handle*, not the bytes: the reference count is incremented and both
    /// handles then share the same immutable buffer.
    fn clone(&self) -> RefcBinary {
        self.header().refc.inc();
        RefcBinary {
            alloc: self.alloc.clone(),
            base: self.base,
            len: self.len,
        }
    }
}

impl Drop for RefcBinary {
    fn drop(&mut self) {
        if !self.header().refc.dec_and_test(0) {
            // Other handles still reference the buffer; they will free it.
            return;
        }
        // `total` and `MIN_ALIGN` are exactly the values used by `with_allocator`, which
        // already proved they form a valid `Layout`; recomputing it here is infallible.
        let layout = Layout::from_size_align(HEADER_SIZE + self.len, MIN_ALIGN)
            .expect("RefcBinary layout is valid: it was accepted by with_allocator");
        // SAFETY: `base`/`layout` are exactly the pointer and layout `with_allocator`
        // obtained from `self.alloc` through `alloc_typed(payload_type(), ..)`, the
        // count just reached 0 so no other handle references the buffer, and it is not
        // used after this call. This is the sole free, through the same allocator the
        // buffer was allocated from and with the same allocation type, so the per-type
        // and aggregate counters are unwound together.
        unsafe { self.alloc.dealloc_typed(payload_type(), self.base, layout) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocator::{AllocatorConfig, CarrierAllocator};
    use crate::size_class::SizeClasses;
    use crate::types::{types_for_allocator, AllocatorId};
    use beam_common::AllocKind;

    /// An `AllocRef` on a real carrier allocator with a small carrier, so tests reach
    /// both the slab and the dedicated-carrier paths quickly and can inspect `stats`.
    fn test_allocator(name: &str) -> AllocRef {
        let config = AllocatorConfig::builder()
            .name(name)
            .carrier_size(64 * 1024)
            .build();
        CarrierAllocator::new(config)
            .expect("allocator construction maps nothing")
            .share()
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn content_round_trips_across_the_size_range() {
        let alloc = test_allocator("roundtrip");
        for &len in &[0usize, 1, 63, 64, 65, 1024, 1024 * 1024] {
            let data = pattern(len);
            let binary = RefcBinary::with_allocator(alloc.clone(), &data).expect("allocate");
            assert_eq!(binary.len(), len, "len for size {len}");
            assert_eq!(binary.is_empty(), len == 0);
            assert_eq!(binary.as_bytes(), &data[..], "content for size {len}");
            assert_eq!(binary.refcount(), 1);
        }
        assert_eq!(alloc.stats().live_blocks, 0, "every binary was freed");
        assert_eq!(alloc.stats().live_bytes, 0);
    }

    #[test]
    fn data_ptr_is_the_first_payload_byte() {
        let alloc = test_allocator("data-ptr");
        for &len in &[1usize, 2, 63, 64, 65, 1023, 1024, 64 * 1024] {
            let data = pattern(len);
            let binary = RefcBinary::with_allocator(alloc.clone(), &data).expect("allocate");
            // The contract: `data_ptr` points at the first payload byte (the header is
            // *before* it), so it agrees with the `as_bytes` base pointer.
            assert_eq!(
                binary.as_bytes().as_ptr() as usize,
                binary.data_ptr() as usize,
                "data_ptr must be the payload base for size {len}"
            );
            // SAFETY: `data_ptr` points at `len >= 1` initialised payload bytes.
            let first = unsafe { *binary.data_ptr() };
            assert_eq!(first, data[0], "first byte for size {len}");
        }
    }

    #[test]
    fn data_ptr_is_unit_aligned() {
        let alloc = test_allocator("alignment");
        for &len in &[0usize, 1, 16, 100, 1024, 5000, 1 << 20] {
            let data = pattern(len);
            let binary = RefcBinary::with_allocator(alloc.clone(), &data).expect("allocate");
            assert_eq!(
                binary.data_ptr() as usize % MIN_ALIGN,
                0,
                "data_ptr must be at least MIN_ALIGN-aligned for size {len}"
            );
        }
    }

    #[test]
    fn empty_binary_is_well_formed() {
        let alloc = test_allocator("empty");
        let binary = RefcBinary::with_allocator(alloc.clone(), &[]).expect("allocate");
        assert_eq!(binary.len(), 0);
        assert!(binary.is_empty());
        assert!(binary.as_bytes().is_empty());
        assert_eq!(binary.refcount(), 1);
        // A zero-length payload still hands out a valid, aligned (non-dereferenced) pointer.
        assert_ne!(binary.data_ptr(), core::ptr::null_mut());
        assert_eq!(binary.data_ptr() as usize % MIN_ALIGN, 0);
    }

    #[test]
    fn clone_and_drop_track_the_refcount() {
        let alloc = test_allocator("refcount");
        let binary = RefcBinary::with_allocator(alloc.clone(), b"hello").expect("allocate");
        assert_eq!(binary.refcount(), 1);

        let clone = binary.clone();
        assert_eq!(binary.refcount(), 2, "Clone bumps the shared count");
        assert_eq!(clone.refcount(), 2);

        drop(clone);
        assert_eq!(binary.refcount(), 1, "dropping a clone decrements once");
        // The buffer survives while a handle is alive.
        assert_eq!(binary.as_bytes(), b"hello");
        assert_eq!(binary.len(), 5);
    }

    #[test]
    fn final_drop_frees_the_buffer_exactly_once() {
        let alloc = test_allocator("noleak");

        // Small (slab) path: `live_blocks`/`live_bytes` return to zero, but the class
        // carrier is retained for the allocator's lifetime (a documented M1 policy).
        let before_small = alloc.stats();
        {
            let binary = RefcBinary::with_allocator(alloc.clone(), &[0u8; 200]).expect("allocate");
            assert_eq!(alloc.stats().live_blocks, before_small.live_blocks + 1);
            let clone = binary.clone();
            assert_eq!(alloc.stats().live_blocks, before_small.live_blocks + 1);
            drop(binary);
            assert_eq!(
                alloc.stats().live_blocks,
                before_small.live_blocks + 1,
                "a surviving clone keeps the block live"
            );
            drop(clone);
        }
        let after_small = alloc.stats();
        assert_eq!(after_small.live_blocks, before_small.live_blocks);
        assert_eq!(after_small.live_bytes, before_small.live_bytes);

        // Large (dedicated carrier) path: the block's carrier is unmapped on the final
        // drop, so `carrier_bytes` returns to the slab-only baseline.
        let before_large = alloc.stats();
        {
            let binary =
                RefcBinary::with_allocator(alloc.clone(), &vec![0u8; 100_000]).expect("allocate");
            assert!(
                alloc.stats().carrier_bytes > before_large.carrier_bytes,
                "the large payload maps a dedicated carrier"
            );
            drop(binary);
        }
        let after_large = alloc.stats();
        assert_eq!(after_large.live_blocks, before_large.live_blocks);
        assert_eq!(after_large.live_bytes, before_large.live_bytes);
        assert_eq!(
            after_large.carrier_bytes, before_large.carrier_bytes,
            "the dedicated carrier is returned to the OS on the final drop"
        );
    }

    #[test]
    fn many_varied_sizes_free_with_the_exact_layout() {
        let alloc = test_allocator("varied");
        // Exercise a spread of sizes straddling the slab/class boundaries and the
        // slab/dedicated-carrier threshold. A mismatched dealloc layout would be caught
        // by the allocator's header-plausibility check (the block would be leaked).
        for len in (0..=80).chain([100, 200, 1000, 1010, 1020, 1024, 1030, 1032, 2000, 8192]) {
            let data = pattern(len);
            let binary = RefcBinary::with_allocator(alloc.clone(), &data).expect("allocate");
            let clone = binary.clone();
            assert_eq!(binary.as_bytes(), &data[..], "len {len}");
            assert_eq!(clone.as_bytes(), &data[..], "len {len}");
            drop(binary);
            drop(clone);
        }
        let stats = alloc.stats();
        assert_eq!(stats.live_blocks, 0, "every block was returned");
        assert_eq!(stats.live_bytes, 0);
    }

    #[test]
    fn allocation_failure_reports_a_large_object_kind() {
        // A footprint cap of 16 bytes rejects the very first carrier mapping, so any
        // payload allocation fails before touching memory.
        let config = AllocatorConfig::builder()
            .name("oom")
            .max_footprint(Some(16))
            .build();
        let alloc: AllocRef = CarrierAllocator::new(config)
            .expect("allocator construction maps nothing")
            .share();

        match RefcBinary::with_allocator(alloc.clone(), &[0u8; 64]) {
            Err(BeamError::Alloc(failure)) => {
                assert_eq!(failure.kind, AllocKind::LargeObject);
                assert_eq!(failure.requested, HEADER_SIZE + 64, "the requested total bytes");
            }
            Err(other) => panic!("expected an allocation failure, got {other:?}"),
            Ok(_) => panic!("the capped allocator must reject the allocation"),
        }
        assert_eq!(alloc.stats().live_blocks, 0, "a failed allocation leaks nothing");
    }

    #[test]
    fn new_uses_the_registry_binary_allocator() {
        let binary = RefcBinary::new(&[1, 2, 3, 4]).expect("allocate");
        assert_eq!(binary.as_bytes(), &[1, 2, 3, 4]);
        assert_eq!(binary.refcount(), 1);
        // Round-trips through the `binary_alloc` instance in the registry.
        assert!(allocator(AllocType::Binary).stats().alloc_calls >= 1);
    }

    #[test]
    fn handles_can_move_across_threads() {
        let alloc = test_allocator("threads");
        let binary = RefcBinary::with_allocator(alloc, b"shared").expect("allocate");
        let clone = binary.clone();
        assert_eq!(binary.refcount(), 2);

        let worker = std::thread::spawn(move || {
            // `clone` is `Send`; the shared count is visible from the other thread.
            assert_eq!(clone.as_bytes(), b"shared");
            assert_eq!(clone.refcount(), 2);
        });
        worker.join().expect("worker thread must not panic");
        assert_eq!(binary.refcount(), 1);
        assert_eq!(binary.as_bytes(), b"shared");
    }

    /// The one place that decides the payload's allocation type must name the legacy
    /// `ERTS_ALC_T_BINARY` type — the `BINARY` row of `binary_alloc` (i.e.
    /// `AllocatorId::Binary`), **not** its `DRV_BINARY` sibling (which the C reserves
    /// for driver-owned binaries).
    #[test]
    fn payload_type_is_the_binary_alloc_binary_type() {
        let ty = payload_type();
        assert_eq!(ty.name(), "BINARY");
        assert_eq!(ty.allocator(), AllocatorId::Binary);

        // `binary_alloc` has exactly these two types; the doc comment on `payload_type`
        // justifies picking `BINARY` of the pair.
        let mut on_binary_alloc: Vec<&str> =
            types_for_allocator(AllocatorId::Binary).map(|t| t.name()).collect();
        on_binary_alloc.sort_unstable();
        assert_eq!(on_binary_alloc, ["BINARY", "DRV_BINARY"]);
    }

    /// A payload is attributed to the chosen type's slot: allocating raises it, and the
    /// last handle dropping unwinds it — in lockstep with the aggregate totals.
    #[test]
    fn payload_is_attributed_to_the_binary_type_slot() {
        let alloc = test_allocator("typed-attribution");
        let ty = payload_type();
        let drv = TypeId::from_name("DRV_BINARY").expect("DRV_BINARY is declared");

        let base_slot = alloc.type_stats(ty);
        let base_stats = alloc.stats();

        let data = pattern(200);
        let binary = RefcBinary::with_allocator(alloc.clone(), &data).expect("allocate");

        let slot = alloc.type_stats(ty);
        assert_eq!(slot.blocks, base_slot.blocks + 1, "one live block of type BINARY");
        let stats = alloc.stats();
        assert_eq!(
            stats.live_blocks,
            base_stats.live_blocks + 1,
            "the aggregate sees the same single block"
        );

        // The allocator records the *whole in-band block* it handed out — its own
        // 16-byte block header plus the class-rounded payload (the payload here is
        // HEADER_SIZE + 200 = 216 bytes, which the default table rounds up to 256) — so
        // the attributed bytes are that block size, not the raw `HEADER_SIZE + len`.
        let classes = SizeClasses::default_small_block();
        let class = classes
            .class_for(HEADER_SIZE + data.len())
            .expect("216 bytes is within the small-block range");
        let block_size = UNIT_SIZE + classes.class_size(class).expect("a known class");
        assert_eq!(
            slot.bytes,
            base_slot.bytes + block_size,
            "the type slot carries the allocator's block size"
        );
        assert!(block_size >= HEADER_SIZE + data.len(), "the block covers header + payload");
        // Per-type and aggregate attribution agree exactly (no double counting, no gap).
        assert_eq!(
            slot.bytes - base_slot.bytes,
            stats.live_bytes - base_stats.live_bytes,
            "per-type bytes must equal the aggregate delta"
        );
        assert_eq!(stats.live_blocks, slot.blocks);
        assert_eq!(stats.live_bytes, slot.bytes);
        assert_eq!(slot.bytes % UNIT_SIZE, 0, "the recorded block size is unit-aligned");

        // A clone shares the block, so nothing is counted twice ...
        let clone = binary.clone();
        assert_eq!(alloc.type_stats(ty).blocks, slot.blocks, "a clone adds no block");
        assert_eq!(alloc.type_stats(ty).bytes, slot.bytes, "a clone adds no bytes");
        // ... and the sibling `DRV_BINARY` slot is never touched.
        assert_eq!(alloc.type_stats(drv).blocks, 0);
        assert_eq!(alloc.type_stats(drv).bytes, 0);

        drop(binary);
        assert_eq!(
            alloc.type_stats(ty).blocks,
            slot.blocks,
            "a surviving clone keeps the block live"
        );
        drop(clone);

        // The last handle releases the block: the type slot and the aggregate both
        // return to their baseline.
        let end_slot = alloc.type_stats(ty);
        assert_eq!(end_slot.blocks, base_slot.blocks);
        assert_eq!(end_slot.bytes, base_slot.bytes);
        let end_stats = alloc.stats();
        assert_eq!(end_stats.live_blocks, base_stats.live_blocks);
        assert_eq!(end_stats.live_bytes, base_stats.live_bytes);
    }

    /// An alloc/free cycle over slab and dedicated-carrier sizes leaves neither the
    /// per-type slot nor the aggregate totals above their baseline (no leak).
    #[test]
    fn repeated_binary_cycles_leave_no_per_type_or_aggregate_leak() {
        let alloc = test_allocator("typed-noleak");
        let ty = payload_type();
        let base_slot = alloc.type_stats(ty);
        let base_stats = alloc.stats();

        // 0…1024 straddle the class/slab boundaries; 1025 and 100_000 take the
        // dedicated single-block-carrier path.
        for &len in &[0usize, 1, 17, 64, 65, 200, 1000, 1024, 1025, 4096, 100_000] {
            let data = pattern(len);
            let binary =
                RefcBinary::with_allocator(alloc.clone(), &data).expect("allocate");

            let slot = alloc.type_stats(ty);
            assert_eq!(slot.blocks, base_slot.blocks + 1, "len {len}");
            assert!(slot.bytes >= base_slot.bytes + HEADER_SIZE + len, "len {len}");
            assert_eq!(
                slot.bytes - base_slot.bytes,
                alloc.stats().live_bytes - base_stats.live_bytes,
                "per-type delta == aggregate delta for len {len}"
            );

            let clone = binary.clone();
            assert_eq!(alloc.type_stats(ty).blocks, slot.blocks, "len {len}");
            drop(binary);
            drop(clone);

            let freed = alloc.type_stats(ty);
            assert_eq!(freed.blocks, base_slot.blocks, "len {len}: no leaked block");
            assert_eq!(freed.bytes, base_slot.bytes, "len {len}: no leaked bytes");
        }

        let end_stats = alloc.stats();
        assert_eq!(end_stats.live_blocks, base_stats.live_blocks);
        assert_eq!(end_stats.live_bytes, base_stats.live_bytes);
    }
}
