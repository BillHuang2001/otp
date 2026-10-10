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

//! The [`DefaultArithHook`](super::DefaultArithHook) implementation: match-spec
//! arithmetic over the term classes this crate can compute exactly.
//!
//! The integer operators go through the checked `i64` path (a 60-bit small, with
//! any overflow a typed refusal) and the float operators through `f64`, matching
//! the ordinary Erlang operators the C `guard_tab` binds them to
//! (`../../erts/emulator/beam/erl_db_util.c:887-894`). Every operation is
//! **total-or-refusing** — a value this module cannot compute exactly is a typed
//! error, never a fabricated answer; the [`default_arith`] docs enumerate exactly
//! which failure each case maps to.
//!
//! Split out of `expr/mod.rs` so that file stays within the crate's ~1000-line
//! guideline (crate-layer `CONTEXT.md`, Constraints).

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_term::prelude::*;

use super::guard::write_float_term;
use super::{classify, ArithOp};
use crate::error::{badarg, unsupported};

/// The Erlang spelling of an [`ArithOp`] (used in the typed error messages, so a
/// refusal reads as the operator the spec actually wrote).
fn arith_name(op: ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::IntDiv => "div",
        ArithOp::Rem => "rem",
        ArithOp::Band => "band",
        ArithOp::Bor => "bor",
        ArithOp::Bxor => "bxor",
        ArithOp::Bsl => "bsl",
        ArithOp::Bsr => "bsr",
    }
}

/// The typed "outside the small-integer subset" failure for `op`.
fn not_small(op: &str) -> BeamError {
    unsupported(&format!(
        "match-spec arithmetic '{op}': the result is not a small integer"
    ))
}

/// The typed "operand `index` (1-based) is not a small integer" failure of the
/// integer path.
fn not_small_operand(op: &str, index: usize) -> BeamError {
    unsupported(&format!(
        "match-spec arithmetic '{op}': operand {} is not a small integer",
        index + 1
    ))
}

/// Whether `class` is an Erlang **integer** (a small integer or a bignum).
fn is_integer_class(class: TermClass) -> bool {
    matches!(class, TermClass::Small | TermClass::Big)
}

/// The numeric value of `term` as an `f64`, or `None` when it is not a number.
///
/// A small integer converts exactly (the same rounding every integer→float
/// conversion uses); a bignum goes through `beam_term::number::big_to_f64`, whose
/// `None` — a magnitude beyond the finite `f64` range — is exactly the `badarith`
/// the oracle's conversion raises.
fn numeric_as_f64(term: Eterm) -> Option<f64> {
    match classify(term) {
        TermClass::Small => Some(signed_value(term) as f64),
        // SAFETY: `classify` established a live boxed float block.
        TermClass::Float => Some(unsafe { float_value(term) }),
        TermClass::Big => big_to_f64(term),
        // An atom/list/tuple/map/… is not a number.
        _ => None,
    }
}

/// Materialise the `f64` result of a float arithmetic op, or the oracle's
/// arithmetic error when the operation overflowed.
///
/// Erlang raises `badarith` rather than producing a non-finite float, so an
/// `inf`/`NaN` is **never** returned as a term.
fn float_result(value: f64, heap: &mut Heap) -> Result<Eterm> {
    if !value.is_finite() {
        return Err(badarg());
    }
    write_float_term(heap, value)
}

/// The [`DefaultArithHook`](super::DefaultArithHook) implementation: `+ - * / div
/// rem band bor bxor bsl bsr`, plus unary `+`/`-`.
///
/// It is deliberately **total-or-refusing** — never wrong:
///
/// * `+`/`-`/`*`: an **integer pair** keeps the checked `i64` path (an `i64`
///   overflow, a **bignum** operand, or a result outside the 60-bit small range —
///   all bignums in the oracle — is the typed `unsupported_ets`); a **float**
///   operand puts the whole operation into `f64` (a bignum on the other side
///   converts through `big_to_f64`) and yields a boxed float. Unary `+`/`-` is the
///   identity / the negation of a small integer, and of a float a *float* result.
/// * `/` **always** yields a float (`1/2` is `0.5`, `4/2` is `2.0`), computed in
///   `f64`; it follows the same operand conversion as the float path above.
/// * A float operation that overflows to `±inf`/`NaN` is `badarg` — the oracle's
///   `badarith`; a non-finite float term is never returned.
/// * A **non-number** operand is `badarg`, the oracle's own `badarith` (the guard
///   path then treats the clause as unmet rather than as a capability gap).
/// * `div`/`rem` by zero ⇒ `badarg`, which is what the oracle raises.
/// * Shift operators mirror the C `erts_shift`
///   (`../../erts/emulator/beam/erl_arith.c:196-247`). A **negative** count
///   *reverses* the direction: the `bsr` count is negated and the sign of the
///   resulting count chooses the direction, so `bsl`/`bsr` merely swap (`8 bsr -1`
///   is `16`, `1 bsl -1` is `0`, `8 bsl -1` is `4`). A **right** shift of a 60-bit
///   small by 59 or more (including 64+) saturates to the sign's `0`/`-1`
///   (`1 bsr 100` is `0`, `-1 bsr 100` is `-1`) — never `badarg`. A **left** shift
///   (directly, or a `bsr` with a negative count) that leaves the 60-bit small
///   range — a count of 64 or more, or lost bits such as `2 bsl 62` — is a bignum
///   in the oracle, so it is the typed `unsupported_ets`. A zero operand short-
///   circuits to `0` for every count (`0 bsl 100` is `0`), as the C does.
/// * A wrong operand count ⇒ `badarg`; only reachable by calling the seam
///   directly, since the decoder rejects it at compile time.
pub(super) fn default_arith(op: ArithOp, args: &[Eterm], heap: &mut Heap) -> Result<Eterm> {
    let name = arith_name(op);
    // An operand term; a wrong arity is `badarg` (the decoder rejects it earlier).
    let arg = |index: usize| -> Result<Eterm> { args.get(index).copied().ok_or_else(badarg) };
    // The integer path's operand reader: only a small integer is in its subset.
    let operand = |index: usize| -> Result<i64> {
        let term = arg(index)?;
        if is_small(term) {
            Ok(signed_value(term))
        } else {
            Err(not_small_operand(name, index))
        }
    };
    // A computed value is in the subset only when it still fits a 60-bit small.
    let result = |value: i64| -> Result<Eterm> {
        try_make_small(value).map_err(|_| not_small(name))
    };
    // The checked `i64` form: `None` is the overflow the checked op detected.
    let checked = |value: Option<i64>| -> Result<Eterm> {
        match value {
            Some(value) => result(value),
            None => Err(not_small(name)),
        }
    };
    match (op, args.len()) {
        // Unary `+`/`-`: the identity / the negation. On a float that is a *float*
        // result (`+1.5` is `1.5`, `-1.5` is `-1.5`); on a small integer the
        // existing checked path is unchanged.
        (ArithOp::Add | ArithOp::Sub, 1) => {
            let term = arg(0)?;
            match classify(term) {
                TermClass::Float => {
                    if op == ArithOp::Sub {
                        // SAFETY: `classify` established a live boxed float block.
                        float_result(-unsafe { float_value(term) }, heap)
                    } else {
                        Ok(term)
                    }
                }
                TermClass::Small => {
                    let value = signed_value(term);
                    if op == ArithOp::Sub {
                        checked(value.checked_neg())
                    } else {
                        result(value)
                    }
                }
                // A bignum operand is an integer outside this hook's subset.
                TermClass::Big => Err(not_small_operand(name, 0)),
                // Not a number at all — the oracle's `badarith`.
                _ => Err(badarg()),
            }
        }
        // Binary `+`/`-`/`*`: an integer pair keeps the checked `i64` path; a float
        // operand moves the whole operation into `f64`.
        (ArithOp::Add | ArithOp::Sub | ArithOp::Mul, 2) => {
            let (left, right) = (arg(0)?, arg(1)?);
            let (left_class, right_class) = (classify(left), classify(right));
            if left_class == TermClass::Float || right_class == TermClass::Float {
                let left_value = numeric_as_f64(left).ok_or_else(badarg)?;
                let right_value = numeric_as_f64(right).ok_or_else(badarg)?;
                let value = match op {
                    ArithOp::Add => left_value + right_value,
                    ArithOp::Sub => left_value - right_value,
                    // `ArithOp::Mul` — the only `op` left.
                    _ => left_value * right_value,
                };
                return float_result(value, heap);
            }
            if is_integer_class(left_class) && is_integer_class(right_class) {
                return match op {
                    ArithOp::Add => checked(operand(0)?.checked_add(operand(1)?)),
                    ArithOp::Sub => checked(operand(0)?.checked_sub(operand(1)?)),
                    // `ArithOp::Mul` — the only `op` left.
                    _ => checked(operand(0)?.checked_mul(operand(1)?)),
                };
            }
            // A non-number operand — the oracle's `badarith`.
            Err(badarg())
        }
        // `/` ALWAYS yields a float in Erlang (`1/2` is `0.5`, `4/2` is `2.0`).
        (ArithOp::Div, 2) => {
            let left = numeric_as_f64(arg(0)?).ok_or_else(badarg)?;
            let right = numeric_as_f64(arg(1)?).ok_or_else(badarg)?;
            // A zero divisor — integer `0` and float `0.0` alike — is `badarith`.
            if right == 0.0 {
                return Err(badarg());
            }
            float_result(left / right, heap)
        }
        (ArithOp::IntDiv, 2) | (ArithOp::Rem, 2) => {
            let (left, right) = (operand(0)?, operand(1)?);
            if right == 0 {
                return Err(badarg());
            }
            checked(if op == ArithOp::IntDiv {
                left.checked_div(right)
            } else {
                left.checked_rem(right)
            })
        }
        // `band`/`bor`/`bxor` on two 60-bit values always stay 60-bit, but the
        // result is range-checked like every other op rather than assumed.
        (ArithOp::Band, 2) => result(operand(0)? & operand(1)?),
        (ArithOp::Bor, 2) => result(operand(0)? | operand(1)?),
        (ArithOp::Bxor, 2) => result(operand(0)? ^ operand(1)?),
        (ArithOp::Bsl, 2) | (ArithOp::Bsr, 2) => {
            let (value, count) = (operand(0)?, operand(1)?);
            // The C `erts_shift` returns the value unchanged for a zero count or a
            // zero operand (`i == 0 || ires == 0`); the zero-operand case matters
            // because it short-circuits even an out-of-range count (`0 bsl 100`).
            if value == 0 || count == 0 {
                return result(value);
            }
            // …and it *reverses the direction* for a negative count: `bsr` negates
            // the count (`i = -signed_val(arg2)`), and the sign of the resulting
            // count chooses the direction, so `bsl`/`bsr` merely swap. `|count|`
            // fits `i64` because a small integer is at most 60 bits.
            let (left, amount) = if count > 0 {
                (op == ArithOp::Bsl, count)
            } else {
                (op == ArithOp::Bsr, -count)
            };
            // A count past `u32` is far beyond the 64-bit width; saturate so the
            // boundary handling below applies (left ⇒ bignum, right ⇒ sign).
            let amount = u32::try_from(amount).unwrap_or(u32::MAX);
            if left {
                // A left shift must not silently drop shifted-out bits: `2 bsl 62`
                // is a bignum in the oracle and a bare `<<` would answer `0`. Any
                // count of 64 or more leaves the 60-bit range for sure, so it is
                // refused the same way (never guessed).
                if amount >= 64 {
                    return Err(not_small(name));
                }
                let shifted = value << amount;
                if (shifted >> amount) != value {
                    return Err(not_small(name));
                }
                result(shifted)
            } else {
                // An arithmetic right shift of a 60-bit value saturates to the
                // sign's `0`/`-1`; clamping the amount at 63 yields exactly that
                // (and keeps the shift in range for the `i64` operation).
                result(value >> amount.min(63))
            }
        }
        _ => Err(badarg()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::{
        big_size_words, float_value, is_small, make_small, write_big, Eterm, MAX_SMALL, NIL,
    };

    /// A scratch heap with room for the float/bignum blocks the tests build.
    fn heap() -> Heap {
        Heap::new(64).expect("test heap")
    }

    /// A float term on `heap`.
    fn float_term(heap: &mut Heap, value: f64) -> Eterm {
        write_float_term(heap, value).expect("the heap has room for a float")
    }

    /// Whether `term` is a boxed float.
    fn is_float_term(term: Eterm) -> bool {
        classify(term) == TermClass::Float
    }

    /// The `f64` value of a term these tests produced as a float.
    fn as_float(term: Eterm) -> f64 {
        assert!(is_float_term(term), "expected a float term");
        // SAFETY: the assertion above established a live boxed float.
        unsafe { float_value(term) }
    }

    /// A positive bignum term on `heap` (least-significant digit first).
    fn bignum(heap: &mut Heap, digits: &[u64]) -> Eterm {
        // SAFETY: `alloc_words` hands back `big_size_words(digits.len())`
        // exclusively-owned, writable words — all of which `write_big` fills — and the
        // heap never reallocates, so the term stays live for the heap's lifetime.
        unsafe {
            let hp = heap
                .alloc_words(big_size_words(digits.len()))
                .expect("the heap has room for a bignum");
            write_big(hp, false, digits)
        }
    }

    /// Assert a refusal is the typed "not a small integer" arithmetic gap.
    fn assert_unsupported_arithmetic(error: BeamError) {
        let raise = error.into_raise().expect("the refusal is an explicit raise");
        let reason = raise
            .reason
            .as_atom()
            .expect("the reason is an atom")
            .to_string();
        assert!(
            reason.starts_with("unsupported_ets: match-spec arithmetic"),
            "expected the typed arithmetic gap, got `{reason}`"
        );
    }

    #[test]
    fn division_always_yields_a_float() {
        // erl: `2/2` is `1.0`, `4/2` is `2.0` and `1/2` is `0.5` — `/` never
        // returns an integer, even when the division is exact.
        let mut heap = heap();
        let one = default_arith(ArithOp::Div, &[make_small(2), make_small(2)], &mut heap)
            .expect("2/2 is answerable");
        assert!(is_float_term(one), "2/2 is the float 1.0, not the integer 1");
        assert!(!is_small(one), "2/2 must not be an immediate integer");
        assert_eq!(as_float(one), 1.0);

        let two = default_arith(ArithOp::Div, &[make_small(4), make_small(2)], &mut heap)
            .expect("4/2 is answerable");
        assert!(is_float_term(two));
        assert_eq!(as_float(two), 2.0);

        let half = default_arith(ArithOp::Div, &[make_small(1), make_small(2)], &mut heap)
            .expect("1/2 is answerable");
        assert!(is_float_term(half));
        assert_eq!(as_float(half), 0.5);
    }

    #[test]
    fn a_float_operand_promotes_arithmetic_to_a_float() {
        // erl: `1 + 1.5` is `2.5`, `2 - 0.5` is `1.5`, `20 * 1.5` is `30.0`.
        let mut heap = heap();
        let one_and_a_half = float_term(&mut heap, 1.5);
        let sum = default_arith(ArithOp::Add, &[make_small(1), one_and_a_half], &mut heap)
            .expect("1 + 1.5 is answerable");
        assert!(is_float_term(sum), "1 + 1.5 is a float");
        assert_eq!(as_float(sum), 2.5);

        let half = float_term(&mut heap, 0.5);
        let difference = default_arith(ArithOp::Sub, &[make_small(2), half], &mut heap)
            .expect("2 - 0.5 is answerable");
        assert!(is_float_term(difference), "2 - 0.5 is a float");
        assert_eq!(as_float(difference), 1.5);

        let one_and_a_half = float_term(&mut heap, 1.5);
        let product = default_arith(ArithOp::Mul, &[make_small(20), one_and_a_half], &mut heap)
            .expect("20 * 1.5 is answerable");
        assert!(is_float_term(product), "20 * 1.5 is a float");
        assert_eq!(as_float(product), 30.0);
    }

    #[test]
    fn unary_plus_and_minus_on_a_float() {
        // erl: `+1.5` is `1.5` and `-1.5` is `-1.5` — both floats.
        let mut heap = heap();
        let positive = float_term(&mut heap, 1.5);
        let unchanged =
            default_arith(ArithOp::Add, &[positive], &mut heap).expect("+1.5 is answerable");
        assert!(is_float_term(unchanged));
        assert_eq!(as_float(unchanged), 1.5);

        let negative =
            default_arith(ArithOp::Sub, &[positive], &mut heap).expect("-1.5 is answerable");
        assert!(is_float_term(negative), "unary - of a float is a float");
        assert_eq!(as_float(negative), -1.5);
    }

    #[test]
    fn a_zero_divisor_is_badarg() {
        // erl: `1/0`, `1/0.0` and `1.0/0.0` all raise `badarith`.
        let mut heap = heap();
        for (label, args) in [
            ("1/0", vec![make_small(1), make_small(0)]),
            ("1/0.0", vec![make_small(1), float_term(&mut heap, 0.0)]),
            ("1.0/0.0", vec![float_term(&mut heap, 1.0), float_term(&mut heap, 0.0)]),
        ] {
            let error = default_arith(ArithOp::Div, &args, &mut heap)
                .expect_err("a zero divisor is badarg");
            assert!(matches!(error, BeamError::BadArg), "{label} is badarg");
        }
    }

    #[test]
    fn a_float_overflow_is_badarg_never_a_non_finite_float() {
        // erl: `1.0e308 * 10.0` and `1.0e308 + 1.0e308` raise `badarith` — an
        // overflowed float is an error, never an `inf` term.
        let mut heap = heap();
        let huge = float_term(&mut heap, 1.0e308);
        let ten = float_term(&mut heap, 10.0);
        let error = default_arith(ArithOp::Mul, &[huge, ten], &mut heap)
            .expect_err("1.0e308 * 10.0 is badarg");
        assert!(matches!(error, BeamError::BadArg));

        let error = default_arith(ArithOp::Add, &[huge, huge], &mut heap)
            .expect_err("1.0e308 + 1.0e308 is badarg");
        assert!(matches!(error, BeamError::BadArg));
    }

    #[test]
    fn small_integer_arithmetic_is_unchanged() {
        // `2 + 2` is still the *small integer* 4 (the integer path is untouched).
        let mut heap = heap();
        let four = default_arith(ArithOp::Add, &[make_small(2), make_small(2)], &mut heap)
            .expect("2 + 2 is answerable");
        assert!(is_small(four), "2 + 2 is the small integer 4");
        assert_eq!(four, make_small(4));

        // …and a small-integer overflow is still the typed gap, not a guessed value.
        let error = default_arith(ArithOp::Add, &[make_small(MAX_SMALL), make_small(1)], &mut heap)
            .expect_err("MAX_SMALL + 1 is a bignum");
        assert_unsupported_arithmetic(error);
    }

    #[test]
    fn a_bignum_operand_on_an_integer_op_is_still_the_typed_gap() {
        // The oracle answers `(1 bsl 200) + 1` with a bignum — arithmetic this hook
        // does not have — so it stays the typed `unsupported_ets`, never a guessed
        // small integer. The bitwise/shift operators are unchanged in the same way.
        let mut heap = heap();
        // 2^200: limb 3 covers bits 192..256, so bit 200 is bit 8 of it.
        let power = bignum(&mut heap, &[0, 0, 0, 1 << 8]);
        for op in [
            ArithOp::Add,
            ArithOp::Sub,
            ArithOp::Mul,
            ArithOp::IntDiv,
            ArithOp::Rem,
            ArithOp::Band,
        ] {
            let error = default_arith(op, &[power, make_small(2)], &mut heap)
                .expect_err("a bignum operand is outside the integer path");
            assert_unsupported_arithmetic(error);
        }
    }

    #[test]
    fn a_bignum_operand_with_a_float_converts_through_big_to_f64() {
        // erl: `(1 bsl 64) * 1.0` is `1.8446744073709552e19` and `(1 bsl 64) / 2` is
        // `9.223372036854776e18` — the bignum converts to the nearest double and the
        // operation yields a float.
        let mut heap = heap();
        let two_pow_64 = bignum(&mut heap, &[0, 1]);
        let one = float_term(&mut heap, 1.0);
        let product = default_arith(ArithOp::Mul, &[two_pow_64, one], &mut heap)
            .expect("(1 bsl 64) * 1.0 is answerable");
        assert!(is_float_term(product));
        assert_eq!(as_float(product), 18446744073709551616.0);

        let two_pow_64 = bignum(&mut heap, &[0, 1]);
        let quotient = default_arith(ArithOp::Div, &[two_pow_64, make_small(2)], &mut heap)
            .expect("(1 bsl 64) / 2 is answerable");
        assert!(is_float_term(quotient));
        assert_eq!(as_float(quotient), 9223372036854775808.0);
    }

    #[test]
    fn a_bignum_beyond_the_f64_range_with_a_float_is_badarg() {
        // erl: `(1 bsl 1100) * 1.0` raises `badarith` — the integer cannot convert
        // to a finite double. `big_to_f64` returns `None`, which is that `badarg`.
        let mut heap = heap();
        // 2^1100: limb 17 covers bits 1088..1152, so bit 1100 is bit 12 of it.
        let mut digits = vec![0u64; 18];
        digits[17] = 1 << 12;
        let huge = bignum(&mut heap, &digits);
        let one = float_term(&mut heap, 1.0);
        let error = default_arith(ArithOp::Mul, &[huge, one], &mut heap)
            .expect_err("beyond the f64 range is badarg");
        assert!(matches!(error, BeamError::BadArg));
    }

    #[test]
    fn a_non_number_operand_is_badarg() {
        // erl: `1 + []` and `[] * 2` raise `badarith` — a non-number operand is the
        // oracle's own error (the guard path then treats the clause as unmet).
        let mut heap = heap();
        let error = default_arith(ArithOp::Add, &[make_small(1), NIL], &mut heap)
            .expect_err("1 + [] is badarg");
        assert!(matches!(error, BeamError::BadArg));
        let error = default_arith(ArithOp::Mul, &[NIL, make_small(2)], &mut heap)
            .expect_err("[] * 2 is badarg");
        assert!(matches!(error, BeamError::BadArg));
    }
}
