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

//! Bitstring **wide integer field** integration tests — integer fields **wider
//! than 64 bits**, plus the at-most-64-bit fields whose value can still exceed
//! `MAX_SMALL`, on both the match and the construction side.
//!
//! Two instruction forms own an integer field; both are covered here:
//!
//! * the `integer` **command** of a flattened `bs_match` command list — the
//!   `erts_bs_get_integer_2` big path (`erl_bits.c:170-398`). Every read export of
//!   the committed oracle fixture `tests/fixtures/bsbig.beam` compiles into
//!   `bs_start_match3` + `bs_match` with `{integer, Live, Flags, Size, Unit, Dst}`
//!   commands whose `Size * Unit` is 65, 68, 72 or 128 bits;
//! * `bs_get_integer2` with a wide `Size * Unit` — the opcode the compiler emits for
//!   a **dynamic** size. This fixture (like every other committed fixture) compiles
//!   its wide reads into `bs_match` commands, so that arm is covered by the
//!   synthetic-module tests at the bottom of this file instead;
//! * the `integer` **segment** of `bs_create_bin` (`fmt_big_be`/`fmt_big_le`,
//!   `erl_bits.c:724`/`:811`), which accepts a small integer *or* a bignum and writes
//!   `value mod 2^Size` (a negative value becoming its `Size`-bit two's complement).
//!
//! The second committed oracle fixture, `tests/fixtures/bssmall.beam`, covers the
//! **at-most-64-bit** fields: `MAX_SMALL` is only `2^59-1`, so a 60..64-bit unsigned
//! field — and *every* little-endian field of 9 or more bits, for which ERTS has no
//! small-integer fast path — can produce a bignum, and a non-byte-multiple
//! little-endian field (`<<Y:12/little>>`, `<<Y:20/little>>`, …) takes ERTS' general
//! bit-level path (`erl_bits.c:305-313`). Those reads converge on the same
//! `IS_USMALL` (`big.h:76`) small-vs-bignum decision, so a correct engine yields a
//! small exactly when the value is in `MIN_SMALL..=MAX_SMALL`.
//!
//! Every expected value is the **real system `erl` oracle's** answer for the same
//! input (OTP 29 / `erts-17.1`), re-derived with a probe module exactly as
//! `tests/fixtures/README.md` prescribes; nothing is guessed. Every malformed or
//! short input is a typed [`BeamError`] — no test panics on runtime data.
//!
//! # Two harness limits this file works around
//!
//! 1. **Bignum results without `unsafe`.** A wide read may return a **bignum**, and
//!    no *safe* bignum reader is reachable from an integration test: the library is
//!    `#![deny(unsafe_code)]`, and only `terms/`/`families/bitstrings/raw.rs`
//!    (both `pub(crate)`-visible) dereference heap words. The read assertions
//!    therefore pin the value through the fixture's own `e256/1` segment, which
//!    writes `value mod 2^256`:
//!    * `be256_parts` computes the expected 32-byte pattern in plain Rust from the
//!      oracle's integer, so the comparison is against an **independent** value;
//!    * `e256` is injective on every value a field of at most 128 bits here can
//!      produce, so equal bytes are equal integers — and the write path it uses is
//!      itself pinned *absolutely* by the construction cases below, whose inputs are
//!      small integers (a shared read/write convention bug cannot cancel out);
//!    * a result that fits a **small integer** is additionally compared with
//!      `make_small` directly, so the machine-word path is pinned without a round trip.
//! 2. **No partial-bit subject/result builder.** `terms::make_heap_binary` builds a
//!    whole-byte heap binary, so a field whose subject must be exactly 65 or 68 bits
//!    (with a following `binary` tail, which forces the surplus to be byte-aligned)
//!    cannot be fed from the test side, and `terms::binary_bytes` rejects a
//!    non-byte-aligned result bitstring. Those two forms are therefore verified as a
//!    **round trip through the fixture's own constructor** (`g68(e68(N)) == N`,
//!    `g68le(e68le(N)) == N`): the byte-aligned read/write pair (`g72`/`e72`) pins the
//!    shared field convention absolutely, so a cancelling bug cannot hide in the
//!    round trip. A 65-bit form has no constructor in the fixture and is covered by
//!    the (byte-aligned) `g72`/`g128` siblings plus the `bs_get_integer2` synthetic
//!    test below; see the crate `CONTEXT.md`.

use std::collections::BTreeMap;

use beam_common::BeamError;
use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{load_file, Function, Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{is_small, make_small, Atom, Eterm, MAX_SMALL};

/// The committed wide-integer fixture (`erlc bsbig.erl`).
const BSBIG_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bsbig.beam");

/// The committed at-most-64-bit fixture (`erlc bssmall.erl`).
const BSSMALL_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/bssmall.beam");

/// `is_big(x)` — a boxed bignum. `beam_term`'s predicate dereferences the header
/// word of a boxed term, so it is `unsafe` (the same reason
/// `beam_engine::predicates` exists); this test module's only `unsafe`.
///
/// SAFETY: every call site passes a term the engine just returned from a run — a
/// live machine word — so if it is boxed its header is a live term word.
#[allow(unsafe_code)]
fn is_bignum(term: Eterm) -> bool {
    // SAFETY: `term` is a live machine word returned by an engine run.
    unsafe { beam_term::is_big(term) }
}

/// The `BSF_LITTLE` flag bit (`erl_bits.h:462`) — the fixture's flag literals decode
/// to these bits, and the synthetic modules below set them directly.
const BSF_LITTLE: u32 = 2;
/// The `BSF_SIGNED` flag bit (`erl_bits.h:463`).
const BSF_SIGNED: u32 = 4;

fn load() -> Module {
    load_file(BSBIG_BEAM).unwrap_or_else(|e| panic!("load {BSBIG_BEAM}: {e:?}"))
}

/// The at-most-64-bit fixture, whose `e256/1` pins a (possibly bignum) result
/// byte-exactly through the crate's public binary reader.
fn load_small() -> Module {
    load_file(BSSMALL_BEAM).unwrap_or_else(|e| panic!("load {BSSMALL_BEAM}: {e:?}"))
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

/// A byte-aligned heap binary argument (the crate's only binary builder).
fn bin(ctx: &mut MinimalContext, data: &[u8]) -> Eterm {
    terms::make_heap_binary(ctx, data).expect("heap binary")
}

fn bytes(term: Eterm) -> Vec<u8> {
    terms::binary_bytes(term)
        .unwrap_or_else(|e| panic!("not a binary: {e:?}"))
        .to_vec()
}

/// `magnitude mod 2^256`, negated when `negative`, as 32 big-endian bytes — the exact
/// image of the fixture's `e256/1`. Pure Rust: no engine code contributes to the
/// oracle side of a comparison.
fn be256_parts(magnitude: u128, negative: bool) -> Vec<u8> {
    let mut out = [0u8; 32];
    out[16..].copy_from_slice(&magnitude.to_be_bytes());
    if negative {
        let mut carry = 1u16;
        for byte in out.iter_mut().rev() {
            let v = u16::from(!*byte) + carry;
            *byte = (v & 0xFF) as u8;
            carry = v >> 8;
        }
    }
    out.to_vec()
}

/// Assert `name(input)` returns exactly the integer `expected`.
///
/// `expected` fits an `i128`, which covers every value these fields produce except
/// the `2^127` boundary case (asserted separately through [`expect_int_parts`]).
fn expect_int(
    module: &Module,
    ctx: &mut MinimalContext,
    name: &str,
    input: &[u8],
    expected: i128,
) {
    expect_int_parts(module, ctx, name, input, expected.unsigned_abs(), expected < 0);
}

/// Assert `name(input)` returns exactly `negative ? -magnitude : magnitude` (see the
/// module docs for why the value is pinned through the fixture's own 256-bit encoder).
fn expect_int_parts(
    module: &Module,
    ctx: &mut MinimalContext,
    name: &str,
    input: &[u8],
    magnitude: u128,
    negative: bool,
) {
    let arg = bin(ctx, input);
    let value =
        run(module, name, &[arg], ctx).unwrap_or_else(|e| panic!("{name}({input:?}): {e:?}"));
    // The machine-word path, directly: `make_int` (`bitstrings/mod.rs`) builds a small
    // exactly when the magnitude fits `MAX_SMALL` (`i64::MAX`) — the same split
    // `i64::try_from` performs here.
    if let Ok(small) = i64::try_from(magnitude) {
        let small = if negative { -small } else { small };
        assert_eq!(value, make_small(small), "{name}({input:?})");
    }
    let encoded = run(module, "e256", &[value], ctx)
        .map(bytes)
        .unwrap_or_else(|e| panic!("e256({name}({input:?})): {e:?}"));
    assert_eq!(encoded, be256_parts(magnitude, negative), "{name}({input:?})");
}

/// Assert `name(input)` raises a typed error (the fixture's `case_end` path).
fn expect_raise(module: &Module, name: &str, input: &[u8], ctx: &mut MinimalContext) {
    let arg = bin(ctx, input);
    let result = run(module, name, &[arg], ctx);
    assert!(
        matches!(result, Err(BeamError::Raise(_))),
        "{name}({input:?}) should raise, got {result:?}"
    );
}

/// Assert `name(input)` (a `bssmall` read of at most 64 bits) returns exactly
/// `negative ? -magnitude : magnitude`, **and** that its representation is the one
/// ERTS' `IS_USMALL` (`big.h:76`) picks.
///
/// A small integer is correct exactly when the magnitude is `<= MAX_SMALL`, or
/// `<= MAX_SMALL + 1` for a negative value (i.e. down to `MIN_SMALL`); anything
/// else must be a bignum. The classification is asserted with `beam_term`'s own
/// predicates — a bignum where a small is correct would be just as wrong as the
/// reverse. The exact value is pinned through the fixture's own `e256/1` (see the
/// module docs for why an integration test cannot read a bignum directly).
fn expect_field(
    module: &Module,
    ctx: &mut MinimalContext,
    name: &str,
    input: &[u8],
    magnitude: u64,
    negative: bool,
) {
    let arg = bin(ctx, input);
    let value =
        run(module, name, &[arg], ctx).unwrap_or_else(|e| panic!("{name}({input:?}): {e:?}"));
    let max_small = MAX_SMALL as u64;
    let fits_small = magnitude <= max_small || (negative && magnitude == max_small + 1);
    if fits_small {
        assert!(
            is_small(value),
            "{name}({input:?}) = {value:?} must be a SMALL integer"
        );
    } else {
        assert!(
            is_bignum(value),
            "{name}({input:?}) = {value:?} must be a bignum"
        );
    }
    let encoded = run(module, "e256", &[value], ctx)
        .map(bytes)
        .unwrap_or_else(|e| panic!("e256({name}({input:?})): {e:?}"));
    assert_eq!(
        encoded,
        be256_parts(u128::from(magnitude), negative),
        "{name}({input:?})"
    );
}

/// Run a unary construction export and return the exact bytes of the result binary.
fn build_bytes(module: &Module, name: &str, arg: Eterm, ctx: &mut MinimalContext) -> Vec<u8> {
    let value = run(module, name, &[arg], ctx).unwrap_or_else(|e| panic!("{name}: {e:?}"));
    bytes(value)
}

/// A **bignum** argument term, produced by the fixture's own read path: this crate's
/// public API has no safe bignum builder either. The read assertion inside pins the
/// value against the oracle first, so the term is a trustworthy input; a wrong read
/// would make the construction assertion below fail.
fn bignum_arg(
    module: &Module,
    ctx: &mut MinimalContext,
    name: &str,
    input: &[u8],
    magnitude: u128,
) -> Eterm {
    assert!(magnitude > i64::MAX as u128, "{name}: not a bignum value");
    expect_int_parts(module, ctx, name, input, magnitude, false);
    let arg = bin(ctx, input);
    run(module, name, &[arg], ctx).unwrap_or_else(|e| panic!("{name}: {e:?}"))
}

/// `2^64` (a two-digit bignum): the 72-bit **little-endian** read of `<<0:64,1:8>>`.
fn two_pow_64(module: &Module, ctx: &mut MinimalContext) -> Eterm {
    bignum_arg(module, ctx, "g72le", &[0, 0, 0, 0, 0, 0, 0, 0, 1], 1u128 << 64)
}

/// `2^100` (a two-digit bignum): the 128-bit read of a field with bit 100 set.
fn two_pow_100(module: &Module, ctx: &mut MinimalContext) -> Eterm {
    bignum_arg(
        module,
        ctx,
        "g128",
        &[0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        1u128 << 100,
    )
}

// ---------------------------------------------------------------------------
// The fixture really emits the wide forms
// ---------------------------------------------------------------------------

/// Guard: the committed `.beam` still contains each wide form (so a recompiled
/// fixture cannot silently stop covering one, and the loader's lowering is exercised
/// for each). Every one of the fixture's wide reads is a `bs_match` `integer`
/// command — the compiler reserves the standalone `bs_get_integer2` for a *dynamic*
/// size — and each construction export is a `bs_create_bin` with a wide `integer`
/// segment.
#[test]
fn fixture_emits_the_wide_integer_forms() {
    let module = load();
    let (mut wide_match_commands, mut create_bins) = (0usize, 0usize);

    for function in &module.functions {
        for instr in &function.code {
            match instr {
                Instr::BsMatch { commands, .. } => {
                    wide_match_commands += wide_integer_commands(&module, commands);
                }
                Instr::BsCreateBin { .. } => create_bins += 1,
                _ => {}
            }
        }
    }

    assert_eq!(wide_match_commands, 12, "wide `bs_match` integer commands");
    assert_eq!(create_bins, 8, "`bs_create_bin` instructions");
}

/// The number of `integer` commands in a flattened `bs_match` command list whose
/// `Size * Unit` exceeds 64 bits. The list is the compiler's raw group layout
/// `[Name, Live, Flags, Size, Unit, Dst]`, so the size and unit of an `integer`
/// command sit at offsets 3 and 4.
fn wide_integer_commands(module: &Module, commands: &[Operand]) -> usize {
    let mut wide = 0;
    for (i, operand) in commands.iter().enumerate() {
        let Operand::Atom(atom) = operand else { continue };
        if module.atom_name(*atom) != Some("integer") {
            continue;
        }
        let size = match commands.get(i + 3) {
            Some(Operand::Integer(size)) => *size as u128,
            _ => 1,
        };
        let unit = match commands.get(i + 4) {
            Some(Operand::Integer(unit)) => *unit as u128,
            _ => 1,
        };
        if size * unit > 64 {
            wide += 1;
        }
    }
    wide
}

// ---------------------------------------------------------------------------
// Reading — 72-bit fields (a whole number of bytes)
// ---------------------------------------------------------------------------

#[test]
fn g72_reads_a_72_bit_big_endian_field() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: 1, 18591708106338011145 (0x010203040506070809), 4722366482869645213695
    // (a two-digit bignum, `2^72 - 1`).
    expect_int(&module, &mut ctx, "g72", &[0, 0, 0, 0, 0, 0, 0, 0, 1], 1);
    expect_int(
        &module,
        &mut ctx,
        "g72",
        &[1, 2, 3, 4, 5, 6, 7, 8, 9],
        18591708106338011145,
    );
    expect_int(&module, &mut ctx, "g72", &[0xFF; 9], 4722366482869645213695);
}

#[test]
fn g72s_reads_a_72_bit_signed_field() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: -1 for all-ones, 1 for `<<0:64,1:8>>` and for `<<1:72>>` (the same 9
    // bytes — a big-endian field's last bit is its least significant one).
    expect_int(&module, &mut ctx, "g72s", &[0xFF; 9], -1);
    expect_int(&module, &mut ctx, "g72s", &[0, 0, 0, 0, 0, 0, 0, 0, 1], 1);
    // Only the top bit set: the two's complement of `2^71`.
    expect_int(
        &module,
        &mut ctx,
        "g72s",
        &[0x80, 0, 0, 0, 0, 0, 0, 0, 0],
        -2361183241434822606848,
    );
}

#[test]
fn g72le_reads_a_72_bit_little_endian_field() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: 18446744073709551616 (2^64) for `<<0:64,1:8>>` / `<<1:72>>`, and
    // 166599134359138271745 (0x09_0807060504030201) for the digit sequence.
    expect_int(
        &module,
        &mut ctx,
        "g72le",
        &[0, 0, 0, 0, 0, 0, 0, 0, 1],
        1i128 << 64,
    );
    expect_int(
        &module,
        &mut ctx,
        "g72le",
        &[1, 2, 3, 4, 5, 6, 7, 8, 9],
        166599134359138271745,
    );
    // Only the first byte set: the least significant byte of the field.
    expect_int(&module, &mut ctx, "g72le", &[0x80, 0, 0, 0, 0, 0, 0, 0, 0], 128);
}

// ---------------------------------------------------------------------------
// Reading — a 128-bit field and a `size * unit` spelling
// ---------------------------------------------------------------------------

#[test]
fn g128_reads_a_128_bit_field() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: 1 for `<<1:128>>` (`<<0:127,1:1>>` is a 128-bit subject whose last byte
    // is `0x01`) and 1267650600228229401496703205376 (2^100) for the bit-100 field.
    expect_int(
        &module,
        &mut ctx,
        "g128",
        &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        1,
    );
    expect_int(
        &module,
        &mut ctx,
        "g128",
        &[0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        1i128 << 100,
    );
    // The largest unsigned 128-bit field value: 2^127 (the top bit set), which needs a
    // bignum whose top digit has its own high bit set.
    expect_int_parts(
        &module,
        &mut ctx,
        "g128",
        &[0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        1u128 << 127,
        false,
    );
}

#[test]
fn g9u8_reads_a_72_bit_field_spelled_size_times_unit() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: 18591708106338011145 — `9/unit:8` is the same 72-bit field as `72`.
    expect_int(
        &module,
        &mut ctx,
        "g9u8",
        &[1, 2, 3, 4, 5, 6, 7, 8, 9],
        18591708106338011145,
    );
    expect_int(&module, &mut ctx, "g9u8", &[0, 0, 0, 0, 0, 0, 0, 0, 1], 1);
}

// ---------------------------------------------------------------------------
// Reading — several wide fields in one command list, and the `case` forms
// ---------------------------------------------------------------------------

#[test]
fn seq_reads_two_integer_commands_in_one_command_list() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: seq(<<1:72,2:8,3:8>>) = {1,2} — a 72-bit `integer` command followed by
    // an 8-bit one in the *same* flattened `bs_match` list.
    let arg = bin(&mut ctx, &[0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3]);
    let result = run(&module, "seq", &[arg], &mut ctx).expect("seq");
    assert_eq!(terms::tuple_arity(result).expect("tuple"), 2);
    let first = terms::tuple_element(result, 0).expect("element 0");
    let second = terms::tuple_element(result, 1).expect("element 1");
    let encoded = bytes(run(&module, "e256", &[first], &mut ctx).expect("e256"));
    assert_eq!(encoded, be256_parts(1, false));
    assert_eq!(second, make_small(2));
}

#[test]
fn the_case_wrapped_reads_return_the_same_wide_values() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: the `c_*` group wraps each pattern in `case ... end`, so the same field
    // is read through the `bs_match` command list inside a `case` (with
    // `ensure_exactly` instead of `ensure_at_least`).
    expect_int(&module, &mut ctx, "c_g72", &[0xFF; 9], 4722366482869645213695);
    expect_int(&module, &mut ctx, "c_g72", &[0, 0, 0, 0, 0, 0, 0, 0, 1], 1);
    expect_int(&module, &mut ctx, "c_g72s", &[0xFF; 9], -1);
    expect_int(
        &module,
        &mut ctx,
        "c_g72le",
        &[0, 0, 0, 0, 0, 0, 0, 0, 1],
        1i128 << 64,
    );
}

#[test]
fn a_wide_field_that_does_not_fit_raises() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // The oracle raises `error:{case_clause,Subject}` when the subject is not exactly
    // 72 bits. `terms::make_heap_binary` is byte-granular, so the subjects below are
    // byte binaries (8 or 10 bytes) — one byte short/too long — rather than the
    // probe's 71-bit literal; the observable (`bs_match`'s `ensure_exactly 72`
    // failing into the `case_end` handler) is identical, only the subject in the
    // reason differs.
    expect_raise(&module, "c_g72", &[0; 8], &mut ctx);
    expect_raise(&module, "c_g72", &[0; 10], &mut ctx);
    expect_raise(&module, "c_g72le", &[0, 0, 0, 0, 0, 0, 0, 1], &mut ctx);
}

// ---------------------------------------------------------------------------
// Construction — `bs_create_bin`'s wide `integer` segment
// ---------------------------------------------------------------------------

#[test]
fn e72_writes_a_72_bit_big_endian_segment() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: <<0,0,0,0,0,0,0,0,1>>, <<1,0,0,0,0,0,0,0,0>> (2^64), nine zero bytes
    // (2^100 masked into 72 bits) and the two's complement of -5.
    let p64 = two_pow_64(&module, &mut ctx);
    let p100 = two_pow_100(&module, &mut ctx);
    assert_eq!(
        build_bytes(&module, "e72", make_small(1), &mut ctx),
        [0, 0, 0, 0, 0, 0, 0, 0, 1]
    );
    assert_eq!(
        build_bytes(&module, "e72", p64, &mut ctx),
        [1, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(build_bytes(&module, "e72", p100, &mut ctx), [0; 9]);
    assert_eq!(
        build_bytes(&module, "e72", make_small(-5), &mut ctx),
        [255, 255, 255, 255, 255, 255, 255, 255, 251]
    );
}

#[test]
fn e72s_writes_a_72_bit_big_endian_segment() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: the unsigned encoding for 1, and nine 0xFF bytes for -1.
    assert_eq!(
        build_bytes(&module, "e72s", make_small(1), &mut ctx),
        [0, 0, 0, 0, 0, 0, 0, 0, 1]
    );
    assert_eq!(build_bytes(&module, "e72s", make_small(-1), &mut ctx), [0xFF; 9]);
}

#[test]
fn e72le_writes_a_72_bit_little_endian_segment() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: <<5,0,0,0,0,0,0,0,0>>, <<0,0,0,0,0,0,0,0,1>> (2^64) and the little-endian
    // two's complement of -5 (the least significant byte first).
    let p64 = two_pow_64(&module, &mut ctx);
    assert_eq!(
        build_bytes(&module, "e72le", make_small(5), &mut ctx),
        [5, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        build_bytes(&module, "e72le", p64, &mut ctx),
        [0, 0, 0, 0, 0, 0, 0, 0, 1]
    );
    assert_eq!(
        build_bytes(&module, "e72le", make_small(-5), &mut ctx),
        [251, 255, 255, 255, 255, 255, 255, 255, 255]
    );
}

#[test]
fn e128_and_e256_write_wide_segments() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: e128(1 bsl 100) = <<0,0,0,16,0,...,0>> (16 bytes), e128(-1) = 16 0xFF
    // bytes, e256(1) = 31 zero bytes then 1, e256(-1) = 32 0xFF bytes.
    let p100 = two_pow_100(&module, &mut ctx);
    let mut expected_128 = vec![0u8; 16];
    expected_128[3] = 0x10;
    assert_eq!(build_bytes(&module, "e128", p100, &mut ctx), expected_128);
    assert_eq!(build_bytes(&module, "e128", make_small(-1), &mut ctx), [0xFF; 16]);
    let mut expected_256 = vec![0u8; 32];
    expected_256[31] = 1;
    assert_eq!(build_bytes(&module, "e256", make_small(1), &mut ctx), expected_256);
    assert_eq!(build_bytes(&module, "e256", make_small(-1), &mut ctx), [0xFF; 32]);
}

#[test]
fn e9u8_writes_a_72_bit_segment_spelled_size_times_unit() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // Oracle: `9/unit:8` is the same 72-bit segment as `72`.
    assert_eq!(
        build_bytes(&module, "e9u8", make_small(1), &mut ctx),
        [0, 0, 0, 0, 0, 0, 0, 0, 1]
    );
}

// ---------------------------------------------------------------------------
// Non-byte-aligned fields: round-tripped through the fixture's own constructors
// ---------------------------------------------------------------------------

/// `e68`/`g68` and `e68le`/`g68le` round-trip a 68-bit field (see the module docs:
/// the partial-bit subject cannot be built or read from the test side, so the
/// fixture's own encoder produces it). The byte-aligned `g72`/`e72` pair pins the
/// shared field convention absolutely, so a cancelling bug cannot hide here.
#[test]
fn non_byte_aligned_wide_fields_round_trip_through_the_fixture() {
    let module = load();
    let mut ctx = MinimalContext::new();

    // A 68-bit big-endian field of a small integer: `g68(e68(N)) == N`.
    for n in [0i64, 1, 5, 255] {
        let subject = run(&module, "e68", &[make_small(n)], &mut ctx).expect("e68");
        let value = run(&module, "g68", &[subject], &mut ctx).expect("g68");
        assert_eq!(value, make_small(n), "g68(e68({n}))");
    }
    // A 68-bit little-endian field likewise: `g68le(e68le(N)) == N`.
    for n in [1i64, 5, 9, 0xFF] {
        let subject = run(&module, "e68le", &[make_small(n)], &mut ctx).expect("e68le");
        let value = run(&module, "g68le", &[subject], &mut ctx).expect("g68le");
        assert_eq!(value, make_small(n), "g68le(e68le({n}))");
    }
    // The 4-bit trailing group and a bignum value: `g68(e68(2^64)) == 2^64` and the
    // little-endian pair (`e68le(2^64)` places the bit in the last 4-bit group).
    let p64 = two_pow_64(&module, &mut ctx);
    let subject = run(&module, "e68", &[p64], &mut ctx).expect("e68");
    let value = run(&module, "g68", &[subject], &mut ctx).expect("g68");
    let encoded = bytes(run(&module, "e256", &[value], &mut ctx).expect("e256"));
    assert_eq!(encoded, be256_parts(1u128 << 64, false), "g68(e68(2^64))");
    let subject = run(&module, "e68le", &[p64], &mut ctx).expect("e68le");
    let value = run(&module, "g68le", &[subject], &mut ctx).expect("g68le");
    let encoded = bytes(run(&module, "e256", &[value], &mut ctx).expect("e256"));
    assert_eq!(encoded, be256_parts(1u128 << 64, false), "g68le(e68le(2^64))");
}

// ---------------------------------------------------------------------------
// The byte-aligned read/write pair round-trips exactly
// ---------------------------------------------------------------------------

#[test]
fn the_wide_read_and_write_paths_round_trip() {
    let module = load();
    let mut ctx = MinimalContext::new();
    // `e72(g72(B)) == B` for a byte-aligned 72-bit big-endian field, and the
    // little-endian pair likewise.
    let field = [1u8, 2, 3, 4, 5, 6, 7, 8, 9];
    let read = run(&module, "g72", &[bin(&mut ctx, &field)], &mut ctx).expect("g72");
    assert_eq!(build_bytes(&module, "e72", read, &mut ctx), field);
    let read = run(&module, "g72le", &[bin(&mut ctx, &field)], &mut ctx).expect("g72le");
    assert_eq!(build_bytes(&module, "e72le", read, &mut ctx), field);
    let read = run(&module, "g72s", &[bin(&mut ctx, &field)], &mut ctx).expect("g72s");
    assert_eq!(build_bytes(&module, "e72s", read, &mut ctx), field);
}

// ---------------------------------------------------------------------------
// `bs_get_integer2` with a wide, *dynamic* size (synthetic modules)
// ---------------------------------------------------------------------------

/// Build a one-function synthetic module (the loader is not involved), mirroring the
/// scaffolding of `tests/opcodes.rs`.
///
/// This fixture — like every other committed one — compiles its wide reads into
/// `bs_match` commands, so the standalone `bs_get_integer2` opcode (which the compiler
/// emits only for a **dynamic** size) has no `.beam` coverage; these modules run it
/// against the exact same 9-byte subjects as the fixture tests above.
fn synthetic(code: Vec<Instr>) -> (Module, Label) {
    let mut module = Module::default();
    let name = module.atom_table.intern("synthetic");
    module.name = name;
    let mut labels: BTreeMap<Label, usize> = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    let entry = labels.keys().next().copied().unwrap_or(Label(0));
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: name,
        name: module.atom_table.intern("g"),
        arity: 1,
        code,
        blocks: Vec::new(),
        labels,
    });
    (module, entry)
}

/// `g(Bin) -> <<X:Size/Flags, _/binary>> = Bin, X.` as the raw opcodes: enter a match
/// context over `x(0)`, read the field into `x(0)`, return it.
///
/// Unlike the compiled `bs_match` form, `bs_get_integer2` has **no** `ensure_at_least`
/// (and hence no unit/alignment requirement), so a wider-than-byte-aligned subject is
/// read directly — this is also what lets the non-byte-aligned 68-bit field be tested
/// on the match side despite the byte-granular subject builder.
fn get_integer_module(size: i64, unit: u32, flags: u32) -> (Module, Label) {
    synthetic(vec![
        Instr::Label(Label(1)),
        Instr::BsStartMatch3 {
            fail: NO_FAIL,
            bin: Operand::X(0),
            live: 1,
            dst: Operand::X(1),
        },
        Instr::BsGetInteger2 {
            fail: NO_FAIL,
            ctx: Operand::X(1),
            live: 1,
            size: Operand::Integer(size),
            unit,
            flags,
            dst: Operand::X(0),
        },
        Instr::Return,
    ])
}

#[test]
fn bs_get_integer2_executes_a_wide_field() {
    let fixture = load();
    let mut ctx = MinimalContext::new();

    // A 72-bit unsigned big-endian field whose value is still a small integer (1) —
    // the direct assertion, no round trip.
    let (module, entry) = get_integer_module(72, 1, 0);
    let arg = bin(&mut ctx, &[0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let mut machine = Machine::new();
    let value = machine
        .run_label(&module, entry, 1, &[arg], &mut ctx)
        .expect("bs_get_integer2");
    assert_eq!(value, make_small(1));

    // A 72-bit **signed** field of all ones: `-1` through the two's-complement path.
    let (module, entry) = get_integer_module(72, 1, BSF_SIGNED);
    let arg = bin(&mut ctx, &[0xFF; 9]);
    let mut machine = Machine::new();
    let value = machine
        .run_label(&module, entry, 1, &[arg], &mut ctx)
        .expect("bs_get_integer2");
    assert_eq!(value, make_small(-1));

    // A 72-bit **little-endian** field of `<<0:64,1:8>>`: `2^64`, a bignum, pinned
    // through the fixture's own 256-bit encoder (as in the read tests above).
    let (module, entry) = get_integer_module(72, 1, BSF_LITTLE);
    let arg = bin(&mut ctx, &[0, 0, 0, 0, 0, 0, 0, 0, 1]);
    let mut machine = Machine::new();
    let value = machine
        .run_label(&module, entry, 1, &[arg], &mut ctx)
        .expect("bs_get_integer2");
    let encoded = bytes(run(&fixture, "e256", &[value], &mut ctx).expect("e256"));
    assert_eq!(encoded, be256_parts(1u128 << 64, false));

    // A 68-bit big-endian field with a 4-bit trailing group (not a whole number of
    // bytes): `0x123456789ABCDEF0F`, read straight from the 9-byte subject (the
    // `bs_get_integer2` arm has no `ensure_at_least` to reject the partial group).
    let (module, entry) = get_integer_module(68, 1, 0);
    let arg = bin(&mut ctx, &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0xF0]);
    let mut machine = Machine::new();
    let value = machine
        .run_label(&module, entry, 1, &[arg], &mut ctx)
        .expect("bs_get_integer2");
    let encoded = bytes(run(&fixture, "e256", &[value], &mut ctx).expect("e256"));
    assert_eq!(encoded, be256_parts(20988295479420645135, false));

    // A 68-bit **little-endian** field (the same subject): `5 << 64`.
    let (module, entry) = get_integer_module(68, 1, BSF_LITTLE);
    let arg = bin(&mut ctx, &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0xF0]);
    let mut machine = Machine::new();
    let value = machine
        .run_label(&module, entry, 1, &[arg], &mut ctx)
        .expect("bs_get_integer2");
    let encoded = bytes(run(&fixture, "e256", &[value], &mut ctx).expect("e256"));
    assert_eq!(encoded, be256_parts(294057678491205645330, false));

    // A 65-bit field (`2^65 - 1`): just over one machine word. Only the first 65 bits
    // of the 9-byte subject are read.
    let (module, entry) = get_integer_module(65, 1, 0);
    let arg = bin(&mut ctx, &[0xFF; 9]);
    let mut machine = Machine::new();
    let value = machine
        .run_label(&module, entry, 1, &[arg], &mut ctx)
        .expect("bs_get_integer2");
    let encoded = bytes(run(&fixture, "e256", &[value], &mut ctx).expect("e256"));
    assert_eq!(encoded, be256_parts(36893488147419103231, false));

    // A 128-bit field: `2^100` (a two-digit bignum).
    let (module, entry) = get_integer_module(128, 1, 0);
    let arg = bin(&mut ctx, &[0, 0, 0, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let mut machine = Machine::new();
    let value = machine
        .run_label(&module, entry, 1, &[arg], &mut ctx)
        .expect("bs_get_integer2");
    let encoded = bytes(run(&fixture, "e256", &[value], &mut ctx).expect("e256"));
    assert_eq!(encoded, be256_parts(1u128 << 100, false));
}

#[test]
fn bs_get_integer2_fails_a_short_wide_field() {
    let mut ctx = MinimalContext::new();
    // `fail = NO_FAIL`: a field the subject cannot hold is a hard match failure. The
    // engine's `fail_step` raises `{badmatch, Subject}` (a `BeamError::BadMatch`) when
    // the fail label is `NO_FAIL` — the same shape `g72` uses.
    let (module, entry) = get_integer_module(72, 1, 0);
    let arg = bin(&mut ctx, &[0; 8]);
    let mut machine = Machine::new();
    let result = machine.run_label(&module, entry, 1, &[arg], &mut ctx);
    assert!(
        matches!(result, Err(BeamError::BadMatch)),
        "a short wide field should fail the match, got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Sanity: the fixture's exports are the ones the tests rely on
// ---------------------------------------------------------------------------

#[test]
fn every_wide_export_is_present() {
    // A guard against a stale/rebuilt fixture: each export the tests rely on must
    // resolve to an atom in the module (a missing one fails the lookup).
    let module = load();
    for name in [
        "g72", "g72s", "g72le", "g68", "g68le", "g65", "g128", "g9u8", "seq", "c_g72", "c_g72s",
        "c_g72le", "e72", "e72s", "e72le", "e68", "e68le", "e128", "e9u8", "e256",
    ] {
        let _: Atom = export_atom(&module, name);
    }
}

#[test]
fn every_at_most_64_bit_export_is_present() {
    let module = load_small();
    for name in [
        "u9", "u12", "u17", "u20", "u36", "u59", "u60", "u61", "u62", "u64", "u64x", "s59", "s60",
        "s61", "s64", "u9le", "u12le", "u17le", "u20le", "u36le", "u60le", "u64le", "s64le",
        "c_u64", "e256",
    ] {
        let _: Atom = export_atom(&module, name);
    }
}

// ---------------------------------------------------------------------------
// At most 64 bits: a value above `MAX_SMALL` still needs the exact integer
// (`bssmall.beam`)
// ---------------------------------------------------------------------------

/// The headline case: a legal **64-bit unsigned** field above `MAX_SMALL`
/// (`2^59-1`) must yield the exact bignum, not a typed error, while a value that
/// fits a small must stay a small.
#[test]
fn u64_reads_the_exact_bignum_above_max_small() {
    let module = load_small();
    let mut ctx = MinimalContext::new();

    // Oracle: `erl -noshell -eval '<<Y:64>> = <<16#F000000000000000:64>>,
    //        io:format("~p~n",[Y]), halt().'` → 17293822569102704640
    expect_field(
        &module,
        &mut ctx,
        "u64x",
        &[0xF0, 0, 0, 0, 0, 0, 0, 0],
        17293822569102704640,
        false,
    );
    // Oracle: `<<Y:64>> = <<16#8000000000000000:64>>` → 9223372036854775808
    // (the sign bit set, but the field is *unsigned*: `2^63`, not `-2^63`).
    expect_field(
        &module,
        &mut ctx,
        "u64x",
        &[0x80, 0, 0, 0, 0, 0, 0, 0],
        9223372036854775808,
        false,
    );
    // Oracle: `<<Y:64>> = <<16#FFFFFFFFFFFFFFFF:64>>` → 18446744073709551615
    // (`2^64-1`: the largest field a machine word can hold, a one-digit bignum).
    expect_field(
        &module,
        &mut ctx,
        "u64",
        &[0xFF; 8],
        18446744073709551615,
        false,
    );
    // Oracle: `u64(<<16#ABCDEF0123456789:64>>)` → 12379813738877118345.
    expect_field(
        &module,
        &mut ctx,
        "u64",
        &[0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89],
        12379813738877118345,
        false,
    );
    // The same read with `ensure_exactly 64` (`<<Y:64>>`): `0x07FFFFFFFFFFFFFF`
    // is exactly `MAX_SMALL`, so it must stay a SMALL — a bignum here would be a
    // divergence just as much as a small where a bignum belongs.
    // Oracle: `<<Y:64>> = <<16#07FFFFFFFFFFFFFF:64>>` → 576460752303423487
    expect_field(
        &module,
        &mut ctx,
        "u64x",
        &[0x07, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        576460752303423487,
        false,
    );
    // Oracle: `<<Y:64>> = <<1:64>>` → 1 (the small path is untouched).
    expect_field(
        &module,
        &mut ctx,
        "u64x",
        &[0, 0, 0, 0, 0, 0, 0, 1],
        1,
        false,
    );
}

/// The `MAX_SMALL`/`MIN_SMALL` boundary, at the widths where it matters.
///
/// A 59-bit unsigned field's maximum is exactly `MAX_SMALL` (`2^59-1`), so it can
/// **never** need a bignum; a 60-bit one crosses the line at `2^59`.
#[test]
fn the_max_small_boundary_is_a_small_and_the_next_value_is_a_bignum() {
    let module = load_small();
    let mut ctx = MinimalContext::new();

    // Oracle: `erl -noshell -eval '<<Y:59>> = <<16#7FFFFFFFFFFFFFF:59>>,
    //        io:format("~p~n",[Y]), halt().'` → 576460752303423487 (a SMALL).
    // Read the first 59 bits of an all-ones 8-byte subject.
    expect_field(&module, &mut ctx, "u59", &[0xFF; 8], 576460752303423487, false);
    // Oracle: `<<Y:60>> = <<16#800000000000000:60>>` → 576460752303423488
    // (`MAX_SMALL + 1`: the first value at 60 bits that needs a bignum).
    expect_field(
        &module,
        &mut ctx,
        "u60",
        &[0x80, 0, 0, 0, 0, 0, 0, 0],
        576460752303423488,
        false,
    );
    // Oracle: `«u60»(<<0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF>>)` read as a 60-bit
    // field → 1152921504606846975 (`2^60-1`, a bignum).
    expect_field(
        &module,
        &mut ctx,
        "u60",
        &[0xFF; 8],
        1152921504606846975,
        false,
    );
    // Oracle: `«u61»(<<0xFF;8>>)` → 2305843009213693951 (`2^61-1`).
    expect_field(
        &module,
        &mut ctx,
        "u61",
        &[0xFF; 8],
        2305843009213693951,
        false,
    );
    // Oracle: `«u62»(<<16#3FFFFFFFFFFFFFFF:64>>)` → 1152921504606846975.
    expect_field(
        &module,
        &mut ctx,
        "u62",
        &[0x3F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
        1152921504606846975,
        false,
    );
}

/// Signed fields go through the same small-vs-bignum decision: the magnitude of a
/// negative value may reach `MAX_SMALL + 1` (`MIN_SMALL`) and still be a small.
#[test]
fn signed_fields_use_the_same_small_vs_bignum_split() {
    let module = load_small();
    let mut ctx = MinimalContext::new();

    // Oracle: `erl -noshell -eval '<<Y:59/signed>> = <<16#FFFFFFFFFFFFFFFF:59>>,
    //        io:format("~p~n",[Y]), halt().'` → -1 (a SMALL; `signed` sign-extends
    // at 59 bits, so 59 one-bits are `-1`). The low 5 bits of the 8-byte subject
    // are not part of the 59-bit field.
    expect_field(&module, &mut ctx, "s59", &[0xFF; 8], 1, true);
    // Oracle: `<<Y:60/signed>> = <<16#800000000000000:60>>` → -576460752303423488
    // (`MIN_SMALL` = `-(MAX_SMALL + 1)`: negative but still a SMALL).
    expect_field(
        &module,
        &mut ctx,
        "s60",
        &[0x80, 0, 0, 0, 0, 0, 0, 0],
        576460752303423488,
        true,
    );
    // Oracle: `«s60»(<<16#40,0,0,0,0,0,0,0>>)` read as 60 signed bits →
    // 288230376151711744 (`2^58`, positive and a SMALL).
    expect_field(
        &module,
        &mut ctx,
        "s60",
        &[0x40, 0, 0, 0, 0, 0, 0, 0],
        288230376151711744,
        false,
    );
    // Oracle: `«s61»(<<16#40,0,...>>)` read as 61 signed bits →
    // 576460752303423488 (`2^59`, positive, a bignum).
    expect_field(
        &module,
        &mut ctx,
        "s61",
        &[0x40, 0, 0, 0, 0, 0, 0, 0],
        576460752303423488,
        false,
    );
    // Oracle: `«s61»(<<16#C0,0,...>>)` → -576460752303423488 (`-2^59` = `MIN_SMALL`,
    // a SMALL: sign-extending bit 60 makes the 61-bit field `-2^59`).
    expect_field(
        &module,
        &mut ctx,
        "s61",
        &[0xC0, 0, 0, 0, 0, 0, 0, 0],
        576460752303423488,
        true,
    );
    // Oracle: `<<Y:64/signed>> = <<16#8000000000000000:64>>` → -9223372036854775808
    // (`-2^63`, the most negative 64-bit value: a bignum).
    expect_field(
        &module,
        &mut ctx,
        "s64",
        &[0x80, 0, 0, 0, 0, 0, 0, 0],
        9223372036854775808,
        true,
    );
    // Oracle: `<<Y:64/signed>> = <<16#FFFFFFFFFFFFFFFF:64>>` → -1 (a SMALL).
    expect_field(&module, &mut ctx, "s64", &[0xFF; 8], 1, true);
}

/// Little-endian fields of 9 or more bits have **no** small-integer fast path in
/// ERTS (`erl_bits.c:280-313`), so they all take the general path — including the
/// non-byte-multiple widths the standard compiler emits. Before this fix those
/// returned a typed `unsupported` (the byte-wise `read_uint_le` requires a whole
/// number of bytes).
#[test]
fn non_byte_aligned_little_endian_fields_read_exactly() {
    let module = load_small();
    let mut ctx = MinimalContext::new();
    let probe = [0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89];

    // Oracle: `u12le(<<16#ABCDEF0123456789:64>>)` → 3243, i.e. the first 12 bits
    // read little-endian (`0xAB` low byte, the high nibble `0xC`).
    expect_field(&module, &mut ctx, "u12le", &probe, 3243, false);
    // Oracle: `u17le(<<16#ABCDEF0123456789:64>>)` → 118187.
    expect_field(&module, &mut ctx, "u17le", &probe, 118187, false);
    // Oracle: `u20le(<<16#ABCDEF0123456789:64>>)` → 970155.
    expect_field(&module, &mut ctx, "u20le", &probe, 970155, false);
    // Oracle: `u36le(<<16#ABCDEF0123456789:64>>)` → 8622427563 (a SMALL).
    expect_field(&module, &mut ctx, "u36le", &probe, 8622427563, false);
    // Oracle: `u9le(<<16#F0,0,...>>)` → 240 (9 bits = one whole byte plus one bit:
    // `0xF0` is the low byte and the 9th bit, the top bit of the next byte, is 0).
    expect_field(&module, &mut ctx, "u9le", &[0xF0, 0, 0, 0, 0, 0, 0, 0], 240, false);
    // Oracle: `u9le(<<16#ABCDEF0123456789:64>>)` → 427 (the 9th bit — the top bit
    // of the second byte `0xCD` — is set, so it contributes `2^8`).
    expect_field(&module, &mut ctx, "u9le", &probe, 427, false);
    // Oracle: `u17le(<<16#12,16#34,16#56,16#78,16#9A,16#BC,16#DE,16#F0>>)` → 13330.
    expect_field(
        &module,
        &mut ctx,
        "u17le",
        &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0],
        13330,
        false,
    );
}

/// A little-endian field can also need a bignum (`> MAX_SMALL`), and a
/// byte-aligned one still works.
#[test]
fn little_endian_fields_above_max_small_build_the_exact_bignum() {
    let module = load_small();
    let mut ctx = MinimalContext::new();

    // Oracle: `u60le(<<16#FF;8>>)` → 1152921504606846975 (`2^60-1`, a bignum).
    expect_field(
        &module,
        &mut ctx,
        "u60le",
        &[0xFF; 8],
        1152921504606846975,
        false,
    );
    // Oracle: `u60le(<<16#12,16#34,16#56,16#78,16#9A,16#BC,16#DE,16#F0>>)` →
    // 1143558727028585490 (a 60-bit little-endian field, a bignum).
    expect_field(
        &module,
        &mut ctx,
        "u60le",
        &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0],
        1143558727028585490,
        false,
    );
    // Oracle: `u64le(<<16#FFFFFFFFFFFFFFFF:64>>)` → 18446744073709551615.
    expect_field(
        &module,
        &mut ctx,
        "u64le",
        &[0xFF; 8],
        18446744073709551615,
        false,
    );
    // Oracle: `u64le(<<16#ABCDEF0123456789:64>>)` → 9900958322455989675.
    expect_field(
        &module,
        &mut ctx,
        "u64le",
        &[0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89],
        9900958322455989675,
        false,
    );
    // Oracle: `u64le(<<16#F0,0,...>>)` → 240 (a byte-aligned LE field, a SMALL).
    expect_field(
        &module,
        &mut ctx,
        "u64le",
        &[0xF0, 0, 0, 0, 0, 0, 0, 0],
        240,
        false,
    );
    // Oracle: `u60le(<<16#80,0,...>>)` → 128 (a SMALL).
    expect_field(
        &module,
        &mut ctx,
        "u60le",
        &[0x80, 0, 0, 0, 0, 0, 0, 0],
        128,
        false,
    );
    // Oracle: `s64le(<<16#C0,0,...>>)` → 192 (byte-aligned `little-signed`, positive).
    expect_field(
        &module,
        &mut ctx,
        "s64le",
        &[0xC0, 0, 0, 0, 0, 0, 0, 0],
        192,
        false,
    );
    // Oracle: `s64le(<<16#FF;8>>)` → -1 (byte-aligned `little-signed`).
    expect_field(&module, &mut ctx, "s64le", &[0xFF; 8], 1, true);
}

/// The standalone `bs_get_integer2` opcode (the compiler emits it only for a
/// **dynamic** size) takes the same fixed path: a 64-bit unsigned field above
/// `MAX_SMALL` is the exact bignum, and the in-range case stays a small.
#[test]
fn bs_get_integer2_reads_a_64_bit_field_above_max_small() {
    let fixture = load_small();
    let mut ctx = MinimalContext::new();

    // Oracle: `<<Y:64>> = <<16#F000000000000000:64>>` → 17293822569102704640.
    let (module, entry) = get_integer_module(64, 1, 0);
    let arg = bin(&mut ctx, &[0xF0, 0, 0, 0, 0, 0, 0, 0]);
    let mut machine = Machine::new();
    let value = machine
        .run_label(&module, entry, 1, &[arg], &mut ctx)
        .expect("bs_get_integer2");
    assert!(is_bignum(value), "{value:?} must be a bignum");
    let encoded = bytes(run(&fixture, "e256", &[value], &mut ctx).expect("e256"));
    assert_eq!(encoded, be256_parts(17293822569102704640, false));

    // Oracle: `<<Y:64>> = <<1:64>>` → 1 (in range, stays a SMALL).
    let arg = bin(&mut ctx, &[0, 0, 0, 0, 0, 0, 0, 1]);
    let mut machine = Machine::new();
    let value = machine
        .run_label(&module, entry, 1, &[arg], &mut ctx)
        .expect("bs_get_integer2");
    assert_eq!(value, make_small(1));

    // A 60-bit little-endian `bs_get_integer2` field above `MAX_SMALL`:
    // Oracle: `<<Y:60/little>> = <<16#FFFFFFFFFFFFFFFF:60>>`... the first 60 bits of
    // an all-ones subject read little-endian → 1152921504606846975.
    let (module, entry) = get_integer_module(60, 1, BSF_LITTLE);
    let arg = bin(&mut ctx, &[0xFF; 8]);
    let mut machine = Machine::new();
    let value = machine
        .run_label(&module, entry, 1, &[arg], &mut ctx)
        .expect("bs_get_integer2");
    assert!(is_bignum(value), "{value:?} must be a bignum");
    let encoded = bytes(run(&fixture, "e256", &[value], &mut ctx).expect("e256"));
    assert_eq!(encoded, be256_parts(1152921504606846975, false));
}