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

//! The numeric arm of the term comparison — `erts_cmp_compound`'s `mixed_types`
//! block (`erts/emulator/beam/utils.c:2345-2461`) and its helpers:
//! `erts_float_comp` (`erts/emulator/beam/erl_utils.h:187`), `big_to_double`,
//! `double_to_big` and `big_comp` (`erts/emulator/beam/big.c`).
//!
//! [`super::cmp_numbers`] is reached only for a pair of **different** numeric
//! representations; `(small, small)`, `(float, float)` and the identical-word fast
//! path are handled by [`super::cmp_impl`] first.
//!
//! ## Exact vs arithmetic mode
//!
//! * **Exact** mode (`cmp`/`eq`, i.e. `=:=`) never converts: a mixed integer/float
//!   pair compares by its `_DEF` class rank, exactly as the C's
//!   `exact_fall_through:` label does (`j = b_tag - a_tag`). `1 =/= 1.0`.
//! * **Arithmetic** mode (`cmp_arith`, i.e. `==`) compares *numerically*, and the C
//!   is careful to do so **exactly** — the naive "convert both to `f64`" is lossy
//!   for anything wider than the 53-bit mantissa, and ERTS instead reaches for the
//!   exact integer ↔ float algorithms reproduced here. This module is the port of
//!   those algorithms; [`crate::number::number_as_f64`] is *not* used on this path
//!   (see its doc note).
//!
//! The three implementations of the C's `_NUMBER_CODE` cases:
//! `SMALL_FLOAT`/`FLOAT_SMALL` (`utils.c:2366-2391`/`2431-2456`) and
//! `BIG_FLOAT`/`FLOAT_BIG` (`utils.c:2392-2430`, the C swaps the operands and
//! negates the result for `FLOAT_BIG`). `SMALL_BIG`/`BIG_SMALL`/`BIG_BIG` are the
//! direct sign/limb comparisons (`utils.c:2360-2365`).
//!
//! ## Special values
//!
//! ERTS's own float parser, its `binary_to_term/1` and its bit-syntax float
//! matching all *refuse* a non-finite double, so `±inf`/`NaN` cannot be observed
//! through `==` in a real system — the C's behaviour for them is unspecified (its
//! `double_to_big` even spins forever on an infinite input). The crate still
//! answers deterministically, per its "never a wrong result, never a panic"
//! contract:
//!
//! * `±inf` is decided by the C's own out-of-range branches (a finite integer is
//!   below `+inf` and above `-inf`), which is also mathematically correct.
//! * `NaN` sorts **after** every number in arithmetic mode, mirroring the C macro
//!   `erts_float_comp` (which reports "greater" for an unordered pair) and the
//!   existing float/float arithmetic comparison in [`super::cmp_floats`], so
//!   `nan_float =/= x` and `nan_float > x` for every other number `x`. (In exact
//!   mode `NaN` keeps its total-order slot from `erts_float_exact_sortable`.)

use core::cmp::Ordering;

use crate::bitstring::float_value;
use crate::eterm::Eterm;
use crate::immediate::signed_value;
use crate::number::big_to_f64;
use crate::ops::TermClass;
use crate::thing::big_parts;

use super::class_rank;

/// Bits per bignum limb: an `ErtsDigit` is one machine word (`big.h:29`,
/// `typedef Uint ErtsDigit`), i.e. `D_EXP == 64` on `ARCH_64`.
const LIMB_BITS: usize = 64;

/// `MAX_LOSSLESS_FLOAT` (`utils.c:2352`) — `(2^53) - 2`, the largest magnitude a
/// `double` and the small-integer range share without loss.
const MAX_LOSSLESS_FLOAT: f64 = ((1u64 << 53) - 2) as f64;

/// `MIN_LOSSLESS_FLOAT` (`utils.c:2353`).
const MIN_LOSSLESS_FLOAT: f64 = -MAX_LOSSLESS_FLOAT;

/// `MAX_SMALL` on this platform (`erl_term.h`): `2^59 - 1`.
const MAX_SMALL_I: i64 = (1i64 << 59) - 1;

/// `MIN_SMALL` (`erl_term.h`): `-2^59`.
const MIN_SMALL_I: i64 = -(1i64 << 59);

/// `(double)(MAX_SMALL + 1)` (`utils.c:2375`) — exactly `2^59`.
const MAX_SMALL_PLUS_1: f64 = (MAX_SMALL_I + 1) as f64;

/// `(double)(MIN_SMALL - 1)` (`utils.c:2378`). `-2^59 - 1` is not representable, so
/// the C's cast rounds it to the nearest double, `-2^59`; the const cast here
/// rounds identically.
const MIN_SMALL_MINUS_1: f64 = (MIN_SMALL_I - 1) as f64;

/// `BIG_ARITY_FLOAT_MAX` (`utils.c:2354`) — the limb count of the largest finite
/// double seen as a bignum, `1024 / D_EXP == 16`.
const BIG_ARITY_FLOAT_MAX: usize = 1024 / LIMB_BITS;

/// Compare two numeric terms whose [`TermClass`]es differ.
///
/// `ca`/`cb` are the classes of `a`/`b`; the caller only routes here for a pair of
/// *different* representations, so `(Small, Small)`, `(Float, Float)` and
/// `(Atom, Atom)` never arrive. `exact` selects `=:=` (`true`) or `==` (`false`).
///
/// # Safety
///
/// The caller guarantees `a`/`b` are live terms of classes `ca`/`cb`.
pub(super) fn cmp_numbers(
    a: Eterm,
    b: Eterm,
    ca: TermClass,
    cb: TermClass,
    exact: bool,
) -> Ordering {
    use TermClass::{Big, Float, Small};
    match (ca, cb) {
        // `SMALL_BIG`/`BIG_SMALL`: a bignum is always outside the 60-bit small
        // range, so the small's side is decided solely by the bignum's sign.
        (Small, Big) => big_sign(b),
        (Big, Small) => big_sign(a).reverse(),
        (Big, Big) => cmp_bigs(a, b),
        (Small, Float) => cmp_small_float(a, b, exact),
        (Float, Small) => cmp_float_small(a, b, exact),
        (Big, Float) => cmp_big_float(a, b, exact),
        (Float, Big) => cmp_float_big(a, b, exact),
        // Unreachable (the caller handles same-class pairs, and the three numeric
        // classes are exhaustive), but kept total so a future class cannot panic.
        _ => Ordering::Equal,
    }
}

/// `j = big_sign(bw) ? 1 : -1` — a small vs a bignum: `Less` when the bignum is
/// positive (the small is below it), `Greater` when it is negative.
///
/// # Safety
///
/// `x` must be a live bignum term.
fn big_sign(x: Eterm) -> Ordering {
    // SAFETY: the caller guarantees a live bignum.
    let (negative, _) = unsafe { big_parts(x) };
    if negative {
        Ordering::Greater
    } else {
        Ordering::Less
    }
}

/// `erts_float_comp(x, y)` (`erl_utils.h:187`) — the C macro, verbatim:
/// `x < y ⇒ Less`, `x == y ⇒ Equal`, otherwise `Greater`. An unordered (NaN) pair
/// therefore reports `Greater`, exactly as the macro does.
fn erts_float_comp(x: f64, y: f64) -> Ordering {
    if x < y {
        Ordering::Less
    } else if x == y {
        Ordering::Equal
    } else {
        Ordering::Greater
    }
}

/// `pow(2.0, e)` as the C uses it: `2^e` for a `double`, saturating to `+inf` once
/// the exponent exceeds the largest finite binary exponent (the C's `pow` overflows
/// to `+inf` at the same point, `e >= 1024`).
fn pow2(e: usize) -> f64 {
    if e >= 1024 {
        f64::INFINITY
    } else {
        2.0f64.powi(e as i32)
    }
}

/// `SMALL_FLOAT` (`utils.c:2366-2391`): a small integer vs a float.
///
/// The comparison is exact. Inside the lossless window the small converts to `f64`
/// without error; outside it the C falls back to an **integer** subtraction
/// (`signed_val(aw) - (Sint) f`), which is exact because at those magnitudes a
/// `double` is a whole number.
fn cmp_small_float(a: Eterm, b: Eterm, exact: bool) -> Ordering {
    if exact {
        // The C's `exact_fall_through: j = b_tag - a_tag`, i.e. SMALL < FLOAT.
        return class_rank(TermClass::Small).cmp(&class_rank(TermClass::Float));
    }
    // SAFETY: the caller established `b`'s class as `Float`.
    let f = unsafe { float_value(b) };
    let sa = signed_value(a);
    if f.is_nan() {
        // Documented rule (see the module docs): a NaN sorts after every number.
        return Ordering::Less;
    }
    if f < MAX_LOSSLESS_FLOAT && f > MIN_LOSSLESS_FLOAT {
        // The float is within the no-loss limit: `signed_val(aw)` → `f64` is exact.
        return erts_float_comp(sa as f64, f);
    }
    if f > MAX_SMALL_PLUS_1 {
        // The float is a positive bignum, i.e. bigger.
        return Ordering::Less;
    }
    if f < MIN_SMALL_MINUS_1 {
        // The float is a negative bignum, i.e. smaller.
        return Ordering::Greater;
    }
    // The float is a `Sint` but one that rounds the small away (`|f| >= 2^53 - 2`,
    // where a double is a whole number), so the exact integer subtraction decides.
    (sa - (f as i64)).cmp(&0)
}

/// `FLOAT_SMALL` (`utils.c:2431-2456`) — the exact mirror of [`cmp_small_float`].
fn cmp_float_small(a: Eterm, b: Eterm, exact: bool) -> Ordering {
    if exact {
        // `exact_fall_through: j = b_tag - a_tag`, i.e. FLOAT > SMALL.
        return class_rank(TermClass::Float).cmp(&class_rank(TermClass::Small));
    }
    // SAFETY: the caller established `a`'s class as `Float`.
    let f = unsafe { float_value(a) };
    let sb = signed_value(b);
    if f.is_nan() {
        return Ordering::Greater;
    }
    if f < MAX_LOSSLESS_FLOAT && f > MIN_LOSSLESS_FLOAT {
        return erts_float_comp(f, sb as f64);
    }
    if f > MAX_SMALL_PLUS_1 {
        return Ordering::Greater;
    }
    if f < MIN_SMALL_MINUS_1 {
        return Ordering::Less;
    }
    ((f as i64) - sb).cmp(&0)
}

/// `BIG_FLOAT` (`utils.c:2401-2430`): a bignum vs a float.
///
/// The C decides by magnitude *before* converting anything: a float inside the
/// small range is automatically below/above every bignum (by the bignum's sign), a
/// limb count above the largest finite double's settles the comparison outright,
/// and only a float in the same band as the bignum needs the exact conversion —
/// the bignum via `big_to_double` in the lossless window, or the float via
/// `double_to_big` and a bignum-vs-bignum `big_comp` outside it.
fn cmp_big_float(a: Eterm, b: Eterm, exact: bool) -> Ordering {
    if exact {
        // `exact_fall_through: j = b_tag - a_tag`, i.e. BIG < FLOAT.
        return class_rank(TermClass::Big).cmp(&class_rank(TermClass::Float));
    }
    // SAFETY: the caller established `b`'s class as `Float`.
    let f = unsafe { float_value(b) };
    // SAFETY: the caller established `a`'s class as `Big`; the limb slice borrows
    // the term, which stays live for the whole call.
    let (neg, limbs_a) = unsafe { big_parts(a) };
    if f.is_nan() {
        return Ordering::Less;
    }
    if f.is_infinite() {
        // A finite bignum is strictly between `-inf` and `+inf`. (The C reaches the
        // same verdict for a limb count below 16, and would otherwise feed the
        // infinite value to a `double_to_big` that never terminates — a hang this
        // crate cannot reproduce for input its own parser refuses.)
        return if f > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    if f < MAX_SMALL_PLUS_1 && f > MIN_SMALL_MINUS_1 {
        // The float is a `Sint`, so the bignum is outside its range: its sign decides.
        return if neg { Ordering::Less } else { Ordering::Greater };
    }
    let arity = limbs_a.len();
    if arity > BIG_ARITY_FLOAT_MAX || pow2(arity.saturating_sub(1) * LIMB_BITS) > f.abs() {
        // The bignum's size shows it is bigger than the absolute float.
        return if neg { Ordering::Less } else { Ordering::Greater };
    }
    if arity < BIG_ARITY_FLOAT_MAX && (pow2(arity * LIMB_BITS) - 1.0) < f.abs() {
        // The bignum's size shows it is smaller than the absolute float.
        return if f < 0.0 {
            Ordering::Greater
        } else {
            Ordering::Less
        };
    }
    if f < MAX_LOSSLESS_FLOAT && f > MIN_LOSSLESS_FLOAT {
        // Kept for fidelity with the C, though unreachable with 64-bit `Eterm`s: a
        // `Sint` float (which covers the whole lossless window) is caught above.
        // `big_to_double` fails only for an unrepresentable magnitude — then the
        // bignum's sign decides, as in the C.
        return match big_to_f64(a) {
            None => {
                if neg {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            Some(fa) => erts_float_comp(fa, f),
        };
    }
    // The float is outside the lossless window: convert it to an exact bignum and
    // compare the two bignums (`double_to_big` + `big_comp`).
    let mut buf = [0u64; BIG_ARITY_FLOAT_MAX];
    let (neg_f, limbs_f) = double_to_big(f, &mut buf);
    cmp_big_limbs(neg, limbs_a, neg_f, &buf[..limbs_f])
}

/// `FLOAT_BIG` (`utils.c:2392-2400`): the C swaps `aw`/`bw`, runs the `BIG_FLOAT`
/// logic and negates the result (`j = -j`).
fn cmp_float_big(a: Eterm, b: Eterm, exact: bool) -> Ordering {
    cmp_big_float(b, a, exact).reverse()
}

/// `double_to_big(x)` (`big.c:2536-2583`) — the **exact** conversion of a finite
/// `f64` into `(negative, limb count)` written into `out` as least-significant-first
/// `ErtsDigit` (`u64`) limbs, normalised so the most significant limb is non-zero.
///
/// The C divides `x` by `2^64` until it is below `1.0` (counting the limb width),
/// then multiplies back out, truncating one `ErtsDigit` at a time. A finite double is
/// below `2^1024`, so the width is at most `1024 / 64 == 16` limbs — exactly
/// [`BIG_ARITY_FLOAT_MAX`], the C's static `BIG_NEED_SIZE(BIG_ARITY_FLOAT_MAX)` buffer.
///
/// Only reached from the `BIG_FLOAT` magnitude-band branch, where `x` is finite.
fn double_to_big(x: f64, out: &mut [u64; BIG_ARITY_FLOAT_MAX]) -> (bool, usize) {
    debug_assert!(x.is_finite(), "double_to_big needs a finite value");
    // The C tests `x >= 0`, which classifies `-0.0` as positive; `x < 0.0` is that test.
    let negative = x < 0.0;
    let mut x = if negative { -x } else { x };
    // `(double)(D_MASK) + 1` — `D_MASK` is the 64-bit digit mask, so this is `2^64`
    // exactly (a power of two, so every division/multiplication below is exact).
    const DBASE: f64 = 18_446_744_073_709_551_616.0;
    let mut ds = 0usize;
    while x >= 1.0 {
        x /= DBASE; // "shift" right
        ds += 1;
    }
    debug_assert!(ds <= BIG_ARITY_FLOAT_MAX, "a finite f64 needs at most 16 limbs");
    for i in (0..ds).rev() {
        x *= DBASE; // "shift" left
        let d = x as u64; // trunc
        out[i] = d;
        x -= d as f64; // remove the integer part
    }
    (negative, ds)
}

/// Compare two bignums limb-by-limb (`big_comp`, `big.c:3375-3389`).
///
/// # Safety
///
/// `a` and `b` must be live bignum terms.
fn cmp_bigs(a: Eterm, b: Eterm) -> Ordering {
    // SAFETY: the caller guarantees both operands are live bignums; the limb slices
    // borrow terms that stay live for the whole call.
    let (na, la) = unsafe { big_parts(a) };
    // SAFETY: as above.
    let (nb, lb) = unsafe { big_parts(b) };
    cmp_big_limbs(na, la, nb, lb)
}

/// `big_comp`'s core over bare limb slices: the sign first (`BIG_SIGN(xp) ==
/// BIG_SIGN(yp)` — else the negative one is smaller), then the unsigned magnitude.
fn cmp_big_limbs(na: bool, la: &[u64], nb: bool, lb: &[u64]) -> Ordering {
    if na != nb {
        return if na { Ordering::Less } else { Ordering::Greater };
    }
    let magnitude = cmp_magnitude(la, lb);
    if na {
        magnitude.reverse()
    } else {
        magnitude
    }
}

/// The unsigned magnitude comparison of two normalised limb slices (`I_comp`,
/// `big.c:455-475`): the longer slice is the larger value — both are normalised, so
/// neither has a leading zero limb — and equal lengths compare the limbs from the
/// most significant down.
fn cmp_magnitude(la: &[u64], lb: &[u64]) -> Ordering {
    if la.len() != lb.len() {
        return la.len().cmp(&lb.len());
    }
    for i in (0..la.len()).rev() {
        if la[i] != lb[i] {
            return la[i].cmp(&lb[i]);
        }
    }
    Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::{cmp, cmp_arith, eq};
    use crate::heap::Heap;
    use crate::immediate::make_small;
    use crate::thing::{big_size_words, write_big, write_float, FLOAT_SIZE_WORDS};
    use crate::AtomTable;

    /// Build a bignum term owned by `heap` (which must outlive the result).
    fn big(heap: &mut Heap, negative: bool, limbs: &[u64]) -> Eterm {
        // SAFETY: the arena owns the `big_size_words(limbs.len())`-word block and
        // stays alive for as long as the returned term is used.
        let hp = unsafe { heap.alloc_words(big_size_words(limbs.len())).unwrap() };
        // SAFETY: `hp` addresses exactly `big_size_words(limbs.len())` writable
        // words, which `write_big` fully initialises.
        unsafe { write_big(hp, negative, limbs) }
    }

    /// Build a run of `n` limbs holding the single value `2^(64*(n-1) + top_bit)`.
    fn pow2_limbs(n: usize, top_bit: u32) -> Vec<u64> {
        let mut v = vec![0u64; n];
        v[n - 1] = 1u64 << top_bit;
        v
    }

    struct Arena {
        heap: Heap,
        atoms: AtomTable,
    }

    impl Arena {
        fn new() -> Arena {
            Arena {
                heap: Heap::new(4096),
                atoms: AtomTable::new(),
            }
        }

        fn big(&mut self, negative: bool, limbs: &[u64]) -> Eterm {
            big(&mut self.heap, negative, limbs)
        }

        fn float(&mut self, value: f64) -> Eterm {
            // SAFETY: the arena keeps the `FLOAT_SIZE_WORDS`-word block alive.
            let hp = unsafe { self.heap.alloc_words(FLOAT_SIZE_WORDS).unwrap() };
            // SAFETY: `hp` addresses exactly `FLOAT_SIZE_WORDS` writable words.
            unsafe { write_float(hp, value) }
        }

        fn arith(&self, a: Eterm, b: Eterm) -> Ordering {
            cmp_arith(a, b, &self.atoms)
        }

        /// `a == f` — builds the float first so `Arena` is not borrowed twice.
        fn arith_f(&mut self, a: Eterm, f: f64) -> Ordering {
            let b = self.float(f);
            cmp_arith(a, b, &self.atoms)
        }

        /// `f == b` — the float-first mirror of [`Arena::arith_f`].
        fn arith_fr(&mut self, f: f64, b: Eterm) -> Ordering {
            let a = self.float(f);
            cmp_arith(a, b, &self.atoms)
        }

        fn exact(&self, a: Eterm, b: Eterm) -> Ordering {
            cmp(a, b, &self.atoms)
        }
    }

    /// `2^53 = 9007199254740992` — the first magnitude where a `double` cannot
    /// represent every integer. The objective's repro:
    /// `9007199254740993 == 9007199254740992.0` is `false` and
    /// `9007199254740993 > 9007199254740992.0` — the lossy `f64` conversion used to
    /// round the integer down to `2^53` and wrongly report `Equal`.
    #[test]
    fn two_pow_53_asymmetry_is_exact() {
        let mut ar = Arena::new();
        let odd = make_small(9_007_199_254_740_993); // 2^53 + 1
        let f = ar.float(9_007_199_254_740_992.0); // 2^53 exactly

        assert_eq!(ar.arith(odd, f), Ordering::Greater, "2^53+1 > 2^53");
        assert_eq!(ar.arith(f, odd), Ordering::Less, "2^53 < 2^53+1");
        assert!(!eq(odd, f, &ar.atoms), "2^53+1 == 2^53.0 must be false");

        // The signed direction of the same asymmetry.
        let neg_odd = make_small(-9_007_199_254_740_993);
        let nf = ar.float(-9_007_199_254_740_992.0);
        assert_eq!(ar.arith(neg_odd, nf), Ordering::Less, "-2^53-1 < -2^53");
        assert_eq!(ar.arith(nf, neg_odd), Ordering::Greater, "-2^53 > -2^53-1");
    }

    /// Around the `MAX_LOSSLESS_FLOAT = 2^53 - 2` boundary: at and just below the
    /// window the small converts losslessly; just above, the integral subtraction
    /// branch decides.
    #[test]
    fn lossless_boundary_small_vs_float() {
        let mut ar = Arena::new();
        let base = ar.float(9_007_199_254_740_990.0); // 2^53 - 2

        assert_eq!(
            ar.arith(make_small(9_007_199_254_740_990), base),
            Ordering::Equal,
            "2^53-2 == (2^53-2).0",
        );
        assert_eq!(
            ar.arith(make_small(9_007_199_254_740_989), base),
            Ordering::Less,
            "2^53-3 < (2^53-2).0",
        );
        assert_eq!(
            ar.arith(make_small(9_007_199_254_740_991), base),
            Ordering::Greater,
            "2^53-1 > (2^53-2).0",
        );
        // Just inside the window, an odd small still compares exactly.
        assert_eq!(
            ar.arith(make_small(-9_007_199_254_740_989), base),
            Ordering::Less,
            "-2^53+3 < (2^53-2).0",
        );
        // Exact mode ignores the values entirely: SMALL < FLOAT.
        assert_eq!(
            ar.exact(make_small(9_007_199_254_740_990), base),
            Ordering::Less,
            "=:= orders a small below a float",
        );
    }

    /// A small vs a fractional float, and the two zeros.
    #[test]
    fn small_vs_fractional_float() {
        let mut ar = Arena::new();
        let one = ar.float(1.0);
        let one_and_a_half = ar.float(1.5);
        let half = ar.float(0.5);
        let zero = ar.float(0.0);
        let neg_zero = ar.float(-0.0);

        assert_eq!(ar.arith(make_small(1), one_and_a_half), Ordering::Less);
        assert_eq!(ar.arith(one_and_a_half, make_small(1)), Ordering::Greater);
        assert_eq!(ar.arith(make_small(1), one), Ordering::Equal);
        assert_eq!(ar.arith(make_small(-1), half), Ordering::Less);
        assert_eq!(ar.arith(make_small(0), half), Ordering::Less);
        assert_eq!(ar.arith(half, make_small(0)), Ordering::Greater);

        // `0 == -0.0` is true arithmetically; `=:=` is not (and orders SMALL first).
        assert_eq!(ar.arith(make_small(0), neg_zero), Ordering::Equal);
        assert_eq!(ar.arith(neg_zero, zero), Ordering::Equal);
        assert_ne!(ar.exact(neg_zero, zero), Ordering::Equal, "-0.0 =/= +0.0");
        assert_eq!(ar.exact(neg_zero, zero), Ordering::Less, "-0.0 < +0.0");
        assert_ne!(ar.exact(make_small(0), neg_zero), Ordering::Equal);
    }

    /// Bignum vs float, both signs, across the C's magnitude bands.
    #[test]
    fn bignum_vs_float_both_signs() {
        let mut ar = Arena::new();
        let pos_small_float = ar.float(1.0);
        let neg_small_float = ar.float(-1.0);

        // 2^100 (two limbs) vs a float inside the small range: the bignum's sign
        // decides (band 1).
        let two_pow_100 = ar.big(false, &pow2_limbs(2, 36));
        let neg_two_pow_100 = ar.big(true, &pow2_limbs(2, 36));
        assert_eq!(ar.arith(two_pow_100, pos_small_float), Ordering::Greater);
        assert_eq!(ar.arith(two_pow_100, neg_small_float), Ordering::Greater);
        assert_eq!(ar.arith(neg_two_pow_100, pos_small_float), Ordering::Less);
        assert_eq!(ar.arith(neg_two_pow_100, neg_small_float), Ordering::Less);
        assert_eq!(ar.arith(pos_small_float, two_pow_100), Ordering::Less);
        assert_eq!(ar.arith(neg_small_float, two_pow_100), Ordering::Less);

        // A bignum whose limb count exceeds the largest finite double's (16) is
        // definitely bigger in magnitude: the sign decides (band 2, `arity > 16`).
        let huge = ar.big(false, &pow2_limbs(17, 0));
        let neg_huge = ar.big(true, &pow2_limbs(17, 0));
        let one_e_300 = ar.float(1e300);
        assert_eq!(ar.arith(huge, one_e_300), Ordering::Greater);
        assert_eq!(ar.arith(neg_huge, one_e_300), Ordering::Less);
        assert_eq!(ar.arith(one_e_300, huge), Ordering::Less);
        assert_eq!(ar.arith(one_e_300, neg_huge), Ordering::Greater);

        // One limb, float far bigger in magnitude: the float's sign decides
        // (band 3, `arity < 16 && 2^(arity*64) - 1 < |f|`).
        let two_pow_60 = ar.big(false, &pow2_limbs(1, 60));
        assert_eq!(ar.arith(two_pow_60, one_e_300), Ordering::Less);
        assert_eq!(ar.arith(one_e_300, two_pow_60), Ordering::Greater);
        let neg_one_e_300 = ar.float(-1e300);
        assert_eq!(ar.arith(two_pow_60, neg_one_e_300), Ordering::Greater);
        assert_eq!(ar.arith(neg_one_e_300, two_pow_60), Ordering::Less);
    }

    /// The final band: the float is in the same magnitude range as the bignum and
    /// outside the lossless window, so it is converted to an exact bignum and the
    /// two bignums are compared limb-by-limb.
    #[test]
    fn bignum_vs_float_exact_bignum_path() {
        let mut ar = Arena::new();
        // 2^910 needs 15 limbs; `2^896 <= 2^910 < 2^960`, so bands 1-3 all pass.
        let big_2_910 = ar.big(false, &pow2_limbs(15, 910 - 14 * 64));
        assert_eq!(ar.arith_f(big_2_910, 2.0f64.powi(910)), Ordering::Equal);
        assert_eq!(ar.arith_f(big_2_910, 2.0f64.powi(909)), Ordering::Greater);
        assert_eq!(ar.arith_f(big_2_910, 2.0f64.powi(911)), Ordering::Less);
        // ... and the mirror image (the float is bigger than the bignum).
        assert_eq!(ar.arith_fr(2.0f64.powi(910), big_2_910), Ordering::Equal);
        assert_eq!(ar.arith_fr(2.0f64.powi(911), big_2_910), Ordering::Greater);
        assert_eq!(ar.arith_fr(2.0f64.powi(909), big_2_910), Ordering::Less);
        // A 16-limb bignum (as wide as any finite double) vs `1e290`: also the
        // exact path — `2^1023 > 1e290 > 2^960`.
        let big_16 = ar.big(false, &pow2_limbs(16, 63)); // 2^1023
        assert_eq!(ar.arith_f(big_16, 1e290), Ordering::Greater);
        assert_eq!(ar.arith_f(big_16, 2.0f64.powi(1022)), Ordering::Greater);
        assert_eq!(ar.arith_fr(-1e290, big_16), Ordering::Less);
    }

    /// `1e20`, `1e30` and `2^100` cross-magnitude vectors (both operand orders).
    #[test]
    fn cross_magnitude_vectors() {
        let mut ar = Arena::new();
        let two_pow_100 = ar.big(false, &pow2_limbs(2, 36)); // 2^100 = 1.2676506e30
        assert_eq!(ar.arith_f(two_pow_100, 1e20), Ordering::Greater);
        assert_eq!(ar.arith_fr(1e20, two_pow_100), Ordering::Less);
        // `2^100 = 1.2676506e30 > 1e30`, decided by the exact-bignum path (the
        // float needs two limbs, like the bignum).
        assert_eq!(ar.arith_f(two_pow_100, 1e30), Ordering::Greater);
        assert_eq!(ar.arith_fr(1e30, two_pow_100), Ordering::Less);
        assert_eq!(ar.arith_f(two_pow_100, 2.0f64.powi(100)), Ordering::Equal);
        // A 1-limb bignum against much larger floats.
        let two_pow_63 = ar.big(false, &pow2_limbs(1, 63));
        assert_eq!(ar.arith_f(two_pow_63, 1e20), Ordering::Less);
        assert_eq!(ar.arith_f(two_pow_63, 1e19), Ordering::Less);
        assert_eq!(ar.arith_f(two_pow_63, 9.0e18), Ordering::Greater);
    }

    /// NaN sorts after every number in arithmetic mode; `±inf` is decided by the
    /// C's out-of-range branches. (None of these floats can be produced by ERTS's
    /// own surface — see the module docs — so the rule is this crate's documented
    /// deterministic choice.)
    #[test]
    fn special_float_values() {
        let mut ar = Arena::new();
        let nan = ar.float(f64::NAN);
        let inf = ar.float(f64::INFINITY);
        let neg_inf = ar.float(f64::NEG_INFINITY);
        let one = make_small(1);
        let big_2_100 = ar.big(false, &pow2_limbs(2, 36));
        let neg_big_2_100 = ar.big(true, &pow2_limbs(2, 36));

        // NaN is greatest.
        assert_eq!(ar.arith(one, nan), Ordering::Less);
        assert_eq!(ar.arith(nan, one), Ordering::Greater);
        assert_eq!(ar.arith(big_2_100, nan), Ordering::Less);
        assert_eq!(ar.arith(nan, big_2_100), Ordering::Greater);
        assert_eq!(ar.arith_f(nan, 1.0), Ordering::Greater);

        // ±inf.
        assert_eq!(ar.arith(one, inf), Ordering::Less);
        assert_eq!(ar.arith(inf, one), Ordering::Greater);
        assert_eq!(ar.arith(one, neg_inf), Ordering::Greater);
        assert_eq!(ar.arith(neg_inf, one), Ordering::Less);
        assert_eq!(ar.arith(big_2_100, inf), Ordering::Less);
        assert_eq!(ar.arith(inf, big_2_100), Ordering::Greater);
        assert_eq!(ar.arith(neg_big_2_100, inf), Ordering::Less);
        assert_eq!(ar.arith(big_2_100, neg_inf), Ordering::Greater);
        assert_eq!(ar.arith(neg_inf, neg_big_2_100), Ordering::Less);
    }

    /// A property sweep over the boundaries: antisymmetry (`cmp_arith(a,b) ==
    /// cmp_arith(b,a).reverse()`) and exact-mode consistency (`==` may merge
    /// `1`/`1.0`, but `=:=` never does).
    #[test]
    fn antisymmetry_boundary_sweep() {
        let mut ar = Arena::new();
        // Small integers around 0, the lossless boundary and `2^53`.
        let smalls: Vec<Eterm> = [
            -(1i64 << 59),
            -(1i64 << 59) + 1,
            -9_007_199_254_740_993,
            -9_007_199_254_740_991,
            -1,
            0,
            1,
            9_007_199_254_740_989,
            9_007_199_254_740_990,
            9_007_199_254_740_991,
            9_007_199_254_740_993,
            (1i64 << 59) - 2,
            (1i64 << 59) - 1,
        ]
        .iter()
        .map(|v| make_small(*v))
        .collect();
        let floats: Vec<Eterm> = [
            -1e300,
            -9_007_199_254_740_992.0,
            -9_007_199_254_740_990.0,
            -1.5,
            -0.0,
            0.0,
            0.5,
            1.0,
            9_007_199_254_740_990.0,
            9_007_199_254_740_992.0,
            1e30,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ]
        .iter()
        .map(|v| ar.float(*v))
        .collect();

        for a in &smalls {
            for b in &floats {
                assert_eq!(
                    ar.arith(*a, *b),
                    ar.arith(*b, *a).reverse(),
                    "cmp_arith must be antisymmetric",
                );
                // `=:=` is never satisfied across a representation change.
                assert_ne!(
                    ar.exact(*a, *b),
                    Ordering::Equal,
                    "a small is never =:= to a float",
                );
            }
        }
    }
}
