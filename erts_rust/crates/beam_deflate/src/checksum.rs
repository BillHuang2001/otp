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
//! The two RFC 1950/1952 checksums, and their `*_combine` forms.
//!
//! Ported from `../../erts/emulator/zlib/{adler32.c,crc32.c}` (zlib 1.3.2):
//!
//! * [`adler32`] — the RFC 1950 trailer. Initial value `1` (the C's
//!   `adler32(0, Z_NULL, 0)` convention); an empty slice returns the accumulator
//!   unchanged, as the C does for a non-`NULL` buffer.
//! * [`crc32`] — the RFC 1952 (gzip) trailer. Initial value `0`; an empty slice returns
//!   the accumulator unchanged, as the C does for a non-`NULL` buffer.
//! * [`adler32_combine`] / [`crc32_combine`] — fold a second stream's checksum onto the
//!   first, given the second stream's length (`zlib`'s `*_combine`).
//!
//! Both checksums are *observables* the moment they land in a zlib/gzip trailer, so the
//! arithmetic (the `NMAX` batching, the `BASE` folding, the reflected CRC-32 table with
//! `0xEDB88320`) is reproduced exactly rather than "equivalently".

/// Adler-32's largest prime (`BASE` in `adler32.c`).
const ADLER_BASE: u32 = 65521;

/// The largest `n` for which zlib still defers the modulo: `255n(n+1)/2 +
/// (n+1)(BASE-1) <= 2^32-1` (`NMAX` in `adler32.c`).
const ADLER_NMAX: usize = 5552;

/// The reflected CRC-32 polynomial (IEEE 802.3, as used by gzip).
const CRC_POLY: u32 = 0xedb8_8320;

/// The CRC-32 lookup table (`crc32.c`'s `crc_table`), built by the same bit loop the C
/// runs at first use.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0usize;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                CRC_POLY ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
};

/// Adler-32 of `data`, continuing from `adler`.
///
/// Start with `1` (the RFC and the C's `adler32(0, Z_NULL, 0)` identity). The
/// accumulator is folded modulo [`ADLER_BASE`] once per `NMAX` bytes, exactly as the C
/// does, so the result is independent of how the input is split — which is what
/// [`adler32_combine`] relies on.
pub fn adler32(adler: u32, data: &[u8]) -> u32 {
    let mut s1 = adler & 0xffff;
    let mut s2 = (adler >> 16) & 0xffff;
    for chunk in data.chunks(ADLER_NMAX) {
        for &byte in chunk {
            s1 += u32::from(byte);
            s2 += s1;
        }
        s1 %= ADLER_BASE;
        s2 %= ADLER_BASE;
    }
    (s2 << 16) | s1
}

/// Folds `adler2` (the checksum of a stream that *follows* the one `adler1` describes)
/// onto `adler1`, where the second stream is `len2` bytes long (`adler32_combine`).
///
/// The identity `adler32_combine(adler32(1, a), adler32(1, b), b.len()) ==
/// adler32(1, a ++ b)` is the property the implementation is tested against.
pub fn adler32_combine(adler1: u32, adler2: u32, len2: u64) -> u32 {
    let rem = (len2 % u64::from(ADLER_BASE)) as u32;
    let mut sum1 = adler1 & 0xffff;
    let mut sum2 = rem * sum1 % ADLER_BASE;
    sum1 += (adler2 & 0xffff) + ADLER_BASE - 1;
    sum2 += ((adler1 >> 16) & 0xffff) + ((adler2 >> 16) & 0xffff) + ADLER_BASE - rem;
    if sum1 >= ADLER_BASE {
        sum1 -= ADLER_BASE;
    }
    if sum1 >= ADLER_BASE {
        sum1 -= ADLER_BASE;
    }
    if sum2 >= ADLER_BASE << 1 {
        sum2 -= ADLER_BASE << 1;
    }
    if sum2 >= ADLER_BASE {
        sum2 -= ADLER_BASE;
    }
    sum1 | (sum2 << 16)
}

/// CRC-32 (gzip / `crc32.c`) of `data`, continuing from `crc`.
///
/// Start with `0`. The accumulator is complemented on entry and exit, exactly as the C
/// does, so `crc32(0, a ++ b) == crc32(crc32(0, a), b)`.
pub fn crc32(crc: u32, data: &[u8]) -> u32 {
    let mut c = crc ^ 0xffff_ffff;
    for &byte in data {
        c = CRC_TABLE[((c ^ u32::from(byte)) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xffff_ffff
}

/// Folds `crc2` (the CRC of a stream that *follows* the one `crc1` describes) onto
/// `crc1`, where the second stream is `len2` bytes long (`crc32_combine`).
///
/// Implemented with the four-way GF(2) matrix squaring of `crc32.c`'s pre-1.3
/// `crc32_combine` (kept because it needs no precomputed `x2n` table):
/// the identity `crc32_combine(crc32(0, a), crc32(0, b), b.len()) == crc32(0, a ++ b)`
/// is what the tests pin.
pub fn crc32_combine(crc1: u32, crc2: u32, len2: u64) -> u32 {
    /// One row-wise application of a GF(2) matrix to a word (`gf2_matrix_times`).
    fn matrix_times(mat: &[u32; 32], mut vec: u32) -> u32 {
        let mut sum = 0u32;
        let mut idx = 0;
        while vec != 0 {
            if vec & 1 != 0 {
                sum ^= mat[idx];
            }
            vec >>= 1;
            idx += 1;
        }
        sum
    }

    /// `square(out, in)`: `out` becomes `in * in` (`gf2_matrix_square`).
    fn matrix_square(square: &mut [u32; 32], mat: &[u32; 32]) {
        for n in 0..32 {
            square[n] = matrix_times(mat, mat[n]);
        }
    }

    // The operator for one zero bit (x^0), then x^1.
    let mut odd = [0u32; 32];
    odd[0] = CRC_POLY;
    let mut row = 1u32;
    for slot in odd.iter_mut().skip(1) {
        *slot = row;
        row <<= 1;
    }
    let mut even = [0u32; 32];
    // even = odd^2 (operator for two bits), odd = even^2 (operator for four bits).
    matrix_square(&mut even, &odd);
    matrix_square(&mut odd, &even);

    let mut len = len2;
    let mut crc = crc1;
    while len != 0 {
        matrix_square(&mut even, &odd);
        if len & 1 != 0 {
            crc = matrix_times(&even, crc);
        }
        len >>= 1;
        if len == 0 {
            break;
        }
        matrix_square(&mut odd, &even);
        if len & 1 != 0 {
            crc = matrix_times(&odd, crc);
        }
        len >>= 1;
    }

    crc ^ crc2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical CRC-32 known-answer test (the value every CRC catalogue lists).
    #[test]
    fn crc32_matches_the_known_answer() {
        assert_eq!(crc32(0, b""), 0, "an empty input leaves the accumulator alone");
        assert_eq!(crc32(0, b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(0, b"a"), 0xe8b7_be43);
    }

    /// The Adler-32 known answers, including the `BASE` fold that a naive
    /// `sum % 65521`-per-byte loop would still pass — the point here is the rolling
    /// identity and the trailer value `zlib:compress/1` writes.
    #[test]
    fn adler32_matches_the_known_answer() {
        assert_eq!(adler32(1, b""), 1, "the initial value is the identity");
        assert_eq!(adler32(1, b"Wikipedia"), 0x11e6_0398);
        // The empty zlib stream's trailer: `zlib:compress(<<>>)` ends with 00 00 00 01.
        assert_eq!(adler32(1, b""), 0x0000_0001);
    }

    /// Splitting the input must not change either checksum: the `NMAX` batching folds
    /// modulo at the chunk boundary, and the continuation argument carries the state.
    #[test]
    fn incremental_and_one_shot_agree() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 37 % 256) as u8).collect();
        for split in [0usize, 1, 15, 16, 5551, 5552, 5553, 9_999, 10_000] {
            let (a, b) = data.split_at(split);
            assert_eq!(
                adler32(adler32(1, a), b),
                adler32(1, &data),
                "adler32 split at {split}"
            );
            assert_eq!(
                crc32(crc32(0, a), b),
                crc32(0, &data),
                "crc32 split at {split}"
            );
        }
    }

    /// `*_combine` is the "I have two independent checksums" form; it must reconstruct the
    /// whole-input checksum.
    #[test]
    fn combine_reconstructs_the_whole_input() {
        let a = b"the quick brown fox ".to_vec();
        let b = b"jumps over the lazy dog".to_vec();
        let mut whole = a.clone();
        whole.extend_from_slice(&b);

        assert_eq!(
            adler32_combine(adler32(1, &a), adler32(1, &b), b.len() as u64),
            adler32(1, &whole)
        );
        assert_eq!(
            crc32_combine(crc32(0, &a), crc32(0, &b), b.len() as u64),
            crc32(0, &whole)
        );

        // An empty second part folds to the first checksum, and combining onto an empty
        // first part yields the second.
        assert_eq!(adler32_combine(adler32(1, &a), 1, 0), adler32(1, &a));
        assert_eq!(crc32_combine(crc32(0, &a), 0, 0), crc32(0, &a));
        assert_eq!(adler32_combine(1, adler32(1, &b), b.len() as u64), adler32(1, &b));
        assert_eq!(crc32_combine(0, crc32(0, &b), b.len() as u64), crc32(0, &b));
    }

    /// The combine forms must hold across a length that crosses the Adler `NMAX`
    /// boundary and the CRC matrix's four-bit stride.
    #[test]
    fn combine_holds_for_longer_splits() {
        let whole: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        for split in [1usize, 5_552, 5_553, 65_536, 131_071, 199_999] {
            let (a, b) = whole.split_at(split);
            assert_eq!(
                adler32_combine(adler32(1, a), adler32(1, b), b.len() as u64),
                adler32(1, &whole),
                "adler32 combine at {split}"
            );
            assert_eq!(
                crc32_combine(crc32(0, a), crc32(0, b), b.len() as u64),
                crc32(0, &whole),
                "crc32 combine at {split}"
            );
        }
    }
}
