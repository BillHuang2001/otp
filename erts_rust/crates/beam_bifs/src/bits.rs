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

//! Heap bitstring/binary helpers shared by the binary and unicode BIFs.
//!
//! This module owns the *safe* API for building and reading **on-heap
//! bitstrings** (`ErlHeapBits`) — the frozen `[thing_word, size, data…]` block
//! (`beam_term::boxed::ERL_HEAP_BITS_SIZE`, `../../erts/emulator/beam/erl_bits.h`
//! `header_heap_bits`). The raw word access lives in [`crate::access`] (the crate's
//! only `#[allow(unsafe_code)]` module); this module layers the byte/bit-level
//! semantics on top and is otherwise safe Rust.
//!
//! Interior layout (owned by `beam_term`, reproduced here):
//! `word 0 = make_header(1 + data_words, HEAP_BITS_SUBTAG)`, `word 1 = bit size`,
//! `words 2.. = the bit string`, packed MSB-first starting at the low byte of
//! `word 2`. For a byte-aligned binary the bytes are stored in order, so a binary
//! of `n` bytes occupies `2 + ceil(n / 8)` words.
//!
//! # M1 scope and deviations
//!
//! * **Every binary this module *builds* stays on the heap.** C keeps a binary of
//!   more than `ERL_ONHEAP_BINARY_LIMIT` (64) bytes off-heap behind a `BinRef`, and
//!   [`make_bitstring`] follows C's *layout* byte-for-byte (the on-heap block) while
//!   deliberately ignoring the 64-byte *placement* rule — the heap is an arena that
//!   never moves, so the limit buys nothing here. This is a deviation from C's
//!   placement policy, not from its layout.
//! * **Readers accept *both* representations.** The engine's own
//!   `bs_create_bin`/`bs_init_writable` path (`beam_engine`'s
//!   `families/bitstrings/raw.rs::make_binary`) DOES build the off-heap
//!   `[BinRef][ErlSubBits]` refc binary for any payload above
//!   `ERL_ONHEAP_BINARY_LIMIT` (64) bytes, so [`bytes`] follows the frozen
//!   `beam_term::bitstring::bitstring_parts` indirection and reads such a term
//!   correctly (a byte-aligned window is the contiguous slice at
//!   `base + offset_bits / 8`; a mid-byte window is re-packed bit by bit).
//! * The consumers are the `binary`/`unicode` BIFs plus `erlang:size/1`
//!   (`crate::term_ops`), which call [`make_binary`]/[`make_bitstring`]/
//!   [`byte_size`]/[`bytes`]. A few helpers still carry `#[allow(dead_code)]` for
//!   callers that are not wired up yet.

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_term::Eterm;

use crate::access;

/// Builds the on-heap binary `<<bytes/binary>>` on `heap`.
///
/// Equivalent to [`make_bitstring`] with a bit length of `bytes.len() * 8`.
///
/// # Errors
///
/// Propagates [`BeamError::Alloc`](beam_common::BeamError::Alloc) when `heap` cannot
/// fit the block.
#[allow(dead_code)]
pub(crate) fn make_binary(heap: &mut Heap, bytes: &[u8]) -> Result<Eterm> {
    // `saturating_mul` is exact for every slice that can exist (`len` would have to
    // exceed `usize::MAX / 8` to saturate), and a saturated bit count still passes
    // the `bit_len <= bytes.len() * 8` check in `make_bitstring`.
    make_bitstring(heap, bytes, bytes.len().saturating_mul(8))
}

/// Builds the on-heap bitstring `<<bytes:bit_len>>` on `heap`.
///
/// `bytes` supplies the bits, MSB-first: the first `bit_len` bits of `bytes` become
/// the bitstring's bits, and the caller is expected to have zeroed the unused bits
/// of the last used byte. The unused bytes of the final data word are zeroed here,
/// so the result is deterministic.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `bit_len > bytes.len() * 8`; otherwise propagates
/// [`BeamError::Alloc`](beam_common::BeamError::Alloc) when `heap` cannot fit the
/// block.
#[allow(dead_code)]
pub(crate) fn make_bitstring(heap: &mut Heap, bytes: &[u8], bit_len: usize) -> Result<Eterm> {
    access::make_heap_bitstring(heap, bytes, bit_len)
}

/// `is_binary(term)` — a bitstring whose size is a whole number of bytes.
#[allow(dead_code)]
#[inline]
pub(crate) fn is_binary(term: Eterm) -> bool {
    access::is_binary(term)
}

/// `is_bitstring(term)` — a heap or sub bitstring of any bit size.
#[inline]
pub(crate) fn is_bitstring(term: Eterm) -> bool {
    access::is_bitstring(term)
}

/// The size of `term` in **bits**.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a bitstring.
pub(crate) fn bit_size(term: Eterm) -> Result<usize> {
    access::bitstring_bit_size(term)
}

/// The size of `term` in **bytes** (the `byte_size/1` BIF's value).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a byte-aligned bitstring — exactly the
/// oracle's behaviour (`byte_size(<<1:1>>)` is `badarg`).
#[allow(dead_code)]
pub(crate) fn byte_size(term: Eterm) -> Result<usize> {
    if !is_binary(term) {
        return Err(BeamError::BadArg);
    }
    Ok(access::bitstring_bit_size(term)? / 8)
}

/// Copies the bytes of a binary out of the term.
///
/// Handles **both** frozen bitstring representations: an on-heap `ErlHeapBits` and
/// an off-heap `ErlSubBits` window (the `[BinRef][ErlSubBits]` refc binary the
/// engine builds through `bs_create_bin` above `ERL_ONHEAP_BINARY_LIMIT` bytes).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a byte-aligned bitstring.
#[allow(dead_code)]
pub(crate) fn bytes(term: Eterm) -> Result<Vec<u8>> {
    access::binary_bytes(term)
}

/// Hands the bytes of a binary to `body` **without copying them out of the term**.
///
/// The borrowing form of [`bytes`]: the slice is lent to `body` for the duration of
/// the call (a closure scope, since a returned `&[u8]` is not expressible — see
/// [`crate::access::with_binary_bytes`]), which is what lets a caller append a binary
/// element straight into its destination instead of through a per-element temporary.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a byte-aligned binary; otherwise whatever
/// `body` returns.
pub(crate) fn with_bytes<R>(term: Eterm, body: impl FnOnce(&[u8]) -> Result<R>) -> Result<R> {
    access::with_binary_bytes(term, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_term::{make_small, Eterm, NIL};

    fn heap() -> Heap {
        Heap::new(256).expect("test heap")
    }

    /// `n` bytes with a recognisable pattern (so a byte swap or an off-by-one cannot
    /// pass unnoticed).
    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(7).wrapping_add(3)).collect()
    }

    #[test]
    fn binaries_round_trip_through_their_size_and_bytes() {
        let mut heap = heap();
        // The byte counts around the C on-heap limit (64) and the word boundaries.
        for n in [0usize, 1, 2, 7, 8, 9, 63, 64, 65, 128] {
            let data = pattern(n);
            let bin = make_binary(&mut heap, &data).expect("the binary fits");
            assert!(is_binary(bin), "{n}-byte binary must be a binary");
            assert!(is_bitstring(bin), "{n}-byte binary must be a bitstring");
            assert_eq!(bit_size(bin).unwrap(), n * 8, "bit size of a {n}-byte binary");
            assert_eq!(byte_size(bin).unwrap(), n, "byte size of a {n}-byte binary");
            assert_eq!(bytes(bin).unwrap(), data, "bytes of a {n}-byte binary");
        }
        // Omitting the byte count must not disturb an earlier binary in the heap.
        let first = make_binary(&mut heap, &pattern(65)).expect("fits");
        let second = make_binary(&mut heap, &[0xAB, 0xCD]).expect("fits");
        assert_eq!(bytes(first).unwrap(), pattern(65));
        assert_eq!(bytes(second).unwrap(), vec![0xAB, 0xCD]);
    }

    #[test]
    fn bitstrings_round_trip_through_their_bit_size() {
        let mut heap = heap();
        for bit_len in 1..=64usize {
            let n = bit_len.div_ceil(8);
            // A value whose bits are all set, so a wrong bit size cannot hide.
            let data = vec![0xFFu8; n];
            let bits = make_bitstring(&mut heap, &data, bit_len).expect("the bitstring fits");
            assert!(is_bitstring(bits), "{bit_len}-bit value must be a bitstring");
            assert_eq!(bit_size(bits).unwrap(), bit_len, "bit size of a {bit_len}-bit value");
            if bit_len % 8 == 0 {
                assert!(is_binary(bits), "a {bit_len}-bit value is byte aligned");
                assert_eq!(byte_size(bits).unwrap(), bit_len / 8);
                assert_eq!(bytes(bits).unwrap(), data);
            } else {
                assert!(!is_binary(bits), "a {bit_len}-bit value is not byte aligned");
                assert!(matches!(byte_size(bits), Err(BeamError::BadArg)));
                assert!(matches!(bytes(bits), Err(BeamError::BadArg)));
            }
        }
    }

    #[test]
    fn bitstrings_pack_their_bits_msb_first() {
        let mut heap = heap();
        // `<<1:1>>`: bit 0 is the MSB of the first byte.
        let one_bit = make_bitstring(&mut heap, &[0b1000_0000], 1).expect("fits");
        assert_eq!(bit_size(one_bit).unwrap(), 1);
        // `<<0,255:8>>` / `<<1:9>>`: the first data byte holds the first 8 bits.
        let nine = make_bitstring(&mut heap, &[0x00, 0b1000_0000], 9).expect("fits");
        assert_eq!(bit_size(nine).unwrap(), 9);
        assert!(matches!(byte_size(nine), Err(BeamError::BadArg)));
        // `<<1:8>>` is the same bits as `<<1>>`.
        let byte_aligned = make_bitstring(&mut heap, &[0x01], 8).expect("fits");
        assert_eq!(bytes(byte_aligned).unwrap(), vec![0x01]);
    }

    #[test]
    fn make_bitstring_rejects_a_bit_length_beyond_the_source() {
        let mut heap = heap();
        assert!(matches!(make_bitstring(&mut heap, &[0, 0], 17), Err(BeamError::BadArg)));
        assert!(matches!(make_bitstring(&mut heap, &[], 1), Err(BeamError::BadArg)));
        assert_eq!(heap.used(), 0, "a rejected bit length allocates nothing");
        // The exact bound is accepted.
        assert!(make_bitstring(&mut heap, &[0, 0], 16).is_ok());
    }

    #[test]
    fn classification_and_accessors_reject_non_bitstrings() {
        let mut heap = heap();
        let tuple = heap.tuple(&[make_small(1)]).expect("fits");
        let list = heap.cons(make_small(1), NIL).expect("fits");
        for term in [make_small(7), NIL, tuple, list] {
            assert!(!is_binary(term));
            assert!(!is_bitstring(term));
            assert!(matches!(bit_size(term), Err(BeamError::BadArg)));
            assert!(matches!(byte_size(term), Err(BeamError::BadArg)));
            assert!(matches!(bytes(term), Err(BeamError::BadArg)));
        }
        // An empty binary is a binary of size zero.
        let empty: Eterm = make_binary(&mut heap, &[]).expect("fits");
        assert!(is_binary(empty));
        assert_eq!(bit_size(empty).unwrap(), 0);
        assert_eq!(byte_size(empty).unwrap(), 0);
        assert!(bytes(empty).unwrap().is_empty());
    }
}
