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

//! Core allocator behaviour: the slab/mid/large routing, the free and realloc round
//! trips, the alignment handling and the carrier/counter accounting.

use super::*;

#[test]
fn alignment_padding_is_a_function_of_the_alignment() {
    assert_eq!(padding_for(1), 0);
    assert_eq!(padding_for(8), 0);
    assert_eq!(padding_for(16), 0);
    assert_eq!(padding_for(32), 16);
    assert_eq!(padding_for(64), 48);
    assert_eq!(padding_for(os_page_size()), os_page_size() - UNIT_SIZE);
    // The padded payload is always at an `align` boundary given a page-aligned
    // block start.
    let base = 4096usize;
    for align in [1usize, 8, 16, 32, 64, 256, 4096] {
        let payload = base + BLOCK_HEADER_SIZE + padding_for(align);
        assert_eq!(payload % align, 0, "align {align}");
    }
}

#[test]
fn plausible_rejects_foreign_headers() {
    let good = BlockHdr {
        size: 16 + 64,
        payload_offset: 0,
        class: 2,
        carrier: core::ptr::null_mut(),
        flags: BlockFlags::from_bits(0).bits(),
    };
    assert!(CarrierAllocator::plausible(&good, 0));
    // Wrong padding for the alignment the caller claimed.
    assert!(!CarrierAllocator::plausible(&good, 48));
    // Size smaller than the header plus padding.
    let mut too_small = good;
    too_small.size = 8;
    assert!(!CarrierAllocator::plausible(&too_small, 0));
    // The `prev_free` block-*state* bit (bit 7) is tolerated, because a **live**
    // block of a variable-size multi-block carrier carries it whenever its
    // predecessor is free; refusing it here would make `locate` skip such a block
    // and `dealloc` leak it. That accounts for all eight bits of the flag byte, so
    // the "unknown bit" case that used to be asserted here no longer exists — the
    // tolerated mask is exactly one bit wider than the taxonomy union.
    let mut prev_free = good;
    prev_free.flags |= PREV_FREE_BIT;
    assert!(CarrierAllocator::plausible(&prev_free, 0));
    assert_eq!(
        ALL_FLAGS | PREV_FREE_BIT,
        0xFF,
        "one bit beyond the taxonomy"
    );
    // Large flag without the `CLASS_NONE` class and vice versa.
    let mut mismatched = good;
    mismatched.class = CLASS_NONE;
    assert!(!CarrierAllocator::plausible(&mismatched, 0));
    let mut mismatched = good;
    let mut flags = BlockFlags::empty();
    flags.set_large();
    mismatched.flags = flags.bits();
    assert!(!CarrierAllocator::plausible(&mismatched, 0));
}

#[test]
fn the_prev_free_bit_is_read_from_the_block_flag_accessors() {
    // `PREV_FREE_BIT` is `BlockFlags::set_prev_free`'s bit, obtained through the
    // public accessor pair rather than re-declared here, and it is outside the
    // taxonomy union (which `block.rs` pins at `0b0111_1111`).
    assert_eq!(PREV_FREE_BIT, 1 << 7);
    assert_eq!(ALL_FLAGS & PREV_FREE_BIT, 0);
    let mut flags = BlockFlags::empty();
    flags.set_prev_free();
    assert_eq!(flags.bits(), PREV_FREE_BIT);
    flags.clear_prev_free();
    assert_eq!(flags.bits(), 0);
}
#[test]
fn alloc_dealloc_round_trip_returns_the_counters_to_zero() {
    let alloc = allocator();
    let baseline = alloc.stats();
    // A freshly constructed allocator owns no counters. The `mseg` block is the one
    // exception: it is the *live view* of the process-global segment allocator (see
    // `MsegStats`), which every other test in this binary moves at will, so it is
    // compared against itself rather than against the all-zero `Default`.
    assert_eq!(
        baseline,
        AllocStats {
            mseg: baseline.mseg,
            ..AllocStats::default()
        },
        "a fresh allocator owns nothing (bar the process-global mseg view)"
    );
    let l = layout(100, 16);
    let ptr = alloc.alloc(l).expect("alloc");
    let live = alloc.stats();
    assert_eq!(live.alloc_calls, 1);
    assert_eq!(live.live_blocks, 1);
    assert!(live.live_bytes >= 100);
    assert_eq!(live.live_bytes, live.peak_live_bytes);
    assert_eq!(live.carriers, 1);
    let block_size = live.live_bytes;
    // SAFETY: `ptr` is a live allocation of 100 bytes on `alloc`.
    unsafe { alloc.dealloc(ptr, l) };
    let after = alloc.stats();
    assert_eq!(after.live_blocks, 0);
    assert_eq!(after.live_bytes, 0);
    assert_eq!(after.free_calls, 1);
    assert_eq!(after.alloc_calls, 1);
    assert_eq!(after.peak_live_bytes, block_size);
    // The slab carrier is retained (M1 policy) even once fully free.
    assert_eq!(after.carriers, 1);
    assert_eq!(after.carrier_bytes, baseline.carrier_bytes + 64 * 1024);
}

#[test]
fn blocks_at_the_sbct_get_dedicated_carriers_and_release_them() {
    let alloc = allocator();
    let baseline = alloc.stats();
    // The dedicate-vs-share boundary is the policy's single-block-carrier threshold
    // (`+M<alloc>sbct`), *not* the size-class ladder: at or above it a request gets a
    // carrier of its own (the legacy `sb_alloc`).
    let sbct = CarrierPolicy::default().small_block_carrier_threshold();
    let l = layout(sbct, 16);
    let first = alloc.alloc(l).expect("alloc");
    let second = alloc.alloc(l).expect("alloc");
    assert_ne!(first.as_ptr(), second.as_ptr());
    let live = alloc.stats();
    assert_eq!(live.sbc_carriers, baseline.sbc_carriers + 2);
    assert_eq!(live.mbc_carriers, baseline.mbc_carriers);
    assert_eq!(live.carriers, baseline.carriers + 2);
    assert_eq!(live.live_blocks, 2);
    assert!(live.carrier_bytes > baseline.carrier_bytes);
    // SAFETY: both are live allocations on `alloc`.
    unsafe {
        alloc.dealloc(first, l);
    }
    // SAFETY: as above.
    unsafe {
        alloc.dealloc(second, l);
    }
    let after = alloc.stats();
    assert_eq!(after.carriers, baseline.carriers);
    assert_eq!(after.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(after.carrier_bytes, baseline.carrier_bytes);
    assert_eq!(after.live_bytes, 0);
    assert_eq!(after.live_blocks, 0);
    assert_eq!(after.free_calls, 2);
}

#[test]
fn mid_size_blocks_share_one_variable_size_carrier_until_the_last_one_is_freed() {
    let alloc = allocator();
    let baseline = alloc.stats();
    // Above the size-class ladder, below `sbct`, `MIN_ALIGN`-aligned: the request may
    // share a carrier, so it is carved out of a pooled variable-size multi-block
    // carrier (the legacy `mb_alloc`) instead of getting one of its own.
    let l = layout(4097, 16);
    let first = alloc.alloc(l).expect("alloc");
    let after_first = alloc.stats();
    assert_eq!(
        after_first.sbc_carriers, baseline.sbc_carriers,
        "a mid request must not map a dedicated single-block carrier"
    );
    assert_eq!(after_first.mbc_carriers, baseline.mbc_carriers + 1);
    let footprint = after_first.carrier_bytes;
    let mut rest = Vec::new();
    for _ in 0..64 {
        rest.push(alloc.alloc(l).expect("alloc"));
    }
    let shared = alloc.stats();
    assert_eq!(
        shared.mbc_carriers,
        baseline.mbc_carriers + 1,
        "one carrier serves every mid request"
    );
    assert_eq!(shared.carrier_bytes, footprint, "the footprint stays flat");
    assert_eq!(shared.live_blocks, 65);

    // Freeing every block coalesces the carrier back into one free block spanning it,
    // and that is what releases the segment.
    // SAFETY: every pointer is a live allocation of this allocator, freed once.
    unsafe { alloc.dealloc(first, l) };
    for ptr in rest {
        // SAFETY: as above.
        unsafe { alloc.dealloc(ptr, l) };
    }
    let after = alloc.stats();
    assert_eq!(after.live_blocks, 0);
    assert_eq!(after.live_bytes, 0);
    assert_eq!(after.mbc_carriers, baseline.mbc_carriers);
    assert_eq!(after.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(after.carriers, baseline.carriers);
    assert_eq!(after.carrier_bytes, baseline.carrier_bytes);
    assert_eq!(after.free_calls, 65);
}
#[test]
fn unaligned_large_blocks_keep_their_padding_in_the_header() {
    let alloc = allocator();
    for align in [32usize, 64, 256, 4096] {
        let l = layout(2000, align);
        let ptr = alloc.alloc(l).expect("alloc");
        assert_eq!(ptr.as_ptr() as usize % align, 0);
        // SAFETY: the block is live and was allocated with this alignment.
        unsafe { alloc.dealloc(ptr, l) };
    }
    assert_eq!(alloc.stats().live_bytes, 0);
    assert_eq!(
        alloc.stats().carriers,
        0,
        "every large carrier was released"
    );
}

#[test]
fn slab_alignment_up_to_the_minimum_is_honoured() {
    let alloc = allocator();
    for align in [1usize, 2, 4, 8, 16] {
        let l = layout(17, align);
        let ptr = alloc.alloc(l).expect("alloc");
        assert_eq!(ptr.as_ptr() as usize % align, 0, "align {align}");
        // SAFETY: live allocation on `alloc`.
        unsafe { alloc.dealloc(ptr, l) };
    }
    assert_eq!(alloc.stats().live_bytes, 0);
}

#[test]
fn unsupported_alignments_are_rejected() {
    let alloc = allocator();
    // Larger than any plausible page size: this allocator cannot super-align a
    // carrier, so it must refuse rather than return a misaligned block.
    let l = layout(64, 1 << 20);
    assert!(matches!(
        alloc.alloc(l),
        Err(beam_common::BeamError::BadArg)
    ));
    assert_eq!(alloc.stats().alloc_calls, 0);
}

#[test]
fn many_carriers_are_subdivided_and_reused() {
    let alloc = allocator();
    // Each 64 KiB carrier holds 63 whole blocks of class 1024 (+16 header), so
    // this needs dozens of carriers.
    let l = layout(1024, 16);
    let mut ptrs = Vec::new();
    for i in 0..4000usize {
        let ptr = alloc.alloc(l).expect("alloc");
        // Pattern the whole payload so a mis-sized block shows up as corruption.
        // SAFETY: the allocation holds 1024 writable bytes.
        unsafe { core::ptr::write_bytes(ptr.as_ptr(), (i & 0xFF) as u8, 1024) };
        ptrs.push((ptr, (i & 0xFF) as u8));
    }
    let stats = alloc.stats();
    assert!(
        stats.carriers >= 60,
        "expected many carriers, got {}",
        stats.carriers
    );
    assert_eq!(stats.live_blocks, 4000);
    for (ptr, pattern) in &ptrs {
        // SAFETY: still live, 1024 bytes long.
        let sample = unsafe { core::ptr::read_volatile(ptr.as_ptr().add(1023)) };
        assert_eq!(sample, *pattern, "block contents were clobbered");
    }
    for (ptr, _) in ptrs {
        // SAFETY: each pointer is freed exactly once here.
        unsafe { alloc.dealloc(ptr, l) };
    }
    let after = alloc.stats();
    assert_eq!(after.live_blocks, 0);
    assert_eq!(after.live_bytes, 0);
    assert_eq!(after.alloc_calls, 4000);
    assert_eq!(after.free_calls, 4000);
}

#[test]
fn realloc_grows_in_place_while_the_class_has_room() {
    let alloc = allocator();
    let l = layout(100, 16);
    let ptr = alloc.alloc(l).expect("alloc");
    // Class 128 holds 128 bytes of payload, so 120 still fits.
    let same = alloc.realloc(ptr, l, 120).expect("realloc");
    assert_eq!(same.as_ptr(), ptr.as_ptr(), "no move needed");
    assert_eq!(alloc.stats().realloc_calls, 1);
    assert_eq!(alloc.stats().live_blocks, 1);
    // SAFETY: `ptr` is still the live allocation.
    unsafe { alloc.dealloc(same, l) };
    assert_eq!(alloc.stats().live_bytes, 0);
}

#[test]
fn realloc_grow_moves_and_preserves_the_payload() {
    let alloc = allocator();
    let l = layout(100, 16);
    let ptr = alloc.alloc(l).expect("alloc");
    for i in 0..100 {
        // SAFETY: 100 writable bytes.
        unsafe { *ptr.as_ptr().add(i) = i as u8 };
    }
    let grown = alloc.realloc(ptr, l, 4000).expect("realloc");
    for i in 0..100 {
        // SAFETY: the grown block holds at least 100 initialised bytes.
        assert_eq!(unsafe { *grown.as_ptr().add(i) }, i as u8);
    }
    assert_eq!(alloc.stats().live_blocks, 1, "the old block was released");
    // SAFETY: `grown` is the live allocation (allocated with the same layout).
    unsafe { alloc.dealloc(grown, l) };
    assert_eq!(alloc.stats().live_bytes, 0);
}

#[test]
fn realloc_shrink_keeps_the_prefix() {
    let alloc = allocator();
    let l = layout(2000, 16);
    let ptr = alloc.alloc(l).expect("alloc");
    for i in 0..64 {
        // SAFETY: 2000 writable bytes.
        unsafe { *ptr.as_ptr().add(i) = (i as u8) ^ 0x5A };
    }
    let shrunk = alloc.realloc(ptr, l, 64).expect("realloc");
    for i in 0..64 {
        // SAFETY: the block still holds at least 64 bytes.
        assert_eq!(unsafe { *shrunk.as_ptr().add(i) }, (i as u8) ^ 0x5A);
    }
    // SAFETY: live allocation on `alloc`.
    unsafe { alloc.dealloc(shrunk, l) };
    assert_eq!(alloc.stats().live_bytes, 0);
    assert_eq!(alloc.stats().realloc_calls, 1);
}

#[test]
fn realloc_of_a_foreign_pointer_reports_badarg() {
    let alloc = allocator();
    let foreign = layout(64, 16);
    // A block that was never handed out by this allocator: the header lookup
    // finds something implausible and the call fails instead of corrupting memory.
    let buffer = Box::new([0u8; 4096]);
    // SAFETY: offset 1024 is inside the 4096-byte buffer, so the 16 bytes below it
    // that `locate` inspects are in-bounds (they are zeroed, i.e. an implausible
    // header).
    let bogus =
        NonNull::new(unsafe { buffer.as_ptr().add(1024) as *mut u8 }).expect("non-null");
    assert!(matches!(
        alloc.realloc(bogus, foreign, 4096),
        Err(beam_common::BeamError::BadArg)
    ));
    // Whatever happened, no live block was created and nothing was freed. (The `mseg`
    // block is the process-global segment-allocator view, which parallel tests in this
    // binary move, so it is held out of the comparison.)
    let after = alloc.stats();
    assert_eq!(
        after,
        AllocStats {
            mseg: after.mseg,
            ..AllocStats::default()
        }
    );
}

#[test]
fn max_footprint_caps_carrier_acquisition() {
    let config = AllocatorConfig::builder()
        .name("capped")
        .carrier_size(64 * 1024)
        .max_footprint(Some(128 * 1024))
        .build();
    let alloc = CarrierAllocator::new(config).expect("allocator");
    let l = layout(1024, 16);
    let mut pointers = Vec::new();
    let failure = loop {
        match alloc.alloc(l) {
            Ok(ptr) => pointers.push(ptr),
            Err(error) => break error,
        }
    };
    assert_eq!(pointers.len(), 126, "two 64 KiB carriers of 63 blocks each");
    match failure {
        beam_common::BeamError::Alloc(failure) => {
            assert_eq!(failure.kind, beam_common::AllocKind::Carrier);
        }
        other => panic!("expected an allocation failure, got {other:?}"),
    }
    assert_eq!(alloc.stats().carriers, 2);
    for ptr in pointers {
        // SAFETY: each pointer is live and freed exactly once.
        unsafe { alloc.dealloc(ptr, l) };
    }
    assert_eq!(alloc.stats().live_bytes, 0);
    // The cap also bounds the large path.
    let big = layout(4096 * 1024, 16);
    assert!(matches!(
        alloc.alloc(big),
        Err(beam_common::BeamError::Alloc(_))
    ));
}

#[test]
fn alloc_zeroed_hands_out_zeroed_memory() {
    let alloc = allocator();
    let l = layout(512, 16);
    let ptr = alloc.alloc(l).expect("alloc");
    // SAFETY: 512 writable bytes.
    unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0xFF, 512) };
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, l) };
    let zeroed = alloc.alloc_zeroed(l).expect("alloc_zeroed");
    for i in 0..512 {
        // SAFETY: 512 initialised (zeroed) bytes.
        assert_eq!(unsafe { *zeroed.as_ptr().add(i) }, 0);
    }
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(zeroed, l) };
}

#[test]
fn alloc_bytes_uses_the_minimum_alignment() {
    let alloc = allocator();
    let ptr = alloc.alloc_bytes(48).expect("alloc_bytes");
    assert_eq!(ptr.as_ptr() as usize % MIN_ALIGN, 0);
    // SAFETY: 48 writable bytes.
    unsafe { core::ptr::write_bytes(ptr.as_ptr(), 7, 48) };
    let l = layout(48, MIN_ALIGN);
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, l) };
}

#[test]
fn zero_sized_allocations_still_return_a_pointer() {
    let alloc = allocator();
    let l = layout(0, 16);
    let ptr = alloc.alloc(l).expect("alloc");
    assert_eq!(ptr.as_ptr() as usize % MIN_ALIGN, 0);
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, l) };
    assert_eq!(alloc.stats().live_bytes, 0);
}

#[test]
fn share_and_accessors_agree_with_stats() {
    let alloc = allocator();
    let l = layout(200, 16);
    let ptr = alloc.alloc(l).expect("alloc");
    assert_eq!(alloc.name(), "test");
    assert_eq!(alloc.carrier_count(), 1);
    assert_eq!(alloc.carrier_bytes(), 64 * 1024);
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, l) };
    let shared: AllocRef = allocator().share();
    assert_eq!(shared.name(), "test");
    let ptr = shared.alloc_bytes(32).expect("alloc through the handle");
    // SAFETY: live allocation on `shared`, freed exactly once.
    unsafe { shared.dealloc(ptr, layout(32, MIN_ALIGN)) };
    assert_eq!(shared.stats().live_bytes, 0);
}

#[test]
fn dealloc_of_a_foreign_pointer_is_ignored() {
    let alloc = allocator();
    let buffer = Box::new([0u8; 4096]);
    // SAFETY: offset 1024 is inside the buffer, so the header `dealloc` inspects
    // below it is in-bounds (and zeroed, hence implausible).
    let bogus =
        NonNull::new(unsafe { buffer.as_ptr().add(1024) as *mut u8 }).expect("non-null");
    // SAFETY: the pointer is not one this allocator handed out; the call must
    // detect that and leave the allocator's state untouched.
    unsafe { alloc.dealloc(bogus, layout(64, 16)) };
    // The allocator's own counters are untouched. The `mseg` block is the
    // process-global segment-allocator view — shared with the other tests in this
    // binary — so it is compared against itself.
    let after = alloc.stats();
    assert_eq!(
        after,
        AllocStats {
            mseg: after.mseg,
            ..AllocStats::default()
        }
    );
}
