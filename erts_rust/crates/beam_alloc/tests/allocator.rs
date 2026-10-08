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

//! Crate-level integration tests for the allocator seam.
//!
//! These consume the crate **only** through its public API — [`beam_alloc::Allocator`]
//! (via [`beam_alloc::CarrierAllocator`], [`beam_alloc::AllocRef`] and the process-wide
//! [`beam_alloc::default_allocator`]), [`beam_alloc::SizeClasses`], the
//! [`beam_alloc::Heap`] built on top of a real carrier allocator, and the frozen term
//! constructors of `beam_term` — so they validate the cross-crate contract rather than
//! module internals (see the crate `CONTEXT.md`, Test Strategy).
//!
//! Ground truth for the *observable* behavior they pin is the C allocator in
//! `erts/emulator/beam/erl_alloc_util.c`: pointers are at least `UNIT_SIZE`-aligned,
//! a freed block's contents are untouched until reuse, and live/peak accounting
//! returns to zero once everything is freed.

use std::alloc::Layout;
use std::mem::size_of;
use std::ptr;
use std::ptr::NonNull;

use beam_alloc::prelude::*;
use beam_alloc::CarrierPolicy;
use beam_alloc::Strategy;
use beam_common::{AllocKind, BeamError};
use beam_term::prelude::{
    arityval, car, cdr, is_arity_value, is_boxed, is_list, is_not_nil_list, is_tuple, make_small,
    tuple_val, Eterm,
};

/// A deterministic 64-bit LCG, so the mixed-size workload is reproducible without
/// adding a random-number dependency (the crate's dependency set is frozen).
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Lcg {
        Lcg(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    /// A value in `0..bound`.
    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).expect("valid layout")
}

/// An allocator with a small carrier so tests reach multiple carriers quickly.
fn small_allocator(name: &str) -> CarrierAllocator {
    let config = AllocatorConfig::builder()
        .name(name)
        .carrier_size(64 * 1024)
        .build();
    CarrierAllocator::new(config).expect("allocator construction maps nothing")
}

/// The allocator-**owned** counters of `stats`: the snapshot with its
/// [`AllocStats::mseg`] block left exactly as *this* snapshot has it, so the result is
/// comparable against the all-zero [`AllocStats::default()`].
///
/// `mseg` is deliberately not an allocator-local counter — it is a live view of the
/// process-global segment allocator (see `beam_alloc::MsegStats`), which every test in
/// this binary run moves, and they run in parallel. Comparing two *independent* readings
/// of it (e.g. a fresh snapshot against a fresh `default()`) would therefore be a race;
/// comparing one snapshot against itself, as here, is not.
fn owned_counters(stats: AllocStats) -> AllocStats {
    AllocStats {
        mseg: stats.mseg,
        ..AllocStats::default()
    }
}
#[test]
fn alloc_dealloc_round_trips_across_the_size_spectrum() {
    let alloc = small_allocator("spectrum");
    assert_eq!(alloc.name(), "spectrum");
    let sizes = [
        0usize, 1, 8, 16, 17, 63, 64, 127, 128, 1000, 1024, 1025, 4096, 100_000,
    ];
    let mut live = Vec::new();
    for (index, &size) in sizes.iter().enumerate() {
        let l = layout(size, MIN_ALIGN);
        let ptr = alloc.alloc(l).expect("alloc");
        assert_eq!(
            ptr.as_ptr() as usize % MIN_ALIGN,
            0,
            "alignment for size {size}"
        );
        let pattern = 0xA5u8 ^ (index as u8);
        if size > 0 {
            // SAFETY: the allocation holds `size` writable bytes.
            unsafe { ptr::write_bytes(ptr.as_ptr(), pattern, size) };
        }
        live.push((ptr, l, size, pattern));
    }

    // Every block still holds its own pattern: blocks are neither overlapped nor
    // truncated (the slab and large paths both have to get the size right).
    for &(ptr, _, size, pattern) in &live {
        if size == 0 {
            // A zero-sized request has no payload bytes to check.
            continue;
        }
        for offset in [0usize, size / 2, size - 1] {
            // SAFETY: `offset < size` for every non-empty entry of `sizes`, and the
            // allocation is still live.
            let byte = unsafe { ptr::read_volatile(ptr.as_ptr().add(offset)) };
            assert_eq!(byte, pattern, "size {size}, offset {offset}");
        }
    }

    for (ptr, l, _, _) in live {
        // SAFETY: each pointer is freed exactly once, with the layout it was
        // allocated with.
        unsafe { alloc.dealloc(ptr, l) };
    }
    let stats = alloc.stats();
    assert_eq!(stats.live_blocks, 0);
    assert_eq!(stats.live_bytes, 0);
    assert_eq!(stats.alloc_calls, sizes.len() as u64);
    assert_eq!(stats.free_calls, sizes.len() as u64);
    assert!(
        stats.peak_live_bytes > 100_000,
        "the peak saw the large block"
    );
}

#[test]
fn requested_alignments_are_honoured() {
    let alloc = small_allocator("align");
    for &align in &[1usize, 2, 4, WORD_ALIGN, 16, 32, 64, 128, 256, 1024, 4096] {
        for &size in &[0usize, 1, 16, 100, 1024, 4096, 10_000] {
            let l = layout(size, align);
            let ptr = alloc.alloc(l).expect("alloc");
            assert_eq!(
                ptr.as_ptr() as usize % align,
                0,
                "alignment {align} (size {size}) must be honoured"
            );
            if size > 0 {
                // SAFETY: `size` writable bytes are held for the whole block.
                unsafe { ptr::write_bytes(ptr.as_ptr(), 0x5A, size) };
            }
            // SAFETY: live allocation freed exactly once with its own layout.
            unsafe { alloc.dealloc(ptr, l) };
        }
    }
    assert_eq!(alloc.stats().live_bytes, 0);

    // An alignment no page-aligned carrier can satisfy is refused up front rather
    // than silently returning a misaligned block.
    let huge = layout(16, 1 << 20);
    assert!(matches!(alloc.alloc(huge), Err(BeamError::BadArg)));
}

#[test]
fn thousands_of_mixed_size_allocations_force_many_carriers() {
    let alloc = small_allocator("mixed");
    let mut rng = Lcg::new(0x1234_5678_9abc_def0);
    let classes = [16usize, 32, 64, 128, 256, 512, 1024];
    let mut live: Vec<(NonNull<u8>, Layout, u8)> = Vec::new();
    for _ in 0..5000 {
        // Sizes stay inside the slab classes, so what grows is the number of carriers
        // (rather than one carrier per large block).
        let capacity = classes[rng.below(classes.len())];
        let size = 1 + rng.below(capacity);
        let l = layout(size, MIN_ALIGN);
        let ptr = alloc.alloc(l).expect("alloc");
        let pattern = (rng.next() & 0xFF) as u8;
        // SAFETY: the allocation holds `size` writable bytes.
        unsafe { ptr::write_bytes(ptr.as_ptr(), pattern, size) };
        live.push((ptr, l, pattern));
    }

    let stats = alloc.stats();
    assert_eq!(stats.live_blocks, 5000);
    assert!(
        stats.carriers > 8,
        "a few MiB of small blocks must span many 64 KiB carriers, saw {}",
        stats.carriers
    );
    for &(ptr, l, pattern) in &live {
        let size = l.size();
        // SAFETY: `size >= 1` and the block is still live.
        let last = unsafe { ptr::read_volatile(ptr.as_ptr().add(size - 1)) };
        assert_eq!(last, pattern, "block of size {size} was clobbered");
    }

    // Free in a shuffled order so the intrusive free lists are exercised beyond the
    // trivial LIFO case (the shuffle keys are drawn once, so they are a fixed order).
    let mut keyed: Vec<(u64, NonNull<u8>, Layout, u8)> = live
        .into_iter()
        .map(|(ptr, l, pattern)| (rng.next(), ptr, l, pattern))
        .collect();
    keyed.sort_by_key(|(key, _, _, _)| *key);
    for (_, ptr, l, _) in keyed {
        // SAFETY: each pointer is freed exactly once with its own layout.
        unsafe { alloc.dealloc(ptr, l) };
    }
    let stats = alloc.stats();
    assert_eq!(stats.live_blocks, 0);
    assert_eq!(stats.live_bytes, 0);
    assert_eq!(stats.free_calls, 5000);
    assert_eq!(stats.alloc_calls, 5000);
}

#[test]
fn the_sbct_threshold_splits_the_shared_and_dedicated_paths() {
    let alloc = small_allocator("threshold");
    let sbct = CarrierPolicy::default().small_block_carrier_threshold();
    // At the size-class threshold the block is carved from a slab carrier, so the
    // footprint jumps to the whole carrier.
    let at_threshold = layout(1024, MIN_ALIGN);
    let ptr = alloc.alloc(at_threshold).expect("alloc");
    assert_eq!(alloc.carrier_bytes(), 64 * 1024);
    assert_eq!(alloc.carrier_count(), 1);
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, at_threshold) };

    // One byte above the ladder the request is *shared*: it is carved out of a pooled
    // variable-size multi-block carrier (sized by the policy's `smbcs` floor — 1 MiB by
    // default, not the allocator's own 64 KiB carrier size), not given a carrier of its
    // own. So the dedicate-vs-share boundary is `sbct`, not the ladder threshold.
    let above = layout(1025, MIN_ALIGN);
    let ptr = alloc.alloc(above).expect("alloc");
    assert_eq!(
        alloc.carrier_count(),
        2,
        "the slab carrier plus one shared variable-size carrier"
    );
    assert_eq!(
        alloc.stats().sbc_carriers,
        0,
        "no dedicated carrier was mapped"
    );
    assert_eq!(alloc.carrier_bytes(), 64 * 1024 + 1024 * 1024);
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, above) };
    assert_eq!(
        alloc.carrier_count(),
        1,
        "the empty variable-size carrier coalesced and went back to the OS"
    );

    // At the policy's single-block-carrier threshold the block really does get a carrier
    // of its own: header + payload, rounded up to the allocation unit.
    let dedicated = layout(sbct, MIN_ALIGN);
    let ptr = alloc.alloc(dedicated).expect("alloc");
    let own = alloc.carrier_bytes() - 64 * 1024;
    assert!(
        (16 + sbct..=16 + sbct + 15).contains(&own),
        "the dedicated carrier should be the block itself, saw {own}"
    );
    assert_eq!(alloc.stats().sbc_carriers, 1);
    assert_eq!(alloc.carrier_count(), 2);
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, dedicated) };
    assert_eq!(
        alloc.carrier_count(),
        1,
        "the dedicated carrier went back to the OS"
    );
    assert_eq!(alloc.stats().live_bytes, 0);
}

#[test]
fn blocks_at_or_above_the_sbct_get_dedicated_carriers_that_are_released_on_free() {
    let alloc = small_allocator("large");
    let baseline = alloc.stats();
    let sbct = CarrierPolicy::default().small_block_carrier_threshold();
    let l = layout(sbct, MIN_ALIGN);
    let first = alloc.alloc(l).expect("alloc");
    let second = alloc.alloc(l).expect("alloc");
    assert_ne!(first.as_ptr(), second.as_ptr());
    // SAFETY: each allocation holds `sbct` writable bytes.
    unsafe {
        ptr::write_bytes(first.as_ptr(), 0x11, sbct);
        ptr::write_bytes(second.as_ptr(), 0x22, sbct);
    }
    let stats = alloc.stats();
    assert_eq!(stats.sbc_carriers, baseline.sbc_carriers + 2);
    assert_eq!(stats.carriers, baseline.carriers + 2);
    assert_eq!(stats.live_blocks, 2);
    assert!(stats.carrier_bytes >= baseline.carrier_bytes + 2 * sbct);
    assert_eq!(stats.peak_live_bytes, stats.live_bytes);

    // SAFETY: both are live allocations on this allocator, freed exactly once.
    unsafe {
        alloc.dealloc(first, l);
        alloc.dealloc(second, l);
    }
    let stats = alloc.stats();
    assert_eq!(stats.carriers, baseline.carriers);
    assert_eq!(stats.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(stats.carrier_bytes, baseline.carrier_bytes);
    assert_eq!(stats.live_blocks, 0);
    assert_eq!(stats.live_bytes, 0);
}
#[test]
fn realloc_preserves_the_payload_when_growing_and_shrinking() {
    let alloc = small_allocator("realloc");
    let base = layout(96, MIN_ALIGN);
    let ptr = alloc.alloc(base).expect("alloc");
    for offset in 0..96 {
        // SAFETY: the allocation holds 96 writable bytes.
        unsafe { *ptr.as_ptr().add(offset) = offset as u8 };
    }

    // Grow across the threshold: the block moves into a dedicated carrier.
    let grown = alloc.realloc(ptr, base, 40_000).expect("realloc grow");
    for offset in 0..96 {
        // SAFETY: the grown block holds at least the original 96 initialised bytes.
        assert_eq!(unsafe { *grown.as_ptr().add(offset) }, offset as u8);
    }
    assert_eq!(alloc.stats().live_blocks, 1, "the old block was released");

    // Shrink: the block still has room, so it is updated in place.
    let shrunk = alloc
        .realloc(grown, layout(40_000, MIN_ALIGN), 128)
        .expect("realloc shrink");
    assert_eq!(
        shrunk.as_ptr(),
        grown.as_ptr(),
        "shrinking within the block must not move"
    );
    for offset in 0..96 {
        // SAFETY: the block still holds at least 96 initialised bytes.
        assert_eq!(unsafe { *shrunk.as_ptr().add(offset) }, offset as u8);
    }

    // Grow within the same size class: still no move.
    let same = alloc
        .realloc(shrunk, layout(128, MIN_ALIGN), 120)
        .expect("realloc in place");
    assert_eq!(same.as_ptr(), shrunk.as_ptr());

    // SAFETY: the final pointer is the single live allocation, freed exactly once.
    unsafe { alloc.dealloc(same, layout(120, MIN_ALIGN)) };
    let stats = alloc.stats();
    assert_eq!(stats.realloc_calls, 3);
    assert_eq!(stats.live_blocks, 0);
    assert_eq!(stats.live_bytes, 0);
}

#[test]
fn stats_track_allocations_frees_and_carriers() {
    let alloc = small_allocator("stats");
    let baseline = alloc.stats();
    assert_eq!(
        baseline,
        owned_counters(baseline),
        "a fresh allocator maps nothing"
    );

    let l = layout(64, MIN_ALIGN);
    let first = alloc.alloc(l).expect("alloc");
    let second = alloc.alloc(l).expect("alloc");
    let stats = alloc.stats();
    assert_eq!(stats.alloc_calls, 2);
    assert_eq!(stats.free_calls, 0);
    assert_eq!(stats.realloc_calls, 0);
    assert_eq!(stats.live_blocks, 2);
    // Live bytes use the block size (header included), so a full free returns to 0.
    assert_eq!(stats.live_bytes, 2 * (16 + 64));
    assert_eq!(stats.peak_live_bytes, stats.live_bytes);
    assert_eq!(stats.carriers, 1);
    assert_eq!(stats.carrier_bytes, 64 * 1024);
    assert_eq!(stats.peak_carrier_bytes, stats.carrier_bytes);

    // SAFETY: `first` is live and freed exactly once.
    unsafe { alloc.dealloc(first, l) };
    let stats = alloc.stats();
    assert_eq!(stats.free_calls, 1);
    assert_eq!(stats.live_blocks, 1);
    assert_eq!(stats.live_bytes, 16 + 64);
    assert_eq!(stats.peak_live_bytes, 2 * (16 + 64));

    // SAFETY: `second` is live and freed exactly once.
    unsafe { alloc.dealloc(second, l) };
    let stats = alloc.stats();
    assert_eq!(stats.live_blocks, 0);
    assert_eq!(stats.live_bytes, 0);
    // The drained slab carrier is retained (documented M1 retention policy) rather
    // than unmapped, so the carrier counters stay put.
    assert_eq!(stats.carriers, 1);
    assert_eq!(stats.carrier_bytes, 64 * 1024);
}

#[test]
fn the_footprint_cap_fails_with_a_carrier_allocation_error() {
    let config = AllocatorConfig::builder()
        .name("capped")
        .carrier_size(64 * 1024)
        .max_footprint(Some(128 * 1024))
        .build();
    let alloc = CarrierAllocator::new(config).expect("allocator");

    let l = layout(1024, MIN_ALIGN);
    let mut pointers = Vec::new();
    let failure = loop {
        match alloc.alloc(l) {
            Ok(ptr) => pointers.push(ptr),
            Err(error) => break error,
        }
    };
    match failure {
        BeamError::Alloc(failure) => {
            assert_eq!(failure.kind, AllocKind::Carrier);
            assert!(failure.requested > 0);
        }
        other => panic!("expected an allocation failure, got {other:?}"),
    }
    assert!(pointers.len() >= 2, "the cap admitted at least one carrier");
    assert_eq!(
        alloc.stats().carrier_bytes,
        128 * 1024,
        "the cap was never exceeded"
    );

    // The cap also bounds the dedicated (large) path.
    assert!(matches!(
        alloc.alloc(layout(4 * 1024 * 1024, MIN_ALIGN)),
        Err(BeamError::Alloc(_))
    ));

    for ptr in pointers {
        // SAFETY: each pointer is live and freed exactly once.
        unsafe { alloc.dealloc(ptr, l) };
    }
    assert_eq!(alloc.stats().live_bytes, 0);
}

#[test]
fn a_custom_size_class_table_drives_the_paths() {
    let classes = SizeClasses::new([32, 64, 128], 128).expect("valid classes");
    assert_eq!(classes.class_for(128), Some(2));
    assert_eq!(classes.class_for(129), None);
    let config = AllocatorConfig::builder()
        .name("custom")
        .classes(classes)
        .carrier_size(4096)
        .build();
    let alloc = CarrierAllocator::new(config).expect("allocator");
    assert_eq!(alloc.name(), "custom");

    // At the table's threshold the block is slab-allocated (16 + 128 bytes live).
    let small = layout(128, MIN_ALIGN);
    let ptr = alloc.alloc(small).expect("alloc");
    assert_eq!(alloc.stats().live_bytes, 16 + 128);
    assert_eq!(alloc.carrier_bytes(), 4096);
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, small) };

    // One byte past the table's threshold the request is *shared*: it is carved out of
    // a pooled variable-size multi-block carrier (sized by the policy's `smbcs` floor,
    // 1 MiB), never given a carrier of its own — the routing boundary is `sbct`, not the
    // class table's threshold.
    let above = layout(129, MIN_ALIGN);
    let ptr = alloc.alloc(above).expect("alloc");
    let block = alloc.stats().live_bytes;
    assert!(
        (16 + 129..=16 + 129 + 15).contains(&block),
        "the block is the request rounded up to the allocation unit, saw {block}"
    );
    assert_eq!(
        alloc.stats().sbc_carriers,
        0,
        "no dedicated carrier was mapped"
    );
    assert_eq!(
        alloc.stats().mbc_carriers,
        2,
        "the slab carrier plus the shared MBC"
    );
    assert_eq!(alloc.carrier_bytes(), 4096 + 1024 * 1024);
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, above) };
    assert_eq!(alloc.stats().live_bytes, 0);
    assert_eq!(
        alloc.stats().mbc_carriers,
        1,
        "the empty variable-size carrier went back"
    );
    assert_eq!(alloc.carrier_bytes(), 4096);
}
#[test]
fn strategy_and_config_accessors_accept_every_variant() {
    for strategy in [
        Strategy::FirstFit,
        Strategy::GoodFit,
        Strategy::BestFit,
        Strategy::AFirstFit,
    ] {
        let config = AllocatorConfig::builder()
            .strategy(strategy)
            .zeroed_carriers(false)
            .build();
        assert_eq!(config.strategy(), strategy);
        assert_eq!(config.carrier_size(), AllocatorConfig::DEFAULT_CARRIER_SIZE);
        assert!(!config.zeroed_carriers());
        assert_eq!(config.max_footprint(), None);
        assert_eq!(config.classes().large_threshold(), 1024);
        let alloc = CarrierAllocator::new(config).expect("allocator");
        let l = layout(100, MIN_ALIGN);
        let ptr = alloc.alloc(l).expect("alloc");
        // SAFETY: live allocation freed exactly once.
        unsafe { alloc.dealloc(ptr, l) };
        assert_eq!(alloc.stats().live_bytes, 0);
    }
}

#[test]
fn the_registry_default_allocator_is_usable_through_an_alloc_ref() {
    let shared: &AllocRef = default_allocator();
    assert_eq!(shared.name(), "std_alloc");
    let l = layout(256, MIN_ALIGN);
    let ptr = shared.alloc(l).expect("alloc");
    // SAFETY: the allocation holds 256 writable bytes.
    unsafe { ptr::write_bytes(ptr.as_ptr(), 0xC3, 256) };
    // SAFETY: live allocation freed exactly once.
    unsafe { shared.dealloc(ptr, l) };

    // A private allocator behind the very same `AllocRef` type.
    let private: AllocRef = small_allocator("private").share();
    assert_eq!(private.name(), "private");
    let ptr = private.alloc_bytes(48).expect("alloc_bytes");
    assert_eq!(ptr.as_ptr() as usize % MIN_ALIGN, 0);
    // SAFETY: live allocation freed exactly once.
    unsafe { private.dealloc(ptr, layout(48, MIN_ALIGN)) };
    assert_eq!(private.stats().live_bytes, 0);

    // Every registered allocation type resolves to a named allocator with counters.
    let stats = all_stats();
    assert_eq!(stats.len(), AllocType::ALL.len());
    for (ty, _) in stats {
        assert!(!allocator(ty).name().is_empty());
        assert_eq!(allocator(ty).name(), ty.as_str());
    }
}

#[test]
fn a_heap_constructs_terms_over_a_carrier_allocator() {
    let alloc: AllocRef = small_allocator("heap").share();
    let mut heap = Heap::with_allocator(alloc.clone(), 512).expect("heap");
    assert_eq!(heap.capacity(), 512);
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.available(), 512);
    assert!(std::sync::Arc::ptr_eq(heap.allocator(), &alloc));

    // A cons cell: two freshly bumped words, returned as a list term.
    let start = heap.hp() as usize;
    let list = heap.cons(make_small(1), make_small(2)).expect("cons");
    assert!(is_list(list));
    assert!(is_not_nil_list(list));
    // SAFETY: `list` is a live 2-word cons cell built by `heap.cons`.
    unsafe {
        assert_eq!(car(list), make_small(1));
        assert_eq!(cdr(list), make_small(2));
    }
    assert_eq!(heap.used(), 2);
    assert_eq!(
        heap.hp() as usize,
        start + 2 * size_of::<Eterm>(),
        "the bump pointer moved"
    );

    // A tuple: an arityval header plus element words.
    let tuple = heap
        .tuple(&[make_small(7), make_small(8), make_small(9)])
        .expect("tuple");
    assert!(is_boxed(tuple));
    // SAFETY: `tuple` is a live boxed term built by `heap.tuple`, so its header word
    // is initialised and aligned.
    assert!(unsafe { is_tuple(tuple) });
    // SAFETY: as above — the header word holds the arity.
    assert_eq!(unsafe { arityval(*tuple_val(tuple)) }, 3);
    // SAFETY: the tuple header and all three element words are initialised.
    unsafe {
        assert!(is_arity_value(*tuple_val(tuple)));
        assert_eq!(*tuple_val(tuple).add(1), make_small(7));
        assert_eq!(*tuple_val(tuple).add(2), make_small(8));
        assert_eq!(*tuple_val(tuple).add(3), make_small(9));
    }
    assert_eq!(heap.used(), 2 + 4);

    // Raw words for the copy engine, still inside the heap and pointer-stable.
    let before = heap.hp() as usize;
    // SAFETY: `alloc_words` hands back four exclusively-owned writable words.
    let words = unsafe { heap.alloc_words(4) }.expect("alloc_words");
    assert_eq!(words as usize, before);
    // SAFETY: the four words were just allocated and are exclusively ours.
    unsafe {
        for index in 0..4 {
            *words.add(index) = make_small(index as i64);
        }
        assert_eq!(*words.add(3), make_small(3));
    }
    assert_eq!(heap.hp() as usize, before + 4 * size_of::<Eterm>());
    assert_eq!(heap.used(), 10);
    assert_eq!(heap.peak_used(), 10);
    assert_eq!(
        heap.hend() as usize - heap.hp() as usize,
        (512 - 10) * size_of::<Eterm>()
    );

    // A too-large request is a *collectable* condition, not a panic, and consumes
    // nothing.
    // SAFETY: the failing request writes nothing before it reports exhaustion.
    let exhausted = unsafe { heap.alloc_words(512) };
    match exhausted {
        Err(BeamError::Alloc(failure)) => assert_eq!(failure.kind, AllocKind::Heap),
        other => panic!("expected a heap exhaustion error, got {other:?}"),
    }
    assert_eq!(heap.used(), 10, "a failed request must not consume words");

    heap.reset();
    assert_eq!(heap.used(), 0);
    assert_eq!(heap.peak_used(), 10, "the peak survives a reset");
    assert!(
        alloc.stats().carriers >= 1,
        "the heap was carved from a carrier"
    );

    // Dropping the heap returns its block to the allocator.
    drop(heap);
    assert_eq!(alloc.stats().live_blocks, 0);
}

#[test]
fn a_tiny_heap_reports_exhaustion_without_touching_memory() {
    let alloc: AllocRef = small_allocator("tiny-heap").share();
    let mut heap = Heap::with_allocator(alloc.clone(), 8).expect("heap");
    // SAFETY: the request fails before any word is handed out.
    let first = unsafe { heap.alloc_words(8) };
    assert!(first.is_ok());
    assert_eq!(heap.used(), 8);
    assert_eq!(heap.available(), 0);
    // SAFETY: as above — no words are consumed on the failure path.
    match unsafe { heap.alloc_words(1) } {
        Err(BeamError::Alloc(failure)) => {
            assert_eq!(failure.kind, AllocKind::Heap);
            assert_eq!(failure.requested, size_of::<Eterm>());
        }
        other => panic!("expected heap exhaustion, got {other:?}"),
    }
    assert_eq!(heap.used(), 8);
    drop(heap);
    assert_eq!(alloc.stats().live_blocks, 0);
}

#[test]
fn dropping_an_allocator_releases_every_carrier_it_mapped() {
    let alloc = small_allocator("drop");
    let small = layout(64, MIN_ALIGN);
    let large = layout(128 * 1024, MIN_ALIGN);
    let a = alloc.alloc(small).expect("alloc");
    let b = alloc.alloc(large).expect("alloc");
    assert!(alloc.stats().carriers >= 2);
    let _ = (a, b);
    // Deliberately drop the allocator with live blocks still outstanding: every
    // carrier must still be handed back to the OS (in debug builds the `munmap` in
    // `Carrier::unmap` is asserted to succeed, so a leak would be loud here).
    drop(alloc);
}

#[test]
fn mid_size_blocks_share_one_variable_size_carrier_until_it_is_fully_free() {
    let alloc = small_allocator("shared");
    let baseline = alloc.stats();
    assert_eq!(baseline, owned_counters(baseline));
    // 2000 bytes is above the size-class ladder and far below `sbct` (512 KiB), so every
    // one of these requests may share a carrier: they are carved out of one pooled
    // variable-size multi-block carrier (the legacy `mb_alloc`).
    let l = layout(2000, MIN_ALIGN);
    let mut live = Vec::new();
    let mut footprint = None;
    for _ in 0..200 {
        live.push(alloc.alloc(l).expect("alloc"));
        let stats = alloc.stats();
        assert_eq!(
            stats.mbc_carriers,
            baseline.mbc_carriers + 1,
            "one MBC serves them all"
        );
        assert_eq!(
            stats.sbc_carriers, baseline.sbc_carriers,
            "a mid request never maps a dedicated carrier"
        );
        match footprint {
            Some(bytes) => assert_eq!(stats.carrier_bytes, bytes, "the footprint stays flat"),
            None => footprint = Some(stats.carrier_bytes),
        }
    }
    let stats = alloc.stats();
    assert_eq!(stats.live_blocks, 200);
    assert_eq!(stats.live_bytes, 200 * (16 + 2000));
    assert_eq!(stats.peak_live_bytes, stats.live_bytes);

    // SAFETY: every pointer is live and freed exactly once with its own layout.
    unsafe {
        for ptr in live {
            alloc.dealloc(ptr, l);
        }
    }
    // Freeing everything coalesces the carrier back into a single free block spanning it,
    // which is what releases the segment: not one byte of the allocator's footprint is
    // left behind (there were no slab or dedicated carriers in this test at all).
    let after = alloc.stats();
    assert_eq!(after.live_blocks, 0);
    assert_eq!(after.live_bytes, 0);
    assert_eq!(after.carriers, 0);
    assert_eq!(after.sbc_carriers, 0);
    assert_eq!(after.mbc_carriers, 0);
    assert_eq!(after.carrier_bytes, 0);
    assert_eq!(after.free_calls, 200);
}

#[test]
fn a_fragmented_variable_size_carrier_reuses_its_holes_and_coalesces_back() {
    let alloc = small_allocator("defrag");
    let baseline = alloc.stats();
    let l = layout(2000, MIN_ALIGN);
    let mut first: Vec<NonNull<u8>> = (0..100).map(|_| alloc.alloc(l).expect("alloc")).collect();
    let stats = alloc.stats();
    assert_eq!(stats.mbc_carriers, baseline.mbc_carriers + 1);
    let footprint = stats.carrier_bytes;

    // Free every other block: the carrier is now a comb of 50 free 2016-byte holes
    // interleaved with 50 live blocks.
    // SAFETY: each pointer is live here and freed exactly once.
    unsafe {
        for index in (0..first.len()).step_by(2) {
            alloc.dealloc(first[index], l);
        }
    }
    let fragmented = alloc.stats();
    assert_eq!(fragmented.live_blocks, 50);
    // Fragmentation must not cost a *new* mapping: the 50 holes and the 50 live blocks
    // still share the one carrier that was mapped above. That carrier cannot be released
    // yet either — a variable-size carrier goes back to the OS only when freeing its last
    // block coalesces the whole segment into a single free block (`is_fully_free`), which
    // the all-freed assertion at the end of this test pins — so the count stays at
    // `baseline + 1` rather than dropping to the bare baseline.
    assert_eq!(
        fragmented.mbc_carriers,
        baseline.mbc_carriers + 1,
        "a carrier with live blocks stays pooled (and no second one is mapped)"
    );

    // The holes are big enough for the same size, so a second batch is served from them:
    // no carrier is added and the footprint does not grow. This is what coalescing and
    // the free list buy — a fragmented carrier stays usable.
    for index in (0..first.len()).step_by(2) {
        first[index] = alloc.alloc(l).expect("realloc from a hole");
    }
    let refilled = alloc.stats();
    assert_eq!(
        refilled.mbc_carriers,
        baseline.mbc_carriers + 1,
        "no new carrier"
    );
    assert_eq!(refilled.carrier_bytes, footprint, "and no new footprint");
    assert_eq!(refilled.live_blocks, 100);

    // Freeing everything in a scattered order coalesces the whole carrier and returns it,
    // so the allocator is exactly back to its baseline.
    // SAFETY: every pointer is live and freed exactly once.
    unsafe {
        for ptr in first
            .iter()
            .skip(1)
            .step_by(2)
            .chain(first.iter().step_by(2))
        {
            alloc.dealloc(*ptr, l);
        }
    }
    let after = alloc.stats();
    assert_eq!(after.live_blocks, baseline.live_blocks);
    assert_eq!(after.live_bytes, baseline.live_bytes);
    assert_eq!(after.carriers, baseline.carriers);
    assert_eq!(after.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(after.mbc_carriers, baseline.mbc_carriers);
    assert_eq!(after.carrier_bytes, baseline.carrier_bytes);
}

#[test]
fn a_mixed_slab_mid_and_large_workload_returns_to_its_steady_state() {
    let alloc = small_allocator("steady");
    let sbct = CarrierPolicy::default().small_block_carrier_threshold();
    // Slab carriers are retained for the allocator's lifetime (the documented policy), so
    // the baseline this workload returns to is the allocator's *steady state* — taken
    // after the slab carrier exists — not a fresh allocator's all-zero stats. The warm-up
    // uses the **one** slab size the workload below also uses (`SLAB_SIZE`), so a single
    // retained slab carrier serves the whole run: a second slab size would map a second
    // per-class carrier, which would legitimately still be there at the end.
    const SLAB_SIZE: usize = 512;
    let warm = alloc.alloc(layout(SLAB_SIZE, MIN_ALIGN)).expect("warm-up");
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(warm, layout(SLAB_SIZE, MIN_ALIGN)) };
    let baseline = alloc.stats();
    assert_eq!(baseline.live_blocks, 0);
    assert_eq!(baseline.live_bytes, 0);
    assert_eq!(baseline.sbc_carriers, 0);
    assert_eq!(
        baseline.mbc_carriers, 1,
        "exactly the one slab carrier the warm-up mapped"
    );

    // A long mixed sequence over all three paths: slab blocks (all the same class, so the
    // retained slab carrier serves them and the steady state stays comparable), mid blocks
    // sharing variable-size carriers, and dedicated single-block carriers at or above
    // `sbct`.
    let mut live: Vec<(NonNull<u8>, Layout)> = Vec::new();
    for index in 0..200usize {
        let l = match index % 4 {
            0 => layout(SLAB_SIZE, MIN_ALIGN),
            1 => layout(2000 + (index * 24) % 300, MIN_ALIGN),
            2 => layout(4097 + (index * 64) % 5000, MIN_ALIGN),
            _ => layout(sbct + (index % 3) * 4096, MIN_ALIGN),
        };
        live.push((alloc.alloc(l).expect("alloc"), l));
    }
    let loaded = alloc.stats();
    assert_eq!(loaded.live_blocks, 200);
    assert!(loaded.sbc_carriers >= 1, "the dedicated path ran");

    // Free every other block first, then the rest, so the coalescing merges happen across
    // holes rather than against untouched neighbours.
    let mut order: Vec<usize> = (0..live.len()).collect();
    order.sort_by_key(|&index| (index % 2, index));
    for index in order {
        let (ptr, l) = live[index];
        // SAFETY: every pointer is live here and freed exactly once with its own layout.
        unsafe { alloc.dealloc(ptr, l) };
    }
    let after = alloc.stats();
    assert_eq!(after.live_blocks, baseline.live_blocks);
    assert_eq!(after.live_bytes, baseline.live_bytes);
    // Both other paths hand their mapping back: a variable-size mid carrier is released
    // once its last block is freed (the segment coalesces), and a dedicated single-block
    // carrier is released on its only free. Only the slab carrier is retained (M1 policy),
    // so the retained set is exactly the warm-up's one carrier.
    assert_eq!(
        after.carriers, baseline.carriers,
        "only the retained slab carrier survives; mid and dedicated carriers are released"
    );
    assert_eq!(after.carrier_bytes, baseline.carrier_bytes);
    assert_eq!(after.sbc_carriers, baseline.sbc_carriers);
    assert_eq!(
        after.mbc_carriers, baseline.mbc_carriers,
        "the retained slab carrier alone, plus no leftover variable-size carrier"
    );
    assert_eq!(after.carriers, after.sbc_carriers + after.mbc_carriers);
}

#[test]
fn dropping_an_allocator_releases_live_slab_mid_and_large_carriers() {
    let alloc = small_allocator("drop-mixed");
    let sbct = CarrierPolicy::default().small_block_carrier_threshold();
    let _slab = alloc.alloc(layout(64, MIN_ALIGN)).expect("slab");
    let _mid: Vec<NonNull<u8>> = (0..8)
        .map(|_| alloc.alloc(layout(2000, MIN_ALIGN)).expect("mid"))
        .collect();
    let _large = alloc.alloc(layout(sbct, MIN_ALIGN)).expect("large");
    let stats = alloc.stats();
    assert_eq!(stats.sbc_carriers, 1);
    assert_eq!(
        stats.mbc_carriers, 2,
        "the slab carrier plus the variable-size one"
    );
    assert_eq!(stats.live_blocks, 10);
    // Deliberately drop the allocator with live slab, mid and large blocks: `Drop` drains
    // the variable-size pool first and then the single-block table, so every mapping is
    // released exactly once (the `munmap` in `Carrier::unmap` is asserted in debug
    // builds, which makes a missed release loud here).
    drop(alloc);
}
