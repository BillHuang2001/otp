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

//! Oracle-pinned integration tests for the **length-framed**
//! `erlang:decode_packet/3` types (`sunrm`, `asn1`, `cdr`, `fcgi`, `tpkt`),
//! registered by `src/packet/framing.rs` and driven through the **public** API
//! only (`default_registry()` + `BifRegistry::call`). `tests/packet.rs` owns the
//! `raw`/`1`/`2`/`4`/`line` half of the family.
//!
//! Every golden below carries the probe that pinned it against the system OTP
//! 29.1.1 / erts-17.1 oracle. The result terms are inspected with the registered
//! `erlang:tuple_size/1`, `erlang:element/2`, `erlang:byte_size/1` and
//! `binary:at/2` BIFs plus `beam_term::{atom_val, signed_value}`.
//!
//! One live cross-check shells out to the system `erl` and **skips loudly** when
//! the oracle is absent (the `tests/os_bif.rs` pattern); the committed
//! assertions pass without it.

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, signed_value, AtomTable, Eterm, NIL};

/// A generous heap: several tests build a handful of small binaries.
fn heap() -> Heap {
    Heap::new(16384).expect("test heap")
}

/// The atoms `decode_packet/3` resolves by name: the result tags, the packet
/// types exercised here and the option key they use.
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
#[allow(clippy::too_many_arguments)]
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

/// Decodes `bytes` as `type` with no options and returns the result term.
fn run(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    type_name: &str,
    bytes: &[u8],
) -> Eterm {
    let ty = ctx.atom(type_name).expect("seeded");
    let bin = binary_term(reg, ctx, bytes);
    decode(reg, ctx, ty, bin, NIL).expect("decode_packet")
}

/// Decodes `bytes` as `type` with a single `{packet_size, Size}` option.
fn run_sized(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    type_name: &str,
    bytes: &[u8],
    size: i64,
) -> Eterm {
    let ty = ctx.atom(type_name).expect("seeded");
    let bin = binary_term(reg, ctx, bytes);
    let value = ctx.int(size).expect("fits");
    let opt = option(ctx, "packet_size", value);
    let opts = ctx.list(&[opt]).expect("fits");
    decode(reg, ctx, ty, bin, opts).expect("decode_packet")
}

// ---------------------------------------------------------------------------
// `tpkt` (`TCP_PB_TPKT`, `packet_parser.c:451-463`)
// ---------------------------------------------------------------------------

#[test]
fn tpkt_uses_the_header_relative_declared_length() {
    // ORACLE: decode_packet(tpkt, <<3,0,0,5,65,66>>, []) => {ok,<<3,0,0,5,65>>,<<"B">>}
    //         decode_packet(tpkt, <<3,0,0,5>>, [])     => {more,5}
    //         decode_packet(tpkt, <<3,0,0,255,1>>, [])  => {more,255}
    //         decode_packet(tpkt, <<3,0>>, [])          => {more,undefined}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let out = run(&reg, &mut ctx, "tpkt", &[3, 0, 0, 5, 65, 66]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[3, 0, 0, 5, 65], b"B");

    // Exactly the 4-byte header: the packet length is 5, so one byte is missing.
    let out = run(&reg, &mut ctx, "tpkt", &[3, 0, 0, 5]);
    assert_more(&reg, &mut ctx, &atoms, out, 5);

    // packet_length 0x00FF => a 255-byte packet.
    let out = run(&reg, &mut ctx, "tpkt", &[3, 0, 0, 255, 1]);
    assert_more(&reg, &mut ctx, &atoms, out, 255);

    // Fewer bytes than the 4-byte header: the C's `goto more` (length 0).
    let out = run(&reg, &mut ctx, "tpkt", &[3, 0]);
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    let out = run(&reg, &mut ctx, "tpkt", &[]);
    assert_more_undefined(&reg, &mut ctx, &atoms, out);
}

#[test]
fn tpkt_rejects_a_foreign_version_and_a_short_declared_length() {
    // ORACLE: decode_packet(tpkt, <<2,0,0,5,65>>, [])   => {error,invalid}
    //         decode_packet(tpkt, <<3,0,0,2,65,66>>, []) => {error,invalid}
    //         decode_packet(tpkt, <<3,0,0,3,65,66>>, []) => {error,invalid}
    //         decode_packet(tpkt, <<3,0,0,4,65,66>>, []) => {ok,<<3,0,0,4>>,<<"AB">>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `vrsn != 3` is the C's `goto error`.
    let out = run(&reg, &mut ctx, "tpkt", &[2, 0, 0, 5, 65]);
    assert_invalid(&reg, &mut ctx, &atoms, out);

    // A declared length below the header size wraps to a huge `plen` in C's
    // `unsigned` arithmetic, which the `remain:` ceiling then rejects.
    for declared in [2_u8, 3] {
        let out = run(&reg, &mut ctx, "tpkt", &[3, 0, 0, declared, 65, 66]);
        assert_invalid(&reg, &mut ctx, &atoms, out);
    }

    // A declared length of exactly the header size is an empty packet.
    let out = run(&reg, &mut ctx, "tpkt", &[3, 0, 0, 4, 65, 66]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[3, 0, 0, 4], b"AB");
}

#[test]
fn tpkt_packet_size_bounds_the_declared_length() {
    // ORACLE: decode_packet(tpkt, <<3,0,0,5,65,66>>, [{packet_size,5}])  => {ok,<<3,0,0,5,65>>,<<"B">>}
    //         decode_packet(tpkt, <<3,0,0,5,65,66>>, [{packet_size,1}])  => {ok,<<3,0,0,5,65>>,<<"B">>}
    //         decode_packet(tpkt, <<3,0,1,44,65>>, [{packet_size,100}])  => {error,invalid}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `packet_size` bounds the *payload*, not the header, so `plen = 1 <= 5`
    // and `plen = 1 <= 1` both pass.
    for size in [5_i64, 1] {
        let out = run_sized(&reg, &mut ctx, "tpkt", &[3, 0, 0, 5, 65, 66], size);
        assert_ok(&reg, &mut ctx, &atoms, out, &[3, 0, 0, 5, 65], b"B");
    }

    // `<<3,0,1,44,...>>` declares 300 bytes => plen 296 > 100.
    let out = run_sized(&reg, &mut ctx, "tpkt", &[3, 0, 1, 44, 65], 100);
    assert_invalid(&reg, &mut ctx, &atoms, out);
}

#[test]
fn tpkt_reproduces_the_c_int_wraparound_with_a_huge_packet_size() {
    // The C sums `hlen + plen` in 32-bit `unsigned` and returns `int`, and
    // `decode_packet_3` then tests the result for `< 0`. With a `packet_size`
    // above `INT_MAX` the `remain:` ceiling no longer bounds the sum, so both
    // the wrap and the sign are observable:
    // ORACLE: decode_packet(tpkt, <<3,0,0,0,65,66>>, [{packet_size,4294967295}])
    //            => {more,undefined}
    //         decode_packet(tpkt, <<3,0,0,3,65,66>>, [{packet_size,4294967295}])
    //            => {ok,<<3,0,0>>,<<3,65,66>>}
    //         decode_packet(tpkt, <<3,0,0,4,65,66>>, [{packet_size,4294967295}])
    //            => {ok,<<3,0,0,4>>,<<"AB">>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // plen = 0 - 4 = 0xFFFFFFFC => 4 + 0xFFFFFFFC wraps to 0 => `{more,undefined}`.
    let out = run_sized(&reg, &mut ctx, "tpkt", &[3, 0, 0, 0, 65, 66], 4_294_967_295);
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    // plen = 3 - 4 = 0xFFFFFFFF => the sum wraps to 3 bytes.
    let out = run_sized(&reg, &mut ctx, "tpkt", &[3, 0, 0, 3, 65, 66], 4_294_967_295);
    assert_ok(&reg, &mut ctx, &atoms, out, &[3, 0, 0], &[3, 65, 66]);

    // plen = 0 => the sum is the 4-byte header.
    let out = run_sized(&reg, &mut ctx, "tpkt", &[3, 0, 0, 4, 65, 66], 4_294_967_295);
    assert_ok(&reg, &mut ctx, &atoms, out, &[3, 0, 0, 4], b"AB");
}

// ---------------------------------------------------------------------------
// `sunrm` (`TCP_PB_RM`, `packet_parser.c:286-293`)
// ---------------------------------------------------------------------------

#[test]
fn sunrm_masks_off_the_end_of_record_bit() {
    // ORACLE: decode_packet(sunrm, <<0,0,0,5,65,66,67,68>>, [])   => {more,9}
    //         decode_packet(sunrm, <<128,0,0,5,65,66,67,68>>, []) => {more,9}
    //         decode_packet(sunrm, <<0,0,0,4,65,66,67,68>>, [])   => {ok,<<0,0,0,4,65,66,67,68>>,<<>>}
    //         decode_packet(sunrm, <<128,0,0,4,65,66,67,68>>, []) => {ok,<<128,0,0,4,65,66,67,68>>,<<>>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // The top bit (`0x80`) is the end-of-record flag, masked off before the length.
    for lead in [0_u8, 128] {
        let out = run(&reg, &mut ctx, "sunrm", &[lead, 0, 0, 5, 65, 66, 67, 68]);
        assert_more(&reg, &mut ctx, &atoms, out, 9);

        let out = run(&reg, &mut ctx, "sunrm", &[lead, 0, 0, 4, 65, 66, 67, 68]);
        assert_ok(&reg, &mut ctx, &atoms, out, &[lead, 0, 0, 4, 65, 66, 67, 68], b"");
    }
}

#[test]
fn sunrm_needs_four_header_bytes_and_bounds_the_length() {
    // ORACLE: decode_packet(sunrm, <<0,0,0>>, [])             => {more,undefined}
    //         decode_packet(sunrm, <<0,0,0,2,65>>, [])        => {more,6}
    //         decode_packet(sunrm, <<0,0,0,255,255,255,255>>, []) => {more,259}
    //         decode_packet(sunrm, <<255,255,255,255,65>>, []) => {error,invalid}
    //         decode_packet(sunrm, <<255,255,255,252,65>>, []) => {error,invalid}
    //         decode_packet(sunrm, <<127,255,255,255,65>>, []) => {error,invalid}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let out = run(&reg, &mut ctx, "sunrm", &[0, 0, 0]);
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    let out = run(&reg, &mut ctx, "sunrm", &[0, 0, 0, 2, 65]);
    assert_more(&reg, &mut ctx, &atoms, out, 6);

    // The declared length is the first four bytes: 0x000000FF = 255 => 259 bytes.
    let out = run(&reg, &mut ctx, "sunrm", &[0, 0, 0, 255, 255, 255, 255]);
    assert_more(&reg, &mut ctx, &atoms, out, 259);

    // 0x7FFFFFFF (after masking) and 0x7FFFFFFC exceed `INT_MAX - 4`.
    for head in [[255_u8, 255, 255, 255], [255, 255, 255, 252], [127, 255, 255, 255]] {
        let bytes = [head[0], head[1], head[2], head[3], 65];
        let out = run(&reg, &mut ctx, "sunrm", &bytes);
        assert_invalid(&reg, &mut ctx, &atoms, out);
    }
}

#[test]
fn sunrm_packet_size_is_the_payload_ceiling() {
    // ORACLE: decode_packet(sunrm, <<0,0,0,4,65,66,67,68>>, [{packet_size,8}]) => {ok,<<0,0,0,4,65,66,67,68>>,<<>>}
    //         decode_packet(sunrm, <<0,0,0,4,65,66,67,68>>, [{packet_size,3}]) => {error,invalid}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let out = run_sized(&reg, &mut ctx, "sunrm", &[0, 0, 0, 4, 65, 66, 67, 68], 8);
    assert_ok(&reg, &mut ctx, &atoms, out, &[0, 0, 0, 4, 65, 66, 67, 68], b"");

    let out = run_sized(&reg, &mut ctx, "sunrm", &[0, 0, 0, 4, 65, 66, 67, 68], 3);
    assert_invalid(&reg, &mut ctx, &atoms, out);
}

// ---------------------------------------------------------------------------
// `asn1` (`TCP_PB_ASN1`, `packet_parser.c:324-363`)
// ---------------------------------------------------------------------------

#[test]
fn asn1_short_length_form() {
    // ORACLE: decode_packet(asn1, <<48,3,1,2,3>>, []) => {ok,<<48,3,1,2,3>>,<<>>}
    //         decode_packet(asn1, <<48,5,1,2,3>>, []) => {more,7}
    //         decode_packet(asn1, <<48,3>>, [])       => {more,5}
    //         decode_packet(asn1, <<48>>, [])         => {more,undefined}
    //         decode_packet(asn1, <<>>, [])           => {more,undefined}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let out = run(&reg, &mut ctx, "asn1", &[48, 3, 1, 2, 3]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[48, 3, 1, 2, 3], b"");

    let out = run(&reg, &mut ctx, "asn1", &[48, 5, 1, 2, 3]);
    assert_more(&reg, &mut ctx, &atoms, out, 7);

    let out = run(&reg, &mut ctx, "asn1", &[48, 3]);
    assert_more(&reg, &mut ctx, &atoms, out, 5);

    // `n < 2` is the C's first `goto more`.
    for bytes in [&[48_u8][..], &[]] {
        let out = run(&reg, &mut ctx, "asn1", bytes);
        assert_more_undefined(&reg, &mut ctx, &atoms, out);
    }
}

#[test]
fn asn1_long_length_form_covers_zero_through_four_bytes() {
    // ORACLE: decode_packet(asn1, <<48,16#80,1,2,3>>, [])      => {ok,<<48,128>>,<<1,2,3>>}
    //         decode_packet(asn1, <<48,16#80>>, [])            => {ok,<<48,128>>,<<>>}
    //         decode_packet(asn1, <<48,16#81,3,1,2,3>>, [])    => {ok,<<48,129,3,1,2,3>>,<<>>}
    //         decode_packet(asn1, <<48,16#82,0,5,1,2,3,4,5>>, []) => {ok,<<48,130,0,5,1,2,3,4,5>>,<<>>}
    //         decode_packet(asn1, <<48,16#83,0,0,3,1,2,3>>, [])   => {ok,<<48,131,0,0,3,1,2,3>>,<<>>}
    //         decode_packet(asn1, <<48,16#84,0,0,0,3,1,2,3>>, []) => {ok,<<48,132,0,0,0,3,1,2,3>>,<<>>}
    //         decode_packet(asn1, <<48,16#82,0,3,1,2>>, [])       => {more,7}
    //         decode_packet(asn1, <<48,16#83,0,0,3,1,2>>, [])     => {more,8}
    //         decode_packet(asn1, <<48,16#84,0,0,0,3,1,2>>, [])   => {more,9}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // A `0` selector means the length payload is empty.
    let out = run(&reg, &mut ctx, "asn1", &[48, 0x80, 1, 2, 3]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[48, 0x80], &[1, 2, 3]);

    // ... and is complete at just two bytes.
    let out = run(&reg, &mut ctx, "asn1", &[48, 0x80]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[48, 0x80], b"");

    let out = run(&reg, &mut ctx, "asn1", &[48, 0x81, 3, 1, 2, 3]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[48, 0x81, 3, 1, 2, 3], b"");

    let out = run(&reg, &mut ctx, "asn1", &[48, 0x82, 0, 5, 1, 2, 3, 4, 5]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[48, 0x82, 0, 5, 1, 2, 3, 4, 5], b"");

    let out = run(&reg, &mut ctx, "asn1", &[48, 0x83, 0, 0, 3, 1, 2, 3]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[48, 0x83, 0, 0, 3, 1, 2, 3], b"");

    let out = run(&reg, &mut ctx, "asn1", &[48, 0x84, 0, 0, 0, 3, 1, 2, 3]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[48, 0x84, 0, 0, 0, 3, 1, 2, 3], b"");

    // `nn < length` is the C's "need more" condition for the length payload.
    let out = run(&reg, &mut ctx, "asn1", &[48, 0x82, 0, 3, 1, 2]);
    assert_more(&reg, &mut ctx, &atoms, out, 7);

    let out = run(&reg, &mut ctx, "asn1", &[48, 0x83, 0, 0, 3, 1, 2]);
    assert_more(&reg, &mut ctx, &atoms, out, 8);

    let out = run(&reg, &mut ctx, "asn1", &[48, 0x84, 0, 0, 0, 3, 1, 2]);
    assert_more(&reg, &mut ctx, &atoms, out, 9);
}

#[test]
fn asn1_rejects_a_long_length_selector_above_four() {
    // ORACLE: decode_packet(asn1, <<48,16#85,1,2,3,4,5>>, []) => {error,invalid}
    //         decode_packet(asn1, <<48,16#85,1,2,3,4>>, [])   => {more,undefined}
    //         decode_packet(asn1, <<48,16#88,1,2>>, [])       => {more,undefined}
    //         decode_packet(asn1, <<48,127,1,2>>, [])         => {more,129}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // The `nn < length` check runs *before* the selector switch, so a
    // 5-byte selector with 5 bytes left reaches `default: goto error`...
    let out = run(&reg, &mut ctx, "asn1", &[48, 0x85, 1, 2, 3, 4, 5]);
    assert_invalid(&reg, &mut ctx, &atoms, out);

    // ... while one with only 4 bytes left is still "need more".
    let out = run(&reg, &mut ctx, "asn1", &[48, 0x85, 1, 2, 3, 4]);
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    let out = run(&reg, &mut ctx, "asn1", &[48, 0x88, 1, 2]);
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    // A *short-form* length of 127 is a plain 129-byte packet.
    let out = run(&reg, &mut ctx, "asn1", &[48, 127, 1, 2]);
    assert_more(&reg, &mut ctx, &atoms, out, 129);
}

#[test]
fn asn1_long_tag_format_skips_the_continuation_bytes() {
    // ORACLE: decode_packet(asn1, <<16#1f,16#81,0,3,1,2,3>>, [])          => {ok,<<31,129,0,3,1,2,3>>,<<>>}
    //         decode_packet(asn1, <<16#1f,16#81,0,3,1,2>>, [])           => {more,7}
    //         decode_packet(asn1, <<16#1f,16#05,1,2,3>>, [])              => {ok,<<31,5,1,2>>,<<3>>}
    //         decode_packet(asn1, <<16#1f,16#81,16#82,16#03,0,3,1,2,3>>, []) => {ok,<<31,129,130,3,0>>,<<3,1,2,3>>}
    //         decode_packet(asn1, <<16#1f,16#81,0,16#82,0,2,1,2>>, [])    => {ok,<<31,129,0,130,0,2,1,2>>,<<>>}
    //         decode_packet(asn1, <<16#1f>>, [])                          => {more,undefined}
    //         decode_packet(asn1, <<16#1f,16#81>>, [])                    => {more,undefined}
    //         decode_packet(asn1, <<16#1f,16#80,16#80>>, [])              => {more,undefined}
    //         decode_packet(asn1, <<16#1f,16#81,0,16#80>>, [])            => {ok,<<31,129,0,128>>,<<>>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let out = run(&reg, &mut ctx, "asn1", &[0x1f, 0x81, 0, 3, 1, 2, 3]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[31, 129, 0, 3, 1, 2, 3], b"");

    let out = run(&reg, &mut ctx, "asn1", &[0x1f, 0x81, 0, 3, 1, 2]);
    assert_more(&reg, &mut ctx, &atoms, out, 7);

    // A single continuation byte that does *not* have the high bit set ends the
    // tag run immediately.
    let out = run(&reg, &mut ctx, "asn1", &[0x1f, 0x05, 1, 2, 3]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[31, 5, 1, 2], &[3]);

    // Two continuation bytes.
    let out = run(&reg, &mut ctx, "asn1", &[0x1f, 0x81, 0x82, 0x03, 0, 3, 1, 2, 3]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[31, 129, 130, 3, 0], &[3, 1, 2, 3]);

    // A long tag followed by a long length.
    let out = run(&reg, &mut ctx, "asn1", &[0x1f, 0x81, 0, 0x82, 0, 2, 1, 2]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[31, 129, 0, 130, 0, 2, 1, 2], b"");

    // `nn < 2` behind the tag run, and the run consuming the whole buffer.
    for bytes in [&[0x1f_u8][..], &[0x1f, 0x81][..], &[0x1f, 0x80, 0x80][..]] {
        let out = run(&reg, &mut ctx, "asn1", bytes);
        assert_more_undefined(&reg, &mut ctx, &atoms, out);
    }

    // A long tag with a zero-length long-form length.
    let out = run(&reg, &mut ctx, "asn1", &[0x1f, 0x81, 0, 0x80]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[31, 129, 0, 128], b"");
}

#[test]
fn asn1_reproduces_the_c_int_wraparound_with_a_huge_packet_size() {
    // ORACLE: decode_packet(asn1, <<48,16#84,255,255,255,250,1>>, [{packet_size,4294967295}])
    //            => {more,undefined}
    //         decode_packet(asn1, <<48,16#84,255,255,255,244,1>>, [{packet_size,4294967295}])
    //            => {error,invalid}
    //         decode_packet(asn1, <<48,3,1,2,3>>, [{packet_size,4}]) => {ok,<<48,3,1,2,3>>,<<>>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // hlen 6 + plen 0xFFFFFFFA wraps to 0.
    let out =
        run_sized(&reg, &mut ctx, "asn1", &[48, 0x84, 255, 255, 255, 250, 1], 4_294_967_295);
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    // hlen 6 + plen 0xFFFFFFF4 = 0xFFFFFFFA, which is negative as an `int`.
    let out =
        run_sized(&reg, &mut ctx, "asn1", &[48, 0x84, 255, 255, 255, 244, 1], 4_294_967_295);
    assert_invalid(&reg, &mut ctx, &atoms, out);

    // A `packet_size` that only bounds the payload leaves a normal packet alone.
    let out = run_sized(&reg, &mut ctx, "asn1", &[48, 3, 1, 2, 3], 4);
    assert_ok(&reg, &mut ctx, &atoms, out, &[48, 3, 1, 2, 3], b"");
}

// ---------------------------------------------------------------------------
// `cdr` (`TCP_PB_CDR`, `packet_parser.c:365-377`)
// ---------------------------------------------------------------------------

#[test]
fn cdr_checks_the_giop_magic_and_uses_the_flag_selected_byte_order() {
    // ORACLE: decode_packet(cdr, <<"GIOP",1,2,0,0,0,0,0,4,65,66,67,68>>, [])
    //            => {ok,<<"GIOP",1,2,0,0,0,0,0,4,65,66,67,68>>,<<>>}
    //         decode_packet(cdr, <<"GIOP",1,2,1,0,4,0,0,0,65,66,67,68>>, [])
    //            => {ok,<<"GIOP",1,2,1,0,4,0,0,0,65,66,67,68>>,<<>>}
    //         decode_packet(cdr, <<"GIOP",1,2,1,0,0,0,0,4,65,66,67,68>>, [])
    //            => {more,67108876}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `flags` bit 0 clear: a big-endian message size.
    let be = [71, 73, 79, 80, 1, 2, 0, 0, 0, 0, 0, 4, 65, 66, 67, 68];
    let out = run(&reg, &mut ctx, "cdr", &be);
    assert_ok(&reg, &mut ctx, &atoms, out, &be, b"");

    // `flags` bit 0 set: a little-endian `4,0,0,0` is still 4.
    let le = [71, 73, 79, 80, 1, 2, 1, 0, 4, 0, 0, 0, 65, 66, 67, 68];
    let out = run(&reg, &mut ctx, "cdr", &le);
    assert_ok(&reg, &mut ctx, &atoms, out, &le, b"");

    // ... whereas a big-endian `0,0,0,4` read little-endian is 0x04000000.
    let le_big = [71, 73, 79, 80, 1, 2, 1, 0, 0, 0, 0, 4, 65, 66, 67, 68];
    let out = run(&reg, &mut ctx, "cdr", &le_big);
    assert_more(&reg, &mut ctx, &atoms, out, 67_108_876);
}

#[test]
fn cdr_needs_its_twelve_byte_header_and_validates_the_magic() {
    // ORACLE: decode_packet(cdr, <<"GIOP",1,2,0,0,0,0,0,4>>, [])            => {more,16}
    //         decode_packet(cdr, <<"GIOP",1,2,0,0,0,0,0>>, [])              => {more,undefined}
    //         decode_packet(cdr, <<"XXXX",1,2,0,0,0,0,0,4,65>>, [])         => {error,invalid}
    //         decode_packet(cdr, <<"GIOP",1,2,0,0,255,255,255,255,65>>, []) => {error,invalid}
    //         decode_packet(cdr, <<"GIOP",1,2,0,0,128,0,0,0,65>>, [])       => {error,invalid}
    //         decode_packet(cdr, <<"GIOP",1,2,1,0,0,0,0,128,65>>, [])       => {error,invalid}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let out = run(&reg, &mut ctx, "cdr", &[71, 73, 79, 80, 1, 2, 0, 0, 0, 0, 0, 4]);
    assert_more(&reg, &mut ctx, &atoms, out, 16);

    let out = run(&reg, &mut ctx, "cdr", &[71, 73, 79, 80, 1, 2, 0, 0, 0, 0, 0]);
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    let out = run(&reg, &mut ctx, "cdr", &[88, 88, 88, 88, 1, 2, 0, 0, 0, 0, 0, 4, 65]);
    assert_invalid(&reg, &mut ctx, &atoms, out);

    // 0xFFFFFFFF (big-endian) and 0x80000000 (either endianness) all exceed
    // `INT_MAX - 12`.
    for size in [[255_u8, 255, 255, 255], [128, 0, 0, 0]] {
        let be = [71, 73, 79, 80, 1, 2, 0, 0, size[0], size[1], size[2], size[3], 65];
        let out = run(&reg, &mut ctx, "cdr", &be);
        assert_invalid(&reg, &mut ctx, &atoms, out);
    }
    let le = [71, 73, 79, 80, 1, 2, 1, 0, 0, 0, 0, 128, 65];
    let out = run(&reg, &mut ctx, "cdr", &le);
    assert_invalid(&reg, &mut ctx, &atoms, out);
}

#[test]
fn cdr_reproduces_the_c_int_wraparound_with_a_huge_packet_size() {
    // ORACLE: decode_packet(cdr, <<"GIOP",1,2,0,0,255,255,255,244,65,66>>, [{packet_size,4294967295}])
    //            => {more,undefined}
    //         decode_packet(cdr, <<"GIOP",1,2,0,0,255,255,255,247,65,66>>, [{packet_size,4294967295}])
    //            => {ok,<<"GIO">>,<<80,1,2,0,0,255,255,255,247,65,66>>}
    //         decode_packet(cdr, <<"GIOP",1,2,0,0,255,255,255,240,65,66>>, [{packet_size,4294967295}])
    //            => {error,invalid}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // 12 + 0xFFFFFFF4 wraps to 0.
    let out = run_sized(
        &reg,
        &mut ctx,
        "cdr",
        &[71, 73, 79, 80, 1, 2, 0, 0, 255, 255, 255, 244, 65, 66],
        4_294_967_295,
    );
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    // 12 + 0xFFFFFFF7 wraps to 3 bytes of packet.
    let out = run_sized(
        &reg,
        &mut ctx,
        "cdr",
        &[71, 73, 79, 80, 1, 2, 0, 0, 255, 255, 255, 247, 65, 66],
        4_294_967_295,
    );
    assert_ok(
        &reg,
        &mut ctx,
        &atoms,
        out,
        b"GIO",
        &[80, 1, 2, 0, 0, 255, 255, 255, 247, 65, 66],
    );

    // 12 + 0xFFFFFFF0 = 0xFFFFFFFC, negative as an `int`.
    let out = run_sized(
        &reg,
        &mut ctx,
        "cdr",
        &[71, 73, 79, 80, 1, 2, 0, 0, 255, 255, 255, 240, 65, 66],
        4_294_967_295,
    );
    assert_invalid(&reg, &mut ctx, &atoms, out);
}

// ---------------------------------------------------------------------------
// `fcgi` (`TCP_PB_FCGI`, `packet_parser.c:379-389`)
// ---------------------------------------------------------------------------

#[test]
fn fcgi_sums_the_content_and_padding_lengths_and_trims_the_padding() {
    // ORACLE: decode_packet(fcgi, <<1,2,0,1,0,3,2,0,65,66,67,9,9>>, [])
    //            => {ok,<<1,2,0,1,0,3,2,0,65,66,67>>,<<>>}
    //         decode_packet(fcgi, <<1,2,0,1,0,0,4,0,9,9,9,9>>, [])
    //            => {ok,<<1,2,0,1,0,0,4,0>>,<<>>}
    //         decode_packet(fcgi, <<1,2,0,1,0,0,0,0>>, []) => {ok,<<1,2,0,1,0,0,0,0>>,<<>>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // contentLength 3 + paddingLength 2 => a 13-byte packet whose body is the
    // 11 bytes up to the padding.
    let out = run(&reg, &mut ctx, "fcgi", &[1, 2, 0, 1, 0, 3, 2, 0, 65, 66, 67, 9, 9]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[1, 2, 0, 1, 0, 3, 2, 0, 65, 66, 67], b"");

    // An empty content with 4 padding bytes: only the header is the body.
    let out = run(&reg, &mut ctx, "fcgi", &[1, 2, 0, 1, 0, 0, 4, 0, 9, 9, 9, 9]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[1, 2, 0, 1, 0, 0, 4, 0], b"");

    let out = run(&reg, &mut ctx, "fcgi", &[1, 2, 0, 1, 0, 0, 0, 0]);
    assert_ok(&reg, &mut ctx, &atoms, out, &[1, 2, 0, 1, 0, 0, 0, 0], b"");
}

#[test]
fn fcgi_requires_version_one_and_its_eight_byte_header() {
    // ORACLE: decode_packet(fcgi, <<2,2,0,1,0,3,2,0,65,66>>, []) => {error,invalid}
    //         decode_packet(fcgi, <<0,2,0,1,0,3,2,0>>, [])      => {error,invalid}
    //         decode_packet(fcgi, <<1,2,0,1,0,3,2,0,65,66>>, []) => {more,13}
    //         decode_packet(fcgi, <<1,2,0,1,0,3,2>>, [])         => {more,undefined}
    //         decode_packet(fcgi, <<1,2,0,1,0,255,255,0,9>>, []) => {more,518}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for version in [2_u8, 0] {
        let out = run(&reg, &mut ctx, "fcgi", &[version, 2, 0, 1, 0, 3, 2, 0, 65, 66]);
        assert_invalid(&reg, &mut ctx, &atoms, out);
    }

    let out = run(&reg, &mut ctx, "fcgi", &[1, 2, 0, 1, 0, 3, 2, 0, 65, 66]);
    assert_more(&reg, &mut ctx, &atoms, out, 13);

    let out = run(&reg, &mut ctx, "fcgi", &[1, 2, 0, 1, 0, 3, 2]);
    assert_more_undefined(&reg, &mut ctx, &atoms, out);

    // contentLength 0x00FF = 255, paddingLength 0xFF = 255 => 8 + 510 = 518.
    let out = run(&reg, &mut ctx, "fcgi", &[1, 2, 0, 1, 0, 255, 255, 0, 9]);
    assert_more(&reg, &mut ctx, &atoms, out, 518);
}

#[test]
fn fcgi_packet_size_bounds_the_declared_length() {
    // ORACLE: decode_packet(fcgi, <<1,2,0,1,0,3,2,0,65,66,67,9,9>>, [{packet_size,10}])
    //            => {ok,<<1,2,0,1,0,3,2,0,65,66,67>>,<<>>}
    //         decode_packet(fcgi, <<1,2,0,1,0,3,2,0,65,66,67,9,9>>, [{packet_size,4}])
    //            => {error,invalid}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let out = run_sized(&reg, &mut ctx, "fcgi", &[1, 2, 0, 1, 0, 3, 2, 0, 65, 66, 67, 9, 9], 10);
    assert_ok(&reg, &mut ctx, &atoms, out, &[1, 2, 0, 1, 0, 3, 2, 0, 65, 66, 67], b"");

    // plen 3 + 2 = 5 > 4.
    let out = run_sized(&reg, &mut ctx, "fcgi", &[1, 2, 0, 1, 0, 3, 2, 0, 65, 66, 67, 9, 9], 4);
    assert_invalid(&reg, &mut ctx, &atoms, out);
}

#[test]
fn a_non_binary_bin_is_still_badarg_for_the_new_types() {
    // The argument validation runs before the type is dispatched
    // (`erl_bif_port.c:1513-1556`), so the new arms never see a malformed `Bin`
    // and every type answers the oracle's `badarg` rather than a length.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bad_bin = ctx.int(42).expect("fits");
    for ty_name in ["sunrm", "asn1", "cdr", "fcgi", "tpkt"] {
        let ty = ctx.atom(ty_name).expect("seeded");
        assert!(
            matches!(decode(&reg, &mut ctx, ty, bad_bin, NIL), Err(BeamError::BadArg)),
            "{ty_name} with a non-binary Bin"
        );
    }
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
fn live_oracle_agrees_on_the_five_framed_types() {
    // One live cross-check against the system ERTS: the shapes pinned above must
    // also come out of the real `erl` today. `~w` (not `~p`) is used so the
    // oracle does not line-wrap the output. Skips loudly when unavailable.
    let Some(output) = oracle_eval(
        "io:format(\"~w~n\",[[\
             erlang:decode_packet(tpkt,<<3,0,0,5,65,66>>,[]),\
             erlang:decode_packet(tpkt,<<3,0,0,2,65,66>>,[]),\
             erlang:decode_packet(sunrm,<<128,0,0,5,65,66,67,68>>,[]),\
             erlang:decode_packet(asn1,<<48,3,1,2,3>>,[]),\
             erlang:decode_packet(asn1,<<31,129,0,3,1,2,3>>,[]),\
             erlang:decode_packet(asn1,<<48,133,1,2,3,4,5>>,[]),\
             erlang:decode_packet(cdr,<<\"GIOP\",1,2,1,0,4,0,0,0,65,66,67,68>>,[]),\
             erlang:decode_packet(cdr,<<\"XXXX\",1,2,0,0,0,0,0,4,65>>,[]),\
             erlang:decode_packet(fcgi,<<1,2,0,1,0,3,2,0,65,66,67,9,9>>,[]),\
             erlang:decode_packet(fcgi,<<2,2,0,1,0,3,2,0,65,66>>,[])]])",
    ) else {
        return;
    };
    assert_eq!(
        output.trim(),
        "[{ok,<<3,0,0,5,65>>,<<66>>},\
         {error,invalid},\
         {more,9},\
         {ok,<<48,3,1,2,3>>,<<>>},\
         {ok,<<31,129,0,3,1,2,3>>,<<>>},\
         {error,invalid},\
         {ok,<<71,73,79,80,1,2,1,0,4,0,0,0,65,66,67,68>>,<<>>},\
         {error,invalid},\
         {ok,<<1,2,0,1,0,3,2,0,65,66,67>>,<<>>},\
         {error,invalid}]",
        "the live oracle disagrees with the pinned goldens"
    );
}
