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
//! The decompression direction — zlib's `inflate.c`/`inftrees.c`/`inffast.c`.
//!
//! [`Inflate`] is `inflateInit2`'s stream: [`Inflate::inflate`] is one `inflate()` pump
//! step and [`decompress_all`] is the bounded one-shot loop the ETF `COMPRESSED` and
//! `zlib:uncompress/1` paths use.
//!
//! # Layout
//!
//! | Module | The C it ports |
//! |---|---|
//! | [`pump`] | `inflate.c`'s `inflate()`, `inflateSetDictionary` (see the module docs for the `#ifdef GUNZIP` header/trailer states) |
//! | [`inftrees`] | `inftrees.c`'s `inflate_table()` — the canonical-Huffman table builder |
//! | [`inffixed`] | `inffixed.h` — the fixed-code tables, as data |
//! | [`inffast`] | `inffast.c`'s `inflate_fast()` — the bulk literal/match decoder |
//! | [`window`] | `updatewindow()` / `inflateGetDictionary()` — the 32 KiB sliding window |
//! | [`state`] | `inflate.h`'s `struct inflate_state` + `inflateReset`/`inflateReset2` |
//!
//! # What is exact, and what is a typed error
//!
//! Every framing zlib accepts is ported: zlib/RFC 1950 (`window_bits` `8..=15`, and
//! `0` for "take the window size from the header"), raw RFC 1951 (`-15..=-8`), gzip /
//! RFC 1952 (`16 + 8 ..= 16 + 15`, with the `FEXTRA`/`FNAME`/`FCOMMENT`/`FHCRC`
//! fields and the CRC-32/ISIZE trailer all verified), and auto-detect
//! (`32 + 8 ..= 32 + 15`). Every accepted `window_bits` goes through
//! [`pump::inflate`]'s single state machine, so stored/fixed/dynamic blocks, the
//! preset-dictionary protocol, all six [`Flush`] modes and the C's `Z_BUF_ERROR` rules
//! are the same code for all four framings. Anything `inflateReset2` rejects
//! (`-7..=-1`, `17..=23`, `33..=39`, `|window_bits| > 47`) is
//! [`beam_common::BeamError::BadArg`] at [`Inflate::new`] — the C's `Z_STREAM_ERROR`,
//! which is the same set.

use beam_common::{BeamError, Result};

use crate::{Flush, Pump, Status};

mod inffast;
mod inffixed;
mod inftrees;
mod pump;
mod state;
mod window;

use state::{InflateState, Mode};

/// Output bytes [`decompress_all`] offers per pump step.
///
/// Any value works (the loop grows the body as needed); this one is the `zlib` NIF
/// layer's `INFLATE_OUT_CHUNKSIZE` (16 KiB), so the one-shot route exercises the same
/// output-buffer sizes the streaming route sees.
const OUT_CHUNK: usize = 16 * 1024;

/// An `inflate` stream (`z_stream` in `inflate.c`'s `HEAD`/`TYPE`/`LEN`/… state machine).
pub struct Inflate {
    state: InflateState,
    window_bits: i8,
}

impl Inflate {
    /// `inflateInit2_` — `window_bits` is `8..=15` (zlib), `-15..=-8` (raw RFC 1951),
    /// `16 + 8 ..= 16 + 15` (gzip) or `32 + 8 ..= 32 + 15` (auto-detect zlib/gzip).
    ///
    /// `0` is also accepted and means "take the window size from the zlib header"
    /// (`16` does the same for a gzip member, defaulting to 15). Everything outside
    /// that set is [`BeamError::BadArg`] — the C's `Z_STREAM_ERROR` from
    /// `inflateReset2`, which rejects the same values.
    pub fn new(window_bits: i8) -> Result<Inflate> {
        let mut state = InflateState::new();
        state.reset_with_window_bits(window_bits)?;
        Ok(Inflate { state, window_bits })
    }

    /// One `inflate()` pump step: consume at most `input.len()` bytes, write at most
    /// `output.len()` bytes, and report zlib's status plus both counts.
    ///
    /// Stream-level failures are in [`Pump::status`], not `Err`: a malformed stream is
    /// [`Status::DataError`], a call that could not make progress is
    /// [`Status::BufError`], and a stream that needs its preset dictionary is
    /// [`Status::NeedDict`] carrying the Adler-32 to check the caller's dictionary
    /// against. `Err` is returned only for a configuration the stream was never able
    /// to accept, which cannot happen after [`Inflate::new`].
    pub fn inflate(&mut self, input: &[u8], output: &mut [u8], flush: Flush) -> Result<Pump> {
        Ok(pump::inflate(&mut self.state, input, output, flush))
    }

    /// `inflateReset` — reuse the stream, keeping the window bits.
    ///
    /// The window buffer is kept for reuse but its contents are forgotten (`whave`
    /// is zeroed), exactly as the C does, so the next stream starts with no history.
    pub fn reset(&mut self) -> Result<()> {
        self.state.reset();
        Ok(())
    }

    /// `inflateReset2` — reuse the stream with new window bits.
    ///
    /// The accepted set is [`Inflate::new`]'s; the window buffer is dropped when the
    /// window size changes (the C frees it in that case).
    pub fn reset_with_window_bits(&mut self, window_bits: i8) -> Result<()> {
        self.state.reset_with_window_bits(window_bits)?;
        self.window_bits = window_bits;
        Ok(())
    }

    /// `inflateSetDictionary` — install the preset dictionary `NeedDict` asked for.
    ///
    /// For a framed stream this is only legal while the stream is waiting for it
    /// (`Status::NeedDict` was reported), and the dictionary's Adler-32 must match the
    /// one the header carried; otherwise it is [`BeamError::BadArg`] (the C's
    /// `Z_STREAM_ERROR` / `Z_DATA_ERROR` — a rejected configuration, not a stream
    /// status). A raw (`-15`) stream accepts a dictionary at any point before its
    /// first block, as the C does.
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> Result<()> {
        pump::set_dictionary(&mut self.state, dictionary)
    }

    /// `inflateGetDictionary` — the bytes a following stream would need.
    ///
    /// Up to `w_size` bytes, oldest first (the C's ring order, which the mirrored
    /// window makes one contiguous slice), and fewer while the history is shorter.
    pub fn get_dictionary(&self) -> &[u8] {
        window::dictionary(&self.state)
    }

    /// Whether the deflate data (and its trailer) has been fully consumed — the C's
    /// `state->mode` being at or past `CHECK`, i.e. `Z_STREAM_END` is what the next
    /// (or last) call reports.
    pub fn at_stream_end(&self) -> bool {
        matches!(self.state.mode, Mode::Check | Mode::Length | Mode::Done)
    }

    /// The window bits this stream was created with (or last reset to).
    ///
    /// Note that this is the *requested* value: `window_bits` `0`/`16` means "from the
    /// header", and the effective size lives in the stream state.
    pub fn window_bits(&self) -> i8 {
        self.window_bits
    }
}

/// Inflate `input` to the end of the stream, refusing to write more than `limit` bytes;
/// returns the body and the number of **input** bytes the stream consumed.
///
/// This is the bounded one-shot loop: `inflateInit2(window_bits)`, repeated
/// `inflate(…, Flush::None)` into a growing buffer until `StreamEnd`, then `inflateEnd`.
///
/// * `window_bits` selects the framing exactly as [`Inflate::new`] does, so this is
///   the `zlib:uncompress/1` shape for `15`, the raw `zlib:zip` shape for `-15` and
///   the `zlib:gunzip/1` shape for `16 + 15`.
/// * `limit` is a hard ceiling: the body is checked after every pump step, so a
///   crafted stream can exceed it by at most one output chunk before the call fails,
///   and never drives an unbounded allocation.
/// * `consumed` excludes any bytes that follow the stream in `input` — what
///   `binary_to_term/2`'s `used` option reports for a `COMPRESSED` image.
///
/// A truncated stream, a bad header, a corrupt block or a bad trailer is
/// [`BeamError::BadArg`], as is exceeding `limit`.
pub fn decompress_all(window_bits: i8, input: &[u8], limit: usize) -> Result<(Vec<u8>, usize)> {
    let mut stream = Inflate::new(window_bits)?;
    let mut body: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; OUT_CHUNK];
    let mut consumed = 0usize;
    loop {
        // Offer one byte past the remaining budget so that "there is more output than
        // the limit allows" is detectable without ever growing `body` past `limit + 1`.
        let room = limit.saturating_sub(body.len()).saturating_add(1);
        let offer = room.min(chunk.len());
        let pump = stream.inflate(&input[consumed..], &mut chunk[..offer], Flush::None)?;
        consumed += pump.consumed;
        body.extend_from_slice(&chunk[..pump.produced]);
        if body.len() > limit {
            return Err(BeamError::BadArg);
        }
        match pump.status {
            Status::StreamEnd => return Ok((body, consumed)),
            Status::Ok => {
                if pump.consumed == 0 && pump.produced == 0 {
                    // Cannot happen with `Flush::None` (the C reports `Z_BUF_ERROR`
                    // instead), but a lost-progress loop would hang, so refuse.
                    return Err(BeamError::BadArg);
                }
            }
            // `Z_BUF_ERROR` (truncated / no progress), `Z_DATA_ERROR` (bad stream),
            // `Z_NEED_DICT` (this entry point has no way to supply one),
            // `Z_STREAM_ERROR` and `Z_MEM_ERROR`.
            _ => return Err(BeamError::BadArg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checksum::adler32;

    /// The zlib image of `b"hello"` at level 0 — a stored block with the wrapper and
    /// the Adler-32 trailer.
    const HELLO_ZLIB: [u8; 16] = [
        0x78, 0x01, 0x01, 0x05, 0x00, 0xfa, 0xff, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x06, 0x2c, 0x02,
        0x15,
    ];

    /// The one-shot loop decodes and reports the consumed length, ignoring trailing
    /// bytes the way `binary_to_term/2`'s `used` needs.
    #[test]
    fn decompress_all_reports_the_consumed_length() {
        let mut image = HELLO_ZLIB.to_vec();
        image.extend_from_slice(b"trailing junk");
        let (body, used) = decompress_all(15, &image, usize::MAX).expect("a valid stream");
        assert_eq!(body, b"hello");
        assert_eq!(used, HELLO_ZLIB.len());
    }

    /// An empty input, a truncated stream and a corrupt trailer are all refusals.
    #[test]
    fn malformed_streams_are_refused() {
        assert!(matches!(decompress_all(15, &[], 1024), Err(BeamError::BadArg)));
        for cut in 0..HELLO_ZLIB.len() {
            assert!(
                matches!(decompress_all(15, &HELLO_ZLIB[..cut], 1024), Err(BeamError::BadArg)),
                "truncated at {cut}"
            );
        }
        let mut bad = HELLO_ZLIB;
        bad[15] ^= 0xff;
        assert!(matches!(decompress_all(15, &bad, 1024), Err(BeamError::BadArg)));
        // Not a zlib stream at all.
        assert!(matches!(decompress_all(15, b"not zlib", 1024), Err(BeamError::BadArg)));
    }

    /// The output bound is enforced: a stream that decodes past `limit` fails, and the
    /// bound is a ceiling rather than a hint.
    #[test]
    fn the_output_bound_is_a_ceiling() {
        assert!(matches!(decompress_all(15, &HELLO_ZLIB, 4), Err(BeamError::BadArg)));
        let (body, _) = decompress_all(15, &HELLO_ZLIB, 5).expect("exactly the limit");
        assert_eq!(body, b"hello");
    }

    /// `Inflate::new` rejects exactly the `window_bits` values `inflateReset2` does.
    #[test]
    fn the_rejected_window_bits_are_zlibs() {
        for bits in [-7i8, -1, -16, 17, 23, 33, 39, 48, 100, i8::MAX, i8::MIN] {
            assert!(
                matches!(Inflate::new(bits), Err(BeamError::BadArg)),
                "{bits} must be rejected"
            );
        }
        for bits in [-15i8, -8, 0, 8, 15, 16, 24, 31, 32, 40, 47] {
            assert!(Inflate::new(bits).is_ok(), "{bits}");
        }
    }

    /// A fresh stream has no dictionary and is not at the end.
    #[test]
    fn a_fresh_stream_is_empty() {
        let stream = Inflate::new(15).expect("zlib 15");
        assert_eq!(stream.get_dictionary(), b"");
        assert!(!stream.at_stream_end());
        assert_eq!(stream.window_bits(), 15);
    }

    /// A dictionary-carrying stream cannot be decompressed one-shot (the entry point
    /// has no dictionary to offer) but reports the id through the streaming API.
    #[test]
    fn a_dictionary_stream_needs_its_dictionary() {
        let image = [120u8, 249, 97, 60, 15, 250, 43, 193, 84, 10, 0, 71, 142, 7, 52];
        assert!(matches!(decompress_all(15, &image, 1024), Err(BeamError::BadArg)));
        let mut stream = Inflate::new(15).expect("zlib 15");
        let mut out = [0u8; 64];
        let pump = stream.inflate(&image, &mut out, Flush::None).expect("pump");
        let dict: &[u8] = b"the quick brown fox jumps over the lazy dog";
        assert_eq!(pump.status, Status::NeedDict(adler32(1, dict)));
    }
}
