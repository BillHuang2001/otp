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

//! Bit-level term payloads: the `ErlHeapBits` / `ErlSubBits` block writers and
//! readers, plus the crate's public bit- and float-payload helpers.
//!
//! This module owns the *third* interior layout `beam_term` is responsible for: a
//! **bitstring**. A bitstring is a boxed term ([`crate::boxed`]) carrying one of two
//! headers:
//!
//! * an **on-heap bitstring** (`HEAP_BITS_SUBTAG`, the C `ErlHeapBits`) —
//!   `[thing_word, n_bits, data…]`, written by [`write_heap_bits`]; the tagged root
//!   is `make_boxed(hp)`;
//! * a **sub-bitstring** (`SUB_BITS_SUBTAG`, the C `ErlSubBits`) —
//!   `[HEADER_SUB_BITS, base_flags, start, end, orig]`, written by
//!   [`write_sub_bits`]; it is a *window* into other storage (an off-heap
//!   refcounted binary — see [`crate::refc`] — or, for a match context, an on-heap
//!   bitstring).
//!
//! Ground truth: `erts/emulator/beam/erl_bits.h:60-176` (the `ErlSubBits` /
//! `ErlHeapBits` definitions, `NBYTES`, `heap_bits_size`) and `erl_bits.c:490-519`
//! (`erl_sub_bits_init`, which fixes `start`/`end`/`base_flags`).
//!
//! # Bit order
//!
//! Bits are stored and read **most-significant-bit first inside each byte**, with no
//! shifting: byte 0 holds bits 0..8 of the string from its high bit down, so
//! `<<1:3>>` is the single byte `0b0010_0000` with `n_bits = 3`. The payload is a
//! plain byte copy into the first data word (byte 0 at the lowest address of that
//! word); the unused trailing bits of the last byte are zero-filled, so the term
//! carries no garbage from the caller's buffer and a round-trip is canonical. This
//! reproduces `beam_codec`'s oracle-verified `make_bitstring` exactly.
//!
//! # Heap-agnostic contract
//!
//! `beam_term` is a dependency *sink* and cannot allocate: the caller reserves the
//! words (production: `beam_alloc`; tests: [`crate::heap::Heap`]), sizes them with
//! [`heap_bits_size_words`] (or [`crate::refc::sub_bits_size_words`]), passes the
//! `hp: *mut Eterm` base and receives back the tagged root word.
//!
//! # The crate's single public float reader
//!
//! [`float_bits`] / [`float_value`] — the boxed-float payload readers — are defined
//! here as the crate's *only* public definition of the float layout;
//! [`crate::compare`], [`crate::hashing`] and [`crate::thing`] all forward to them
//! (the layout has exactly one owner; `crate::thing` re-exports them so
//! `beam_term::thing`'s accessor surface is unchanged).
//!
//! # `unsafe` contract
//!
//! Every reader is `unsafe`: an `Eterm` being a live, well-formed term of the
//! expected kind is a caller invariant, and reading a raw heap word has no safe
//! form. Each documents, under `# Safety`, that invariant and the lifetime of any
//! borrowed data. A writer that can detect a bad *argument* (a byte count too small
//! for the bit count) returns a typed [`TermError`] instead of writing a corrupt
//! term (workspace `CONTEXT.md` §3).

use core::mem::size_of;

use crate::boxed::{boxed_val, make_boxed, make_header, ERL_HEAP_BITS_SIZE, HEADER_SUB_BITS};
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::tag::HEAP_BITS_SUBTAG;

/// Number of bytes in one term word (`size_of::<Eterm>()`, 8 on the frozen
/// `ARCH_64` layout).
const WORD_BYTES: usize = size_of::<Eterm>();

// The frozen bitstring layout is the 64-bit `ARCH_64` ABI of the C ground truth
// (`heap_bits_size` divides the byte count by `sizeof(Eterm)`).
const _: () = assert!(WORD_BYTES == 8, "the frozen term layout assumes ARCH_64");

/// Word index `1` of an `ErlSubBits`: the `base_flags` word.
const SUB_BITS_BASE_WORD: usize = 1;
/// Word index `2` of an `ErlSubBits`: the `start` bit offset.
const SUB_BITS_START_WORD: usize = 2;
/// Word index `3` of an `ErlSubBits`: the `end` bit offset.
const SUB_BITS_END_WORD: usize = 3;
/// Word index `4` of an `ErlSubBits`: the `orig` term.
const SUB_BITS_ORIG_WORD: usize = 4;

/// `ERL_SUB_BITS_FLAG_MASK` (`erl_bits.h:95`): the low two bits of an
/// `ErlSubBits::base_flags` word hold the mutable/volatile flags, so the base
/// pointer must be masked with `!ERL_SUB_BITS_FLAG_MASK` before use
/// (`erl_sub_bits_get_base`, `erl_bits.h:124-125`).
pub const ERL_SUB_BITS_FLAG_MASK: usize = 0x3;
/// `ERL_SUB_BITS_FLAG_MUTABLE` (`erl_bits.h:91`): the `ErlSubBits` itself may be
/// modified in place (match contexts, private-append binaries).
pub const ERL_SUB_BITS_FLAG_MUTABLE: usize = 1 << 0;
/// `ERL_SUB_BITS_FLAG_VOLATILE` (`erl_bits.h:94`): the underlying data may move
/// (a writable binary), so the stored base pointer must be re-followed.
pub const ERL_SUB_BITS_FLAG_VOLATILE: usize = 1 << 1;

/// The mask to apply to an `ErlSubBits::base_flags` word to recover the data base
/// pointer. Alias of [`ERL_SUB_BITS_FLAG_MASK`] under the name the bitstring shims
/// use; the two are the same value by construction.
pub const BITSTRING_POINTER_FLAG_MASK: usize = ERL_SUB_BITS_FLAG_MASK;

/// `ceil(n / d)`, written with divisions only so it can never overflow (the
/// `(n + d - 1) / d` form wraps for a corrupt/huge `n`).
#[inline]
const fn ceil_div(n: usize, d: usize) -> usize {
    // `q * d <= n`, so the multiply cannot overflow — unlike `(n + d - 1) / d`.
    let q = n / d;
    if q * d == n {
        q
    } else {
        q + 1
    }
}

/// `NBYTES(n_bits)` (`erl_bits.h:25`): the whole bytes a bit count occupies, i.e.
/// `ceil(n_bits / 8)`. Overflow-free for every bit count.
#[inline]
pub const fn bit_count_to_bytes(n_bits: usize) -> usize {
    ceil_div(n_bits, 8)
}

/// `heap_bits_size(num_bits)` (`erl_bits.h:156-164`): the word count of the
/// `ErlHeapBits` block holding `n_bits` bits —
/// `2 + ceil(ceil(n_bits / 8) / sizeof(Eterm))` (`heap_bin_size__(NBYTES(n))`,
/// where `2` is the `[thing_word, size]` head).
///
/// Overflow-free for every bit count, so a corrupt `size` word is rejected by the
/// header-arity check in [`bitstring_parts`]-style callers rather than wrapping.
#[inline]
pub const fn heap_bits_size_words(n_bits: usize) -> usize {
    ERL_HEAP_BITS_SIZE + ceil_div(bit_count_to_bytes(n_bits), WORD_BYTES)
}

// ---------------------------------------------------------------------------
// Writers
// ---------------------------------------------------------------------------

/// Writes an on-heap bitstring (`ErlHeapBits`) at `hp` and returns the tagged root.
///
/// Layout pinned to `erl_bits.h:150-164`: `[thing_word, n_bits, data…]`, where
/// `thing_word = make_header(heap_bits_size(n_bits) - 1, HEAP_BITS_SUBTAG)`, the
/// size word holds the **bit** count, and the root is `make_bitstring(hp)` ==
/// `make_boxed(hp)`. The `n_bytes = ceil(n_bits / 8)` payload bytes are copied in
/// with byte 0 at the lowest address of the first data word, the trailing padding
/// bytes of the last data word are zeroed, and the unused trailing bits of the last
/// byte are cleared (so the term never carries the caller's garbage).
///
/// A `bytes` slice shorter than `ceil(n_bits / 8)` yields
/// [`TermError::BadBitstring`] and writes nothing; extra bytes past
/// `ceil(n_bits / 8)` are ignored (callers should pass exactly that many).
///
/// # Safety
///
/// `hp` must point at `heap_bits_size_words(n_bits)` exclusively-owned, writable
/// words, which stay live (and unmoved) for as long as the returned term is used.
pub unsafe fn write_heap_bits(hp: *mut Eterm, bytes: &[u8], n_bits: usize) -> TermResult<Eterm> {
    let n_bytes = bit_count_to_bytes(n_bits);
    if bytes.len() < n_bytes {
        return Err(TermError::BadBitstring);
    }
    let words = heap_bits_size_words(n_bits);
    // SAFETY: the caller reserved `words = 2 + ceil(n_bytes / WORD_BYTES)` words: word
    // 0 receives the header, word 1 the bit size and words `2..` the data — exactly the
    // `ErlHeapBits` layout (`erl_bits.h:150-164`), so the header arity is `words - 1`
    // and the root is `make_bitstring(hp) == make_boxed(hp)`. The data words are zeroed
    // first (initialising the trailing padding bytes), then the `n_bytes` payload bytes
    // are copied in.
    unsafe {
        *hp = make_header(words - 1, HEAP_BITS_SUBTAG);
        *hp.add(1) = Eterm::from_raw(n_bits);
        let data = hp.add(ERL_HEAP_BITS_SIZE).cast::<u8>();
        let data_bytes = (words - ERL_HEAP_BITS_SIZE) * WORD_BYTES;
        core::ptr::write_bytes(data, 0, data_bytes);
        if n_bytes > 0 {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), data, n_bytes);
        }
        let tail = n_bits % 8;
        if tail != 0 {
            // Clear the unused low bits of the last payload byte (MSB-first order).
            *data.add(n_bytes - 1) &= 0xFFu8 << (8 - tail);
        }
    }
    Ok(make_boxed(hp))
}

/// Writes a **binary** (a bitstring of `bytes.len() * 8` bits) at `hp`.
///
/// A thin wrapper over [`write_heap_bits`], matching the C `HEAP_BITSTRING` /
/// `make_binary` path. A byte count whose bit count overflows `usize` yields
/// [`TermError::BadBitstring`] rather than wrapping.
///
/// # Safety
///
/// As [`write_heap_bits`], with `n_bits = bytes.len() * 8`.
pub unsafe fn write_binary(hp: *mut Eterm, bytes: &[u8]) -> TermResult<Eterm> {
    match bytes.len().checked_mul(8) {
        // SAFETY: the caller reserved `heap_bits_size_words(bytes.len() * 8)` words.
        Some(n_bits) => unsafe { write_heap_bits(hp, bytes, n_bits) },
        // A byte count of more than `usize::MAX / 8` bits is unrepresentable.
        None => Err(TermError::BadBitstring),
    }
}

/// Writes an `ErlSubBits` block at `hp` and returns the tagged root.
///
/// Layout pinned to `erl_bits.h:60-82,130`:
/// `[HEADER_SUB_BITS, base_flags, start, end, orig]` (5 words), with the root
/// `make_bitstring(hp)` == `make_boxed(hp)`. `base_flags` packs the data base
/// pointer with the low [`ERL_SUB_BITS_FLAG_MASK`] bits used as flags (see
/// [`ERL_SUB_BITS_FLAG_MUTABLE`] / [`ERL_SUB_BITS_FLAG_VOLATILE`]); `start`/`end`
/// are **bit** offsets into that base and the size is `end - start`; `orig` is the
/// originating term (a `BinRef` for a refc binary, or an on-heap bitstring for a
/// match context).
///
/// # Safety
///
/// `hp` must point at `crate::refc::sub_bits_size_words()` (5) exclusively-owned,
/// writable words, which stay live (and unmoved) for as long as the returned term is
/// used. `base_flags` must point at storage covering bits `start..end`.
pub unsafe fn write_sub_bits(
    hp: *mut Eterm,
    base_flags: Eterm,
    start: Eterm,
    end: Eterm,
    orig: Eterm,
) -> Eterm {
    // SAFETY: the caller reserved the 5-word `ErlSubBits` block at `hp`; every word is
    // written before the pointer escapes.
    unsafe {
        *hp = HEADER_SUB_BITS;
        *hp.add(SUB_BITS_BASE_WORD) = base_flags;
        *hp.add(SUB_BITS_START_WORD) = start;
        *hp.add(SUB_BITS_END_WORD) = end;
        *hp.add(SUB_BITS_ORIG_WORD) = orig;
    }
    make_boxed(hp)
}

// ---------------------------------------------------------------------------
// Readers
// ---------------------------------------------------------------------------

/// Splits an `ErlSubBits` into `(base_flags, start, end, orig)`.
///
/// The exact inverse of [`write_sub_bits`] (`erl_bits.h:60-82`). `start`/`end` are
/// **bit** offsets; the size is `end - start`. `base_flags` still carries the flag
/// bits — mask with [`ERL_SUB_BITS_FLAG_MASK`] before using it as a pointer.
///
/// # Safety
///
/// `t` must be a live `ErlSubBits` term (a bitstring whose header is
/// `HEADER_SUB_BITS`), kept alive by the caller.
pub unsafe fn sub_bits_parts(t: Eterm) -> (Eterm, Eterm, Eterm, Eterm) {
    let p = boxed_val(t);
    // SAFETY: the caller guarantees `t` is a live `ErlSubBits`, so words 1..4 are
    // initialised by [`write_sub_bits`].
    unsafe {
        (
            *p.add(SUB_BITS_BASE_WORD),
            *p.add(SUB_BITS_START_WORD),
            *p.add(SUB_BITS_END_WORD),
            *p.add(SUB_BITS_ORIG_WORD),
        )
    }
}

/// Decomposes a bitstring into `(data base byte pointer, start bit offset, bit size)`.
///
/// Both representations are handled (`erl_bits.h:53-56`, `ERTS_GET_BITSTRING_REF`):
///
/// * `ErlHeapBits` — base `= boxed_val(t) + 2`, offset `0`, size = the `size` word;
/// * `ErlSubBits` — base `= base_flags & !ERL_SUB_BITS_FLAG_MASK`, offset `= start`,
///   size `= end - start`.
///
/// The base points at the **byte** containing bit 0 of the underlying storage, so
/// bit `i` of the bitstring is bit `(offset + i) % 8` (MSB-first) of
/// `base[(offset + i) / 8]`.
///
/// # Safety
///
/// `t` must be a live bitstring (on-heap or sub) term. For a sub-bitstring the base
/// pointer must still be valid (not a volatile writable binary that has moved). The
/// returned base pointer borrows the term's storage, so the caller must keep the
/// owning heap (or the underlying binary) alive and unmoved while it is used.
pub unsafe fn bitstring_parts(t: Eterm) -> (*const u8, usize, usize) {
    let p = boxed_val(t);
    // SAFETY: `t` is a live boxed bitstring, so its header word is initialised.
    let hdr = unsafe { *p };
    if hdr == HEADER_SUB_BITS {
        // SAFETY: the header fixes the block at the 5-word `ErlSubBits` layout, so
        // words 1..3 are live (`erl_bits.h:60-82`).
        let base_flags = unsafe { (*p.add(SUB_BITS_BASE_WORD)).raw() };
        let start = unsafe { (*p.add(SUB_BITS_START_WORD)).raw() };
        let end = unsafe { (*p.add(SUB_BITS_END_WORD)).raw() };
        (
            (base_flags & !ERL_SUB_BITS_FLAG_MASK) as *const u8,
            start,
            end - start,
        )
    } else {
        // SAFETY: an `ErlHeapBits` is `[thing_word, size, data…]`, so word 1 is the bit
        // size and word 2 the first payload byte (`erl_bits.h:150-164`).
        let size = unsafe { (*p.add(1)).raw() };
        (unsafe { p.add(ERL_HEAP_BITS_SIZE) }.cast::<u8>(), 0, size)
    }
}

/// Reads bit `i` (0-based, most-significant bit of the first byte first) of a
/// bitstring term.
///
/// Reproduces the bit test of `erts_cmp_bits` / `erts_bs_get_*` (`erl_bits.c`):
/// `(base[(offset + i) / 8] >> (7 - ((offset + i) % 8))) & 1`.
///
/// # Safety
///
/// `t` must be a live bitstring term and `i` must be less than its bit size. For a
/// sub-bitstring the base storage must still be valid.
pub unsafe fn bit_at(t: Eterm, i: usize) -> bool {
    // SAFETY: `t` is a live bitstring; bit `i` is within its size by the caller.
    let (base, offset, _size) = unsafe { bitstring_parts(t) };
    unsafe { raw_bit(base, offset + i) != 0 }
}

/// Read bit `bit` (0 = most significant bit of the base byte) of a bitstring base.
///
/// # Safety
///
/// `base` must point at bit data covering bit `bit`.
#[inline]
unsafe fn raw_bit(base: *const u8, bit: usize) -> u8 {
    // SAFETY: the caller guarantees the byte is in range.
    unsafe { (*base.add(bit >> 3) >> (7 - (bit & 7))) & 1 }
}

/// The raw `f64` bits stored in a boxed float term.
///
/// The 64-bit flonum is a 1-word thing (`erl_term.h:479`, `HEADER_FLONUM`), so the
/// value word is `boxed_val(t)[1]`. This is the crate's single public definition of
/// the float payload read; [`crate::compare`], [`crate::hashing`] and
/// [`crate::thing`] forward to it.
///
/// # Safety
///
/// `t` must be a live boxed float (`is_float(t)`).
pub unsafe fn float_bits(t: Eterm) -> u64 {
    // SAFETY: the caller guarantees `t` is a live boxed float, so its block is the
    // 2-word `[HEADER_FLONUM, bits]` and word 1 is initialised.
    unsafe { (*boxed_val(t).add(1)).raw() as u64 }
}

/// The `f64` value stored in a boxed float term.
///
/// The exact inverse of [`crate::thing::write_float`], delegating to [`float_bits`].
///
/// # Safety
///
/// As [`float_bits`]: `t` must be a live boxed float.
pub unsafe fn float_value(t: Eterm) -> f64 {
    // SAFETY: the caller guarantees `t` is a live boxed float.
    f64::from_bits(unsafe { float_bits(t) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{arityval, bitstring_size, is_binary, is_bitstring};
    use crate::heap::Heap;
    use crate::immediate::{make_small, NIL};
    use crate::thing::write_float;

    fn new_heap() -> Heap {
        Heap::new(256)
    }

    #[test]
    fn size_helpers_hit_the_boundaries() {
        for (n, b) in [
            (0usize, 0usize),
            (1, 1),
            (7, 1),
            (8, 1),
            (9, 2),
            (16, 2),
            (63, 8),
            (64, 8),
            (65, 9),
            (512, 64),
        ] {
            assert_eq!(bit_count_to_bytes(n), b, "NBYTES({n})");
        }
        // `heap_bits_size(n) = 2 + ceil(NBYTES(n) / 8)`.
        for (n, w) in [
            (0usize, 2usize),
            (1, 3),
            (8, 3),
            (63, 3),
            (64, 3),
            (65, 4),
            (512, 10),
            (513, 11),
        ] {
            assert_eq!(heap_bits_size_words(n), w, "heap_bits_size({n})");
        }
        // Overflow-free: the largest bit count must not wrap the byte/word math.
        assert!(bit_count_to_bytes(usize::MAX) < usize::MAX);
        assert!(heap_bits_size_words(usize::MAX) > 2);
    }

    #[test]
    fn heap_bits_block_layout_is_pinned() {
        let mut heap = new_heap();
        // 16 bits => 2 payload bytes => 1 data word => 3 words total.
        let words = heap_bits_size_words(16);
        assert_eq!(words, 3);
        // SAFETY: `alloc_words` returns `words` exclusively-owned, writable words.
        let hp = unsafe { heap.alloc_words(words).unwrap() };
        // SAFETY: `hp` owns `words` words.
        let term = unsafe { write_heap_bits(hp, &[0xAB, 0xCD], 16).unwrap() };
        assert_eq!(heap.used(), 3);
        // SAFETY: `term` is the bitstring this test just built; its 3 words are live.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(p, hp, "the root points at the block base");
            assert_eq!(*p, make_header(words - 1, HEAP_BITS_SUBTAG));
            assert_eq!(arityval(*p), words - 1);
            assert_eq!(*p.add(1), Eterm::from_raw(16)); // the bit size word
            assert_eq!(bitstring_size(term), 16);
            assert!(is_bitstring(term));
            assert!(is_binary(term), "16 bits is byte aligned");
        }
        let (base, off, size) = unsafe { bitstring_parts(term) };
        assert_eq!((off, size), (0, 16));
        // SAFETY: `base` covers the 2 payload bytes just written.
        let payload = unsafe { core::slice::from_raw_parts(base, 2) };
        assert_eq!(payload, &[0xAB, 0xCD]);
    }

    #[test]
    fn non_byte_aligned_bits_round_trip() {
        // `<<1:3>>` is the single byte `0b0010_0000`, MSB-first.
        let mut heap = new_heap();
        // SAFETY: 1 payload byte fits in the reserved 3 words.
        let term = unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(3)).unwrap();
            write_heap_bits(hp, &[0b0010_0000], 3).unwrap()
        };
        assert_eq!(unsafe { bitstring_parts(term) }.2, 3);
        assert!(!unsafe { bit_at(term, 0) });
        assert!(!unsafe { bit_at(term, 1) });
        assert!(unsafe { bit_at(term, 2) });
        assert!(!unsafe { is_binary(term) }, "3 bits is not byte aligned");

        // Every bit length 1..=64 round-trips a known pattern.
        for n_bits in 1..=64usize {
            heap.reset();
            let n_bytes = bit_count_to_bytes(n_bits);
            // A pattern that exercises both a set and a clear bit in every byte.
            let bytes: Vec<u8> = (0..n_bytes).map(|i| (!(i as u8) & 0x55) | 0x80).collect();
            // SAFETY: `heap_hp` owns `heap_bits_size_words(n_bits)` words.
            let term = unsafe {
                let hp = heap.alloc_words(heap_bits_size_words(n_bits)).unwrap();
                write_heap_bits(hp, &bytes, n_bits).unwrap()
            };
            let (base, off, size) = unsafe { bitstring_parts(term) };
            assert_eq!((off, size), (0, n_bits), "n_bits={n_bits}");
            // SAFETY: `base` covers `ceil(n_bits / 8)` bytes.
            let stored = unsafe { core::slice::from_raw_parts(base, n_bytes) };
            for i in 0..n_bits {
                let want = (bytes[i >> 3] >> (7 - (i & 7))) & 1 == 1;
                assert_eq!(unsafe { bit_at(term, i) }, want, "n_bits={n_bits} bit={i}");
            }
            // The trailing bits of the last byte are zeroed.
            let tail = n_bits % 8;
            if tail != 0 {
                assert_eq!(
                    stored[n_bytes - 1] & (0xFFu8 >> tail),
                    0,
                    "n_bits={n_bits}: trailing bits must be zero"
                );
            }
        }
    }

    #[test]
    fn write_heap_bits_rejects_a_short_byte_count() {
        let mut heap = new_heap();
        // SAFETY: no word is written on the error paths, so a small reservation is fine.
        unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(512)).unwrap();
            assert_eq!(write_heap_bits(hp, &[0], 9), Err(TermError::BadBitstring));
            assert_eq!(write_heap_bits(hp, &[], 1), Err(TermError::BadBitstring));
            // Exactly enough is accepted.
            assert!(write_heap_bits(hp, &[0], 8).is_ok());
            // Extra bytes are ignored (only `ceil(n_bits / 8)` are copied).
            assert!(write_heap_bits(hp, &[0, 0], 3).is_ok());
        }
    }

    #[test]
    fn write_binary_matches_the_bit_count() {
        let mut heap = new_heap();
        // SAFETY: the reservation matches `heap_bits_size_words(len * 8)`.
        let term = unsafe {
            let hp = heap.alloc_words(heap_bits_size_words(4 * 8)).unwrap();
            write_binary(hp, &[1, 2, 3, 4]).unwrap()
        };
        assert_eq!(unsafe { bitstring_parts(term) }.2, 32);
        assert!(unsafe { is_binary(term) });
        // SAFETY: the base covers the 4 payload bytes.
        let payload = unsafe { core::slice::from_raw_parts(bitstring_parts(term).0, 4) };
        assert_eq!(payload, &[1, 2, 3, 4]);
    }

    #[test]
    fn sub_bits_block_layout_is_pinned() {
        // A byte-aligned base so the low flag bits are free.
        #[repr(align(8))]
        struct Storage([u8; 2]);
        let storage = Storage([0b1010_1010, 0b1100_0000]);
        let base = storage.0.as_ptr() as usize;

        let mut heap = new_heap();
        // SAFETY: 5 words reserved for the `ErlSubBits`.
        let term = unsafe {
            let hp = heap
                .alloc_words(crate::refc::sub_bits_size_words())
                .unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(base),
                Eterm::from_raw(3),
                Eterm::from_raw(12),
                make_small(0),
            )
        };
        // SAFETY: `term` is the `ErlSubBits` just written.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, HEADER_SUB_BITS);
            assert_eq!((*p.add(1)).raw() as *const u8, storage.0.as_ptr());
            assert_eq!((*p.add(2)).raw(), 3);
            assert_eq!((*p.add(3)).raw(), 12);
            assert_eq!(*p.add(4), make_small(0));

            let (base_flags, start, end, orig) = sub_bits_parts(term);
            assert_eq!(base_flags.raw() as *const u8, storage.0.as_ptr());
            assert_eq!((start.raw(), end.raw(), orig), (3, 12, make_small(0)));

            let (pbase, off, size) = bitstring_parts(term);
            assert_eq!(pbase, storage.0.as_ptr());
            assert_eq!((off, size), (3, 9));
            // Bits 3..12 of `0b1010_1010_1100_0000` are `0101_0110_0`.
            let bits: Vec<bool> = (0..9).map(|i| bit_at(term, i)).collect();
            assert_eq!(
                bits,
                [false, true, false, true, false, true, true, false, false]
            );
        }

        // Flag bits in `base_flags` are masked off the base pointer.
        let mut heap = new_heap();
        // SAFETY: 5 words reserved; the flags do not change the layout.
        let term = unsafe {
            let hp = heap
                .alloc_words(crate::refc::sub_bits_size_words())
                .unwrap();
            write_sub_bits(
                hp,
                Eterm::from_raw(base | ERL_SUB_BITS_FLAG_VOLATILE),
                Eterm::from_raw(0),
                Eterm::from_raw(8),
                NIL,
            )
        };
        assert_eq!(unsafe { bitstring_parts(term) }.0, storage.0.as_ptr());
    }

    #[test]
    fn float_readers_wrap_the_thing_layout() {
        let mut heap = new_heap();
        for value in [
            0.0f64,
            -0.0,
            1.5,
            f64::NAN,
            f64::INFINITY,
            f64::MIN,
            f64::MAX,
        ] {
            heap.reset();
            // SAFETY: 2 words reserved for the `[HEADER_FLONUM, bits]` block.
            let term = unsafe {
                let hp = heap.alloc_words(crate::thing::FLOAT_SIZE_WORDS).unwrap();
                write_float(hp, value)
            };
            // Bit-pattern comparison so NaN and -0.0 round-trip.
            assert_eq!(unsafe { float_bits(term) }, value.to_bits());
            assert_eq!(unsafe { float_value(term) }.to_bits(), value.to_bits());
        }
    }
}
