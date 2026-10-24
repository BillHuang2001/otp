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

//! Bit-level payload comparison — `erts_cmp_bits` (`erts/emulator/beam/erl_bits.h:559`)
//! and its out-of-line body `erts_cmp_bits__` (`erl_bits.c:2293`).
//!
//! Split out of [`super`] because this is the one comparison arm whose *cost* is a
//! function of the payload length, so its shape (whole bytes first, then the tail
//! bits) is worth keeping under its own roof — and out of a file that is already at
//! the crate's ~1000-line budget.
//!
//! # One order, three ways to walk it
//!
//! A bitstring's bit sequence is **the bytes in order, and inside each byte the bits
//! MSB first** ([`crate::bitstring::bit_at`]). Ordering two bitstrings is the
//! lexicographic order of that sequence over the `min(a_size, b_size)` compared bits
//! (`erts_cmp_bits`, `utils.c:2322-2335` compares `MIN(a_size, b_size)` bits, then the
//! two sizes). That one order is computed here three ways, chosen by alignment:
//!
//! * **both bases byte-aligned** — the whole `n / 8` bytes are compared as one
//!   `&[u8]` pair (which lowers to `memcmp`), then the `n % 8` tail bits from the top
//!   of the next byte. Byte-lexicographic order *is* bit-lexicographic order over a
//!   whole-byte prefix (an unsigned byte order is its 8 MSB-first bits interpreted as
//!   a number), and the tail bits are compared exactly as the bit loop would;
//! * **byte-aligned but with a partial tail only** (`n < 8`) — the shift/mask
//!   comparison of the `n` top bits of the single byte, no loop;
//! * **either base bit-offset not byte-aligned** (only reachable through a
//!   sub-bitstring: `ErlSubBits.start` is a *bit* offset) — the plain bit loop, i.e.
//!   exactly the C's `erts_cmp_bits__` result, computed one bit at a time.
//!
//! All three are the same relation, so which one runs can never change a verdict —
//! the tail comparison is `(byte >> (8 - tail)).cmp(..)` rather than a raw byte
//! compare precisely so that bits **past** `n` (zeroed by every writer in this crate,
//! but not guaranteed for a forged block) cannot influence the result, matching the
//! bit loop that ignores them.
//!
//! Only the aligned forms are new: the bit loop was the whole of this arm before, and
//! keeping it as the unaligned fallback means the sub-bitstring path is unchanged.

use core::cmp::Ordering;

/// Compare the first `n_bits` bits of two bitstring bases —
/// `erts_cmp_bits(a_base, a_off, b_base, b_off, n_bits)`.
///
/// A base points at the **first byte** of the bitstring data; the offsets are **bit**
/// offsets into that base (the `ErlSubBits.start`/`ErlHeapBits` forms
/// [`crate::bitstring::bitstring_parts`] hands back — `0` for a heap bitstring).
///
/// # Safety
///
/// Both `a_base`/`b_base` must point at readable bitstring data covering `n_bits` bits
/// from the respective offset, as `bitstring_parts` guarantees for a live bitstring
/// term. A `n_bits` of `0` reads nothing.
pub(super) unsafe fn cmp_bits(
    a_base: *const u8,
    a_off: usize,
    b_base: *const u8,
    b_off: usize,
    n_bits: usize,
) -> Ordering {
    if n_bits == 0 {
        return Ordering::Equal;
    }
    let (a_bit, b_bit) = (a_off & 7, b_off & 7);
    if a_bit == 0 && b_bit == 0 {
        // SAFETY: the caller guarantees both bases cover `n_bits` bits, so the
        // `n_bits / 8` whole bytes starting at each (byte-aligned) offset are readable
        // and in bounds of their allocations.
        unsafe { cmp_bits_aligned(a_base.add(a_off >> 3), b_base.add(b_off >> 3), n_bits) }
    } else {
        // SAFETY: as above — every bit index visited is `< n_bits`.
        unsafe { cmp_bits_unaligned(a_base, a_off, b_base, b_off, n_bits) }
    }
}

/// [`cmp_bits`] for two **byte-aligned** bases.
///
/// # Safety
///
/// Both pointers must point at readable data covering `n_bits` bits.
unsafe fn cmp_bits_aligned(a: *const u8, b: *const u8, n_bits: usize) -> Ordering {
    let whole = n_bits >> 3;
    if whole > 0 {
        // SAFETY: the caller guarantees `n_bits` readable bits from each pointer, i.e.
        // `whole == n_bits / 8` whole bytes.
        let (as_, bs) = unsafe {
            (
                core::slice::from_raw_parts(a, whole),
                core::slice::from_raw_parts(b, whole),
            )
        };
        let ord = as_.cmp(bs);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    let tail = n_bits & 7;
    if tail == 0 {
        return Ordering::Equal;
    }
    // SAFETY: `tail != 0` means bit `n_bits - 1` is compared, so the byte holding it
    // (byte `whole` from the aligned base) is readable on both sides — a bitstring's
    // data is a byte array.
    let (ab, bb) = unsafe { (*a.add(whole), *b.add(whole)) };
    // Only the top `tail` bits are part of the compared sequence; the low bits are
    // padding the bit loop never looks at.
    (ab >> (8 - tail)).cmp(&(bb >> (8 - tail)))
}

/// [`cmp_bits`] for a pair whose bit offsets are not both byte-aligned — the plain
/// bit loop (`erts_cmp_bits__`'s result, one bit at a time).
///
/// # Safety
///
/// Both bases must point at readable bitstring data covering `n_bits` bits from the
/// respective offset.
unsafe fn cmp_bits_unaligned(
    a_base: *const u8,
    a_off: usize,
    b_base: *const u8,
    b_off: usize,
    n_bits: usize,
) -> Ordering {
    let mut i = 0;
    while i < n_bits {
        // SAFETY: `i < n_bits`, so both bits are within the caller's guaranteed range.
        let (abit, bbit) = unsafe {
            (
                crate::bitstring::raw_bit(a_base, a_off + i),
                crate::bitstring::raw_bit(b_base, b_off + i),
            )
        };
        if abit != bbit {
            return abit.cmp(&bbit);
        }
        i += 1;
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cmp_bits` over two byte slices at offset 0 — the aligned form.
    fn cmp(a: &[u8], b: &[u8], n_bits: usize) -> Ordering {
        // SAFETY: both slices cover `n_bits` bits: the callers below always pass
        // `n_bits <= 8 * len`.
        unsafe { cmp_bits(a.as_ptr(), 0, b.as_ptr(), 0, n_bits) }
    }

    /// The reference: the literal bit-by-bit loop over the *same* order.
    fn reference(a: &[u8], b: &[u8], n_bits: usize) -> Ordering {
        let bit = |s: &[u8], i: usize| (s[i >> 3] >> (7 - (i & 7))) & 1;
        let mut i = 0;
        while i < n_bits {
            let (x, y) = (bit(a, i), bit(b, i));
            if x != y {
                return x.cmp(&y);
            }
            i += 1;
        }
        Ordering::Equal
    }

    #[test]
    fn empty_is_equal() {
        assert_eq!(cmp(&[], &[], 0), Ordering::Equal);
        assert_eq!(cmp(&[0xff], &[0x00], 0), Ordering::Equal);
    }

    #[test]
    fn whole_bytes_order_lexicographically() {
        assert_eq!(cmp(b"abc", b"abc", 24), Ordering::Equal);
        assert_eq!(cmp(b"abc", b"abd", 24), Ordering::Less);
        assert_eq!(cmp(b"abd", b"abc", 24), Ordering::Greater);
        assert_eq!(cmp(b"ab", b"abc", 16), Ordering::Equal);
        // A longer *prefix* is not visible to a shorter comparison.
        assert_eq!(cmp(b"abz", b"abc", 16), Ordering::Equal);
        // The first differing byte decides, high byte order.
        assert_eq!(cmp(&[0x01, 0xff], &[0x02, 0x00], 16), Ordering::Less);
    }

    #[test]
    fn tail_bits_are_compared_from_the_top() {
        // 0b1000_0000 vs 0b0000_0000 over the first bit.
        assert_eq!(cmp(&[0x80], &[0x00], 1), Ordering::Greater);
        // Same byte, but the compared prefix covers a differing lower bit only when
        // that bit is inside the prefix.
        assert_eq!(cmp(&[0b1010_0000], &[0b1001_0000], 2), Ordering::Equal);
        assert_eq!(cmp(&[0b1010_0000], &[0b1001_0000], 3), Ordering::Greater);
        // Bits past `n_bits` are padding and must not be read (the reference ignores
        // them; a forged block can carry junk there).
        assert_eq!(cmp(&[0b1010_1111], &[0b1010_0000], 4), Ordering::Equal);
        // 11-bit comparison over two bytes: 8 whole + 3 tail.
        assert_eq!(cmp(&[0x00, 0b0010_0000], &[0x00, 0b0001_1111], 11), Ordering::Greater);
        // The tail's low bits are outside the 3 compared bits: equal prefixes.
        assert_eq!(cmp(&[0x00, 0b0010_0000], &[0x00, 0b0011_1111], 11), Ordering::Equal);
        assert_eq!(cmp(&[0x00, 0b0010_0000], &[0x00, 0b0110_0000], 11), Ordering::Less);
        // The whole-byte part still decides before the tail is looked at.
        assert_eq!(cmp(&[0x01, 0x00], &[0x00, 0xff], 11), Ordering::Greater);
    }

    #[test]
    fn unaligned_offsets_match_the_bit_loop() {
        // A sub-bitstring: base `b"Xabc"`, 8-bit offset, i.e. the bits of `abc`.
        let base_a = [0x58u8, b'a', b'b', b'c'];
        let base_b = [0x59u8, b'a', b'b', b'd'];
        // SAFETY: both arrays cover 24 bits from bit offset 8.
        let aligned = unsafe { cmp_bits(base_a.as_ptr().add(1), 0, base_b.as_ptr().add(1), 0, 24) };
        // SAFETY: both arrays cover 24 bits from bit offset 8.
        let unaligned =
            unsafe { cmp_bits(base_a.as_ptr(), 8, base_b.as_ptr(), 8, 24) };
        assert_eq!(aligned, Ordering::Less);
        assert_eq!(unaligned, aligned);
    }

    #[test]
    fn agrees_with_the_bit_loop_over_a_corpus() {
        // Every prefix length of every pair over a small byte corpus: the aligned fast
        // path must agree with the literal bit loop.
        let corpus: [&[u8]; 6] = [
            &[0x00, 0x00],
            &[0x00, 0x01],
            &[0x80, 0x00],
            &[0xff, 0xff],
            &[0x0f, 0xf0],
            &[0x0f, 0xf0, 0x55],
        ];
        for a in corpus {
            for b in corpus {
                for n_bits in 0..=8 * a.len().min(b.len()) {
                    assert_eq!(
                        cmp(a, b, n_bits),
                        reference(a, b, n_bits),
                        "a={a:x?} b={b:x?} n_bits={n_bits}"
                    );
                }
            }
        }
    }

    #[test]
    fn unaligned_agrees_with_the_bit_loop_over_a_corpus() {
        // Same corpus, but with both operands shifted by 1..8 bits inside a padded base,
        // so the unaligned path is exercised against the literal bit loop.
        let corpus: [&[u8]; 4] = [&[0x00, 0x00, 0x00], &[0x00, 0x01, 0x80], &[0x7f, 0xff, 0x01], &[0xff, 0xff, 0xff]];
        for a in corpus {
            for b in corpus {
                for off in 1..8 {
                    for n_bits in 0..=8 {
                        // SAFETY: the arrays cover `off + n_bits <= 15` bits.
                        let got = unsafe {
                            cmp_bits(a.as_ptr(), off, b.as_ptr(), off, n_bits)
                        };
                        // The reference over the same (offset) bit stream.
                        let reference = {
                            let bit = |s: &[u8], i: usize| (s[i >> 3] >> (7 - (i & 7))) & 1;
                            let mut i = 0;
                            let mut out = Ordering::Equal;
                            while i < n_bits {
                                let (x, y) = (bit(a, off + i), bit(b, off + i));
                                if x != y {
                                    out = x.cmp(&y);
                                    break;
                                }
                                i += 1;
                            }
                            out
                        };
                        assert_eq!(got, reference, "a={a:x?} b={b:x?} off={off} n={n_bits}");
                    }
                }
            }
        }
    }
}
