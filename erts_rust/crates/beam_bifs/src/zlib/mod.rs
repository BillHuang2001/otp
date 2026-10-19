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

//! The `zlib` module's surface: the six self-contained convenience functions
//! (`compress/1`, `uncompress/1`, `zip/1`, `unzip/1`, `gzip/1`, `gunzip/1`) served here as
//! stdlib fast paths, plus the stream interface `zlib.erl` is written over.
//!
//! # Why the functions are registered here
//!
//! `zlib` is a **preloaded** module: `../../erts/preloaded/src/zlib.erl` is real Erlang, and
//! the six convenience functions are written over the stream interface (`open/0` →
//! `deflateInit/2` → `deflate/3` → `deflateEnd/1` → `close/1`). The convenience functions
//! are registered as fast paths that intercept before those wrappers run, so they keep
//! working — byte for byte — whatever state the stream layer below them is in. None of the
//! six is a `bif.tab` entry: the table declares **NIFs**, and these are `zlib.erl`
//! functions, which is why they sit beside `erlang:memory/0,1` in the registry rather than
//! in the declared roster.
//!
//! Every byte they emit comes from [`beam_deflate`], the workspace's one zlib 1.3.2 port
//! (the C this repository ships is `../../erts/emulator/zlib/`), so it is the oracle's byte
//! for byte rather than a merely conforming stream — see "Byte identity" below. `zlib.erl`
//! only ever asks for `Z_DEFAULT_COMPRESSION`, which is why that is the only level the
//! six functions use.
//!
//! All six take **iodata** (the set [`crate::binary::iodata_bytes`] accepts), so a bare
//! integer, an atom, a list element outside `0..=255` and a non-byte-aligned bitstring are
//! the oracle's `error:badarg`, and every malformed stream is the bare-atom
//! `error:data_error`.
//!
//! # `zlib:compress/1` and `zlib:uncompress/1` (RFC 1950)
//!
//! `compress/1` is `deflateInit(Z, default)` + `deflate(Z, Data, finish)`: one zlib stream
//! (2-byte header, deflate data, big-endian Adler-32 trailer), so `zlib:compress(<<>>)` is
//! 8 bytes.
//!
//! `uncompress/1` decides in two steps (`zlib.erl:699-720`), and both decide the error: the
//! argument must be iodata (`iolist_size/1` is the body's first call), and a stream shorter
//! than 8 bytes is `error:data_error` — raised *inside* the `try`'s `of` body and therefore
//! not covered by that body's `catch _:_ -> badarg`. A longer stream the inflater cannot
//! inflate is the NIF's own `data_error`, so a bad or truncated stream is `data_error` too,
//! never `badarg`. Pinned against the oracle (OTP 29.1.1 / erts-17.1):
//! `zlib:uncompress(<<>>)` and `zlib:uncompress(<<1,2,3,4,5,6,7>>)` are `data_error` (the
//! length arm), `zlib:uncompress(<<1,2,3,4,5,6,7,8>>)` and `zlib:uncompress(<<120,156>>)`
//! are `data_error` (the inflater), while `zlib:uncompress(42)` and
//! `zlib:uncompress(hello)` are `badarg`.
//!
//! There is **no size ceiling**: the payload is inflated through
//! `beam_deflate`'s bounded entry point with an `expansion_limit` derived from the stream's
//! own length, which is at or above the greatest output the DEFLATE format can produce from
//! that input (`1032:1`) — so every stream the oracle inflates inflates here too, while a
//! corrupt or hostile stream still cannot drive an unbounded allocation.
//!
//! # `zlib:zip/1` and `zlib:unzip/1` (RFC 1951, raw)
//!
//! The **raw** pair: `zlib.erl:727-733` calls
//! `deflateInit(Z, default, deflated, -MAX_WBITS, 8, default)` and `zlib.erl:748` calls
//! `inflateInit(Z, -MAX_WBITS)` — a 15-bit window with **no** zlib header and **no**
//! Adler-32 trailer. `zlib:zip/1` is *not* gzip (that is the separate RFC 1952
//! `zlib:gzip/1` below).
//!
//! * `zip/1` asks [`beam_deflate`] for that framing directly
//!   (`DeflateConfig::raw`); the deflate body does not depend on the `windowBits` *sign*
//!   (only the framing does), so the bytes are the ones
//!   `deflateInit(…, -MAX_WBITS, …)` would have written.
//! * `unzip/1` takes iodata too and inflates the raw stream. **There is no length arm**: a
//!   non-iodata argument is `error:badarg`, and everything else the inflater cannot finish
//!   — including the empty stream — is the bare `error:data_error`.
//! * bytes after the end of the deflate stream are **ignored**, exactly as `uncompress/1`
//!   ignores them: `zlib:unzip(<<Zip/binary, 99, 99>>)` is the payload.
//!
//! Error classes and goldens are pinned against the oracle: `zlib:zip(<<"hello">>)` is
//! `<<203,72,205,201,201,7,0>>` and `zlib:zip(<<>>)` is `<<3,0>>`, while `zlib:unzip(<<>>)`,
//! `zlib:unzip(<<1,2,3>>)` and `zlib:unzip(<<1,2,3,4,5,6,7,8>>)` are `data_error`, and
//! `zlib:unzip(42)`, `zlib:unzip(hello)`, `zlib:zip(42)` and `zlib:zip([1|2])` are
//! `badarg`.
//!
//! # `zlib:gzip/1` and `zlib:gunzip/1` (RFC 1952)
//!
//! `zlib.erl:759-763` deflates with window bits `16 + ?MAX_WBITS` (31) and `zlib.erl:775-779`
//! inflates with the same, so a **gzip member** is `[10-byte header][raw DEFLATE][CRC-32 LE]
//! [ISIZE LE]` (RFC 1952 §2.3/§2.3.1):
//!
//! * `gzip/1` takes iodata like `compress/1` and asks [`beam_deflate`] for that framing
//!   (`DeflateConfig::gzip`), which writes the fixed header zlib itself writes (`1f 8b 08
//!   00` — deflate, no flags, no MTIME — then `XFL = 0`, which is what
//!   `Z_DEFAULT_COMPRESSION` selects, and `OS = 3`, the Unix code), the raw DEFLATE payload
//!   `zip/1` emits and the CRC-32/ISIZE of the **original** data — the oracle's bytes, whole.
//! * `gunzip/1` parses each member's header (honouring every `FLG` bit: `FEXTRA`, `FNAME`,
//!   `FCOMMENT` are skipped, `FHCRC` is **verified** against the header's own CRC-32, and
//!   the reserved bits are rejected), inflates the member, verifies the CRC-32 and the
//!   `ISIZE` trailer, and repeats for every **concatenated** member — exactly what the
//!   oracle's `EOS_BEHAVIOR_RESET` (`zlib.erl:776`) does. Any input left over that is not
//!   another complete member is `data_error`, as in the oracle (the oracle's
//!   `zlib:gunzip(<<Member/binary, 99, 99>>)` is `data_error`, while
//!   `zlib:gunzip(<<A/binary, B/binary>>)` is the two payloads concatenated).
//!
//! The whole failure matrix is the bare `error:data_error` (a malformed header, a bad magic
//! byte, a bad `CM`, a reserved flag bit, a truncated or corrupt member, a bad CRC-32, a bad
//! `ISIZE`, trailing junk, and the **empty** input, which completes no member at all) —
//! `badarg` is reserved for a non-iodata argument. Both are pinned live against OTP 29.1.1
//! / erts-17.1.
//!
//! # Byte identity
//!
//! `zlib:compress/1` is a documented wrapper around `deflateInit(Z, default)` +
//! `deflate(Z, Data, finish)`, so its output is an *observable*: it has to be the oracle's
//! bytes, not merely a conforming stream. [`beam_deflate`] is a port of
//! `../../erts/emulator/zlib/{deflate,trees}.c` — the same C the oracle links — so the lazy
//! matching (`deflate_slow`, `good`/`lazy`/`nice`/`chain` = 8/16/128/128), the
//! block-splitting threshold, and the stored/fixed/dynamic choice with its exact tie-breaks
//! (`static_lenb <= opt_lenb`, `stored_len + 4 <= opt_lenb`, and the fixed tree winning the
//! `static_lenb == opt_lenb` tie) are the oracle's own decisions rather than an
//! approximation of them. `zip/1` and `gzip/1` ask it for their own framing, and the raw
//! body is the same bytes zlib's `windowBits = 15` stream carries between its header and
//! its trailer (`one_shot::tests::the_three_framings_agree_byte_for_byte` is that pin).
//!
//! The evidence is `beam_deflate`'s own corpus sweep — 742 deterministic inputs — plus this
//! crate's bounded, fail-loud live-oracle gate (`tests/zlib_oracle.rs`), which checks all
//! three encoders and both round-trip directions.
//!
//! # The stream API (`zlib.erl`'s 19 NIF-level functions)
//!
//! The rest of the module — `open/0`, `deflateInit/1,2,6`, `deflate/2,3`, `inflate/2,3`,
//! `safeInflate/2`, `deflateParams/3`, `deflateSetDictionary/2`, `inflateGetDictionary/1`,
//! `deflateReset/1`, `deflateEnd/1`, `inflateInit/1,2`, `inflateSetDictionary/2`,
//! `inflateReset/1`, `inflateEnd/1`, `save_progress/2`, `restore_progress/2`,
//! `enqueue_input/2` and `close/1` — is written over 19 **non-exported local** functions of
//! `zlib.beam` (`zlib.erl:100-106`: `open_nif/0` … `enqueue_nif/2`), i.e. a NIF surface a
//! runtime has to serve rather than Erlang that can run on its own. That surface takes a
//! per-stream resource with a controlling process, a queued-input pump and a stash, and it
//! drives [`beam_deflate`]'s streaming entry points directly; its design is
//! `../../ZLIB_STREAM_BRIEF.md`.
//!
//! Only the six convenience functions above are registered today, so a stream call still
//! reaches its `erlang:nif_error(undef)` stub and is `undef`.

mod one_shot;
mod stream;

use crate::registry::BifRegistry;

/// Registers the six self-contained `zlib` functions.
///
/// Every key is a `zlib.erl` function, not a `bif.tab` NIF, so the six are stdlib fast
/// paths — registered here the way `erlang:memory/0,1` is. The stream surface has no
/// registered key: `zlib.erl` reaches it through its own non-exported NIF stubs.
/// Whether `Module:Function/Arity` is one of the `zlib` BIF-surface keys this crate
/// serves — the six convenience functions plus the 19 stream keys.
///
/// The local-call predicate a host consults before entering compiled code.
pub fn serves_local_call(module: &str, function: &str, arity: usize) -> bool {
    module == "zlib" && stream::is_nif_key(function, arity)
}

pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("zlib", "compress", 1, one_shot::compress_1);
    reg.register("zlib", "uncompress", 1, one_shot::uncompress_1);
    reg.register("zlib", "zip", 1, one_shot::zip_1);
    reg.register("zlib", "unzip", 1, one_shot::unzip_1);
    reg.register("zlib", "gzip", 1, one_shot::gzip_1);
    reg.register("zlib", "gunzip", 1, one_shot::gunzip_1);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The six fast paths are exactly the registered keys — no stream-API key is
    /// registered (that surface is `zlib.erl`'s own NIF stubs, not a registry entry).
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
        // The stream API has no registry key (its NIF stubs).
        assert!(!reg.contains("zlib", "open", 0));
        assert_eq!(reg.len(), 6);
    }
}
