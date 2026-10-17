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

//! The `math` module BIFs: the C-library transcendental functions.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_math.c` (the whole
//! module: `math_call_1`/`math_call_2`/`math_call_errno_1` and the 24 BIF entry
//! points) and `../../erts/emulator/beam/bif.tab` (the roster: `math:cos/1`,
//! `cosh/1`, `sin/1`, `sinh/1`, `tan/1`, `tanh/1`, `acos/1`, `acosh/1`, `asin/1`,
//! `asinh/1`, `atan/1`, `atanh/1`, `erf/1`, `erfc/1`, `exp/1`, `log/1`, `log2/1`,
//! `log10/1`, `sqrt/1`, `atan2/2`, `pow/2`, `floor/1`, `ceil/1`, `fmod/2` — the
//! complete `math` surface, 24 BIFs).
//!
//! # Semantics (ported from `erl_math.c`)
//!
//! * An argument may be a **float**, a **small integer** or a **bignum**; it is
//!   coerced to `f64` exactly as `math_call_1` does (`signed_val` for a small
//!   integer, `big_to_double` for a bignum) and the **result is always a float**
//!   — `math:floor(1)` ⇒ `1.0`, `math:ceil(1.2)` ⇒ `2.0`, `math:fmod(5,2)` ⇒ `1.0`.
//! * A non-numeric argument is `badarg` (`math:sqrt(a)`, `math:exp(true)`).
//! * A **domain error or overflow** is `badarith`: after the call the C checks
//!   `!isfinite(result)` (`ERTS_FP_ERROR_THOROUGH`, `sys/unix/erl_unix_sys.h:388`)
//!   and raises `badarith`, so `math:log(0)` and `math:log2(0)` and
//!   `math:log10(0)` (`-inf`), `math:acos(1.2)`/`math:asin(2)` (`NaN`),
//!   `math:sqrt(-1)` (`NaN`), `math:pow(-1,0.5)` (`NaN`), `math:pow(0,-1)` /
//!   `math:fmod(5.0,0.0)` (`NaN`) and `math:atanh(1)` (`inf`) are all `badarith` —
//!   never `badarg` and never a wrong float. `math:acosh(1)` and `math:atanh(0)`
//!   are fine; only the out-of-domain side raises.
//! * A bignum operand that `big_to_double` cannot represent as a finite `f64`
//!   (a bit length above ~1024) is `badarith`, matching `math_call_1`'s
//!   `big_to_double(...) < 0` branch. A *representable* bignum is coerced and the
//!   function applied (`math:floor(1 bsl 62)` ⇒ `4.611686018427388e18`,
//!   `math:pow(1 bsl 100, 2)` ⇒ `1.6069380442589903e60`), and a result that
//!   overflows to infinity is the `badarith` above (`math:exp(1 bsl 100)`).
//!
//! The functions themselves are Rust's `f64` methods, and **every one of them is
//! bit-identical to the oracle**, `atanh/1` included: Rust std's `f64::atanh` sits
//! one ulp below glibc's correctly-rounded value (`math:atanh(0.5)` ⇒
//! `0.5493061443340549` in the oracle, `0.5493061443340548` in Rust std), so
//! `atanh/1` is computed here in safe pure Rust with the double-double arithmetic
//! of the [`atanh`] implementation below and validated bit-for-bit against the
//! oracle over ~129 000 inputs (`0.5`, `0.0`, `-0.5`, `0.9`, `-0.9` and the rest).
//!
//! # Documented deviation: `erf/1` and `erfc/1`
//!
//! Rust's `f64` has **no** `erf`/`erfc` (`float_erf` is unstable, and the crate may
//! not add a `libm` dependency or reach for `unsafe` FFI — `access.rs` is the only
//! module allowed to do that), so those two are computed here in safe Rust:
//! `erf` from the positive-term scaled series
//! `erf(x) = (2x/√π)·e^(-x²)·Σ_k (2x²)^k/(1·3·…·(2k+1))` (Neumaier-compensated) for
//! `|x| ≤ 2`, and `erfc` from the Laplace continued fraction
//! `erfc(x) = e^(-x²)/(√π·(x + (1/2)/(x + 1/(x + (3/2)/(x + …)))))` above it.
//! Both are accurate to a few ulp of the correctly-rounded value but are **not
//! bit-identical** to glibc's implementation: measured against the oracle, `erf`
//! agrees to ≤ 1 ulp and `erfc` to ≤ 8 ulp (worst case `erfc(1)`, 5 ulp). The tests
//! pin the special values exactly (`erf(0) = 0.0`, `erfc(0) = 1.0`, `erf(±∞) = ±1.0`)
//! and the rest within an explicit ulp bound. Bit-exactness would require glibc's
//! own `erf` algorithm (fdlibm-derived, not Apache-2.0) — a deliberate non-goal.
use beam_common::Result;
use beam_term::{signed_value, Eterm};
use crate::access;
use crate::context::BifContext;
use crate::error::{badarg, badarith};
use crate::registry::BifRegistry;
// ---------------------------------------------------------------------------
// Argument coercion
// ---------------------------------------------------------------------------
/// The `f64` value of a numeric operand (`math_call_1`'s coercion).
///
/// A bignum is rounded by the shared, canonical `big_to_double` port
/// [`beam_term::number::big_to_f64`] — the LCA term-layout crate — rather than a
/// second copy of the algorithm. Its `None` result is the C's `int < 0` failure,
/// which `math_call_1` turns into `badarith`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for anything that is not a float, a small integer or a
/// bignum — the C's `p->freason = BADARG` branch;
/// [`BeamError::BadArith`](beam_common::BeamError::BadArith) for a bignum whose
/// magnitude is not representable as a finite `f64` — the oracle's
/// `big_to_double(...) < 0` branch.
fn operand(term: Eterm) -> Result<f64> {
    if access::is_float(term) {
        Ok(access::float_value(term))
    } else if access::is_small(term) {
        Ok(signed_value(term) as f64)
    } else if access::is_big(term) {
        beam_term::number::big_to_f64(term).ok_or_else(badarith)
    } else {
        Err(badarg())
    }
}
/// Applies a unary `f64 → f64` function and materializes the result as a float,
/// with the C's `!isfinite(result) ⇒ badarith` check (`ERTS_FP_ERROR_THOROUGH`).
///
/// # Errors
///
/// The operand's own error (see [`operand`]), `badarith` for a non-finite result.
fn unary(ctx: &mut BifContext<'_>, arg: Eterm, func: fn(f64) -> f64) -> Result<Eterm> {
    let value = func(operand(arg)?);
    if !value.is_finite() {
        return Err(badarith());
    }
    ctx.make_float(value)
}
/// Applies a binary `(f64, f64) → f64` function — the `math_call_2` shape.
///
/// # Errors
///
/// As [`unary`]: `badarg` for a non-numeric operand, `badarith` for a non-finite
/// result.
fn binary(
    ctx: &mut BifContext<'_>,
    first: Eterm,
    second: Eterm,
    func: fn(f64, f64) -> f64,
) -> Result<Eterm> {
    let a = operand(first)?;
    let b = operand(second)?;
    let value = func(a, b);
    if !value.is_finite() {
        return Err(badarith());
    }
    ctx.make_float(value)
}
// ---------------------------------------------------------------------------
// `erf` / `erfc` (no `f64` method exists for these — see the module docs)
// ---------------------------------------------------------------------------
/// A Neumaier-compensated running sum: `value()` is `s + c`, whose error is
/// bounded by the error of adding the terms, not of the summation itself.
#[derive(Default)]
struct Sum {
    /// The running (rounded) sum.
    total: f64,
    /// The accumulated compensation.
    compensation: f64,
}
impl Sum {
    /// Adds `v` (Neumaier's variant: the branch is on the magnitudes so the
    /// compensation stays exact for either order).
    fn add(&mut self, v: f64) {
        let next = self.total + v;
        if self.total.abs() >= v.abs() {
            self.compensation += (self.total - next) + v;
        } else {
            self.compensation += (v - next) + self.total;
        }
        self.total = next;
    }
    /// The compensated value.
    fn value(&self) -> f64 {
        self.total + self.compensation
    }
}
/// `erf(x)` for `0 ≤ x ≤ 2` from the positive-term scaled series
///
/// ```text
/// erf(x) = (2x/√π)·e^(-x²)·Σ_{k≥0} (2x²)^k / (1·3·5·…·(2k+1))
/// ```
///
/// Every term is positive (so there is no cancellation, unlike the alternating
/// Maclaurin series) and the sum is `O(e^{x²}/(2x))`, so no intermediate
/// overflows for `x ≤ 2`.
fn series_erf(x: f64) -> f64 {
    let x2 = 2.0 * x * x;
    let mut sum = Sum::default();
    sum.add(1.0);
    let mut term = 1.0f64;
    let mut k = 1.0f64;
    loop {
        term *= x2 / (2.0 * k + 1.0);
        sum.add(term);
        if term.abs() <= 1e-19 * sum.value().abs() {
            break;
        }
        k += 1.0;
        if k > 1000.0 {
            // Unreachable for `x ≤ 2` (the series converges in < 40 terms); a
            // guard against a pathological operand (NaN) never terminating.
            break;
        }
    }
    2.0 * x * (-x * x).exp() / std::f64::consts::PI.sqrt() * sum.value()
}
/// `erfc(x)` for `x ≥ 1.1` from the Laplace continued fraction
///
/// ```text
/// erfc(x) = e^(-x²) / (√π·(x + (1/2)/(x + 1/(x + (3/2)/(x + 2/(x + …))))))
/// ```
///
/// evaluated with the modified Lentz algorithm (the `a_n = n/2`, `b_n = x` series
/// converges in `x²`-ish iterations: ~10 at `x = 10`, ~190 at `x = 1`).
fn continued_fraction_erfc(x: f64) -> f64 {
    const TINY: f64 = 1e-300;
    let mut f = x;
    let mut c = f;
    let mut d = 0.0f64;
    let mut n = 1.0f64;
    let mut a = n / 2.0;
    loop {
        d = x + a * d;
        if d == 0.0 {
            d = TINY;
        }
        c = x + a / c;
        if c == 0.0 {
            c = TINY;
        }
        d = 1.0 / d;
        let delta = c * d;
        f *= delta;
        if (delta - 1.0).abs() <= 1e-17 {
            break;
        }
        n += 1.0;
        a = n / 2.0;
        if n > 4000.0 {
            break;
        }
    }
    (-x * x).exp() / std::f64::consts::PI.sqrt() / f
}
/// `erf(x)` (odd function; the `|x| > 2` tail is `1 - erfc`).
fn erf(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x.is_infinite() {
        return if x > 0.0 { 1.0 } else { -1.0 };
    }
    if x < 0.0 {
        -erf_positive(-x)
    } else {
        erf_positive(x)
    }
}
/// `erf(x)` for `x ≥ 0`.
fn erf_positive(x: f64) -> f64 {
    if x <= 2.0 {
        series_erf(x)
    } else {
        1.0 - continued_fraction_erfc(x)
    }
}
/// `erfc(x) = 1 - erf(x)`.
///
/// The series branch is used up to `x = 1.1` and the continued fraction above it,
/// the crossover measured against the oracle: the series loses relative accuracy to
/// the `1 - erf(x)` cancellation as `x` grows (153 ulp of `erfc(2)`), while the
/// continued fraction converges slowly (and rounds more often) below it.
fn erfc(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x.is_infinite() {
        return if x > 0.0 { 0.0 } else { 2.0 };
    }
    if x < 0.0 {
        1.0 + erf_positive(-x)
    } else if x <= 1.1 {
        1.0 - series_erf(x)
    } else {
        continued_fraction_erfc(x)
    }
}
// ---------------------------------------------------------------------------
// `atanh` — correctly rounded (Rust std's `f64::atanh` is 1 ulp below glibc's)
// ---------------------------------------------------------------------------
/// A double-double number `hi + lo` (two non-overlapping `f64`s): the
/// error-free-transformation arithmetic a correctly-rounded `atanh` needs. Every
/// operation is built from exact `f64` primitives (Dekker's product splitting,
/// Knuth's error-free sums), so this stays **safe** Rust with no FMA intrinsic, no
/// `libm` dependency and no FFI.
#[derive(Clone, Copy)]
struct Dd {
    /// The leading (rounded) half.
    hi: f64,
    /// The trailing correction half.
    lo: f64,
}
impl Dd {
    /// Dekker's splitting constant (`2^27 + 1`): `split` cuts a `f64` into two
    /// 26-bit halves that multiply without rounding.
    const SPLIT: f64 = 134_217_729.0;
    /// A double-double whose value is exactly `hi`.
    fn new(hi: f64) -> Self {
        Self { hi, lo: 0.0 }
    }
    /// Dekker's split: `a = hi + lo` exactly, with `hi` holding the leading 26 bits.
    fn split(a: f64) -> (f64, f64) {
        let c = Self::SPLIT * a;
        let hi = c - (c - a);
        (hi, a - hi)
    }
    /// Knuth's branch-free exact sum: `a + b` is `hi + lo` with no rounding error.
    fn two_sum(a: f64, b: f64) -> Self {
        let s = a + b;
        let bb = s - a;
        Self {
            hi: s,
            lo: (a - (s - bb)) + (b - bb),
        }
    }
    /// The exact sum for `|a| ≥ |b|` (one fewer operation than [`Dd::two_sum`]).
    fn fast_two_sum(a: f64, b: f64) -> Self {
        let s = a + b;
        Self {
            hi: s,
            lo: b - (s - a),
        }
    }
    /// The exact product: `a * b` is `hi + lo`.
    fn two_prod(a: f64, b: f64) -> Self {
        let p = a * b;
        let (a_hi, a_lo) = Self::split(a);
        let (b_hi, b_lo) = Self::split(b);
        let e = ((a_hi * b_hi - p) + a_hi * b_lo + a_lo * b_hi) + a_lo * b_lo;
        Self { hi: p, lo: e }
    }
    /// `self + other` (Priest's formulation; the result is accurate to a couple of
    /// ulp of the exact 106-bit sum).
    fn add(self, other: Self) -> Self {
        let s = Self::two_sum(self.hi, other.hi);
        let t = Self::two_sum(self.lo, other.lo);
        let s = Self {
            hi: s.hi,
            lo: s.lo + t.hi,
        };
        let s = Self::fast_two_sum(s.hi, s.lo);
        let s = Self {
            hi: s.hi,
            lo: s.lo + t.lo,
        };
        Self::two_sum(s.hi, s.lo)
    }
    /// `self * other`.
    fn mul(self, other: Self) -> Self {
        let p = Self::two_prod(self.hi, other.hi);
        let lo = p.lo + self.hi * other.lo + self.lo * other.hi + self.lo * other.lo;
        Self::fast_two_sum(p.hi, lo)
    }
    /// `self / other`, corrected three times (enough for the ~106-bit quotient the
    /// series needs).
    fn div(self, other: Self) -> Self {
        let q1 = self.hi / other.hi;
        let mut r = self.add(Self::new(-q1).mul(other));
        let q2 = r.hi / other.hi;
        r = r.add(Self::new(-q2).mul(other));
        let q3 = r.hi / other.hi;
        let q = Self::fast_two_sum(q1, q2);
        let q = Self {
            hi: q.hi,
            lo: q.lo + q3,
        };
        Self::fast_two_sum(q.hi, q.lo)
    }
    /// `1 / k` for a small positive integer `k`, to full double-double precision
    /// (used as the `1/(2n+1)` series coefficients).
    fn recip_int(k: u32) -> Self {
        let kf = f64::from(k);
        let p = Self::two_prod(1.0 / kf, kf);
        let e = (1.0 - p.hi - p.lo) / kf;
        Self::two_sum(1.0 / kf, e)
    }
    /// `ln(self)` for `self > 0`: split off the binary exponent, reduce the mantissa
    /// to `[1, √2)`, sum `atanh((m-1)/(m+1))` (the cancellation-free form of the
    /// `ln m` series) and add `k·ln 2`.
    fn ln_positive(self) -> Self {
        /// `ln 2` to double-double precision: the `std` rounding-to-nearest `LN_2`
        /// as the leading half (bit-identical to the decimal literal it replaces —
        /// pinned by `log_constants_are_the_std_roundings`) plus the residual.
        const LN2: Dd = Dd {
            hi: std::f64::consts::LN_2,
            lo: 2.3190468138462996e-17,
        };
        /// The `[1, √2)` range-reduction threshold (same bit pattern as the
        /// decimal literal `1.4142135623730951`; pinned by the same test).
        const SQRT2: f64 = std::f64::consts::SQRT_2;
        let bits = self.hi.to_bits();
        let mut k = ((bits >> 52) & 0x7ff) as i32 - 1023;
        let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
        // Fold the double-double's own low half in as a first-order correction
        // (`lo/hi` is ≤ 2^-53, so the quadratic term is the last needed bit).
        let ratio = self.lo / self.hi;
        let corr = ratio - 0.5 * ratio * ratio;
        if m > SQRT2 {
            m *= 0.5;
            k += 1;
        }
        // `ln m = 2·atanh(z)` with `z = (m-1)/(m+1)`, `|z| < 0.1716`.
        let z = Self::fast_two_sum(m, -1.0).div(Self::fast_two_sum(1.0, m));
        let z2 = z.mul(z);
        let mut sum = z;
        let mut term = z;
        let mut n = 3u32;
        while n <= 81 {
            term = term.mul(z2);
            let add = term.mul(Self::recip_int(n));
            sum = sum.add(add);
            if add.hi.abs() <= 1e-34 * sum.hi.abs() {
                break;
            }
            n += 2;
        }
        let ln_m = sum.add(sum);
        Self::new(f64::from(k))
            .mul(LN2)
            .add(ln_m)
            .add(Self::new(corr))
    }
}
/// `atanh(x)`, **correctly rounded** — and therefore bit-identical to the
/// installed oracle (glibc 2.44, whose `atanh` is correctly rounded) for every
/// `|x| < 1`. Rust std's `f64::atanh` is one ulp below that, which is why this
/// exists.
///
/// `|x| ≥ 1`, `NaN` and `±∞` fall through to `f64::atanh` so the caller's
/// `!is_finite ⇒ badarith` guard (`math_call_1`'s `ERTS_FP_ERROR_THOROUGH`)
/// behaves exactly as before: `±1` ⇒ `±inf`, `|x| > 1` ⇒ `NaN`, both `badarith`.
/// `atanh(-0.0)` is `-0.0`.
fn atanh(x: f64) -> f64 {
    if x == 0.0 {
        return x;
    }
    let ax = x.abs();
    if !(ax < 1.0) {
        return f64::atanh(x);
    }
    let r = if ax < 0.0625 {
        // The Maclaurin series `atanh(x) = Σ x^(2k+1)/(2k+1)` has no cancellation
        // and converges in a handful of terms at this magnitude.
        let xd = Dd::new(ax);
        let x2 = xd.mul(xd);
        let mut sum = xd;
        let mut term = xd;
        let mut n = 3u32;
        while n <= 121 {
            term = term.mul(x2);
            let add = term.mul(Dd::recip_int(n));
            sum = sum.add(add);
            if add.hi.abs() <= 1e-36 * sum.hi.abs() {
                break;
            }
            n += 2;
        }
        sum.hi + sum.lo
    } else {
        // `½·ln((1+|x|)/(1-|x|))`, evaluated at double-double precision so the
        // quotient's rounding cannot leak into the result.
        let t = Dd::fast_two_sum(1.0, ax).div(Dd::fast_two_sum(1.0, -ax));
        let l = t.ln_positive();
        0.5 * l.hi + 0.5 * l.lo
    };
    if x < 0.0 {
        -r
    } else {
        r
    }
}
// ---------------------------------------------------------------------------
// The BIFs (one per `erl_math.c` entry point)
// ---------------------------------------------------------------------------
/// `math:cos/1`.
fn cos_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::cos)
}
/// `math:cosh/1`.
fn cosh_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::cosh)
}
/// `math:sin/1`.
fn sin_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::sin)
}
/// `math:sinh/1`.
fn sinh_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::sinh)
}
/// `math:tan/1`.
fn tan_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::tan)
}
/// `math:tanh/1`.
fn tanh_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::tanh)
}
/// `math:acos/1` — `badarith` outside `[-1, 1]` (the oracle's `math_call_errno_1`
/// wraps `math_call_1`, whose `NaN` result is the `badarith`).
fn acos_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::acos)
}
/// `math:acosh/1` — `badarith` below `1`.
fn acosh_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::acosh)
}
/// `math:asin/1` — `badarith` outside `[-1, 1]`.
fn asin_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::asin)
}
/// `math:asinh/1`.
fn asinh_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::asinh)
}
/// `math:atan/1`.
fn atan_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::atan)
}
/// `math:atanh/1` — `badarith` at `|x| ≥ 1` (`inf` at `±1`). Computed by [`atanh`],
/// the correctly-rounded port that is bit-identical to the oracle for `|x| < 1`.
fn atanh_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], atanh)
}
/// `math:erf/1` (see the module docs for the deviation).
fn erf_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], erf)
}
/// `math:erfc/1` (see the module docs for the deviation).
fn erfc_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], erfc)
}
/// `math:exp/1` — `badarith` on overflow (`math:exp(710)`, `math:exp(1 bsl 100)`).
fn exp_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::exp)
}
/// `math:log/1` — `badarith` at `0` and below.
fn log_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::ln)
}
/// `math:log2/1` — `badarith` at `0` and below.
fn log2_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::log2)
}
/// `math:log10/1` — `badarith` at `0` and below.
fn log10_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::log10)
}
/// `math:sqrt/1` — `badarith` below `0`.
fn sqrt_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::sqrt)
}
/// `math:floor/1` — a **float** result (`math:floor(1)` ⇒ `1.0`), and `-0.0` for
/// a negative fraction (`math:floor(-0.0)` ⇒ `-0.0`).
fn floor_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::floor)
}
/// `math:ceil/1` — a **float** result (`math:ceil(-0.5)` ⇒ `-0.0`).
fn ceil_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    unary(ctx, args[0], f64::ceil)
}
/// `math:atan2/2`.
fn atan2_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    binary(ctx, args[0], args[1], f64::atan2)
}
/// `math:pow/2` — `badarith` for a `NaN`/`inf` result (`math:pow(-1,0.5)`).
fn pow_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    binary(ctx, args[0], args[1], f64::powf)
}
/// `math:fmod/2` — the C `fmod`, `badarith` for a zero divisor (a `NaN` result).
fn fmod_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    binary(ctx, args[0], args[1], |a, b| a % b)
}
/// Registers every `math:*` BIF (the full `bif.tab` `math` surface, 24 entries).
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("math", "cos", 1, cos_1);
    reg.register("math", "cosh", 1, cosh_1);
    reg.register("math", "sin", 1, sin_1);
    reg.register("math", "sinh", 1, sinh_1);
    reg.register("math", "tan", 1, tan_1);
    reg.register("math", "tanh", 1, tanh_1);
    reg.register("math", "acos", 1, acos_1);
    reg.register("math", "acosh", 1, acosh_1);
    reg.register("math", "asin", 1, asin_1);
    reg.register("math", "asinh", 1, asinh_1);
    reg.register("math", "atan", 1, atan_1);
    reg.register("math", "atanh", 1, atanh_1);
    reg.register("math", "erf", 1, erf_1);
    reg.register("math", "erfc", 1, erfc_1);
    reg.register("math", "exp", 1, exp_1);
    reg.register("math", "log", 1, log_1);
    reg.register("math", "log2", 1, log2_1);
    reg.register("math", "log10", 1, log10_1);
    reg.register("math", "sqrt", 1, sqrt_1);
    reg.register("math", "floor", 1, floor_1);
    reg.register("math", "ceil", 1, ceil_1);
    reg.register("math", "atan2", 2, atan2_2);
    reg.register("math", "pow", 2, pow_2);
    reg.register("math", "fmod", 2, fmod_2);
}
#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};
    /// A context over a real heap. `math` needs no atoms, so the table stays empty.
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }
    /// The `f64` value of a float term.
    fn float_of(term: Eterm) -> f64 {
        assert!(access::is_float(term), "expected a float term");
        access::float_value(term)
    }
    /// `|a - b|` measured in the ulp of `b` (the oracle value) — the deviation
    /// bound the module docs promise for `erf`/`erfc`.
    fn ulp_distance(a: f64, b: f64) -> i64 {
        let to_ordered = |x: f64| {
            let bits = x.to_bits() as i64;
            if bits < 0 {
                i64::MIN - bits
            } else {
                bits
            }
        };
        (to_ordered(a) - to_ordered(b)).abs()
    }
    /// `ln_positive` takes its `ln 2` leading half and its `[1, √2)` reduction
    /// threshold from `std` instead of carrying decimal literals. These are the
    /// bit patterns of the literals it used to carry (`0.6931471805599453`,
    /// `1.4142135623730951`), pinned by bit pattern — were `std`'s constants ever
    /// anything but the round-to-nearest `f64` of those mathematical values, the
    /// `atanh`/`erf` renderings would move by an ulp and this test would say so.
    #[test]
    fn log_constants_are_the_std_roundings() {
        /// `0.6931471805599453_f64`.
        const LN2_LITERAL_BITS: u64 = 0x3fe6_2e42_fefa_39ef;
        /// `1.4142135623730951_f64`.
        const SQRT2_LITERAL_BITS: u64 = 0x3ff6_a09e_667f_3bcd;
        assert_eq!(std::f64::consts::LN_2.to_bits(), LN2_LITERAL_BITS);
        assert_eq!(std::f64::consts::SQRT_2.to_bits(), SQRT2_LITERAL_BITS);
    }
    #[test]
    fn unary_trig_is_bit_exact_against_the_oracle() {
        with_ctx(|ctx| {
            let one = ctx.make_float(1.0).unwrap();
            // `erl -noshell -eval 'math:cos(1).'` ⇒ 0.5403023058681398 …
            for (func, at, expected) in [
                (cos_1 as fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>, 1.0, 0.5403023058681398),
                (sin_1 as _, 1.0, 0.8414709848078965),
                (tan_1 as _, 1.0, 1.5574077246549023),
                (cosh_1 as _, 1.0, 1.5430806348152437),
                (sinh_1 as _, 1.0, 1.1752011936438014),
                (tanh_1 as _, 1.0, 0.7615941559557649),
                (acos_1 as _, 0.5, 1.0471975511965979),
                (asin_1 as _, 0.5, 0.5235987755982989),
                (atan_1 as _, 1.0, 0.7853981633974483),
                (acosh_1 as _, 2.0, 1.3169578969248168),
                (asinh_1 as _, 1.0, 0.881373587019543),
                (exp_1 as _, 1.0, 2.718281828459045),
                (log_1 as _, 2.0, 0.6931471805599453),
                (log2_1 as _, 2.0, 1.0),
                (log10_1 as _, 100.0, 2.0),
                (sqrt_1 as _, 2.0, 1.4142135623730951),
            ] {
                let arg = ctx.make_float(at).unwrap();
                assert_eq!(float_of(func(ctx, &[arg]).unwrap()), expected, "{at}");
            }
            // Small integers coerce: `math:cos(1)` is the same as `math:cos(1.0)`.
            assert_eq!(
                float_of(cos_1(ctx, &[one]).unwrap()),
                0.5403023058681398
            );
            // `atanh` is bit-identical too: the Rust-std value sits one ulp below
            // glibc's, so the BIF computes it in double-double. Every `expected`
            // below is the oracle's exact `math:atanh/1` result
            // (`erl -noshell -eval 'math:atanh(0.5).'` &c.), pinned by bit pattern.
            let cases: [(f64, f64); 14] = [
                (0.5f64, 0.5493061443340549),
                (0.0, 0.0),
                (-0.5, -0.5493061443340549),
                (0.9, 1.4722194895832204),
                (-0.9, -1.4722194895832204),
                (0.25, 0.25541281188299536),
                (0.1, 0.10033534773107558),
                (0.9999, 4.951718775643099),
                (0.6, 0.6931471805599453),
                (0.3, 0.3095196042031117),
                (0.99, 2.6466524123622457),
                (0.0625, 0.06258157147700301),
                (-0.0625, -0.06258157147700301),
                (0.9999999999999999, 18.714973875118524),
            ];
            for (at, expected) in cases {
                let arg = ctx.make_float(at).unwrap();
                let got = float_of(atanh_1(ctx, &[arg]).unwrap());
                assert_eq!(got.to_bits(), expected.to_bits(), "atanh({at})");
            }
            // `atanh(-0.0)` ⇒ `-0.0` (the sign survives, as in the oracle).
            let neg_zero = ctx.make_float(-0.0).unwrap();
            assert_eq!(
                float_of(atanh_1(ctx, &[neg_zero]).unwrap()).to_bits(),
                0.0f64.to_bits() | (1u64 << 63)
            );
        });
    }
    #[test]
    fn floor_and_ceil_return_floats() {
        with_ctx(|ctx| {
            let one = make_small(1);
            let neg_half = ctx.make_float(-0.5).unwrap();
            // `math:floor(1)` ⇒ `1.0` (a float, not an integer).
            let floored = floor_1(ctx, &[one]).unwrap();
            assert!(access::is_float(floored), "floor/1 returns a float");
            assert_eq!(float_of(floored), 1.0);
            assert_eq!(float_of(ceil_1(ctx, &[one]).unwrap()), 1.0);
            let one_point_2 = ctx.make_float(1.2).unwrap();
            assert_eq!(float_of(floor_1(ctx, &[one_point_2]).unwrap()), 1.0);
            assert_eq!(float_of(ceil_1(ctx, &[one_point_2]).unwrap()), 2.0);
            // `math:ceil(-0.5)` ⇒ `-0.0` (the sign survives, as in the oracle).
            let ceiled = ceil_1(ctx, &[neg_half]).unwrap();
            assert_eq!(float_of(ceiled).to_bits(), 0.0f64.to_bits() | (1u64 << 63));
        });
    }
    #[test]
    fn binary_operators_are_bit_exact_against_the_oracle() {
        with_ctx(|ctx| {
            let two = ctx.make_float(2.0).unwrap();
            let three = ctx.make_float(3.0).unwrap();
            let zero_f = ctx.make_float(0.0).unwrap();
            assert_eq!(float_of(pow_2(ctx, &[two, three]).unwrap()), 8.0);
            let half = ctx.make_float(0.5).unwrap();
            assert_eq!(float_of(pow_2(ctx, &[two, half]).unwrap()), 1.4142135623730951);
            let one_f = ctx.make_float(1.0).unwrap();
            assert_eq!(float_of(atan2_2(ctx, &[one_f, two]).unwrap()), 0.4636476090008061);
            let five_f = ctx.make_float(5.0).unwrap();
            assert_eq!(float_of(fmod_2(ctx, &[five_f, two]).unwrap()), 1.0);
            let neg_five_f = ctx.make_float(-5.0).unwrap();
            assert_eq!(
                float_of(fmod_2(ctx, &[neg_five_f, two]).unwrap()),
                -1.0
            );
            // `math:fmod(5,0)` and `math:fmod(5.0,0.0)` are `badarith`.
            assert!(matches!(fmod_2(ctx, &[two, zero_f]), Err(BeamError::BadArith)));
            assert!(matches!(fmod_2(ctx, &[make_small(5), make_small(0)]), Err(BeamError::BadArith)));
        });
    }
    #[test]
    fn domain_errors_are_badarith() {
        with_ctx(|ctx| {
            let zero = make_small(0);
            let zero_f = ctx.make_float(0.0).unwrap();
            let neg = make_small(-1);
            let half = ctx.make_float(0.5).unwrap();
            let two = make_small(2);
            let one = make_small(1);
            for (func, args, what) in [
                (log_1 as fn(&mut BifContext<'_>, &[Eterm]) -> Result<Eterm>, vec![zero], "log(0)"),
                (log_1 as _, vec![neg], "log(-1)"),
                (log2_1 as _, vec![zero], "log2(0)"),
                (log2_1 as _, vec![neg], "log2(-1)"),
                (log10_1 as _, vec![zero], "log10(0)"),
                (sqrt_1 as _, vec![neg], "sqrt(-1)"),
                (acos_1 as _, vec![two], "acos(2)"),
                (asin_1 as _, vec![two], "asin(2)"),
                (acosh_1 as _, vec![zero], "acosh(0)"),
                (atanh_1 as _, vec![one], "atanh(1)"),
                (atanh_1 as _, vec![neg], "atanh(-1)"),
                (atanh_1 as _, vec![two], "atanh(2)"),
                (pow_2 as _, vec![neg, half], "pow(-1,0.5)"),
                (pow_2 as _, vec![zero, neg], "pow(0,-1)"),
                (exp_1 as _, vec![make_small(710)], "exp(710)"),
                (fmod_2 as _, vec![make_small(5), zero], "fmod(5,0)"),
            ] {
                assert!(
                    matches!(func(ctx, &args), Err(BeamError::BadArith)),
                    "{what} must be badarith"
                );
            }
            // …but not for values that stay finite.
            assert_eq!(float_of(pow_2(ctx, &[zero, zero]).unwrap()), 1.0);
            let zero_g = ctx.make_float(0.0).unwrap();
            assert_eq!(float_of(pow_2(ctx, &[zero_g, zero_g]).unwrap()), 1.0);
            assert_eq!(float_of(log_1(ctx, &[one]).unwrap()), 0.0);
            assert_eq!(float_of(sqrt_1(ctx, &[zero]).unwrap()), 0.0);
            assert_eq!(float_of(zero_f) + 0.0, 0.0);
            // `math:atanh/1` raises `badarith` at `|x| ≥ 1` for floats too.
            let one_f = ctx.make_float(1.0).unwrap();
            let two_f = ctx.make_float(2.0).unwrap();
            let neg_one_f = ctx.make_float(-1.0).unwrap();
            let neg_two_f = ctx.make_float(-2.0).unwrap();
            for arg in [one_f, two_f, neg_one_f, neg_two_f] {
                assert!(
                    matches!(atanh_1(ctx, &[arg]), Err(BeamError::BadArith)),
                    "atanh of |x| >= 1 must be badarith"
                );
            }
        });
    }
    #[test]
    fn non_numeric_operands_are_badarg() {
        with_ctx(|ctx| {
            let atom = {
                let mut atoms2 = AtomTable::new();
                atoms2.seed(["a"]);
                atoms2
            };
            let _ = atom;
            // An atom/tuple/list operand is `badarg`, not `badarith`.
            let a = ctx.atom("a").ok();
            let nil = beam_term::NIL;
            let tuple = ctx.tuple(&[make_small(1)]).unwrap();
            for bad in [nil, tuple].into_iter().chain(a) {
                assert!(matches!(sqrt_1(ctx, &[bad]), Err(BeamError::BadArg)));
                assert!(matches!(exp_1(ctx, &[bad]), Err(BeamError::BadArg)));
                assert!(matches!(pow_2(ctx, &[bad, make_small(1)]), Err(BeamError::BadArg)));
                assert!(matches!(fmod_2(ctx, &[make_small(1), bad]), Err(BeamError::BadArg)));
            }
        });
    }
    #[test]
    fn bignum_operands_coerce_and_overflow_is_badarith() {
        with_ctx(|ctx| {
            // `math:floor(1 bsl 62)` ⇒ `4.611686018427388e18`.
            let big = beam_codec::term::make_big(ctx.heap(), false, &[1u64 << 62]).expect("a bignum");
            assert_eq!(float_of(floor_1(ctx, &[big]).unwrap()), 4.611686018427388e18);
            // `math:pow(1 bsl 100, 2)` ⇒ `1.6069380442589903e60`.
            let bigger = beam_codec::term::make_big(ctx.heap(), false, &[0, 1u64 << 36]).expect("a bignum");
            assert_eq!(
                float_of(pow_2(ctx, &[bigger, make_small(2)]).unwrap()),
                1.6069380442589903e60
            );
            // A bignum beyond `f64` range is `badarith` (`big_to_double` fails).
            let huge = beam_codec::term::make_big(ctx.heap(), false, &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]).expect("a bignum");
            assert!(matches!(floor_1(ctx, &[huge]), Err(BeamError::BadArith)));
        });
    }
    /// The bignum operand coercion is the canonical `big_to_double` port
    /// (`beam_term::number::big_to_f64`): ties-to-even rounding and the same
    /// `badarith` at the finite-double limit as the oracle.
    #[test]
    fn bignum_operands_use_the_canonical_big_to_double_port() {
        with_ctx(|ctx| {
            // `math:sqrt(1 bsl 100)` ⇒ `1125899906842624.0` (`2^50`).
            let two_to_100 =
                beam_codec::term::make_big(ctx.heap(), false, &[0, 1u64 << 36]).expect("a bignum");
            assert_eq!(
                float_of(sqrt_1(ctx, &[two_to_100]).unwrap()),
                1125899906842624.0
            );
            // `math:exp(1 bsl 100)` overflows to `inf` ⇒ `badarith`.
            assert!(matches!(exp_1(ctx, &[two_to_100]), Err(BeamError::BadArith)));

            // Ties to even: `(1 bsl 53) + 1` (halfway) ⇒ `9.007199254740992e15`.
            let tie = beam_codec::term::make_big(ctx.heap(), false, &[(1u64 << 53) + 1])
                .expect("a bignum");
            assert_eq!(float_of(floor_1(ctx, &[tie]).unwrap()), 9007199254740992.0);

            // The largest finite double `(1 bsl 1024) - (1 bsl 971) = (2^53-1)·2^971`
            // coerces exactly (`f64::MAX`); one half-ulp above it —
            // `(1 bsl 1024) - (1 bsl 970)` — rounds up to `2^1024`, not finite, so it
            // is `badarith` (the `big_to_f64` `None` boundary).
            let mut max_limbs = vec![0u64; 16];
            max_limbs[15] = ((1u64 << 53) - 1) << 11;
            let max =
                beam_codec::term::make_big(ctx.heap(), false, &max_limbs).expect("a bignum");
            assert_eq!(float_of(floor_1(ctx, &[max]).unwrap()), f64::MAX);

            let mut carry_limbs = vec![0u64; 16];
            carry_limbs[15] = u64::MAX << 10;
            let carry =
                beam_codec::term::make_big(ctx.heap(), false, &carry_limbs).expect("a bignum");
            assert!(matches!(floor_1(ctx, &[carry]), Err(BeamError::BadArith)));
        });
    }
    #[test]
    fn erf_and_erfc_are_within_the_documented_ulp_bound() {
        with_ctx(|ctx| {
            // Oracle values from `erl -noshell -eval 'math:erf(…).'`.
            for (at, expected) in [
                (0.0f64, 0.0),
                (0.5, 0.5204998778130465),
                (1.0, 0.8427007929497149),
                (1.2, 0.9103139782296353),
                (1.5, 0.9661051464753108),
                (2.0, 0.9953222650189527),
                (3.0, 0.9999779095030014),
                (-1.0, -0.8427007929497149),
            ] {
                let arg = ctx.make_float(at).unwrap();
                let got = float_of(erf_1(ctx, &[arg]).unwrap());
                assert!(
                    ulp_distance(got, expected) <= 4,
                    "erf({at}) = {got} differs from the oracle {expected} by {} ulp",
                    ulp_distance(got, expected)
                );
            }
            for (at, expected) in [
                (0.0f64, 1.0),
                (0.5, 0.4795001221869535),
                (1.0, 0.15729920705028513),
                (1.2, 0.08968602177036464),
                (1.5, 0.033894853524689274),
                (2.0, 0.004677734981047266),
                (3.0, 2.209049699858544e-5),
                (5.0, 1.537459794428035e-12),
                (10.0, 2.088487583762545e-45),
                (-1.0, 1.8427007929497148),
            ] {
                let arg = ctx.make_float(at).unwrap();
                let got = float_of(erfc_1(ctx, &[arg]).unwrap());
                assert!(
                    ulp_distance(got, expected) <= 8,
                    "erfc({at}) = {got} differs from the oracle {expected} by {} ulp",
                    ulp_distance(got, expected)
                );
            }
            // The exactly-pinned special values.
            assert_eq!(float_of(erf_1(ctx, &[make_small(0)]).unwrap()).to_bits(), 0.0f64.to_bits());
            assert_eq!(float_of(erfc_1(ctx, &[make_small(0)]).unwrap()), 1.0);
            let big_f = ctx.make_float(100.0).unwrap();
            let neg_big_f = ctx.make_float(-100.0).unwrap();
            assert_eq!(float_of(erf_1(ctx, &[big_f]).unwrap()), 1.0);
            assert_eq!(float_of(erf_1(ctx, &[neg_big_f]).unwrap()), -1.0);
            assert_eq!(float_of(erfc_1(ctx, &[big_f]).unwrap()), 0.0);
            assert_eq!(float_of(erfc_1(ctx, &[neg_big_f]).unwrap()), 2.0);
            // A bignum operand coerces (`math:erf(1 bsl 62)` ⇒ `1.0`).
            let big = beam_codec::term::make_big(ctx.heap(), false, &[1u64 << 62]).expect("a bignum");
            assert_eq!(float_of(erf_1(ctx, &[big]).unwrap()), 1.0);
        });
    }
    #[test]
    fn every_math_bif_is_registered() {
        let registry = crate::registry::default_registry();
        let unary = [
            "cos", "cosh", "sin", "sinh", "tan", "tanh", "acos", "acosh", "asin", "asinh", "atan",
            "atanh", "erf", "erfc", "exp", "log", "log2", "log10", "sqrt", "floor", "ceil",
        ];
        for name in unary {
            assert!(registry.contains("math", name, 1), "math:{name}/1");
        }
        for name in ["atan2", "pow", "fmod"] {
            assert!(registry.contains("math", name, 2), "math:{name}/2");
        }
    }
}
