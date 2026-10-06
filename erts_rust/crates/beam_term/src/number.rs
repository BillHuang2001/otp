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
//! silently wrong value. [`number_as_f64`] is the best-effort total variant used
//! by `compare`'s arithmetic-mode mixed int/float comparison.
//!
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
/// * `None` — `x` is not a bignum, or its magnitude is too large for a finite
///   double (`bitlen > 1024`, or a result that rounds up to `2^1024`).
///
/// A `None` result is exactly the C's `int < 0` return, which `i_fconv`
/// (`emu/float_instrs.tab:44`) turns into a `badarith` exception. The result is
/// never a silently wrong value.
pub fn big_to_f64(x: Eterm) -> Option<f64> {
    // SAFETY: `x` is a valid term; `is_big` only dereferences the header word of a
    // boxed term (and returns `false` for any non-boxed word), which is live for
    // any valid boxed `Eterm`.
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
/// Total and panic-free over any valid [`Eterm`]:
/// * a small integer → its value;
/// * a float → its value;
/// * a bignum → [`big_to_f64`] when the value is representable, else
///   `±f64::INFINITY` (the sign taken from the bignum) — the same best-effort
///   behaviour as before, but now with the faithful nearest-double rounding for
///   every representable bignum;
/// * any non-number (atom, list, tuple, …) → `f64::NAN`.
///
/// This is what `compare`'s arithmetic-mode mixed integer/float comparison uses;
/// a `NaN` therefore only ever arises from a non-number operand.
pub fn number_as_f64(x: Eterm) -> f64 {
    // SAFETY: an `Eterm` is a valid term by construction; `class`, `float_value`
    // and `big_parts` only dereference the header/payload of a boxed term, which is
    // live for any valid boxed term (the same invariant `compare::cmp` and
    // `size_object` rely on).
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
}
