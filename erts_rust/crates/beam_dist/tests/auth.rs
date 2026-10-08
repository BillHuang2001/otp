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

//! Integration tests for the `beam_dist::auth` seam: the RFC 1321 MD5, the
//! distribution challenge digest `md5(cookie ++ decimal(challenge))`, the
//! [`Md5Digest`]/[`SystemChallengeSource`] providers, and their wiring into the
//! `beam_dist::handshake` `ChallengeDigest`/`ChallengeSource` seam.
//!
//! The known-answer vectors are cross-checked against the system oracle
//! (OTP 29 / erts-17.1):
//!
//! ```text
//! $ erl -noshell -eval 'io:format("~s",[binary:encode_hex(
//!     erlang:md5(<<"COOKIE","12345">>))])' -s init stop
//! e8326bad89676cdadf29fb538c8f2b0c
//! ```
//!
//! (the `gen_digest(Challenge, Cookie)` body in `lib/kernel/src/dist_util.erl`
//! is `erlang:md5([atom_to_list(Cookie) | integer_to_list(Challenge)])`, i.e.
//! cookie first, then the unsigned decimal challenge).

use beam_common::Result;

use beam_dist::auth::{gen_challenge, gen_digest, md5, verify, Md5Digest, SystemChallengeSource};
use beam_dist::handshake::{verify_digest, ChallengeDigest, ChallengeReply, ChallengeSource};
use beam_dist::transport::Write;

/// A `Vec<u8>`-backed [`Write`] sink so the message codecs run without a socket.
#[derive(Default)]
struct VecWriter(Vec<u8>);

impl Write for VecWriter {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.0.extend_from_slice(buf);
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Lowercase-hex rendering, mirroring the oracle's `binary:encode_hex/1`.
fn hex(digest: &[u8; 16]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The RFC 1321 §A.5 test suite, verbatim.
#[test]
fn md5_rfc1321_vectors() {
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

/// The 56-byte padding boundary (an extra padded block) and a multi-block input,
/// against oracle-derived expectations.
#[test]
fn md5_padding_and_multiblock() {
    assert_eq!(
        hex(&md5(&[b'a'; 55])),
        "ef1772b6dff9a122358552954ad0df65"
    );
    assert_eq!(
        hex(&md5(&[b'a'; 56])),
        "3b0c8ac703f828b04c6c197006d17218"
    );
    assert_eq!(
        hex(&md5(&[b'a'; 64])),
        "014842d480b571495a4a0363793f7367"
    );
    assert_eq!(
        hex(&md5(&[b'a'; 65])),
        "c743a45e0d2e6a95cb859adae0248435"
    );
    assert_eq!(
        hex(&md5(&[b'a'; 1000])),
        "cabe45dcc9ae5b66ba86600cca6b8ba8"
    );
}

/// Oracle-derived `gen_digest` known-answer vectors.
///
/// * `gen_digest(b"COOKIE", 12345)`:
///   `erl -noshell -eval 'io:format("~s",[binary:encode_hex(
///    erlang:md5(<<"COOKIE","12345">>))])' -s init stop`
///   → `e8326bad89676cdadf29fb538c8f2b0c`
/// * `gen_digest(b"COOKIE", 4294967295)` (pins the unsigned `%u` rendering):
///   same command with `<<"COOKIE","4294967295">>`
///   → `88c5fcced016fee71b36aae87ac1b1fe`
/// * `gen_digest(b"mysecretcookie", 1)`:
///   same command with `<<"mysecretcookie","1">>`
///   → `12a44bb116338edf60d04ac85095c6e0`
/// * `gen_digest(b"", 0)` degrades to `md5("0")`
///   → `cfcd208495d565ef66e7dff9f98764da`
#[test]
fn gen_digest_known_answers() {
    assert_eq!(hex(&gen_digest(b"COOKIE", 12345)), "e8326bad89676cdadf29fb538c8f2b0c");
    assert_eq!(
        hex(&gen_digest(b"COOKIE", 4294967295)),
        "88c5fcced016fee71b36aae87ac1b1fe"
    );
    assert_eq!(
        hex(&gen_digest(b"mysecretcookie", 1)),
        "12a44bb116338edf60d04ac85095c6e0"
    );
    assert_eq!(hex(&gen_digest(b"", 0)), "cfcd208495d565ef66e7dff9f98764da");
}

/// Property: `gen_digest` is exactly `md5(cookie ++ challenge.to_string())`.
#[test]
fn gen_digest_is_concatenation() {
    for challenge in [0u32, 1, 9, 10, 99, 100, 65535, 1_000_000, u32::MAX] {
        for cookie in [&b""[..], &b"cookie"[..], &b"a"[..]] {
            let mut joined = cookie.to_vec();
            joined.extend_from_slice(challenge.to_string().as_bytes());
            assert_eq!(gen_digest(cookie, challenge), md5(&joined));
        }
    }
}

/// `Md5Digest` is the concrete `ChallengeDigest`: consistent with `gen_digest`
/// and accepted/rejected correctly by the handshake's `verify_digest` seam.
#[test]
fn md5_digest_implements_challenge_digest() {
    let d = Md5Digest;
    let digest = d.digest(b"COOKIE", 12345);
    assert_eq!(digest, gen_digest(b"COOKIE", 12345));
    assert!(verify_digest(&digest, 12345, b"COOKIE", &d));

    assert!(!verify_digest(&digest, 12345, b"cookie", &d));
    assert!(!verify_digest(&digest, 12346, b"COOKIE", &d));

    let mut flipped = digest;
    flipped[0] ^= 0x80;
    assert!(!verify_digest(&flipped, 12345, b"COOKIE", &d));

    // `Default` constructs the same zero-sized provider.
    assert_eq!(Md5Digest::default().digest(b"k", 7), gen_digest(b"k", 7));
}

/// The `verify` convenience helper agrees with the digest formula.
#[test]
fn verify_helper_agrees() {
    let digest = gen_digest(b"cookie", 42);
    assert!(verify(b"cookie", 42, &digest));
    assert!(!verify(b"cookie", 43, &digest));
    assert!(!verify(b"cookief", 42, &digest));
}

/// `gen_challenge` yields unpredictable, unique `u32` nonces, and the
/// `SystemChallengeSource` provider surfaces them through the `ChallengeSource`
/// seam.
#[test]
fn challenge_nonce_source() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        assert!(seen.insert(gen_challenge()));
    }

    let source = SystemChallengeSource;
    let a = source.next_challenge();
    let b = source.next_challenge();
    assert_ne!(a, b);

    // The nonce is usable as a digest input on both sides of the handshake.
    let digest = Md5Digest.digest(b"nocookie", a);
    assert!(verify_digest(&digest, a, b"nocookie", &Md5Digest));
    assert!(!verify_digest(&digest, b, b"nocookie", &Md5Digest));
}

/// End-to-end: the `Md5Digest` output travels as a `ChallengeReply` body and is
/// verified by the peer with the same cookie and the sender's challenge.
#[test]
fn challenge_reply_round_trip() {
    let our_challenge: u32 = 0xdead_beef;
    let cookie = b"the-cookie";

    let reply = ChallengeReply {
        challenge: SystemChallengeSource.next_challenge(),
        digest: Md5Digest.digest(cookie, our_challenge),
    };

    let mut w = VecWriter::default();
    reply.encode(&mut w).expect("challenge reply encodes");
    assert_eq!(w.0.len(), 21, "1 tag byte + 4 challenge + 16 digest");

    let decoded = ChallengeReply::decode(&w.0).expect("challenge reply decodes");
    assert_eq!(decoded, reply);
    assert!(verify_digest(&decoded.digest, our_challenge, cookie, &Md5Digest));

    // A peer using the wrong cookie fails authentication.
    assert!(!verify_digest(&decoded.digest, our_challenge, b"other-cookie", &Md5Digest));
}
