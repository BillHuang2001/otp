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

//! Bitstring terms (`ErlHeapBits` / `ErlSubBits`): a **thin forwarding layer** over
//! [`beam_term::bitstring`], the lowest common ancestor that owns the frozen
//! bitstring block layout (crate-layer `CONTEXT.md` §2, "No duplication across
//! crates").
//!
//! Nothing here re-derives a word offset, a tag or a mask:
//!
//! * [`make_binary`] / [`make_bitstring`] size the block with
//!   [`beam_term::bitstring::heap_bits_size_words`] and hand it to
//!   [`beam_term::bitstring::write_heap_bits`], which writes the pinned
//!   `[thing_word, n_bits, data…]` layout (zero-filled padding + cleared trailing
//!   bits, so the term never carries the caller's garbage);
//! * [`make_refc_binary`] writes the **off-heap** representation instead: it takes the
//!   payload from an [`OffHeapBinaries`] host and hands the block — and the host's
//!   pointer — to [`beam_term::refc::write_refc_binary`], so the frozen
//!   `[BinRef][ErlSubBits]` layout is never re-derived here either;
//! * the readers ([`bitstring_parts`] / [`bitstring_size_bits`] /
//!   [`bitstring_bytes`]) validate the term and then forward to
//!   [`beam_term::bitstring::bitstring_parts`];
//! * the sizing helpers (`bit_count_to_bytes` = `NBYTES(n_bits)`,
//!   `heap_bits_size_words` = the `ErlHeapBits` block word count,
//!   `BITSTRING_POINTER_FLAG_MASK` = the `base_flags` flag mask) are **re-exported**
//!   from the owner, so every existing caller keeps its import path.
//!
//! # The guard layer (the only logic this module still owns)
//!
//! `beam_term`'s readers are `unsafe fn`s whose preconditions are *caller*
//! invariants (they do not validate), so a term of the wrong kind or a corrupt block
//! must become a typed [`beam_common::BeamError::BadArg`] here instead of reaching
//! undefined behaviour (workspace `CONTEXT.md` §3). Rejected before the LCA reader is
//! called:
//!
//! * a term that is not a bitstring at all;
//! * an `ErlHeapBits` whose header arity disagrees with its `size` word;
//! * an `ErlSubBits` whose `end` bit offset is below its `start` (the LCA reader
//!   subtracts unconditionally).
//!
//! [`make_bitstring`] additionally requires `bytes` to be **exactly**
//! `ceil(n_bits / 8)` bytes long, because `write_heap_bits` tolerates a longer slice
//! and would silently drop the extra payload. [`make_refc_binary`] guards the other
//! direction: a `bytes` slice too short for the requested bit window, an overflowing
//! window, and a host pointer that is null or misaligned (which would corrupt
//! `ErlSubBits::base_flags`) are all rejected with `BadArg` before any word is
//! written.
//!
//! # `unsafe` contract
//!
//! This module is inside the crate's single `#[allow(unsafe_code)]` subtree
//! (`mod term;` in `lib.rs`). Every `unsafe` block writes into words `beam_alloc`
//! just handed out, or reads words a live `Eterm` points at, and carries a
//! `// SAFETY:` rationale. The public functions are *safe*: a wrong kind of term
//! yields [`beam_common::BeamError::BadArg`] and a corrupt block is rejected by its
//! self-describing header, never by undefined behaviour.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::bitstring::{
    bitstring_parts as beam_term_bitstring_parts, sub_bits_parts, write_heap_bits,
};
use beam_term::prelude::{arityval, boxed_val, is_bitstring, Eterm, HEADER_SUB_BITS};
use beam_term::refc::{write_refc_binary, ERL_REFC_BITS_SIZE};

use crate::error;
use crate::off_heap::OffHeapBinaries;

/// The bit/word sizing helpers, **re-exported verbatim from the layout owner**
/// ([`beam_term::bitstring`]): [`bit_count_to_bytes`] is `NBYTES(n_bits)`
/// (`erl_bits.h:25`), [`heap_bits_size_words`] is the `ErlHeapBits` block word count
/// (`erl_bits.h:156-164`) and [`BITSTRING_POINTER_FLAG_MASK`] is the mask of the
/// `ErlSubBits::base_flags` flag bits (`erl_bits.h:95`).
pub use beam_term::bitstring::{
    bit_count_to_bytes, heap_bits_size_words, BITSTRING_POINTER_FLAG_MASK,
};

/// Materializes a **binary** (`n_bits = bytes.len() * 8`) on `heap`.
///
/// # On-heap vs off-heap
///
/// This always materializes an on-heap `ErlHeapBits` block, whatever the size. ERTS
/// instead stores a binary larger than `ERL_ONHEAP_BINARY_LIMIT` (64 bytes) as an
/// off-heap ref-counted payload behind an on-heap `BinRef`+`ErlSubBits` pair; here
/// that representation is produced by [`make_refc_binary`], which the decoder selects
/// when an [`OffHeapBinaries`] host is installed. The choice is an *interior* policy —
/// no externally pinned layout/ABI depends on it — and the observable term
/// (`is_binary/1`, `byte_size/1`, contents) is identical either way, as is the ETF
/// image: the encoder reads both representations through [`bitstring_bytes`] (see
/// `../../CONTEXT.md`, "Known Issues").
pub fn make_binary(heap: &mut Heap, bytes: &[u8]) -> Result<Eterm> {
    // A byte count of `usize::MAX / 8` could not be held in memory anyway; report it
    // rather than wrapping.
    match bytes.len().checked_mul(8) {
        Some(n_bits) => make_bitstring(heap, bytes, n_bits),
        None => Err(error::system_limit()),
    }
}

/// Materializes a bitstring of `n_bits` bits on `heap`.
///
/// `bytes` must be exactly `ceil(n_bits / 8)` bytes long — the payload of the
/// bitstring, most significant bit first inside each byte (so `<<1:3>>` is
/// `&[0b0010_0000]` with `n_bits = 3`). The unused trailing bits of the last byte
/// are forced to zero, so the term carries no garbage from the caller's buffer (the
/// C decoder copies the trailing byte verbatim; equality/`term_to_binary` ignore
/// those bits there, and zeroing them here makes our own round-trip canonical).
///
/// The block is sized with [`heap_bits_size_words`] and written by
/// [`beam_term::bitstring::write_heap_bits`], which owns the layout.
///
/// A `bytes` length that is not `ceil(n_bits / 8)` (in particular
/// `n_bits > bytes.len() * 8`) yields `BeamError::BadArg` **before** anything is
/// allocated; so does a block size that cannot be satisfied.
pub fn make_bitstring(heap: &mut Heap, bytes: &[u8], n_bits: usize) -> Result<Eterm> {
    if bytes.len() != bit_count_to_bytes(n_bits) {
        return Err(error::bad_arg());
    }
    let words = heap_bits_size_words(n_bits);
    // SAFETY: `alloc_words` returned `words` exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(words)? };
    // SAFETY: `hp` owns `heap_bits_size_words(n_bits)` words and `bytes` is exactly
    // `ceil(n_bits / 8)` bytes long — the two preconditions `write_heap_bits`
    // documents. The block stays live (and unmoved) on `heap` for as long as the
    // returned term is used.
    Ok(unsafe { write_heap_bits(hp, bytes, n_bits)? })
}

/// Materializes a bitstring whose payload lives **off-heap**: the
/// `[BinRef][ErlSubBits]` pair ERTS builds for a bitstring larger than
/// [`ERL_ONHEAP_BITS_LIMIT`](beam_term::refc::ERL_ONHEAP_BITS_LIMIT).
///
/// `host` copies `bytes` into a fresh off-heap refcounted payload and hands back the
/// value of the `BinRef.val` word (a pointer to the payload's first data byte); the
/// host keeps the payload alive (see [`OffHeapBinaries`]). The on-heap block is then
/// written by [`beam_term::refc::write_refc_binary`], the frozen layout owner — this
/// function re-derives no word offset. `offset_bits`/`size_bits` are the window into
/// the payload (the decoder uses `0`/`size_in_bits`, a full binary), and `next` — the
/// ERTS MSO off-heap **chain link** — is left null: nothing in this workspace walks
/// the chain (`beam_term::size` and `beam_gc` only size/copy the 8 on-heap words), so
/// the host's store is the ownership record, as the C's `MSO` list is not reproduced
/// here.
///
/// A `bytes` slice shorter than `ceil((offset_bits + size_bits) / 8)` bytes, an
/// `offset_bits + size_bits` overflow, a null or misaligned payload pointer from the
/// host, or a heap that cannot hold the 8-word pair yields `BeamError` — nothing is
/// left half-written. (`write_refc_binary` requires the payload base to be
/// word-aligned, because the low bits of `ErlSubBits::base_flags` are flags; a
/// non-conforming host is rejected here rather than corrupting the base pointer.)
pub fn make_refc_binary(
    heap: &mut Heap,
    host: &mut dyn OffHeapBinaries,
    bytes: &[u8],
    offset_bits: usize,
    size_bits: usize,
) -> Result<Eterm> {
    let end_bit = offset_bits
        .checked_add(size_bits)
        .ok_or_else(error::bad_arg)?;
    if bytes.len() < bit_count_to_bytes(end_bit) {
        return Err(error::bad_arg());
    }
    // The payload is the host's: it copies `bytes` and owns the result.
    let val = host.alloc_refc_payload(bytes)?;
    // The payload's first byte becomes the `ErlSubBits` base, so it must be non-null
    // and word-aligned. `beam_alloc::RefcBinary::data_ptr` guarantees both; a host
    // that violates the contract is rejected rather than allowed to set flag bits in
    // (or null out) the base pointer.
    if val.is_null() || (val as usize) & BITSTRING_POINTER_FLAG_MASK != 0 {
        return Err(error::bad_arg());
    }
    // Only once the payload exists is the on-heap pair allocated, so a failed payload
    // allocation leaves the heap untouched.
    // SAFETY: `alloc_words` returned `ERL_REFC_BITS_SIZE` exclusively-owned, writable
    // words.
    let hp = unsafe { heap.alloc_words(ERL_REFC_BITS_SIZE)? };
    // SAFETY: `hp` owns the 8 words the `[BinRef][ErlSubBits]` pair occupies — the
    // layout `write_refc_binary` documents. `val` is a non-null, word-aligned pointer
    // to the host's payload (kept alive by the host for as long as the term), which
    // the guard above checked, and `base = val` means the window starts at the
    // payload's first data byte covering the `offset_bits + size_bits` bits the
    // `bytes` length check above guarantees exist. `next = null` is a valid chain link.
    Ok(unsafe {
        write_refc_binary(
            hp,
            val,
            core::ptr::null_mut(),
            val as *const u8,
            offset_bits,
            size_bits,
        )
    })
}

/// Splits a bitstring term into `(base, offset_bits, size_bits)`.
///
/// Both bitstring representations are handled by
/// [`beam_term::bitstring::bitstring_parts`], the layout owner:
///
/// * `ErlHeapBits` — `base = boxed_val(t) + 2`, `offset_bits = 0`,
///   `size_bits` = the `size` word;
/// * `ErlSubBits` — `base = base_flags & !ERL_SUB_BITS_FLAG_MASK`,
///   `offset_bits = start`, `size_bits = end - start`.
///
/// `base` points at the **byte** containing bit 0 of the underlying storage, so bit
/// `i` of the bitstring is bit `(offset_bits + i) % 8` (MSB-first) of
/// `base[(offset_bits + i) / 8]`. A term that is not a bitstring, an `ErlHeapBits`
/// whose header arity disagrees with its `size` word, or an `ErlSubBits` with
/// `end < start` yields `BeamError::BadArg` — never a panic, never UB.
pub fn bitstring_parts(term: Eterm) -> Result<(*const u8, usize, usize)> {
    // SAFETY: `is_bitstring` reads only the header word of a boxed term (the caller
    // invariant for any `Eterm`) and never dereferences a non-boxed word.
    if !unsafe { is_bitstring(term) } {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_bitstring` established that `term` is boxed with a live, initialised
    // header word, so reading that word is sound.
    let header = unsafe { *boxed_val(term) };
    if header == HEADER_SUB_BITS {
        // `ErlSubBits`: the header value fixes the block at `ERL_SUB_BITS_SIZE` words,
        // so words 1..4 are live and initialised by the writer.
        // SAFETY: as above — the header identifies a full `ErlSubBits` block.
        let (_base_flags, start, end, _orig) = unsafe { sub_bits_parts(term) };
        // The LCA reader computes `end - start` unconditionally. A negative size is
        // impossible in a well-formed term, so reject it here rather than let the
        // subtraction wrap.
        if end.raw() < start.raw() {
            return Err(error::bad_arg());
        }
    } else {
        // `ErlHeapBits`: `[thing_word, size, data…]`. The header arity is
        // `heap_bits_size(size) - 1`; a mismatch means a corrupt/mismatched block.
        // SAFETY: the header is a live term word and word 1 is the size word of a
        // 2-word-or-longer `ErlHeapBits` block (`erl_bits.h:150-164`).
        let size_bits = unsafe { (*boxed_val(term).add(1)).raw() };
        if heap_bits_size_words(size_bits) != arityval(header) + 1 {
            return Err(error::bad_arg());
        }
    }
    // SAFETY: `term` is a live bitstring and, for either representation, its block has
    // just been validated (for a sub-bitstring, `end >= start`); the returned base
    // borrows the term's storage, so the caller keeps the owning heap (or the
    // underlying binary) alive and unmoved while it is used.
    Ok(unsafe { beam_term_bitstring_parts(term) })
}

/// The bit size of a bitstring term.
///
/// A term that is not a bitstring (or whose block is corrupt) yields
/// `BeamError::BadArg`.
pub fn bitstring_size_bits(term: Eterm) -> Result<usize> {
    Ok(bitstring_parts(term)?.2)
}

/// A safe, borrowed byte view of a bitstring term: `(bytes, offset_bits,
/// size_bits)`.
///
/// `bytes` spans `ceil((offset_bits + size_bits) / 8)` bytes starting at the
/// bitstring's base, so the `size_bits` significant bits are addressable as
/// `bytes[(offset_bits + i) / 8]` bit `(offset_bits + i) % 8` (MSB-first). The
/// borrowed slice aliases the term's storage: the caller must keep the owning heap
/// (or, for a sub-bitstring, the underlying binary) alive and unmoved for `'a`.
///
/// This is the safe counterpart of [`bitstring_parts`] used by the encoder and
/// decoder — it keeps the raw-pointer arithmetic inside this module, so
/// `encode.rs`/`decode.rs` stay safe Rust.
pub fn bitstring_bytes<'a>(term: Eterm) -> Result<(&'a [u8], usize, usize)> {
    let (base, offset_bits, size_bits) = bitstring_parts(term)?;
    let Some(end_bit) = offset_bits.checked_add(size_bits) else {
        return Err(error::bad_arg());
    };
    let n_bytes = bit_count_to_bytes(end_bit);
    // SAFETY: `bitstring_parts` established that `base` points at the first byte of the
    // storage backing a live bitstring of `offset_bits + size_bits` bits, so the
    // `ceil((offset_bits + size_bits) / 8)` bytes from there are inside that object and
    // stay live for `'a` as long as the caller keeps the owner alive (the same invariant
    // `beam_term`'s `*_parts` accessors document).
    let bytes = unsafe { core::slice::from_raw_parts(base, n_bytes) };
    Ok((bytes, offset_bits, size_bits))
}

#[cfg(test)]
mod tests {
    //! Forwarding tests: the layout itself is asserted word by word in
    //! `beam_term::bitstring`; here we pin that this layer delegates to it (the same
    //! block, the same sizing) and that every guard turns a mismatched term into
    //! `BadArg` with no panic.

    use beam_common::BeamError;
    use beam_term::prelude::{make_boxed, make_header, make_small, HEAP_BITS_SUBTAG, NIL};

    use super::*;

    /// A heap large enough for every block built here.
    fn new_heap() -> Heap {
        Heap::new(64).expect("the carrier allocator never fails for a tiny heap")
    }

    /// Asserts a layer call failed with `BeamError::BadArg`.
    fn assert_bad_arg<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    #[test]
    fn sizing_helpers_are_the_beam_term_ones() {
        for n in [0usize, 1, 7, 8, 9, 63, 64, 65, 512, 513, usize::MAX] {
            assert_eq!(
                bit_count_to_bytes(n),
                beam_term::bitstring::bit_count_to_bytes(n),
                "NBYTES({n})"
            );
            assert_eq!(
                heap_bits_size_words(n),
                beam_term::bitstring::heap_bits_size_words(n),
                "heap_bits_size({n})"
            );
        }
        assert_eq!(BITSTRING_POINTER_FLAG_MASK, 0x3);
        assert_eq!(
            heap_bits_size_words(512),
            10,
            "heap_bits_size(n) = 2 + ceil(NBYTES(n) / 8)"
        );
        // Overflow-free: an absurd size word stays finite instead of wrapping.
        assert!(heap_bits_size_words(usize::MAX) > heap_bits_size_words(512));
    }

    #[test]
    fn empty_binary_is_a_two_word_block() {
        let mut heap = new_heap();
        let term = make_binary(&mut heap, &[]).expect("the empty binary fits");
        assert_eq!(heap.used(), 2, "[thing_word, size] and no data word");

        // SAFETY: `term` is the 2-word block this test just wrote.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, make_header(1, HEAP_BITS_SUBTAG));
            assert_eq!((*p.add(1)).raw(), 0);
        }
        assert!(unsafe { is_bitstring(term) });
        assert_eq!(bitstring_size_bits(term).unwrap(), 0);
        let (bytes, off, size) = bitstring_bytes(term).unwrap();
        assert_eq!((bytes, off, size), (&[][..], 0, 0));
        assert_eq!(bitstring_parts(term).unwrap().1, 0);
    }

    #[test]
    fn binary_block_layout_is_byte_exact() {
        let mut heap = new_heap();
        let payload = [0xDEu8, 0xAD, 0xBE, 0xEF];
        let base = heap.hp();
        let term = make_binary(&mut heap, &payload).expect("fits");
        // 2 head words + ceil(4/8) = 1 data word.
        assert_eq!(heap.used(), 3);

        // SAFETY: `base` is the first word of the 3-word block just written; the payload
        // was copied little-endian into the data word at index 2.
        unsafe {
            assert_eq!(*base, make_header(2, HEAP_BITS_SUBTAG));
            assert_eq!((*base.add(1)).raw(), 32);
            let mut stored = [0u8; 4];
            core::ptr::copy_nonoverlapping(base.add(2).cast::<u8>(), stored.as_mut_ptr(), 4);
            assert_eq!(stored, payload);
        }
        assert_eq!(boxed_val(term), base, "root = make_boxed(hp)");

        let (bytes, off, size) = bitstring_bytes(term).unwrap();
        assert_eq!((bytes, off, size), (&payload[..], 0, 32));
        assert_eq!(bitstring_size_bits(term).unwrap(), 32);
    }

    #[test]
    fn bitstring_zeroes_the_unused_trailing_bits() {
        let mut heap = new_heap();
        // `<<1:3>>`: the significant bits are `001`, the rest of the byte is garbage.
        let term = make_bitstring(&mut heap, &[0b0011_1111], 3).expect("fits");
        assert_eq!(heap.used(), 3);
        let (bytes, off, size) = bitstring_bytes(term).unwrap();
        assert_eq!((bytes, off, size), (&[0b0010_0000][..], 0, 3));
    }

    #[test]
    fn make_bitstring_rejects_a_wrong_byte_count() {
        let mut heap = new_heap();
        // `n_bits > bytes.len() * 8` — the classic lying bit count.
        assert_bad_arg(make_bitstring(&mut heap, &[0], 9));
        assert_bad_arg(make_bitstring(&mut heap, &[], 1));
        // Too *many* bytes for the bit count is equally a contract violation
        // (`write_heap_bits` would silently ignore the extra payload).
        assert_bad_arg(make_bitstring(&mut heap, &[0, 0], 3));
        assert_eq!(heap.used(), 0, "a rejected bitstring allocated nothing");

        // The exact byte count is accepted at every byte boundary.
        for (n_bits, n_bytes) in [(0usize, 0usize), (1, 1), (8, 1), (9, 2), (16, 2)] {
            let bytes = vec![0u8; n_bytes];
            let mut heap = new_heap();
            let term = make_bitstring(&mut heap, &bytes, n_bits).expect("exact");
            assert_eq!(bitstring_size_bits(term).unwrap(), n_bits);
        }
    }

    #[test]
    fn sub_bitstring_is_read_through_its_base_and_offsets() {
        /// The underlying storage of `[0b1010_1010, 0b1100_0000]`, 8-byte aligned so
        /// that the low two `base_flags` bits are free for the flag mask.
        #[repr(align(8))]
        struct Storage([u8; 2]);
        let storage = Storage([0b1010_1010, 0b1100_0000]);
        let payload: &[u8] = &storage.0;
        let base = payload.as_ptr() as usize;
        // `ErlSubBits`: start = 3, end = 12 ⇒ 9 bits from bit 3, i.e. `010101011`.
        let mut words = [
            HEADER_SUB_BITS,
            Eterm::from_raw(base),
            Eterm::from_raw(3),
            Eterm::from_raw(12),
            make_small(0),
        ];
        let sub = make_boxed(words.as_mut_ptr());

        assert!(unsafe { is_bitstring(sub) });
        assert_eq!(bitstring_size_bits(sub).unwrap(), 9);
        let (bytes, off, size) = bitstring_bytes(sub).unwrap();
        assert_eq!((bytes, off, size), (payload, 3, 9));

        // The flags in the low bits of `base_flags` are masked off the pointer.
        words[1] = Eterm::from_raw(base | BITSTRING_POINTER_FLAG_MASK);
        let sub = make_boxed(words.as_mut_ptr());
        let (offset, size) = {
            let (_, off, size) = bitstring_parts(sub).unwrap();
            (off, size)
        };
        assert_eq!((offset, size), (3, 9));
        assert_eq!(bitstring_bytes(sub).unwrap().0, payload);
    }

    #[test]
    fn corrupt_and_wrong_terms_are_bad_arg() {
        let mut heap = new_heap();
        // Not a bitstring at all.
        assert_bad_arg(bitstring_parts(NIL));
        assert_bad_arg(bitstring_parts(make_small(7)));
        assert_bad_arg(bitstring_size_bits(NIL));
        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        assert_bad_arg(bitstring_parts(tuple));

        // A heap-bits header whose arity disagrees with its size word: 2 data words are
        // needed for 100 bits, but only 1 is claimed (arity 2 ⇒ 3 words).
        let mut words = [
            make_header(2, HEAP_BITS_SUBTAG),
            Eterm::from_raw(100),
            make_small(0),
        ];
        let bad = make_boxed(words.as_mut_ptr());
        assert!(unsafe { is_bitstring(bad) });
        assert_bad_arg(bitstring_parts(bad));

        // A sub-bits term with `end < start` underflows the size word.
        let storage = [0u8; 4];
        let mut sub_words = [
            HEADER_SUB_BITS,
            Eterm::from_raw(storage.as_ptr() as usize),
            Eterm::from_raw(9),
            Eterm::from_raw(4),
            make_small(0),
        ];
        let bad = make_boxed(sub_words.as_mut_ptr());
        assert_bad_arg(bitstring_parts(bad));
    }
}
