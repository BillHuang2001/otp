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

//! **Memory-accounting completeness** — the heap and off-heap bytes a process is charged for,
//! pinned as one report.
//!
//! A per-process memory report has to answer two questions this crate could not answer before:
//! *how many words does the process's **young generation** hold* and *how many bytes does its
//! whole heap plus its off-heap store amount to*.
//! Both were silently wrong or missing:
//!
//! 1. **The young occupancy was read from the current space only.**
//!    `Collector::young()` is the allocatable space; a
//!    [`grow_nursery`](beam_gc::Collector::grow_nursery) retires the space it replaces and
//!    points the allocator at a fresh one, so the retired space's live words — never moved,
//!    still evacuated by the next collection — were dropped from the figure.
//!    [`HeapObservable::heap_used_words`](beam_gc::HeapObservable::heap_used_words) is the
//!    chain-summed occupancy that fixes it (test 1 below), and
//!    [`HeapFootprint`](beam_gc::HeapFootprint) is the struct that reports it.
//! 2. **The off-heap store had no public surface at all**, and no single struct said "these are
//!    the bytes this crate accounts for". [`HeapFootprint::of`](beam_gc::HeapFootprint::of)
//!    is that struct: reserved heap capacity, live off-heap payload, and the roster's own slot
//!    array (test 2 below).
//!
//! ## Test 1 — `retired_nursery_spaces_are_accounted`
//!
//! Three spaces of chain (two growths) on the copying collector and on **both**
//! [`AnyCollector`](beam_gc::AnyCollector) strategies: the chain-summed occupancy equals the
//! hand-summed occupancy of every space and is **strictly greater** than the current-space read,
//! while the footprint's used words equal `heap_used_words() + mature used words` and its
//! capacities equal the collector's own `heap_size_words()`/`total_heap_size_words()`.
//!
//! ## Test 2 — `accounted_bytes_track_the_allocators_live_bytes`
//!
//! The honesty gate on the byte figures: everything this crate accounts for must be a live
//! `beam_alloc` allocation, so the footprint's allocator-visible subset
//! ([`heap_bytes`](beam_gc::HeapFootprint::heap_bytes)) can never exceed the sum of the
//! allocators' own live bytes, and must stay within a measured band of it.
//!
//! **The measured band.** With the workload below (a 111 995-word nursery chain plus an
//! 8 192-word mature space, i.e. **961 496** reserved heap bytes, plus 256 off-heap payloads of
//! 4 096 bytes = **1 048 576** payload bytes) the observed ratio `heap_bytes() / live` is
//! **0.993889** — reproduced on every run, not an estimate. The chosen band is **[0.90, 1.00]**.
//! The remaining 12 360 bytes (≈0.6 %) are entirely the allocators' own accounting on top of the
//! figures the footprint reports, and they are **not** a footprint error:
//!
//! * 12 288 bytes — 48 bytes of `RefcBinary` header per payload (256 × 48), because the payload's
//!   allocation carries its 32-byte control header and size-class rounding while
//!   `overhead_bytes()` reports the **payload length** (measured: 4 144 live bytes per 4 096-byte
//!   payload);
//! * 72 bytes — the size-class rounding of the four heap blocks the collector owns (measured:
//!   961 568 live bytes for 961 496 reserved).
//!
//! Nothing else in this test binary allocates through `beam_alloc`, so that difference is the
//! whole of it.
//!
//! **The roster's slot array is observed, not pinned.** The 256 pushes leave the roster's `Vec` at
//! an *observed* capacity of 256 positions — **16 384** slot bytes on this target, the figure a run
//! prints — but that capacity is the allocator's growth schedule, not this crate's contract: any
//! `Vec` growth (doubling, 1.5×, `reserve_exact`) may differ between toolchains. The test therefore
//! pins the array only as a documented two-sided range — at least one slot position per push, at
//! most a doubling of the element count — and never as an exact capacity, so a differing growth
//! schedule cannot turn this test red for a reason unrelated to the accounting contract.
//!
//! The band discriminates: deleting **either** half collapses the ratio (payload-only →
//! 0.5185, capacity-only → 0.4754), which the two "each half is more than a quarter of `live`"
//! assertions below state explicitly — so the band cannot pass on one half alone.
//!
//! **Why the tests serialise.** `beam_alloc`'s registry is process-global and the tests of one
//! integration file are threads of one binary, so the two (allocating) tests here take
//! [`SERIAL`] and the byte figure is read while no other test in this binary is allocating.
//! Every allocation the workload makes is therefore its own, which is what makes the band
//! reproducible; the figure is still never a delta against a baseline, so the
//! `heap_bytes() <= live` assertion is a real invariant and not a restatement of the workload.
//!
//! Never asserted anywhere here: RSS, wall-clock time, or any other machine-dependent figure.
use core::mem::size_of;
use std::sync::{Mutex, MutexGuard};

use beam_alloc::RefcBinary;
use beam_gc::prelude::{
    AnyCollector, Collector, GcConfig, HeapFootprint, HeapObservable, HeapPolicy, OffHeap,
    OffHeapEntry, WORD_BYTES,
};
use beam_term::prelude::{make_small, NIL};

/// The serialisation lock over the process-global allocator registry (see the module docs).
static SERIAL: Mutex<()> = Mutex::new(());

/// Takes the serialisation lock, ignoring a previous test's panic (a poisoned lock must not turn
/// a failure into a cascade of spurious ones).
fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The policy every chain workload here starts from: an 8 192-word space in each generation, so
/// a growth retires a space large enough to matter to the figures.
const YOUNG_WORDS: usize = 8_192;
const MATURE_WORDS: usize = 8_192;

/// The number of off-heap payloads the byte workload registers.
const PAYLOAD_COUNT: usize = 256;
/// The payload length of each registered off-heap binary, in bytes.
const PAYLOAD_LEN: usize = 4_096;
/// The band `heap_bytes() / live` must stay inside (see the module docs for the observed value
/// and for what the remaining difference is).
const ACCOUNTED_MIN_PERCENT: usize = 90;
const ACCOUNTED_MAX_PERCENT: usize = 100;

fn chain_policy() -> HeapPolicy {
    HeapPolicy {
        initial_young_words: YOUNG_WORDS,
        initial_mature_words: MATURE_WORDS,
        ..HeapPolicy::default()
    }
}

/// The sum of every allocator's live bytes — the honest upper bound on what this crate may
/// account for.
fn allocator_live_bytes() -> usize {
    beam_alloc::registry::all_stats()
        .iter()
        .map(|(_, stats)| stats.live_bytes)
        .sum()
}

/// Fills `cells` cons cells into the collector's current nursery space.
fn fill(collector: &mut Collector, cells: i64) {
    let heap = collector.young_mut().heap_mut();
    for i in 0..cells {
        heap.cons(make_small(i), NIL)
            .expect("the cell fits the space");
    }
}

// ---------------------------------------------------------------------------
// 1. Retired nursery spaces are accounted for
// ---------------------------------------------------------------------------

#[test]
fn retired_nursery_spaces_are_accounted() {
    let _guard = serial();

    // --- the copying collector ---
    let mut c = Collector::new(chain_policy()).expect("collector");
    // 600 cons cells = 1 200 words in the first space. Nothing ever reclaims them: the growth
    // below retires the space with those words still live in it.
    fill(&mut c, 600);
    let first_used = c.young().used_words();
    assert_eq!(first_used, 1_200, "600 cons cells are two words each");

    c.grow_nursery(20_000).expect("the nursery grows");
    assert_eq!(
        c.young_space_count(),
        2,
        "the growth retained the first space"
    );
    assert_eq!(
        c.young().used_words(),
        0,
        "the fresh space starts empty — the 1 200 words are in the retired one"
    );

    // Part of the fresh space, then a second growth: the chain now has two retired spaces, one
    // of them holding the words above and one holding these.
    fill(&mut c, 200);
    let second_used = c.young().used_words();
    assert_eq!(second_used, 400, "200 cons cells are two words each");
    c.grow_nursery(60_000).expect("the nursery grows again");
    assert_eq!(c.young_space_count(), 3, "two growths, two retired spaces");
    assert_eq!(c.young().used_words(), 0, "the third space starts empty");

    let chain_used = first_used + second_used + c.young().used_words();
    assert_eq!(
        c.heap_used_words(),
        chain_used,
        "the chain-summed occupancy is the sum over every space"
    );
    assert_eq!(
        c.heap_used_words(),
        1_600,
        "the two retired spaces' live words are both counted"
    );
    assert!(
        c.heap_used_words() > c.young().used_words(),
        "a current-space-only read drops every retired space: {} vs {}",
        c.heap_used_words(),
        c.young().used_words()
    );

    let footprint = HeapFootprint::of(&c, &OffHeap::new());
    assert_eq!(footprint.young_capacity_words, c.heap_size_words());
    assert_eq!(footprint.young_used_words, c.heap_used_words());
    assert_eq!(footprint.mature_capacity_words, c.mature().capacity_words());
    assert_eq!(footprint.mature_used_words, c.mature().used_words());
    assert_eq!(
        footprint.capacity_words(),
        c.total_heap_size_words(),
        "the capacity half agrees with the collector's own totals"
    );
    assert_eq!(
        footprint.used_words(),
        c.heap_used_words() + c.mature().used_words(),
        "the used half is the young occupancy plus the mature occupancy"
    );
    assert_eq!(footprint.used_words(), c.total_heap_used_words());
    assert!(
        footprint.used_words() <= footprint.capacity_words(),
        "occupancy can never exceed capacity"
    );

    // --- both AnyCollector strategies ---
    for config in [
        GcConfig::copying(chain_policy()),
        GcConfig::immix(chain_policy()),
    ] {
        let kind = config.mature();
        let mut any = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(any.kind(), kind, "the strategy was selected");

        let heap = any.young_mut().heap_mut();
        for i in 0..600i64 {
            heap.cons(make_small(i), NIL)
                .expect("the cell fits the space");
        }
        let first_used = any.young().used_words();
        assert_eq!(first_used, 1_200, "{kind:?}");

        any.grow_nursery(20_000).expect("the nursery grows");
        assert_eq!(any.young_space_count(), 2, "{kind:?}");
        let heap = any.young_mut().heap_mut();
        for i in 0..200i64 {
            heap.cons(make_small(i), NIL)
                .expect("the cell fits the space");
        }
        let second_used = any.young().used_words();
        any.grow_nursery(60_000).expect("the nursery grows again");
        assert_eq!(any.young_space_count(), 3, "{kind:?}");

        let chain_used = first_used + second_used + any.young().used_words();
        assert_eq!(any.heap_used_words(), chain_used, "{kind:?}");
        assert!(
            any.heap_used_words() > any.young().used_words(),
            "{kind:?}: the retired spaces are counted: {} vs {}",
            any.heap_used_words(),
            any.young().used_words()
        );

        let footprint = HeapFootprint::of(&any, &OffHeap::new());
        assert_eq!(
            footprint.young_capacity_words,
            any.heap_size_words(),
            "{kind:?}"
        );
        assert_eq!(
            footprint.young_used_words,
            any.heap_used_words(),
            "{kind:?}"
        );
        assert_eq!(
            footprint.mature_capacity_words,
            any.mature_capacity_words(),
            "{kind:?}"
        );
        assert_eq!(
            footprint.mature_used_words,
            any.mature_used_words(),
            "{kind:?}"
        );
        assert_eq!(
            footprint.capacity_words(),
            any.total_heap_size_words(),
            "{kind:?}"
        );
        assert_eq!(
            footprint.used_words(),
            any.heap_used_words() + any.mature_used_words(),
            "{kind:?}"
        );
        assert_eq!(
            footprint.used_words(),
            any.total_heap_used_words(),
            "{kind:?}"
        );
        assert!(
            footprint.used_words() <= footprint.capacity_words(),
            "{kind:?}: occupancy can never exceed capacity"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Accounted bytes track the allocators' live bytes
// ---------------------------------------------------------------------------

#[test]
fn accounted_bytes_track_the_allocators_live_bytes() {
    let _guard = serial();

    // (i) A grown nursery chain: two growths over a space the workload keeps live.
    let mut c = Collector::new(chain_policy()).expect("collector");
    fill(&mut c, 600);
    c.grow_nursery(20_000).expect("the nursery grows");
    fill(&mut c, 200);
    c.grow_nursery(60_000).expect("the nursery grows again");
    assert_eq!(
        c.young_space_count(),
        3,
        "the chain is what the capacity half reports"
    );

    // (ii) A known number of off-heap refcounted binaries of a known payload length.
    let mut roster = OffHeap::new();
    for _ in 0..PAYLOAD_COUNT {
        roster.push(RefcBinary::new(&[7u8; PAYLOAD_LEN]).expect("off-heap binary"));
    }

    let footprint = HeapFootprint::of(&c, &roster);
    let live = allocator_live_bytes();

    // The off-heap half, exactly.
    assert_eq!(
        footprint.off_heap_payload_bytes,
        PAYLOAD_COUNT * PAYLOAD_LEN,
        "every payload is charged its length"
    );
    assert_eq!(footprint.off_heap_entries, PAYLOAD_COUNT);

    // The roster array: one slot per position, none of it in the allocator-visible subset.
    assert_eq!(
        footprint.off_heap_slot_bytes,
        roster.slot_bytes(),
        "the field is the roster's own reserved array"
    );
    assert_eq!(
        roster.slot_count(),
        PAYLOAD_COUNT,
        "one reserved position per push: the roster is slot-stable, so no position is ever reused"
    );
    // The *capacity* of that reserved array is the allocator's business, never this crate's
    // contract: a `Vec`'s growth schedule (doubling, 1.5×, `reserve_exact`) may differ between
    // toolchains, so the figure is pinned as a two-sided range of the element count instead of an
    // exact capacity.
    let slot_bytes_per_position = size_of::<Option<OffHeapEntry>>();
    assert!(
        footprint.off_heap_slot_bytes >= PAYLOAD_COUNT * slot_bytes_per_position,
        "a Vec never holds more elements than its capacity, so the reserved array is at least one \
         slot position per push: {} slot bytes for {PAYLOAD_COUNT} slots of {slot_bytes_per_position} \
         bytes",
        footprint.off_heap_slot_bytes
    );
    assert!(
        footprint.off_heap_slot_bytes <= 2 * PAYLOAD_COUNT * slot_bytes_per_position,
        "nor more than a doubling of the element count, the std growth bound — the exact capacity \
         is the allocator's business, never this crate's contract: {} slot bytes for {PAYLOAD_COUNT} \
         slots of {slot_bytes_per_position} bytes",
        footprint.off_heap_slot_bytes
    );
    assert_eq!(
        footprint.total_bytes(),
        footprint.heap_bytes() + footprint.off_heap_slot_bytes,
        "the roster array is counted by the total only"
    );

    // The HARD honesty invariant: nothing this crate accounts for may exceed the allocator's own
    // live bytes — the guard against double counting and invented bytes.
    assert!(
        footprint.heap_bytes() <= live,
        "accounted {} bytes exceed the allocators' {} live bytes",
        footprint.heap_bytes(),
        live
    );

    // The measured band (see the module docs: observed 0.993889, band [0.90, 1.00]).
    let observed = footprint.heap_bytes() as f64 / live as f64;
    assert!(
        footprint.heap_bytes() * 100 >= live * ACCOUNTED_MIN_PERCENT,
        "accounted/live = {observed:.6} is below the {ACCOUNTED_MIN_PERCENT}% band: the \
         footprint under-reports a half of what it accounts for ({:?})",
        footprint
    );
    assert!(
        footprint.heap_bytes() * 100 <= live * ACCOUNTED_MAX_PERCENT,
        "accounted/live = {observed:.6} is above the {ACCOUNTED_MAX_PERCENT}% band: the \
         footprint over-reports ({:?})",
        footprint
    );
    // Why the band discriminates: neither half may be a rounding detail.
    assert!(
        footprint.capacity_bytes() * 4 > live,
        "the heap-capacity half is more than a quarter of live ({} of {live}), so dropping it \
         cannot pass the band",
        footprint.capacity_bytes()
    );
    assert!(
        footprint.off_heap_payload_bytes * 4 > live,
        "the payload half is more than a quarter of live ({} of {live}), so dropping it cannot \
         pass the band",
        footprint.off_heap_payload_bytes
    );

    // The word/byte conversion is the single constant the footprint uses.
    assert_eq!(
        footprint.capacity_bytes(),
        footprint.capacity_words() * WORD_BYTES
    );
    assert_eq!(footprint.used_bytes(), footprint.used_words() * WORD_BYTES);
    assert!(
        footprint.used_bytes() <= footprint.capacity_bytes(),
        "the used half is a sub-range of the reserved half"
    );
    assert!(
        WORD_BYTES * 8 == u64::BITS as usize,
        "one term word per machine word"
    );

    // The record, for the report and for a reader of a failing run.
    println!(
        "heap_accounting: capacity={} words ({} bytes), used={} words ({} bytes), \
         payload={} bytes, roster={} entries in {} slot bytes, heap_bytes={}, \
         total_bytes={}, allocator live={} bytes, accounted/live={:.6}",
        footprint.capacity_words(),
        footprint.capacity_bytes(),
        footprint.used_words(),
        footprint.used_bytes(),
        footprint.off_heap_payload_bytes,
        footprint.off_heap_entries,
        footprint.off_heap_slot_bytes,
        footprint.heap_bytes(),
        footprint.total_bytes(),
        live,
        observed
    );
}
