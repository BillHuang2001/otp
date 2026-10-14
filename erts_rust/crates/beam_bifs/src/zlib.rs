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

//! `zlib:compress/1` and `zlib:uncompress/1` — the two convenience functions of the
//! preloaded `zlib` module, served over the workspace's own RFC 1950 codec.
//!
//! # Why they are registered here
//!
//! `zlib` is a **preloaded** module: `../../erts/preloaded/src/zlib.erl` is real
//! Erlang, and `compress/1`/`uncompress/1` are written over the stream interface
//! (`open/0` → `deflateInit/2` → `deflate/3` → `deflateEnd/1` → `close/1`). Every
//! one of those is an `erlang:nif_error(undef)` stub
//! (`zlib.erl:185-194`, `open_nif/0` …) for the statically linked zlib NIF
//! (`../../erts/emulator/beam/erl_zlib.c`), which this runtime does not carry — so
//! the whole family is `undef` unless a host serves it.
//!
//! The **capability** already exists in the workspace, though: `beam_codec::compress`
//! is `flate2` with the `rust_backend` (miniz_oxide) in the same RFC 1950
//! (zlib-wrapped deflate) framing `erl_zlib_compress2` produces, and `beam_codec` is
//! already a dependency of this crate — **no new dependency edge**. `compress/1` and
//! `uncompress/1` are therefore registered here as the two stdlib fast paths the
//! registry also carries for `erlang:memory/0,1` (neither is a `bif.tab` entry: the
//! table declares **NIFs**, and these two are `zlib.erl` functions).
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
//! # Byte identity is not promised; interoperability is
//!
//! The deflate payload is produced by miniz_oxide, not by zlib, so the exact bytes
//! of a compressed stream may differ from the oracle's (the framing, the round trip
//! and RFC 1950 conformance are what this module pins). Any conforming inflater —
//! the oracle's included — decodes what this module emits, and this module decodes
//! what the oracle emits (see `tests/zlib.rs`).
//!
//! # Documented limits (never a silently wrong result)
//!
//! * **A decompressed payload above `beam_codec::compress::MAX_UNCOMPRESSED`
//!   (32 MiB) is `data_error`**, where the oracle would inflate it. The bound is
//!   `beam_codec::compress::decompress`'s: it exists so a corrupt or hostile stream
//!   can never drive an unbounded allocation, and it is the only shape of this
//!   module's behaviour that is not the oracle's. A typed error, never a wrong value.
//! * **The rest of the `zlib` interface is not served.** The stream API
//!   (`open/0`, `deflate/2,3`, `inflate/2,3`, `deflateInit/1,2,6`, `gzip/1`,
//!   `gunzip/1`, `zip/1`, `unzip/1`, …) needs a real per-stream resource with a
//!   controlling process, so it stays `undef` (its NIF stubs), exactly as before.
//!   Only the two self-contained functions are registered.

use beam_common::{BeamError, ExitReason, Raise, Result};
use beam_term::Eterm;

use beam_codec::compress::{compress as zlib_compress, decompress as zlib_decompress};

use crate::binary::iodata_bytes;
use crate::bits;
use crate::context::BifContext;
use crate::registry::BifRegistry;

/// The compression level `zlib:compress/1` uses: `deflateInit(Z, default)`
/// (`zlib.erl:686`), i.e. zlib's `Z_DEFAULT_COMPRESSION`, which zlib documents as
/// level 6.
const DEFAULT_COMPRESSION_LEVEL: u8 = 6;

/// The shortest argument `zlib:uncompress/1` will hand to the inflater
/// (`Size >= 8`, `zlib.erl:703`); anything shorter is `data_error`.
const MIN_UNCOMPRESS_LENGTH: usize = 8;

/// `zlib:compress(Data)` — `Data` (iodata) compressed into one zlib stream.
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

/// `zlib:uncompress(Data)` — the payload of the zlib stream `Data` (iodata).
///
/// # Errors
///
/// [`BeamError::BadArg`] for a non-iodata argument; the `error`-class
/// [`data_error`] for a stream shorter than [`MIN_UNCOMPRESS_LENGTH`] bytes and for
/// one the inflater rejects (a bad header, a truncated deflate block, a bad Adler-32
/// trailer, or a payload past `beam_codec::compress::MAX_UNCOMPRESSED`).
fn uncompress_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let bytes = iodata_bytes(args[0])?;
    if bytes.len() < MIN_UNCOMPRESS_LENGTH {
        return Err(data_error());
    }
    let inflated = match zlib_decompress(&bytes) {
        Ok(inflated) => inflated,
        // Every zlib failure is `BadArg` at the codec's boundary; that is exactly the
        // class `zlib:uncompress/1` reports as `data_error`. Any other error class is
        // a genuine capability gap and is passed through untouched.
        Err(BeamError::BadArg) => return Err(data_error()),
        Err(other) => return Err(other),
    };
    bits::make_binary(ctx.heap(), &inflated)
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

/// Registers the two self-contained `zlib` functions.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("zlib", "compress", 1, compress_1);
    reg.register("zlib", "uncompress", 1, uncompress_1);
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

        // `zlib:compress(<<"hello">>)` is a 13-byte binary in the oracle
        // (OTP 29.1.1 / erts-17.1, zlib 1.3): `<<120,156,203,72,205,201,201,7,0,6,44,2,21>>`.
        // The deflate payload is miniz_oxide's, not zlib's, so only the framing (a
        // 2-byte header and a 4-byte Adler trailer) and a same-order size are pinned.
        let stream = bytes_of(compressed);
        assert_eq!(stream[0], 120, "the zlib header's first byte");
        assert_eq!(stream[1], 156, "the zlib header's second byte (level 6)");
        assert_eq!(
            stream.len(),
            13,
            "miniz_oxide's stream for `<<\"hello\">>` is the oracle's 13 bytes"
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
    fn register_installs_the_two_zlib_functions_with_the_right_arity() {
        let mut reg = BifRegistry::new();
        register(&mut reg);

        assert!(reg.contains("zlib", "compress", 1));
        assert!(reg.contains("zlib", "uncompress", 1));
        assert!(!reg.contains("zlib", "compress", 2));
        assert!(!reg.contains("zlib", "zip", 1));
        assert!(!reg.contains("zlib", "unzip", 1));
        assert!(!reg.contains("zlib", "open", 0));
        assert_eq!(reg.len(), 2);
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
}
