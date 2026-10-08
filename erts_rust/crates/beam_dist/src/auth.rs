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

//! The distribution challenge/response authentication: MD5 digest and nonce.
//!
//! Owns the concrete, byte-exact implementation of the handshake digest
//! `md5(cookie ++ decimal(challenge))` (ground truth: `dist.c` `gen_digest`,
//! `lib/kernel/src/dist_util.erl` `gen_digest/2`, `lib/erl_interface/src/connect/ei_connect.c`
//! `gen_digest`) behind the [`crate::handshake::ChallengeDigest`] seam, plus the
//! challenge nonce source.
//!
//! # Why an in-tree MD5
//!
//! `beam_dist` is deliberately dependency-light: the handshake only ever needs
//! `MD5(cookie ++ decimal(challenge))` and a nonce. Pulling in the `md-5` crate
//! would drag `digest` + `generic-array` + `typenum` + `block-buffer` into the
//! build and churn the workspace-wide `Cargo.lock`. RFC 1321 is ~120 lines of
//! safe integer arithmetic, so it lives here instead — fully safe Rust, no
//! `unsafe`, and no external dependency.
//!
//! # Digest formula (wire-frozen, contract Part I §A.5)
//!
//! `gen_digest(cookie, challenge) = md5(cookie_bytes ++ decimal(challenge))`
//! — the cookie comes **first**, followed by the *unsigned decimal* rendering of
//! the 32-bit challenge (no leading zeros, no length prefix, no NUL terminator;
//! the cookie is likewise not NUL-terminated).
//!
//! This matches `ei_connect.c:gen_digest`:
//! ```c
//! char chbuf[21];
//! sprintf(chbuf, "%u", challenge);
//! erts_md5_update(&c, (unsigned char *) cookie, (unsigned) strlen(cookie));
//! erts_md5_update(&c, (unsigned char *) chbuf, (unsigned) strlen(chbuf));
//! ```
//! and `dist_util.erl:gen_digest/2`:
//! ```erlang
//! gen_digest(Challenge, Cookie) when is_integer(Challenge), is_atom(Cookie) ->
//!     erlang:md5([atom_to_list(Cookie)|integer_to_list(Challenge)]).
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::handshake::{ChallengeDigest, ChallengeSource};

/// The MD5 per-round left-rotation amounts (RFC 1321 §3.4), four 16-step rounds.
const MD5_SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The MD5 additive constants `K[i] = floor(2^32 * abs(sin(i + 1)))` (RFC 1321 §3.4).
const MD5_K: [u32; 64] = [
    0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee, //
    0xf57c_0faf, 0x4787_c62a, 0xa830_4613, 0xfd46_9501, //
    0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be, //
    0x6b90_1122, 0xfd98_7193, 0xa679_438e, 0x49b4_0821, //
    0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa, //
    0xd62f_105d, 0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8, //
    0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed, //
    0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a, //
    0xfffa_3942, 0x8771_f681, 0x6d9d_6122, 0xfde5_380c, //
    0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70, //
    0x289b_7ec6, 0xeaa1_27fa, 0xd4ef_3085, 0x0488_1d05, //
    0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665, //
    0xf429_2244, 0x432a_ff97, 0xab94_23a7, 0xfc93_a039, //
    0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1, //
    0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1, //
    0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb, 0xeb86_d391,
];

/// The MD5 initial state words (RFC 1321 §3.3).
const MD5_INIT: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];

/// Runs the 64-step MD5 compression function over one 512-bit block.
///
/// The block length is not re-checked: every caller passes exactly one
/// `chunks_exact(64)` slice, and out-of-range indexing would be a logic bug
/// (never reachable from wire data), not an input error.
fn md5_compress(state: &mut [u32; 4], block: &[u8]) {
    let mut m = [0u32; 16];
    for (i, word) in m.iter_mut().enumerate() {
        let o = i * 4;
        *word = u32::from_le_bytes([block[o], block[o + 1], block[o + 2], block[o + 3]]);
    }

    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];

    for i in 0..64 {
        let (f, g) = match i {
            0..=15 => ((b & c) | (!b & d), i),
            16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
            32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
            _ => (c ^ (b | !d), (7 * i) % 16),
        };
        let rotated = a
            .wrapping_add(f)
            .wrapping_add(MD5_K[i])
            .wrapping_add(m[g])
            .rotate_left(MD5_SHIFTS[i]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(rotated);
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

/// Computes the MD5 message digest (RFC 1321) of `data`.
///
/// Fully safe Rust; no allocation, no `unsafe`, and it never panics — any byte
/// string (including the empty one; `data.len()` is only ever converted with a
/// wrapping multiply) is valid input.
///
/// (The RFC 1321 `"abc"` vector:)
///
/// ```
/// use beam_dist::auth::md5;
/// assert_eq!(
///     md5(b"abc"),
///     [
///         0x90, 0x01, 0x50, 0x98, 0x3c, 0xd2, 0x4f, 0xb0,
///         0xd6, 0x96, 0x3f, 0x7d, 0x28, 0xe1, 0x7f, 0x72,
///     ]
/// );
/// ```
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut state = MD5_INIT;

    // Full 512-bit blocks straight from the input.
    let mut blocks = data.chunks_exact(64);
    for block in &mut blocks {
        md5_compress(&mut state, block);
    }

    // RFC 1321 §3.1/§3.2 padding: a single `1` bit, zeros up to the 56-byte
    // boundary, then the 64-bit little-endian *bit* length. A tail of 56..=63
    // bytes needs a second block, hence the 128-byte scratch buffer.
    let rem = blocks.remainder();
    let mut tail = [0u8; 128];
    tail[..rem.len()].copy_from_slice(rem);
    tail[rem.len()] = 0x80;
    let tail_len = if rem.len() <= 55 { 64 } else { 128 };
    let bit_len = (data.len() as u64).wrapping_mul(8);
    tail[tail_len - 8..tail_len].copy_from_slice(&bit_len.to_le_bytes());

    for block in tail[..tail_len].chunks_exact(64) {
        md5_compress(&mut state, block);
    }

    let mut digest = [0u8; 16];
    for (i, word) in state.iter().enumerate() {
        digest[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    digest
}

/// Computes the v6 distribution challenge digest `md5(cookie ++ decimal(challenge))`.
///
/// The cookie bytes are used verbatim (no NUL terminator, no length prefix) and
/// the challenge is rendered as its **unsigned decimal** string
/// (`challenge.to_string()`, equivalent to C's `sprintf("%u", …)`) — see the
/// module docs for the ground truth.
pub fn gen_digest(cookie: &[u8], challenge: u32) -> [u8; 16] {
    let decimal = challenge.to_string();
    let mut buf = Vec::with_capacity(cookie.len() + decimal.len());
    buf.extend_from_slice(cookie);
    buf.extend_from_slice(decimal.as_bytes());
    md5(&buf)
}

/// Verifies a received digest against the one computed over `challenge`.
///
/// The comparison is branch-free over the 16 bytes (XOR-fold), so a mismatch
/// does not leak *where* the digest diverges through timing. Returns `true` only
/// on an exact match.
pub fn verify(cookie: &[u8], challenge: u32, got: &[u8; 16]) -> bool {
    let expected = gen_digest(cookie, challenge);
    let mut diff = 0u8;
    for i in 0..16 {
        diff |= expected[i] ^ got[i];
    }
    diff == 0
}

/// Folds an MD5 digest into a `u32` by XOR-ing its four little-endian words.
///
/// The exact analogue of `ei_connect.c`'s `md_32` helper (a union of
/// `unsigned char c[16]` / `unsigned int x[4]`, returned as
/// `x[0] ^ x[1] ^ x[2] ^ x[3]`), used for the [`gen_challenge`] nonce.
fn md_32(data: &[u8]) -> u32 {
    let d = md5(data);
    u32::from_le_bytes([d[0], d[1], d[2], d[3]])
        ^ u32::from_le_bytes([d[4], d[5], d[6], d[7]])
        ^ u32::from_le_bytes([d[8], d[9], d[10], d[11]])
        ^ u32::from_le_bytes([d[12], d[13], d[14], d[15]])
}

/// Generates a fresh challenge nonce.
///
/// Mirrors the spirit of `ei_connect.c:gen_challenge` / `dist_util.erl:gen_challenge/0`:
/// a blob of per-call entropy (here: wall-clock nanoseconds since the UNIX epoch,
/// a process-global monotonic sequence number and the OS pid) is hashed and
/// folded into a `u32` with [`md_32`]. The value only needs to be *unpredictable
/// and unique per handshake* — the sequence number guarantees uniqueness even if
/// the clock is coarse, and the clock separates successive processes.
///
/// Standard library only (`std::time`, `std::process`, `std::sync::atomic`); it
/// never panics (a clock before the epoch degrades to `0` rather than aborting).
pub fn gen_challenge() -> u32 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);

    let nanos: u128 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = u64::from(std::process::id());

    let mut blob = [0u8; 32];
    blob[..16].copy_from_slice(&nanos.to_le_bytes());
    blob[16..24].copy_from_slice(&seq.to_le_bytes());
    blob[24..32].copy_from_slice(&pid.to_le_bytes());
    md_32(&blob)
}

/// The concrete [`ChallengeDigest`] provider: RFC 1321 MD5 over
/// `cookie ++ decimal(challenge)`.
///
/// Hand this to [`crate::handshake::verify_digest`] (or send its output in a
/// [`crate::handshake::ChallengeReply`] / [`crate::handshake::ChallengeAck`]) to
/// close the authentication loop that [`crate::handshake`] leaves pluggable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Md5Digest;

impl ChallengeDigest for Md5Digest {
    fn digest(&self, cookie: &[u8], challenge: u32) -> [u8; 16] {
        gen_digest(cookie, challenge)
    }
}

/// The concrete [`ChallengeSource`] provider: a fresh [`gen_challenge`] nonce per
/// handshake.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SystemChallengeSource;

impl ChallengeSource for SystemChallengeSource {
    fn next_challenge(&self) -> u32 {
        gen_challenge()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handshake::verify_digest;

    /// Renders a digest as lowercase hex (test-only, mirrors `ei_connect.c:hex`).
    fn hex(digest: &[u8; 16]) -> String {
        let mut s = String::with_capacity(32);
        for b in digest {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('?'));
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('?'));
        }
        s
    }

    /// The canonical RFC 1321 §A.5 test-suite vectors.
    #[test]
    fn rfc1321_suite() {
        assert_eq!(hex(&md5(b"")), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(hex(&md5(b"a")), "0cc175b9c0f1b6a831c399e269772661");
        assert_eq!(hex(&md5(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hex(&md5(b"message digest")),
            "f96b697d7cb7938d525a2f31aaf161d0"
        );
        assert_eq!(
            hex(&md5(b"abcdefghijklmnopqrstuvwxyz")),
            "c3fcd3d76192e4007dfb496cca67e13b"
        );
        assert_eq!(
            hex(&md5(
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"
            )),
            "d174ab98d277d9f5a5611c2c9f419d9f"
        );
        assert_eq!(
            hex(&md5(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            )),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
        assert_eq!(
            hex(&md5(b"The quick brown fox jumps over the lazy dog")),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    /// The 55/56/64/65-byte padding boundary (single-block → two-block tail) and
    /// a long (>64-byte, multi-block) input.
    ///
    /// Expected values from the system oracle (OTP 29 / erts-17.1):
    /// `erl -noshell -eval 'A = binary:copy(<<"a">>,55),
    ///  io:format("~s",[binary:encode_hex(erlang:md5(A))])' -s init stop`
    #[test]
    fn padding_boundaries() {
        let a = |n: usize| vec![b'a'; n];
        assert_eq!(
            hex(&md5(&a(55))),
            "ef1772b6dff9a122358552954ad0df65",
            "55 bytes: 1 + 0x80 + len fits a single padded block"
        );
        assert_eq!(
            hex(&md5(&a(56))),
            "3b0c8ac703f828b04c6c197006d17218",
            "56 bytes: forces a second padded block"
        );
        assert_eq!(
            hex(&md5(&a(64))),
            "014842d480b571495a4a0363793f7367",
            "64 bytes: one full block + an all-padding block"
        );
        assert_eq!(
            hex(&md5(&a(65))),
            "c743a45e0d2e6a95cb859adae0248435",
            "65 bytes: remainder 1"
        );
        assert_eq!(
            hex(&md5(&a(1000))),
            "cabe45dcc9ae5b66ba86600cca6b8ba8",
            "1000 bytes: 15 full blocks + tail"
        );
    }

    /// The digest formula is `md5(cookie ++ decimal(challenge))` — cookie FIRST.
    ///
    /// Expected values from the system oracle (OTP 29 / erts-17.1):
    /// `erl -noshell -eval 'io:format("~s",[binary:encode_hex(
    ///   erlang:md5(<<"COOKIE","12345">>))])' -s init stop`
    #[test]
    fn gen_digest_oracle_vectors() {
        assert_eq!(hex(&gen_digest(b"COOKIE", 12345)), "e8326bad89676cdadf29fb538c8f2b0c");
        assert_eq!(
            hex(&gen_digest(b"COOKIE", 4294967295)),
            "88c5fcced016fee71b36aae87ac1b1fe",
            "u32::MAX pins the unsigned `%u` decimal rendering"
        );
        assert_eq!(
            hex(&gen_digest(b"mysecretcookie", 1)),
            "12a44bb116338edf60d04ac85095c6e0"
        );
        // An empty cookie degrades to `md5("0")` — the well-known digest of "0".
        assert_eq!(hex(&gen_digest(b"", 0)), "cfcd208495d565ef66e7dff9f98764da");
    }

    /// The cookie must not be commutative with the challenge: `md5("x" ++ "0")`
    /// is *not* `md5("0" ++ "x")`.
    #[test]
    fn gen_digest_order_matters() {
        // The cookie comes first: `md5("x" ++ "0")`, never `md5("0" ++ "x")`.
        assert_ne!(gen_digest(b"x", 0), md5(b"0x"));
        // ... and it really is the plain concatenation, not a length-prefixed form.
        assert_eq!(gen_digest(b"COOK", 12345), md5(b"COOK12345"));
    }

    /// Decimal rendering has no leading zeros and no length prefix (`%u`).
    #[test]
    fn gen_digest_decimal_rendering() {
        assert_eq!(gen_digest(b"c", 0), md5(b"c0"));
        assert_eq!(gen_digest(b"c", 7), md5(b"c7"));
        assert_eq!(gen_digest(b"c", 10), md5(b"c10"));
        assert_eq!(gen_digest(b"c", 100), md5(b"c100"));
        assert_eq!(gen_digest(b"c", u32::MAX), md5(b"c4294967295"));
    }

    /// `Md5Digest` is the concrete `ChallengeDigest` and agrees with `gen_digest`.
    #[test]
    fn md5_digest_trait_agrees() {
        let d = Md5Digest;
        assert_eq!(d.digest(b"COOKIE", 12345), gen_digest(b"COOKIE", 12345));
        assert_eq!(d.digest(b"", 0), gen_digest(b"", 0));
        // ... and drives the handshake's `verify_digest` seam end-to-end.
        let digest = d.digest(b"COOKIE", 12345);
        assert!(verify_digest(&digest, 12345, b"COOKIE", &d));
        assert!(!verify_digest(&digest, 12346, b"COOKIE", &d));
        assert!(!verify_digest(&digest, 12345, b"NOTCOOKIE", &d));
    }

    /// The `verify` helper accepts the right digest and rejects a wrong one
    /// (including a single flipped bit).
    #[test]
    fn verify_helper() {
        let good = gen_digest(b"cookie", 4242);
        assert!(verify(b"cookie", 4242, &good));

        let mut bad = good;
        bad[7] ^= 0x01;
        assert!(!verify(b"cookie", 4242, &bad));

        let mut all_flipped = good;
        for b in &mut all_flipped {
            *b = !*b;
        }
        assert!(!verify(b"cookie", 4242, &all_flipped));

        assert!(!verify(b"cookie", 4243, &good));
        assert!(!verify(b"COOKIE", 4242, &good));
    }

    /// `gen_challenge` is unique across calls and non-trivial (not a constant).
    #[test]
    fn gen_challenge_uniqueness() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            assert!(seen.insert(gen_challenge()), "challenge nonces must be unique");
        }
    }

    /// The challenge-source seam yields fresh nonces through the trait object.
    #[test]
    fn system_challenge_source() {
        let src = SystemChallengeSource;
        let first = src.next_challenge();
        let second = src.next_challenge();
        assert_ne!(first, second);
    }

    /// `md_32` folds an MD5 digest (cross-checked against a manual `md5` fold).
    #[test]
    fn md_32_folds_digest() {
        let blob: Vec<u8> = (0u8..32).collect();
        let d = md5(&blob);
        let expected = u32::from_le_bytes([d[0], d[1], d[2], d[3]])
            ^ u32::from_le_bytes([d[4], d[5], d[6], d[7]])
            ^ u32::from_le_bytes([d[8], d[9], d[10], d[11]])
            ^ u32::from_le_bytes([d[12], d[13], d[14], d[15]]);
        assert_eq!(md_32(&blob), expected);
    }

    /// `hex` sanity: it renders the digest the way the oracle's
    /// `binary:encode_hex/1` does (lowercase, 32 chars).
    #[test]
    fn hex_is_lowercase_32_chars() {
        let s = hex(&md5(b"abc"));
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
