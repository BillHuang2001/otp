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
/// A non-bitstring is the oracle's `badarg`. An **off-heap** (refc) binary is a
/// typed capability gap rather than a `badarg`: it *is* a bitstring, so reporting
/// `badarg` would let a guard silently under-match (workspace `CONTEXT.md` §3).
pub(super) fn bitstring_length(operand: Eterm, bytes: bool) -> Result<Eterm> {
    match super::classify(operand) {
        // SAFETY: the class established a live `ErlHeapBits`/`ErlSubBits` block, which
        // is all `bitstring_size` reads.
        TermClass::Bitstring => {
            let bits = unsafe { bitstring_size(operand) };
            Ok(make_small(if bytes { (bits + 7) / 8 } else { bits } as i64))
        }
        TermClass::BinRef => Err(unsupported(
            "match-spec 'byte_size'/'bit_size' on an off-heap (refc) binary",
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

/// `trunc/1` (`round == false`) / `round/1`.
///
/// An integer operand is the **identity** (what `erl` returns). A float truncates
/// toward zero (`f64::trunc`) or rounds **half away from zero** (`f64::round` is
/// exactly Erlang's — verified against `erl`: `round(2.5)` is `3`, `round(-2.5)` is
/// `-3`, `round(2.4)` is `2`, `trunc(1.9)` is `1`, `trunc(-1.9)` is `-1`). A result
/// outside the 60-bit small range is a bignum in the oracle (`round(1.0e30)`), so it
/// is the typed `unsupported_ets` — never a truncated wrong integer. A non-number is
/// the oracle's `badarg`.
pub(super) fn integral_of(operand: Eterm, round: bool) -> Result<Eterm> {
    match super::classify(operand) {
        // An integer operand is returned unchanged (`trunc/1`/`round/1` are the
        // identity on integers, including a bignum).
        TermClass::Small | TermClass::Big => Ok(operand),
        TermClass::Float => {
            // SAFETY: the class established a live float block.
            let value = unsafe { float_value(operand) };
            let integral = if round { value.round() } else { value.trunc() };
            float_to_small(integral, "trunc'/'round")
        }
        _ => Err(badarg()),
    }
}

/// `ceil/1` (`up = true`) / `floor/1` (`up = false`). Identity on an integer
/// (small **and** bignum) — `ceil(7)` is `7` and returns the operand's own term,
/// exactly as `erl` does.
pub(super) fn ceil_floor(operand: Eterm, up: bool) -> Result<Eterm> {
    match super::classify(operand) {
        TermClass::Small | TermClass::Big => Ok(operand),
        TermClass::Float => {
            // SAFETY: classify() established a live float block.
            let v = unsafe { float_value(operand) };
            float_to_small(
                if up { v.ceil() } else { v.floor() },
                if up { "ceil" } else { "floor" },
            )
        }
        _ => Err(badarg()),
    }
}

/// The shared `f64` → small-integer conversion of `ceil/1`, `floor/1`, `trunc/1`
/// and `round/1`.
///
/// A result outside the 60-bit small range (or a non-finite one) is a **bignum**
/// in the oracle, so it is the typed `unsupported_ets` — never a truncated wrong
/// integer.
fn float_to_small(value: f64, what: &str) -> Result<Eterm> {
    // `2^59` is the exclusive bound of the 60-bit small range.
    const BOUND: f64 = 576_460_752_303_423_488.0;
    if !value.is_finite() || value.abs() >= BOUND {
        return Err(unsupported(&format!(
            "match-spec '{what}': the result is not a small integer"
        )));
    }
    // `|value| < 2^59` and `value` is integer-valued, so the cast is exact and in
    // range.
    try_make_small(value as i64).map_err(|_| {
        unsupported(&format!(
            "match-spec '{what}': the result is not a small integer"
        ))
    })
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
    use std::cmp::Ordering::{Greater, Less};
    let is_integer =
        |term: Eterm| matches!(super::classify(term), TermClass::Small | TermClass::Big);
    if !is_integer(lower) || !is_integer(upper) {
        return Err(badarg());
    }
    if !is_integer(value) {
        return Ok(false);
    }
    Ok(cmp_arith(lower, value, atoms) != Greater && cmp_arith(value, upper, atoms) != Less)
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

/// `bnot/1` — the bitwise complement of a small integer (a 60-bit small's
/// complement is always a 60-bit small). A **bignum** operand needs bignum
/// arithmetic and is the typed `unsupported_ets` (never a wrong answer), and any
/// other operand is the oracle's `badarg`.
pub(super) fn bnot(operand: Eterm) -> Result<Eterm> {
    match super::classify(operand) {
        TermClass::Small => Ok(make_small(!signed_value(operand))),
        TermClass::Big => Err(unsupported("match-spec 'bnot': a bignum operand")),
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
