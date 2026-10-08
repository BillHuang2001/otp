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

//! The typed (interposition) surface: the `*_typed`/`*_unsized` entry points, the
//! per-type accounting, and the trait defaults a minimal implementor inherits.

use super::*;

// -- Typed (interposition) surface ---------------------------------------------

/// A minimal implementor that overrides **nothing** of the typed surface, so every
/// typed call exercises a trait default; it counts the `alloc`s that reach it and
/// forwards everything to an inner [`CarrierAllocator`].
struct Forwarding {
    inner: AllocRef,
    allocs: std::sync::atomic::AtomicUsize,
}

impl Allocator for Forwarding {
    fn alloc(&self, layout: Layout) -> Result<NonNull<u8>> {
        self.allocs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.alloc(layout)
    }

    fn realloc(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>> {
        self.inner.realloc(ptr, old_layout, new_size)
    }

    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: forwarded unchanged from the caller.
        unsafe { self.inner.dealloc(ptr, layout) };
    }

    fn stats(&self) -> AllocStats {
        self.inner.stats()
    }

    fn name(&self) -> &str {
        "forwarding"
    }
}

#[test]
fn the_type_attributed_allocations_land_in_their_own_slot() {
    let alloc = allocator();
    let heap = type_id("HEAP");
    let msg = type_id("MSG");
    let untouched = type_id("ATOM");
    assert_ne!(heap, msg);

    let l = layout(64, 16);
    let a = alloc.alloc_typed(heap, l).expect("typed heap alloc");
    let b = alloc.alloc_typed(msg, l).expect("typed msg alloc");
    let stats = alloc.stats();
    assert_eq!(stats.alloc_calls, 2);
    assert_eq!(stats.live_blocks, 2);

    let heap_stats = alloc.type_stats(heap);
    let msg_stats = alloc.type_stats(msg);
    assert_eq!(heap_stats.blocks, 1);
    assert_eq!(msg_stats.blocks, 1);
    // Both requests are the same size class, so each type holds exactly one block
    // and the two slots add up to the allocator's live total.
    assert_eq!(heap_stats.bytes, msg_stats.bytes);
    assert_eq!(heap_stats.bytes + msg_stats.bytes, stats.live_bytes);
    assert_eq!(heap_stats.peak_bytes, heap_stats.bytes);
    // A type nobody allocated for stays at zero.
    assert_eq!(alloc.type_stats(untouched), TypeStats::default());

    // An *untyped* allocation is aggregate-only: it must not be charged to `heap`.
    let plain = alloc.alloc(l).expect("untyped alloc");
    assert_eq!(alloc.type_stats(heap).blocks, 1);
    assert_eq!(alloc.type_stats(heap).bytes, heap_stats.bytes);
    assert_eq!(alloc.stats().live_blocks, 3);
    // SAFETY: live allocation, freed exactly once, untyped as it was allocated.
    unsafe { alloc.dealloc(plain, l) };

    // SAFETY: both live typed allocations, freed exactly once each with the layout
    // they were allocated with.
    unsafe { alloc.dealloc_typed(heap, a, l) };
    // SAFETY: as above.
    unsafe { alloc.dealloc_typed(msg, b, l) };
    let after = alloc.stats();
    assert_eq!(after.live_blocks, 0);
    assert_eq!(after.live_bytes, 0);
    assert_eq!(after.free_calls, 3);
    assert_eq!(alloc.type_stats(heap).blocks, 0);
    assert_eq!(alloc.type_stats(heap).bytes, 0);
    assert_eq!(alloc.type_stats(msg).blocks, 0);
    assert_eq!(alloc.type_stats(msg).bytes, 0);
    // The per-type high-water mark survives the free.
    assert_eq!(alloc.type_stats(heap).peak_bytes, heap_stats.bytes);
    assert_eq!(alloc.type_stats(untouched), TypeStats::default());
}

#[test]
fn realloc_typed_keeps_the_attribution_on_the_same_type() {
    let alloc = allocator();
    let msg = type_id("MSG");
    let other = type_id("HEAP");
    let l = layout(64, 16);
    let ptr = alloc.alloc_typed(msg, l).expect("alloc");

    // Shrinking within the block's capacity does not move it, so neither the block
    // nor its attribution changes.
    let same = alloc
        .realloc_typed(msg, ptr, l, 32)
        .expect("shrink in place");
    assert_eq!(same, ptr);
    assert_eq!(alloc.type_stats(msg).blocks, 1);
    assert_eq!(alloc.stats().realloc_calls, 1);

    // Growing past the capacity moves the block through a real typed alloc + free,
    // both attributed to `msg`. 4097 bytes is above the size-class ladder and below
    // the policy's single-block-carrier threshold, so the new block is carved out of
    // a pooled variable-size multi-block carrier — no dedicated carrier is mapped.
    let grown = alloc.realloc_typed(msg, ptr, l, 4097).expect("grow");
    assert_ne!(grown.as_ptr(), ptr.as_ptr());
    let stats = alloc.stats();
    assert_eq!(stats.realloc_calls, 2);
    assert_eq!(stats.live_blocks, 1);
    assert_eq!(
        stats.sbc_carriers, 0,
        "a mid-size block shares a variable-size carrier instead of owning one"
    );
    assert_eq!(
        stats.mbc_carriers, 2,
        "the slab carrier the first request mapped plus the variable-size one"
    );
    assert_eq!(alloc.type_stats(msg).blocks, 1);
    assert_eq!(alloc.type_stats(msg).bytes, stats.live_bytes);
    assert_eq!(alloc.type_stats(other), TypeStats::default());
    // SAFETY: the live grown block, freed exactly once with its layout.
    unsafe { alloc.dealloc_typed(msg, grown, layout(4097, 16)) };
    assert_eq!(alloc.stats().live_bytes, 0);
    assert_eq!(alloc.type_stats(msg).blocks, 0);
    assert_eq!(alloc.type_stats(msg).bytes, 0);
}

#[test]
fn alloc_unsized_round_trips_at_the_minimum_alignment() {
    let alloc = allocator();
    let ty = type_id("MSG");
    for size in [0usize, 1, 63, 64, 65, 1024, 4097] {
        let ptr = alloc.alloc_unsized(ty, size).expect("alloc_unsized");
        assert_eq!(ptr.as_ptr() as usize % MIN_ALIGN, 0, "size {size}");
        if size > 0 {
            // SAFETY: `size` writable bytes in a live allocation.
            unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0xAB, size) };
        }
        assert_eq!(alloc.type_stats(ty).blocks, 1);
        let before = alloc.stats();
        assert_eq!(before.live_blocks, 1);
        // SAFETY: the live typed allocation, freed exactly once without a layout.
        unsafe { alloc.dealloc_unsized(ty, ptr) };
        let after = alloc.stats();
        assert_eq!(after.live_blocks, 0, "size {size}");
        assert_eq!(after.live_bytes, 0, "size {size}");
        // The slot's block/byte counters return to zero; the high-water mark is
        // the per-type history and is kept.
        assert_eq!(alloc.type_stats(ty).blocks, 0, "size {size}");
        assert_eq!(alloc.type_stats(ty).bytes, 0, "size {size}");
        assert!(alloc.type_stats(ty).peak_bytes > 0, "size {size}");
    }
    let stats = alloc.stats();
    assert_eq!(stats.live_bytes, 0);
    // The over-ladder last request shared a variable-size carrier that coalesced back
    // and was released with its last block; the slab carriers the smaller requests
    // mapped stay (M1 policy).
    assert_eq!(stats.sbc_carriers, 0);
    assert_eq!(stats.carriers, stats.sbc_carriers + stats.mbc_carriers);
}

#[test]
fn the_typed_defaults_forward_to_the_untyped_methods() {
    let inner = allocator().share();
    let subject = Forwarding {
        inner,
        allocs: std::sync::atomic::AtomicUsize::new(0),
    };
    assert_eq!(subject.name(), "forwarding");
    let ty = type_id("MSG");
    let l = layout(64, 16);

    // `alloc_unsized` -> `alloc_bytes` -> `alloc`.
    let ptr = subject.alloc_unsized(ty, 64).expect("alloc_unsized");
    assert_eq!(
        subject.allocs.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the default must forward to `alloc`"
    );
    // A default that records no type data reports the zero value.
    assert_eq!(subject.type_stats(ty), TypeStats::default());
    // ... and a default report carries the per-allocator items only.
    let info = subject.allocator_info();
    assert_eq!(info.name, "forwarding");
    assert_eq!(info.items.len(), 13);

    // The default `dealloc_unsized` probe recognises the block through its in-band
    // header and forwards to `dealloc`.
    // SAFETY: the live allocation, freed exactly once through the default.
    unsafe { subject.dealloc_unsized(ty, ptr) };
    assert_eq!(subject.stats().live_bytes, 0);
    assert_eq!(subject.stats().live_blocks, 0);

    // `alloc_typed`/`realloc_typed`/`dealloc_typed` forward too.
    let typed = subject.alloc_typed(ty, l).expect("alloc_typed");
    let grown = subject
        .realloc_typed(ty, typed, l, 128)
        .expect("realloc_typed");
    // SAFETY: the live (possibly moved) allocation, freed exactly once.
    unsafe { subject.dealloc_typed(ty, grown, layout(128, 16)) };
    assert_eq!(subject.stats().live_bytes, 0);
}
