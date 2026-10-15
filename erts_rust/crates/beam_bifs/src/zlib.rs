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

//! `zlib:compress/1`, `zlib:uncompress/1`, `zlib:zip/1`, `zlib:unzip/1`, `zlib:gzip/1`
//! and `zlib:gunzip/1` — the six self-contained convenience functions of the preloaded
//! `zlib` module, served over the workspace's own codecs.
//!
//! # Why they are registered here
//!
//! `zlib` is a **preloaded** module: `../../erts/preloaded/src/zlib.erl` is real
//! Erlang, and all six functions are written over the stream interface
//! (`open/0` → `deflateInit/2` → `deflate/3` → `deflateEnd/1` → `close/1`). Every
//! one of those is an `erlang:nif_error(undef)` stub
//! (`zlib.erl:185-194`, `open_nif/0` …) for the statically linked zlib NIF
//! (`../../erts/emulator/beam/erl_zlib.c`), which this runtime does not carry — so
//! the whole family is `undef` unless a host serves it.
//!
//! The **capability** already exists in the workspace, though: `beam_codec::compress`
//! is `flate2` with the `rust_backend` (miniz_oxide) in the same RFC 1950
//! (zlib-wrapped deflate) framing `erl_zlib_compress2` produces, and `beam_codec` is
//! already a dependency of this crate — **no new dependency edge**. All six are
//! therefore registered here as stdlib fast paths the registry also carries for
//! `erlang:memory/0,1` (none is a `bif.tab` entry: the table declares **NIFs**, and
//! these are `zlib.erl` functions). `zip/1` and `gzip/1` reuse that same encoder, and
//! `unzip/1`/`gunzip/1` are served by this crate's own raw-DEFLATE decoder,
//! [`crate::deflate`]: the codec is RFC 1950**-only** (its `ZlibDecoder` validates the
//! header and the Adler-32 trailer), so a header-less stream cannot be re-wrapped into
//! it, and this crate may not add an external dependency.
//!
//! # `zlib:compress/1`
//!
//! * the argument is **iodata** (`zlib.erl:683-693` hands it to `deflate/3`, whose
//!   NIF takes it through `erlang:iolist_to_iovec/1`), so every shape
//!   [`crate::binary::iodata_bytes`] accepts is accepted, and everything else —
//!   a bare integer, an atom, a list element outside `0..=255`, a bitstring that is
//!   not byte aligned — is the oracle's `error:badarg`;
//! * the level is zlib's `Z_DEFAULT_COMPRESSION`, which `deflateInit(Z, default)`
//!   selects and which is level 6 ([`DEFAULT_COMPRESSION_LEVEL`]);
//! * the result is a **binary** — the whole zlib stream (2-byte header, deflate
//!   data, Adler-32 trailer), so `zlib:compress(<<>>)` is 8 bytes.
//!
//! The stream is **byte-identical to the oracle's for a short body** (one fixed-Huffman
//! block in both encoders: `<<>>`, `<<"a">>`, `<<"abc">>`, `<<"hello">>`, `<<"world">>`,
//! `<<"abcdefgh">>` are goldens in `tests/zlib.rs`) but not for a repetitive one, where
//! the encoder's block-type choice differs — see "`compress/1`'s byte-size divergence on
//! repetitive bodies" below. The framing, the Adler-32 and the round trip are the
//! oracle's in every case.
//!
//! # `zlib:uncompress/1`
//!
//! `zlib.erl:699-720` decides in two steps, and both decide the error:
//!
//! * the argument must again be iodata — `iolist_size/1` is the first thing the body
//!   does, so a non-iodata argument is `error:badarg`;
//! * a stream shorter than [`MIN_UNCOMPRESS_LENGTH`] (8) bytes is
//!   `error:data_error`, raised *inside* the `try`'s `of` body and therefore **not**
//!   covered by that body's `catch _:_ -> badarg` — and a longer one the inflater
//!   cannot inflate is the NIF's own `data_error`, so a bad or truncated stream is
//!   `data_error` too, never `badarg`.
//!
//! The error classes are pinned against the oracle (OTP 29.1.1 / erts-17.1):
//! `zlib:uncompress(<<>>)` and `zlib:uncompress(<<1,2,3,4,5,6,7>>)` are
//! `data_error` (the length arm), `zlib:uncompress(<<1,2,3,4,5,6,7,8>>)` and
//! `zlib:uncompress(<<120,156>>)` are `data_error` (the inflater), while
//! `zlib:uncompress(42)` and `zlib:uncompress(hello)` are `badarg`.
//!
//! There is **no size ceiling**: the payload is inflated through
//! `beam_codec::compress::decompress_bounded` with an [`expansion_limit`] derived from
//! the stream's own length, which is at or above the greatest output the DEFLATE format
//! can produce from that input (`1032:1`) — so every stream the oracle inflates
//! inflates here too, while a corrupt or hostile stream still cannot drive an
//! unbounded allocation.
//!
//! # `zlib:zip/1` and `zlib:unzip/1`
//!
//! The **raw** pair: `zlib.erl:727-733` calls
//! `deflateInit(Z, default, deflated, -MAX_WBITS, 8, default)` and `zlib.erl:748` calls
//! `inflateInit(Z, -MAX_WBITS)` — a 15-bit window with **no** zlib header and **no**
//! Adler-32 trailer. `zlib:zip/1` is *not* gzip (gzip is the separate RFC 1952
//! `zlib:gzip/1` below).
//!
//! * `zip/1` takes iodata (the same `erlang:iolist_to_iovec/1` hand-off as
//!   `compress/1`) and returns the raw DEFLATE payload: the zlib stream
//!   `beam_codec::compress::compress` emits, stripped of its 2-byte header and 4-byte
//!   trailer by [`raw_deflate_of`] — a **checked** strip, so a stream that is not a
//!   header-less-deflate carrier is a typed capability gap rather than a wrong slice.
//! * `unzip/1` takes iodata too and inflates the raw stream with
//!   [`crate::deflate::inflate`]. **There is no length arm**: a non-iodata argument is
//!   `error:badarg`, and everything else the inflater cannot finish — including the
//!   empty stream — is the bare `error:data_error` ([`data_error`]).
//! * bytes after the end of the deflate stream are **ignored**, exactly as
//!   `uncompress/1` ignores them: `zlib:unzip(<<Zip/binary, 99, 99>>)` is the payload.
//!
//! The error classes and both goldens are pinned against the oracle
//! (OTP 29.1.1 / erts-17.1): `zlib:zip(<<"hello">>)` is `<<203,72,205,201,201,7,0>>` and
//! `zlib:zip(<<>>)` is `<<3,0>>` while `zlib:unzip(<<>>)`, `zlib:unzip(<<1,2,3>>)` and
//! `zlib:unzip(<<1,2,3,4,5,6,7,8>>)` are `data_error`, and `zlib:unzip(42)`,
//! `zlib:unzip(hello)`, `zlib:zip(42)` and `zlib:zip([1,2|3])` are `badarg`.
//!
//! # `zlib:gzip/1` and `zlib:gunzip/1` (RFC 1952)
//!
//! `zlib.erl:759-763` deflates with window bits `16 + ?MAX_WBITS` (31) and
//! `zlib.erl:775-779` inflates with the same, so a **gzip member** is
//! `[10-byte header][raw DEFLATE][CRC-32 LE][ISIZE LE]` (RFC 1952 §2.3/§2.3.1):
//!
//! * `gzip/1` takes iodata like `compress/1` and writes the fixed header zlib itself
//!   writes (`1f 8b 08 00` — deflate, no flags, no MTIME — then `XFL = 0`, which is what
//!   `Z_DEFAULT_COMPRESSION` selects, and `OS = 3`, the Unix code), followed by the very
//!   same raw DEFLATE payload `zip/1` emits and the CRC-32/ISIZE of the **original**
//!   data ([`crate::checksum::crc32_step`]). The deflate payload is miniz_oxide's, so
//!   only interoperability — not byte identity beyond the goldens — is promised.
//! * `gunzip/1` parses the header (honouring every `FLG` bit: `FEXTRA`, `FNAME`,
//!   `FCOMMENT` are skipped, `FHCRC` is **verified** against the header's own CRC-32,
//!   and the reserved bits are rejected), inflates the member with
//!   [`crate::deflate::inflate`], verifies the CRC-32 and the `ISIZE` trailer, and
//!   repeats for every **concatenated** member — exactly what the oracle's
//!   `EOS_BEHAVIOR_RESET` (`zlib.erl:776`) does. Any input left over that is not
//!   another complete member is `data_error`, as in the oracle (the oracle's
//!   `zlib:gunzip(<<Member/binary, 99, 99>>)` is `data_error`, while
//!   `zlib:gunzip(<<A/binary, B/binary>>)` is the two payloads concatenated).
//!
//! The whole failure matrix is the bare `error:data_error` (a malformed header, a bad
//! magic byte, a bad `CM`, a reserved flag bit, a truncated or corrupt member, a bad
//! CRC-32, a bad `ISIZE`, trailing junk, and the **empty** input, which completes no
//! member at all) — `badarg` is reserved for a non-iodata argument. Both are pinned
//! live against OTP 29.1.1 / erts-17.1.
//!
//! # Byte identity is not promised; interoperability is
//!
//! The deflate payload is produced by miniz_oxide, not by zlib, so the exact bytes
//! of a compressed stream may differ from the oracle's (the framing, the round trip
//! and RFC 1950/RFC 1951/RFC 1952 conformance are what this module pins) — for the
//! `zip/1` and `gzip/1` goldens below they happen to agree byte for byte. Any
//! conforming inflater — the oracle's included — decodes what this module emits, and
//! this module decodes what the oracle emits: `tests/zlib.rs` cross-checks both
//! directions against streams the system `erl` produced.
//!
//! ## `compress/1`'s byte-size divergence on repetitive bodies (documented, closed as a gap)
//!
//! The two encoders choose different **block types** for a repetitive body, so
//! `byte_size(zlib:compress(B))` is the encoder's own answer and not the oracle's.
//! The measured witness is `zlib:compress(<<0:8192>>)` — note `<<0:8192>>` is 8192
//! *bits*, i.e. 1024 zero bytes:
//!
//! | | bytes | deflate body |
//! |---|---|---|
//! | oracle (real libz, `deflate_slow`, level 6) | **17** | a **fixed-Huffman** block (`<<99,96,24,5,163,96,20,140,84,0,0>>`) |
//! | this module (miniz_oxide, level 6) | **23** | a **dynamic-Huffman** block (`<<237,192,1,1,0,0,0,130,32,255,175,110,72,80,192,171,1>>`) |
//!
//! Both streams carry the same 2-byte header `<<120,156>>` and the same Adler-32
//! trailer `<<4,0,0,1>>`, and both inflate to the same 1024 bytes, so only the
//! *size* (and the body) differ. `erts/emulator/beam/erl_zlib.c:62,152` initialises
//! the stream with `deflateInit(streamp, level)`, which is `deflateInit2(…, MAX_WBITS
//! = 15, DEF_MEM_LEVEL = 8, Z_DEFAULT_STRATEGY)`, and `flate2`'s `rust_backend` exposes
//! **only** `Compression` (a level) — no `strategy` and no `memLevel` — so the choice is
//! miniz_oxide's own. The whole level range was swept for this body: level 0 stores
//! (1035 bytes), level 1 is 26 bytes, and levels 2..=9 are all 23 bytes — **no level
//! reproduces the oracle's 17**, because miniz never emits the fixed-Huffman block
//! zlib's `deflate_slow` picks for a long zero run (its lazy-match/`num_probes` search
//! differs independently as well). Closing the gap would need either the real libz (a new
//! dependency edge, deliberately out of scope) or a faithful zlib `deflate_slow` port;
//! special-casing an input pattern is forbidden. The same divergence shows on the medium
//! body `list_to_binary(lists:seq(1,100))` (oracle 108 bytes, here 111), which is why
//! `tests/zlib.rs` pins that body's round trip and not its size.
//!
//! **Short bodies are unaffected** — a body small enough to be one fixed-Huffman block in
//! both encoders is byte-identical (`zlib:compress(<<>>)`, `<<"a">>`, `<<"abc">>`,
//! `<<"hello">>`, `<<"world">>` and `<<"abcdefgh">>` are byte-exact goldens in
//! `tests/zlib.rs`), which is also why `uncompress(compress(B)) =:= B` and the `abc`
//! golden are the shapes the differential corpus pins.
//!
//! # Documented limits (never a silently wrong result)
//!
//! * **`zip/1`'s counterpart `unzip/1` still has an output ceiling**
//!   (`beam_codec::compress::MAX_UNCOMPRESSED`, 32 MiB): a payload above it is
//!   `data_error` where the oracle would inflate it. `uncompress/1` and `gunzip/1` no
//!   longer have one (both inflate up to [`expansion_limit`], the format's own
//!   `1032:1` bound). A typed error, never a wrong value.
//! * **The rest of the `zlib` interface is not served.** The stream API
//!   (`open/0`, `deflate/2,3`, `inflate/2,3`, `deflateInit/1,2,6`, …) needs a real
//!   per-stream resource with a controlling process, so it stays `undef` (its NIF
//!   stubs), exactly as before. Only the six self-contained functions are registered.

use beam_common::{BeamError, ExitReason, Raise, Result};
use beam_term::Eterm;

use beam_codec::compress::{
    compress as zlib_compress, decompress_bounded as zlib_decompress_bounded, MAX_UNCOMPRESSED,
};

use crate::binary::iodata_bytes;
use crate::bits;
use crate::checksum::crc32_step;
use crate::context::BifContext;
use crate::deflate;
use crate::error::unsupported_bif;
use crate::registry::BifRegistry;

/// The compression level `zlib:compress/1` uses: `deflateInit(Z, default)`
/// (`zlib.erl:686`), i.e. zlib's `Z_DEFAULT_COMPRESSION`, which zlib documents as
/// level 6.
const DEFAULT_COMPRESSION_LEVEL: u8 = 6;

/// The shortest argument `zlib:uncompress/1` will hand to the inflater
/// (`Size >= 8`, `zlib.erl:703`); anything shorter is `data_error`.
const MIN_UNCOMPRESS_LENGTH: usize = 8;

/// What a zlib stream always carries around its payload: the 2-byte header and the
/// 4-byte big-endian Adler-32 trailer (RFC 1950 §2.2/§2.3) — exactly the bytes
/// `zlib:zip/1` drops.
const ZLIB_FRAMING_LENGTH: usize = 6;

/// `CM` — the compression-method nibble of a zlib header's first byte meaning
/// "deflate" (RFC 1950 §2.2).
const DEFLATE_METHOD: u8 = 8;

/// `FDICT` — the preset-dictionary flag of a zlib header's `FLG` byte (RFC 1950 §2.2).
/// Set would mean a 4-byte dictionary id sits between the header and the payload.
const FDICT_BIT: u8 = 0x20;

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

/// `ID1`/`ID2` — the two magic bytes that open every gzip member (RFC 1952 §2.3.1).
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// The fixed part of a gzip member header (RFC 1952 §2.3.1): the magic, `CM` = deflate,
/// `FLG` = 0 (no extra field, name, comment or header CRC), `MTIME` = 0 (absent), then
/// `XFL` = 0 and `OS` = 3.
///
/// `XFL` is `0` for every level but 1 and 9 (zlib writes `4`/`2`/`0`), and
/// [`DEFAULT_COMPRESSION_LEVEL`] is 6; `OS` = 3 is zlib's Unix `OS_CODE`, so this is
/// byte for byte the header the oracle's `zlib:gzip/1` writes.
const GZIP_HEADER: [u8; 10] = [GZIP_MAGIC[0], GZIP_MAGIC[1], DEFLATE_METHOD, 0, 0, 0, 0, 0, 0, 3];

/// `FLG & 0xE0` — the three reserved flag bits, which zlib rejects (§2.3.1).
const GZIP_FLAG_RESERVED: u8 = 0xe0;
/// `FHCRC` — a 2-byte CRC-16 of the header follows the optional fields (§2.3.1).
const GZIP_FLAG_FHCRC: u8 = 0x02;
/// `FEXTRA` — a 2-byte `XLEN` and `XLEN` extra bytes follow (§2.3.1).
const GZIP_FLAG_FEXTRA: u8 = 0x04;
/// `FNAME` — a zero-terminated original file name follows (§2.3.1).
const GZIP_FLAG_FNAME: u8 = 0x08;
/// `FCOMMENT` — a zero-terminated comment follows (§2.3.1).
const GZIP_FLAG_FCOMMENT: u8 = 0x10;

/// What follows a gzip member's DEFLATE payload: the 4-byte CRC-32 of the *original*
/// data and the 4-byte `ISIZE` (§2.3.1), both little-endian.
const GZIP_TRAILER_LENGTH: usize = 8;
/// `zlib:compress(Data)` — `Data` (iodata) compressed into one zlib stream.
///
/// The deflate payload is [`zlib_compress`]'s (miniz_oxide's, not zlib's), so the
/// stream is byte-identical to the oracle's only where both encoders pick the same
/// blocks — a short body, e.g. `<<"abc">>`; a repetitive body's **size** differs
/// (the module doc's byte-identity section). The 2-byte header, the Adler-32 trailer
/// and the round trip are the oracle's in every case, and this function never
/// special-cases an input pattern.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument — the oracle's `badarg` from
/// `erlang:iolist_to_iovec/1` — and for an internal codec failure (there is no such
/// path: the encoder writes into a `Vec`), which the codec reports with the same
/// class.
fn compress_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    let deflated = zlib_compress(&bytes, DEFAULT_COMPRESSION_LEVEL)?;
    bits::make_binary(ctx.heap(), &deflated)
}

/// `zlib:zip(Data)` — `Data` (iodata) as one **raw DEFLATE** (RFC 1951) stream.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument — the oracle's `badarg` from
/// `erlang:iolist_to_iovec/1` — and for an internal codec failure (there is no such
/// path: the encoder writes into a `Vec`), which the codec reports with the same
/// class. A codable stream that is not the expected zlib framing is the typed
/// capability gap [`unsupported_bif`] (see [`raw_deflate_of`]).
fn zip_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    let stream = zlib_compress(&bytes, DEFAULT_COMPRESSION_LEVEL)?;
    bits::make_binary(ctx.heap(), raw_deflate_of(&stream)?)
}

/// `zlib:unzip(Data)` — the payload of the **raw DEFLATE** stream `Data` (iodata).
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument; the `error`-class
/// [`data_error`] for a stream [`crate::deflate::inflate`] cannot finish — the empty
/// stream included, since (unlike `uncompress/1`) `zlib:unzip/1` has no length arm —
/// and for a payload past `beam_codec::compress::MAX_UNCOMPRESSED`.
fn unzip_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    // The decoder reports every failure in the codec's own class (`BadArg`), which is
    // exactly the class `zlib:unzip/1` reports as `data_error`; any other error class
    // is a genuine capability gap and is passed through untouched. The consumed-byte
    // count is not needed: everything after the deflate stream is ignored anyway.
    let (inflated, _consumed) = match deflate::inflate(&bytes, MAX_UNCOMPRESSED) {
        Ok(inflated) => inflated,
        Err(BeamError::BadArg) => return Err(data_error()),
        Err(other) => return Err(other),
    };
    bits::make_binary(ctx.heap(), &inflated)
}

/// The raw DEFLATE payload of the zlib stream [`zlib_compress`] produced.
///
/// A zlib stream is `[CMF][FLG][DEFLATE…][ADLER-32:4 BE]` (RFC 1950 §2.2/§2.3), so the
/// payload is the slice between the 2-byte header and the 4-byte trailer — the byte
/// string `zlib:zip/1`'s `deflateInit(…, -MAX_WBITS, …)` emits, which is why `zip/1`
/// needs no separate encoder.
///
/// The strip is **checked**: the header has to be there ([`ZLIB_FRAMING_LENGTH`]), the
/// compression method has to be deflate ([`DEFLATE_METHOD`]), the preset-dictionary bit
/// has to be clear ([`FDICT_BIT`] — a dictionary would put a 4-byte id where the payload
/// starts) and the header's own check value has to be right
/// (`(CMF << 8 | FLG) % 31 == 0`, RFC 1950 §2.2). A violation means the encoder no longer
/// produces what this function assumes, so it is reported as the typed capability gap
/// [`unsupported_bif`] — never as an unchecked slice of the stream, which would be a
/// silently wrong result.
fn raw_deflate_of(stream: &[u8]) -> Result<&[u8]> {
    if stream.len() < ZLIB_FRAMING_LENGTH {
        return Err(unsupported_bif());
    }
    let (cmf, flg) = (stream[0], stream[1]);
    let header_check = ((u32::from(cmf) << 8) | u32::from(flg)) % 31 == 0;
    if cmf & 0x0f != DEFLATE_METHOD || flg & FDICT_BIT != 0 || !header_check {
        return Err(unsupported_bif());
    }
    Ok(&stream[2..stream.len() - 4])
}

/// `zlib:uncompress(Data)` — the payload of the zlib stream `Data` (iodata).
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument; the `error`-class
/// [`data_error`] for a stream shorter than [`MIN_UNCOMPRESS_LENGTH`] bytes and for
/// one the inflater rejects (a bad header, a truncated deflate block, a bad Adler-32
/// trailer).
///
/// There is no size ceiling: the payload is inflated through the codec's *bounded*
/// entry point with an [`expansion_limit`] — the format's own `1032:1` bound on this
/// input — so every stream the oracle inflates inflates here.
fn uncompress_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    if bytes.len() < MIN_UNCOMPRESS_LENGTH {
        return Err(data_error());
    }
    let inflated = match zlib_decompress_bounded(&bytes, expansion_limit(bytes.len())) {
        Ok(inflated) => inflated,
        // Every zlib failure is `BadArg` at the codec's boundary; that is exactly the
        // class `zlib:uncompress/1` reports as `data_error`. Any other error class is
        // a genuine capability gap and is passed through untouched.
        Err(BeamError::BadArg) => return Err(data_error()),
        Err(other) => return Err(other),
    };
    bits::make_binary(ctx.heap(), &inflated)
}

/// `zlib:gzip(Data)` — `Data` (iodata) as one RFC 1952 gzip member.
///
/// The member is [`GZIP_HEADER`], the raw DEFLATE payload [`raw_deflate_of`] takes off
/// the codec's zlib stream (exactly what `zip/1` emits), then the little-endian CRC-32
/// of the **original** `Data` and its `ISIZE` (the payload length modulo `2^32`).
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument — the oracle's `badarg` from
/// `erlang:iolist_to_iovec/1` — and for an internal codec failure (there is no such
/// path: the encoder writes into a `Vec`), which the codec reports with the same
/// class. A codable stream that is not the expected zlib framing is the typed
/// capability gap [`unsupported_bif`] (see [`raw_deflate_of`]).
fn gzip_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    let stream = zlib_compress(&bytes, DEFAULT_COMPRESSION_LEVEL)?;
    let payload = raw_deflate_of(&stream)?;
    let mut member =
        Vec::with_capacity(GZIP_HEADER.len() + payload.len() + GZIP_TRAILER_LENGTH);
    member.extend_from_slice(&GZIP_HEADER);
    member.extend_from_slice(payload);
    member.extend_from_slice(&crc32_step(0, &bytes).to_le_bytes());
    // `ISIZE` is the original length **modulo 2^32** (RFC 1952 §2.3.1), which is what
    // the `as u32` truncation of `usize` performs.
    member.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    bits::make_binary(ctx.heap(), &member)
}

/// `zlib:gunzip(Data)` — the payloads of the RFC 1952 gzip member(s) in `Data` (iodata).
///
/// Every member is parsed ([`gzip_payload_start`]), inflated with
/// [`crate::deflate::inflate`] and verified against its CRC-32 and `ISIZE` trailer, and
/// the payloads are concatenated — the oracle resets its stream at a member boundary
/// (`inflateInit(Z, 16 + ?MAX_WBITS, reset)`, `zlib.erl:776`), so concatenated members
/// are one call's worth of output.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument. **Everything else** is the bare
/// `error:data_error` ([`data_error`]): a malformed header (a missing or bad magic byte,
/// a bad `CM`, a reserved `FLG` bit, a truncated or mismatching optional field), an
/// inflater failure, a truncated/mismatching trailer, and any input left over that is
/// not another complete member — the **empty** input included, since it completes no
/// member at all. That is exactly the oracle's matrix (OTP 29.1.1 / erts-17.1).
fn gunzip_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    // The bound is the format's own, over the whole input, and shared across members.
    let limit = expansion_limit(bytes.len());
    let mut out = Vec::new();
    let mut members = 0usize;
    let mut pos = 0usize;
    while pos < bytes.len() {
        let payload = gzip_payload_start(&bytes, pos)?;
        let (inflated, consumed) = match deflate::inflate(
            &bytes[payload..],
            limit.saturating_sub(out.len()),
        ) {
            Ok(inflated) => inflated,
            // The decoder reports every failure in the codec's own class (`BadArg`),
            // which is exactly the class `zlib:gunzip/1` reports as `data_error`; any
            // other error class is a genuine capability gap and is passed through.
            Err(BeamError::BadArg) => return Err(data_error()),
            Err(other) => return Err(other),
        };
        let trailer = payload + consumed;
        if trailer + GZIP_TRAILER_LENGTH > bytes.len() {
            return Err(data_error());
        }
        let crc = u32::from_le_bytes(
            bytes[trailer..trailer + 4]
                .try_into()
                .expect("the slice is four bytes"),
        );
        if crc != crc32_step(0, &inflated) {
            return Err(data_error());
        }
        let isize = u32::from_le_bytes(
            bytes[trailer + 4..trailer + GZIP_TRAILER_LENGTH]
                .try_into()
                .expect("the slice is four bytes"),
        );
        if isize != inflated.len() as u32 {
            return Err(data_error());
        }
        out.extend_from_slice(&inflated);
        members += 1;
        pos = trailer + GZIP_TRAILER_LENGTH;
    }
    if members == 0 {
        // The oracle reaches this through `inflateEnd/1` ("no end of stream was found",
        // `zlib.erl:673-682`), which is a `data_error` too — the empty input.
        return Err(data_error());
    }
    bits::make_binary(ctx.heap(), &out)
}

/// The offset of the DEFLATE payload of the gzip member starting at `start` in `bytes`.
///
/// The header is the fixed ten bytes of [`GZIP_HEADER`]'s own shape (its values are
/// `gzip/1`'s and are not re-read here) followed, per `FLG`, by the optional extra
/// field, file name, comment and header CRC-16 (RFC 1952 §2.3.1). `FHCRC` is
/// **verified** — the stored 16-bit value must be the low half of the CRC-32 of every
/// header byte before it, which is what zlib compares
/// (`hold != (state->check & 0xffff)`, `../../erts/emulator/zlib/inflate.c:679`).
///
/// # Errors
///
/// [`data_error`] for every malformed-header shape: fewer than ten bytes, a wrong magic
/// byte or `CM`, a reserved `FLG` bit, an optional field that runs past the input, or a
/// mismatching `FHCRC`. Never a guessed offset, which would be a silently wrong result.
fn gzip_payload_start(bytes: &[u8], start: usize) -> Result<usize> {
    let rest = &bytes[start..];
    if rest.len() < GZIP_HEADER.len() {
        return Err(data_error());
    }
    if rest[0] != GZIP_MAGIC[0] || rest[1] != GZIP_MAGIC[1] || rest[2] != DEFLATE_METHOD {
        return Err(data_error());
    }
    let flags = rest[3];
    if flags & GZIP_FLAG_RESERVED != 0 {
        return Err(data_error());
    }
    let mut cursor = GZIP_HEADER.len();
    if flags & GZIP_FLAG_FEXTRA != 0 {
        let length = read_u16_le(rest, cursor).ok_or_else(data_error)?;
        cursor += 2;
        let end = cursor + usize::from(length);
        if end > rest.len() {
            return Err(data_error());
        }
        cursor = end;
    }
    if flags & GZIP_FLAG_FNAME != 0 {
        cursor = skip_zero_terminated(rest, cursor)?;
    }
    if flags & GZIP_FLAG_FCOMMENT != 0 {
        cursor = skip_zero_terminated(rest, cursor)?;
    }
    if flags & GZIP_FLAG_FHCRC != 0 {
        let stored = read_u16_le(rest, cursor).ok_or_else(data_error)?;
        if u32::from(stored) != crc32_step(0, &rest[..cursor]) & 0xffff {
            return Err(data_error());
        }
        cursor += 2;
    }
    Ok(start + cursor)
}

/// The little-endian 16-bit field at `offset`, or `None` when it does not fit.
fn read_u16_le(bytes: &[u8], offset: usize) -> Option<u16> {
    let field: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(field))
}

/// The offset just past the zero byte that ends a `FNAME`/`FCOMMENT` field, or
/// [`data_error`] when the input runs out first.
fn skip_zero_terminated(bytes: &[u8], from: usize) -> Result<usize> {
    let mut cursor = from;
    loop {
        match bytes.get(cursor) {
            None => return Err(data_error()),
            Some(0) => return Ok(cursor + 1),
            Some(_) => cursor += 1,
        }
    }
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

/// Registers the six self-contained `zlib` functions.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("zlib", "compress", 1, compress_1);
    reg.register("zlib", "uncompress", 1, uncompress_1);
    reg.register("zlib", "zip", 1, zip_1);
    reg.register("zlib", "unzip", 1, unzip_1);
    reg.register("zlib", "gzip", 1, gzip_1);
    reg.register("zlib", "gunzip", 1, gunzip_1);
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{signed_value, AtomTable};

    use crate::access;

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
        // `<<120,156,203,72,205,201,201,7,0,6,44,2,21>>` under OTP 29.1.1 / erts-17.1,
        // and miniz_oxide emits those very bytes: a short body is one fixed-Huffman
        // block in both encoders, so the whole stream is byte-identical (the
        // repetitive-body divergence the module doc records does not apply).
        let stream = bytes_of(compressed);
        assert_eq!(
            stream,
            vec![120, 156, 203, 72, 205, 201, 201, 7, 0, 6, 44, 2, 21],
            "miniz_oxide's stream for `<<\"hello\">>` is the oracle's byte for byte"
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
    fn register_installs_the_six_zlib_functions_with_the_right_arity() {
        let mut reg = BifRegistry::new();
        register(&mut reg);

        assert!(reg.contains("zlib", "compress", 1));
        assert!(reg.contains("zlib", "uncompress", 1));
        assert!(reg.contains("zlib", "zip", 1));
        assert!(reg.contains("zlib", "unzip", 1));
        assert!(reg.contains("zlib", "gzip", 1));
        assert!(reg.contains("zlib", "gunzip", 1));
        assert!(!reg.contains("zlib", "compress", 2));
        assert!(!reg.contains("zlib", "zip", 2));
        assert!(!reg.contains("zlib", "unzip", 2));
        assert!(!reg.contains("zlib", "gzip", 2));
        assert!(!reg.contains("zlib", "gunzip", 2));
        // The stream API stays unserved (its NIF stubs).
        assert!(!reg.contains("zlib", "open", 0));
        assert_eq!(reg.len(), 6);
    }

    #[test]
    fn zip_produces_the_oracle_golden_raw_deflate_streams() {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = context(&mut heap, &atoms);

        // `zlib:zip(<<"hello">>)` and `zlib:zip(<<>>)` under OTP 29.1.1 / erts-17.1,
        // byte for byte (both fixed-Huffman blocks, verified live against the oracle).
        // The zlib stream `compress/1` builds for `<<"hello">>` is
        // `[120,156] ++ payload ++ [6,44,2,21]`, so stripping the framing (2 + 4 bytes)
        // reproduces the oracle's raw payload exactly.
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
    fn the_raw_deflate_strip_rejects_a_stream_it_does_not_understand() {
        // The strip is checked, so a stream that is not a header-less-deflate carrier
        // is a typed capability gap, never a silently wrong slice of it.
        for stream in [
            &b""[..],
            &[120, 156, 3, 0][..],       // too short: no room for the trailer
            &[119, 9, 3, 0, 0, 0][..],   // CM = 7, not deflate
            &[120, 157, 3, 0, 0, 0][..], // FLG's check value is wrong
        ] {
            assert!(
                matches!(raw_deflate_of(stream), Err(BeamError::Raise(raise))
                    if raise.reason == ExitReason::atom("unsupported_bif")),
                "{stream:?} must be the typed capability gap"
            );
        }
        // The FDICT bit (0x20) makes a preset dictionary part of the stream, so the
        // payload does not start after the 2-byte header: `120,32` has FDICT set and a
        // header check value that is nevertheless valid.
        assert!(
            matches!(raw_deflate_of(&[120, 32, 1, 2, 3, 4, 5, 6]), Err(BeamError::Raise(raise))
                if raise.reason == ExitReason::atom("unsupported_bif")),
            "a preset dictionary must be the typed capability gap"
        );
        // The real encoder's framing is accepted.
        let stream = zlib_compress(b"hello", DEFAULT_COMPRESSION_LEVEL).expect("deflate");
        assert_eq!(
            raw_deflate_of(&stream).expect("the encoder's own framing"),
            &stream[2..stream.len() - 4]
        );
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
        let stream = zlib_compress(data, DEFAULT_COMPRESSION_LEVEL).expect("deflate");
        member.extend_from_slice(raw_deflate_of(&stream).expect("the encoder's framing"));
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
        // byte for byte: the fixed header, miniz_oxide's raw payload (the same bytes
        // `zip/1` emits for this payload), then the CRC-32 and ISIZE of the original.
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
        let stream = zlib_compress(&payload, DEFAULT_COMPRESSION_LEVEL).expect("deflate");
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
