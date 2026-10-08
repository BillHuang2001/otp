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

//! Oracle-pinned integration tests for `erlang:decode_packet/3`, registered by
//! `src/packet.rs`, driven through the **public** API only (`default_registry()`
//! + `BifRegistry::call`).
//!
//! Every golden below carries the probe that pinned it (system OTP 29.1.1 /
//! erts-17.1). The result tuples are inspected with the registered
//! `erlang:tuple_size/1`, `erlang:element/2`, `erlang:byte_size/1` and
//! `binary:at/2` BIFs plus `beam_term::{atom_val, signed_value}`.
//!
//! One live cross-check shells out to the system `erl` and **skips loudly** when
//! the oracle is absent (the `tests/os_bif.rs` pattern).

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::{BeamError, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, signed_value, AtomTable, Eterm, NIL};

/// A generous heap: these BIFs build at most a 3-tuple and two small binaries.
fn heap() -> Heap {
    Heap::new(4096).expect("test heap")
}

/// The atoms `decode_packet/3` resolves by name, plus every valid packet-type
/// atom and the three option keys.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "ok",
        "error",
        "more",
        "undefined",
        "invalid",
        "raw",
        "line",
        "asn1",
        "sunrm",
        "cdr",
        "fcgi",
        "tpkt",
        "http",
        "httph",
        "http_bin",
        "httph_bin",
        "ssl_tls",
        "packet_size",
        "line_length",
        "line_delimiter",
    ]);
    atoms
}

/// Calls `module:function/arity` through the full public registry.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm> {
    reg.call(module, function, arity, ctx, args)
}

/// `erlang:decode_packet(Type, Bin, Opts)`.
fn decode(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    ty: Eterm,
    bin: Eterm,
    opts: Eterm,
) -> Result<Eterm> {
    reg.call("erlang", "decode_packet", 3, ctx, &[ty, bin, opts])
}

/// The small-integer value of `term`.
fn as_int(term: Eterm) -> i64 {
    signed_value(term)
}

/// The name of an atom term, as the shared atom table spells it.
fn atom_name(atoms: &AtomTable, term: Eterm) -> String {
    atoms.name(atom_val(term)).expect("an interned atom name").to_string()
}

/// A binary as an Erlang term, built by the registered
/// `erlang:list_to_binary/1` (there is no public binary constructor).
fn binary_term(reg: &BifRegistry, ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    let mut elems = Vec::new();
    for byte in bytes {
        elems.push(ctx.int(i64::from(*byte)).expect("fits"));
    }
    let list = ctx.list(&elems).expect("the binary fits");
    call(reg, ctx, "erlang", "list_to_binary", 1, &[list]).expect("list_to_binary")
}

/// The bytes of a binary term, read back one byte at a time through the
/// registered `binary:at/2`/`erlang:byte_size/1` BIFs.
fn binary_bytes(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> Vec<u8> {
    let size = as_int(call(reg, ctx, "erlang", "byte_size", 1, &[term]).expect("byte_size"));
    let mut out = Vec::new();
    for i in 0..size {
        let index = ctx.int(i).expect("fits");
        let byte = call(reg, ctx, "binary", "at", 2, &[term, index]).expect("binary:at");
        out.push(as_int(byte) as u8);
    }
    out
}

/// `erlang:tuple_size/1`.
fn tuple_size(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> usize {
    as_int(call(reg, ctx, "erlang", "tuple_size", 1, &[term]).expect("tuple_size")) as usize
}

/// `erlang:element/2` (1-based).
fn element(reg: &BifRegistry, ctx: &mut BifContext<'_>, index: i64, term: Eterm) -> Eterm {
    let index = ctx.int(index).expect("fits");
    call(reg, ctx, "erlang", "element", 2, &[index, term]).expect("element")
}

/// Asserts `out` is `{ok, Body, Rest}` with the pinned body/rest bytes.
fn assert_ok(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    atoms: &AtomTable,
    out: Eterm,
    body: &[u8],
    rest: &[u8],
) {
    assert_eq!(tuple_size(reg, ctx, out), 3, "{{ok,_,_}} is a 3-tuple");
    assert_eq!(atom_name(atoms, element(reg, ctx, 1, out)), "ok");
    let body_term = element(reg, ctx, 2, out);
    assert_eq!(binary_bytes(reg, ctx, body_term), body, "packet body");
    let rest_term = element(reg, ctx, 3, out);
    assert_eq!(binary_bytes(reg, ctx, rest_term), rest, "rest");
}

/// Asserts `out` is `{more, undefined}`.
fn assert_more_undefined(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    atoms: &AtomTable,
    out: Eterm,
) {
    assert_eq!(tuple_size(reg, ctx, out), 2);
    assert_eq!(atom_name(atoms, element(reg, ctx, 1, out)), "more");
    assert_eq!(atom_name(atoms, element(reg, ctx, 2, out)), "undefined");
}

/// Asserts `out` is `{more, Required}`.
fn assert_more(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    atoms: &AtomTable,
    out: Eterm,
    required: i64,
) {
    assert_eq!(tuple_size(reg, ctx, out), 2);
    assert_eq!(atom_name(atoms, element(reg, ctx, 1, out)), "more");
    assert_eq!(as_int(element(reg, ctx, 2, out)), required);
}

/// Asserts `out` is `{error, invalid}`.
fn assert_invalid(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    atoms: &AtomTable,
    out: Eterm,
) {
    assert_eq!(tuple_size(reg, ctx, out), 2);
    assert_eq!(atom_name(atoms, element(reg, ctx, 1, out)), "error");
    assert_eq!(atom_name(atoms, element(reg, ctx, 2, out)), "invalid");
}

/// A `{Key, Value}` option 2-tuple.
fn option(ctx: &mut BifContext<'_>, key: &str, value: Eterm) -> Eterm {
    let key = ctx.atom(key).expect("seeded");
    ctx.tuple(&[key, value]).expect("the option fits")
}

// ---------------------------------------------------------------------------
// `raw` / `0`
// ---------------------------------------------------------------------------

#[test]
fn raw_returns_the_whole_binary_and_needs_any_data() {
    // ORACLE: decode_packet(raw, <<1,2>>, []) => {ok,<<1,2>>,<<>>}
    //         decode_packet(raw, <<>>, [])   => {more,undefined}
    //         decode_packet(0, <<1,2>>, [])  => {ok,<<1,2>>,<<>>}
    //         decode_packet(0, <<>>, [])     => {more,undefined}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for name in ["raw"] {
        let ty = ctx.atom(name).expect("seeded");
        // A whole binary is one packet, with an empty rest.
        let bin = binary_term(&reg, &mut ctx, &[1, 2]);
        let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
        assert_ok(&reg, &mut ctx, &atoms, out, &[1, 2], &[]);

        // An empty binary is `{more, undefined}`.
        let empty = binary_term(&reg, &mut ctx, &[]);
        let out = decode(&reg, &mut ctx, ty, empty, NIL).expect("decode_packet");
        assert_more_undefined(&reg, &mut ctx, &atoms, out);
    }
    for int in [0_i64] {
        let ty = ctx.int(int).expect("fits");
        let bin = binary_term(&reg, &mut ctx, &[1, 2]);
        let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
        assert_ok(&reg, &mut ctx, &atoms, out, &[1, 2], &[]);

        let empty = binary_term(&reg, &mut ctx, &[]);
        let out = decode(&reg, &mut ctx, ty, empty, NIL).expect("decode_packet");
        assert_more_undefined(&reg, &mut ctx, &atoms, out);
    }
}

#[test]
fn raw_ignores_packet_size_and_line_length() {
    // ORACLE: decode_packet(raw, <<1,2>>, [{packet_size,1}])   => {ok,<<1,2>>,<<>>}
    //         decode_packet(raw, <<>>, [{packet_size,5}])      => {more,undefined}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.atom("raw").expect("seeded");
    let value = ctx.int(1).expect("fits");
    let opt = option(&mut ctx, "packet_size", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, &[1, 2]);
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, &[1, 2], &[]);

    let value = ctx.int(5).expect("fits");
    let opt = option(&mut ctx, "line_length", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let empty = binary_term(&reg, &mut ctx, &[]);
    let out = decode(&reg, &mut ctx, ty, empty, opts).expect("decode_packet");
    assert_more_undefined(&reg, &mut ctx, &atoms, out);
}

// ---------------------------------------------------------------------------
// `1` / `2` / `4`
// ---------------------------------------------------------------------------

#[test]
fn one_byte_length_prefix() {
    // ORACLE: decode_packet(1, <<2,97,98>>, []) => {ok,<<"ab">>,<<>>}
    //         decode_packet(1, <<2,97>>, [])     => {more,3}
    //         decode_packet(1, <<>>, [])         => {more,undefined}
    //         decode_packet(1, <<0>>, [])        => {ok,<<>>,<<>>}
    //         decode_packet(1, <<0,1,2>>, [])    => {ok,<<>>,<<1,2>>}
    //         decode_packet(1, <<255>>, [])      => {more,256}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.int(1).expect("fits");
    let bin = binary_term(&reg, &mut ctx, &[2, 97, 98]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"ab", b"");

    let bin = binary_term(&reg, &mut ctx, &[2, 97]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 3);

    let bin = binary_term(&reg, &mut ctx, &[]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    let bin = binary_term(&reg, &mut ctx, &[0]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"", b"");

    let bin = binary_term(&reg, &mut ctx, &[0, 1, 2]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"", &[1, 2]);

    let bin = binary_term(&reg, &mut ctx, &[255]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 256);
}

#[test]
fn two_byte_length_prefix_is_big_endian() {
    // ORACLE: decode_packet(2, <<0,3,97,98,99,100>>, []) => {ok,<<"abc">>,<<"d">>}
    //         decode_packet(2, <<0,3,97>>, [])           => {more,5}
    //         decode_packet(2, <<0>>, [])                => {more,undefined}
    //         decode_packet(2, <<0,0>>, [])              => {ok,<<>>,<<>>}
    //         decode_packet(2, <<1,0>>, [])              => {more,258}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.int(2).expect("fits");
    let bin = binary_term(&reg, &mut ctx, &[0, 3, 97, 98, 99, 100]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"abc", b"d");

    let bin = binary_term(&reg, &mut ctx, &[0, 3, 97]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 5);

    let bin = binary_term(&reg, &mut ctx, &[0]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    let bin = binary_term(&reg, &mut ctx, &[0, 0]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"", b"");

    // 0x0100 = 256 => a 258-byte packet.
    let bin = binary_term(&reg, &mut ctx, &[1, 0]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 258);
}

#[test]
fn four_byte_length_prefix_and_the_int_max_ceiling() {
    // ORACLE: decode_packet(4, <<0,0,0,2,97,98,99>>, []) => {ok,<<"ab">>,<<"c">>}
    //         decode_packet(4, <<0,0,0,2,97>>, [])       => {more,6}
    //         decode_packet(4, <<0,0,0,0>>, [])          => {ok,<<>>,<<>>}
    //         decode_packet(4, <<255,255,255,255>>, [])  => {error,invalid}
    //         decode_packet(4, <<127,255,255,255>>, [])  => {error,invalid}
    //         decode_packet(4, <<127,255,255,250>>, [])  => {more,2147483646}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.int(4).expect("fits");
    let bin = binary_term(&reg, &mut ctx, &[0, 0, 0, 2, 97, 98, 99]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"ab", b"c");

    let bin = binary_term(&reg, &mut ctx, &[0, 0, 0, 2, 97]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 6);

    let bin = binary_term(&reg, &mut ctx, &[0, 0, 0, 0]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"", b"");

    // `0xFFFFFFFF > INT_MAX - 4` => the `remain:` ceiling rejects it.
    let bin = binary_term(&reg, &mut ctx, &[255, 255, 255, 255]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_invalid(&reg, &mut ctx, &atoms, out);

    // `0x7FFFFFFF > INT_MAX - 4` likewise.
    let bin = binary_term(&reg, &mut ctx, &[127, 255, 255, 255]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_invalid(&reg, &mut ctx, &atoms, out);

    // The largest accepted declared length (INT_MAX - 5) reports `{more, ...}`
    // without allocating.
    let bin = binary_term(&reg, &mut ctx, &[127, 255, 255, 250]);
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 2147483646);
}

#[test]
fn packet_size_bounds_a_length_prefixed_packet() {
    // ORACLE: decode_packet(1, <<100,1,2>>, [{packet_size,50}])  => {error,invalid}
    //         decode_packet(1, <<100,1,2>>, [{packet_size,100}]) => {more,101}
    //         decode_packet(2, <<0,5,1>>, [{packet_size,3}])     => {error,invalid}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.int(1).expect("fits");
    let value = ctx.int(50).expect("fits");
    let opt = option(&mut ctx, "packet_size", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, &[100, 1, 2]);
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_invalid(&reg, &mut ctx, &atoms, out);

    let value = ctx.int(100).expect("fits");
    let opt = option(&mut ctx, "packet_size", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, &[100, 1, 2]);
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 101);

    let ty = ctx.int(2).expect("fits");
    let value = ctx.int(3).expect("fits");
    let opt = option(&mut ctx, "packet_size", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, &[0, 5, 1]);
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_invalid(&reg, &mut ctx, &atoms, out);
}

// ---------------------------------------------------------------------------
// `line`
// ---------------------------------------------------------------------------

#[test]
fn line_splits_on_the_newline_delimiter() {
    // ORACLE: decode_packet(line, <<"a\nb">>, []) => {ok,<<"a\n">>,<<"b">>}
    //         decode_packet(line, <<"abc">>, [])  => {more,undefined}
    //         decode_packet(line, <<>>, [])       => {more,undefined}
    //         decode_packet(line, <<"\n">>, [])   => {ok,<<"\n">>,<<>>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.atom("line").expect("seeded");
    let bin = binary_term(&reg, &mut ctx, b"a\nb");
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"a\n", b"b");

    let bin = binary_term(&reg, &mut ctx, b"abc");
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    let bin = binary_term(&reg, &mut ctx, b"");
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    let bin = binary_term(&reg, &mut ctx, b"\n");
    let out = decode(&reg, &mut ctx, ty, bin, NIL).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"\n", b"");
}

#[test]
fn line_length_truncates_the_line() {
    // ORACLE: decode_packet(line, <<"aaaa\nbbbb">>, [{line_length,2}]) => {ok,<<"aa">>,<<"aa\nbbbb">>}
    //         decode_packet(line, <<"a\nb">>, [{line_length,2}])       => {ok,<<"a\n">>,<<"b">>}
    //         decode_packet(line, <<"aaaa">>, [{line_length,2}])       => {ok,<<"aa">>,<<"aa">>}
    //         decode_packet(line, <<"abcd">>, [{line_length,4}])       => {ok,<<"abcd">>,<<>>}
    //         decode_packet(line, <<"abcd">>, [{line_length,5}])       => {more,undefined}
    //         decode_packet(line, <<"ab\n">>, [{line_length,3}])       => {ok,<<"ab\n">>,<<>>}
    //         decode_packet(line, <<"a\n">>, [{line_length,1}])        => {ok,<<"a">>,<<"\n">>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.atom("line").expect("seeded");
    let cases: &[(&[u8], i64, &[u8], &[u8])] = &[
        (b"aaaa\nbbbb", 2, b"aa", b"aa\nbbbb"),
        (b"a\nb", 2, b"a\n", b"b"),
        (b"aaaa", 2, b"aa", b"aa"),
        (b"abcd", 4, b"abcd", b""),
        (b"ab\n", 3, b"ab\n", b""),
        (b"a\n", 1, b"a", b"\n"),
    ];
    for (input, limit, body, rest) in cases {
        let value = ctx.int(*limit).expect("fits");
        let opt = option(&mut ctx, "line_length", value);
        let opts = ctx.list(&[opt]).expect("fits");
        let bin = binary_term(&reg, &mut ctx, input);
        let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
        assert_ok(&reg, &mut ctx, &atoms, out, body, rest);
    }

    // The buffer has not yet reached `line_length`: still `{more, undefined}`.
    let value = ctx.int(5).expect("fits");
    let opt = option(&mut ctx, "line_length", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, b"abcd");
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_more_undefined(&reg, &mut ctx, &atoms, out);
}

#[test]
fn line_delimiter_selects_another_terminator() {
    // ORACLE: decode_packet(line, <<"a;b">>, [{line_delimiter,59}]) => {ok,<<"a;">>,<<"b">>}
    //         decode_packet(line, <<"a\nb">>, [{line_delimiter,59}]) => {more,undefined}
    //         decode_packet(line, <<"a0b">>, [{line_delimiter,48}]) => {ok,<<"a0">>,<<"b">>}
    //         decode_packet(line, <<1,0,2>>, [{line_delimiter,0}])  => {ok,<<1,0>>,<<2>>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.atom("line").expect("seeded");

    let value = ctx.int(59).expect("fits");
    let opt = option(&mut ctx, "line_delimiter", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, b"a;b");
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"a;", b"b");

    let value = ctx.int(59).expect("fits");
    let opt = option(&mut ctx, "line_delimiter", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, b"a\nb");
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    let value = ctx.int(48).expect("fits");
    let opt = option(&mut ctx, "line_delimiter", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, b"a0b");
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"a0", b"b");

    // A NUL delimiter is a valid byte value.
    let value = ctx.int(0).expect("fits");
    let opt = option(&mut ctx, "line_delimiter", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, &[1, 0, 2]);
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, &[1, 0], &[2]);
}

#[test]
fn packet_size_bounds_a_line_and_line_length_zero_is_no_limit() {
    // ORACLE: decode_packet(line, <<"aaaa\nb">>, [{packet_size,3}])     => {error,invalid}
    //         decode_packet(line, <<"a">>, [{line_length,1},{packet_size,1}]) => {ok,<<"a">>,<<>>}
    //         decode_packet(line, <<"abc">>, [{line_length,0}])         => {more,undefined}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.atom("line").expect("seeded");

    let value = ctx.int(3).expect("fits");
    let opt = option(&mut ctx, "packet_size", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, b"aaaa\nb");
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_invalid(&reg, &mut ctx, &atoms, out);

    let value = ctx.int(1).expect("fits");
    let length = option(&mut ctx, "line_length", value);
    let value = ctx.int(1).expect("fits");
    let size = option(&mut ctx, "packet_size", value);
    let opts = ctx.list(&[length, size]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, b"a");
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, b"a", b"");

    let value = ctx.int(0).expect("fits");
    let opt = option(&mut ctx, "line_length", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, b"abc");
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_more_undefined(&reg, &mut ctx, &atoms, out);
}

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

#[test]
fn bad_bin_or_opts_are_badarg() {
    // ORACLE: erlang:decode_packet(raw, 42, [])            => badarg
    //         erlang:decode_packet(raw, [], [])            => badarg
    //         erlang:decode_packet(raw, <<1:3>>, [])       => badarg (not byte aligned)
    //         erlang:decode_packet(raw, <<>>, foo)         => badarg
    //         erlang:decode_packet(raw, <<>>, [foo])       => badarg
    //         erlang:decode_packet(raw, <<>>, [{foo,1}])   => badarg
    //         erlang:decode_packet(raw, <<>>, [{}])        => badarg
    //         erlang:decode_packet(raw, <<>>, [{packet_size}])   => badarg
    //         erlang:decode_packet(raw, <<>>, [{packet_size,1,2}]) => badarg
    //         erlang:decode_packet(raw, <<>>, [1])         => badarg
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.atom("raw").expect("seeded");

    // A non-binary `Bin`.
    let not_binary = ctx.int(42).expect("fits");
    let empty = binary_term(&reg, &mut ctx, &[]);
    assert!(matches!(
        decode(&reg, &mut ctx, ty, not_binary, NIL),
        Err(BeamError::BadArg)
    ));
    // A list is not a bitstring.
    assert!(matches!(decode(&reg, &mut ctx, ty, NIL, NIL), Err(BeamError::BadArg)));

    // A non-list `Opts`.
    let reason = ctx.atom("raw").expect("seeded");
    assert!(matches!(
        decode(&reg, &mut ctx, ty, empty, reason),
        Err(BeamError::BadArg)
    ));

    // An unknown option element (a bare integer).
    let element = ctx.int(7).expect("fits");
    let unknown_element = ctx.list(&[element]).expect("fits");
    assert!(matches!(
        decode(&reg, &mut ctx, ty, empty, unknown_element),
        Err(BeamError::BadArg)
    ));

    // A 1-tuple option.
    let element = ctx.int(7).expect("fits");
    let one_tuple = ctx.tuple(&[element]).expect("fits");
    let opts = ctx.list(&[one_tuple]).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, ty, empty, opts), Err(BeamError::BadArg)));

    // A 3-tuple option.
    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");
    let three = ctx.int(3).expect("fits");
    let three_tuple = ctx.tuple(&[one, two, three]).expect("fits");
    let opts = ctx.list(&[three_tuple]).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, ty, empty, opts), Err(BeamError::BadArg)));

    // An empty-tuple option `{}`.
    let empty_tuple = ctx.tuple(&[]).expect("fits");
    let opts = ctx.list(&[empty_tuple]).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, ty, empty, opts), Err(BeamError::BadArg)));

    // An unknown key.
    let value = ctx.int(1).expect("fits");
    let unknown = option(&mut ctx, "line", value);
    let opts = ctx.list(&[unknown]).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, ty, empty, opts), Err(BeamError::BadArg)));

    // An improper option list.
    let value = ctx.int(1).expect("fits");
    let opt = option(&mut ctx, "packet_size", value);
    let tail = ctx.atom("raw").expect("seeded");
    let improper = ctx.cons(opt, tail).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, ty, empty, improper), Err(BeamError::BadArg)));
}

#[test]
fn non_aligned_bitstring_bin_is_badarg() {
    // ORACLE: erlang:decode_packet(raw, <<1:3>>, []) => badarg
    //         erlang:decode_packet(1, <<1:3>>, [])   => badarg
    // A non-byte-aligned bitstring is `is_bitstring` but has no aligned bytes, so
    // `erts_get_aligned_binary_bytes` returns NULL (`erl_bif_port.c:1551-1556`).
    // The crate's registered BIFs cannot build `<<1:3>>`, so it is materialized
    // with the public, safe `beam_codec::term::make_bitstring`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let unaligned =
        beam_codec::term::make_bitstring(ctx.heap(), &[0b1000_0000], 3).expect("the bitstring fits");
    for ty in [ctx.atom("raw").expect("seeded"), ctx.int(1).expect("fits")] {
        assert!(matches!(
            decode(&reg, &mut ctx, ty, unaligned, NIL),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn packet_size_value_validation() {
    // ORACLE: erlang:decode_packet(1, <<100,1,2>>, [{packet_size,-1}])   => badarg
    //         erlang:decode_packet(1, <<100,1,2>>, [{packet_size,foo}])  => badarg
    //         erlang:decode_packet(1, <<2,1,2>>, [{packet_size,4294967296}]) => badarg
    //         erlang:decode_packet(1, <<2,1,2>>, [{packet_size,4294967295}]) => {ok,<<1,2>>,<<>>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ty = ctx.int(1).expect("fits");
    let bin = binary_term(&reg, &mut ctx, &[100, 1, 2]);

    // A negative value.
    let value = ctx.int(-1).expect("fits");
    let opt = option(&mut ctx, "packet_size", value);
    let opts = ctx.list(&[opt]).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, ty, bin, opts), Err(BeamError::BadArg)));

    // A non-integer value.
    let value = ctx.atom("raw").expect("seeded");
    let opt = option(&mut ctx, "packet_size", value);
    let opts = ctx.list(&[opt]).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, ty, bin, opts), Err(BeamError::BadArg)));

    // `2^32 > UINT_MAX` is rejected.
    let value = ctx.int(4_294_967_296).expect("fits");
    let opt = option(&mut ctx, "packet_size", value);
    let opts = ctx.list(&[opt]).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, ty, bin, opts), Err(BeamError::BadArg)));

    // `UINT_MAX` exactly is accepted.
    let value = ctx.int(4_294_967_295).expect("fits");
    let opt = option(&mut ctx, "packet_size", value);
    let opts = ctx.list(&[opt]).expect("fits");
    let bin = binary_term(&reg, &mut ctx, &[2, 1, 2]);
    let out = decode(&reg, &mut ctx, ty, bin, opts).expect("decode_packet");
    assert_ok(&reg, &mut ctx, &atoms, out, &[1, 2], &[]);
}

#[test]
fn line_delimiter_out_of_range_or_wrong_type_is_badarg() {
    // ORACLE: erlang:decode_packet(line, <<"a;b">>, [{line_delimiter,300}]) => badarg
    //         erlang:decode_packet(raw, <<"a;b">>, [{line_delimiter,59}])  => badarg
    //         erlang:decode_packet(line, <<"a;b">>, [{line_delimiter,-1}]) => badarg
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let line = ctx.atom("line").expect("seeded");
    let bin = binary_term(&reg, &mut ctx, b"a;b");

    let value = ctx.int(300).expect("fits");
    let opt = option(&mut ctx, "line_delimiter", value);
    let opts = ctx.list(&[opt]).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, line, bin, opts), Err(BeamError::BadArg)));

    let value = ctx.int(-1).expect("fits");
    let opt = option(&mut ctx, "line_delimiter", value);
    let opts = ctx.list(&[opt]).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, line, bin, opts), Err(BeamError::BadArg)));

    // `line_delimiter` is only valid for the `line` type.
    let raw = ctx.atom("raw").expect("seeded");
    let value = ctx.int(59).expect("fits");
    let opt = option(&mut ctx, "line_delimiter", value);
    let opts = ctx.list(&[opt]).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, raw, bin, opts), Err(BeamError::BadArg)));
}

#[test]
fn an_unknown_packet_type_is_badarg() {
    // ORACLE: erlang:decode_packet(bogus, <<>>, []) => badarg (cause => badopt)
    //         erlang:decode_packet(3, <<>>, [])     => badarg
    //         erlang:decode_packet(99, <<>>, [])    => badarg
    //         erlang:decode_packet(-1, <<>>, [])    => badarg
    //         erlang:decode_packet(1.0, <<>>, [])   => badarg
    //         erlang:decode_packet(cd, <<>>, [])    => badarg
    //         erlang:decode_packet(ssl, <<>>, [])   => badarg
    let reg = default_registry();
    let mut heap = heap();
    let mut atoms = seeded_atoms();
    // The non-packet-type names probed above.
    atoms.seed(["bogus", "cd", "ssl"]);
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let empty = binary_term(&reg, &mut ctx, &[]);

    for name in ["bogus", "cd", "ssl"] {
        let ty = ctx.atom(name).expect("seeded");
        assert!(
            matches!(decode(&reg, &mut ctx, ty, empty, NIL), Err(BeamError::BadArg)),
            "{name} is not a packet type"
        );
    }
    for int in [3_i64, 99, -1] {
        let ty = ctx.int(int).expect("fits");
        assert!(
            matches!(decode(&reg, &mut ctx, ty, empty, NIL), Err(BeamError::BadArg)),
            "{int} is not a packet type"
        );
    }
    let ty = ctx.make_float(1.0).expect("fits");
    assert!(matches!(decode(&reg, &mut ctx, ty, empty, NIL), Err(BeamError::BadArg)));
}

#[test]
fn valid_unimplemented_types_are_a_typed_capability_gap() {
    // The other valid packet types (`packet_parser.h:34-50`) are recognised — so
    // they are not `badarg` — but this Milestone reports them as the typed
    // `unsupported_bif` gap, never a fabricated packet.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for name in [
        "asn1", "sunrm", "cdr", "fcgi", "tpkt", "http", "httph", "http_bin", "httph_bin",
        "ssl_tls",
    ] {
        let ty = ctx.atom(name).expect("seeded");
        let bin = binary_term(&reg, &mut ctx, &[1, 2, 3]);
        match decode(&reg, &mut ctx, ty, bin, NIL) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(
                    raise.reason,
                    ExitReason::atom("unsupported_bif"),
                    "{name} must be the typed capability gap"
                );
            }
            other => panic!("{name}: expected unsupported_bif, got {other:?}"),
        }
    }

    // Arguments are still validated first: a non-binary `Bin` stays `badarg`.
    let ty = ctx.atom("asn1").expect("seeded");
    let bad_bin = ctx.int(42).expect("fits");
    assert!(matches!(
        decode(&reg, &mut ctx, ty, bad_bin, NIL),
        Err(BeamError::BadArg)
    ));

    // So does a bad option for an unimplemented type.
    let bin = binary_term(&reg, &mut ctx, &[1, 2, 3]);
    let value = ctx.int(3).expect("fits");
    let opt = option(&mut ctx, "packet_size", value);
    let value = ctx.int(300).expect("fits");
    let bad_delimiter = option(&mut ctx, "line_delimiter", value);
    let opts = ctx.list(&[opt, bad_delimiter]).expect("fits");
    assert!(matches!(
        decode(&reg, &mut ctx, ty, bin, opts),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// Live oracle cross-check
// ---------------------------------------------------------------------------

/// Runs one `-eval` statement under the system `erl`; `None` when the oracle is
/// unavailable (the test then skips loudly).
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

#[test]
fn live_oracle_agrees_on_the_six_types() {
    // One live cross-check against the system ERTS: the shapes pinned above must
    // also come out of the real `erl` today. `~w` (not `~p`) is used so the
    // oracle does not line-wrap the output. Skips loudly when unavailable.
    let Some(output) = oracle_eval(
        "io:format(\"~w~n\",[[\
             erlang:decode_packet(raw,<<1,2>>,[]),\
             erlang:decode_packet(1,<<2,97,98>>,[]),\
             erlang:decode_packet(2,<<0,3,97,98,99,100>>,[]),\
             erlang:decode_packet(4,<<0,0,0,2,97,98,99>>,[]),\
             erlang:decode_packet(line,<<97,10,98>>,[]),\
             erlang:decode_packet(line,<<97,97,97,97>>,[{line_length,2}]),\
             erlang:decode_packet(2,<<0,3,97>>,[])]])",
    ) else {
        return;
    };
    assert_eq!(
        output.trim(),
        "[{ok,<<1,2>>,<<>>},\
         {ok,<<97,98>>,<<>>},\
         {ok,<<97,98,99>>,<<100>>},\
         {ok,<<97,98>>,<<99>>},\
         {ok,<<97,10>>,<<98>>},\
         {ok,<<97,97>>,<<97,97>>},\
         {more,5}]",
        "the live oracle disagrees with the pinned goldens"
    );
}
