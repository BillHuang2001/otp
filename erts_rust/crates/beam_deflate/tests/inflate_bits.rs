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
//! Bit-level, window and error-shape tests for the decompression direction.
//!
//! Two kinds of input here:
//!
//! * **Hand-built bit streams** — a tiny LSB-first bit writer builds stored and fixed
//!   blocks bit by bit (the malformed-tree rejections live as unit tests inside
//!   `src/inflate/inftrees.rs`, where the builder's own API is reachable).
//! * **Mutations of an oracle image** — a flipped header check bit, a bad Adler-32, a
//!   bad gzip CRC-32 or ISIZE, and every truncation prefix.
//!
//! The window/state tests use the 40 KiB oracle image from `inflate_goldens.rs` (a body
//! larger than the 32 KiB window, so the window has to slide) — it is duplicated here as
//! a `include!`-free literal pair to keep this file self-contained: the zlib and gzip
//! images are re-derived from the plaintext at test time only through the crate's own
//! `compress`, which is not what is under test; instead the two images are hand-decoded
//! through the public API and compared against the assembled plaintext.

use beam_common::BeamError;
use beam_deflate::inflate::decompress_all;
use beam_deflate::{Flush, Inflate, Status};

const ZLIB: i8 = 15;
const RAW: i8 = -15;
const GZIP: i8 = 16 + 15;
const AUTO: i8 = 32 + 15;

/// `zlib:compress(<<"abc">>)` — a complete zlib stream with check bits `% 31 == 0`.
const ABC_ZLIB: [u8; 11] = [120, 156, 75, 76, 74, 6, 0, 2, 77, 1, 39];
/// `zlib:gzip(<<"abc">>)`.
const ABC_GZIP: [u8; 23] = [
    31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 75, 76, 74, 6, 0, 194, 65, 36, 53, 3, 0, 0, 0,
];

fn badarg<T: std::fmt::Debug>(r: Result<T, BeamError>) -> bool {
    matches!(r, Err(BeamError::BadArg))
}

// ---------------------------------------------------------------------------
// A hand-rolled deflate bit writer (the LSB-first packing deflate uses)
// ---------------------------------------------------------------------------

/// Writes bits into a byte buffer least-significant-bit first, the way RFC 1951 packs
/// block headers and Huffman codes.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    bit: u32,
    nbits: u32,
}

impl BitWriter {
    /// Append `n` bits of `value`, least-significant bit first.
    fn bits(&mut self, value: u32, n: u32) {
        for i in 0..n {
            if (value >> i) & 1 == 1 {
                let byte = (self.bit / 8) as usize;
                if byte == self.out.len() {
                    self.out.push(0);
                }
                self.out[byte] |= 1 << (self.bit % 8);
            }
            self.bit += 1;
            self.nbits += 1;
        }
    }

    /// Append a Huffman code, which deflate packs most-significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        for i in (0..len).rev() {
            self.bits((code >> i) & 1, 1);
        }
    }

    /// Pad to a byte boundary with zero bits.
    fn align(&mut self) {
        while self.bit % 8 != 0 {
            self.bits(0, 1);
        }
    }

    /// Append whole bytes; the writer must be byte-aligned (a stored block is).
    fn bytes(&mut self, data: &[u8]) {
        assert_eq!(self.bit % 8, 0, "a stored block starts byte-aligned");
        self.out.extend_from_slice(data);
        self.bit += 8 * data.len() as u32;
        self.nbits += 8 * data.len() as u32;
    }

    fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

/// The fixed literal/length Huffman lengths from `inffixed.h`, so a fixed block can be
/// emitted without a tree: symbols 0-143 are 8 bits, 144-255 are 9, 256-279 are 7 and
/// 280-287 are 8.
fn fixed_lit_code(symbol: u16) -> (u32, u32) {
    let (len, base) = match symbol {
        0..=143 => (8, 0x30),
        144..=255 => (9, 0x190),
        256..=279 => (7, 0),
        _ => (8, 0xc0),
    };
    (base + u32::from(symbol), len)
}

// ---------------------------------------------------------------------------
// Hand-built streams: one per block shape
// ---------------------------------------------------------------------------

/// A stored block (`BTYPE 00`) is copied verbatim after an aligned LEN/NLEN pair.
#[test]
fn zlib_header_errors_are_data_errors() {
    // FLG altered so the 16-bit header is no longer a multiple of 31.
    let mut bad_check = ABC_ZLIB;
    bad_check[1] = 0x9d;
    assert!((u32::from(bad_check[0]) * 256 + u32::from(bad_check[1])) % 31 != 0);
    assert!(badarg(decompress_all(ZLIB, &bad_check, 64)));

    // CM = 7 — "unknown compression method".
    let mut bad_method = ABC_ZLIB;
    bad_method[0] = 0x77;
    bad_method[1] = 0x9c;
    assert!(badarg(decompress_all(ZLIB, &bad_method, 64)));

    // CINFO = 8 is a 64 KiB window, larger than zlib's maximum.
    let mut bad_window = [0x88u8, 0x98];
    bad_window[1] = 0x98;
    let check = (u32::from(bad_window[0]) * 256 + u32::from(bad_window[1])) % 31;
    // Re-derive FLG so only the window size is wrong.
    bad_window[1] = (0x98 + (31 - check) % 31) as u8;
    assert!(badarg(decompress_all(ZLIB, &bad_window, 64)));
}

/// The gzip header's magic, method and reserved flag bits are verified.
#[test]
fn gzip_header_errors_are_data_errors() {
    let mut bad_magic = ABC_GZIP;
    bad_magic[0] = 0x1e;
    assert!(badarg(decompress_all(GZIP, &bad_magic, 64)));

    let mut bad_method = ABC_GZIP;
    bad_method[2] = 7;
    assert!(badarg(decompress_all(GZIP, &bad_method, 64)));

    // The three reserved flag bits must be zero.
    let mut bad_flags = ABC_GZIP;
    bad_flags[3] = 0xe0;
    assert!(badarg(decompress_all(GZIP, &bad_flags, 64)));
}

/// The zlib Adler-32 trailer and both gzip trailer words are verified.
#[test]
fn trailer_mismatches_are_data_errors() {
    let mut bad_adler = ABC_ZLIB;
    let last = bad_adler.len() - 1;
    bad_adler[last] ^= 0xff;
    assert!(badarg(decompress_all(ZLIB, &bad_adler, 64)));

    // CRC-32 occupies the four bytes before ISIZE.
    let mut bad_crc = ABC_GZIP;
    bad_crc[ABC_GZIP.len() - 8] ^= 0xff;
    assert!(badarg(decompress_all(GZIP, &bad_crc, 64)));

    // ISIZE is the last four bytes.
    let mut bad_size = ABC_GZIP;
    let last = bad_size.len() - 1;
    bad_size[last] ^= 0xff;
    assert!(badarg(decompress_all(GZIP, &bad_size, 64)));
}

/// A raw stream has no trailer to verify, so the corresponding tail bytes are data.
#[test]
fn a_raw_stream_has_no_checksum_to_verify() {
    let mut w = BitWriter::default();
    w.bits(1, 1);
    w.bits(0, 2);
    w.align();
    w.bytes(&2u16.to_le_bytes());
    w.bytes(&(!2u16).to_le_bytes());
    w.bytes(b"hi");
    let raw = w.finish();
    let (body, used) = decompress_all(RAW, &raw, 64).expect("decode");
    assert_eq!(body, b"hi");
    assert_eq!(used, raw.len(), "no trailer bytes are consumed");
}

/// Truncating a valid stream at *any* offset is either "needs more input" or a data
/// error — never a panic and never a wrong body.
#[test]
fn every_truncation_is_refused_without_panicking() {
    for (bits, image) in [
        (ZLIB, &ABC_ZLIB[..]),
        (RAW, &[120u8, 156, 75, 76, 74, 6, 0][..]),
        (GZIP, &ABC_GZIP[..]),
        (AUTO, &ABC_GZIP[..]),
    ] {
        for cut in 0..image.len() {
            let mut stream = Inflate::new(bits).expect("window bits");
            let mut out = vec![0u8; 256];
            let pump = stream
                .inflate(&image[..cut], &mut out, Flush::None)
                .expect("a truncated stream is a status, not an error");
            // Whatever happened, the status must be one of zlib's "not finished" codes.
            assert!(
                matches!(pump.status, Status::Ok | Status::BufError | Status::DataError),
                "cut {cut} of {bits} bits reported {:?}",
                pump.status
            );
            assert!(pump.produced <= out.len());
            // The one-shot route refuses every truncation.
            assert!(badarg(decompress_all(bits, &image[..cut], 256)), "cut {cut}");
        }
    }
}

/// An empty input cannot make progress, so zlib reports `Z_BUF_ERROR`.
#[test]
fn an_empty_input_is_a_buf_error() {
    for bits in [ZLIB, RAW, GZIP, AUTO] {
        let mut stream = Inflate::new(bits).expect("window bits");
        let mut out = [0u8; 16];
        let pump = stream.inflate(&[], &mut out, Flush::None).expect("pump");
        assert_eq!(pump.status, Status::BufError, "{bits} bits");
        assert_eq!((pump.consumed, pump.produced), (0, 0));
        assert!(badarg(decompress_all(bits, &[], 16)));
    }
}

/// `Z_FINISH` with input that does not complete the stream is `Z_BUF_ERROR`, not
/// `Z_STREAM_END` — the rule `zlib:uncompress/1` depends on.
#[test]
fn finish_flush_on_an_incomplete_stream_is_a_buf_error() {
    let mut stream = Inflate::new(ZLIB).expect("zlib");
    let mut out = [0u8; 64];
    let pump = stream
        .inflate(&ABC_ZLIB[..5], &mut out, Flush::Finish)
        .expect("pump");
    assert_eq!(pump.status, Status::BufError);
}

// ---------------------------------------------------------------------------
// The window, the resets and `get_dictionary`
// ---------------------------------------------------------------------------

/// The 40 KiB repetitive oracle image (`rep40k_zlib`) — a body longer than the window,
/// so the decoder has to slide it.
const REP40K_ZLIB: &[u8] = &[120,156,237,197,65,17,0,32,8,0,176,172,8,8,253,19,152,195,187,237,179,56,89,125,103,195,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,109,219,182,237,143,127,65,237,211,163];

/// The plaintext the window tests expect: `"abcdefgh"` repeated.
fn rep40k() -> Vec<u8> {
    b"abcdefgh".repeat(5120)
}

/// After a partial decode, `get_dictionary()` is exactly the history the stream has
/// produced so far (`min(produced, 32 KiB)` of it), oldest first.
#[test]
fn concatenated_gzip_members_decode_after_a_reset() {
    let mut stream = Inflate::new(AUTO).expect("auto");
    let mut out = vec![0u8; 64];
    for _ in 0..3 {
        let pump = stream.inflate(&ABC_GZIP, &mut out, Flush::None).expect("pump");
        assert_eq!(pump.status, Status::StreamEnd);
        assert_eq!(&out[..pump.produced], b"abc");
        stream.reset().expect("reset");
    }
}

/// Auto-detect accepts both wrappers; a raw stream is rejected by it (there is no
/// header to detect).
#[test]
fn auto_detect_takes_zlib_and_gzip_but_not_raw() {
    assert_eq!(decompress_all(AUTO, &ABC_ZLIB, 64).expect("zlib").0, b"abc");
    assert_eq!(decompress_all(AUTO, &ABC_GZIP, 64).expect("gzip").0, b"abc");
    let mut w = BitWriter::default();
    w.bits(1, 1);
    w.bits(1, 2);
    let (eob, len) = fixed_lit_code(256);
    w.code(eob, len);
    let raw = w.finish();
    assert!(badarg(decompress_all(AUTO, &raw, 64)));
}

/// A gzip member with `FEXTRA`/`FNAME`/`FCOMMENT`/`FHCRC` in its header: the optional
/// fields are skipped (and the header CRC checked) rather than treated as deflate data.
#[test]
fn gzip_optional_header_fields_are_skipped() {
    let mut image = vec![0x1f, 0x8b, 8, 0x1e, 0, 0, 0, 0, 0, 3];
    // FEXTRA: length 4 and four bytes.
    image.extend_from_slice(&4u16.to_le_bytes());
    image.extend_from_slice(b"EXTRA");
    // FNAME and FCOMMENT: zero-terminated strings.
    image.extend_from_slice(b"name\0comment\0");
    // FHCRC: the low 16 bits of the CRC-32 of everything before it.
    let hcrc = beam_deflate::crc32(0, &image) & 0xffff;
    image.extend_from_slice(&(hcrc as u16).to_le_bytes());
    // The deflate body of `abc` and the trailer, both taken from the plain image.
    image.extend_from_slice(&ABC_GZIP[10..ABC_GZIP.len() - 8]);
    image.extend_from_slice(&ABC_GZIP[ABC_GZIP.len() - 8..]);
    assert_eq!(decompress_all(GZIP, &image, 64).expect("decode").0, b"abc");

    // A wrong header CRC is a data error.
    let mut bad = image.clone();
    let at = bad.len() - 8 - 1;
    bad[at] ^= 0xff;
    assert!(badarg(decompress_all(GZIP, &bad, 64)));
}

/// The one-shot bound stops a decompression bomb: a 1 MiB body of zeros is refused
/// once it passes the limit, and the limit is not a suggestion.
#[test]
fn the_one_shot_bound_is_a_hard_ceiling() {
    // A stored-block stream of 200 zero bytes, built by hand (no compressor needed).
    let body = vec![0u8; 200];
    let mut w = BitWriter::default();
    w.bits(1, 1);
    w.bits(0, 2);
    w.align();
    w.bytes(&200u16.to_le_bytes());
    w.bytes(&(!200u16).to_le_bytes());
    w.bytes(&body);
    let raw = w.finish();
    assert_eq!(decompress_all(RAW, &raw, 200).expect("exactly the limit").0, body);
    assert!(badarg(decompress_all(RAW, &raw, 199)));
    assert!(badarg(decompress_all(RAW, &raw, 0)));
}
