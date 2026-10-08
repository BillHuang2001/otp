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

//! Element-update helpers for `ets:update_counter/3,4` and
//! `ets:update_element/3`.
//!
//! The term-level helpers below are shared by the `Interior` trait defaults (in
//! the parent module) and the hash interior's overriding inherent methods, so the
//! counter arithmetic and the tuple-rebuild logic exist in exactly one place.

use core::cmp::Ordering;

use beam_common::{BeamError, Result};
use beam_term::prelude::*;

use crate::error::badarg;
use crate::key::object_elements;

/// One decoded `ets:update_counter` operation.
pub(super) struct CounterOp {
    /// The 1-based element position (already checked to be `>= 1`; the
    /// `keypos`/arity rules are checked against the object by the caller).
    pub(super) pos: usize,
    /// The increment: any integer — a small integer or a bignum.
    pub(super) incr: Eterm,
    /// The `{Threshold, SetValue}` pair of the four-tuple form.
    pub(super) warp: Option<(Eterm, Eterm)>,
}

/// The arity of `object` when it is a tuple; `badarg` otherwise.
pub(super) fn object_arity(object: Eterm) -> Result<usize> {
    // SAFETY: `is_tuple` classifies a live term through its tag/header word only.
    if unsafe { is_tuple(object) } {
        // SAFETY: `object` is a live tuple (checked above), so its header word
        // holds a valid arity.
        Ok(unsafe { tuple_size(object) })
    } else {
        Err(badarg())
    }
}

/// Validate a 1-based element position against `object`'s arity and return a raw
/// pointer to that element word (for a destructive write).
///
/// `badarg` when `object` is not a tuple, or `pos` is outside `1..=arity`.
pub(crate) fn counter_slot(object: Eterm, pos: usize) -> Result<*mut Eterm> {
    if pos == 0 {
        return Err(badarg());
    }
    // SAFETY: `is_tuple` only reads the header word of a boxed term.
    if !unsafe { is_tuple(object) } {
        return Err(badarg());
    }
    // SAFETY: `object` is a live tuple (checked above).
    let arity = unsafe { tuple_size(object) };
    if pos > arity {
        return Err(badarg());
    }
    // SAFETY: `tuple_val` is the header pointer of the live tuple, and element
    // `pos` lives at `header + pos` (the header word occupies slot 0); `1 <= pos <=
    // arity` keeps the displacement within the tuple's `arity` element words.
    Ok(unsafe { tuple_val(object).add(pos) })
}

/// The integer element `pos` (1-based) of the tuple `object`.
///
/// `badarg` when `object` is not a tuple, `pos` is outside `1..=arity`, or the
/// element is not an integer. A **bignum** element is a valid `ets` counter and
/// is returned as-is.
pub(super) fn read_counter(object: Eterm, pos: usize) -> Result<Eterm> {
    let slot = counter_slot(object, pos)?;
    // SAFETY: `counter_slot` established that `slot` addresses element `pos` of
    // the live tuple `object`, so the word is an initialised term; `is_integer`
    // only dereferences the header word of a boxed term.
    let value = unsafe { *slot };
    // SAFETY: as above — the value read from the live slot is a live term.
    if unsafe { is_integer(value) } {
        Ok(value)
    } else {
        Err(counter_operand_error(value))
    }
}

/// The exact sum `old + incr` of two integer counter operands, materialised on
/// `heap`.
///
/// `badarg` for a non-integer operand (matching `ets:update_counter`). The sum is
/// computed exactly in arbitrary precision and returned as a **small integer**
/// when it fits the 60-bit range, else as a **bignum** written on `heap` —
/// exactly the promotion real ETS performs (`erl_db_util.c:3203` `db_add_counter`
/// → `big_plus`/`big_norm`, which normalises a fitting result back to a small).
///
/// `heap` must outlive the returned term, which points into it when the result is
/// a bignum. A zero result is always `make_small(0)` — never a zero-width bignum.
pub(crate) fn add_counters(
    old: Eterm,
    incr: Eterm,
    heap: &mut beam_alloc::Heap,
) -> Result<Eterm> {
    let (old_negative, old_digits) = magnitude(old)?;
    let (incr_negative, incr_digits) = magnitude(incr)?;
    let (negative, digits) = add_signed(
        old_negative,
        &old_digits,
        incr_negative,
        &incr_digits,
    );
    materialize_integer(negative, &digits, heap)
}

/// The `(negative, magnitude)` view of an integer operand.
///
/// `magnitude` is little-endian `ErtsDigit` (`u64`) limbs, normalised so the most
/// significant limb is non-zero (an empty slice is the value zero). Non-integers
/// are `badarg` (`counter_operand_error`).
fn magnitude(x: Eterm) -> Result<(bool, Vec<u64>)> {
    // SAFETY: `x` is a live term (a stored element or a caller-supplied operand),
    // so classifying it through its tag/header word is well defined.
    if !unsafe { is_integer(x) } {
        return Err(counter_operand_error(x));
    }
    if is_small(x) {
        let value = signed_value(x);
        // `unsigned_abs` also covers `MIN_SMALL` (whose negation does not fit an
        // `i64`) without overflowing.
        Ok(if value < 0 {
            (true, normalize(vec![value.unsigned_abs()]))
        } else {
            (false, normalize(vec![value as u64]))
        })
    } else {
        // SAFETY: `x` is an integer that is not a small, i.e. a live bignum, so
        // `big_parts` reads its header and digit words.
        let (negative, digits) = unsafe { big_parts(x) };
        Ok((negative, normalize(digits.to_vec())))
    }
}

/// Drop zero limbs above the most significant non-zero one (`big_norm`'s
/// leading-zero strip, `big.c:3344`), so the magnitude length is the significant
/// limb count and an empty slice means zero.
fn normalize(mut digits: Vec<u64>) -> Vec<u64> {
    while matches!(digits.last(), Some(0)) {
        digits.pop();
    }
    digits
}

/// Exact signed addition over the `(negative, magnitude)` representation.
fn add_signed(
    a_negative: bool,
    a: &[u64],
    b_negative: bool,
    b: &[u64],
) -> (bool, Vec<u64>) {
    if a_negative == b_negative {
        // Same sign: add the magnitudes, keep the sign (`I_add`, `big.c:3748`).
        (a_negative, add_magnitudes(a, b))
    } else {
        // Opposite signs: subtract the smaller magnitude from the larger and take
        // its sign; equal magnitudes cancel to zero (`big.c:3754-3761`).
        match compare_magnitudes(a, b) {
            Ordering::Equal => (false, Vec::new()),
            Ordering::Greater => (a_negative, subtract_magnitudes(a, b)),
            Ordering::Less => (b_negative, subtract_magnitudes(b, a)),
        }
    }
}

/// `a + b` over little-endian limbs (`I_add`), carrying in `u128`.
fn add_magnitudes(a: &[u64], b: &[u64]) -> Vec<u64> {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(long.len() + 1);
    let mut carry = 0u64;
    for i in 0..long.len() {
        let sum = long[i] as u128 + short.get(i).copied().unwrap_or(0) as u128 + carry as u128;
        out.push(sum as u64);
        carry = (sum >> 64) as u64;
    }
    if carry != 0 {
        out.push(carry);
    }
    out
}

/// `a - b` over little-endian limbs (`I_sub`), borrowing in `u64`.
///
/// The caller must pass `a >= b` (magnitude comparison); the result is normalised.
fn subtract_magnitudes(a: &[u64], b: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0u64;
    for i in 0..a.len() {
        let (partial, borrow_1) = a[i].overflowing_sub(b.get(i).copied().unwrap_or(0));
        let (digit, borrow_2) = partial.overflowing_sub(borrow);
        out.push(digit);
        // At most one of the two borrows can be set, so their sum is the outgoing
        // borrow (0 or 1).
        borrow = u64::from(borrow_1) + u64::from(borrow_2);
    }
    normalize(out)
}

/// Compare two normalised magnitudes (`I_comp`): by limb count, then limb-wise
/// from the most significant down.
fn compare_magnitudes(a: &[u64], b: &[u64]) -> Ordering {
    if a.len() != b.len() {
        return a.len().cmp(&b.len());
    }
    for i in (0..a.len()).rev() {
        if a[i] != b[i] {
            return a[i].cmp(&b[i]);
        }
    }
    Ordering::Equal
}

/// Materialise an exact integer result on `heap`: a small when it fits the
/// 60-bit range, else a bignum (`small_to_big`/`big_norm`, `big.c:3344-3369`).
fn materialize_integer(
    negative: bool,
    digits: &[u64],
    heap: &mut beam_alloc::Heap,
) -> Result<Eterm> {
    if digits.is_empty() {
        // A cancelled-out sum is the small zero, never a zero-width bignum.
        return Ok(make_small(0));
    }
    if digits.len() == 1 {
        // Widen to `i128` so `-mag` cannot overflow before the range check.
        let value: i128 = if negative {
            -(digits[0] as i128)
        } else {
            digits[0] as i128
        };
        if value >= MIN_SMALL as i128 && value <= MAX_SMALL as i128 {
            return Ok(make_small(value as i64));
        }
    }
    // SAFETY: `alloc_words` hands out `big_size_words(n) == n + 1` uninitialised
    // words, all of which `write_big` fills (header + the `n` digits); the heap
    // never reallocates, so the term stays valid for the heap's lifetime.
    let hp = unsafe { heap.alloc_words(big_size_words(digits.len()))? };
    // SAFETY: `hp` addresses exactly `big_size_words(len)` writable words and
    // `digits` is non-empty (`len >= 1` checked above) and normalised.
    Ok(unsafe { write_big(hp, negative, digits) })
}

/// A counter operand that must be an integer: a small integer or a bignum is
/// returned as-is, anything else is `badarg`.
fn counter_operand(operand: Eterm) -> Result<Eterm> {
    // SAFETY: `operand` is a live term (a caller-supplied `ets` operand), so
    // classifying it through its tag/header word is well defined.
    if unsafe { is_integer(operand) } {
        Ok(operand)
    } else {
        Err(counter_operand_error(operand))
    }
}

/// The error for a counter operand that is not an integer.
///
/// Every counter operand — the element, the increment, the threshold and the warp
/// value — must be an integer; real ETS accepts bignums too and so does this
/// crate, so anything else (a float, an atom, a tuple, …) is `badarg`
/// (`erl_db.c:1561-1582` rejects a non-integer operand with `badarg`).
pub(crate) fn counter_operand_error(x: Eterm) -> BeamError {
    // SAFETY: `x` is a live term; the caller only routes non-integers here.
    debug_assert!(
        !unsafe { is_integer(x) },
        "an integer operand is never an operand error"
    );
    badarg()
}

/// Whether the `{Pos, Incr, Threshold, SetValue}` threshold rule fires for
/// `result = old + incr`: a non-negative increment that pushed the counter
/// **above** `threshold`, or a negative one that dropped it **below**.
///
/// The comparison is the exact integer order (`beam_term::compare::cmp`), so it
/// is correct across the small/bignum boundary — matching the C's
/// `CMP(incr, make_small(0))` / `CMP(newcnt, threshold)` (`erl_db.c:1650-1652`).
pub(super) fn warp_fires(
    incr: Eterm,
    result: Eterm,
    threshold: Eterm,
    atoms: &AtomTable,
) -> bool {
    if cmp(incr, make_small(0), atoms) == Ordering::Less {
        cmp(result, threshold, atoms) == Ordering::Less
    } else {
        cmp(result, threshold, atoms) == Ordering::Greater
    }
}

/// Build a copy of the `arity`-tuple `object` on `heap`, overwriting each `pos`
/// (`1 <= pos <= arity`) with its `updates` value; later entries win.
pub(crate) fn replace_elements(
    object: Eterm,
    arity: usize,
    updates: &[(usize, Eterm)],
    heap: &mut beam_alloc::Heap,
) -> Result<Eterm> {
    // SAFETY: `object` is a live tuple with `arity` element words (established by
    // the caller) and stays valid for the duration of the copy below.
    let elements = unsafe { object_elements(object) };
    // SAFETY: `alloc_tuple` writes the arity header and hands back `arity`
    // uninitialised element words; the `copy_nonoverlapping` plus the update loop
    // write every one of them before `make_tuple` exposes the pointer as a term.
    unsafe {
        let hdr = heap.alloc_tuple(arity)?;
        core::ptr::copy_nonoverlapping(elements.as_ptr(), hdr.add(1), arity);
        for &(pos, value) in updates {
            // `pos` is 1-based; the header occupies slot 0, so element `pos` lives
            // at `hdr + pos` (in range, since the caller validated `pos <= arity`).
            *hdr.add(pos) = value;
        }
        Ok(make_tuple(hdr))
    }
}

/// Decode an `ets:update_counter` operand into its operations, reporting whether
/// the *result* is a list.
///
/// Mirrors the BIF's operand dispatch (`../../erts/emulator/beam/erl_db.c`,
/// `do_update_counter`), verified against the system `erl`:
/// * an integer is a bare increment on `keypos + 1` (a scalar result);
/// * a tuple is a single `{Pos,Incr}` / `{Pos,Incr,Threshold,SetValue}` (scalar);
/// * **anything else** is read as a list of such tuples, and the result is a list
///   — so a non-list operand (an atom, an improper list) is `badarg`.
pub(super) fn decode_counter_ops(update_op: Eterm, keypos: usize) -> Result<(Vec<CounterOp>, bool)> {
    // SAFETY: `is_integer`/`is_tuple` classify a live term through its tag word.
    if unsafe { is_integer(update_op) } {
        let op = CounterOp {
            pos: keypos + 1,
            incr: counter_operand(update_op)?,
            warp: None,
        };
        return Ok((vec![op], false));
    }
    // SAFETY: as above — a tag/header-word classification.
    if unsafe { is_tuple(update_op) } {
        return Ok((vec![decode_counter_tuple(update_op)?], false));
    }
    let mut ops = Vec::new();
    let mut element = update_op;
    while is_list(element) {
        // SAFETY: `element` is a live non-`NIL` cons cell (checked above), so its
        // two words are initialised.
        let head = unsafe { car(element) };
        // SAFETY: as above — the cell's tail word of the same live cell.
        let tail = unsafe { cdr(element) };
        ops.push(decode_counter_tuple(head)?);
        element = tail;
    }
    if element != NIL {
        // An improper list (or a non-list operand such as an atom).
        return Err(badarg());
    }
    Ok((ops, true))
}

/// Decode one `{Pos, Incr}` / `{Pos, Incr, Threshold, SetValue}` operation.
///
/// `erl` requires `Pos` to be a **small** integer — a bignum position is
/// `badarg`, deliberately *even though* a bignum `Incr` / `Threshold` /
/// `SetValue` is accepted (`erl_db.c:1573`: `if (!is_small(tpl[1])) goto
/// finalize;`) — and `Incr` / `Threshold` / `SetValue` to be integers (small or
/// bignum); anything else is `badarg`.
fn decode_counter_tuple(upop: Eterm) -> Result<CounterOp> {
    let arity = object_arity(upop)?;
    if arity != 2 && arity != 4 {
        return Err(badarg());
    }
    // SAFETY: `upop` is a live tuple with at least two elements (its arity was
    // just checked), so elements 0 and 1 are initialised.
    let (pos_term, incr_term) = unsafe { (tuple_element(upop, 0), tuple_element(upop, 1)) };
    // The position must be a small integer (`erl` rejects a bignum position with
    // `badarg`, not with a bignum failure).
    if !is_small(pos_term) {
        return Err(badarg());
    }
    let pos = signed_value(pos_term);
    if pos < 1 {
        return Err(badarg());
    }
    let incr = counter_operand(incr_term)?;
    let warp = if arity == 4 {
        // SAFETY: `upop` is a live 4-tuple (its arity was checked), so elements 2
        // and 3 are initialised.
        let (threshold, set_value) =
            unsafe { (tuple_element(upop, 2), tuple_element(upop, 3)) };
        Some((counter_operand(threshold)?, counter_operand(set_value)?))
    } else {
        None
    };
    Ok(CounterOp {
        pos: pos as usize,
        incr,
        warp,
    })
}

/// A proper list of `values` built on `heap` (the `ets:update_counter` list
/// result).
pub(super) fn build_list(heap: &mut beam_alloc::Heap, values: &[Eterm]) -> Result<Eterm> {
    let mut list = NIL;
    for &value in values.iter().rev() {
        list = heap.cons(value, list)?;
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bignum of `digits` (least significant first) on `heap`.
    fn big_on(heap: &mut beam_alloc::Heap, negative: bool, digits: &[u64]) -> Eterm {
        // SAFETY: `alloc_words` hands out `big_size_words(n)` writable words, all
        // written by `write_big` (`digits` is non-empty at every call site below).
        let hp = unsafe { heap.alloc_words(big_size_words(digits.len())).unwrap() };
        // SAFETY: as above — `hp` addresses exactly `big_size_words(n)` words.
        unsafe { write_big(hp, negative, digits) }
    }

    /// Assert an integer term's exact `(sign, magnitude)` value, so the check is
    /// independent of whether the result is a small or a bignum.
    fn assert_value(term: Eterm, negative: bool, digits: &[u64]) {
        assert_eq!(
            magnitude(term).unwrap(),
            (negative, digits.to_vec()),
            "value of {:#x}",
            term.raw()
        );
    }

    #[test]
    fn small_sums_that_fit_stay_small() {
        let mut out = beam_alloc::Heap::new(64).unwrap();
        let sum = add_counters(make_small(2), make_small(3), &mut out).unwrap();
        assert_eq!(sum, make_small(5));
        assert!(is_small(sum));

        let sum = add_counters(make_small(-5), make_small(2), &mut out).unwrap();
        assert_eq!(sum, make_small(-3));

        // A sum that cancels is the small zero.
        let sum = add_counters(make_small(3), make_small(-3), &mut out).unwrap();
        assert_eq!(sum, make_small(0));
    }

    #[test]
    fn out_of_range_sums_promote_to_bignums() {
        let mut out = beam_alloc::Heap::new(64).unwrap();

        // `MAX_SMALL + 1 == 2^59`, one limb.
        let sum = add_counters(make_small(MAX_SMALL), make_small(1), &mut out).unwrap();
        assert!(unsafe { is_big(sum) });
        assert_value(sum, false, &[1u64 << 59]);

        // `MIN_SMALL - 1 == -(2^59 + 1)`.
        let sum = add_counters(make_small(MIN_SMALL), make_small(-1), &mut out).unwrap();
        assert!(unsafe { is_big(sum) });
        assert_value(sum, true, &[(1u64 << 59) + 1]);

        // The two boundary values themselves are still smalls.
        let sum = add_counters(make_small(MAX_SMALL - 1), make_small(1), &mut out).unwrap();
        assert_eq!(sum, make_small(MAX_SMALL));
        let sum = add_counters(make_small(MIN_SMALL + 1), make_small(-1), &mut out).unwrap();
        assert_eq!(sum, make_small(MIN_SMALL));
    }

    #[test]
    fn bignum_plus_small_keeps_the_bignum_resolution() {
        let mut out = beam_alloc::Heap::new(64).unwrap();
        // `2^64 + 5`, where `2^64` is the two-limb magnitude `[0, 1]`.
        let big = big_on(&mut out, false, &[0, 1]);
        let sum = add_counters(big, make_small(5), &mut out).unwrap();
        assert_value(sum, false, &[5, 1]);

        // Small + bignum (the operands' order must not matter).
        let big = big_on(&mut out, true, &[7, 1]);
        let sum = add_counters(make_small(2), big, &mut out).unwrap();
        assert_value(sum, true, &[5, 1]);
    }

    #[test]
    fn bignum_plus_bignum_carries_across_limbs() {
        let mut out = beam_alloc::Heap::new(128).unwrap();
        // `[u64::MAX, u64::MAX] + 1 == 2^128`, a three-limb magnitude.
        let a = big_on(&mut out, false, &[u64::MAX, u64::MAX]);
        let b = big_on(&mut out, false, &[1]);
        let sum = add_counters(a, b, &mut out).unwrap();
        assert_value(sum, false, &[0, 0, 1]);

        // Two three-limb magnitudes whose sum stays three limbs.
        let a = big_on(&mut out, false, &[0, 0, 1]);
        let b = big_on(&mut out, false, &[0, 0, 1]);
        let sum = add_counters(a, b, &mut out).unwrap();
        assert_value(sum, false, &[0, 0, 2]);
    }

    #[test]
    fn opposite_signs_subtract_by_magnitude() {
        let mut out = beam_alloc::Heap::new(128).unwrap();

        // `2^64 x 1 - 1 == u64::MAX`, still one limb after the borrow.
        let a = big_on(&mut out, false, &[0, 1]);
        let b = big_on(&mut out, true, &[1]);
        let sum = add_counters(a, b, &mut out).unwrap();
        assert_value(sum, false, &[u64::MAX]);

        // The magnitude sub-result loses its leading zero limb.
        let a = big_on(&mut out, false, &[0, 0, 1]);
        let b = big_on(&mut out, true, &[1]);
        let sum = add_counters(a, b, &mut out).unwrap();
        assert_value(sum, false, &[u64::MAX, u64::MAX]);

        // `-(2^64 + 5) + (2^64 + 1) == -4`, a small.
        let a = big_on(&mut out, true, &[5, 1]);
        let b = big_on(&mut out, false, &[1, 1]);
        let sum = add_counters(a, b, &mut out).unwrap();
        assert_eq!(sum, make_small(-4));

        // Equal magnitudes cancel to the small zero, never a zero-width bignum.
        let a = big_on(&mut out, false, &[3, 9, 1]);
        let b = big_on(&mut out, true, &[3, 9, 1]);
        let sum = add_counters(a, b, &mut out).unwrap();
        assert_eq!(sum, make_small(0));
    }

    #[test]
    fn non_integer_operands_are_badarg() {
        let mut out = beam_alloc::Heap::new(64).unwrap();
        // `NIL` stands in for any non-integer; floats/atoms take the same path.
        assert!(matches!(
            add_counters(NIL, make_small(1), &mut out),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            add_counters(make_small(1), NIL, &mut out),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn warp_fires_compares_exact_integers_across_the_small_boundary() {
        let mut atoms = AtomTable::new();
        // The comparison is pure integer order, so the atom table is only needed to
        // satisfy `cmp`'s signature (integers never consult it).
        atoms.intern("unused");
        let mut out = beam_alloc::Heap::new(64).unwrap();

        let big = big_on(&mut out, false, &[0, 1]); // 2^64
        let nbig = big_on(&mut out, true, &[0, 1]); // -2^64

        // A non-negative increment fires when the result is strictly above the
        // threshold — and a bignum result is above any small threshold.
        assert!(warp_fires(make_small(1), big, make_small(5), &atoms));
        assert!(!warp_fires(make_small(1), make_small(5), make_small(5), &atoms));
        // A zero increment counts as non-negative.
        assert!(warp_fires(make_small(0), make_small(6), make_small(5), &atoms));

        // A negative increment fires when the result is strictly below.
        assert!(warp_fires(make_small(-1), make_small(0), big, &atoms));
        assert!(!warp_fires(nbig, big, nbig, &atoms));

        // A bignum increment's sign selects the rule, too.
        assert!(warp_fires(big, make_small(6), make_small(5), &atoms));
        assert!(warp_fires(nbig, make_small(4), make_small(5), &atoms));
    }
}
