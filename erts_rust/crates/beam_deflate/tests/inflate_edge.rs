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
//! The decompression direction's edge cases, every expectation computed **by hand**:
//!
//! * **The extreme distance.** A small fixed-Huffman (BTYPE=01) block emitter (RFC 1951
//!   §3.2.6) builds streams bit by bit: `N` literals, then a length-285 / distance-29
//!   match at exactly 32 768 bytes back (the furthest the distance alphabet reaches),
//!   then end-of-block. The same emitter builds the two ways a distance can be invalid:
//!   one that reaches further back than the bytes decoded so far, and a distance
//!   "symbol" the format does not define (30/31).
//! * **The window after a partial decode.** `get_dictionary` is the last
//!   `min(w_size, produced)` bytes of the body, oldest first — checked against the known
//!   plaintext for a 32 KiB window (zlib and raw framing) and for a 512-byte one, where
//!   the window truncates the tail. The C's trailer quirk (`CHECK` re-bases the output
//!   counter, so a framed decode that reaches the Adler-32 in the same call contributes
//!   *nothing* to the window) is pinned too.
//! * **`inflateReset2` re-framing.** One handle walks raw → zlib → raw → gzip, and each
//!   wrong pairing of stream and framing is a data error (raw bytes that cannot be a zlib
//!   header by construction, so the verdicts are deterministic).
//! * **The status boundaries** the C's `(incnt == 0 && out == 0) || flush == Z_FINISH`
//!   rule produces: a zero-length output buffer, a full one, and a re-call after
//!   `Z_STREAM_END`.
//!
//! Only one test touches the oracle, and only to prove the hand-built maximum-distance
//! stream really is a deflate stream (it decodes it with the system `erl` when one is
//! installed); it prints `SKIP …` and returns otherwise, so this file never needs `erl`.
//!
//! The hand-built streams were validated against the oracle up front, with
//! `zlib:inflateInit(Z,-15)` + `zlib:inflate/2`:
//!
//! * 32 768 literals `i % 251` + length-258/distance-32768 → 33 026 bytes, CRC-32
//!   `654028887`, identical to the assembled `prefix ++ prefix[..258]`;
//! * 10 literals + length-3/distance-100 → `error:data_error`
//!   (zlib's "invalid distance too far back");
//! * distance symbols 30 and 31 → `error:data_error` ("invalid distance code").
mod zsupport;

use beam_common::BeamError;
use beam_deflate::inflate::decompress_all;
use beam_deflate::{Deflate, DeflateConfig, Flush, Inflate, Method, Status};

/// zlib / RFC 1950 (`zlib:compress/1`).
const ZLIB: i8 = 15;
/// raw RFC 1951 (`zlib:zip/1`) with the largest window.
const RAW: i8 = -15;
/// raw RFC 1951 with a 512-byte window.
const RAW_9: i8 = -9;
/// gzip / RFC 1952 (`zlib:gzip/1`) with the largest window.
const GZIP: i8 = 16 + 15;

/// The largest distance deflate can name — 2^15, the window's size (`MAX_WBITS` is 15).
const MAX_DISTANCE: u32 = 32768;

// ---------------------------------------------------------------------------
// A hand-rolled deflate bit writer and the fixed-code tables
// ---------------------------------------------------------------------------

/// Writes bits into a byte buffer least-significant-bit first, the way RFC 1951 packs
/// block headers, extra bits and (once reversed by [`BitWriter::code`]) Huffman codes.
///
/// Every bit allocates its byte, so a stream may end in any number of zero bits — which
/// is what an end-of-block code (seven zero bits) needs.
#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    /// Bits written so far; `out.len() == bit.div_ceil(8)`.
    bit: usize,
}

impl BitWriter {
    /// Append `n` bits of `value`, least-significant bit first.
    fn bits(&mut self, value: u32, n: u32) {
        for i in 0..n {
            let byte = self.bit / 8;
            if byte == self.out.len() {
                self.out.push(0);
            }
            if (value >> i) & 1 == 1 {
                self.out[byte] |= 1 << (self.bit % 8);
            }
            self.bit += 1;
        }
    }

    /// Append a Huffman code, which deflate packs most-significant bit first.
    fn code(&mut self, code: u32, n: u32) {
        for i in (0..n).rev() {
            let bit = (code >> i) & 1;
            self.bits(bit, 1);
        }
    }

    /// Pad to the next byte boundary with zero bits (a stored block's alignment).
    fn align(&mut self) {
        while self.bit % 8 != 0 {
            self.bits(0, 1);
        }
    }

    /// Append whole bytes; the writer must be byte-aligned (a stored block is).
    fn bytes(&mut self, data: &[u8]) {
        assert_eq!(self.bit % 8, 0, "a stored block starts byte-aligned");
        self.out.extend_from_slice(data);
        self.bit += 8 * data.len();
    }

    fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// The fixed literal/length code for `symbol`, as `(code, bits)` — RFC 1951 §3.2.6:
/// symbols 0-143 are 8 bits, 144-255 are 9, 256-279 are 7 and 280-287 are 8.
fn fixed_lit_code(symbol: u16) -> (u32, u32) {
    match symbol {
        0..=143 => (0x30 + u32::from(symbol), 8),
        144..=255 => (0x190 + u32::from(symbol - 144), 9),
        256..=279 => (u32::from(symbol - 256), 7),
        280..=287 => (0xc0 + u32::from(symbol - 280), 8),
        _ => panic!("there is no fixed literal/length code for {symbol}"),
    }
}

/// RFC 1951 §3.2.5's length table, symbols 257..=285: the base length and the number of
/// extra bits that follow the code.
const LENGTH_BASE: [u32; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// RFC 1951 §3.2.5's distance table, symbols 0..=29: the base distance and the number of
/// extra bits. Symbol 29 is the last one — `base + (1 << extra) - 1 == 32768` — so a
/// distance of 32769 has no encoding at all.
const DISTANCE_BASE: [u32; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Encode `length` (`3..=258`) as `(literal/length symbol, extra value, extra bits)`.
fn encode_length(length: u32) -> (u16, u32, u32) {
    for (i, (&base, &extra)) in LENGTH_BASE.iter().zip(LENGTH_EXTRA.iter()).enumerate() {
        if length >= base && length < base + (1 << extra) {
            return (257 + i as u16, length - base, extra);
        }
    }
    panic!("deflate cannot encode a length of {length}");
}

/// Encode `distance` as `(distance symbol, extra value, extra bits)`, or `None` when the
/// format cannot name it (anything past 32768 — the alphabet simply stops).
fn encode_distance(distance: u32) -> Option<(u16, u32, u32)> {
    for (i, (&base, &extra)) in DISTANCE_BASE.iter().zip(DISTANCE_EXTRA.iter()).enumerate() {
        if distance >= base && distance < base + (1 << extra) {
            return Some((i as u16, distance - base, extra));
        }
    }
    None
}

/// Start a stream: ONE final fixed-Huffman block (`BFINAL 1`, `BTYPE 01`).
fn fixed_block() -> BitWriter {
    let mut w = BitWriter::default();
    w.bits(1, 1); // BFINAL
    w.bits(1, 2); // BTYPE = 01: fixed Huffman codes
    w
}

/// Emit a literal byte.
fn write_literal(w: &mut BitWriter, byte: u8) {
    let (code, bits) = fixed_lit_code(u16::from(byte));
    w.code(code, bits);
}

/// Emit a literal/length symbol (`256` is the end-of-block code).
fn write_symbol(w: &mut BitWriter, symbol: u16) {
    let (code, bits) = fixed_lit_code(symbol);
    w.code(code, bits);
}

/// Emit a length/distance pair: both Huffman codes plus their extra bits.
fn write_match(w: &mut BitWriter, length: u32, distance: u32) {
    let (length_symbol, length_extra, length_bits) = encode_length(length);
    write_symbol(w, length_symbol);
    w.bits(length_extra, length_bits);
    let (distance_symbol, distance_extra, distance_bits) = encode_distance(distance)
        .unwrap_or_else(|| panic!("deflate cannot encode a distance of {distance}"));
    // The fixed distance codes are the 5-bit symbol values, sent MSB first.
    w.code(u32::from(distance_symbol), 5);
    w.bits(distance_extra, distance_bits);
}

/// Emit the end-of-block code.
fn write_eob(w: &mut BitWriter) {
    write_symbol(w, 256);
}

/// A raw (`windowBits -15`) stream of ONE final stored block holding `body`.
///
/// Its first byte is `0x01` (final, `BTYPE 00`, five padding bits), so it can never be
/// mistaken for a zlib header — which is what makes the wrong-framing verdicts below
/// deterministic.
fn stored_block(body: &[u8]) -> Vec<u8> {
    assert!(
        body.len() <= 0xffff,
        "one stored block holds at most 65535 bytes"
    );
    let mut w = BitWriter::default();
    w.bits(1, 1); // BFINAL
    w.bits(0, 2); // BTYPE = 00: stored
    w.align();
    w.bytes(&(body.len() as u16).to_le_bytes());
    w.bytes(&(!(body.len() as u16)).to_le_bytes());
    w.bytes(body);
    w.finish()
}

/// The 32 768-byte literal prefix the maximum-distance stream is built from: every byte
/// value in `0..251`, four times over, so a mis-copied match cannot hide.
fn distance_prefix() -> Vec<u8> {
    (0..MAX_DISTANCE).map(|i| (i % 251) as u8).collect()
}

/// The maximum-distance stream and the body it must decode to: `prefix`, then a
/// length-258 match 32 768 bytes back, which copies `prefix[..258]`.
fn maximum_distance_stream() -> (Vec<u8>, Vec<u8>) {
    let prefix = distance_prefix();
    let mut w = fixed_block();
    for &byte in &prefix {
        write_literal(&mut w, byte);
    }
    write_match(&mut w, 258, MAX_DISTANCE);
    write_eob(&mut w);
    let mut expected = prefix.clone();
    expected.extend_from_slice(&prefix[..258]);
    (w.finish(), expected)
}

/// Decode a whole stream through `stream` with a generous output buffer, asserting
/// `Z_STREAM_END` and that every byte of `image` was consumed.
fn decode_all(stream: &mut Inflate, image: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; 1 << 16];
    let mut body = Vec::new();
    let mut consumed = 0usize;
    loop {
        let step = stream
            .inflate(&image[consumed..], &mut buf, Flush::None)
            .expect("pump");
        consumed += step.consumed;
        body.extend_from_slice(&buf[..step.produced]);
        match step.status {
            Status::StreamEnd => break,
            Status::Ok | Status::BufError => {
                assert!(
                    step.consumed != 0 || step.produced != 0,
                    "no progress at {consumed}/{} bytes",
                    image.len()
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(consumed, image.len(), "the stream ends at its last byte");
    body
}

// ---------------------------------------------------------------------------
// The extreme distance
// ---------------------------------------------------------------------------

/// A match at exactly 32 768 bytes back decodes: it is the furthest the window reaches,
/// so `inflate` must copy it rather than report "invalid distance too far back".
#[test]
fn a_match_at_the_maximum_distance_decodes() {
    let (image, expected) = maximum_distance_stream();

    let mut stream = Inflate::new(RAW).expect("raw 15");
    let mut out = vec![0u8; expected.len()];
    let step = stream
        .inflate(&image, &mut out, Flush::None)
        .expect("a status, not an error");
    assert_eq!(
        step.status,
        Status::StreamEnd,
        "a 32768-byte distance is at the window's limit, not past it"
    );
    assert_eq!(step.consumed, image.len(), "the whole image is consumed");
    assert_eq!(
        step.produced,
        expected.len(),
        "a length-258 match is copied whole"
    );
    assert_eq!(out, expected, "the match repeats the first 258 bytes");
    assert!(stream.at_stream_end());

    // The one-shot route (16 KiB output chunks, so the match is served from the window
    // rather than from this call's output) decodes the same body.
    let (body, used) = decompress_all(RAW, &image, expected.len()).expect("the bounded loop");
    assert_eq!(body, expected);
    assert_eq!(used, image.len());

    // The hand-built stream is a real deflate stream: the system `erl` decodes it to the
    // same 33 026 bytes (CRC-32 654028887), as recorded in this file's module docs.
    if zsupport::have_oracle() {
        let oracle = zsupport::oracle_inflate_wbits(&image, RAW);
        assert_eq!(
            oracle.as_deref(),
            Some(&expected[..]),
            "the oracle must decode the hand-built stream to the same body"
        );
    } else {
        println!("SKIP inflate edge oracle cross-check: erl not found");
    }
}

/// 32 768 is a hard ceiling: the distance alphabet ends there, so a longer distance has
/// no encoding at all — and the two 5-bit codes past the last distance symbol are
/// rejected rather than read as distances.
///
/// Oracle verdicts (recorded here so the test needs no `erl`; see the module docs):
/// distance codes 30 and 31 both decode as `error:data_error`.
#[test]
fn the_distance_alphabet_stops_at_32768() {
    let mut lowest = u32::MAX;
    let mut highest = 0;
    for (&base, &extra) in DISTANCE_BASE.iter().zip(DISTANCE_EXTRA.iter()) {
        lowest = lowest.min(base);
        highest = highest.max(base + (1 << extra) - 1);
    }
    assert_eq!(lowest, 1, "the shortest distance is one byte back");
    assert_eq!(
        highest, MAX_DISTANCE,
        "the furthest distance the alphabet can name is one window"
    );

    // The span is closed at both ends: the top of the last symbol has an encoding, the
    // next distance has none at all, so 32769+ cannot appear in a *valid* stream.
    assert_eq!(
        encode_distance(MAX_DISTANCE).map(|_| ()),
        Some(()),
        "32768 is encodable"
    );
    assert_eq!(
        encode_distance(MAX_DISTANCE + 1),
        None,
        "32769 has no encoding"
    );
    assert_eq!(
        encode_distance(0),
        None,
        "a distance of zero has no encoding either"
    );

    // Symbols 30 and 31 exist in the fixed 5-bit code but name nothing: the decoder's
    // "invalid distance code" rejection is what makes 32769+ inexpressible.
    for symbol in [30u32, 31] {
        let mut w = fixed_block();
        write_symbol(&mut w, 257); // a length of 3 comes first
        w.code(symbol, 5); // ... then a distance code with no meaning
        write_eob(&mut w);
        let mut stream = Inflate::new(RAW).expect("raw 15");
        let mut out = [0u8; 16];
        let step = stream
            .inflate(&w.finish(), &mut out, Flush::None)
            .expect("a status, not an error");
        assert_eq!(
            step.status,
            Status::DataError,
            "distance code {symbol} must be rejected"
        );
    }
}

/// A distance that reaches back further than the bytes decoded so far is a data error,
/// and the literals before it are kept — the "too far back" side of the same bound.
///
/// Oracle verdict: the 10-literal + length-3/distance-100 stream decodes as
/// `error:data_error` (zlib's "invalid distance too far back"); the 100-literal contrast
/// below decodes to 103 bytes.
#[test]
fn a_distance_past_the_decoded_bytes_is_a_data_error() {
    let mut w = fixed_block();
    for &byte in b"0123456789" {
        write_literal(&mut w, byte);
    }
    write_match(&mut w, 3, 100); // 100 bytes back, with only 10 bytes of history
    write_eob(&mut w);
    let image = w.finish();

    let mut stream = Inflate::new(RAW).expect("raw 15");
    let mut out = [0u8; 64];
    let step = stream
        .inflate(&image, &mut out, Flush::None)
        .expect("a malformed stream is a status, not an error");
    assert_eq!(step.status, Status::DataError);
    assert_eq!(
        &out[..step.produced],
        b"0123456789",
        "the literals before the bad match are kept"
    );
    assert!(
        !stream.at_stream_end(),
        "a data error is not the end of the stream"
    );
    assert!(matches!(
        decompress_all(RAW, &image, 64),
        Err(BeamError::BadArg)
    ));

    // The same distance is legal once the history is there: 100 literals, then 100 back.
    let mut w = fixed_block();
    for i in 0..100u32 {
        write_literal(&mut w, (i % 251) as u8);
    }
    write_match(&mut w, 3, 100);
    write_eob(&mut w);
    let mut stream = Inflate::new(RAW).expect("raw 15");
    let mut out = [0u8; 128];
    let step = stream
        .inflate(&w.finish(), &mut out, Flush::None)
        .expect("pump");
    assert_eq!(step.status, Status::StreamEnd);
    assert_eq!(step.produced, 103);
    assert_eq!(
        &out[100..103],
        &out[..3],
        "the match repeats the 100-byte prefix"
    );
}

// ---------------------------------------------------------------------------
// The window, as `inflateGetDictionary` reports it
// ---------------------------------------------------------------------------

/// After a partial decode `get_dictionary` is exactly the history the stream has
/// produced — oldest first, and hand-computed from the known body for both framings.
#[test]
fn the_dictionary_after_a_partial_decode_is_the_produced_prefix() {
    let plain = zsupport::mixed_kib();
    for (bits, config) in [(ZLIB, DeflateConfig::zlib(6)), (RAW, DeflateConfig::raw(6))] {
        let image = Deflate::compress_with(config, &plain).expect("compress");
        let mut stream = Inflate::new(bits).expect("window bits");
        let mut buf = vec![0u8; plain.len()];
        let half = image.len() / 2;
        let step = stream
            .inflate(&image[..half], &mut buf, Flush::None)
            .expect("pump");
        assert_ne!(
            step.status,
            Status::StreamEnd,
            "{bits}: half an image is not the end"
        );
        let produced = &buf[..step.produced];
        assert!(
            !produced.is_empty(),
            "{bits}: the first half decodes something"
        );
        assert!(
            produced.len() < plain.len(),
            "{bits}: the first half must not decode everything"
        );
        assert_eq!(
            produced,
            &plain[..produced.len()],
            "{bits}: the prefix is exact"
        );
        // Nothing has been forgotten yet: 4 KiB of history is smaller than one window.
        assert_eq!(
            stream.get_dictionary(),
            produced,
            "{bits}: the whole history, in stream order"
        );
    }

    // A short window truncates the history *mid-stream* too: `inflateGetDictionary`
    // returns the last `min(w_size, produced)` bytes, so once more than 512 bytes have
    // been produced a 512-byte window has forgotten everything before the tail. A stored
    // block drained through a fixed-size output buffer makes every step exact.
    //
    // Oracle verdicts (`inflateInit(Z,-9)` + a 5-byte block header and then 100/512/388
    // body bytes, reading `inflateGetDictionary/1` after each): 100 bytes, then exactly
    // `body[100..612]`, then exactly `body[488..1000]`.
    let body: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    let image = stored_block(&body);
    let mut stream = Inflate::new(RAW_9).expect("raw 9");
    let mut out = vec![0u8; 512];
    let mut got: Vec<u8> = Vec::new();
    let mut fed = 0usize;

    // 100 bytes: fewer than the window holds, so nothing is forgotten yet.
    let step = stream
        .inflate(&image, &mut out[..100], Flush::None)
        .expect("pump");
    fed += step.consumed;
    got.extend_from_slice(&out[..step.produced]);
    assert_eq!(step.produced, 100, "the output buffer bounds the copy");
    assert_eq!(got, &body[..100]);
    assert_eq!(
        stream.get_dictionary(),
        &got[..],
        "100 bytes of history fit in a 512-byte window"
    );

    // 612 bytes: the window now holds only its last 512 — `body[100..612]`.
    let step = stream
        .inflate(&image[fed..], &mut out[..512], Flush::None)
        .expect("pump");
    fed += step.consumed;
    got.extend_from_slice(&out[..step.produced]);
    assert_eq!(step.produced, 512, "the second call fills the buffer");
    assert_eq!(got, &body[..612]);
    assert_eq!(
        stream.get_dictionary(),
        &body[100..612],
        "a 512-byte window keeps only the last 512 of 612 bytes produced"
    );

    // The rest of the body: 1000 bytes produced, so the window is `body[488..1000]`.
    let step = stream
        .inflate(&image[fed..], &mut out[..512], Flush::None)
        .expect("pump");
    fed += step.consumed;
    got.extend_from_slice(&out[..step.produced]);
    assert_eq!(
        step.status,
        Status::StreamEnd,
        "1000 bytes is the whole body"
    );
    assert_eq!(
        fed,
        image.len(),
        "the stored block is consumed to its last byte"
    );
    assert_eq!(got, &body[..], "the stored block decodes to its body");
    assert_eq!(
        stream.get_dictionary(),
        &body[488..],
        "the window is the tail of the produced stream, oldest first"
    );
}

/// The window keeps its last `w_size` bytes, so a 512-byte window truncates the tail;
/// and a framed decode that reaches the Adler-32 contributes nothing to it, because the
/// C's `CHECK` state re-bases its output counter before `updatewindow` runs.
#[test]
fn the_window_truncates_the_tail_and_the_trailer_is_not_windowed() {
    let plain = zsupport::mixed_kib();
    assert!(plain.len() > 512);

    // A raw stream has no trailer, so one call windows the whole body: the last 512
    // bytes of a 512-byte window (`zlib:inflateGetDictionary/1` reports 512 here).
    let raw9 = Deflate::compress_with(
        DeflateConfig::new(6, Method::Deflated, RAW_9, 8, Default::default()),
        &plain,
    )
    .expect("compress");
    let mut stream = Inflate::new(RAW_9).expect("raw 9");
    assert_eq!(
        decode_all(&mut stream, &raw9),
        plain,
        "the 9-bit window decodes it all"
    );
    assert_eq!(
        stream.get_dictionary(),
        &plain[plain.len() - 512..],
        "a 512-byte window keeps only the tail, oldest first"
    );

    // A 32 KiB window keeps the whole 4 KiB body of a raw stream (the oracle reports
    // 4096 for this shape and 0 for the framed one).
    let raw = Deflate::compress_with(DeflateConfig::raw(6), &plain).expect("compress");
    let mut stream = Inflate::new(RAW).expect("raw 15");
    assert_eq!(decode_all(&mut stream, &raw), plain);
    assert_eq!(
        stream.get_dictionary(),
        &plain[..],
        "less than a window keeps everything"
    );

    // Everything but the four trailer bytes: the call leaves in `CHECK` before the
    // re-base, so the whole body is windowed (the oracle's dictionary is 4096 bytes).
    let zlib = Deflate::compress_with(DeflateConfig::zlib(6), &plain).expect("compress");
    let mut stream = Inflate::new(ZLIB).expect("zlib 15");
    let mut buf = vec![0u8; plain.len()];
    let step = stream
        .inflate(&zlib[..zlib.len() - 4], &mut buf, Flush::None)
        .expect("pump");
    assert_eq!(&buf[..step.produced], &plain[..]);
    assert_eq!(
        stream.get_dictionary(),
        &plain[..],
        "no trailer means no re-base"
    );

    // ... while a framed decode that takes the Adler-32 in the same call windows nothing
    // (the oracle's dictionary after a one-shot `zlib:inflate/2` is empty).
    let mut stream = Inflate::new(ZLIB).expect("zlib 15");
    assert_eq!(decode_all(&mut stream, &zlib), plain);
    assert_eq!(
        stream.get_dictionary(),
        b"",
        "the trailer re-base leaves this call's output out of the window"
    );
}

// ---------------------------------------------------------------------------
// `inflateReset2` re-framing
// ---------------------------------------------------------------------------

/// One handle is re-framed through raw → zlib → raw → gzip with `inflateReset2`, and each
/// wrong pairing of stream and framing is a data error that a further reset recovers from.
#[test]
fn reset_with_window_bits_reframes_the_same_handle() {
    let raw_plain = b"raw payload";
    let raw = Deflate::compress_with(DeflateConfig::raw(6), raw_plain).expect("raw");
    let zlib_plain = b"zlib payload";
    let zlib = Deflate::compress_with(DeflateConfig::zlib(6), zlib_plain).expect("zlib");
    let gzip_plain = b"gzip payload";
    let gzip = Deflate::compress_with(DeflateConfig::gzip(6), gzip_plain).expect("gzip");

    let mut stream = Inflate::new(RAW).expect("raw 15");
    assert_eq!(stream.window_bits(), RAW);
    assert_eq!(decode_all(&mut stream, &raw), raw_plain);

    // `inflateReset2` to the zlib framing: the bits follow, the history is forgotten and a
    // zlib stream decodes on the very same handle.
    stream.reset_with_window_bits(ZLIB).expect("zlib 15");
    assert_eq!(stream.window_bits(), ZLIB);
    assert_eq!(stream.get_dictionary(), b"", "a reset forgets the window");
    assert!(!stream.at_stream_end());
    assert_eq!(decode_all(&mut stream, &zlib), zlib_plain);

    // ... and back to raw.
    stream.reset_with_window_bits(RAW).expect("raw 15");
    assert_eq!(stream.window_bits(), RAW);
    assert_eq!(decode_all(&mut stream, &raw), raw_plain);

    // ... and on to gzip (`16 + 15`).
    stream.reset_with_window_bits(GZIP).expect("gzip 15");
    assert_eq!(stream.window_bits(), GZIP);
    assert_eq!(decode_all(&mut stream, &gzip), gzip_plain);

    // Wrong framing: a gzip member is not a zlib stream (its header check fails), and a
    // zlib stream is not a gzip member (there is no gzip magic to find).
    let mut out = vec![0u8; 64];
    stream.reset_with_window_bits(ZLIB).expect("zlib 15");
    let step = stream.inflate(&gzip, &mut out, Flush::None).expect("pump");
    assert_eq!(step.status, Status::DataError, "a gzip member at zlib bits");
    stream.reset_with_window_bits(GZIP).expect("gzip 15");
    let step = stream.inflate(&zlib, &mut out, Flush::None).expect("pump");
    assert_eq!(step.status, Status::DataError, "a zlib stream at gzip bits");

    // Raw bytes under the zlib framing: a stored block starts with `0x01`, whose CM field
    // is 1 (not `Z_DEFLATED`), so the header is rejected rather than guessed at.
    let stored = stored_block(b"hi");
    assert_eq!(stored[0], 0x01);
    stream.reset_with_window_bits(ZLIB).expect("zlib 15");
    let step = stream
        .inflate(&stored, &mut out, Flush::None)
        .expect("pump");
    assert_eq!(
        step.status,
        Status::DataError,
        "a raw stored block at zlib bits"
    );

    // A data error is recoverable: `inflateReset2` re-arms the handle for a good stream.
    stream.reset_with_window_bits(ZLIB).expect("zlib 15");
    assert_eq!(decode_all(&mut stream, &zlib), zlib_plain);
    // A rejected window-bits value changes nothing: `inflateReset2` validates the bits
    // before it touches the state, so the framing and the finished stream are both kept.
    assert!(matches!(
        stream.reset_with_window_bits(17),
        Err(BeamError::BadArg)
    ));
    assert_eq!(
        stream.window_bits(),
        ZLIB,
        "a rejected reset keeps the old framing"
    );
    assert!(stream.at_stream_end(), "and the old stream state");
    let step = stream.inflate(&zlib, &mut out, Flush::None).expect("pump");
    assert_eq!(step.status, Status::StreamEnd, "still the finished stream");
    assert_eq!((step.consumed, step.produced), (0, 0));
    // A valid reset then re-arms it for another round.
    stream.reset_with_window_bits(ZLIB).expect("zlib 15");
    assert_eq!(decode_all(&mut stream, &zlib), zlib_plain);
}

// ---------------------------------------------------------------------------
// Status boundaries: the C's "(no progress) || Z_FINISH" rule
// ---------------------------------------------------------------------------
//
// `inflate_bits` already pins `Z_FINISH` on a *truncated* stream and `inflate_goldens`
// the first zero-length output buffer after a decode has started, so neither is repeated
// here: what follows is the fresh-handle half of the same rule (a header is progress), the
// stored block's copy state, a *full* buffer with input left, and a re-call after the end.
/// A zero-length output buffer cannot produce anything; the C only calls that
/// `Z_BUF_ERROR` once a call makes no progress at all, so consuming the header still
/// counts (and is reported as `Z_OK`).
#[test]
fn a_zero_length_output_makes_no_progress_and_is_a_buf_error() {
    let plain = b"abcdefgh";
    let zlib = Deflate::compress_with(DeflateConfig::zlib(6), plain).expect("zlib");
    let mut stream = Inflate::new(ZLIB).expect("zlib 15");
    let step = stream.inflate(&zlib, &mut [], Flush::None).expect("pump");
    assert_eq!(step.produced, 0);
    assert!(step.consumed > 0, "the two-byte header is progress");
    assert_eq!(
        step.status,
        Status::Ok,
        "consuming the header is not a buffer error"
    );
    let started = step.consumed;

    // The second call has output-only work left, so it can make no progress at all:
    // `Z_BUF_ERROR`, and it consumes nothing.
    let step = stream
        .inflate(&zlib[started..], &mut [], Flush::None)
        .expect("pump");
    assert_eq!(step.status, Status::BufError);
    assert_eq!((step.consumed, step.produced), (0, 0));

    // An empty input with an empty output is the same case.
    let step = stream.inflate(&[], &mut [], Flush::None).expect("pump");
    assert_eq!(step.status, Status::BufError);
    assert_eq!((step.consumed, step.produced), (0, 0));

    // The stream is untouched by the failed calls: a normal call still finishes it.
    let mut out = vec![0u8; plain.len()];
    let step = stream
        .inflate(&zlib[started..], &mut out, Flush::None)
        .expect("pump");
    assert_eq!(step.status, Status::StreamEnd);
    assert_eq!(&out[..step.produced], plain);

    // A raw stored block has no header to consume, so its *first* empty-output call
    // already runs to the copy and stops: `Z_OK` for the five bytes it advanced over,
    // then `Z_BUF_ERROR`.
    let image = stored_block(b"stored");
    let mut stream = Inflate::new(RAW).expect("raw 15");
    let step = stream.inflate(&image, &mut [], Flush::None).expect("pump");
    assert_eq!(step.status, Status::Ok);
    assert_eq!(step.produced, 0);
    assert_eq!(step.consumed, 5, "BFINAL/BTYPE, alignment, LEN and NLEN");
    let step = stream
        .inflate(&image[step.consumed..], &mut [], Flush::None)
        .expect("pump");
    assert_eq!(step.status, Status::BufError);
    assert_eq!((step.consumed, step.produced), (0, 0));
}

/// A full output buffer with input left is a success (not a buffer error): the stream is
/// merely waiting for somewhere to write. Under `Z_FINISH` the same call is a
/// `Z_BUF_ERROR`, because the flush asked for the end of the stream.
#[test]
fn a_full_output_buffer_with_more_to_produce_is_ok() {
    let body = vec![0x5au8; 200];
    let image = stored_block(&body);

    let mut stream = Inflate::new(RAW).expect("raw 15");
    let mut out = [0u8; 50];
    let step = stream.inflate(&image, &mut out, Flush::None).expect("pump");
    assert_eq!(step.produced, 50, "the output buffer is filled");
    assert!(step.consumed < image.len(), "input is left over");
    assert_eq!(step.status, Status::Ok, "a full output is not an error");
    assert!(!stream.at_stream_end());
    // Draining it yields the rest of the body.
    let mut body_out = out.to_vec();
    let mut consumed = step.consumed;
    loop {
        let step = stream
            .inflate(&image[consumed..], &mut out, Flush::None)
            .expect("pump");
        consumed += step.consumed;
        body_out.extend_from_slice(&out[..step.produced]);
        match step.status {
            Status::StreamEnd => break,
            Status::Ok | Status::BufError => {
                assert!(step.consumed != 0 || step.produced != 0, "progress");
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(body_out, body);

    // `Z_FINISH` with a buffer too small to finish in: `Z_BUF_ERROR`, even though this
    // call did make progress (the C's `flush == Z_FINISH` arm of the same rule).
    let mut stream = Inflate::new(RAW).expect("raw 15");
    let mut out = [0u8; 50];
    let step = stream
        .inflate(&image, &mut out, Flush::Finish)
        .expect("pump");
    assert_eq!(step.produced, 50);
    assert_eq!(step.status, Status::BufError);
    // With enough room the same flush reports the end of the stream.
    let mut stream = Inflate::new(RAW).expect("raw 15");
    let mut out = vec![0u8; body.len()];
    let step = stream
        .inflate(&image, &mut out, Flush::Finish)
        .expect("pump");
    assert_eq!(step.status, Status::StreamEnd);
    assert_eq!(&out[..step.produced], &body[..]);
}

/// Once the stream has ended the handle keeps answering `Z_STREAM_END` — the C reaches
/// `DONE` without touching the input, and the no-progress rule only rewrites `Z_OK`.
#[test]
fn inflating_again_after_stream_end_reports_stream_end() {
    let plain = b"the end";
    let zlib = Deflate::compress_with(DeflateConfig::zlib(6), plain).expect("zlib");
    let mut stream = Inflate::new(ZLIB).expect("zlib 15");
    let mut out = vec![0u8; plain.len()];
    let step = stream.inflate(&zlib, &mut out, Flush::None).expect("pump");
    assert_eq!(step.status, Status::StreamEnd);
    assert_eq!(&out[..step.produced], plain);
    assert!(stream.at_stream_end());

    // Again, with nothing to offer: `Z_STREAM_END`, nothing consumed or produced.
    let step = stream.inflate(&[], &mut out, Flush::None).expect("pump");
    assert_eq!(step.status, Status::StreamEnd);
    assert_eq!((step.consumed, step.produced), (0, 0));
    // Again, with a whole new stream to offer: the finished stream does not restart.
    let step = stream.inflate(&zlib, &mut out, Flush::None).expect("pump");
    assert_eq!(step.status, Status::StreamEnd);
    assert_eq!((step.consumed, step.produced), (0, 0));

    // `inflateReset` is what makes the same bytes decodable again.
    stream.reset().expect("inflateReset");
    assert!(!stream.at_stream_end());
    let step = stream.inflate(&zlib, &mut out, Flush::None).expect("pump");
    assert_eq!(step.status, Status::StreamEnd);
    assert_eq!(&out[..step.produced], plain);
}
