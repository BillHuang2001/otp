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

//! Integration tests for the `erlang:term_to_binary/2` option surface
//! (`compressed` / `{compressed, Level}` / `{minor_version, N}`) and
//! `erlang:binary_to_term/2`'s `used` / `safe` options, driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call) — the crate's public seam.
//!
//! Every expectation is pinned to the OTP 29 / erts-17.1 oracle (`erl` on
//! `PATH`); the oracle command that produced each one is cited in a comment next
//! to the assertion. Terms are built and read with the sibling **safe**
//! `beam_codec::term` helpers (`make_binary`/`make_flatmap`/`bitstring_bytes`/…),
//! because the crate's own bitstring helpers are private.
//!
//! Coverage maps 1:1 onto the behaviour `src/binconv.rs` now implements:
//!
//! * the C revert rule (`external.c:2250-2301`) — a body too small to pay for
//!   zlib's 5-byte framing stays a *plain* image, a large repetitive one becomes
//!   `[131][80][len:4 BE][zlib]`;
//! * `parse_t2b_opts`'s rejections (a non-list, an unknown option, an
//!   out-of-range value);
//! * `{minor_version, N}`: `2` is the default tag set (`NEW_FLOAT_EXT` +
//!   UTF-8 atoms), `0`/`1` clear `DFLAG_UTF8_ATOMS` (Latin-1 `ATOM_EXT`, `100`)
//!   and `0` also clears `DFLAG_NEW_FLOATS` (legacy 31-byte `FLOAT_EXT`, `99`)
//!   — with a UTF-8-tag fallback for a name that is not Latin-1-representable
//!   (`external.c:2906-2932`), and combined with `compressed`;
//! * `binary_to_term/2`'s `used` count (which includes the `131` version byte, and
//!   for a top-level `COMPRESSED` image stops at the zlib stream's end, so bytes after
//!   the image are uncounted) and `safe` (which refuses an image that would create an
//!   atom);
//! * the two remaining `term_to_binary/2` flags: `deterministic` (a map's pairs are
//!   re-sorted into full Erlang term order, byte-exactly as the oracle does — see
//!   `tests/term_to_binary_options.rs` for the live cross-check) and `local` (always a
//!   typed `unsupported_bif` — its `LOCAL_EXT` payload is the node-local hash), plus
//!   the C's `badarg` for `iovec`/`fragment`, their tuples, and the mutually exclusive
//!   `local` + `deterministic` pair.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_bifs::{default_registry, AtomInterner, BifContext};
use beam_codec::compress::compress as zlib_compress;
use beam_codec::term as codec_term;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{compare::eq, signed_value, Atom, AtomTable, Eterm, NIL};

/// `external.c`'s `VERSION_MAGIC` — the first byte of every image.
const VERSION_MAGIC: u8 = 131;
/// `external.c`'s `COMPRESSED` tag (`80`) — the second byte of a zlib image.
const COMPRESSED_EXT: u8 = 80;

/// `binary_to_term(<<131,119,8,"tjenixen">>)` — a `SMALL_ATOM_UTF8_EXT` image
/// of a name no seeded table knows, i.e. one `safe` must refuse.
const FRESH_ATOM_BLOB: &[u8] = &[131, 119, 8, 116, 106, 101, 110, 105, 120, 101, 110];

/// `term_to_binary(fun lists:reverse/1)` — the `EXPORT_EXT` (`113`) image the
/// oracle round-trips byte-exactly
/// (`erl -noshell -eval 'F = binary_to_term(<<131,113,119,5,108,105,115,116,115,
/// 119,7,114,101,118,101,114,115,101,97,1>>), io:format("~p~n",[term_to_binary(F)
/// =:= <<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>]),halt().'
/// ⇒ `true`, and `F` prints as `fun lists:reverse/1`).
const EXPORT_EXT_BLOB: &[u8] = &[
    131, 113, 119, 5, 108, 105, 115, 116, 115, 119, 7, 114, 101, 118, 101, 114, 115, 101, 97, 1,
];

/// The names every test table is seeded with. `compressed`, `minor_version`,
/// `used`, `safe` and `bogus` are the option atoms the BIFs recognise (or
/// reject); `local`/`deterministic` select a flag and `iovec`/`fragment` are the
/// oracle's `badarg` for a `term_to_binary/2` caller; `lists`/`reverse` are
/// **deliberately absent** so the `EXPORT_EXT` test actually exercises the
/// [`AtomInterner`] seam.
///
/// `a` is seeded before `b`, so `a`'s atom index is the lower one — the
/// `deterministic` tests rely on that making a `[(a, _), (b, _)]` flat map's
/// storage order coincide with the atom-name order the option demands.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "a",
        "b",
        "hello",
        "compressed",
        "minor_version",
        "used",
        "safe",
        "bogus",
        "deterministic",
        "local",
        "iovec",
        "fragment",
    ]);
    atoms
}

/// The bytes of a byte-aligned binary term.
///
/// Re-packs the `size_bits` bits starting at the term's bit offset into a fresh
/// MSB-first buffer, so the result is canonical whatever representation the term
/// uses (a freshly built binary has offset `0`, but the reader must not assume it).
fn bytes_of(term: Eterm) -> Vec<u8> {
    let (buf, offset, size_bits) =
        codec_term::bitstring_bytes(term).expect("the result is a bitstring");
    assert_eq!(size_bits % 8, 0, "the result is a byte-aligned binary");
    let mut out = vec![0u8; size_bits / 8];
    for i in 0..size_bits {
        let bit = offset + i;
        if (buf[bit / 8] >> (7 - (bit % 8))) & 1 == 1 {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    out
}

/// The option list `[Name]` — a bare-atom option (e.g. `compressed`).
fn option_atom(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.list(&[ctx.atom(name).expect("seeded")])
        .expect("fits")
}

/// The option list `[{Name, Value}]` — a single `{Name, Value}` option whose
/// value is the small integer `value`.
fn option_tuple(ctx: &mut BifContext<'_>, name: &str, value: i64) -> Eterm {
    let value = ctx.int(value).expect("a small int fits");
    let pair = ctx
        .tuple(&[ctx.atom(name).expect("seeded"), value])
        .expect("fits");
    ctx.list(&[pair]).expect("fits")
}

/// Asserts `result` is `badarg`.
fn assert_badarg(result: Result<Eterm>) {
    assert!(
        matches!(result, Err(BeamError::BadArg)),
        "expected badarg, got {result:?}"
    );
}

/// Whether `err` is the typed `unsupported_bif` capability gap (as opposed to a
/// `badarg` rejection or the codec's `unsupported_external_term`).
fn is_unsupported_bif(err: &BeamError) -> bool {
    match err {
        BeamError::Raise(raise) => raise.reason.as_atom() == Some("unsupported_bif"),
        _ => false,
    }
}

#[test]
fn term_to_binary_small_compressed_reverts_to_plain() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let list = ctx
        .list(&[
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(3).expect("fits"),
        ])
        .expect("fits");

    // erl -noshell -eval 'io:format("~p~n",[term_to_binary([1,2,3],[compressed])]),halt().'
    //   % <<131,107,0,3,1,2,3>>
    // The body cannot pay for zlib's 5-byte framing, so the image stays plain.
    let expected = vec![VERSION_MAGIC, 107, 0, 3, 1, 2, 3];
    let compressed = option_atom(&mut ctx, "compressed");
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, compressed])
                .expect("term_to_binary([1,2,3],[compressed])")
        ),
        expected
    );

    // Every explicit level reverts too —
    // erl -noshell -eval 'io:format("~p~n",[[term_to_binary([1,2,3],[{compressed,N}])
    //   =:= term_to_binary([1,2,3]) || N <- lists:seq(0,9)]]),halt().'
    //   % [true,true,true,true,true,true,true,true,true,true]
    for level in 0..=9i64 {
        let options = option_tuple(&mut ctx, "compressed", level);
        assert_eq!(
            bytes_of(
                reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, options])
                    .expect("a compressed image")
            ),
            expected,
            "{{compressed, {level}}} must revert for the small body"
        );
    }
}

#[test]
fn term_to_binary_large_compressed_emits_compressed_ext() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // A large, highly repetitive term — the oracle's `lists:duplicate(500, hello)`.
    let hello = ctx.atom("hello").expect("seeded");
    let mut big = NIL;
    for _ in 0..500 {
        big = ctx.cons(hello, big).expect("fits");
    }

    // erl -noshell -eval 'io:format("~p~n",[byte_size(term_to_binary(
    //   lists:duplicate(500,hello)))]),halt().'  % 3507
    let plain = bytes_of(
        reg.call("erlang", "term_to_binary", 1, &mut ctx, &[big])
            .expect("the plain image"),
    );
    assert_eq!(plain.len(), 3507, "the oracle's plain image for 500 `hello`s");

    // erl ... 'C = term_to_binary(Big,[compressed]),
    //          io:format("~p~n",[{binary:at(C,0),binary:at(C,1),binary:part(C,2,4)}]),halt().'
    //   % {131,80,<<0,0,13,178>>}   (0x0DB2 = 3506 = byte_size(Plain) - 1)
    let compressed = option_atom(&mut ctx, "compressed");
    let image = bytes_of(
        reg.call("erlang", "term_to_binary", 2, &mut ctx, &[big, compressed])
            .expect("the compressed image"),
    );
    assert_eq!(image[0], VERSION_MAGIC, "VERSION_MAGIC");
    assert_eq!(image[1], COMPRESSED_EXT, "COMPRESSED_EXT");
    let size = u32::from_be_bytes([image[2], image[3], image[4], image[5]]) as usize;
    assert_eq!(
        size,
        plain.len() - 1,
        "the 4-byte size field is the body length (without the version byte)"
    );
    assert!(
        image.len() < plain.len(),
        "compression helped: {} vs {}",
        image.len(),
        plain.len()
    );

    // `binary_to_term/1` unwraps a top-level COMPRESSED image back to the equal
    // term — erl ... 'binary_to_term(term_to_binary(Big,[compressed])) =:= Big' % true
    let blob = codec_term::make_binary(ctx.heap(), &image).expect("fits");
    let back = reg
        .call("erlang", "binary_to_term", 1, &mut ctx, &[blob])
        .expect("binary_to_term of a COMPRESSED image");
    assert!(
        eq(big, back, &atoms),
        "binary_to_term(term_to_binary(Big,[compressed])) must equal Big"
    );

    // Every accepted level wraps the large body (no level is refused, and none
    // reports a capability gap) — erl ... 'T = lists:duplicate(500,hello),
    //   [io:format("L=~p head=~p~n",[L,binary:part(term_to_binary(T,[{compressed,L}]),0,2)])
    //   || L <- lists:seq(1,9)], halt().'  % every L ⇒ head = {131,80}
    for level in 1..=9i64 {
        let options = option_tuple(&mut ctx, "compressed", level);
        let image = bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[big, options])
                .expect("a {compressed, Level} image"),
        );
        assert_eq!(
            &image[..2],
            &[VERSION_MAGIC, COMPRESSED_EXT],
            "{{compressed, {level}}} must wrap the large body"
        );
        assert_eq!(
            u32::from_be_bytes([image[2], image[3], image[4], image[5]]) as usize,
            plain.len() - 1,
            "the size field is the body length (level {level})"
        );
        // The image round-trips to the very same term.
        let blob = codec_term::make_binary(ctx.heap(), &image).expect("fits");
        let back = reg
            .call("erlang", "binary_to_term", 1, &mut ctx, &[blob])
            .expect("binary_to_term of the COMPRESSED image");
        assert!(eq(big, back, &atoms), "round-trip at level {level}");
    }

    // `{compressed, 0}` stores without compressing, so the wrapper does not pay
    // off (`deflated.len() > body.len() - 5`): the image reverts to plain —
    // erl ... 'term_to_binary(Big,[{compressed,0}]) =:= term_to_binary(Big)' % true
    let level0 = option_tuple(&mut ctx, "compressed", 0);
    let image0 = bytes_of(
        reg.call("erlang", "term_to_binary", 2, &mut ctx, &[big, level0])
            .expect("a {compressed, 0} image"),
    );
    assert_eq!(image0, plain);
    assert_ne!(image0[1], COMPRESSED_EXT, "the image must have reverted");
}

#[test]
fn term_to_binary_rejects_malformed_options() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let term = ctx
        .list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
        .expect("fits");
    let compressed = ctx.atom("compressed").expect("seeded");

    // A bare `{compressed, 99}` tuple (not a list) — `badarg`
    // (`term_to_binary([1,2], {compressed,99})` ⇒ `badarg`).
    let tuple = ctx
        .tuple(&[compressed, ctx.int(99).expect("fits")])
        .expect("fits");
    assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, tuple]));

    // A bare `compressed` atom (not a list) — `badarg`
    // (`term_to_binary([1,2], compressed)` ⇒ `badarg`).
    assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, compressed]));

    // `[{compressed, 10}]` (level > 9) and `[{compressed, -1}]` — `badarg`
    // (`term_to_binary([1,2], [{compressed,10}])` / `[{compressed,-1}]` ⇒ `badarg`).
    for level in [10i64, -1] {
        let options = option_tuple(&mut ctx, "compressed", level);
        assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, options]));
    }

    // `[{minor_version, -1}]` and `[{minor_version, 3}]` — `badarg`.
    for version in [-1i64, 3] {
        let options = option_tuple(&mut ctx, "minor_version", version);
        assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, options]));
    }

    // An unknown option atom — `badarg` (`term_to_binary([1,2], [bogus])` ⇒ `badarg`).
    let unknown = option_atom(&mut ctx, "bogus");
    assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, unknown]));

    // An improper option list `[{compressed, 6} | 1]` — `badarg`
    // (`term_to_binary([1,2], [{compressed,6}|x])` ⇒ `badarg`).
    let pair = ctx
        .tuple(&[compressed, ctx.int(6).expect("fits")])
        .expect("fits");
    let improper = ctx
        .cons(pair, ctx.int(1).expect("fits"))
        .expect("fits");
    assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[term, improper]));
}

#[test]
fn term_to_binary_minor_version_2_is_the_default_image() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let one = ctx.int(1).expect("fits");
    let map = codec_term::make_flatmap(ctx.heap(), &[(a, one)]).expect("fits");

    // erl -noshell -eval 'io:format("~p~n",[term_to_binary(#{a=>1},[{minor_version,2}])]),halt().'
    //   % <<131,116,0,0,0,1,119,1,97,97,1>>
    let expected = vec![VERSION_MAGIC, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];
    let mv2 = option_tuple(&mut ctx, "minor_version", 2);
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[map, mv2])
                .expect("term_to_binary(#{a=>1},[{minor_version,2}])")
        ),
        expected
    );

    // ...and `{minor_version, 2}` is byte-identical to the default `/1` tag set —
    // erl ... 'term_to_binary(#{a=>1})' % <<131,116,0,0,0,1,119,1,97,97,1>>
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 1, &mut ctx, &[map])
                .expect("term_to_binary(#{a=>1})")
        ),
        expected
    );
}

#[test]
fn term_to_binary_minor_version_0_and_1_clear_the_utf8_atom_flag() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // An atom-free, float-free term is byte-identical for every minor version —
    // erl ... 'term_to_binary([1,2,3],[{minor_version,N}])'  % <<131,107,0,3,1,2,3>>
    let list = ctx
        .list(&[
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(3).expect("fits"),
        ])
        .expect("fits");
    let expected = vec![VERSION_MAGIC, 107, 0, 3, 1, 2, 3];
    for version in [0i64, 1] {
        let options = option_tuple(&mut ctx, "minor_version", version);
        assert_eq!(
            bytes_of(
                reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, options])
                    .expect("an atom-free term encodes for mv 0/1")
            ),
            expected,
            "minor_version {version} on [1,2,3]"
        );
    }

    // Minor versions 0/1 clear `DFLAG_UTF8_ATOMS`, so the oracle writes the Latin-1
    // `ATOM_EXT` (100) — erl ... 'term_to_binary(hello,[{minor_version,0}])'
    //   % <<131,100,0,5,"hello">>
    //   'term_to_binary([hello],[{minor_version,1}])'
    //   % <<131,108,0,0,0,1,100,0,5,"hello",106>>
    let hello = ctx.atom("hello").expect("seeded");
    let latin1_hello = vec![VERSION_MAGIC, 100, 0, 5, 104, 101, 108, 108, 111];
    for version in [0i64, 1] {
        let options = option_tuple(&mut ctx, "minor_version", version);
        assert_eq!(
            bytes_of(
                reg.call("erlang", "term_to_binary", 2, &mut ctx, &[hello, options])
                    .expect("an atom-bearing term encodes for mv 0/1")
            ),
            latin1_hello,
            "minor_version {version} on the atom `hello`"
        );
    }

    // ...including an atom nested inside a container — the oracle's
    // `term_to_binary([hello],[{minor_version,1}])`.
    let nested = ctx.list(&[hello]).expect("fits");
    let latin1_nested = vec![
        VERSION_MAGIC, 108, 0, 0, 0, 1, 100, 0, 5, 104, 101, 108, 108, 111, 106,
    ];
    for version in [0i64, 1] {
        let options = option_tuple(&mut ctx, "minor_version", version);
        assert_eq!(
            bytes_of(
                reg.call("erlang", "term_to_binary", 2, &mut ctx, &[nested, options])
                    .expect("a nested atom encodes for mv 0/1")
            ),
            latin1_nested,
            "minor_version {version} on `[hello]`"
        );
    }
}

/// A **wide** (non-Latin-1) atom has no `ATOM_EXT` image, so the C `enc_atom` falls
/// back to the UTF-8 tag family under `{minor_version, 0|1}` too
/// (`external.c:2906-2932`) — an atom-bearing term is never refused.
#[test]
fn term_to_binary_minor_version_0_falls_back_to_utf8_for_a_wide_atom() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let mut atoms = seeded_atoms();
    atoms.seed(["\u{03bb}"]);
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // erl -noshell -eval 'io:format("~w~n",[term_to_binary(list_to_atom([955]),
    //   [{minor_version,0}])]),halt().'  % <<131,119,2,206,187>>
    let wide = ctx.atom("\u{03bb}").expect("seeded");
    for version in [0i64, 1, 2] {
        let options = option_tuple(&mut ctx, "minor_version", version);
        assert_eq!(
            bytes_of(
                reg.call("erlang", "term_to_binary", 2, &mut ctx, &[wide, options])
                    .expect("a wide atom encodes for every minor version")
            ),
            vec![VERSION_MAGIC, 119, 2, 206, 187],
            "minor_version {version} on the atom `λ`"
        );
    }
}

/// The `{minor_version, 0}` flavour clears `DFLAG_NEW_FLOATS` as well, so a float goes
/// on the wire as the legacy 31-byte `FLOAT_EXT` (99) — the OTP-17-era form.
#[test]
fn term_to_binary_minor_version_0_writes_the_legacy_float_ext() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let float = ctx.make_float(1.5).expect("fits");
    // erl -noshell -eval 'io:format("~w~n",[term_to_binary(1.5,[{minor_version,0}])]),halt().'
    //   % <<131,99,"1.50000000000000000000e+00",0,0,0,0,0>>
    let mut expected = vec![VERSION_MAGIC, 99];
    expected.extend_from_slice(b"1.50000000000000000000e+00");
    expected.extend_from_slice(&[0u8; 5]);
    let mv0 = option_tuple(&mut ctx, "minor_version", 0);
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[float, mv0])
                .expect("term_to_binary(1.5,[{minor_version,0}])")
        ),
        expected
    );

    // A nested float takes the legacy form too —
    // erl ... 'term_to_binary([1.5],[{minor_version,0}])'
    //   % <<131,108,0,0,0,1,99,"1.50000000000000000000e+00",0,0,0,0,0,106>>
    let list = ctx.list(&[float]).expect("fits");
    let mut expected = vec![VERSION_MAGIC, 108, 0, 0, 0, 1, 99];
    expected.extend_from_slice(b"1.50000000000000000000e+00");
    expected.extend_from_slice(&[0u8; 5]);
    expected.push(106);
    let mv0 = option_tuple(&mut ctx, "minor_version", 0);
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, mv0])
                .expect("term_to_binary([1.5],[{minor_version,0}])")
        ),
        expected
    );

    // The same float under minor version 1 keeps `NEW_FLOAT_EXT` —
    // erl ... 'term_to_binary(1.5,[{minor_version,1}])'  % <<131,70,63,248,0,0,0,0,0,0>>
    let mv1 = option_tuple(&mut ctx, "minor_version", 1);
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[float, mv1])
                .expect("term_to_binary(1.5,[{minor_version,1}])")
        ),
        vec![VERSION_MAGIC, 70, 63, 248, 0, 0, 0, 0, 0, 0]
    );

    // `{minor_version, 2}` (the default) is the modern form too —
    // erl ... 'term_to_binary(1.5,[{minor_version,2}]) =:= term_to_binary(1.5)' % true
    let mv2 = option_tuple(&mut ctx, "minor_version", 2);
    let default_image = bytes_of(
        reg.call("erlang", "term_to_binary", 1, &mut ctx, &[float])
            .expect("term_to_binary(1.5)"),
    );
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[float, mv2])
                .expect("term_to_binary(1.5,[{minor_version,2}])")
        ),
        default_image
    );
}

/// A `{minor_version, N}` flavour combines with `compressed`: the *selected* flavour's
/// body is what zlib deflates, so the 4-byte size field is that body's length.
#[test]
fn term_to_binary_minor_version_combines_with_compressed() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // A large, highly repetitive atom-bearing term — `lists:duplicate(500, hello)`.
    let hello = ctx.atom("hello").expect("seeded");
    let mut big = NIL;
    for _ in 0..500 {
        big = ctx.cons(hello, big).expect("fits");
    }

    // The plain body is 1 byte longer per atom under the Latin-1 flavour:
    // erl -noshell -eval 'T = lists:duplicate(500,hello),
    //   [io:format("mv~p plain=~p size_field=~p~n",[V, byte_size(term_to_binary(T,[{minor_version,V}])),
    //   binary:decode_unsigned(binary:part(term_to_binary(T,[{minor_version,V},compressed]),2,4))])
    //   || V <- [0,1,2]], halt().'
    //   % mv0 plain=4007 size_field=4006 / mv1 plain=4007 size_field=4006
    //   % mv2 plain=3507 size_field=3506
    let flattened = |ctx: &mut BifContext<'_>, version: i64| -> Vec<u8> {
        let options = option_tuple(ctx, "minor_version", version);
        bytes_of(
            reg.call("erlang", "term_to_binary", 2, ctx, &[big, options])
                .expect("the plain flavour image"),
        )
    };
    assert_eq!(flattened(&mut ctx, 0).len(), 4007, "mv0 plain image");
    assert_eq!(flattened(&mut ctx, 1).len(), 4007, "mv1 plain image");
    assert_eq!(flattened(&mut ctx, 2).len(), 3507, "mv2 plain image");

    for version in [0i64, 1] {
        // `[{minor_version, N}, compressed]` — both options in one flat list.
        let pair = ctx
            .tuple(&[
                ctx.atom("minor_version").expect("seeded"),
                ctx.int(version).expect("a small int fits"),
            ])
            .expect("fits");
        let options = ctx
            .list(&[pair, ctx.atom("compressed").expect("seeded")])
            .expect("fits");
        let image = bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[big, options])
                .expect("minor version + compressed"),
        );
        assert_eq!(image[0], VERSION_MAGIC, "VERSION_MAGIC (mv {version})");
        assert_eq!(image[1], COMPRESSED_EXT, "COMPRESSED_EXT (mv {version})");
        assert_eq!(
            u32::from_be_bytes([image[2], image[3], image[4], image[5]]) as usize,
            4006,
            "the size field is the Latin-1 body length (mv {version})"
        );

        // The Latin-1 flavour is what was deflated: the honest premise is the plain
        // image's `ATOM_EXT` (100) — `[131][108][count:4][100][len]["hello"]…`.
        let plain = flattened(&mut ctx, version);
        assert_eq!(plain[1], 108, "LIST_EXT");
        assert_eq!(plain[6], 100, "ATOM_EXT (100) under minor version {version}");

        // The image round-trips to the very same term.
        let blob = codec_term::make_binary(ctx.heap(), &image).expect("fits");
        let back = reg
            .call("erlang", "binary_to_term", 1, &mut ctx, &[blob])
            .expect("binary_to_term of a minor-version COMPRESSED image");
        assert!(eq(big, back, &atoms), "the image round-trips (mv {version})");
    }
}/// An [`AtomInterner`](beam_bifs::AtomInterner) over a **clone** of the table the
/// context borrows.
///
/// This reproduces the index coupling `binary_to_term` relies on: the clone has
/// the context's indices for every already-interned name, and a name the hook
/// creates gets exactly the index the context's own table would have assigned —
/// which is the index the decoder wrote into the decoded term.
struct Interner {
    table: Rc<RefCell<AtomTable>>,
}

impl AtomInterner for Interner {
    fn intern(&mut self, name: &str) -> Atom {
        self.table.borrow_mut().intern(name)
    }
}

#[test]
fn export_ext_fun_round_trips_through_decode_and_encode() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();

    // The blob carries the fresh names `lists`/`reverse`, so decoding must intern
    // them through the hook (over a clone of the context table).
    let shared = Rc::new(RefCell::new(atoms.clone()));
    let fun = {
        let mut interner = Interner {
            table: Rc::clone(&shared),
        };
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let blob = codec_term::make_binary(ctx.heap(), EXPORT_EXT_BLOB).expect("the blob fits");
        reg.call("erlang", "binary_to_term", 1, &mut ctx, &[blob])
            .expect("an EXPORT_EXT image decodes")
    };

    // The decoded term is the external fun `fun lists:reverse/1`
    // (erl ... 'binary_to_term(<<131,113,...>>)' ⇒ `fun lists:reverse/1`).
    assert_eq!(codec_term::fun_arity_term(fun).expect("a fun"), 1);
    let table = shared.borrow();
    assert_eq!(
        table.name(codec_term::fun_module(fun).expect("an external fun")),
        Some("lists")
    );
    assert_eq!(
        table.name(codec_term::fun_function(fun).expect("an external fun")),
        Some("reverse")
    );

    // Re-encode over the table the interner extended (the decoded fun's atom
    // indices are only valid there): the result is the very same `EXPORT_EXT`
    // image — erl ... 'term_to_binary(binary_to_term(<<131,113,...>>))'
    //   % <<131,113,119,5,108,105,115,116,115,119,7,114,101,118,101,114,115,101,97,1>>
    let mut heap2 = Heap::new(4096).expect("test heap");
    let mut ctx2 = BifContext::new(&mut heap2, &table, ProcessId::new(1, 0));
    let image = reg
        .call("erlang", "term_to_binary", 1, &mut ctx2, &[fun])
        .expect("the fun re-encodes");
    assert_eq!(bytes_of(image), EXPORT_EXT_BLOB.to_vec());
}

#[test]
fn binary_to_term_used_option_counts_the_version_byte() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // erl -noshell -eval 'io:format("~p~n",[binary_to_term(<<131,97,17>>,[used])]),halt().'
    //   % {17,3}   (the count includes the `131` version byte)
    let blob = codec_term::make_binary(ctx.heap(), &[131, 97, 17]).expect("fits");
    let used = option_atom(&mut ctx, "used");
    let result = reg
        .call("erlang", "binary_to_term", 2, &mut ctx, &[blob, used])
        .expect("binary_to_term(<<131,97,17>>, [used])");
    assert_eq!(codec_term::tuple_arity(result).expect("a 2-tuple"), 2);
    assert_eq!(
        signed_value(codec_term::tuple_element(result, 0).expect("element 0")),
        17
    );
    assert_eq!(
        signed_value(codec_term::tuple_element(result, 1).expect("element 1")),
        3
    );

    // Trailing bytes are ignored by `/1`, and the `used` count still stops at the
    // term's end — erl ... 'binary_to_term(<<131,97,17,99>>,[used])'  % {17,3}
    let trailing = codec_term::make_binary(ctx.heap(), &[131, 97, 17, 99]).expect("fits");
    let used = option_atom(&mut ctx, "used");
    let result = reg
        .call("erlang", "binary_to_term", 2, &mut ctx, &[trailing, used])
        .expect("binary_to_term(<<131,97,17,99>>, [used])");
    assert_eq!(
        signed_value(codec_term::tuple_element(result, 0).expect("element 0")),
        17
    );
    assert_eq!(
        signed_value(codec_term::tuple_element(result, 1).expect("element 1")),
        3
    );
}

#[test]
fn binary_to_term_safe_option_refuses_a_fresh_atom() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // erl -noshell -eval 'io:format("~p~n",[catch binary_to_term(
    //   <<131,119,8,"tjenixen">>,[safe])]),halt().'  % badarg
    let fresh = codec_term::make_binary(ctx.heap(), FRESH_ATOM_BLOB).expect("fits");
    let safe = option_atom(&mut ctx, "safe");
    assert_badarg(reg.call("erlang", "binary_to_term", 2, &mut ctx, &[fresh, safe]));

    // The premise: without `safe`, the very same image *does* decode (its atom is
    // created through the hook elsewhere) — here, with the plain context and no
    // interner, it reports the documented `TermNotFound` gap rather than `badarg`.
    let fresh = codec_term::make_binary(ctx.heap(), FRESH_ATOM_BLOB).expect("fits");
    assert!(matches!(
        reg.call("erlang", "binary_to_term", 1, &mut ctx, &[fresh]),
        Err(BeamError::TermNotFound)
    ));
}

// ---------------------------------------------------------------------------
// The `local` / `deterministic` options and the C's `badarg` rejections
// ---------------------------------------------------------------------------

/// `term_to_binary/2` accepts exactly `parse_t2b_opts`'s option set: `compressed`,
/// `{compressed, Level}`, `{minor_version, N}`, the bare `deterministic`/`local`
/// atoms — and nothing else. `iovec`/`fragment` (and their tuples) are the C's
/// `badarg` for this caller, and `local` + `deterministic` are mutually exclusive.
#[test]
fn term_to_binary_accepts_the_oracles_option_set() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let list = ctx
        .list(&[
            ctx.int(1).expect("fits"),
            ctx.int(2).expect("fits"),
            ctx.int(3).expect("fits"),
        ])
        .expect("fits");

    // erl -noshell -eval 'io:format("~p~n",[term_to_binary([1,2,3],[deterministic])]),halt().'
    //   % <<131,107,0,3,1,2,3>>   (a map-free term has nothing to re-sort, so the
    //   option is accepted *and* byte-identical to the default image)
    let deterministic = option_atom(&mut ctx, "deterministic");
    assert_eq!(
        bytes_of(
            reg.call(
                "erlang",
                "term_to_binary",
                2,
                &mut ctx,
                &[list, deterministic]
            )
            .expect("term_to_binary([1,2,3],[deterministic])")
        ),
        vec![VERSION_MAGIC, 107, 0, 3, 1, 2, 3]
    );

    // erl ... 'term_to_binary([1,2,3],[deterministic,deterministic])'  % <<131,107,0,3,1,2,3>>
    // (repeating one option is fine; only the `local`+`deterministic` *pair* is refused)
    let repeated = ctx
        .list(&[
            ctx.atom("deterministic").expect("seeded"),
            ctx.atom("deterministic").expect("seeded"),
        ])
        .expect("fits");
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, repeated])
                .expect("term_to_binary([1,2,3],[deterministic,deterministic])")
        ),
        vec![VERSION_MAGIC, 107, 0, 3, 1, 2, 3]
    );

    // erl -noshell -eval 'try [term_to_binary([1,2,3],[O]) || O <- [iovec, fragment]]
    //   catch error:badarg -> io:format("badarg") end, halt().'  % badarg
    // `iovec` and `fragment` are accepted by `parse_t2b_opts` only for a caller that
    // can receive an iovec / passed a fragment size (`external.c:1565-1617`);
    // `term_to_binary/2` passes `NULL` for both, so it answers `badarg` — as it does
    // for their tuples (`[{iovec,1}]`/`[{fragment,10}]` ⇒ `badarg`).
    for name in ["iovec", "fragment"] {
        let option = option_atom(&mut ctx, name);
        assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, option]));
        let tuple = option_tuple(&mut ctx, name, 1);
        assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, tuple]));
    }

    // erl -noshell -eval 'try term_to_binary([1,2,3],[local,deterministic])
    //   catch error:badarg -> io:format("badarg") end, halt().'  % badarg
    // `parse_t2b_opts` rejects the pair after scanning the list
    // (`external.c:1543-1545`), so the order does not matter.
    for order in [["local", "deterministic"], ["deterministic", "local"]] {
        let both = ctx
            .list(&[
                ctx.atom(order[0]).expect("seeded"),
                ctx.atom(order[1]).expect("seeded"),
            ])
            .expect("fits");
        assert_badarg(reg.call("erlang", "term_to_binary", 2, &mut ctx, &[list, both]));
    }
}

/// `[local]` wraps the image in `[131][LOCAL_EXT][4-byte node hash]`; the hash is
/// `local_node_hash(...)` — node/runtime state (`beam_rt`/`beam_dist`), re-derived on
/// every VM start — so the BIF reports the typed `unsupported_bif` gap rather than a
/// fabricated hash (`beam_codec::Encoder::finish_local` can write the wrapper; the
/// missing piece is the hash value).
#[test]
fn term_to_binary_local_is_a_typed_gap() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // erl -noshell -eval 'io:format("~p~n",[term_to_binary([1,2],[local])]),halt().'
    //   % <<131,121,Hash:4/binary,107,0,2,1,2>>   (LOCAL_EXT is tag 121; `Hash` is
    //   different on every run, which is exactly why it cannot be fabricated here)
    let list = ctx
        .list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
        .expect("fits");
    let local = option_atom(&mut ctx, "local");
    let err = reg
        .call("erlang", "term_to_binary", 2, &mut ctx, &[list, local])
        .expect_err("local has no seam for the node hash");
    assert!(
        is_unsupported_bif(&err),
        "expected the typed unsupported_bif gap, got {err:?}"
    );

    // `[local,compressed]` is a valid oracle option list, but the gap is reached
    // before compression is considered, so it reports the same typed gap.
    let both = ctx
        .list(&[
            ctx.atom("local").expect("seeded"),
            ctx.atom("compressed").expect("seeded"),
        ])
        .expect("fits");
    let err = reg
        .call("erlang", "term_to_binary", 2, &mut ctx, &[list, both])
        .expect_err("local + compressed still hits the hash gap");
    assert!(is_unsupported_bif(&err), "local + compressed, got {err:?}");
}

/// The oracle's `[deterministic]` image of `#{a=>1,b=>2}` — pairs sorted by atom
/// **name**. (The plain image of the same map is ordered by atom index instead, which
/// for this table is the reverse: `<<…,119,1,98,97,2,119,1,97,97,1>>`.)
///
/// `erl -noshell -eval 'io:format("~p~n",[term_to_binary(#{a=>1,b=>2},
/// [deterministic])]),halt().'`
///   % <<131,116,0,0,0,2,119,1,97,97,1,119,1,98,97,2>>
const DETERMINISTIC_MAP_AB: &[u8] = &[
    131, 116, 0, 0, 0, 2, 119, 1, 97, 97, 1, 119, 1, 98, 97, 2,
];

/// `[deterministic]` is byte-exact for a term that has nothing to re-sort: a map-free
/// term is unchanged, and a flat map whose stored order already *is* the atom-name
/// order is emitted as-is (the re-sort of the flag is then a no-op). The complementary
/// case — a map that really does need re-sorting — is
/// [`term_to_binary_deterministic_resorts_a_flat_map_like_the_oracle`].
#[test]
fn term_to_binary_deterministic_is_byte_exact_when_no_resort_is_needed() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");

    // `seeded_atoms` interns `a` before `b`, so this pair order is both the storage
    // order and the deterministic (name) order — `make_flatmap` preserves the caller's
    // order (sorting is the map constructor's job).
    let map = codec_term::make_flatmap(ctx.heap(), &[(a, one), (b, two)]).expect("fits");
    let deterministic = option_atom(&mut ctx, "deterministic");
    assert_eq!(
        bytes_of(
            reg.call(
                "erlang",
                "term_to_binary",
                2,
                &mut ctx,
                &[map, deterministic]
            )
            .expect("term_to_binary(#{a=>1,b=>2},[deterministic])")
        ),
        DETERMINISTIC_MAP_AB.to_vec()
    );

    // The same map with no option is byte-identical (its stored order already is the
    // deterministic order) — erl ... 'term_to_binary(#{a=>1,b=>2})' would only differ
    // for a table whose `a`/`b` indices were reversed.
    let map = codec_term::make_flatmap(ctx.heap(), &[(a, one), (b, two)]).expect("fits");
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 1, &mut ctx, &[map])
                .expect("term_to_binary(#{a=>1,b=>2})")
        ),
        DETERMINISTIC_MAP_AB.to_vec()
    );

    // A map nested in a container is walked too —
    // erl ... 'term_to_binary({1,#{a=>1,b=>2}},[deterministic])'
    //   % <<131,104,2,97,1,116,0,0,0,2,119,1,97,97,1,119,1,98,97,2>>
    let map = codec_term::make_flatmap(ctx.heap(), &[(a, one), (b, two)]).expect("fits");
    let nested = ctx.tuple(&[one, map]).expect("fits");
    let deterministic = option_atom(&mut ctx, "deterministic");
    let mut expected = vec![VERSION_MAGIC, 104, 2, 97, 1];
    expected.extend_from_slice(&DETERMINISTIC_MAP_AB[1..]);
    assert_eq!(
        bytes_of(
            reg.call(
                "erlang",
                "term_to_binary",
                2,
                &mut ctx,
                &[nested, deterministic]
            )
            .expect("term_to_binary({1,#{a=>1,b=>2}},[deterministic])")
        ),
        expected
    );
}

/// A map whose storage order differs from the deterministic (`erts_cmp`, i.e. atom
/// **name**) order is re-sorted by `[deterministic]`
/// (`beam_codec::Encoder::with_deterministic`) — byte-exactly the oracle's image, never
/// the wrongly ordered plain one.
#[test]
fn term_to_binary_deterministic_resorts_a_flat_map_like_the_oracle() {
    /// `erl -noshell -eval 'io:format("~p~n",[term_to_binary(#{b=>1,a=>2},
    /// [deterministic])]),halt().'` ⇒ `<<131,116,0,0,0,2,119,1,97,97,2,119,1,98,97,1>>`
    /// — the pairs in atom-**name** order (`a` before `b`).
    const DETERMINISTIC_MAP_BA: &[u8] = &[
        131, 116, 0, 0, 0, 2, 119, 1, 97, 97, 2, 119, 1, 98, 97, 1,
    ];
    /// The store→encode order the *caller* installed, `[b, a]`: `make_flatmap` keeps
    /// the pairs in the order it is given (canonical ordering is the map constructor's
    /// job), so the plain image of the same map is `b=>1, a=>2`.
    const PLAIN_MAP_BA: &[u8] = &[
        131, 116, 0, 0, 0, 2, 119, 1, 98, 97, 1, 119, 1, 97, 97, 2,
    ];

    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");

    // Storage order `[b, a]` vs the name order `[a, b]`.
    let map = codec_term::make_flatmap(ctx.heap(), &[(b, one), (a, two)]).expect("fits");
    let deterministic = option_atom(&mut ctx, "deterministic");
    assert_eq!(
        bytes_of(
            reg.call(
                "erlang",
                "term_to_binary",
                2,
                &mut ctx,
                &[map, deterministic]
            )
            .expect("term_to_binary(#{b=>1,a=>2},[deterministic])")
        ),
        DETERMINISTIC_MAP_BA.to_vec()
    );

    // A map nested in a container is re-sorted too (the flag lives on the encoder, so
    // every map it reaches is affected) —
    // erl ... 'term_to_binary({1,#{b=>1,a=>2}},[deterministic])'
    //   % <<131,104,2,97,1,116,0,0,0,2,119,1,97,97,2,119,1,98,97,1>>
    let map = codec_term::make_flatmap(ctx.heap(), &[(b, one), (a, two)]).expect("fits");
    let nested = ctx.tuple(&[one, map]).expect("fits");
    let deterministic = option_atom(&mut ctx, "deterministic");
    let mut expected = vec![VERSION_MAGIC, 104, 2, 97, 1];
    expected.extend_from_slice(&DETERMINISTIC_MAP_BA[1..]);
    assert_eq!(
        bytes_of(
            reg.call(
                "erlang",
                "term_to_binary",
                2,
                &mut ctx,
                &[nested, deterministic]
            )
            .expect("term_to_binary({1,#{b=>1,a=>2}},[deterministic])")
        ),
        expected
    );

    // The *plain* image of the very same map keeps the stored order: the option is the
    // only difference, so this also proves the re-sort really happened.
    let map = codec_term::make_flatmap(ctx.heap(), &[(b, one), (a, two)]).expect("fits");
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 1, &mut ctx, &[map])
                .expect("term_to_binary(#{b=>1,a=>2})")
        ),
        PLAIN_MAP_BA.to_vec()
    );
}

/// `binary_to_term/2`'s `[used]` count for a top-level `COMPRESSED` (tag 80) image is
/// the image's own length: the C's count starts at the whole input and subtracts what
/// the inflater left unread once the zlib stream ended
/// (`external.c:1797-1801`/`:2011-2014`), so bytes *after* the stream are not counted.
#[test]
fn binary_to_term_used_option_counts_only_the_compressed_image() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // A hand-built image of a tiny body, so the count is exact whatever
    // `term_to_binary/2`'s revert rule would do with the body:
    // `[131][80][len:4 BE][zlib]` followed by four trailing bytes.
    let body = [107u8, 0, 3, 1, 2, 3]; // LIST_EXT of [1,2,3], no version byte
    let stream = zlib_compress(&body, 6).expect("deflate");
    let mut image = vec![VERSION_MAGIC, COMPRESSED_EXT];
    image.extend_from_slice(&(body.len() as u32).to_be_bytes());
    image.extend_from_slice(&stream);
    let image_len = image.len();
    let mut padded = image.clone();
    padded.extend_from_slice(&[9, 8, 7, 6]);

    // erl -noshell -eval 'Body = binary:part(term_to_binary([1,2,3]),1,6),
    //   Z = zlib:compress(Body), C = <<131,80,6:32,Z/binary>>,
    //   {T,N} = binary_to_term(<<C/binary,9,8,7,6>>,[used]),
    //   io:format("~p~n",[{T =:= [1,2,3], N =:= byte_size(C)}]),halt().'  % {true,true}
    //   (the count is the image's length, never the padded input's)
    let blob = codec_term::make_binary(ctx.heap(), &padded).expect("fits");
    let used = option_atom(&mut ctx, "used");
    let result = reg
        .call("erlang", "binary_to_term", 2, &mut ctx, &[blob, used])
        .expect("binary_to_term of a padded COMPRESSED image");
    assert_eq!(
        signed_value(codec_term::tuple_element(result, 1).expect("element 1")),
        image_len as i64,
        "the count must stop at the zlib stream's end, not the padded input's"
    );
    // The decoded term is `[1,2,3]`: re-encoding it yields the body's plain image, and
    // the four trailing bytes did not reach the decoder.
    let decoded = codec_term::tuple_element(result, 0).expect("element 0");
    assert_eq!(
        bytes_of(
            reg.call("erlang", "term_to_binary", 1, &mut ctx, &[decoded])
                .expect("the decoded list re-encodes")
        ),
        vec![VERSION_MAGIC, 107, 0, 3, 1, 2, 3]
    );

    // Without trailing bytes the count is the whole input (the common case, answered by
    // a single probe rather than the search).
    let blob = codec_term::make_binary(ctx.heap(), &image).expect("fits");
    let used = option_atom(&mut ctx, "used");
    let result = reg
        .call("erlang", "binary_to_term", 2, &mut ctx, &[blob, used])
        .expect("binary_to_term of the unpadded COMPRESSED image");
    assert_eq!(
        signed_value(codec_term::tuple_element(result, 1).expect("element 1")),
        image_len as i64
    );
}
