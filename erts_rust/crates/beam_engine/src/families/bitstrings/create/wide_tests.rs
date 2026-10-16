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

//! The `bs_create_bin` **`integer` segment arm at narrow field widths** (`nbits <= 64`).
//!
//! `erts_bs_put_integer_*` (`erl_bits.c:926`) masks the segment value into the field
//! width exactly as the wide field does (`fmt_big_be`/`fmt_big_le`, `erl_bits.c:724`/
//! `:811`), so a **bignum** value in a narrow field is truncated — never rejected.
//! This suite pins that routing: a bignum takes `push_wide_int` at every width, a
//! small keeps the machine-word path, and a genuine non-integer (a float, an atom) is
//! still the bare `badarg`. `signed` never changes a *built* field's bytes (the value
//! is its `nbits`-bit two's complement either way), which the oracle confirms
//! (`<<200:8>>` and `<<200:8/signed>>` are both `<<200>>`).
//!
//! Every expected byte below is the **live oracle's**, from the system OTP 29.1.1 /
//! erts-17.1:
//! ```text
//! X = (1 bsl 70) + 5,
//! {<<X:8>>, <<X:16>>, <<X:32>>, <<X:64>>, <<X:64/signed>>,
//!  <<(-X):8>>, <<(-X):16>>, <<(-X):32>>, <<(-X):64>>, <<(-X):64/signed>>,
//!  <<(-X):8/signed>>, <<X:64/little>>, <<X:64/little-signed>>, <<(-X):32/little>>,
//!  <<X:24>>, <<X:24/little>>, <<X:20>>, <<X:20/little>>, <<X:12>>, <<X:12/little>>,
//!  <<X:1>>, <<X:0>>, <<576460752303423488:64>>, <<7811896436376826984:64>>,
//!  <<X:70>>, <<(-X):70>>}
//! ⇒ {<<5>>, <<0,5>>, <<0,0,0,5>>, <<0,0,0,0,0,0,0,5>>, <<0,0,0,0,0,0,0,5>>,
//!    <<251>>, <<255,251>>, <<255,255,255,251>>,
//!    <<255,255,255,255,255,255,255,251>>, <<255,255,255,255,255,255,255,251>>,
//!    <<251>>, <<5,0,0,0,0,0,0,0>>, <<5,0,0,0,0,0,0,0>>, <<251,255,255,255>>,
//!    <<0,0,5>>, <<5,0,0>>, <<0,0,5:4>>, <<5,0,0:4>>, <<0,5:4>>, <<5,0:4>>,
//!    <<1:1>>, <<>>, <<8,0,0,0,0,0,0,0>>, <<"linkpath">>,
//!    <<0,0,0,0,0,0,0,0,5:6>>, <<255,255,255,255,255,255,255,255,59:6>>}
//! ```
//! and the failure classes from the same oracle: a float or atom value, a negative
//! size, and a zero-width float field are `{error,badarg}`; a positive bignum size
//! (`<<1:(1 bsl 60)>>`) and a `size*unit` overflow
//! (`<<1:576460752303423487/unit:256>>`) are `{error,system_limit}`.

use super::*;
use beam_common::BeamError;
use beam_loader::{LiteralArea, NO_FAIL};
use beam_term::make_small;

use crate::context::MinimalContext;

/// `X = (1 bsl 70) + 5` — the two `u64` digits (least significant first) of the
/// bignum the oracle table above uses. `2^70 = 2^64 * 2^6`.
const X_DIGITS: [u64; 2] = [5, 1 << 6];

/// The ETF payload of a flag-atom list such as `[little, signed]` (`LIST_EXT` over
/// `ATOM_EXT`s, tailed by `NIL_EXT`).
pub(super) fn flags_etf(names: &[&str]) -> Vec<u8> {
    let mut payload = vec![131u8, 108]; // VERSION_MAGIC, LIST_EXT
    payload.extend_from_slice(&(names.len() as u32).to_be_bytes());
    for name in names {
        payload.push(100); // ATOM_EXT
        payload.extend_from_slice(&(name.len() as u16).to_be_bytes());
        payload.extend_from_slice(name.as_bytes());
    }
    payload.push(106); // NIL_EXT
    payload
}

/// A module naming the `integer` segment type whose literal area holds `payloads`
/// (ETF), in `Operand::Literal(idx)` index order.
pub(super) fn literal_module(payloads: &[Vec<u8>]) -> Module {
    let mut module = Module::default();
    module.atom_table.intern("integer");
    if !payloads.is_empty() {
        let area =
            LiteralArea::decode(payloads, &mut module.atom_table).expect("the literals decode");
        module.literals = std::sync::Arc::new(area);
    }
    module
}

/// A module naming the `integer` segment type and — when `names` is non-empty —
/// holding the flag list `names` as its literal 0.
pub(super) fn seg_module(names: &[&str]) -> Module {
    if names.is_empty() {
        return literal_module(&[]);
    }
    literal_module(&[flags_etf(names)])
}

/// The flags operand for a segment: the flag-list literal, or no flags at all.
pub(super) fn flag_operand(names: &[&str]) -> Operand {
    if names.is_empty() {
        Operand::Nil
    } else {
        Operand::Literal(0)
    }
}

/// The flag list a `(little, signed)` case asks for.
pub(super) fn flag_names(little: bool, signed: bool) -> Vec<&'static str> {
    let mut names = Vec::new();
    if little {
        names.push("little");
    }
    if signed {
        names.push("signed");
    }
    names
}

/// A bignum term from `digits` (least significant first), asserted to be the shape
/// the arm's bignum route keys on (`raw::big_parts_safe`).
pub(super) fn bignum(
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    negative: bool,
    digits: &[u64],
) -> Eterm {
    let value = raw::make_big(machine, ctx, negative, digits).expect("a bignum term");
    assert!(
        raw::big_parts_safe(value).is_some(),
        "the fixture must be a bignum, not a small"
    );
    value
}

/// Build one `integer` segment `[type, index, unit, flags, value, size]` and run the
/// segment walk over it. `fail` is [`NO_FAIL`], so a segment failure raises (and,
/// with no live catch frame, yields `SegReason::typed`).
pub(super) fn build(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    unit: usize,
    flags: Operand,
    value: Operand,
    size: Operand,
) -> Result<std::result::Result<(Vec<u8>, usize), Step>> {
    let segments = [
        Operand::Atom(module.atom_table.lookup("integer").expect("the type atom")),
        Operand::Integer(1),
        Operand::Integer(unit as i64),
        flags,
        value,
        size,
    ];
    assemble(
        machine,
        module,
        &LabelMap::default(),
        NO_FAIL,
        &segments,
        ctx,
    )
}

/// A successful build's `(bytes, bit count)` for one bignum into a `nbits` field.
fn build_ok(
    nbits: usize,
    negative: bool,
    little: bool,
    signed: bool,
    digits: &[u64],
) -> (Vec<u8>, usize) {
    let names = flag_names(little, signed);
    let module = seg_module(&names);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let value = bignum(&mut machine, &mut ctx, negative, digits);
    machine.set_x(0, value).expect("x(0) in range");
    build(
        &module,
        &mut machine,
        &mut ctx,
        1,
        flag_operand(&names),
        Operand::X(0),
        Operand::Integer(nbits as i64),
    )
    .expect("assemble")
    .expect("the build succeeds")
}

/// The typed error a failing build raises with no live catch frame.
fn build_err(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    unit: usize,
    value: Operand,
    size: Operand,
) -> BeamError {
    build(module, machine, ctx, unit, Operand::Nil, value, size).expect_err("the build must fail")
}

/// A bignum masked into every narrow width, big- and little-endian, signed and
/// unsigned — the oracle table above, row by row.
#[test]
fn a_bignum_is_masked_into_a_narrow_integer_field_like_the_oracle() {
    // (nbits, negative, little, signed, oracle bytes). The last byte of a
    // non-byte-aligned field carries the field's trailing bits left-aligned, its
    // unused low bits zero.
    let cases: &[(usize, bool, bool, bool, &[u8])] = &[
        (8, false, false, false, &[5]),
        (16, false, false, false, &[0, 5]),
        (32, false, false, false, &[0, 0, 0, 5]),
        (64, false, false, false, &[0, 0, 0, 0, 0, 0, 0, 5]),
        (64, false, false, true, &[0, 0, 0, 0, 0, 0, 0, 5]),
        (8, true, false, false, &[251]),
        (8, true, false, true, &[251]),
        (16, true, false, false, &[255, 251]),
        (32, true, false, false, &[255, 255, 255, 251]),
        (
            64,
            true,
            false,
            false,
            &[255, 255, 255, 255, 255, 255, 255, 251],
        ),
        (
            64,
            true,
            false,
            true,
            &[255, 255, 255, 255, 255, 255, 255, 251],
        ),
        (64, false, true, false, &[5, 0, 0, 0, 0, 0, 0, 0]),
        (64, false, true, true, &[5, 0, 0, 0, 0, 0, 0, 0]),
        (32, true, true, false, &[251, 255, 255, 255]),
        (24, false, false, false, &[0, 0, 5]),
        (24, false, true, false, &[5, 0, 0]),
        (20, false, false, false, &[0, 0, 0x50]),
        (20, false, true, false, &[5, 0, 0x00]),
        (12, false, false, false, &[0, 0x50]),
        (12, false, true, false, &[5, 0x00]),
        (1, false, false, false, &[0x80]),
        (0, false, false, false, &[]),
        // The wide (`> 64` bit) path, unchanged, for contrast.
        (70, false, false, false, &[0, 0, 0, 0, 0, 0, 0, 0, 0x14]),
        (
            70,
            true,
            false,
            false,
            &[255, 255, 255, 255, 255, 255, 255, 255, 0xEC],
        ),
    ];
    for &(nbits, negative, little, signed, expected) in cases {
        let (bytes, out_bits) = build_ok(nbits, negative, little, signed, &X_DIGITS);
        assert_eq!(out_bits, nbits, "the field is exactly {nbits} bits");
        assert_eq!(
            bytes, expected,
            "X = (1 bsl 70) + 5, negative={negative}, little={little}, signed={signed}, \
             {nbits} bits"
        );
    }
}

/// Single-digit bignums just above `MAX_SMALL` (`2^59 - 1`) — the vectors the oracle
/// table names, plus a bignum whose digits are one machine word.
#[test]
fn a_single_digit_bignum_masks_into_a_64_bit_field_like_the_oracle() {
    // 2^59 = 576460752303423488 is a bignum (MAX_SMALL is 2^59 - 1).
    assert_eq!(
        build_ok(64, false, false, false, &[1 << 59]),
        (vec![8, 0, 0, 0, 0, 0, 0, 0], 64)
    );
    // 7811896436376826984 = 0x6C696E6B70617468 = "linkpath".
    assert_eq!(
        build_ok(64, false, false, false, &[0x6C69_6E6B_7061_7468]),
        (b"linkpath".to_vec(), 64)
    );
    // The same bignum in a narrow byte field keeps its low byte.
    assert_eq!(build_ok(8, false, false, false, &[1 << 59]), (vec![0], 8));
}

/// A bignum **literal** masks into a narrow field the same way — the shape `erlc`
/// actually emits for a large `<<Int:Size>>` constant (`beam_loader::lower`'s
/// `rewrite_wide_integers` rewrites the wide operand to a synthesized bignum literal,
/// whose term carries `TAG_LITERAL_PTR`).
#[test]
fn a_bignum_literal_masks_into_a_narrow_integer_field_like_the_oracle() {
    // `SMALL_BIG_EXT` (110) of magnitude 2^59 = 576460752303423488 (8 little-endian
    // digits, positive sign): `<<576460752303423488:64>>` = <<8,0,0,0,0,0,0,0>> and
    // `<<576460752303423488:8>>` = <<0>>.
    let big = vec![131, 110, 8, 0, 0, 0, 0, 0, 0, 0, 0, 8];
    for (nbits, expected) in [(64usize, vec![8u8, 0, 0, 0, 0, 0, 0, 0]), (8, vec![0])] {
        let module = literal_module(&[big.clone()]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let out = build(
            &module,
            &mut machine,
            &mut ctx,
            1,
            Operand::Nil,
            Operand::Literal(0),
            Operand::Integer(nbits as i64),
        )
        .expect("assemble")
        .expect("the build succeeds");
        assert_eq!(out, (expected, nbits), "a bignum literal, {nbits} bits");
    }
}

/// The field width may come from a runtime `size(Expr)` operand (an x-register), not
/// only a literal in the segment list.
#[test]
fn a_bignum_masks_into_a_runtime_sized_narrow_field_like_the_oracle() {
    for (little, expected) in [(false, &[0u8, 0, 5][..]), (true, &[5u8, 0, 0][..])] {
        // `S = 24, <<X:S>>` / `<<X:S/little>>`.
        let names = flag_names(little, false);
        let module = seg_module(&names);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let value = bignum(&mut machine, &mut ctx, false, &X_DIGITS);
        machine.set_x(0, value).expect("x(0) in range");
        machine.set_x(1, make_small(24)).expect("x(1) in range");
        let out = build(
            &module,
            &mut machine,
            &mut ctx,
            1,
            flag_operand(&names),
            Operand::X(0),
            Operand::X(1),
        )
        .expect("assemble")
        .expect("the build succeeds");
        assert_eq!(out, (expected.to_vec(), 24), "little={little}");
    }
}

/// The bignum route did **not** swallow the genuine non-integer: a float and an atom
/// are still the bare `badarg`, at a narrow width and at width zero.
#[test]
fn a_non_integer_operand_is_still_badarg_like_the_oracle() {
    // `<<1.5:8>>`, `<<foo:8>>` and `<<1.5:0>>` are all `{error,badarg}`.
    let mut module = seg_module(&[]);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let float = crate::terms::test_support::make_float(&mut ctx, 1.5).expect("a float");
    machine.set_x(0, float).expect("x(0) in range");
    for nbits in [8, 0] {
        let err = build_err(
            &module,
            &mut machine,
            &mut ctx,
            1,
            Operand::X(0),
            Operand::Integer(nbits),
        );
        assert_eq!(
            err.as_atom(),
            Some("badarg"),
            "a float value in a {nbits}-bit field: {err:?}"
        );
    }
    let atom = Operand::Atom(module.atom_table.intern("foo"));
    let err = build_err(
        &module,
        &mut machine,
        &mut ctx,
        1,
        atom,
        Operand::Integer(8),
    );
    assert_eq!(err.as_atom(), Some("badarg"), "an atom value: {err:?}");
}

/// The size/unit failure classes the oracle reports, unchanged by the bignum route.
#[test]
fn the_size_and_unit_failure_classes_are_the_oracles() {
    let module = seg_module(&[]);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    // A negative size: `<<X:(-1)>>` is `{error,badarg}`.
    let err = build_err(
        &module,
        &mut machine,
        &mut ctx,
        1,
        Operand::X(0),
        Operand::Integer(-1),
    );
    assert_eq!(err.as_atom(), Some("badarg"), "a negative size: {err:?}");
    // A positive bignum size: `<<1:(1 bsl 60)>>` is `{error,system_limit}`.
    let size = crate::terms::test_support::make_integer(&mut ctx, 1i64 << 60).expect("a bignum");
    machine.set_x(1, size).expect("x(1) in range");
    let err = build_err(
        &module,
        &mut machine,
        &mut ctx,
        1,
        Operand::Integer(1),
        Operand::X(1),
    );
    assert_eq!(
        err.as_atom(),
        Some("system_limit"),
        "a positive bignum size: {err:?}"
    );
    // A `size * unit` overflow at the small-value maximum:
    // `<<1:576460752303423487/unit:256>>` is `{error,system_limit}`.
    let err = build_err(
        &module,
        &mut machine,
        &mut ctx,
        256,
        Operand::Integer(1),
        Operand::Integer(576460752303423487),
    );
    assert_eq!(
        err.as_atom(),
        Some("system_limit"),
        "a size*unit overflow: {err:?}"
    );
}
