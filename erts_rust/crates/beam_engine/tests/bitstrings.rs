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

//! Bitstring (`bs_*`) instruction-family integration tests.
//!
//! Loads the committed oracle fixture `tests/fixtures/bitstrings.beam` (produced by
//! the system `erlc`, OTP 29 / erts-17.1) and runs its exports end to end through
//! [`Machine::run_export`], asserting the exact results of the constructs the
//! Milestone-1 bitstring family implements: `bs_create_bin` (integer / binary /
//! string segments) and the match side (`bs_start_match3`, `bs_match`,
//! `bs_get_integer2`, `bs_get_binary2`, `bs_skip_bits2`, `bs_get_tail`,
//! `bs_test_tail2`, `bs_get_position`/`bs_set_position`).
//!
//! Every error is a typed [`BeamError`] — no test panics on runtime data.

use beam_common::BeamError;
use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{make_small, Atom, Eterm};

/// The committed bitstring fixture (`erlc bitstrings.erl`).
const BITSTRINGS_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bitstrings.beam"
);

fn load(path: &str) -> Module {
    load_file(path).unwrap_or_else(|e| panic!("load {path}: {e:?}"))
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
    arity: u32,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_export(module, export_atom(module, name), arity, args, ctx)
}

fn bytes(term: Eterm) -> Vec<u8> {
    terms::binary_bytes(term)
        .unwrap_or_else(|e| panic!("not a binary: {e:?}"))
        .to_vec()
}

fn bin(ctx: &mut MinimalContext, data: &[u8]) -> Eterm {
    terms::make_heap_binary(ctx, data).expect("heap binary")
}

// ---------------------------------------------------------------------------
// Construction (`bs_create_bin`)
// ---------------------------------------------------------------------------

#[test]
fn mk_builds_an_integer_and_a_binary_segment() {
    // mk(A, B) -> <<A:8, B:16>>
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let result = run(
        &module,
        "mk",
        2,
        &[make_small(1), make_small(258)],
        &mut ctx,
    )
    .expect("mk");
    assert_eq!(bytes(result), vec![1, 1, 2]);
}

#[test]
fn pair_builds_two_integer_segments() {
    // pair(A, B) -> <<A:8, B:8>>
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let result = run(
        &module,
        "pair",
        2,
        &[make_small(1), make_small(2)],
        &mut ctx,
    )
    .expect("pair");
    assert_eq!(bytes(result), vec![1, 2]);
}

#[test]
fn le16_builds_a_little_endian_segment() {
    // le16(A) -> <<A:16/little>>
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let result = run(&module, "le16", 1, &[make_small(258)], &mut ctx).expect("le16");
    assert_eq!(bytes(result), vec![2, 1]);
}

#[test]
fn lit_bin_appends_a_string_constant_segment() {
    // lit_bin(X) -> <<"ab", X/binary>>. The `{atom,string}` segment carries the
    // `StrT` byte offset of "ab" and its byte count, so the engine appends those
    // bytes before the operand.
    // `erl -noshell -eval 'binary_to_list(bitstrings:lit_bin(<<7,9>>))'` -> [97,98,7,9]
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let x = bin(&mut ctx, &[7, 9]);
    let result = run(&module, "lit_bin", 1, &[x], &mut ctx).expect("lit_bin");
    assert_eq!(bytes(result), vec![97, 98, 7, 9]);
}

#[test]
fn lit3_mixes_integer_string_and_binary_segments() {
    // lit3(A, B) -> <<A:8, "z", B/binary>>. `bitstrings.beam`'s `StrT` is `<<"abz">>`,
    // so the `"z"` segment indexes offset 2.
    // `erl -noshell -eval 'binary_to_list(bitstrings:lit3(3,<<4,5>>))'` -> [3,122,4,5]
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let b = bin(&mut ctx, &[4, 5]);
    let result = run(&module, "lit3", 2, &[make_small(3), b], &mut ctx).expect("lit3");
    assert_eq!(bytes(result), vec![3, 122, 4, 5]);
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

#[test]
fn first8_reads_a_leading_integer() {
    // first8(Bin) -> case Bin of <<X:8, _/binary>> -> X end
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = bin(&mut ctx, &[7, 9]);
    let result = run(&module, "first8", 1, &[arg], &mut ctx).expect("first8");
    assert_eq!(result, make_small(7));
}

#[test]
fn take2_binds_two_integers() {
    // take2(Bin) -> case Bin of <<A:8, B:8, _/binary>> -> {A, B} end
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = bin(&mut ctx, &[1, 2, 3]);
    let result = run(&module, "take2", 1, &[arg], &mut ctx).expect("take2");
    assert_eq!(terms::tuple_arity(result).unwrap(), 2);
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(1));
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(2));
}

#[test]
fn tail1_skips_a_byte_and_binds_the_tail_binary() {
    // tail1(Bin) -> case Bin of <<_:8, Rest/binary>> -> Rest end
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = bin(&mut ctx, &[1, 2, 3]);
    let result = run(&module, "tail1", 1, &[arg], &mut ctx).expect("tail1");
    assert_eq!(bytes(result), vec![2, 3]);
}

#[test]
fn head16_reads_an_exact_two_byte_integer() {
    // head16(Bin) -> case Bin of <<X:16>> -> X end
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = bin(&mut ctx, &[1, 2]);
    let result = run(&module, "head16", 1, &[arg], &mut ctx).expect("head16");
    assert_eq!(result, make_small(258));
}

#[test]
fn head16_rejects_a_longer_subject() {
    // `ensure_exactly 16` fails on a 3-byte subject; the clause has no match, so the
    // `case` raises `{case_clause, Bin}`.
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = bin(&mut ctx, &[1, 2, 3]);
    let result = run(&module, "head16", 1, &[arg], &mut ctx);
    assert!(result.is_err(), "expected a typed error, got {result:?}");
}

#[test]
fn first8_on_a_non_binary_raises() {
    // `bs_start_match3` on an integer is a match failure with no clause.
    let module = load(BITSTRINGS_BEAM);
    let mut ctx = MinimalContext::new();
    let result = run(&module, "first8", 1, &[make_small(1)], &mut ctx);
    assert!(result.is_err(), "expected a typed error, got {result:?}");
}


