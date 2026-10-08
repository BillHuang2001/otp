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

//! Copy-correctness evidence for the **refc binary** term shape through a collection.
//!
//! A full refc binary is an 8-word on-heap pair `[BinRef(3)][ErlSubBits(5)]` written by
//! `beam_term::refc::write_refc_binary`, whose returned root is the `ErlSubBits` — the
//! *higher*-addressed object — while its `orig` slot (word 4) holds a boxed pointer
//! **back** to the lower-addressed `BinRef`. `beam_gc`'s Cheney scan must therefore:
//!
//! * copy the `ErlSubBits` (5 words) when it forwards the root, leaving the `BinRef`;
//! * recognise the `ErlSubBits` as an opaque thing (using its `thing_arityval + 1` to
//!   skip it) but forward its `orig` word (`erl_gc.c`'s special `HEADER_SUB_BITS`
//!   case) to a fresh copy of the `BinRef`;
//! * copy the `BinRef` (3 words) as an opaque thing, leaving its off-heap `val`
//!   pointer and its `next` link **byte-identical** (the payload lives off the heap
//!   and is never moved by a collection).
//!
//! The test asserts all of that, and that the to-space holds exactly
//! [`ERL_REFC_BITS_SIZE`] (8) words for the pair. It exercises the public API only;
//! every `unsafe` block carries a `// SAFETY:` rationale.

use beam_alloc::{default_allocator, RefcBinary};
use beam_gc::prelude::{collect, HeapSpace};
use beam_term::prelude::{
    boxed_val, is_refc_binary, refc_bytes, refc_size, write_refc_binary, Eterm, BIN_REF_SUBTAG,
    ERL_BIN_REF_SIZE, ERL_REFC_BITS_SIZE, ERL_SUB_BITS_SIZE, HEADER_BIN_REF, HEADER_SUB_BITS,
    SUB_BITS_SUBTAG,
};

/// Allocates an empty `words`-word semi-space over the default allocator.
fn space(words: usize) -> HeapSpace {
    HeapSpace::with_allocator(default_allocator().clone(), words).expect("heap allocation")
}

/// The frozen 8-word pair geometry (`erl_bits.h`): `BinRef(3) + ErlSubBits(5)`.
#[test]
fn refc_binary_pair_geometry_matches_erl_bits_h() {
    assert_eq!(ERL_BIN_REF_SIZE, 3);
    assert_eq!(ERL_SUB_BITS_SIZE, 5);
    assert_eq!(ERL_REFC_BITS_SIZE, 8);
    assert_eq!(HEADER_BIN_REF.raw() & 0x3F, BIN_REF_SUBTAG);
    assert_eq!(HEADER_SUB_BITS.raw() & 0x3F, SUB_BITS_SUBTAG);
}

#[test]
fn refc_binary_survives_collection_with_orig_forwarded() {
    // A real off-heap payload; its bytes never move, so the on-heap pair must keep
    // pointing at them verbatim.
    let payload = RefcBinary::new(b"a refc binary payload, off the process heap").expect("payload");
    let bytes = payload.as_bytes().to_vec();
    let size_bits = bytes.len() * 8;

    let mut from = space(ERL_REFC_BITS_SIZE);
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved at
    // `hp`, the `write_refc_binary` contract; `payload.data_ptr()` is a live,
    // word-aligned pointer to `size_bits` bits of off-heap storage (kept alive by
    // `payload` for the whole test), and `null` is the (empty) off-heap chain link.
    let root = unsafe {
        let hp = from
            .heap_mut()
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words for the pair");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            0,
            size_bits,
        )
    };
    assert_eq!(from.used_words(), ERL_REFC_BITS_SIZE);
    // SAFETY: `root` is the live `ErlSubBits` written above.
    assert!(unsafe { is_refc_binary(root) });

    // The to-space is sized exactly to the pair: the copy must fit precisely.
    let mut to = space(ERL_REFC_BITS_SIZE);
    let mut roots = [root];
    let stats = collect(&from, &mut to, &mut roots).expect("collect");

    // (a) The root (the `ErlSubBits`) moved into the to-space, out of the from-space.
    let new_root = roots[0];
    assert!(
        to.contains_term(new_root),
        "the SubBits root must point into the to-space"
    );
    assert!(
        !from.contains_term(new_root),
        "the SubBits root must no longer point into the from-space"
    );

    // (b) `orig` (word 4) was forwarded to the copied `BinRef` in the to-space.
    // SAFETY: `new_root` is a live, 5-word `ErlSubBits` (established below), so its
    // `orig` word is in-bounds.
    let orig = unsafe { *boxed_val(new_root).add(4) };
    assert!(
        to.contains_term(orig),
        "the copied `orig` must point into the to-space"
    );
    // SAFETY: `orig` is a live boxed term; its header word is readable.
    assert_eq!(
        unsafe { *boxed_val(orig) },
        HEADER_BIN_REF,
        "`orig` must point at the copied BinRef"
    );

    // (c) The term is still a refc binary and still describes the original payload.
    // SAFETY: `new_root` is the live, copied `ErlSubBits`; its off-heap payload is
    // `payload`, alive for the whole test.
    unsafe {
        assert!(is_refc_binary(new_root));
        assert_eq!(refc_size(new_root), size_bits);
        assert_eq!(
            refc_bytes(new_root),
            &bytes[..],
            "the off-heap payload bytes are untouched"
        );
    }

    // (d) The copied `BinRef.val` word is byte-identical to the off-heap pointer: a
    // collection never moves or rewrites the payload reference.
    // SAFETY: `orig` is the live copied `BinRef`, so words 1 and 2 are in-bounds.
    let val = unsafe { *boxed_val(orig).add(1) };
    let next = unsafe { *boxed_val(orig).add(2) };
    assert_eq!(
        val.raw() as *mut u8,
        payload.data_ptr(),
        "the off-heap `val` pointer is left verbatim"
    );
    assert_eq!(
        next.raw(),
        0,
        "the (empty) off-heap chain link is left verbatim"
    );

    // (e) The to-space holds exactly the 8 words of the pair (2 objects).
    assert_eq!(
        to.used_words(),
        ERL_REFC_BITS_SIZE,
        "the pair is exactly 8 words"
    );
    assert_eq!(stats.words_copied, ERL_REFC_BITS_SIZE);
    assert_eq!(stats.objects_copied, 2, "the SubBits + the BinRef");
    assert_eq!(stats.words_reclaimed, 0, "the whole from-space was live");
}

/// The focused `move_size` proof for the pair: an `ErlSubBits` (5) plus the `BinRef`
/// it forwards to (3) is exactly [`ERL_REFC_BITS_SIZE`], however the copy is ordered.
#[test]
fn refc_binary_copy_is_exactly_eight_words() {
    for len in [1usize, 64, 4096] {
        let data = vec![0xABu8; len];
        let payload = RefcBinary::new(&data).expect("payload");

        let mut from = space(ERL_REFC_BITS_SIZE);
        // SAFETY: as in the test above — 8 reserved words, a live off-heap payload of
        // `len * 8` bits, and a null chain link.
        let root = unsafe {
            let hp = from
                .heap_mut()
                .alloc_words(ERL_REFC_BITS_SIZE)
                .expect("8 words");
            write_refc_binary(
                hp,
                payload.data_ptr() as *mut Eterm,
                core::ptr::null_mut(),
                payload.data_ptr(),
                0,
                len * 8,
            )
        };

        let mut to = space(ERL_REFC_BITS_SIZE);
        let mut roots = [root];
        let stats = collect(&from, &mut to, &mut roots).expect("collect");

        assert_eq!(to.used_words(), ERL_REFC_BITS_SIZE, "len = {len}");
        assert_eq!(stats.words_copied, ERL_REFC_BITS_SIZE, "len = {len}");
    }
}
