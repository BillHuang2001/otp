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

//! The six self-contained `zlib` functions — `compress/1`, `uncompress/1`, `zip/1`,
//! `unzip/1`, `gzip/1`, `gunzip/1` — over [`beam_deflate`], the workspace's one zlib port.
//!
//! The per-function observables (argument set, error classes, framings, goldens) are
//! documented in the parent module; each function below carries only its own contract.
//! Everything here is safe Rust and returns [`beam_common::Result`].

use beam_codec::compress::MAX_UNCOMPRESSED;
use beam_common::{BeamError, ExitReason, Raise, Result};
use beam_deflate::{Deflate, DeflateConfig};
use beam_term::Eterm;

use crate::binary::iodata_bytes;
use crate::bits;
use crate::context::BifContext;

/// The compression level all six functions use: `deflateInit(Z, default)`
/// (`zlib.erl:686`), i.e. zlib's `Z_DEFAULT_COMPRESSION`, which zlib resolves to level 6
/// (`configuration_table[6]`).
const DEFAULT_COMPRESSION_LEVEL: i8 = beam_deflate::DEFAULT_COMPRESSION;

/// The shortest argument `zlib:uncompress/1` will hand to the inflater
/// (`Size >= 8`, `zlib.erl:703`); anything shorter is `data_error`.
const MIN_UNCOMPRESS_LENGTH: usize = 8;

/// The greatest expansion the DEFLATE format itself allows: `1032` output bytes per
/// input byte (a 258-byte match encoded in two one-bit codes).
///
/// It bounds [`expansion_limit`], so the inflating entry points carry no *artificial*
/// ceiling: a bound derived from the stream's own length can never reject a stream the
/// oracle inflates, while a corrupt or hostile one still cannot allocate without bound.
const MAX_DEFLATE_EXPANSION: usize = 1032;

/// The largest output the inflating entry points will accept for an input of
/// `input_len` bytes — the format's own [`MAX_DEFLATE_EXPANSION`] ceiling.
fn expansion_limit(input_len: usize) -> usize {
    input_len.saturating_mul(MAX_DEFLATE_EXPANSION)
}

/// The RFC 1952 member shape both directions of the gzip pair use. `gzip/1` asks
/// [`beam_deflate`] for it and `gunzip/1` lets the inflater parse it, so no production code
/// reads these — they are the *pinned* values the tests below assert the encoder emits and
/// the parser accepts.
///
/// `ID1`/`ID2` are the two magic bytes that open every member, `CM` = deflate, `FLG` = 0
/// (no extra field, name, comment or header CRC), `MTIME` = 0 (absent), `XFL` = 0 and
/// `OS` = 3 (zlib's Unix `OS_CODE`). `XFL` is `0` for every level but 1 and 9 (zlib writes
/// `4`/`2`/`0`) and [`DEFAULT_COMPRESSION_LEVEL`] resolves to level 6, which is what
/// `GZIP_HEADER` spells out.
#[cfg(test)]
mod gzip_rfc1952 {
    /// `ID1`/`ID2` — the magic bytes that open every gzip member (RFC 1952 §2.3.1).
    pub(super) const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

    /// `CM` — the compression-method byte meaning "deflate" (§2.3.1).
    pub(super) const DEFLATE_METHOD: u8 = 8;

    /// The fixed ten-byte header of a default-level member (§2.3.1).
    pub(super) const GZIP_HEADER: [u8; 10] = [
        GZIP_MAGIC[0],
        GZIP_MAGIC[1],
        DEFLATE_METHOD,
        0,
        0,
        0,
        0,
        0,
        0,
        3,
    ];

    /// `FHCRC` — a 2-byte CRC-16 of the header follows the optional fields (§2.3.1).
    pub(super) const GZIP_FLAG_FHCRC: u8 = 0x02;
    /// `FEXTRA` — a 2-byte `XLEN` and `XLEN` extra bytes follow (§2.3.1).
    pub(super) const GZIP_FLAG_FEXTRA: u8 = 0x04;
    /// `FNAME` — a zero-terminated original file name follows (§2.3.1).
    pub(super) const GZIP_FLAG_FNAME: u8 = 0x08;
    /// `FCOMMENT` — a zero-terminated comment follows (§2.3.1).
    pub(super) const GZIP_FLAG_FCOMMENT: u8 = 0x10;
}

#[cfg(test)]
use gzip_rfc1952::{
    GZIP_FLAG_FCOMMENT, GZIP_FLAG_FEXTRA, GZIP_FLAG_FHCRC, GZIP_FLAG_FNAME, GZIP_HEADER,
    GZIP_MAGIC,
};

/// `zlib:compress(Data)` — `Data` (iodata) compressed into one zlib (RFC 1950) stream.
///
/// The bytes are the oracle's for any input: [`beam_deflate::compress`] is the port of the
/// C the oracle links (the parent module's "Byte identity" section). The 2-byte header, the
/// Adler-32 trailer and the round trip are pinned by this module's tests.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument — the oracle's `badarg` from
/// `erlang:iolist_to_iovec/1`.
pub(super) fn compress_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    let deflated = beam_deflate::compress(&bytes, DEFAULT_COMPRESSION_LEVEL)?;
    bits::make_binary(ctx.heap(), &deflated)
}

/// `zlib:zip(Data)` — `Data` (iodata) as one **raw DEFLATE** (RFC 1951) stream.
///
/// The framing is the encoder's own (`DeflateConfig::raw`), not a slice of a zlib stream:
/// a `windowBits` sign selects only the framing, so the deflate body is the same byte
/// string either way, and there is no unchecked strip to get wrong.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument — the oracle's `badarg` from
/// `erlang:iolist_to_iovec/1`.
pub(super) fn zip_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    let raw = Deflate::compress_with(DeflateConfig::raw(DEFAULT_COMPRESSION_LEVEL), &bytes)?;
    bits::make_binary(ctx.heap(), &raw)
}

/// `zlib:unzip(Data)` — the payload of the **raw DEFLATE** stream `Data` (iodata).
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument; the `error`-class [`data_error`] for a
/// stream the inflater cannot finish — the empty stream included, since (unlike
/// `uncompress/1`) `zlib:unzip/1` has no length arm — and for a payload past
/// [`MAX_UNCOMPRESSED`], the one output ceiling this pair keeps.
pub(super) fn unzip_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    // The inflater reports every failure in the codec's own class (`BadArg`), which is
    // exactly the class `zlib:unzip/1` reports as `data_error`; any other error class is a
    // genuine capability gap and is passed through untouched. The consumed-byte count is
    // not needed: everything after the deflate stream is ignored anyway.
    let (inflated, _consumed) =
        match beam_deflate::decompress_all(-beam_deflate::MAX_WBITS, &bytes, MAX_UNCOMPRESSED) {
            Ok(inflated) => inflated,
            Err(BeamError::BadArg) => return Err(data_error()),
            Err(other) => return Err(other),
        };
    bits::make_binary(ctx.heap(), &inflated)
}

/// `zlib:uncompress(Data)` — the payload of the zlib stream `Data` (iodata).
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument; the `error`-class [`data_error`] for a
/// stream shorter than [`MIN_UNCOMPRESS_LENGTH`] bytes and for one the inflater rejects (a
/// bad header, a truncated deflate block, a bad Adler-32 trailer).
///
/// There is no size ceiling: the payload is inflated through the codec's *bounded* entry
/// point with an [`expansion_limit`] — the format's own `1032:1` bound on this input — so
/// every stream the oracle inflates inflates here.
pub(super) fn uncompress_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    if bytes.len() < MIN_UNCOMPRESS_LENGTH {
        return Err(data_error());
    }
    let inflated = match beam_deflate::decompress_bounded(&bytes, expansion_limit(bytes.len())) {
        Ok(inflated) => inflated,
        // Every zlib failure is `BadArg` at the codec's boundary; that is exactly the class
        // `zlib:uncompress/1` reports as `data_error`. Any other error class is a genuine
        // capability gap and is passed through untouched.
        Err(BeamError::BadArg) => return Err(data_error()),
        Err(other) => return Err(other),
    };
    bits::make_binary(ctx.heap(), &inflated)
}

/// `zlib:gzip(Data)` — `Data` (iodata) as one RFC 1952 gzip member.
///
/// The member is the encoder's own gzip framing (`DeflateConfig::gzip`): [`GZIP_HEADER`],
/// the very same raw DEFLATE payload `zip/1` emits, then the little-endian CRC-32 of the
/// **original** `Data` and its `ISIZE` (the payload length modulo `2^32`).
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument — the oracle's `badarg` from
/// `erlang:iolist_to_iovec/1`.
pub(super) fn gzip_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    let member = Deflate::compress_with(DeflateConfig::gzip(DEFAULT_COMPRESSION_LEVEL), &bytes)?;
    bits::make_binary(ctx.heap(), &member)
}

/// `zlib:gunzip(Data)` — the payloads of the RFC 1952 gzip member(s) in `Data` (iodata).
///
/// The gzip framing is the inflater's own (`windowBits` `16 + MAX_WBITS`), exactly what
/// `zlib.erl:776` opens: it parses each member's header (honouring every `FLG` bit —
/// `FEXTRA`/`FNAME`/`FCOMMENT` are skipped, `FHCRC` is verified, the reserved bits are
/// rejected), inflates the payload, verifies the CRC-32 and the `ISIZE` trailer, and
/// reports how many bytes the member occupied, so the loop below consumes one member per
/// iteration and concatenates their payloads — the oracle's `EOS_BEHAVIOR_RESET`
/// semantics, where concatenated members are one call's worth of output.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument. **Everything else** is the bare
/// `error:data_error` ([`data_error`]): a malformed header (a missing or bad magic byte, a
/// bad `CM`, a reserved `FLG` bit, a truncated or mismatching optional field), an inflater
/// failure, a truncated/mismatching trailer, and any input left over that is not another
/// complete member — the **empty** input included, since it completes no member at all.
/// That is exactly the oracle's matrix (OTP 29.1.1 / erts-17.1).
pub(super) fn gunzip_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    // The bound is the format's own, over the whole input, and shared across members.
    let limit = expansion_limit(bytes.len());
    let mut out = Vec::new();
    let mut members = 0usize;
    let mut pos = 0usize;
    while pos < bytes.len() {
        let (inflated, consumed) = match beam_deflate::decompress_all(
            beam_deflate::GZIP_WBITS_OFFSET + beam_deflate::MAX_WBITS,
            &bytes[pos..],
            limit.saturating_sub(out.len()),
        ) {
            Ok(member) => member,
            // The inflater reports every failure in the codec's own class (`BadArg`), which
            // is exactly the class `zlib:gunzip/1` reports as `data_error`; any other error
            // class is a genuine capability gap and is passed through.
            Err(BeamError::BadArg) => return Err(data_error()),
            Err(other) => return Err(other),
        };
        if consumed == 0 {
            // A member always occupies at least its header, so a zero-length one would spin
            // this loop forever. The inflater never reports it; refuse rather than hang.
            return Err(data_error());
        }
        out.extend_from_slice(&inflated);
        members += 1;
        pos += consumed;
    }
    if members == 0 {
        // The oracle reaches this through `inflateEnd/1` ("no end of stream was found",
        // `zlib.erl:673-682`), which is a `data_error` too — the empty input.
        return Err(data_error());
    }
    bits::make_binary(ctx.heap(), &out)
}
/// The `error`-class raise with the **bare atom** reason `data_error` — the oracle's
/// own reason for a stream `zlib:uncompress/1` cannot inflate (`zlib.erl:715` for the
/// too-short arm, and the NIF's `data_error` for the rest).
///
/// A bare atom needs no atom table (exactly like [`crate::error::function_clause`]),
/// so this is an [`ExitReason::atom`] raise rather than a term-carrying one.
fn data_error() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("data_error")))
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{signed_value, AtomTable};

    use crate::access;
    use crate::checksum::crc32_step;
    /// A heap large enough for the 100 kB round trip below.
    fn heap() -> Heap {
        Heap::new(1 << 18).expect("test heap")
    }

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "hello"]);
        atoms
    }

    fn context<'a>(heap: &'a mut Heap, atoms: &'a AtomTable) -> BifContext<'a> {
        BifContext::new(heap, atoms, ProcessId::new(1, 0))
    }

    /// The bytes of a binary term, read through the crate's term accessor.
    fn bytes_of(term: Eterm) -> Vec<u8> {
        assert!(
            crate::bits::is_bitstring(term),
            "expected a binary, got {term:?}"
        );
        let (bytes, offset, size_bits) =
            beam_codec::term::bitstring_bytes(term).expect("a readable bitstring");
        assert_eq!(offset % 8, 0, "the result is a whole number of bytes");
        let len = size_bits / 8;
        bytes[offset / 8..offset / 8 + len].to_vec()
    }

    fn binary(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
        bits::make_binary(ctx.heap(), bytes).expect("the test heap has room")
    }

    #[test]
    fn compress_returns_a_binary_and_round_trips_through_uncompress() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let plain = binary(&mut ctx, b"hello");
        let compressed = compress_1(&mut ctx, &[plain]).expect("compress/1");

        // `zlib:compress(<<"hello">>)` is the 13-byte
        // `<<120,156,203,72,205,201,201,7,0,6,44,2,21>>` under OTP 29.1.1 / erts-17.1:
        // one fixed-Huffman block, then the Adler-32.
        let stream = bytes_of(compressed);
        assert_eq!(
            stream,
            vec![120, 156, 203, 72, 205, 201, 201, 7, 0, 6, 44, 2, 21],
            "the oracle's stream for `<<\"hello\">>`, byte for byte"
        );

        let back = uncompress_1(&mut ctx, &[compressed]).expect("uncompress/1");
        assert_eq!(bytes_of(back), b"hello");
    }

    #[test]
    fn the_empty_binary_round_trips_and_compresses_to_the_eight_byte_store() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let empty = binary(&mut ctx, b"");
        let compressed = compress_1(&mut ctx, &[empty]).expect("compress/1");
        // The oracle's `zlib:compress(<<>>)` is 8 bytes: header + an empty final
        // block + Adler-32. Adler-32 of the empty payload is 1, which is the store
        // form here too.
        assert_eq!(bytes_of(compressed), vec![120, 156, 3, 0, 0, 0, 0, 1]);

        let back = uncompress_1(&mut ctx, &[compressed]).expect("uncompress/1");
        assert_eq!(bytes_of(back), Vec::<u8>::new());
    }

    #[test]
    fn round_trips_hold_for_several_sizes() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        for size in [0usize, 1, 13, 1000, 100_000] {
            // A repeating-but-not-constant payload, so the compressor cannot just emit
            // one store block.
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let plain = binary(&mut ctx, &payload);
            let compressed = compress_1(&mut ctx, &[plain]).expect("compress/1");
            let back = uncompress_1(&mut ctx, &[compressed]).expect("uncompress/1");
            assert_eq!(bytes_of(back), payload, "round trip at size {size}");
        }
    }

    #[test]
    fn both_bifs_accept_the_whole_iodata_set() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let hello = binary(&mut ctx, b"hello");
        let one = ctx.int(b'h' as i64).expect("fits");
        let two = ctx.int(b'i' as i64).expect("fits");
        // `[<<"hel">>, [104, 105]]` (the integer pair below) and the improper
        // `[104 | <<"ello">>]`.
        let nested = {
            let tail = ctx.cons(two, beam_term::NIL).expect("fits");
            let inner = ctx.cons(one, tail).expect("fits");
            let head = binary(&mut ctx, b"hel");
            ctx.list(&[head, inner]).expect("fits")
        };
        let from_list = compress_1(&mut ctx, &[hello]).expect("a binary");
        let improper = {
            let head = ctx.int(104).expect("fits");
            let tail = binary(&mut ctx, b"ello");
            ctx.cons(head, tail).expect("fits")
        };
        let from_improper = compress_1(&mut ctx, &[improper]).expect("a binary tail");
        let from_nested = compress_1(&mut ctx, &[nested]).expect("nested iodata");

        for (stream, expected) in [
            (from_list, &b"hello"[..]),
            (from_improper, &b"hello"[..]),
            (from_nested, &b"helhi"[..]),
        ] {
            let back = uncompress_1(&mut ctx, &[stream]).expect("uncompress/1");
            assert_eq!(bytes_of(back), expected);
        }
    }

    #[test]
    fn a_non_iodata_argument_is_badarg_on_both_bifs() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let hello_atom = ctx.atom("hello").expect("seeded");
        let over_long = ctx.int(256).expect("fits");
        let negative = ctx.int(-1).expect("fits");
        let non_aligned = bits::make_bitstring(ctx.heap(), &[0b1010_0000], 4).expect("fits");
        let improper_junk = {
            let head = ctx.int(1).expect("fits");
            let tail = ctx.atom("hello").expect("seeded");
            ctx.cons(head, tail).expect("fits")
        };

        for bad in [
            ctx.int(42).expect("fits"),
            hello_atom,
            over_long,
            negative,
            non_aligned,
            improper_junk,
        ] {
            assert!(
                matches!(compress_1(&mut ctx, &[bad]), Err(BeamError::BadArg)),
                "compress/1 of {bad:?} must be badarg"
            );
            assert!(
                matches!(uncompress_1(&mut ctx, &[bad]), Err(BeamError::BadArg)),
                "uncompress/1 of {bad:?} must be badarg"
            );
        }
    }

    #[test]
    fn a_short_or_corrupt_stream_is_the_error_class_data_error() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        // The length arm of `zlib.erl:703`: 0 and 7 bytes are `data_error` (not
        // `badarg`, and neither reaches the inflater).
        for length in [0usize, 2, 7] {
            let short = binary(&mut ctx, &vec![0u8; length]);
            assert!(
                matches!(uncompress_1(&mut ctx, &[short]), Err(BeamError::Raise(raise))
                    if raise.class == beam_common::ExceptionClass::Error
                        && raise.reason == ExitReason::atom("data_error")),
                "{length} bytes must be data_error"
            );
        }

        // The inflater arm: 8 garbage bytes are long enough to be handed over.
        let garbage = binary(&mut ctx, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(
            matches!(uncompress_1(&mut ctx, &[garbage]), Err(BeamError::Raise(raise))
                if raise.reason == ExitReason::atom("data_error")),
            "8 garbage bytes must be data_error"
        );

        // A valid stream with trailing garbage still inflates: the inflater stops at
        // the end of the zlib stream (`zlib:uncompress` ignores the rest, oracle-verified).
        let hello = binary(&mut ctx, b"hello");
        let compressed = compress_1(&mut ctx, &[hello]).expect("compress/1");
        let mut padded = bytes_of(compressed);
        padded.push(99);
        let padded = binary(&mut ctx, &padded);
        let back = uncompress_1(&mut ctx, &[padded]).expect("trailing bytes are ignored");
        assert_eq!(bytes_of(back), b"hello");
    }

    #[test]
    fn the_oracle_golden_stream_inflates() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        // `zlib:compress(<<"hello">>)` under OTP 29.1.1 / erts-17.1, byte for byte.
        // A conforming inflater must accept the oracle's own stream.
        let golden = binary(&mut ctx, &[120, 156, 203, 72, 205, 201, 201, 7, 0, 6, 44, 2, 21]);
        let back = uncompress_1(&mut ctx, &[golden]).expect("the oracle's stream inflates");
        assert_eq!(bytes_of(back), b"hello");
    }

    #[test]
    fn zip_produces_the_oracle_golden_raw_deflate_streams() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        // `zlib:zip(<<"hello">>)` and `zlib:zip(<<>>)` under OTP 29.1.1 / erts-17.1,
        // byte for byte (both fixed-Huffman blocks, verified live against the oracle).
        let hello = binary(&mut ctx, b"hello");
        let zipped = zip_1(&mut ctx, &[hello]).expect("zip/1");
        assert_eq!(bytes_of(zipped), vec![203, 72, 205, 201, 201, 7, 0]);

        let empty = binary(&mut ctx, b"");
        let zipped_empty = zip_1(&mut ctx, &[empty]).expect("zip/1");
        assert_eq!(bytes_of(zipped_empty), vec![3, 0]);
    }

    #[test]
    fn zip_and_unzip_round_trip_and_ignore_trailing_bytes() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let plain = binary(&mut ctx, b"hello");
        let zipped = zip_1(&mut ctx, &[plain]).expect("zip/1");
        let back = unzip_1(&mut ctx, &[zipped]).expect("unzip/1");
        assert_eq!(bytes_of(back), b"hello");

        // Bytes after the end of the deflate stream are ignored (oracle-verified:
        // `zlib:unzip(<<Zip/binary, 99, 99>>)` is the payload).
        let mut padded = bytes_of(zipped);
        padded.extend_from_slice(&[99, 99]);
        let padded = binary(&mut ctx, &padded);
        let back = unzip_1(&mut ctx, &[padded]).expect("trailing bytes are ignored");
        assert_eq!(bytes_of(back), b"hello");

        // A larger, non-constant payload exercises the whole encoder/decoder pair.
        let payload: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let plain = binary(&mut ctx, &payload);
        let zipped = zip_1(&mut ctx, &[plain]).expect("zip/1");
        let back = unzip_1(&mut ctx, &[zipped]).expect("unzip/1");
        assert_eq!(bytes_of(back), payload);
    }

    #[test]
    fn unzip_reports_data_error_for_any_stream_the_inflater_cannot_finish() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        // Unlike `uncompress/1` there is **no** length arm: the empty stream is
        // `data_error` too (oracle-verified).
        for raw in [&b""[..], &[1, 2, 3][..], &[1, 2, 3, 4, 5, 6, 7, 8][..], &[0xFF, 0xFF][..]]
        {
            let stream = binary(&mut ctx, raw);
            assert!(
                matches!(unzip_1(&mut ctx, &[stream]), Err(BeamError::Raise(raise))
                    if raise.class == beam_common::ExceptionClass::Error
                        && raise.reason == ExitReason::atom("data_error")),
                "unzip/1 of {raw:?} must be data_error"
            );
        }

        // A well-formed raw stream whose trailing byte is dropped mid-block.
        let plain = binary(&mut ctx, b"hello");
        let zipped = zip_1(&mut ctx, &[plain]).expect("zip/1");
        let truncated = bytes_of(zipped)[..5].to_vec();
        let truncated = binary(&mut ctx, &truncated);
        assert!(
            matches!(unzip_1(&mut ctx, &[truncated]), Err(BeamError::Raise(raise))
                if raise.reason == ExitReason::atom("data_error")),
            "a truncated stream must be data_error"
        );
    }

    #[test]
    fn a_non_iodata_argument_is_badarg_on_the_raw_pair_too() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let improper = {
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let three = ctx.int(3).expect("fits");
            let tail = ctx.cons(two, three).expect("fits");
            ctx.cons(one, tail).expect("fits")
        };
        let over_long = ctx.int(256).expect("fits");
        let non_aligned = bits::make_bitstring(ctx.heap(), &[0b1010_0000], 4).expect("fits");

        for (what, bad) in [
            ("an integer", ctx.int(42).expect("fits")),
            ("an atom", ctx.atom("hello").expect("seeded")),
            ("an out-of-range integer", over_long),
            ("a non-byte-aligned bitstring", non_aligned),
            ("an improper list", improper),
        ] {
            assert!(
                matches!(zip_1(&mut ctx, &[bad]), Err(BeamError::BadArg)),
                "zip/1 of {what} must be badarg"
            );
            assert!(
                matches!(unzip_1(&mut ctx, &[bad]), Err(BeamError::BadArg)),
                "unzip/1 of {what} must be badarg"
            );
        }

        // A plain list of byte values *is* iodata, on both (oracle-verified:
        // `zlib:zip([1,2,3])` is `<<99,100,98,6,0>>`).
        let list = {
            let a = ctx.int(1).expect("fits");
            let b = ctx.int(2).expect("fits");
            let c = ctx.int(3).expect("fits");
            ctx.list(&[a, b, c]).expect("fits")
        };
        let zipped = zip_1(&mut ctx, &[list]).expect("an integer list is iodata");
        assert_eq!(bytes_of(zipped), vec![99, 100, 98, 6, 0]);
        let back = unzip_1(&mut ctx, &[zipped]).expect("unzip/1");
        assert_eq!(bytes_of(back), vec![1, 2, 3]);
    }

    #[test]
    fn the_three_framings_agree_byte_for_byte() {
        // `compress/1` asks for the RFC 1950 framing, `zip/1` for RFC 1951 and `gzip/1`
        // for RFC 1952. A `windowBits` sign selects only the framing, so the *deflate
        // body* is the same byte string in all three — the property that lets the three
        // functions drive one encoder instead of slicing a stream apart — and the gzip
        // member is that body between [`GZIP_HEADER`] and the CRC-32/ISIZE trailer.
        let inputs: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"hello".to_vec(),
            b"hello hello hello".to_vec(),
            (0..1000u32).map(|i| (i % 251) as u8).collect(),
            (0..40_000u32).map(|i| (i % 251) as u8).collect(),
            vec![0u8; 32_768],
        ];
        for level in [
            beam_deflate::DEFAULT_COMPRESSION,
            beam_deflate::NO_COMPRESSION,
            beam_deflate::BEST_SPEED,
            6,
            beam_deflate::BEST_COMPRESSION,
        ] {
            for input in &inputs {
                let zlib = beam_deflate::compress(input, level).expect("the zlib framing");
                let raw = Deflate::compress_with(DeflateConfig::raw(level), input).expect("raw");
                assert_eq!(
                    raw,
                    &zlib[2..zlib.len() - 4],
                    "the raw body at level {level} for {} bytes",
                    input.len()
                );

                let member =
                    Deflate::compress_with(DeflateConfig::gzip(level), input).expect("gzip");
                // `XFL` is 2 at level 9, 4 below 2, 0 for everything else (zlib's
                // `gzip_xfl`), and level `-1` has already been resolved to 6 by then.
                let xfl = if level == beam_deflate::BEST_COMPRESSION {
                    2
                } else if level == beam_deflate::NO_COMPRESSION || level == beam_deflate::BEST_SPEED
                {
                    4
                } else {
                    0
                };
                let mut expected = GZIP_HEADER;
                expected[8] = xfl;
                let mut expected = expected.to_vec();
                expected.extend_from_slice(&raw);
                expected.extend_from_slice(&crc32_step(0, input).to_le_bytes());
                expected.extend_from_slice(&(input.len() as u32).to_le_bytes());
                assert_eq!(
                    member, expected,
                    "the gzip member at level {level} for {} bytes",
                    input.len()
                );
            }
        }
    }

    #[test]
    fn a_non_small_integer_is_badarg_and_never_a_binary() {
        // Guard against a silently wrong result: an integer argument must not be
        // read as a byte value.
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let big = ctx.int(1 << 40).expect("fits");
        assert!(matches!(compress_1(&mut ctx, &[big]), Err(BeamError::BadArg)));
        // The readback helper is the same one the tests above trust.
        let ok = binary(&mut ctx, &[7, 8]);
        assert_eq!(bytes_of(ok), vec![7, 8]);
        assert_eq!(signed_value(ctx.int(5).expect("fits")), 5);
        assert!(access::is_small(ok) == false);
    }

    // -----------------------------------------------------------------------
    // RFC 1952 (`gzip/1` / `gunzip/1`)
    // -----------------------------------------------------------------------

    /// `gzip/1` of `bytes`, as raw bytes.
    fn gzip_of(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Vec<u8> {
        let plain = binary(ctx, bytes);
        bytes_of(gzip_1(ctx, &[plain]).expect("gzip/1"))
    }

    /// `gunzip/1` of the member(s) in `bytes`.
    fn gunzip_of(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Result<Eterm> {
        let packed = binary(ctx, bytes);
        gunzip_1(ctx, &[packed])
    }

    /// A gzip member whose header carries `flags` and the raw `optional` field bytes
    /// (`FHCRC` is appended and computed here), around a deflate payload of `data`.
    fn member_with_header(data: &[u8], flags: u8, optional: &[u8]) -> Vec<u8> {
        let mut header = GZIP_HEADER;
        header[3] = flags;
        let mut member = header.to_vec();
        member.extend_from_slice(optional);
        if flags & GZIP_FLAG_FHCRC != 0 {
            member.extend_from_slice(&(crc32_step(0, &member) as u16).to_le_bytes());
        }
        member.extend_from_slice(
            &Deflate::compress_with(DeflateConfig::raw(DEFAULT_COMPRESSION_LEVEL), data)
                .expect("the raw framing"),
        );
        member.extend_from_slice(&crc32_step(0, data).to_le_bytes());
        member.extend_from_slice(&(data.len() as u32).to_le_bytes());
        member
    }

    /// Asserts that `result` is the bare-atom `error:data_error` raise.
    fn assert_data_error(result: Result<Eterm>, what: &str) {
        match result {
            Err(BeamError::Raise(raise)) => assert_eq!(
                raise.reason,
                ExitReason::atom("data_error"),
                "{what}: the reason"
            ),
            other => panic!("{what}: expected error:data_error, got {other:?}"),
        }
    }

    #[test]
    fn gzip_writes_the_oracle_goldens_byte_for_byte() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        // `zlib:gzip(<<"hello">>)` and `zlib:gzip(<<>>)` under OTP 29.1.1 / erts-17.1,
        // byte for byte: the fixed header, the raw payload (`zip/1`'s bytes for this
        // body), then the CRC-32 and ISIZE of the original.
        assert_eq!(
            gzip_of(&mut ctx, b"hello"),
            vec![
                31, 139, 8, 0, 0, 0, 0, 0, 0, 3, // the header
                203, 72, 205, 201, 201, 7, 0, // the deflate payload
                134, 166, 16, 54, // CRC-32 (LE)
                5, 0, 0, 0, // ISIZE (LE)
            ]
        );
        assert_eq!(
            gzip_of(&mut ctx, b""),
            vec![31, 139, 8, 0, 0, 0, 0, 0, 0, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn gzip_and_gunzip_round_trip_over_several_sizes() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        for size in [0usize, 1, 13, 1000, 100_000] {
            let payload: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let member = gzip_of(&mut ctx, &payload);
            assert_eq!(
                &member[..GZIP_HEADER.len()],
                &GZIP_HEADER,
                "the header is the fixed one"
            );
            assert_eq!(
                bytes_of(gunzip_of(&mut ctx, &member).expect("gunzip/1")),
                payload,
                "round trip at size {size}"
            );
        }
    }

    #[test]
    fn gunzip_concatenates_members_and_rejects_anything_else_after_one() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let first = gzip_of(&mut ctx, b"hello");
        let second = gzip_of(&mut ctx, b"world");

        // The oracle resets at a member boundary (`inflateInit(Z, 16 + ?MAX_WBITS,
        // reset)`, `zlib.erl:776`), so concatenated members are one call's output:
        // `zlib:gunzip(<<A/binary, B/binary>>)` is `<<"helloworld">>`.
        let mut both = first.clone();
        both.extend_from_slice(&second);
        assert_eq!(
            bytes_of(gunzip_of(&mut ctx, &both).expect("both members")),
            b"helloworld"
        );

        // Anything left over that is not another complete member is `data_error`, as
        // in the oracle (junk bytes, a lone magic byte, a bare header, and — the
        // degenerate case — the empty input, which completes no member at all).
        let mut junk = first.clone();
        junk.extend_from_slice(&[99, 99]);
        assert_data_error(gunzip_of(&mut ctx, &junk), "two junk bytes");

        let mut lone_magic = first.clone();
        lone_magic.push(GZIP_MAGIC[0]);
        assert_data_error(gunzip_of(&mut ctx, &lone_magic), "a lone magic byte");

        let mut bare_header = first.clone();
        bare_header.extend_from_slice(&GZIP_HEADER);
        assert_data_error(gunzip_of(&mut ctx, &bare_header), "a bare header");

        assert_data_error(gunzip_of(&mut ctx, b""), "the empty input");
    }

    #[test]
    fn gunzip_honours_every_optional_header_field() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let payload = b"hello, gzip";

        // All four optional fields at once: FEXTRA's length and bytes, the
        // zero-terminated FNAME and FCOMMENT, and a correct FHCRC. FTEXT (0x01) is
        // informational and must be ignored too.
        let mut optional = vec![3, 0, 0xaa, 0xbb, 0xcc];
        optional.extend_from_slice(b"name.txt\0");
        optional.extend_from_slice(b"a comment\0");
        let flags = 0x01 | GZIP_FLAG_FHCRC | GZIP_FLAG_FEXTRA | GZIP_FLAG_FNAME | GZIP_FLAG_FCOMMENT;
        let member = member_with_header(payload, flags, &optional);
        assert_eq!(
            bytes_of(gunzip_of(&mut ctx, &member).expect("every optional field")),
            payload
        );

        // A wrong FHCRC is `data_error` — zlib compares it rather than skipping it
        // (`inflate.c:679`).
        let mut bad_crc = member.clone();
        let crc_at = GZIP_HEADER.len() + optional.len();
        bad_crc[crc_at] ^= 0xff;
        assert_data_error(gunzip_of(&mut ctx, &bad_crc), "a bad FHCRC");

        // A reserved flag bit is `data_error` (`state->flags & 0xe000`, `inflate.c:563`).
        let reserved = member_with_header(payload, 0x20, &[]);
        assert_data_error(gunzip_of(&mut ctx, &reserved), "a reserved FLG bit");

        // A zero-terminated field that runs past the input is `data_error`, never a
        // guessed payload offset.
        let unterminated = member_with_header(payload, GZIP_FLAG_FNAME, b"no terminator");
        assert_data_error(gunzip_of(&mut ctx, &unterminated), "an unterminated FNAME");

        // An FEXTRA length that runs past the input is `data_error` too.
        let oversized_extra = member_with_header(payload, GZIP_FLAG_FEXTRA, &[200, 0, 1, 2]);
        assert_data_error(
            gunzip_of(&mut ctx, &oversized_extra),
            "an FEXTRA length past the input",
        );
    }

    #[test]
    fn gunzip_reports_the_oracles_failure_matrix() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        let good = gzip_of(&mut ctx, b"hello");
        let length = good.len();

        // Every probe below is `data_error` in the oracle (OTP 29.1.1 / erts-17.1).
        let mut bad_magic = good.clone();
        bad_magic[0] = 0;
        assert_data_error(gunzip_of(&mut ctx, &bad_magic), "a bad ID1");

        let mut bad_magic2 = good.clone();
        bad_magic2[1] = 0;
        assert_data_error(gunzip_of(&mut ctx, &bad_magic2), "a bad ID2");

        let mut bad_cm = good.clone();
        bad_cm[2] = 7;
        assert_data_error(gunzip_of(&mut ctx, &bad_cm), "a bad CM");

        let mut bad_crc = good.clone();
        bad_crc[length - 8] ^= 0xff;
        assert_data_error(gunzip_of(&mut ctx, &bad_crc), "a corrupted CRC-32");

        let mut bad_isize = good.clone();
        bad_isize[length - 1] ^= 0xff;
        assert_data_error(gunzip_of(&mut ctx, &bad_isize), "a corrupted ISIZE");

        // Four bytes short of the deflate stream's last byte, and truncated inside the
        // header.
        assert_data_error(
            gunzip_of(&mut ctx, &good[..length - 1]),
            "truncated inside the trailer",
        );
        assert_data_error(gunzip_of(&mut ctx, &good[..9]), "truncated in the header");
        assert_data_error(gunzip_of(&mut ctx, &good[..10]), "only the header");

        // A non-iodata argument is the one `badarg` of the pair.
        for bad in [ctx.int(42).expect("fits"), ctx.atom("hello").expect("seeded")] {
            assert!(matches!(gzip_1(&mut ctx, &[bad]), Err(BeamError::BadArg)));
            assert!(matches!(gunzip_1(&mut ctx, &[bad]), Err(BeamError::BadArg)));
        }
    }

    #[test]
    fn the_expansion_limit_is_the_formats_own_and_uncompress_has_no_artificial_ceiling() {
        // The old ceiling was `beam_codec::compress::MAX_UNCOMPRESSED` (32 MiB): a
        // payload above it was `data_error` where the oracle inflates it. The input
        // here is tiny (a run of zeros), so only the *inflated* size crosses the old
        // ceiling — and the test heap is deliberately far too small to hold the
        // result, so the observable failure is the heap's own `Alloc`, never
        // `data_error`. That is the regression witness: the inflater must not stop.
        let payload = vec![0u8; MAX_UNCOMPRESSED + 1];
        let stream = beam_deflate::compress(&payload, DEFAULT_COMPRESSION_LEVEL).expect("deflate");
        let bound = expansion_limit(stream.len());
        assert!(
            bound >= payload.len(),
            "the derived bound {bound} must admit a {}-byte payload from a {}-byte stream",
            payload.len(),
            stream.len()
        );

        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);
        let packed = binary(&mut ctx, &stream);
        match uncompress_1(&mut ctx, &[packed]) {
            Ok(_) => panic!("the tiny test heap cannot hold {} bytes", payload.len()),
            Err(BeamError::Alloc(_)) => {}
            Err(other) => panic!("the old ceiling must be gone, got {other:?}"),
        }
    }
}
