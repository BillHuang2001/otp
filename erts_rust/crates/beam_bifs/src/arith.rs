//! Arithmetic BIFs for `erlang`: `+`, `-`, `*`, `/`, `div`, `rem`, `abs` and the
//! unary `-`/`+`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_bif_op.c` (the operator
//! BIFs and their guard variants), `erl_arith.c` (`erts_arith_*`) and `big.c`
//! (`big_plus`/`big_minus`/`big_times`/`big_div`/`big_rem`/`big_to_double`), plus
//! the `ubif erlang:'+'/2` … roster in `../../erts/emulator/beam/bif.tab`
//! (lines 295–316: `'>' '>=' '<' '=<' '=:=' '==' '=/' '/=' '+' '-' '*' '/' 'div'
//! 'rem' '-' '/'`). The names are registered *quoted*, exactly as `bif.tab`
//! spells them.
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
//! Term *ordering* — including bignums — is [`beam_term::compare`]'s, used by
//! [`crate::compare`]; this module only needs the magnitude comparison
//! ([`cmp_mag`]) that its sign handling composes.
//!
//! # Documented limits
//!
//! A result whose magnitude exceeds `BIG_ARITY_MAX` digits is
//! [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) (reported by
//! [`beam_codec::term::make_big`]) and a full destination heap is the allocator's
//! error; neither is reachable from 60-bit operands.

use core::cmp::Ordering;

use beam_common::Result;
use beam_term::{signed_value, Eterm, MAX_SMALL, MIN_SMALL};

use crate::access;
use crate::context::BifContext;
use crate::error::badarith;
use crate::registry::BifRegistry;

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

/// `A + B`.
fn add(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match (number(args[0])?, number(args[1])?) {
        (Num::Int(x), Num::Int(y)) => int_term(ctx, &x.add(&y)),
        (x, y) => ctx.make_float(as_float(&x)? + as_float(&y)?),
    }
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

    /// The `f64` nearest to `self`, ties to even — the C `big_to_double`.
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
/// # Errors
///
/// `badarith` when the magnitude has more than 1024 bits (at least `2^1024`, above
/// the largest finite double) or when rounding carries out of it.
fn mag_to_f64(mag: &[u64]) -> Result<f64> {
    let limbs = mag.len();
    if limbs == 0 {
        return Ok(0.0);
    }
    let top_bits = 64 - mag[limbs - 1].leading_zeros() as usize;
    let bit_len = (limbs - 1) * 64 + top_bits;
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
        ] {
            assert!(reg.contains("erlang", name, arity), "missing erlang:{name}/{arity}");
        }
        assert_eq!(reg.len(), 9);
    }
}
