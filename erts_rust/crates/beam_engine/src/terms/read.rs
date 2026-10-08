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

//! The tuple/list/binary **readers**: structure access that never allocates.
//! A wrong term type or an out-of-range index is [`BeamError::BadArg`].
use beam_common::{BeamError, Result};
use beam_term::Eterm;

use super::HEAP_BIN_HEAD_WORDS;

/// The arity of a tuple term; a non-tuple yields [`BeamError::BadArg`].
///
/// The exact inverse of [`make_tuple`](super::make_tuple) (`beam_term::tuple_size`).
pub fn tuple_arity(t: Eterm) -> Result<usize> {
    // SAFETY: `is_tuple` checks `is_boxed` before dereferencing the header word.
    if !unsafe { beam_term::is_tuple(t) } {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `is_tuple` established that `t` is a live tuple.
    Ok(unsafe { beam_term::tuple_size(t) })
}

/// The zero-based `i`-th element of a tuple; an out-of-range index or a non-tuple
/// yields [`BeamError::BadArg`].
pub fn tuple_element(t: Eterm, i: usize) -> Result<Eterm> {
    let arity = tuple_arity(t)?;
    if i >= arity {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `tuple_arity` established that `t` is a live tuple of `arity`
    // elements and `i < arity`, the bounds `tuple_element` requires.
    Ok(unsafe { beam_term::tuple_element(t, i) })
}

/// The head (`CAR`) of a cons cell; `NIL` or a non-list yields
/// [`BeamError::BadArg`].
pub fn cons_head(t: Eterm) -> Result<Eterm> {
    // `is_list` reads only the primary tag, never dereferences.
    if !beam_term::is_list(t) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `is_list` established that `t` is a header-less 2-word cons cell.
    Ok(unsafe { beam_term::car(t) })
}

/// The tail (`CDR`) of a cons cell; `NIL` or a non-list yields
/// [`BeamError::BadArg`].
pub fn cons_tail(t: Eterm) -> Result<Eterm> {
    // `is_list` reads only the primary tag, never dereferences.
    if !beam_term::is_list(t) {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `is_list` established that `t` is a header-less 2-word cons cell.
    Ok(unsafe { beam_term::cdr(t) })
}

/// Collects the elements of a proper list into a `Vec`.
///
/// Walks the cons cells until [`NIL`](beam_term::NIL); the empty list yields an
/// empty `Vec`. A non-list term or an improper tail (the final `CDR` is neither a
/// cons cell nor `NIL`) yields [`BeamError::BadArg`].
pub fn list_elements(t: Eterm) -> Result<Vec<Eterm>> {
    let mut out = Vec::new();
    let mut cur = t;
    loop {
        if beam_term::is_nil(cur) {
            return Ok(out);
        }
        if !beam_term::is_list(cur) {
            // Improper tail, or a non-list to begin with.
            return Err(BeamError::BadArg);
        }
        // SAFETY: `is_list` established that `cur` is a live cons cell.
        unsafe {
            out.push(beam_term::car(cur));
            cur = beam_term::cdr(cur);
        }
    }
}

/// The byte payload of an on-heap binary term; a non-binary (or a non-byte-aligned
/// bitstring) yields [`BeamError::BadArg`].
///
/// The returned slice borrows the term's storage, so the caller must keep the
/// owning heap alive and unmoved for as long as it is used.
pub fn binary_bytes<'a>(t: Eterm) -> Result<&'a [u8]> {
    // SAFETY: `is_binary` checks `is_boxed` before dereferencing the header word.
    if !unsafe { beam_term::is_binary(t) } {
        return Err(BeamError::BadArg);
    }
    // A binary larger than `ERL_ONHEAP_BINARY_LIMIT` bytes is stored **off** the
    // process heap: its term is the frozen 8-word `[BinRef][ErlSubBits]` pair, so the
    // heap path below would read the pair's own words instead of the payload.
    // `beam_term::refc::refc_bytes` re-derives the byte view from that frozen layout.
    // SAFETY: `t` is a live bitstring (checked above); if its header is an
    // `ErlSubBits` its `orig` slot is a live term, which is all `is_refc_binary`
    // dereferences.
    if unsafe { beam_term::refc::is_refc_binary(t) } {
        // SAFETY: `t` is a live refc-binary bitstring whose off-heap payload is owned
        // by the machine that built it (a `beam_alloc::RefcBinary` handle), so the
        // payload stays alive and unmoved for `'a` — the caller's contract.
        return Ok(unsafe { beam_term::refc::refc_bytes(t) });
    }
    // SAFETY: `is_binary` established that `t` is a live bitstring whose size is a
    // whole number of bytes; the size word (word 1) is live.
    let num_bytes = unsafe { beam_term::bitstring_size(t) } / 8;
    if num_bytes == 0 {
        return Ok(&[]);
    }
    // SAFETY: a well-formed byte binary owns `2 + ceil(num_bytes / word_bytes)`
    // words, so the `num_bytes` data bytes starting at word 2 are live and in
    // bounds; the caller keeps the owning heap alive for `'a`.
    Ok(unsafe {
        core::slice::from_raw_parts(
            beam_term::boxed_val(t)
                .add(HEAP_BIN_HEAD_WORDS)
                .cast::<u8>(),
            num_bytes,
        )
    })
}
