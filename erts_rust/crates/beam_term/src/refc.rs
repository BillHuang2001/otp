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

//! Off-heap ("refc") binaries: the `BinRef` + `ErlSubBits` on-heap pair.
//!
//! A binary larger than [`ERL_ONHEAP_BINARY_LIMIT`] (or a bitstring exceeding
//! [`ERL_ONHEAP_BITS_LIMIT`] bits) is not copied onto the process heap: its payload
//! lives in an **off-heap refcounted object** owned by `beam_alloc`/`beam_gc`, and
//! the term is an on-heap **pair**:
//!
//! ```text
//!   hp[0] = HEADER_BIN_REF      // BinRef.thing_word  = make_header(2, BIN_REF_SUBTAG)
//!   hp[1] = val                 // BinRef.val   -> the off-heap payload
//!   hp[2] = next                // BinRef.next  -> the MSO off-heap chain
//!   hp[3] = HEADER_SUB_BITS     // ErlSubBits.thing_word = make_header(4, SUB_BITS_SUBTAG)
//!   hp[4] = base_flags          // base pointer | flags
//!   hp[5] = start               // bit offset into the base
//!   hp[6] = end                 // end bit offset (size = end - start)
//!   hp[7] = orig                // = make_boxed(hp)  (a boxed pointer to the BinRef)
//! ```
//!
//! and the tagged root is `make_boxed(hp + 3)` — the **`ErlSubBits`**, which is what
//! a term points at (`erts/emulator/beam/erl_bits.c:2672-2712`,
//! `erts_wrap_refc_bitstring`: `br = *hpp`, `sb = &(*hpp)[ERL_BIN_REF_SIZE]`,
//! `return make_bitstring(sb)`). Note the ordering: the `BinRef` sits at the **lower**
//! address and the `ErlSubBits` above it, so the root offset is
//! [`bin_ref_size_words`]. `ERL_REFC_BITS_SIZE` (8) is the pair's total size.
//!
//! A `BinRef` is never a user-visible term on its own — it is only reachable through
//! the `ErlSubBits::orig` slot — but a *sub-bitstring window* is just an `ErlSubBits`
//! (see [`crate::bitstring::write_sub_bits`]) whose `orig` is a `BinRef`.
//!
//! Ground truth: `erts/emulator/beam/erl_bits.h:58-176` (the `ErlSubBits`/`BinRef`/
//! `ErlHeapBits` definitions, `HEADER_SUB_BITS`/`HEADER_BIN_REF`,
//! `ERL_SUB_BITS_SIZE`/`ERL_BIN_REF_SIZE`/`ERL_REFC_BITS_SIZE`, the on-heap limits)
//! and `erl_bits.c:2672-2712`.
//!
//! # Frozen inter-crate contract (`beam_alloc` / `beam_gc`)
//!
//! * The on-heap `BinRef.val` word holds a raw pointer to the **first data byte** of
//!   the off-heap refcounted payload. Where that payload's own header lives (and how
//!   it is refcounted / swept) is `beam_alloc`'s and `beam_gc`'s internal concern —
//!   `beam_term` only reads/writes the pointer word ([`write_bin_ref`] /
//!   [`bin_ref_parts`]).
//! * `ErlSubBits.orig` is a **term**: a boxed pointer to the `BinRef` (a full
//!   refc binary), or an on-heap bitstring when the `ErlSubBits` is a match context
//!   (`ERL_SUB_BITS_FLAG_MUTABLE`).
//! * The 8-word on-heap arrangement is exactly `[BinRef(3)][ErlSubBits(5)]` with the
//!   root at `hp + 3`, as written by [`write_refc_binary`]; `beam_gc` must move/copy
//!   all 8 words together and fix up `orig` to the new `BinRef` address.
//!
//! # `unsafe` contract
//!
//! Every function is `unsafe`: an `Eterm` being a live, well-formed term (and the
//! off-heap payload being alive) is a caller invariant, and reading or writing a raw
//! heap word has no safe form. Each documents, under `# Safety`, that invariant and
//! the lifetime of any borrowed slice.

use crate::bitstring::{sub_bits_parts, ERL_SUB_BITS_FLAG_MASK};
use crate::boxed::{
    boxed_val, is_bin_ref, is_boxed_term, make_boxed, ERL_BIN_REF_SIZE, ERL_SUB_BITS_SIZE,
    HEADER_BIN_REF, HEADER_SUB_BITS,
};
use crate::eterm::Eterm;

/// `ERL_BIN_REF_SIZE` (`erl_bits.h:144`): the words in a `BinRef`
/// (`[thing_word, val, next]`).
#[inline]
pub const fn bin_ref_size_words() -> usize {
    ERL_BIN_REF_SIZE
}

/// `ERL_SUB_BITS_SIZE` (`erl_bits.h:128`): the words in an `ErlSubBits`
/// (`[thing_word, base_flags, start, end, orig]`).
#[inline]
pub const fn sub_bits_size_words() -> usize {
    ERL_SUB_BITS_SIZE
}

/// `ERL_REFC_BITS_SIZE` (`erl_bits.h:147`): the words the on-heap
/// `BinRef` + `ErlSubBits` pair occupies (`ERL_BIN_REF_SIZE + ERL_SUB_BITS_SIZE`).
pub const ERL_REFC_BITS_SIZE: usize = ERL_BIN_REF_SIZE + ERL_SUB_BITS_SIZE;

/// `ERL_ONHEAP_BINARY_LIMIT` (`erl_bits.h:167`): the largest binary (in **bytes**)
/// stored on the heap as an `ErlHeapBits`; larger binaries become refc binaries.
pub const ERL_ONHEAP_BINARY_LIMIT: usize = 64;

/// `ERL_ONHEAP_BITS_LIMIT` (`erl_bits.h:168`): the largest bitstring (in **bits**)
/// stored on the heap as an `ErlHeapBits`; larger bitstrings become sub-bitstrings.
pub const ERL_ONHEAP_BITS_LIMIT: usize = ERL_ONHEAP_BINARY_LIMIT * 8;

// ---------------------------------------------------------------------------
// BinRef
// ---------------------------------------------------------------------------

/// Writes a `BinRef` block at `hp` and returns the tagged root.
///
/// Layout pinned to `erl_bits.h:135-144`: `[HEADER_BIN_REF, val, next]` (3 words),
/// with the root `make_boxed(hp)`. In ERTS a `BinRef` is `{thing_word, Binary *val,
/// off_heap_header *next}`; per the frozen inter-crate contract (see the module docs)
/// `val` here holds a raw pointer to the payload's **first data byte** and `next`
/// threads the process's off-heap (MSO) chain.
///
/// # Safety
///
/// `hp` must point at [`bin_ref_size_words`] (3) exclusively-owned, writable words,
/// which stay live (and unmoved) for as long as the returned term is used. `val` must
/// stay valid for that long, and `next` must be a valid off-heap chain link (or null).
pub unsafe fn write_bin_ref(hp: *mut Eterm, val: *mut Eterm, next: *mut Eterm) -> Eterm {
    // SAFETY: the caller reserved the 3-word `BinRef` block at `hp`; every word is
    // written before the pointer escapes.
    unsafe {
        *hp = HEADER_BIN_REF;
        *hp.add(1) = Eterm::from_raw(val as usize);
        *hp.add(2) = Eterm::from_raw(next as usize);
    }
    make_boxed(hp)
}

/// Splits a `BinRef` term into `(val, next)`.
///
/// The exact inverse of [`write_bin_ref`] (`erl_bits.h:135-139`). `val` points at the
/// payload's first data byte and `next` at the next off-heap chain link.
///
/// # Safety
///
/// `t` must be a live `BinRef` term (a boxed term whose header is `HEADER_BIN_REF`),
/// kept alive by the caller.
pub unsafe fn bin_ref_parts(t: Eterm) -> (*mut Eterm, *mut Eterm) {
    let p = boxed_val(t);
    // SAFETY: the caller guarantees `t` is a live `BinRef`, so words 1 and 2 are
    // initialised by [`write_bin_ref`].
    unsafe {
        (
            (*p.add(1)).raw() as *mut Eterm,
            (*p.add(2)).raw() as *mut Eterm,
        )
    }
}

// ---------------------------------------------------------------------------
// The combined refc-binary term (BinRef + ErlSubBits)
// ---------------------------------------------------------------------------

/// Writes the full 8-word refc-binary pair at `hp` and returns the tagged root (the
/// `ErlSubBits`, at `hp + `[`bin_ref_size_words`]).
///
/// The arrangement is pinned to `erts_wrap_refc_bitstring`
/// (`erl_bits.c:2672-2712`): the `BinRef` at `hp[0..3]` (see [`write_bin_ref`]) and
/// the `ErlSubBits` at `hp[3..8]` (see [`crate::bitstring::write_sub_bits`]), whose
/// `orig` slot holds `make_boxed(hp)` — a boxed pointer back to the `BinRef`.
/// `base` is the off-heap payload's first data byte, `offset_bits`/`size_bits` the
/// window into it (a full binary uses `offset_bits = 0`).
///
/// # Safety
///
/// `hp` must point at [`ERL_REFC_BITS_SIZE`] (8) exclusively-owned, writable words,
/// which stay live (and unmoved) for as long as the returned term is used. `val` must
/// point at the off-heap payload's first data byte (kept alive by the caller),
/// `next` must be a valid off-heap chain link (or null), and `base` must point at
/// storage covering bits `offset_bits..offset_bits + size_bits` and be aligned so
/// that the low [`ERL_SUB_BITS_FLAG_MASK`] bits are clear (word-aligned, as the
/// `beam_alloc` payload contract guarantees).
pub unsafe fn write_refc_binary(
    hp: *mut Eterm,
    val: *mut Eterm,
    next: *mut Eterm,
    base: *const u8,
    offset_bits: usize,
    size_bits: usize,
) -> Eterm {
    debug_assert_eq!(
        base as usize & ERL_SUB_BITS_FLAG_MASK,
        0,
        "the off-heap payload base must be word-aligned (frozen contract)"
    );
    // SAFETY: the caller reserved `ERL_REFC_BITS_SIZE` (8) words: the `BinRef` at
    // `hp[0..3]` and the `ErlSubBits` at `hp[3..8]`. Every word is written before the
    // returned pointer escapes. `orig = make_boxed(hp)` is a boxed pointer to the
    // `BinRef`, exactly as `erts_wrap_refc_bitstring` sets it.
    unsafe {
        *hp = HEADER_BIN_REF;
        *hp.add(1) = Eterm::from_raw(val as usize);
        *hp.add(2) = Eterm::from_raw(next as usize);

        let sb = hp.add(ERL_BIN_REF_SIZE);
        *sb = HEADER_SUB_BITS;
        *sb.add(1) = Eterm::from_raw(base as usize);
        *sb.add(2) = Eterm::from_raw(offset_bits);
        *sb.add(3) = Eterm::from_raw(offset_bits + size_bits);
        *sb.add(4) = make_boxed(hp);
        make_boxed(sb)
    }
}

/// Whether `t` is a full refc binary: an `ErlSubBits` whose `orig` is a `BinRef`.
///
/// Reproduces the `*boxed_val(orig) == HEADER_BIN_REF` test
/// (`erl_bits.c:117`, `ERTS_GET_BITSTRING_REF`). An on-heap bitstring, a
/// match-context `ErlSubBits` (whose `orig` is an on-heap bitstring or a non-bin-ref
/// term), or any non-bitstring yields `false`.
///
/// # Safety
///
/// If `t` is a boxed term, its header word must be a live, aligned term word and — if
/// it is an `ErlSubBits` — its `orig` slot must be a live term.
pub unsafe fn is_refc_binary(t: Eterm) -> bool {
    if !is_boxed_term(t) {
        return false;
    }
    let p = boxed_val(t);
    // SAFETY: `t` is boxed, so its header word is live.
    if unsafe { *p } != HEADER_SUB_BITS {
        return false;
    }
    // SAFETY: the header fixed the block at the 5-word `ErlSubBits` layout, so `orig`
    // (word 4) is a live term.
    let orig = unsafe { *p.add(4) };
    // SAFETY: `orig` is a live term; `is_bin_ref` dereferences only a boxed header.
    unsafe { is_bin_ref(orig) }
}

/// The start of a refc binary's window, in **bits**.
///
/// This is the `ErlSubBits::start` slot (`erl_bits.h:76`) — a **bit** offset, not a
/// byte offset (the `ErlSubBits` layout stores bit offsets; a full binary has
/// `start == 0`). For the whole-binary view the payload's data base is the
/// `base_flags & !ERL_SUB_BITS_FLAG_MASK` pointer.
///
/// # Safety
///
/// `t` must be a live `ErlSubBits` term (e.g. produced by [`write_refc_binary`] or
/// [`crate::bitstring::write_sub_bits`]).
pub unsafe fn refc_offset(t: Eterm) -> usize {
    // SAFETY: `t` is a live `ErlSubBits`, so its `start` slot is initialised.
    unsafe { sub_bits_parts(t).1.raw() }
}

/// The size of a refc binary's window, in **bits** (`ErlSubBits::end - start`).
///
/// This is a **bit** count, matching [`refc_bytes`]'s
/// `ceil(refc_size(t) / 8)`-byte view (`erl_bits.h:53-55`,
/// `ERTS_GET_BITSTRING`'s `size`).
///
/// # Safety
///
/// `t` must be a live `ErlSubBits` term with `end >= start`.
pub unsafe fn refc_size(t: Eterm) -> usize {
    // SAFETY: `t` is a live `ErlSubBits`, so its `start`/`end` slots are initialised.
    let (_base_flags, start, end, _orig) = unsafe { sub_bits_parts(t) };
    end.raw() - start.raw()
}

/// A borrowed byte view of a refc binary's payload.
///
/// The slice starts at the byte containing bit [`refc_offset`] and spans through the
/// byte containing the last referenced bit, i.e. its length is
/// `ceil((offset + size) / 8) - floor(offset / 8)`. For a full binary
/// (`offset == 0`) that is exactly `ceil(refc_size(t) / 8)` bytes of the off-heap
/// payload.
///
/// # Safety
///
/// `t` must be a live `ErlSubBits` term whose underlying off-heap payload is alive
/// and will not move for `'a` (the caller holds a reference to the refcounted
/// payload — e.g. a `beam_alloc` binary handle). `end` must not precede `start`.
pub unsafe fn refc_bytes<'a>(t: Eterm) -> &'a [u8] {
    // SAFETY: `t` is a live `ErlSubBits`, so words 1..4 are initialised.
    let (base_flags, start, end, _orig) = unsafe { sub_bits_parts(t) };
    let base = (base_flags.raw() & !ERL_SUB_BITS_FLAG_MASK) as *const u8;
    let start = start.raw();
    let end = end.raw();
    let first_byte = start >> 3;
    let last_byte = crate::bitstring::bit_count_to_bytes(end);
    // SAFETY: the caller guarantees the payload covers bits `start..end`, so the bytes
    // `first_byte..last_byte` are inside it and stay live for `'a`.
    unsafe { core::slice::from_raw_parts(base.add(first_byte), last_byte - first_byte) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitstring::{bit_at, bitstring_parts, write_sub_bits};
    use crate::boxed::{arityval, is_bitstring, make_header};
    use crate::heap::Heap;
    use crate::immediate::{make_small, NIL};
    use crate::tag::{BIN_REF_SUBTAG, SUB_BITS_SUBTAG};

    fn new_heap() -> Heap {
        Heap::new(64)
    }

    #[test]
    fn sizes_and_limits_match_erl_bits_h() {
        assert_eq!(bin_ref_size_words(), 3);
        assert_eq!(sub_bits_size_words(), 5);
        assert_eq!(ERL_REFC_BITS_SIZE, 8);
        assert_eq!(ERL_ONHEAP_BINARY_LIMIT, 64);
        assert_eq!(ERL_ONHEAP_BITS_LIMIT, 512);
        // `HEADER_BIN_REF = _make_header(2, 0x28)`, `HEADER_SUB_BITS = _make_header(4, 0x24)`.
        assert_eq!(HEADER_BIN_REF.raw(), make_header(2, BIN_REF_SUBTAG).raw());
        assert_eq!(HEADER_BIN_REF.raw(), 0xA8);
        assert_eq!(HEADER_SUB_BITS.raw(), make_header(4, SUB_BITS_SUBTAG).raw());
        assert_eq!(HEADER_SUB_BITS.raw(), 0x124);
        assert_eq!(arityval(HEADER_BIN_REF), 2);
        assert_eq!(arityval(HEADER_SUB_BITS), 4);
    }

    #[test]
    fn bin_ref_block_layout_is_pinned() {
        let mut heap = new_heap();
        // SAFETY: 3 words reserved for the `BinRef`.
        let term = unsafe {
            let hp = heap.alloc_words(bin_ref_size_words()).unwrap();
            write_bin_ref(hp, 0x1000 as *mut Eterm, core::ptr::null_mut())
        };
        // SAFETY: `term` is the `BinRef` just written.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, HEADER_BIN_REF);
            assert_eq!((*p.add(1)).raw(), 0x1000);
            assert_eq!((*p.add(2)).raw(), 0);

            let (val, next) = bin_ref_parts(term);
            assert_eq!(val as usize, 0x1000);
            assert!(next.is_null());
        }
    }

    #[test]
    fn combined_block_layout_is_pinned() {
        #[repr(align(8))]
        struct Storage([u8; 8]);
        let storage = Storage([0xDE, 0xAD, 0xBE, 0xEF, 0, 0, 0, 0]);

        let mut heap = new_heap();
        // SAFETY: `ERL_REFC_BITS_SIZE` (8) words reserved for the pair.
        let root = unsafe {
            let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).unwrap();
            write_refc_binary(
                hp,
                0x2000 as *mut Eterm,
                core::ptr::null_mut(),
                storage.0.as_ptr(),
                0,
                32,
            )
        };
        assert_eq!(heap.used(), 8);

        // SAFETY: `root` is the pair just written; all 8 words are live.
        unsafe {
            let br = boxed_val(root).sub(bin_ref_size_words());
            let sb = boxed_val(root);
            // BinRef at the lower address...
            assert_eq!(*br, HEADER_BIN_REF);
            assert_eq!((*br.add(1)).raw(), 0x2000);
            assert_eq!((*br.add(2)).raw(), 0);
            // ... and the ErlSubBits at `hp + ERL_BIN_REF_SIZE`, holding the root.
            assert_eq!(sb, br.add(bin_ref_size_words()));
            assert_eq!(*sb, HEADER_SUB_BITS);
            assert_eq!((*sb.add(1)).raw(), storage.0.as_ptr() as usize);
            assert_eq!((*sb.add(2)).raw(), 0);
            assert_eq!((*sb.add(3)).raw(), 32);
            assert_eq!(
                *sb.add(4),
                make_boxed(br),
                "orig is a boxed pointer to the BinRef"
            );
        }

        assert!(unsafe { is_refc_binary(root) });
        assert_eq!(unsafe { refc_offset(root) }, 0);
        assert_eq!(unsafe { refc_size(root) }, 32);
        assert_eq!(unsafe { refc_bytes(root) }, &storage.0[..4]);
        // SAFETY: `root` is a live bitstring with byte-aligned size.
        assert!(unsafe { is_bitstring(root) });
        assert_eq!(unsafe { bitstring_parts(root) }.2, 32);
        assert!(unsafe { bit_at(root, 0) }); // 0xDE = 0b1101_1110
        assert!(!unsafe { bit_at(root, 2) });
    }

    #[test]
    fn windowed_payload_reads_through_the_bit_offset() {
        #[repr(align(8))]
        struct Storage([u8; 4]);
        let storage = Storage([0b1010_1010, 0b1100_0000, 0, 0]);

        let mut heap = new_heap();
        // SAFETY: 8 words reserved; a 9-bit window from bit 3.
        let root = unsafe {
            let hp = heap.alloc_words(ERL_REFC_BITS_SIZE).unwrap();
            write_refc_binary(
                hp,
                0x3000 as *mut Eterm,
                core::ptr::null_mut(),
                storage.0.as_ptr(),
                3,
                9,
            )
        };
        assert_eq!(unsafe { refc_offset(root) }, 3);
        assert_eq!(unsafe { refc_size(root) }, 9);
        // Bits 3..12 are `0101_0110_0`; `refc_bytes` spans bytes 0..2 (2 bytes).
        assert_eq!(unsafe { refc_bytes(root) }, &storage.0[..2]);
        let bits: Vec<bool> = (0..9).map(|i| unsafe { bit_at(root, i) }).collect();
        assert_eq!(
            bits,
            [false, true, false, true, false, true, true, false, false]
        );
    }

    #[test]
    fn is_refc_binary_rejects_everything_else() {
        let mut heap = new_heap();
        assert!(!unsafe { is_refc_binary(make_small(1)) });
        assert!(!unsafe { is_refc_binary(NIL) });

        // An on-heap bitstring is not a refc binary.
        // SAFETY: 3 words for a 2-byte heap binary.
        let heap_bin = unsafe {
            let hp = heap
                .alloc_words(crate::bitstring::heap_bits_size_words(16))
                .unwrap();
            crate::bitstring::write_heap_bits(hp, &[1, 2], 16).unwrap()
        };
        assert!(!unsafe { is_refc_binary(heap_bin) });

        // An `ErlSubBits` whose `orig` is not a `BinRef` is a match context, not a
        // refc binary.
        // SAFETY: 5 words for the `ErlSubBits`; `orig = NIL` is a live term.
        let sub = unsafe {
            let hp = heap.alloc_words(sub_bits_size_words()).unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(0x4000),
                Eterm::from_raw(0),
                Eterm::from_raw(8),
                NIL,
            )
        };
        assert!(!unsafe { is_refc_binary(sub) });

        // A plain tuple is not a bitstring at all.
        let tup = heap.tuple(&[make_small(1)]).unwrap();
        assert!(!unsafe { is_refc_binary(tup) });
    }
}
