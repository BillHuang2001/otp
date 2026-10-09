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

//! Thin forwarding layer over [`beam_term::thing`], [`beam_term::bitstring`],
//! [`beam_term::fun`], [`beam_term::record`], [`beam_term::tuple`] and
//! [`beam_term::list`], split into the **bitstring** ([`binary`]), the
//! **fun / export** ([`fun`]) and the **native-record** ([`record`]) forwarding
//! layers.
//!
//! The frozen interior layout of a float, a bignum, a flat map, the external
//! pid/port/ref blocks, a bitstring, **every kind of fun/export** and a **native
//! record**, is owned by [`beam_term`] (the lowest common ancestor for term
//! operations — workspace `CONTEXT.md` §2, "No duplication across crates"). Every
//! constructor and accessor here therefore only:
//!
//! 1. checks the documented bounds/type/validity preconditions, mapping a failure
//!    to a [`beam_common::BeamError`], and
//! 2. either allocates the block from [`beam_alloc::Heap`] (the `make_*` family) or
//!    forwards straight to the matching [`beam_term`] accessor (the `*_parts`
//!    family, the bitstring sizing helpers and the tuple/cons accessors).
//!
//! All layout arithmetic — header construction, word offsets, digit/slice math —
//! lives in [`beam_term`]; this module retains only the small guard constants
//! declared below plus the one oversized *flat* map writer that [`beam_term`] cannot
//! produce (see [`make_flatmap`]).
//!
//! [`beam_term`]'s accessors are `unsafe fn` because the validity of an `Eterm` is
//! a caller invariant and there is no safe way to read or write a heap word, so the
//! crate-wide `#![deny(unsafe_code)]` is lifted for this module subtree alone
//! (`#[allow(unsafe_code)]` on the `mod term;` declaration in `lib.rs`, which
//! covers [`binary`], [`fun`] and [`record`] too). The crate's entire `unsafe`
//! surface is this single auditable subtree: the `beam_alloc` allocation calls, the
//! `beam_term` forwards and the minimal type/validity guards — each carrying a
//! `// SAFETY:` rationale.
//!
//! Every error stays [`beam_common::Result`] (via [`crate::error`]); an accessor
//! never panics and never exhibits undefined behaviour on a mismatched term.

mod binary;
mod fun;
mod record;
pub use binary::{
    bit_count_to_bytes, bitstring_bytes, bitstring_parts, bitstring_size_bits, make_binary,
    make_bitstring, make_refc_binary, BITSTRING_POINTER_FLAG_MASK,
};
/// The whole fun / export forwarding layer ([`fun`]): the boxed fun-term
/// constructors, the term-level accessors and the frozen layout surface
/// (`fun_kind_of_header`/`fun_words`/`fun_desc_words`/`fun_true_arity` plus
/// [`FunKind`], [`FunPid`], `FUN_MAX_FIELD`, `FUN_DESC_WORDS_EXTERNAL` and
/// `MAX_ARG`) re-exported verbatim from the layout owner (`beam_term::fun`).
pub use fun::{
    fun_arity_term, fun_creator_pid, fun_desc_words, fun_env, fun_function, fun_index,
    fun_kind_of_header, fun_kind_term, fun_module, fun_num_free_term, fun_old_index, fun_old_uniq,
    fun_true_arity, fun_true_arity_term, fun_uniq_bytes, fun_words, make_fun_external,
    make_fun_imported, FunKind, FunPid, FUN_DESC_WORDS_EXTERNAL, FUN_MAX_FIELD, MAX_ARG,
};
#[cfg(test)]
pub use fun::make_fun_local;
/// The whole native-record forwarding layer ([`record`]): the record constructor,
/// the instance/definition readers and the frozen sizing surface
/// (`record_block_words`/`record_def_size_words`/`record_inst_size_words` plus the
/// `AM_TRUE`/`AM_FALSE` atoms) re-exported from the layout owner
/// (`beam_term::record`).
pub use record::{
    is_record, make_record, record_block_words, record_declaration_keys, record_def_size_words,
    record_definition, record_definition_field_order, record_definition_is_exported,
    record_definition_keys, record_definition_module, record_definition_name, record_field_count,
    record_inst_size_words, record_values, AM_FALSE, AM_TRUE,
};
use core::mem::size_of;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{
    arityval, boxed_val, is_big, is_external_pid, is_external_port, is_external_ref, is_flatmap,
    is_float, is_list, is_tuple, make_arityval, make_boxed, make_header, make_tuple, Eterm,
    MAP_SMALL_MAP_LIMIT, MAP_SUBTAG, MAX_ARITYVAL,
};
use beam_term::thing;

use crate::error;

/// `BIG_ARITY_MAX` (`erl_term.h:465-469`, 64-bit `ARCH_64`): the largest bignum
/// digit count [`make_big`] accepts. The bignum layout itself is owned by
/// [`beam_term::thing`].
const BIG_ARITY_MAX: usize = (1 << 16) - 1;

/// `EXTERNAL_THING_HEAD_SIZE` (`erl_term.h:1161`):
/// `offsetof(ExternalThing, data) / sizeof(Uint)` — the `[header, node, next]`
/// head, three words on `ARCH_64`. Only the external-reference validity guard needs
/// this offset; the identifier layout itself is owned by [`beam_term::thing`].
const EXTERNAL_THING_HEAD_SIZE: usize = 3;
/// Number of `u32`s that fit in one term word (two on 64-bit). Only the
/// external-reference validity guard needs it; the identifier layout itself is
/// owned by [`beam_term::thing`].
const U32_PER_WORD: usize = size_of::<Eterm>() / size_of::<u32>();

/// `ERTS_MAX_REF_NUMBERS` (`erl_term.h:780`): the largest external-reference id
/// count [`make_external_ref`] accepts (and therefore the largest
/// [`external_ref_parts`] reports). Re-exported from its owner [`beam_term::thing`].
pub use beam_term::thing::MAX_REF_IDS;

/// Materializes a float term on `heap`.
///
/// The `[HEADER_FLONUM, f64 bits]` layout and the root word are written by
/// [`beam_term::thing::write_float`].
pub fn make_float(heap: &mut Heap, value: f64) -> Result<Eterm> {
    // SAFETY: `alloc_words` returned `thing::FLOAT_SIZE_WORDS` exclusively-owned,
    // writable words.
    let hp = unsafe { heap.alloc_words(thing::FLOAT_SIZE_WORDS)? };
    // SAFETY: `hp` owns exactly those words, which `write_float` fully initialises.
    Ok(unsafe { thing::write_float(hp, value) })
}

/// Reads the value of a float term.
///
/// Delegates to [`beam_term::thing::float_value`]. A term that is not a float
/// yields `BeamError::BadArg`.
pub fn float_value(term: Eterm) -> Result<f64> {
    // SAFETY: `is_float` reads only the header word of a boxed term, which is the
    // caller invariant for any `Eterm`.
    if !unsafe { is_float(term) } {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_float` established that `term` is a live boxed float, the exact
    // invariant `thing::float_value` documents.
    Ok(unsafe { thing::float_value(term) })
}

/// Materializes a bignum term on `heap`.
///
/// `digits` are `ErtsDigit`s (`big.h:29`: `typedef Uint ErtsDigit`, exactly one
/// machine word — `u64` on `ARCH_64`), **least significant first**; `negative` sets
/// the header's `_BIG_SIGN_BIT`. The `[header, d0 … d(n-1)]` layout is written by
/// [`beam_term::thing::write_big`].
///
/// An empty `digits` is an illegal zero-width bignum (`external.c:4474-4486` strips
/// the leading zeroes precisely to avoid building one) and yields
/// `BeamError::BadArg`; more than `BIG_ARITY_MAX` (`(1 << 16) - 1`) digits yields
/// `BeamError::SystemLimit`.
pub fn make_big(heap: &mut Heap, negative: bool, digits: &[u64]) -> Result<Eterm> {
    let n = digits.len();
    if n == 0 {
        return Err(error::bad_arg());
    }
    if n > BIG_ARITY_MAX {
        return Err(error::system_limit());
    }
    // SAFETY: `alloc_words` returned `thing::big_size_words(n)` exclusively-owned,
    // writable words.
    let hp = unsafe { heap.alloc_words(thing::big_size_words(n))? };
    // SAFETY: `hp` owns `big_size_words(n)` words and `digits` is non-empty, the
    // preconditions `write_big` documents.
    Ok(unsafe { thing::write_big(hp, negative, digits) })
}

/// Splits a bignum term into `(negative, digits)`.
///
/// `digits` are `ErtsDigit`s (`u64`), least significant first, borrowed from the
/// term's storage (the caller must keep the owning heap alive). Delegates to
/// [`beam_term::thing::big_parts`]. A term that is not a bignum yields
/// `BeamError::BadArg`.
pub fn big_parts<'a>(term: Eterm) -> Result<(bool, &'a [u64])> {
    // SAFETY: `is_big` reads only the header word of a boxed term (the caller
    // invariant for any `Eterm`).
    if !unsafe { is_big(term) } {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_big` established that `term` is a live bignum; the caller keeps the
    // owning heap alive for `'a`.
    Ok(unsafe { thing::big_parts(term) })
}

/// Materializes a flat map on `heap`, whatever its pair count.
///
/// The `[arityval(n), k0 … k(n-1), head, n, keys, v0 … v(n-1)]` layout (and the
/// 3-word empty block) is written by [`beam_term::thing::write_flatmap`] for
/// `n <= MAP_SMALL_MAP_LIMIT` ([`beam_term::map::MAP_SMALL_MAP_LIMIT`], imported
/// here from its owner) and by [`write_large_flatmap`] above it (byte-identical
/// layout — see that writer).
///
/// Builds a **flat** map and keeps the caller's pair order as-is (no sort, no dedup), so
/// the wire order equals the given order. A map with more than `MAP_SMALL_MAP_LIMIT`
/// (32) distinct keys would normally be a hash map instead — build that with
/// [`beam_term::map::write_map`], which applies ERTS's `erts_cmp_flatmap_keys` order
/// (see the crate `CONTEXT.md`, §"Map key encode order"). A pair count beyond
/// `MAX_ARITYVAL` cannot be represented by the `arityval` header and yields
/// `BeamError::SystemLimit`.
pub fn make_flatmap(heap: &mut Heap, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let n = pairs.len();
    if n > MAX_ARITYVAL {
        // The pair count becomes the flatmap `arityval`, whose size field is 24 bits.
        return Err(error::system_limit());
    }
    // NOTE: duplicate keys are **not** detected — that needs term comparison (owned
    // by `beam_term`/`beam_ets`) — so the caller's pair order is preserved as-is,
    // deliberately **not** re-sorted into ERTS's `erts_cmp_flatmap_keys` order
    // (`erl_map.h` `MAP_HEADER_FLATMAP`; `external.c:3746-3769`). Sorting is the map
    // *constructor's* job (`beam_term::map::write_map`), not the codec's. (See the
    // crate `CONTEXT.md`, §"Map key encode order".)
    // SAFETY: `alloc_words` returned `thing::flatmap_size_words(n)` exclusively-owned,
    // writable words.
    let hp = unsafe { heap.alloc_words(thing::flatmap_size_words(n))? };
    if n <= MAP_SMALL_MAP_LIMIT {
        // SAFETY: `hp` owns `flatmap_size_words(n)` words and `n <= MAP_SMALL_MAP_LIMIT`,
        // the preconditions `write_flatmap` documents.
        Ok(unsafe { thing::write_flatmap(hp, pairs) })
    } else {
        // SAFETY: `hp` owns `flatmap_size_words(n)` words and
        // `MAP_SMALL_MAP_LIMIT < n <= MAX_ARITYVAL`, the preconditions
        // `write_large_flatmap` documents.
        Ok(unsafe { write_large_flatmap(hp, pairs) })
    }
}

/// Materializes a **canonical** map on `heap` from a wire-order pair sequence.
///
/// This is the ERTS map constructor, used by the `MAP_EXT` decoder: duplicate
/// keys are collapsed **last value wins** and the produced map is either a flat
/// map whose keys are sorted in [`beam_term::map::cmp_flatmap_keys`] order (at
/// most [`MAP_SMALL_MAP_LIMIT`] = 32 distinct keys) or a hash map (HAMT) above
/// that — exactly the shape ERTS's `binary_to_term` produces
/// (`erl_map.c:3187` `erts_validate_and_sort_flatmap`, `erl_map.c:644`
/// `erts_hashmap_from_array`, both reached from the `external.c:5438-5456`
/// fixup). A caller that must reject duplicate keys compares the produced
/// [`beam_term::map::map_size`] against the input length.
///
/// Delegates the layout to [`beam_term::map::write_map`] (the LCA writer), which
/// reserves nothing itself: this forwards the exact
/// [`beam_term::map::map_words_capacity`]`(pairs.len())` reservation.
/// See the crate `CONTEXT.md`, §"Map key encode order".
pub fn make_map(
    heap: &mut Heap,
    pairs: &[(Eterm, Eterm)],
    atoms: &beam_term::AtomTable,
) -> Result<Eterm> {
    // SAFETY: `alloc_words` returned `beam_term::map::map_words_capacity(pairs.len())`
    // exclusively-owned, writable words — the exact reservation `write_map`
    // documents.
    let hp = unsafe { heap.alloc_words(beam_term::map::map_words_capacity(pairs.len()))? };
    // SAFETY: `hp` points at those words; `write_map` writes every word of the
    // produced map (flat or HAMT) and the returned term borrows `hp`'s storage,
    // which the owning `heap` keeps live and unmoved.
    Ok(unsafe { beam_term::map::write_map(hp, pairs, atoms)? })
}

/// Writes a flat map of any non-empty pair count at `hp` and returns the tagged
/// root term.
///
/// # Why this lives here
///
/// The lowest common ancestor cannot produce a flat map with more than
/// [`MAP_SMALL_MAP_LIMIT`] pairs: [`beam_term::thing::write_flatmap`] asserts that
/// limit and [`beam_term::map::write_map`] switches to a **hash map** (a different
/// term kind with a different interior) above it. `beam_codec` needs the oversized
/// *flat* map nonetheless: our decoder materializes **every** `MAP_EXT` image as one
/// flat map, and the encoder must re-emit it byte-exactly in the map's stored order
/// (see the crate `CONTEXT.md`, §"Map key encode order"). This writer is that
/// missing LCA entry point, verbatim: it is byte-for-byte the layout
/// [`beam_term::thing::write_flatmap`] emits for the small case — pinned by
/// `oversized_flatmap_layout_matches_beam_term` — so a map of any pair count
/// round-trips as a flat map and every existing reader still applies.
///
/// Layout (`erl_map.c:673-700`, `erl_map.h:181-199`): the word block
/// `[arityval(n), k0 … k(n-1), MAP_HEADER_FLATMAP, n, keys, v0 … v(n-1)]`, where
/// `keys = make_tuple(hp)` points back at the base of the keys seq and the root is
/// the flatmap head `make_boxed(hp + 1 + n)`.
///
/// # Safety
///
/// `hp` must point at [`beam_term::thing::flatmap_size_words`]`(pairs.len())`
/// exclusively-owned, writable words, which are all written by this call and must
/// stay live (and unmoved) for as long as the returned term is used.
/// `0 < pairs.len() <= MAX_ARITYVAL`.
unsafe fn write_large_flatmap(hp: *mut Eterm, pairs: &[(Eterm, Eterm)]) -> Eterm {
    let n = pairs.len();
    debug_assert!(
        n > 0 && n <= MAX_ARITYVAL,
        "a non-empty flat map fits the arityval header"
    );
    // SAFETY: the caller reserved `4 + 2n` words at `hp`. The keys tuple occupies
    // `hp[0..=n]`, the flatmap head prefix `[head, size, keys]` occupies
    // `hp[1+n..=3+n]`, and the values occupy `hp[4+n..4+2n]` — every write below is
    // in bounds (`n > 0`, so this writer never has to handle the empty block).
    unsafe {
        *hp = make_arityval(n);
        for i in 0..n {
            *hp.add(1 + i) = pairs[i].0;
        }
        *hp.add(1 + n) = make_header(4, MAP_SUBTAG);
        *hp.add(2 + n) = Eterm::from_raw(n);
        *hp.add(3 + n) = make_tuple(hp);
        for j in 0..n {
            *hp.add(4 + n + j) = pairs[j].1;
        }
        make_boxed(hp.add(1 + n))
    }
}
/// Splits a flat map into its `(keys, values)`.
///
/// Both slices are borrowed from the term's storage (the caller must keep the
/// owning heap alive), have the same length and are in wire order. Delegates to
/// [`beam_term::thing::flatmap_parts`]. A term that is not a *flat* map (or is a
/// hashmap) yields `BeamError::BadArg`.
pub fn flatmap_parts<'a>(term: Eterm) -> Result<(&'a [Eterm], &'a [Eterm])> {
    // SAFETY: `is_flatmap` reads only the header word of a boxed term (the caller
    // invariant for any `Eterm`).
    if !unsafe { is_flatmap(term) } {
        return Err(error::bad_arg());
    }
    // A well-formed flatmap never exceeds `MAX_ARITYVAL` pairs (the `arityval`
    // header's 24-bit size field). The root points at the `[head, size, keys]`
    // prefix, so word `1` is the raw pair count; a larger value means a
    // corrupt/mismatched block and is rejected before any slice is built.
    // SAFETY: `is_flatmap` established that `term` is a live flat map, so word `1` of
    // its head prefix is a live, initialised word.
    let n = unsafe { (*boxed_val(term).add(1)).raw() };
    if n > MAX_ARITYVAL {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_flatmap` established that `term` is a live flat map; the caller
    // keeps the owning heap alive for `'a`.
    Ok(unsafe { thing::flatmap_parts(term) })
}

/// Every `(key, value)` pair of the map `term` in the order the map **stores** them.
///
/// A flat map yields its keys-tuple order; a hash map (HAMT) yields its node-slot
/// pre-order (slots ascending, descending depth-first at each). Delegates to
/// [`beam_term::map::map_pairs_storage_order`]. A term that is not a map (or is an
/// immediate) yields `BeamError::BadArg`.
///
/// This is the reader the encoder uses for a **hash map** (a map [`flatmap_parts`]
/// rejects). `beam_term` drives the HAMT trie with the bit-exact `erts_map_hash`, so a
/// `write_map`-built hash map has the same node structure as the oracle's; the encoder
/// emits its pairs in the **reverse** of this order, matching the C encoder's LIFO
/// work-stack walk (see `encode::Encoder::encode_hashmap_pairs` and the crate
/// `CONTEXT.md`, §"Map key encode order").
pub fn map_pairs_storage_order(term: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    beam_term::map::map_pairs_storage_order(term).map_err(Into::into)
}

/// Materializes an external pid on `heap`.
///
/// The `[header, node, next, data.pid]` layout is written by
/// [`beam_term::thing::write_external_pid`]; `number` and `serial` are the two
/// `Uint32`s of `data.pid` (`num` first, then `ser`).
///
/// **M1 substitution:** there is no node registry yet, so the `node` slot holds the
/// sysname atom term and the unused off-heap-link (`next`) slot holds `creation`;
/// the word positions and arity are exactly C's (see the crate `CONTEXT.md`,
/// "Known Issues").
pub fn make_external_pid(
    heap: &mut Heap,
    node: Eterm,
    number: u32,
    serial: u32,
    creation: u32,
) -> Result<Eterm> {
    // SAFETY: `alloc_words` returned `thing::external_pid_size_words()`
    // exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(thing::external_pid_size_words())? };
    // SAFETY: `hp` owns exactly those words, which `write_external_pid` fully
    // initialises.
    Ok(unsafe { thing::write_external_pid(hp, node, number, serial, creation) })
}

/// Splits an external pid term into `(node, number, serial, creation)`.
///
/// Delegates to [`beam_term::thing::external_pid_parts`]. A term that is not an
/// external pid, or whose header arity is not `EXTERNAL_PID_HEAP_SIZE - 1`, yields
/// `BeamError::BadArg`.
pub fn external_pid_parts(term: Eterm) -> Result<(Eterm, u32, u32, u32)> {
    // SAFETY: `is_external_pid` reads only the header word of a boxed term (the
    // caller invariant for any `Eterm`).
    if !unsafe { is_external_pid(term) } {
        return Err(error::bad_arg());
    }
    // The header arity fixes the block size; a mismatch is a corrupt/mismatched block,
    // not a valid pid.
    // SAFETY: `is_external_pid` established that `term` is boxed, so its header word is
    // a live, initialised term word.
    if arityval(unsafe { *boxed_val(term) }) + 1 != thing::external_pid_size_words() {
        return Err(error::bad_arg());
    }
    // SAFETY: the guards established that `term` is a live external pid of the fixed
    // `external_pid_size_words()` block.
    Ok(unsafe { thing::external_pid_parts(term) })
}

/// Materializes an external port on `heap`.
///
/// The `[header, node, next, data.port.id]` layout is written by
/// [`beam_term::thing::write_external_port`]; `number` is the 64-bit `V4_PORT_EXT`
/// id and the `node`/`next` slots carry the sysname atom and `creation` (the M1
/// substitution described on [`make_external_pid`]).
pub fn make_external_port(
    heap: &mut Heap,
    node: Eterm,
    number: u64,
    creation: u32,
) -> Result<Eterm> {
    // SAFETY: `alloc_words` returned `thing::external_port_size_words()`
    // exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(thing::external_port_size_words())? };
    // SAFETY: `hp` owns exactly those words, which `write_external_port` fully
    // initialises.
    Ok(unsafe { thing::write_external_port(hp, node, number, creation) })
}

/// Splits an external port term into `(node, number, creation)`.
///
/// Delegates to [`beam_term::thing::external_port_parts`]. A term that is not an
/// external port, or whose header arity is not `EXTERNAL_PORT_HEAP_SIZE - 1`, yields
/// `BeamError::BadArg`.
pub fn external_port_parts(term: Eterm) -> Result<(Eterm, u64, u32)> {
    // SAFETY: `is_external_port` reads only the header word of a boxed term (the
    // caller invariant for any `Eterm`).
    if !unsafe { is_external_port(term) } {
        return Err(error::bad_arg());
    }
    // The header arity fixes the block size; a mismatch is a corrupt/mismatched block,
    // not a valid port.
    // SAFETY: `is_external_port` established that `term` is boxed, so its header word
    // is a live, initialised term word.
    if arityval(unsafe { *boxed_val(term) }) + 1 != thing::external_port_size_words() {
        return Err(error::bad_arg());
    }
    // SAFETY: the guards established that `term` is a live external port of the fixed
    // `external_port_size_words()` block.
    Ok(unsafe { thing::external_port_parts(term) })
}

/// Materializes an external reference on `heap`.
///
/// The `[header, node, next, data.ui32]` layout (id **count** first, then the ids) is
/// written by [`beam_term::thing::write_external_ref`]; the `node`/`next` slots carry
/// the sysname atom and `creation` (the M1 substitution described on
/// [`make_external_pid`]) and `ids` are the 32-bit `NEWER_REFERENCE_EXT` identifier
/// words. Any count up to `MAX_REF_IDS` is accepted, **including zero**: a zero-id
/// external reference is exactly what a `len == 0` `NEW_REFERENCE_EXT` /
/// `NEWER_REFERENCE_EXT` image decodes to (the 4-word `[header, node, creation,
/// count = 0]` block `write_external_ref` builds, re-encoding as the 11-byte
/// `[90][0][node][creation]`). More than `MAX_REF_IDS` ids yields
/// `BeamError::SystemLimit`.
pub fn make_external_ref(
    heap: &mut Heap,
    node: Eterm,
    ids: &[u32],
    creation: u32,
) -> Result<Eterm> {
    let n = ids.len();
    if n > MAX_REF_IDS {
        return Err(error::system_limit());
    }
    // SAFETY: `alloc_words` returned `thing::external_ref_size_words(n)`
    // exclusively-owned, writable words.
    let hp = unsafe { heap.alloc_words(thing::external_ref_size_words(n))? };
    // SAFETY: `hp` owns `external_ref_size_words(n)` words and `n <= MAX_REF_IDS`,
    // the preconditions `write_external_ref` documents.
    Ok(unsafe { thing::write_external_ref(hp, node, ids, creation) })
}

/// Splits an external reference term into `(node, ids, creation)`.
///
/// `ids` is borrowed from the term's storage (the caller must keep the owning heap
/// alive). Delegates to [`beam_term::thing::external_ref_parts`]. A zero-id external
/// reference reports an empty slice. A term that is not an external reference, or
/// whose id count does not fit its data section, yields `BeamError::BadArg`.
pub fn external_ref_parts<'a>(term: Eterm) -> Result<(Eterm, &'a [u32], u32)> {
    // SAFETY: `is_external_ref` reads only the header word of a boxed term (the
    // caller invariant for any `Eterm`).
    if !unsafe { is_external_ref(term) } {
        return Err(error::bad_arg());
    }
    // `data_words` = `arityval(header) + 1 - EXTERNAL_THING_HEAD_SIZE`; a header too
    // small for the `[header, node, next]` head is corrupt.
    // SAFETY: `is_external_ref` established that `term` is boxed, so its header word is
    // a live, initialised term word.
    let Some(data_words) = arityval(unsafe { *boxed_val(term) })
        .checked_sub(EXTERNAL_THING_HEAD_SIZE - 1)
    else {
        return Err(error::bad_arg());
    };
    // The count is self-describing (`external_ref_no_numbers(x) = external_ref_data(x)[0]`),
    // so it must fit the data section it was read from before any slice is built. A
    // zero count is legal (a `len == 0` wire reference).
    // SAFETY: the header arity fixed the block at `EXTERNAL_THING_HEAD_SIZE +
    // data_words` words, so the `data.ui32` section at word `EXTERNAL_THING_HEAD_SIZE`
    // is live and its first `u32` is initialised.
    let n = unsafe { *boxed_val(term).add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>() as usize };
    if n > MAX_REF_IDS || n + 1 > data_words * U32_PER_WORD {
        return Err(error::bad_arg());
    }
    // SAFETY: the guards established that `term` is a live external reference whose id
    // count fits its data section; the caller keeps the owning heap alive for `'a`.
    Ok(unsafe { thing::external_ref_parts(term) })
}

/// The arity of a tuple term.
///
/// Delegates to [`beam_term::tuple_size`]. A term that is not a tuple yields
/// `BeamError::BadArg`.
pub fn tuple_arity(term: Eterm) -> Result<usize> {
    // SAFETY: `is_tuple` reads only the header word of a boxed term (the caller
    // invariant for any `Eterm`).
    if !unsafe { is_tuple(term) } {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_tuple` established that `term` is a live tuple whose header carries
    // the arity, the precondition `tuple_size` documents.
    Ok(unsafe { beam_term::tuple_size(term) })
}

/// The element at `index` of a tuple term, zero-based.
///
/// Delegates to [`beam_term::tuple_element`]. An out-of-range index or a term that is
/// not a tuple yields `BeamError::BadArg`.
pub fn tuple_element(term: Eterm, index: usize) -> Result<Eterm> {
    let arity = tuple_arity(term)?;
    if index >= arity {
        return Err(error::bad_arg());
    }
    // SAFETY: `tuple_arity` established that `term` is a live tuple of `arity`
    // elements and `index < arity`, the bounds `tuple_element` requires.
    Ok(unsafe { beam_term::tuple_element(term, index) })
}

/// The head (`CAR`) of a cons cell.
///
/// Delegates to [`beam_term::car`] (`erl_term.h`; `NIL` is an immediate, not a cons
/// cell, and yields `BeamError::BadArg`).
pub fn cons_head(term: Eterm) -> Result<Eterm> {
    // `is_list` reads only the primary tag of the word, never dereferences, so it is
    // safe for any `Eterm`.
    if !is_list(term) {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_list` established that `term` is a header-less 2-word `[CAR, CDR]`
    // cons cell, the precondition `car` documents.
    Ok(unsafe { beam_term::car(term) })
}

/// The tail (`CDR`) of a cons cell.
///
/// Delegates to [`beam_term::cdr`] (`erl_term.h`; `NIL` is an immediate, not a cons
/// cell, and yields `BeamError::BadArg`).
pub fn cons_tail(term: Eterm) -> Result<Eterm> {
    // `is_list` reads only the primary tag of the word, never dereferences, so it is
    // safe for any `Eterm`.
    if !is_list(term) {
        return Err(error::bad_arg());
    }
    // SAFETY: `is_list` established that `term` is a header-less 2-word `[CAR, CDR]`
    // cons cell, the precondition `cdr` documents.
    Ok(unsafe { beam_term::cdr(term) })
}

#[cfg(test)]
mod tests {
    //! Raw-word unit tests: every block layout is asserted word-by-word against the
    //! legacy C ground truth, and every accessor is checked for its error behaviour
    //! on the wrong term type / out-of-range index.
    //!
    //! `unsafe` below only dereferences pointers into a `Heap` this test has just
    //! allocated and written itself (and, for the parts accessors, reads back the
    //! same words); each block has its own `// SAFETY:` note.

    use beam_alloc::Heap;
    use beam_common::{BeamError, Result};
    use beam_term::prelude::{
        is_bignum_header, is_external_pid_header, is_external_port_header, is_external_ref_header,
        is_flatmap_header, is_map_header, make_arityval, make_header, make_small, make_tuple,
        tuple0, BIG_SIGN_BIT, EXTERNAL_PID_SUBTAG, EXTERNAL_PORT_SUBTAG, EXTERNAL_REF_SUBTAG,
        HEADER_FLONUM, MAP_SUBTAG, NEG_BIG_SUBTAG, NIL, POS_BIG_SUBTAG,
    };

    use super::*;

    /// A heap large enough for every block built in this module (shared with the
    /// [`fun`](crate::term::fun) submodule's tests).
    pub(crate) fn new_heap() -> Heap {
        Heap::new(256).expect("the carrier allocator never fails for a tiny heap")
    }

    /// Asserts a codec call failed with `BeamError::BadArg` (`BeamError` is not
    /// `PartialEq`, so `matches!` is used).
    pub(crate) fn assert_bad_arg<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::BadArg) => {}
            other => panic!("expected BadArg, got {other:?}"),
        }
    }

    /// Asserts a codec call failed with `BeamError::SystemLimit`.
    pub(crate) fn assert_system_limit<T: core::fmt::Debug>(result: Result<T>) {
        match result {
            Err(BeamError::SystemLimit) => {}
            other => panic!("expected SystemLimit, got {other:?}"),
        }
    }

    #[test]
    fn float_block_and_bit_patterns() {
        for value in [
            3.14_f64,
            0.0,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            f64::MIN,
            f64::MAX,
        ] {
            let mut heap = new_heap();
            let term = make_float(&mut heap, value).expect("2 words fit");
            assert_eq!(heap.used(), 2);

            // SAFETY: `term` is a float this test just built on `heap`, so the block
            // `[HEADER_FLONUM, bits]` is live and word `1` is initialised.
            unsafe {
                let p = boxed_val(term);
                assert_eq!(*p, HEADER_FLONUM);
                assert_eq!(*p.add(1), Eterm::from_raw(value.to_bits() as usize));
            }
            // Compare by bit pattern so NaN round-trips too.
            assert_eq!(float_value(term).unwrap().to_bits(), value.to_bits());
        }
    }

    #[test]
    fn float_value_rejects_non_floats() {
        let mut heap = new_heap();
        assert_bad_arg(float_value(make_small(7)));
        assert_bad_arg(float_value(NIL));
        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        assert_bad_arg(float_value(tuple));
    }

    #[test]
    fn bignum_block_layout_and_parts() {
        let mut heap = new_heap();
        let digits = [0x0A_u64, 0x0B, 0x0C];
        let term = make_big(&mut heap, false, &digits).expect("fits");
        // `BIG_NEED_SIZE(3) = 3 + 1`: the header word plus one `ErtsDigit` per word.
        assert_eq!(heap.used(), 4);

        // SAFETY: `term` is a bignum this test just built; word `0` is its header and
        // the digit words follow it (`BIG_V(xp) = (ErtsDigit *)((xp) + 1)`).
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, make_header(3, POS_BIG_SUBTAG));
            assert!(is_bignum_header(*p));
            assert_eq!(arityval(*p), digits.len());
            assert_eq!((*p).raw() & BIG_SIGN_BIT, 0);
            let stored = core::slice::from_raw_parts(p.add(1).cast::<u64>(), digits.len());
            assert_eq!(stored, &digits[..]);
        }

        let (negative, got) = big_parts(term).unwrap();
        assert!(!negative);
        assert_eq!(got, &digits[..]);
    }

    #[test]
    fn bignum_sign_bit_and_digit_words() {
        let mut heap = new_heap();
        let term = make_big(&mut heap, true, &[u64::MAX]).expect("fits");
        assert_eq!(heap.used(), 2, "header + one digit word");

        // SAFETY: `term` is a bignum this test just built; read its header and the
        // single digit word.
        unsafe {
            let p = boxed_val(term);
            assert_eq!(*p, make_header(1, NEG_BIG_SUBTAG));
            assert_ne!((*p).raw() & BIG_SIGN_BIT, 0);
            assert_eq!((*p.add(1)).raw() as u64, u64::MAX);
        }

        let (negative, got) = big_parts(term).unwrap();
        assert!(negative);
        assert_eq!(got, &[u64::MAX]);
    }

    #[test]
    fn bignum_rejects_empty_and_over_long_digits() {
        let mut heap = new_heap();
        assert_bad_arg(make_big(&mut heap, false, &[]));
        assert_eq!(heap.used(), 0, "the rejected bignum allocated nothing");

        let too_many = vec![0u64; BIG_ARITY_MAX + 1];
        assert_system_limit(make_big(&mut heap, false, &too_many));
        assert_eq!(heap.used(), 0);

        // Exactly `BIG_ARITY_MAX` digits is still accepted.
        let mut big_heap = Heap::new(BIG_ARITY_MAX + 1).expect("a 64 KiB word heap");
        let at_max = vec![1u64; BIG_ARITY_MAX];
        let term = make_big(&mut big_heap, false, &at_max).expect("at the digit bound");
        assert_eq!(big_parts(term).unwrap().1, &at_max[..]);
    }

    #[test]
    fn big_parts_rejects_non_bignums() {
        let mut heap = new_heap();
        let float = make_float(&mut heap, 1.0).unwrap();
        assert_bad_arg(big_parts(make_small(1)));
        assert_bad_arg(big_parts(float));
    }

    #[test]
    fn flatmap_empty_is_exactly_three_words() {
        let mut heap = new_heap();
        let base = heap.hp();
        let map = make_flatmap(&mut heap, &[]).expect("fits");
        assert_eq!(heap.used(), 3, "the empty flatmap is 3 words");

        // SAFETY: `base` is the first word of the 3-word block just written.
        unsafe {
            assert_eq!(*base, make_header(4, MAP_SUBTAG));
            assert!(is_map_header(*base));
            assert!(is_flatmap_header(*base));
            assert_eq!((*base.add(1)).raw(), 0);
            assert_eq!(*base.add(2), tuple0());
        }
        assert_eq!(boxed_val(map), base, "root = make_boxed(hp)");
        let (keys, values) = flatmap_parts(map).unwrap();
        assert!(keys.is_empty());
        assert!(values.is_empty());
    }

    #[test]
    fn flatmap_two_pairs_word_by_word() {
        let mut heap = new_heap();
        let base = heap.hp();
        let k0 = make_small(10);
        let v0 = make_small(100);
        let k1 = make_small(20);
        let v1 = make_small(200);
        let map = make_flatmap(&mut heap, &[(k0, v0), (k1, v1)]).expect("fits");
        assert_eq!(heap.used(), 4 + 2 * 2);

        // SAFETY: `base` is the first word of the `4 + 2n` block just written; every
        // index below is within that block.
        unsafe {
            assert_eq!(*base, make_arityval(2)); // word 0: keys-tuple arity
            assert_eq!(*base.add(1), k0); // word 1: k0
            assert_eq!(*base.add(2), k1); // word 2: k1
            assert_eq!(*base.add(3), make_header(4, MAP_SUBTAG)); // word 1+n: head
            assert!(is_flatmap_header(*base.add(3)));
            assert_eq!((*base.add(4)).raw(), 2); // word 2+n: RAW size == n
            assert_eq!(*base.add(5), make_tuple(base)); // word 3+n: keys tuple
            assert_eq!(*base.add(6), v0); // word 4+n: v0
            assert_eq!(*base.add(7), v1); // word 7: v1

            // The keys tuple term (word `3+n`) points back at the block base, and the
            // flatmap root is the head at `hp + 1 + n` (`flatmap_parts` reads the
            // keys/values relative to that head: keys at head+2, values at head+3).
            let keys_tup = *base.add(5); // word 3+n
            assert_eq!(boxed_val(keys_tup), base);
            let head = boxed_val(map);
            assert_eq!(head, base.add(3)); // hp + 1 + n
            assert_eq!(*head.add(2), keys_tup); // head+2 == keys tuple
            assert_eq!(*head.add(3), v0); // head+3 == first value
        }

        let (keys, values) = flatmap_parts(map).unwrap();
        assert_eq!(keys, &[k0, k1]);
        assert_eq!(values, &[v0, v1]);
    }

    #[test]
    fn flatmap_above_the_small_limit_uses_the_flatmap_layout() {
        // `MAP_SMALL_MAP_LIMIT` (32) + 8 = 40 pairs: past the small limit, so this
        // exercises the crate's own oversized-flatmap writer, whose layout must be
        // word-for-word the documented flatmap block (`erl_map.h:181-199`).
        let n = 40usize;
        let mut heap = new_heap();
        let base = heap.hp();
        let pairs: Vec<(Eterm, Eterm)> = (0..n)
            .map(|i| (make_small(i as i64), make_small(i as i64 + 1000)))
            .collect();
        let map = make_flatmap(&mut heap, &pairs).expect("a map of any size fits");
        assert_eq!(heap.used(), 4 + 2 * n, "the block is 4 + 2n words");

        // SAFETY: `base` is the first word of the `4 + 2n` block just written; every
        // index below is within that block.
        unsafe {
            assert_eq!(*base, make_arityval(n)); // word 0: keys-tuple arity
            for i in 0..n {
                assert_eq!(*base.add(1 + i), pairs[i].0); // words 1..=n: keys
            }
            assert_eq!(*base.add(1 + n), make_header(4, MAP_SUBTAG)); // word 1+n: head
            assert!(is_flatmap_header(*base.add(1 + n)));
            assert_eq!((*base.add(2 + n)).raw(), n); // word 2+n: RAW size == n
            assert_eq!(*base.add(3 + n), make_tuple(base)); // word 3+n: keys tuple
            for j in 0..n {
                assert_eq!(*base.add(4 + n + j), pairs[j].1); // words 4+n..: values
            }
            // The root is the flatmap head at `hp + 1 + n`, exactly as the small path.
            assert_eq!(boxed_val(map), base.add(1 + n));
        }

        let (keys, values) = flatmap_parts(map).unwrap();
        assert_eq!(keys.len(), n);
        assert_eq!(values.len(), n);
        assert_eq!(keys, &pairs.iter().map(|p| p.0).collect::<Vec<_>>()[..]);
        assert_eq!(values, &pairs.iter().map(|p| p.1).collect::<Vec<_>>()[..]);
    }

    #[test]
    fn oversized_flatmap_layout_matches_beam_term() {
        // The retained writer is the LCA's flatmap writer with its `MAP_SMALL_MAP_LIMIT`
        // bound lifted, so the same pairs must produce the same block word for word.
        // Checked at a small pair count, where both writers are in range.
        let pairs: Vec<(Eterm, Eterm)> = (0..4)
            .map(|i| (make_small(i as i64), make_small(i as i64 + 1000)))
            .collect();
        let n = pairs.len();
        let words = thing::flatmap_size_words(n);
        let mut heap = Heap::new(2 * words).expect("a tiny heap");
        // SAFETY: `alloc_words` returned `words` exclusively-owned, writable words per
        // block, and `n <= MAP_SMALL_MAP_LIMIT` is inside both writers' documented range.
        let (lca_hp, lca, ours_hp, ours) = unsafe {
            let lca_hp = heap.alloc_words(words).unwrap();
            let ours_hp = heap.alloc_words(words).unwrap();
            (
                lca_hp,
                thing::write_flatmap(lca_hp, &pairs),
                ours_hp,
                write_large_flatmap(ours_hp, &pairs),
            )
        };
        // SAFETY: both blocks are the `4 + 2n` words the calls above just initialised.
        let (lca_words, ours_words) = unsafe {
            (
                core::slice::from_raw_parts(lca_hp, words),
                core::slice::from_raw_parts(ours_hp, words),
            )
        };
        for i in 0..words {
            if i == 3 + n {
                // The keys tuple term points back at the block base, so its raw word is
                // block-specific: compare the *offset* it encodes, not the address.
                assert_eq!(boxed_val(lca_words[i]), lca_hp, "LCA keys tuple");
                assert_eq!(boxed_val(ours_words[i]), ours_hp, "our keys tuple");
            } else {
                assert_eq!(lca_words[i], ours_words[i], "word {i} of the flat map");
            }
        }
        // The same root offset: the flatmap head at `hp + 1 + n`.
        assert_eq!(boxed_val(lca), lca_hp.wrapping_add(1 + n));
        assert_eq!(boxed_val(ours), ours_hp.wrapping_add(1 + n));
    }    #[test]
    fn flatmap_parts_rejects_non_maps_and_a_corrupt_size() {
        let mut heap = new_heap();
        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        assert_bad_arg(flatmap_parts(tuple));
        assert_bad_arg(flatmap_parts(make_small(1)));

        // A size word past `MAX_ARITYVAL` cannot belong to a well-formed flatmap
        // (`flatmap_parts` reads it from the head prefix) and is rejected before any
        // slice is built.
        let map = make_flatmap(&mut heap, &[(make_small(1), make_small(2))]).unwrap();
        // SAFETY: `map` is the 2-pair flatmap just built; word 1 of its head prefix is
        // the raw size field. The term is dropped afterwards.
        unsafe {
            let size_word = boxed_val(map).add(1);
            *size_word = Eterm::from_raw(MAX_ARITYVAL + 1);
        }
        assert_bad_arg(flatmap_parts(map));
    }

    #[test]
    fn external_pid_block_layout_and_parts() {
        let mut heap = new_heap();
        let node = make_small(7);
        let pid =
            make_external_pid(&mut heap, node, 0xAAAA_BBBB, 0xCCCC_DDDD, 0x1234_5678).expect("fits");
        assert_eq!(heap.used(), thing::external_pid_size_words());

        // SAFETY: `pid` is the 4-word block this test just wrote; word `3` is the
        // `data.pid = { Uint32 num; Uint32 ser; }` union.
        unsafe {
            let p = boxed_val(pid);
            assert_eq!(
                *p,
                make_header(thing::external_pid_size_words() - 1, EXTERNAL_PID_SUBTAG)
            );
            assert!(is_external_pid_header(*p));
            assert_eq!(*p.add(1), node);
            assert_eq!(
                (*p.add(2)).raw() as u32,
                0x1234_5678,
                "creation lives in the off-heap-link slot"
            );
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, 0xAAAA_BBBB);
            assert_eq!(*data.add(1), 0xCCCC_DDDD);
        }

        let (n, num, ser, cre) = external_pid_parts(pid).unwrap();
        assert_eq!(
            (n, num, ser, cre),
            (node, 0xAAAA_BBBB, 0xCCCC_DDDD, 0x1234_5678)
        );
        assert_bad_arg(external_pid_parts(make_small(1)));
        // A block of the right arity but the wrong subtag is rejected.
        assert_bad_arg(external_pid_parts(
            make_external_port(&mut heap, node, 1, 2).unwrap(),
        ));
    }

    #[test]
    fn external_port_block_layout_and_parts() {
        let mut heap = new_heap();
        let node = make_small(9);
        let port = make_external_port(&mut heap, node, 0x1122_3344_5566_7788, 0x99AA_BBCC)
            .expect("fits");
        assert_eq!(heap.used(), thing::external_port_size_words());

        // SAFETY: `port` is the 4-word block this test just wrote; word `3` is the
        // 64-bit `data.port.id` (`ARCH_64`).
        unsafe {
            let p = boxed_val(port);
            assert_eq!(
                *p,
                make_header(thing::external_port_size_words() - 1, EXTERNAL_PORT_SUBTAG)
            );
            assert!(is_external_port_header(*p));
            assert_eq!(*p.add(1), node);
            assert_eq!((*p.add(2)).raw() as u32, 0x99AA_BBCC);
            assert_eq!((*p.add(3)).raw() as u64, 0x1122_3344_5566_7788);
        }

        let (n, num, cre) = external_port_parts(port).unwrap();
        assert_eq!((n, num, cre), (node, 0x1122_3344_5566_7788, 0x99AA_BBCC));
        assert_bad_arg(external_port_parts(make_small(1)));
        // A pid of the same size but a different subtag is not a port.
        let pid = make_external_pid(&mut heap, node, 1, 2, 3).unwrap();
        assert_bad_arg(external_port_parts(pid));
    }

    #[test]
    fn external_ref_block_layout_and_parts() {
        let mut heap = new_heap();
        let node = make_small(11);
        let ids = [0x1111_1111u32, 0x2222_2222, 0x3333_3333];
        let r = make_external_ref(&mut heap, node, &ids, 0xABCD_1234).expect("fits");
        // `EXTERNAL_THING_HEAD_SIZE` + `DW` = 3 + (3 / 2 + 1) = 5 words.
        assert_eq!(heap.used(), EXTERNAL_THING_HEAD_SIZE + 3 / U32_PER_WORD + 1);

        // SAFETY: `r` is the 5-word block this test just wrote. The data section is the
        // `data.ui32` array at word 3: the id count first (`bif.c:4840`), then the ids.
        unsafe {
            let p = boxed_val(r);
            assert_eq!(
                *p,
                make_header(3 / U32_PER_WORD + 1 + EXTERNAL_THING_HEAD_SIZE - 1, EXTERNAL_REF_SUBTAG)
            );
            assert!(is_external_ref_header(*p));
            assert_eq!(arityval(*p) + 1, heap.used());
            assert_eq!(*p.add(1), node);
            assert_eq!((*p.add(2)).raw() as u32, 0xABCD_1234);
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, ids.len() as u32, "the id count heads the data section");
            assert_eq!(*data.add(1), ids[0]);
            assert_eq!(*data.add(2), ids[1]);
            assert_eq!(*data.add(3), ids[2]);
        }

        let (n, got, cre) = external_ref_parts(r).unwrap();
        assert_eq!(n, node);
        assert_eq!(got, &ids[..]);
        assert_eq!(cre, 0xABCD_1234);
        assert_bad_arg(external_ref_parts(make_small(1)));
    }

    #[test]
    fn external_ref_single_id_and_bounds() {
        let mut heap = new_heap();
        let r = make_external_ref(&mut heap, NIL, &[0xDEAD_BEEF], 0x01).expect("fits");
        // `DW = 1 / 2 + 1 = 1`; the head (3 words) + the 1-word data section.
        assert_eq!(heap.used(), EXTERNAL_THING_HEAD_SIZE + 1);
        // SAFETY: `r` is the 4-word block this test just wrote.
        unsafe {
            let p = boxed_val(r);
            assert_eq!(
                *p,
                make_header(1 / U32_PER_WORD + 1 + EXTERNAL_THING_HEAD_SIZE - 1, EXTERNAL_REF_SUBTAG)
            );
            assert_eq!(*p.add(1), NIL);
            assert_eq!((*p.add(2)).raw() as u32, 0x01);
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, 1);
            assert_eq!(*data.add(1), 0xDEAD_BEEF);
        }
        let (n, ids, cre) = external_ref_parts(r).unwrap();
        assert_eq!(n, NIL);
        assert_eq!(ids, &[0xDEAD_BEEF]);
        assert_eq!(cre, 0x01);

        // A zero-id reference is representable (`external_ref_size_words(0) == 4`).
        let zero = make_external_ref(&mut heap, NIL, &[], 0).expect("a zero-id ref fits");
        assert_eq!(external_ref_parts(zero).unwrap().1, &[] as &[u32]);
        // More than `MAX_REF_IDS` ids is a hard bound.
        assert_system_limit(make_external_ref(&mut heap, NIL, &[1, 2, 3, 4, 5, 6], 0));
        // `MAX_REF_IDS` itself is accepted.
        let max = make_external_ref(&mut heap, NIL, &[1, 2, 3, 4, 5], 0).expect("fits");
        assert_eq!(external_ref_parts(max).unwrap().1, &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn external_ref_zero_id_block_layout_and_parts() {
        let mut heap = new_heap();
        let r = make_external_ref(&mut heap, NIL, &[], 0x0123_4567).expect("fits");
        // `external_ref_size_words(0) = 3 + 1 = 4` words: the head plus the one-word
        // data section holding the zero count.
        assert_eq!(heap.used(), EXTERNAL_THING_HEAD_SIZE + 1);
        // SAFETY: `r` is the 4-word block this test just wrote; the data section holds
        // the zero id count and no ids.
        unsafe {
            let p = boxed_val(r);
            assert_eq!(
                *p,
                make_header(0 / U32_PER_WORD + 1 + EXTERNAL_THING_HEAD_SIZE - 1, EXTERNAL_REF_SUBTAG)
            );
            assert_eq!(*p.add(1), NIL);
            assert_eq!((*p.add(2)).raw() as u32, 0x0123_4567);
            let data = p.add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            assert_eq!(*data, 0, "the id count is zero");
        }
        let (n, ids, cre) = external_ref_parts(r).unwrap();
        assert_eq!(n, NIL);
        assert_eq!(ids, &[] as &[u32]);
        assert_eq!(cre, 0x0123_4567);
    }

    #[test]
    fn external_ref_parts_rejects_a_corrupt_id_count() {
        let mut heap = new_heap();
        let r = make_external_ref(&mut heap, NIL, &[0xAA, 0xBB], 0).expect("fits");
        // SAFETY: `r` is a block this test owns; the data section starts at word 3 and
        // is overwritten with an id count the block cannot hold. The term is dropped
        // afterwards, so no accessor ever sees the corrupt state.
        unsafe {
            let data = boxed_val(r).add(EXTERNAL_THING_HEAD_SIZE).cast::<u32>();
            *data = (MAX_REF_IDS + 1) as u32;
        }
        assert_bad_arg(external_ref_parts(r));
    }

    #[test]
    fn tuple_accessors_round_trip_and_bounds() {
        let mut heap = new_heap();
        let a = make_small(1);
        let b = make_small(2);
        let c = make_small(3);
        let t = heap.tuple(&[a, b, c]).unwrap();

        assert_eq!(tuple_arity(t).unwrap(), 3);
        assert_eq!(tuple_element(t, 0).unwrap(), a);
        assert_eq!(tuple_element(t, 1).unwrap(), b);
        assert_eq!(tuple_element(t, 2).unwrap(), c);
        assert_bad_arg(tuple_element(t, 3));
        assert_bad_arg(tuple_element(t, usize::MAX));

        // Not a tuple: a small int, `NIL` and a boxed non-tuple.
        assert_bad_arg(tuple_arity(a));
        assert_bad_arg(tuple_arity(NIL));
        assert_bad_arg(tuple_element(NIL, 0));
        let float = make_float(&mut heap, 1.0).unwrap();
        assert_bad_arg(tuple_arity(float));

        // The zero-arity tuple is still a tuple.
        assert_eq!(tuple_arity(tuple0()).unwrap(), 0);
        assert_bad_arg(tuple_element(tuple0(), 0));
    }

    #[test]
    fn cons_accessors_round_trip_and_reject_nil() {
        let mut heap = new_heap();
        let proper = heap.cons(make_small(1), NIL).unwrap();
        assert_eq!(cons_head(proper).unwrap(), make_small(1));
        assert_eq!(cons_tail(proper).unwrap(), NIL);

        // An improper list: the tail is a non-nil term.
        let improper = heap.cons(make_small(1), make_small(2)).unwrap();
        assert_eq!(cons_head(improper).unwrap(), make_small(1));
        assert_eq!(cons_tail(improper).unwrap(), make_small(2));

        // `NIL` is an immediate, not a cons cell, and neither is a tuple.
        assert_bad_arg(cons_head(NIL));
        assert_bad_arg(cons_tail(NIL));
        assert_bad_arg(cons_head(make_small(3)));
        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        assert_bad_arg(cons_tail(tuple));
    }
}
