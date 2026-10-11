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

//! The evaluation helpers behind the [`Expr`](super::Expr) variants: the guard
//! BIFs, the term-class probes, the boolean/atom conversions and the [`SelfHook`]
//! process-identity seam.
//!
//! Every helper classifies a term through [`super::classify`] (the shared probe)
//! and never re-implements term order or hashing — comparison delegates to
//! `beam_term::compare`. A construct outside the implemented subset is a typed
//! error, never a fabricated answer; the `// SAFETY:` rationales below name the
//! class that established each raw read.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_term::prelude::*;

use super::{CmpOp, TypeTest};
use crate::error::{badarg, unsupported};
use crate::select::atom_name;

/// `tuple_size/1`.
pub(super) fn tuple_size_of(operand: Eterm) -> Result<Eterm> {
    if super::classify(operand) != TermClass::Tuple {
        return Err(badarg());
    }
    // SAFETY: `class` established a live tuple header, whose arity word is readable.
    Ok(make_small(unsafe { tuple_size(operand) } as i64))
}

/// `byte_size/1` (`bytes`) / `bit_size/1`: the bitstring's bit count, or its byte
/// count `NBYTES(bits) = (bits + 7) / 8` (`erts/emulator/beam/erl_bits.h:27`).
///
/// A non-bitstring is the oracle's `badarg`. An **off-heap** (refc) binary is
/// handled: its on-heap `[BinRef][ErlSubBits]` pair is rooted at the `ErlSubBits`
/// (`beam_term::refc`), which classifies as `TermClass::Bitstring`, so the size is
/// read exactly — oracle parity, not a gap. The `TermClass::BinRef` arm below is
/// therefore reachable only for a **bare** `BinRef` word, which is never a
/// standalone user-visible term (a `BinRef` only ever appears as the first word of
/// that pair).
pub(super) fn bitstring_length(operand: Eterm, bytes: bool) -> Result<Eterm> {
    match super::classify(operand) {
        // SAFETY: the class established a live `ErlHeapBits`/`ErlSubBits` block, which
        // is all `bitstring_size` reads.
        TermClass::Bitstring => {
            let bits = unsafe { bitstring_size(operand) };
            Ok(make_small(if bytes { (bits + 7) / 8 } else { bits } as i64))
        }
        // A bare `BinRef` is not a bitstring root, so `bitstring_size` would read its
        // `val` pointer word as a bit size — report the documented gap rather than take
        // a wild read.
        TermClass::BinRef => Err(unsupported(
            "match-spec 'byte_size'/'bit_size': a bare BinRef is not a bitstring root",
        )),
        _ => Err(badarg()),
    }
}

/// `float/1`: a small integer casts to the nearest `f64` (the same rounding the
/// oracle's integer→float conversion uses), a bignum goes through
/// `beam_term::number::big_to_f64` (faithful nearest-double rounding, `badarg` when
/// beyond the `f64` range — what the oracle raises), a float is the identity, and
/// anything else is `badarg`.
pub(super) fn float_of(operand: Eterm, heap: &mut Heap) -> Result<Eterm> {
    let value = match super::classify(operand) {
        TermClass::Small => signed_value(operand) as f64,
        TermClass::Big => match big_to_f64(operand) {
            None => return Err(badarg()),
            Some(value) => value,
        },
        // `float/1` on a float is the identity (the term is returned unchanged).
        TermClass::Float => return Ok(operand),
        _ => return Err(badarg()),
    };
    write_float_term(heap, value)
}

/// Materialise a fresh float term on `heap`.
pub(super) fn write_float_term(heap: &mut Heap, value: f64) -> Result<Eterm> {
    // SAFETY: `alloc_words(FLOAT_SIZE_WORDS)` hands back the two exclusively-owned,
    // writable words a float occupies; `write_float` writes both before the term
    // escapes, and the heap outlives it.
    unsafe {
        let words = heap.alloc_words(FLOAT_SIZE_WORDS)?;
        Ok(write_float(words, value))
    }
}

/// Materialise a fresh bignum term on `heap` (`digits` are least-significant-first
/// `u64` limbs, exactly `beam_term::write_big`'s input).
fn write_big_term(heap: &mut Heap, negative: bool, digits: &[u64]) -> Result<Eterm> {
    // SAFETY: `alloc_words(big_size_words(digits.len()))` hands back that many
    // exclusively-owned, writable words — exactly `write_big`'s precondition (one
    // header word plus one word per digit), every one of which `write_big` writes
    // before the term escapes; the heap outlives it. `digits` is non-empty (both
    // callers guarantee it), so the zero-width-bignum case cannot arise.
    unsafe {
        let words = heap.alloc_words(big_size_words(digits.len()))?;
        Ok(write_big(words, negative, digits))
    }
}

/// The sign and the least-significant-first `u64` magnitude limbs of a finite,
/// **integral** `f64` whose magnitude is at least `2^59`.
///
/// A finite `f64` is `mantissa * 2^exp` with a 53-bit `mantissa`
/// (`(1 << 52) | fraction`) and `exp = biased_exponent - 1075`. At or above `2^59`
/// the biased exponent is at least `1082`, so `exp >= 7` — always non-negative,
/// hence the value is the mantissa shifted **left** (no right shift, so no rounding
/// is involved and the conversion is exact at any magnitude up to `2^1024`).
fn f64_magnitude_limbs(value: f64) -> (bool, Vec<u64>) {
    debug_assert!(
        value.is_finite() && value.fract() == 0.0 && value.abs() >= 576_460_752_303_423_488.0,
        "f64_magnitude_limbs takes a finite integral value whose magnitude is at least 2^59"
    );
    let bits = value.to_bits();
    let negative = (bits >> 63) != 0;
    let mantissa = (bits & ((1u64 << 52) - 1)) | (1u64 << 52);
    let exp = (((bits >> 52) & 0x7FF) as i64 - 1075) as u32;
    let word_shift = (exp / 64) as usize;
    let bit_shift = exp % 64;
    // `mantissa` is 53 bits wide, so a `bit_shift` in `0..=63` splits it across at
    // most two adjacent words (`bit_shift == 0` puts nothing in the high word).
    let mut limbs = vec![0u64; word_shift + 2];
    limbs[word_shift] |= mantissa << bit_shift;
    if bit_shift != 0 {
        limbs[word_shift + 1] |= mantissa >> (64 - bit_shift);
    }
    // Canonicalise: the most significant limb is non-zero (`mantissa >= 2^52`).
    while limbs.len() > 1 && limbs[limbs.len() - 1] == 0 {
        limbs.pop();
    }
    (negative, limbs)
}

/// `trunc/1` (`round == false`) / `round/1`.
///
/// An integer operand is the **identity** (what `erl` returns). A float truncates
/// toward zero (`f64::trunc`) or rounds **half away from zero** (`f64::round` is
/// exactly Erlang's — verified against `erl`: `round(2.5)` is `3`, `round(-2.5)` is
/// `-3`, `round(2.4)` is `2`, `trunc(1.9)` is `1`, `trunc(-1.9)` is `-1`). A finite
/// integral result outside the 60-bit small range is a **bignum** in the oracle
/// (`round(1.0e30)` is `1000000000000000019884624838656`), materialised exactly
/// from the float's bits by [`float_to_integer`] — never a truncated wrong integer.
/// A non-number is the oracle's `badarg`.
pub(super) fn integral_of(operand: Eterm, round: bool, heap: &mut Heap) -> Result<Eterm> {
    match super::classify(operand) {
        // An integer operand is returned unchanged (`trunc/1`/`round/1` are the
        // identity on integers, including a bignum).
        TermClass::Small | TermClass::Big => Ok(operand),
        TermClass::Float => {
            // SAFETY: the class established a live float block.
            let value = unsafe { float_value(operand) };
            let integral = if round { value.round() } else { value.trunc() };
            float_to_integer(integral, heap, "trunc'/'round")
        }
        _ => Err(badarg()),
    }
}

/// `ceil/1` (`up = true`) / `floor/1` (`up = false`). Identity on an integer
/// (small **and** bignum) — `ceil(7)` is `7` and returns the operand's own term,
/// exactly as `erl` does. A float's ceiling/floor outside the 60-bit small range is
/// a **bignum** in the oracle (`ceil(1.0e30)` is `1000000000000000019884624838656`,
/// `floor(-1.0e30)` is its negation), materialised by [`float_to_integer`].
pub(super) fn ceil_floor(operand: Eterm, up: bool, heap: &mut Heap) -> Result<Eterm> {
    match super::classify(operand) {
        TermClass::Small | TermClass::Big => Ok(operand),
        TermClass::Float => {
            // SAFETY: classify() established a live float block.
            let v = unsafe { float_value(operand) };
            float_to_integer(
                if up { v.ceil() } else { v.floor() },
                heap,
                if up { "ceil" } else { "floor" },
            )
        }
        _ => Err(badarg()),
    }
}

/// The shared `f64` → integer conversion of `ceil/1`, `floor/1`, `trunc/1` and
/// `round/1`: the small-range fast path is a 60-bit immediate, and a finite
/// integral result outside that range is the oracle's **bignum**, materialised
/// exactly (the value is `mantissa * 2^exp` with `exp >= 7`, so no rounding is
/// involved — see [`f64_magnitude_limbs`]).
///
/// The caller passes an already-integral value (`trunc`/`round`/`ceil`/`floor` of a
/// float), so an in-range value casts exactly. A **non-finite** value is the one
/// case with no oracle counterpart: `erl` cannot hold an `±inf`/`NaN` float
/// (`binary_to_term/1` rejects the encoding with `badarg`, verified against erts-17.1)
/// and this runtime's arithmetic hook refuses a float overflow, so it is a typed
/// capability gap rather than a fabricated integer.
fn float_to_integer(value: f64, heap: &mut Heap, what: &str) -> Result<Eterm> {
    // `2^59` is the exclusive bound of the 60-bit small range.
    const BOUND: f64 = 576_460_752_303_423_488.0;
    if !value.is_finite() {
        return Err(unsupported(&format!(
            "match-spec '{what}': a non-finite float result"
        )));
    }
    if value.abs() < BOUND {
        // `|value| < 2^59` and `value` is integral, so the cast is exact and in
        // range.
        return try_make_small(value as i64).map_err(|_| {
            unsupported(&format!(
                "match-spec '{what}': the result is not a small integer"
            ))
        });
    }
    let (negative, digits) = f64_magnitude_limbs(value);
    write_big_term(heap, negative, &digits)
}

/// `max/2` (`want_max = true`) / `min/2`. Pure **term order** (`cmp_arith`):
/// `max` is the second operand when the first is the lesser, `min` the second when
/// the first is the greater; an equal pair yields the first. Never raises — the
/// oracle's `max/2`/`min/2` accept any two terms.
pub(super) fn max_min(a: Eterm, b: Eterm, atoms: &AtomTable, want_max: bool) -> Eterm {
    use std::cmp::Ordering::{Greater, Less};
    match cmp_arith(a, b, atoms) {
        Less if want_max => b,
        Greater if !want_max => b,
        _ => a,
    }
}

/// `binary_part/2` — the spec operand is a **runtime** `{Pos, Len}` tuple.
pub(super) fn binary_part2(binary: Eterm, spec: Eterm, heap: &mut Heap) -> Result<Eterm> {
    // SAFETY: `is_tuple` inspects only the header word of a boxed term, so it is
    // sound for any live value word.
    if !unsafe { is_tuple(spec) } {
        return Err(badarg());
    }
    // SAFETY: `is_tuple(spec)` established a live tuple header.
    let arity = unsafe { tuple_size(spec) };
    if arity != 2 {
        return Err(badarg());
    }
    // SAFETY: `arity == 2`, so indices 0 and 1 are `< tuple_size(spec)`.
    let position = unsafe { tuple_element(spec, 0) };
    // SAFETY: as above.
    let length = unsafe { tuple_element(spec, 1) };
    binary_part3(binary, position, length, heap)
}

/// `binary_part/3` — the C `erts_binary_part` (`erl_bif_binary.c`) over the
/// crate's bitstring layout.
///
/// A non-bitstring source, a non-integer or negative `Pos`, a `Len` that runs past
/// the end, or a **non-byte-aligned** source (`TAIL_BITS(size) != 0`) is the
/// oracle's `badarg`. A negative `Len` counts backwards from `Pos`, which the C
/// expresses as `Len = -Len; Pos -= Len` with the bounds re-checked. The result is
/// materialised on `heap`; an off-heap (refc) binary source is accepted (it *is* a
/// bitstring) and read bit by bit.
pub(super) fn binary_part3(
    binary: Eterm,
    position: Eterm,
    length: Eterm,
    heap: &mut Heap,
) -> Result<Eterm> {
    // An on-heap bitstring and the `ErlSubBits` root of an off-heap refc binary
    // both classify as `Bitstring` (the frozen refc-binary root is the `ErlSubBits`,
    // `beam_term::refc`).
    match super::classify(binary) {
        TermClass::Bitstring => {}
        // A **bare** `BinRef` word is never a user-visible term, and it is not a
        // bitstring root: `bitstring_parts` would read its `val` pointer word as a
        // bit size. Report the documented capability gap rather than taking a wild
        // read (mirroring `bitstring_length`'s `BinRef` arm).
        TermClass::BinRef => {
            return Err(unsupported(
                "match-spec 'binary_part': a bare BinRef is not a bitstring root",
            ))
        }
        _ => return Err(badarg()),
    }
    // `term_to_Uint`: a small integer is the only admissible form here (a bignum, a
    // negative value or any other term is observably `badarg`).
    if !is_small(position) {
        return Err(badarg());
    }
    let mut start = signed_value(position);
    if start < 0 {
        return Err(badarg());
    }
    if !is_small(length) {
        return Err(badarg());
    }
    let mut count = signed_value(length);
    if count < 0 {
        count = count.checked_neg().ok_or_else(badarg)?;
        if count > start {
            return Err(badarg());
        }
        start -= count;
    }
    let end = start.checked_add(count).ok_or_else(badarg)?;
    // SAFETY: the class check established a live bitstring block, which is all
    // `bitstring_parts` reads.
    let (_base, _offset, size_bits) = unsafe { bitstring_parts(binary) };
    if size_bits % 8 != 0 {
        return Err(badarg());
    }
    let byte_size = (size_bits / 8) as i64;
    if end > byte_size {
        return Err(badarg());
    }
    // Gather the bytes MSB-first through `bit_at`, which accounts for a
    // sub-bitstring's base offset, so the result needs no knowledge of the layout.
    let mut bytes = Vec::with_capacity(count as usize);
    for index in start..end {
        let mut byte = 0u8;
        for bit in 0..8usize {
            let offset = (index as usize) * 8 + bit;
            // SAFETY: `offset < size_bits`, the bitstring's bit count.
            if unsafe { bit_at(binary, offset) } {
                byte |= 1 << (7 - bit);
            }
        }
        bytes.push(byte);
    }
    // SAFETY: `alloc_words(heap_bits_size_words(bits))` hands back exactly the
    // exclusively-owned, writable words `write_binary` initialises; the heap
    // outlives the term.
    let hp = unsafe { heap.alloc_words(heap_bits_size_words(bytes.len() * 8))? };
    // SAFETY: the reservation above matches `write_binary`'s precondition.
    unsafe { write_binary(hp, &bytes) }.map_err(BeamError::from)
}

/// `is_record/3` — the C `is_record_3` (`erl_bif_op.c`).
///
/// A non-atom `Name` is `badarg`. An **atom** `Size` selects the native-record
/// form: the term must be a native record whose definition module is the `Size`
/// atom and whose definition name is `Name`. A small-integer `Size` is the classic
/// tuple form: the term is a tuple of that arity whose **first element is `Name`**
/// (compared with `=:=`, i.e. [`eq`]). Anything else is `badarg`.
pub(super) fn is_record3(term: Eterm, name: Eterm, size: Eterm, atoms: &AtomTable) -> Result<bool> {
    if !is_atom(name) {
        return Err(badarg());
    }
    let name_atom = atom_val(name);
    if is_atom(size) {
        // The native-record form compares module + name; a non-record is `false`.
        if super::classify(term) != TermClass::Record {
            return Ok(false);
        }
        // SAFETY: the class check established a live native-record term.
        return Ok(unsafe { record_is_native(term, atom_val(size), name_atom) });
    }
    if !is_small(size) {
        return Err(badarg());
    }
    let want = signed_value(size);
    if !unsafe { is_tuple(term) } {
        return Ok(false);
    }
    // SAFETY: `is_tuple(term)` established a live tuple header.
    let arity = unsafe { tuple_size(term) } as i64;
    if arity != want {
        return Ok(false);
    }
    // A negative or zero `Size` can never equal a tuple's arity (`>= 1`), and a
    // zero-arity tuple does not exist, so there is no element 0 to read.
    if arity == 0 {
        return Ok(false);
    }
    // SAFETY: `arity >= 1`, so index 0 is `< tuple_size(term)`.
    Ok(eq(unsafe { tuple_element(term, 0) }, name, atoms))
}

/// `is_function/2` — whether `term` is a fun of exactly `arity`.
///
/// A negative `arity` (small or bignum) is the oracle's `badarg`; a positive
/// **bignum** arity can never match (no fun has more than `MAX_SMALL` parameters)
/// and is therefore `false` for a fun and `false` for anything else; any other
/// arity term is `badarg`.
pub(super) fn is_function2(term: Eterm, arity: Eterm) -> Result<bool> {
    let want: Option<usize> = match super::classify(arity) {
        TermClass::Small => {
            let value = signed_value(arity);
            if value < 0 {
                return Err(badarg());
            }
            Some(value as usize)
        }
        TermClass::Big => {
            // SAFETY: the class established a live bignum block, which is all
            // `big_parts` reads (its sign word and magnitude words).
            let negative = unsafe { big_parts(arity) }.0;
            if negative {
                return Err(badarg());
            }
            // A positive bignum is far beyond any fun arity.
            None
        }
        _ => return Err(badarg()),
    };
    if super::classify(term) != TermClass::Fun {
        return Ok(false);
    }
    let Some(want) = want else {
        return Ok(false);
    };
    // SAFETY: the class established a live fun block, so its header word (word 0)
    // is readable and carries the arity field.
    let header = unsafe { *boxed_val(term) };
    Ok(fun_arity(header) == want)
}

/// `is_integer/3` — whether `Value` is an integer in the inclusive range
/// `Lower..=Upper`. A non-integer bound is `badarg` (the oracle raises for a float
/// or any other non-integer bound), and a non-integer `Value` is `false`.
pub(super) fn is_integer3(
    value: Eterm,
    lower: Eterm,
    upper: Eterm,
    atoms: &AtomTable,
) -> Result<bool> {
    use std::cmp::Ordering::Greater;
    let is_integer =
        |term: Eterm| matches!(super::classify(term), TermClass::Small | TermClass::Big);
    if !is_integer(lower) || !is_integer(upper) {
        return Err(badarg());
    }
    if !is_integer(value) {
        return Ok(false);
    }
    // The C `is_integer_3` is `CMP_LE(Lower, Value) && CMP_LE(Value, Upper)` — BOTH
    // bounds are inclusive, so the upper test is `Value =< Upper` (not `Value < Upper`).
    Ok(cmp_arith(lower, value, atoms) != Greater && cmp_arith(value, upper, atoms) != Greater)
}

/// `node/0` — the local node name. The runtime-shape constant
/// `nonode@nohost` (a node that has not been given a name), resolved from the
/// caller's [`AtomTable`]; an uninterned name is the honest typed `badarg` (an
/// atom index is never fabricated).
pub(super) fn node0(atoms: &AtomTable) -> Result<Eterm> {
    atoms
        .lookup("nonode@nohost")
        .map(make_atom)
        .ok_or_else(badarg)
}

/// `node/1` — the node of a pid/port/reference (all `nonode@nohost` here, since no
/// distributed node is attached); any other term is the oracle's `badarg`.
pub(super) fn node1(term: Eterm, atoms: &AtomTable) -> Result<Eterm> {
    match super::classify(term) {
        TermClass::Pid
        | TermClass::ExternalPid
        | TermClass::Port
        | TermClass::ExternalPort
        | TermClass::Ref
        | TermClass::ExternalRef => node0(atoms),
        _ => Err(badarg()),
    }
}

/// `xor/2` — the exclusive-or of two booleans; a non-boolean operand is `badarg`.
pub(super) fn xor(a: Eterm, b: Eterm, atoms: &AtomTable) -> Result<bool> {
    let left = bool_of_atom(a, atoms).ok_or_else(badarg)?;
    let right = bool_of_atom(b, atoms).ok_or_else(badarg)?;
    Ok(left != right)
}

/// `abs/1` — the absolute value, sign-corrected at the term layer.
///
/// * A **small** integer keeps the 60-bit immediate fast path — except
///   `abs(MIN_SMALL)` (= `2^59`, exactly one past `MAX_SMALL`), whose oracle result
///   is a **bignum**.
/// * A **float** yields the boxed `|value|` (`abs(-1.5)` is `1.5`).
/// * A **bignum** keeps its magnitude digits verbatim and drops the sign bit (the
///   magnitude of a canonical bignum is already least-significant-first), so
///   `abs(-(1 bsl 70))` re-materialises `1 bsl 70` exactly.
///
/// Any other operand is the oracle's `badarg`.
pub(super) fn abs_of(operand: Eterm, heap: &mut Heap) -> Result<Eterm> {
    match super::classify(operand) {
        TermClass::Small => {
            let magnitude = signed_value(operand).unsigned_abs();
            if magnitude <= MAX_SMALL as u64 {
                return Ok(make_small(magnitude as i64));
            }
            // Only `abs(MIN_SMALL)` reaches here: its magnitude is exactly `2^59`,
            // so the oracle's result is a bignum.
            write_big_term(heap, false, &[magnitude])
        }
        // SAFETY: the class established a live float block.
        TermClass::Float => write_float_term(heap, unsafe { float_value(operand) }.abs()),
        // SAFETY: the class established a live bignum block, which is all
        // `big_parts` reads (its sign word and least-significant-first digits).
        TermClass::Big => {
            let (_, digits) = unsafe { big_parts(operand) };
            write_big_term(heap, false, digits)
        }
        _ => Err(badarg()),
    }
}

/// `bnot/1` — the bitwise complement, `bnot(N) = -N - 1`.
///
/// A **small** operand stays in the 60-bit small range: `!v` for `v` in
/// `-2^59..=2^59-1` is itself in that range, so it is the immediate
/// `make_small(!signed_value(operand))` (e.g. `bnot(MIN_SMALL)` is `2^59 - 1`).
///
/// A **bignum** operand is complemented exactly (`erl` returns a bignum):
/// * a non-negative `N` becomes `-(N + 1)`, an add-one-with-carry that may grow a new
///   high limb (`bnot(2^64 - 1)` is `-2^64`);
/// * a negative `N = -M` becomes the positive `M - 1`, a subtract-one-with-borrow
///   whose result is re-canonicalised of any high zero limb (`bnot(-2^64)` is
///   `2^64 - 1`).
///
/// Any other operand is the oracle's `badarg`.
pub(super) fn bnot(operand: Eterm, heap: &mut Heap) -> Result<Eterm> {
    match super::classify(operand) {
        TermClass::Small => Ok(make_small(!signed_value(operand))),
        // SAFETY: the class established a live bignum block, which is all
        // `big_parts` reads (its sign word and least-significant-first digits).
        TermClass::Big => {
            let (negative, digits) = unsafe { big_parts(operand) };
            if negative {
                // `N = -M`, so `bnot(N) = M - 1`: subtract one with borrow, high
                // limbs first becoming all-ones while the borrow propagates.
                let mut limbs = digits.to_vec();
                let mut index = 0;
                while index < limbs.len() && limbs[index] == 0 {
                    limbs[index] = u64::MAX;
                    index += 1;
                }
                if index == limbs.len() {
                    // A zero bignum (never produced by a canonical producer, which
                    // strips high zero digits): no `M >= 1`, so refuse rather than
                    // fabricate a value.
                    return Err(badarg());
                }
                limbs[index] -= 1;
                while limbs.len() > 1 && limbs[limbs.len() - 1] == 0 {
                    limbs.pop();
                }
                write_big_term(heap, false, &limbs)
            } else {
                // `N >= 0`, so `bnot(N) = -(N + 1)`: add one with carry, appending a
                // new high limb when every digit carries out.
                let mut limbs = digits.to_vec();
                let mut index = 0;
                loop {
                    if index == limbs.len() {
                        limbs.push(1);
                        break;
                    }
                    let (sum, carry) = limbs[index].overflowing_add(1);
                    limbs[index] = sum;
                    if !carry {
                        break;
                    }
                    index += 1;
                }
                write_big_term(heap, true, &limbs)
            }
        }
        _ => Err(badarg()),
    }
}

/// Materialise a map from already-evaluated key/value pairs on `heap`, through
/// `beam_term::map::write_map` (last-wins dedup, a flat map at most
/// `MAP_SMALL_MAP_LIMIT` keys, else the ERTS HAMT layout) — the frozen map builder,
/// never a local one.
pub(super) fn build_map(
    pairs: Vec<(Eterm, Eterm)>,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> Result<Eterm> {
    let capacity = map_words_capacity(pairs.len());
    // SAFETY: `alloc_words(capacity)` hands back `capacity` exclusively-owned,
    // writable words — exactly `write_map`'s precondition — and `write_map`
    // initialises every word of the map it returns, which borrows that storage; the
    // heap outlives the term.
    unsafe {
        let hp = heap.alloc_words(capacity)?;
        write_map(hp, &pairs, atoms).map_err(BeamError::from)
    }
}

/// A comparison, using `beam_term::compare` (never a local term order).
pub(super) fn compare(op: CmpOp, left: Eterm, right: Eterm, atoms: &AtomTable) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    match op {
        CmpOp::ExactEq => cmp(left, right, atoms) == Equal,
        CmpOp::ExactNe => cmp(left, right, atoms) != Equal,
        CmpOp::ArithEq => cmp_arith(left, right, atoms) == Equal,
        CmpOp::ArithNe => cmp_arith(left, right, atoms) != Equal,
        CmpOp::Lt => cmp_arith(left, right, atoms) == Less,
        CmpOp::Le => cmp_arith(left, right, atoms) != Greater,
        CmpOp::Gt => cmp_arith(left, right, atoms) == Greater,
        CmpOp::Ge => cmp_arith(left, right, atoms) != Less,
    }
}

/// A type test (`is_*`).
pub(super) fn type_test(test: TypeTest, operand: Eterm, atoms: &AtomTable) -> bool {
    use TermClass::*;
    let kind = super::classify(operand);
    match test {
        TypeTest::Atom => kind == Atom,
        TypeTest::Integer => matches!(kind, Small | Big),
        TypeTest::Float => kind == Float,
        TypeTest::Number => matches!(kind, Small | Big | Float),
        TypeTest::Tuple => kind == Tuple,
        // `is_list([])` is true in Erlang.
        TypeTest::List => matches!(kind, List | Nil),
        // SAFETY: `is_binary` reads the header of a bitstring term only; the class
        // check guarantees that.
        TypeTest::Binary => (kind == Bitstring && unsafe { is_binary(operand) }) || kind == BinRef,
        TypeTest::Bitstring => matches!(kind, Bitstring | BinRef),
        TypeTest::Map => kind == Map,
        TypeTest::Pid => matches!(kind, Pid | ExternalPid),
        TypeTest::Port => matches!(kind, Port | ExternalPort),
        TypeTest::Reference => matches!(kind, Ref | ExternalRef),
        TypeTest::Function => kind == Fun,
        TypeTest::Boolean => matches!(atom_name(operand, atoms), Some("true") | Some("false")),
    }
}

/// `element(N, Tuple)`.
pub(super) fn element(position: Eterm, tuple: Eterm) -> Result<Eterm> {
    if !is_small(position) {
        return Err(badarg());
    }
    let index = signed_value(position);
    if index < 1 {
        return Err(badarg());
    }
    if !unsafe { is_tuple(tuple) } {
        return Err(badarg());
    }
    // SAFETY: `is_tuple(tuple)` established a live tuple header.
    let arity = unsafe { tuple_size(tuple) };
    let offset = usize::try_from(index - 1).map_err(|_| badarg())?;
    if offset >= arity {
        return Err(badarg());
    }
    // SAFETY: `offset < arity == tuple_size(tuple)`.
    Ok(unsafe { tuple_element(tuple, offset) })
}

/// `size/1`: the arity of a tuple, or the byte size of a binary.
pub(super) fn size(operand: Eterm) -> Result<Eterm> {
    let kind = super::classify(operand);
    if kind == TermClass::Tuple {
        // SAFETY: `class` established a live tuple header.
        let arity = unsafe { tuple_size(operand) };
        return Ok(make_small(arity as i64));
    }
    // SAFETY: `is_binary` reads the header of a bitstring term only; the class
    // check guarantees that.
    if kind == TermClass::Bitstring && unsafe { is_binary(operand) } {
        // SAFETY: `is_binary` established a byte-aligned bitstring.
        let bits = unsafe { bitstring_size(operand) };
        return Ok(make_small((bits / 8) as i64));
    }
    Err(badarg())
}

/// The atom `true`/`false`.
pub(super) fn bool_term(value: bool, atoms: &AtomTable) -> Result<Eterm> {
    let name = if value { "true" } else { "false" };
    atoms.lookup(name).map(make_atom).ok_or_else(badarg)
}

/// Interpret a term as a boolean: the literal atom `true`/`false`, else `None`.
pub(super) fn bool_of_atom(term: Eterm, atoms: &AtomTable) -> Option<bool> {
    match atom_name(term, atoms) {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

/// The evaluation seam for `self/0` (mirrors [`super::ArithHook`]).
///
/// The ETS layer owns no process identity: [`super::eval`] takes no hook
/// parameter, so — exactly like the [`super::ArithHook`] seam — the hook is
/// installed **per thread** with [`set_self_hook`] by a runtime that knows the
/// calling process. Without one, `self/0` answers the typed `badarg` (see
/// [`self_term`]), which is the oracle's own error class for an unevaluable
/// guard: the clause is skipped, exactly as before, and the caller's
/// classification of the gap is unchanged.
pub trait SelfHook {
    /// The calling process's pid as a term, or the typed `badarg` when
    /// unavailable.
    fn self_pid(&self) -> Result<Eterm> {
        Err(badarg())
    }
}

thread_local! {
    /// The `self/0` hook installed on this thread; `None` means "no process
    /// identity" (the typed `badarg`, see [`set_self_hook`]).
    static SELF_HOOK: RefCell<Option<Rc<dyn SelfHook + 'static>>> = RefCell::new(None);
}

/// Install `hook` as the `self/0` hook of the **current thread**; `None` restores
/// the default (no process identity ⇒ the typed `badarg`).
///
/// Process-local by construction, exactly like [`super::set_arith_hook`]: every
/// table op runs on the calling thread, and a runtime installs the hook for the
/// process it is running.
pub fn set_self_hook(hook: Option<Rc<dyn SelfHook + 'static>>) {
    SELF_HOOK.with(|cell| *cell.borrow_mut() = hook);
}

/// The `self/0` hook in force on this thread, if one is installed.
///
/// The `Rc` is **cloned out** of the `RefCell` before it is returned, so no borrow
/// is held while the hook runs.
fn self_hook() -> Option<Rc<dyn SelfHook + 'static>> {
    SELF_HOOK.with(|cell| cell.borrow().clone())
}

/// `self/0`: the installed [`SelfHook`]'s pid, else the typed `badarg` (the ETS
/// layer has no process identity — the documented default gap).
pub(super) fn self_term() -> Result<Eterm> {
    match self_hook() {
        Some(hook) => hook.self_pid(),
        None => Err(badarg()),
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the numeric guard helpers that materialise a **bignum**: the
    //! `abs/1`/`bnot/1` bignum paths and the `trunc/1`/`round/1`/`ceil/1`/`floor/1`
    //! float→bignum conversion. The expectations are the values the system `erl`
    //! (OTP 29 / erts-17.1) produces through `ets:select/2`, quoted beside each case.

    use super::super::classify;
    use super::{abs_of, bnot, ceil_floor, integral_of, write_float_term};
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_term::{
        big_parts, big_size_words, float_value, make_small, signed_value, write_big, Eterm,
        TermClass, MAX_SMALL, MIN_SMALL, NIL,
    };

    /// A scratch heap with room for every bignum/float block a test builds.
    fn heap() -> Heap {
        Heap::new(256).expect("test heap")
    }

    /// A bignum term on `heap` (least-significant-first `u64` digits).
    fn big(heap: &mut Heap, negative: bool, digits: &[u64]) -> Eterm {
        // SAFETY: `alloc_words(big_size_words(digits.len()))` hands back exactly those
        // exclusively-owned, writable words, every one of which `write_big` fills; the
        // heap is a never-relocating arena that outlives the term.
        unsafe {
            let hp = heap
                .alloc_words(big_size_words(digits.len()))
                .expect("the heap has room for a bignum");
            write_big(hp, negative, digits)
        }
    }

    /// A float term on `heap`.
    fn float_term(heap: &mut Heap, value: f64) -> Eterm {
        write_float_term(heap, value).expect("the heap has room for a float")
    }

    /// Assert `term` is a bignum with exactly this sign and least-significant-first
    /// digits.
    fn assert_big(term: Eterm, negative: bool, digits: &[u64]) {
        let class = classify(term);
        assert_eq!(class, TermClass::Big, "the result must be a bignum, got {class:?}");
        // SAFETY: `classify` established a live bignum block, which is all `big_parts`
        // reads; the heap owning the term is still alive in this call.
        let (got_negative, got_digits) = unsafe { big_parts(term) };
        assert_eq!(got_negative, negative, "the bignum's sign");
        assert_eq!(got_digits, digits, "the bignum's digits (LS-first)");
    }

    /// The `f64` value of a term one of these tests produced as a float.
    fn as_float(term: Eterm) -> f64 {
        assert_eq!(classify(term), TermClass::Float, "expected a float term");
        // SAFETY: the assertion above established a live boxed float.
        unsafe { float_value(term) }
    }

    #[test]
    fn abs_of_a_small_integer_stays_small_except_min_small() {
        // erl: `abs(-3)` is 3 and `abs(MAX_SMALL)` is MAX_SMALL.
        let mut heap = heap();
        assert_eq!(signed_value(abs_of(make_small(-3), &mut heap).unwrap()), 3);
        let max = abs_of(make_small(MAX_SMALL), &mut heap).unwrap();
        assert_eq!(signed_value(max), MAX_SMALL);

        // erl: `abs(-576460752303423488)` is `576460752303423488` (= 2^59) — the one
        // small operand whose absolute value is a BIGNUM.
        let min = abs_of(make_small(MIN_SMALL), &mut heap).unwrap();
        assert_big(min, false, &[1u64 << 59]);
    }

    #[test]
    fn abs_of_a_bignum_keeps_its_magnitude() {
        // erl: `abs(-(1 bsl 70))` == `abs(1 bsl 70)` == `1180591620717411303424`
        // (= 2^70); `abs(-(2^64 + 5))` == `18446744073709551621`.
        let mut heap = heap();
        let magnitude = [0u64, 1u64 << 6]; // 2^70 = 2^(64 + 6)
        assert_big(abs_of(big(&mut heap, false, &magnitude), &mut heap).unwrap(), false, &magnitude);
        assert_big(abs_of(big(&mut heap, true, &magnitude), &mut heap).unwrap(), false, &magnitude);

        let other = [5u64, 1u64]; // 2^64 + 5
        assert_big(abs_of(big(&mut heap, true, &other), &mut heap).unwrap(), false, &other);
    }

    #[test]
    fn abs_of_a_float_is_the_boxed_magnitude() {
        // erl: `abs(-1.5)` is the float `1.5`.
        let mut heap = heap();
        let result = abs_of(float_term(&mut heap, -1.5), &mut heap).unwrap();
        assert_eq!(as_float(result), 1.5);
    }

    #[test]
    fn bnot_of_a_small_integer_is_the_immediate_complement() {
        // erl: `bnot(0)` is -1, `bnot(-1)` is 0, `bnot(MAX_SMALL)` is MIN_SMALL and
        // `bnot(MIN_SMALL)` is `576460752303423487` (= 2^59 - 1 = MAX_SMALL).
        let mut heap = heap();
        assert_eq!(signed_value(bnot(make_small(0), &mut heap).unwrap()), -1);
        assert_eq!(signed_value(bnot(make_small(-1), &mut heap).unwrap()), 0);
        assert_eq!(
            signed_value(bnot(make_small(MAX_SMALL), &mut heap).unwrap()),
            MIN_SMALL
        );
        assert_eq!(
            signed_value(bnot(make_small(MIN_SMALL), &mut heap).unwrap()),
            MAX_SMALL
        );
    }

    #[test]
    fn bnot_of_a_non_negative_bignum_adds_one_and_negates() {
        // erl: `bnot(18446744073709551615)` (= 2^64 - 1) is `-18446744073709551616`
        // (= -2^64) — the add-one carry GROWS a high limb.
        let mut heap = heap();
        let result = bnot(big(&mut heap, false, &[u64::MAX]), &mut heap).unwrap();
        assert_big(result, true, &[0, 1]);

        // erl: `bnot(18446744073709551616)` (= 2^64) is `-18446744073709551617`.
        let result = bnot(big(&mut heap, false, &[0, 1]), &mut heap).unwrap();
        assert_big(result, true, &[1, 1]);
    }

    #[test]
    fn bnot_of_a_negative_bignum_subtracts_one_and_canonicalises() {
        // erl: `bnot(-18446744073709551616)` (= -(2^64)) is `18446744073709551615` —
        // the subtract-one borrow leaves a high zero limb that is stripped.
        let mut heap = heap();
        let result = bnot(big(&mut heap, true, &[0, 1]), &mut heap).unwrap();
        assert_big(result, false, &[u64::MAX]);

        // erl: `bnot(-(2^64 + 5))` is `18446744073709551620` (= 2^64 + 4).
        let result = bnot(big(&mut heap, true, &[5, 1]), &mut heap).unwrap();
        assert_big(result, false, &[4, 1]);
    }

    #[test]
    fn integral_conversions_of_a_large_float_materialise_the_oracle_bignum() {
        // erl: `trunc(1.0e30)` == `round(1.0e30)` == `ceil(1.0e30)` ==
        //      `1000000000000000019884624838656`, and the negatives for `floor(-1.0e30)`
        //      etc. == `-1000000000000000019884624838656`.
        // The limbs are the exact integer decomposed least-significant-first.
        const ONE_E30: [u64; 2] = [0x4675_0000_0000_0000, 0x0000_000C_9F2C_9CD0];
        let mut heap = heap();

        for round in [false, true] {
            let result = integral_of(float_term(&mut heap, 1.0e30), round, &mut heap).unwrap();
            assert_big(result, false, &ONE_E30);
            let result = integral_of(float_term(&mut heap, -1.0e30), round, &mut heap).unwrap();
            assert_big(result, true, &ONE_E30);
        }
        for up in [false, true] {
            let result = ceil_floor(float_term(&mut heap, 1.0e30), up, &mut heap).unwrap();
            assert_big(result, false, &ONE_E30);
            let result = ceil_floor(float_term(&mut heap, -1.0e30), up, &mut heap).unwrap();
            assert_big(result, true, &ONE_E30);
        }
    }

    #[test]
    fn the_float_to_bignum_shift_covers_limb_boundaries() {
        // erl: `trunc(18446744073709551616.0)` (= 2^64) is `18446744073709551616` and
        // `trunc(8.307674973655724e+34)` (= 2^116) is `83076749736557242056487941267521536`.
        let mut heap = heap();
        let two_64 = float_term(&mut heap, 2f64.powi(64));
        let result = integral_of(two_64, false, &mut heap).unwrap();
        assert_big(result, false, &[0, 1]); // 2^64 = 2^(64 + 0)

        let two_116 = float_term(&mut heap, 2f64.powi(116));
        let result = integral_of(two_116, false, &mut heap).unwrap();
        assert_big(result, false, &[0, 1u64 << 52]); // 2^116 = 2^52 * 2^64
    }

    #[test]
    fn a_non_number_operand_is_badarg() {
        // erl raises `badarg` for `abs`/`bnot`/`trunc`/`round`/`ceil`/`floor` of a
        // non-number (here the empty list).
        let mut heap = heap();
        for error in [
            abs_of(NIL, &mut heap),
            bnot(NIL, &mut heap),
            integral_of(NIL, false, &mut heap),
            ceil_floor(NIL, true, &mut heap),
        ] {
            assert!(
                matches!(error, Err(BeamError::BadArg)),
                "a non-number operand is badarg"
            );
        }
    }
}
