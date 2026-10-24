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

//! **RFC 1321 MD5** (the `RSA Data Security, Inc. MD5 Message-Digest Algorithm`), the workspace's
//! single MD5 implementation.
//!
//! [`digest`] is the one-shot form and [`Md5`] the streaming one. Both live here because this crate
//! is the workspace's **lowest common ancestor**: three compatibility contracts need the same
//! arithmetic, and duplicating it would let the copies drift apart.
//!
//! * **A.2** (`.beam` binary) — the module checksum `erts_md5_finish` feeds into
//!   `beam->checksum` after a walk of the loaded code (`../../erts/emulator/beam/beam_file.c:1753-1829`),
//!   i.e. what `Module:module_info(md5)` reports.
//! * **A.5** (distribution) — the handshake digest of the connection setup
//!   (`../../lib/erl_interface/src/connect/ei_connect.c`).
//! * **A.7** (OTP app contracts) — the `erlang:md5/1` BIF family: `md5/1`, `md5_init/0`,
//!   `md5_update/2` and `md5_final/1` (`../../erts/emulator/beam/erl_bif_chksum.c:440-525`), whose
//!   context argument is the raw **96-byte `erts_md5_state` image**
//!   (`../../erts/emulator/beam/erl_md5.h:30-35`).
//!
//! The C ground truth is `../../erts/emulator/beam/erl_md5.c` (read-only reference): the semantics
//! below are reproduced field-for-field, because that 96-byte image is *observable* through
//! `erlang:md5_init/0` and `erlang:md5_update/2`.
//!
//! # The streaming context is the C context
//!
//! [`Md5`] is byte-for-byte the C `erts_md5_state`: four state words, the total length of the
//! *whole-block* bytes already hashed, the number of bytes buffered, and the 64-byte block buffer.
//! The raw-state accessors ([`Md5::state`], [`Md5::total_len`], [`Md5::buffered_len`],
//! [`Md5::buffer`]) and the inverse constructor [`Md5::from_parts`] exist so that `beam_bifs` can
//! marshal that image in and out without duplicating any of the arithmetic: the byte layout belongs
//! to the BIF layer, the MD5 itself belongs to this module.
//!
//! # Properties
//!
//! Allocation-free (fixed arrays only), panic-free (no indexing outside a proven bound, no
//! `unwrap`), **safe Rust** (the crate root carries `#![forbid(unsafe_code)]`) and dependency-free
//! (this crate's only dependency remains `libc`, which this module does not even need).
//!
//! # Examples
//!
//! ```
//! // The RFC 1321 Appendix A.5 `"abc"` vector.
//! assert_eq!(
//!     beam_common::digest(b"abc"),
//!     [0x90, 0x01, 0x50, 0x98, 0x3c, 0xd2, 0x4f, 0xb0, 0xd6, 0x96, 0x3f, 0x7d, 0x28, 0xe1, 0x7f, 0x72]
//! );
//! ```

use crate::error::{BeamError, Result};

/// The MD5 block length in bytes: the size of the message block one `hash_part` call folds in.
pub const BLOCK_LEN: usize = 64;

/// The MD5 digest length in bytes.
pub const DIGEST_LEN: usize = 16;

/// The four initial state words (RFC 1321 §3.3, the C's `A0`/`B0`/`C0`/`D0`).
const IV: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

/// The 64 per-round left-rotation amounts `S[i]` (RFC 1321 §3.4).
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The 64 per-round additive constants `K[i] = floor(2^32 * abs(sin(i + 1)))` (RFC 1321 §3.4) —
/// the C's `sk[].k` (`../../erts/emulator/beam/erl_md5.c:158-177`).
#[rustfmt::skip]
const KS: [u32; 64] = [
    0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee, 0xf57c_0faf, 0x4787_c62a, 0xa830_4613,
    0xfd46_9501, 0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be, 0x6b90_1122, 0xfd98_7193,
    0xa679_438e, 0x49b4_0821, 0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa, 0xd62f_105d,
    0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8, 0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed,
    0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a, 0xfffa_3942, 0x8771_f681, 0x6d9d_6122,
    0xfde5_380c, 0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70, 0x289b_7ec6, 0xeaa1_27fa,
    0xd4ef_3085, 0x0488_1d05, 0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665, 0xf429_2244,
    0x432a_ff97, 0xab94_23a7, 0xfc93_a039, 0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1,
    0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1, 0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb,
    0xeb86_d391,
];

/// One-shot MD5 of `data` (`erts_md5`, `../../erts/emulator/beam/erl_md5.c:146`).
///
/// # Examples
///
/// ```
/// // The RFC 1321 Appendix A.5 `"abc"` vector, as bytes and as hex.
/// assert_eq!(
///     beam_common::digest(b"abc"),
///     [0x90, 0x01, 0x50, 0x98, 0x3c, 0xd2, 0x4f, 0xb0, 0xd6, 0x96, 0x3f, 0x7d, 0x28, 0xe1, 0x7f, 0x72]
/// );
/// let hex: String = beam_common::digest(b"abc")
///     .iter()
///     .map(|byte| format!("{byte:02x}"))
///     .collect();
/// assert_eq!(hex, "900150983cd24fb0d6963f7d28e17f72");
///
/// // Padding: a 56-byte message no longer fits its 8-byte length field in one block.
/// let hex = |data: &[u8]| -> String {
///     beam_common::digest(data).iter().map(|byte| format!("{byte:02x}")).collect()
/// };
/// assert_eq!(hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
/// assert_eq!(hex(&[b'A'; 56]), "a2f3e2024931bd470555002aa5ccc010");
/// ```
pub fn digest(data: &[u8]) -> [u8; DIGEST_LEN] {
    let mut md5 = Md5::new();
    md5.update(data);
    md5.finish()
}

/// A streaming MD5 context — the Rust `erts_md5_state`
/// (`../../erts/emulator/beam/erl_md5.h:30-35`).
///
/// The field set and its meaning are the C's: `a`/`b`/`c`/`d` are the four chained state words,
/// `total_len` counts the bytes already folded in as whole 64-byte blocks, `len` is how many bytes
/// of the next block are buffered in `buf`.
///
/// # Examples
///
/// ```
/// use beam_common::Md5;
///
/// let mut md5 = Md5::new();
/// md5.update(b"ab");
/// md5.update(b"c");
/// assert_eq!(md5.finish(), beam_common::digest(b"abc"));
/// assert_eq!(md5.total_len(), 0, "three bytes are buffered, not hashed");
/// assert_eq!(md5.buffered_len(), 3);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Md5 {
    /// State word `a`.
    a: u32,
    /// State word `b`.
    b: u32,
    /// State word `c`.
    c: u32,
    /// State word `d`.
    d: u32,
    /// The total length of the whole 64-byte blocks already hashed (the C's `tot_len`).
    total_len: u64,
    /// How many bytes of `buf` are pending (the C's `len`; `0..=BLOCK_LEN`).
    len: usize,
    /// The pending bytes of the block being assembled (the C's `buf`).
    buf: [u8; BLOCK_LEN],
}

impl Md5 {
    /// A fresh context (`erts_md5_init`, `../../erts/emulator/beam/erl_md5.c:76`): the IV, an empty
    /// buffer, and both lengths zero.
    pub fn new() -> Md5 {
        Md5 {
            a: IV[0],
            b: IV[1],
            c: IV[2],
            d: IV[3],
            total_len: 0,
            len: 0,
            buf: [0u8; BLOCK_LEN],
        }
    }

    /// Runs `msg` through the context (`erts_md5_update`,
    /// `../../erts/emulator/beam/erl_md5.c:87`).
    ///
    /// The buffering asymmetry of the C is preserved exactly: a whole 64-byte update on an empty
    /// buffer hashes the block immediately, while two 32-byte updates leave the buffer *full and
    /// unhashed* (the top-up comparison is `<`, not `<=`). Both are legal states, and both finish to
    /// the same digest.
    pub fn update(&mut self, msg: &[u8]) {
        let mut part = msg;

        if self.len > 0 {
            let copy_size = BLOCK_LEN - self.len;
            if copy_size < part.len() {
                // Top the buffered block up and hash it.
                let (head, tail) = part.split_at(copy_size);
                self.buf[self.len..].copy_from_slice(head);
                part = tail;
                self.hash_buffered();
                self.len = 0;
                self.total_len = self.total_len.wrapping_add(BLOCK_LEN as u64);
            } else {
                // The whole input fits in the pending block.
                let end = self.len + part.len();
                self.buf[self.len..end].copy_from_slice(part);
                self.len = end;
                return;
            }
        }

        while part.len() >= BLOCK_LEN {
            let (head, tail) = part.split_at(BLOCK_LEN);
            let mut block = [0u8; BLOCK_LEN];
            block.copy_from_slice(head);
            self.hash_part(&block);
            part = tail;
            self.total_len = self.total_len.wrapping_add(BLOCK_LEN as u64);
        }

        if !part.is_empty() {
            self.buf[..part.len()].copy_from_slice(part);
            self.len = part.len();
        }
    }

    /// Finishes the context into a 16-byte digest (`erts_md5_finish` + `pad`,
    /// `../../erts/emulator/beam/erl_md5.c:125` and `:222`).
    ///
    /// Takes `&self`: the padding is applied to a **copy** of the state, so `erlang:md5_final/1` is
    /// pure — finishing twice yields the same digest, and a context can be finished, updated and
    /// finished again.
    pub fn finish(&self) -> [u8; DIGEST_LEN] {
        let (padded, padded_len) = self.padded();
        let mut state = self.clone();

        let mut off = 0;
        while off < padded_len {
            let mut block = [0u8; BLOCK_LEN];
            block.copy_from_slice(&padded[off..off + BLOCK_LEN]);
            state.hash_part(&block);
            off += BLOCK_LEN;
        }

        let mut out = [0u8; DIGEST_LEN];
        out[..4].copy_from_slice(&state.a.to_le_bytes());
        out[4..8].copy_from_slice(&state.b.to_le_bytes());
        out[8..12].copy_from_slice(&state.c.to_le_bytes());
        out[12..].copy_from_slice(&state.d.to_le_bytes());
        out
    }

    /// Rebuilds a context from its raw parts — the inverse of [`Md5::state`]/[`Md5::total_len`]/
    /// [`Md5::buffered_len`]/[`Md5::buffer`], i.e. of the 96-byte `erts_md5_state` image
    /// `erlang:md5_init/0` hands out and `erlang:md5_update/2` accepts.
    ///
    /// The **full 64-byte** `buffer` is stored verbatim (not just `buffer[..len]`), so a context
    /// whose bytes past `len` are non-zero round-trips byte-exactly.
    ///
    /// # Errors
    ///
    /// `BeamError::BadArg` when `len > BLOCK_LEN`. The C's own out-of-range `len > 64` case is
    /// **undefined behaviour** (it reads and writes past its 64-byte buffer), so the caller owns
    /// mapping that case to its own capability-gap error rather than reproducing it; a `len` of
    /// exactly `64` is a state the C itself produces and is accepted.
    ///
    /// # Examples
    ///
    /// ```
    /// use beam_common::{digest, BeamError, Md5};
    ///
    /// // The `len == 64` state the oracle itself produces: a full buffer, nothing hashed yet.
    /// let iv = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    /// let md5 = Md5::from_parts(iv, 0, 64, [b'A'; 64]).unwrap();
    /// assert_eq!(md5.buffered_len(), 64);
    /// assert_eq!(md5.finish(), digest(&[b'A'; 64]));
    ///
    /// // Bytes past `buffered_len` survive verbatim, so an image round-trips byte-exactly.
    /// let mut buffer = [0u8; 64];
    /// buffer[..3].copy_from_slice(b"abc");
    /// buffer[63] = 0xff;
    /// let md5 = Md5::from_parts(iv, 0, 3, buffer).unwrap();
    /// assert_eq!(md5.buffer(), &buffer);
    /// assert_eq!(md5.finish(), digest(b"abc"));
    ///
    /// // The oracle's own `len == 64` state is legal; `len == 65` is not.
    /// assert!(matches!(
    ///     Md5::from_parts(iv, 0, 65, [0u8; 64]),
    ///     Err(BeamError::BadArg)
    /// ));
    /// ```
    pub fn from_parts(
        state: [u32; 4],
        total_len: u64,
        len: usize,
        buffer: [u8; BLOCK_LEN],
    ) -> Result<Md5> {
        if len > BLOCK_LEN {
            return Err(BeamError::BadArg);
        }
        Ok(Md5 {
            a: state[0],
            b: state[1],
            c: state[2],
            d: state[3],
            total_len,
            len,
            buf: buffer,
        })
    }

    /// The four state words `[a, b, c, d]`, in the C's field order.
    pub fn state(&self) -> [u32; 4] {
        [self.a, self.b, self.c, self.d]
    }

    /// The number of bytes already folded in as whole 64-byte blocks (the C's `tot_len`).
    pub fn total_len(&self) -> u64 {
        self.total_len
    }

    /// How many bytes are pending in the block buffer (the C's `len`) — `0..=BLOCK_LEN`, and
    /// `BLOCK_LEN` is reachable by construction (two 32-byte updates).
    pub fn buffered_len(&self) -> usize {
        self.len
    }

    /// The 64-byte block buffer, including the bytes past [`Md5::buffered_len`].
    pub fn buffer(&self) -> &[u8; BLOCK_LEN] {
        &self.buf
    }

    /// `pad` (`../../erts/emulator/beam/erl_md5.c:222`): the pending bytes, then `0x80`, then zeros
    /// up to the 56-byte boundary, then the 64-bit little-endian **bit** length of
    /// `total_len + buffered_len`. Returns the buffer and its length in bytes (64 or 128).
    fn padded(&self) -> ([u8; 2 * BLOCK_LEN], usize) {
        let size = self.len;
        let filled = (size + 1) % BLOCK_LEN;
        // The C writes the zero run as the unsigned `(64 - 8 - filled) & (64 - 1)`, which relies on
        // the subtraction *wrapping* for `filled > 56` (a buffered 56..=63 bytes leaves no room for
        // the 8-byte length field). Adding the modulus before subtracting is the non-negative
        // equivalent: `(56 + 64 - filled) & (64 - 1) == (56 - filled).rem_euclid(64)`.
        let zeroes = (56 + BLOCK_LEN - filled) & (BLOCK_LEN - 1);

        let mut out = [0u8; 2 * BLOCK_LEN];
        out[..size].copy_from_slice(&self.buf[..size]);
        out[size] = 0x80;
        // `out[size + 1..size + 1 + zeroes]` is already zero.
        let pos = size + 1 + zeroes;
        // The C's `ASSERT((pad_ptr - pad_buf) % 64 == 0)`: the padded message is a whole number of
        // blocks, so `pos + 8` is 64 or 128 and the 128-byte buffer holds the length field.
        debug_assert!(
            (pos + 8).is_multiple_of(BLOCK_LEN),
            "the padded message must be block-aligned"
        );
        let total_bits = self.total_len.wrapping_add(size as u64).wrapping_mul(8);
        out[pos..pos + 8].copy_from_slice(&total_bits.to_le_bytes());
        (out, pos + 8)
    }

    /// Folds the pending `buf[..len]` block into the state.
    fn hash_buffered(&mut self) {
        let block = self.buf;
        self.hash_part(&block);
    }

    /// `hash_part` (`../../erts/emulator/beam/erl_md5.c:156`): folds one 64-byte block into the
    /// state — the 64 RFC 1321 rounds, `wrapping_add` throughout.
    fn hash_part(&mut self, block: &[u8; BLOCK_LEN]) {
        let words = block_words(block);

        let mut a = self.a;
        let mut b = self.b;
        let mut c = self.c;
        let mut d = self.d;

        for (i, (k, shift)) in KS.iter().zip(SHIFTS.iter()).enumerate() {
            let (f, g) = if i <= 15 {
                (d ^ (b & (c ^ d)), i)
            } else if i <= 31 {
                (c ^ (d & (b ^ c)), (5 * i + 1) & 0xF)
            } else if i <= 47 {
                (b ^ c ^ d, (3 * i + 5) & 0xF)
            } else {
                (c ^ (b | !d), (7 * i) & 0xF)
            };
            // `g` is masked to `0..=15` in every arm above (and equals `i <= 15` in the first), so
            // this index cannot go out of bounds.
            let f = f
                .wrapping_add(a)
                .wrapping_add(*k)
                .wrapping_add(words[g])
                .rotate_left(*shift);

            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f);
        }

        self.a = self.a.wrapping_add(a);
        self.b = self.b.wrapping_add(b);
        self.c = self.c.wrapping_add(c);
        self.d = self.d.wrapping_add(d);
    }
}

impl Default for Md5 {
    /// A fresh context — the same as [`Md5::new`].
    fn default() -> Md5 {
        Md5::new()
    }
}

/// The 16 little-endian 32-bit words of one message block (`get_little_int32` in the C).
fn block_words(block: &[u8; BLOCK_LEN]) -> [u32; 16] {
    let mut words = [0u32; 16];
    for (i, word) in words.iter_mut().enumerate() {
        // `i <= 15` and `BLOCK_LEN == 64`, so `at + 3 <= 63` is in bounds.
        let at = i * 4;
        *word = u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]]);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A digest as lowercase hex.
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// A deterministic message whose byte `i` is `i % 251` (the corpus convention shared with
    /// `tests/checksum.rs`).
    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// The RFC 1321 Appendix A.5 test suite, the classic 10^6-byte `"a"` vector (multi-block, with
    /// a non-zero high word in its padded length field) and the `b""`/`[b'A'; _]` anchors.
    #[test]
    fn rfc_1321_suite_vectors() {
        let cases: [(&[u8], &str); 7] = [
            (&[], "d41d8cd98f00b204e9800998ecf8427e"),
            (b"a", "0cc175b9c0f1b6a831c399e269772661"),
            (b"abc", "900150983cd24fb0d6963f7d28e17f72"),
            (b"message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                b"abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (data, expected) in cases {
            assert_eq!(hex(&digest(data)), expected, "md5 of {:?}", data);
        }

        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(hex(&digest(&million_a)), "7707d6ae4e027c70eea2a935c2296f21");
        // A split that leaves the last chunk at the pad boundary.
        let mut md5 = Md5::new();
        md5.update(&million_a[..999_993]);
        md5.update(&million_a[999_993..]);
        assert_eq!(hex(&md5.finish()), "7707d6ae4e027c70eea2a935c2296f21");
    }

    /// The byte-array anchors the `erlang:md5/1` BIF tests pin, including the `len == 64` case.
    #[test]
    fn known_vectors() {
        assert_eq!(
            digest(b"abc"),
            [144, 1, 80, 152, 60, 210, 79, 176, 214, 150, 63, 125, 40, 225, 127, 114]
        );
        assert_eq!(
            digest(b""),
            [212, 29, 140, 217, 143, 0, 178, 4, 233, 128, 9, 152, 236, 248, 66, 126]
        );
        // > 64 bytes exercises the whole-block path.
        assert_eq!(
            digest(&[b'A'; 100]),
            [138, 220, 89, 55, 230, 53, 246, 201, 175, 100, 111, 11, 35, 86, 15, 174]
        );
        // Exactly one block.
        assert_eq!(
            digest(&[b'A'; 64]),
            [210, 137, 169, 117, 101, 188, 45, 39, 172, 139, 133, 69, 165, 221, 186, 69]
        );
    }

    /// Every padding boundary, pinned against the system oracle (OTP 29.1.1 / erts-17.1) over the
    /// `patterned` byte convention.
    ///
    /// With `size` buffered bytes, `pad` derives `filled = (size + 1) % 64` and the zero run
    /// `(56 - filled) mod 64` (the C writes that as the unsigned `(64 - 8 - filled) & (64 - 1)`).
    /// For a message whose length is `56..=63` beyond a whole number of 64-byte blocks the 8-byte
    /// length field no longer fits the final block, so the subtraction underflows — which a checked
    /// Rust `usize` subtraction cannot express. This pins **every** length `0..=130` plus `1000` and
    /// `4096`, which covers that boundary window twice over.
    #[test]
    fn pad_boundary_lengths_match_known_vectors() {
        const KNOWN: &[(usize, &str)] = &[
            (0, "d41d8cd98f00b204e9800998ecf8427e"),
            (1, "93b885adfe0da089cdf634904fd59f71"),
            (2, "441077cc9e57554dd476bdfb8b8b8102"),
            (3, "b95f67f61ebb03619622d798f45fc2d3"),
            (4, "37b59afd592725f9305e484a5d7f5168"),
            (5, "d05374dc381d9b52806446a71c8e79b1"),
            (6, "d15ae53931880fd7b724dd7888b4b4ed"),
            (7, "9aa461e1eca4086f9230aa49c90b0c61"),
            (8, "3677509751ccf61539174d2b9635a7bf"),
            (9, "a6e7d3b46fdfaf0bde2a1f832a00d2de"),
            (10, "c56bd5480f6e5413cb62a0ad9666613a"),
            (11, "5b86fa8ad8f4357ea417214182177be8"),
            (12, "50a73d7013e9803e3b20888f8fcafb15"),
            (13, "b20d4797e23eea3ea5778970d2e226f3"),
            (14, "aa541e601b7b9ddd0504d19866350d4e"),
            (15, "58b7ce493ac99c66058538dacb1e3c94"),
            (16, "1ac1ef01e96caf1be0d329331a4fc2a8"),
            (17, "1bdd36b0a024c90db383512607293692"),
            (18, "633ab81aea5942052b794524e1a28477"),
            (19, "2d325313eb5df436c078435fa0f5eff1"),
            (20, "1549d1aae20214e065ab4b76aaac89a8"),
            (21, "7e437c81824d3982e70c88b5da8ea94b"),
            (22, "2f5f7e7216832ae19c353023618a35a8"),
            (23, "6535e52506c27eaa1033891ff4f3a74e"),
            (24, "8bd9c8efbbac58748951ca5a45cfd386"),
            (25, "d983c63bf41853056787fe1bb764dbff"),
            (26, "b4f24c1219fb00d081c4020c56263451"),
            (27, "b0ae6708c5e1be10668f57d3916cf423"),
            (28, "ba7bb5ad4dba5bde028703007969cb25"),
            (29, "ea880e16eac1b1488aff8a25d11d6271"),
            (30, "c7172f0903c4919eb232f18ab7a30c42"),
            (31, "e9e77893ba926e732f483282f416ffac"),
            (32, "b4ffcb23737cec315a4a4d1aa2a620ce"),
            (33, "5506a276a0a9acc3093f9169c73cf8c5"),
            (34, "e5a849897d9cc0b25b286c1f0bfb50e3"),
            (35, "f54fa30ea7b26d3e11c54d3c8451bcf0"),
            (36, "07602fe0229e486957081a49e3f06f83"),
            (37, "7c4bba98253ca834bf9ed43fd8b2f959"),
            (38, "cf8df427548bbfdb1e11143fdf008b85"),
            (39, "1431a6895a8f435755395f9ba83e76bf"),
            (40, "30dd5e4cae35ba892cc66d7736723980"),
            (41, "8ee247a1063931bedaf4c2fa3e4e261a"),
            (42, "c32ceee2d2245df8589f94fcda0c9f2c"),
            (43, "f25fa0e071d1f1cdc6632c6b673bccd5"),
            (44, "370491b643e97577f4f74bd88576d1ec"),
            (45, "b292bf16e3aafaf41f19c921068214f8"),
            (46, "52921aae5ccc9b6e8e45853419d0c80f"),
            (47, "f1375be31969155ef76f04741cd861d7"),
            (48, "04605ca542b2d82b9886a4b4b9acfb1c"),
            (49, "fa887ba0fa491faaacbb82bc5fefcd5b"),
            (50, "06470e932ad7c7cedf548b5ccb9d4806"),
            (51, "ad130b245e2dd894267cb0ddc532d169"),
            (52, "a9eeb95053682248608e97d79e89ca82"),
            (53, "cc26a3dc608268b98ecd1f3946c4b718"),
            (54, "33dd62a2df6538daf1cf821d9cde61f9"),
            (55, "6912ee65fff2d9f9ce2508cddf8bcda0"),
            (56, "51fdd1acda72405dfdfa03fcb85896d7"),
            (57, "5320ef4c17ef34a0cf2db763338d25eb"),
            (58, "9f4f41b5cde885f94cfc0e06e78f929d"),
            (59, "e39965bc00ecacd90fd875f77eff499a"),
            (60, "63ed72093ae09e2c8553ee069e63d702"),
            (61, "0d08fc14ac5baa37792377355dbad0ae"),
            (62, "f3cdffe2e160a061754a06dafcfd688b"),
            (63, "48a6295221902e8e0938f773a7185e72"),
            (64, "b2d3f56bc197fd985d5965079b5e7148"),
            (65, "8bd7053801c768420faf816fadba971c"),
            (66, "e58b3261a467f02ba51b215c013df4c3"),
            (67, "73062234b55754c3383480d5ef70dce5"),
            (68, "f752ebd79a813ef27c35bed69e2ee69f"),
            (69, "10907846eb89ef5dc5d4935a09dad0e7"),
            (70, "5f1f5f64b84400fb9ad6d8ecd9c142a0"),
            (71, "3157d7bb98a202b50cf0c437aa216c39"),
            (72, "70e7ade70281b0afcb1d4ed13efc2e25"),
            (73, "0bb96a503b1626c9ab16c1291c663e75"),
            (74, "5bed4126b3c973f685fcf92a738d4dab"),
            (75, "7523c240f2a44e86dd22504ca49f098d"),
            (76, "6710949ed8ae17c44fb77496bedcb2ab"),
            (77, "4a4c43373b9e40035e6e40cba227ce0b"),
            (78, "91977cbcc32cdeaec7a0fa24bb948d6a"),
            (79, "a6a0f1373cf3dbee116df2738d6f544d"),
            (80, "761f6d007f6e5c64c8d161a5ced4e0aa"),
            (81, "d44ea4d5a7074b88883a82f2b4cfbe67"),
            (82, "3097eda5666e2b2723e8949fcff2f244"),
            (83, "ab247a3d9bc600f594d5a6c50b80583f"),
            (84, "b229430e3db2dfdd13aa1da1bac14d5c"),
            (85, "befef62987c6dcdf24febd0bb7cd3678"),
            (86, "bfc3e5c7c461500ff085a66548378e0e"),
            (87, "a5712194537c75f0dd5a5ab3e9ebaf03"),
            (88, "8daac097e9044b85b75999d6c3bccd24"),
            (89, "b8124df21129685597c53a3f606ffd28"),
            (90, "8fbc4d795c22d958248582a8df7332ed"),
            (91, "36d217135db136b2bdf1617d7e9c79ce"),
            (92, "1b3e6271a3a4b663c509a1255027ca99"),
            (93, "a25f596574031ff9c34314c1b1f6bf34"),
            (94, "aca7017e5bb62bfdd5bbfded78c8987a"),
            (95, "8129e53a694add0560b1534b32fe5912"),
            (96, "da0e48224106c7535a4cd8db2ac7b8e3"),
            (97, "cbd4ace3d766d8e44f63e0de8f110f04"),
            (98, "bdc17a0ef2777512cb402c90e9d13e31"),
            (99, "47695ad6af968d6f1cdd2d8c5c87a466"),
            (100, "7acedd1a84a4cfcb6e7a16003242945e"),
            (101, "225489d3d073ac705f7b3ad358eabab2"),
            (102, "301da87a7b2ec27514c3a2789d5dbe49"),
            (103, "16222c503718f1420958133c330fe3f8"),
            (104, "d778ce7f642aa23355948477da4cc11c"),
            (105, "e873c37f8977e200a594b815e1a87ef3"),
            (106, "e8f8f41528d4f855d8fdf4055bbabe2f"),
            (107, "cacf3d3d1e7d21c97d265f64d9864b75"),
            (108, "6bf48f161eff9f7005bd6667f30a5c27"),
            (109, "42e7bb8e780b3b26616ecbcace81fa1a"),
            (110, "225afd8ec21f86f66211adf54afc2e86"),
            (111, "4fad3ab7d8546851ec1bb63ea7e6f5a8"),
            (112, "d1fec2ac3715e791ca5f489f300381b3"),
            (113, "f62807c995735b44699bb8179100ce87"),
            (114, "54050b090344e3284f390806ff716371"),
            (115, "50482241280543b88f7af3fc13d65c65"),
            (116, "4c36f27d4786fe2fb8caac690b6d62f7"),
            (117, "5a0edf0b97977ee5afb3d185b64fb610"),
            (118, "4541055c6675b614d27c537c3bb15675"),
            (119, "1c772251899a7ff007400b888d6b2042"),
            (120, "b7ba1efc6022e9ed272f00b8831e26e6"),
            (121, "b0b2d719a838db877b6d6571a39a1cdc"),
            (122, "800aa956ec16f603ecdba66c2dc6e4cf"),
            (123, "8827d2778287c58a242acd4c549beb31"),
            (124, "cfbc5aa0b61103c1a982d8927b26f575"),
            (125, "a1f5b691f74f566a2be1765731084f8a"),
            (126, "80749be03f5724fa4ca0aef8909379b7"),
            (127, "8402b21e7bc7906493bae0dac017f1f9"),
            (128, "37eff01866ba3f538421b30b7cbefcac"),
            (129, "46f986692847558fc38b0cece591c20f"),
            (130, "7c05c285d0263c40a0437421b387a2a1"),
            (1000, "a24f1e3ef66950e1327f210e3997ba2c"),
            (4096, "a0c16616c91907bd14e999986cf822d5"),
        ];

        for &(len, expected) in KNOWN {
            let data = patterned(len);
            assert_eq!(
                hex(&digest(&data)),
                expected,
                "the one-shot digest of {len} bytes"
            );
            // The streaming form pads through the same `pad`; an irregular chunking must not
            // change the digest.
            let mut md5 = Md5::new();
            for chunk in data.chunks(7) {
                md5.update(chunk);
            }
            assert_eq!(
                hex(&md5.finish()),
                expected,
                "the streaming digest of {len} bytes"
            );
        }

        // The window the C's wrapping subtraction lives in is really covered: `filled = (len + 1) % 64`
        // is `55..=64` exactly for a 54..=63-byte buffered remainder.
        for len in 54..=63usize {
            assert!(
                KNOWN.iter().any(|&(n, _)| n == len),
                "the boundary window is not covered at {len} bytes"
            );
        }
        // ...as is the explicit list the padding discussion names.
        for len in [55, 56, 57, 62, 63, 64, 65, 119, 120, 127, 128, 1000, 4096] {
            assert!(
                KNOWN.iter().any(|&(n, _)| n == len),
                "the pinned length list is not covered at {len} bytes"
            );
        }
    }

    /// `digest` is exactly `init` → `update` → `finish`, at every length.
    #[test]
    fn digest_matches_the_streaming_context() {
        let mut lengths: Vec<usize> = (0..=130).collect();
        lengths.extend([1000, 4096]);
        for len in lengths {
            let data = patterned(len);
            let mut md5 = Md5::new();
            md5.update(&data);
            assert_eq!(md5.finish(), digest(&data), "at {len} bytes");
        }
    }

    /// Any split of the same payload finishes to the same digest.
    #[test]
    fn streaming_splits_are_invariant() {
        let data = patterned(200);
        let expected = digest(&data);
        for split in [0usize, 1, 63, 64, 65, 100, 128, 199, 200] {
            let mut md5 = Md5::new();
            md5.update(&data[..split]);
            md5.update(&data[split..]);
            assert_eq!(md5.finish(), expected, "a split at {split}");
        }
    }

    /// `finish` takes `&self`, so a context is re-finishable and stays usable afterwards.
    #[test]
    fn finish_is_pure_and_repeatable() {
        let first = patterned(100);
        let second = patterned(37);
        let mut md5 = Md5::new();
        md5.update(&first);

        assert_eq!(md5.finish(), md5.finish(), "finishing twice is idempotent");
        assert_eq!(
            md5.buffered_len(),
            100 - 64,
            "finish did not consume the buffer"
        );
        assert_eq!(md5.total_len(), 64, "finish did not hash a block");

        md5.update(&second);
        let mut joined = first.clone();
        joined.extend_from_slice(&second);
        assert_eq!(
            md5.finish(),
            digest(&joined),
            "an update after a finish still counts"
        );
    }

    /// The raw-part round trip the `erlang:md5_final/1` BIF needs, including the C's `len == 64`
    /// state and the verbatim tail of `buffer`.
    #[test]
    fn from_parts_round_trips_the_context_image() {
        // The `len == 64` state the oracle itself produces (`update(&[b'A'; 32])` twice): a full
        // buffer, nothing hashed yet, the IV still in place.
        let md5 = Md5::from_parts(IV, 0, 64, [b'A'; BLOCK_LEN]).unwrap();
        assert_eq!(md5.state(), IV);
        assert_eq!(md5.total_len(), 0);
        assert_eq!(md5.buffered_len(), 64);
        assert_eq!(md5.buffer(), &[b'A'; BLOCK_LEN]);
        assert_eq!(md5.finish(), digest(&[b'A'; 64]));

        // A fresh context is the same thing `new` produces.
        let fresh = Md5::from_parts(IV, 0, 0, [0u8; BLOCK_LEN]).unwrap();
        assert_eq!(fresh, Md5::new());
        assert_eq!(fresh, Md5::default());
        assert_eq!(fresh.finish(), digest(b""));

        // Parts taken from a live context rebuild it exactly — including whether the block was
        // hashed (`len == 0`, `total_len == 64`) or is still buffered.
        let mut hashed = Md5::new();
        hashed.update(&[b'A'; BLOCK_LEN]);
        assert_eq!(hashed.buffered_len(), 0);
        assert_eq!(hashed.total_len(), BLOCK_LEN as u64);
        let rebuilt = Md5::from_parts(
            hashed.state(),
            hashed.total_len(),
            hashed.buffered_len(),
            *hashed.buffer(),
        )
        .unwrap();
        assert_eq!(rebuilt, hashed);
        assert_eq!(rebuilt.finish(), digest(&[b'A'; 64]));

        // Bytes past `buffered_len` are kept verbatim, so a non-zero tail round-trips and does not
        // leak into the digest.
        let mut buffer = [0u8; BLOCK_LEN];
        buffer[..4].copy_from_slice(b"abcd");
        buffer[40..].copy_from_slice(&[0xAA; 24]);
        let md5 = Md5::from_parts(IV, 0, 4, buffer).unwrap();
        assert_eq!(md5.buffer(), &buffer);
        assert_eq!(md5.buffered_len(), 4);
        assert_eq!(md5.finish(), digest(b"abcd"));

        // The out-of-range `len` the C reads past its buffer for.
        assert!(matches!(
            Md5::from_parts(IV, 64, 65, [0u8; BLOCK_LEN]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            Md5::from_parts(IV, 0, usize::MAX, [0u8; BLOCK_LEN]),
            Err(BeamError::BadArg)
        ));
    }

    /// The C's buffering asymmetry: a whole-block update hashes at once, two half-block updates
    /// leave a full and unhashed buffer.
    #[test]
    fn update_keeps_the_c_buffer_asymmetry() {
        let mut md5 = Md5::new();
        md5.update(&[b'A'; BLOCK_LEN]);
        assert_eq!(md5.buffered_len(), 0, "a whole block is hashed immediately");
        assert_eq!(md5.total_len(), BLOCK_LEN as u64);

        let mut md5 = Md5::new();
        md5.update(&[b'A'; 32]);
        md5.update(&[b'B'; 32]);
        assert_eq!(
            md5.buffered_len(),
            BLOCK_LEN,
            "the top-up comparison is `<`, so a full buffer stays unhashed"
        );
        assert_eq!(md5.total_len(), 0);
        let joined = [&[b'A'; 32][..], &[b'B'; 32][..]].concat();
        assert_eq!(md5.finish(), digest(&joined));

        // ...and the single 64-byte update above finishes to the same value as the split form.
        let mut single = Md5::new();
        single.update(&[b'A'; 32]);
        single.update(&[b'B'; 32]);
        let mut whole = vec![b'A'; 32];
        whole.extend_from_slice(&[b'B'; 32]);
        assert_eq!(single.finish(), digest(&whole));
    }

    /// The two public constants are the RFC 1321 structure sizes.
    #[test]
    fn constants_are_the_rfc_sizes() {
        assert_eq!(BLOCK_LEN, 64);
        assert_eq!(DIGEST_LEN, 16);
        assert_eq!(digest(b"").len(), DIGEST_LEN);
    }
}
