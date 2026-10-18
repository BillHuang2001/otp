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

//! The **growth** half of the writable-accumulator optimisation: what happens when an
//! in-place `private_append` outgrows the `bs_init_writable` payload.
//!
//! C grows the writable binary in place (`GROW_PROC_BIN_SIZE`, `erl_bits.c:1965-1976`) and
//! only falls back to the copy-rebuild form when the payload is pinned. The engine's
//! counterpart is [`raw::InPlace::grow`] -> [`Machine::grow_writable_payload`] ->
//! `beam_alloc::RefcBinary::ensure_capacity`, with the copy-rebuild route as the promoted
//! fallback. These cases pin the four properties that make the optimisation worth having —
//! and the one it must never trade for correctness:
//!
//! 1. an append loop is **amortised, not quadratic**: the number of payload moves is flat
//!    (logarithmic in the total) and the bytes a move copies stay within a small multiple
//!    of the data, asserted structurally from `RefcBinary::growth_counters` — never from a
//!    wall clock;
//! 2. a **move mid-accumulation** loses nothing: the prefix written before the move is
//!    still readable through the published term and the registry re-keys to the new block,
//!    so the next append stays in place;
//! 3. a **shared** payload refuses growth and the build falls back to the copy-rebuild
//!    route, which is byte-identical and leaves the accumulator's window unbumped — a
//!    refusal costs the optimisation only;
//! 4. the accumulator is **indifferent to the run boundary**: the on-heap cost of the term
//!    is the frozen 8-word `[BinRef][ErlSubBits]` pair whatever the payload grew to, and
//!    the writable registry plus the payload roster survive [`Machine::reset`].
//!
//! Every case drives the accumulator the way `bs_create_bin` does — re-deriving the append
//! target from the live term each iteration ([`append_chunk`]) rather than holding a sink
//! across builds, because a growth may relocate the payload and republish the pair.
//!
//! Ground truth (read-only): `erts/emulator/beam/erl_bits.c:1943-2020` (`GROW_PROC_BIN_SIZE`
//! and the non-writable fallback arm) and `beam_alloc/src/binary/growth.rs` (the
//! in-place/move protocol and the `grow_span` schedule).

use super::*;

use crate::context::MinimalContext;

// ---------------------------------------------------------------------------
// Helpers: the accumulator loop, the payload roster and byte comparison
// ---------------------------------------------------------------------------

/// One `<<Acc/bitstring, Chunk/binary>>` step, driven exactly as `bs_create_bin`'s
/// `private_append` segment drives it: re-derive the in-place target from the live
/// accumulator, push the bits, and publish.
///
/// `chunk` is an ordinary buffer (never a slice of the payload), so the writer's alias
/// check cannot promote the build for that reason — the only promotion route left is a
/// refused growth.
fn append_chunk(machine: &mut Machine, acc: Eterm, chunk: &[u8], iteration: usize) -> Eterm {
    let (base, capacity_bits, end_bits) = raw::writable_append_target(machine, acc)
        .unwrap_or_else(|| panic!("append {iteration}: the accumulator is a writable target"));
    let mut writer = BitWriter::in_place(raw::InPlace::new(base, capacity_bits, end_bits, acc));
    writer
        .push_bits_from(chunk, 0, chunk.len() * 8, machine)
        .unwrap_or_else(|err| panic!("append {iteration}: the append reports no raise: {err:?}"));
    match writer.finish_built() {
        Built::Term(term) => term,
        Built::Bytes(..) => panic!(
            "append {iteration}: the build promoted to the copy-rebuild form instead of \
             growing the payload in place"
        ),
    }
}

/// The `(payload base, window start, window end)` of a live bitstring — `raw::heap_bits`
/// reports `(base, bit size, bit offset)`, i.e. `(base, end, start)` here.
///
/// For a refc binary whose window starts at 0 the base is the payload's `data_ptr()`,
/// which is the value [`Machine::refc_binaries`]' handles are matched by.
fn window(term: Eterm) -> (*const u8, usize, usize) {
    let (base, end, start) = raw::heap_bits(term).expect("a readable bitstring");
    (base, start, end)
}

/// The accumulated bits of a term, zero-padded to whole bytes, with the exact bit count.
fn bytes_of(term: Eterm) -> (Vec<u8>, usize) {
    built_bytes(Built::Term(term))
}

/// The roster index of the handle that owns the payload the accumulator's window names.
fn payload_index(machine: &Machine, acc: Eterm) -> usize {
    let (base, ..) = window(acc);
    machine
        .refc_binaries()
        .iter()
        .position(|binary| binary.data_ptr() as *const u8 == base)
        .expect("the accumulator's payload is on the machine's off-heap roster")
}

/// Assert two byte vectors are equal, reporting the first divergent offset instead of
/// dumping a 64 KiB literal into the failure message.
fn assert_same_bytes(actual: &[u8], expected: &[u8], what: &str) {
    assert_eq!(actual.len(), expected.len(), "{what}: byte count");
    if let Some(at) = actual.iter().zip(expected).position(|(a, e)| a != e) {
        panic!(
            "{what}: byte {at} is {:#04x}, expected {:#04x}",
            actual[at], expected[at]
        );
    }
}

// ---------------------------------------------------------------------------
// 1. A monotone append is amortised, not quadratic
// ---------------------------------------------------------------------------

/// A 512-byte-chunk append loop up to 64 KiB on a **1 KiB** `bs_init_writable` hint stays in
/// place on **every** iteration, and the growth curve is flat: a handful of moves and an
/// amortised copy cost.
///
/// * Structural, not wall-clock: the figures come from `RefcBinary::growth_counters()` of
///   the accumulator's own handle (`in_place_grows` / `moves` / `bytes_copied`), so the
///   assertion is about the *shape* of the curve and reads identically on any host.
/// * The move count is asserted against a **small constant**, not a count proportional to
///   the appended data: 64 KiB in 512-byte chunks is 128 iterations, so a quadratic
///   rebuild would move the payload ~128 times, while the doubling schedule needs only
///   `log2(65536 / 4096) = 4` moves once the 4 KiB reservation the 1 KiB hint buys is
///   exhausted.
/// * `bytes_copied` stays inside `2 *` the appended bytes: a move copies the payload's
///   whole live length (its reservation), which the doubling schedule keeps proportional to
///   the data, never to the iteration count.
#[test]
fn a_64_kib_monotone_append_is_amortised_not_quadratic() {
    let hint = 1024usize;
    let chunk = vec![0xA5u8; 512];
    let total_target = 64 * 1024;
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let acc = raw::make_writable(&mut machine, &mut ctx, hint).expect("a writable accumulator");
    assert_eq!(
        machine.refc_binaries().len(),
        1,
        "the accumulator is the machine's only off-heap payload"
    );

    let mut reference: Vec<u8> = Vec::new();
    let mut iterations = 0usize;
    while reference.len() < total_target {
        let term = append_chunk(&mut machine, acc, &chunk, iterations);
        assert_eq!(
            term, acc,
            "append {iterations}: the build publishes the accumulator term itself"
        );
        reference.extend_from_slice(&chunk);
        iterations += 1;
    }
    assert_eq!(
        reference.len(),
        iterations * chunk.len(),
        "every iteration appended exactly one chunk"
    );

    // The published term reads the whole logical bitstring, byte for byte.
    let (bytes, nbits) = bytes_of(acc);
    assert_eq!(nbits, reference.len() * 8, "the exact bit count");
    assert_same_bytes(&bytes, &reference, "the 64 KiB accumulator");
    let (base, start, end) = window(acc);
    assert_eq!(
        (start, end),
        (0, reference.len() * 8),
        "the window spans the whole payload prefix"
    );

    // The growth curve, read from the accumulator's own handle.
    let index = payload_index(&machine, acc);
    assert_eq!(
        machine.refc_binaries()[index].data_ptr() as *const u8,
        base,
        "the roster's handle names the payload the window points at"
    );
    let counters = machine.refc_binaries()[index].growth_counters();
    let capacity = machine.refc_binaries()[index].capacity();
    // Observed (hint = 1024 B, chunk = 512 B, total = 64 KiB): in_place_grows = 2,
    // moves = 4, bytes_copied = 61440 B (4096 + 8192 + 16384 + 32768), final capacity
    // 65536 B, 128 iterations. The assertions below are deliberately the *loose* bounds
    // that would catch a quadratic implementation, not a pin on those exact figures.
    assert!(
        capacity >= reference.len(),
        "the final capacity {capacity} covers all {} appended bytes",
        reference.len()
    );
    assert!(
        counters.moves <= 12,
        "moves must be flat in the appended data, not proportional to it: got {} moves \
         and {} in-place grows for {} bytes in {} iterations (bytes_copied = {})",
        counters.moves,
        counters.in_place_grows,
        reference.len(),
        iterations,
        counters.bytes_copied
    );
    assert!(
        counters.in_place_grows >= 1,
        "the 1 KiB hint's 4x reservation must be reused at least once before the first \
         move: got {} in-place grows",
        counters.in_place_grows
    );
    assert!(
        counters.bytes_copied <= 2 * reference.len(),
        "a move copies the payload's whole reservation, so the amortised cost stays \
         proportional to the data: copied {} bytes for {} appended bytes",
        counters.bytes_copied,
        reference.len()
    );
    assert_eq!(
        machine.refc_binaries().len(),
        1,
        "growth never allocates a second payload handle — it widens or relocates the one"
    );
}

// ---------------------------------------------------------------------------
// 2. A move mid-accumulation preserves every accumulated byte
// ---------------------------------------------------------------------------

/// Appending past the reservation **relocates** the payload; the accumulator's published
/// term, its window and its every written byte survive, and the registry re-keys so the
/// next append is in place again.
///
/// The shape: an 8-byte capacity on a 32-byte reservation, then a 32-byte append (an
/// in-place grow that commits the reservation) and an 8-byte append (which exceeds both, so
/// `beam_alloc` relocates). The third append then fits the relocated payload outright.
#[test]
fn a_move_mid_accumulation_preserves_every_accumulated_byte() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let acc = raw::make_writable(&mut machine, &mut ctx, 8).expect("a writable accumulator");
    let before: Vec<u8> = (0..32u8).collect();
    let moving: Vec<u8> = (0xA0..0xA8u8).collect();
    let after_move: Vec<u8> = (0xC0..0xC8u8).collect();

    // (a) 32 bytes into an 8-byte capacity: the reservation absorbs it in place.
    let first = append_chunk(&mut machine, acc, &before, 0);
    assert_eq!(first, acc, "the first append publishes the accumulator");
    let (base_before, start, end) = window(first);
    assert_eq!(
        (start, end),
        (0, before.len() * 8),
        "the first append's window"
    );
    let index = payload_index(&machine, first);
    assert_eq!(
        machine.refc_binaries()[index].capacity(),
        before.len(),
        "the capacity was committed to the reservation, not past it"
    );
    let counters = machine.refc_binaries()[index].growth_counters();
    assert_eq!(counters.moves, 0, "the first append did not have to move");
    let live_before_move = machine.refc_binaries()[index].len();

    // (b) 8 more bytes exceed both the capacity and the reservation: a move.
    let second = append_chunk(&mut machine, acc, &moving, 1);
    assert_eq!(
        second, acc,
        "the move republishes the same accumulator term"
    );
    let (base_after, start_after, end_after) = window(second);
    assert_eq!(
        (start_after, end_after),
        (0, (before.len() + moving.len()) * 8),
        "the republished window carries every appended bit"
    );
    assert_ne!(base_after, base_before, "the payload was relocated");
    assert_eq!(
        machine.refc_binaries()[index].data_ptr() as *const u8,
        base_after,
        "the roster's handle names the new block"
    );
    let counters = machine.refc_binaries()[index].growth_counters();
    assert_eq!(counters.moves, 1, "exactly one move");
    assert_eq!(
        counters.bytes_copied, live_before_move,
        "a move copies the payload's whole live length (its reservation), which is why the \
         written prefix survives it"
    );
    assert!(
        raw::is_refc_binary(second),
        "the accumulator is still an off-heap refc binary after the move"
    );

    // Every byte written before the move is still readable through the published term.
    let mut reference = before.clone();
    reference.extend_from_slice(&moving);
    let (bytes, nbits) = bytes_of(second);
    assert_eq!(nbits, reference.len() * 8, "the full logical bitstring");
    assert_same_bytes(&bytes, &reference, "the accumulated bytes across the move");

    // (c) The registry re-keyed: the next append targets the NEW base, with room to spare,
    //     so it neither promotes nor grows again.
    let (next_base, capacity_bits, end_bits) =
        raw::writable_append_target(&machine, second).expect("the accumulator is still writable");
    assert_eq!(
        next_base as *const u8, base_after,
        "the writable registry followed the move"
    );
    assert_eq!(
        end_bits,
        reference.len() * 8,
        "the window end the sink starts from"
    );
    assert!(
        capacity_bits >= end_bits + after_move.len() * 8,
        "the relocated capacity ({capacity_bits} bits) still has room for the next append"
    );
    let capacity_before_last = machine.refc_binaries()[index].capacity();
    let moves_before_last = counters.moves;
    let third = append_chunk(&mut machine, second, &after_move, 2);
    assert_eq!(third, acc, "the next append takes the in-place route");
    assert_eq!(
        window(third),
        (base_after, 0, (reference.len() + after_move.len()) * 8),
        "the payload neither moved nor promoted"
    );
    assert_eq!(
        machine.refc_binaries()[index].capacity(),
        capacity_before_last,
        "no growth was needed at all"
    );
    assert_eq!(
        machine.refc_binaries()[index].growth_counters().moves,
        moves_before_last,
        "and no second move happened"
    );
    reference.extend_from_slice(&after_move);
    let (bytes, nbits) = bytes_of(third);
    assert_eq!(nbits, reference.len() * 8);
    assert_same_bytes(&bytes, &reference, "the bytes after the move");
}

// ---------------------------------------------------------------------------
// 3. A shared payload refuses growth; the copy-rebuild route is byte-identical
// ---------------------------------------------------------------------------

/// A payload a second handle shares cannot be relocated (`RefcBinary::ensure_capacity`
/// requires a unique handle — C's `erts_pin_writable_binary` condition), so the writer
/// promotes to the copy-rebuild form. The result is **byte-identical** and the
/// accumulator's window is left unbumped: a refused growth never publishes a partial write,
/// so it costs the optimisation only.
#[test]
fn a_shared_payload_refuses_growth_and_copy_rebuilds_identically() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let acc = raw::make_writable(&mut machine, &mut ctx, 8).expect("a writable accumulator");
    let head: Vec<u8> = vec![0x11, 0x22, 0x33, 0x44];
    let tail = vec![0x5Au8; 16];

    // Seed 4 bytes in place: they fit the 8-byte capacity and are published.
    let seeded = append_chunk(&mut machine, acc, &head, 0);
    assert_eq!(seeded, acc, "the seed is in place");
    let (base, start, end) = window(seeded);
    assert_eq!((start, end), (0, head.len() * 8), "the seeded window");
    let index = payload_index(&machine, seeded);
    let capacity = machine.refc_binaries()[index].capacity();
    let live = machine.refc_binaries()[index].len();

    // A host adopts the payload: the clone makes `refcount() == 2`, so growth is refused.
    let adopted = machine.refc_binaries()[index].clone();
    assert_eq!(
        machine.refc_binaries()[index].refcount(),
        2,
        "the adopted handle shares the payload"
    );

    // 4 + 16 bytes exceeds the 8-byte capacity, so the writer must grow — and cannot.
    let built = {
        let (base, capacity_bits, end_bits) =
            raw::writable_append_target(&machine, seeded).expect("still a writable target");
        let mut writer =
            BitWriter::in_place(raw::InPlace::new(base, capacity_bits, end_bits, seeded));
        writer
            .push_bits_from(&tail, 0, tail.len() * 8, &mut machine)
            .expect("a refused growth is not an error");
        writer.finish_built()
    };
    let mut reference = head.clone();
    reference.extend_from_slice(&tail);
    match built {
        Built::Bytes(bytes, nbits) => {
            assert_eq!(nbits, reference.len() * 8, "the whole logical bitstring");
            assert_same_bytes(&bytes, &reference, "the copy-rebuild result");
        }
        Built::Term(_) => panic!(
            "a shared payload cannot be grown, so the build must promote to the \
             copy-rebuild form"
        ),
    }
    // The bytes are the real contract, whichever route produced them: materialising the
    // promoted build reads exactly the accumulated bits.
    let rebuilt = built_term(
        Built::Bytes(reference.clone(), reference.len() * 8),
        &mut machine,
        &mut ctx,
    )
    .expect("the copy-rebuild result materialises");
    let (bytes, nbits) = bytes_of(rebuilt);
    assert_eq!(nbits, reference.len() * 8);
    assert_same_bytes(&bytes, &reference, "the materialised copy-rebuild result");

    // Nothing was written into (or reserved on) the accumulator's payload: the refused
    // growth left its window, its capacity and its live length exactly as they were.
    assert_eq!(
        window(seeded),
        (base, 0, head.len() * 8),
        "the accumulator's window was left unbumped"
    );
    assert_eq!(
        machine.refc_binaries()[index].capacity(),
        capacity,
        "the payload did not grow"
    );
    assert_eq!(
        machine.refc_binaries()[index].len(),
        live,
        "nor did its live length"
    );
    assert_eq!(
        machine.refc_binaries()[index].data_ptr() as *const u8,
        base,
        "nor did it move"
    );
    let counters = machine.refc_binaries()[index].growth_counters();
    assert_eq!(
        (counters.moves, counters.in_place_grows),
        (0, 0),
        "the seed fit the 8-byte capacity outright, and the refused growth recorded nothing"
    );
    // Reading the accumulator's own 4 bytes still yields exactly those 4 bytes.
    assert_eq!(
        bytes_of(seeded),
        (head.clone(), head.len() * 8),
        "the accumulator itself is unchanged"
    );

    // Dropping the adopted handle makes the payload growable again — the refusal was the
    // refcount, not a lost registry entry.
    drop(adopted);
    assert_eq!(machine.refc_binaries()[index].refcount(), 1);
    let grown = append_chunk(&mut machine, seeded, &tail, 1);
    assert_eq!(
        grown, acc,
        "with the payload unique again the append is in place"
    );
    let (bytes, nbits) = bytes_of(grown);
    assert_eq!(nbits, reference.len() * 8);
    assert_same_bytes(&bytes, &reference, "the in-place append after the refusal");
}

// ---------------------------------------------------------------------------
// 4. The accumulator is indifferent to the run boundary
// ---------------------------------------------------------------------------

/// The payload is **off-heap**, so no sizing walk sees its capacity: the accumulator's
/// on-heap cost is the frozen 8-word `[BinRef][ErlSubBits]` pair before and after the
/// payload grew manyfold. And the accumulator outlives a run boundary: [`Machine::reset`]
/// clears the register/stack state, never the payload roster or the writable registry, so
/// the same term still reads its full window and still appends in place.
///
/// The engine has no collector (`MinimalContext` only ever grows its reserved heap), so
/// these are the reachable invariants; a moving collection's counterpart is
/// `beam_gc`'s.
#[test]
fn an_accumulator_survives_a_run_boundary_and_reports_its_logical_size() {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let acc = raw::make_writable(&mut machine, &mut ctx, 16).expect("a writable accumulator");
    // (a) The on-heap size is the frozen pair, whatever the payload's capacity.
    let words_before = beam_term::size::size_object(acc);
    assert_eq!(
        words_before,
        beam_term::refc::ERL_REFC_BITS_SIZE,
        "a refc binary is the 8-word [BinRef][ErlSubBits] pair"
    );

    let chunk = vec![0x33u8; 1024];
    let mut reference: Vec<u8> = Vec::new();
    for iteration in 0..17 {
        let term = append_chunk(&mut machine, acc, &chunk, iteration);
        assert_eq!(term, acc, "append {iteration} is in place");
        reference.extend_from_slice(&chunk);
    }
    assert_eq!(reference.len(), 17 * 1024, "17 KiB accumulated");
    let index = payload_index(&machine, acc);
    let capacity = machine.refc_binaries()[index].capacity();
    assert!(
        capacity >= reference.len(),
        "the payload grew to at least the appended bytes ({capacity} >= {})",
        reference.len()
    );
    assert_eq!(
        beam_term::size::size_object(acc),
        words_before,
        "the term's on-heap size is still the 8-word pair after the payload grew {} \
         times over ({capacity} bytes of capacity)",
        capacity / 16
    );

    // (b) The run boundary: `reset` clears registers and stacks only.
    machine.reset();
    let (_, start, end) = window(acc);
    assert_eq!(
        (start, end),
        (0, reference.len() * 8),
        "the accumulator still reads its full window after `reset`"
    );
    assert_eq!(
        payload_index(&machine, acc),
        index,
        "the payload roster still names the accumulator's payload after `reset`"
    );
    let after_reset = append_chunk(&mut machine, acc, &chunk, 17);
    assert_eq!(
        after_reset, acc,
        "the writable registry survived `reset`: the append after the run boundary is in \
         place"
    );
    reference.extend_from_slice(&chunk);
    let (bytes, nbits) = bytes_of(acc);
    assert_eq!(
        nbits,
        reference.len() * 8,
        "the bit count across the boundary"
    );
    assert_same_bytes(&bytes, &reference, "the bytes across the run boundary");
    let (base_after, start_after, end_after) = window(acc);
    assert_eq!(
        (start_after, end_after),
        (0, reference.len() * 8),
        "the window end followed the append past the boundary"
    );
    assert_eq!(
        base_after,
        machine.refc_binaries()[index].data_ptr() as *const u8,
        "the window still names the payload the roster holds"
    );
}

// ---------------------------------------------------------------------------
// 5. A zero-capacity accumulator still grows to a real result
// ---------------------------------------------------------------------------

/// `bs_init_writable` with an `x(0) == 0` hint really allocates `bin_size = 0`
/// (`erl_bits.c:2023`), so the first append has to create the payload. It must make
/// **monotone** progress — never a zero-progress loop, never a promotion — and produce the
/// exact bytes.
#[test]
fn a_zero_capacity_accumulator_grows_to_a_real_result() {
    let module = Module::default();
    let labels = LabelMap::default();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine
        .set_x(0, beam_term::make_small(0))
        .expect("x(0) is the capacity hint");
    let step = exec(
        &mut machine,
        &module,
        &labels,
        &Instr::BsInitWritable,
        0,
        0,
        &mut ctx,
    )
    .expect("bs_init_writable reports no raise")
    .expect("the family claims bs_init_writable");
    assert_eq!(step, Step::Next);
    let acc = machine.x(0).expect("the accumulator replaced the hint");
    let index = payload_index(&machine, acc);
    assert_eq!(
        machine.refc_binaries()[index].capacity(),
        0,
        "the zero hint really is a zero-capacity payload"
    );
    assert_eq!(window(acc).1, 0, "a 0-bit window starts at 0");
    assert_eq!(window(acc).2, 0, "a 0-bit window ends at 0");
    assert_eq!(
        window(acc).0,
        machine.refc_binaries()[index].data_ptr() as *const u8,
        "and it names the zero-capacity payload"
    );
    assert_eq!(
        machine.refc_binaries()[index].len(),
        0,
        "whose backing store is empty"
    );

    let chunk = vec![0x7Eu8; 1024];
    let mut reference: Vec<u8> = Vec::new();
    for iteration in 0..6 {
        let term = append_chunk(&mut machine, acc, &chunk, iteration);
        assert_eq!(term, acc, "append {iteration} is in place");
        reference.extend_from_slice(&chunk);
        assert_eq!(
            window(acc).2,
            reference.len() * 8,
            "append {iteration} made monotone progress"
        );
        let capacity = machine.refc_binaries()[index].capacity();
        assert!(
            capacity >= reference.len(),
            "append {iteration}: the capacity ({capacity}) keeps up with the {} appended \
             bytes",
            reference.len()
        );
    }
    assert!(
        reference.len() > 4 * 1024,
        "the accumulated result is well past a few KiB"
    );
    let (bytes, nbits) = bytes_of(acc);
    assert_eq!(nbits, reference.len() * 8, "the exact bit count");
    assert_same_bytes(&bytes, &reference, "the zero-capacity accumulator's bytes");
    let counters = machine.refc_binaries()[index].growth_counters();
    assert!(
        counters.moves + counters.in_place_grows >= 1,
        "growing from a zero capacity must have grown the payload at least once"
    );
}
