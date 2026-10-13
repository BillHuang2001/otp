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

//! Oracle-pinned integration tests for `erlang:decode_packet/3`'s `ssl_tls`
//! family (`src/packet/tls.rs`), driven through the **public** API only
//! (`default_registry()` + `BifRegistry::call`).
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

/// A generous heap: these BIFs build a 5-tuple plus a small binary.
fn heap() -> Heap {
    Heap::new(8192).expect("test heap")
}

/// The atoms `decode_packet/3` resolves by name for this family, plus the
/// packet-type atom and the option key.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "ok",
        "error",
        "more",
        "undefined",
        "invalid",
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

/// `erlang:decode_packet(ssl_tls, Bin, Opts)`.
fn decode_ssl_tls(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    bin: Eterm,
    opts: Eterm,
) -> Result<Eterm> {
    let ty = ctx.atom("ssl_tls").expect("seeded");
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

/// A `{packet_size, N}` option 2-tuple.
fn packet_size(ctx: &mut BifContext<'_>, value: i64) -> Eterm {
    let key = ctx.atom("packet_size").expect("seeded");
    let value = ctx.int(value).expect("fits");
    ctx.tuple(&[key, value]).expect("the option fits")
}

/// Asserts `out` is the ssl_tls packet with the pinned fields and body/rest
/// bytes: `{ok, {ssl_tls, [], ContentType, {Major, Minor}, Body}, Rest}`.
#[allow(clippy::too_many_arguments)]
fn assert_ssl_tls_ok(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    atoms: &AtomTable,
    out: Eterm,
    content_type: i64,
    major: i64,
    minor: i64,
    body: &[u8],
    rest: &[u8],
) {
    assert_eq!(tuple_size(reg, ctx, out), 3, "{{ok,_,_}} is a 3-tuple");
    assert_eq!(atom_name(atoms, element(reg, ctx, 1, out)), "ok");

    let packet = element(reg, ctx, 2, out);
    assert_eq!(tuple_size(reg, ctx, packet), 5, "the ssl_tls packet is a 5-tuple");
    assert_eq!(atom_name(atoms, element(reg, ctx, 1, packet)), "ssl_tls");
    // The C `TUPLE5(..., am_ssl_tls, NIL, ...)`: the second element is the empty
    // list.
    assert_eq!(element(reg, ctx, 2, packet), NIL, "the second element is []");
    assert_eq!(as_int(element(reg, ctx, 3, packet)), content_type, "content type");

    let version = element(reg, ctx, 4, packet);
    assert_eq!(tuple_size(reg, ctx, version), 2, "{{Major,Minor}} is a 2-tuple");
    assert_eq!(as_int(element(reg, ctx, 1, version)), major, "major version");
    assert_eq!(as_int(element(reg, ctx, 2, version)), minor, "minor version");

    let body_term = element(reg, ctx, 5, packet);
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

/// Asserts `result` is the crate's typed `unsupported_bif` capability gap.
fn assert_unsupported_bif(result: Result<Eterm>, what: &str) {
    match result {
        Err(BeamError::Raise(raise)) if raise.reason == ExitReason::atom("unsupported_bif") => {}
        other => panic!("{what}: expected the typed unsupported_bif gap, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The normal `<<ContentType:8, Version:16, Length:16>>` record header
// ---------------------------------------------------------------------------

#[test]
fn a_normal_record_header_reports_its_fields_and_body() {
    // ORACLE: decode_packet(ssl_tls, <<22,3,1,0,4,"ABCD","c">>, [])
    //         => {ok,{ssl_tls,[],22,{3,1},<<"ABCD">>},<<"c">>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bin = binary_term(&reg, &mut ctx, &[22, 3, 1, 0, 4, 65, 66, 67, 68, 99]);
    let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
    assert_ssl_tls_ok(&reg, &mut ctx, &atoms, out, 22, 3, 1, b"ABCD", b"c");
}

#[test]
fn a_zero_length_record_body_is_a_complete_packet() {
    // ORACLE: decode_packet(ssl_tls, <<22,3,1,0,0>>, [])
    //         => {ok,{ssl_tls,[],22,{3,1},<<>>},<<>>}
    //         decode_packet(ssl_tls, <<22,3,1,0,0,9,9>>, [])
    //         => {ok,{ssl_tls,[],22,{3,1},<<>>},<<9,9>>}
    //         decode_packet(ssl_tls, <<22,3,1,0,0>>, [{packet_size,0}])
    //         => {ok,{ssl_tls,[],22,{3,1},<<>>},<<>>}   (0 = no limit)
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bin = binary_term(&reg, &mut ctx, &[22, 3, 1, 0, 0]);
    let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
    assert_ssl_tls_ok(&reg, &mut ctx, &atoms, out, 22, 3, 1, b"", b"");

    let bin = binary_term(&reg, &mut ctx, &[22, 3, 1, 0, 0, 9, 9]);
    let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
    assert_ssl_tls_ok(&reg, &mut ctx, &atoms, out, 22, 3, 1, b"", &[9, 9]);

    let bin = binary_term(&reg, &mut ctx, &[22, 3, 1, 0, 0]);
    let opt = packet_size(&mut ctx, 0);
    let opts = ctx.list(&[opt]).expect("fits");
    let out = decode_ssl_tls(&reg, &mut ctx, bin, opts).expect("decode_packet");
    assert_ssl_tls_ok(&reg, &mut ctx, &atoms, out, 22, 3, 1, b"", b"");
}

#[test]
fn the_16_bit_record_length_is_read_big_endian_and_unsigned() {
    // ORACLE: decode_packet(ssl_tls, <<22,3,1,255,255>>, []) => {more,65540}
    //         decode_packet(ssl_tls, <<22,3,1,255,255>>, [{packet_size,65535}])
    //         => {more,65540}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bin = binary_term(&reg, &mut ctx, &[22, 3, 1, 255, 255]);
    let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 65540);

    let bin = binary_term(&reg, &mut ctx, &[22, 3, 1, 255, 255]);
    let opt = packet_size(&mut ctx, 65535);
    let opts = ctx.list(&[opt]).expect("fits");
    let out = decode_ssl_tls(&reg, &mut ctx, bin, opts).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 65540);
}

// ---------------------------------------------------------------------------
// The SSLv2 ClientHello form
// ---------------------------------------------------------------------------

#[test]
fn an_ssl2_client_hello_is_rebuilt_as_a_tls_record() {
    // ORACLE: decode_packet(ssl_tls, <<128,4,1,3,1,65,66>>, [])
    //         => {ok,{ssl_tls,[],22,{3,1},<<1,0,0,3,3,1,65>>},<<"B">>}
    // The declared length is `(0x8004 land 0x7fff) - 3 = 1`, so the packet is
    // 6 bytes; the body is the 4-byte prefix `[1, 3:24]` (3 = packet_sz - 3)
    // followed by `bytes[3..6]`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bin = binary_term(&reg, &mut ctx, &[128, 4, 1, 3, 1, 65, 66]);
    let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
    assert_ssl_tls_ok(&reg, &mut ctx, &atoms, out, 22, 3, 1, &[1, 0, 0, 3, 3, 1, 65], b"B");
}

#[test]
fn an_ssl2_packet_leaves_the_bytes_after_it_as_the_rest() {
    // ORACLE: decode_packet(ssl_tls, <<128,4,1,3,1,65,66,77,88>>, [])
    //         => {ok,{ssl_tls,[],22,{3,1},<<1,0,0,3,3,1,65>>},<<"BMX">>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bin = binary_term(&reg, &mut ctx, &[128, 4, 1, 3, 1, 65, 66, 77, 88]);
    let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
    assert_ssl_tls_ok(&reg, &mut ctx, &atoms, out, 22, 3, 1, &[1, 0, 0, 3, 3, 1, 65], b"BMX");
}

#[test]
fn the_ssl2_length_word_is_masked_with_0x7fff() {
    // ORACLE: decode_packet(ssl_tls, <<255,255,1,3,1,65,66>>, []) => {more,32769}
    // `get_int16(<<255,255>>) & 0x7fff = 32767`; 32767 - 3 = 32764; 5 + 32764.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bin = binary_term(&reg, &mut ctx, &[255, 255, 1, 3, 1, 65, 66]);
    let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 32769);
}

#[test]
fn a_high_first_byte_with_a_third_byte_other_than_one_is_a_normal_record() {
    // ORACLE: decode_packet(ssl_tls, <<128,4,2,3,1,65,66>>, [])
    //         => {more,774}
    // The SSLv2 form requires `ptr[2] == 1`; otherwise the bytes are read as a
    // normal record whose length is `get_int16(<<3,1>>) = 769`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bin = binary_term(&reg, &mut ctx, &[128, 4, 2, 3, 1, 65, 66]);
    let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 774);
}

#[test]
fn the_ssl2_subtraction_wraps_as_an_unsigned_32_bit_quantity() {
    // ORACLE: an SSLv2 length field below the 3-byte header wraps just under
    // 2^32, so the `remain:` ceiling only accepts it for an enormous
    // `{packet_size, N}`:
    //   <<128,1,1,3,1,65,66>> + {packet_size,4294967293} => {error,invalid}
    //   <<128,2,1,3,1,65,66>> + {packet_size,4294967294} => {error,invalid}
    //   <<128,0,1,3,1,65,66>> + {packet_size,4294967292} => {error,invalid}
    //   <<128,1,1,3,1,65,66>> + {packet_size,4294967294}
    //     => {ok,{ssl_tls,[],22,{3,1},<<1,0,0,0>>},<<3,1,65,66>>}
    //   <<128,2,1,3,1,65,66>> + {packet_size,4294967295}
    //     => {ok,{ssl_tls,[],22,{3,1},<<1,0,0,1,3>>},<<1,65,66>>}
    // The two accepted cases are the C's `hlen + plen` narrowed to a 32-bit
    // `unsigned` and then to `int`: 5 + 4294967294 == 3 and 5 + 4294967295 == 4.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for (bytes, limit) in [
        (&[128u8, 1, 1, 3, 1, 65, 66][..], 4_294_967_293_i64),
        (&[128u8, 2, 1, 3, 1, 65, 66][..], 4_294_967_294),
        (&[128u8, 0, 1, 3, 1, 65, 66][..], 4_294_967_292),
    ] {
        let bin = binary_term(&reg, &mut ctx, bytes);
        let opt = packet_size(&mut ctx, limit);
        let opts = ctx.list(&[opt]).expect("fits");
        let out = decode_ssl_tls(&reg, &mut ctx, bin, opts).expect("decode_packet");
        assert_invalid(&reg, &mut ctx, &atoms, out);
    }

    let bin = binary_term(&reg, &mut ctx, &[128, 1, 1, 3, 1, 65, 66]);
    let opt = packet_size(&mut ctx, 4_294_967_294);
    let opts = ctx.list(&[opt]).expect("fits");
    let out = decode_ssl_tls(&reg, &mut ctx, bin, opts).expect("decode_packet");
    assert_ssl_tls_ok(&reg, &mut ctx, &atoms, out, 22, 3, 1, &[1, 0, 0, 0], &[3, 1, 65, 66]);

    let bin = binary_term(&reg, &mut ctx, &[128, 2, 1, 3, 1, 65, 66]);
    let opt = packet_size(&mut ctx, 4_294_967_295);
    let opts = ctx.list(&[opt]).expect("fits");
    let out = decode_ssl_tls(&reg, &mut ctx, bin, opts).expect("decode_packet");
    assert_ssl_tls_ok(&reg, &mut ctx, &atoms, out, 22, 3, 1, &[1, 0, 0, 1, 3], &[1, 65, 66]);
}

#[test]
fn the_c_undefined_ssl2_wrap_corner_is_the_typed_gap() {
    // The C's `ssl_tls_erl` callback is handed `packet_sz - 3 < 0` when the
    // wrapped SSLv2 length leaves a packet below its own 3-byte header, and the
    // emulator aborts (`sys_memcpy` with a negative size, exit 134 on a live
    // probe). That input therefore has no defined oracle answer and this crate
    // reports its honest typed capability gap instead of a fabricated packet.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bin = binary_term(&reg, &mut ctx, &[128, 0, 1, 3, 1, 65, 66]);
    let opt = packet_size(&mut ctx, 4_294_967_293);
    let opts = ctx.list(&[opt]).expect("fits");
    assert_unsupported_bif(
        decode_ssl_tls(&reg, &mut ctx, bin, opts),
        "packet_sz == 2 in the SSLv2 path",
    );
}

// ---------------------------------------------------------------------------
// The shared `remain:` rule: `more` / `invalid` / the `packet_size` ceiling
// ---------------------------------------------------------------------------

#[test]
fn a_buffer_shorter_than_the_header_is_more_undefined() {
    // ORACLE: decode_packet(ssl_tls, <<>>, [])          => {more,undefined}
    //         decode_packet(ssl_tls, <<22>>, [])        => {more,undefined}
    //         decode_packet(ssl_tls, <<22,3,1,0>>, [])  => {more,undefined}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for bytes in [&[][..], &[22u8][..], &[22u8, 3, 1, 0][..]] {
        let bin = binary_term(&reg, &mut ctx, bytes);
        let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
        assert_more_undefined(&reg, &mut ctx, &atoms, out);
    }
}

#[test]
fn an_incomplete_packet_asks_for_the_whole_packet_size() {
    // ORACLE: decode_packet(ssl_tls, <<22,3,1,0,4,65>>, []) => {more,9}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bin = binary_term(&reg, &mut ctx, &[22, 3, 1, 0, 4, 65]);
    let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
    assert_more(&reg, &mut ctx, &atoms, out, 9);
}

#[test]
fn the_packet_size_option_bounds_the_declared_length() {
    // ORACLE: decode_packet(ssl_tls, <<22,3,1,0,4,"ABCD","c">>, [{packet_size,3}])
    //         => {error,invalid}          (4 > 3)
    //         decode_packet(ssl_tls, <<22,3,1,0,4,"ABCD">>, [{packet_size,4}])
    //         => {ok,{ssl_tls,[],22,{3,1},<<"ABCD">>},<<>>}   (4 == 4 is allowed)
    //         decode_packet(ssl_tls, <<22,3,1,0,4,"ABCD">>, [{packet_size,9}])
    //         => {ok,{ssl_tls,[],22,{3,1},<<"ABCD">>},<<>>}
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bin = binary_term(&reg, &mut ctx, &[22, 3, 1, 0, 4, 65, 66, 67, 68, 99]);
    let opt = packet_size(&mut ctx, 3);
    let opts = ctx.list(&[opt]).expect("fits");
    let out = decode_ssl_tls(&reg, &mut ctx, bin, opts).expect("decode_packet");
    assert_invalid(&reg, &mut ctx, &atoms, out);

    for limit in [4_i64, 9] {
        let bin = binary_term(&reg, &mut ctx, &[22, 3, 1, 0, 4, 65, 66, 67, 68]);
        let opt = packet_size(&mut ctx, limit);
        let opts = ctx.list(&[opt]).expect("fits");
        let out = decode_ssl_tls(&reg, &mut ctx, bin, opts).expect("decode_packet");
        assert_ssl_tls_ok(&reg, &mut ctx, &atoms, out, 22, 3, 1, b"ABCD", b"");
    }
}

#[test]
fn an_ssl2_length_below_the_header_is_invalid_under_the_default_ceiling() {
    // ORACLE: decode_packet(ssl_tls, <<128,0,1,3,1,65,66>>, []) => {error,invalid}
    //         decode_packet(ssl_tls, <<128,1,1,3,1,65,66>>, []) => {error,invalid}
    //         decode_packet(ssl_tls, <<128,2,1,3,1,65,66>>, []) => {error,invalid}
    //         decode_packet(ssl_tls, <<128,4,1,3,1,65,66>>, [{packet_size,1}])
    //         => {ok,{ssl_tls,[],22,{3,1},<<1,0,0,3,3,1,65>>},<<"B">>}  (1 == 1)
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for bytes in [
        &[128u8, 0, 1, 3, 1, 65, 66][..],
        &[128u8, 1, 1, 3, 1, 65, 66][..],
        &[128u8, 2, 1, 3, 1, 65, 66][..],
    ] {
        let bin = binary_term(&reg, &mut ctx, bytes);
        let out = decode_ssl_tls(&reg, &mut ctx, bin, NIL).expect("decode_packet");
        assert_invalid(&reg, &mut ctx, &atoms, out);
    }

    let bin = binary_term(&reg, &mut ctx, &[128, 4, 1, 3, 1, 65, 66]);
    let opt = packet_size(&mut ctx, 1);
    let opts = ctx.list(&[opt]).expect("fits");
    let out = decode_ssl_tls(&reg, &mut ctx, bin, opts).expect("decode_packet");
    assert_ssl_tls_ok(&reg, &mut ctx, &atoms, out, 22, 3, 1, &[1, 0, 0, 3, 3, 1, 65], b"B");
}

#[test]
fn the_arguments_are_validated_before_the_ssl_tls_rules() {
    // ORACLE: decode_packet(ssl_tls, 42, [])              => error:badarg
    //         decode_packet(ssl_tls, <<22,3,1,0,0>>, foo) => error:badarg
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let bad_bin = ctx.int(42).expect("fits");
    assert!(matches!(
        decode_ssl_tls(&reg, &mut ctx, bad_bin, NIL),
        Err(BeamError::BadArg)
    ));

    let bin = binary_term(&reg, &mut ctx, &[22, 3, 1, 0, 0]);
    let bad_opts = ctx.atom("ssl_tls").expect("seeded");
    assert!(matches!(
        decode_ssl_tls(&reg, &mut ctx, bin, bad_opts),
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
fn live_oracle_agrees_on_the_ssl_tls_goldens() {
    // One live cross-check against the system ERTS: the shapes pinned above must
    // also come out of the real `erl` today. `~w` is used — it never line-wraps
    // and renders every binary as its byte list (`<<65,66,67,68>>`, not
    // `<<"ABCD">>`). Skips loudly when unavailable.
    let Some(output) = oracle_eval(
        "io:format(\"~w~n\",[[\
             erlang:decode_packet(ssl_tls,<<22,3,1,0,4,65,66,67,68,99>>,[]),\
             erlang:decode_packet(ssl_tls,<<128,4,1,3,1,65,66>>,[]),\
             erlang:decode_packet(ssl_tls,<<128,4,1,3,1,65,66,77,88>>,[]),\
             erlang:decode_packet(ssl_tls,<<255,255,1,3,1,65,66>>,[]),\
             erlang:decode_packet(ssl_tls,<<128,4,2,3,1,65,66>>,[]),\
             erlang:decode_packet(ssl_tls,<<22,3,1,0,0,9,9>>,[]),\
             erlang:decode_packet(ssl_tls,<<22,3,1,0,4,65>>,[]),\
             erlang:decode_packet(ssl_tls,<<22,3,1,255,255>>,[]),\
             erlang:decode_packet(ssl_tls,<<>>,[]),\
             erlang:decode_packet(ssl_tls,<<22,3,1,0,4,65,66,67,68,99>>,[{packet_size,3}]),\
             erlang:decode_packet(ssl_tls,<<22,3,1,0,4,65,66,67,68>>,[{packet_size,4}]),\
             erlang:decode_packet(ssl_tls,<<128,0,1,3,1,65,66>>,[]),\
             erlang:decode_packet(ssl_tls,<<128,4,1,3,1,65,66>>,[{packet_size,1}]),\
             erlang:decode_packet(ssl_tls,<<128,1,1,3,1,65,66>>,[{packet_size,4294967294}]),\
             erlang:decode_packet(ssl_tls,<<128,2,1,3,1,65,66>>,[{packet_size,4294967295}])]])",
    ) else {
        return;
    };
    assert_eq!(
        output.trim(),
        "[{ok,{ssl_tls,[],22,{3,1},<<65,66,67,68>>},<<99>>},\
         {ok,{ssl_tls,[],22,{3,1},<<1,0,0,3,3,1,65>>},<<66>>},\
         {ok,{ssl_tls,[],22,{3,1},<<1,0,0,3,3,1,65>>},<<66,77,88>>},\
         {more,32769},\
         {more,774},\
         {ok,{ssl_tls,[],22,{3,1},<<>>},<<9,9>>},\
         {more,9},\
         {more,65540},\
         {more,undefined},\
         {error,invalid},\
         {ok,{ssl_tls,[],22,{3,1},<<65,66,67,68>>},<<>>},\
         {error,invalid},\
         {ok,{ssl_tls,[],22,{3,1},<<1,0,0,3,3,1,65>>},<<66>>},\
         {ok,{ssl_tls,[],22,{3,1},<<1,0,0,0>>},<<3,1,65,66>>},\
         {ok,{ssl_tls,[],22,{3,1},<<1,0,0,1,3>>},<<1,65,66>>}]",
        "the live oracle disagrees with the pinned goldens"
    );
}
