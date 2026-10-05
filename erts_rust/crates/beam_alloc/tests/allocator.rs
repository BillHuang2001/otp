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
fn the_large_threshold_splits_the_slab_and_dedicated_paths() {
    let alloc = small_allocator("threshold");
    // At the threshold the block is carved from a slab carrier, so the footprint
    // jumps to the whole carrier.
    let at_threshold = layout(1024, MIN_ALIGN);
    let ptr = alloc.alloc(at_threshold).expect("alloc");
    assert_eq!(alloc.carrier_bytes(), 64 * 1024);
    assert_eq!(alloc.carrier_count(), 1);
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, at_threshold) };

    // One byte above it, the block gets a carrier of its own: header + payload,
    // rounded up to the allocation unit.
    let above = layout(1025, MIN_ALIGN);
    let ptr = alloc.alloc(above).expect("alloc");
    let dedicated = alloc.carrier_bytes() - 64 * 1024;
    assert!(
        (16 + 1025..=16 + 1025 + 15).contains(&dedicated),
        "dedicated carrier should be the block itself, saw {dedicated}"
    );
    assert_eq!(alloc.carrier_count(), 2);
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, above) };
    assert_eq!(
        alloc.carrier_count(),
        1,
        "the dedicated carrier went back to the OS"
    );
}

#[test]
fn large_blocks_get_dedicated_carriers_that_are_released_on_free() {
    let alloc = small_allocator("large");
    let baseline = alloc.stats();
    let l = layout(64 * 1024, MIN_ALIGN);
    let first = alloc.alloc(l).expect("alloc");
    let second = alloc.alloc(l).expect("alloc");
    assert_ne!(first.as_ptr(), second.as_ptr());
    // SAFETY: each allocation holds 64 KiB of writable bytes.
    unsafe {
        ptr::write_bytes(first.as_ptr(), 0x11, 64 * 1024);
        ptr::write_bytes(second.as_ptr(), 0x22, 64 * 1024);
    }
    let stats = alloc.stats();
    assert_eq!(stats.carriers, baseline.carriers + 2);
    assert_eq!(stats.live_blocks, 2);
    assert!(stats.carrier_bytes >= baseline.carrier_bytes + 128 * 1024);
    assert_eq!(stats.peak_live_bytes, stats.live_bytes);

    // SAFETY: both are live allocations on this allocator, freed exactly once.
    unsafe {
        alloc.dealloc(first, l);
        alloc.dealloc(second, l);
    }
    let stats = alloc.stats();
    assert_eq!(stats.carriers, baseline.carriers);
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
    assert_eq!(
        alloc.stats(),
        AllocStats::default(),
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

    // One byte past it, the request gets a carrier of its own.
    let above = layout(129, MIN_ALIGN);
    let ptr = alloc.alloc(above).expect("alloc");
    let dedicated = alloc.stats().live_bytes;
    assert!(
        (16 + 129..=16 + 129 + 15).contains(&dedicated),
        "saw {dedicated}"
    );
    // SAFETY: live allocation freed exactly once.
    unsafe { alloc.dealloc(ptr, above) };
    assert_eq!(alloc.stats().live_bytes, 0);
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
