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
//! zlib-faithful `deflate`/`inflate` in safe Rust — the workspace's one zlib port.
//!
//! Ground truth is the C this repository already ships (`../../erts/emulator/zlib/`,
//! zlib 1.3.2) plus the two Erlang-facing layers that consume it
//! (`../../erts/emulator/nifs/common/zlib_nif.c` and `../../erts/preloaded/src/zlib.erl`).
//! The port exists because `zlib` output is an **observable**: `zlib:compress/1`,
//! `zlib:zip/1`, `zlib:gzip/1`, the `zlib` stream API and ETF's `COMPRESSED` tag all
//! hand zlib's byte string to the user, so a merely conforming encoder is not enough.
//!
//! This crate owns bytes only — no terms, no BIF registry, no host state. The
//! consumers are `beam_codec` (ETF `COMPRESSED`) and `beam_bifs` (`zlib` BIFs).
//!
//! # Shape of the API
//!
//! Two layers, deliberately:
//!
//! * **One-shot** — [`Deflate::compress_with`] (`deflateInit` + `deflate(Z_FINISH)` +
//!   `deflateEnd`) and [`inflate::decompress_all`] (a bounded `inflate` loop that also
//!   reports the consumed input length). [`compress`], [`decompress`],
//!   [`decompress_bounded`] and [`decompress_bounded_used`] are the zlib-wrapped
//!   convenience forms — exactly the `zlib:compress/1` / `zlib:uncompress/1` /
//!   ETF-`COMPRESSED` shapes.
//! * **Streaming** — [`Deflate::deflate`] and [`Inflate::inflate`] are zlib's own
//!   `(next_in, avail_in, next_out, avail_out, flush)` call, ported as
//!   `(input, output, flush) -> Result<Pump>`: one pump step that consumes at most
//!   `input.len()` bytes, writes at most `output.len()` bytes and reports zlib's status
//!   code plus both counts. That is the primitive `zlib:deflate/3`, `zlib:inflate/2` and
//!   `zlib:safeInflate/2` need, because their per-call output chunking is observable.
//!
//! ```text
//! deflateInit(Z, Level)            Deflate::new(DeflateConfig::zlib(level))?
//! deflate(Z, In, Flush)            Deflate::deflate(&mut d, input, &mut buf, flush)?
//! deflateParams(Z, Level, Strat)   Deflate::set_params(level, strategy, &mut buf)?
//! deflateSetDictionary(Z, Dict)    Deflate::set_dictionary(dict)?
//! deflateReset(Z)                  Deflate::reset()?
//! inflateInit(Z, WindowBits)       Inflate::new(window_bits)?
//! inflate(Z, In, Flush)            Inflate::inflate(&mut i, input, &mut buf, flush)?
//! inflateSet/GetDictionary         Inflate::set_dictionary / get_dictionary
//! inflateReset(Z) / inflateReset2  Inflate::reset / reset_with_window_bits
//! ```
//!
//! # Error strategy
//!
//! Stream-level outcomes are **status codes**, not errors: a malformed stream is
//! `Ok(Pump { status: Status::DataError, .. })`, exactly as zlib's `inflate` returns
//! `Z_DATA_ERROR`. [`beam_common::Result`] is reserved for a rejected *configuration*
//! (an out-of-range level, an unsupported `windowBits`, a dictionary that a finished
//! stream may not take) — i.e. the cases where the C's `deflateInit2_`/`inflateInit2_`
//! return `Z_STREAM_ERROR` before any byte is processed. Nothing in this crate panics on
//! data, and no entry point silently substitutes a merely-conforming stream for a
//! byte-exact one: a parameter combination that is not ported returns a typed error.

#![forbid(unsafe_code)]

use beam_common::Result;

pub mod checksum;
pub mod deflate;
pub mod inflate;

pub use checksum::{adler32, adler32_combine, crc32, crc32_combine};
pub use deflate::{Deflate, DeflateConfig};
pub use inflate::{decompress_all, Inflate};

/// `Z_DEFAULT_COMPRESSION` — the level sentinel `deflateInit(Z, default)` passes; zlib
/// resolves it to level 6 (`configuration_table[6]`).
pub const DEFAULT_COMPRESSION: i8 = -1;

/// `Z_NO_COMPRESSION` — the level that stores instead of compressing.
pub const NO_COMPRESSION: i8 = 0;

/// `Z_BEST_SPEED`.
pub const BEST_SPEED: i8 = 1;

/// `Z_BEST_COMPRESSION`.
pub const BEST_COMPRESSION: i8 = 9;

/// `DEF_MEM_LEVEL` (8) — the `memLevel` every `zlib` convenience function uses.
pub const DEFAULT_MEM_LEVEL: u8 = 8;

/// `MAX_WBITS` (15) — the largest window, and the one `zlib:compress/1` uses.
pub const MAX_WBITS: i8 = 15;

/// `MIN_WBITS` — the smallest window `deflateInit2`/`inflateInit2` accept.
pub const MIN_WBITS: i8 = 8;

/// The offset that selects the gzip framing: `windowBits` `16 + 8 ..= 16 + 15`.
pub const GZIP_WBITS_OFFSET: i8 = 16;

/// The offset that selects header auto-detection on the **inflate** side:
/// `windowBits` `32 + 8 ..= 32 + 15` accepts a zlib or a gzip stream.
pub const AUTO_WBITS_OFFSET: i8 = 32;

/// A `deflate`/`inflate` flush mode — zlib's `Z_NO_FLUSH` … `Z_BLOCK`.
///
/// The `zlib` module maps its own names onto these (`none` → [`Flush::None`],
/// `sync` → [`Flush::Sync`], `full` → [`Flush::Full`], `finish` → [`Flush::Finish`];
/// [`Flush::Partial`] and [`Flush::Block`] have no `zlib` binding but are part of the
/// C's surface).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Flush {
    /// `Z_NO_FLUSH` — emit only what the compressor already decided to emit.
    #[default]
    None,
    /// `Z_PARTIAL_FLUSH` — flush to a byte boundary without aligning the stream.
    Partial,
    /// `Z_SYNC_FLUSH` — flush everything and align on a byte boundary.
    Sync,
    /// `Z_FULL_FLUSH` — [`Flush::Sync`] plus resetting the compression history.
    Full,
    /// `Z_FINISH` — the final flush; the stream is complete afterwards.
    Finish,
    /// `Z_BLOCK` — stop at the next deflate block boundary.
    Block,
}

impl Flush {
    /// zlib's integer code for this mode (`Z_NO_FLUSH` = 0 … `Z_BLOCK` = 5).
    ///
    /// Exposed so a host that keeps the C's numeric flush mode (the `zlib` NIF layer
    /// receives `?Z_NO_FLUSH` … `?Z_FINISH` as integers) can translate without a
    /// second table.
    pub fn code(self) -> i32 {
        match self {
            Flush::None => 0,
            Flush::Partial => 1,
            Flush::Sync => 2,
            Flush::Full => 3,
            Flush::Finish => 4,
            Flush::Block => 5,
        }
    }
}

/// A compression strategy — zlib's `Z_*_STRATEGY`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Strategy {
    /// `Z_DEFAULT_STRATEGY` — the normal LZ77 + Huffman mode.
    #[default]
    Default,
    /// `Z_FILTERED` — favour shorter matches; for data produced by a filter.
    Filtered,
    /// `Z_HUFFMAN_ONLY` — no string matching, only Huffman coding.
    HuffmanOnly,
    /// `Z_RLE` — run-length encoding only (distance-1 matches).
    Rle,
    /// `Z_FIXED` — never emit a dynamic Huffman block.
    Fixed,
}

impl Strategy {
    /// zlib's integer code for this strategy (`Z_DEFAULT_STRATEGY` = 0 … `Z_FIXED` = 4).
    pub fn code(self) -> i32 {
        match self {
            Strategy::Default => 0,
            Strategy::Filtered => 1,
            Strategy::HuffmanOnly => 2,
            Strategy::Rle => 3,
            Strategy::Fixed => 4,
        }
    }
}

/// The compression method — zlib's `Z_DEFLATED` (8) is the only one that exists.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Method {
    /// `Z_DEFLATED`.
    #[default]
    Deflated,
}

impl Method {
    /// zlib's integer code for this method (`Z_DEFLATED` = 8).
    pub fn code(self) -> i32 {
        match self {
            Method::Deflated => 8,
        }
    }
}

/// A `deflate`/`inflate` return code — zlib's `Z_OK`, `Z_STREAM_END`, … .
///
/// This is an *outcome*, not an error: a stream-level failure is reported here the way
/// the C reports it, and only a rejected configuration becomes a
/// [`beam_common::BeamError`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// `Z_OK`.
    Ok,
    /// `Z_STREAM_END` — the end of the compressed stream was reached.
    StreamEnd,
    /// `Z_NEED_DICT` — a preset dictionary is required; the payload is the Adler-32
    /// dictionary id (the C's `strm->adler`).
    NeedDict(u32),
    /// `Z_BUF_ERROR` — no progress was possible with the buffers given.
    BufError,
    /// `Z_DATA_ERROR` — the input is not a valid stream.
    DataError,
    /// `Z_STREAM_ERROR` — the stream state is inconsistent with the request.
    StreamError,
    /// `Z_MEM_ERROR`.
    MemError,
}

impl Status {
    /// Whether this code is one of zlib's negative (`< 0`) returns.
    pub fn is_error(self) -> bool {
        matches!(
            self,
            Status::BufError
                | Status::DataError
                | Status::StreamError
                | Status::MemError
        )
    }
}

/// The result of one pump step: zlib's return code plus the C's `avail_in`/`avail_out`
/// delta, as counts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pump {
    /// zlib's return code for the call.
    pub status: Status,
    /// Input bytes consumed (`avail_in` before minus after).
    pub consumed: usize,
    /// Output bytes written (`avail_out` before minus after).
    pub produced: usize,
}

/// One-shot zlib-wrapped (RFC 1950) compression at `level` — the `zlib:compress/1`
/// image, and the stream an ETF `COMPRESSED` payload carries.
///
/// `level` is `-1` ([`DEFAULT_COMPRESSION`], i.e. 6) or `0..=9`; anything else is
/// [`beam_common::BeamError::BadArg`]. Window `15`, `memLevel` 8, default strategy —
/// `deflateInit(Z, Level)`. Byte-identical to the oracle at every supported level.
pub fn compress(input: &[u8], level: i8) -> Result<Vec<u8>> {
    Deflate::compress_with(DeflateConfig::zlib(level), input)
}

/// One-shot zlib-wrapped (RFC 1950) decompression with no output bound.
///
/// A convenience for callers that already know the payload size; a caller facing
/// untrusted input must use [`decompress_bounded`] (or
/// [`decompress_bounded_used`]) so a crafted stream cannot drive an unbounded
/// allocation. A malformed stream is [`beam_common::BeamError::BadArg`].
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    decompress_all(MAX_WBITS, input, usize::MAX).map(|(body, _used)| body)
}

/// One-shot zlib-wrapped (RFC 1950) decompression that refuses to inflate past `limit`
/// bytes.
///
/// The bound is a hard ceiling, not a hint: exceeding it is
/// [`beam_common::BeamError::BadArg`] and growth stops at the moment the limit is
/// passed, so memory stays bounded by `min(actual, limit)`. A malformed stream (bad
/// header, truncation, corrupt block, bad Adler-32 trailer) is the same error.
pub fn decompress_bounded(input: &[u8], limit: usize) -> Result<Vec<u8>> {
    decompress_bounded_used(input, limit).map(|(body, _used)| body)
}

/// Like [`decompress_bounded`], but also reports how many **input** bytes the stream
/// consumed.
///
/// This is what `binary_to_term/2`'s `used` option reports for a `COMPRESSED` image: the
/// count covers the deflate data plus the Adler-32 trailer and **excludes** any bytes
/// that follow the stream in `data`.
pub fn decompress_bounded_used(input: &[u8], limit: usize) -> Result<(Vec<u8>, usize)> {
    decompress_all(MAX_WBITS, input, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flush/strategy/method codes are the C's constants; a host that keeps the
    /// numeric form must be able to translate in both directions.
    #[test]
    fn the_numeric_codes_are_zlibs() {
        assert_eq!(Flush::None.code(), 0);
        assert_eq!(Flush::Partial.code(), 1);
        assert_eq!(Flush::Sync.code(), 2);
        assert_eq!(Flush::Full.code(), 3);
        assert_eq!(Flush::Finish.code(), 4);
        assert_eq!(Flush::Block.code(), 5);
        assert_eq!(Strategy::Default.code(), 0);
        assert_eq!(Strategy::Filtered.code(), 1);
        assert_eq!(Strategy::HuffmanOnly.code(), 2);
        assert_eq!(Strategy::Rle.code(), 3);
        assert_eq!(Strategy::Fixed.code(), 4);
        assert_eq!(Method::Deflated.code(), 8);
    }

    /// `is_error` follows zlib's sign convention: `Z_NEED_DICT` is positive and is not
    /// an error (it asks for a dictionary), `Z_BUF_ERROR` is negative and is.
    #[test]
    fn need_dict_is_not_an_error() {
        assert!(!Status::Ok.is_error());
        assert!(!Status::StreamEnd.is_error());
        assert!(!Status::NeedDict(1).is_error());
        assert!(Status::BufError.is_error());
        assert!(Status::DataError.is_error());
        assert!(Status::StreamError.is_error());
        assert!(Status::MemError.is_error());
    }
}
