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

//! Bitstring **match-form** integration tests: the forms `tests/bitstrings.rs` does
//! not reach.
//!
//! Loads the committed oracle fixture `tests/fixtures/bsmatch.beam` (produced by the
//! system `erlc`, OTP 29 / erts-17.1) and runs its exports end to end through
//! [`Machine::run_export`], asserting the exact results of:
//!
//! * `bs_match_string` — a literal string in a binary pattern (`strlong/1`, an 80-bit
//!   literal); the compiler folds literals of at most 64 bits into the `'=:='`
//!   `bs_match` command instead (`str/1`), so both are covered here;
//! * `bs_get_float2` — `<<F:16/float>>` / `<<F:32/float>>` / `<<F:32/float-little>>`
//!   / `<<F:64/float>>` / `<<F:64/float-little>>`;
//! * `bs_get_utf8` / `bs_get_utf16` / `bs_get_utf32` (+ `-little`);
//! * `bs_skip_utf8` / `bs_skip_utf16` / `bs_skip_utf32`.
//!
//! Every expected value is the **real system `erl` oracle's** answer for the same
//! input (see `tests/fixtures/README.md`); nothing is guessed. Every malformed /
//! short input is a typed [`BeamError`] — no test panics on runtime data.
//!
//! The `c_*` exports wrap each pattern in a `case`: a mismatch reaches the engine's
//! `case_end` handler and raises, whereas a *function-head* mismatch jumps to the
//! function's `func_info` label (a no-op at M1), so the head-pattern exports are only
//! exercised on their success paths.

use beam_common::BeamError;
use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{load_file, Instr, Module, Operand};
use beam_term::{make_small, Atom, Eterm};

/// The committed bitstring match fixture (`erlc bsmatch.erl`).
const BSMATCH_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bsmatch.beam"
);

fn load() -> Module {
    load_file(BSMATCH_BEAM).unwrap_or_else(|e| panic!("load {BSMATCH_BEAM}: {e:?}"))
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
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_export(module, export_atom(module, name), args.len() as u32, args, ctx)
}

fn bin(ctx: &mut MinimalContext, data: &[u8]) -> Eterm {
    terms::make_heap_binary(ctx, data).expect("heap binary")
}

fn bytes(term: Eterm) -> Vec<u8> {
    terms::binary_bytes(term)
        .unwrap_or_else(|e| panic!("not a binary: {e:?}"))
        .to_vec()
}

/// Call `name/1` with a freshly built binary and require the exact `expected` bytes.
fn expect_bin(name: &str, input: &[u8], expected: &[u8]) {
    let module = load();
    let mut ctx = MinimalContext::new();
    let arg = bin(&mut ctx, input);
    let result = run(&module, name, &[arg], &mut ctx)
        .unwrap_or_else(|e| panic!("{name}({input:?}): {e:?}"));
    assert_eq!(bytes(result), expected, "{name}({input:?})");
}

/// Call `name/1` with a freshly built binary and require the exact float result.
fn expect_float(name: &str, input: &[u8], expected: f64) {
    let module = load();
    let mut ctx = MinimalContext::new();
    let arg = bin(&mut ctx, input);
    let result = run(&module, name, &[arg], &mut ctx)
        .unwrap_or_else(|e| panic!("{name}({input:?}): {e:?}"));
    let got = terms::float_value(result).unwrap_or_else(|e| panic!("{name}: not a float: {e:?}"));
    assert_eq!(got, expected, "{name}({input:?})");
}

/// Call `name/1` with a freshly built binary and require the exact small-integer result.
fn expect_int(name: &str, input: &[u8], expected: i64) {
    let module = load();
    let mut ctx = MinimalContext::new();
    let arg = bin(&mut ctx, input);
    let result = run(&module, name, &[arg], &mut ctx)
        .unwrap_or_else(|e| panic!("{name}({input:?}): {e:?}"));
    assert_eq!(result, make_small(expected), "{name}({input:?})");
}

/// Call `name/1` with a freshly built binary and require a typed `case_clause` raise
/// (the oracle raises `error:{case_clause, _}` for every input used here).
fn expect_raise(name: &str, input: &[u8]) {
    let module = load();
    let mut ctx = MinimalContext::new();
    let arg = bin(&mut ctx, input);
    let result = run(&module, name, &[arg], &mut ctx);
    assert!(
        matches!(result, Err(BeamError::Raise(_))),
        "{name}({input:?}) should raise, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// The fixture really emits the opcodes under test
// ---------------------------------------------------------------------------

/// Guard: the committed `.beam` still contains each target instruction, so a
/// recompiled fixture cannot silently stop covering a form (and the loader's
/// lowering is exercised for each). `'=:='` lives *inside* a `bs_match` command
/// list, not as an instruction of its own.
#[test]
fn fixture_emits_the_target_opcodes() {
    let module = load();
    let (mut match_string, mut float2, mut utf8, mut utf16, mut utf32) = (false, false, false, false, false);
    let (mut skip8, mut skip16, mut skip32, mut eq_command) = (false, false, false, false);

    for function in &module.functions {
        for instr in &function.code {
            match instr {
                Instr::BsMatchString { .. } => match_string = true,
                Instr::BsGetFloat2(_) => float2 = true,
                Instr::BsGetUtf8 { .. } => utf8 = true,
                Instr::BsGetUtf16 { .. } => utf16 = true,
                Instr::BsGetUtf32 { .. } => utf32 = true,
                Instr::BsSkipUtf8 { .. } => skip8 = true,
                Instr::BsSkipUtf16 { .. } => skip16 = true,
                Instr::BsSkipUtf32 { .. } => skip32 = true,
                Instr::BsMatch { commands, .. } => {
                    eq_command |= commands.iter().any(|op| match op {
                        // The atom's *name* is `=:=`; the quotes `beam_disasm`
                        // prints are just the non-alphanumeric-atom notation.
                        Operand::Atom(atom) => module.atom_name(*atom) == Some("=:="),
                        _ => false,
                    });
                }
                _ => {}
            }
        }
    }

    assert!(match_string, "no BsMatchString in the fixture");
    assert!(float2, "no BsGetFloat2 in the fixture");
    assert!(utf8 && utf16 && utf32, "missing a BsGetUtf* in the fixture");
    assert!(skip8 && skip16 && skip32, "missing a BsSkipUtf* in the fixture");
    assert!(eq_command, "no `'=:='` bs_match command in the fixture");
}

// ---------------------------------------------------------------------------
// Literal string in a binary pattern
// ---------------------------------------------------------------------------

#[test]
fn str_folds_a_short_literal_into_the_eq_command() {
    // str(<<"ab", R/binary>>) -> R.  (16-bit literal → `'=:='` bs_match command)
    expect_bin("str", b"abXYZ", b"XYZ");
    // An empty tail is fine.
    expect_bin("str", b"ab", b"");
}

#[test]
fn strlong_uses_the_standalone_bs_match_string() {
    // strlong(<<"abcdefghij", R/binary>>) -> R.  (80-bit literal → bs_match_string)
    expect_bin("strlong", b"abcdefghijXYZ", b"XYZ");
    expect_bin("strlong", b"abcdefghij", b"");
}

// ---------------------------------------------------------------------------
// Fixed float reads (`bs_get_float2`)
// ---------------------------------------------------------------------------

#[test]
fn f16_reads_a_binary16_float() {
    // Oracle: f16(<<62,0>>) = 1.5, f16(<<180,0>>) = -0.25, f16(<<56,0>>) = 0.5.
    expect_float("f16", &[62, 0], 1.5);
    expect_float("f16", &[180, 0], -0.25);
    expect_float("f16", &[56, 0], 0.5);
    // Zero has two encodings; both are exactly 0.0.
    expect_float("f16", &[0, 0], 0.0);
    expect_float("f16", &[0x80, 0], -0.0);
}

#[test]
fn f32_reads_a_binary32_float_big_endian() {
    // Oracle: f32(<<63,192,0,0>>) = 1.5, f32(<<192,32,0,0>>) = -2.5,
    // f32(<<61,204,204,205>>) = 0.10000000149011612.
    expect_float("f32", &[63, 192, 0, 0], 1.5);
    expect_float("f32", &[192, 32, 0, 0], -2.5);
    expect_float("f32", &[61, 204, 204, 205], 0.100_000_001_490_116_12);
}

#[test]
fn f32le_reads_a_binary32_float_little_endian() {
    // Oracle: f32le(<<0,0,192,63>>) = 1.5, f32le(<<0,0,32,192>>) = -2.5.
    expect_float("f32le", &[0, 0, 192, 63], 1.5);
    expect_float("f32le", &[0, 0, 32, 192], -2.5);
}

#[test]
fn f64_reads_a_binary64_float_big_endian() {
    // Oracle: f64(<<64,9,33,251,84,68,45,24>>) = 3.141592653589793,
    // f64(<<254,55,228,60,136,0,117,156>>) = -1.0e300.
    expect_float("f64", &[64, 9, 33, 251, 84, 68, 45, 24], 3.141_592_653_589_793);
    expect_float("f64", &[254, 55, 228, 60, 136, 0, 117, 156], -1.0e300);
}

#[test]
fn f64le_reads_a_binary64_float_little_endian() {
    // Oracle: f64le(<<24,45,68,84,251,33,9,64>>) = 3.141592653589793.
    expect_float("f64le", &[24, 45, 68, 84, 251, 33, 9, 64], 3.141_592_653_589_793);
}

#[test]
fn float_reads_reject_a_short_field() {
    // A `<<F:32/float>>` pattern needs four bytes; the `case` clause has no match.
    expect_raise("c_f16", &[1]);
    expect_raise("c_f32", &[1, 2]);
    expect_raise("c_f64", &[1, 2, 3, 4]);
}

#[test]
fn f32_rejects_a_longer_subject() {
    // `ensure_exactly 0` after the read: five bytes are not a 32-bit float field.
    expect_raise("c_f32", &[1, 2, 3, 4, 5]);
}

// ---------------------------------------------------------------------------
// UTF reads (`bs_get_utf{8,16,32}`)
// ---------------------------------------------------------------------------

#[test]
fn u8_reads_a_utf8_scalar() {
    // Oracle: u8(<<65>>) = 65, u8(<<226,130,172>>) = 8364 (U+20AC, the euro sign).
    expect_int("u8", &[65], 65);
    expect_int("u8", &[226, 130, 172], 8364);
}

#[test]
fn u16_reads_a_utf16_scalar_big_endian() {
    // Oracle: u16(<<32,172>>) = 8364; the surrogate pair D83D DE00 is U+1F600 = 128512.
    expect_int("u16", &[32, 172], 8364);
    expect_int("u16", &[216, 61, 222, 0], 128_512);
}

#[test]
fn u16le_reads_a_utf16_scalar_little_endian() {
    // Oracle: u16le(<<172,32>>) = 8364.
    expect_int("u16le", &[172, 32], 8364);
}

#[test]
fn u32_reads_a_utf32_scalar() {
    // Oracle: u32(<<0,0,32,172>>) = 8364, u32(<<0,16,255,255>>) = 1114111 (U+10FFFF).
    expect_int("u32", &[0, 0, 32, 172], 8364);
    expect_int("u32", &[0, 16, 255, 255], 1_114_111);
}

#[test]
fn u8_rejects_a_malformed_utf8_sequence() {
    // Truncated (E2 82), a reserved lead byte (FF), a bad continuation byte,
    // an overlong C0 80, and the surrogate ED A0 80 — all rejected by the oracle.
    expect_raise("c_u8", &[226, 130]);
    expect_raise("c_u8", &[255]);
    expect_raise("c_u8", &[226, 40, 172]);
    expect_raise("c_u8", &[192, 128]);
    expect_raise("c_u8", &[237, 160, 128]);
}

#[test]
fn u16_rejects_a_lone_surrogate() {
    // D83D is an unpaired high surrogate; DE00 an unpaired low surrogate.
    expect_raise("c_u16", &[216, 61]);
    expect_raise("c_u16", &[222, 0]);
}

#[test]
fn u32_rejects_a_surrogate_and_an_out_of_range_scalar() {
    // 0xD800 is a surrogate; 0x110000 is beyond U+10FFFF.
    expect_raise("c_u32", &[0, 0, 216, 0]);
    expect_raise("c_u32", &[0, 17, 0, 0]);
}

// ---------------------------------------------------------------------------
// UTF skips (`bs_skip_utf{8,16,32}`)
// ---------------------------------------------------------------------------

#[test]
fn sk8_skips_one_utf8_scalar() {
    // Oracle: sk8(<<"arest">>) = <<"rest">>, sk8(<<U+20AC,"x">>) = <<"x">>.
    expect_bin("sk8", b"arest", b"rest");
    expect_bin("sk8", &[226, 130, 172, 120], b"x");
}

#[test]
fn sk16_skips_one_utf16_scalar() {
    // Oracle: sk16(<<0,1,"xy">>) = <<"xy">>.
    expect_bin("sk16", &[0, 1, 120, 121], b"xy");
    // A surrogate pair is one scalar, so both 16-bit words are skipped.
    expect_bin("sk16", &[216, 61, 222, 0, 120, 121], b"xy");
}

#[test]
fn sk32_skips_one_utf32_scalar() {
    // Oracle: sk32(<<0,0,0,1,"xy">>) = <<"xy">>.
    expect_bin("sk32", &[0, 0, 0, 1, 120, 121], b"xy");
}

#[test]
fn skips_reject_a_malformed_scalar() {
    expect_raise("c_sk8", &[255]);
    expect_raise("c_sk16", &[216, 61]);
    expect_raise("c_sk32", &[0, 0, 216, 0]);
}
