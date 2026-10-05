//! Element-update helpers for `ets:update_counter/3,4` and
//! `ets:update_element/3`.
//!
//! The term-level helpers below are shared by the `Interior` trait defaults (in
//! the parent module) and the hash interior's overriding inherent methods, so the
//! counter arithmetic and the tuple-rebuild logic exist in exactly one place.

use beam_common::{BeamError, Result};
use beam_term::prelude::*;

use crate::error::badarg;
use crate::key::object_elements;

/// One decoded `ets:update_counter` operation.
pub(super) struct CounterOp {
    /// The 1-based element position (already checked to be `>= 1`; the
    /// `keypos`/arity rules are checked against the object by the caller).
    pub(super) pos: usize,
    /// The increment: a small integer (`erl` accepts a bignum too; that is the
    /// documented representation gap).
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
/// element is not an integer; a **bignum** element is a valid `ets` counter but is
/// the documented representation gap ([`BeamError::SystemLimit`]).
pub(super) fn read_counter(object: Eterm, pos: usize) -> Result<Eterm> {
    let slot = counter_slot(object, pos)?;
    // SAFETY: `counter_slot` established that `slot` addresses element `pos` of
    // the live tuple `object`, so the word is an initialised term.
    let value = unsafe { *slot };
    if is_small(value) {
        Ok(value)
    } else {
        Err(counter_operand_error(value))
    }
}

/// `old + incr` for two small-integer counter operands, or the closest
/// representable failure.
///
/// `badarg` for a non-integer operand (matching `ets:update_counter`). A bignum
/// operand is valid in real ETS but is not addable here, and a sum outside the
/// 60-bit small range (which real ETS promotes to a bignum) is likewise
/// unavailable: both report [`BeamError::SystemLimit`] rather than a wrong value.
pub(crate) fn add_counters(old: Eterm, incr: Eterm) -> Result<Eterm> {
    if !is_small(incr) {
        return Err(counter_operand_error(incr));
    }
    if !is_small(old) {
        return Err(counter_operand_error(old));
    }
    // Widen to `i128` so the 60-bit sum cannot wrap before it is range-checked.
    let sum = signed_value(old) as i128 + signed_value(incr) as i128;
    if sum < MIN_SMALL as i128 || sum > MAX_SMALL as i128 {
        // Real ETS promotes the sum to a bignum; this crate has no bignum support.
        return Err(BeamError::SystemLimit);
    }
    Ok(make_small(sum as i64))
}

/// A counter operand that must be an integer: a small integer is returned as-is,
/// a bignum is the representation gap, anything else is `badarg`.
fn counter_operand(operand: Eterm) -> Result<Eterm> {
    if is_small(operand) {
        Ok(operand)
    } else {
        Err(counter_operand_error(operand))
    }
}

/// The error for a counter operand that is not a small integer: `badarg` unless it
/// is a bignum (a valid ETS operand this crate cannot process), which is a
/// representation-limit gap.
pub(crate) fn counter_operand_error(x: Eterm) -> BeamError {
    // SAFETY: `x` is a live term (a stored element or a caller-supplied operand),
    // so classifying it through its header word is well defined.
    if unsafe { is_integer(x) } {
        BeamError::SystemLimit
    } else {
        badarg()
    }
}

/// Whether the `{Pos, Incr, Threshold, SetValue}` threshold rule fires for
/// `result = old + incr`: a non-negative increment that pushed the counter
/// **above** `threshold`, or a negative one that dropped it **below**.
///
/// Every operand is a validated small integer, so the comparison is exact.
pub(super) fn warp_fires(incr: Eterm, result: Eterm, threshold: Eterm) -> bool {
    if signed_value(incr) < 0 {
        signed_value(result) < signed_value(threshold)
    } else {
        signed_value(result) > signed_value(threshold)
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
/// `erl` requires `Pos` to be a **small** integer and `Incr` / `Threshold` /
/// `SetValue` to be integers; anything else is `badarg` (a bignum is the
/// representation gap, reported from [`counter_operand`]).
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
