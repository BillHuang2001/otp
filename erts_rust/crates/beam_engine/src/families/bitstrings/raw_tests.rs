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

//! `raw.rs`'s unit tests: the frozen-layout accessors (heap bits, the `ErlSubBits`
//! match context, bignums, the size-dispatching result builders) and the in-place
//! writable accumulator, including the split-pair canary case that proves a payload
//! republish writes only the two pointer words.

use super::*;
// The writer a `bs_create_bin` build drives over this sink, and what it produces (the
// split-pair cases assert the in-place `Term` form, never a copy-rebuild).
use super::super::{BitWriter, Built};
use crate::context::{ExecutionContext, MinimalContext};

/// `make_binary` picks the frozen representation: an on-heap `ErlHeapBits` at
/// the 64-byte limit, an off-heap refc binary above it — and the read path
/// (`heap_bits`/`subject_bytes`) recovers the same bytes either way.
#[test]
fn make_binary_picks_the_frozen_representation() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let heap_bytes: Vec<u8> = (0..64u8).collect();
    let heap = make_binary(&mut machine, &mut ctx, &heap_bytes, 64 * 8).unwrap();
    // SAFETY: `heap` is a live bitstring term built on the line above.
    assert!(!unsafe { beam_term::refc::is_refc_binary(heap) });
    assert_eq!(subject_bytes(heap).unwrap(), (heap_bytes.as_slice(), 0));
    assert_eq!(heap_bits(heap).unwrap().1, 64 * 8);

    let refc_bytes: Vec<u8> = (1..=65u8).collect();
    let refc = make_binary(&mut machine, &mut ctx, &refc_bytes, 65 * 8).unwrap();
    // SAFETY: `refc` is a live bitstring term built on the line above.
    assert!(unsafe { beam_term::refc::is_refc_binary(refc) });
    assert_eq!(subject_bytes(refc).unwrap(), (refc_bytes.as_slice(), 0));
    assert_eq!(heap_bits(refc).unwrap().1, 65 * 8);
    // The generic reader (`terms::binary_bytes`) is refc-aware too.
    assert_eq!(
        crate::terms::binary_bytes(refc).unwrap(),
        refc_bytes.as_slice()
    );
}

/// A non-byte-aligned refc result keeps its exact bit count and zeroes the
/// unused low bits of the final byte.
#[test]
fn make_binary_masks_the_trailing_bits_of_a_refc_result() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let bytes = vec![0xFFu8; 65];
    let nbits = 65 * 8 - 5;
    let term = make_binary(&mut machine, &mut ctx, &bytes, nbits).unwrap();
    // SAFETY: `term` is a live bitstring term built on the line above.
    assert!(unsafe { beam_term::refc::is_refc_binary(term) });
    assert_eq!(heap_bits(term).unwrap().1, nbits);
    let (read, off) = subject_bytes(term).unwrap();
    assert_eq!(off, 0, "a full refc binary's window is byte-aligned");
    assert_eq!(read.len(), 65);
    assert_eq!(read[64], 0b1110_0000);
}

/// `make_writable` is the 0-bit refc window `bs_init_writable` returns: its
/// payload is live (so a following append can seed from it) but it covers no
/// bits.
#[test]
fn make_writable_returns_a_zero_bit_refc_window() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let term = make_writable(&mut machine, &mut ctx, 256).unwrap();
    // SAFETY: `term` is a live bitstring term built on the line above.
    assert!(unsafe { beam_term::refc::is_refc_binary(term) });
    let (_, bits, _) = heap_bits(term).unwrap();
    assert_eq!(bits, 0);
    assert_eq!(subject_bytes(term).unwrap(), (&[] as &[u8], 0));
}

/// A refc binary whose window starts on a **non-byte-aligned** bit is a readable
/// subject: `heap_bits` reports its window offset (`start % 8`) and
/// `subject_bytes` hands out a slice that starts at the byte holding the window's
/// first bit and covers every window bit.
///
/// The payload bytes `[0b1010_1100, 0b0011_0101, 0b1000_0000]` (MSB-first) with a
/// window of bits `1..17` (offset 1, size 16) hold `0b0101_1000_0110_1011`.
#[test]
fn a_non_byte_aligned_refc_window_is_readable() {
    let mut ctx = MinimalContext::new();
    // A word-aligned payload buffer (the frozen `write_refc_binary` base-alignment
    // contract); the bytes are little-endian in the buffer, so write them byte by
    // byte rather than relying on the word's memory order.
    let mut payload: Vec<Eterm> = vec![Eterm::default(); 2];
    let want_bytes = [0b1010_1100u8, 0b0011_0101u8, 0b1000_0000u8];
    // SAFETY: `payload` holds 2 words (16 bytes) and `want_bytes` is 3 bytes, so
    // the copy is in bounds within `payload`'s own allocation.
    unsafe {
        core::ptr::copy_nonoverlapping(
            want_bytes.as_ptr(),
            payload.as_mut_ptr().cast::<u8>(),
            want_bytes.len(),
        );
    }
    let base = payload.as_ptr().cast::<u8>();
    let val = payload.as_mut_ptr();
    let hp = ctx
        .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
        .expect("the arena hands out 8 words");
    // SAFETY: `hp` points at `ERL_REFC_BITS_SIZE` exclusively-owned words in the
    // context's pointer-stable arena, `val`/`base` point at the live, word-aligned
    // `payload`, and `next` is null. The window is bits `1..17` — deliberately not
    // byte-aligned. The term is used only while `ctx`/`payload` live.
    let refc = unsafe {
        beam_term::refc::write_refc_binary(hp, val, core::ptr::null_mut(), base, 1, 16)
    };
    let (bits, off) = {
        let (_, bits, off) = heap_bits(refc).unwrap();
        (bits, off)
    };
    assert_eq!(bits, 16);
    assert_eq!(off, 1, "the window starts one bit into the first byte");
    let (bytes, read_off) = subject_bytes(refc).unwrap();
    assert_eq!(read_off, 1);
    // `ceil((1 + 16) / 8) == 3` bytes from `base`.
    assert_eq!(bytes, want_bytes.as_slice());
    // Reading the window's 16 bits at `off` reproduces the window bits.
    let mut got = 0u16;
    for i in 0..16usize {
        let bit_index = read_off + i;
        let bit = (bytes[bit_index / 8] >> (7 - (bit_index % 8))) & 1;
        got = (got << 1) | u16::from(bit);
    }
    assert_eq!(got, 0b0101_1000_0110_1011);
}

/// A bare off-heap binary reference (`HEADER_BIN_REF`) is not a readable subject:
/// the family must never read it as raw payload bytes.
#[test]
fn a_bare_bin_ref_is_not_a_readable_subject() {
    let mut ctx = MinimalContext::new();
    let hp = ctx
        .alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE)
        .expect("the arena hands out 8 words");
    // SAFETY: `hp` points at `ERL_REFC_BITS_SIZE` words; `write_refc_binary` fully
    // writes the `[BinRef][ErlSubBits]` pair, and `make_boxed(hp)` is the bare
    // `BinRef` term under test (a live block whose header is `HEADER_BIN_REF`).
    let bin_ref = unsafe {
        beam_term::refc::write_refc_binary(
            hp,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            hp.cast::<u8>(),
            0,
            0,
        );
        beam_term::make_boxed(hp)
    };
    assert!(heap_bits(bin_ref).is_none());
    assert!(subject_bytes(bin_ref).is_err());
}

/// A machine that has built no refc term enumerates no payload handle: the
/// accessor a host uses to seed a copy's source off-heap table starts empty.
#[test]
fn refc_binaries_is_empty_on_a_fresh_machine() {
    let machine = Machine::new();
    assert!(
        machine.refc_binaries().is_empty(),
        "a fresh machine has built no off-heap payload"
    );
}

/// Every off-heap payload the machine builds is **enumerable**: the refc term
/// `make_binary` returns appears in the accessor, and the exposed handle's
/// `data_ptr` is exactly the pointer the term's `BinRef.val` word holds — the
/// lookup key a host's copy resolves the payload by.
#[test]
fn a_built_refc_binary_is_enumerable_and_the_handle_is_the_bin_ref_payload() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let bytes: Vec<u8> = (1..=65u8).collect();
    let term = make_binary(&mut machine, &mut ctx, &bytes, 65 * 8).unwrap();
    // SAFETY: `term` is a live bitstring term built on the line above.
    assert!(unsafe { beam_term::refc::is_refc_binary(term) });

    let handles = machine.refc_binaries();
    assert_eq!(handles.len(), 1, "the one refc term the machine built");
    assert_eq!(handles[0].len(), 65);
    assert_eq!(handles[0].as_bytes(), bytes.as_slice());

    // The frozen representation: the boxed root is the `ErlSubBits`, whose `orig`
    // slot (word 4) is the boxed `BinRef` holding the payload pointer in its `val`
    // word (word 1) — `write_refc_binary`'s layout.
    // SAFETY: `term` is a live boxed `ErlSubBits`, so word 4 is the live term
    // `write_refc_binary` wrote there.
    let orig = unsafe { *boxed_val(term).add(4) };
    // SAFETY: `orig` is the live `BinRef` term read out of `orig` above.
    let (val, _next) = unsafe { beam_term::refc::bin_ref_parts(orig) };
    assert_eq!(
        val as *mut u8,
        handles[0].data_ptr(),
        "the term's BinRef.val is the enumerated handle's payload"
    );
}

/// Cloning an exposed handle takes a second reference to the payload, so the
/// payload outlives the machine that built it: dropping the whole machine leaves
/// the clone's bytes readable.
#[test]
fn a_cloned_refc_binary_handle_outlives_the_machine() {
    let handle = {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let _term = make_writable(&mut machine, &mut ctx, 65).unwrap();
        let handles = machine.refc_binaries();
        assert_eq!(handles.len(), 1);
        handles[0].clone()
    }; // the machine is dropped here, releasing its own handle.

    // The writable accumulator's payload is **capacity-carrying** with its whole
    // backing store live (`len == capacity`, the C's `bin_size`): the accumulator
    // covers no *bits* (the term's window is 0 bits), but every byte the payload
    // offers is initialised, because that is what makes a later relocation copy the
    // bytes an appender wrote through the payload pointer.
    assert_eq!(
        handle.len(),
        handle.capacity(),
        "the whole backing store is live"
    );
    assert_eq!(handle.capacity(), 65, "the capacity is the requested hint");
    assert!(
        handle.reserved_bytes() >= handle.capacity(),
        "the payload reserves at least its capacity"
    );
    assert_eq!(
        handle.as_bytes(),
        &[0u8; 65][..],
        "the live bytes are zero-filled, never uninitialised"
    );
    drop(handle);
}

// -----------------------------------------------------------------------
// The capacity-carrying writable payload (`bs_init_writable`) and its growth
// -----------------------------------------------------------------------

/// `make_writable` builds a **capacity-carrying** payload: its capacity is the
/// requested byte count, its reservation covers that capacity (`beam_alloc`'s 4x
/// policy) and carries the growth slack, its *live* length is that whole capacity
/// (zero-filled — the initialised backing store a relocation copies), the writable
/// registry answers that *real* capacity in bytes, and the term is still the frozen
/// 0-bit refc window over that payload.
#[test]
fn make_writable_builds_a_capacity_carrying_live_payload() {
    for hint in [0usize, 1, 8, 256, 1024] {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let term = make_writable(&mut machine, &mut ctx, hint).unwrap();

        let handles = machine.refc_binaries();
        assert_eq!(handles.len(), 1, "one payload backs the accumulator");
        let payload = &handles[0];
        assert_eq!(
            payload.len(),
            payload.capacity(),
            "hint {hint}: the whole backing store is live"
        );
        assert_eq!(payload.capacity(), hint, "hint {hint}: the capacity is the hint");
        assert!(
            payload.reserved_bytes() >= payload.capacity(),
            "hint {hint}: the reservation covers the capacity"
        );
        assert!(
            payload.reserved_bytes() >= hint.saturating_mul(beam_alloc::DEFAULT_RESERVATION_FACTOR)
                || payload.reserved_bytes() == beam_alloc::MAX_PAYLOAD_BYTES,
            "hint {hint}: the reservation carries the accumulator's growth slack"
        );
        assert!(
            payload.as_bytes().iter().all(|byte| *byte == 0),
            "hint {hint}: the live bytes are zero-filled"
        );
        // The registry is keyed on the payload, and answers its real capacity in bytes.
        assert_eq!(
            machine.writable_capacity(payload.data_ptr()),
            Some(payload.capacity()),
            "hint {hint}: the registry answers the payload's capacity"
        );

        // The term is the frozen 0-bit window over that payload.
        // SAFETY: `term` is the live refc binary `make_writable` just built.
        assert!(unsafe { beam_term::refc::is_refc_binary(term) });
        // SAFETY: as above — a live bitstring term.
        let (base, start, size) = unsafe { beam_term::bitstring_parts(term) };
        assert_eq!(
            base,
            payload.data_ptr() as *const u8,
            "hint {hint}: the window's base is the payload"
        );
        assert_eq!((start, size), (0, 0), "hint {hint}: a 0-bit window");
    }
}

/// An in-place grow keeps the payload's address and raises the registry's cached
/// capacity to what `beam_alloc` committed; a request past the reservation **moves**
/// the payload and re-keys the registry, leaving no stale key behind.
#[test]
fn grow_writable_payload_grows_in_place_then_moves_and_rekeys() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let _term = make_writable(&mut machine, &mut ctx, 8).unwrap();
    let base = machine.refc_binaries()[0].data_ptr();
    let reserved = machine.refc_binaries()[0].reserved_bytes();
    assert!(reserved >= 8, "the reservation covers the hint");

    // (a) Still inside the reservation: same address, raised capacity.
    let committed = reserved.min(20).max(9);
    let (in_place_base, in_place_capacity) = machine
        .grow_writable_payload(base, committed)
        .unwrap()
        .expect("a request inside the reservation grows in place");
    assert_eq!(in_place_base, base, "an in-place grow does not move the payload");
    assert!(
        in_place_capacity >= committed,
        "the committed capacity covers the request"
    );
    assert_eq!(in_place_capacity, machine.refc_binaries()[0].capacity());
    assert_eq!(
        machine.refc_binaries()[0].len(),
        in_place_capacity,
        "the growth keeps the whole (now wider) backing store live"
    );
    assert_eq!(
        machine.writable_capacity(base),
        Some(in_place_capacity),
        "the registry's cached capacity followed the grow"
    );

    // (b) Past the reservation: the payload is reallocated and the registry re-keyed.
    let wanted = reserved + 1024;
    let (moved_base, moved_capacity) = machine
        .grow_writable_payload(base, wanted)
        .unwrap()
        .expect("a request past the reservation still grows");
    assert_ne!(moved_base, base, "the payload was relocated");
    assert!(moved_capacity >= wanted, "the new capacity covers the request");
    assert_eq!(
        machine.refc_binaries()[0].data_ptr(),
        moved_base,
        "the machine's handle names the new block"
    );
    assert_eq!(
        machine.refc_binaries()[0].len(),
        moved_capacity,
        "the relocated payload's whole backing store is live"
    );
    assert_eq!(
        machine.writable_capacity(moved_base),
        Some(moved_capacity),
        "the registry was re-keyed to the new base"
    );
    assert_eq!(
        machine.writable_capacity(base),
        None,
        "no stale key is left naming the freed block"
    );
}

/// A zero capacity hint is reproduced faithfully (C's `erl_bits.c:2023` really does
/// allocate `bin_size = 0`), and such a payload still makes **progress**: growing it
/// asks for one byte, which exceeds a zero reservation, so `beam_alloc` relocates it to
/// a `UNIT_SIZE` reservation with a one-byte capacity — never a zero-progress loop.
#[test]
fn a_zero_capacity_writable_payload_grows_monotonically() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let _term = make_writable(&mut machine, &mut ctx, 0).unwrap();
    let base = machine.refc_binaries()[0].data_ptr();
    assert_eq!(machine.refc_binaries()[0].capacity(), 0);
    assert_eq!(machine.refc_binaries()[0].len(), 0);
    assert_eq!(machine.refc_binaries()[0].reserved_bytes(), 0);

    let (new_base, capacity) = machine
        .grow_writable_payload(base, 1)
        .unwrap()
        .expect("a zero-capacity payload can still grow");
    assert_ne!(new_base, base, "a zero reservation cannot grow in place");
    assert!(capacity >= 1, "the first growth buys at least the requested byte");
    assert_eq!(machine.refc_binaries()[0].capacity(), capacity);
    assert_eq!(
        machine.refc_binaries()[0].len(),
        capacity,
        "the first growth makes its whole backing store live"
    );
    assert_eq!(machine.writable_capacity(new_base), Some(capacity));
}

/// A grow is refused — `Ok(None)`, never an error — for a payload the machine does not
/// own, for one it never registered as writable, and for a **shared** one (a second
/// handle exists, exactly C's `erts_pin_writable_binary` condition). The payload and
/// the registry are unchanged by every refusal.
#[test]
fn grow_writable_payload_refuses_what_it_cannot_grow() {
    // (a) An ordinary (non-writable) refc binary: never a growth target.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let bytes = vec![7u8; 80];
    let _ordinary = make_binary(&mut machine, &mut ctx, &bytes, 640).unwrap();
    let ordinary_base = machine.refc_binaries()[0].data_ptr();
    assert_eq!(
        machine.grow_writable_payload(ordinary_base, 4096).unwrap(),
        None,
        "an ordinary binary's payload keeps its immutability"
    );
    assert_eq!(machine.refc_binaries()[0].capacity(), 80);
    assert_eq!(machine.writable_capacity(ordinary_base), None);

    // (b) A base no handle of this machine names.
    assert_eq!(
        machine.grow_writable_payload(core::ptr::null_mut(), 16).unwrap(),
        None
    );

    // (c) A *shared* writable payload: cloning the handle makes the refcount 2, which
    //     `beam_alloc` refuses to grow (a host adopted the payload).
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let _acc = make_writable(&mut machine, &mut ctx, 8).unwrap();
    let shared_base = machine.refc_binaries()[0].data_ptr();
    let adopted = machine.refc_binaries()[0].clone();
    assert_eq!(machine.refc_binaries()[0].refcount(), 2);
    assert_eq!(
        machine.grow_writable_payload(shared_base, 8192).unwrap(),
        None,
        "a payload another handle shares is never relocated"
    );
    assert_eq!(
        machine.refc_binaries()[0].data_ptr(),
        shared_base,
        "the shared payload did not move"
    );
    assert_eq!(machine.refc_binaries()[0].capacity(), 8, "nor did it grow");
    assert_eq!(
        machine.refc_binaries()[0].len(),
        8,
        "nor did its length move"
    );
    assert_eq!(
        machine.writable_capacity(shared_base),
        Some(8),
        "the registry entry is unchanged"
    );
    drop(adopted);
}

/// The **republish**: when the sink's growth relocates the payload, the accumulator's
/// `[BinRef][ErlSubBits]` pair is rewritten over the new block — same `Eterm`, same
/// window (`start`/`end`), same bytes — and the registry follows it.
#[test]
fn a_relocation_republishes_the_accumulator_pair() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let acc = make_writable(&mut machine, &mut ctx, 8).unwrap();

    // Seed three bytes through the sink (in place: 24 bits of a 64-bit capacity).
    let (base, capacity_bits, end_bits) = writable_append_target(&machine, acc).expect("writable");
    let mut sink = InPlace::new(base, capacity_bits, end_bits, acc);
    sink.copy_bytes(&[1, 2, 3]);
    let acc = sink.finish();
    let size_before = beam_term::size::size_object(acc);
    // The two blocks of the pair, captured before the growth: the republish keeps both
    // addresses, so they are the identity this case asserts afterwards. (Nothing
    // collects in a `MinimalContext`, so these words cannot go stale in between.)
    // SAFETY: `acc` is the live refc-binary accumulator, so its `orig` slot is the live
    // `BinRef` of that same pair.
    let (pair_block, bin_ref_block) = unsafe {
        let root = boxed_val(acc);
        (root, boxed_val(*root.add(beam_term::SUB_BITS_ORIG_WORD)))
    };

    // Grow well past the 8-byte capacity / 32-byte reservation: the payload moves.
    let (base, capacity_bits, end_bits) = writable_append_target(&machine, acc).expect("writable");
    let mut sink = InPlace::new(base, capacity_bits, end_bits, acc);
    assert!(
        sink.grow(&mut machine, 64 * 8).unwrap(),
        "the sink grew past its reservation"
    );
    assert_ne!(sink.base, base, "the payload was relocated");
    assert!(
        sink.remaining_bits() >= 64 * 8,
        "the refreshed capacity is in bits"
    );
    sink.copy_bytes(&[4, 5, 6]);
    let grown = sink.finish();
    assert_eq!(grown, acc, "the republish keeps the accumulator's term");

    // The pair now names the NEW payload, with the same window start.
    let new_base = machine.refc_binaries()[0].data_ptr();
    assert_ne!(new_base, base);
    // SAFETY: `grown` is the live refc-binary accumulator.
    let (window_base, start, size) = unsafe { beam_term::bitstring_parts(grown) };
    assert_eq!(window_base, new_base as *const u8, "the pair names the new block");
    assert_eq!(start, 0, "the republish preserved the window start");
    assert_eq!(size, 48, "and the written bits");
    // The pair's `orig` slot still points at its own `BinRef`, whose `val` word now
    // carries the new payload address.
    // SAFETY: `grown` is the live refc-binary pair, so its `orig` slot is the live
    // `BinRef` of that same pair.
    let (orig, val) = unsafe {
        let orig = *boxed_val(grown).add(4);
        let (val, _next) = beam_term::refc::bin_ref_parts(orig);
        (orig, val)
    };
    assert_eq!(
        val as *const u8, new_base,
        "the `BinRef.val` word names the new block"
    );
    assert_eq!(
        boxed_val(orig),
        bin_ref_block,
        "and it is still this pair's own `BinRef` block — the republish names it through \
         the root's `orig` word, never through a fixed offset from the root (a collection \
         copies the two blocks independently)"
    );
    assert_eq!(
        boxed_val(grown),
        pair_block,
        "the republish keeps the `ErlSubBits` root's own address"
    );
    assert_eq!(
        machine.writable_capacity(new_base),
        Some(machine.refc_binaries()[0].capacity()),
        "the registry followed the move"
    );
    assert_eq!(machine.writable_capacity(base), None, "no stale key");

    // The bytes are the ones written, before and after the move.
    assert_eq!(subject_bytes(grown).unwrap(), (&[1u8, 2, 3, 4, 5, 6][..], 0));
    // The move copied them because the payload's logical length is its whole capacity
    // (`beam_alloc`'s relocation copies exactly `len()` bytes), and the live prefix is
    // the accumulator's written bytes.
    let payload = &machine.refc_binaries()[0];
    assert_eq!(
        payload.len(),
        payload.capacity(),
        "the whole backing store is live, so a relocation copies the written bytes"
    );
    assert_eq!(
        &payload.as_bytes()[..6],
        &[1u8, 2, 3, 4, 5, 6],
        "the relocated payload carries the bytes written before the move"
    );
    // The accumulator's on-heap size is the fixed pair, unchanged by the growth: the
    // payload's capacity and reservation never enter the term's size.
    assert_eq!(beam_term::size::size_object(grown), size_before);
    assert_eq!(size_before, beam_term::refc::ERL_REFC_BITS_SIZE);
}

// -----------------------------------------------------------------------
// The split pair: a republish writes two words and nothing else
// -----------------------------------------------------------------------
//
// `republish_over` names the relocated payload in exactly two frozen words — the
// `BinRef`'s `val` word and the `ErlSubBits`'s `base_flags` word. Nothing in the
// representation makes those two blocks one allocation: a collection copies an
// `ErlSubBits` as its own five words and forwards its `orig` word as that block's
// *child* (`beam_gc/src/copy.rs`'s `move_size`), so the 3-word `BinRef` lands as an
// independent allocation the collector may place anywhere in the to-space. These
// cases build that split layout by hand and plant a canary directly behind the
// `BinRef`, so a rewrite that assumed adjacency (a whole-pair write from the
// `BinRef`'s address) is caught as a five-word overrun — a canary assertion, not a
// check on the resulting bytes, which the overrun would leave perfectly correct.

/// The canary block's word count — one word more than the five an old whole-pair
/// rewrite would have overrun into, with margin.
const CANARY_WORDS: usize = 8;

/// The `i`-th canary word's raw value: recognisable, and distinct from every frozen
/// header word and from every pointer these cases allocate.
fn canary_word(i: usize) -> usize {
    0xCAFE_0000 | i
}

/// A **split** refc pair — an `ErlSubBits` whose `orig` word names a `BinRef` block
/// at an unrelated address — with a canary block planted immediately after the
/// 3-word `BinRef`.
///
/// The arena is a bump allocator, so two consecutive reservations are adjacent:
/// reserving the `BinRef` and then the canary puts the canary exactly at
/// `BinRef + ERL_BIN_REF_SIZE`, where a whole-pair rewrite from the `BinRef`'s
/// address would write. The `ErlSubBits` is reserved last, after the canary, so the
/// pair is genuinely split rather than merely reordered.
struct SplitPair {
    /// The boxed `ErlSubBits` — the accumulator term.
    root: Eterm,
    /// Its `orig` word: the boxed `BinRef` term.
    orig: Eterm,
    /// The `ErlSubBits` block's own address.
    root_block: *mut Eterm,
    /// The `BinRef` block's address.
    bin_ref_block: *mut Eterm,
    /// The canary block's address, directly behind the `BinRef`.
    canary: *mut Eterm,
    /// The `BinRef.next` chain word, which a republish must never touch.
    next: *mut Eterm,
}

/// Builds the split pair over `payload`, with the window `start_bits..end_bits` and
/// `flags` in the low `ERL_SUB_BITS_FLAG_MASK` bits of `base_flags`.
fn split_pair(
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    payload: *mut u8,
    start_bits: usize,
    end_bits: usize,
    flags: usize,
) -> SplitPair {
    // A `next` link no allocator issues, so "the republish left it alone" is exact.
    let next = 0xDEAD_BEEFu64 as *mut Eterm;

    let bin_ref_size = beam_term::refc::bin_ref_size_words();
    let bin_ref_block = reserve(machine, ctx, bin_ref_size, RootSet::ALL_LIVE)
        .expect("the arena hands out the BinRef block");
    let canary = reserve(machine, ctx, CANARY_WORDS, RootSet::ALL_LIVE)
        .expect("the arena hands out the canary block");
    let root_block = reserve(
        machine,
        ctx,
        beam_term::refc::sub_bits_size_words(),
        RootSet::ALL_LIVE,
    )
    .expect("the arena hands out the ErlSubBits block");
    assert_eq!(
        bin_ref_block.wrapping_add(bin_ref_size),
        canary,
        "the bump arena places the canary directly behind the BinRef"
    );
    assert_ne!(
        root_block, bin_ref_block,
        "the pair is split: the ErlSubBits does not follow its BinRef"
    );
    for i in 0..CANARY_WORDS {
        // SAFETY: `i < CANARY_WORDS`, inside the block just reserved.
        unsafe { *canary.add(i) = Eterm::from_raw(canary_word(i)) };
    }
    // SAFETY: `bin_ref_block` is `bin_ref_size` exclusively-owned words; `payload` is
    // the caller's live payload, which covers bits `start_bits..end_bits`; `next` is a
    // distinguishable chain link.
    let orig = unsafe {
        beam_term::refc::write_bin_ref(bin_ref_block, payload.cast::<Eterm>(), next)
    };
    // SAFETY: `root_block` is `sub_bits_size_words()` exclusively-owned words;
    // `base_flags` packs the live `payload` with `flags` in its masked low bits; the
    // window is inside that payload; `orig` is the live `BinRef` just written.
    let root = unsafe {
        beam_term::bitstring::write_sub_bits(
            root_block,
            Eterm::from_raw(payload as usize | flags),
            Eterm::from_raw(start_bits),
            Eterm::from_raw(end_bits),
            orig,
        )
    };
    SplitPair {
        root,
        orig,
        root_block,
        bin_ref_block,
        canary,
        next,
    }
}

/// The canary block's words as they stand now.
fn canary_snapshot(canary: *mut Eterm) -> Vec<usize> {
    (0..CANARY_WORDS)
        .map(|i| {
            // SAFETY: `i < CANARY_WORDS`, inside the canary block `split_pair`
            // reserved for the whole case.
            unsafe { *canary.add(i) }.raw()
        })
        .collect()
}

/// **A republish writes exactly two words and never overruns a split pair.**
///
/// The relocated-payload republish (`raw::republish_over`) must name the new payload
/// in the `BinRef`'s `val` word and the `ErlSubBits`'s `base_flags` word, and write
/// nothing else: a whole-pair rewrite from the `BinRef`'s address would write five
/// words past the end of a 3-word block — a heap overrun whenever a collection has
/// separated the two blocks, which is the normal state after any collection between
/// two appends.
///
/// The canary assertion is the point of this case: the resulting bytes are correct
/// under either implementation, so only a word-for-word check on the `BinRef`'s
/// neighbour can see the difference.
#[test]
fn a_republish_writes_only_the_two_payload_words_and_leaves_a_split_pairs_neighbour_intact() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();

    // The payload before the relocation, holding three written bytes.
    let mut old_payload: Vec<Eterm> = vec![Eterm::default(); 2];
    let written = [0xA1u8, 0xB2, 0xC3];
    // SAFETY: `old_payload` is 16 bytes and the copy is 3 bytes, so it is in bounds
    // within the buffer's own allocation.
    unsafe {
        core::ptr::copy_nonoverlapping(
            written.as_ptr(),
            old_payload.as_mut_ptr().cast::<u8>(),
            written.len(),
        );
    }
    let old_base = old_payload.as_mut_ptr().cast::<u8>();

    // A non-zero flag bit, so "the flags survive" is not vacuously true.
    let flags = beam_term::bitstring::ERL_SUB_BITS_FLAG_VOLATILE;
    let pair = split_pair(&mut machine, &mut ctx, old_base, 0, 24, flags);
    let canary_before = canary_snapshot(pair.canary);
    assert_eq!(
        canary_before[0],
        canary_word(0),
        "the canary block is planted"
    );

    // The payload's new home — what a relocation copies the live bytes to, at an
    // address unrelated to either block of the pair.
    let mut new_payload: Vec<Eterm> = vec![Eterm::default(); 2];
    // SAFETY: `new_payload` is the same size as `old_payload`, and `old_base` covers
    // the three written bytes copied here.
    unsafe {
        core::ptr::copy_nonoverlapping(
            old_base,
            new_payload.as_mut_ptr().cast::<u8>(),
            written.len(),
        );
    }
    let new_base = new_payload.as_mut_ptr().cast::<u8>();
    assert_ne!(new_base, old_base, "the payload really moved");

    republish_over(pair.root, new_base);

    // Nothing outside the pair was written: the `BinRef`'s neighbour is byte-identical
    // word for word. (A plain `assert_eq!`, so a release build checks it too.)
    assert_eq!(
        canary_snapshot(pair.canary),
        canary_before,
        "the republish writes no word past the 3-word BinRef — a whole-pair rewrite \
         would overrun five words into this block"
    );

    // The `BinRef`: `val` names the new payload, `next` is untouched.
    // SAFETY: `pair.orig` is the live `BinRef` term `split_pair` wrote, and
    // `pair.root` is the live `ErlSubBits` naming it.
    let (val_after, next_after) = unsafe { beam_term::refc::bin_ref_parts(pair.orig) };
    assert_eq!(
        val_after as *const u8, new_base,
        "`BinRef.val` names the new payload"
    );
    assert_eq!(
        next_after, pair.next,
        "`BinRef.next` is the off-heap chain link and is not rewritten"
    );

    // The `ErlSubBits`: `base_flags` names the new payload with its flag bits kept,
    // and the window plus the back-pointer to the `BinRef` are byte-identical.
    // SAFETY: `pair.root` is the live `ErlSubBits` term `split_pair` built.
    let (base_flags_after, start_after, end_after, orig_after) =
        unsafe { beam_term::bitstring::sub_bits_parts(pair.root) };
    assert_eq!(
        (base_flags_after.raw() & !beam_term::bitstring::ERL_SUB_BITS_FLAG_MASK) as *const u8,
        new_base,
        "`ErlSubBits.base_flags` names the new payload"
    );
    assert_eq!(
        base_flags_after.raw() & beam_term::bitstring::ERL_SUB_BITS_FLAG_MASK,
        flags,
        "the flag bits of `base_flags` survive the republish"
    );
    assert_eq!(
        (start_after.raw(), end_after.raw()),
        (0, 24),
        "a payload relocation changes neither the window start nor its end"
    );
    assert_eq!(
        orig_after, pair.orig,
        "the back-pointer to the pair's own `BinRef` is unchanged"
    );
    assert_eq!(
        boxed_val(pair.orig),
        pair.bin_ref_block,
        "the pair still names its own `BinRef` block — the one the canary follows"
    );
    assert_eq!(
        boxed_val(pair.root),
        pair.root_block,
        "the root keeps its own address, so every alias of the term reads the new payload"
    );

    // The term now reads the relocated payload's bytes, and nothing in the pair's
    // on-heap size changed.
    assert_eq!(
        subject_bytes(pair.root).unwrap(),
        (&written[..], 0),
        "the republished pair reads the relocated bytes"
    );
    assert_eq!(
        beam_term::size::size_object(pair.root),
        beam_term::refc::ERL_REFC_BITS_SIZE,
        "the pair is still the frozen 8-word shape"
    );
}

/// **The same invariant through the real growth route.** A writable accumulator whose
/// pair a collection has split grows its payload past its reservation, so
/// [`InPlace::grow`] takes the `Moved` arm and republishes; the bytes before and after
/// the move concatenate, the `BinRef`'s neighbour is untouched, and — the amortisation
/// property — the *next* append still lands **in place** (`Built::Term`, never the
/// copy-rebuild `Built::Bytes`).
#[test]
fn a_split_pairs_payload_move_keeps_the_next_append_in_place() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();

    // A payload the machine owns and has registered writable, seeded with a three-byte
    // written prefix through the real sink.
    let mut seeded = make_writable(&mut machine, &mut ctx, 8).unwrap();
    let (payload, capacity_bits, _end) =
        writable_append_target(&machine, seeded).expect("writable");
    let mut sink = InPlace::new(payload, capacity_bits, 0, seeded);
    sink.copy_bytes(&[1, 2, 3]);
    seeded = sink.finish();
    // SAFETY: `seeded` is a live refc binary whose window the sink just published.
    let (_, start, size) = unsafe { beam_term::bitstring_parts(seeded) };
    assert_eq!((start, size), (0, 24), "the accumulator holds three bytes");

    // Now hand the accumulator's role to a **split** pair over the same payload: the
    // shape a collection leaves between two appends. (The original contiguous pair
    // `make_writable` built is simply unreferenced from here on.)
    let flags = beam_term::bitstring::ERL_SUB_BITS_FLAG_VOLATILE;
    let pair = split_pair(&mut machine, &mut ctx, payload, 0, 24, flags);
    let canary_before = canary_snapshot(pair.canary);

    // A build that outgrows the 8-byte capacity *and* its 32-byte reservation: the
    // payload is relocated and the pair republished.
    let (base, capacity_bits, end_bits) =
        writable_append_target(&machine, pair.root).expect("writable");
    assert_eq!(end_bits, 24, "the split pair carries the written prefix");
    let mut writer = BitWriter::in_place(InPlace::new(base, capacity_bits, end_bits, pair.root));
    writer
        .push_bits_from(&[4u8; 64], 0, 64 * 8, &mut machine)
        .expect("the build grows the payload and appends");
    let grown = match writer.finish_built() {
        Built::Term(term) => term,
        Built::Bytes(..) => panic!("the payload moved, it did not promote to a copy-rebuild"),
    };
    assert_eq!(grown, pair.root, "the republish keeps the accumulator's term");

    // Nothing outside the pair was written while the payload moved.
    assert_eq!(
        canary_snapshot(pair.canary),
        canary_before,
        "the growth's republish writes no word past the 3-word BinRef"
    );

    // The pair names the payload's new home, with the window, the chain word and the
    // flag bits all unchanged.
    let new_base = machine.refc_binaries()[0].data_ptr();
    assert_ne!(new_base, payload, "the payload was relocated");
    // SAFETY: `pair.orig` is the live `BinRef` of the split pair.
    let (val_after, next_after) = unsafe { beam_term::refc::bin_ref_parts(pair.orig) };
    assert_eq!(val_after as *const u8, new_base, "`BinRef.val` followed the move");
    assert_eq!(next_after, pair.next, "`BinRef.next` is not rewritten");
    // SAFETY: `pair.root` is the live `ErlSubBits` of the split pair.
    let (base_flags_after, start_after, end_after, orig_after) =
        unsafe { beam_term::bitstring::sub_bits_parts(pair.root) };
    assert_eq!(
        (base_flags_after.raw() & !beam_term::bitstring::ERL_SUB_BITS_FLAG_MASK) as *const u8,
        new_base,
        "`ErlSubBits.base_flags` followed the move"
    );
    assert_eq!(
        base_flags_after.raw() & beam_term::bitstring::ERL_SUB_BITS_FLAG_MASK,
        flags,
        "the flag bits survive the growth's republish"
    );
    assert_eq!((start_after.raw(), end_after.raw()), (0, 24 + 64 * 8));
    assert_eq!(orig_after, pair.orig, "the `BinRef` back-pointer is unchanged");
    assert_eq!(
        boxed_val(pair.orig),
        pair.bin_ref_block,
        "the pair still names its own `BinRef` block after two appends"
    );
    assert_eq!(boxed_val(pair.root), pair.root_block, "the root keeps its address");
    assert_eq!(
        machine.writable_capacity(new_base),
        Some(machine.refc_binaries()[0].capacity()),
        "the registry followed the move"
    );

    // The bytes are the concatenation: the prefix written before the move, then the
    // appended tail.
    let mut expected: Vec<u8> = vec![1, 2, 3];
    expected.extend_from_slice(&[4u8; 64]);
    assert_eq!(subject_bytes(grown).unwrap(), (&expected[..], 0));

    // The amortisation property: the next append still lands **in place** over the
    // relocated payload, so the accumulator loop stays O(1) per iteration.
    let (base, capacity_bits, end_bits) =
        writable_append_target(&machine, grown).expect("still writable after the move");
    assert_eq!(base, new_base, "the append target is the relocated payload");
    let mut writer = BitWriter::in_place(InPlace::new(base, capacity_bits, end_bits, grown));
    writer
        .push_bits_from(&[5u8; 8], 0, 8 * 8, &mut machine)
        .expect("the follow-up append succeeds");
    let grown_again = match writer.finish_built() {
        Built::Term(term) => term,
        Built::Bytes(..) => panic!("the follow-up append must stay in place, not rebuild"),
    };
    assert_eq!(grown_again, pair.root, "an in-place append keeps the term");
    assert_eq!(
        canary_snapshot(pair.canary),
        canary_before,
        "the follow-up append writes no word outside the payload either"
    );
    expected.extend_from_slice(&[5u8; 8]);
    assert_eq!(
        subject_bytes(pair.root).unwrap(),
        (&expected[..], 0),
        "the whole window reads back after both appends"
    );
}

/// A second on-heap pair over the **same** payload (a GC copy, a family-built
/// `BinRef`, a match context) makes the relocation refuse — the old block would be
/// freed under it — while the accumulator's **own** pair never blocks its growth.
#[test]
fn a_second_pair_over_the_payload_refuses_the_move() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let acc = make_writable(&mut machine, &mut ctx, 8).unwrap();
    let (base, capacity_bits, _end) = writable_append_target(&machine, acc).expect("writable");

    // A second `[BinRef][ErlSubBits]` pair over the very same payload, parked in a
    // register — the shape a GC copy of the pair leaves behind.
    let hp = reserve(
        &mut machine,
        &mut ctx,
        beam_term::refc::ERL_REFC_BITS_SIZE,
        RootSet::ALL_LIVE,
    )
    .unwrap();
    // SAFETY: `hp` is 8 exclusively-owned words; `base` is the live payload the machine
    // keeps alive; the alias covers a 0-bit window at the payload's start.
    let alias = unsafe {
        beam_term::refc::write_refc_binary(
            hp,
            base.cast::<Eterm>(),
            core::ptr::null_mut(),
            base,
            0,
            0,
        )
    };
    machine.set_x(1, alias).unwrap();

    // `cur == capacity`, so the next bit needs a growth the move would have to serve.
    let mut sink = InPlace::new(base, capacity_bits, capacity_bits, acc);
    assert!(
        !sink.grow(&mut machine, 8).unwrap(),
        "an aliased payload must not be relocated"
    );
    assert_eq!(sink.base, base, "the refusal left the sink on the old block");
    assert_eq!(
        machine.refc_binaries()[0].data_ptr(),
        base,
        "the payload did not move"
    );
    assert_eq!(
        machine.writable_capacity(base),
        Some(8),
        "the registry entry is unchanged"
    );

    // The accumulator's OWN pair anywhere in the root set is not an alias.
    machine.set_x(1, beam_term::NIL).unwrap();
    machine.set_x(0, acc).unwrap();
    let mut sink = InPlace::new(base, capacity_bits, capacity_bits, acc);
    assert!(
        sink.grow(&mut machine, 8).unwrap(),
        "the accumulator's own pair never blocks growth"
    );
    assert_eq!(
        sink.base, base,
        "8 bytes + 8 bits still fits the 32-byte reservation"
    );
}
/// A test context whose heap is **one pre-filled block**: every allocation returns
/// the same `words`-word buffer, every byte of which already holds `0xFF`.
///
/// A build then writes into memory that is *dirty* everywhere — the only way to
/// observe the padding a builder must zero, because a fresh arena is zero-filled
/// by construction and would hide the omission.
struct DirtyHeap {
    block: Vec<Eterm>,
}

impl DirtyHeap {
    fn new(words: usize) -> Self {
        Self {
            block: vec![Eterm::from_raw(usize::MAX); words],
        }
    }
}

impl ExecutionContext for DirtyHeap {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        Err(error::unsupported(&format!(
            "dirty heap: no BIF {module}:{function}/{arity}"
        )))
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }

    fn alloc_words(&mut self, _n: usize) -> Result<*mut Eterm> {
        Ok(self.block.as_mut_ptr())
    }
}

/// `make_bits` initialises the **whole** data region of the `ErlHeapBits` block it
/// builds — including the padding bytes of the last data word, which are not part
/// of the payload.
///
/// That is the invariant the LCA writer (`beam_term::bitstring::write_heap_bits`)
/// establishes by zeroing the data region before the payload copy. A builder that
/// copied only the payload bytes would leave whatever the heap held there: the
/// context's block is pre-filled with `0xFF`, so the omission is observable, and a
/// GC word-copy of the block would propagate the garbage.
#[test]
fn make_bits_zeroes_the_padding_of_the_last_data_word() {
    // A 1-bit and a 5-bit result both occupy one payload byte of a one-word data
    // region, so 7 of the data word's 8 bytes are padding.
    for nbits in [1usize, 5] {
        let num_bytes = nbits.div_ceil(8);
        // The word count and the data-region offset both come from `beam_term`
        // (the frozen-layout owner), so this test asserts the invariant without
        // re-deriving the layout either.
        let num_words = beam_term::bitstring::heap_bits_size_words(nbits);
        assert_eq!(num_words, 3, "the payload fits one data word");
        let mut ctx = DirtyHeap::new(num_words);
        let mut machine = Machine::new();
        // All-ones source: only the trailing-bit mask decides the payload byte, so
        // the payload assertion cannot pass by accident.
        let source = [0xFFu8];
        let term = make_bits(&mut machine, &mut ctx, &source, nbits).unwrap();

        // SAFETY: `term` is the live boxed `ErlHeapBits` `make_bits` just built in
        // `ctx`'s block, whose data region starts at word `ERL_HEAP_BITS_SIZE` and is
        // `num_words - ERL_HEAP_BITS_SIZE` words wide.
        let data = unsafe { boxed_val(term).add(beam_term::ERL_HEAP_BITS_SIZE).cast::<u8>() };
        let data_bytes =
            (num_words - beam_term::ERL_HEAP_BITS_SIZE) * core::mem::size_of::<Eterm>();

        // The payload byte keeps exactly the leading `nbits` bits ...
        // SAFETY: byte 0 of the freshly built `num_bytes`-byte payload.
        let payload = unsafe { *data };
        assert_eq!(
            payload,
            0xFFu8 << (8 - nbits),
            "the unused low bits of the final payload byte are cleared (nbits = {nbits})"
        );
        // ... and every trailing padding byte of the data word is zero.
        for i in num_bytes..data_bytes {
            // SAFETY: `i < data_bytes`, so the byte is inside the freshly built
            // data region.
            let padding = unsafe { *data.add(i) };
            assert_eq!(
                padding, 0,
                "padding byte {i} of the data word must be zero (nbits = {nbits})"
            );
        }
    }
}
