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

//! Numeric term → `f64` conversion — the faithful `big_to_double` port.
//!
//! Ground truth: `erts/emulator/beam/big.c:2412-2529` (`big_to_double`) and its
//! caller `erts/emulator/beam/emu/float_instrs.tab:38-52` (`i_fconv`), which
//! converts a bignum source operand to the inexact double via `big_to_double` and
//! raises `badarith` when the conversion fails (`< 0`).
//!
//! The whole point of this module is *faithful* rounding. Accumulating a bignum
//! digit by digit (`d = d * base + digit`, the previous `compare.rs` helper)
//! rounds once per digit and compounds the error, so it can land on the wrong
//! side of the true value; IEEE 754 requires the *nearest* representable double,
//! ties to even. [`big_to_f64`] reproduces the C algorithm exactly:
//!
//! 1. Compute the exact bit length (`(limbs-1) * 64 + msd_bits`, where
//!    `msd_bits = erts_fit_in_bits_uint(msd)`).
//! 2. `bitlen > 1024` is at least `2^1024` — above the largest finite double — so
//!    it is rejected up front.
//! 3. Otherwise take the top **54** bits (53 mantissa + one half bit), then round
//!    to nearest with ties to even (half bit set and either the mantissa is odd or
//!    some lesser bit is set ⇒ round up; a carry out of the mantissa increments
//!    the exponent), and scale by `2^exp` (`ldexp`).
//! 4. A non-finite result (a mantissa that carries up to `2^1024`) is also a
//!    failure.
//!
//! [`big_to_f64`] returns `None` for anything that is not a representable bignum
//! (not a bignum at all, or a magnitude too large for a finite double); callers
//! such as `beam_engine`'s `Fconv` map `None` onto `badarith`. It never returns a
//! silently wrong value. [`number_as_f64`] is the best-effort total variant; it is
//! **not** used for term comparison — `compare`'s arithmetic-mode mixed
//! integer/float comparison uses ERTS's exact algorithm
//! (`compare::numbers`), because converting both sides to `f64` is lossy beyond
//! the 53-bit mantissa.//!
//! Every `unsafe` block here carries a `// SAFETY:` rationale; the public
//! functions are safe, exactly like [`crate::compare::cmp`] / [`crate::size::size_object`],
//! because the crate treats an [`Eterm`] as a valid term by construction.

use crate::bitstring::float_value;
use crate::boxed::is_big;
use crate::eterm::Eterm;
use crate::immediate::signed_value;
use crate::ops::{class, TermClass};
use crate::thing::big_parts;

/// Bits per `ErtsDigit` on this platform (`D_EXP`, `big.h`).
///
/// A bignum's limbs are one machine word each (`typedef Uint ErtsDigit`), i.e.
/// `u64` on `ARCH_64`. The algorithm below does not assume the 32-bit `D_EXP`
/// short-cut the C takes; it stays correct for any limb width.
const D_EXP: usize = 64;

/// Convert a bignum term to the nearest `f64` (ties to even), or `None`.
///
/// This is the faithful port of `big_to_double` (`big.c:2412-2529`). It returns:
/// * `Some(d)` — `x` is a bignum whose value is representable as a **finite**
///   double; `d` is the nearest such double (ties to even);
/// * `None` — `x` is not a bignum (including a word that is not a term at all — a
///   bare header word such as the ABI's `THE_NON_VALUE`; see
///   [`crate::tag::is_term_word`]), or its magnitude is too large for a finite
///   double (`bitlen > 1024`, or a result that rounds up to `2^1024`).
///
/// A `None` result is exactly the C's `int < 0` return, which `i_fconv`
/// (`emu/float_instrs.tab:44`) turns into a `badarith` exception. The result is
/// never a silently wrong value.
pub fn big_to_f64(x: Eterm) -> Option<f64> {
    // SAFETY: `is_big` now tests "is a dereferenceable boxed term" first, so `x` is
    // either a live boxed term (whose header word is valid) or reported `false`
    // without a dereference — including for a word that is not a term at all.
    if !unsafe { is_big(x) } {
        return None;
    }
    // SAFETY: `x` is a live bignum (just checked), so `big_parts` reads its header
    // arity (the digit count) and borrows its one-word limbs (least significant
    // first). The borrow does not outlive `x`.
    let (negative, limbs) = unsafe { big_parts(x) };

    let n = limbs.len();
    if n == 0 {
        // An arity-0 bignum is illegal (`BIG_NEED_SIZE(n) = n + 1`); treat a
        // malformed term as "not a representable bignum" rather than reading out of
        // bounds. (`big_to_double` asserts `xl > 0`.)
        return None;
    }
    let msd = limbs[n - 1];
    // A normalised bignum has a non-zero most-significant limb; C asserts it.
    debug_assert!(msd != 0, "non-normalised bignum limb");

    // `erts_fit_in_bits_uint(msd)` (`utils.c:397` → `fit_in_bits`): the number of
    // bits needed to represent `msd`, i.e. `64 - leading_zeros`, with `0` mapping to
    // `1` (the C's `bits++` after an empty loop) — reproduced by `.max(1)`.
    let msd_bits = (D_EXP - msd.leading_zeros() as usize).max(1);
    let bitlen = (n - 1) * D_EXP + msd_bits;

    // On 64-bit limbs a bignum is always wider than the 53-bit mantissa, but keep
    // the exact short-cut ("the value fits the mantissa, so accumulating digit by
    // digit cannot round"). With `D_EXP == 64` this requires `n == 1`.
    if bitlen <= 53 {
        debug_assert_eq!(n, 1, "a multi-limb value wider than the mantissa");
        let d = limbs[0] as f64;
        return Some(if negative { -d } else { d });
    }

    // More than 1024 bits is at least 2^1024, above the largest finite double
    // (2^1024 - 2^971). Reject it here rather than in the loop below, which may
    // visit every limb to determine rounding.
    if bitlen > 1024 {
        return None;
    }

    // First take the top 54 bits (53 of mantissa plus one half bit), then visit as
    // few lower limbs as possible to determine rounding.
    let guard = bitlen - 54; // bitlen >= 54 here, so guard >= 0

    // Walk the limbs from the most significant down. The loop ends at the limb where
    // the 54-bit cut lands (`i`), which is the one that may still hold "lesser bits".
    let (mut mant, mut lesser_bits, mut i): (u64, u64, usize) = {
        let mut mant: u64 = 0;
        let mut i = n - 1;
        loop {
            let dig = limbs[i];
            let lsb = i * D_EXP; // bit position of this limb's least significant bit
            if lsb > guard {
                // Entirely above the cut: every set bit lands within the 54 bits. The
                // shift is `54 - msd_bits <= 53`, so it never overflows.
                mant |= dig << (lsb - guard);
            } else {
                // Lowest part of the mantissa plus maybe some lesser bits. `k < 64`
                // because the limb above (`lsb == (i+1)*64`) already satisfied
                // `lsb > guard` unless this is the top limb (whose `lsb <= 64`).
                let k = guard - lsb;
                let lesser_bits = dig & ((1u64 << k) - 1);
                mant |= dig >> k;
                break (mant, lesser_bits, i);
            }
            i -= 1;
        }
    };

    let half_bit = (mant & 1) as u32;
    mant >>= 1; // 53 significant bits remain
    let mut exp = (guard + 1) as i32;

    // Round to nearest, ties to even.
    if half_bit != 0 {
        // The half bit is set: round up unless the value is exactly even and a half
        // (then round down to the even mantissa).
        let mut round_up = (mant & 1) != 0;
        if !round_up {
            // Mantissa even: any set lesser bit wins the tie (round up); if none is
            // set and there are no more limbs, it is exactly even and a half.
            loop {
                if lesser_bits != 0 {
                    round_up = true;
                    break;
                }
                if i == 0 {
                    // Exactly even and a half: round down.
                    break;
                }
                i -= 1;
                lesser_bits = limbs[i];
            }
        }

        if round_up {
            mant += 1;
            if mant == (1u64 << 53) {
                mant >>= 1; // carried out of the mantissa
                exp += 1;
            }
        }
    }
    // `mant_exp_done:`

    // `ldexp((double)mant, exp)`: `mant < 2^53` is exact and `exp >= 1`, so scaling
    // by an exact power of two is a pure exponent adjustment (exact unless it
    // overflows/underflows). `mant as f64 * 2^exp` is therefore bit-exact `ldexp`.
    let d = mant as f64 * pow2(exp);
    if !d.is_finite() {
        return None;
    }

    Some(if negative { -d } else { d })
}

/// `2^e` for `e >= 0` — an exact power of two (`e` is at most ~972 here).
#[inline]
fn pow2(e: i32) -> f64 {
    // `2.0f64.powi` uses binary exponentiation over powers of two, all exactly
    // representable for `0 <= e <= 1023`, so the product is exact.
    2.0f64.powi(e)
}

/// Best-effort `f64` value of a numeric term (`number_as_f64`).
///
/// **Not used for exact comparison.** `compare`'s arithmetic-mode mixed
/// integer/float comparison deliberately does *not* call this: converting both
/// sides to `f64` loses any integer precision beyond the 53-bit mantissa (e.g. it
/// would report `9007199254740993 == 9007199254740992.0`), so that path ports
/// ERTS's own exact integer ↔ float algorithm instead (`compare::numbers`).
/// This function remains the convenience total conversion for callers that
/// genuinely want a best-effort `double` (and [`big_to_f64`] the faithful,
/// fallible one).
///
/// Total and panic-free over **any** word:
/// * a small integer → its value;
/// * a float → its value;
/// * a bignum → [`big_to_f64`] when the value is representable, else
///   `±f64::INFINITY` (the sign taken from the bignum) — the same best-effort
///   behaviour as before, but now with the faithful nearest-double rounding for
///   every representable bignum;
/// * any non-number (atom, list, tuple, …) → `f64::NAN`;
/// * a word that is not a term at all (a bare header word, e.g. the ABI's
///   `THE_NON_VALUE`; see [`crate::tag::is_term_word`]) → `f64::NAN`, via
///   [`TermClass::NonValue`] and without any dereference.
pub fn number_as_f64(x: Eterm) -> f64 {
    // SAFETY: `class` is total: it classifies an immediate by its tag bits, derefs
    // the header of a well-formed boxed term, and answers `NonValue` (no deref) for
    // a word that is not a term at all. `float_value`/`big_parts` are only reached
    // for the classes that establish a live float/bignum block.
    match unsafe { class(x) } {
        TermClass::Small => signed_value(x) as f64,
        TermClass::Float => unsafe { float_value(x) },
        TermClass::Big => big_to_f64(x).unwrap_or_else(|| {
            // SAFETY: `x` classified as `Big`, so it is a live bignum.
            let (negative, _) = unsafe { big_parts(x) };
            if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }
        }),
        _ => f64::NAN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_small, NIL};
    use crate::thing::{big_size_words, write_big, write_float, FLOAT_SIZE_WORDS};
    use crate::Atom;

    /// Build a bignum term owned by `heap` (which must outlive the result).
    fn big(heap: &mut Heap, negative: bool, limbs: &[u64]) -> Eterm {
        // SAFETY: the arena owns the `big_size_words(limbs.len())`-word block and
        // stays alive for as long as the returned term is used.
        let hp = unsafe { heap.alloc_words(big_size_words(limbs.len())).unwrap() };
        // SAFETY: `hp` addresses exactly `big_size_words(limbs.len())` writable
        // words, which `write_big` fully initialises.
        unsafe { write_big(hp, negative, limbs) }
    }

    fn float(heap: &mut Heap, value: f64) -> Eterm {
        // SAFETY: the arena keeps the `FLOAT_SIZE_WORDS`-word block alive.
        let hp = unsafe { heap.alloc_words(FLOAT_SIZE_WORDS).unwrap() };
        // SAFETY: `hp` addresses exactly `FLOAT_SIZE_WORDS` writable words.
        unsafe { write_float(hp, value) }
    }

    /// The bignum whose value is `sum(limbs[i] << (64 * i))`, as a decimal string.
    ///
    /// Arbitrary-precision integer → decimal (schoolbook, base 10^9 limbs). Used to
    /// build an **independent** correctly-rounded reference for [`big_to_f64`]: a
    /// full-precision decimal string parsed by `str::parse::<f64>()` is guaranteed
    /// by Rust to yield the nearest representable double (ties to even) or `±inf`
    /// on overflow — no dependency on this crate's own rounding.
    fn limbs_to_decimal(limbs: &[u64]) -> String {
        const BASE: u128 = 1_000_000_000;
        // Little-endian base-1e9 digits; start at 0.
        let mut dec: Vec<u32> = vec![0];
        for &limb in limbs.iter().rev() {
            // dec = dec * 2^64 + limb
            let mut carry = limb as u128;
            for d in dec.iter_mut() {
                let t = ((*d as u128) << 64) + carry;
                *d = (t % BASE) as u32;
                carry = t / BASE;
            }
            while carry > 0 {
                dec.push((carry % BASE) as u32);
                carry /= BASE;
            }
        }
        let mut s = String::new();
        let mut it = dec.iter().rev();
        s.push_str(&it.next().unwrap().to_string());
        for d in it {
            s.push_str(&format!("{d:09}"));
        }
        s
    }

    /// The correctly-rounded `f64` of the bignum `limbs` (LS-first), independent of
    /// [`big_to_f64`]: full-precision decimal → `str::parse::<f64>()` (documented as
    /// correctly rounded; overflow yields `±inf`).
    fn reference_f64(negative: bool, limbs: &[u64]) -> f64 {
        let mut s = limbs_to_decimal(limbs);
        if negative {
            s.insert(0, '-');
        }
        s.parse::<f64>().expect("decimal parses as f64")
    }

    /// Assert [`big_to_f64`] agrees with the independent reference for one bignum:
    /// `Some(d)` (finite, nearest-even) for a representable value, `None` exactly
    /// when the value overflows to `±inf`.
    fn check_against_reference(negative: bool, limbs: &[u64]) {
        let mut heap = Heap::new(big_size_words(limbs.len()) + 8);
        let t = big(&mut heap, negative, limbs);
        let want = reference_f64(negative, limbs);
        match big_to_f64(t) {
            Some(got) => {
                assert!(
                    want.is_finite(),
                    "big_to_f64 gave {got:e} but the exact value overflows ({limbs:?})"
                );
                assert_eq!(
                    got.to_bits(),
                    want.to_bits(),
                    "big_to_f64 gave {got:e}, reference gives {want:e} for {limbs:?}"
                );
            }
            None => {
                assert!(
                    !want.is_finite(),
                    "big_to_f64 gave None but the exact value is {want:e} for {limbs:?}"
                );
            }
        }
    }

    /// The limbs of the value `2^bitlen - 1` (all `bitlen` low bits set).
    fn all_ones(bitlen: usize) -> Vec<u64> {
        let n = bitlen.div_ceil(64);
        let mut v = vec![u64::MAX; n];
        let rem = bitlen % 64;
        if rem != 0 {
            *v.last_mut().unwrap() = (1u64 << rem) - 1;
        }
        v
    }

    /// The limbs of `2^(bitlen-1)` (single top bit set), `bitlen >= 1`.
    fn lone_top_bit(bitlen: usize) -> Vec<u64> {
        let n = bitlen.div_ceil(64);
        let mut v = vec![0u64; n];
        v[(bitlen - 1) / 64] = 1u64 << ((bitlen - 1) % 64);
        v
    }

    /// The limbs of the value `big + 1`, where `big` is `limbs` (little-endian).
    fn plus_one(limbs: &[u64]) -> Vec<u64> {
        let mut v = limbs.to_vec();
        for w in v.iter_mut() {
            let (s, carry) = w.overflowing_add(1);
            *w = s;
            if !carry {
                return v;
            }
        }
        v.push(1);
        v
    }

    #[test]
    fn big_to_f64_rejects_non_bignums() {
        let mut heap = Heap::new(8);
        assert_eq!(big_to_f64(make_small(7)), None);
        assert_eq!(big_to_f64(NIL), None);
        assert_eq!(big_to_f64(make_atom(Atom::from_raw(1))), None);
        let f = float(&mut heap, 1.5);
        assert_eq!(big_to_f64(f), None);
    }

    #[test]
    fn number_as_f64_covers_small_float_and_non_number() {
        let mut heap = Heap::new(8);
        assert_eq!(number_as_f64(make_small(42)), 42.0);
        assert_eq!(number_as_f64(make_small(-7)), -7.0);
        assert_eq!(number_as_f64(make_small(0)), 0.0);

        let f = float(&mut heap, 3.5);
        assert_eq!(number_as_f64(f), 3.5);
        let nf = float(&mut heap, -0.25);
        assert_eq!(number_as_f64(nf), -0.25);

        assert!(number_as_f64(NIL).is_nan());
        assert!(number_as_f64(make_atom(Atom::from_raw(1))).is_nan());
        assert!(number_as_f64(heap.cons(make_small(1), NIL).unwrap()).is_nan());
    }

    #[test]
    fn exact_powers_of_two() {
        let mut heap = Heap::new(64);
        // 2^60: one limb.
        assert_eq!(big_to_f64(big(&mut heap, false, &[1u64 << 60])), Some(2.0f64.powi(60)));
        // 2^64: two limbs [0, 1].
        assert_eq!(big_to_f64(big(&mut heap, false, &[0, 1])), Some(2.0f64.powi(64)));
        // 2^100: [0, 1 << 36].
        assert_eq!(
            big_to_f64(big(&mut heap, false, &[0, 1u64 << 36])),
            Some(2.0f64.powi(100))
        );
        // -(2^70): [0, 1 << 6].
        assert_eq!(
            big_to_f64(big(&mut heap, true, &[0, 1u64 << 6])),
            Some(-(2.0f64.powi(70)))
        );
    }

    #[test]
    fn mantissa_boundary_rounds_to_even() {
        let mut heap = Heap::new(16);
        // Exactly 2^53 (54-bit value): representable.
        assert_eq!(big_to_f64(big(&mut heap, false, &[1u64 << 53])), Some(9007199254740992.0));
        // 2^53 + 1 is exactly halfway between 2^53 and 2^53 + 2 -> ties to even -> 2^53.
        assert_eq!(
            big_to_f64(big(&mut heap, false, &[(1u64 << 53) + 1])),
            Some(9007199254740992.0)
        );
        // 2^53 + 3 -> 2^53 + 4.
        assert_eq!(
            big_to_f64(big(&mut heap, false, &[(1u64 << 53) + 3])),
            Some(((1u64 << 53) + 4) as f64)
        );
    }

    /// A case where the naive `d = d * 2^64 + digit` accumulation lands on the
    /// wrong double: `[1, 2^53 + 1]` is `2^117 + 2^64 + 1`, whose nearest double is
    /// `2^117 + 2^65` (it is just past the halfway point). The naive loop first
    /// rounds the top limb `2^53 + 1` down to `2^53`, then `2^117 + 1` rounds back
    /// to `2^117` — one ULP away (a factor of `2^65`) from the correct result.
    #[test]
    fn faithful_rounding_beats_digit_by_digit_accumulation() {
        let mut heap = Heap::new(16);
        let t = big(&mut heap, false, &[1, (1u64 << 53) + 1]);
        let expected = 2.0f64.powi(117) + 2.0f64.powi(65);
        assert_eq!(big_to_f64(t), Some(expected));

        // The old (naive) accumulation would have produced exactly 2^117 here.
        let naive = 2.0f64.powi(117);
        assert_ne!(expected, naive);
    }

    #[test]
    fn boundary_bitlen_and_non_finite_results() {
        let mut heap = Heap::new(200);

        // 2^1024: bitlen 1025 > 1024 -> Not representable.
        let mut limbs = vec![0u64; 17];
        limbs[16] = 1;
        assert_eq!(big_to_f64(big(&mut heap, false, &limbs)), None);
        // number_as_f64 keeps the sign for an unrepresentable magnitude.
        let big_neg = big(&mut heap, true, &limbs);
        assert_eq!(number_as_f64(big_neg), f64::NEG_INFINITY);
        let big_pos = big(&mut heap, false, &limbs);
        assert_eq!(number_as_f64(big_pos), f64::INFINITY);

        // 2^1023: bitlen 1024, finite -> Some.
        let mut p1023 = vec![0u64; 16];
        p1023[15] = 1u64 << 63;
        assert_eq!(big_to_f64(big(&mut heap, false, &p1023)), Some(2.0f64.powi(1023)));

        // 2^1024 - 2^970 (bits 970..=1023 set): bitlen 1024, but it rounds up to
        // 2^1024 -> non-finite -> None.
        let mut carry = vec![0u64; 16];
        carry[15] = u64::MAX << 10; // bits 10..=63 of limb 15 => bits 970..=1023
        assert_eq!(big_to_f64(big(&mut heap, false, &carry)), None);
    }

    /// The limbs of a pseudo-random `bitlen`-bit value (top bit forced set so the
    /// bignum is normalised), deterministic for a given `seed`.
    fn pattern_limbs(bitlen: usize, seed: u64) -> Vec<u64> {
        let n = bitlen.div_ceil(64);
        let mut v = vec![0u64; n];
        let mut x = seed | 1;
        for w in v.iter_mut() {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            *w = x;
        }
        let rem = bitlen % 64;
        if rem != 0 {
            *v.last_mut().unwrap() &= (1u64 << rem) - 1;
        }
        v[(bitlen - 1) / 64] |= 1u64 << ((bitlen - 1) % 64);
        v
    }

    /// A systematic sweep of the whole `bitlen` range: every width `1..=1100`
    /// against a power of two, an all-ones value and a random pattern, both signs.
    /// Each is checked against the independent correctly-rounded reference
    /// ([`reference_f64`]) — this covers the mantissa ties-to-even behaviour, the
    /// 1024-bit finite/overflow boundary and many-limb (≥3 limbs) values.
    #[test]
    fn bit_length_sweep_matches_correctly_rounded_reference() {
        for bitlen in 1..=1100usize {
            let cases = [
                lone_top_bit(bitlen),
                all_ones(bitlen),
                pattern_limbs(bitlen, (bitlen as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)),
            ];
            for limbs in cases {
                check_against_reference(false, &limbs);
                check_against_reference(true, &limbs);
            }
        }
    }

    /// One ULP either side of the rounding boundary across every bit width: for each
    /// `bitlen` the value `2^bitlen - 1` (which rounds up to `2^bitlen` when the
    /// mantissa carries) and its successor `2^bitlen` are both checked.
    #[test]
    fn sweep_across_carry_boundaries() {
        for bitlen in 54..=1030usize {
            let ones = all_ones(bitlen);
            check_against_reference(false, &ones);
            check_against_reference(true, &ones);
            let succ = plus_one(&ones); // 2^bitlen
            check_against_reference(false, &succ);
            check_against_reference(true, &succ);
        }
    }

    /// The exact overflow threshold and its neighbours, bitlen 1024.
    #[test]
    fn overflow_threshold_neighbourhood() {
        // f64::MAX = 2^1024 - 2^971: bits 971..=1023 set (limb 15, offsets 11..=63).
        let mut max = vec![0u64; 16];
        max[15] = u64::MAX << 11;
        assert_eq!(big_to_f64(big(&mut Heap::new(32), false, &max)), Some(f64::MAX));
        check_against_reference(false, &max);

        // f64::MAX - 2^970: one ULP below the max is still finite.
        let mut below = max.clone();
        below[15] &= !(1u64 << 10); // clear bit 970
        check_against_reference(false, &below);
        assert!(big_to_f64(big(&mut Heap::new(32), false, &below)).unwrap().is_finite());

        // f64::MAX + 1 = 2^1024 - 2^970: the exact halfway point to 2^1024; it
        // overflows to infinity (and `big_to_f64` fails -> the caller raises
        // badarith).
        let mut above = max.clone();
        above[15] |= 1u64 << 10;
        assert_eq!(big_to_f64(big(&mut Heap::new(32), false, &above)), None);
        assert_eq!(number_as_f64(big(&mut Heap::new(32), true, &above)), f64::NEG_INFINITY);
        check_against_reference(false, &above);

        // 2^1024 (bitlen 1025): rejected up front.
        let p1024 = lone_top_bit(1025);
        assert_eq!(big_to_f64(big(&mut Heap::new(32), false, &p1024)), None);
    }

    /// Hand-computed vectors at and around the 53/54-bit mantissa boundary, pinned
    /// against the exact semantics of ties-to-even. These are the values that a
    /// naive digit-by-digit accumulation gets wrong.
    #[test]
    fn mantissa_boundary_hand_computed() {
        let mut heap = Heap::new(32);
        let cases: &[(u64, f64)] = &[
            // 2^53 is exactly representable.
            (1u64 << 53, (1u64 << 53) as f64),
            // 2^53 + 1: exactly halfway between 2^53 and 2^53+2 -> ties to even -> 2^53.
            ((1u64 << 53) + 1, (1u64 << 53) as f64),
            // 2^53 + 2: representable (even).
            ((1u64 << 53) + 2, ((1u64 << 53) + 2) as f64),
            // 2^53 + 3: halfway between 2^53+2 and 2^53+4 -> ties to even -> 2^53+4.
            ((1u64 << 53) + 3, ((1u64 << 53) + 4) as f64),
            // 2^54 + 2: halfway between 2^54 and 2^54+4 -> ties to even -> 2^54.
            ((1u64 << 54) + 2, (1u64 << 54) as f64),
            // 2^54 + 6: halfway between 2^54+4 and 2^54+8 -> ties to even -> 2^54+8.
            ((1u64 << 54) + 6, ((1u64 << 54) + 8) as f64),
            // 2^60 (one limb, still exact).
            (1u64 << 60, (1u64 << 60) as f64),
        ];
        for &(value, want) in cases {
            assert_eq!(big_to_f64(big(&mut heap, false, &[value])), Some(want), "{value}");
        }
    }

    /// A two-limb value where the rounded result carries out of the mantissa and
    /// increments the exponent: `2^70 - 1` rounds up to `2^70`.
    #[test]
    fn carry_out_of_mantissa_increments_exponent() {
        let mut heap = Heap::new(64);
        // 2^70 - 1: two limbs, the low 64 bits and the low 6 bits of the next.
        let v = vec![u64::MAX, (1u64 << 6) - 1];
        assert_eq!(big_to_f64(big(&mut heap, false, &v)), Some(2.0f64.powi(70)));
        check_against_reference(false, &v);

        // 2^70 - 2^17 = 2^17 * (2^53 - 1) is exactly representable (mantissa 2^53-1).
        let mut exact = v.clone();
        exact[0] &= !((1u64 << 17) - 1);
        assert_eq!(
            big_to_f64(big(&mut heap, false, &exact)),
            Some(2.0f64.powi(70) - 2.0f64.powi(17)),
        );
        check_against_reference(false, &exact);
    }

    /// The limbs of the value with exactly the given bit positions set.
    fn value_from_bits(bits: &[usize]) -> Vec<u64> {
        let maxb = *bits.iter().max().expect("at least one bit");
        let mut v = vec![0u64; maxb / 64 + 1];
        for &b in bits {
            v[b / 64] |= 1u64 << (b % 64);
        }
        v
    }

    /// A curated set of vectors whose expected `f64` values were **verified against
    /// the real system Erlang** (`erl -noshell -eval 'float(X)'` →
    /// `<<F:64/float>>`): `tests/number_oracle.rs` performs the live cross-check over
    /// a wide sweep; this test keeps a meaningful, oracle-pinned core in the crate
    /// itself so it is exercised even where `erl` is not installed. Expected values
    /// are the oracle's exact output bits.
    #[test]
    fn oracle_verified_vectors() {
        // (name, negative, limbs, expected) with `None` = the oracle raised
        // `badarg` (out of range; `i_fconv` raises `badarith`).
        let two_pow = |e: i32| 2.0f64.powi(e);
        let max = {
            let mut v = vec![0u64; 16];
            v[15] = u64::MAX << 11; // 2^1024 - 2^971
            v
        };
        let halfway = {
            let mut v = vec![0u64; 16];
            v[15] = u64::MAX << 10; // 2^1024 - 2^970
            v
        };
        let cases: Vec<(&str, bool, Vec<u64>, Option<f64>)> = vec![
            // 2^53 + 1 -> 2^53 (ties to even). Oracle: 0x4340000000000000.
            ("2^53 + 1", false, vec![(1u64 << 53) + 1], Some(two_pow(53))),
            // 2^54 + 2 -> 2^54 (ties to even). Oracle: 0x4350000000000000.
            ("2^54 + 2", false, vec![(1u64 << 54) + 2], Some(two_pow(54))),
            // 2^54 + 6 -> 2^54 + 8. Oracle: 0x4350000000000002.
            ("2^54 + 6", false, vec![(1u64 << 54) + 6], Some(two_pow(54) + 8.0)),
            // 2^70 - 1 -> 2^70 (mantissa carries). Oracle: 0x4450000000000000.
            ("2^70 - 1", false, vec![u64::MAX, (1u64 << 6) - 1], Some(two_pow(70))),
            // 2^117 + 2^64 + 1 -> 2^117 + 2^65 (just past halfway). Oracle: 0x4740000000000001.
            (
                "2^117 + 2^64 + 1",
                false,
                value_from_bits(&[117, 64, 0]),
                Some(f64::from_bits(0x4740_0000_0000_0001)),
            ),
            // 2^70 + 2^17 is the exact halfway point -> ties to even -> 2^70.
            ("2^70 + 2^17", false, value_from_bits(&[70, 17]), Some(two_pow(70))),
            // 2^70 + 2^17 + 2^16 is past halfway -> rounds up to 2^70 + 2^18.
            (
                "2^70 + 2^17 + 2^16",
                false,
                value_from_bits(&[70, 17, 16]),
                Some(f64::from_bits(0x4450_0000_0000_0001)),
            ),
            // 2^70 + 2^18 + 2^17 is the next halfway point -> ties to even -> 2^70 + 2^19.
            (
                "2^70 + 2^18 + 2^17",
                false,
                value_from_bits(&[70, 18, 17]),
                Some(f64::from_bits(0x4450_0000_0000_0002)),
            ),
            // 2^100 + 2^47 + 12345: just above halfway -> 2^100 + 2^48.
            (
                "2^100 + 2^47 + 12345",
                false,
                {
                    let mut v = value_from_bits(&[100, 47]);
                    v[0] += 12345;
                    v
                },
                Some(f64::from_bits(0x4630_0000_0000_0001)),
            ),
            // 2^200 + 1 -> 2^200.
            ("2^200 + 1", false, value_from_bits(&[200, 0]), Some(two_pow(200))),
            // 2^1023: the largest power of two representable exactly.
            ("2^1023", false, value_from_bits(&[1023]), Some(f64::from_bits(0x7FE0_0000_0000_0000))),
            // f64::MAX = 2^1024 - 2^971: representable exactly.
            ("f64::MAX", false, max.clone(), Some(f64::MAX)),
            // -(2^70 - 1) -> -2^70. Oracle: 0xC450000000000000.
            ("-(2^70 - 1)", true, vec![u64::MAX, (1u64 << 6) - 1], Some(-two_pow(70))),
            // 2^1024 - 2^970: the exact halfway point to 2^1024 -> overflow -> None.
            ("2^1024 - 2^970", false, halfway, None),
            // 2^1024 - 1: overflow -> None.
            ("2^1024 - 1", false, vec![u64::MAX; 16], None),
        ];

        // The bit patterns above were captured from the system Erlang; the two
        // `from_bits` constants double as a documentation of the oracle output.
        assert_eq!(f64::from_bits(0x4740_0000_0000_0001), two_pow(117) + two_pow(65));

        for (name, negative, limbs, want) in cases {
            let mut heap = Heap::new(big_size_words(limbs.len()) + 8);
            let t = big(&mut heap, negative, &limbs);
            assert_eq!(big_to_f64(t), want, "big_to_f64({name})");
            // Independently confirmed against the same correctly-rounded reference.
            check_against_reference(negative, &limbs);
            match want {
                Some(w) => assert_eq!(number_as_f64(t).to_bits(), w.to_bits(), "number_as_f64({name})"),
                None => assert_eq!(
                    number_as_f64(t),
                    if negative { f64::NEG_INFINITY } else { f64::INFINITY },
                    "number_as_f64({name})",
                ),
            }
        }
    }

    /// The tie-break descent across **lower limbs**: when the half bit is set and the
    /// kept mantissa is even, the C walks down through the remaining limbs looking for
    /// any set "lesser" bit — it may descend through many all-zero limbs to `i == 0`
    /// (round down) or stop at the first non-zero limb (round up). Every expected value
    /// below was captured from the system Erlang (`float/1`).
    #[test]
    fn multi_limb_tie_break_descent() {
        // (name, limbs, expected)
        let cases: Vec<(&str, Vec<u64>, f64)> = vec![
            // Exactly even and a half with every lower limb zero -> round down.
            ("2^200 + 2^147", value_from_bits(&[200, 147]), 2.0f64.powi(200)),
            // A single set bit far below the window wins the tie -> round up.
            (
                "2^200 + 2^147 + 1",
                value_from_bits(&[200, 147, 0]),
                f64::from_bits(0x4C70_0000_0000_0001), // 2^200 + 2^148
            ),
            // Five limbs, tie, all lower limbs zero -> round down.
            ("2^300 + 2^247", value_from_bits(&[300, 247]), 2.0f64.powi(300)),
            (
                "2^300 + 2^247 + 2^100",
                value_from_bits(&[300, 247, 100]),
                f64::from_bits(0x52B0_0000_0000_0001), // 2^300 + 2^248
            ),
            // Just past the tie -> round up.
            (
                "2^200 + 2^148 + 2^147",
                value_from_bits(&[200, 148, 147]),
                f64::from_bits(0x4C70_0000_0000_0002), // 2^200 + 2^149
            ),
            // 16 limbs: the descent walks from limb 15 all the way to limb 0.
            ("2^1023 + 2^970", value_from_bits(&[1023, 970]), 2.0f64.powi(1023)),
            (
                "2^1023 + 2^970 + 1",
                value_from_bits(&[1023, 970, 0]),
                f64::from_bits(0x7FE0_0000_0000_0001), // 2^1023 + 2^971
            ),
        ];
        for (name, limbs, want) in cases {
            let mut heap = Heap::new(big_size_words(limbs.len()) + 8);
            let t = big(&mut heap, false, &limbs);
            assert_eq!(big_to_f64(t), Some(want), "big_to_f64({name})");
            check_against_reference(false, &limbs);
        }
    }
}
