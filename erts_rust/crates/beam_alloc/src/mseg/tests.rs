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

//! Tests for the `mseg` memory-segment allocator ([`MsegAllocator`]).
//!
//! They exercise the public API against the real OS (`mmap`/`munmap`) and live in a
//! separate file so that `mseg.rs` stays focused on the implementation.

use core::ptr;

use beam_common::BeamError;

use super::*;

fn allocator() -> MsegAllocator {
    MsegAllocator::with_defaults()
}

fn fill(seg: &mut Segment, seed: u8) {
    for (index, byte) in seg.as_mut_slice().iter_mut().enumerate() {
        *byte = seed.wrapping_add(index as u8);
    }
}

fn check_pattern(seg: &Segment, seed: u8, len: usize) {
    for (index, byte) in seg.as_slice().iter().take(len).enumerate() {
        assert_eq!(
            *byte,
            seed.wrapping_add(index as u8),
            "byte {index} of the preserved contents"
        );
    }
}

#[test]
fn map_and_free_round_trip_over_a_range_of_sizes() {
    // No reuse cache: every request is a fresh mapping, so the "a fresh mapping is
    // zeroed" and "the mapping was returned to the OS" claims are about this request
    // and not about a recycled segment.
    let alloc = MsegAllocator::new(MsegConfig::builder().cache(false).build());
    let page = page_size();
    let baseline = alloc.stats();
    for size in [0usize, 1, 8, page - 1, page, page + 1, 4 * page, 64 * page + 3] {
        let mut seg = alloc.map(size, 16).expect("map");
        assert_eq!(seg.size(), size, "the usable size is what was asked for");
        assert!(seg.capacity() >= size);
        assert_eq!(seg.as_ptr() as usize % 16, 0);
        assert_eq!(seg.as_slice().len(), size);
        assert!(
            seg.as_slice().iter().all(|byte| *byte == 0),
            "a fresh mapping is zeroed"
        );
        if size > 0 {
            fill(&mut seg, 7);
            check_pattern(&seg, 7, size);
        }
        assert_eq!(seg.kind(), SegmentKind::MultiBlock);
        drop(seg);
    }
    let after = alloc.stats();
    assert_eq!(after.segments, 0, "every segment was handed back");
    assert_eq!(after.bytes, 0);
    assert_eq!(after.mapped_segments, 0, "nothing stays mapped");
    assert_eq!(after.mapped_bytes, 0);
    assert_eq!(after.cached_segments, 0);
    assert_eq!(after.calls.alloc, 8);
    assert_eq!(after.calls.dealloc, 8);
    assert!(after.calls.create >= 1 && after.calls.destroy == after.calls.create);
    assert_eq!(baseline.calls.create, 0);
}

#[test]
fn requested_alignments_are_honoured_and_the_slack_is_trimmed() {
    let alloc = allocator();
    let page = page_size();
    for align in [16usize, 64, page, 4 * page, 64 * 1024, 2 * 1024 * 1024] {
        let seg = alloc.map(4096, align).expect("map");
        let payload = seg.as_ptr() as usize;
        assert_eq!(payload % align, 0, "payload must be {align}-aligned");
        assert!(seg.capacity() >= 4096);
        assert!(
            seg.capacity() <= 4096 + page,
            "the trailing slack is trimmed to at most one page"
        );
        assert!(
            payload - seg.mapping_base() <= page + MSEG_HEADER_SIZE,
            "the leading slack is trimmed to at most one page plus the header"
        );
        assert_eq!(seg.mapping_len() % page, 0);
        if align >= SUPER_ALIGNED_SIZE {
            assert!(is_super_aligned(payload), "a super-aligned request");
        }
    }
}

#[test]
fn invalid_alignments_are_typed_errors() {
    let alloc = allocator();
    for align in [0usize, 3, 5, 1000, page_size() + 1] {
        assert!(
            matches!(alloc.map(4096, align), Err(BeamError::BadArg)),
            "alignment {align} must be rejected"
        );
        assert!(matches!(alloc.map_sbc(4096, align), Err(BeamError::BadArg)));
    }
    // A request whose page-rounded extent overflows is a typed allocation error,
    // never a panic or a wrap-around.
    assert!(matches!(alloc.map(usize::MAX, 16), Err(BeamError::Alloc(_))));
    assert!(matches!(
        alloc.map(usize::MAX - 4096, 2 * 1024 * 1024),
        Err(BeamError::Alloc(_))
    ));
}

#[test]
fn realloc_grows_in_place_when_the_mapping_has_room() {
    let alloc = allocator();
    // `map(4096, 16)` keeps one partial page past the usable area, so there is
    // room to grow without a remap.
    let mut seg = alloc.map(4096, 16).expect("map");
    assert!(seg.capacity() > 4096);
    fill(&mut seg, 1);
    let before = seg.as_ptr();
    let creates = alloc.stats().calls.create;

    alloc.realloc(&mut seg, 4096, 8000).expect("grow in place");
    assert_eq!(seg.as_ptr(), before, "an in-place grow does not move");
    assert_eq!(seg.size(), 8000);
    check_pattern(&seg, 1, 4096);
    assert_eq!(alloc.stats().calls.create, creates, "no new mapping");
    assert_eq!(alloc.stats().calls.recreate, 0);
}

#[test]
fn realloc_remaps_when_the_mapping_is_too_small_and_preserves_contents() {
    let alloc = allocator();
    let mut seg = alloc.map(4096, 4096).expect("map");
    assert_eq!(seg.capacity(), 4096, "a page-aligned request has no slack");
    fill(&mut seg, 9);

    alloc.realloc(&mut seg, 4096, 256 * 1024).expect("remap");
    assert_eq!(seg.size(), 256 * 1024);
    assert!(seg.capacity() >= 256 * 1024);
    check_pattern(&seg, 9, 4096);
    assert_eq!(alloc.stats().calls.recreate, 1);
    assert!(alloc.stats().calls.create >= 2);
}

#[test]
fn realloc_shrinks_in_place_and_returns_the_tail_to_the_os() {
    let alloc = allocator();
    let mut seg = alloc.map(64 * 1024, 4096).expect("map");
    assert_eq!(seg.capacity(), 64 * 1024);
    fill(&mut seg, 3);

    alloc.realloc(&mut seg, 64 * 1024, 4096).expect("shrink");
    assert_eq!(seg.size(), 4096);
    assert_eq!(seg.capacity(), 4096, "the freed tail was unmapped");
    assert_eq!(seg.mapping_len(), 2 * 4096);
    check_pattern(&seg, 3, 4096);
    assert!(alloc.stats().calls.destroy >= 1);

    // A shrink the thresholds consider not worth releasing memory for still takes
    // effect logically (`old_size < abs_shrink_th` *and* `100*shrink < rel*old`,
    // the legacy `mseg_realloc` test), it just keeps the pages mapped.
    let alloc = MsegAllocator::new(
        MsegConfig::builder()
            .shrink_thresholds(usize::MAX, 100)
            .build(),
    );
    let mut seg = alloc.map(64 * 1024, 4096).expect("map");
    fill(&mut seg, 13);
    let mapping_len = seg.mapping_len();
    let mapped_bytes = alloc.stats().mapped_bytes;

    alloc.realloc(&mut seg, 64 * 1024, 4096).expect("shrink");
    assert_eq!(seg.size(), 4096);
    assert_eq!(seg.mapping_len(), mapping_len, "the mapping was kept");
    assert_eq!(seg.capacity(), 64 * 1024);
    assert_eq!(alloc.stats().mapped_bytes, mapped_bytes);
    check_pattern(&seg, 13, 4096);

    // A shrink that frees nothing at all is not trimmed away either.
    alloc.realloc(&mut seg, 4096, 4096).expect("same size");
    assert_eq!(seg.size(), 4096);
    assert_eq!(seg.capacity(), 64 * 1024);
}

#[test]
fn realloc_rejects_a_mismatched_old_size_without_touching_the_segment() {
    let alloc = allocator();
    let mut seg = alloc.map(4096, 16).expect("map");
    fill(&mut seg, 5);
    let before = seg.as_ptr();
    assert!(matches!(
        alloc.realloc(&mut seg, 4095, 8192),
        Err(BeamError::BadArg)
    ));
    assert_eq!(seg.as_ptr(), before);
    assert_eq!(seg.size(), 4096);
    check_pattern(&seg, 5, 4096);
}

#[test]
fn counters_return_to_the_baseline_and_peaks_are_high_water_marks() {
    let alloc = allocator();
    let baseline = alloc.stats();
    assert_eq!(baseline.segments, 0);
    assert_eq!(baseline.mapped_segments, 0);

    let mut live = Vec::new();
    for index in 0..4usize {
        live.push(alloc.map(4096 * (index + 1), 16).expect("map"));
    }
    let peak = alloc.stats();
    assert_eq!(peak.segments, 4);
    assert_eq!(peak.mapped_segments, 4);
    assert_eq!(peak.max_segments, 4);
    assert_eq!(peak.max_segments_ever, 4);
    assert_eq!(peak.watermark, 4);
    assert_eq!(peak.bytes, 4096 * (1 + 2 + 3 + 4));
    assert!(peak.mapped_bytes > 0 && peak.peak_mapped_bytes == peak.mapped_bytes);
    assert_eq!(peak.cached_segments, 0);

    live.clear();
    let freed = alloc.stats();
    assert_eq!(freed.segments, 0, "no live segment is left");
    assert_eq!(freed.bytes, 0);
    assert_eq!(
        freed.cached_segments, 4,
        "the cache holds them, the OS not yet released"
    );
    assert_eq!(freed.mapped_segments, 4);
    assert!(
        freed.max_segments_ever >= 4 && freed.max_bytes_ever >= peak.bytes,
        "peaks are high-water marks"
    );

    alloc.clear_cache();
    let cleared = alloc.stats();
    assert_eq!(cleared.mapped_segments, 0, "the address space was returned");
    assert_eq!(cleared.mapped_bytes, 0);
    assert_eq!(cleared.cached_segments, 0);
    assert_eq!(cleared.calls.destroy, cleared.calls.create);
    assert_eq!(cleared.max_segments_ever, 4, "an ever-maximum survives");

    // A new period keeps the "ever" maxima but restarts the period maxima.
    let live = alloc.map(4096, 16).expect("map");
    alloc.reset_max();
    let period = alloc.stats();
    assert_eq!(period.segments, 1);
    assert_eq!(period.max_segments, 1);
    assert_eq!(period.max_segments_ever, 4);
    drop(live);
}

#[test]
fn the_reuse_cache_serves_a_freed_segment() {
    let alloc = allocator();
    let mut first = alloc.map(4096, 16).expect("map");
    fill(&mut first, 11);
    let address = first.as_ptr() as usize;
    let creates = alloc.stats().calls.create;
    drop(first);

    let cached = alloc.stats();
    assert_eq!(cached.cached_segments, 1);
    assert_eq!(cached.cache_hits, 0);
    assert_eq!(cached.mapped_segments, 1, "cached segments stay mapped");

    let second = alloc.map(4096, 16).expect("map");
    assert_eq!(second.as_ptr() as usize, address, "the cached segment is reused");
    assert_eq!(second.size(), 4096);
    assert_eq!(alloc.stats().cache_hits, 1);
    assert_eq!(alloc.stats().calls.create, creates, "no new mapping");
    check_pattern(&second, 11, second.size());
}

#[test]
fn a_reused_cached_segment_is_retargeted_to_the_request() {
    // A very generous bad-fit bound keeps the cached segment untouched, so this
    // pins the *bookkeeping* of a reuse: the reported size and alignment are the
    // ones just requested, while the mapping keeps its (larger) capacity.
    let alloc = MsegAllocator::new(
        MsegConfig::builder()
            .bad_fit_thresholds(usize::MAX, 10_000)
            .build(),
    );
    drop(alloc.map(8192, 16).expect("map"));

    let reused = alloc.map(1024, 64).expect("reuse");
    assert_eq!(alloc.stats().cache_hits, 1);
    assert_eq!(reused.size(), 1024, "the reported size is the request");
    assert_eq!(reused.align(), 64, "the reported alignment is the request");
    assert_eq!(reused.as_ptr() as usize % 64, 0);
    assert!(reused.capacity() >= 8192, "the mapping was kept");
    assert_eq!(alloc.stats().mapped_segments, 1);

    // The segment header was rewritten for the request: a bare-pointer release
    // still describes the segment it is handed.
    let payload = reused.as_ptr();
    core::mem::forget(reused);
    let rebuilt = unsafe { alloc.from_raw(payload) }.expect("reconstruct");
    assert_eq!(rebuilt.size(), 1024);
    assert_eq!(rebuilt.align(), 64);
    assert_eq!(rebuilt.kind(), SegmentKind::MultiBlock);
    drop(rebuilt);
    assert_eq!(alloc.stats().segments, 0);
    assert_eq!(alloc.stats().cached_segments, 1);
}

#[test]
fn the_cache_can_be_disabled_and_is_bounded() {
    let alloc = MsegAllocator::new(MsegConfig::builder().cache(false).build());
    drop(alloc.map(4096, 16).expect("map"));
    let info = alloc.stats();
    assert_eq!(info.cached_segments, 0);
    assert_eq!(info.mapped_segments, 0, "an uncached free unmaps immediately");
    assert_eq!(info.calls.destroy, info.calls.create);

    // `mcs` is clamped to MAX_CACHE_SIZE and never exceeded. The segments are all
    // mapped before any is released, otherwise the cache would simply hand the
    // previous one back.
    let alloc = MsegAllocator::new(MsegConfig::builder().max_cache_size(1000).build());
    assert_eq!(alloc.config().max_cache_size, MAX_CACHE_SIZE);
    let mut live = Vec::new();
    for _ in 0..MAX_CACHE_SIZE + 5 {
        live.push(alloc.map(8192, 16).expect("map"));
    }
    assert_eq!(alloc.stats().mapped_segments, MAX_CACHE_SIZE as u64 + 5);
    drop(live);
    let info = alloc.stats();
    assert_eq!(info.cached_segments, MAX_CACHE_SIZE as u64, "the cache is bounded");
    assert_eq!(info.mapped_segments, MAX_CACHE_SIZE as u64);
    assert_eq!(info.calls.destroy, 5, "the surplus went to the OS");
    assert_eq!(alloc.check_cache(), MAX_CACHE_SIZE as u64 - 1);
    alloc.clear_cache();
    assert_eq!(alloc.stats().mapped_segments, 0);
    assert_eq!(alloc.stats().cached_segments, 0);
    assert!(alloc.stats().calls.check_cache == 1);
}

#[test]
fn the_configured_limits_are_enforced_with_typed_errors() {
    // `mmc`: the total number of mapped segments. The cache is disabled so that a
    // released segment really releases its slot (a cached segment stays counted
    // against the limit, which the next assertion pins down).
    let alloc = MsegAllocator::new(
        MsegConfig::builder().max_carriers(2).cache(false).build(),
    );
    let first = alloc.map(4096, 16).expect("map");
    let second = alloc.map(4096, 16).expect("map");
    match alloc.map(4096, 16) {
        Err(BeamError::Alloc(failure)) => {
            assert_eq!(failure.kind, beam_common::AllocKind::Carrier);
            assert_eq!(failure.requested, 4096);
        }
        other => panic!("expected a typed allocation failure, got {other:?}"),
    }
    drop(first);
    let third = alloc.map(4096, 16).expect("map after the mapping was released");
    drop((second, third));

    // A cached segment keeps its slot: with the cache enabled the freed segment is
    // still mapped, so it is reused instead of a third one being mapped.
    let alloc = MsegAllocator::new(MsegConfig::builder().max_carriers(2).build());
    let first = alloc.map(4096, 16).expect("map");
    let second = alloc.map(4096, 16).expect("map");
    assert!(matches!(alloc.map(4096, 16), Err(BeamError::Alloc(_))));
    drop(first);
    assert_eq!(alloc.stats().mapped_segments, 2, "cached, still mapped");
    let reused = alloc.map(4096, 16).expect("the cached segment is reused");
    assert_eq!(alloc.stats().cache_hits, 1);
    assert_eq!(alloc.stats().mapped_segments, 2, "no third mapping");
    drop((second, reused));

    // `mmsbc` / `mmmbc`: the per-kind limits.
    let alloc = MsegAllocator::new(
        MsegConfig::builder()
            .max_sbc_carriers(1)
            .max_mbc_carriers(1)
            .build(),
    );
    let sbc = alloc.map_sbc(4096, 16).expect("sbc");
    assert_eq!(sbc.kind(), SegmentKind::SingleBlock);
    assert!(matches!(alloc.map_sbc(4096, 16), Err(BeamError::Alloc(_))));
    let mbc = alloc.map(4096, 16).expect("mbc");
    assert_eq!(mbc.kind(), SegmentKind::MultiBlock);
    assert!(matches!(alloc.map(4096, 16), Err(BeamError::Alloc(_))));
    drop((sbc, mbc));
}

#[test]
fn a_bare_pointer_can_reconstruct_and_release_a_segment() {
    let alloc = allocator();
    let seg = alloc.map(8192, 256).expect("map");
    let payload = seg.as_ptr();
    // Hand the segment over to the bare-pointer API: forget the handle so that it
    // is not released twice.
    core::mem::forget(seg);

    let mut rebuilt = unsafe { alloc.from_raw(payload) }.expect("reconstruct");
    assert_eq!(rebuilt.size(), 8192);
    assert_eq!(rebuilt.align(), 256);
    fill(&mut rebuilt, 21);
    check_pattern(&rebuilt, 21, 8192);
    let entry = rebuilt.entry;
    core::mem::forget(rebuilt);

    // SAFETY: `payload` is a live segment of this allocator and its handle was
    // forgotten, so this is the only owner left.
    unsafe { alloc.free_ptr(payload) }.expect("free");
    let info = alloc.stats();
    assert_eq!(info.segments, 0);
    assert_eq!(info.bytes, 0);
    assert_eq!(info.cached_segments, 1);
    alloc.clear_cache();
    assert_eq!(alloc.stats().mapped_bytes, 0);
    assert!(entry.map_len > 0);

    // A null pointer is a no-op (the legacy `erts_mseg_dealloc(NULL)` path).
    // SAFETY: the null pointer is never dereferenced by `free_ptr`.
    unsafe { alloc.free_ptr(ptr::null_mut()) }.expect("null free");
}

#[test]
fn a_corrupt_header_is_a_typed_error_not_a_crash() {
    let alloc = allocator();
    let seg = alloc.map(4096, 16).expect("map");
    let payload = seg.as_ptr();
    core::mem::forget(seg);
    let header_address = payload as usize - MSEG_HEADER_SIZE;

    // SAFETY: `header_address` is inside the segment's reserved header area, which
    // is owned by this test (the handle was forgotten) and is at least 8 bytes.
    let saved = unsafe { ptr::read_unaligned(header_address as *const u64) };
    // SAFETY: as above — corrupt and restore the magic word of a segment we own.
    unsafe { ptr::write_unaligned(header_address as *mut u64, 0) };
    // SAFETY: the payload is a live segment of this allocator.
    assert!(matches!(
        unsafe { alloc.free_ptr(payload) },
        Err(BeamError::BadArg)
    ));
    // The failed call must not have released anything.
    assert_eq!(alloc.stats().segments, 1);
    // SAFETY: restore the header we corrupted, then release the segment.
    unsafe { ptr::write_unaligned(header_address as *mut u64, saved) };
    // SAFETY: as above.
    unsafe { alloc.free_ptr(payload) }.expect("free");
    assert_eq!(alloc.stats().segments, 0);
}

#[test]
fn segments_are_send_and_multiple_allocators_are_independent() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<Segment>();
    assert_send::<MsegAllocator>();
    assert_sync::<MsegAllocator>();

    let first = MsegAllocator::with_defaults();
    let second = MsegAllocator::with_defaults();
    let handle = first.clone();
    let seg = first.map(4096, 16).expect("map");
    handle.free(seg);
    assert_eq!(first.stats().cached_segments, 1);
    assert_eq!(second.stats().cached_segments, 0, "state is not shared");
    assert_eq!(second.stats().calls.create, 0);
}

#[test]
fn config_defaults_match_the_legacy_options() {
    let config = MsegConfig::default();
    assert_eq!(config.max_carriers, usize::MAX); // +Mummc: unlimited
    assert_eq!(config.max_sbc_carriers, 256); // +M<alloc>mmsbc
    assert_eq!(config.max_mbc_carriers, usize::MAX); // +M<alloc>mmmbc
    assert!(config.cache); // ErtsMsegOpt_t::cache
    assert_eq!(config.max_cache_size, 10); // +MMmcs
    assert_eq!(config.abs_max_cache_bad_fit, 4 * 1024 * 1024); // +MMamcbf
    assert_eq!(config.rel_max_cache_bad_fit, 20); // +MMrmcbf
    assert_eq!(config.abs_shrink_threshold, 0); // rsbcst
    assert_eq!(config.rel_shrink_threshold, 0); // rsbcst
    assert_eq!(MsegAllocator::VERSION, "0.9");
    assert_eq!(MsegAllocator::unit_size(), SUPER_ALIGNED_SIZE);
    assert_eq!(SUPER_ALIGN_BITS, if usize::BITS == 64 { 14 } else { 18 });
    assert!(is_super_aligned(SUPER_ALIGNED_SIZE));
    assert!(!is_super_aligned(SUPER_ALIGNED_SIZE - 1));
    assert_eq!(
        MsegAllocator::with_defaults().config(),
        MsegConfig::default()
    );
}

#[test]
fn the_reuse_cache_prefers_a_good_fit_and_trims_a_bad_one() {
    // Default bad-fit bounds: a cached 64 KiB segment cannot serve a 4 KiB request
    // "for free", so it is trimmed down to the request.
    let alloc = MsegAllocator::new(
        MsegConfig::builder()
            .bad_fit_thresholds(4096, 20)
            .build(),
    );
    drop(alloc.map(64 * 1024, 4096).expect("map"));
    let cached = alloc.stats();
    assert_eq!(cached.cached_segments, 1);
    let mapped_before = cached.mapped_bytes;

    let reused = alloc.map(4096, 4096).expect("reuse");
    assert_eq!(reused.capacity(), 4096, "the bad fit was trimmed");
    assert_eq!(alloc.stats().cache_hits, 1);
    assert!(alloc.stats().mapped_bytes < mapped_before);
    assert_eq!(alloc.stats().calls.create, 1, "no new mapping was needed");

    // With generous bad-fit bounds the cached segment is reused untouched. The
    // relative bound is a percentage of the requested size (`100*slack < rel*size`),
    // so "for free" needs a very generous one here: 4 KiB out of 64 KiB is a 1500%
    // slack.
    let alloc = MsegAllocator::new(
        MsegConfig::builder()
            .bad_fit_thresholds(usize::MAX, 10_000)
            .build(),
    );
    drop(alloc.map(64 * 1024, 4096).expect("map"));
    let reused = alloc.map(4096, 4096).expect("reuse");
    assert!(reused.capacity() >= 64 * 1024, "kept as-is");
    assert_eq!(reused.size(), 4096, "the reported size is the request");
    assert_eq!(alloc.stats().calls.create, 1);
}

#[test]
fn a_cached_segment_of_the_wrong_kind_or_alignment_is_not_reused() {
    let alloc = allocator();
    drop(alloc.map(4096, 16).expect("map"));
    let mapped = alloc.stats().mapped_segments;
    // Wrong kind: an sbc request cannot take the cached mbc segment.
    let sbc = alloc.map_sbc(4096, 16).expect("sbc");
    assert_eq!(alloc.stats().cache_hits, 0);
    assert_eq!(alloc.stats().mapped_segments, mapped + 1);
    drop(sbc);
    // Incompatible alignment: the cached 16-aligned payload cannot serve 4096.
    let aligned = alloc.map(4096, 4096).expect("aligned");
    assert_eq!(alloc.stats().cached_segments, 2);
    drop(aligned);
    assert_eq!(alloc.stats().segments, 0);
}
