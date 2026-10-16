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

//! The `bs_create_bin` **`float` segment arm** at every accepted width, with a
//! **bignum** value.
//!
//! C's `erts_bs_put_float` (`erl_bits.c:1341`) accepts a float, a **small integer**
//! *and* a **bignum** (`is_big(arg)` -> `big_to_double`, `:1371` for 64 bits and
//! `:1404` for 32 bits); `big_to_double`'s `< 0` return — a magnitude too large for
//! a finite double — is the bare `badarg` (`c_p->fvalue = am_no_float`, `:1373`).
//! The engine used to reject every bignum with `badarg`, a wrong result; this suite
//! pins the fixed behaviour: a bignum's nearest finite double is encoded, and only a
//! magnitude that overflows a finite double still fails.
//!
//! Every expected byte vector is the **live oracle's** (system OTP 29.1.1 /
//! erts-17.1), from a compiled helper module:
//! ```text
//! V = 12345678901234567890,
//! {<<V/float>>, <<V/float-little>>, <<V:32/float>>, <<V:32/float-little>>,
//!  <<V:16/float>>, <<V:16/float-little>>}
//! ⇒ {<<67,229,106,149,49,157,99,225>>, <<225,99,157,49,149,106,229,67>>,
//!    <<95,43,84,170>>, <<170,84,43,95>>, <<124,0>>, <<0,124>>}
//! {<<(-V)/float>>, <<(-V)/float-little>>, <<(-V):32/float>>,
//!  <<(-V):32/float-little>>, <<(-V):16/float>>, <<(-V):16/float-little>>}
//! ⇒ {<<195,229,106,149,49,157,99,225>>, <<225,99,157,49,149,106,229,195>>,
//!    <<223,43,84,170>>, <<170,84,43,223>>, <<252,0>>, <<0,252>>}
//! <<(1 bsl 1023)/float>> ⇒ <<127,224,0,0,0,0,0,0>>
//! <<(1 bsl 1024)/float>> ⇒ {error,badarg}   (1050 and 2000 likewise)
//! <<(-(1 bsl 1024))/float>> ⇒ {error,badarg}
//! <<576460752303423488/float>> ⇒ <<67,160,0,0,0,0,0,0>>
//! <<((1 bsl 60) + 1)/float>>   ⇒ <<67,176,0,0,0,0,0,0>>
//! ```

use super::*;
use beam_loader::{LiteralArea, NO_FAIL};
use beam_term::make_small;

use crate::context::MinimalContext;

use super::wide_tests::flags_etf;

/// `12345678901234567890` — a single-limb bignum (well above `MAX_SMALL`, `2^59 - 1`)
/// whose bits the oracle vectors above come from.
const BIG_FIT: [u64; 1] = [12345678901234567890];

/// A module naming the `float` segment type whose literal area holds the flag list
/// `names` (ETF) as literal 0 when `names` is non-empty.
fn float_module(names: &[&str]) -> Module {
    let mut module = Module::default();
    module.atom_table.intern("float");
    if !names.is_empty() {
        let area = LiteralArea::decode(&[flags_etf(names)], &mut module.atom_table)
            .expect("the flag literal decodes");
        module.literals = std::sync::Arc::new(area);
    }
    module
}

/// The flags operand for a segment: the flag-list literal, or no flags at all.
fn flag_operand(names: &[&str]) -> Operand {
    if names.is_empty() {
        Operand::Nil
    } else {
        Operand::Literal(0)
    }
}

/// Build one `float` segment `[type, index, unit, flags, value, size]` and run the
/// segment walk over it. `fail` is [`NO_FAIL`], so a segment failure raises (and,
/// with no live catch frame, yields the typed reason error).
fn build_float(
    module: &Module,
    machine: &mut Machine,
    ctx: &mut MinimalContext,
    unit: usize,
    flags: Operand,
    value: Operand,
    size: Operand,
) -> Result<std::result::Result<(Vec<u8>, usize), Step>> {
    let segments = [
        Operand::Atom(module.atom_table.lookup("float").expect("the type atom")),
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

/// A bignum term from `digits` (least significant first), asserted to be the shape
/// the arm's bignum route keys on.
fn bignum(
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

/// The little-endian `u64` limbs of `2^exponent` (`exponent / 64 + 1` limbs, the
/// most significant holding the single set bit).
fn power_of_two(exponent: usize) -> Vec<u64> {
    let mut digits = vec![0u64; exponent / 64 + 1];
    digits[exponent / 64] = 1u64 << (exponent % 64);
    digits
}

/// A bignum encodes as its nearest finite double at every accepted width and
/// endianness — the oracle table above, row by row.
#[test]
fn a_bignum_float_segment_builds_like_the_oracle() {
    // (nbits, little, negative, oracle bytes). `unit` is 1, so `size == nbits`.
    let cases: &[(usize, bool, bool, &[u8])] = &[
        (64, false, false, &[67, 229, 106, 149, 49, 157, 99, 225]),
        (64, true, false, &[225, 99, 157, 49, 149, 106, 229, 67]),
        (32, false, false, &[95, 43, 84, 170]),
        (32, true, false, &[170, 84, 43, 95]),
        (16, false, false, &[124, 0]),
        (16, true, false, &[0, 124]),
        (64, false, true, &[195, 229, 106, 149, 49, 157, 99, 225]),
        (64, true, true, &[225, 99, 157, 49, 149, 106, 229, 195]),
        (32, false, true, &[223, 43, 84, 170]),
        (32, true, true, &[170, 84, 43, 223]),
        (16, false, true, &[252, 0]),
        (16, true, true, &[0, 252]),
    ];
    for &(nbits, little, negative, expected) in cases {
        let names: &[&str] = if little { &["little"] } else { &[] };
        let module = float_module(names);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let value = bignum(&mut machine, &mut ctx, negative, &BIG_FIT);
        machine.set_x(0, value).expect("x(0) in range");
        let out = build_float(
            &module,
            &mut machine,
            &mut ctx,
            1,
            flag_operand(names),
            Operand::X(0),
            Operand::Integer(nbits as i64),
        )
        .expect("assemble")
        .expect("the build succeeds");
        assert_eq!(
            out,
            (expected.to_vec(), nbits),
            "negative={negative}, little={little}, {nbits} bits"
        );
    }
}

/// A single-limb bignum just above `MAX_SMALL` — the `bitlen > 53` general path of
/// `big_to_double`, not the `n == 1 && bitlen <= 53` short-cut.
#[test]
fn a_single_limb_bignum_just_above_max_small_builds_like_the_oracle() {
    // 2^59 = 576460752303423488 and (1 bsl 60) + 1: both bignums.
    for (digits, expected) in [
        (power_of_two(59), vec![67u8, 160, 0, 0, 0, 0, 0, 0]),
        (vec![(1u64 << 60) + 1], vec![67u8, 176, 0, 0, 0, 0, 0, 0]),
    ] {
        assert_eq!(digits.len(), 1, "a single-limb fixture");
        let module = float_module(&[]);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let value = bignum(&mut machine, &mut ctx, false, &digits);
        machine.set_x(0, value).expect("x(0) in range");
        let out = build_float(
            &module,
            &mut machine,
            &mut ctx,
            1,
            Operand::Nil,
            Operand::X(0),
            Operand::Integer(64),
        )
        .expect("assemble")
        .expect("the build succeeds");
        assert_eq!(out, (expected, 64), "digits={digits:?}");
    }
}

/// `2^1023` is the largest power of two still representable as a finite double, so
/// it **is** encoded — the boundary of `big_to_double`'s range check.
#[test]
fn a_bignum_at_the_top_of_the_double_range_is_encoded_like_the_oracle() {
    // `<<(1 bsl 1023)/float>>` = <<127,224,0,0,0,0,0,0>>.
    let module = float_module(&[]);
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let value = bignum(&mut machine, &mut ctx, false, &power_of_two(1023));
    machine.set_x(0, value).expect("x(0) in range");
    let out = build_float(
        &module,
        &mut machine,
        &mut ctx,
        1,
        Operand::Nil,
        Operand::X(0),
        Operand::Integer(64),
    )
    .expect("assemble")
    .expect("the build succeeds");
    assert_eq!(out, (vec![127, 224, 0, 0, 0, 0, 0, 0], 64));
}

/// A bignum whose magnitude overflows a finite double is still the bare `badarg`
/// (C's `big_to_double < 0` -> `c_p->fvalue = am_no_float`, `erl_bits.c:1373`).
#[test]
fn a_bignum_float_segment_overflow_still_fails_badarg_like_the_oracle() {
    let module = float_module(&[]);
    for exponent in [1024usize, 1050, 2000] {
        for negative in [false, true] {
            let mut machine = Machine::new();
            let mut ctx = MinimalContext::new();
            let value = bignum(&mut machine, &mut ctx, negative, &power_of_two(exponent));
            machine.set_x(0, value).expect("x(0) in range");
            let err = build_float(
                &module,
                &mut machine,
                &mut ctx,
                1,
                Operand::Nil,
                Operand::X(0),
                Operand::Integer(64),
            )
            .expect_err("the build must fail");
            assert_eq!(
                err.as_atom(),
                Some("badarg"),
                "2^{exponent} (negative={negative}): {err:?}"
            );
        }
    }
}

/// The small-integer and float routes are unchanged (the fix only adds the bignum
/// arm). `<<5/float>>` = <<64,20,0,0,0,0,0,0>> and `<<5.5/float>>` =
/// <<64,22,0,0,0,0,0,0>>, both the oracle's.
#[test]
fn a_small_and_a_float_value_are_unchanged() {
    let module = float_module(&[]);
    for (value, expected) in [
        (make_small(5), vec![64u8, 20, 0, 0, 0, 0, 0, 0]),
        (
            {
                let mut ctx = MinimalContext::new();
                crate::terms::test_support::make_float(&mut ctx, 5.5).expect("a float")
            },
            vec![64u8, 22, 0, 0, 0, 0, 0, 0],
        ),
    ] {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        machine.set_x(0, value).expect("x(0) in range");
        let out = build_float(
            &module,
            &mut machine,
            &mut ctx,
            1,
            Operand::Nil,
            Operand::X(0),
            Operand::Integer(64),
        )
        .expect("assemble")
        .expect("the build succeeds");
        assert_eq!(out, (expected, 64));
    }
}
