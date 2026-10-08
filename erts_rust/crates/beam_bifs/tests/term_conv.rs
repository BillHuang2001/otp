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

//! Integration tests for the `term_conv` BIFs —
//! `erlang:bitstring_to_list/1`, `erlang:delete_element/2`,
//! `erlang:insert_element/3`, `erlang:iolist_to_iovec/1`, `erlang:is_integer/3`,
//! `erlang:display/1`, `erlang:display_string/2`, `erlang:external_size/1,2`,
//! `binary:referenced_byte_size/1`, and the textual pid/ref/port conversions
//! `erlang:pid_to_list/1`, `erlang:list_to_pid/1`, `erlang:ref_to_list/1`,
//! `erlang:list_to_ref/1`, `erlang:pid_to_binary/1`, `erlang:ref_to_binary/1`,
//! `erlang:port_to_binary/1` — driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call) (the crate's public seam).
//!
//! Every expectation is pinned to the OTP 29 / erts-17.1 oracle; the probe is
//! quoted at the assertion. One test re-runs the shared goldens under the live
//! system `erl` and **skips loudly** when the oracle is unavailable. The three
//! `*_to_binary/1` keys are OTP-30 additions the oracle does not export, so they
//! are pinned to `bif.c` instead (see
//! [`the_to_binary_variants_are_absent_from_the_otp29_oracle`]).
//!
//! Bitstring arguments/results are built and read with the sibling **safe**
//! `beam_codec::term` helpers, because the crate's own bitstring helpers are
//! private. All term construction goes through the `BifContext`, so its heap
//! borrow stays unique.

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, BytesSink, TableRegistry};
use beam_codec::error::is_unsupported;
use beam_codec::term as codec_term;
use beam_common::BeamError;
use beam_process::{PortId, ProcessId};
use beam_term::{atom_val, compare::eq, is_small, signed_value, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true", "false", "ok", "a", "b", "c", "x", "stdout", "stderr", "stdin", "bogus",
    ]);
    atoms
}

fn heap() -> Heap {
    Heap::new(16 * 1024).expect("test heap")
}

/// Calls the registered BIF `module:name/arity`.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    name: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    reg.call(module, name, arity, ctx, args)
}

/// A small integer on the context heap.
fn small(ctx: &mut BifContext<'_>, v: i64) -> Eterm {
    ctx.int(v).expect("a small int fits")
}

/// A byte-aligned binary on the context heap.
fn bin(ctx: &mut BifContext<'_>, bytes: &[u8]) -> Eterm {
    codec_term::make_binary(ctx.heap(), bytes).expect("the binary fits")
}

/// A `size_bits`-wide bitstring on the context heap.
fn bitstr(ctx: &mut BifContext<'_>, bytes: &[u8], size_bits: usize) -> Eterm {
    codec_term::make_bitstring(ctx.heap(), bytes, size_bits).expect("the bitstring fits")
}

/// The proper list `[v0, v1, …]` of the small integers `values`.
fn ints(ctx: &mut BifContext<'_>, values: &[i64]) -> Eterm {
    let elems: Vec<Eterm> = values.iter().map(|&v| ctx.int(v).expect("fits")).collect();
    ctx.list(&elems).expect("the list fits")
}

/// The number of elements of a proper list (a binary element is not a small
/// integer, so `int_elements` cannot count an iovec).
fn len_of(list: Eterm) -> usize {
    let mut n = 0;
    let mut cur = list;
    while codec_term::cons_head(cur).is_ok() {
        n += 1;
        cur = codec_term::cons_tail(cur).expect("a cons");
    }
    n
}

/// The small-integer elements of a proper list result, stopping at the first
/// non-integer element (so a list ending in a trailing bitstring still reads).
fn int_elements(list: Eterm) -> Vec<i64> {
    let mut out = Vec::new();
    let mut cur = list;
    while let Ok(head) = codec_term::cons_head(cur) {
        if !is_small(head) {
            break;
        }
        out.push(signed_value(head));
        cur = codec_term::cons_tail(cur).expect("a cons");
    }
    out
}

/// The `(packed bytes, bit length)` of a bitstring term, canonicalized so a
/// non-zero source bit offset cannot hide.
fn bits_of(term: Eterm) -> (Vec<u8>, usize) {
    let (bytes, offset, size_bits) =
        codec_term::bitstring_bytes(term).expect("the result is a bitstring");
    let mut packed = vec![0u8; size_bits.div_ceil(8)];
    for i in 0..size_bits {
        let bit = offset + i;
        if (bytes[bit / 8] >> (7 - (bit % 8))) & 1 == 1 {
            packed[i / 8] |= 0x80 >> (i % 8);
        }
    }
    (packed, size_bits)
}

/// The `true`/`false` atom a BIF returned, as a `bool`.
fn as_bool(atoms: &AtomTable, term: Eterm) -> bool {
    match atoms.name(atom_val(term)) {
        Some("true") => true,
        Some("false") => false,
        other => panic!("expected a boolean, got {other:?}"),
    }
}

/// Runs one `-eval` statement under the system `erl`; `None` (with a loud
/// message) when the oracle is unavailable, so the cross-check skips instead of
/// failing.
fn oracle_eval(statement: &str) -> Option<String> {
    match Command::new("erl")
        .args(["-noshell", "-eval", statement])
        .output()
    {
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

// ---------------------------------------------------------------------------
// bitstring_to_list/1
// ---------------------------------------------------------------------------

#[test]
fn bitstring_to_list_of_a_whole_byte_binary() {
    // ORACLE: `erlang:bitstring_to_list(<<1,2,3>>)` => `[1,2,3]`;
    //         `erlang:bitstring_to_list(<<>>)` => `[]`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let three = bin(&mut ctx, &[1, 2, 3]);
    let list =
        call(&reg, &mut ctx, "erlang", "bitstring_to_list", 1, &[three]).expect("a bitstring");
    assert_eq!(int_elements(list), vec![1, 2, 3]);

    let empty = bin(&mut ctx, &[]);
    let list =
        call(&reg, &mut ctx, "erlang", "bitstring_to_list", 1, &[empty]).expect("a bitstring");
    assert!(eq(list, NIL, &atoms), "an empty bitstring is the empty list");
}

#[test]
fn bitstring_to_list_keeps_the_trailing_bits_as_a_bitstring() {
    // ORACLE: `erlang:bitstring_to_list(<<1,2,3:4>>)` => `[1,2,<<3:4>>]`;
    //         `erlang:bitstring_to_list(<<1:9>>)` => `[0,<<1:1>>]`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `<<1,2,3:4>>`: two whole bytes then the 4-bit remainder `0011`.
    let partial = bitstr(&mut ctx, &[1, 2, 0b0011_0000], 20);
    let list =
        call(&reg, &mut ctx, "erlang", "bitstring_to_list", 1, &[partial]).expect("a bitstring");
    assert_eq!(int_elements(list), vec![1, 2]);
    let tail = codec_term::cons_tail(list).expect("a cons");
    let last = codec_term::cons_head(codec_term::cons_tail(tail).expect("a cons"))
        .expect("the trailing element");
    assert_eq!(bits_of(last), (vec![0b0011_0000], 4));

    // `<<1:9>>`: one whole byte (0) then the 1-bit remainder (1).
    let nine = bitstr(&mut ctx, &[0, 0b1000_0000], 9);
    let list =
        call(&reg, &mut ctx, "erlang", "bitstring_to_list", 1, &[nine]).expect("a bitstring");
    assert_eq!(int_elements(list), vec![0]);
    let last = codec_term::cons_head(codec_term::cons_tail(list).expect("a cons"))
        .expect("the trailing element");
    assert_eq!(bits_of(last), (vec![0b1000_0000], 1));
}

#[test]
fn bitstring_to_list_rejects_non_bitstrings() {
    // ORACLE: `erlang:bitstring_to_list([1,2])` and `(5)`/`(a)` are all `badarg`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let list = ints(&mut ctx, &[1, 2]);
    let a = ctx.atom("a").expect("seeded");
    let five = small(&mut ctx, 5);
    let tuple = ctx.tuple(&[five]).expect("fits");
    for term in [list, five, a, tuple, NIL] {
        assert!(matches!(
            call(&reg, &mut ctx, "erlang", "bitstring_to_list", 1, &[term]),
            Err(BeamError::BadArg)
        ));
    }
}

// ---------------------------------------------------------------------------
// delete_element/2 and insert_element/3
// ---------------------------------------------------------------------------

#[test]
fn delete_element_removes_the_indexed_element() {
    // ORACLE: `erlang:delete_element(1,{a,b,c})` => `{b,c}`;
    //         `erlang:delete_element(3,{a,b,c})` => `{a,b}`;
    //         `erlang:delete_element(1,{a})` => `{}`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let c = ctx.atom("c").expect("seeded");
    let tuple = ctx.tuple(&[a, b, c]).expect("fits");

    let one = small(&mut ctx, 1);
    let dropped = call(&reg, &mut ctx, "erlang", "delete_element", 2, &[one, tuple])
        .expect("a tuple");
    assert_eq!(codec_term::tuple_arity(dropped).expect("a tuple"), 2);
    assert_eq!(codec_term::tuple_element(dropped, 0).expect("0"), b);
    assert_eq!(codec_term::tuple_element(dropped, 1).expect("1"), c);

    let three = small(&mut ctx, 3);
    let dropped = call(&reg, &mut ctx, "erlang", "delete_element", 2, &[three, tuple])
        .expect("a tuple");
    assert_eq!(codec_term::tuple_element(dropped, 1).expect("1"), b);

    let one_tuple = ctx.tuple(&[a]).expect("fits");
    let emptied = call(
        &reg,
        &mut ctx,
        "erlang",
        "delete_element",
        2,
        &[one, one_tuple],
    )
    .expect("a tuple");
    assert_eq!(codec_term::tuple_arity(emptied).expect("a tuple"), 0);
}

#[test]
fn insert_element_accepts_one_past_the_last_position() {
    // ORACLE: `erlang:insert_element(1,{a,b},x)` => `{x,a,b}`;
    //         `erlang:insert_element(3,{a,b},x)` => `{a,b,x}`;
    //         `erlang:insert_element(1,{},x)` => `{x}`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let x = ctx.atom("x").expect("seeded");
    let tuple = ctx.tuple(&[a, b]).expect("fits");

    let one = small(&mut ctx, 1);
    let inserted = call(
        &reg,
        &mut ctx,
        "erlang",
        "insert_element",
        3,
        &[one, tuple, x],
    )
    .expect("a tuple");
    assert_eq!(codec_term::tuple_arity(inserted).expect("a tuple"), 3);
    assert_eq!(codec_term::tuple_element(inserted, 0).expect("0"), x);
    assert_eq!(codec_term::tuple_element(inserted, 1).expect("1"), a);
    assert_eq!(codec_term::tuple_element(inserted, 2).expect("2"), b);

    let three = small(&mut ctx, 3);
    let inserted = call(
        &reg,
        &mut ctx,
        "erlang",
        "insert_element",
        3,
        &[three, tuple, x],
    )
    .expect("a tuple");
    assert_eq!(codec_term::tuple_element(inserted, 2).expect("2"), x);

    let empty = ctx.tuple(&[]).expect("fits");
    let inserted = call(
        &reg,
        &mut ctx,
        "erlang",
        "insert_element",
        3,
        &[one, empty, x],
    )
    .expect("a tuple");
    assert_eq!(codec_term::tuple_arity(inserted).expect("a tuple"), 1);
    assert_eq!(codec_term::tuple_element(inserted, 0).expect("0"), x);
}

#[test]
fn delete_and_insert_element_reject_bad_indexes_and_non_tuples() {
    // ORACLE: index `0`, an index past the bound (`4` on `{a,b,c}` for delete,
    // `4` on `{a,b}` for insert), a non-integer index, a non-tuple and the empty
    // tuple (for delete) are all `badarg`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let two_tuple = ctx.tuple(&[a, b]).expect("fits");
    let three_tuple = ctx.tuple(&[a, b, a]).expect("fits");
    let empty = ctx.tuple(&[]).expect("fits");
    let float = ctx.make_float(1.0).expect("fits");

    let zero = small(&mut ctx, 0);
    let one = small(&mut ctx, 1);
    let three = small(&mut ctx, 3);
    let four = small(&mut ctx, 4);

    for index in [zero, four, float, a] {
        assert!(matches!(
            call(
                &reg,
                &mut ctx,
                "erlang",
                "delete_element",
                2,
                &[index, three_tuple]
            ),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            call(
                &reg,
                &mut ctx,
                "erlang",
                "insert_element",
                3,
                &[index, two_tuple, a]
            ),
            Err(BeamError::BadArg)
        ));
    }
    // Index 3 is past the end of the 2-tuple for delete and insert.
    assert!(matches!(
        call(
            &reg,
            &mut ctx,
            "erlang",
            "delete_element",
            2,
            &[three, two_tuple]
        ),
        Err(BeamError::BadArg)
    ));
    // The empty tuple has no deletable element.
    assert!(matches!(
        call(&reg, &mut ctx, "erlang", "delete_element", 2, &[one, empty]),
        Err(BeamError::BadArg)
    ));
    // Non-tuples.
    let seven = small(&mut ctx, 7);
    for term in [NIL, seven] {
        assert!(matches!(
            call(&reg, &mut ctx, "erlang", "delete_element", 2, &[one, term]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            call(
                &reg,
                &mut ctx,
                "erlang",
                "insert_element",
                3,
                &[one, term, a]
            ),
            Err(BeamError::BadArg)
        ));
    }
}

// ---------------------------------------------------------------------------
// binary:referenced_byte_size/1
// ---------------------------------------------------------------------------

#[test]
fn referenced_byte_size_is_the_byte_size() {
    // ORACLE: `binary:referenced_byte_size(list_to_binary([1,2,3]))` => `3`;
    //         `binary:referenced_byte_size(binary:copy(<<1>>,100))` => `100`;
    //         `binary:referenced_byte_size(binary:copy(<<1>>,100000))` => `100000`.
    // This runtime keeps every binary whole on the heap (there is no off-heap
    // `BinRef`/`SubBits` producer), so the referenced size is the byte size —
    // the C's `br == NULL` branch.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for n in [0usize, 3, 64, 65, 100, 1000] {
        let payload = vec![1u8; n];
        let binary = bin(&mut ctx, &payload);
        let size = call(
            &reg,
            &mut ctx,
            "binary",
            "referenced_byte_size",
            1,
            &[binary],
        )
        .expect("a bitstring");
        assert_eq!(signed_value(size), n as i64, "a {n}-byte binary");
    }

    // A partial trailing byte truncates, exactly like `erlang:size/1`.
    // ORACLE (runtime-built): `binary:referenced_byte_size(<<255:12>>)` truncates.
    let twelve = bitstr(&mut ctx, &[0xFF, 0xF0], 12);
    let size = call(
        &reg,
        &mut ctx,
        "binary",
        "referenced_byte_size",
        1,
        &[twelve],
    )
    .expect("a bitstring");
    assert_eq!(signed_value(size), 1);

    // ORACLE: `binary:referenced_byte_size([1,2])` and `(4)` are `badarg`.
    let list = ints(&mut ctx, &[1, 2]);
    let four = small(&mut ctx, 4);
    for term in [list, four, NIL] {
        assert!(matches!(
            call(
                &reg,
                &mut ctx,
                "binary",
                "referenced_byte_size",
                1,
                &[term]
            ),
            Err(BeamError::BadArg)
        ));
    }
}

// ---------------------------------------------------------------------------
// iolist_to_iovec/1
// ---------------------------------------------------------------------------

#[test]
fn iolist_to_iovec_merges_the_copied_components() {
    // ORACLE: `erlang:iolist_to_iovec([1,2,<<"ab">>,[3,[<<"c">>]],<<"de">>])`
    //   => `[<<1,2,97,98,3,99,100,101>>]` — every copied component merges.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let ab = bin(&mut ctx, b"ab");
    let de = bin(&mut ctx, b"de");
    let c = bin(&mut ctx, b"c");
    let inner = ctx.list(&[c]).expect("fits");
    let three = small(&mut ctx, 3);
    let nested = ctx.list(&[three, inner]).expect("fits");
    let one = small(&mut ctx, 1);
    let two = small(&mut ctx, 2);
    let iolist = ctx.list(&[one, two, ab, nested, de]).expect("fits");

    let iovec =
        call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[iolist]).expect("an iolist");
    assert_eq!(
        len_of(iovec),
        1,
        "the copied run merges into one binary"
    );
    let merged = codec_term::cons_head(iovec).expect("one element");
    assert_eq!(bits_of(merged), (vec![1, 2, 97, 98, 3, 99, 100, 101], 64));
}

#[test]
fn iolist_to_iovec_empty_forms_are_the_empty_list() {
    // ORACLE: `iolist_to_iovec([])`, `(<<>>)`, `([<<>>])` and `([<<>>,<<>>])` all
    //   => `[]` (never `[<<>>]`).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let empty = bin(&mut ctx, &[]);
    let one_empty = ctx.list(&[empty]).expect("fits");
    let two_empty = ctx.list(&[empty, empty]).expect("fits");
    for input in [NIL, empty, one_empty, two_empty] {
        let result = call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[input])
            .expect("a valid iolist");
        assert!(eq(result, NIL, &atoms), "an empty iovec is the empty list");
    }
}

#[test]
fn iolist_to_iovec_accepts_a_bare_binary_and_a_bitstring_tail() {
    // ORACLE: `iolist_to_iovec(<<"abc">>)` => `[<<"abc">>]`;
    //         `iolist_to_iovec([1|<<2>>])` => `[<<1,2>>]`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let abc = bin(&mut ctx, b"abc");
    let iovec =
        call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[abc]).expect("a binary");
    assert_eq!(len_of(iovec), 1);
    let only = codec_term::cons_head(iovec).expect("one element");
    assert_eq!(bits_of(only), (b"abc".to_vec(), 24));

    let two = bin(&mut ctx, &[2]);
    let one = small(&mut ctx, 1);
    let improper = ctx.cons(one, two).expect("fits");
    let iovec = call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[improper])
        .expect("a bitstring tail");
    assert_eq!(len_of(iovec), 1);
    let merged = codec_term::cons_head(iovec).expect("one element");
    assert_eq!(bits_of(merged), (vec![1, 2], 16));
}

#[test]
fn iolist_to_iovec_borrows_a_component_over_the_copy_threshold() {
    // ORACLE (`IOL_SHOULD_COPY`, `erl_iolist.c:39`; threshold
    // `ERL_ONHEAP_BITS_LIMIT * 8` = 512 bytes):
    //   `iolist_to_iovec([binary:copy(<<7>>,512), binary:copy(<<8>>,512)])`
    //     => one 1024-byte binary (copied and merged);
    //   `iolist_to_iovec([binary:copy(<<7>>,513), binary:copy(<<7>>,513)])`
    //     => two 513-byte binaries (borrowed, kept in their own slots);
    //   `iolist_to_iovec([B513,1,2])` => `[B513,<<1,2>>]`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a512 = bin(&mut ctx, &vec![7u8; 512]);
    let b512 = bin(&mut ctx, &vec![8u8; 512]);
    let at_limit = ctx.list(&[a512, b512]).expect("fits");
    let iovec =
        call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[at_limit]).expect("an iolist");
    assert_eq!(len_of(iovec), 1);
    let merged = codec_term::cons_head(iovec).expect("one element");
    assert_eq!(bits_of(merged).1, 1024 * 8);

    let b513 = bin(&mut ctx, &vec![7u8; 513]);
    let b513_again = bin(&mut ctx, &vec![7u8; 513]);
    let over = ctx.list(&[b513, b513_again]).expect("fits");
    let iovec = call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[over]).expect("an iolist");
    assert_eq!(
        len_of(iovec),
        2,
        "a borrowed component keeps its slot"
    );
    let first = codec_term::cons_head(iovec).expect("first");
    assert_eq!(bits_of(first).1, 513 * 8);

    let one = small(&mut ctx, 1);
    let two = small(&mut ctx, 2);
    let mixed = ctx.list(&[b513, one, two]).expect("fits");
    let iovec = call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[mixed]).expect("an iolist");
    let first = codec_term::cons_head(iovec).expect("first");
    assert_eq!(bits_of(first).1, 513 * 8);
    let rest = codec_term::cons_tail(iovec).expect("a cons");
    let second = codec_term::cons_head(rest).expect("second");
    assert_eq!(bits_of(second), (vec![1, 2], 16));
    let tail = codec_term::cons_tail(rest).expect("a cons");
    assert!(eq(tail, NIL, &atoms), "the iovec is exactly two elements");
}

#[test]
fn iolist_to_iovec_rejects_bad_arguments() {
    // ORACLE: a bare integer (`iolist_to_iovec(5)`), a non-byte element (`[300]`,
    // `[-1]`), a non-integer element (`[a]`), an improper list (`[1|2]`) and a
    // **non-byte-aligned** bitstring anywhere in the list (`[<<1:4>>,<<2:4>>]`,
    // `[1,<<2:4>>]`) are all `badarg`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let five = small(&mut ctx, 5);
    assert!(matches!(
        call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[five]),
        Err(BeamError::BadArg)
    ));

    let a = ctx.atom("a").expect("seeded");
    let three_hundred = small(&mut ctx, 300);
    let minus_one = small(&mut ctx, -1);
    for elem in [three_hundred, minus_one, a] {
        let list = ctx.list(&[elem]).expect("fits");
        assert!(matches!(
            call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[list]),
            Err(BeamError::BadArg)
        ));
    }

    let one = small(&mut ctx, 1);
    let two = small(&mut ctx, 2);
    let improper = ctx.cons(one, two).expect("fits");
    assert!(matches!(
        call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[improper]),
        Err(BeamError::BadArg)
    ));

    // Four bits is half a byte: rejected even though the pair would total a byte.
    let odd = bitstr(&mut ctx, &[0b0001_0000], 4);
    assert!(matches!(
        call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[odd]),
        Err(BeamError::BadArg)
    ));
    let even = bitstr(&mut ctx, &[0b0010_0000], 4);
    let pair = ctx.list(&[odd, even]).expect("fits");
    assert!(matches!(
        call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[pair]),
        Err(BeamError::BadArg)
    ));

    let three = bin(&mut ctx, &[1, 2, 3]);
    let mixed = ctx.list(&[three, odd]).expect("fits");
    assert!(matches!(
        call(&reg, &mut ctx, "erlang", "iolist_to_iovec", 1, &[mixed]),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// is_integer/3
// ---------------------------------------------------------------------------

#[test]
fn is_integer_3_is_the_oracle_range_guard() {
    // ORACLE: `is_integer(5,1,10)` => `true`; `is_integer(5,10,1)` => `false`;
    //   `is_integer(5,-100,-100)` => `false`; `is_integer(5,1.0,10)`,
    //   `is_integer(5,1,10.0)` and `is_integer(5,1,inf)` => `badarg`;
    //   `is_integer(a,1,10)` => `false` (the bounds are checked first).
    let reg = default_registry();
    let mut heap = heap();
    let mut atoms = seeded_atoms();
    atoms.seed(["inf"]);
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let five = small(&mut ctx, 5);
    let one = small(&mut ctx, 1);
    let ten = small(&mut ctx, 10);
    let minus = small(&mut ctx, -100);
    let inf = ctx.atom("inf").expect("seeded");
    let a = ctx.atom("a").expect("seeded");
    let float = ctx.make_float(1.0).expect("fits");

    let guard = |ctx: &mut BifContext<'_>, args: &[Eterm]| {
        call(&reg, ctx, "erlang", "is_integer", 3, args)
    };

    for (args, expected) in [
        (vec![five, one, ten], true),
        (vec![five, ten, one], false),
        (vec![five, minus, minus], false),
        // Inclusive bounds.
        (vec![five, five, ten], true),
        (vec![five, one, five], true),
        // A non-integer term is `false`, never an error.
        (vec![a, one, ten], false),
    ] {
        let result = guard(&mut ctx, &args).expect("integer bounds");
        assert_eq!(as_bool(&atoms, result), expected, "is_integer({args:?})");
    }

    for bad in [(float, ten), (one, float), (one, inf)] {
        assert!(matches!(
            guard(&mut ctx, &[five, bad.0, bad.1]),
            Err(BeamError::BadArg)
        ));
        // The bounds are validated first: a non-integer `Term` does not mask them.
        assert!(matches!(
            guard(&mut ctx, &[a, bad.0, bad.1]),
            Err(BeamError::BadArg)
        ));
    }
}

// ---------------------------------------------------------------------------
// display/1 and display_string/2
// ---------------------------------------------------------------------------

#[test]
fn display_writes_the_p_rendering_with_a_crlf_terminator() {
    // ORACLE (`erl -noshell -eval 'erlang:display(…).'`, `| od -c`):
    //   `erlang:display(<<"a b">>)`  => `<<"a b">>\r\n`
    //   `erlang:display([97,98])`    => `"ab"\r\n`
    //   `erlang:display("ström")`    => `"ström"\r\n`; the oracle writes the raw
    //     latin-1 byte `0xF6`, this runtime's printer emits UTF-8 (`0xC3 0xB6`) —
    //     the documented non-ASCII printer divergence (ASCII is byte-exact).
    //   `erlang:display({1,2})`      => `{1,2}\r\n`
    // — each call returns `true`. The `\r\n` is the C's `erts_printf`
    // `write_f_add_cr` newline conversion (`erl_printf.c:135`).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);

        let binary = bin(&mut ctx, b"a b");
        let result = call(&reg, &mut ctx, "erlang", "display", 1, &[binary]).expect("renderable");
        assert!(as_bool(&atoms, result), "display/1 returns true");

        let ab = ints(&mut ctx, &[97, 98]);
        call(&reg, &mut ctx, "erlang", "display", 1, &[ab]).expect("renderable");

        let strom = ints(&mut ctx, &[115, 116, 114, 246, 109]);
        call(&reg, &mut ctx, "erlang", "display", 1, &[strom]).expect("renderable");

        let one = small(&mut ctx, 1);
        let two = small(&mut ctx, 2);
        let tuple = ctx.tuple(&[one, two]).expect("fits");
        call(&reg, &mut ctx, "erlang", "display", 1, &[tuple]).expect("renderable");
    }
    assert_eq!(
        sink.contents(),
        b"<<\"a b\">>\r\n\"ab\"\r\n\"str\xC3\xB6m\"\r\n{1,2}\r\n".to_vec()
    );
}

#[test]
fn display_string_writes_utf8_or_raw_bytes_to_stdout() {
    // ORACLE (`erl -noshell -eval 'erlang:display_string(stdout, …)'`, `| od -c`):
    //   `[97,98,99]`    => `abc`        (a charlist is encoded as UTF-8)
    //   `[]`            => nothing
    //   `<<"bin">>`     => `bin`        (a binary is written byte for byte)
    //   `[246]`         => `0xC3 0xB6`  (U+00F6)
    //   `[16#10FFFF]`   => `0xF4 0x8F 0xBF 0xBF`
    // — each call returns `true` and no line terminator is added.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
        let stdout = ctx.atom("stdout").expect("seeded");

        let payload = ints(&mut ctx, &[97, 98, 99]);
        display_string(&reg, &mut ctx, &atoms, stdout, payload);
        display_string(&reg, &mut ctx, &atoms, stdout, NIL);
        let binary = bin(&mut ctx, b"bin");
        display_string(&reg, &mut ctx, &atoms, stdout, binary);
        let umlaut = ints(&mut ctx, &[246]);
        display_string(&reg, &mut ctx, &atoms, stdout, umlaut);
        let max = ints(&mut ctx, &[0x10FFFF]);
        display_string(&reg, &mut ctx, &atoms, stdout, max);
    }
    assert_eq!(sink.contents(), b"abcbin\xC3\xB6\xF4\x8F\xBF\xBF".to_vec());
}

/// Calls `display_string/2` and asserts the oracle's `true` result.
fn display_string(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    atoms: &AtomTable,
    stream: Eterm,
    payload: Eterm,
) {
    let result = call(reg, ctx, "erlang", "display_string", 2, &[stream, payload])
        .expect("writable");
    assert!(as_bool(atoms, result), "display_string/2 returns true");
}

#[test]
fn display_string_rejects_bad_streams_and_payloads() {
    // ORACLE: a stream that is not an atom (`display_string("stdout","q")`) or is
    // an atom other than `stdout`/`stderr`/`stdin` (`display_string(bogus,"q")`) is
    // `badarg`; so is a payload that is not a charlist or a binary
    // (`display_string(stdout,5)`, `display_string(stdout,[<<1,1>>])` — a binary
    // inside a list — and a non-byte-aligned binary `display_string(stdout,<<1:1>>)`);
    // so is an invalid code point (`[16#D800]`, `[16#110000]`, `[-1]`) and an
    // improper list (`[1|2]`).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);
    let stdout = ctx.atom("stdout").expect("seeded");
    let bogus = ctx.atom("bogus").expect("seeded");
    let charlist = ints(&mut ctx, &[115, 116, 100, 111, 117, 116]);

    for stream in [NIL, bogus, charlist] {
        assert!(matches!(
            call(&reg, &mut ctx, "erlang", "display_string", 2, &[stream, NIL]),
            Err(BeamError::BadArg)
        ));
    }

    let a = ctx.atom("a").expect("seeded");
    let five = small(&mut ctx, 5);
    let two = small(&mut ctx, 2);
    let improper = ctx.cons(five, two).expect("fits");
    let byte = bin(&mut ctx, &[1]);
    let bin_in_list = ctx.list(&[byte]).expect("fits");
    let odd = bitstr(&mut ctx, &[0b1000_0000], 1);
    for payload in [five, a, improper, bin_in_list, odd] {
        assert!(matches!(
            call(
                &reg,
                &mut ctx,
                "erlang",
                "display_string",
                2,
                &[stdout, payload]
            ),
            Err(BeamError::BadArg)
        ));
    }

    for cp in [0x11_0000i64, 0xD800, -1] {
        let payload = ints(&mut ctx, &[cp]);
        assert!(matches!(
            call(
                &reg,
                &mut ctx,
                "erlang",
                "display_string",
                2,
                &[stdout, payload]
            ),
            Err(BeamError::BadArg)
        ));
    }
    assert!(sink.contents().is_empty(), "nothing may be written");
}

#[test]
fn display_string_reports_the_unmodelled_streams_and_binary_payloads() {
    // `stderr`/`stdin` have no seam (the only output sink is the string-valued one
    // `io:format` uses) and a binary payload that is not valid UTF-8 cannot ride a
    // `&str` sink: both are the typed `error:unsupported_bif`, never a write to the
    // wrong stream or a lossy substitution.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut sink = BytesSink::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_output(&mut sink);

    for name in ["stderr", "stdin"] {
        let stream = ctx.atom(name).expect("seeded");
        let err = call(&reg, &mut ctx, "erlang", "display_string", 2, &[stream, NIL])
            .expect_err("no sink for this stream");
        assert!(
            format!("{err:?}").contains("unsupported") || format!("{err:?}").contains("Unsupported"),
            "expected the typed unsupported_bif, got {err:?}"
        );
    }

    let stdout = ctx.atom("stdout").expect("seeded");
    let raw = bin(&mut ctx, &[0xFF, 0x00]);
    assert!(call(&reg, &mut ctx, "erlang", "display_string", 2, &[stdout, raw]).is_err());
    assert!(
        sink.contents().is_empty(),
        "nothing may be written for a rejected payload"
    );
}

// ---------------------------------------------------------------------------
// The textual pid/ref/port conversions
//
//   pid_to_list/1  list_to_pid/1  ref_to_list/1  list_to_ref/1
//   pid_to_binary/1  ref_to_binary/1  port_to_binary/1
//
// The `*_to_list/1` forms render the same `%T` text the `*_to_binary/1` forms
// return as bytes, so each golden below pins the charlist and the binary together.
// ---------------------------------------------------------------------------

/// The bytes of a `*_to_list/1` charlist result.
fn text_bytes(list: Eterm) -> Vec<u8> {
    int_elements(list).into_iter().map(|v| v as u8).collect()
}

/// The bytes of a `*_to_binary/1` result.
fn binary_text(term: Eterm) -> Vec<u8> {
    let (packed, size_bits) = bits_of(term);
    assert_eq!(size_bits % 8, 0, "a byte-aligned binary");
    packed[..size_bits / 8].to_vec()
}

/// The charlist of the ASCII `text`.
fn chars(ctx: &mut BifContext<'_>, text: &str) -> Eterm {
    let elems: Vec<Eterm> = text
        .bytes()
        .map(|b| ctx.int(i64::from(b)).expect("a byte fits"))
        .collect();
    ctx.list(&elems).expect("the list fits")
}

#[test]
fn pid_to_list_renders_the_oracle_text() {
    // ORACLE (OTP 29.1.1 / erts-17.1):
    //   erl -noshell -eval 'io:format("~p~n",[erlang:pid_to_list(erlang:list_to_pid("<0.123.7>"))])'
    //     => "<0.123.7>"
    //   erl -noshell -eval 'io:format("~p~n",[erlang:pid_to_list(self())])'
    //     => "<0.10.0>"
    //   erl -noshell -eval 'io:format("~p~n",[try erlang:pid_to_list(5) catch error:R -> R end])'
    //     => badarg
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(10, 0));

    let pid = ProcessId::new(123, 7).to_eterm();
    let text = call(&reg, &mut ctx, "erlang", "pid_to_list", 1, &[pid]).expect("a pid");
    // The exact list of small integers the oracle prints.
    assert_eq!(int_elements(text), vec![60, 48, 46, 49, 50, 51, 46, 55, 62]);
    assert_eq!(text_bytes(text), b"<0.123.7>");

    // The oracle's own `-noshell` main process is `<0.10.0>`.
    let main = ProcessId::new(10, 0).to_eterm();
    let text = call(&reg, &mut ctx, "erlang", "pid_to_list", 1, &[main]).expect("a pid");
    assert_eq!(text_bytes(text), b"<0.10.0>");

    // A non-pid — an integer, the empty list, a port — is `badarg`.
    let five = small(&mut ctx, 5);
    let port = PortId::new(3).to_eterm();
    for term in [five, NIL, port] {
        assert!(matches!(
            call(&reg, &mut ctx, "erlang", "pid_to_list", 1, &[term]),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn list_to_pid_parses_and_round_trips() {
    // ORACLE:
    //   erlang:list_to_pid(erlang:pid_to_list(self())) =:= self()   => true
    //   erlang:list_to_pid("<0.123.7>")                            => <0.123.7>
    //   erlang:list_to_pid("<0.01.0>")                             => <0.1.0>
    //   erlang:list_to_pid("<0.0.4294967295>")                     => <0.0.4294967295>
    //   and `"bogus"`, `"<0.10.0"` (missing `>`), `[300]` (not a byte),
    //   `<<"<0.1.0>">>` (a binary), `"<0.268435456.0>"` (number past 2^28-1),
    //   `"<0.0.4294967296>"` (serial past 2^32-1), `"<1.1.0>"` (another node) and a
    //   65-character string are all `badarg`.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(10, 0));

    // `list_to_pid(pid_to_list(P)) =:= P`.
    for pid in [
        ProcessId::new(123, 7),
        ProcessId::new(0, 0),
        ProcessId::new(268_435_455, 4_294_967_295),
    ] {
        let text = call(&reg, &mut ctx, "erlang", "pid_to_list", 1, &[pid.to_eterm()])
            .expect("a pid");
        let back = call(&reg, &mut ctx, "erlang", "list_to_pid", 1, &[text]).expect("a pid text");
        assert!(
            eq(back, pid.to_eterm(), &atoms),
            "the round trip must hold for {pid:?}"
        );
    }

    // Leading zeros are accepted, and normalize away.
    let zeros = chars(&mut ctx, "<0.01.0>");
    let back = call(&reg, &mut ctx, "erlang", "list_to_pid", 1, &[zeros]).expect("a pid text");
    assert!(eq(
        back,
        ProcessId::new(1, 0).to_eterm(),
        &atoms
    ));
    let text = call(&reg, &mut ctx, "erlang", "pid_to_list", 1, &[back]).expect("a pid");
    assert_eq!(text_bytes(text), b"<0.1.0>");

    // Every malformed string is `badarg` — never a fabricated pid.
    for bad in [
        "bogus",
        "",
        "<0.10.0",
        "<0.10.0> ",
        "<0.10.0.1>",
        "<1.1.0>",
        "<0.268435456.0>",
        "<0.0.4294967296>",
        "<999999999999999999999999.1.0>",
        "<0..1>",
        "<0.+1.0>",
        "<0.1.0",
    ] {
        let text = chars(&mut ctx, bad);
        assert!(
            matches!(
                call(&reg, &mut ctx, "erlang", "list_to_pid", 1, &[text]),
                Err(BeamError::BadArg)
            ),
            "{bad:?} must be badarg"
        );
    }

    // The argument must be a proper list of byte-sized small integers.
    let not_a_byte = ints(&mut ctx, &[300]);
    let sixty = small(&mut ctx, 60);
    let forty_eight = small(&mut ctx, 48);
    let improper = ctx.cons(sixty, forty_eight).expect("fits");
    let binary = bin(&mut ctx, b"<0.1.0>");
    let five = small(&mut ctx, 5);
    for term in [not_a_byte, improper, binary, five] {
        assert!(matches!(
            call(&reg, &mut ctx, "erlang", "list_to_pid", 1, &[term]),
            Err(BeamError::BadArg)
        ));
    }

    // `intlist_to_buf`'s 64-byte buffer bound (`bif.c:4538-4547`): a 64-character
    // string parses, the 65-character form is `badarg`.
    let mut pid_64 = String::from("<0.");
    pid_64.push_str(&"0".repeat(57));
    pid_64.push_str("1.0>");
    assert_eq!(pid_64.len(), 64);
    let text = chars(&mut ctx, &pid_64);
    assert!(call(&reg, &mut ctx, "erlang", "list_to_pid", 1, &[text]).is_ok());

    let mut pid_65 = String::from("<0.");
    pid_65.push_str(&"0".repeat(58));
    pid_65.push_str("1.0>");
    assert_eq!(pid_65.len(), 65);
    let text = chars(&mut ctx, &pid_65);
    assert!(matches!(
        call(&reg, &mut ctx, "erlang", "list_to_pid", 1, &[text]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn ref_to_list_renders_the_m1_ref_shape() {
    // ORACLE:
    //   erlang:ref_to_list(erlang:list_to_ref("#Ref<0.0.0.7>"))  => "#Ref<0.0.0.7>"
    //   erlang:is_reference(erlang:list_to_ref("#Ref<0.0.0.1>")) => true
    //   erlang:list_to_ref(erlang:ref_to_list(erlang:make_ref())) =:= ... => parses
    //   erlang:ref_to_list(5)                                    => error:badarg
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(10, 0));

    // A reference parsed from the canonical M1 shape renders it back verbatim.
    let text = chars(&mut ctx, "#Ref<0.0.0.7>");
    let reference =
        call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[text]).expect("a ref text");
    let rendered = call(&reg, &mut ctx, "erlang", "ref_to_list", 1, &[reference]).expect("a ref");
    assert_eq!(text_bytes(rendered), b"#Ref<0.0.0.7>");
    assert!(eq(
        call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[rendered]).expect("a ref text"),
        reference,
        &atoms
    ));

    // Leading zeros normalize (`#Ref<00.00.00.01>` => `#Ref<0.0.0.1>` in the oracle).
    let zeros = chars(&mut ctx, "#Ref<00.00.00.01>");
    let reference =
        call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[zeros]).expect("a ref text");
    let rendered = call(&reg, &mut ctx, "erlang", "ref_to_list", 1, &[reference]).expect("a ref");
    assert_eq!(text_bytes(rendered), b"#Ref<0.0.0.1>");

    // A non-reference is `badarg`.
    let five = small(&mut ctx, 5);
    for term in [five, NIL, ProcessId::new(1, 0).to_eterm()] {
        assert!(matches!(
            call(&reg, &mut ctx, "erlang", "ref_to_list", 1, &[term]),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn ref_to_list_renders_a_runtime_built_reference() {
    // The ETS bridge hands its table id out as the crate's `access` ref thing
    // (`src/ets.rs` `new_2`), so a *runtime-built* reference renders the same
    // `#Ref<0.0.0.N>` text and round-trips — exactly the convention
    // `src/print.rs`'s `format_ref` and this module's `ref_text` share.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let name = ctx.atom("bogus").expect("seeded");
    let tid = call(&reg, &mut ctx, "ets", "new", 2, &[name, NIL]).expect("a table is created");
    assert!(
        matches!(
            call(&reg, &mut ctx, "erlang", "is_reference", 1, &[tid]),
            Ok(_)
        ),
        "the tid is a reference"
    );

    let rendered = call(&reg, &mut ctx, "erlang", "ref_to_list", 1, &[tid]).expect("a ref");
    let bytes = text_bytes(rendered);
    assert!(
        bytes.starts_with(b"#Ref<0.0.0.") && bytes.ends_with(b">"),
        "unexpected ref text {bytes:?}"
    );

    let back = call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[rendered]).expect("a ref text");
    assert!(eq(back, tid, &atoms), "the runtime ref round-trips");
}

#[test]
fn list_to_ref_rejects_shapes_the_m1_ref_thing_cannot_hold() {
    // The Milestone-1 ref thing is `[header, node, serial, id]`
    // (`access::make_ref_thing`): one id word, node/serial always `0`. A reference
    // naming another node (`#Ref<1.0.0.1>`) or a non-zero upper ref number
    // (`#Ref<0.1.0.1>`) — both fine in the oracle — is a reference this runtime
    // cannot materialize, so it is `badarg`, never a silently lossy reference.
    // `"bogus"`, `"#Ref<0.0.0>"` (one number short), `"#Ref<0.0.0.1.2>"` (one too
    // many) and a 72-character string are likewise `badarg`; the 71-character form
    // is within `intlist_to_buf`'s buffer (`bif.c:4727-4732`).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for bad in [
        "bogus",
        "",
        "#Ref<0.0.0>",
        "#Ref<0.0.0.1.2>",
        "#Ref<1.0.0.1>",
        "#Ref<0.1.0.1>",
        "#Ref<0.0.1.1>",
        "#Ref<0.0.0.1",
        "#Ref<0.0.0.1>>",
    ] {
        let text = chars(&mut ctx, bad);
        assert!(
            matches!(
                call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[text]),
                Err(BeamError::BadArg)
            ),
            "{bad:?} must be badarg"
        );
    }

    let binary = bin(&mut ctx, b"#Ref<0.0.0.1>");
    let five = small(&mut ctx, 5);
    for term in [binary, five, NIL] {
        assert!(matches!(
            call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[term]),
            Err(BeamError::BadArg)
        ));
    }

    for (text, len) in [("71", 71usize), ("72", 72usize)] {
        let mut s = String::from("#Ref<0.");
        s.push_str(&"0".repeat(len - 13));
        s.push_str("1.0.1>");
        assert_eq!(s.len(), len);
        let t = chars(&mut ctx, &s);
        let result = call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[t]);
        assert!(
            matches!(result, Err(BeamError::BadArg)),
            "{text}-character string with a non-zero upper ref number must be badarg"
        );
    }

    // The buffer bound itself: a 71-character string in the representable shape
    // parses, the 72-character form is `badarg` (`bif.c:4727-4732`; oracle-verified
    // at the 71/72 boundary).
    let mut fits = String::from("#Ref<0.0.0.");
    fits.push_str(&"0".repeat(58));
    fits.push_str("1>");
    assert_eq!(fits.len(), 71);
    let t = chars(&mut ctx, &fits);
    let reference = call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[t]).expect("a ref text");
    let rendered = call(&reg, &mut ctx, "erlang", "ref_to_list", 1, &[reference]).expect("a ref");
    assert_eq!(text_bytes(rendered), b"#Ref<0.0.0.1>");

    let mut too_long = String::from("#Ref<0.0.0.");
    too_long.push_str(&"0".repeat(59));
    too_long.push_str("1>");
    assert_eq!(too_long.len(), 72);
    let t = chars(&mut ctx, &too_long);
    assert!(matches!(
        call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[t]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn the_to_binary_variants_return_the_list_bytes() {
    // OTP-30 additions (`bif.c:4879`/`:4886`/`:4893`). The 29.1.1 oracle does not
    // export them (`erlang:function_exported(erlang,pid_to_binary,1)` => `false`),
    // so the bytes are pinned to the C instead: `term2binary_dsprintf`
    // (`bif.c:4863`) renders them through the same `"%T"` format as
    // `term2list_dsprintf`, so the binary is byte-identical to the `*_to_list/1`
    // charlist.
    let reg = default_registry();
    let mut heap = heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(10, 0));

    let pid = ProcessId::new(123, 7).to_eterm();
    let as_list = call(&reg, &mut ctx, "erlang", "pid_to_list", 1, &[pid]).expect("a pid");
    let as_binary = call(&reg, &mut ctx, "erlang", "pid_to_binary", 1, &[pid]).expect("a pid");
    assert_eq!(binary_text(as_binary), b"<0.123.7>");
    assert_eq!(binary_text(as_binary), text_bytes(as_list));

    let text = chars(&mut ctx, "#Ref<0.0.0.7>");
    let reference =
        call(&reg, &mut ctx, "erlang", "list_to_ref", 1, &[text]).expect("a ref text");
    let as_list = call(&reg, &mut ctx, "erlang", "ref_to_list", 1, &[reference]).expect("a ref");
    let as_binary =
        call(&reg, &mut ctx, "erlang", "ref_to_binary", 1, &[reference]).expect("a ref");
    assert_eq!(binary_text(as_binary), b"#Ref<0.0.0.7>");
    assert_eq!(binary_text(as_binary), text_bytes(as_list));

    let port = PortId::new(3).to_eterm();
    let as_binary = call(&reg, &mut ctx, "erlang", "port_to_binary", 1, &[port]).expect("a port");
    // The oracle's `#Port<0.3>` for `erlang:port_to_list(open_port({spawn,"true"},[]))`.
    assert_eq!(binary_text(as_binary), b"#Port<0.3>");
    assert!(matches!(
        call(
            &reg,
            &mut ctx,
            "erlang",
            "port_to_list",
            1,
            &[port]
        ),
        Ok(_)
    ));

    // Each variant still type-checks its one argument.
    let five = small(&mut ctx, 5);
    for (name, term) in [
        ("pid_to_binary", five),
        ("ref_to_binary", five),
        ("port_to_binary", five),
        ("port_to_binary", pid),
    ] {
        assert!(matches!(
            call(&reg, &mut ctx, "erlang", name, 1, &[term]),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn the_to_binary_variants_are_absent_from_the_otp29_oracle() {
    // Documents *why* the three `*_to_binary/1` goldens above are pinned to the C
    // rather than to the live `erl`: they are OTP-30 additions the installed
    // 29.1.1 oracle does not export. Skips loudly when the oracle is unavailable.
    let statement = concat!(
        "io:format(\"~p~n\",[erlang:function_exported(erlang,pid_to_binary,1)]),",
        "io:format(\"~p~n\",[erlang:function_exported(erlang,port_to_binary,1)]),",
        "io:format(\"~p~n\",[erlang:function_exported(erlang,ref_to_binary,1)]),",
        "halt()."
    );
    let Some(stdout) = oracle_eval(statement) else {
        return;
    };
    assert_eq!(
        stdout,
        concat!("false\n", "false\n", "false\n"),
        "the oracle now exports the OTP-30 *_to_binary/1 forms; pin them live"
    );
}

// ---------------------------------------------------------------------------
// Live oracle cross-check
// ---------------------------------------------------------------------------/// The goldens the other tests assert, re-run under the live system `erl`.
///
/// One statement per invocation is the most robust shape, so the whole block is a
/// single `-eval` with `io:format` calls; the oracle's stdout must match exactly.
/// Skips loudly when `erl` is unavailable.
#[test]
fn the_shared_goldens_match_the_live_oracle() {
    let statement = concat!(
        "io:format(\"~w~n\",[erlang:bitstring_to_list(<<1,2,3:4>>)]),",
        "io:format(\"~w~n\",[erlang:delete_element(2,{a,b,c})]),",
        "io:format(\"~w~n\",[erlang:insert_element(3,{a,b},x)]),",
        "io:format(\"~w~n\",[binary:referenced_byte_size(binary:copy(<<1>>,65))]),",
        "io:format(\"~w~n\",[erlang:iolist_to_iovec([1,2,<<\"ab\">>])]),",
        "io:format(\"~w~n\",[erlang:is_integer(5,1,10)]),",
        "io:format(\"~w~n\",[try erlang:is_integer(5,1,10.0) catch error:R -> R end]),",
        "halt()."
    );
    let Some(stdout) = oracle_eval(statement) else {
        return;
    };
    assert_eq!(
        stdout,
        concat!(
            "[1,2,<<3:4>>]\n",
            "{a,c}\n",
            "{a,b,x}\n",
            "65\n",
            "[<<1,2,97,98>>]\n",
            "true\n",
            "badarg\n",
        ),
        "the live oracle disagrees with this test file's goldens"
    );
}

/// The textual-conversion goldens, re-run under the live system `erl`.
///
/// Probes (OTP 29.1.1 / erts-17.1), one statement per `io:format`:
///
/// ```text
/// erl -noshell -eval 'io:format("~p~n",[erlang:pid_to_list(erlang:list_to_pid("<0.123.7>"))])'
///   => "<0.123.7>"
/// erl -noshell -eval 'io:format("~p~n",[erlang:list_to_pid("<0.123.7>")])'
///   => <0.123.7>
/// erl -noshell -eval 'io:format("~p~n",[erlang:ref_to_list(erlang:list_to_ref("#Ref<0.0.0.7>"))])'
///   => "#Ref<0.0.0.7>"
/// erl -noshell -eval 'io:format("~p~n",[try erlang:list_to_pid("bogus") catch error:R -> R end])'
///   => badarg
/// erl -noshell -eval 'io:format("~p~n",[try erlang:list_to_ref("#Ref<0.0.0") catch error:R -> R end])'
///   => badarg
/// ```
///
/// Skips loudly when `erl` is unavailable.
#[test]
fn the_conversion_goldens_match_the_live_oracle() {
    let statement = concat!(
        "io:format(\"~p~n\",[erlang:pid_to_list(erlang:list_to_pid(\"<0.123.7>\"))]),",
        "io:format(\"~p~n\",[erlang:list_to_pid(\"<0.123.7>\")]),",
        "io:format(\"~p~n\",[erlang:ref_to_list(erlang:list_to_ref(\"#Ref<0.0.0.7>\"))]),",
        "io:format(\"~p~n\",[try erlang:list_to_pid(\"bogus\") catch error:R -> R end]),",
        "io:format(\"~p~n\",[try erlang:list_to_ref(\"#Ref<0.0.0\") catch error:R -> R end]),",
        "halt()."
    );
    let Some(stdout) = oracle_eval(statement) else {
        return;
    };
    assert_eq!(
        stdout,
        concat!(
            "\"<0.123.7>\"\n",
            "<0.123.7>\n",
            "\"#Ref<0.0.0.7>\"\n",
            "badarg\n",
            "badarg\n",
        ),
        "the live oracle disagrees with this test file's conversion goldens"
    );
}

// ---------------------------------------------------------------------------
// external_size/1,2
// ---------------------------------------------------------------------------

/// The atom table the `external_size` tests use: the `term_to_binary/2` option atoms
/// plus the `nonode@nohost` local-node fallback (so an internal pid has a wire image).
fn external_size_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "a",
        "b",
        "c",
        "hello",
        "compressed",
        "minor_version",
        "bogus",
        "nonode@nohost",
    ]);
    atoms
}

/// The byte length of a binary term the tests built.
fn binary_len(term: Eterm) -> i64 {
    let (_, offset, size_bits) = codec_term::bitstring_bytes(term).expect("a binary");
    assert_eq!(offset % 8, 0, "the image is byte aligned");
    assert_eq!(size_bits % 8, 0, "the image is a whole number of bytes");
    (size_bits / 8) as i64
}

/// The small integer `erlang:external_size/1,2` answered.
fn reported_size(term: Eterm) -> i64 {
    assert!(is_small(term), "a size must be a small integer, got {term:?}");
    signed_value(term)
}

/// `erlang:external_size/1`.
fn external_size_1(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> i64 {
    reported_size(call(reg, ctx, "erlang", "external_size", 1, &[term]).expect("encodable"))
}

/// `erlang:external_size/2`.
fn external_size_2(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    term: Eterm,
    options: Eterm,
) -> i64 {
    reported_size(
        call(reg, ctx, "erlang", "external_size", 2, &[term, options]).expect("encodable"),
    )
}

/// `byte_size(erlang:term_to_binary(Term))` — the image `external_size/1` must agree
/// with for these terms (the documented `Size1 =< Size2` contract,
/// `erts/preloaded/src/erlang.erl:2658-2679`).
fn image_len(reg: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> i64 {
    binary_len(call(reg, ctx, "erlang", "term_to_binary", 1, &[term]).expect("encodable"))
}

/// The proper list `items` on the context heap (an option list).
fn option_list(ctx: &mut BifContext<'_>, items: &[Eterm]) -> Eterm {
    ctx.list(items).expect("the option list fits")
}

#[test]
fn external_size_matches_the_oracle_for_the_primitive_shapes() {
    // ORACLE (OTP 29.1.1 / erts-17.1), one probe per shape:
    //   erlang:external_size(5)         => 3
    //   erlang:external_size(a)         => 4
    //   erlang:external_size(hello)     => 8
    //   erlang:external_size([1,2,3])   => 7
    //   erlang:external_size({a,b,c})   => 12
    //   erlang:external_size(1.5)       => 10
    //   erlang:external_size(1 bsl 100) => 17
    let reg = default_registry();
    let mut heap = heap();
    let atoms = external_size_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let five = small(&mut ctx, 5);
    let a = ctx.atom("a").expect("seeded");
    let hello = ctx.atom("hello").expect("seeded");
    let list = ints(&mut ctx, &[1, 2, 3]);
    let b = ctx.atom("b").expect("seeded");
    let c = ctx.atom("c").expect("seeded");
    let tuple = ctx.tuple(&[a, b, c]).expect("fits");
    let float = ctx.make_float(1.5).expect("fits");
    // `1 bsl 100` as the two little-endian 64-bit digits `[0, 1 << 36]`.
    let big = codec_term::make_big(ctx.heap(), false, &[0, 1 << 36]).expect("2^100 fits");

    for (term, expected) in [
        (five, 3),
        (a, 4),
        (hello, 8),
        (list, 7),
        (tuple, 12),
        (float, 10),
        (big, 17),
    ] {
        assert_eq!(
            external_size_1(&reg, &mut ctx, term),
            expected,
            "external_size/1 disagrees"
        );
        assert_eq!(
            image_len(&reg, &mut ctx, term),
            expected,
            "byte_size(term_to_binary/1) disagrees"
        );
    }
}

#[test]
fn external_size_2_validates_and_applies_the_term_to_binary_options() {
    // ORACLE (OTP 29.1.1 / erts-17.1):
    //   erlang:external_size(a)                       => 4
    //   erlang:external_size(a,[{minor_version,0}])   => 5   (ATOM_EXT)
    //   erlang:external_size(a,[{minor_version,1}])   => 5   (ATOM_EXT)
    //   erlang:external_size(1.5,[{minor_version,0}]) => 33  (legacy FLOAT_EXT)
    //   erlang:external_size(a,[compressed])          => 4   (level never applied)
    //   erlang:external_size(a,[{compressed,9}])      => 4
    //   erlang:external_size(5,[bogus])               => badarg
    //   erlang:external_size(5,[{minor_version,3}])   => badarg
    //   erlang:external_size(5,[{compressed,10}])     => badarg
    //   erlang:external_size(5,[a|5])                 => badarg (improper list)
    //   erlang:external_size(5,5)                     => badarg (not a list)
    let reg = default_registry();
    let mut heap = heap();
    let atoms = external_size_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let compressed = ctx.atom("compressed").expect("seeded");
    let minor_version = ctx.atom("minor_version").expect("seeded");
    let bogus = ctx.atom("bogus").expect("seeded");
    let zero = small(&mut ctx, 0);
    let one = small(&mut ctx, 1);
    let three = small(&mut ctx, 3);
    let five = small(&mut ctx, 5);
    let nine = small(&mut ctx, 9);
    let ten = small(&mut ctx, 10);
    let float = ctx.make_float(1.5).expect("fits");

    let mv0 = ctx.tuple(&[minor_version, zero]).expect("fits");
    let mv1 = ctx.tuple(&[minor_version, one]).expect("fits");
    let mv3 = ctx.tuple(&[minor_version, three]).expect("fits");
    let lvl9 = ctx.tuple(&[compressed, nine]).expect("fits");
    let lvl10 = ctx.tuple(&[compressed, ten]).expect("fits");

    let mv0_opts = option_list(&mut ctx, &[mv0]);
    let mv1_opts = option_list(&mut ctx, &[mv1]);
    let mv3_opts = option_list(&mut ctx, &[mv3]);
    let compressed_opts = option_list(&mut ctx, &[compressed]);
    let lvl9_opts = option_list(&mut ctx, &[lvl9]);
    let lvl10_opts = option_list(&mut ctx, &[lvl10]);
    let bogus_opts = option_list(&mut ctx, &[bogus]);
    let improper = ctx.cons(a, five).expect("fits");

    assert_eq!(external_size_1(&reg, &mut ctx, a), 4, "the default tag set");
    assert_eq!(external_size_2(&reg, &mut ctx, a, mv0_opts), 5, "minor_version 0");
    assert_eq!(external_size_2(&reg, &mut ctx, a, mv1_opts), 5, "minor_version 1");
    assert_eq!(
        external_size_2(&reg, &mut ctx, float, mv0_opts),
        33,
        "the legacy FLOAT_EXT of minor_version 0"
    );
    // `compressed` is validated through the *same* list, but the size probe never
    // deflates the body (see `the_external_size_of_a_compressible_term_...`).
    assert_eq!(external_size_2(&reg, &mut ctx, a, compressed_opts), 4, "compressed");
    assert_eq!(external_size_2(&reg, &mut ctx, a, lvl9_opts), 4, "{{compressed, 9}}");

    // The same options as `term_to_binary/2`: the size is that image's byte length.
    let mv0_image =
        call(&reg, &mut ctx, "erlang", "term_to_binary", 2, &[a, mv0_opts]).expect("encodable");
    assert_eq!(binary_len(mv0_image), 5, "term_to_binary/2 with minor_version 0");

    for bad in [bogus_opts, mv3_opts, lvl10_opts, improper, five] {
        assert!(
            matches!(
                call(&reg, &mut ctx, "erlang", "external_size", 2, &[five, bad]),
                Err(BeamError::BadArg)
            ),
            "a rejected option list must be badarg"
        );
    }
}

#[test]
fn external_size_of_a_compressible_term_is_the_uncompressed_image() {
    // ORACLE (OTP 29.1.1 / erts-17.1) for `T = lists:duplicate(500,a)`:
    //   byte_size(term_to_binary(T))              => 1507
    //   erlang:external_size(T)                   => 1507
    //   byte_size(term_to_binary(T,[compressed])) => 33
    //   erlang:external_size(T,[compressed])      => 1507
    // and for the 500-byte string `lists:duplicate(500,$a)`: 504 / 504.
    //
    // The oracle's `external_size_2` passes only the *flags* to its size probe
    // (`external.c:2221-2246`, `external.c:736-743`), which never deflates; the
    // documented contract is the upper bound
    // `byte_size(term_to_binary(T, Opts)) =< erlang:external_size(T, Opts)`
    // (`erts/preloaded/src/erlang.erl:2687-2710`).
    let reg = default_registry();
    let mut heap = heap();
    let atoms = external_size_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let a = ctx.atom("a").expect("seeded");
    let compressed = ctx.atom("compressed").expect("seeded");
    let elements = vec![a; 500];
    let list = ctx.list(&elements).expect("the list fits");
    let compressed_opts = option_list(&mut ctx, &[compressed]);

    assert_eq!(external_size_1(&reg, &mut ctx, list), 1507, "external_size/1");
    assert_eq!(
        external_size_2(&reg, &mut ctx, list, compressed_opts),
        1507,
        "the compression level does not shrink the size"
    );
    assert_eq!(image_len(&reg, &mut ctx, list), 1507, "the plain image");

    // The byte-list form is a `STRING_EXT` image the oracle sizes at 504 either way.
    let chars: Vec<Eterm> = (0..500).map(|_| ctx.int(97).expect("fits")).collect();
    let string = ctx.list(&chars).expect("the list fits");
    assert_eq!(external_size_1(&reg, &mut ctx, string), 504);
    assert_eq!(external_size_2(&reg, &mut ctx, string, compressed_opts), 504);
}

#[test]
fn external_size_of_an_unencodable_term_is_the_typed_gap() {
    // With no `LocalNode` hook and no `nonode@nohost` atom, an internal pid has no wire
    // image: the codec's typed `unsupported_external_term`, never a fabricated number.
    let reg = default_registry();
    let mut gap_heap = heap();
    let mut gap_atoms = AtomTable::new();
    gap_atoms.seed(["a", "b"].iter().copied());
    let mut ctx = BifContext::new(&mut gap_heap, &gap_atoms, ProcessId::new(1, 0));

    let self_pid = ctx.self_pid().to_eterm();
    let nested = ctx.tuple(&[self_pid]).expect("fits");
    for term in [self_pid, nested] {
        let err = call(&reg, &mut ctx, "erlang", "external_size", 1, &[term]).unwrap_err();
        assert!(is_unsupported(&err), "expected a typed gap, got {err:?}");
        let err = call(&reg, &mut ctx, "erlang", "external_size", 2, &[term, NIL]).unwrap_err();
        assert!(is_unsupported(&err), "expected a typed gap, got {err:?}");
    }

    // The very same term sizes once the local node is nameable — `self()` is 29 bytes
    // in the oracle (`byte_size(term_to_binary(self()))`).
    let mut nameable_heap = heap();
    let nameable_atoms = external_size_atoms();
    let mut ctx = BifContext::new(&mut nameable_heap, &nameable_atoms, ProcessId::new(1, 0));
    let self_pid = ctx.self_pid().to_eterm();
    assert_eq!(external_size_1(&reg, &mut ctx, self_pid), 29);
    assert_eq!(image_len(&reg, &mut ctx, self_pid), 29);
}

/// The `external_size/1,2` goldens, re-run under the live system `erl`.
///
/// Probes (OTP 29.1.1 / erts-17.1), one statement per `io:format`:
///
/// ```text
/// erlang:external_size(5)                       => 3
/// erlang:external_size(a)                       => 4
/// erlang:external_size(hello)                   => 8
/// erlang:external_size([1,2,3])                 => 7
/// erlang:external_size({a,b,c})                 => 12
/// erlang:external_size(1.5)                     => 10
/// erlang:external_size(1 bsl 100)               => 17
/// erlang:external_size(a,[{minor_version,0}])   => 5
/// erlang:external_size(1.5,[{minor_version,0}]) => 33
/// erlang:external_size(lists:duplicate(500,a),[compressed]) => 1507
/// erlang:external_size(lists:duplicate(500,a))
///     =:= byte_size(term_to_binary(lists:duplicate(500,a))) => true
/// erlang:external_size(5,[bogus])               => badarg
/// erlang:external_size(5,[{compressed,10}])     => badarg
/// ```
///
/// Skips loudly when `erl` is unavailable.
#[test]
fn the_external_size_goldens_match_the_live_oracle() {
    let statement = concat!(
        "io:format(\"~p~n\",[erlang:external_size(5)]),",
        "io:format(\"~p~n\",[erlang:external_size(a)]),",
        "io:format(\"~p~n\",[erlang:external_size(hello)]),",
        "io:format(\"~p~n\",[erlang:external_size([1,2,3])]),",
        "io:format(\"~p~n\",[erlang:external_size({a,b,c})]),",
        "io:format(\"~p~n\",[erlang:external_size(1.5)]),",
        "io:format(\"~p~n\",[erlang:external_size(1 bsl 100)]),",
        "io:format(\"~p~n\",[erlang:external_size(a,[{minor_version,0}])]),",
        "io:format(\"~p~n\",[erlang:external_size(1.5,[{minor_version,0}])]),",
        "io:format(\"~p~n\",[erlang:external_size(lists:duplicate(500,a),[compressed])]),",
        "io:format(\"~p~n\",[erlang:external_size(lists:duplicate(500,a)) =:= \
         byte_size(term_to_binary(lists:duplicate(500,a)))]),",
        "io:format(\"~p~n\",[try erlang:external_size(5,[bogus]) catch error:R1 -> R1 end]),",
        "io:format(\"~p~n\",[try erlang:external_size(5,[{compressed,10}]) \
         catch error:R2 -> R2 end]),",
        "halt()."
    );
    let Some(stdout) = oracle_eval(statement) else {
        return;
    };
    assert_eq!(
        stdout,
        concat!(
            "3\n",
            "4\n",
            "8\n",
            "7\n",
            "12\n",
            "10\n",
            "17\n",
            "5\n",
            "33\n",
            "1507\n",
            "true\n",
            "badarg\n",
            "badarg\n",
        ),
        "the live oracle disagrees with this test file's external_size goldens"
    );
}
