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
use beam_term::prelude::{
    boxed_val, is_arity_value, is_boxed_term, make_boxed, make_header, make_small, signed_value,
    try_make_small,
};
use beam_term::{
    Atom, AtomTable, Eterm, ERL_HEAP_BITS_SIZE, HAMT_SUBTAG_HEAD_ARRAY,
    HAMT_SUBTAG_HEAD_BITMAP, HAMT_SUBTAG_NODE_BITMAP, HEADER_ARITY_OFFS, HEADER_FLONUM,
    HEADER_MAP_SUBTAG_MASK, HEAP_BITS_SUBTAG, REF_SUBTAG,
};

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

/// `FUN_HEADER_ARITY_OFFS` (`erl_term.h:413`): `_HEADER_ARITY_OFFS + 2`.
const FUN_HEADER_ARITY_OFFS: u32 = HEADER_ARITY_OFFS + 2;

/// The width of the fun header's `arity`/`environment` fields — `erl_fun.h`'s
/// "the arity and number of free variables are both limited to 255"
/// (`erl_term.h:403`).
const FUN_FIELD_MAX: usize = 0xFF;

/// The `arity` field of a fun term's header, or `None` when `x` is not a fun.
///
/// `MAKE_FUN_HEADER(Arity, NumFree, External)` (`erl_term.h:417-424`) packs the
/// arity at bit `FUN_HEADER_ARITY_OFFS` (8) and the environment size at bit 16,
/// eight bits each; `TAG_PRIMARY_HEADER` is zero, so the header word is the pure
/// bit field. `erlang:is_function/2` (`crate::type_test`) is the only caller.
#[inline]
pub(crate) fn fun_arity(x: Eterm) -> Option<u32> {
    if !is_function(x) {
        return None;
    }
    // SAFETY: [`is_function`] (`is_any_fun`) established that `x` is boxed with a
    // `FUN_SUBTAG` header, so its header word is a live, initialised term word.
    let header = unsafe { (*boxed_val(x)).raw() };
    Some(((header >> FUN_HEADER_ARITY_OFFS) & FUN_FIELD_MAX) as u32)
}

/// The **true** arity of a fun term — the arity the C `FUN_DEF` printer emits
/// (`erl_printf_term.c:721-723`), i.e. what `erlang:fun_to_list/1` renders.
///
/// For a [`beam_term::fun::FunKind::External`] fun this is descriptor word 3,
/// the inlined `Export`'s `info.mfa.arity`, which may exceed the header's 8-bit
/// field (an `EXPORT_EXT` fun with true arity 256 renders `fun lists:reverse/256`
/// — oracle-verified). For a local/imported fun it is the header's 8-bit field,
/// i.e. the same value [`fun_arity`] returns.
///
/// This is **not** what `erlang:fun_info/2`'s `arity` item or
/// `erlang:fun_info_mfa/1` report: those read the header's 8-bit field
/// (`beam_codec::term::fun_arity_term`, the C `fun_arity(funp)`), so true arity
/// 256 reports `0` and true arity 300 reports `44`.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `x` is not a fun.
#[inline]
pub(crate) fn fun_true_arity(x: Eterm) -> Result<usize> {
    if !is_function(x) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: [`is_function`] (`is_any_fun`) established that `x` is a live boxed
    // fun, so its whole block is live — the precondition of `fun_true_arity`; the
    // only error it can return (`TermError::BadFun`) is excluded by that guard.
    Ok(unsafe { beam_term::fun::fun_true_arity(x)? })
}

/// `is_record(x)` — a native record (`RECORD_SUBTAG`).
///
/// This is the C `is_record` macro (`erl_term.h:437`), **not** a tuple test: a
/// tuple carries the `ARITYVAL` subtag, so `is_record({a,b})` is `false` — exactly
/// what `erlang:is_record/1` documents.
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

/// `true` iff `a`/`b` are an **internal/external pair of the same category**: a pid
/// against an external pid, a port against an external port, or a reference against
/// an external reference — in either operand order.
///
/// These are the *only* pairs whose ordering a local node changes.
/// [`beam_term::compare::cmp_with_local_node`] /
/// [`beam_term::compare::cmp_arith_with_local_node`] differ from the context-free
/// [`beam_term::compare::cmp`] / [`beam_term::compare::cmp_arith`] exactly on the
/// mixed arms of `erts_cmp_compound` (`utils.c:1751-1811`, `:2219-2308`), where the
/// runtime's own node is substituted for the internal operand's *implicit* node;
/// every other pair is ranked/field-walked identically. So this is the cheap
/// pre-test that decides whether a caller must pay for the local node at all
/// (`crate::compare::local_node_id`, which costs an atom-table lookup or a hook
/// call) — and 99.99% of comparisons can never be a mixed identity pair.
///
/// The category must match: an internal pid against an external *port* is not a
/// mixed pair (they are different term classes, decided by the class ladder as
/// before).
///
/// # Caller invariant
///
/// As [`is_tuple`]: if a word is a boxed term, its header word must be a live,
/// aligned term word. A word that is not a term at all (a bare header word, e.g.
/// the ABI's `THE_NON_VALUE`) is `false` from every predicate below — never a
/// dereference — so it is `false` here too, which is exactly what a caller wants
/// (such a word is not an identity pair).
#[inline]
pub(crate) fn is_mixed_identity_pair(a: Eterm, b: Eterm) -> bool {
    (is_internal_pid(a) && is_external_pid(b))
        || (is_external_pid(a) && is_internal_pid(b))
        || (is_internal_port(a) && is_external_port(b))
        || (is_external_port(a) && is_internal_port(b))
        || (is_internal_ref(a) && is_external_ref(b))
        || (is_external_ref(a) && is_internal_ref(b))
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

// ---------------------------------------------------------------------------
// Heap bitstrings (`ErlHeapBits`, `erl_bits.h`)
//
// The items below are a documented part of the `access.rs` surface reserved for
// dependents: `crate::bits` is their only caller, and `erlang:size/1`
// (`crate::term_ops`) is the only BIF wired to it in M1 — the `binary`/`unicode` BIFs
// that read the bytes back land later, so the unused-tail items carry
// `#[allow(dead_code)]` (the same convention as [`is_big`] above).
// ---------------------------------------------------------------------------

/// `ERL_HEAP_BITS_SIZE` — the number of words *before* the data area of an on-heap
/// bitstring (`[thing_word, size, data…]`).
const HEAP_BITS_PREFIX_WORDS: usize = ERL_HEAP_BITS_SIZE;

/// The total words a heap bitstring of `bit_len` bits occupies.
///
/// `erl_bits.h:heap_bits_size(bits)` is `sizeof(ErlHeapBits)/sizeof(Eterm) - 1 +
/// ceil(NBYTES(bits) / sizeof(Eterm))` = two prefix words plus `ceil(bits / 64)`
/// data words.
#[allow(dead_code)]
fn heap_bits_words(bit_len: usize) -> usize {
    HEAP_BITS_PREFIX_WORDS + bit_len.div_ceil(8 * size_of::<Eterm>())
}

/// Builds the on-heap bitstring `<<bytes:bit_len>>` on `heap`.
///
/// The block is the frozen `ErlHeapBits` of `erl_bits.h`:
/// `[make_header(1 + data_words, HEAP_BITS_SUBTAG), bit_len, data…]`, where
/// `data_words = ceil(bit_len / 64)`. The bits are the first `bit_len` bits of
/// `bytes`, packed **MSB-first** in address order (`erl_bits.h` addresses the data
/// area as a byte array: `Base = (byte*)&_hb->data[0]`, so a byte-aligned binary's
/// bytes are stored in order). The unused bytes of the final data word are zeroed,
/// so the layout is fully deterministic and endianness-agnostic.
///
/// Unlike C, no 64-byte on-heap limit is enforced: Milestone-1 keeps *every* binary
/// on the process heap (see `crate::bits`).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `bit_len > bytes.len() * 8`; otherwise propagates
/// [`BeamError::Alloc`](beam_common::BeamError::Alloc) when `heap` cannot fit the
/// block.
#[allow(dead_code)]
pub(crate) fn make_heap_bitstring(heap: &mut Heap, bytes: &[u8], bit_len: usize) -> Result<Eterm> {
    // `saturating_mul` can only saturate for a slice no allocator could ever hand
    // out (`len > usize::MAX / 8`), where every `bit_len` still fits — so the check
    // below stays exact without a wrapping multiply.
    if bit_len > bytes.len().saturating_mul(8) {
        return Err(BeamError::BadArg);
    }
    let data_words = bit_len.div_ceil(8 * size_of::<Eterm>());
    // SAFETY: `alloc_words` hands out `2 + data_words` exclusively-owned, writable
    // words.
    let p = unsafe { heap.alloc_words(heap_bits_words(bit_len))? };
    // SAFETY: `p` addresses the `2 + data_words` words just handed out; the header
    // word, the bit-size word and every data byte are written before the pointer
    // escapes this block.
    unsafe {
        *p = make_header(1 + data_words, HEAP_BITS_SUBTAG);
        *p.add(1) = Eterm::from_raw(bit_len);
        let data = core::slice::from_raw_parts_mut(
            p.add(HEAP_BITS_PREFIX_WORDS).cast::<u8>(),
            data_words * size_of::<Eterm>(),
        );
        let nbytes = bit_len.div_ceil(8);
        data[..nbytes].copy_from_slice(&bytes[..nbytes]);
        data[nbytes..].fill(0);
    }
    Ok(make_boxed(p as *const Eterm))
}

/// The bit size of a bitstring (heap or sub bits).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a bitstring.
pub(crate) fn bitstring_bit_size(term: Eterm) -> Result<usize> {
    if !is_bitstring(term) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: [`is_bitstring`] confirmed `term` is a live heap- or sub-bitstring
    // whose backing thing is valid for `ERL_SUB_BITS_SIZE`/`ERL_HEAP_BITS_SIZE`
    // words — never unvalidated memory.
    Ok(unsafe { beam_term::bitstring_size(term) })
}

/// Copies the bytes of a byte-aligned bitstring (a *binary*) out of the term.
///
/// **Both** frozen bitstring representations are handled, through the LCA reader
/// [`beam_term::bitstring::bitstring_parts`]:
///
/// * an **on-heap** `ErlHeapBits` (`[thing_word, n_bits, data…]`), whose base is
///   `boxed_val(term) + 2` with a zero bit offset — the bytes are stored in address
///   order (`erl_bits.h`: the data area is byte-addressed);
/// * an **off-heap** `ErlSubBits` window (a `[BinRef][ErlSubBits]` refc binary, e.g.
///   built by `beam_engine`'s `bs_create_bin` for any payload above 64 bytes, or a
///   match context), whose base is `base_flags & !ERL_SUB_BITS_FLAG_MASK` and whose
///   start is a **bit** offset into that storage.
///
/// When that window starts on a byte boundary the answer is the contiguous slice
/// `base + offset_bits / 8`; when it starts mid-byte there is no contiguous byte
/// image, so the bits are re-packed MSB-first by [`bitstring_bits`] — never a
/// guessed image.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a byte-aligned bitstring.
#[allow(dead_code)]
pub(crate) fn binary_bytes(term: Eterm) -> Result<Vec<u8>> {
    if !is_binary(term) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: [`is_binary`] confirmed `term` is a live, byte-aligned bitstring (heap
    // or sub), so `bitstring_parts` reads only that term's own words and returns a
    // base pointer into storage backing bits `offset_bits..offset_bits + size_bits`
    // — kept alive by `term` for the whole of the copy below.
    let (base, offset_bits, size_bits) = unsafe { beam_term::bitstring::bitstring_parts(term) };
    let nbytes = size_bits / 8;
    if offset_bits & 7 != 0 {
        // A mid-byte window has no contiguous byte image: re-pack bit by bit.
        return bitstring_bits(term);
    }
    if nbytes == 0 {
        // No bytes to read — return before touching `base`, which may be null or
        // dangling for a zero-length window (`from_raw_parts` is UB for a zero-length
        // slice built from such a pointer).
        return Ok(Vec::new());
    }
    // SAFETY: `offset_bits` and `size_bits` are both multiples of 8 (`is_binary`), so
    // bits `offset_bits..offset_bits + size_bits` are exactly the `nbytes > 0`
    // initialised bytes at `base + offset_bits / 8`, all inside the live storage the
    // base pointer covers; the copy goes out byte by byte, so it is
    // endianness-agnostic.
    let data = unsafe { base.add(offset_bits / 8) };
    Ok(unsafe { core::slice::from_raw_parts(data, nbytes) }.to_vec())
}

/// Hands the bytes of a byte-aligned bitstring (a *binary*) to `body` **without
/// copying them out of the term**.
///
/// This is the borrowing sibling of [`binary_bytes`]: it accepts exactly the same
/// two frozen representations (an on-heap `ErlHeapBits` and an off-heap
/// `[BinRef][ErlSubBits]` window) and rejects exactly the same arguments, but instead
/// of copying the payload into a new `Vec` it lends a byte slice to `body`.
///
/// The slice's scope is a **closure** rather than a return value because a returned
/// `&[u8]` is not expressible: an `Eterm` is a bare machine word that carries no
/// lifetime (`beam_term::bitstring::bitstring_parts` is an `unsafe` raw-pointer API),
/// and the payload may be an off-heap refc binary whose refcounted ownership the term
/// does not express as a Rust lifetime (see `crate::binconv::bytes_arg`). Bounded by
/// `body` the slice provably cannot outlive the term, and the caller copies it
/// straight into its own destination — disjoint from term storage at every call site,
/// so `body` may write while it reads (it must not, however, write into the very
/// storage the slice borrows).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a byte-aligned bitstring; otherwise
/// whatever `body` returns.
pub(crate) fn with_binary_bytes<R>(
    term: Eterm,
    body: impl FnOnce(&[u8]) -> Result<R>,
) -> Result<R> {
    if !is_binary(term) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: [`is_binary`] confirmed `term` is a live, byte-aligned bitstring (heap
    // or sub), so `bitstring_parts` reads only that term's own words and returns a
    // base pointer into storage backing bits `offset_bits..offset_bits + size_bits`
    // — kept alive by `term` for the whole of the borrow handed to `body`.
    let (base, offset_bits, size_bits) = unsafe { beam_term::bitstring::bitstring_parts(term) };
    let nbytes = size_bits / 8;
    if offset_bits & 7 != 0 {
        // A mid-byte window has no contiguous byte image: re-pack the bits MSB-first
        // into a scratch buffer. This is the one case that allocates (a *plain*
        // binary, which is every binary this crate builds, is always byte-aligned).
        let packed = bitstring_bits(term)?;
        return body(&packed);
    }
    if nbytes == 0 {
        // No bytes to read — hand out an empty slice without touching `base`, which
        // may be null or dangling for a zero-length window (`from_raw_parts` is UB for
        // a zero-length slice built from such a pointer).
        return body(&[]);
    }
    // SAFETY: `offset_bits` and `size_bits` are both multiples of 8 (`is_binary`), so
    // bits `offset_bits..offset_bits + size_bits` are exactly the `nbytes > 0`
    // initialised bytes at `base + offset_bits / 8`, all inside the live storage the
    // base pointer covers. The slice never escapes `body`, which is called inside this
    // block while `term` is still live; the length is in bytes, so it is
    // endianness-agnostic.
    let data = unsafe { base.add(offset_bits / 8) };
    body(unsafe { core::slice::from_raw_parts(data, nbytes) })
}

/// Copies the bit data of **any** bitstring (heap or sub, byte-aligned or not) out
/// of the term.
///
/// Returns `ceil(bit_len / 8)` bytes, packed **MSB-first**: bit `i` of the
/// bitstring (`0 <= i < bit_len`, counted from the most-significant bit of the
/// first byte) is bit `7 - (i % 8)` of byte `i / 8`, exactly the packing
/// [`make_heap_bitstring`] writes and `erl_bits.h` addresses. The bit length is
/// [`bitstring_bit_size`]. This is the read half for the `<<X:L>>` render form
/// (`crate::print`), which splits the bytes into the whole leading bytes plus the
/// trailing `L = bit_len % 8` bits.
///
/// Unlike [`binary_bytes`], this accepts a **non-byte-aligned** bitstring (and an
/// off-heap `SUB_BITS` thing): it reads bit by bit through the frozen
/// `beam_term::bit_at` accessor, so it never follows a `BinRef`/match-context
/// indirection and never touches unvalidated memory.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a bitstring.
pub(crate) fn bitstring_bits(term: Eterm) -> Result<Vec<u8>> {
    let bit_len = bitstring_bit_size(term)?;
    let mut out = vec![0u8; bit_len.div_ceil(8)];
    for i in 0..bit_len {
        // SAFETY: [`bitstring_bit_size`] confirmed `term` is a live bitstring and
        // `i < bit_len` keeps bit `i` within its size, so `bit_at` reads an
        // initialised bit of the (heap- or sub-)bitstring's storage.
        if unsafe { beam_term::bit_at(term, i) } {
            out[i / 8] |= 1u8 << (7 - (i % 8));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Maps (`beam_term::map` owns both layouts: flat map and HAMT hash map)
//
// `is_flatmap`/`is_hashmap`/`make_flatmap`/`flatmap_pairs` are the flat-map half of
// this surface and keep their original shapes for existing callers; the `map_*`/
// `make_map` items below handle **both** representations (the crate's `maps` BIFs
// need a map with more than `MAP_SMALL_MAP_LIMIT` distinct keys to be a real HAMT,
// not a `system_limit`).
// ---------------------------------------------------------------------------

/// `is_flatmap(x)` — a flat map (a small map), as opposed to a hashmap.
#[allow(dead_code)]
#[inline]
pub(crate) fn is_flatmap(x: Eterm) -> bool {
    // SAFETY: `x` is a live machine word; `is_flatmap` checks `is_boxed` before
    // dereferencing the header.
    unsafe { beam_term::is_flatmap(x) }
}

/// `is_hashmap(x)` — a hash map (a HAMT), i.e. a map above `MAP_SMALL_MAP_LIMIT`.
#[inline]
pub(crate) fn is_hashmap(x: Eterm) -> bool {
    // SAFETY: as [`is_flatmap`].
    unsafe { beam_term::is_hashmap(x) }
}

/// Builds a flat map from `(key, value)` pairs on `heap`.
///
/// The layout is owned by `beam_term::thing` (`write_flatmap`); the pair order is
/// preserved and duplicate keys are not detected (that needs term comparison, owned
/// by `beam_term::compare`).
///
/// # Errors
///
/// Propagates [`BeamError::Alloc`](beam_common::BeamError::Alloc) when `heap` cannot
/// fit the block.
#[allow(dead_code)]
pub(crate) fn make_flatmap(heap: &mut Heap, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let words = beam_term::thing::flatmap_size_words(pairs.len());
    // SAFETY: `alloc_words` hands out `words` exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(words)? };
    // SAFETY: `p` is the base of those `words` words; `write_flatmap` writes all of
    // them and returns a boxed pointer into the same block, which stays live (and
    // unmoved) for as long as `heap` does, which the caller keeps alive.
    Ok(unsafe { beam_term::thing::write_flatmap(p, pairs) })
}

/// Copies a flat map's keys and values out of the term (each in wire order).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a flat map.
#[allow(dead_code)]
pub(crate) fn flatmap_pairs(term: Eterm) -> Result<(Vec<Eterm>, Vec<Eterm>)> {
    if !is_flatmap(term) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: [`is_flatmap`] confirmed `term` is a live flat map in the
    // `write_flatmap` layout, so both borrowed slices are live and in bounds; they
    // are copied below, so nothing borrows past this call (the caller's heap stays
    // alive regardless).
    let (keys, values) = unsafe { beam_term::thing::flatmap_parts(term) };
    Ok((keys.to_vec(), values.to_vec()))
}

/// Builds a map from `(key, value)` pairs on `heap`, choosing the representation
/// ERTS does: at most `MAP_SMALL_MAP_LIMIT` (32) **distinct** keys give a flat map
/// (keys stored in `erts_cmp_flatmap_keys` order), more give a HAMT hash map whose
/// stored pair order is the oracle's `maps:to_list/1` order.
///
/// This is the layout-owning [`beam_term::map::write_map`] behind the crate's map
/// producers. The pair order of `pairs` is irrelevant: `write_map` sorts a flat map
/// and places a hash map's keys by `erts_map_hash`, collapsing duplicate keys
/// **last value wins** in both cases.
///
/// The atom table passed to `write_map` is a **fresh empty one**. The builder
/// consults it only to name atoms in a *tie-break* (`compare::cmp_atoms`, reached
/// from `cmp`/`eq` for keys whose hashes collide); an unknown name falls back to the
/// raw atom index, so the result stays deterministic and — because distinct atoms
/// have distinct indices — never merges two distinct keys. The same substitution
/// (`beam_abi`'s driver host `map_term`) is used there for the same reason: taking
/// `ctx.atoms()` alongside `ctx.heap()` would need two simultaneous borrows of
/// [`BifContext`](crate::context::BifContext).
///
/// # Errors
///
/// Propagates [`BeamError::Alloc`](beam_common::BeamError::Alloc) when `heap` cannot
/// fit the block, and the builder's own `TermError` (reconciled to
/// [`BeamError::BadArg`]) for a malformed key.
pub(crate) fn make_map(heap: &mut Heap, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let atoms = AtomTable::new();
    let words = beam_term::map::map_words_capacity(pairs.len());
    // SAFETY: `alloc_words` hands out `words` exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(words)? };
    // SAFETY: `p` is the base of those `words` words; `write_map` writes every word
    // it needs (the reservation is a documented upper bound, so trailing words may
    // stay unwritten) and returns a boxed root into the same block, which stays live
    // and unmoved for as long as `heap` does, which the caller keeps alive.
    Ok(unsafe { beam_term::map::write_map(p, pairs, &atoms)? })
}

/// The `(key, value)` pairs of **any** map — flat map or HAMT — in the order the map
/// **stores** them: a flat map's keys-tuple order, a hash map's HAMT slot traversal
/// order. That is the order the oracle's `maps:keys/1`, `maps:values/1` and
/// `maps:to_list/1` report.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a map.
pub(crate) fn map_pairs_storage_order(term: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    Ok(beam_term::map::map_pairs_storage_order(term)?)
}

/// The number of keys in **any** map — flat map or HAMT (`erts_map_size`).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a map.
pub(crate) fn map_size(term: Eterm) -> Result<usize> {
    Ok(beam_term::map::map_size(term, &AtomTable::new())?)
}

/// The value bound to `key` in **any** map — flat map or HAMT — or `None`.
///
/// The key match is the frozen exact (`=:=`) one (`beam_term::map::map_get`), so the
/// caller must have classified `term` as a map first. `atoms` is the caller's real
/// table (it decides only the atom-name tie-break inside `=:=`); no heap is involved,
/// so a BIF can pass `ctx.atoms()` here.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a map.
pub(crate) fn map_get(term: Eterm, key: Eterm, atoms: &AtomTable) -> Result<Option<Eterm>> {
    Ok(beam_term::map::map_get(term, key, atoms)?)
}

// ---------------------------------------------------------------------------
// Hash-map (HAMT) node interiors
//
// The frozen node layout is owned by `beam_term::map` (a sibling crate); the
// accessors below are the crate's read-only view of it, mirroring the C's
// `hashmap_node_size` / the `hashmap_info` walk's node switch
// (`erl_map.c:2296-2320`, `erl_map.c:3490-3536`):
//
// | node | header | words | layout |
// |---|---|---|---|
// | head, 16 slots | `HAMT_SUBTAG_HEAD_ARRAY` | `18` | `[hdr][size][16 slots]` |
// | head, bitmap | `HAMT_SUBTAG_HEAD_BITMAP(bmp)` | `2 + n` | `[hdr][size][n slots]` |
// | inner, bitmap | `HAMT_SUBTAG_NODE_BITMAP(bmp)` | `1 + n` | `[hdr][n slots]` |
// | collision | `make_arityval(n)` | `1 + n` | a tuple of `n` kv cons cells |
//
// with `n = popcount(bmp)`. A slot is one **term word**: a leaf cons cell, a
// sub-node or — at the last level — a collision tuple.
// ---------------------------------------------------------------------------

/// The class of a hash-map node, read from its header word
/// (`_HEADER_MAP_SUBTAG_MASK`, `erl_map.h:169`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HamtNode {
    /// `HAMT_SUBTAG_HEAD_ARRAY` — the head node with all 16 slots present,
    /// `[hdr][size][16 slots]`.
    HeadArray,
    /// `HAMT_SUBTAG_HEAD_BITMAP(bmp)` — the head node holding only the slots the
    /// bitmap marks, `[hdr][size][n slots]`.
    HeadBitmap,
    /// `HAMT_SUBTAG_NODE_BITMAP(bmp)` — an inner node, `[hdr][n slots]`.
    InnerBitmap,
    /// A collision node — a plain `make_arityval(n)` tuple of `n` kv cons cells.
    Collision,
}

/// The class of the hash-map node `term` — `None` for anything that is not a
/// hash-map node (a leaf cons cell, a flat map, an immediate, a tuple, …).
///
/// An inner node and a collision node are *not* reachable as an ordinary Erlang
/// term, but both are returned by [`crate::erts_internal::map_hashmap_children/1`],
/// which the oracle lets a caller hand straight back to it (`erl_map.c:3425`),
/// so the classification covers all four classes.
///
/// A collision node's header is an arityval, i.e. *not* a map header, so it is
/// told apart by [`is_arity_value`] after the map-header test fails.
pub(crate) fn hamt_node_kind(term: Eterm) -> Option<HamtNode> {
    if !is_boxed_term(term) {
        return None;
    }
    // SAFETY: `term` is a live boxed term (and not a bare header word), so
    // `boxed_val` points at its header word.
    let header = unsafe { *boxed_val(term) };
    if beam_term::is_map_header(header) {
        match header.raw() & HEADER_MAP_SUBTAG_MASK {
            HAMT_SUBTAG_HEAD_ARRAY => Some(HamtNode::HeadArray),
            HAMT_SUBTAG_HEAD_BITMAP => Some(HamtNode::HeadBitmap),
            HAMT_SUBTAG_NODE_BITMAP => Some(HamtNode::InnerBitmap),
            // The remaining map tag is the flat map, which has no HAMT interior.
            _ => None,
        }
    } else if is_arity_value(header) {
        Some(HamtNode::Collision)
    } else {
        None
    }
}

/// A hash-map node's interior: its class and its slot words in **storage order**,
/// exactly as the C reads them (`*ptr++`, `erl_map.c:3490-3536`).
pub(crate) struct HamtNodeView {
    /// The node's class.
    pub kind: HamtNode,
    /// The slot words in storage order (slot `0` first).
    pub slots: Vec<Eterm>,
}
/// Reads the interior of the hash-map node `term` (see [`HamtNode`] for the
/// layout; `hashmap_node_size`, `erl_map.c:2296-2320`).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `term` is not a well-formed hash-map node — not a
/// node at all, or a bitmap node with no slot set.
pub(crate) fn hamt_node(term: Eterm) -> Result<HamtNodeView> {
    let kind = hamt_node_kind(term).ok_or(BeamError::BadArg)?;
    let base = boxed_val(term);
    // SAFETY: `term` is a live boxed hash-map node, so `base` points at its
    // header word (word 0 of its block).
    let header = unsafe { *base };
    // `slots_off` is the word offset of slot 0 (`hashmap_node_size`'s `*nodep`
    // advancement: the head's `size` word sits between the header and the slots).
    let (slots_off, count) = match kind {
        HamtNode::HeadArray => (2usize, 16usize),
        HamtNode::HeadBitmap | HamtNode::InnerBitmap => {
            let bitmap = beam_term::map_header_val(header) as u16;
            let slots_off = if kind == HamtNode::HeadBitmap { 2 } else { 1 };
            (slots_off, bitmap.count_ones() as usize)
        }
        HamtNode::Collision => (1usize, beam_term::header_arity(header)),
    };
    if count == 0 {
        // A bitmap node always has at least one slot and a collision node at
        // least two (`ASSERT(sz >= 1)` / `ASSERT(sz >= 2)`); a zero-slot header
        // is malformed, and `count - 1` indexing downstream would underflow.
        return Err(BeamError::BadArg);
    }
    let mut slots = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: the node owns `slots_off + count` words (the frozen layout
        // above), so slot `i` lies inside its own live block.
        slots.push(unsafe { *base.add(slots_off + i) });
    }
    Ok(HamtNodeView { kind, slots })
}
// ---------------------------------------------------------------------------
// Native records (`beam_term::record` owns the frozen layout; `erl_record.h`)
//
// Milestone-1 had no record layout; `beam_term::record` now provides it, so the
// record BIFs (`crate::records`) and the native half of `erlang:is_record/2,3`
// (`crate::type_test`) read and write through these safe wrappers. Every
// `beam_term::record::*` function is `unsafe` (it dereferences a header word), so
// the wrappers below carry the crate's single `#[allow(unsafe_code)]`.
// ---------------------------------------------------------------------------

/// Builds a native-record construction block on `heap` and returns the tagged
/// **instance** root (`record_defs`/`write_record`, `erl_record.h:41-96`).
///
/// Reserves `record_block_words(fields.len())` words from the caller's
/// `beam_alloc::Heap`, then lays out the frozen
/// `[field_order tuple][definition][instance]` block with
/// [`beam_term::write_record`]. `fields` are `(key-atom-term, value)` pairs in
/// **declaration** order; the cached definition hash is left as the `0`
/// placeholder (see below).
///
/// # Errors
///
/// [`BeamError::BadArg`] when [`beam_term::write_record`] rejects the fields — a
/// non-atom key, a duplicate key, or an arity beyond `MAX_ARITYVAL`
/// (`TermError::BadRecord` reconciled to `badarg`); otherwise propagates
/// [`BeamError::Alloc`](beam_common::BeamError::Alloc) when `heap` cannot fit the
/// block.
/// This writer is **not** on any runtime path: `records:create/4` and
/// `records:update/4` build through
/// [`beam_codec::term::make_record`](beam_codec::term::make_record), which stores
/// the real `beam_term::hashing::compute_record_definition_hash` value at
/// definition word 2. It is retained only because the `compare` test suite pins
/// the record layout through it (where the stored word is irrelevant —
/// `beam_term::hashing` recomputes the definition hash rather than reading it).
#[allow(dead_code)]
pub(crate) fn native_record_write(
    heap: &mut Heap,
    module: Atom,
    name: Atom,
    is_exported: bool,
    fields: &[(Eterm, Eterm)],
) -> Result<Eterm> {
    let words = beam_term::record_block_words(fields.len());
    // SAFETY: `alloc_words` hands out `words` exclusively-owned, writable words.
    let p = unsafe { heap.alloc_words(words)? };
    // SAFETY: `p` is the base of those `words` words; `write_record` writes all of
    // them (the `field_order` tuple, the definition and the instance) and returns a
    // boxed root into the same block, which stays live (and unmoved) for as long as
    // `heap` does.
    Ok(unsafe {
        beam_term::write_record(p, module, name, is_exported, fields, make_small(0))?
    })
}

/// The definition term of a native-record instance
/// (`record_definition`, the boxed pointer at instance word 1).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `rec` is not a well-formed native record
/// (`TermError::BadRecord` reconciled to `badarg`).
pub(crate) fn native_record_definition(rec: Eterm) -> Result<Eterm> {
    // SAFETY: `rec` is a live machine word; `record_definition` returns
    // `TermError::BadRecord` (never a panic) for a non-record or a malformed block.
    Ok(unsafe { beam_term::record_definition(rec)? })
}

/// The `module` atom term of a native-record definition (word 3).
///
/// `def` must come from [`native_record_definition`].
///
/// # Errors
///
/// [`BeamError::BadArg`] when `def` is not a well-formed definition.
pub(crate) fn native_record_definition_module(def: Eterm) -> Result<Eterm> {
    // SAFETY: `def` is a live definition term obtained from
    // `native_record_definition`; `record_definition_module` re-validates it.
    Ok(unsafe { beam_term::record_definition_module(def)? })
}

/// The `name` atom term of a native-record definition (word 4).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `def` is not a well-formed definition.
pub(crate) fn native_record_definition_name(def: Eterm) -> Result<Eterm> {
    // SAFETY: as [`native_record_definition_module`].
    Ok(unsafe { beam_term::record_definition_name(def)? })
}

/// The `is_exported` atom term (`true`/`false`) of a native-record definition
/// (word 5).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `def` is not a well-formed definition.
pub(crate) fn native_record_definition_is_exported(def: Eterm) -> Result<Eterm> {
    // SAFETY: as [`native_record_definition_module`].
    Ok(unsafe { beam_term::record_definition_is_exported(def)? })
}

/// The definition's field **keys** in **declaration** order
/// (`records_get_field_names_1`, `erl_record.c:1298-1326`).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `def` is not a well-formed definition or its
/// `field_order` is malformed.
pub(crate) fn native_record_declaration_keys(def: Eterm) -> Result<Vec<Eterm>> {
    // SAFETY: as [`native_record_definition_module`].
    Ok(unsafe { beam_term::record_declaration_keys(def)? })
}

/// The value of the field whose key equals `key`, or `None` when no key matches
/// (`records_get_2`, `erl_record.c:1247-1274`).
///
/// # Errors
///
/// [`BeamError::BadArg`] when `rec` is not a well-formed native record.
pub(crate) fn native_record_get(
    rec: Eterm,
    key: Eterm,
    atoms: &AtomTable,
) -> Result<Option<Eterm>> {
    // SAFETY: `rec` is a live machine word; `record_get` returns
    // `TermError::BadRecord` for a non-record and never indexes out of bounds.
    Ok(unsafe { beam_term::record_get(rec, key, atoms)? })
}

/// The native-record branch of `is_record_2` (`erl_bif_op.c:306-312`) — whether
/// `rec` is a native record whose definition name is `name`.
///
/// A non-record returns `false` (never a panic).
#[inline]
pub(crate) fn native_record_name_is(rec: Eterm, name: Atom) -> bool {
    // SAFETY: `rec` is a live machine word; `record_name_is` is a total predicate
    // that dereferences only a validated record header.
    unsafe { beam_term::record_name_is(rec, name) }
}

/// `erl_is_native_record` (`erl_record.c:214-221`) — whether `rec` is a native
/// record whose definition module and name are `module`/`name`.
///
/// A non-record returns `false` (never a panic).
#[inline]
pub(crate) fn native_record_is_native(rec: Eterm, module: Atom, name: Atom) -> bool {
    // SAFETY: as [`native_record_name_is`].
    unsafe { beam_term::record_is_native(rec, module, name) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_alloc::RefcBinary;
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
    fn bitstring_bits_re_packs_any_bitstring_msb_first() {
        let mut heap = heap();
        // `<<1:1>>` — one bit, in the MSB of the single data byte.
        let one = make_heap_bitstring(&mut heap, &[0b1000_0000], 1).expect("fits");
        assert_eq!(bitstring_bits(one).unwrap(), vec![0b1000_0000]);
        // `<<5:3,3:2>>` = `10111` ⇒ the canonical MSB-first re-packing.
        let five = make_heap_bitstring(&mut heap, &[0b1011_1000], 5).expect("fits");
        assert_eq!(bitstring_bits(five).unwrap(), vec![0b1011_1000]);
        // `<<1:9>>` — a whole leading byte then the trailing bit.
        let nine = make_heap_bitstring(&mut heap, &[0x00, 0b1000_0000], 9).expect("fits");
        assert_eq!(bitstring_bits(nine).unwrap(), vec![0x00, 0b1000_0000]);
        // A byte-aligned binary re-packs to exactly its own bytes.
        let bin = make_heap_bitstring(&mut heap, &[0xAB, 0xCD], 16).expect("fits");
        assert_eq!(bitstring_bits(bin).unwrap(), vec![0xAB, 0xCD]);
        // An empty binary yields no bytes.
        let empty = make_heap_bitstring(&mut heap, &[], 0).expect("fits");
        assert_eq!(bitstring_bits(empty).unwrap(), Vec::<u8>::new());
        // A non-bitstring is rejected.
        for term in [make_small(7), NIL] {
            assert!(matches!(bitstring_bits(term), Err(BeamError::BadArg)));
        }
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
    /// [`is_mixed_identity_pair`] recognises exactly the three internal/external
    /// pairs `beam_term::compare`'s node-aware entry points decide differently, in
    /// **both** operand orders — and nothing else. Every term below is built with the
    /// safe `beam_codec::term` writers (the same frozen layouts the oracle's
    /// `binary_to_term` produces), so this pins the pre-test at term-shape level.
    #[test]
    fn mixed_identity_pair_is_exactly_the_three_internal_external_pairs() {
        let mut heap = Heap::new(64).expect("test heap");
        let remote = beam_term::make_atom(beam_term::Atom::from_raw(7));
        let internal_pid = ProcessId::new(10, 0).to_eterm();
        let external_pid = beam_codec::term::make_external_pid(&mut heap, remote, 10, 0, 0)
            .expect("the external pid block fits");
        let internal_port = beam_term::make_internal_port(beam_term::make_port_data(3));
        let external_port = beam_codec::term::make_external_port(&mut heap, remote, 3, 0)
            .expect("the external port block fits");
        let internal_ref = beam_codec::make_internal_ref(&mut heap, 9).expect("the ref fits");
        let external_ref = beam_codec::term::make_external_ref(&mut heap, remote, &[9], 0)
            .expect("the external ref block fits");

        let pairs = [
            ("pid", internal_pid, external_pid),
            ("port", internal_port, external_port),
            ("ref", internal_ref, external_ref),
        ];
        for (label, internal, external) in pairs {
            assert!(is_mixed_identity_pair(internal, external), "{label}, internal first");
            assert!(is_mixed_identity_pair(external, internal), "{label}, external first");
        }

        // The categories must match: an internal pid against an external *port* is not
        // the pair (the two are different term classes, decided by the class ladder).
        let cross = [
            ("pid/port", internal_pid, external_port),
            ("pid/ref", internal_pid, external_ref),
            ("port/pid", internal_port, external_pid),
            ("port/ref", internal_port, external_ref),
            ("ref/pid", internal_ref, external_pid),
            ("ref/port", internal_ref, external_port),
        ];
        for (label, a, b) in cross {
            assert!(!is_mixed_identity_pair(a, b), "cross-category {label}");
            assert!(!is_mixed_identity_pair(b, a), "cross-category {label}, reversed");
        }

        // Two internals, two externals, and anything that is not an identity term at all.
        let non_pairs = [
            ("internal/internal", internal_pid, internal_port),
            ("external/external", external_pid, external_port),
            ("same internal pid", internal_pid, internal_pid),
            ("same external pid", external_pid, external_pid),
            ("pid/int", internal_pid, make_small(1)),
            ("pid/atom", external_pid, remote),
            ("port/nil", internal_port, NIL),
            ("ref/tuple", internal_ref, heap.tuple(&[make_small(1)]).expect("fits")),
        ];
        for (label, a, b) in non_pairs {
            assert!(!is_mixed_identity_pair(a, b), "not a mixed pair: {label}");
            assert!(!is_mixed_identity_pair(b, a), "not a mixed pair: {label}, reversed");
        }

        // A word that is not a term at all (the ABI's `THE_NON_VALUE` is `0`, a bare
        // header word) is `false` from every boxed predicate, hence `false` here too —
        // never a dereference.
        let non_value = Eterm::from_raw(0);
        assert!(!is_mixed_identity_pair(non_value, internal_pid));
        assert!(!is_mixed_identity_pair(internal_pid, non_value));
        assert!(!is_mixed_identity_pair(non_value, non_value));
    }
    #[test]
    fn heap_bitstring_has_the_frozen_header_and_layout() {
        let mut heap = Heap::new(64).expect("test heap");
        let bin = make_heap_bitstring(&mut heap, &[0xAB, 0xCD], 16).expect("the binary fits");
        assert!(is_boxed(bin));
        assert!(is_bitstring(bin));
        assert!(is_binary(bin));
        assert_eq!(bitstring_bit_size(bin).unwrap(), 16);
        // SAFETY: `bin` is the live heap bitstring built above; word 0 is its header,
        // word 1 the bit size and words 2.. its data (all written by
        // `make_heap_bitstring`).
        unsafe {
            assert_eq!(*boxed_val(bin), make_header(2, HEAP_BITS_SUBTAG));
            assert_eq!((*boxed_val(bin).add(1)).raw(), 16);
            // The bytes are stored in address order (`erl_bits.h`: the data area is
            // byte-addressed), so this is endianness-agnostic.
            assert_eq!(*(boxed_val(bin).add(2).cast::<u8>()), 0xAB);
            assert_eq!(*(boxed_val(bin).add(2).cast::<u8>().add(1)), 0xCD);
            // The unused bytes of the final data word are zeroed.
            assert_eq!(*(boxed_val(bin).add(2).cast::<u8>().add(2)), 0x00);
            assert_eq!(*(boxed_val(bin).add(2).cast::<u8>().add(7)), 0x00);
        }
        assert_eq!(binary_bytes(bin).unwrap(), vec![0xAB, 0xCD]);
    }

    #[test]
    fn heap_bitstring_sizes_follow_the_frozen_word_count() {
        let mut heap = Heap::new(64).expect("test heap");
        // `erl_bits.h:heap_bits_size`: 2 + ceil(bits / 64) words, header arity
        // `1 + ceil(bits / 64)`.
        for (nbytes, words, arity) in [(0usize, 2usize, 1usize), (2, 3, 2), (8, 3, 2), (64, 10, 9)] {
            let before = heap.used();
            let bin =
                make_heap_bitstring(&mut heap, &vec![0u8; nbytes], nbytes * 8).expect("fits");
            assert_eq!(heap.used() - before, words, "{nbytes}-byte binary word count");
            // SAFETY: `bin` is a live heap bitstring; word 0 is its header.
            let header = unsafe { *boxed_val(bin) };
            assert_eq!(header, make_header(arity, HEAP_BITS_SUBTAG));
        }
    }

    /// The bytes `<<(I rem 256) || I <- lists:seq(1, N)>>` holds — the oracle's
    /// >64-byte subject for the off-heap refc-binary path.
    fn ramp(n: usize) -> Vec<u8> {
        (1..=n).map(|i| (i % 256) as u8).collect()
    }

    /// Builds the off-heap `[BinRef][ErlSubBits]` pair the engine's `bs_create_bin`
    /// writes for a payload above `ERL_ONHEAP_BINARY_LIMIT` (64) bytes, and returns
    /// the boxed root.
    ///
    /// `payload` owns the storage the pair points into and must outlive every use of
    /// the returned term.
    fn refc_binary(
        heap: &mut Heap,
        payload: &RefcBinary,
        offset_bits: usize,
        size_bits: usize,
    ) -> Eterm {
        let base = payload.data_ptr();
        // SAFETY: `alloc_words` hands out `ERL_REFC_BITS_SIZE` (8) exclusively-owned,
        // writable words — exactly the pair `write_refc_binary` fully writes.
        let hp = unsafe { heap.alloc_words(beam_term::refc::ERL_REFC_BITS_SIZE) }
            .expect("the 8-word pair fits");
        // SAFETY: `hp` is that reserved pair; `base` is the first byte of `payload`'s
        // live, `UNIT_SIZE`-aligned data area (kept alive by the caller), which covers
        // bits `offset_bits..offset_bits + size_bits`.
        unsafe {
            beam_term::refc::write_refc_binary(
                hp,
                base.cast::<Eterm>(),
                core::ptr::null_mut(),
                base,
                offset_bits,
                size_bits,
            )
        }
    }

    #[test]
    fn binary_bytes_reads_an_off_heap_refc_binary() {
        let mut heap = Heap::new(64).expect("test heap");
        let data = ramp(96);
        assert!(data.len() > 64, "the subject must exceed ERL_ONHEAP_BINARY_LIMIT");
        let payload = RefcBinary::new(&data).expect("the off-heap payload allocates");

        // The whole-binary view: `start = 0`, the full bit size.
        let root = refc_binary(&mut heap, &payload, 0, data.len() * 8);
        assert!(is_bitstring(root));
        assert!(is_binary(root));
        assert_eq!(bitstring_bit_size(root).unwrap(), data.len() * 8);
        assert_eq!(
            binary_bytes(root).unwrap(),
            data,
            "the off-heap payload must be read verbatim, not from boxed_val + 2"
        );
        // The bit-by-bit reader agrees (it re-packs the same bits MSB-first).
        assert_eq!(bitstring_bits(root).unwrap(), data);

        // A byte-aligned SUB_WINDOW: `start = 8` bits, one byte shorter.
        let window = refc_binary(&mut heap, &payload, 8, (data.len() - 1) * 8);
        assert!(is_binary(window));
        assert_eq!(bitstring_bit_size(window).unwrap(), (data.len() - 1) * 8);
        assert_eq!(binary_bytes(window).unwrap(), data[1..].to_vec());

        // A multi-byte-aligned interior window (`start = 64` bits).
        let inner = refc_binary(&mut heap, &payload, 64, 8 * 8);
        assert!(is_binary(inner));
        assert_eq!(binary_bytes(inner).unwrap(), data[8..16].to_vec());

        // A mid-byte window has no contiguous byte image, so it is re-packed bit by
        // bit: `start = 4`, size 8 bits ⇒ the low nibble of `data[0]` (0x01) then the
        // high nibble of `data[1]` (0x02) ⇒ `0b0001_0000`.
        let mid = refc_binary(&mut heap, &payload, 4, 8);
        assert!(is_binary(mid), "an 8-bit window is byte aligned");
        assert_eq!(binary_bytes(mid).unwrap(), vec![0x10]);
        assert_eq!(binary_bytes(mid).unwrap(), bitstring_bits(mid).unwrap());

        // A zero-length window must not touch the base pointer.
        let empty = refc_binary(&mut heap, &payload, 0, 0);
        assert!(is_binary(empty));
        assert_eq!(binary_bytes(empty).unwrap(), Vec::<u8>::new());
    }

    /// `with_binary_bytes` is `binary_bytes`' borrowing twin: the same
    /// representations, the same `BadArg` rule, and byte-for-byte the same answer —
    /// reached without a copy for every window that starts on a byte boundary.
    #[test]
    fn with_binary_bytes_lends_the_bytes_binary_bytes_copies() {
        /// Runs `with_binary_bytes` and copies its slice out, so the borrow's value
        /// can be compared with [`binary_bytes`]' owned form.
        fn lent(term: Eterm) -> Result<Vec<u8>> {
            with_binary_bytes(term, |bytes| Ok(bytes.to_vec()))
        }

        let mut heap = Heap::new(64).expect("test heap");

        // An on-heap `ErlHeapBits` binary (the shape this crate builds).
        let on_heap = make_heap_bitstring(&mut heap, b"hello world", 11 * 8).expect("fits");
        assert_eq!(lent(on_heap).unwrap(), b"hello world".to_vec());
        assert_eq!(lent(on_heap).unwrap(), binary_bytes(on_heap).unwrap());

        // The off-heap `[BinRef][ErlSubBits]` pair, in each window shape.
        let data = ramp(96);
        let payload = RefcBinary::new(&data).expect("the off-heap payload allocates");
        let whole = refc_binary(&mut heap, &payload, 0, data.len() * 8);
        assert_eq!(lent(whole).unwrap(), data);
        // A byte-aligned interior window starts at `offset_bits / 8`.
        let inner = refc_binary(&mut heap, &payload, 64, 8 * 8);
        assert_eq!(lent(inner).unwrap(), data[8..16].to_vec());
        assert_eq!(lent(inner).unwrap(), binary_bytes(inner).unwrap());
        // A mid-byte window has no contiguous image: it is re-packed (MSB-first) into
        // a scratch buffer, and the answer still equals the copying form's.
        let mid = refc_binary(&mut heap, &payload, 4, 8);
        assert_eq!(lent(mid).unwrap(), vec![0x10]);
        assert_eq!(lent(mid).unwrap(), binary_bytes(mid).unwrap());
        // A zero-length window must not touch the base pointer, and lends `&[]`.
        let empty = refc_binary(&mut heap, &payload, 0, 0);
        assert_eq!(lent(empty).unwrap(), Vec::<u8>::new());
        let mut saw_len = usize::MAX;
        with_binary_bytes(empty, |bytes| {
            saw_len = bytes.len();
            Ok(())
        })
        .unwrap();
        assert_eq!(saw_len, 0, "an empty binary lends an empty slice");

        // Anything that is not a byte-aligned binary is still `BadArg` — including a
        // non-byte-aligned bitstring, exactly like the copying form.
        let bits = make_heap_bitstring(&mut heap, &[0xFF; 2], 9).expect("fits");
        let tuple = heap.tuple(&[make_small(1)]).expect("fits");
        for term in [bits, tuple, make_small(7), NIL] {
            assert!(matches!(with_binary_bytes(term, |_| Ok(())), Err(BeamError::BadArg)));
            assert!(matches!(binary_bytes(term), Err(BeamError::BadArg)));
        }

        // `body`'s own error travels out unchanged (the accessor never invents one).
        let escaped = with_binary_bytes(on_heap, |_| Err::<(), _>(BeamError::BadArity));
        assert!(matches!(escaped, Err(BeamError::BadArity)));
    }

    #[test]
    fn empty_flatmap_round_trips() {
        let mut heap = Heap::new(64).expect("test heap");
        let map = make_flatmap(&mut heap, &[]).expect("the empty map fits");
        assert!(is_flatmap(map));
        assert!(!is_hashmap(map));
        let (keys, values) = flatmap_pairs(map).unwrap();
        assert!(keys.is_empty());
        assert!(values.is_empty());
        // The empty block is the three-word `[head, 0, tuple0]` form.
        assert_eq!(heap.used(), 3);
    }

    #[test]
    fn flatmaps_round_trip_in_wire_order() {
        let mut heap = Heap::new(256).expect("test heap");
        for n in [1usize, 2, 32] {
            let pairs: Vec<(Eterm, Eterm)> = (0..n)
                .map(|i| (make_small(i as i64), make_small(-(i as i64))))
                .collect();
            let map = make_flatmap(&mut heap, &pairs).expect("the map fits");
            assert!(is_flatmap(map));
            let (keys, values) = flatmap_pairs(map).unwrap();
            assert_eq!(keys.len(), n);
            assert_eq!(values.len(), n);
            for (i, (key, value)) in pairs.iter().enumerate() {
                assert_eq!(keys[i], *key, "key {i}");
                assert_eq!(values[i], *value, "value {i}");
            }
        }
    }

    #[test]
    fn flatmap_accessors_reject_non_maps() {
        let mut heap = Heap::new(64).expect("test heap");
        let tuple = heap.tuple(&[make_small(1)]).expect("the tuple fits");
        for term in [make_small(1), NIL, tuple] {
            assert!(!is_flatmap(term));
            assert!(!is_hashmap(term));
            assert!(matches!(flatmap_pairs(term), Err(BeamError::BadArg)));
        }
    }
}