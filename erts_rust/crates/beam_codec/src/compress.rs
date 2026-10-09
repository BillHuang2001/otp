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

//! zlib (RFC 1950) compression for the `COMPRESSED` external-term tag.
//!
//! A compressed `term_to_binary` image is
//! `[VERSION_MAGIC][COMPRESSED_EXT][uncompressed_size:4 BE][zlib stream]`, where the
//! 4-byte size is the byte length of the **inner** term (tag + data) *excluding* the
//! version byte and the stream is a zlib-wrapped (RFC 1950) deflate stream — a
//! 2-byte zlib header plus an Adler-32 trailer, **not** raw deflate.
//!
//! Ground truth is the legacy C `../../erts/emulator/beam/external.c`
//! (`binary2term_prepare`, `external.c:1744-1816`, decode; the `erlang:term_to_binary/2`
//! path, `external.c:2291`, encode) with `../../erts/emulator/beam/erl_zlib.c`. The C
//! calls zlib's `compress2`/`uncompress` (i.e. `deflateInit`/`inflateInit` with the
//! default window), so this module uses [`flate2`]'s zlib wrappers and never raw deflate.
//!
//! This module is safe Rust: it owns only the zlib plumbing. Building the wrapper
//! bytes (the version byte, the tag, the size field) is [`crate::encode`]'s job and
//! recognising/handling the top-level wrapper is [`crate::decode`]'s.

use std::io::{Read, Write};

use beam_common::Result;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::error::bad_arg;

/// The smallest accepted zlib compression level.
pub const MIN_LEVEL: u8 = 0;

/// The largest accepted zlib compression level (`Z_BEST_COMPRESSION`).
///
/// This is the same `0..=9` range the C `{compressed, Level}` option accepts.
pub const MAX_LEVEL: u8 = 9;

/// The upper bound on a decompressed payload, in bytes.
///
/// Mirrors the C's sanity bound on the declared uncompressed size
/// (`dest_len > 32*1024*1024`, `external.c:1768`). It exists so a corrupt or hostile
/// stream can never drive an unbounded allocation: a stream that inflates past this
/// bound is rejected as [`BeamError::BadArg`](beam_common::BeamError::BadArg) instead
/// of exhausting memory.
pub const MAX_UNCOMPRESSED: usize = 32 * 1024 * 1024;

/// Compresses `data` with zlib (RFC 1950) at `level` (`0..=9`).
///
/// `level` is the zlib compression level, exactly like the C `{compressed, Level}`
/// option: `0` stores, `9` is best compression. A `level` outside [`MIN_LEVEL`]`..=`[`MAX_LEVEL`]
/// yields [`BeamError::BadArg`](beam_common::BeamError::BadArg); a zlib failure (it
/// can only fail on an internal error here) yields the same. The output is the whole
/// zlib stream: 2-byte header, deflate data, Adler-32 trailer.
pub fn compress(data: &[u8], level: u8) -> Result<Vec<u8>> {
    if level > MAX_LEVEL {
        return Err(bad_arg());
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(u32::from(level)));
    encoder.write_all(data).map_err(|_| bad_arg())?;
    encoder.finish().map_err(|_| bad_arg())
}

/// Decompresses a zlib (RFC 1950) stream.
///
/// Any zlib error — a bad header, a truncated stream, a corrupt deflate block or a
/// bad Adler-32 trailer — yields
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg). Output is capped at
/// [`MAX_UNCOMPRESSED`] bytes; a larger stream is rejected with the same error rather
/// than allocating without bound. Never panics.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match decoder.read(&mut chunk) {
            // The stream ended (all input consumed, deflate final block seen).
            Ok(0) => return Ok(out),
            Ok(read) => {
                if out.len() + read > MAX_UNCOMPRESSED {
                    return Err(bad_arg());
                }
                out.extend_from_slice(&chunk[..read]);
            }
            Err(_) => return Err(bad_arg()),
        }
    }
}

/// Decompresses a zlib (RFC 1950) stream, refusing to inflate past `limit` bytes.
///
/// This is the bound the C actually applies when unwrapping a `COMPRESSED` image: it
/// allocates exactly the 4-byte *declared* inner size and requires zlib to fill that
/// buffer completely (`erl_zlib_uncompress(state->extp, &dlen, bytes, size) != Z_OK ||
/// dlen != dest_len`, `external.c:1805-1810`). Inflating **more** than the declaration
/// is therefore an error, and so is inflating less (the caller compares the returned
/// length against the declaration).
///
/// Unlike [`decompress`]'s fixed [`MAX_UNCOMPRESSED`] ceiling — which the C only uses as
/// a heuristic to decide whether to *pre-allocate* the declared size (`dest_len >
/// 32*1024*1024`, `external.c:1768`) and which the C then bypasses by measuring the real
/// uncompressed size first (`binary2term_uncomp_size`, `external.c:1741`) — this bound
/// never rejects a legitimately large payload. Memory stays bounded by
/// `min(actual, limit)` bytes: growth stops the moment `limit` is exceeded, so an
/// inflated flow that never ends cannot exhaust memory.
///
/// Any zlib error (bad header, truncation, corrupt deflate block, bad Adler-32) or an
/// inflated length `> limit` yields
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg). Never panics.
pub fn decompress_bounded(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match decoder.read(&mut chunk) {
            Ok(0) => return Ok(out),
            Ok(read) => {
                if out.len().saturating_add(read) > limit {
                    return Err(bad_arg());
                }
                out.extend_from_slice(&chunk[..read]);
            }
            Err(_) => return Err(bad_arg()),
        }
    }
}

/// The `BeamError` a `level` outside [`MIN_LEVEL`]`..=`[`MAX_LEVEL`] yields.
///
/// Exposed so [`crate::encode`]'s compressed entry points can reject an out-of-range
/// level *before* encoding the term body — the C `{compressed, Level}` option is
/// validated by the BIF (`external.c:2291`) rather than silently clamped.
pub(crate) fn reject_bad_level(level: u8) -> Result<()> {
    if level > MAX_LEVEL {
        Err(bad_arg())
    } else {
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    //! Wire-level tests for the zlib plumbing: round-trips at every level, the
    //! emptiness cases and the negative cases (bad header, truncation, a corrupt
    //! trailer). The `COMPRESSED` tag itself is exercised in [`crate::decode`] /
    //! [`crate::encode`].

    use super::*;
    use beam_common::BeamError;

    fn assert_bad_arg<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    #[test]
    fn every_level_round_trips() {
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        for level in MIN_LEVEL..=MAX_LEVEL {
            let packed = compress(&data, level).unwrap();
            // Level 0 still emits a real zlib stream (header + stored blocks + trailer).
            assert!(packed.len() > 2, "level {level} produced {packed:?}");
            assert_eq!(decompress(&packed).unwrap(), data, "level {level}");
        }
    }

    #[test]
    fn empty_input_round_trips() {
        for level in [0u8, 6, 9] {
            let packed = compress(&[], level).unwrap();
            // A zlib stream of an empty payload is the 2-byte header and the 4-byte
            // Adler-32 of nothing, i.e. at least 6 bytes.
            assert!(packed.len() >= 6, "level {level}: {packed:?}");
            assert!(decompress(&packed).unwrap().is_empty());
        }
    }

    #[test]
    fn large_repetitive_input_compresses_well() {
        let data = vec![b'A'; 100_000];
        let packed = compress(&data, 6).unwrap();
        assert!(
            packed.len() < data.len() / 10,
            "expected strong compression, got {} vs {}",
            packed.len(),
            data.len()
        );
        assert_eq!(decompress(&packed).unwrap(), data);
    }

    #[test]
    fn level_out_of_range_is_bad_arg() {
        assert_bad_arg(compress(b"x", 10));
        assert_bad_arg(compress(b"x", u8::MAX));
        // `0` and `9` are the inclusive bounds and are accepted.
        assert!(compress(b"x", MIN_LEVEL).is_ok());
        assert!(compress(b"x", MAX_LEVEL).is_ok());
    }

    #[test]
    fn decompress_rejects_malformed_streams() {
        // Empty input: no zlib header at all.
        assert_bad_arg(decompress(&[]));
        // A single byte cannot hold the 2-byte header.
        assert_bad_arg(decompress(&[0x78]));
        // A plausible but wrong header (bad CMF/FLG check bits).
        assert_bad_arg(decompress(&[0x00, 0x00, 0x00, 0x00]));
        // A valid header with no deflate data and no trailer.
        assert_bad_arg(decompress(&[0x78, 0x9C]));

        // A valid stream with its Adler-32 trailer corrupted is rejected.
        let mut packed = compress(b"the quick brown fox", 6).unwrap();
        let last = packed.len() - 1;
        packed[last] ^= 0xFF;
        assert_bad_arg(decompress(&packed));

        // A valid stream truncated before its trailer is rejected.
        let packed = compress(b"the quick brown fox", 6).unwrap();
        assert_bad_arg(decompress(&packed[..packed.len() - 4]));
        // Truncation at every offset before the trailer is also rejected (never a panic).
        for len in 0..packed.len() - 4 {
            assert_bad_arg(decompress(&packed[..len]));
        }
    }

    #[test]
    fn decompress_bounded_inflates_exactly_within_its_limit() {
        let data = vec![b'z'; 100_000];
        let packed = compress(&data, 6).unwrap();
        // The exact limit is fine; anything below it is rejected (the payload inflates
        // past it), and the *empty* stream is only accepted at a limit of 0.
        assert_eq!(decompress_bounded(&packed, data.len()).unwrap(), data);
        assert_eq!(
            decompress_bounded(&packed, data.len() + 1).unwrap(),
            data,
            "a limit larger than the payload is fine — the caller compares the length"
        );
        assert_bad_arg(decompress_bounded(&packed, data.len() - 1));
        assert_bad_arg(decompress_bounded(&packed, 0));
        assert!(decompress_bounded(&compress(&[], 6).unwrap(), 0)
            .unwrap()
            .is_empty());

        // The bound never rejects a legitimately large payload, unlike the fixed
        // `MAX_UNCOMPRESSED` ceiling of `decompress` (this is the divergence the
        // `COMPRESSED` wrapper must not have — see `tests/compressed_oracle.rs`).
        let big = vec![0u8; MAX_UNCOMPRESSED + 1];
        let big_packed = compress(&big, 6).unwrap();
        assert_bad_arg(decompress(&big_packed));
        assert_eq!(
            decompress_bounded(&big_packed, big.len()).unwrap(),
            big,
            "a payload above MAX_UNCOMPRESSED must inflate when the declared size says so"
        );
    }
}
