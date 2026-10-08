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

//! Bitstring `bs_create_bin` **`string` constant segment** integration tests.
//!
//! Loads the committed oracle fixture `tests/fixtures/bsstring.beam` (produced by the
//! system `erlc`, OTP 29 / erts-17.1) and runs its exports end to end through
//! [`Machine::run_export`].
//!
//! A `string` segment is what the compiler emits for a *literal* byte string
//! concatenated with a dynamic part (`<<"prefix", X/binary>>`): the literal is stored
//! in the module's `StrT` table and the segment carries its byte **offset** as the
//! value operand and its byte **count** as the size operand
//! (`beam_ssa_codegen.erl:1970-1976`). The engine appends those bytes with
//! `erts_bs_put_string` semantics, i.e. at the current (possibly non-byte-aligned)
//! bit position, exactly as the oracle does.
//!
//! Every expected byte sequence (and the `StrT` table contents themselves) was
//! derived from the same system `erl` oracle — for example
//! `erl -noshell -eval 'io:format("~w~n",[binary_to_list(bsstring:prefix(<<7,9>>))])'`
//! — and the segment each export emits was confirmed with `beam_disasm:file/1` (see
//! `tests/fixtures/README.md`). Nothing here is guessed.
//!
//! The `c_*`-style failure paths are not part of the fixture: a `string` segment is a
//! compile-time constant, so the oracle has no input that makes one fail. The engine's
//! malformed-segment paths (a non-offset value, an `all` size, an out-of-range slice)
//! are covered by the unit tests in `src/families/bitstrings/create.rs`.

use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{make_small, Atom, Eterm};

/// The committed string-segment fixture (`erlc bsstring.erl`).
const BSSTRING_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bsstring.beam"
);

fn load() -> Module {
    load_file(BSSTRING_BEAM).unwrap_or_else(|e| panic!("load {BSSTRING_BEAM}: {e:?}"))
}

fn export_atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned"))
}

/// Run an export with `args` on a fresh machine over the caller-owned `ctx`.
fn run(
    module: &Module,
    name: &str,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Eterm {
    let mut machine = Machine::new();
    machine
        .run_export(module, export_atom(module, name), args.len() as u32, args, ctx)
        .unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

/// The exact bytes of a byte-aligned result binary.
fn bytes(term: Eterm) -> Vec<u8> {
    terms::binary_bytes(term)
        .unwrap_or_else(|e| panic!("not a binary: {e:?}"))
        .to_vec()
}

/// The exact `(bytes, bit count)` of a result bitstring, byte-aligned or not.
///
/// `unaligned_tail/1` deliberately ends on a 4-bit boundary, so the result is a
/// bitstring whose trailing bits must be asserted too — `terms::binary_bytes`
/// rejects it by design.
fn bitstring(term: Eterm) -> (Vec<u8>, usize) {
    // SAFETY: `term` is a live bitstring the engine just produced; the arena the
    // context owns outlives this call (the context is borrowed for the whole test).
    let (base, offset, size) = unsafe { beam_term::bitstring_parts(term) };
    let mut out = vec![0u8; size.div_ceil(8)];
    for i in 0..size {
        let bit_index = offset + i;
        // SAFETY: the base pointer covers `offset + size` bits by construction, so
        // the byte holding bit `bit_index` is inside the bitstring's storage.
        let bit = (unsafe { *base.add(bit_index / 8) } >> (7 - (bit_index % 8))) & 1;
        out[i / 8] |= bit << (7 - (i % 8));
    }
    (out, size)
}

fn bin(ctx: &mut MinimalContext, data: &[u8]) -> Eterm {
    terms::make_heap_binary(ctx, data).expect("heap binary")
}

/// Assert a byte-aligned export's exact result bytes.
fn expect_bytes(name: &str, args: &[Eterm], expected: &[u8]) {
    let module = load();
    let mut ctx = MinimalContext::new();
    let result = run(&module, name, args, &mut ctx);
    assert_eq!(bytes(result), expected, "{name}({args:?})");
}

// ---------------------------------------------------------------------------
// The fixture's `StrT` table — the offsets the segments index
// ---------------------------------------------------------------------------

#[test]
fn the_string_table_is_the_oracle_table() {
    // `beam_lib:chunks(F, ["StrT"])` on the committed fixture reports
    // `<<"abhicdabczxyQ">>`; every segment offset below indexes into it.
    let module = load();
    assert_eq!(module.strings, b"abhicdabczxyQ".to_vec());
}

// ---------------------------------------------------------------------------
// A string prefix followed by a whole binary
// ---------------------------------------------------------------------------

#[test]
fn prefix_appends_the_string_before_the_binary() {
    // prefix(X) -> <<"ab", X/binary>>. The "ab" segment is at StrT offset 0.
    let mut ctx = MinimalContext::new();
    // `erl -noshell -eval 'binary_to_list(bsstring:prefix(<<7,9>>))'` -> [97,98,7,9]
    expect_bytes("prefix", &[bin(&mut ctx, &[7, 9])], &[97, 98, 7, 9]);
}

#[test]
fn prefix_with_an_empty_binary_is_just_the_string() {
    let mut ctx = MinimalContext::new();
    // `bsstring:prefix(<<>>)` -> [97,98]
    expect_bytes("prefix", &[bin(&mut ctx, &[])], &[97, 98]);
}

// ---------------------------------------------------------------------------
// Multiple string segments and a reused substring (offset into the pool)
// ---------------------------------------------------------------------------

#[test]
fn two_strings_surround_an_integer() {
    // two(A) -> <<"ab", A:8, "cd">>. "ab" is at offset 0 and "cd" at offset 4, so
    // the second step exercises a string read from the middle of the table.
    let mut ctx = MinimalContext::new();
    let _ = &mut ctx;
    // `bsstring:two(255)` -> [97,98,255,99,100]
    expect_bytes("two", &[make_small(255)], &[97, 98, 255, 99, 100]);
}

#[test]
fn a_trailing_string_reusing_a_substring_of_the_first_is_read_at_its_own_offset() {
    // reuse(X) -> <<"abc", X/binary, "bc">>. The trailing "bc" is a suffix of the
    // already-stored "abc", so `beam_dict:string/2` stores no second copy and the
    // segment's offset points into the middle of the table (offset 7).
    let mut ctx = MinimalContext::new();
    // `bsstring:reuse(<<1,2>>)` -> [97,98,99,1,2,98,99]
    expect_bytes("reuse", &[bin(&mut ctx, &[1, 2])], &[97, 98, 99, 1, 2, 98, 99]);
    let mut ctx = MinimalContext::new();
    // `bsstring:reuse(<<>>)` -> [97,98,99,98,99]
    expect_bytes("reuse", &[bin(&mut ctx, &[])], &[97, 98, 99, 98, 99]);
}

// ---------------------------------------------------------------------------
// The string at a non-byte-aligned position
// ---------------------------------------------------------------------------

#[test]
fn mixed_writes_the_string_at_a_non_byte_aligned_offset() {
    // mixed(A) -> <<A:4, "hi", 3:4>> — "hi" starts at bit 4, so it is copied bit by
    // bit (`erts_copy_bits_fwd`, `erl_bits.c:1647-1650`) while the total stays 24
    // bits (byte-aligned).
    // `bsstring:mixed(10)` -> [166,134,147]
    expect_bytes("mixed", &[make_small(10)], &[166, 134, 147]);
    // `bsstring:mixed(0)` -> [6,134,147]
    expect_bytes("mixed", &[make_small(0)], &[6, 134, 147]);
}

#[test]
fn a_string_after_a_non_byte_aligned_tail_keeps_the_exact_bit_count() {
    // unaligned_tail(A) -> <<A:12, "Q">> — the result is 20 bits, so it is a
    // bitstring (not a binary) and its exact size matters.
    let cases: &[(i64, &[u8])] = &[
        // `bsstring:unaligned_tail(16#AAB)` -> <<170,181,1:4>>
        (0xAAB, &[170, 181, 0b0001_0000]),
        // `bsstring:unaligned_tail(0)` -> <<0,5,1:4>>
        (0, &[0, 5, 0b0001_0000]),
        // `bsstring:unaligned_tail(16#ABC)` -> <<171,197,1:4>>
        (0xABC, &[171, 197, 0b0001_0000]),
    ];
    let module = load();
    let mut ctx = MinimalContext::new();
    for &(value, expected) in cases {
        let result = run(&module, "unaligned_tail", &[make_small(value)], &mut ctx);
        let (got, nbits) = bitstring(result);
        assert_eq!(nbits, 20, "unaligned_tail({value}) bit count");
        assert_eq!(got, expected, "unaligned_tail({value})");
    }
}

// ---------------------------------------------------------------------------
// A string between two dynamic segments, and a single-byte string
// ---------------------------------------------------------------------------

#[test]
fn combo_writes_a_string_between_two_dynamic_segments() {
    // combo(A, B) -> <<A:8, "xy", B/binary, "z">>. "xy" is at offset 10 and the
    // trailing "z" reuses the byte at offset 9.
    let mut ctx = MinimalContext::new();
    let b = bin(&mut ctx, &[2, 3]);
    // `bsstring:combo(1,<<2,3>>)` -> [1,120,121,2,3,122]
    expect_bytes("combo", &[make_small(1), b], &[1, 120, 121, 2, 3, 122]);
}

#[test]
fn a_one_byte_string_pairs_with_an_integer() {
    // one(A) -> <<"z", A:8>>. `bsstring:one(65)` -> [122,65]
    expect_bytes("one", &[make_small(65)], &[122, 65]);
}

#[test]
fn a_string_at_offset_zero_followed_by_an_integer() {
    // at_zero(A) -> <<"hi", A:8>>; "hi" is reused from offset 2.
    // `bsstring:at_zero(5)` -> [104,105,5]
    expect_bytes("at_zero", &[make_small(5)], &[104, 105, 5]);
}

// ---------------------------------------------------------------------------
// The empty literal piece the compiler drops
// ---------------------------------------------------------------------------

#[test]
fn an_empty_literal_piece_contributes_nothing() {
    // empty_prefix(X) -> <<"", X/binary>>. `bs_args/3` drops the empty binary
    // segment and rewrites it as a 0-bit integer segment, so the result is just X.
    // `bsstring:empty_prefix(<<4,5>>)` -> [4,5]
    let mut ctx = MinimalContext::new();
    expect_bytes("empty_prefix", &[bin(&mut ctx, &[4, 5])], &[4, 5]);
}
