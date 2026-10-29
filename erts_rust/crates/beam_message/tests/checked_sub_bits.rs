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

//! Hard evidence for the copier's **shape-checked** `SUB_BITS` handling: the verdict on a
//! mis-shaped `ErlSubBits` block is total and typed, and a well-shaped sub-binary copies with the
//! window semantics `beam_term`'s sub-binary builder produces.
//!
//! **A. The corruption sweep — the shape verdict is total and typed.**
//! 1. Each of the **seven** one-word corruptions of a genuine `[BinRef(3)][ErlSubBits(5)]` pair (no
//!    storage, an inverted window, an immediate origin, a compound origin of the wrong kind, a
//!    `BinRef` with no payload, an `ErlHeapBits` whose arity contradicts its size word) is refused
//!    with [`BeamError::BadArg`] on **both** routes — the empty-table single-pass fast path and the
//!    sized preflight-then-write path — writing nothing: the destination heap and all three
//!    off-heap lists stay exactly as the caller left them.
//! 2. The unmutated block still copies and shares its payload, so the refusals are the mutation's
//!    verdict rather than a broken fixture.
//! 3. A **seeded** 256-iteration sweep over well-shaped-looking blocks (a live `BinRef` origin, a
//!    base from `{none, payload, payload | volatile, 1}`, a window from the whole payload) accepts
//!    **exactly** the blocks whose masked base is a real pointer and whose window is ordered;
//!    everything else is the typed `BadArg` with nothing written, and every accepted copy's bit
//!    image equals the source window's bits (no accepted window reads outside the payload, so the
//!    sweep is UB-free by construction).
//!
//! **B. Window semantics of a copied sub-binary** — the shapes
//! `beam_term::refc::write_sub_binary` produces (the arm is chosen on the WINDOW's bit size):
//! 4. a byte-aligned refc window copies the 8-word pair and **shares** the parent payload;
//! 5. a non-byte-aligned refc window keeps its absolute bit offset and the shared payload;
//! 6. a refc window of a refc window composes the offsets, keeping the payload shared;
//! 7. a small window of an **on-heap** parent is the heap arm (a fresh `ErlHeapBits` in the
//!    destination, nothing registered);
//! 8. a window of a refc window under `ERL_ONHEAP_BITS_LIMIT` is the heap arm too, so the child no
//!    longer references the 1025-byte payload at all;
//! 9. a window of a **literal-tagged** parent is the heap arm, and copying `{literal, window}`
//!    shares the literal subterm by identity while the window's bits survive the copy.
//!
//! Every suite is a **self-check**: the oracle figures in the comments were captured with the
//! system Erlang (OTP 29.1.2 / erts-17.1.1) via `binary:part/3` and
//! `binary:referenced_byte_size/1`; no test invokes `erl`/`erlc`.

use beam_alloc::{Heap, RefcBinary};
use beam_common::BeamError;
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, CopyContext};
use beam_term::prelude::*;

/// The verdict of one `copy_struct` call — `Ok` with the copied root, or the crate's typed error.
type Verdict = beam_common::Result<Eterm>;

/// A 200-byte payload whose bytes are position-dependent, so a partial or shifted copy is visible.
fn payload() -> Vec<u8> {
    (0..200usize).map(|i| ((i * 7) % 251) as u8).collect()
}

/// Which destination a copy runs into — the two routes `copy_struct` takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// An empty off-heap table, so the copy takes the **single-pass fast path** (no preflight walk).
    EmptyTable,
    /// One pre-registered `RefcBinary` handle, so the copy takes the **sized preflight-then-write**
    /// path — and a refusal must leave that handle exactly where it is.
    PreloadedTable,
}

/// A source heap holding the 8-word `[BinRef(3)][ErlSubBits(5)]` pair over a genuine, handle-owned
/// payload of `bytes`, plus the off-heap table owning it.
fn refc_source_of(bytes: &[u8]) -> (Heap, OffHeapTable, Eterm) {
    let mut off = OffHeapTable::new();
    off.push_binary(RefcBinary::new(bytes).expect("the payload allocates"));
    let data_ptr = off.binaries()[0].data_ptr();
    let mut heap = Heap::new(64).expect("source heap");
    // SAFETY: `off` owns a live, word-aligned payload of `bytes.len()` bytes for as long as the
    // term is used (the table outlives it in every caller), and `ERL_REFC_BITS_SIZE` words are
    // reserved for the pair.
    let root = unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(hp, data_ptr as *mut Eterm, core::ptr::null_mut(), data_ptr, 0, bytes.len() * 8)
    };
    (heap, off, root)
}

/// The standard fixture: a genuine refc binary over [`payload`].
fn refc_source() -> (Heap, OffHeapTable, Eterm) {
    refc_source_of(&payload())
}

/// Overwrites one data word of the source `ErlSubBits` block (`root` must be that block's root).
///
/// # Safety
///
/// `root` must be a live `ErlSubBits` root and `index` one of its four data words
/// ([`SUB_BITS_BASE_WORD`] .. `= SUB_BITS_ORIG_WORD`); nothing else may read the block concurrently.
unsafe fn write_sb_word(root: Eterm, index: usize, value: Eterm) {
    // SAFETY: the caller guarantees `root` is a live 5-word `ErlSubBits` block.
    unsafe { *boxed_val(root).add(index) = value };
}

/// Applies mis-shaped-mutation `case` (1..=7) to a freshly built pair rooted at `root`; case 8 is
/// the control. Each arm writes one shape word, so the refusal is attributable to that word.
fn mutate(case: usize, heap: &mut Heap, root: Eterm) {
    match case {
        // 1. `base_flags = 0` — "no storage": the null base the C treats as no payload.
        1 => unsafe { write_sb_word(root, SUB_BITS_BASE_WORD, Eterm::from_raw(0)) },
        // 2. `start > end` — the window would underflow.
        2 => unsafe {
            write_sb_word(root, SUB_BITS_START_WORD, Eterm::from_raw(8));
            write_sb_word(root, SUB_BITS_END_WORD, Eterm::from_raw(4));
        },
        // 3. `orig = NIL`, an immediate (a non-boxed origin).
        3 => unsafe { write_sb_word(root, SUB_BITS_ORIG_WORD, NIL) },
        // 4. `orig = make_small(1)`, also an immediate.
        4 => unsafe { write_sb_word(root, SUB_BITS_ORIG_WORD, make_small(1)) },
        // 5. `orig` = a boxed 2-tuple living in the source heap: a real compound of the wrong kind.
        5 => {
            let tuple = heap.tuple(&[make_small(1), make_small(2)]).expect("the tuple fits");
            unsafe { write_sb_word(root, SUB_BITS_ORIG_WORD, tuple) };
        }
        // 6. `orig` = a boxed `BinRef` whose `val` is 0: the right header, no payload behind it.
        6 => {
            // SAFETY: `ERL_BIN_REF_SIZE` (3) words are reserved and fully written.
            let bin_ref = unsafe {
                let hp = heap.alloc_words(ERL_BIN_REF_SIZE).expect("the bin ref fits");
                write_bin_ref(hp, core::ptr::null_mut(), core::ptr::null_mut())
            };
            unsafe { write_sb_word(root, SUB_BITS_ORIG_WORD, bin_ref) };
        }
        // 7. `orig` = a boxed `ErlHeapBits` whose header arity contradicts its bit-size word.
        7 => {
            let bits = 16;
            // SAFETY: `heap_bits_size_words(16)` (3) words are reserved and the 2-byte payload fits.
            let heap_bits = unsafe {
                let hp = heap.alloc_words(heap_bits_size_words(bits)).expect("the heap bits fit");
                write_heap_bits(hp, &[0xA5, 0x5A], bits).expect("the payload fits")
            };
            // The block claims 4096 bits — `heap_bits_size_words(4096)` (66) words — while its
            // header only covers 3, so the arity check must refuse it.
            // SAFETY: word 1 of a live `ErlHeapBits` block is its bit-size word.
            unsafe { *boxed_val(heap_bits).add(1) = Eterm::from_raw(4096) };
            unsafe { write_sb_word(root, SUB_BITS_ORIG_WORD, heap_bits) };
        }
        8 => {}
        _ => panic!("mutations 1..=7 plus the control 8"),
    }
}

/// Runs one `copy_struct` of `root` out of `src_off` on `route`, returning the destination heap and
/// table so the caller can assert on what was (and was not) written, plus the verdict.
fn copy_on(route: Route, root: Eterm, src_off: &OffHeapTable) -> (Heap, OffHeapTable, Verdict) {
    let mut dst = Heap::new(256).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    if route == Route::PreloadedTable {
        // A handle this copy did not register: its presence keeps the table non-empty (so the
        // sized path is taken) and a refusal must leave it exactly where it is.
        dst_off.push_binary(RefcBinary::new(b"preloaded").expect("the preload allocates"));
    }
    let result = {
        let mut ctx = CopyContext::new(src_off, &mut dst_off);
        copy_struct(root, &mut dst, &mut ctx)
    };
    (dst, dst_off, result)
}

/// Asserts that `result` is the typed `BadArg` refusal and that it left the destination untouched.
fn assert_refused(context: &str, route: Route, dst: &Heap, dst_off: &OffHeapTable, result: Verdict) {
    match result {
        Ok(_) => panic!("{context}: a mis-shaped block must be refused, not copied"),
        Err(err) => assert!(
            matches!(err, BeamError::BadArg),
            "{context}: the refusal must be the typed BadArg, got {err:?}"
        ),
    }
    assert_eq!(dst.used(), 0, "{context}: the refusal writes no destination word");
    assert!(dst_off.is_empty(), "{context}: the refusal registers no MSO handle");
    assert!(dst_off.nodes().is_empty(), "{context}: the refusal registers no node reference");
    let expected = usize::from(route == Route::PreloadedTable);
    assert_eq!(dst_off.binaries().len(), expected, "{context}: the binary list is unchanged");
}

/// The seven one-word corruptions are all refusals, on **both** routes, and each writes nothing.
#[test]
fn every_mis_shaped_sub_bits_block_is_refused_on_both_routes() {
    for case in 1..=7 {
        for route in [Route::EmptyTable, Route::PreloadedTable] {
            // A freshly rebuilt source block per case, so no mutation inherits another's state.
            let (mut src, src_off, root) = refc_source();
            mutate(case, &mut src, root);
            let (dst, dst_off, result) = copy_on(route, root, &src_off);
            assert_refused(&format!("mutation {case} on {route:?}"), route, &dst, &dst_off, result);
        }
    }
}

/// The control: an unmutated block copies and shares the payload exactly as `tests/binary.rs`
/// pins — which is what makes the seven refusals above the mutation's verdict.
#[test]
fn the_unmutated_block_copies_and_shares_its_payload() {
    let bytes = payload();
    for route in [Route::EmptyTable, Route::PreloadedTable] {
        let (_src, src_off, root) = refc_source_of(&bytes);
        let data_ptr = src_off.binaries()[0].data_ptr();
        let (dst, dst_off, result) = copy_on(route, root, &src_off);
        let copy = result.expect("the genuine pair copies");
        assert_eq!(unsafe { refc_offset(copy) }, 0, "{route:?}: a whole-binary window starts at bit 0");
        assert_eq!(unsafe { refc_size(copy) }, bytes.len() * 8, "{route:?}");
        assert_eq!(dst.used(), ERL_REFC_BITS_SIZE, "{route:?}: the whole 8-word pair");
        // One new handle, plus the preload the sized route started with.
        let preloaded = usize::from(route == Route::PreloadedTable);
        assert_eq!(dst_off.binaries().len(), preloaded + 1, "{route:?}: one new handle");
        let held = dst_off.binaries().last().expect("the destination holds the registered handle");
        assert_eq!(held.data_ptr(), data_ptr, "{route:?}: the payload is shared");
        assert_eq!(unsafe { refc_bytes(copy) }, &bytes[..], "{route:?}");
    }
}

/// A seeded sweep over **well-shaped-looking** blocks: the header is fixed, the origin is the live
/// `BinRef`, and only the base word and the window vary. The verdict is exactly `BadArg` iff the
/// masked base is absent or the window is inverted; every other block copies, and the copy's bit
/// image equals the source window's bits.
///
/// The window is drawn from `0..=payload bits`, so an accepted block's reads stay inside the
/// payload (UB-free by construction); the `base = 1` arm masks to 0, so it can never be read.
#[test]
fn a_seeded_sweep_accepts_exactly_the_readable_windows() {
    let bytes = payload();
    let total_bits = bytes.len() * 8;
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = move || {
        // A four-line xorshift64 — a deterministic, dependency-free stream.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for iteration in 0..256 {
        // `_src` stays bound so the block `root` points into outlives every read below.
        let (_src, src_off, root) = refc_source_of(&bytes);
        let data_ptr = src_off.binaries()[0].data_ptr() as usize;
        // The four base-flags shapes: no storage, the payload, the payload with the volatile flag,
        // and a masked-to-zero non-pointer.
        let bases = [0usize, data_ptr, data_ptr | ERL_SUB_BITS_FLAG_VOLATILE, 1usize];
        let base_flags = bases[(next() % bases.len() as u64) as usize];
        let start = (next() as usize) % (total_bits + 1);
        let end = (next() as usize) % (total_bits + 1);
        // SAFETY: `root` is a live `ErlSubBits` root and the three rewritten words are its base and
        // its window; nothing else reads the block concurrently.
        unsafe {
            write_sb_word(root, SUB_BITS_BASE_WORD, Eterm::from_raw(base_flags));
            write_sb_word(root, SUB_BITS_START_WORD, Eterm::from_raw(start));
            write_sb_word(root, SUB_BITS_END_WORD, Eterm::from_raw(end));
        }

        let context = format!("iteration {iteration}: base_flags={base_flags:#x} start={start} end={end}");
        let (dst, dst_off, result) = copy_on(Route::EmptyTable, root, &src_off);
        if base_flags & !ERL_SUB_BITS_FLAG_MASK == 0 || start > end {
            assert_refused(&context, Route::EmptyTable, &dst, &dst_off, result);
            continue;
        }
        let copy = result.expect("a readable window copies");
        assert_eq!(unsafe { refc_offset(copy) }, start, "{context}: absolute bit offset");
        assert_eq!(unsafe { refc_size(copy) }, end - start, "{context}: window size");
        assert_eq!(dst.used(), ERL_REFC_BITS_SIZE, "{context}: the shared 8-word pair");
        for i in 0..(end - start) {
            // SAFETY: both blocks are live, well-shaped refc windows covering `i` bits.
            let (copied_bit, source_bit) = unsafe { (try_bit_at(copy, i), try_bit_at(root, i)) };
            assert_eq!(copied_bit, source_bit, "{context}: bit {i} of the copied window");
        }
    }
}

/// Builds a window of `parent` at `bit_offset`/`bit_size`, reserving exactly what
/// [`sub_binary_words`] reports (as `beam_term/tests/refc_window.rs` does).
fn window(heap: &mut Heap, parent: Eterm, bit_offset: usize, bit_size: usize) -> Eterm {
    // SAFETY: `parent` is a live bitstring (every caller builds it) and the reservation is exactly
    // the count the writer reports for this window.
    let words = unsafe { sub_binary_words(parent, bit_size) }.expect("a windowable parent");
    // SAFETY: the words are exclusively owned and written by the writer below.
    let hp = unsafe { heap.alloc_words(words) }.expect("the window fits");
    // SAFETY: `hp` owns `words` words, the count just reported for this window.
    unsafe { write_sub_binary(hp, parent, bit_offset, bit_size) }.expect("the window is in range")
}

/// Whether `addr` lies inside `heap`'s live region `[base, hend)`.
fn heap_contains(heap: &Heap, addr: usize) -> bool {
    let base = heap.hp() as usize - heap.used() * core::mem::size_of::<Eterm>();
    addr >= base && addr < heap.hend() as usize
}

/// The window's own bit image, packed MSB-first into `size.div_ceil(8)` bytes — what
/// `binary:part/3` would answer, assembled through the checked per-bit reader.
fn window_image(term: Eterm, size: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; size.div_ceil(8)];
    for i in 0..size {
        // SAFETY: the caller passes a live bitstring covering `size` bits.
        if unsafe { try_bit_at(term, i) }.expect("an in-window bit") {
            bytes[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    bytes
}

/// Case 1 — a **byte-aligned** refc window shares the parent payload verbatim.
///
/// Oracle probe (OTP 29.1.2 / erts-17.1.1): `Parent = binary:copy(<<16#A5>>, 1025)`,
/// `W = binary:part(Parent, 0, 600)` gives `bit_size(W) == 4800`, `byte_size(W) == 600`,
/// `binary:referenced_byte_size(W) == 1025`, and the image is 600 bytes of `16#A5`.
#[test]
fn a_byte_aligned_refc_window_shares_the_parent_payload() {
    let bytes = vec![0xA5u8; 1025];
    let (mut src, src_off, parent) = refc_source_of(&bytes);
    let data_ptr = src_off.binaries()[0].data_ptr();
    let w1 = window(&mut src, parent, 0, 4800);
    // 4800 bits is above `ERL_ONHEAP_BITS_LIMIT` (512), so the builder takes the refc arm.
    let words = unsafe { sub_binary_words(parent, 4800) }.expect("a refc parent windows");
    assert_eq!(words, ERL_REFC_BITS_SIZE, "above the limit the refc arm builds the 8-word pair");
    assert!(unsafe { is_refc_binary(w1) }, "the window is a refc binary");

    let (dst, dst_off, result) = copy_on(Route::EmptyTable, w1, &src_off);
    let copy = result.expect("the window copies");
    assert_eq!(unsafe { refc_offset(copy) }, 0, "the window starts at bit 0");
    assert_eq!(unsafe { refc_size(copy) }, 4800, "4800 bits, as the oracle reports");
    assert_eq!(unsafe { bitstring_parts(copy) }.2, 4800, "the frozen bitstring view agrees");
    assert_eq!(dst.used(), ERL_REFC_BITS_SIZE, "the copy is the whole 8-word pair");

    // The `binary:part/3` image: 600 bytes of 0xA5, carried by the SHARED payload.
    let view = unsafe { refc_bytes(copy) };
    assert_eq!(view.len(), 600, "byte_size(W) == 600");
    assert!(view.iter().all(|byte| *byte == 0xA5), "the image is 600 bytes of 0xA5");
    assert_eq!(view.as_ptr() as usize, data_ptr as usize, "no byte of the payload is copied");

    // `binary:referenced_byte_size(W) == 1025`: one handle over the parent's own payload.
    assert_eq!(dst_off.binaries().len(), 1, "exactly one registered handle");
    let held = &dst_off.binaries()[0];
    assert_eq!(held.data_ptr(), data_ptr, "the same payload pointer");
    assert_eq!(held.as_bytes().len(), 1025, "referenced_byte_size == 1025");
    assert!(dst_off.is_empty() && dst_off.nodes().is_empty(), "nothing else is registered");
}

/// Case 2 — a **non-byte-aligned** refc window keeps its absolute bit offset and the shared payload.
///
/// Oracle probe: `W = binary:part(Parent, 1, 600)` (bit offset 11) of the same 1025-byte `16#A5`
/// parent gives `bit_size(W) == 4800`, `byte_size(W) == 600`,
/// `binary:referenced_byte_size(W) == 1025`, and its 600-byte image is `16#2D` throughout (the
/// parent's `1010_0101` pattern shifted by the window's 3-bit residue).
///
/// Two *views* are pinned: `refc_bytes` is the **payload's** byte window (from the byte containing
/// bit 11 — the unshifted `16#A5` run, `ceil(4811/8) - 1 == 601` bytes), while the oracle's figure
/// is the window's **own** re-aligned image.
#[test]
fn a_non_byte_aligned_refc_window_keeps_its_bit_offset() {
    let bytes = vec![0xA5u8; 1025];
    let (mut src, src_off, parent) = refc_source_of(&bytes);
    let data_ptr = src_off.binaries()[0].data_ptr();
    let w2 = window(&mut src, parent, 11, 4800);
    assert!(unsafe { is_refc_binary(w2) }, "above the limit the window is a refc binary");
    assert_eq!(unsafe { refc_offset(w2) }, 11, "the builder records the absolute bit offset");

    let (dst, dst_off, result) = copy_on(Route::EmptyTable, w2, &src_off);
    let copy = result.expect("the window copies");
    assert_eq!(unsafe { refc_offset(copy) }, 11, "the offset survives the copy");
    assert_eq!(unsafe { refc_size(copy) }, 4800);
    assert_eq!(dst.used(), ERL_REFC_BITS_SIZE);

    let view = unsafe { refc_bytes(copy) };
    assert_eq!(view.len(), 601, "ceil((11 + 4800) / 8) - (11 / 8)");
    assert!(view.iter().all(|byte| *byte == 0xA5), "the payload bytes, unshifted");
    assert_eq!(view.as_ptr() as usize, data_ptr as usize + 1, "the view is inside the payload");

    let image = window_image(copy, 4800);
    assert!(image.iter().all(|byte| *byte == 0x2D), "image all 0x2D, as the oracle reports");
    assert_eq!(dst_off.binaries().len(), 1, "still one handle over the parent payload");
    assert_eq!(dst_off.binaries()[0].as_bytes().len(), 1025, "referenced_byte_size == 1025");
}

/// Case 3 — a refc window **of a refc window** composes the offsets and keeps the payload shared.
///
/// Oracle probe: `W2 = binary:part(binary:part(Parent, 0, 600), 5, 595)` of the same parent gives
/// `bit_size(W2) == 4760`, `byte_size(W2) == 595`, `binary:referenced_byte_size(W2) == 1025`
/// (composing windows never narrows the referenced payload), image all `16#A5`.
#[test]
fn a_refc_window_of_a_refc_window_composes_its_offset() {
    let bytes = vec![0xA5u8; 1025];
    let (mut src, src_off, parent) = refc_source_of(&bytes);
    let data_ptr = src_off.binaries()[0].data_ptr();
    let w1 = window(&mut src, parent, 0, 4800);
    let composed = window(&mut src, w1, 40, 4760);
    let words = unsafe { sub_binary_words(w1, 4760) }.expect("a refc window windows");
    assert_eq!(words, ERL_REFC_BITS_SIZE);
    assert_eq!(unsafe { refc_offset(composed) }, 40, "the child's absolute offset is composed");
    assert_eq!(unsafe { refc_size(composed) }, 4760);

    let (dst, dst_off, result) = copy_on(Route::EmptyTable, composed, &src_off);
    let copy = result.expect("the composed window copies");
    assert_eq!(unsafe { refc_offset(copy) }, 40);
    assert_eq!(unsafe { refc_size(copy) }, 4760);
    assert_eq!(dst.used(), ERL_REFC_BITS_SIZE);
    // Bit 40 is byte aligned, so the view is the window's own 595 bytes of 0xA5.
    let view = unsafe { refc_bytes(copy) };
    assert_eq!(view.len(), 595, "byte_size(W2) == 595");
    assert!(view.iter().all(|byte| *byte == 0xA5), "image all 0xA5");
    assert_eq!(view.as_ptr() as usize, data_ptr as usize + 5, "still the parent's payload");
    assert_eq!(dst_off.binaries().len(), 1);
    assert_eq!(dst_off.binaries()[0].as_bytes().len(), 1025, "referenced_byte_size == 1025");
}

/// Case 4 — a small window of an **on-heap** parent is the heap arm: a fresh `ErlHeapBits` copy
/// whose bits are the parent's window bits, living inside the destination.
///
/// Oracle counterpart (`binary:part/3` of a small on-heap binary): a byte-aligned 3-byte window
/// answers `bit_size 24`, `byte_size 3`, `binary:referenced_byte_size 3`. This window is 21 bits at
/// bit offset 3, so the test pins the 21 bits and the copy's residency (what the heap arm is for).
#[test]
fn a_small_window_of_an_on_heap_parent_copies_the_window_bits() {
    let bytes = [0xA5u8, 0x3C, 0xDE, 0x7E, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    let parent_bits = bytes.len() * 8;
    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `heap_bits_size_words(80)` words hold the block and the 10-byte payload fits.
    let parent = unsafe {
        let hp = src.alloc_words(heap_bits_size_words(parent_bits)).expect("the heap binary fits");
        write_heap_bits(hp, &bytes, parent_bits).expect("the payload fits")
    };
    let small = window(&mut src, parent, 3, 21);
    let words = unsafe { sub_binary_words(parent, 21) }.expect("a heap parent windows");
    assert_eq!(words, heap_bits_size_words(21), "under the limit the builder copies the bits");
    assert_ne!(words, ERL_REFC_BITS_SIZE, "and it is not the 8-word pair");

    let src_off = OffHeapTable::new();
    let (dst, dst_off, result) = copy_on(Route::EmptyTable, small, &src_off);
    let copy = result.expect("the window copies");
    // SAFETY: `copy` is a live boxed term, so its header word is readable.
    let copied_header = header_subtag(unsafe { *boxed_val(copy) }) as usize;
    assert_eq!(copied_header, HEAP_BITS_SUBTAG, "an on-heap bitstring, not an ErlSubBits");
    let (copied_base, copied_off, copied_size) = unsafe { bitstring_parts(copy) };
    assert_eq!(copied_off, 0, "the window bits are re-aligned to bit 0 of the copy");
    assert_eq!(copied_size, 21, "21 bits, base aligned");
    for i in 0..21 {
        // SAFETY: both terms are live bitstrings covering 21 bits.
        assert_eq!(unsafe { bit_at(copy, i) }, unsafe { bit_at(small, i) }, "bit {i} of the copy");
    }
    assert!(heap_contains(&dst, copied_base as usize), "the copy's data is in the destination");
    assert!(!heap_contains(&src, copied_base as usize), "no aliasing into the source heap");
    assert!(dst_off.binaries().is_empty(), "no payload to share, so nothing is registered");
    assert!(dst_off.is_empty() && dst_off.nodes().is_empty());
    assert_eq!(dst.used(), heap_bits_size_words(21));
}

/// Case 5 — a window of a refc window that falls **under** the limit is the heap arm, so the child
/// no longer references the 1025-byte payload at all.
///
/// Oracle probe: `W = binary:part(binary:part(Parent, 0, 600), 5, 4)` gives `bit_size(W) == 32`,
/// `byte_size(W) == 4`, `binary:referenced_byte_size(W) == 4`, image 4 bytes of `16#A5`.
#[test]
fn a_window_under_the_limit_drops_the_payload_reference() {
    let bytes = vec![0xA5u8; 1025];
    let (mut src, src_off, parent) = refc_source_of(&bytes);
    let w1 = window(&mut src, parent, 0, 4800);
    let child = window(&mut src, w1, 40, 32);
    let words = unsafe { sub_binary_words(w1, 32) }.expect("a refc window windows");
    assert_eq!(words, heap_bits_size_words(32), "32 bits is under the limit: a fresh block");
    // SAFETY: `child` is a live boxed term, so its header word is readable.
    let child_header = header_subtag(unsafe { *boxed_val(child) }) as usize;
    assert_eq!(child_header, HEAP_BITS_SUBTAG, "the child is an on-heap bitstring");

    let (dst, dst_off, result) = copy_on(Route::EmptyTable, child, &src_off);
    let copy = result.expect("the child copies");
    let (copied_base, copied_off, copied_size) = unsafe { bitstring_parts(copy) };
    assert_eq!((copied_off, copied_size), (0, 32), "32 bits, base aligned");
    // SAFETY: the copy's 32-bit window occupies exactly 4 payload bytes at `copied_base`.
    let image = unsafe { core::slice::from_raw_parts(copied_base, 4) };
    assert_eq!(image, &[0xA5u8; 4], "image all 0xA5, as the oracle reports");
    assert!(dst_off.binaries().is_empty(), "the child owns no payload to share");
    assert!(dst_off.is_empty() && dst_off.nodes().is_empty());
    assert_eq!(dst.used(), heap_bits_size_words(32));
}

/// Case 6 — a window of a **literal-tagged** parent is the heap arm, and copying
/// `{literal_parent, window}` shares the literal subterm by identity while the window's 16 bits
/// survive the copy.
#[test]
fn a_literal_parent_is_shared_while_its_window_is_copied() {
    let bytes = [0xA5u8, 0x3C, 0xDE, 0x7E, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    let parent_bits = bytes.len() * 8;
    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `heap_bits_size_words(80)` words hold the block and the 10-byte payload fits.
    let parent = unsafe {
        let hp = src.alloc_words(heap_bits_size_words(parent_bits)).expect("the heap binary fits");
        write_heap_bits(hp, &bytes, parent_bits).expect("the payload fits")
    };
    // A literal-area compound: the copier shares it by identity instead of descending into it.
    let literal_parent = make_literal(parent);
    assert!(is_literal(literal_parent), "the parent carries the literal-area bit");
    let small = window(&mut src, literal_parent, 2, 16);
    let words = unsafe { sub_binary_words(literal_parent, 16) }.expect("a literal parent windows");
    assert_eq!(words, heap_bits_size_words(16), "16 bits is under the limit: a fresh block");
    let pair = src.tuple(&[literal_parent, small]).expect("the 2-tuple fits");

    let src_off = OffHeapTable::new();
    let (dst, dst_off, result) = copy_on(Route::EmptyTable, pair, &src_off);
    let copy = result.expect("the tuple copies");
    // SAFETY: `copy` is a live 2-tuple over two materialised (or shared) elements.
    let (shared, copied_window) = unsafe { (tuple_element(copy, 0), tuple_element(copy, 1)) };
    assert_eq!(shared.raw(), literal_parent.raw(), "the literal subterm is shared by identity");
    let (_base, _off, size) = unsafe { bitstring_parts(copied_window) };
    assert_eq!(size, 16, "the window's 16 bits survive the copy");
    for i in 0..16 {
        // SAFETY: both windows are live bitstrings covering 16 bits.
        assert_eq!(unsafe { bit_at(copied_window, i) }, unsafe { bit_at(small, i) }, "bit {i}");
    }
    assert!(dst_off.is_empty() && dst_off.binaries().is_empty() && dst_off.nodes().is_empty());
    assert!(heap_contains(&dst, boxed_val(copied_window) as usize), "the window is in `dst`");
}
