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

//! Integration tests for proper binary/bitstring copy.
//!
//! Covers the required evidence of the crate `CONTEXT.md` `Test Strategy`:
//!
//! 1. an **off-heap refc binary** (the 8-word `[BinRef(3)][ErlSubBits(5)]` pair whose root is the
//!    `ErlSubBits`) is copied by **sharing its payload** — the source `RefcBinary` handle is
//!    refcounted into the destination off-heap table, the copied `BinRef.val` is the *same* payload
//!    pointer, and the copied `ErlSubBits.orig` is fixed up to the copied `BinRef`,
//! 2. dropping the destination table returns the source refcount,
//! 3. an on-heap small binary (`ErlHeapBits`) is copied **by value** (opaque interior),
//! 4. a refc binary whose payload **no held handle owns** is **flattened** into a fresh on-heap
//!    bitstring (sized as that block, so a one-word-short destination is still refused atomically).

use beam_alloc::{Heap, RefcBinary};
use beam_message::off_heap::OffHeapTable;
use beam_message::{copy_struct, copy_struct_simple, size_of_copy, CopyContext};
use beam_term::prelude::*;

/// A 200-byte payload whose bytes are position-dependent, so a partial or shifted copy is visible.
fn payload() -> Vec<u8> {
    (0..200usize).map(|i| ((i * 7) % 251) as u8).collect()
}

/// Writes the on-heap refc-binary pair for `data_ptr`/`len` and returns the tagged root (the
/// `ErlSubBits`).
///
/// # Safety
///
/// `heap` must have 8 spare words and `data_ptr` must be a live, 4-byte-aligned payload of `len`
/// bytes that outlives the returned term.
unsafe fn refc_binary_term(heap: &mut Heap, data_ptr: *mut u8, len: usize) -> Eterm {
    // SAFETY: the caller keeps the payload alive; `data_ptr` is `UNIT_SIZE`-aligned (`data_ptr` is
    // `base + HEADER_SIZE` with `HEADER_SIZE == UNIT_SIZE`), satisfying the flag-bit assert.
    unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(
            hp,
            data_ptr as *mut Eterm,
            core::ptr::null_mut(),
            data_ptr,
            0,
            len * 8,
        )
    }
}

#[test]
fn a_refc_binary_shares_its_payload_with_the_destination() {
    let bytes = payload();
    let source_binary = RefcBinary::new(&bytes).expect("the payload allocates");
    let data_ptr = source_binary.data_ptr();
    assert_eq!(source_binary.refcount(), 1);

    // The source process's on-heap term and its off-heap handle.
    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `source_binary` outlives `original` and `data_ptr` is `UNIT_SIZE`-aligned.
    let original = unsafe { refc_binary_term(&mut src, data_ptr, bytes.len()) };
    let mut src_off = OffHeapTable::new();
    src_off.push_binary(source_binary);
    assert_eq!(src_off.binaries()[0].refcount(), 1, "the source holds one handle");

    assert!(unsafe { is_refc_binary(original) });
    assert_eq!(
        size_of_copy(original),
        ERL_REFC_BITS_SIZE,
        "the preflight sizes the whole 8-word pair"
    );

    let mut dst = Heap::new(64).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(original, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    // Exactly ONE destination binary, refcount 2 (source + destination), same payload pointer.
    assert_eq!(dst_off.binaries().len(), 1);
    let held = &dst_off.binaries()[0];
    assert_eq!(held.data_ptr(), data_ptr, "the payload is shared, not deep-copied");
    assert_eq!(held.as_bytes(), &bytes[..]);
    assert_eq!(held.refcount(), 2, "one reference for the source, one for the destination");
    assert_eq!(src_off.binaries()[0].refcount(), 2);

    // The copied pair is a well-formed refc binary over the same payload.
    assert!(unsafe { is_refc_binary(copy) });
    assert_ne!(copy.raw(), original.raw(), "a real on-heap copy of the pair");
    assert_eq!(dst.used(), ERL_REFC_BITS_SIZE, "the whole 8-word pair is materialised");
    assert_eq!(unsafe { refc_bytes(copy) }, &bytes[..]);
    assert_eq!(unsafe { refc_offset(copy) }, 0);
    assert_eq!(unsafe { refc_size(copy) }, bytes.len() * 8);

    // The copied `BinRef.val` is the *source* payload pointer and `orig` reaches the copied
    // `BinRef` — the fix-up a leaf treatment of the `ErlSubBits` would leave dangling.
    // SAFETY: `copy` is a live refc binary, so its `BinRef` sits `ERL_BIN_REF_SIZE` words below.
    let copied_bin_ref = unsafe { boxed_val(copy).sub(ERL_BIN_REF_SIZE) };
    // SAFETY: `copied_bin_ref` is the copied `BinRef` head, so word 1 is its `val`.
    assert_eq!(unsafe { (*copied_bin_ref.add(1)).raw() }, data_ptr as usize);
    // SAFETY: `copy` is the `ErlSubBits`, so word 4 is its `orig`.
    assert_eq!(
        unsafe { (*boxed_val(copy).add(4)).raw() },
        make_boxed(copied_bin_ref).raw(),
        "orig must reach the *copied* BinRef"
    );

    // Dropping the destination table releases only its own reference.
    drop(dst_off);
    assert_eq!(
        src_off.binaries()[0].refcount(),
        1,
        "the source's handle is left behind"
    );
}

#[test]
fn two_occurrences_of_a_refc_binary_share_one_payload() {
    let bytes = payload();
    let mut src_off = OffHeapTable::new();
    src_off.push_binary(RefcBinary::new(&bytes).expect("the payload allocates"));
    let data_ptr = src_off.binaries()[0].data_ptr();

    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `src_off` owns the payload, which outlives `pair`.
    let term = unsafe { refc_binary_term(&mut src, data_ptr, bytes.len()) };
    let pair = src.tuple(&[term, term]).expect("the tuple fits");

    let mut dst = Heap::new(64).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(pair, &mut dst, &mut ctx).expect("the copy succeeds")
    };

    // The flat copy produces two on-heap pairs over the SAME shared payload: two handles.
    assert_eq!(dst_off.binaries().len(), 2);
    for binary in dst_off.binaries() {
        assert_eq!(binary.data_ptr(), data_ptr);
        assert_eq!(binary.as_bytes(), &bytes[..]);
    }
    assert_eq!(src_off.binaries()[0].refcount(), 3, "source + two destination handles");

    // Both copied elements are well-formed refc binaries, at distinct addresses.
    // SAFETY: `copy` is a live 2-tuple.
    let (first, second) = unsafe { (tuple_element(copy, 0), tuple_element(copy, 1)) };
    assert!(unsafe { is_refc_binary(first) } && unsafe { is_refc_binary(second) });
    assert_ne!(first.raw(), second.raw());
    assert_eq!(unsafe { refc_bytes(first) }, &bytes[..]);
    assert_eq!(unsafe { refc_bytes(second) }, &bytes[..]);
}

#[test]
fn an_on_heap_small_binary_is_copied_by_value() {
    let bytes = [0xDEu8, 0xAD, 0xBE, 0xEF];
    let bits = bytes.len() * 8;
    let words = heap_bits_size_words(bits);

    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `heap_bits_size_words(32)` words hold the `ErlHeapBits`, and the 4-byte payload fits.
    let original = unsafe {
        let hp = src.alloc_words(words).expect("the heap binary fits");
        write_heap_bits(hp, &bytes, bits).expect("the payload fits the reserved words")
    };
    assert!(!unsafe { is_refc_binary(original) }, "a small binary stays on-heap");
    assert_eq!(size_of_copy(original), words);

    let mut dst = Heap::new(64).expect("destination heap");
    let copy = copy_struct_simple(original, &mut dst).expect("the copy succeeds");

    assert_eq!(dst.used(), words, "the heap binary is copied verbatim, interior opaque");
    // SAFETY: both terms are live `ErlHeapBits` of the same shape.
    assert_ne!(boxed_val(original), boxed_val(copy), "a real copy, not a shared pointer");
    let (base, start, size) = unsafe { bitstring_parts(copy) };
    assert_eq!((start, size), (0, bits));
    // SAFETY: the copied payload spans bits `0..bits`, i.e. `ceil(bits/8)` live bytes at `base`.
    let copied_bytes = unsafe { core::slice::from_raw_parts(base, bits.div_ceil(8)) };
    assert_eq!(copied_bytes, &bytes[..]);
}

/// Whether `addr` lies inside `heap`'s live region `[base, hend)`.
fn heap_contains(heap: &Heap, addr: usize) -> bool {
    let base = heap.hp() as usize - heap.used() * core::mem::size_of::<Eterm>();
    addr >= base && addr < heap.hend() as usize
}

#[test]
fn a_match_context_is_flattened_and_leaves_no_dangling_source_reference() {
    // An on-heap bitstring over a known, non-uniform pattern, so a bit shift would be visible.
    let bytes = [0xA5u8, 0x3C, 0xDE, 0x7E, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
    let hb_bits = bytes.len() * 8; // 80

    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `heap_bits_size_words(80)` words hold the `ErlHeapBits`; the payload fits.
    let original = unsafe {
        let hp = src
            .alloc_words(heap_bits_size_words(hb_bits))
            .expect("the heap binary fits");
        write_heap_bits(hp, &bytes, hb_bits).expect("the payload fits")
    };

    // A match context over it: the window starts at a NON-zero, NON-byte-aligned bit offset.
    let start = 3usize;
    let size = 21usize; // deliberately not a multiple of 8
    let end = start + size;
    // SAFETY: the on-heap bitstring's data base is word-aligned (its low flag bits are clear).
    let base = unsafe { bitstring_parts(original) }.0;
    // SAFETY: 5 words are reserved for the `ErlSubBits`; `base` covers bits `start..end`.
    let mc = unsafe {
        let hp = src.alloc_words(5).expect("the match context fits");
        write_sub_bits(
            hp,
            Eterm::from_raw(base as usize),
            Eterm::from_raw(start),
            Eterm::from_raw(end),
            original,
        )
    };
    assert!(
        !unsafe { is_refc_binary(mc) },
        "the match context is not a refc binary"
    );
    // The preflight sizes the *flattened window*, NOT the 5-word `ErlSubBits` block.
    assert_eq!(size_of_copy(mc), heap_bits_size_words(size));

    // Copy the match context out of the source heap.
    let mut dst = Heap::new(64).expect("destination heap");
    let copy = copy_struct_simple(mc, &mut dst).expect("the copy succeeds");

    // The copy is a fresh ON-HEAP bitstring (not an `ErlSubBits`), of the window's bit size.
    // SAFETY: `copy` is a live boxed term.
    let header = unsafe { *boxed_val(copy) };
    assert_eq!(
        header.raw() & TAG_HEADER_MASK,
        HEAP_BITS_SUBTAG,
        "a match context is flattened to an on-heap bitstring"
    );
    assert!(!unsafe { is_refc_binary(copy) });
    let (copied_base, copied_off, copied_size) = unsafe { bitstring_parts(copy) };
    assert_eq!((copied_off, copied_size), (0, size), "the window's bits, base-aligned");

    // Every copied bit equals the SOURCE WINDOW bit `start + i` - the bit shift is honoured.
    for i in 0..size {
        // SAFETY: `original` and `copy` are live bitstrings covering the tested bits.
        assert_eq!(
            unsafe { bit_at(copy, i) },
            unsafe { bit_at(original, start + i) },
            "bit {i} of the copied window"
        );
    }

    // The core property: the copy's data lives INSIDE the destination and NOT inside the source,
    // so it stays valid once the source process/heap is gone.
    assert!(
        heap_contains(&dst, copied_base as usize),
        "the copy's data is in the destination"
    );
    assert!(
        !heap_contains(&src, copied_base as usize),
        "no aliasing into the source heap"
    );
    assert!(
        !heap_contains(&src, boxed_val(copy) as usize),
        "the copied term is not in the source heap"
    );
    assert_eq!(dst.used(), heap_bits_size_words(size));
}

/// A refc binary whose payload **no held handle owns** has no payload to share, so the copier
/// **flattens** the window into a fresh on-heap bitstring — the same routine a `SUB_BITS` match
/// context takes. The copy carries the source payload's bytes with its data inside the
/// destination heap, and registers nothing in the off-heap table (there is no handle to refcount).
#[test]
fn a_refc_binary_with_no_owning_handle_is_flattened() {
    let bytes = payload();
    let bits = bytes.len() * 8;
    let source_binary = RefcBinary::new(&bytes).expect("the payload allocates");
    let data_ptr = source_binary.data_ptr();

    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `source_binary` is alive (it is dropped at the end of the test), so the pair is live.
    let original = unsafe { refc_binary_term(&mut src, data_ptr, bytes.len()) };
    assert!(unsafe { is_refc_binary(original) });
    assert_eq!(
        size_of_copy(original),
        ERL_REFC_BITS_SIZE,
        "the canonical flat count still sizes the whole 8-word pair"
    );

    // The source off-heap table does NOT hold the handle: there is no pair to share.
    let src_off = OffHeapTable::new();
    let mut dst = Heap::new(64).expect("destination heap");
    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(original, &mut dst, &mut ctx)
            .expect("the copy flattens the payload instead of failing")
    };

    // The copy is a fresh ON-HEAP bitstring (not a refc binary), of the window's bit size.
    assert!(
        !unsafe { is_refc_binary(copy) },
        "a flattened refc binary is no longer a refc binary"
    );
    // SAFETY: `copy` is a live boxed term.
    let header = unsafe { *boxed_val(copy) };
    assert_eq!(
        header.raw() & TAG_HEADER_MASK,
        HEAP_BITS_SUBTAG,
        "the flattened window is an on-heap bitstring"
    );
    let (copied_base, copied_off, copied_size) = unsafe { bitstring_parts(copy) };
    assert_eq!((copied_off, copied_size), (0, bits), "the whole window, base-aligned");
    // SAFETY: the copied payload spans bits `0..bits`, i.e. `ceil(bits / 8)` live bytes at `base`.
    let copied_bytes = unsafe { core::slice::from_raw_parts(copied_base, bits.div_ceil(8)) };
    assert_eq!(
        copied_bytes,
        &bytes[..],
        "the flattened copy carries the source payload's bytes"
    );
    assert_eq!(
        dst.used(),
        heap_bits_size_words(bits),
        "the copy costs the flattened block, not the 8-word pair"
    );

    // The core property: the copy's data lives INSIDE the destination and NOT in the source heap or
    // in the source payload, so it stays valid once the source process is gone.
    assert!(
        heap_contains(&dst, copied_base as usize),
        "the copy's data is in the destination"
    );
    assert!(
        !heap_contains(&src, copied_base as usize),
        "no aliasing into the source heap"
    );
    assert_ne!(
        copied_base as usize, data_ptr as usize,
        "the copy does not alias the unresolvable source payload"
    );

    // Nothing is registered: there is no handle to refcount, and the source's own is untouched.
    assert!(dst_off.binaries().is_empty(), "the flattened copy registers no refc binary");
    assert!(dst_off.is_empty() && dst_off.nodes().is_empty(), "and no MSO/node artifact");
    assert_eq!(source_binary.refcount(), 1, "the source holder is untouched");
}

/// The flatten fallback is **sized** as the flattened block, not as the 8-word pair: a refc binary
/// with no held handle is copied at `heap_bits_size_words(size)`, so a destination with exactly that
/// much head-room accepts the copy and one word less refuses it **before writing a word** — the
/// no-partial-write contract the context-aware sizing exists to preserve.
///
/// Both destinations below are DIRTY (one/two words pre-charged) with an EMPTY off-heap table, so
/// both take the single-pass fast path; the refusal case is then re-derived by the preflight path,
/// whose `refc_binary_words` supplies the context-aware flattened size.
#[test]
fn a_flattened_refc_binary_is_preflighted_at_the_flattened_size() {
    let bytes = payload();
    let bits = bytes.len() * 8;
    let total = heap_bits_size_words(bits);
    assert!(total > ERL_REFC_BITS_SIZE, "the flattened block is the larger of the two");

    let source_binary = RefcBinary::new(&bytes).expect("the payload allocates");
    let data_ptr = source_binary.data_ptr();
    let mut src = Heap::new(64).expect("source heap");
    // SAFETY: `source_binary` outlives `original` and `data_ptr` is `UNIT_SIZE`-aligned.
    let original = unsafe { refc_binary_term(&mut src, data_ptr, bytes.len()) };
    let src_off = OffHeapTable::new();

    // A DIRTY destination (one word already handed out, empty off-heap table, so the single-pass
    // fast path): exactly `total` words of head-room left must admit the flattened window.
    let mut dst = Heap::new(total + 1).expect("destination heap");
    // SAFETY: the heap owns the word.
    unsafe { dst.alloc_words(1).expect("the pre-charge fits") };
    let mut dst_off = OffHeapTable::new();
    let copy = {
        let mut ctx = CopyContext::new(&src_off, &mut dst_off);
        copy_struct(original, &mut dst, &mut ctx)
            .expect("the ctx-aware sizing admits the flattened window")
    };
    assert_eq!(dst.used(), 1 + total);
    // SAFETY: `copy` is a live on-heap bitstring covering the whole window.
    assert_eq!(unsafe { bitstring_parts(copy) }.2, bits);

    // One word short: the whole copy is refused as a message-heap exhaustion, writing nothing — and
    // the two words the destination already held are left exactly where they were (the abandoned
    // single-pass attempt never advanced the heap, so nothing is rewound or discarded).
    let mut short = Heap::new(total + 1).expect("destination heap");
    // SAFETY: the heap owns both words.
    unsafe { short.alloc_words(2).expect("the pre-charge fits") };
    let mut short_off = OffHeapTable::new();
    let error = {
        let mut ctx = CopyContext::new(&src_off, &mut short_off);
        copy_struct(original, &mut short, &mut ctx).expect_err("one word short must be refused")
    };
    assert!(
        beam_message::is_message_heap_exhaustion(&error),
        "a refused flatten is a message-heap exhaustion, got {error:?}"
    );
    assert_eq!(short.used(), 2, "the refusal writes no word past the pre-charge");
    assert!(short_off.binaries().is_empty(), "and registers no off-heap artifact");
}
