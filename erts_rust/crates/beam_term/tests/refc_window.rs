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

//! Integration tests for the **sub-binary window** surface —
//! [`write_sub_binary`] / [`sub_binary_words`] / [`refc_payload`], the crate's port of
//! `erts_build_sub_bitstring` (`erl_bits.c:2637-2665`) and the payload accessor
//! `binary:referenced_byte_size/1` needs (`erl_bif_binary.c:2691-2717`).
//!
//! These exercise the public surface only (the same reason `tests/internal_ref.rs` is a
//! separate file rather than `src/refc.rs`'s in-module module: the ~1000-line guideline).
//! They pin the three properties a producer depends on:
//!
//! * the **arm** is chosen on the *window's* bit size — `ERL_ONHEAP_BITS_LIMIT` is the
//!   heap arm and one bit more the refc arm — and each arm's bits are the parent's bits
//!   at `bit_offset + i`, byte aligned or not;
//! * the refc arm keeps `BinRef.val` **equal to the parent payload pointer** (so the
//!   payload, and therefore the referenced size, stays reachable) with `start` =
//!   the parent's bit offset + `bit_offset` and `end` = `start + bit_size`;
//! * `sub_binary_words` is exactly what the writer uses, so a producer can reserve first
//!   and write once.
//!
//! ## What this crate deliberately cannot answer
//!
//! `binary:referenced_byte_size/1` is the payload's **allocated** size
//! (`(br->val)->orig_size`), which lives in the off-heap payload header that
//! `beam_alloc` owns. The frozen on-heap pair has no slot for it (`BinRef` is the
//! C-exact 3 words `[thing_word, val, next]` with a `HEADER_BIN_REF` arity of 2;
//! `ErlSubBits`' `base_flags` spends its only 2 spare bits on
//! `ERL_SUB_BITS_FLAG_MUTABLE`/`_VOLATILE`), so the size is answered by whatever owns
//! the payload, keyed on [`refc_payload`]'s `BinRef.val` word. The last test states that
//! contract rather than asserting a figure this crate cannot know.
use beam_term::bitstring::{
    bit_at, bitstring_parts, heap_bits_size_words, sub_bits_parts, write_sub_bits,
    ERL_SUB_BITS_FLAG_MUTABLE,
};
use beam_term::error::TermError;
use beam_term::heap::Heap;
use beam_term::immediate::{make_small, NIL};
use beam_term::refc::{
    bin_ref_parts, is_refc_binary, refc_offset, refc_payload, refc_size, sub_binary_words,
    write_refc_binary, write_sub_binary, ERL_ONHEAP_BITS_LIMIT, ERL_REFC_BITS_SIZE,
};
use beam_term::{arityval, boxed_val, header_subtag, Eterm, HEAP_BITS_SUBTAG};

/// A word-aligned payload buffer — `write_refc_binary` requires the low
/// `ERL_SUB_BITS_FLAG_MASK` bits of the data base to be clear.
#[repr(align(8))]
struct Payload<const N: usize>([u8; N]);

/// Builds a refc binary over `payload` at bits `offset..offset + size`.
fn refc_parent(heap: &mut Heap, payload: &[u8], offset: usize, size: usize) -> Eterm {
    // SAFETY: `ERL_REFC_BITS_SIZE` words are reserved, `payload` outlives every use of the
    // term here, and it is word aligned (the `Payload` attribute).
    unsafe {
        let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).expect("the pair fits");
        write_refc_binary(
            hp,
            payload.as_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.as_ptr(),
            offset,
            size,
        )
    }
}

/// The `n` bits of a bitstring term, as a `Vec<bool>` for readable assertions.
fn bits(t: Eterm, n: usize) -> Vec<bool> {
    // SAFETY: `t` is a live bitstring of at least `n` bits at every call site.
    (0..n).map(|i| unsafe { bit_at(t, i) }).collect()
}

/// Builds a window of `parent` at `bit_offset`/`bit_size` with the reported reservation.
fn window(heap: &mut Heap, parent: Eterm, off: usize, size: usize) -> Eterm {
    // SAFETY: `parent` is a live bitstring; the reservation is exactly the reported one.
    let words = unsafe { sub_binary_words(parent, size) }.expect("a windowable parent");
    let hp = unsafe { heap.alloc_words(words) }.expect("the window fits");
    // SAFETY: `hp` owns `words` words, the count just reported for this window.
    unsafe { write_sub_binary(hp, parent, off, size) }.expect("the window is in range")
}

/// One 64-byte payload with bit patterns that differ at every offset (so a
/// off-by-one-byte or an un-shifted copy cannot pass by luck).
fn payload_64() -> Payload<64> {
    let mut bytes = [0u8; 64];
    for (i, b) in bytes.iter_mut().enumerate() {
        // A pseudo-random but deterministic pattern: no byte repeats its neighbour.
        *b = (i as u8).wrapping_mul(37).wrapping_add(0x5B);
    }
    Payload(bytes)
}

/// A 128-byte payload for the windows that must exceed `ERL_ONHEAP_BITS_LIMIT`.
fn payload_128() -> Payload<128> {
    let mut bytes = [0u8; 128];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(53).wrapping_add(0xA7);
    }
    Payload(bytes)
}

#[test]
fn every_heap_arm_window_is_the_parent_bits_at_the_offset() {
    let payload = payload_64();
    let mut heap = Heap::new(4096);
    let parent = refc_parent(&mut heap, &payload.0, 0, 64 * 8);
    let parent_bits = bits(parent, 64 * 8);

    // Byte-aligned and non-byte-aligned offsets; sizes from 0 through the on-heap limit.
    let cases = [
        (0usize, 0usize),
        (0, 1),
        (0, 7),
        (0, 8),
        (0, 512),
        (1, 511),
        (3, 21),
        (7, 505),
        (13, 64),
        (37, 129),
        (255, 257),
        (509, 3),
    ];
    for (off, size) in cases {
        let child = window(&mut heap, parent, off, size);
        assert_eq!(
            unsafe { bitstring_parts(child) }.2,
            size,
            "{off}+{size}: bit size"
        );
        assert_eq!(
            bits(child, size),
            parent_bits[off..off + size].to_vec(),
            "{off}+{size}: window bits"
        );
        assert!(
            !unsafe { is_refc_binary(child) },
            "{off}+{size}: at or below the limit is the heap arm"
        );
        // The heap arm is an `ErlHeapBits` whose header prices exactly its words...
        let words = heap_bits_size_words(size);
        let hdr = unsafe { *boxed_val(child) };
        assert_eq!(header_subtag(hdr), HEAP_BITS_SUBTAG as u32, "{off}+{size}: subtag");
        assert_eq!(arityval(hdr), words - 1, "{off}+{size}: header arity");
        // ... and the bits past the window (in the last byte, and in the rest of the
        // block) are zero.
        let (base, dst_off, _) = unsafe { bitstring_parts(child) };
        assert_eq!(dst_off, 0);
        let n_bytes = size.div_ceil(8);
        // The data words are `words - 2` words wide, i.e. `(words - 2) * 8` bytes.
        // SAFETY: `base` covers exactly the payload bytes the writer zero-filled.
        let stored = unsafe { core::slice::from_raw_parts(base, (words - 2) * 8) };
        assert!(
            stored[n_bytes..].iter().all(|b| *b == 0),
            "{off}+{size}: padding bytes"
        );
        if size % 8 != 0 {
            assert_eq!(
                stored[n_bytes - 1] & (0xFFu8 >> (size % 8)),
                0,
                "{off}+{size}: trailing bits of the last byte"
            );
        }
    }
}

#[test]
fn the_threshold_is_the_window_size_and_sub_binary_words_agrees() {
    let payload = Payload::<128>([0xC3; 128]);
    let mut heap = Heap::new(4096);
    let parent = refc_parent(&mut heap, &payload.0, 0, 128 * 8);

    // Exactly at the limit: the heap arm, `2 + 64 / 8 = 10` words.
    // SAFETY: `parent` is a live refc binary.
    let at_limit = unsafe { sub_binary_words(parent, ERL_ONHEAP_BITS_LIMIT) }.expect("windowable");
    assert_eq!(at_limit, heap_bits_size_words(ERL_ONHEAP_BITS_LIMIT));
    assert_eq!(at_limit, 10);
    let used = heap.used();
    let child = window(&mut heap, parent, 8, ERL_ONHEAP_BITS_LIMIT);
    assert_eq!(heap.used() - used, at_limit, "the writer used what it reserved");
    assert!(!unsafe { is_refc_binary(child) });

    // One bit more: the refc arm, the 8-word pair.
    // SAFETY: as above.
    let over = unsafe { sub_binary_words(parent, ERL_ONHEAP_BITS_LIMIT + 1) }.expect("windowable");
    assert_eq!(over, ERL_REFC_BITS_SIZE);
    let used = heap.used();
    let child = window(&mut heap, parent, 8, ERL_ONHEAP_BITS_LIMIT + 1);
    assert_eq!(heap.used() - used, over, "the writer used what it reserved");
    assert!(unsafe { is_refc_binary(child) });
    assert_eq!(unsafe { refc_size(child) }, ERL_ONHEAP_BITS_LIMIT + 1);
}

#[test]
fn the_refc_arm_keeps_the_payload_reachable_with_start_and_end() {
    let payload = Payload::<128>([0x3C; 128]);
    let mut heap = Heap::new(4096);
    // The parent is itself a window into the payload (start 48), so the child's `start`
    // must be the parent's start plus the offset — the C's `inner_offset + offset`.
    let parent = refc_parent(&mut heap, &payload.0, 48, 900);
    let parent_bits = bits(parent, 900);
    let (off, size) = (200usize, 700usize); // 700 > 512, and 200 + 700 == 900

    let child = window(&mut heap, parent, off, size);

    assert!(unsafe { is_refc_binary(child) });
    assert_eq!(unsafe { refc_offset(child) }, 48 + off);
    assert_eq!(unsafe { refc_size(child) }, size);
    assert_eq!(bits(child, size), parent_bits[off..].to_vec());
    // Same payload pointer, same chain link: the child is a second handle on one payload.
    let (child_val, child_next) = unsafe { bin_ref_parts(sub_bits_parts(child).3) };
    let (parent_val, parent_next) = unsafe { bin_ref_parts(sub_bits_parts(parent).3) };
    assert_eq!(child_val, payload.0.as_ptr() as *mut Eterm);
    assert_eq!(child_val, parent_val);
    assert_eq!(child_next, parent_next);
    // The child's `orig` is its own `BinRef` (the pair `write_refc_binary` shapes), never
    // the parent's word — both name the same payload.
    assert_ne!(
        unsafe { sub_bits_parts(child).3 },
        unsafe { sub_bits_parts(parent).3 }
    );
}

#[test]
fn a_window_of_a_window_composes() {
    let payload = payload_128();
    let mut heap = Heap::new(4096);
    let parent = refc_parent(&mut heap, &payload.0, 0, 128 * 8);
    let parent_bits = bits(parent, 128 * 8);
    // A refc window (600 bits) of the parent, then a 100-bit window of that window.
    let inner = window(&mut heap, parent, 100, 600);
    let outer = window(&mut heap, inner, 250, 100);
    assert_eq!(
        bits(outer, 100),
        parent_bits[100 + 250..100 + 250 + 100].to_vec()
    );
    // ... and a byte-aligned window of a *refc* window takes the heap arm and still
    // copies the right bits.
    let copied = window(&mut heap, inner, 8, 64);
    assert!(!unsafe { is_refc_binary(copied) });
    assert_eq!(bits(copied, 64), parent_bits[108..172].to_vec());
}

#[test]
fn refusals_are_typed_and_write_nothing() {
    let payload = payload_64();
    let mut heap = Heap::new(1024);
    let parent = refc_parent(&mut heap, &payload.0, 0, 64 * 8);
    let tup = heap.tuple(&[make_small(1)]).expect("the tuple fits");

    // A non-bitstring parent.
    for bad in [make_small(1), NIL, tup] {
        let used = heap.used();
        let hp = unsafe { heap.alloc_words(10) }.expect("10 words");
        // SAFETY: `hp` owns 10 words — more than any arm needs, which is fine: nothing is
        // written on the refusal path.
        assert_eq!(
            unsafe { write_sub_binary(hp, bad, 0, 8) },
            Err(TermError::BadBitstring)
        );
        assert_eq!(
            unsafe { sub_binary_words(bad, 8) },
            Err(TermError::BadBitstring)
        );
        assert_eq!(heap.used() - used, 10, "the writer itself allocates nothing");
    }

    // A heap parent whose size word contradicts its header arity (3 words claiming 512
    // bits, which needs 10).
    // SAFETY: 3 words reserved; only the header's size word is rewritten.
    let lying = unsafe {
        let hp = heap.alloc_words(heap_bits_size_words(16)).expect("3 words");
        let term = beam_term::bitstring::write_heap_bits(hp, &[1, 2], 16).expect("2 bytes");
        *hp.add(1) = Eterm::from_raw(ERL_ONHEAP_BITS_LIMIT);
        term
    };
    let hp = unsafe { heap.alloc_words(32) }.expect("32 words");
    // SAFETY: 32 words is more than the 10 the lying parent would need.
    assert_eq!(
        unsafe { write_sub_binary(hp, lying, 0, 8) },
        Err(TermError::BadBitstring)
    );

    // A span past the parent's end, and a span that would wrap `usize`.
    for (off, size) in [(0, 1025), (1024, 1), (1000, 100), (usize::MAX, 1)] {
        // SAFETY: the reservation is a valid arm size for the *requested* window; the
        // writer refuses before writing.
        let words = unsafe { sub_binary_words(parent, size.min(ERL_ONHEAP_BITS_LIMIT)) }
            .unwrap_or(ERL_REFC_BITS_SIZE);
        let hp = unsafe { heap.alloc_words(words.max(10)) }.expect("words");
        assert_eq!(
            unsafe { write_sub_binary(hp, parent, off, size) },
            Err(TermError::BadBitstring),
            "{off}+{size} past a 1024-bit parent"
        );
    }

    // A match-context parent: windowable on the heap arm, refused above the limit (its
    // `orig` is not a `BinRef` and its flags say it may change).
    // SAFETY: 5 words for the `ErlSubBits`; the `base_flags` word packs a live data base.
    let ctx = unsafe {
        let hp = heap.alloc_words(beam_term::refc::sub_bits_size_words()).expect("5 words");
        write_sub_bits(
            hp,
            Eterm::from_raw(payload.0.as_ptr() as usize | ERL_SUB_BITS_FLAG_MUTABLE),
            Eterm::from_raw(0),
            Eterm::from_raw(64 * 8),
            NIL,
        )
    };
    assert_eq!(
        unsafe { sub_binary_words(ctx, ERL_ONHEAP_BITS_LIMIT + 1) },
        Err(TermError::BadBitstring)
    );
    let hp = unsafe { heap.alloc_words(ERL_REFC_BITS_SIZE) }.expect("8 words");
    assert_eq!(
        unsafe { write_sub_binary(hp, ctx, 0, 600) },
        Err(TermError::BadBitstring)
    );
    // The heap arm reads a match context's bits like any other window source.
    let copied = window(&mut heap, ctx, 8, 64);
    assert_eq!(bits(copied, 64), bits(ctx, 72)[8..].to_vec());

    // An on-heap parent cannot be shared above the limit either.
    // SAFETY: 3 words for a 2-byte heap binary.
    let heap_bin = unsafe {
        let hp = heap.alloc_words(heap_bits_size_words(16)).expect("3 words");
        beam_term::bitstring::write_heap_bits(hp, &[0xAB, 0xCD], 16).expect("2 bytes")
    };
    assert_eq!(
        unsafe { sub_binary_words(heap_bin, ERL_ONHEAP_BITS_LIMIT + 1) },
        Err(TermError::BadBitstring)
    );
}

#[test]
fn refc_payload_is_the_payload_key_and_refuses_everything_else() {
    let payload = payload_128();
    let mut heap = Heap::new(1024);
    let parent = refc_parent(&mut heap, &payload.0, 0, 128 * 8);

    // A refc window — and the "plain" refc binary it came from — name the payload's first
    // data byte, which is the key `binary:referenced_byte_size/1` looks its allocated size
    // up by.
    // SAFETY: both terms are live refc binaries over the live `payload`.
    assert_eq!(
        unsafe { refc_payload(parent) }.expect("a refc binary"),
        payload.0.as_ptr() as *mut Eterm
    );
    let child = window(&mut heap, parent, 300, 600);
    // SAFETY: as above.
    assert_eq!(
        unsafe { refc_payload(child) }.expect("a refc window"),
        payload.0.as_ptr() as *mut Eterm
    );

    // A *heap-arm* window, an on-heap bitstring, a match context and a non-bitstring are
    // the C's `br == NULL`: each answers its own byte size, so there is no payload key.
    let small = window(&mut heap, parent, 8, 64);
    assert!(!unsafe { is_refc_binary(small) });
    // SAFETY: 3 words for a 2-byte heap binary.
    let heap_bin = unsafe {
        let hp = heap.alloc_words(heap_bits_size_words(16)).expect("3 words");
        beam_term::bitstring::write_heap_bits(hp, &[1, 2], 16).expect("2 bytes")
    };
    // SAFETY: 5 words for the `ErlSubBits`.
    let ctx = unsafe {
        let hp = heap.alloc_words(beam_term::refc::sub_bits_size_words()).expect("5 words");
        write_sub_bits(
            hp,
            Eterm::from_raw(0x8000),
            Eterm::from_raw(0),
            Eterm::from_raw(8),
            NIL,
        )
    };
    // SAFETY: every argument is a live term; none is a refc binary.
    for bad in [small, heap_bin, ctx, make_small(7), NIL] {
        assert_eq!(unsafe { refc_payload(bad) }, Err(TermError::BadBitstring));
    }
}

/// The stated contract, not an assertion this crate could make: the referenced size is the
/// **payload's** allocated width, which this crate cannot see.
///
/// The oracle (`erl_bif_binary.c:2691-2717`) answers `(br->val)->orig_size` for a refc
/// window and its own byte size otherwise. `orig_size` is the payload's *allocated* size —
/// `erl -noshell -eval 'C = binary:copy(<<255>>,1024), B = <<"z",C/binary>>,
/// io:format("~p~n",[binary:referenced_byte_size(B)]).'` prints **2050** for the
/// `erl_eval`-built binary whose payload was grown to its allocated width (`byte_size`
/// 1025), while a compiled `<<"z",C/binary>>` prints **1025**.
///
/// The frozen analogue is `beam_alloc::RefcBinary`'s header, and `capacity()` is the closest
/// of its words: it equals `orig_size` when the payload was allocated at the width the C
/// would have requested, and it differs after a growth-then-trim (nothing shrinks it, where
/// `erts_bin_realloc(binp, size)` records the *new* requested size). `len()` is the apparent
/// size and `reserved_bytes()` the allocator's, so neither is `orig_size` in general.
///
/// What this crate guarantees is exactly the one input that lookup needs: a window and its
/// parent answer the **same** payload key.
#[test]
fn the_referenced_size_is_the_payloads_and_not_the_terms() {
    let payload = Payload::<128>([0x77; 128]);
    let mut heap = Heap::new(1024);
    let parent = refc_parent(&mut heap, &payload.0, 0, 1024);
    let windowed = window(&mut heap, parent, 500, 520);

    // Two different terms — a 1024-bit binary and a 520-bit window — one payload, so one
    // referenced size. `refc_size`/`bitstring_parts` are the *window's* size, which is
    // deliberately not the answer.
    assert_eq!(unsafe { refc_size(windowed) }, 520);
    assert_eq!(unsafe { refc_size(parent) }, 1024);
    assert_eq!(
        // SAFETY: both terms are live refc binaries over the live `payload`.
        unsafe { refc_payload(windowed) },
        // SAFETY: as above.
        unsafe { refc_payload(parent) }
    );
    assert_eq!(
        // SAFETY: as above.
        unsafe { refc_payload(windowed) }.expect("a refc window"),
        payload.0.as_ptr() as *mut Eterm
    );
}
