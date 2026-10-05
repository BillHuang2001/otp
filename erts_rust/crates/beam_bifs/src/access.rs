//! Safe wrappers over `beam_term`'s raw (unsafe) term accessors.
//!
//! This is the crate's **only** `#[allow(unsafe_code)]` module (the
//! `beam_engine::predicates` / `beam_codec::term` convention). `beam_term`'s
//! boxed-term predicates and structure accessors are `unsafe fn` because each
//! dereferences the header word of a boxed term — sound only when the term is a
//! *live* BEAM term. Every BIF module classifies and destructures its arguments
//! through these wrappers and stays safe Rust; no other module in this crate uses
//! `unsafe`.
//!
//! # Safety invariant
//!
//! Every wrapped term is a **live BEAM term**: a BIF argument taken from the
//! machine's registers/stack, or a term built on a live heap — never unvalidated
//! raw memory. The *immediate* fast paths (small ints, atoms, `NIL`, internal
//! pids/ports) are delegated to the safe `beam_term` predicates
//! ([`beam_term::is_small`], [`beam_term::is_atom`], …); the boxed wrappers only
//! dereference a header after `beam_term` has confirmed the word is boxed.
#![allow(unsafe_code)]

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_term::prelude::{boxed_val, make_boxed, make_header, make_small, signed_value, try_make_small};
use beam_term::{Eterm, HEADER_FLONUM, REF_SUBTAG};

// ---------------------------------------------------------------------------
// Classification — safe pass-throughs of the `beam_term` immediate predicates
// ---------------------------------------------------------------------------

/// `is_atom(x)`.
#[inline]
pub(crate) fn is_atom(x: Eterm) -> bool {
    beam_term::is_atom(x)
}

/// `is_small(x)` — a 60-bit small integer.
#[inline]
pub(crate) fn is_small(x: Eterm) -> bool {
    beam_term::is_small(x)
}

/// `is_nil(x)` — the empty list `[]`.
#[inline]
pub(crate) fn is_nil(x: Eterm) -> bool {
    beam_term::is_nil(x)
}

/// `is_list(x)` — a cons cell (`false` for `NIL`).
#[inline]
pub(crate) fn is_list(x: Eterm) -> bool {
    beam_term::is_list(x)
}

/// `is_internal_pid(x)`.
#[inline]
pub(crate) fn is_internal_pid(x: Eterm) -> bool {
    beam_term::is_internal_pid(x)
}

/// `is_internal_port(x)`.
#[inline]
pub(crate) fn is_internal_port(x: Eterm) -> bool {
    beam_term::is_internal_port(x)
}

// ---------------------------------------------------------------------------
// Classification — boxed-term predicates (each dereferences a header word)
// ---------------------------------------------------------------------------

/// `is_tuple(x)`.
#[inline]
pub(crate) fn is_tuple(x: Eterm) -> bool {
    // SAFETY: `x` is a live machine word; `is_tuple` checks `is_boxed` before
    // dereferencing the header.
    unsafe { beam_term::is_tuple(x) }
}

/// `is_float(x)` — a boxed float (`HEADER_FLONUM`).
#[inline]
pub(crate) fn is_float(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_float(x) }
}

/// `is_map(x)`.
#[inline]
pub(crate) fn is_map(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_map(x) }
}

/// `is_big(x)` — a bignum.
// A documented part of the `access.rs` surface reserved for dependents: M1 builds no
// bignums, so no BIF dispatches on it yet.
#[allow(dead_code)]
#[inline]
pub(crate) fn is_big(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_big(x) }
}

/// `is_bitstring(x)`.
#[inline]
pub(crate) fn is_bitstring(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_bitstring(x) }
}

/// `is_binary(x)` — a bitstring whose size is a whole number of bytes.
#[inline]
pub(crate) fn is_binary(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_binary(x) }
}

/// `is_function(x)` — a fun/lambda (`is_any_fun`).
#[inline]
pub(crate) fn is_function(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_any_fun(x) }
}

/// `is_record(x)` — a native record.
// A documented part of the `access.rs` surface reserved for dependents: M1 builds no
// native records, so no BIF dispatches on it yet.
#[allow(dead_code)]
#[inline]
pub(crate) fn is_record(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_record(x) }
}

/// `is_internal_ref(x)` — an on-heap reference.
#[inline]
pub(crate) fn is_internal_ref(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_internal_ref(x) }
}

/// `is_external_pid(x)`.
#[inline]
pub(crate) fn is_external_pid(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_external_pid(x) }
}

/// `is_external_port(x)`.
#[inline]
pub(crate) fn is_external_port(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_external_port(x) }
}

/// `is_external_ref(x)`.
#[inline]
pub(crate) fn is_external_ref(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`].
    unsafe { beam_term::is_external_ref(x) }
}

/// `is_integer(x)` — a small integer or a bignum.
#[inline]
pub(crate) fn is_integer(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`]; the immediate fast path never dereferences.
    unsafe { beam_term::is_integer(x) }
}

/// `is_number(x)` — an integer or a float.
#[inline]
pub(crate) fn is_number(x: Eterm) -> bool {
    // SAFETY: as [`is_tuple`]; the immediate fast path never dereferences.
    unsafe { beam_term::is_number(x) }
}

/// `is_pid(x)` — an internal or external pid.
#[inline]
pub(crate) fn is_pid(x: Eterm) -> bool {
    is_internal_pid(x) || is_external_pid(x)
}

/// `is_port(x)` — an internal or external port.
#[inline]
pub(crate) fn is_port(x: Eterm) -> bool {
    is_internal_port(x) || is_external_port(x)
}

/// `is_reference(x)` — an internal or external reference.
#[inline]
pub(crate) fn is_reference(x: Eterm) -> bool {
    is_internal_ref(x) || is_external_ref(x)
}

// ---------------------------------------------------------------------------
// Structure (each dereferences the boxed word)
// ---------------------------------------------------------------------------

/// `tuple_size(x)` — the arity of a tuple.
#[inline]
pub(crate) fn tuple_size(x: Eterm) -> usize {
    // SAFETY: `x` is a live tuple whose header word is valid (caller invariant;
    // the BIF modules call this only after [`is_tuple`] returned true).
    unsafe { beam_term::tuple_size(x) }
}

/// `tuple_element(x, i)` — element `i` (0-based, unchecked) of a tuple.
#[inline]
pub(crate) fn tuple_element(x: Eterm, i: usize) -> Eterm {
    // SAFETY: as [`tuple_size`]; `i` must be `< tuple_size(x)` (caller invariant).
    unsafe { beam_term::tuple_element(x, i) }
}

/// `CAR(x)` — the head word of a cons cell.
#[inline]
pub(crate) fn car(x: Eterm) -> Eterm {
    // SAFETY: `x` is a live non-`NIL` list term (caller invariant; the BIF modules
    // call this only after [`is_list`] returned true).
    unsafe { beam_term::car(x) }
}

/// `CDR(x)` — the tail word of a cons cell.
#[inline]
pub(crate) fn cdr(x: Eterm) -> Eterm {
    // SAFETY: as [`car`].
    unsafe { beam_term::cdr(x) }
}

// ---------------------------------------------------------------------------
// Floats — the frozen `[HEADER_FLONUM, <bits>]` thing (see `beam_term::size`)
// ---------------------------------------------------------------------------

/// Builds a boxed 64-bit float on `heap`: `[HEADER_FLONUM, <bits as a raw word>]`.
///
/// # Errors
///
/// Propagates [`BeamError::Alloc`] when `heap` cannot fit the two words.
pub(crate) fn make_float(heap: &mut Heap, v: f64) -> Result<Eterm> {
    // SAFETY: `alloc_words` hands out two exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(2)? };
    // SAFETY: `p` addresses the two words just handed out; both are written before
    // the pointer escapes this block.
    unsafe {
        *p = HEADER_FLONUM;
        *p.add(1) = Eterm::from_raw(v.to_bits() as usize);
    }
    Ok(make_boxed(p as *const Eterm))
}

/// The IEEE-754 bit pattern of a boxed float.
#[inline]
pub(crate) fn float_bits(x: Eterm) -> u64 {
    // SAFETY: `x` is a live boxed float (`is_float`); its payload word at offset 1
    // holds the IEEE-754 bits (the `beam_term::size` layout).
    unsafe { (*boxed_val(x).add(1)).raw() as u64 }
}

/// The value of a boxed float.
#[inline]
pub(crate) fn float_value(x: Eterm) -> f64 {
    f64::from_bits(float_bits(x))
}

// ---------------------------------------------------------------------------
// The M1 reference thing (used for the ETS table handle)
// ---------------------------------------------------------------------------

/// The number of payload words of the ref thing: `node`, `serial` and `id`.
const REF_PAYLOAD_WORDS: usize = 3;

/// Builds the frozen 4-word ref thing `[make_header(3, REF_SUBTAG), 0, 0, id]`.
///
/// `beam_term` has no `make_ref` in Milestone-1, so this mirrors
/// `beam_process::MonitorRef::to_eterm`; it is the M1 table handle
/// [`crate::ets::TableRegistry`] keys on (see the crate `CONTEXT.md`, Design
/// Decisions / Known Issues).
///
/// # Errors
///
/// [`BeamError::SystemLimit`] when `id` does not fit a 60-bit small integer;
/// otherwise propagates [`BeamError::Alloc`].
pub(crate) fn make_ref_thing(heap: &mut Heap, id: u64) -> Result<Eterm> {
    let raw = i64::try_from(id).map_err(|_| BeamError::SystemLimit)?;
    let id_term = try_make_small(raw).map_err(|_| BeamError::SystemLimit)?;
    // SAFETY: `alloc_words` hands out four exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(REF_PAYLOAD_WORDS + 1)? };
    // SAFETY: `p` addresses the four words just handed out; every one of them is
    // written before the pointer escapes this block.
    unsafe {
        *p = make_header(REF_PAYLOAD_WORDS, REF_SUBTAG);
        *p.add(1) = make_small(0); // node: the local node
        *p.add(2) = make_small(0); // serial: one id space in M1
        *p.add(3) = id_term; // id
    }
    Ok(make_boxed(p as *const Eterm))
}

/// The numeric id encoded in the M1 ref thing [`make_ref_thing`] builds.
///
/// This is the read-back half of the ETS table handle: `erts_rust/crates/beam_bifs`
/// builds the handle with [`make_ref_thing`] and recovers its id here (see
/// `crate::ets`).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not an internal-ref thing, or when its id
/// word is not a non-negative small integer. The caller treats that as the `ets`
/// `badarg` for an invalid handle.
pub(crate) fn ref_thing_id(term: Eterm) -> Result<u64> {
    if !is_internal_ref(term) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: [`is_internal_ref`] confirmed `term` is a boxed term whose header
    // carries `REF_SUBTAG`. [`make_ref_thing`] is this crate's only producer of
    // such a term and always writes a 4-word `[header, node, serial, id]` block,
    // so word index `REF_PAYLOAD_WORDS` (the id) is in bounds and holds a small
    // integer — never unvalidated memory.
    let id_word = unsafe { *boxed_val(term).add(REF_PAYLOAD_WORDS) };
    if !is_small(id_word) {
        return Err(BeamError::BadArg);
    }
    let id = signed_value(id_word);
    u64::try_from(id).map_err(|_| BeamError::BadArg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{is_boxed, make_small, NIL};

    fn heap() -> Heap {
        Heap::new(32).expect("test heap")
    }

    #[test]
    fn float_round_trips_through_its_bits() {
        let mut heap = heap();
        for value in [0.0f64, 1.0, -2.5, 1.5, f64::MAX, f64::MIN_POSITIVE] {
            let term = make_float(&mut heap, value).expect("the float fits");
            assert!(is_float(term), "not a float: {value}");
            assert_eq!(float_bits(term), value.to_bits());
            assert_eq!(float_value(term), value);
            assert!(is_boxed(term));
        }
        assert!(!is_float(make_small(7)));
    }

    #[test]
    fn tuple_size_and_elements() {
        let mut heap = heap();
        let tuple = heap
            .tuple(&[make_small(10), make_small(20), make_small(30)])
            .expect("the tuple fits");
        assert!(is_tuple(tuple));
        assert_eq!(tuple_size(tuple), 3);
        assert_eq!(tuple_element(tuple, 0), make_small(10));
        assert_eq!(tuple_element(tuple, 2), make_small(30));
        assert!(!is_tuple(make_small(1)));
    }

    #[test]
    fn list_car_and_cdr() {
        let mut heap = heap();
        let list = heap.cons(make_small(1), NIL).expect("the cons fits");
        assert!(is_list(list));
        assert_eq!(car(list), make_small(1));
        assert_eq!(cdr(list), NIL);
    }

    #[test]
    fn ref_thing_is_a_boxed_internal_ref() {
        let mut heap = heap();
        let thing = make_ref_thing(&mut heap, 42).expect("the ref fits");
        assert!(is_boxed(thing));
        assert!(is_internal_ref(thing));
    }

    #[test]
    fn ref_thing_rejects_an_id_beyond_the_small_range() {
        let mut heap = heap();
        assert!(matches!(
            make_ref_thing(&mut heap, u64::MAX),
            Err(BeamError::SystemLimit)
        ));
        assert_eq!(heap.used(), 0, "a rejected id allocates nothing");
    }

    #[test]
    fn composed_predicates() {
        assert!(is_pid(ProcessId::new(1, 0).to_eterm()));
        assert!(is_port(beam_term::make_internal_port(
            beam_term::make_port_data(3)
        )));
        assert!(!is_pid(make_small(1)));
        assert!(!is_port(NIL));
        assert!(!is_reference(NIL));
        assert!(is_number(make_small(1)));
        assert!(is_integer(make_small(1)));
        assert!(is_atom(beam_term::make_atom(beam_term::Atom::from_raw(0))));
    }
}
