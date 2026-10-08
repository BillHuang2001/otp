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

//! Integration tests for the `erlang` **iolist / bitstring** BIFs —
//! `iolist_to_binary/1`, `iolist_size/1`, `list_to_bitstring/1` and
//! `split_binary/2` — driven through
//! [`BifRegistry::call`](beam_bifs::BifRegistry::call) (the crate's public seam).
//!
//! The expectations are pinned to the OTP 29 / erts-17.1 oracle, e.g.
//!
//! ```text
//! erl -noshell -eval 'io:format("~p~n",[iolist_to_binary([<<"ab">>,[99],$d])]), halt().'
//!   % <<"abcd">>
//! erl -noshell -eval 'io:format("~p~n",[catch iolist_to_binary([a])]), halt().'   % badarg
//! erl -noshell -eval 'io:format("~p~n",[iolist_size([<<"ab">>,[99]])]), halt().'  % 3
//! erl -noshell -eval 'io:format("~p~n",[list_to_bitstring([<<1:3>>])]), halt().'
//!   % <<1:3>>
//! erl -noshell -eval 'io:format("~p~n",[list_to_bitstring([<<1:3>>,<<1:5>>])]), halt().'
//!   % <<"!">>
//! erl -noshell -eval 'io:format("~p~n",[split_binary(<<"abcd">>,2)]), halt().'
//!   % {<<"ab">>,<<"cd">>}
//! erl -noshell -eval 'io:format("~p~n",[catch split_binary(<<"abcd">>,5)]), halt().' % badarg
//! ```
//!
//! Bitstring arguments/results are built and read with the sibling **safe**
//! `beam_codec::term` helpers (`make_binary`/`make_bitstring`/`bitstring_bytes`),
//! because the crate's own bitstring helpers are private.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext};
use beam_codec::term as codec_term;
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{compare::eq, signed_value, AtomTable, Eterm, NIL};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a", "b"]);
    atoms
}

/// The list `[V0, V1, …]` of the small integers `values`.
fn ints(ctx: &mut BifContext<'_>, values: &[i64]) -> Eterm {
    let elems: Vec<Eterm> = values
        .iter()
        .map(|&v| ctx.int(v).expect("a small int fits"))
        .collect();
    ctx.list(&elems).expect("the list fits")
}

/// The `(packed bytes, bit length)` of a bitstring term.
///
/// Re-packs the `size_bits` bits that start at the term's bit offset into a fresh
/// MSB-first buffer, so the result is canonical (`<<1:3>>` ⇒ `([0b1000_0000], 3)`)
/// whatever representation the term uses.
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

#[test]
fn iolist_to_binary_flattens_the_oracle_example() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    let ab = codec_term::make_binary(&mut heap, b"ab").expect("fits");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `iolist_to_binary([<<"ab">>,[99],$d])` ⇒ `<<"abcd">>` — a binary leaf, a
    // nested byte list and a bare byte.
    let c_list = ints(&mut ctx, &[99]);
    let d = ctx.int(100).expect("fits");
    let iolist = ctx.list(&[ab, c_list, d]).expect("fits");
    let result = reg
        .call("erlang", "iolist_to_binary", 1, &mut ctx, &[iolist])
        .expect("iolist_to_binary/1");
    assert_eq!(bits_of(result), (b"abcd".to_vec(), 32));

    // The same term as flattening the equivalent flat byte list.
    let flat = ints(&mut ctx, &[97, 98, 99, 100]);
    let expected = reg
        .call("binary", "list_to_bin", 1, &mut ctx, &[flat])
        .expect("binary:list_to_bin/1");
    assert!(eq(result, expected, &atoms));

    // `[]` is the empty binary and deeper nesting is flattened.
    let empty = ctx.list(&[]).expect("fits");
    let result = reg
        .call("erlang", "iolist_to_binary", 1, &mut ctx, &[empty])
        .expect("iolist_to_binary/1 of []");
    assert_eq!(bits_of(result), (Vec::new(), 0));
    let inner = ints(&mut ctx, &[1, 2]);
    let inner_list = ctx.list(&[inner]).expect("fits");
    let three = ctx.int(3).expect("fits");
    let nested = ctx.list(&[inner_list, three]).expect("fits");
    let result = reg
        .call("erlang", "iolist_to_binary", 1, &mut ctx, &[nested])
        .expect("iolist_to_binary/1 of a nested iolist");
    assert_eq!(bits_of(result), (vec![1, 2, 3], 24));
}

#[test]
fn iolist_to_binary_accepts_a_bare_binary_and_a_bitstring_tail() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    let bare = codec_term::make_binary(&mut heap, &[1, 2, 3]).expect("fits");
    // The `<<3>>` tail of the improper cons below is materialized *before* the context
    // takes its borrow of `heap` (a `BifContext` holds `&mut Heap` for its lifetime).
    let tail = codec_term::make_binary(&mut heap, &[3]).expect("fits");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `iolist_to_binary(<<1,2,3>>)` ⇒ `<<1,2,3>>` — a bare **byte-aligned** binary
    // is a valid argument for `iolist_to_binary/1` (unlike `list_to_bin/1`).
    let result = reg
        .call("erlang", "iolist_to_binary", 1, &mut ctx, &[bare])
        .expect("iolist_to_binary/1 of a bare binary");
    assert_eq!(bits_of(result), (vec![1, 2, 3], 24));

    // `iolist_to_binary([1,2|<<3>>])` ⇒ `<<1,2,3>>` — an improper cons tail that is
    // a bitstring is a valid iolist (`erl_iolist.c`'s cons-tail rule).
    let two = ctx.int(2).expect("fits");
    let improper = ctx.cons(two, tail).expect("fits");
    let one = ctx.int(1).expect("fits");
    let improper = ctx.cons(one, improper).expect("fits");
    let result = reg
        .call("erlang", "iolist_to_binary", 1, &mut ctx, &[improper])
        .expect("iolist_to_binary/1 with a bitstring tail");
    assert_eq!(bits_of(result), (vec![1, 2, 3], 24));
}

#[test]
fn iolist_to_binary_rejects_invalid_leaves_and_bare_terms() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    let three_bits = codec_term::make_bitstring(&mut heap, &[0b1000_0000], 3).expect("fits");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `iolist_to_binary([256])`, `([-1])`, `([a])` are `badarg` (a leaf must be a
    // byte or a bitstring).
    for values in [vec![256i64], vec![-1]] {
        let bad = ints(&mut ctx, &values);
        assert!(
            matches!(
                reg.call("erlang", "iolist_to_binary", 1, &mut ctx, &[bad]),
                Err(BeamError::BadArg)
            ),
            "iolist_to_binary({values:?}) must be badarg"
        );
    }
    let a = ctx.atom("a").expect("seeded");
    let with_atom = ctx.list(&[a]).expect("fits");
    assert!(matches!(
        reg.call("erlang", "iolist_to_binary", 1, &mut ctx, &[with_atom]),
        Err(BeamError::BadArg)
    ));

    // A **non-byte-aligned** bitstring leaf is `badarg` for the `Byte` unit
    // (`iolist_to_binary([<<1:3>>])`), and so is a bare non-byte-aligned bitstring.
    let bits_leaf = ctx.list(&[three_bits]).expect("fits");
    assert!(matches!(
        reg.call("erlang", "iolist_to_binary", 1, &mut ctx, &[bits_leaf]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("erlang", "iolist_to_binary", 1, &mut ctx, &[three_bits]),
        Err(BeamError::BadArg)
    ));

    // A bare integer or atom is not an iolist, and an improper tail that is not a
    // bitstring is `badarg` (`[1|2]`).
    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");
    let improper = ctx.cons(one, two).expect("fits");
    for bad in [one, a, improper] {
        assert!(matches!(
            reg.call("erlang", "iolist_to_binary", 1, &mut ctx, &[bad]),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn iolist_size_counts_bytes_and_shares_the_iolist_rules() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    let ab = codec_term::make_binary(&mut heap, b"ab").expect("fits");
    let three_bin = codec_term::make_binary(&mut heap, &[3]).expect("fits");
    let bare = codec_term::make_binary(&mut heap, &[1, 2, 3]).expect("fits");
    let three_bits = codec_term::make_bitstring(&mut heap, &[0b1000_0000], 3).expect("fits");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `iolist_size([<<"ab">>,[99]])` ⇒ `3`.
    let c_list = ints(&mut ctx, &[99]);
    let iolist = ctx.list(&[ab, c_list]).expect("fits");
    let result = reg
        .call("erlang", "iolist_size", 1, &mut ctx, &[iolist])
        .expect("iolist_size/1");
    assert_eq!(signed_value(result), 3);

    // `iolist_size([1,2|<<3>>])` ⇒ `3`; `iolist_size(<<1,2,3>>)` ⇒ `3`;
    // `iolist_size([])` ⇒ `0`.
    let two = ctx.int(2).expect("fits");
    let improper = ctx.cons(two, three_bin).expect("fits");
    let one = ctx.int(1).expect("fits");
    let improper = ctx.cons(one, improper).expect("fits");
    let result = reg
        .call("erlang", "iolist_size", 1, &mut ctx, &[improper])
        .expect("iolist_size/1 with a bitstring tail");
    assert_eq!(signed_value(result), 3);
    let result = reg
        .call("erlang", "iolist_size", 1, &mut ctx, &[bare])
        .expect("iolist_size/1 of a bare binary");
    assert_eq!(signed_value(result), 3);
    let empty = ctx.list(&[]).expect("fits");
    let result = reg
        .call("erlang", "iolist_size", 1, &mut ctx, &[empty])
        .expect("iolist_size/1 of []");
    assert_eq!(signed_value(result), 0);

    // The same `badarg` set as `iolist_to_binary/1`: a bad byte, a non-byte-aligned
    // bitstring leaf, a bare non-byte-aligned bitstring and a bare atom.
    let over = ints(&mut ctx, &[256]);
    let bits_leaf = ctx.list(&[three_bits]).expect("fits");
    let a = ctx.atom("a").expect("seeded");
    for bad in [over, bits_leaf, three_bits, a] {
        assert!(matches!(
            reg.call("erlang", "iolist_size", 1, &mut ctx, &[bad]),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn list_to_bitstring_concatenates_bits() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    // `<<1:3>>` is the single byte `0b0010_0000` with a bit size of 3; `<<1:5>>` is
    // `0b0000_1000` with a bit size of 5.
    let three = codec_term::make_bitstring(&mut heap, &[0b0010_0000], 3).expect("fits");
    let five = codec_term::make_bitstring(&mut heap, &[0b0000_1000], 5).expect("fits");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `list_to_bitstring([<<1:3>>])` ⇒ `<<1:3>>` (a single bitstring element).
    let single = ctx.list(&[three]).expect("fits");
    let result = reg
        .call("erlang", "list_to_bitstring", 1, &mut ctx, &[single])
        .expect("list_to_bitstring/1");
    assert_eq!(bits_of(result), (vec![0b0010_0000], 3));

    // `list_to_bitstring([<<1:3>>,<<1:5>>])` ⇒ `<<"!">>` (`0b0010_0001`) — the two
    // pieces are concatenated **bit by bit**.
    let pair = ctx.list(&[three, five]).expect("fits");
    let result = reg
        .call("erlang", "list_to_bitstring", 1, &mut ctx, &[pair])
        .expect("list_to_bitstring/1 of two bitstrings");
    assert_eq!(bits_of(result), (vec![0b0010_0001], 8));

    // `list_to_bitstring([<<1:3>>,255])` ⇒ `<<63,7:3>>` (the byte is shifted into
    // the partial byte).
    let byte = ctx.int(255).expect("fits");
    let mixed = ctx.list(&[three, byte]).expect("fits");
    let result = reg
        .call("erlang", "list_to_bitstring", 1, &mut ctx, &[mixed])
        .expect("list_to_bitstring/1 of a bitstring and a byte");
    assert_eq!(bits_of(result), (vec![63, 0b1110_0000], 11));

    // `list_to_bitstring([])` ⇒ `<<>>`, and `list_to_bitstring([1|[1]])` ⇒ `<<1,1>>`
    // (a list tail, not a bitstring).
    let empty = ctx.list(&[]).expect("fits");
    let result = reg
        .call("erlang", "list_to_bitstring", 1, &mut ctx, &[empty])
        .expect("list_to_bitstring/1 of []");
    assert_eq!(bits_of(result), (Vec::new(), 0));
    let one = ctx.int(1).expect("fits");
    let tail = ctx.list(&[one]).expect("fits");
    let improper = ctx.cons(one, tail).expect("fits");
    let result = reg
        .call("erlang", "list_to_bitstring", 1, &mut ctx, &[improper])
        .expect("list_to_bitstring/1 with a list tail");
    assert_eq!(bits_of(result), (vec![1, 1], 16));

    // A bitstring **cons tail** is allowed here too: `list_to_bitstring([1|<<1:3>>])`
    // ⇒ `<<1,1:3>>`.
    let improper_tail = ctx.cons(one, three).expect("fits");
    let result = reg
        .call("erlang", "list_to_bitstring", 1, &mut ctx, &[improper_tail])
        .expect("list_to_bitstring/1 with a bitstring tail");
    assert_eq!(bits_of(result), (vec![0b0000_0001, 0b0010_0000], 11));
}

#[test]
fn list_to_bitstring_rejects_bad_leaves_and_a_non_list_top_level() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    let three = codec_term::make_bitstring(&mut heap, &[0b0010_0000], 3).expect("fits");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // A non-list (and non-`[]`) top level is `badarg`.
    let one = ctx.int(1).expect("fits");
    let a = ctx.atom("a").expect("seeded");
    for bad in [three, one, a] {
        assert!(matches!(
            reg.call("erlang", "list_to_bitstring", 1, &mut ctx, &[bad]),
            Err(BeamError::BadArg)
        ));
    }

    // A bad byte (`256`, `-1`) and a non-integer leaf are `badarg`.
    for values in [vec![256i64], vec![-1]] {
        let bad = ints(&mut ctx, &values);
        assert!(matches!(
            reg.call("erlang", "list_to_bitstring", 1, &mut ctx, &[bad]),
            Err(BeamError::BadArg)
        ));
    }
    let float = ctx.make_float(1.0).expect("fits");
    let with_float = ctx.list(&[float]).expect("fits");
    let with_atom = ctx.list(&[a]).expect("fits");
    for bad in [with_float, with_atom] {
        assert!(matches!(
            reg.call("erlang", "list_to_bitstring", 1, &mut ctx, &[bad]),
            Err(BeamError::BadArg)
        ));
    }

    // An improper cons tail that is not a bitstring is `badarg` (`[1|2]`).
    let two = ctx.int(2).expect("fits");
    let improper = ctx.cons(one, two).expect("fits");
    assert!(matches!(
        reg.call("erlang", "list_to_bitstring", 1, &mut ctx, &[improper]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn split_binary_splits_at_a_byte_position() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    let abcd = codec_term::make_binary(&mut heap, b"abcd").expect("fits");
    let nine_bits = codec_term::make_bitstring(&mut heap, &[0b0000_0000, 0b1000_0000], 9).expect("fits");
    let empty = codec_term::make_binary(&mut heap, b"").expect("fits");
    // The two expected halves are materialized before the context borrows `heap`.
    let expected_left = codec_term::make_binary(&mut heap, b"ab").expect("fits");
    let expected_right = codec_term::make_binary(&mut heap, b"cd").expect("fits");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // `split_binary(<<"abcd">>,2)` ⇒ `{<<"ab">>,<<"cd">>}`.
    let two = ctx.int(2).expect("fits");
    let result = reg
        .call("erlang", "split_binary", 2, &mut ctx, &[abcd, two])
        .expect("split_binary/2");
    assert_eq!(codec_term::tuple_arity(result).expect("a 2-tuple"), 2);
    let left = codec_term::tuple_element(result, 0).expect("element 0");
    let right = codec_term::tuple_element(result, 1).expect("element 1");
    assert_eq!(bits_of(left), (b"ab".to_vec(), 16));
    assert_eq!(bits_of(right), (b"cd".to_vec(), 16));
    assert!(eq(left, expected_left, &atoms));
    assert!(eq(right, expected_right, &atoms));

    // The two boundaries: `0` ⇒ `{<<>>, <<"abcd">>}` and `4` ⇒
    // `{<<"abcd">>, <<>>}` (and an empty subject splits at `0`).
    let zero = ctx.int(0).expect("fits");
    let result = reg
        .call("erlang", "split_binary", 2, &mut ctx, &[abcd, zero])
        .expect("split_binary/2 at 0");
    assert_eq!(bits_of(codec_term::tuple_element(result, 0).expect("0")), (Vec::new(), 0));
    assert_eq!(bits_of(codec_term::tuple_element(result, 1).expect("1")), (b"abcd".to_vec(), 32));
    let four = ctx.int(4).expect("fits");
    let result = reg
        .call("erlang", "split_binary", 2, &mut ctx, &[abcd, four])
        .expect("split_binary/2 at the byte size");
    assert_eq!(bits_of(codec_term::tuple_element(result, 0).expect("0")), (b"abcd".to_vec(), 32));
    assert_eq!(bits_of(codec_term::tuple_element(result, 1).expect("1")), (Vec::new(), 0));
    let result = reg
        .call("erlang", "split_binary", 2, &mut ctx, &[empty, zero])
        .expect("split_binary/2 of <<>>");
    assert_eq!(bits_of(codec_term::tuple_element(result, 0).expect("0")), (Vec::new(), 0));
    assert_eq!(bits_of(codec_term::tuple_element(result, 1).expect("1")), (Vec::new(), 0));

    // The bound is `bit_size / 8`, so a non-byte-aligned subject only splits at 0 or
    // at a whole byte: `split_binary(<<1:9>>, 1)` ⇒ `{<<0>>, <<1:1>>}`, and `0`
    // gives back the whole (partial) bitstring — `{<<>>, <<0,1:1>>}`.
    let one = ctx.int(1).expect("fits");
    let result = reg
        .call("erlang", "split_binary", 2, &mut ctx, &[nine_bits, one])
        .expect("split_binary/2 at 1");
    assert_eq!(bits_of(codec_term::tuple_element(result, 0).expect("0")), (vec![0], 8));
    assert_eq!(bits_of(codec_term::tuple_element(result, 1).expect("1")), (vec![0b1000_0000], 1));
    let result = reg
        .call("erlang", "split_binary", 2, &mut ctx, &[nine_bits, zero])
        .expect("split_binary/2 at 0 of <<1:9>>");
    assert_eq!(bits_of(codec_term::tuple_element(result, 0).expect("0")), (Vec::new(), 0));
    assert_eq!(
        bits_of(codec_term::tuple_element(result, 1).expect("1")),
        (vec![0, 0b1000_0000], 9)
    );
}

#[test]
fn split_binary_rejects_out_of_range_and_bad_arguments() {
    let reg = default_registry();
    let mut heap = Heap::new(8192).expect("test heap");
    let atoms = seeded_atoms();
    let abcd = codec_term::make_binary(&mut heap, b"abcd").expect("fits");
    let nine_bits = codec_term::make_bitstring(&mut heap, &[0b0000_0000, 0b1000_0000], 9).expect("fits");
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // A position past `byte_size` is `badarg` (`5` on a 4-byte binary, and `2` on a
    // 9-bit bitstring whose `bit_size / 8` is `1`).
    for (subject, position) in [(abcd, 5i64), (nine_bits, 2)] {
        let pos = ctx.int(position).expect("fits");
        assert!(matches!(
            reg.call("erlang", "split_binary", 2, &mut ctx, &[subject, pos]),
            Err(BeamError::BadArg)
        ));
    }

    // A negative or non-integer position (`term_to_Uint` failure) is `badarg`.
    let negative = ctx.int(-1).expect("fits");
    let float = ctx.make_float(1.0).expect("fits");
    let a = ctx.atom("a").expect("seeded");
    for pos in [negative, float, a] {
        assert!(matches!(
            reg.call("erlang", "split_binary", 2, &mut ctx, &[abcd, pos]),
            Err(BeamError::BadArg)
        ));
    }

    // A non-bitstring subject is `badarg` even with a valid position.
    let one = ctx.int(1).expect("fits");
    for subject in [one, a] {
        assert!(matches!(
            reg.call("erlang", "split_binary", 2, &mut ctx, &[subject, one]),
            Err(BeamError::BadArg)
        ));
    }
}

/// `[]` is the empty iolist for all three flatteners: `<<>>` for the two bitstring
/// producers and `0` for the size (`erl -noshell -eval
/// 'io:format("~p~n",[iolist_to_binary([])]), halt().'` ⇒ `<<>>`).
#[test]
fn nil_is_the_empty_iolist() {
    let reg = default_registry();
    let mut heap = Heap::new(256).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    for (function, arity) in [("iolist_to_binary", 1u32), ("list_to_bitstring", 1)] {
        let result = reg
            .call("erlang", function, arity, &mut ctx, &[NIL])
            .expect("the empty iolist");
        assert_eq!(
            bits_of(result),
            (Vec::new(), 0),
            "{function}([]) must be the empty bitstring"
        );
    }

    let result = reg
        .call("erlang", "iolist_size", 1, &mut ctx, &[NIL])
        .expect("iolist_size([])");
    assert_eq!(signed_value(result), 0);
}
