//! Term inspection and boxed sub-term materialization over [`beam_alloc::Heap`].
//!
//! This is the **only** module in the crate allowed to touch raw heap words: the
//! crate-wide `#![deny(unsafe_code)]` is lifted for it alone (`#[allow(unsafe_code)]`
//! on the `mod term;` declaration in `lib.rs`), so the crate's entire `unsafe`
//! surface is one auditable file. `beam_term` marks its accessors
//! (`is_tuple`, `tuple_element`, `float_val`, `big_val`, …) as `unsafe fn` because
//! the validity of an `Eterm` is a caller invariant and there is no safe way to
//! read or write a heap word; the functions here wrap those accessors and turn a
//! violated invariant into `BeamError::BadArg` instead of undefined behaviour.
//!
//! Every layout below is pinned line-by-line to the legacy C ground truth
//! `erts/emulator/beam/erl_term.h` and `erts/emulator/beam/erl_map.h` (the C is
//! read, never modified).
//!
//! This module is **public but not part of the cross-crate contract**: the
//! `beam_term` API exposes the layout *predicates* but not the *constructors* for
//! floats, bignums, flatmaps and the external identifiers. Those belong in
//! `beam_term` (the lowest common ancestor for term operations, and the crate
//! that owns the frozen layout); when they land there this module collapses into
//! thin forwarders — see the crate `CONTEXT.md`, "Known Issues".

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::Eterm;

/// Materializes a float term on `heap`.
///
/// Layout pinned to `erl_term.h:479` (`HEADER_FLONUM = make_header(1, FLOAT_SUBTAG)`)
/// and `erl_term.h:506` (`PUT_DOUBLE`): the two words `[HEADER_FLONUM, f64 bits]`,
/// with the root word `make_boxed(hp)` pointing at the first of them.
pub fn make_float(heap: &mut Heap, value: f64) -> Result<Eterm> {
    unimplemented!()
}

/// Reads the value of a float term.
///
/// Inverse of [`make_float`]: yields the `f64` stored in the word following the
/// float header (`erl_term.h:479`, `erl_term.h:506`). A term that is not a float
/// yields `BeamError::BadArg`.
pub fn float_value(term: Eterm) -> Result<f64> {
    unimplemented!()
}

/// Materializes a bignum term on `heap`.
///
/// `digits` are base-256 magnitude digits, **least significant first**;
/// `negative` sets the header's `_BIG_SIGN_BIT`. Layout pinned to
/// `erl_term.h:452-471`: `[make_pos_bignum_header(n-1) | make_neg_bignum_header(n-1),
/// d0 … d(n-1)]`, with the root word `make_boxed(hp)`. More than `BIG_ARITY_MAX`
/// (`(1 << 16) - 1` on 64-bit) digits yields `BeamError::SystemLimit`.
pub fn make_big(heap: &mut Heap, negative: bool, digits: &[u8]) -> Result<Eterm> {
    unimplemented!()
}

/// Splits a bignum term into `(negative, digits)`.
///
/// `digits` are base-256 magnitude digits, least significant first, borrowed from
/// the term's storage (the caller must keep the owning heap alive) — the exact
/// inverse of [`make_big`] (`erl_term.h:452-471`; the sign lives in the header's
/// `_BIG_SIGN_BIT`). A term that is not a bignum yields `BeamError::BadArg`.
pub fn big_parts<'a>(term: Eterm) -> Result<(bool, &'a [u8])> {
    unimplemented!()
}

/// Materializes a small flat map on `heap`.
///
/// Layout pinned to `erl_map.h:181-199` (`MAP_HEADER_FLATMAP = 0x12C`, i.e.
/// `_make_header(4, MAP_SUBTAG)`) and `erl_map.c:673-700`: the word block
/// `[arityval(n), k0 … k(n-1), MAP_HEADER_FLATMAP, n, keys, v0 … v(n-1)]`, where
/// `keys = make_tuple(hp)` is emitted **before** the head, the flatmap head sits
/// at word `1 + n`, the `size` field is the raw integer `n`, and the root is
/// `make_boxed(hp + 1 + n)`. `n == 0` is the 3-word block
/// `[MAP_HEADER_FLATMAP, 0, tuple0()]`.
///
/// More than `MAP_SMALL_MAP_LIMIT` (32) pairs needs the HAMT builder and yields
/// [`crate::error::unsupported`].
pub fn make_flatmap(heap: &mut Heap, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    unimplemented!()
}

/// Splits a flat map into its `(keys, values)`.
///
/// Both slices are borrowed from the term's storage (the caller must keep the
/// owning heap alive) and have the same length, in wire order — the exact inverse
/// of [`make_flatmap`] (`erl_map.h:181-199`). A term that is not a *flat* map (or
/// is a hashmap) yields `BeamError::BadArg`.
pub fn flatmap_parts<'a>(term: Eterm) -> Result<(&'a [Eterm], &'a [Eterm])> {
    unimplemented!()
}

/// Materializes an external pid on `heap`.
///
/// Layout pinned to `erl_term.h:1160-1230` (`ExternalThing`): the header
/// `make_external_pid_header(size)` (`= make_header(size - 1, pid_subtag)`)
/// followed by `[node, number, serial, creation]`, with the root word
/// `make_boxed(hp)`. `node` is the node-name atom term.
pub fn make_external_pid(
    heap: &mut Heap,
    node: Eterm,
    number: u32,
    serial: u32,
    creation: u32,
) -> Result<Eterm> {
    unimplemented!()
}

/// Splits an external pid term into `(node, number, serial, creation)`.
///
/// The exact inverse of [`make_external_pid`] (`erl_term.h:1160-1230`). A term
/// that is not an external pid yields `BeamError::BadArg`.
pub fn external_pid_parts(term: Eterm) -> Result<(Eterm, u32, u32, u32)> {
    unimplemented!()
}

/// Materializes an external port on `heap`.
///
/// Layout pinned to `erl_term.h:1160-1230` (`ExternalThing`): the header
/// `make_external_port_header(size)` followed by `[node, number, creation]`, with
/// the root word `make_boxed(hp)`. `number` is the 64-bit `V4_PORT_EXT` id.
pub fn make_external_port(
    heap: &mut Heap,
    node: Eterm,
    number: u64,
    creation: u32,
) -> Result<Eterm> {
    unimplemented!()
}

/// Splits an external port term into `(node, number, creation)`.
///
/// The exact inverse of [`make_external_port`] (`erl_term.h:1160-1230`). A term
/// that is not an external port yields `BeamError::BadArg`.
pub fn external_port_parts(term: Eterm) -> Result<(Eterm, u64, u32)> {
    unimplemented!()
}

/// Materializes an external reference on `heap`.
///
/// Layout pinned to `erl_term.h:1160-1230` (`ExternalThing`): the header
/// `make_external_ref_header(size)` followed by `[node, id0 … id(n-1), creation]`,
/// with the root word `make_boxed(hp)`. `ids` are the 32-bit `NEWER_REFERENCE_EXT`
/// identifier words (three on a 64-bit node). An empty or over-long `ids` slice
/// yields `BeamError::SystemLimit`.
pub fn make_external_ref(
    heap: &mut Heap,
    node: Eterm,
    ids: &[u32],
    creation: u32,
) -> Result<Eterm> {
    unimplemented!()
}

/// Splits an external reference term into `(node, ids, creation)`.
///
/// `ids` is borrowed from the term's storage (the caller must keep the owning heap
/// alive) — the exact inverse of [`make_external_ref`] (`erl_term.h:1160-1230`).
/// A term that is not an external reference yields `BeamError::BadArg`.
pub fn external_ref_parts<'a>(term: Eterm) -> Result<(Eterm, &'a [u32], u32)> {
    unimplemented!()
}

/// The arity of a tuple term.
///
/// Reads the tuple's arity word — `arityval(hdr)` / `thing_arityval(hdr)`
/// (`erl_term.h`, the `TAG_PRIMARY_HEADER` arity in the header's high bits). A
/// term that is not a tuple yields `BeamError::BadArg`.
pub fn tuple_arity(term: Eterm) -> Result<usize> {
    unimplemented!()
}

/// The element at `index` of a tuple term, zero-based.
///
/// Index `i` is the heap word `1 + i` after the tuple's arity header
/// (`erl_term.h`, the tuple `thing` layout). An out-of-range index or a term that
/// is not a tuple yields `BeamError::BadArg`.
pub fn tuple_element(term: Eterm, index: usize) -> Result<Eterm> {
    unimplemented!()
}

/// The head (`CAR`) of a cons cell.
///
/// The head is the word at `CAR_OFFSET` (0) of the header-less 2-word cons cell
/// tagged `TAG_PRIMARY_LIST` (`erl_term.h`; `NIL` is an immediate, not a cons
/// cell, and yields `BeamError::BadArg`).
pub fn cons_head(term: Eterm) -> Result<Eterm> {
    unimplemented!()
}

/// The tail (`CDR`) of a cons cell.
///
/// The tail is the word at `CDR_OFFSET` (1) of the header-less 2-word cons cell
/// tagged `TAG_PRIMARY_LIST` (`erl_term.h`; `NIL` is an immediate, not a cons
/// cell, and yields `BeamError::BadArg`).
pub fn cons_tail(term: Eterm) -> Result<Eterm> {
    unimplemented!()
}
