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

//! Arithmetic BIFs for `erlang`: `+`, `-`, `*`, `/`, `div`, `rem`, `abs`, the
//! unary `-`/`+` and the integer bit operators `band`, `bor`, `bxor`, `bnot`,
//! `bsl`, `bsr`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_op.c` (the operator
//! BIFs and their guard variants), `erl_arith.c` (`erts_arith_*`, `ers_shift`) and
//! `big.c` (`big_plus`/`big_minus`/`big_times`/`big_div`/`big_rem`/`big_to_double`,
//! `I_band`/`I_bor`/`I_bxor`/`I_bnot`/`I_lshift`), plus
//! the `ubif erlang:'+'/2` … roster in `../../erts/emulator/beam/bif.tab`
//! (lines 295–316: `'>' '>=' '<' '=<' '=:=' '==' '=/' '/=' '+' '-' '*' '/' 'div'
//! 'rem' '-' '/'` and, lines 309–314, `'bor' 'band' 'bxor' 'bsl' 'bsr' 'bnot'`).
//! The names are registered *quoted*, exactly as `bif.tab` spells them.
//!
//! # The numeric tower: exact integers
//!
//! Operands are the three numeric classes the frozen term layout has: a **60-bit
//! small integer** ([`beam_term::is_small`]), an arbitrary-precision **bignum**
//! ([`beam_term::is_big`], read with [`beam_codec::term::big_parts`]) and an
//! **`f64` float** (`beam_term::is_float`). Any other term is `badarith`.
//!
//! `+ - * div rem abs` and unary `-`/`+` are **exact**: both integers are widened
//! to [`Big`] — a canonical sign plus little-endian `u64` limbs — the operation
//! runs on that with `u128` intermediates, and the result is written back as a
//! small integer when it fits `MIN_SMALL..=MAX_SMALL` and as a bignum
//! ([`beam_codec::term::make_big`]) otherwise. That mirrors the C promotion path,
//! where the `big_*` helpers compute on `ErtsDigit`s and `big_norm`/
//! `normalize_big` fold the result back into a small integer when it fits.
//!
//! Mixed int/float arithmetic still promotes to a float. An integer operand is
//! converted with the same correctly-rounded, ties-to-even conversion as the C
//! `big_to_double` ([`Big::to_f64`]), failure mode included: an operand whose
//! magnitude is not a finite `f64` is `badarith` (the oracle's
//! `(1 bsl 3000) + 1.0`).
//!
//! # Semantics pinned against the OTP 29 oracle
//!
//! * `+ - *`: integer when both operands are integers, float if either is a float
//!   (`1 + 1.5` ⇒ `2.5`); `123456789012345678 * 98` ⇒ `12098765323209876444`
//!   (a bignum — the `semantics/bignum_multiplication` differential case).
//! * `abs/1`: `abs(-1.0)` ⇒ `1.0`; `abs(-(1 bsl 70))` ⇒ `1180591620717411303424`.
//! * unary `-`: `-1.5` ⇒ `-1.5`; unary `+`: the number, unchanged.
//! * `div`/`rem`: integers only (`1.0 div 2` ⇒ `badarith`), truncated toward zero
//!   (`-7 div 2` ⇒ `-3`, `-7 rem 2` ⇒ `-1`, `7 div -2` ⇒ `-3`, `7 rem -2` ⇒ `1`);
//!   `rem` carries the dividend's sign; a zero divisor is `badarith`.
//! * `/`: *always* a float (`2/1` ⇒ `2.0`); both operands convert to `f64` and a
//!   zero divisor is `badarith`.
//!
//! # The integer bit operators
//!
//! `band`/`bor`/`bxor`/`bnot` are **integer-only** (`1 band 1.0` ⇒ `badarith`) and
//! arbitrary-precision in two's complement. Each [`Big`] operand is widened to a
//! fixed-width two's-complement limb vector one limb wider than the wider
//! magnitude (a negative value is `2^(64w) - mag` over those limbs), the limb
//! operation runs, and the sign decided by the operation's own rule
//! (`a.neg && b.neg` for `band`, `||` for `bor`, `^` for `bxor`, `!a.neg` for
//! `bnot`) picks the conversion back — the same identities the C `I_band`/`I_bor`/
//! `I_bxor`/`I_bnot` rely on (`big.c`). `band(255,15)` ⇒ `15`, `band(-2,3)` ⇒ `2`,
//! `bor(-2,3)` ⇒ `-1`, `bxor(-2,3)` ⇒ `-3`, `bnot(5)` ⇒ `-6`, `bnot(-1)` ⇒ `0`.
//!
//! `bsl`/`bsr` follow `erts_shift` (`erl_arith.c:189-309`): a negative shift count
//! flips the direction (`8 bsl -2` ⇒ `2`, `1 bsr -2` ⇒ `4`); `bsr` is an
//! arithmetic, sign-extending (floor) right shift (`(-3) bsr 1` ⇒ `-2`,
//! `(-255) bsr 100` ⇒ `-1`) while `bsl` preserves sign (`(-1) bsl 4` ⇒ `-16`) and
//! may promote to a bignum (`1 bsl 70` ⇒ `1180591620717411303424`).
//!
//! Term *ordering* — including bignums — is [`beam_term::compare`]'s, used by
//! [`crate::compare`]; this module only needs the magnitude comparison
//! ([`cmp_mag`]) that its sign handling composes, plus the two's-complement
//! identities the sign of a bit operation is read from.
//!
//! # Documented limits
//!
//! A result whose magnitude exceeds `BIG_ARITY_MAX` digits is
//! [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) (reported by
//! [`beam_codec::term::make_big`]) and a full destination heap is the allocator's
//! error; neither is reachable from 60-bit operands.
//!
//! A **left** shift that would need more than `BIG_ARITY_MAX` limbs
//! ([`BIG_MAX_BITS`] bits) is `system_limit` *before* anything is allocated
//! (`1 bsl (1 bsl 70)` and `1 bsl 4194240` in the oracle; `1 bsl 4194239` and
//! `(1 bsl 63) bsl 4194176` still succeed), and a shift count that does not fit a
//! `usize` is either that same `system_limit` (left) or the C's `0`/`-1` (right) —
//! `1 bsr (1 bsl 70)` ⇒ `0`, `(-1) bsr (1 bsl 70)` ⇒ `-1`. `0 bsl <any>` is `0`,
//! never a limit error.

use core::cmp::Ordering;

use beam_common::Result;
use beam_term::{signed_value, Eterm, MAX_SMALL, MIN_SMALL};

use crate::access;
use crate::context::BifContext;
use crate::error::{badarg, badarith, system_limit};
use crate::registry::BifRegistry;

/// `BIG_ARITY_MAX` (`erl_term.h:465-469`, 64-bit `ARCH_64`): the largest bignum
/// digit count, i.e. the most `u64` limbs a materialized bignum may have.
/// [`beam_codec`] carries its own copy of the same constant (it owns
/// [`beam_codec::term::make_big`]); the shift bound needs it here so an oversized
/// left shift is rejected *before* the limb vector is allocated.
const BIG_ARITY_MAX: usize = (1 << 16) - 1;

/// The largest bit length a bignum result may have (`BIG_ARITY_MAX` limbs) — the
/// `system_limit` bound of `big_norm` (`big.c:3360`, `arity > BIG_ARITY_MAX`).
const BIG_MAX_BITS: usize = BIG_ARITY_MAX * 64;
/// A numeric operand, in one of the numeric tower's three classes.
enum Num {
    /// An exact integer (a small integer or a bignum), widened to [`Big`].
    Int(Big),
    /// An `f64` float.
    Float(f64),
}

/// Classifies `t` as one of the numeric tower's three classes.
///
/// # Errors
///
/// [`BeamError::BadArith`](beam_common::BeamError::BadArith) for anything that is
/// not a small integer, a bignum or a float (`1 + a` in the oracle).
fn number(t: Eterm) -> Result<Num> {
    if access::is_small(t) {
        Ok(Num::Int(Big::from_i64(signed_value(t))))
    } else if access::is_big(t) {
        // The term's limbs are borrowed from its own storage (the literal area or a
        // live heap), so they are copied into the canonical value immediately.
        let (negative, digits) = beam_codec::term::big_parts(t)?;
        Ok(Num::Int(Big::from_parts(negative, digits)))
    } else if access::is_float(t) {
        Ok(Num::Float(access::float_value(t)))
    } else {
        Err(badarith())
    }
}

/// Classifies `t` as an **integer** operand (the `div`/`rem` domain).
///
/// # Errors
///
/// Like [`number`]: a non-number *and* a float are both `badarith`
/// (`1.0 div 2` ⇒ `badarith` in the oracle).
fn integer(t: Eterm) -> Result<Big> {
    match number(t)? {
        Num::Int(value) => Ok(value),
        Num::Float(_) => Err(badarith()),
    }
}

/// The `f64` value of a numeric operand: an integer is rounded to the nearest
/// `f64` exactly like the oracle's `big_to_double`.
///
/// # Errors
///
/// `badarith` when an integer operand cannot be represented as a finite `f64`.
fn as_float(n: &Num) -> Result<f64> {
    match n {
        Num::Int(value) => value.to_f64(),
        Num::Float(f) => Ok(*f),
    }
}

/// Materializes an exact integer result: a small integer when the value fits the
/// 60-bit range, a bignum otherwise — the C `big_norm` fold-back.
fn int_term(ctx: &mut BifContext<'_>, value: &Big) -> Result<Eterm> {
    match value.small_value() {
        Some(small) => ctx.int(small),
        None => beam_codec::term::make_big(ctx.heap(), value.negative, &value.mag),
    }
}

// ---------------------------------------------------------------------------
// The integer-promotion seam used by the non-arithmetic BIFs
// ---------------------------------------------------------------------------
//
// Every integer-producing BIF outside this module routes its value through one of
// the three helpers below instead of `BifContext::int` (which is `try_make_small`
// and reports `system_limit` past the 60-bit range). That way a value the oracle
// promotes — a nanosecond timestamp, a 9-byte `decode_unsigned`, a long decimal
// string — becomes the same bignum here.

/// Materializes an exact integer from a sign and a little-endian `u64` magnitude
/// (`ErtsDigit`s, least significant first — the order
/// [`beam_codec::term::make_big`] expects).
///
/// A value that fits the 60-bit range becomes a small integer and an all-zero
/// magnitude is the small integer `0`; anything larger becomes the bignum the
/// oracle would build (`big_norm`). Leading zero limbs are dropped.
pub(crate) fn int_from_mag(ctx: &mut BifContext<'_>, negative: bool, mag: &[u64]) -> Result<Eterm> {
    int_term(ctx, &Big::from_parts(negative, mag))
}

/// Materializes the exact integer term for `value` — [`int_from_mag`] for an
/// `i64` (`system_time/0,1`, `time_offset/0,1` and the rounded float results).
pub(crate) fn int_from_i64(ctx: &mut BifContext<'_>, value: i64) -> Result<Eterm> {
    int_term(ctx, &Big::from_i64(value))
}

/// The exact integer term for a finitely-valued `f64` (the `round`/`trunc`/
/// `ceil`/`floor` promotion).
///
/// The value is decomposed into its 53-bit mantissa and binary exponent, so a
/// magnitude far beyond `i64` becomes the *exact* bignum the oracle returns
/// (`round(1.0e300)`) rather than a saturated or truncated one. A fractional
/// value is truncated toward zero (the caller has already applied its rounding
/// operation, so the value is integral).
pub(crate) fn int_from_f64(ctx: &mut BifContext<'_>, value: f64) -> Result<Eterm> {
    let (negative, mag) = f64_int_parts(value);
    int_term(ctx, &Big::from_parts(negative, &mag))
}

/// The `f64` nearest to an **integer** term, ties to even — the C `big_to_double`
/// behind the oracle's `float/1`.
///
/// The bignum case is the shared, canonical `big_to_double` port
/// [`beam_term::number::big_to_f64`] (the LCA term-layout crate), not a local
/// copy of the rounding algorithm.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-integer *and*
/// for a magnitude the oracle refuses too (`float(1 bsl 2000)` ⇒ `badarg`).
pub(crate) fn integer_to_f64(term: Eterm) -> Result<f64> {
    if access::is_small(term) {
        return Ok(signed_value(term) as f64);
    }
    if access::is_big(term) {
        // `None` is the C's `big_to_double(...) < 0`, which `float/1` reports as
        // `badarg` (the oracle's `float(1 bsl 2000)`).
        return beam_term::number::big_to_f64(term).ok_or_else(badarg);
    }
    Err(badarg())
}

/// The sign and little-endian magnitude of `value`'s exact mathematical value
/// (`±0.0` ⇒ a zero magnitude, hence the `false` sign).
fn f64_int_parts(value: f64) -> (bool, Vec<u64>) {
    let bits = value.to_bits();
    let negative = (bits >> 63) != 0;
    let raw_exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    // A subnormal (or zero) exponent field has no implicit leading bit; its value
    // is `fraction * 2^-1074`. A normal number is `(2^52 + fraction) * 2^(e-1075)`.
    let (mantissa, exponent) = if raw_exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1u64 << 52), raw_exponent - 1075)
    };
    let mut mag = vec![mantissa];
    if exponent >= 0 {
        shift_mag_left(&mut mag, exponent as usize);
    } else {
        shift_mag_right(&mut mag, (-exponent) as usize);
    }
    trim(&mut mag);
    (negative && !mag.is_empty(), mag)
}

/// `mag *= 2^bits`.
fn shift_mag_left(mag: &mut Vec<u64>, bits: usize) {
    if bits == 0 {
        return;
    }
    let limb_shift = bits / 64;
    let bit_shift = bits % 64;
    let mut out = vec![0u64; mag.len() + limb_shift + 1];
    for (i, &limb) in mag.iter().enumerate() {
        let shifted = u128::from(limb) << bit_shift;
        out[i + limb_shift] |= shifted as u64;
        out[i + limb_shift + 1] |= (shifted >> 64) as u64;
    }
    *mag = out;
}

/// `mag >>= bits`, dropping every bit below the new least-significant position.
fn shift_mag_right(mag: &mut Vec<u64>, bits: usize) {
    if bits == 0 {
        return;
    }
    let limb_shift = bits / 64;
    if limb_shift >= mag.len() {
        mag.clear();
        return;
    }
    let bit_shift = bits % 64;
    let mut out = Vec::with_capacity(mag.len() - limb_shift);
    for i in limb_shift..mag.len() {
        let mut limb = mag[i] >> bit_shift;
        if bit_shift != 0 {
            if let Some(&above) = mag.get(i + 1) {
                limb |= above << (64 - bit_shift);
            }
        }
        out.push(limb);
    }
    *mag = out;
}

/// `A + B`.
fn add(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match (number(args[0])?, number(args[1])?) {
        (Num::Int(x), Num::Int(y)) => int_term(ctx, &x.add(&y)),
        (x, y) => ctx.make_float(as_float(&x)? + as_float(&y)?),
    }
}

/// `a + b`, exactly as the `erlang:'+'/2` BIF computes it — **including** the
/// promotion to a bignum when an integer sum leaves the 60-bit small range.
///
/// Exposed for the pure-Erlang `lists.erl` fast paths that fold with `+`, so
/// [`crate::lists::sum`] promotes exactly like the oracle instead of reporting a
/// 60-bit overflow (`lists:sum([576460752303423487,1])` ⇒ `576460752303423488`).
pub(crate) fn add_terms(ctx: &mut BifContext<'_>, a: Eterm, b: Eterm) -> Result<Eterm> {
    add(ctx, &[a, b])
}

/// `A - B`.
fn sub(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match (number(args[0])?, number(args[1])?) {
        (Num::Int(x), Num::Int(y)) => int_term(ctx, &x.sub(&y)),
        (x, y) => ctx.make_float(as_float(&x)? - as_float(&y)?),
    }
}

/// `A * B`.
fn mul(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match (number(args[0])?, number(args[1])?) {
        (Num::Int(x), Num::Int(y)) => int_term(ctx, &x.mul(&y)),
        (x, y) => ctx.make_float(as_float(&x)? * as_float(&y)?),
    }
}

/// `A / B` — always a float.
fn divide(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let x = as_float(&number(args[0])?)?;
    let y = as_float(&number(args[1])?)?;
    if y == 0.0 {
        return Err(badarith());
    }
    ctx.make_float(x / y)
}

/// `A div B` — integer division, truncating toward zero.
fn int_div(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (quotient, _) = integer(args[0])?.div_rem(&integer(args[1])?)?;
    int_term(ctx, &quotient)
}

/// `A rem B` — the remainder of a division truncating toward zero.
fn int_rem(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (_, remainder) = integer(args[0])?.div_rem(&integer(args[1])?)?;
    int_term(ctx, &remainder)
}

/// `abs(A)`.
fn abs(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match number(args[0])? {
        Num::Int(value) => int_term(ctx, &value.abs()),
        Num::Float(f) => ctx.make_float(f.abs()),
    }
}

/// Unary `-A` (negation).
fn negate(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match number(args[0])? {
        Num::Int(value) => int_term(ctx, &value.neg()),
        Num::Float(f) => ctx.make_float(-f),
    }
}

/// Unary `+A` (numeric identity).
fn identity(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match number(args[0])? {
        Num::Int(value) => int_term(ctx, &value),
        Num::Float(f) => ctx.make_float(f),
    }
}

/// `A band B` — bitwise and of two integers, in two's complement
/// (`band(255,15)` ⇒ `15`, `band(-2,3)` ⇒ `2`).
fn band(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    int_term(ctx, &integer(args[0])?.band(&integer(args[1])?))
}

/// `A bor B` — bitwise or of two integers (`bor(-2,3)` ⇒ `-1`).
fn bor(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    int_term(ctx, &integer(args[0])?.bor(&integer(args[1])?))
}

/// `A bxor B` — bitwise exclusive or of two integers (`bxor(-2,3)` ⇒ `-3`).
fn bxor(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    int_term(ctx, &integer(args[0])?.bxor(&integer(args[1])?))
}

/// `bnot A` — bitwise complement of an integer, `-A - 1` (`bnot(5)` ⇒ `-6`,
/// `bnot(-1)` ⇒ `0`).
fn bnot(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    int_term(ctx, &integer(args[0])?.bit_not())
}

/// `A bsl B` — `A` shifted left by `B` bits; a negative `B` shifts right.
fn bsl(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    shift(ctx, &integer(args[0])?, &integer(args[1])?, true)
}

/// `A bsr B` — `A` shifted right by `B` bits, sign-extending (floor division);
/// a negative `B` shifts left.
fn bsr(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    shift(ctx, &integer(args[0])?, &integer(args[1])?, false)
}

/// The `bsl`/`bsr` computation: `a` shifted by `b`, where `left` is the operator
/// (`true` for `bsl`) — the C `erts_shift` (`erl_arith.c:189-309`).
///
/// A negative `b` flips the direction. A shift amount too large for a `usize` is
/// the C's own short-circuit: a left shift of a non-zero operand is `system_limit`
/// (`N bsl PositiveBigNum`), while a right shift discards every bit and yields `0`
/// or `-1`. A zero left operand is returned unchanged (`0 bsl <huge>` ⇒ `0`).
///
/// # Errors
///
/// [`BeamError::BadArith`](beam_common::BeamError::BadArith) is raised by the
/// callers for a non-integer operand; a left shift needing more than
/// [`BIG_MAX_BITS`] bits is
/// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit).
fn shift(ctx: &mut BifContext<'_>, a: &Big, b: &Big, left: bool) -> Result<Eterm> {
    let (b_negative, magnitude) = shift_amount(b);
    // `A bsl -N == A bsr N` and `A bsr -N == A bsl N`.
    let shift_left = left != b_negative;
    if a.is_zero() {
        // `0 bsl <any>` is `0`, however large the shift (`erts_shift` checks the
        // zero operand before the size estimate).
        return ctx.int(0);
    }
    match (shift_left, magnitude) {
        // A left shift that does not even fit the shift amount cannot be
        // represented (`erts_shift`'s `N bsl PositiveBigNum` arm).
        (true, None) => Err(system_limit()),
        // A right shift by an unrepresentably large amount keeps only the sign:
        // `0` for a non-negative operand, `-1` for a negative one.
        (false, None) => ctx.int(if a.negative { -1 } else { 0 }),
        (true, Some(bits)) => int_term(ctx, &a.shl(bits)?),
        (false, Some(bits)) => int_term(ctx, &a.shr(bits)),
    }
}

/// The shift amount of `A bsl B`/`A bsr B`: `B`'s sign and magnitude, where a
/// magnitude of `None` means "larger than any `usize`" (treated as infinite by
/// [`shift`], which is exactly what the C's `MIN_SMALL`/`MAX_SMALL` collapse
/// amounts to).
fn shift_amount(b: &Big) -> (bool, Option<usize>) {
    let magnitude = match b.mag.as_slice() {
        [] => Some(0),
        [limb] => usize::try_from(*limb).ok(),
        _ => None,
    };
    (b.negative, magnitude)
}
/// Registers the `erlang` arithmetic BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "+", 2, add);
    reg.register("erlang", "-", 2, sub);
    reg.register("erlang", "*", 2, mul);
    reg.register("erlang", "/", 2, divide);
    reg.register("erlang", "div", 2, int_div);
    reg.register("erlang", "rem", 2, int_rem);
    reg.register("erlang", "abs", 1, abs);
    reg.register("erlang", "-", 1, negate);
    reg.register("erlang", "+", 1, identity);
    reg.register("erlang", "band", 2, band);
    reg.register("erlang", "bor", 2, bor);
    reg.register("erlang", "bxor", 2, bxor);
    reg.register("erlang", "bnot", 1, bnot);
    reg.register("erlang", "bsl", 2, bsl);
    reg.register("erlang", "bsr", 2, bsr);
}

// ---------------------------------------------------------------------------
// Exact integers: canonical sign + little-endian magnitude
// ---------------------------------------------------------------------------

/// A canonical exact integer: a sign and a little-endian magnitude of `u64` limbs
/// with no leading zero limb; zero is the empty magnitude and is never negative
/// (the C `big_norm` normalization).
///
/// This is the widened form the arithmetic operates on. It is *not* a term: the
/// term is materialized by [`int_term`], as a 60-bit small integer when it fits
/// and as a bignum otherwise.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Big {
    /// `true` for a value below zero (never `true` for the empty magnitude).
    negative: bool,
    /// Least significant limb first; the most significant limb is non-zero.
    mag: Vec<u64>,
}

impl Big {
    /// Canonicalizes `mag`: leading zero limbs are dropped and a zero magnitude
    /// loses its sign.
    fn new(negative: bool, mut mag: Vec<u64>) -> Big {
        trim(&mut mag);
        Big {
            negative: negative && !mag.is_empty(),
            mag,
        }
    }

    /// The value `0`.
    fn zero() -> Big {
        Big {
            negative: false,
            mag: Vec::new(),
        }
    }

    /// The exact integer `value`.
    fn from_i64(value: i64) -> Big {
        Big::new(value < 0, vec![value.unsigned_abs()])
    }

    /// Copies a live term bignum's `(negative, digits)` parts out of its storage.
    fn from_parts(negative: bool, digits: &[u64]) -> Big {
        Big::new(negative, digits.to_vec())
    }

    /// Whether the value is zero.
    fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    /// The value as an `i64` when it fits the 60-bit small range, else `None`.
    fn small_value(&self) -> Option<i64> {
        let magnitude = match self.mag.as_slice() {
            [] => return Some(0),
            [limb] => i128::from(*limb),
            _ => return None,
        };
        let value = if self.negative { -magnitude } else { magnitude };
        if (i128::from(MIN_SMALL)..=i128::from(MAX_SMALL)).contains(&value) {
            Some(value as i64)
        } else {
            None
        }
    }

    /// The exact `self + other`.
    fn add(&self, other: &Big) -> Big {
        if self.is_zero() {
            return other.clone();
        }
        if other.is_zero() {
            return self.clone();
        }
        if self.negative == other.negative {
            return Big::new(self.negative, add_mag(&self.mag, &other.mag));
        }
        // Opposite signs: the smaller magnitude is subtracted from the larger and
        // the larger operand's sign survives; equal magnitudes cancel to zero.
        match cmp_mag(&self.mag, &other.mag) {
            Ordering::Greater => Big::new(self.negative, sub_mag(&self.mag, &other.mag)),
            Ordering::Less => Big::new(other.negative, sub_mag(&other.mag, &self.mag)),
            Ordering::Equal => Big::zero(),
        }
    }

    /// The exact `self - other`.
    fn sub(&self, other: &Big) -> Big {
        self.add(&other.neg())
    }

    /// The exact `self * other`.
    fn mul(&self, other: &Big) -> Big {
        if self.is_zero() || other.is_zero() {
            return Big::zero();
        }
        Big::new(
            self.negative != other.negative,
            mul_mag(&self.mag, &other.mag),
        )
    }

    /// The exact `self div other` and `self rem other`: both truncate toward zero
    /// and `rem` carries the dividend's sign (the C `big_div`/`big_rem`).
    ///
    /// # Errors
    ///
    /// `badarith` for a zero divisor (`big_div` reports the same through
    /// `BADARITH`).
    fn div_rem(&self, other: &Big) -> Result<(Big, Big)> {
        if other.is_zero() {
            return Err(badarith());
        }
        let (quotient, remainder) = div_rem_mag(&self.mag, &other.mag);
        Ok((
            Big::new(self.negative != other.negative, quotient),
            Big::new(self.negative, remainder),
        ))
    }

    /// The exact `-self`.
    fn neg(&self) -> Big {
        Big::new(!self.negative, self.mag.clone())
    }

    /// The exact `abs(self)`.
    fn abs(&self) -> Big {
        Big::new(false, self.mag.clone())
    }

    /// The bitwise `and` of two integers, in two's complement (`band`).
    ///
    /// The result is negative exactly when both operands are (the infinite-precision
    /// sign of the limb operation), which is what [`Big::from_twos`] needs to build
    /// the magnitude back.
    fn band(&self, other: &Big) -> Big {
        bitwise(self, other, |a, b| a & b, self.negative && other.negative)
    }

    /// The bitwise `or` of two integers (`bor`); negative when either operand is.
    fn bor(&self, other: &Big) -> Big {
        bitwise(self, other, |a, b| a | b, self.negative || other.negative)
    }

    /// The bitwise `xor` of two integers (`bxor`); negative when exactly one
    /// operand is.
    fn bxor(&self, other: &Big) -> Big {
        bitwise(self, other, |a, b| a ^ b, self.negative != other.negative)
    }

    /// The bitwise complement of an integer, `-self - 1` (`bnot`).
    fn bit_not(&self) -> Big {
        let width = self.mag.len() + 1;
        let limbs: Vec<u64> = self.to_twos(width).iter().map(|limb| !limb).collect();
        Big::from_twos(&limbs, !self.negative)
    }

    /// The two's-complement limbs of `self`, sign-extended to `width` limbs.
    ///
    /// A non-negative value fills above its magnitude with `0`; a negative one is
    /// `2^(64*width) - mag`, i.e. the complement of the zero-extended magnitude
    /// plus one (the carry out of the top limb is discarded — `width` is at least
    /// one limb above the magnitude, so the sign extension is already correct).
    fn to_twos(&self, width: usize) -> Vec<u64> {
        debug_assert!(width > self.mag.len(), "`to_twos` widens by one limb");
        let mut limbs = vec![0u64; width];
        limbs[..self.mag.len()].copy_from_slice(&self.mag);
        if self.negative {
            negate_twos(&mut limbs);
        }
        limbs
    }

    /// The integer whose two's-complement limbs are `limbs`, with sign `negative`.
    ///
    /// The inverse of [`Big::to_twos`]: the magnitude of a negative value is the
    /// negation of its two's-complement limbs. A caller that computes the sign by
    /// the operation's own rule must therefore pass the sign the limb operation
    /// produces, not the sign of either operand alone.
    fn from_twos(limbs: &[u64], negative: bool) -> Big {
        if !negative {
            return Big::new(false, limbs.to_vec());
        }
        let mut mag = limbs.to_vec();
        negate_twos(&mut mag);
        Big::new(true, mag)
    }

    /// `self << bits`, sign-preserving (`bsl`).
    ///
    /// # Errors
    ///
    /// [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) when the
    /// result would need more than [`BIG_MAX_BITS`] bits (more than
    /// [`BIG_ARITY_MAX`] limbs) — the C `big_norm` arity bound, checked here before
    /// the limb vector is allocated.
    fn shl(&self, bits: usize) -> Result<Big> {
        if bits == 0 || self.is_zero() {
            return Ok(self.clone());
        }
        match bit_len(&self.mag).checked_add(bits) {
            Some(total) if total <= BIG_MAX_BITS => {}
            _ => return Err(system_limit()),
        }
        let limb_shift = bits / 64;
        let bit_shift = bits % 64;
        // One limb more than the shifted magnitude needs: the top input limb's carry
        // goes there (it is zero when the top bits fit, and `Big::new` trims it).
        let mut mag = vec![0u64; self.mag.len() + limb_shift + 1];
        for (i, &limb) in self.mag.iter().enumerate() {
            // `limb << bit_shift` needs 127 bits at most, so the `u128` carries the
            // bits that belong to the next limb.
            let shifted = u128::from(limb) << bit_shift;
            mag[i + limb_shift] |= shifted as u64;
            mag[i + limb_shift + 1] |= (shifted >> 64) as u64;
        }
        Ok(Big::new(self.negative, mag))
    }

    /// `self >> bits`, arithmetic (sign-extending, floor) — the C `I_lshift` right
    /// shift (`big.c:2162-2218`), which rounds a negative value toward negative
    /// infinity by rounding the magnitude up whenever a bit is shifted out.
    fn shr(&self, bits: usize) -> Big {
        if bits == 0 || self.is_zero() {
            return self.clone();
        }
        let limb_shift = bits / 64;
        let bit_shift = bits % 64;
        if limb_shift >= self.mag.len() {
            // Every magnitude bit is shifted out: `0`, or `-1` (`-1 bsr N == -1`).
            return if self.negative {
                Big::from_i64(-1)
            } else {
                Big::zero()
            };
        }
        let mut mag = Vec::with_capacity(self.mag.len() - limb_shift);
        for (i, &limb) in self.mag[limb_shift..].iter().enumerate() {
            let mut out = limb >> bit_shift;
            if bit_shift != 0 {
                if let Some(&above) = self.mag.get(limb_shift + i + 1) {
                    out |= above << (64 - bit_shift);
                }
            }
            mag.push(out);
        }
        if self.negative && any_bits_below(&self.mag, bits) {
            mag = add_mag(&mag, &[1]);
        }
        Big::new(self.negative, mag)
    }

    /// The `f64` nearest to `self`, ties to even — the C `big_to_double`.
    ///
    /// This is only for the crate's **internal** sign+magnitude value (mixed
    /// int/float arithmetic on [`Num`]); the BIF-visible `Eterm` → `f64`
    /// conversion is [`beam_term::number::big_to_f64`].
    ///
    /// # Errors
    ///
    /// `badarith` when the magnitude is not representable as a finite `f64`
    /// (`big_to_double` returns `-1`): a bit length above 1024, or a rounding carry
    /// out of the largest finite double.
    fn to_f64(&self) -> Result<f64> {
        let magnitude = mag_to_f64(&self.mag)?;
        Ok(if self.negative { -magnitude } else { magnitude })
    }
}

/// Drops leading zero limbs (little-endian).
fn trim(mag: &mut Vec<u64>) {
    while mag.last() == Some(&0) {
        mag.pop();
    }
}

/// The number of significant bits in a canonical (trimmed) little-endian
/// magnitude: `0` for zero, the top limb's position plus its bit width otherwise.
fn bit_len(mag: &[u64]) -> usize {
    match mag.last() {
        None => 0,
        Some(&top) => (mag.len() - 1) * 64 + (64 - top.leading_zeros() as usize),
    }
}

/// The two's-complement negation of a fixed-width limb vector, in place
/// (`!limbs + 1`); the carry out of the top limb is discarded.
fn negate_twos(limbs: &mut [u64]) {
    for limb in limbs.iter_mut() {
        *limb = !*limb;
    }
    let mut carry = 1u128;
    for limb in limbs.iter_mut() {
        let total = u128::from(*limb) + carry;
        *limb = total as u64;
        carry = total >> 64;
        if carry == 0 {
            break;
        }
    }
}

/// The bitwise `op` of two integers in two's complement, with `sign` the result's
/// sign (the operation's own sign rule — see [`Big::band`]).
///
/// Both operands are widened to one limb more than the wider magnitude, so the top
/// limb is pure sign extension (`0` or all ones) and the limb `op` is exact.
fn bitwise(a: &Big, b: &Big, op: impl Fn(u64, u64) -> u64, sign: bool) -> Big {
    let width = a.mag.len().max(b.mag.len()) + 1;
    let x = a.to_twos(width);
    let y = b.to_twos(width);
    let limbs: Vec<u64> = x.iter().zip(&y).map(|(&p, &q)| op(p, q)).collect();
    Big::from_twos(&limbs, sign)
}
/// The ordering of two magnitudes (little-endian, no leading zeros).
fn cmp_mag(a: &[u64], b: &[u64]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for (x, y) in a.iter().zip(b).rev() {
        match x.cmp(y) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// The exact `a + b` magnitudes.
fn add_mag(a: &[u64], b: &[u64]) -> Vec<u64> {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut sum = Vec::with_capacity(long.len() + 1);
    let mut carry = 0u128;
    for (i, &limb) in long.iter().enumerate() {
        let total = u128::from(limb) + u128::from(short.get(i).copied().unwrap_or(0)) + carry;
        sum.push(total as u64);
        carry = total >> 64;
    }
    if carry != 0 {
        sum.push(carry as u64);
    }
    sum
}

/// The exact `a - b` magnitudes; `a` must be at least `b`.
fn sub_mag(a: &[u64], b: &[u64]) -> Vec<u64> {
    debug_assert!(cmp_mag(a, b) != Ordering::Less, "`sub_mag` needs a >= b");
    let mut difference = Vec::with_capacity(a.len());
    let mut borrow = 0i128;
    for (i, &limb) in a.iter().enumerate() {
        let diff = i128::from(limb) - i128::from(b.get(i).copied().unwrap_or(0)) - borrow;
        if diff < 0 {
            difference.push((diff + (1i128 << 64)) as u64);
            borrow = 1;
        } else {
            difference.push(diff as u64);
            borrow = 0;
        }
    }
    debug_assert_eq!(borrow, 0, "`sub_mag` left an unconsumed borrow");
    trim(&mut difference);
    difference
}

/// The exact `a * b` magnitudes (schoolbook, `u128` intermediates).
fn mul_mag(a: &[u64], b: &[u64]) -> Vec<u64> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut product = vec![0u64; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        if x == 0 {
            continue;
        }
        let mut carry = 0u128;
        for (j, &y) in b.iter().enumerate() {
            let total = u128::from(product[i + j]) + u128::from(x) * u128::from(y) + carry;
            product[i + j] = total as u64;
            carry = total >> 64;
        }
        let mut k = i + b.len();
        while carry != 0 {
            let total = u128::from(product[k]) + carry;
            product[k] = total as u64;
            carry = total >> 64;
            k += 1;
        }
    }
    trim(&mut product);
    product
}

/// The exact `a div b` and `a rem b` magnitudes (truncated); `b` is non-zero.
fn div_rem_mag(a: &[u64], b: &[u64]) -> (Vec<u64>, Vec<u64>) {
    debug_assert!(!b.is_empty(), "division by a zero magnitude");
    if cmp_mag(a, b) == Ordering::Less {
        // |a| < |b|: the quotient is zero and the remainder is the whole dividend.
        return (Vec::new(), a.to_vec());
    }
    if b.len() == 1 {
        return div_rem_mag_short(a, b[0]);
    }
    div_rem_mag_knuth(a, b)
}

/// Magnitude division by a single-limb divisor (`DIGIT`-wise, `u128` step).
fn div_rem_mag_short(dividend: &[u64], divisor: u64) -> (Vec<u64>, Vec<u64>) {
    debug_assert!(divisor != 0, "division by a zero limb");
    let divisor = u128::from(divisor);
    let mut quotient = vec![0u64; dividend.len()];
    let mut remainder = 0u128;
    for (i, &limb) in dividend.iter().enumerate().rev() {
        let window = (remainder << 64) | u128::from(limb);
        quotient[i] = (window / divisor) as u64;
        remainder = window % divisor;
    }
    trim(&mut quotient);
    let remainder = if remainder == 0 {
        Vec::new()
    } else {
        vec![remainder as u64]
    };
    (quotient, remainder)
}

/// Magnitude division for a multi-limb divisor: Knuth's Algorithm D, as `big_div`
/// in `big.c` implements it.
///
/// The operands are shifted left so the divisor's top bit is set (which bounds the
/// per-limb quotient estimate to at most one too large), each quotient limb is
/// estimated from the dividend window's two leading limbs, and the estimate is
/// corrected by an exact multiply/compare/subtract against the window.
fn div_rem_mag_knuth(a: &[u64], b: &[u64]) -> (Vec<u64>, Vec<u64>) {
    let n = b.len();
    debug_assert!(a.len() >= n, "the quotient needs at least one limb");
    let shift = b[n - 1].leading_zeros() as usize;
    let divisor = shift_left_fixed(b, shift);
    // One limb wider than the dividend, so the window always has a limb above every
    // quotient digit.
    let mut dividend = shift_left_wide(a, shift);
    let quotient_limbs = dividend.len() - n;
    let mut quotient = vec![0u64; quotient_limbs];

    for j in (0..quotient_limbs).rev() {
        let window_end = j + n + 1;
        let high = u128::from(dividend[j + n]);
        let mut guess = if high >= u128::from(divisor[n - 1]) {
            u64::MAX
        } else {
            (((high << 64) | u128::from(dividend[j + n - 1])) / u128::from(divisor[n - 1]))
                as u64
        };
        while !subtract_product(&mut dividend[j..window_end], guess, &divisor) {
            guess -= 1;
        }
        quotient[j] = guess;
    }

    trim(&mut quotient);
    // The divisor was normalized before the loop, so the remainder still carries the
    // shift and must be shifted back down (it is below the divisor, so the bits
    // shifted out are the ones the normalization shifted in).
    let mut remainder = shift_right(&dividend[..n], shift);
    trim(&mut remainder);
    (quotient, remainder)
}

/// `value << shift` for `shift < 64`, in the same number of limbs.
///
/// Exact because the caller passes the shift that clears the top limb's leading
/// zeros, so nothing carries out of the top limb.
fn shift_left_fixed(value: &[u64], shift: usize) -> Vec<u64> {
    if shift == 0 {
        return value.to_vec();
    }
    let mut shifted = Vec::with_capacity(value.len());
    let mut carry = 0u64;
    for &limb in value {
        shifted.push((limb << shift) | carry);
        carry = limb >> (64 - shift);
    }
    debug_assert_eq!(carry, 0, "the shift carried out of the top limb");
    shifted
}

/// `value << shift` for `shift < 64`, keeping the carry out of the top limb as an
/// extra most-significant limb.
fn shift_left_wide(value: &[u64], shift: usize) -> Vec<u64> {
    let mut shifted = Vec::with_capacity(value.len() + 1);
    let mut carry = 0u64;
    if shift == 0 {
        shifted.extend_from_slice(value);
    } else {
        for &limb in value {
            shifted.push((limb << shift) | carry);
            carry = limb >> (64 - shift);
        }
    }
    shifted.push(carry);
    shifted
}

/// `value >> shift` for `shift < 64`, the inverse of [`shift_left_fixed`].
///
/// The caller trims: the most significant limb loses its low bits, and the bits
/// shifted out of the value are the ones [`shift_left_fixed`] moved in.
fn shift_right(value: &[u64], shift: usize) -> Vec<u64> {
    if shift == 0 {
        return value.to_vec();
    }
    let mut shifted = Vec::with_capacity(value.len());
    for (i, &limb) in value.iter().enumerate() {
        let mut out = limb >> shift;
        if i + 1 < value.len() {
            out |= value[i + 1] << (64 - shift);
        }
        shifted.push(out);
    }
    shifted
}

/// Subtracts `factor * divisor` from `window` (one limb wider than `divisor`) and
/// reports whether that product fit — i.e. whether the subtraction happened. A
/// `false` means `factor` is one too large for this window.
fn subtract_product(window: &mut [u64], factor: u64, divisor: &[u64]) -> bool {
    let n = divisor.len();
    debug_assert_eq!(window.len(), n + 1, "the window is one limb wider");
    if factor == 0 {
        return true;
    }
    let mut product = vec![0u64; n + 1];
    let mut carry = 0u128;
    for (i, &limb) in divisor.iter().enumerate() {
        let total = u128::from(limb) * u128::from(factor) + carry;
        product[i] = total as u64;
        carry = total >> 64;
    }
    product[n] = carry as u64;

    // `factor * divisor > window` ⇒ the factor does not fit.
    for i in (0..=n).rev() {
        match product[i].cmp(&window[i]) {
            Ordering::Equal => continue,
            Ordering::Greater => return false,
            Ordering::Less => break,
        }
    }

    let mut borrow = 0i128;
    for i in 0..=n {
        let diff = i128::from(window[i]) - i128::from(product[i]) - borrow;
        if diff < 0 {
            window[i] = (diff + (1i128 << 64)) as u64;
            borrow = 1;
        } else {
            window[i] = diff as u64;
            borrow = 0;
        }
    }
    debug_assert_eq!(borrow, 0, "the window subtraction left a borrow");
    true
}

// ---------------------------------------------------------------------------
// Magnitude → f64 (the C `big_to_double`)
// ---------------------------------------------------------------------------

/// The `f64` nearest to a magnitude, ties to even.
///
/// The internal sign+magnitude companion of [`beam_term::number::big_to_f64`] (the
/// BIF-visible `Eterm` → `f64` conversion); this one serves the crate's internal
/// arbitrary-precision `Big` arithmetic.
///
/// # Errors
///
/// `badarith` when the magnitude has more than 1024 bits (at least `2^1024`, above
/// the largest finite double) or when rounding carries out of it.
fn mag_to_f64(mag: &[u64]) -> Result<f64> {
    if mag.is_empty() {
        return Ok(0.0);
    }
    let bit_len = bit_len(mag);
    if bit_len > 1024 {
        return Err(badarith());
    }
    if bit_len <= 53 {
        // A single limb below `2^53`, which the conversion represents exactly.
        return Ok(mag[0] as f64);
    }
    // Keep the top 53 bits and round on the following bits: the guard bit is the
    // dropped half bit, the sticky bit is "any lesser bit set".
    let shift = bit_len - 53;
    let mut mantissa = limb_bits_at(mag, shift);
    let half_bit = bit_at(mag, shift - 1);
    let lesser_bits = any_bits_below(mag, shift - 1);
    if half_bit && (lesser_bits || mantissa & 1 == 1) {
        mantissa += 1;
    }
    let value = mantissa as f64 * pow2(shift as i32);
    if value.is_finite() {
        Ok(value)
    } else {
        // The rounding carried out of the largest finite double.
        Err(badarith())
    }
}

/// The bits of `mag` from position `shift` up — i.e. `mag >> shift` truncated to a
/// limb (little-endian limbs).
fn limb_bits_at(mag: &[u64], shift: usize) -> u64 {
    let limb = shift / 64;
    let offset = shift % 64;
    let mut bits = mag[limb] >> offset;
    if offset != 0 && limb + 1 < mag.len() {
        bits |= mag[limb + 1] << (64 - offset);
    }
    bits
}

/// Whether bit `i` of `mag` is set (a position at or past the top limb is clear).
fn bit_at(mag: &[u64], i: usize) -> bool {
    match mag.get(i / 64) {
        Some(limb) => (limb >> (i % 64)) & 1 == 1,
        None => false,
    }
}

/// Whether any bit below position `i` of `mag` is set (the rounding sticky bit).
fn any_bits_below(mag: &[u64], i: usize) -> bool {
    let whole = i / 64;
    if mag.iter().take(whole.min(mag.len())).any(|&limb| limb != 0) {
        return true;
    }
    if whole < mag.len() && i % 64 != 0 {
        let mask = (1u64 << (i % 64)) - 1;
        return mag[whole] & mask != 0;
    }
    false
}

/// `2^exp` as an `f64` (the scaling half of C's `ldexp`; `exp` is below 1024 here).
fn pow2(exp: i32) -> f64 {
    (2.0f64).powi(exp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

    /// Words in the test heap: enough for the widest bignum the tests build.
    const TEST_HEAP_WORDS: usize = 8192;

    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["true", "false", "ok", "a", "b"]);
        atoms
    }

    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(TEST_HEAP_WORDS).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// A bignum term built on the context's heap from its little-endian limbs.
    fn bignum(ctx: &mut BifContext<'_>, negative: bool, digits: &[u64]) -> Eterm {
        beam_codec::term::make_big(ctx.heap(), negative, digits).expect("test bignum")
    }

    /// The little-endian limbs of a `u128` magnitude. Every expectation below is a
    /// value pinned against the OTP 29 oracle.
    fn limbs(value: u128) -> Vec<u64> {
        let mut rest = value;
        let mut out = Vec::new();
        while rest != 0 {
            out.push(rest as u64);
            rest >>= 64;
        }
        out
    }

    /// The `(negative, digits)` parts of a bignum result.
    fn parts(t: Eterm) -> (bool, Vec<u64>) {
        let (negative, digits) = beam_codec::term::big_parts(t).expect("expected a bignum");
        (negative, digits.to_vec())
    }

    /// The exact `i128` value of a small-integer or bignum result.
    fn value(t: Eterm) -> i128 {
        if access::is_small(t) {
            return i128::from(signed_value(t));
        }
        let (negative, digits) = parts(t);
        let magnitude = digits
            .iter()
            .rev()
            .fold(0i128, |acc, &limb| (acc << 64) | i128::from(limb));
        if negative {
            -magnitude
        } else {
            magnitude
        }
    }

    fn int(t: Eterm) -> i64 {
        assert!(access::is_small(t), "expected a small int, got {t:?}");
        signed_value(t)
    }

    fn float(t: Eterm) -> f64 {
        assert!(access::is_float(t), "expected a float, got {t:?}");
        access::float_value(t)
    }

    #[test]
    fn integer_addition_subtraction_multiplication() {
        with_ctx(|ctx| {
            assert_eq!(int(add(ctx, &[make_small(2), make_small(3)]).unwrap()), 5);
            assert_eq!(int(sub(ctx, &[make_small(2), make_small(3)]).unwrap()), -1);
            assert_eq!(int(mul(ctx, &[make_small(4), make_small(6)]).unwrap()), 24);
        });
    }

    #[test]
    fn division_always_returns_a_float() {
        with_ctx(|ctx| {
            assert_eq!(float(divide(ctx, &[make_small(2), make_small(1)]).unwrap()), 2.0);
            assert_eq!(float(divide(ctx, &[make_small(7), make_small(2)]).unwrap()), 3.5);
        });
    }

    #[test]
    fn mixed_int_float_promotes_to_float() {
        with_ctx(|ctx| {
            let half = ctx.make_float(1.5).unwrap();
            assert_eq!(float(add(ctx, &[make_small(1), half]).unwrap()), 2.5);
            assert_eq!(float(sub(ctx, &[half, make_small(1)]).unwrap()), 0.5);
            assert_eq!(float(mul(ctx, &[half, make_small(2)]).unwrap()), 3.0);
        });
    }

    #[test]
    fn div_and_rem_truncate_toward_zero() {
        with_ctx(|ctx| {
            assert_eq!(int(int_div(ctx, &[make_small(-7), make_small(2)]).unwrap()), -3);
            assert_eq!(int(int_div(ctx, &[make_small(7), make_small(-2)]).unwrap()), -3);
            assert_eq!(int(int_rem(ctx, &[make_small(-7), make_small(2)]).unwrap()), -1);
            assert_eq!(int(int_rem(ctx, &[make_small(7), make_small(-2)]).unwrap()), 1);
        });
    }

    #[test]
    fn abs_and_unary_operators() {
        with_ctx(|ctx| {
            assert_eq!(int(abs(ctx, &[make_small(-5)]).unwrap()), 5);
            assert_eq!(int(abs(ctx, &[make_small(5)]).unwrap()), 5);
            let neg = ctx.make_float(-1.5).unwrap();
            assert_eq!(float(abs(ctx, &[neg]).unwrap()), 1.5);

            assert_eq!(int(negate(ctx, &[make_small(5)]).unwrap()), -5);
            let half = ctx.make_float(1.5).unwrap();
            assert_eq!(float(negate(ctx, &[half]).unwrap()), -1.5);
            assert_eq!(int(identity(ctx, &[make_small(-5)]).unwrap()), -5);
            assert_eq!(float(identity(ctx, &[half]).unwrap()), 1.5);
        });
    }

    #[test]
    fn non_number_operands_are_badarith() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").unwrap();
            assert!(matches!(add(ctx, &[make_small(1), a]), Err(BeamError::BadArith)));
            assert!(matches!(abs(ctx, &[a]), Err(BeamError::BadArith)));
            assert!(matches!(negate(ctx, &[a]), Err(BeamError::BadArith)));
            assert!(matches!(identity(ctx, &[a]), Err(BeamError::BadArith)));
            assert!(matches!(divide(ctx, &[make_small(1), a]), Err(BeamError::BadArith)));
        });
    }

    #[test]
    fn zero_divisors_are_badarith() {
        with_ctx(|ctx| {
            assert!(matches!(int_div(ctx, &[make_small(1), make_small(0)]), Err(BeamError::BadArith)));
            assert!(matches!(int_rem(ctx, &[make_small(1), make_small(0)]), Err(BeamError::BadArith)));
            assert!(matches!(divide(ctx, &[make_small(1), make_small(0)]), Err(BeamError::BadArith)));
            // A float zero divisor is `badarith` too (`1.0/0.0` in the oracle).
            let zero = ctx.make_float(0.0).unwrap();
            assert!(matches!(divide(ctx, &[make_small(1), zero]), Err(BeamError::BadArith)));
            // A bignum divisor is never zero (zero is the small integer `0`), so the
            // zero-divisor check is reached through the widened value only.
            let big_gap = bignum(ctx, false, &[0, 1]);
            assert!(int_div(ctx, &[big_gap, big_gap]).is_ok());
        });
    }

    #[test]
    fn float_operands_are_badarith_for_div_and_rem() {
        with_ctx(|ctx| {
            let one = ctx.make_float(1.0).unwrap();
            assert!(matches!(int_div(ctx, &[one, make_small(2)]), Err(BeamError::BadArith)));
            assert!(matches!(int_div(ctx, &[make_small(2), one]), Err(BeamError::BadArith)));
            assert!(matches!(int_rem(ctx, &[one, make_small(2)]), Err(BeamError::BadArith)));
        });
    }

    #[test]
    fn results_past_the_small_range_promote_to_a_bignum() {
        with_ctx(|ctx| {
            // `MAX_SMALL + 1` = `2^59`, the first integer past the 60-bit range.
            let sum = add(ctx, &[make_small(MAX_SMALL), make_small(1)]).unwrap();
            assert!(access::is_big(sum));
            assert_eq!(parts(sum), (false, limbs(1u128 << 59)));

            // `MIN_SMALL - 1` = `-(2^59 + 1)`.
            let difference = sub(ctx, &[make_small(MIN_SMALL), make_small(1)]).unwrap();
            assert_eq!(parts(difference), (true, limbs((1u128 << 59) + 1)));

            // `MAX_SMALL * MAX_SMALL` (oracle 332306998946228967073030260463239169).
            let product = mul(ctx, &[make_small(MAX_SMALL), make_small(MAX_SMALL)]).unwrap();
            assert_eq!(
                parts(product),
                (false, limbs(332_306_998_946_228_967_073_030_260_463_239_169u128))
            );

            // `abs(MIN_SMALL)` = `-MIN_SMALL` = `2^59`.
            let absolute = abs(ctx, &[make_small(MIN_SMALL)]).unwrap();
            assert_eq!(parts(absolute), (false, limbs(1u128 << 59)));
            let negated = negate(ctx, &[make_small(MIN_SMALL)]).unwrap();
            assert_eq!(parts(negated), (false, limbs(1u128 << 59)));

            // `MIN_SMALL div -1` = `2^59`, `MIN_SMALL rem -1` = `0`.
            let quotient = int_div(ctx, &[make_small(MIN_SMALL), make_small(-1)]).unwrap();
            assert_eq!(parts(quotient), (false, limbs(1u128 << 59)));
            let remainder = int_rem(ctx, &[make_small(MIN_SMALL), make_small(-1)]).unwrap();
            assert_eq!(int(remainder), 0);
        });
    }

    #[test]
    fn the_differential_bignum_multiplication_is_exact() {
        with_ctx(|ctx| {
            // `semantics/bignum_multiplication`: both operands are small integers and
            // the exact product is a bignum (oracle 12098765323209876444).
            let product = mul(ctx, &[make_small(123456789012345678), make_small(98)]).unwrap();
            assert!(access::is_big(product));
            assert_eq!(parts(product), (false, limbs(12_098_765_323_209_876_444u128)));
        });
    }

    #[test]
    fn bignum_operands_are_exact_against_the_oracle() {
        with_ctx(|ctx| {
            // `(1 bsl 70) + 123456789012345678` ⇒ [123456789012345678, 64].
            let two_to_70 = bignum(ctx, false, &[0, 64]);
            let sum = add(ctx, &[two_to_70, make_small(123456789012345678)]).unwrap();
            assert_eq!(parts(sum), (false, vec![123456789012345678, 64]));

            // `(1 bsl 70) - (1 bsl 60)` = `1023 * 2^60` ⇒ 1179438699212804456448.
            let two_to_60 = bignum(ctx, false, &[1u64 << 60]);
            let difference = sub(ctx, &[two_to_70, two_to_60]).unwrap();
            assert_eq!(
                parts(difference),
                (false, limbs(1_179_438_699_212_804_456_448u128))
            );

            // `(1 bsl 128) * (1 bsl 64)` = `2^192` ⇒ [0, 0, 0, 1].
            let two_to_128 = bignum(ctx, false, &[0, 0, 1]);
            let two_to_64 = bignum(ctx, false, &[0, 1]);
            let product = mul(ctx, &[two_to_128, two_to_64]).unwrap();
            assert_eq!(parts(product), (false, vec![0, 0, 0, 1]));

            // A two-limb x two-limb product (oracle 1219326311370217952261850327336
            // 22923332237463801111263526900).
            let left = bignum(ctx, false, &limbs(123_456_789_012_345_678_901_234_567_890u128));
            let right = bignum(ctx, false, &limbs(987_654_321_098_765_432_109_876_543_210u128));
            let product = mul(ctx, &[left, right]).unwrap();
            assert_eq!(
                parts(product),
                (
                    false,
                    vec![
                        1256160004434509812,
                        7853714950065580681,
                        7839660305685708078,
                        19
                    ]
                )
            );

            // `-7 * 100000000000000000000` = `-700000000000000000000`.
            let hundred_quintillion = bignum(ctx, false, &limbs(100_000_000_000_000_000_000u128));
            let product = mul(ctx, &[make_small(-7), hundred_quintillion]).unwrap();
            assert_eq!(parts(product), (true, limbs(700_000_000_000_000_000_000u128)));

            // `-((1 bsl 200) + 0)` negated, times 3: `-(3 * 2^200)`.
            let two_to_200 = bignum(ctx, false, &[0, 0, 0, 256]);
            let negative = negate(ctx, &[two_to_200]).unwrap();
            let product = mul(ctx, &[negative, make_small(3)]).unwrap();
            assert_eq!(parts(product), (true, vec![0, 0, 0, 768]));

            // A bignum operand with unary `+` is the same exact value.
            let identity_term = identity(ctx, &[two_to_200]).unwrap();
            assert_eq!(parts(identity_term), (false, vec![0, 0, 0, 256]));
        });
    }

    #[test]
    fn bignum_division_and_remainder_match_the_oracle() {
        with_ctx(|ctx| {
            let two_to_70 = bignum(ctx, false, &[0, 64]);
            let two_to_35 = bignum(ctx, false, &[1u64 << 35]);

            // `(2^70) div (2^35)` = `2^35`, `(2^70) rem (2^35)` = `0`.
            assert_eq!(int(int_div(ctx, &[two_to_70, two_to_35]).unwrap()), 1 << 35);
            assert_eq!(int(int_rem(ctx, &[two_to_70, two_to_35]).unwrap()), 0);

            // `(-(2^70)) div (2^35)` = `-(2^35)`, `(-(2^70)) rem (2^35)` = `0`.
            let negated = negate(ctx, &[two_to_70]).unwrap();
            assert_eq!(int(int_div(ctx, &[negated, two_to_35]).unwrap()), -(1 << 35));
            assert_eq!(int(int_rem(ctx, &[negated, two_to_35]).unwrap()), 0);

            // `(2^70) rem 7` = `2`, `(-(2^70)) rem 7` = `-2`: the remainder carries
            // the dividend's sign.
            assert_eq!(int(int_rem(ctx, &[two_to_70, make_small(7)]).unwrap()), 2);
            assert_eq!(int(int_rem(ctx, &[negated, make_small(7)]).unwrap()), -2);

            // A three-limb dividend over a two-limb divisor (the Knuth path), pinned
            // to the oracle: quotient [14068135670835041870, 67], remainder
            // [13889941006043235206, 20496106279].
            let dividend =
                bignum(ctx, false, &[15020605774147554002, 14481235555578642862, 3628068951366]);
            let divisor = bignum(ctx, false, &[18425954751889178346, 53540848029]);
            let quotient = int_div(ctx, &[dividend, divisor]).unwrap();
            assert_eq!(parts(quotient), (false, vec![14068135670835041870, 67]));
            let remainder = int_rem(ctx, &[dividend, divisor]).unwrap();
            assert_eq!(
                parts(remainder),
                (false, vec![13889941006043235206, 20496106279])
            );

            // The same dividend over a single-limb divisor (the short path).
            let quotient = int_div(ctx, &[dividend, make_small(7)]).unwrap();
            assert_eq!(
                parts(quotient),
                (false, vec![7416299131652379604, 17880242856833707508, 518295564480])
            );
            assert_eq!(int(int_rem(ctx, &[dividend, make_small(7)]).unwrap()), 6);

            // Sign combinations of the same division (all four pinned to the oracle).
            let negative_dividend = negate(ctx, &[dividend]).unwrap();
            let negative_divisor = negate(ctx, &[divisor]).unwrap();
            let quotient = int_div(ctx, &[negative_dividend, divisor]).unwrap();
            assert_eq!(parts(quotient), (true, vec![14068135670835041870, 67]));
            let remainder = int_rem(ctx, &[negative_dividend, divisor]).unwrap();
            assert_eq!(
                parts(remainder),
                (true, vec![13889941006043235206, 20496106279])
            );
            let quotient = int_div(ctx, &[dividend, negative_divisor]).unwrap();
            assert_eq!(parts(quotient), (true, vec![14068135670835041870, 67]));
            let remainder = int_rem(ctx, &[dividend, negative_divisor]).unwrap();
            assert_eq!(
                parts(remainder),
                (false, vec![13889941006043235206, 20496106279])
            );
        });
    }

    #[test]
    fn bignum_operands_convert_to_doubles_like_the_oracle() {
        with_ctx(|ctx| {
            let decimal = bignum(ctx, false, &limbs(123_456_789_012_345_678_901_234_567_890u128));
            let two_to_70 = bignum(ctx, false, &[0, 64]);
            let one_point_oh = ctx.make_float(1.0).unwrap();

            // `123456789012345678901234567890 + 1.0` = 1.2345678901234568e29.
            assert_eq!(
                float(add(ctx, &[decimal, one_point_oh]).unwrap()),
                1.2345678901234568e29
            );
            // `(1 bsl 70) + 0.5` = 1.1805916207174113e21.
            let half = ctx.make_float(0.5).unwrap();
            assert_eq!(float(add(ctx, &[two_to_70, half]).unwrap()), 1.1805916207174113e21);
            // `123456789012345678901234567890 / 2` = 6.172839450617284e28.
            assert_eq!(
                float(divide(ctx, &[decimal, make_small(2)]).unwrap()),
                6.172839450617284e28
            );
            // `123456789012345678901234567890 / (1 bsl 70)` = 104571967.85567948.
            assert_eq!(
                float(divide(ctx, &[decimal, two_to_70]).unwrap()),
                104571967.85567948
            );
        });
    }

    #[test]
    fn integer_operands_beyond_the_double_range_are_badarith() {
        with_ctx(|ctx| {
            let one_point_oh = ctx.make_float(1.0).unwrap();

            // `(1 bsl 1025)`: more than 1024 bits, above the largest finite double, so
            // the oracle's `big_to_double` fails with `badarith`.
            let mut huge_digits = vec![0u64; 16];
            huge_digits.push(2);
            let huge = bignum(ctx, false, &huge_digits);
            assert!(matches!(add(ctx, &[huge, one_point_oh]), Err(BeamError::BadArith)));
            assert!(matches!(mul(ctx, &[huge, one_point_oh]), Err(BeamError::BadArith)));
            assert!(matches!(divide(ctx, &[huge, make_small(2)]), Err(BeamError::BadArith)));

            // `(1 bsl 1024) - (1 bsl 971)` is exactly `f64::MAX`.
            let mut max_digits = vec![0u64; 15];
            max_digits.push(((1u64 << 53) - 1) << 11);
            let max = bignum(ctx, false, &max_digits);
            assert_eq!(float(add(ctx, &[max, one_point_oh]).unwrap()), f64::MAX);

            // `(1 bsl 1024) - (1 bsl 970)`: 1024 bits whose rounding carries out of
            // the largest finite double, so it is `badarith` too.
            let mut carry_digits = vec![0u64; 15];
            carry_digits.push(((1u64 << 54) - 1) << 10);
            let carry = bignum(ctx, false, &carry_digits);
            assert!(matches!(add(ctx, &[carry, one_point_oh]), Err(BeamError::BadArith)));
        });
    }

    #[test]
    fn integer_arithmetic_agrees_with_i128_arithmetic() {
        with_ctx(|ctx| {
            // Small integers and genuine bignums, all pairs: the exact results are
            // checked against Rust's own `i128` arithmetic.
            let three_two_59: i128 = 3 * (1 << 59);
            let decimal: i128 = 12_098_765_323_209_876_444;
            let operands: [(i128, Eterm); 8] = [
                (0, make_small(0)),
                (1, make_small(1)),
                (-1, make_small(-1)),
                (i128::from(MAX_SMALL), make_small(MAX_SMALL)),
                (i128::from(MIN_SMALL), make_small(MIN_SMALL)),
                (three_two_59, bignum(ctx, false, &limbs(three_two_59 as u128))),
                (decimal, bignum(ctx, false, &limbs(decimal as u128))),
                (-decimal, bignum(ctx, true, &limbs(decimal as u128))),
            ];
            for &(x, x_term) in &operands {
                for &(y, y_term) in &operands {
                    assert_eq!(value(add(ctx, &[x_term, y_term]).unwrap()), x + y, "{x} + {y}");
                    assert_eq!(value(sub(ctx, &[x_term, y_term]).unwrap()), x - y, "{x} - {y}");
                    assert_eq!(value(mul(ctx, &[x_term, y_term]).unwrap()), x * y, "{x} * {y}");
                    if y != 0 {
                        assert_eq!(
                            value(int_div(ctx, &[x_term, y_term]).unwrap()),
                            x / y,
                            "{x} div {y}"
                        );
                        assert_eq!(
                            value(int_rem(ctx, &[x_term, y_term]).unwrap()),
                            x % y,
                            "{x} rem {y}"
                        );
                    }
                }
                assert_eq!(value(abs(ctx, &[x_term]).unwrap()), x.abs(), "abs({x})");
                assert_eq!(value(negate(ctx, &[x_term]).unwrap()), -x, "-{x}");
                assert_eq!(value(identity(ctx, &[x_term]).unwrap()), x, "+{x}");
            }
        });
    }

    #[test]
    fn magnitude_arithmetic_agrees_with_u128_arithmetic() {
        // The magnitude layer directly: every limb operation is checked against the
        // `u128` value it represents. Products wider than `u128` are skipped here
        // (`checked_mul`) and pinned to the oracle by the tests above.
        let values: [u128; 6] = [
            0,
            1,
            u128::from(u64::MAX),
            1u128 << 64,
            (1u128 << 64) + 12_345,
            12_098_765_323_209_876_444,
        ];
        for &x in &values {
            for &y in &values {
                let x_mag = limbs(x);
                let y_mag = limbs(y);
                assert_eq!(u128_from(&add_mag(&x_mag, &y_mag)), x + y, "{x} + {y}");
                if x >= y {
                    assert_eq!(u128_from(&sub_mag(&x_mag, &y_mag)), x - y, "{x} - {y}");
                }
                if let Some(product) = x.checked_mul(y) {
                    assert_eq!(u128_from(&mul_mag(&x_mag, &y_mag)), product, "{x} * {y}");
                }
                if y != 0 {
                    let (quotient, remainder) = div_rem_mag(&x_mag, &y_mag);
                    assert_eq!(u128_from(&quotient), x / y, "{x} div {y}");
                    assert_eq!(u128_from(&remainder), x % y, "{x} rem {y}");
                }
            }
        }
        assert_eq!(cmp_mag(&limbs(0), &limbs(0)), Ordering::Equal);
        assert_eq!(cmp_mag(&limbs(1), &limbs(2)), Ordering::Less);
        assert_eq!(cmp_mag(&limbs(1 << 64), &limbs(u64::MAX as u128)), Ordering::Greater);
    }

    /// The `u128` value of a little-endian magnitude that fits it.
    fn u128_from(mag: &[u64]) -> u128 {
        mag.iter().rev().fold(0u128, |acc, &limb| (acc << 64) | u128::from(limb))
    }

    /// Asserts that a shift's result is exactly `(-1)^negative * magnitude`,
    /// `magnitude` given as the `u128` the oracle prints. The result may be a small
    /// integer (the fold-back) or a bignum, so the comparison goes through
    /// [`value`].
    fn assert_shift(label: &str, result: Result<Eterm>, negative: bool, magnitude: u128) {
        let term = result.unwrap_or_else(|e| panic!("{label} failed: {e:?}"));
        let magnitude = i128::try_from(magnitude).expect("the magnitude fits an i128");
        let expected = if negative { -magnitude } else { magnitude };
        assert_eq!(value(term), expected, "{label}");
    }    #[test]
    fn bitwise_operations_match_the_oracle() {
        with_ctx(|ctx| {
            // Pinned to `erl -noshell -eval` (OTP 29 / erts-17.1):
            // `255 band 15` ⇒ 15, `-2 band 3` ⇒ 2, `-2 bor 3` ⇒ -1,
            // `-2 bxor 3` ⇒ -3, `bnot 5` ⇒ -6, `bnot -1` ⇒ 0, `bnot 0` ⇒ -1.
            assert_eq!(int(band(ctx, &[make_small(255), make_small(15)]).unwrap()), 15);
            assert_eq!(int(band(ctx, &[make_small(-2), make_small(3)]).unwrap()), 2);
            assert_eq!(int(bor(ctx, &[make_small(-2), make_small(3)]).unwrap()), -1);
            assert_eq!(int(bxor(ctx, &[make_small(-2), make_small(3)]).unwrap()), -3);
            assert_eq!(int(bnot(ctx, &[make_small(5)]).unwrap()), -6);
            assert_eq!(int(bnot(ctx, &[make_small(-1)]).unwrap()), 0);
            assert_eq!(int(bnot(ctx, &[make_small(0)]).unwrap()), -1);

            // `-1 band -1` ⇒ -1, `0 bor 0` ⇒ 0, `-1 bxor -1` ⇒ 0.
            assert_eq!(int(band(ctx, &[make_small(-1), make_small(-1)]).unwrap()), -1);
            assert_eq!(int(bor(ctx, &[make_small(0), make_small(0)]).unwrap()), 0);
            assert_eq!(int(bxor(ctx, &[make_small(-1), make_small(-1)]).unwrap()), 0);

            // The same operators at two-limb width, against the oracle's exact
            // values (all pinned through `erl`):
            let two_to_70 = bsl(ctx, &[make_small(1), make_small(70)]).unwrap();
            assert_eq!(parts(two_to_70), (false, vec![0, 64]));
            let two_to_70_plus_1 = add(ctx, &[two_to_70, make_small(1)]).unwrap();
            let minus_two_to_70 = bsl(ctx, &[make_small(-1), make_small(70)]).unwrap();
            assert_eq!(parts(minus_two_to_70), (true, vec![0, 64]));

            // `(1 bsl 70) band ((1 bsl 70)+1)` ⇒ 2^70 = 1180591620717411303424.
            let and = band(ctx, &[two_to_70, two_to_70_plus_1]).unwrap();
            assert_eq!(parts(and), (false, vec![0, 64]));
            // `(1 bsl 70) bor 1` ⇒ 1180591620717411303425.
            let or = bor(ctx, &[two_to_70, make_small(1)]).unwrap();
            assert_eq!(parts(or), (false, vec![1, 64]));
            // `-(1 bsl 70) bor 1` ⇒ -1180591620717411303423 = -(2^70 - 1): the low
            // bit is set in the operand's two's complement, so the result is the
            // operand plus one.
            let or = bor(ctx, &[minus_two_to_70, make_small(1)]).unwrap();
            assert_eq!(parts(or), (true, vec![u64::MAX, 63]));
            // `-(1 bsl 70) bxor (1 bsl 70)` ⇒ -2361183241434822606848 = -(2^71).
            let xor = bxor(ctx, &[minus_two_to_70, two_to_70]).unwrap();
            assert_eq!(parts(xor), (true, vec![0, 128]));
            // `-(1 bsl 70) band (1 bsl 70)` ⇒ 1180591620717411303424.
            let and = band(ctx, &[minus_two_to_70, two_to_70]).unwrap();
            assert_eq!(parts(and), (false, vec![0, 64]));
            // `bnot (1 bsl 70)` ⇒ -1180591620717411303425; the complement of a
            // negative operand is its magnitude minus one.
            let complement = bnot(ctx, &[two_to_70]).unwrap();
            assert_eq!(parts(complement), (true, vec![1, 64]));
            let complement = bnot(ctx, &[minus_two_to_70]).unwrap();
            assert_eq!(parts(complement), (false, vec![u64::MAX, 63]));
            // `(((1 bsl 70) - 1) band 16#F0F0)` ⇒ 61680 (a small result: the bit
            // operators fold back into the 60-bit range whenever they fit).
            let low_bits = sub(ctx, &[two_to_70, make_small(1)]).unwrap();
            let masked = band(ctx, &[low_bits, make_small(0xF0F0)]).unwrap();
            assert_eq!(int(masked), 61680);
            assert!(access::is_small(masked));
        });
    }

    #[test]
    fn bitwise_operations_agree_with_i128_arithmetic() {
        with_ctx(|ctx| {
            // Two's complement on `i128` is the same operation, so every pair of
            // small/bignum operands (all within `2^70`) can be checked exactly.
            let two_to_70: i128 = 1 << 70;
            let operands: [(i128, Eterm); 7] = [
                (0, make_small(0)),
                (1, make_small(1)),
                (-1, make_small(-1)),
                (i128::from(MAX_SMALL), make_small(MAX_SMALL)),
                (i128::from(MIN_SMALL), make_small(MIN_SMALL)),
                (two_to_70, bignum(ctx, false, &[0, 64])),
                (-two_to_70, bignum(ctx, true, &[0, 64])),
            ];
            for &(x, x_term) in &operands {
                assert_eq!(value(bnot(ctx, &[x_term]).unwrap()), !x, "bnot {x}");
                for &(y, y_term) in &operands {
                    assert_eq!(value(band(ctx, &[x_term, y_term]).unwrap()), x & y, "{x} band {y}");
                    assert_eq!(value(bor(ctx, &[x_term, y_term]).unwrap()), x | y, "{x} bor {y}");
                    assert_eq!(value(bxor(ctx, &[x_term, y_term]).unwrap()), x ^ y, "{x} bxor {y}");
                }
            }
        });
    }

    #[test]
    fn shifts_match_the_oracle() {
        with_ctx(|ctx| {
            // Pinned to `erl -noshell -eval`: `1 bsl 10` ⇒ 1024, `-1 bsl 4` ⇒ -16,
            // `8 bsl -2` ⇒ 2, `255 bsr 4` ⇒ 15, `-1 bsr 4` ⇒ -1, `-3 bsr 1` ⇒ -2,
            // `1 bsr -2` ⇒ 4, `255 bsr 100` ⇒ 0, `-255 bsr 100` ⇒ -1.
            assert_eq!(int(bsl(ctx, &[make_small(1), make_small(10)]).unwrap()), 1024);
            assert_eq!(int(bsl(ctx, &[make_small(-1), make_small(4)]).unwrap()), -16);
            assert_eq!(int(bsl(ctx, &[make_small(8), make_small(-2)]).unwrap()), 2);
            assert_eq!(int(bsr(ctx, &[make_small(255), make_small(4)]).unwrap()), 15);
            assert_eq!(int(bsr(ctx, &[make_small(-1), make_small(4)]).unwrap()), -1);
            assert_eq!(int(bsr(ctx, &[make_small(-3), make_small(1)]).unwrap()), -2);
            assert_eq!(int(bsr(ctx, &[make_small(1), make_small(-2)]).unwrap()), 4);
            assert_eq!(int(bsr(ctx, &[make_small(255), make_small(100)]).unwrap()), 0);
            assert_eq!(int(bsr(ctx, &[make_small(-255), make_small(100)]).unwrap()), -1);
            // The degenerate counts: `5 bsl 0` ⇒ 5, `5 bsr 0` ⇒ 5, `0 bsr 0` ⇒ 0.
            assert_eq!(int(bsl(ctx, &[make_small(5), make_small(0)]).unwrap()), 5);
            assert_eq!(int(bsr(ctx, &[make_small(5), make_small(0)]).unwrap()), 5);
            assert_eq!(int(bsr(ctx, &[make_small(0), make_small(0)]).unwrap()), 0);

            // `1 bsl 70` ⇒ 1180591620717411303424 (a bignum), and the big-value
            // shifts pinned through the oracle:
            // `(1 bsl 70) bsr 65` ⇒ 32, `(1 bsl 70) bsl 3` ⇒ 2^73,
            // `(1 bsl 70) bsl -3` ⇒ 2^67, `(1 bsl 70) bsr -3` ⇒ 2^73,
            // `-(1 bsl 70) bsl -3` ⇒ -(2^67), `-(1 bsl 70) bsr -3` ⇒ -(2^73),
            // `-(1 bsl 70) bsr 69` ⇒ -2, `-(1 bsl 70) bsr 70` ⇒ -1,
            // `-(1 bsl 70) bsr 100` ⇒ -1, `(1 bsl 70) bsr 100` ⇒ 0.
            let two_to_70 = bsl(ctx, &[make_small(1), make_small(70)]).unwrap();
            assert_eq!(parts(two_to_70), (false, vec![0, 64]));
            let minus_two_to_70 = bsl(ctx, &[make_small(-1), make_small(70)]).unwrap();
            assert_eq!(parts(minus_two_to_70), (true, vec![0, 64]));
            let three = make_small(3);

            assert_eq!(int(bsr(ctx, &[two_to_70, make_small(65)]).unwrap()), 32);
            assert_shift("(1 bsl 70) bsl 3", bsl(ctx, &[two_to_70, three]), false, 1 << 73);
            assert_shift(
                "(1 bsl 70) bsl -3",
                bsl(ctx, &[two_to_70, make_small(-3)]),
                false,
                1 << 67,
            );
            assert_shift(
                "(1 bsl 70) bsr -3",
                bsr(ctx, &[two_to_70, make_small(-3)]),
                false,
                1 << 73,
            );
            assert_shift(
                "-(1 bsl 70) bsl -3",
                bsl(ctx, &[minus_two_to_70, make_small(-3)]),
                true,
                1 << 67,
            );
            assert_shift(
                "-(1 bsl 70) bsr -3",
                bsr(ctx, &[minus_two_to_70, make_small(-3)]),
                true,
                1 << 73,
            );
            assert_shift(
                "-(1 bsl 70) bsr 69",
                bsr(ctx, &[minus_two_to_70, make_small(69)]),
                true,
                2,
            );
            assert_eq!(int(bsr(ctx, &[minus_two_to_70, make_small(70)]).unwrap()), -1);
            assert_eq!(int(bsr(ctx, &[minus_two_to_70, make_small(100)]).unwrap()), -1);
            assert_eq!(int(bsr(ctx, &[two_to_70, make_small(100)]).unwrap()), 0);

            // A shift of a bignum whose low bits are set: the arithmetic right shift
            // rounds a negative value toward negative infinity, so the discarded
            // fraction adds one to the magnitude:
            // `-((1 bsl 70) + 1) bsr 1` ⇒ -(2^69 + 1) = -590295810358705651713.
            let odd = sub(ctx, &[minus_two_to_70, make_small(1)]).unwrap();
            assert_eq!(parts(odd), (true, vec![1, 64]));
            assert_shift(
                "-((1 bsl 70) + 1) bsr 1",
                bsr(ctx, &[odd, make_small(1)]),
                true,
                (1 << 69) + 1,
            );
            assert_eq!(int(bsr(ctx, &[odd, make_small(70)]).unwrap()), -2);
            assert_eq!(int(bsr(ctx, &[odd, make_small(71)]).unwrap()), -1);

            // `1 bsl 100000` succeeds (the mask check uses a 60-bit small constant
            // for the digits, well below `BIG_ARITY_MAX`).
            let wide = bsl(ctx, &[make_small(1), make_small(100_000)]).unwrap();
            assert_eq!(parts(wide).1.len(), 1563, "100000 bits need 1563 limbs");
        });
    }

    #[test]
    fn shift_operands_must_be_integers() {
        with_ctx(|ctx| {
            let one_point_oh = ctx.make_float(1.0).unwrap();
            let a = ctx.atom("a").unwrap();
            // `1 bsl 1.0` and `1.0 bsr 2` are `badarith` in the oracle.
            assert!(matches!(bsl(ctx, &[make_small(1), one_point_oh]), Err(BeamError::BadArith)));
            assert!(matches!(bsr(ctx, &[one_point_oh, make_small(2)]), Err(BeamError::BadArith)));
            assert!(matches!(bsl(ctx, &[a, make_small(2)]), Err(BeamError::BadArith)));
            assert!(matches!(bsr(ctx, &[make_small(2), a]), Err(BeamError::BadArith)));
            // The bitwise operators reject floats and non-numbers too.
            assert!(matches!(band(ctx, &[one_point_oh, make_small(1)]), Err(BeamError::BadArith)));
            assert!(matches!(band(ctx, &[make_small(1), one_point_oh]), Err(BeamError::BadArith)));
            assert!(matches!(bor(ctx, &[one_point_oh, one_point_oh]), Err(BeamError::BadArith)));
            assert!(matches!(bxor(ctx, &[make_small(1), a]), Err(BeamError::BadArith)));
            assert!(matches!(bnot(ctx, &[one_point_oh]), Err(BeamError::BadArith)));
            assert!(matches!(bnot(ctx, &[a]), Err(BeamError::BadArith)));
        });
    }

    #[test]
    fn shift_counts_beyond_the_bignum_limit() {
        with_ctx(|ctx| {
            // `1 bsl (1 bsl 70)` ⇒ `system_limit`: the shift amount does not fit a
            // `usize` and the left operand is non-zero (the C `N bsl PositiveBigNum`
            // arm). The rejection happens before any limb vector is allocated.
            let huge_shift = bignum(ctx, false, &[0, 64]);
            let big_operand = bignum(ctx, false, &[0, 64]);
            let negative_operand = bignum(ctx, true, &[0, 64]);
            assert!(matches!(bsl(ctx, &[make_small(1), huge_shift]), Err(BeamError::SystemLimit)));
            assert!(matches!(bsl(ctx, &[big_operand, huge_shift]), Err(BeamError::SystemLimit)));
            // `0 bsl (1 bsl 70)` ⇒ 0 — the C checks the zero operand first.
            assert_eq!(int(bsl(ctx, &[make_small(0), huge_shift]).unwrap()), 0);
            // `1 bsr (1 bsl 70)` ⇒ 0, `(-1) bsr (1 bsl 70)` ⇒ -1.
            assert_eq!(int(bsr(ctx, &[make_small(1), huge_shift]).unwrap()), 0);
            assert_eq!(int(bsr(ctx, &[make_small(-1), huge_shift]).unwrap()), -1);
            assert_eq!(int(bsr(ctx, &[big_operand, huge_shift]).unwrap()), 0);
            assert_eq!(int(bsr(ctx, &[negative_operand, huge_shift]).unwrap()), -1);

            // A negative huge count flips the direction: a right shift of a non-zero
            // operand by it is `system_limit` (`1 bsr -H`, `-1 bsr -H`), a left shift
            // keeps only the sign (`1 bsl -H` ⇒ 0, `-1 bsl -H` ⇒ -1), and
            // `0 bsl/-bsr <it>` is still `0`.
            let minus_huge_shift = bignum(ctx, true, &[0, 64]);
            assert_eq!(int(bsl(ctx, &[make_small(0), minus_huge_shift]).unwrap()), 0);
            assert_eq!(int(bsl(ctx, &[make_small(1), minus_huge_shift]).unwrap()), 0);
            assert_eq!(int(bsl(ctx, &[make_small(-1), minus_huge_shift]).unwrap()), -1);
            assert_eq!(int(bsr(ctx, &[make_small(0), minus_huge_shift]).unwrap()), 0);
            assert!(matches!(
                bsr(ctx, &[make_small(1), minus_huge_shift]),
                Err(BeamError::SystemLimit)
            ));
            assert!(matches!(
                bsr(ctx, &[make_small(-1), minus_huge_shift]),
                Err(BeamError::SystemLimit)
            ));
            // `-(1 bsl 70)` has a left operand beyond `usize` too, and the same
            // direction flip applies to a bignum operand.
            assert_eq!(int(bsl(ctx, &[negative_operand, minus_huge_shift]).unwrap()), -1);
            assert!(matches!(
                bsr(ctx, &[big_operand, minus_huge_shift]),
                Err(BeamError::SystemLimit)
            ));

            // A count that reaches `BIG_MAX_BITS` (or past it) is rejected before the
            // result is allocated: `bit_len(1) + 4194240 > BIG_MAX_BITS`.
            let limit = i64::try_from(BIG_MAX_BITS).expect("the limit fits a small int");
            assert!(matches!(
                bsl(ctx, &[make_small(1), make_small(limit)]),
                Err(BeamError::SystemLimit)
            ));
            assert!(matches!(
                bsl(ctx, &[make_small(-1), make_small(limit)]),
                Err(BeamError::SystemLimit)
            ));
        });
    }

    #[test]
    fn two_complement_helpers_round_trip() {
        // The `Big` two's-complement layer directly: `from_twos` undoes `to_twos`
        // for every width, including the sign-extension corners (`-1`, `-2^64`).
        let values = [
            Big::zero(),
            Big::from_i64(1),
            Big::from_i64(-1),
            Big::from_i64(i64::MAX),
            Big::from_i64(i64::MIN),
            Big::new(false, vec![0, 1]),
            Big::new(true, vec![0, 1]),
            Big::new(true, vec![u64::MAX, u64::MAX]),
        ];
        for value in &values {
            for width in [value.mag.len() + 1, value.mag.len() + 3] {
                let limbs = value.to_twos(width);
                assert_eq!(limbs.len(), width);
                assert_eq!(&Big::from_twos(&limbs, value.negative), value, "{value:?}");
                // The top limb is pure sign extension.
                let extension = if value.negative { u64::MAX } else { 0 };
                assert_eq!(limbs[width - 1], extension, "{value:?}");
            }
        }
    }

    #[test]
    fn register_installs_the_quoted_operator_names() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (name, arity) in [
            ("+", 2),
            ("-", 2),
            ("*", 2),
            ("/", 2),
            ("div", 2),
            ("rem", 2),
            ("abs", 1),
            ("-", 1),
            ("+", 1),
            ("band", 2),
            ("bor", 2),
            ("bxor", 2),
            ("bnot", 1),
            ("bsl", 2),
            ("bsr", 2),
        ] {
            assert!(reg.contains("erlang", name, arity), "missing erlang:{name}/{arity}");
        }
        assert_eq!(reg.len(), 15);
    }
}
