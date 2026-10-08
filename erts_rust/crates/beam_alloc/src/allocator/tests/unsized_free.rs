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

//! The unsized free path — the bounded header probe over every supported alignment —
//! plus the block-header plausibility rules it relies on and the variable-size
//! carrier coalescing/accounting.

use super::*;

#[test]
fn dealloc_unsized_recovers_every_supported_alignment() {
    let alloc = allocator();
    let ty = type_id("HEAP");
    // Over-aligned requests take the large (dedicated single-block carrier) path,
    // whose payload offset differs per alignment; the `MIN_ALIGN`-aligned 2000-byte
    // request takes the mid path (a shared variable-size carrier, offset zero), and
    // the 64-byte one the slab path (offset zero as well).
    for align in [16usize, 32, 64, 256, 4096] {
        let l = layout(2000, align);
        let ptr = alloc.alloc_typed(ty, l).expect("alloc");
        assert_eq!(ptr.as_ptr() as usize % align, 0, "align {align}");
        // SAFETY: the live typed allocation, freed exactly once without a layout.
        unsafe { alloc.dealloc_unsized(ty, ptr) };
        assert_eq!(alloc.stats().live_blocks, 0, "align {align}");
    }
    let l = layout(64, 16);
    let slab = alloc.alloc_typed(ty, l).expect("slab alloc");
    // SAFETY: as above.
    unsafe { alloc.dealloc_unsized(ty, slab) };
    let stats = alloc.stats();
    assert_eq!(stats.live_bytes, 0);
    // Every dedicated carrier went back to the OS; only the slab carrier the last
    // (multi-block) allocation mapped is retained.
    assert_eq!(stats.sbc_carriers, 0);
    assert_eq!(stats.mbc_carriers, 1);
    assert_eq!(stats.carriers, stats.sbc_carriers + stats.mbc_carriers);
    assert_eq!(alloc.type_stats(ty).blocks, 0);
    assert_eq!(alloc.type_stats(ty).bytes, 0);
}

#[test]
fn over_aligned_large_blocks_leave_their_alignment_padding_zeroed() {
    let alloc = allocator();
    let ty = type_id("HEAP");
    // `mseg`'s reuse cache hands a released segment back at the *same* address and
    // does not zero it, so the padding in front of an over-aligned payload can hold
    // a previous owner's block header unless `alloc_large` clears it. A stale but
    // plausible header there would be matched by the first candidate of the
    // unsized-dealloc probe — see
    // `a_reused_segment_cannot_resurrect_a_stale_header_in_the_padding` for the
    // end-to-end sequence.
    for align in [32usize, 64, 256, 4096] {
        let l = layout(2000, align);
        let ptr = alloc.alloc_typed(ty, l).expect("alloc");
        let payload = ptr.as_ptr();
        assert_eq!(payload as usize % align, 0, "align {align}");
        // The payload sits `BLOCK_HEADER_SIZE + padding_for(align)` bytes after the
        // block start, which is exactly `align` bytes after it.
        let base = payload.wrapping_sub(align);
        let padding = align - BLOCK_HEADER_SIZE;
        assert_eq!(padding, padding_for(align), "align {align}");
        // SAFETY: the padding is `padding_for(align)` bytes inside the live
        // dedicated carrier this block owns, between its own header and its
        // payload; it is neither handed to the caller nor overlapped by anything.
        let bytes =
            unsafe { core::slice::from_raw_parts(base.add(BLOCK_HEADER_SIZE), padding) };
        assert!(
            bytes.iter().all(|&b| b == 0),
            "align {align}: the alignment padding must be zeroed"
        );
        // The probe's first candidate (`MIN_ALIGN`, payload offset 0) reads exactly
        // at `payload - MIN_ALIGN`; it must no longer look like one of our headers.
        assert!(
            !plausible_header_at(payload.wrapping_sub(MIN_ALIGN), padding_for(MIN_ALIGN)),
            "align {align}: the padding must not decode as a block header"
        );
        // ... and the probe still finds the real header, so the block frees fully.
        // SAFETY: the live typed allocation, freed exactly once without a layout.
        unsafe { alloc.dealloc_unsized(ty, ptr) };
        assert_eq!(alloc.stats().live_blocks, 0, "align {align}");
        assert_eq!(alloc.stats().live_bytes, 0, "align {align}");
    }
    let stats = alloc.stats();
    assert_eq!(stats.sbc_carriers, 0);
    assert_eq!(stats.carriers, stats.sbc_carriers + stats.mbc_carriers);
}

#[test]
fn a_reused_segment_cannot_resurrect_a_stale_header_in_the_padding() {
    let alloc = allocator();
    let ty = type_id("HEAP");
    let l = layout(2000, 256);

    // A previous owner of the segment leaves a plausible-looking block header at
    // exactly the position the unsized-dealloc probe reads first for its
    // `MIN_ALIGN` candidate (`payload - MIN_ALIGN`). `mseg`'s reuse cache does not
    // zero the segment it hands back, so only the allocator's own padding zeroing
    // can remove it.
    let first = alloc.alloc_typed(ty, l).expect("first alloc");
    let base = first.as_ptr().wrapping_sub(256);
    let stale = BlockHdr {
        size: 80,
        payload_offset: 0,
        class: 0,
        carrier: base.cast::<c_void>(),
        flags: {
            let mut flags = BlockFlags::empty();
            flags.set_free();
            flags.bits()
        },
    };
    // SAFETY: `first` is a live over-aligned block, so `payload - MIN_ALIGN` lies
    // inside its own alignment padding — allocator-internal memory that is never
    // handed out, so no live payload is overwritten.
    unsafe { BlockHeader::store(first.as_ptr().wrapping_sub(MIN_ALIGN), stale) };
    assert!(
        plausible_header_at(
            first.as_ptr().wrapping_sub(MIN_ALIGN),
            padding_for(MIN_ALIGN)
        ),
        "the planted header must be one the probe would accept"
    );

    // Freeing with the correct layout hands the poisoned segment to the reuse cache.
    // SAFETY: the live typed allocation, freed exactly once with its own layout.
    unsafe { alloc.dealloc_typed(ty, first, l) };
    assert_eq!(alloc.stats().live_bytes, 0);

    // The next allocation of the same size is the cache's best (smallest fitting)
    // candidate, so it usually gets that very segment back, stale bytes and all.
    // `mseg` is a process-wide singleton shared with the concurrently running
    // tests, so the segment may have been taken by another test in between; that
    // case is reported and *not* asserted (the invariant test above is the
    // deterministic guarantee), so this test never fails merely because the cache
    // was not reused.
    let second = alloc.alloc_typed(ty, l).expect("second alloc");
    if second.as_ptr() == first.as_ptr() {
        assert!(
            !plausible_header_at(
                second.as_ptr().wrapping_sub(MIN_ALIGN),
                padding_for(MIN_ALIGN)
            ),
            "the reused segment's stale header must have been cleared"
        );
    } else {
        eprintln!(
            "beam_alloc: mseg did not reuse the released segment ({:p} -> {:p}); \
             skipping the reuse assertion",
            first.as_ptr(),
            second.as_ptr()
        );
    }

    // Whichever segment the block landed on, the unsized free must recover every
    // byte (with the stale header gone it matches the real header at the block
    // start, the only candidate left).
    // SAFETY: the live typed allocation, freed exactly once without a layout.
    unsafe { alloc.dealloc_unsized(ty, second) };
    let stats = alloc.stats();
    assert_eq!(stats.live_bytes, 0);
    assert_eq!(stats.live_blocks, 0);
    assert_eq!(stats.sbc_carriers, 0);
    assert_eq!(alloc.type_stats(ty).blocks, 0);
    assert_eq!(alloc.type_stats(ty).bytes, 0);
}

#[test]
fn dealloc_unsized_leaves_a_foreign_pointer_alone() {
    let alloc = allocator();
    let ty = type_id("MSG");
    let live = alloc.alloc_typed(ty, layout(64, 16)).expect("alloc");
    // A zeroed buffer big enough that even the widest probe reads inside it: the
    // pointer sits 32 KiB in, and the probe reaches at most `os_page_size()` below.
    let buffer = vec![0u8; 64 * 1024];
    let bogus =
        NonNull::new(unsafe { buffer.as_ptr().add(32 * 1024) as *mut u8 }).expect("non-null");
    let before = alloc.stats();
    // SAFETY: `bogus` is not a block this allocator handed out; the call must
    // detect that (every candidate header is zeroed, hence no size) and leave the
    // allocator's state untouched.
    unsafe { alloc.dealloc_unsized(ty, bogus) };
    // Every allocator-owned counter is untouched. The `mseg` block is held out of the
    // comparison: it is the *live view* of the process-global segment allocator (see
    // `MsegStats`), and the other tests in this binary — which run in parallel — map
    // and unmap segments while this one runs, so `before` and `after` are two
    // independent global readings.
    let after = alloc.stats();
    assert_eq!(
        after,
        AllocStats {
            mseg: after.mseg,
            ..before
        },
        "a foreign pointer leaves the allocator's own counters alone"
    );
    assert_eq!(alloc.type_stats(ty).blocks, 1);
    // SAFETY: the live typed allocation, freed exactly once.
    unsafe { alloc.dealloc_unsized(ty, live) };
    assert_eq!(alloc.stats().live_bytes, 0);
}

#[test]
fn the_dealloc_probe_is_bounded_and_uses_distinct_payload_offsets() {
    let aligns: Vec<usize> = probe_alignments().collect();
    assert_eq!(aligns.first().copied(), Some(MIN_ALIGN));
    assert_eq!(aligns.last().copied(), Some(os_page_size()));
    // Exactly one candidate per payload offset: 1 + log2(page / UNIT_SIZE).
    assert_eq!(
        aligns.len(),
        1 + (os_page_size() / MIN_ALIGN).ilog2() as usize
    );
    assert!(aligns.len() <= 1 + 16, "the probe stays small");
    assert!(aligns
        .iter()
        .all(|a| a.is_power_of_two() && *a <= os_page_size()));
    assert!(aligns.windows(2).all(|w| w[1] == w[0] * 2));
    let offsets: Vec<usize> = aligns.iter().map(|a| padding_for(*a)).collect();
    let mut unique = offsets.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        offsets.len(),
        "payload offsets must be pairwise distinct"
    );
}

#[test]
fn carriers_split_into_sbc_and_mbc_and_the_totals_agree() {
    let alloc = allocator();
    let ty = type_id("MSG");
    let baseline = alloc.stats();
    assert_eq!(
        baseline.carriers,
        baseline.sbc_carriers + baseline.mbc_carriers
    );

    // A slab request maps one multi-block carrier.
    let small = alloc.alloc_typed(ty, layout(64, 16)).expect("slab alloc");
    let after_slab = alloc.stats();
    assert_eq!(after_slab.mbc_carriers, baseline.mbc_carriers + 1);
    assert_eq!(after_slab.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(after_slab.carriers, baseline.carriers + 1);
    assert_eq!(
        after_slab.carriers,
        after_slab.sbc_carriers + after_slab.mbc_carriers
    );
    assert!(after_slab.carrier_bytes > baseline.carrier_bytes);

    // A request above the ladder and below the single-block-carrier threshold shares
    // a new **variable-size** multi-block carrier: it is still no dedicated carrier.
    let mid = alloc.alloc_typed(ty, layout(4097, 16)).expect("mid alloc");
    let after_mid = alloc.stats();
    assert_eq!(after_mid.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(after_mid.mbc_carriers, after_slab.mbc_carriers + 1);
    assert_eq!(after_mid.carriers, after_slab.carriers + 1);
    assert_eq!(
        after_mid.carriers,
        after_mid.sbc_carriers + after_mid.mbc_carriers
    );

    // A request at or above the policy's single-block-carrier threshold maps a
    // dedicated single-block carrier.
    let sbct = CarrierPolicy::default().small_block_carrier_threshold();
    let large = alloc
        .alloc_typed(ty, layout(sbct, 16))
        .expect("large alloc");
    let after_large = alloc.stats();
    assert_eq!(after_large.sbc_carriers, baseline.sbc_carriers + 1);
    assert_eq!(after_large.mbc_carriers, after_mid.mbc_carriers);
    assert_eq!(after_large.carriers, after_mid.carriers + 1);
    assert_eq!(
        after_large.carriers,
        after_large.sbc_carriers + after_large.mbc_carriers
    );
    let peak = after_large.peak_carrier_bytes;
    assert!(peak >= after_large.carrier_bytes);

    // Freeing the dedicated block releases exactly the single-block carrier (the slab
    // carrier is retained, the M1 policy), and the aggregate follows the split.
    // SAFETY: the live large typed allocation, freed exactly once.
    unsafe { alloc.dealloc_typed(ty, large, layout(sbct, 16)) };
    let after_large_free = alloc.stats();
    assert_eq!(after_large_free.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(after_large_free.mbc_carriers, after_mid.mbc_carriers);
    assert_eq!(after_large_free.carriers, after_mid.carriers);
    assert_eq!(
        after_large_free.carriers,
        after_large_free.sbc_carriers + after_large_free.mbc_carriers
    );
    assert!(after_large_free.carrier_bytes < after_large.carrier_bytes);
    assert_eq!(after_large_free.peak_carrier_bytes, peak);

    // The variable-size carrier goes back too, once its last block has coalesced —
    // that release is the `note_mbc_carrier_down` bookkeeping.
    // SAFETY: the live mid typed allocation, freed exactly once.
    unsafe { alloc.dealloc_typed(ty, mid, layout(4097, 16)) };
    let after_mid_free = alloc.stats();
    assert_eq!(after_mid_free.mbc_carriers, after_slab.mbc_carriers);
    assert_eq!(after_mid_free.carriers, after_slab.carriers);
    assert_eq!(
        after_mid_free.carriers,
        after_mid_free.sbc_carriers + after_mid_free.mbc_carriers
    );

    // SAFETY: the remaining live typed allocation, freed exactly once.
    unsafe { alloc.dealloc_typed(ty, small, layout(64, 16)) };
    let done = alloc.stats();
    // The slab carrier is retained even once fully free (the M1 policy), so only
    // the split invariant is testable here.
    assert_eq!(done.mbc_carriers, baseline.mbc_carriers + 1);
    assert_eq!(done.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(done.carriers, baseline.carriers + 1);
    assert_eq!(done.carriers, done.sbc_carriers + done.mbc_carriers);
    assert_eq!(done.live_blocks, 0);
    assert_eq!(done.live_bytes, 0);
}

#[test]
fn the_policy_thresholds_route_every_request_to_the_right_carrier_kind() {
    let alloc = allocator();
    let ty = type_id("MSG");
    let sbct = CarrierPolicy::default().small_block_carrier_threshold();
    let baseline = alloc.stats();

    // At the size-class ladder: a slab block, from a multi-block carrier dedicated to
    // its class.
    let slab = alloc.alloc_typed(ty, layout(1024, 16)).expect("slab");
    let after_slab = alloc.stats();
    assert_eq!(after_slab.mbc_carriers, baseline.mbc_carriers + 1);
    assert_eq!(after_slab.sbc_carriers, baseline.sbc_carriers);

    // One byte past the ladder, still below `sbct`: a **shared** variable-size
    // carrier, not a dedicated one. (`mbc_carriers` counts the slab *and* the
    // variable-size multi-block carriers; only `sbc_carriers` can tell a dedicated
    // carrier apart.)
    let mid = alloc.alloc_typed(ty, layout(1025, 16)).expect("mid");
    let after_mid = alloc.stats();
    assert_eq!(after_mid.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(after_mid.mbc_carriers, after_slab.mbc_carriers + 1);
    assert_eq!(after_mid.carriers, after_slab.carriers + 1);

    // `sbct` itself is the boundary: at it the request gets a carrier of its own.
    let large = alloc.alloc_typed(ty, layout(sbct, 16)).expect("large");
    let after_large = alloc.stats();
    assert_eq!(after_large.sbc_carriers, baseline.sbc_carriers + 1);
    assert_eq!(after_large.mbc_carriers, after_mid.mbc_carriers);

    // Over-alignment wins over size: a request more aligned than `MIN_ALIGN` can
    // never share a carrier (a carrier's alignment is its base's), so even a small
    // one gets a dedicated single-block carrier.
    let aligned = alloc.alloc_typed(ty, layout(64, 32)).expect("aligned");
    let after_aligned = alloc.stats();
    assert_eq!(after_aligned.sbc_carriers, baseline.sbc_carriers + 2);
    assert_eq!(after_aligned.mbc_carriers, after_mid.mbc_carriers);

    // Every path frees back through its own bookkeeping.
    // SAFETY: each pointer is live and freed exactly once with its own layout.
    unsafe {
        alloc.dealloc_typed(ty, slab, layout(1024, 16));
        alloc.dealloc_typed(ty, mid, layout(1025, 16));
        alloc.dealloc_typed(ty, large, layout(sbct, 16));
        alloc.dealloc_typed(ty, aligned, layout(64, 32));
    }
    let done = alloc.stats();
    assert_eq!(done.live_blocks, 0);
    assert_eq!(done.live_bytes, 0);
    assert_eq!(done.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(
        done.mbc_carriers,
        baseline.mbc_carriers + 1,
        "only the slab carrier is retained; both dedicated carriers and the \
         variable-size one went back to the OS"
    );
    assert_eq!(done.carriers, baseline.carriers + 1);
    assert_eq!(done.carriers, done.sbc_carriers + done.mbc_carriers);
    let slot = alloc.type_stats(ty);
    assert_eq!(slot.blocks, 0, "every block is free again");
    assert_eq!(slot.bytes, 0);
}

#[test]
fn dealloc_accounts_for_the_released_mid_blocks_own_size_not_the_coalesced_extent() {
    let alloc = allocator();
    let ty = type_id("HEAP");
    let l = layout(2000, MIN_ALIGN);
    let first = alloc.alloc_typed(ty, l).expect("first");
    let middle = alloc.alloc_typed(ty, l).expect("middle");
    let last = alloc.alloc_typed(ty, l).expect("last");
    // A mid request is accounted for the block its header records: the request
    // rounded up to `UNIT_SIZE` plus the block header.
    // SAFETY: `middle` is a live block of a pooled variable-size carrier, so its
    // header sits `BLOCK_HEADER_SIZE` below its payload.
    let own = unsafe { FreeList::block_size(middle.as_ptr()) };
    assert_eq!(own, BLOCK_HEADER_SIZE + 2000);
    assert_eq!(alloc.stats().live_bytes, 3 * own);
    assert_eq!(alloc.type_stats(ty).bytes, 3 * own);

    // Free the middle block *after* its predecessor, so the two coalesce: the
    // accounting must still owe exactly the middle block's own size.
    // SAFETY: `first` is live and freed exactly once.
    unsafe { alloc.dealloc_typed(ty, first, l) };
    assert_eq!(alloc.stats().live_bytes, 2 * own);
    // SAFETY: `middle` is live and freed exactly once.
    unsafe { alloc.dealloc_typed(ty, middle, l) };
    // SAFETY: the merged free block starts where `first`'s block did, so its header
    // is the one `crate::mbc` rewrote while coalescing.
    let merged = unsafe { FreeList::block_size(first.as_ptr()) };
    assert_eq!(merged, 2 * own, "the two blocks coalesced into one");
    assert_eq!(
        alloc.stats().live_bytes,
        own,
        "the accounting owes the released block's own size, never the merged extent"
    );
    assert_eq!(alloc.type_stats(ty).bytes, own);

    // The last free coalesces the whole carrier and releases it.
    // SAFETY: `last` is live and freed exactly once.
    unsafe { alloc.dealloc_typed(ty, last, l) };
    let done = alloc.stats();
    assert_eq!(done.live_blocks, 0);
    assert_eq!(done.live_bytes, 0);
    assert_eq!(done.carriers, 0);
    assert_eq!(done.sbc_carriers, 0);
    assert_eq!(done.mbc_carriers, 0);
    assert_eq!(done.carrier_bytes, 0);
    let slot = alloc.type_stats(ty);
    assert_eq!(slot.blocks, 0, "every block is free again");
    assert_eq!(slot.bytes, 0);
}

#[test]
fn a_live_mid_block_with_a_free_predecessor_is_still_recognised_by_the_unsized_probe() {
    let alloc = allocator();
    let ty = type_id("MSG");
    let l = layout(2000, MIN_ALIGN);
    let first = alloc.alloc_typed(ty, l).expect("first");
    let second = alloc.alloc_typed(ty, l).expect("second");
    let third = alloc.alloc_typed(ty, l).expect("third");
    // SAFETY: `second` is a live block, so its header sits below its payload.
    let own = unsafe { FreeList::block_size(second.as_ptr()) };

    // Releasing the first block leaves `second` **live** with a free predecessor, so
    // its own header carries the `prev_free` block-*state* bit (bit 7), which is
    // outside `block::ALL_FLAGS`. `plausible_header` has to tolerate that bit:
    // refusing it would make the unsized probe skip a legitimately live block (and
    // `dealloc` refuse it too), leaking it.
    // SAFETY: `first` is live and freed exactly once.
    unsafe { alloc.dealloc_typed(ty, first, l) };
    // SAFETY: `second` is live, so its header is readable at its block start.
    let header = unsafe { BlockHeader::load(second.as_ptr().sub(BLOCK_HEADER_SIZE)) };
    let flags = BlockFlags::from_bits(header.flags);
    assert!(
        flags.is_prev_free(),
        "the block-state bit must be set for this test to exercise the hazard"
    );
    assert_eq!(header.flags & ALL_FLAGS, 0, "and it is no taxonomy bit");

    // The probe's first candidate is the block's own (offset-0) header, so the block
    // is freed — and coalesces with the free predecessor.
    // SAFETY: `second` is live and freed exactly once.
    unsafe { alloc.dealloc_unsized(ty, second) };
    let after = alloc.stats();
    assert_eq!(after.live_blocks, 1);
    assert_eq!(
        after.live_bytes, own,
        "the released block was accounted for by its own size"
    );
    assert_eq!(alloc.type_stats(ty).bytes, own);

    // SAFETY: `third` is live and freed exactly once.
    unsafe { alloc.dealloc_unsized(ty, third) };
    let done = alloc.stats();
    assert_eq!(done.live_blocks, 0);
    assert_eq!(done.live_bytes, 0);
    assert_eq!(
        done.carriers, 0,
        "the whole carrier coalesced and went back"
    );
    assert_eq!(done.sbc_carriers, 0);
    assert_eq!(done.mbc_carriers, 0);
    assert_eq!(done.carrier_bytes, 0);
    let slot = alloc.type_stats(ty);
    assert_eq!(slot.blocks, 0, "every block is free again");
    assert_eq!(slot.bytes, 0);
}
