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

//! Public-API tests for the **amortised capacity growth** of
//! [`beam_alloc::RefcBinary`] — the off-heap refcounted binary payload behind BEAM's
//! writable-binary idiom (`bs_init_writable` + `private_append`).
//!
//! Everything here is asserted with **structural** evidence (byte contents,
//! `len`/`capacity`/`reserved_bytes`, the growth counters a payload reports, the relocation
//! a move returns, the allocator's per-type and aggregate counters) — never with wall-clock
//! timing, so the tests are deterministic on a loaded host.
//!
//! The policy under test is the C's `GROW_PROC_BIN_SIZE`
//! (`erts/emulator/beam/erl_bits.c:70-73`): double up to
//! [`GROW_DOUBLING_LIMIT_BYTES`], then add 20 %. An append loop on top of it must therefore
//! copy O(n) bytes in total rather than O(n²) — the whole point of the capacity word.

use beam_alloc::binary::{
    GrowOutcome, GrowthCounters, RefcBinary, Relocation, DEFAULT_RESERVATION_FACTOR,
    GROW_DOUBLING_LIMIT_BYTES, MAX_PAYLOAD_BYTES,
};
use beam_alloc::{
    payload_capacity, AllocRef, AllocatorConfig, CarrierAllocator, PayloadGrowOutcome,
    PayloadRelocation, TypeId,
};
use beam_common::{AllocKind, BeamError};

/// An `AllocRef` on a real carrier allocator, so the tests exercise the actual allocation
/// paths (slab and dedicated carrier) and can inspect the counters.
fn test_allocator(name: &str) -> AllocRef {
    let config = AllocatorConfig::builder()
        .name(name)
        .carrier_size(64 * 1024)
        .build();
    CarrierAllocator::new(config)
        .expect("allocator construction maps nothing")
        .share()
}

/// The allocation type a payload is attributed to (the `BINARY` row of `binary_alloc`).
fn payload_type() -> TypeId {
    TypeId::from_name("BINARY").expect("the `BINARY` allocation type is declared")
}

/// A deterministic byte pattern, so an off-by-one or a misplaced chunk cannot hide.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// A position-dependent 64-byte-ish chunk, so a chunk written at the wrong offset changes
/// the bytes instead of coincidentally matching.
fn chunk(index: usize, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((index.wrapping_mul(37) + i + 1) % 251) as u8)
        .collect()
}

/// What one append loop observed.
struct AppendRun {
    payload: RefcBinary,
    expected: Vec<u8>,
    counters: GrowthCounters,
    all_in_place: bool,
}

/// Appends `chunk_len`-byte chunks until the payload holds `total` bytes, recording what the
/// growth protocol reported along the way.
fn append_loop(mut payload: RefcBinary, total: usize, chunk_len: usize) -> AppendRun {
    assert_eq!(total % chunk_len, 0, "the loop must stop on a chunk boundary");
    let mut expected = Vec::with_capacity(total);
    let mut all_in_place = true;
    for index in 0..(total / chunk_len) {
        let bytes = chunk(index, chunk_len);
        let outcome = payload.append(&bytes).expect("append succeeds within the cap");
        all_in_place &= matches!(outcome, GrowOutcome::InPlace { .. });
        expected.extend_from_slice(&bytes);
    }
    AppendRun {
        counters: payload.growth_counters(),
        payload,
        expected,
        all_in_place,
    }
}

/// **Test 1 (amortised linearity).** An accumulator loop that outgrows its reservation must
/// stay **amortised linear**: the bytes the moves copy are O(n), and going to 8× the size
/// must copy at least 4× more (a rebuild-every-append implementation would copy ~64× more).
#[test]
fn append_loop_to_64_kib_is_linear_not_quadratic() {
    let alloc = test_allocator("growth-linear");

    let small = append_loop(
        RefcBinary::with_capacity_and_allocator(alloc.clone(), 1024).expect("reserve"),
        8 * 1024,
        64,
    );
    let large = append_loop(
        RefcBinary::with_capacity_and_allocator(alloc.clone(), 1024).expect("reserve"),
        64 * 1024,
        64,
    );

    for (label, run, total) in [("8 KiB", &small, 8 * 1024usize), ("64 KiB", &large, 64 * 1024)] {
        assert_eq!(run.payload.len(), total, "{label}: length");
        assert_eq!(
            run.payload.as_bytes(),
            &run.expected[..],
            "{label}: the appended bytes are exactly the expected pattern"
        );
        assert!(
            run.payload.capacity() >= run.payload.len(),
            "{label}: the capacity covers the length"
        );
        assert!(
            run.payload.reserved_bytes() <= run.payload.max_capacity(),
            "{label}: the reservation respects the cap"
        );
        assert!(
            run.counters.moves <= 8,
            "{label}: doubling keeps the move count logarithmic, got {:?}",
            run.counters
        );
        assert!(
            run.counters.bytes_copied <= 2 * total,
            "{label}: the moves copy O(n) bytes, not O(n^2) ({:?})",
            run.counters
        );
        assert!(
            run.counters.in_place_grows + run.counters.moves > 0,
            "{label}: growth actually happened ({:?})",
            run.counters
        );
        assert_eq!(
            run.payload.refcount(),
            1,
            "{label}: a grown payload is still the unique handle"
        );
    }

    // The scaling assertion: 8× the payload copies at least 4× the bytes (quadratic growth
    // would be ~64×), and the amortised cost stays below 2 bytes copied per appended byte.
    assert!(
        small.counters.bytes_copied > 0,
        "the small run did have to move: {:?}",
        small.counters
    );
    assert!(
        large.counters.bytes_copied >= 4 * small.counters.bytes_copied,
        "8x the payload copies at least 4x the bytes (small: {:?}, large: {:?})",
        small.counters,
        large.counters
    );
    assert!(
        large.counters.bytes_copied <= 2 * large.payload.len(),
        "the amortised copy cost stays under 2 bytes per appended byte ({:?})",
        large.counters
    );
}

/// **Test 2 (in-place only).** A reservation wide enough for the whole accumulation is
/// consumed **entirely in place**: no allocation, no copy, and — the frozen-contract point —
/// `data_ptr()` never changes, so no on-heap `BinRef.val` word needs republishing.
#[test]
fn a_generous_reservation_grows_entirely_in_place() {
    let alloc = test_allocator("growth-in-place");
    let payload = RefcBinary::with_limits(alloc, 1024, 64 * 1024, 1 << 20).expect("reserve");
    assert_eq!(payload.capacity(), 1024);
    assert_eq!(payload.reserved_bytes(), 64 * 1024);
    assert_eq!(payload.len(), 0);
    let before = payload.data_ptr();

    let run = append_loop(payload, 64 * 1024, 64);

    assert_eq!(run.payload.len(), 64 * 1024);
    assert_eq!(
        run.payload.as_bytes(),
        &run.expected[..],
        "the in-place path appends the exact bytes"
    );
    assert!(
        run.all_in_place,
        "every outcome inside the reservation is InPlace"
    );
    assert_eq!(run.counters.moves, 0, "nothing moved");
    assert_eq!(run.counters.bytes_copied, 0, "nothing was copied");
    assert!(
        run.counters.in_place_grows > 0,
        "the capacity was committed in place ({:?})",
        run.counters
    );
    assert_eq!(
        run.payload.data_ptr(),
        before,
        "an in-place grow leaves the payload address — and every BinRef.val — valid"
    );
    assert!(run.payload.capacity() >= run.payload.len());
}

/// **Test 3 (the relocation protocol).** A move must report where the payload went *and*
/// preserve its bytes exactly, so a host can republish the term from the relocation alone.
#[test]
fn growth_preserves_payload_contents_and_reports_the_relocation() {
    let alloc = test_allocator("growth-moved");
    let mut payload = RefcBinary::with_limits(alloc, 64, 64, 1 << 20).expect("reserve");
    assert_eq!(payload.capacity(), 64);
    assert_eq!(payload.reserved_bytes(), 64);

    // Fits exactly: no growth at all.
    let first = chunk(0, 64);
    assert!(matches!(
        payload.append(&first).expect("fits"),
        GrowOutcome::InPlace { capacity: 64 }
    ));
    assert_eq!(payload.growth_counters().moves, 0);
    let before = payload.data_ptr();
    let before_bytes = payload.as_bytes().to_vec();
    assert_eq!(before_bytes, first);

    // One byte more cannot fit the reservation: this must move.
    let outcome = payload.append(&[0xAB]).expect("growable");
    let GrowOutcome::Moved(relocation) = outcome else {
        panic!("expected a move once the reservation is exhausted, got {outcome:?}");
    };
    assert_eq!(
        relocation.from(),
        before,
        "`from` is the pre-move data pointer"
    );
    assert_eq!(
        relocation.to(),
        payload.data_ptr(),
        "`to` is the payload's new data pointer"
    );
    assert_eq!(relocation.len(), 64, "the 64 live bytes were copied");
    assert_eq!(
        relocation.delta(),
        (relocation.to() as isize) - (relocation.from() as isize),
        "delta is `to - from`"
    );
    assert_eq!(relocation.copied_bytes(), 64);
    assert!(!relocation.is_empty());

    assert_eq!(payload.len(), 65);
    assert_eq!(
        &payload.as_bytes()[..64],
        &before_bytes[..],
        "the move preserved the payload byte for byte"
    );
    assert_eq!(payload.as_bytes()[64], 0xAB, "the appended byte landed last");
    let counters = payload.growth_counters();
    assert_eq!(counters.moves, 1);
    assert_eq!(counters.bytes_copied, 64);
}

/// **Test 4 (`compact`).** `compact` is the "publish then shrink" step: it releases the
/// spare capacity (so the payload stops being charged for memory it will not use) without
/// touching the bytes, and the payload stays growable afterwards.
#[test]
fn compact_releases_the_spare_capacity_without_changing_the_bytes() {
    let alloc = test_allocator("growth-compact");
    let mut payload = RefcBinary::with_capacity_and_allocator(alloc, 1024).expect("reserve");
    assert_eq!(payload.capacity(), 1024);
    assert_eq!(
        payload.reserved_bytes(),
        1024 * DEFAULT_RESERVATION_FACTOR,
        "`with_capacity` reserves `capacity * DEFAULT_RESERVATION_FACTOR`"
    );

    let data = pattern(100);
    payload.append(&data).expect("fits the reservation easily");
    assert_eq!(payload.len(), 100);
    let before = payload.as_bytes().to_vec();

    let outcome = payload.compact().expect("compaction succeeds");
    assert!(
        matches!(outcome, GrowOutcome::Moved(_)),
        "releasing a 4 KiB reservation is a real reallocation, got {outcome:?}"
    );
    assert_eq!(payload.len(), 100);
    assert_eq!(
        payload.capacity(),
        payload.len(),
        "no spare capacity survives compaction"
    );
    assert_eq!(
        payload.reserved_bytes(),
        112,
        "the reservation is `len` rounded up to UNIT_SIZE (100 -> 112)"
    );
    assert_eq!(payload.as_bytes(), &before[..], "the bytes are untouched");

    // A compacted payload is not frozen: it re-grows on demand.
    let more = pattern(20);
    let outcome = payload.append(&more).expect("it can grow again");
    assert!(
        matches!(outcome, GrowOutcome::Moved(_)),
        "the tight reservation forces a real grow, got {outcome:?}"
    );
    assert_eq!(payload.len(), 120);
    let mut expected = before;
    expected.extend_from_slice(&more);
    assert_eq!(payload.as_bytes(), &expected[..]);
    assert!(payload.capacity() >= 120);
}

/// **Test 5 (the cap).** Exceeding the configured maximum is a typed error that changes
/// nothing, and a self-inconsistent limit set is refused before allocating.
#[test]
fn exceeding_the_configured_maximum_is_a_typed_error_and_changes_nothing() {
    let alloc = test_allocator("growth-cap");
    let mut payload = RefcBinary::with_limits(alloc.clone(), 64, 64, 256).expect("reserve");
    assert_eq!(payload.max_capacity(), 256);

    for index in 0..4 {
        payload.append(&chunk(index, 64)).expect("grows up to the cap");
    }
    assert_eq!(payload.len(), 256, "the payload reached the cap exactly");
    assert_eq!(payload.capacity(), 256);

    let before_len = payload.len();
    let before_capacity = payload.capacity();
    let before_ptr = payload.data_ptr();
    let before_bytes = payload.as_bytes().to_vec();
    assert_eq!(payload.refcount(), 1);

    match payload.append(&[0xFF]).expect_err("the cap forbids growth") {
        BeamError::Alloc(failure) => assert_eq!(
            failure.kind,
            AllocKind::LargeObject,
            "a too-large payload request is a large-object failure"
        ),
        other => panic!("expected an allocation failure, got {other:?}"),
    }
    assert_eq!(payload.len(), before_len, "the length is unchanged");
    assert_eq!(payload.capacity(), before_capacity, "the capacity is unchanged");
    assert_eq!(payload.data_ptr(), before_ptr, "the payload did not move");
    assert_eq!(payload.as_bytes(), &before_bytes[..], "the bytes are unchanged");

    // Self-inconsistent limits are `BadArg` and allocate nothing.
    assert!(
        matches!(
            RefcBinary::with_limits(alloc.clone(), 128, 64, 1 << 20),
            Err(BeamError::BadArg)
        ),
        "capacity may not exceed the reservation"
    );
    assert!(
        matches!(
            RefcBinary::with_limits(alloc.clone(), 64, 128, 64),
            Err(BeamError::BadArg)
        ),
        "the reservation may not exceed the maximum"
    );
    assert!(
        matches!(
            RefcBinary::with_limits(
                alloc,
                64,
                MAX_PAYLOAD_BYTES.saturating_add(1),
                MAX_PAYLOAD_BYTES.saturating_add(1)
            ),
            Err(BeamError::BadArg)
        ),
        "the reservation may not exceed the allocator's single-block ceiling"
    );
}

/// **Test 6 (exclusivity).** A payload another handle still shares must refuse growth (the
/// C's `ASSERT(refc == 1)` precondition) rather than reallocate behind its back.
#[test]
fn a_shared_payload_refuses_growth() {
    let alloc = test_allocator("growth-shared");
    let mut payload = RefcBinary::with_capacity_and_allocator(alloc, 1024).expect("reserve");
    payload.append(&pattern(10)).expect("fits");
    let before = payload.as_bytes().to_vec();
    let before_ptr = payload.data_ptr();

    let clone = payload.clone();
    assert_eq!(payload.refcount(), 2);

    let error = payload.reserve(1024).expect_err("a shared payload cannot grow");
    assert!(
        matches!(error, BeamError::BadArg),
        "a refused grow is a typed badarg, got {error:?}"
    );
    assert_eq!(payload.as_bytes(), &before[..], "the payload is unchanged");
    assert_eq!(payload.len(), 10);
    assert_eq!(payload.data_ptr(), before_ptr, "the payload did not move");
    assert_eq!(
        payload.growth_counters(),
        GrowthCounters::default(),
        "a refused grow bumps no counter"
    );

    drop(clone);
    assert_eq!(payload.refcount(), 1);
    payload.reserve(4096).expect("the unique handle may grow");
    assert!(
        payload.capacity() >= 4106,
        "the reservation was raised for `len + extra`"
    );
}

/// **Test 7 (allocator cleanliness).** Growth must be allocator-clean: a move frees the
/// block it replaces, so the live block count never drifts and the per-type and aggregate
/// counters unwind together.
#[test]
fn growth_is_allocator_clean() {
    let alloc = test_allocator("growth-clean");
    let ty = payload_type();
    let base_slot = alloc.type_stats(ty);
    let base_stats = alloc.stats();

    {
        let mut payload = RefcBinary::with_limits(alloc.clone(), 64, 64, 1 << 20).expect("reserve");
        payload.append(&pattern(64)).expect("fits");
        let slot = alloc.type_stats(ty);
        assert_eq!(slot.blocks, base_slot.blocks + 1, "one live payload block");
        assert_eq!(
            slot.bytes - base_slot.bytes,
            alloc.stats().live_bytes - base_stats.live_bytes,
            "per-type bytes equal the aggregate delta"
        );

        // A grow-with-move, then a compact, then another grow-with-move.
        payload.append(&[7u8; 200]).expect("moves");
        payload.compact().expect("compacts");
        payload.append(&[3u8; 10_000]).expect("moves again");
        assert!(
            payload.growth_counters().moves >= 2,
            "the accounting below is about moves: {:?}",
            payload.growth_counters()
        );

        let after = alloc.type_stats(ty);
        assert_eq!(
            after.blocks,
            base_slot.blocks + 1,
            "every move freed the block it replaced: exactly one block stays live"
        );
        assert_eq!(
            after.bytes - base_slot.bytes,
            alloc.stats().live_bytes - base_stats.live_bytes,
            "per-type bytes still equal the aggregate delta after the moves"
        );
    }

    let final_slot = alloc.type_stats(ty);
    assert_eq!(final_slot.blocks, base_slot.blocks, "no leaked block");
    assert_eq!(final_slot.bytes, base_slot.bytes, "no leaked bytes");
    let final_stats = alloc.stats();
    assert_eq!(final_stats.live_blocks, base_stats.live_blocks);
    assert_eq!(final_stats.live_bytes, base_stats.live_bytes);
}

/// **Test 8a (`set_len` must zero-fill).** Growing the logical length must zero-fill what it
/// exposes: `as_bytes` would otherwise read uninitialised off-heap bytes.
#[test]
fn set_len_zero_fills_the_newly_exposed_bytes() {
    let alloc = test_allocator("set-len-zero");
    let mut payload = RefcBinary::with_limits(alloc, 32, 32, 1 << 20).expect("reserve");
    payload.append(&[0xFF; 4]).expect("fits");
    assert_eq!(payload.len(), 4);

    payload.set_len(16).expect("within the capacity");
    assert_eq!(payload.len(), 16);
    assert_eq!(&payload.as_bytes()[..4], &[0xFF; 4], "the live bytes survive");
    assert_eq!(
        &payload.as_bytes()[4..],
        &[0u8; 12],
        "the newly exposed bytes are zeroed"
    );

    // Shrinking is free and rewrites nothing.
    payload.set_len(2).expect("shrink");
    assert_eq!(payload.as_bytes(), &[0xFF, 0xFF]);
    assert_eq!(
        payload.capacity(),
        32,
        "set_len never reallocates on its own"
    );
}

/// **Test 8b.** `set_len` beyond the capacity is a typed error that changes nothing.
#[test]
fn set_len_beyond_capacity_is_badarg() {
    let alloc = test_allocator("set-len-badarg");
    let mut payload = RefcBinary::with_limits(alloc, 8, 8, 1 << 20).expect("reserve");
    let before_ptr = payload.data_ptr();

    match payload.set_len(9) {
        Err(BeamError::BadArg) => {}
        other => panic!("expected a typed badarg, got {other:?}"),
    }
    assert_eq!(payload.len(), 0, "the length is unchanged");
    assert_eq!(payload.capacity(), 8, "the capacity is unchanged");
    assert_eq!(payload.data_ptr(), before_ptr, "the payload did not move");

    // Exactly at the capacity is fine.
    payload.set_len(8).expect("the capacity is 8");
    assert_eq!(payload.as_bytes(), &[0u8; 8]);
}

/// **Test 9 (`payload_capacity`).** The raw-pointer query reads the live capacity back from
/// a payload pointer — the question a host asks about a `BinRef.val` it just read from a live
/// `ErlSubBits`.
#[test]
fn payload_capacity_sees_the_live_capacity() {
    let alloc = test_allocator("growth-raw-capacity");
    let mut payload = RefcBinary::with_capacity_and_allocator(alloc, 256).expect("reserve");
    assert_eq!(
        unsafe { payload_capacity(payload.data_ptr()) },
        payload.capacity(),
        "the raw view agrees with the handle before any growth"
    );

    payload.append(&pattern(300)).expect("an in-place grow");
    assert_eq!(
        unsafe { payload_capacity(payload.data_ptr()) },
        payload.capacity(),
        "and after an in-place grow"
    );

    payload.append(&[0u8; 10_000]).expect("a move");
    assert!(
        payload.growth_counters().moves >= 1,
        "the second append moved the payload"
    );
    assert_eq!(
        unsafe { payload_capacity(payload.data_ptr()) },
        payload.capacity(),
        "and after a move (asked at the *new* address, the only valid one)"
    );
}

/// **Test 10 (the frozen term contract).** Spare capacity is invisible to term sizing: a refc
/// term built over the payload AFTER its final grow counts the 8-word on-heap
/// `[BinRef][ErlSubBits]` pair and reports the *logical* bit length — never the reservation.
///
/// A `Moved` outcome invalidates a term built *before* the move, so this builds the term
/// after the last growth: that is exactly the host-side republish rule.
#[test]
fn a_refc_term_sees_only_the_logical_length_not_the_spare_capacity() {
    use beam_term::refc::{
        refc_bytes, refc_size, write_refc_binary, ERL_REFC_BITS_SIZE,
    };
    use beam_term::size::size_object;
    use beam_term::Eterm;

    let alloc = test_allocator("growth-term");
    let mut payload = RefcBinary::with_limits(alloc, 64, 64, 1 << 20).expect("reserve");
    payload.append(&pattern(64)).expect("fits");
    payload.append(&[7u8; 100]).expect("moves");
    assert_eq!(payload.len(), 164);
    assert!(
        payload.reserved_bytes() > payload.len(),
        "the payload really does carry spare capacity ({:?} reserved vs {} live)",
        payload.reserved_bytes(),
        payload.len()
    );

    let mut heap = beam_term::heap::Heap::new(64);
    // SAFETY: the heap has 8 spare words for the `[BinRef][ErlSubBits]` pair, `data_ptr` is
    // the payload's first byte (16-byte aligned, so the flag-bit assert holds) and the
    // payload stays alive for the whole test.
    let term = unsafe {
        let hp = heap
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("the pair fits the test heap");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            0,
            payload.len() * 8,
        )
    };

    assert_eq!(
        size_object(term),
        ERL_REFC_BITS_SIZE,
        "the spare capacity is off-heap: term sizing counts the 8-word pair only"
    );
    // SAFETY: `term` is the pair just written and its payload is held by `payload`.
    unsafe {
        assert_eq!(
            refc_size(term),
            payload.len() * 8,
            "the term records the LOGICAL bit length, not the reservation"
        );
        assert_eq!(
            refc_bytes(term),
            payload.as_bytes(),
            "and reads exactly the live payload bytes"
        );
    }
}

/// The exported growth surface: the two constants that express the C policy, plus the crate
/// root's non-colliding aliases (the payload-growth outcome must not be confused with
/// `reserved_heap::GrowOutcome`, the *heap* growth verdict).
#[test]
fn the_growth_surface_is_exported_under_non_colliding_names() {
    assert_eq!(
        GROW_DOUBLING_LIMIT_BYTES,
        1 << 24,
        "the C's `GROW_PROC_BIN_SIZE` switch point is 16 MiB"
    );
    assert_eq!(DEFAULT_RESERVATION_FACTOR, 4);
    const { assert!(MAX_PAYLOAD_BYTES > 0) };

    // Type identity, checked without constructing the payload-only types.
    fn same_outcome(outcome: PayloadGrowOutcome) -> GrowOutcome {
        outcome
    }
    fn same_relocation(relocation: PayloadRelocation) -> Relocation {
        relocation
    }
    let outcome = same_outcome(GrowOutcome::InPlace { capacity: 7 });
    assert_eq!(outcome, GrowOutcome::InPlace { capacity: 7 });
    assert_eq!(
        core::mem::size_of::<fn(PayloadRelocation) -> Relocation>(),
        core::mem::size_of::<fn(Relocation) -> Relocation>()
    );
    let _ = same_relocation;
}
