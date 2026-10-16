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

//! Boxed terms: tagged pointers, header words, arityvals and the boxed sub-type
//! predicates.
//!
//! Ground truth: `erts/emulator/beam/erl_term.h` (boxed/list access methods, the
//! header layout and `*_SUBTAG` group) plus `erl_map.h`, `erl_bits.h` and
//! `erl_fun.h` for the thing layouts used by the sub-type tests.
//!
//! This module also owns the crate's canonical **M1 internal-reference thing**
//! layout ([`write_internal_ref`] / [`internal_ref_id`]) — the frozen 4-word
//! `[make_header(3, REF_SUBTAG), node, serial, id]` block this runtime actually
//! builds. [`internal_ref_numbers`] next to it describes the **C** layout only and
//! must not be fed an M1 reference.

use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::fun::{FUN_HEADER_IMPORTED_OFFS, FUN_HEADER_KIND_OFFS};
use crate::immediate::{is_small, make_small, signed_value, try_make_small};
use crate::tag::{
    ARITYVAL_SUBTAG, BIG_TAG_MASK, BIN_REF_SUBTAG, BITSTRING_TAG_MASK, EXTERNAL_TAG_MASK,
    EXTERNAL_PID_SUBTAG, EXTERNAL_PORT_SUBTAG, EXTERNAL_REF_SUBTAG, FLOAT_SUBTAG, FUN_SUBTAG,
    HAMT_SUBTAG_HEAD_ARRAY, HAMT_SUBTAG_HEAD_FLATMAP, HEADER_ARITY_OFFS,
    HEADER_MAP_HASHMAP_HEAD_MASK, HEADER_MAP_SUBTAG_MASK, HEADER_SUBTAG_MASK, HEAP_BITS_SUBTAG,
    MAP_HEADER_ARITY_SZ, MAP_HEADER_TAG_SZ, MAP_SUBTAG, MAX_ARITYVAL, POS_BIG_SUBTAG,
    RECORD_SUBTAG, REF_SUBTAG, SUB_BITS_SUBTAG, TAG_HEADER_MASK, TAG_PRIMARY_BOXED,
    TAG_PRIMARY_MASK, TAG_PTR_MASK, TRANSPARENT_TAG_MASK,
};

// ---------------------------------------------------------------------------
// Boxed pointers
// ---------------------------------------------------------------------------

/// `make_boxed(x)` — tag a header-word pointer as a boxed term.
///
/// Not `const`: pointer-to-integer casts are rejected during const evaluation.
#[inline]
pub fn make_boxed(ptr: *const Eterm) -> Eterm {
    Eterm::from_raw((ptr as usize) + TAG_PRIMARY_BOXED)
}

/// `boxed_val(x)` — the header-word pointer with the pointer tag (incl. the
/// literal bit) removed.
#[inline]
pub const fn boxed_val(x: Eterm) -> *mut Eterm {
    (x.raw() & !TAG_PTR_MASK) as *mut Eterm
}

/// `is_boxed(x)`.
///
/// The C bit trick `!((x) & (_TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED))` — that is
/// `(x & 0x1) == 0`, true for the `BOXED` tag (and the value-invalid `HEADER`
/// pattern). Mirrored verbatim for bit-exactness.
#[inline]
pub const fn is_boxed(x: Eterm) -> bool {
    (x.raw() & (TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED)) == 0
}

/// `is_not_boxed(x)`.
#[inline]
pub const fn is_not_boxed(x: Eterm) -> bool {
    !is_boxed(x)
}

/// Whether `x` is a **dereferenceable boxed term** — [`is_boxed`] *and* a term
/// word ([`crate::tag::is_term_word`]).
///
/// [`is_boxed`] mirrors the C bit trick `(x & (_TAG_PRIMARY_MASK - TAG_PRIMARY_BOXED)) == 0`
/// verbatim, so it is also true for every word whose primary tag is `HEADER` —
/// i.e. for the words that are **not terms at all**: the C ABI's `THE_NON_VALUE`
/// (`0`, or `make_header(0, FLOAT_SUBTAG)` in a debug build) and any other bare
/// header word.
///
/// The C's own boxed predicates dereference such a word (undefined behaviour).
/// In this crate, **every guard whose `is_boxed` test was guarding a
/// `*boxed_val(x)` read tests this predicate instead**, so a stray non-term (a NIF
/// that returned `THE_NON_VALUE`, say) is classified as "not a boxed term" rather
/// than dereferencing a garbage — in release, `NULL` — pointer. For every valid
/// term it agrees with [`is_boxed`] exactly.
#[inline]
pub const fn is_boxed_term(x: Eterm) -> bool {
    is_boxed(x) && crate::tag::is_term_word(x)
}

// ---------------------------------------------------------------------------
// Header words
// ---------------------------------------------------------------------------

/// `_make_header(sz, tag)`.
#[inline]
pub const fn make_header(size: usize, subtag: usize) -> Eterm {
    Eterm::from_raw((size << HEADER_ARITY_OFFS) + subtag)
}

/// `thing_subtag(x)` — the 4-bit header subtag.
#[inline]
pub const fn header_subtag(x: Eterm) -> u32 {
    (x.raw() & HEADER_SUBTAG_MASK) as u32
}

/// `MAP_HEADER_TYPE(Hdr)` — the 2-bit map tag.
#[inline]
pub const fn map_header_type(hdr: Eterm) -> u32 {
    ((hdr.raw() >> HEADER_ARITY_OFFS) & 0x3) as u32
}

/// `MAP_HEADER_ARITY(Hdr)`.
#[inline]
pub const fn map_header_arity(hdr: Eterm) -> u32 {
    ((hdr.raw() >> (HEADER_ARITY_OFFS + MAP_HEADER_TAG_SZ)) & 0xFF) as u32
}

/// `MAP_HEADER_VAL(Hdr)`.
#[inline]
pub const fn map_header_val(hdr: Eterm) -> u32 {
    ((hdr.raw() >> (HEADER_ARITY_OFFS + MAP_HEADER_TAG_SZ + MAP_HEADER_ARITY_SZ)) & 0xFFFF) as u32
}

/// `header_arity(x)` — the arity field of a header, with the map and fun
/// special cases of `_unchecked_header_arity` applied.
#[inline]
pub const fn header_arity(x: Eterm) -> usize {
    if is_map_header(x) {
        map_header_arity(x) as usize
    } else if is_fun_header(x) {
        ERL_FUN_SIZE - 1
    } else {
        x.raw() >> HEADER_ARITY_OFFS
    }
}

/// `thing_arityval(x)` — the arity of a thing header.
#[inline]
pub const fn thing_arityval(x: Eterm) -> usize {
    header_arity(x)
}

/// `header_is_transparent(x)` — true for arityvals and records, i.e. headers that
/// are not wrapped in a thing structure.
#[inline]
pub const fn header_is_transparent(x: Eterm) -> bool {
    ((x.raw() & TRANSPARENT_TAG_MASK) == ARITYVAL_SUBTAG)
        || ((x.raw() & TAG_HEADER_MASK) == RECORD_SUBTAG)
}

/// `header_is_thing(x)`.
#[inline]
pub const fn header_is_thing(x: Eterm) -> bool {
    !header_is_transparent(x)
}

// ---------------------------------------------------------------------------
// Arityvals (tuple headers)
// ---------------------------------------------------------------------------

/// `make_arityval_zero()` — the zero-arity tuple header word (`0`).
#[inline]
pub const fn make_arityval_zero() -> Eterm {
    make_header(0, ARITYVAL_SUBTAG)
}

/// `make_arityval_unchecked(sz)`.
#[inline]
pub const fn make_arityval_unchecked(size: usize) -> Eterm {
    debug_assert!(size <= MAX_ARITYVAL, "arityval out of range");
    make_header(size, ARITYVAL_SUBTAG)
}

/// `make_arityval(sz)` — unchecked (engine hot path); the `sz > 0` precondition
/// is only `debug_assert`ed, mirroring the C macro.
#[inline]
pub const fn make_arityval(size: usize) -> Eterm {
    debug_assert!(size > 0 && size <= MAX_ARITYVAL, "arityval out of range");
    make_header(size, ARITYVAL_SUBTAG)
}

/// Range-checked arityval construction.
#[inline]
pub fn try_make_arityval(size: usize) -> TermResult<Eterm> {
    if size > MAX_ARITYVAL {
        Err(TermError::ArityOutOfRange)
    } else {
        Ok(make_arityval_unchecked(size))
    }
}

/// `arityval(x)`.
#[inline]
pub const fn arityval(x: Eterm) -> usize {
    x.raw() >> HEADER_ARITY_OFFS
}

/// `is_arity_value(x)`.
#[inline]
pub const fn is_arity_value(x: Eterm) -> bool {
    (x.raw() & TAG_HEADER_MASK) == ARITYVAL_SUBTAG
}

/// `is_sane_arity_value(x)`.
#[inline]
pub const fn is_sane_arity_value(x: Eterm) -> bool {
    is_arity_value(x) && (x.raw() >> HEADER_ARITY_OFFS) <= MAX_ARITYVAL
}

// ---------------------------------------------------------------------------
// Record headers
// ---------------------------------------------------------------------------

/// `make_record_header(sz)` — the `RECORD_SUBTAG` twin of [`make_header`]
/// (`erl_record.h:85-86`: `MAKE_RECORD_HEADER(FieldCount)` expands to
/// `make_record_header(RECORD_INST_SIZE(FieldCount) - 1)`).
///
/// This is the header word of a native-record **instance** (see [`crate::record`]);
/// unlike an [`make_arityval`] header it never doubles as a standalone term value.
#[inline]
pub const fn make_record_header(size: usize) -> Eterm {
    make_header(size, RECORD_SUBTAG)
}

// ---------------------------------------------------------------------------
// Thing layouts (only the sizes the sub-type tests need)
// ---------------------------------------------------------------------------

/// `ERL_SUB_BITS_SIZE` (`erl_bits.h`: `ErlSubBits` = thing_word, base_flags, start, end, orig).
pub const ERL_SUB_BITS_SIZE: usize = 5;
/// `ERL_HEAP_BITS_SIZE` (`erl_bits.h`: `ErlHeapBits` = thing_word, size, data[]).
pub const ERL_HEAP_BITS_SIZE: usize = 2;
/// `ERL_BIN_REF_SIZE` (`erl_bits.h`: `BinRef` = thing_word, val, next).
pub const ERL_BIN_REF_SIZE: usize = 3;
/// `ERL_FUN_SIZE` on 64-bit (`erl_fun.h`: `ErlFunThing` = thing_word, entry, env[]).
pub const ERL_FUN_SIZE: usize = 2;

/// `HEADER_FLONUM` (64-bit: a 1-word thing).
pub const HEADER_FLONUM: Eterm = make_header(1, FLOAT_SUBTAG);
/// `HEADER_SUB_BITS`.
pub const HEADER_SUB_BITS: Eterm = make_header(ERL_SUB_BITS_SIZE - 1, SUB_BITS_SUBTAG);
/// `HEADER_BIN_REF`.
pub const HEADER_BIN_REF: Eterm = make_header(ERL_BIN_REF_SIZE - 1, BIN_REF_SUBTAG);

// ---------------------------------------------------------------------------
// Header-only predicates
// ---------------------------------------------------------------------------

/// `is_map_header(x)`.
#[inline]
pub const fn is_map_header(x: Eterm) -> bool {
    (x.raw() & TAG_HEADER_MASK) == MAP_SUBTAG
}

/// `is_flatmap_header(x)`.
#[inline]
pub const fn is_flatmap_header(x: Eterm) -> bool {
    (x.raw() & HEADER_MAP_SUBTAG_MASK) == HAMT_SUBTAG_HEAD_FLATMAP
}

/// `is_hashmap_header(x)`.
#[inline]
pub const fn is_hashmap_header(x: Eterm) -> bool {
    (x.raw() & HEADER_MAP_HASHMAP_HEAD_MASK) == HAMT_SUBTAG_HEAD_ARRAY
}

/// `is_fun_header(x)`.
#[inline]
pub const fn is_fun_header(x: Eterm) -> bool {
    (x.raw() & HEADER_SUBTAG_MASK) == FUN_SUBTAG
}

/// `is_external_fun(FunThing)` (`erl_fun.h:87-88`) — the fun header's external
/// kind bit ([`FUN_HEADER_KIND_OFFS`], bit 24) is set.
///
/// Unlike the C macro (`!!(thing_word >> FUN_HEADER_KIND_OFFS)`, true for any bit
/// at or above 24), this takes the bit alone, so a
/// [`FunKind::Imported`](crate::fun::FunKind::Imported) header (bit 25) reads as
/// non-external — exactly as a C reader of that word would classify it.
///
/// Takes a **header word**, not a term (like [`is_fun_header`]); use
/// [`is_any_fun`] to test a term.
#[inline]
pub const fn is_external_fun(x: Eterm) -> bool {
    (x.raw() >> FUN_HEADER_KIND_OFFS) & 1 != 0
}

/// `is_imported_fun(FunThing)` — the wire-imported kind bit
/// ([`FUN_HEADER_IMPORTED_OFFS`], bit 25, this crate's extension) is set. Valid
/// only while the external bit is clear; see [`crate::fun`].
///
/// Takes a **header word**, not a term.
#[inline]
pub const fn is_imported_fun(x: Eterm) -> bool {
    (x.raw() >> FUN_HEADER_IMPORTED_OFFS) & 1 != 0
}

/// `is_local_fun(FunThing)` (`erl_fun.h:89-90`) — the fun header is neither a C
/// external fun nor a wire-imported one.
///
/// Takes a **header word**, not a term.
#[inline]
pub const fn is_local_fun(x: Eterm) -> bool {
    !is_external_fun(x) && !is_imported_fun(x)
}

/// `is_record_header(x)`.
#[inline]
pub const fn is_record_header(x: Eterm) -> bool {
    (x.raw() & HEADER_SUBTAG_MASK) == RECORD_SUBTAG
}

/// `_is_bignum_header(x)` — true for both positive and negative bignums.
#[inline]
pub const fn is_bignum_header(x: Eterm) -> bool {
    (x.raw() & BIG_TAG_MASK) == POS_BIG_SUBTAG
}

/// `is_ref_thing_header(x)`.
#[inline]
pub const fn is_ref_thing_header(x: Eterm) -> bool {
    (x.raw() & TAG_HEADER_MASK) == REF_SUBTAG
}

/// `is_bitstring_header(x)` — true for both heap and sub bitstrings.
#[inline]
pub const fn is_bitstring_header(x: Eterm) -> bool {
    (x.raw() & BITSTRING_TAG_MASK) == HEAP_BITS_SUBTAG
}

/// `is_external_pid_header(x)`.
#[inline]
pub const fn is_external_pid_header(x: Eterm) -> bool {
    (x.raw() & TAG_HEADER_MASK) == EXTERNAL_PID_SUBTAG
}

/// `is_external_port_header(x)`.
#[inline]
pub const fn is_external_port_header(x: Eterm) -> bool {
    (x.raw() & TAG_HEADER_MASK) == EXTERNAL_PORT_SUBTAG
}

/// `is_external_ref_header(x)`.
#[inline]
pub const fn is_external_ref_header(x: Eterm) -> bool {
    (x.raw() & TAG_HEADER_MASK) == EXTERNAL_REF_SUBTAG
}

/// `is_external_header(x)` — true for external pids, ports and refs.
#[inline]
pub const fn is_external_header(x: Eterm) -> bool {
    (x.raw() & EXTERNAL_TAG_MASK) == EXTERNAL_PID_SUBTAG
}

// ---------------------------------------------------------------------------
// Term predicates (these dereference the boxed pointer)
// ---------------------------------------------------------------------------

macro_rules! boxed_predicate {
    ($name:ident, $header_test:ident, $doc:literal) => {
        #[doc = $doc]
        ///
        /// A word that is not a term at all (a bare header word, e.g. the ABI's
        /// `THE_NON_VALUE`) is `false`, never a dereference — see
        /// [`is_boxed_term`].
        ///
        /// # Safety
        ///
        /// If `x` is a boxed term, its header word must be a live, aligned term
        /// word.
        #[inline]
        pub unsafe fn $name(x: Eterm) -> bool {
            if !is_boxed_term(x) {
                return false;
            }
            unsafe { $header_test(*boxed_val(x)) }
        }
    };
}

boxed_predicate!(is_map, is_map_header, "`is_map(x)`");
boxed_predicate!(is_flatmap, is_flatmap_header, "`is_flatmap(x)`");
boxed_predicate!(is_hashmap, is_hashmap_header, "`is_hashmap(x)`");
boxed_predicate!(is_any_fun, is_fun_header, "`is_any_fun(x)`");
boxed_predicate!(is_record, is_record_header, "`is_record(x)`");
boxed_predicate!(is_big, is_bignum_header, "`is_big(x)`");
boxed_predicate!(
    is_internal_ref,
    is_ref_thing_header,
    "`is_internal_ref(x)`"
);

/// `internal_ref_numbers(x)` (`erl_node_container_utils.h:264`) — **the C-layout
/// reader**: the reference's `num[]` ids as a `u32` slice.
///
/// On 64-bit little-endian the C block is
/// `ErtsORefThing { Eterm header; Uint32 num[ERTS_REF_NUMBERS]; Uint32 marker; }`
/// (`erl_term.h:795-802`), so the ids start at byte offset 8 — i.e. at word 1 of
/// the `[header, w1, w2]` block this crate reads as `make_header(2, REF_SUBTAG)`
/// followed by two payload words (`erl_term.h:873-877`). The `u32` view of those
/// two payload words is the 3 ids followed by the marker word, hence a 4-element
/// slice.
///
/// # This is the C layout, NOT the M1 shape this tree produces
///
/// The reference thing this runtime actually builds is the **M1
/// internal-reference thing** ([`write_internal_ref`]): a 4-word
/// `[make_header(3, REF_SUBTAG), node, serial, id]` block whose payload is three
/// *terms*, not a packed `u32` `num[]`. Its header carries `REF_SUBTAG` too, so
/// [`is_internal_ref`] is `true` for it — but feeding it to this reader would
/// silently return the small-integer **tags** of words 1..2 as if they were ids,
/// i.e. a wrong reference identity instead of an error. **Never pass an M1
/// reference here**: [`internal_ref_id`] is the sanctioned accessor for the M1
/// layout, and it refuses a C-layout block rather than over-reading it.
///
/// This function is retained as the documented C-layout view (`ErtsORefThing`,
/// written by nothing in this tree) — no code in this crate or its dependents calls
/// it.
///
/// # Safety
///
/// `t` must be a live **C-layout** internal reference (see [`is_internal_ref`])
/// whose `[header, w1, w2]` block is valid and live for the returned `'a` (which
/// cannot outlive the term's heap). An M1 reference must not be passed — see above.
pub unsafe fn internal_ref_numbers<'a>(t: Eterm) -> &'a [u32] {
    let p = boxed_val(t);
    // SAFETY: the caller guarantees a validated internal-reference block; its two
    // payload words are exactly 4 `u32`s (3 ids + the marker word) and stay live
    // for `'a` by the caller's contract.
    unsafe { core::slice::from_raw_parts(p.add(1).cast::<u32>(), 4) }
}

// ---------------------------------------------------------------------------
// The M1 internal-reference thing (the shape this runtime actually builds)
// ---------------------------------------------------------------------------

/// The number of **payload** words of an M1 internal-reference thing: `node`,
/// `serial` and `id` (the header word is not counted).
///
/// The M1 producers (`beam_process::MonitorRef::to_eterm`,
/// `beam_bifs`'s `make_ref_thing`, `beam_rt`'s native floor) all use this value.
pub const INTERNAL_REF_PAYLOAD_WORDS: usize = 3;

/// The total number of words of an M1 internal-reference thing, header included.
///
/// The frozen block is
/// `[make_header(3, REF_SUBTAG), node, serial, id]`: word 0 is the header
/// (`make_header(3, REF_SUBTAG)`, raw `0xD0`), words 1 and 2 are the zeroed *node*
/// and *serial* terms (Milestone-1 has a single local node and a single monotonic
/// id space, so the whole identity lives in word 3), and word 3 is `make_small(id)`.
///
/// This is the **M1 substitution** for the C `ErtsORefThing` (`[header(2,
/// REF_SUBTAG), num[0..3] as 4 `u32`s]`): the node container of the C layout is not
/// carried, and the id lives in a *term* rather than inside a packed `u32` array.
///
/// # The id slot is one 60-bit small — narrower than ERTS's own id space
///
/// Because the whole identity is `make_small(id)` in a single fixed word, the
/// representable id space is `0..=`[`MAX_SMALL`](crate::immediate::MAX_SMALL)
/// (`0..= 2^59 - 1`). ERTS's own reference id is wider: the runtime hands out a full
/// `Uint64` (`value = esdp->ref++`, `erl_bif_unique.h:169-178`) and its identity is
/// the whole 64 bits, spread over the three wire numbers — `num[0]`'s 18 bits, the
/// `num[1]` bits 18..31, and all 32 bits of `num[2]`
/// (`erts_set_ref_numbers`/`erts_get_ref_numbers_value`, `erl_bif_unique.h:117-167`;
/// the remaining `num[1]` bits are the C scheduler `thr_id` and two markers, which
/// are not part of the value).
///
/// The slot **cannot** be widened without a term-layout break:
///
/// * a bignum id needs a *variable*-size thing — `[header, size, limbs…]` — so the
///   block would no longer be four words and [`INTERNAL_REF_WORDS`],
///   `beam_gc`/`beam_message`'s sizing and copy walks, [`internal_ref_id`]'s
///   arity check and the three sibling producers all change;
/// * repurposing the `node`/`serial` terms as extra id bits is not a widening
///   either: those words are *semantically* the node container and the serial
///   number (words 1 and 2 above), all three sibling producers pin them to
///   `make_small(0)`, `beam_codec`'s codec rebuilds a decoded internal reference
///   through [`write_internal_ref`] (which zeroes them) — so a reference whose
///   identity lived there would stop round-tripping
///   (`binary_to_term(term_to_binary(R)) =:= R`) — and [`crate::compare::cmp`]
///   compares *all* the payload words, so the identity would then depend on bits the
///   wire cannot carry back.
///
/// An id past the slot is therefore **refused** ([`write_internal_ref`]'s
/// `TermError::SmallIntOutOfRange`), never truncated or substituted: a caller with a
/// wider id must keep the reference in its faithful **external** form
/// (`crate::thing::write_external_ref`, what `beam_codec` does for a wire image whose
/// packed value does not fit this slot).
pub const INTERNAL_REF_WORDS: usize = INTERNAL_REF_PAYLOAD_WORDS + 1;

/// `write_internal_ref(hp, id)` — build the **M1 internal-reference thing**.
///
/// This is the crate's canonical, heap-agnostic writer for an internal reference
/// (the frozen term-layout LCA, workspace `CONTEXT.md` §2), matching the convention
/// of the sibling writers ([`crate::map::write_map`],
/// [`crate::bitstring::write_heap_bits`], [`crate::refc::write_bin_ref`],
/// [`crate::fun::write_fun_local`]): `beam_term` cannot allocate, so the caller
/// reserves the words and passes `hp`.
///
/// It writes exactly [`INTERNAL_REF_WORDS`] words — `[make_header(3, REF_SUBTAG),
/// make_small(0), make_small(0), make_small(id)]` — and returns the tagged root
/// `make_boxed(hp)`.
///
/// # Frozen inter-crate contract
///
/// The four words are **byte-identical** to what `beam_process`'s
/// `MonitorRef::to_eterm`, `beam_bifs`'s `make_ref_thing` and `beam_rt`'s native
/// process floor write, so a reference already in memory hashes/compares/encodes
/// exactly like one this writer built. Those producers and this writer must stay in
/// lockstep; a drift here is a term-layout break.
///
/// # Safety
///
/// `hp` must point to at least [`INTERNAL_REF_WORDS`] writable, properly aligned
/// `Eterm` words, owned exclusively by the caller for the lifetime of the returned
/// term (the writer never reallocates and hands the pointer out inside the root).
///
/// # Errors
///
/// [`TermError::SmallIntOutOfRange`] when `id` does not fit the 60-bit small integer
/// [`INTERNAL_REF_WORDS`] documents as the block's whole id slot (`id >`
/// [`MAX_SMALL`](crate::immediate::MAX_SMALL), i.e. `id > 2^59 - 1`) — never a
/// truncated or substituted id. ERTS's own reference id is a full 64-bit `Uint64`,
/// so this slot is genuinely narrower; see [`INTERNAL_REF_WORDS`] for why widening it
/// would break the frozen byte-identical four-word block shared with `beam_process`,
/// `beam_bifs` and `beam_rt` (a term-layout break). A caller holding a wider id keeps
/// the reference in its faithful external form
/// ([`crate::thing::write_external_ref`]) instead of materializing it here.
pub unsafe fn write_internal_ref(hp: *mut Eterm, id: u64) -> TermResult<Eterm> {
    let id_term = try_make_small(i64::try_from(id).map_err(|_| TermError::SmallIntOutOfRange)?)?;
    // SAFETY: the caller guarantees `hp` addresses `INTERNAL_REF_WORDS` writable
    // words; every one of them is written before the pointer escapes.
    unsafe {
        *hp = make_header(INTERNAL_REF_PAYLOAD_WORDS, REF_SUBTAG);
        *hp.add(1) = make_small(0); // node: the local node
        *hp.add(2) = make_small(0); // serial: one id space in M1
        *hp.add(3) = id_term; // id
    }
    Ok(make_boxed(hp))
}

/// `internal_ref_id(t)` — the numeric id of an **M1 internal-reference thing**.
///
/// The read-back half of [`write_internal_ref`] (and of the sibling producers it is
/// byte-identical to): the id is word [`INTERNAL_REF_PAYLOAD_WORDS`] of the block,
/// a non-negative small integer — the block's whole id slot, so an id past
/// `2^59 - 1` is never found here (see [`INTERNAL_REF_WORDS`]).
///
/// # Layout, and why this refuses a C-layout block
///
/// [`is_internal_ref`] tests the header **subtag** only, so it is `true` for both
/// this tree's M1 reference (header arity 3, four words) and the C `ErtsORefThing`
/// (header arity 2, three words — the shape [`internal_ref_numbers`] reads). The two
/// layouts do not share an id slot, and reading the M1 id word of a three-word block
/// would run past it, so this accessor requires the header arity to be
/// [`INTERNAL_REF_PAYLOAD_WORDS`] and reports a typed error otherwise. It is the only
/// sanctioned way to read an id out of a reference term.
///
/// # Safety
///
/// If `t` is a boxed term, its header word must be a live, aligned term word. A
/// reference block is read only after its header has been validated.
///
/// # Errors
///
/// [`TermError::BadRef`] when `t` is not an M1 internal-reference thing: a word that
/// is not a term, a boxed term of another kind, a C-layout ref block (arity 2), or a
/// block whose id word is not a non-negative small integer. Never a wrong id.
pub unsafe fn internal_ref_id(t: Eterm) -> TermResult<u64> {
    if !unsafe { is_internal_ref(t) } {
        return Err(TermError::BadRef);
    }
    let p = boxed_val(t);
    // SAFETY: `is_internal_ref` confirmed a boxed term whose header carries
    // `REF_SUBTAG`, so its header word is a live, aligned term word.
    if header_arity(unsafe { *p }) != INTERNAL_REF_PAYLOAD_WORDS {
        // A C-layout ref block (arity 2): its id word does not exist. Refuse rather
        // than read past the block.
        return Err(TermError::BadRef);
    }
    // SAFETY: the header arity says the block has `INTERNAL_REF_PAYLOAD_WORDS`
    // payload words, so word `INTERNAL_REF_PAYLOAD_WORDS` (the id) is in bounds.
    let id_word = unsafe { *p.add(INTERNAL_REF_PAYLOAD_WORDS) };
    if !is_small(id_word) {
        return Err(TermError::BadRef);
    }
    u64::try_from(signed_value(id_word)).map_err(|_| TermError::BadRef)
}

boxed_predicate!(
    is_external_pid,
    is_external_pid_header,
    "`is_external_pid(x)`"
);
boxed_predicate!(
    is_external_port,
    is_external_port_header,
    "`is_external_port(x)`"
);
boxed_predicate!(    is_external_ref,
    is_external_ref_header,
    "`is_external_ref(x)`"
);

/// `is_float(x)` — a boxed term whose header is exactly `HEADER_FLONUM`.
///
/// A word that is not a term at all (a bare header word, e.g. the ABI's
/// `THE_NON_VALUE`) is `false`, never a dereference — see [`is_boxed_term`].
///
/// # Safety
///
/// If `x` is a boxed term, its header word must be a live, aligned term word.
#[inline]
pub unsafe fn is_float(x: Eterm) -> bool {
    if !is_boxed_term(x) {
        return false;
    }
    unsafe { *boxed_val(x) == HEADER_FLONUM }
}

/// `is_bitstring(x)`.
///
/// A word that is not a term at all (a bare header word, e.g. the ABI's
/// `THE_NON_VALUE`) is `false`, never a dereference — see [`is_boxed_term`].
///
/// # Safety
///
/// If `x` is a boxed term, its header word must be a live, aligned term word.
#[inline]
pub unsafe fn is_bitstring(x: Eterm) -> bool {
    if !is_boxed_term(x) {
        return false;
    }
    unsafe { is_bitstring_header(*boxed_val(x)) }
}

/// `is_not_bitstring(x)`.
///
/// # Safety
///
/// As [`is_bitstring`].
#[inline]
pub unsafe fn is_not_bitstring(x: Eterm) -> bool {
    let bitstring = unsafe { is_bitstring(x) };
    !bitstring
}

/// `is_bin_ref(x)` — an off-heap binary reference (`HEADER_BIN_REF`).
///
/// A word that is not a term at all (a bare header word, e.g. the ABI's
/// `THE_NON_VALUE`) is `false`, never a dereference — see [`is_boxed_term`].
///
/// # Safety
///
/// If `x` is a boxed term, its header word must be a live, aligned term word.
#[inline]
pub unsafe fn is_bin_ref(x: Eterm) -> bool {
    if !is_boxed_term(x) {
        return false;
    }
    unsafe { *boxed_val(x) == HEADER_BIN_REF }
}

/// `bitstring_size(Bin)` — the bit size of a heap or sub bitstring.
///
/// `erl_bits.h`: for a `SUB_BITS` thing the size is `end - start`; for a
/// `HEAP_BITS` thing it is the `size` word.
///
/// # Safety
///
/// `bin` must be a live bitstring (heap or sub bits) whose backing thing is
/// valid for `ERL_SUB_BITS_SIZE` / `ERL_HEAP_BITS_SIZE` words.
pub unsafe fn bitstring_size(bin: Eterm) -> usize {
    let p = boxed_val(bin);
    let hdr = unsafe { *p };
    if hdr == HEADER_SUB_BITS {
        // ErlSubBits: [thing_word, base_flags, start, end, orig]
        let start = unsafe { *p.add(2) }.raw();
        let end = unsafe { *p.add(3) }.raw();
        end - start
    } else {
        // ErlHeapBits: [thing_word, size, data...]
        unsafe { *p.add(1) }.raw()
    }
}

/// `is_binary(x)` — a bitstring whose size is a whole number of bytes.
///
/// # Safety
///
/// As [`bitstring_size`].
pub unsafe fn is_binary(x: Eterm) -> bool {
    unsafe { is_bitstring(x) && (bitstring_size(x) & 7) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::immediate::{make_small, NIL};
    use crate::tag::{HEADER_MAP_SUBTAG_MASK, NEG_BIG_SUBTAG};

    #[test]
    fn boxed_pointer_tagging_and_masking() {
        let mut words = [make_arityval(1), make_small(9)];
        let base = words.as_mut_ptr() as usize;
        let t = make_boxed(words.as_ptr());
        assert!(is_boxed(t));
        assert!(!is_not_boxed(t));
        assert_eq!(boxed_val(t) as usize, base);
        let literal = Eterm::from_raw(t.raw() | crate::tag::TAG_LITERAL_PTR);
        assert_eq!(boxed_val(literal) as usize, base);
    }

    #[test]
    fn arityval_encoding_is_pinned() {
        assert_eq!(make_arityval_zero().raw(), 0);
        assert_eq!(make_arityval_unchecked(0).raw(), 0);
        assert_eq!(make_arityval(1).raw(), 0x40);
        assert_eq!(make_arityval(2).raw(), 0x80);
        assert_eq!(make_arityval_unchecked(MAX_ARITYVAL).raw(), MAX_ARITYVAL << 6);
        assert!(is_arity_value(make_arityval(3)));
        assert!(is_sane_arity_value(make_arityval(3)));
        assert_eq!(arityval(make_arityval(7)), 7);
        assert_eq!(arityval(make_arityval_zero()), 0);
    }

    #[test]
    fn try_make_arityval_range() {
        assert_eq!(
            try_make_arityval(0).unwrap().raw(),
            make_arityval_zero().raw()
        );
        assert_eq!(
            try_make_arityval(MAX_ARITYVAL).unwrap().raw(),
            make_arityval_unchecked(MAX_ARITYVAL).raw()
        );
        assert_eq!(
            try_make_arityval(MAX_ARITYVAL + 1),
            Err(TermError::ArityOutOfRange)
        );
    }

    #[test]
    fn header_helpers() {
        let tup = make_arityval(5);
        assert_eq!(header_subtag(tup), 0x00);
        assert_eq!(header_arity(tup), 5);
        assert_eq!(thing_arityval(tup), 5);
        assert!(header_is_transparent(tup));
        assert!(!header_is_thing(tup));

        let float = make_header(1, FLOAT_SUBTAG);
        assert_eq!(header_subtag(float), 0x18);
        assert!(!header_is_transparent(float));
        assert!(header_is_thing(float));

        let rec = make_header(3, RECORD_SUBTAG);
        assert_eq!(header_arity(rec), 3);
        assert!(header_is_transparent(rec));
    }

    #[test]
    fn map_header_fields() {
        // MAP_HEADER_HAMT_HEAD_ARRAY = MAKE_MAP_HEADER(tag=2, arity=1, val=0xffff)
        let hdr = make_header(((0xffffusize << 8) | 1) << 2 | 0x2, MAP_SUBTAG);
        assert!(is_map_header(hdr));
        assert!(is_hashmap_header(hdr));
        assert!(!is_flatmap_header(hdr));
        assert_eq!(map_header_type(hdr), 2);
        assert_eq!(map_header_arity(hdr), 1);
        assert_eq!(map_header_val(hdr), 0xffff);
        assert_eq!(header_arity(hdr), 1);
    }

    #[test]
    fn flatmap_header_fields() {
        // MAP_HEADER_FLATMAP = MAKE_MAP_HEADER(tag=0, arity=1, val=0)
        let hdr = make_header((1usize << 2) | 0x0, MAP_SUBTAG);
        assert!(is_map_header(hdr));
        assert!(is_flatmap_header(hdr));
        assert!(!is_hashmap_header(hdr));
        assert_eq!(map_header_type(hdr), 0);
        assert_eq!(map_header_arity(hdr), 1);
        assert_eq!(hdr.raw() & HEADER_MAP_SUBTAG_MASK, HAMT_SUBTAG_HEAD_FLATMAP);
    }

    #[test]
    fn boxed_subtype_predicates() {
        let float_words = [HEADER_FLONUM, make_small(0)];
        let float = make_boxed(float_words.as_ptr());
        let big_words = [make_header(1, POS_BIG_SUBTAG), make_small(0)];
        let big = make_boxed(big_words.as_ptr());
        let nbig_words = [make_header(1, NEG_BIG_SUBTAG), make_small(0)];
        let nbig = make_boxed(nbig_words.as_ptr());
        let ref_words = [make_header(2, REF_SUBTAG), make_small(0), make_small(0)];
        let ref_t = make_boxed(ref_words.as_ptr());
        let rec_words = [make_header(2, RECORD_SUBTAG), make_small(0), make_small(0)];
        let rec = make_boxed(rec_words.as_ptr());
        // MAKE_FUN_HEADER(arity=2, numfree=1, external=0)
        let fun_hdr = Eterm::from_raw(FUN_SUBTAG | (2usize << 8) | (1usize << 16));
        let fun_words = [fun_hdr, make_small(0)];
        let fun = make_boxed(fun_words.as_ptr());

        unsafe {
            assert!(is_float(float));
            assert!(!is_big(float));
            assert!(is_big(big));
            assert!(is_big(nbig));
            assert!(!is_float(big));
            assert!(is_internal_ref(ref_t));
            assert!(is_record(rec));
            assert!(is_any_fun(fun));
            assert_eq!(header_arity(fun_hdr), ERL_FUN_SIZE - 1);
            assert!(!is_map(rec));
            assert!(!is_bitstring(rec));
        }
        assert!(!unsafe { is_float(make_small(1)) });
        assert!(!unsafe { is_float(NIL) });
    }

    #[test]
    fn fun_header_kind_predicates() {
        // MAKE_FUN_HEADER(arity, num_free, external) | imported bit (bit 25).
        let local = Eterm::from_raw(FUN_SUBTAG | (2usize << 8) | (1usize << 16));
        let external = Eterm::from_raw(FUN_SUBTAG | (2usize << 8) | (1usize << 24));
        let imported = Eterm::from_raw(FUN_SUBTAG | (1usize << 25));
        assert!(is_fun_header(local) && is_fun_header(external) && is_fun_header(imported));
        assert!(is_local_fun(local));
        assert!(!is_external_fun(local) && !is_imported_fun(local));
        assert!(is_external_fun(external));
        assert!(!is_local_fun(external) && !is_imported_fun(external));
        assert!(is_imported_fun(imported));
        assert!(!is_external_fun(imported) && !is_local_fun(imported));
    }

    #[test]
    fn external_headers() {
        assert!(is_external_pid_header(make_header(4, EXTERNAL_PID_SUBTAG)));
        assert!(is_external_port_header(make_header(4, EXTERNAL_PORT_SUBTAG)));
        assert!(is_external_ref_header(make_header(5, EXTERNAL_REF_SUBTAG)));
        assert!(is_external_header(make_header(4, EXTERNAL_PID_SUBTAG)));
        assert!(is_external_header(make_header(4, EXTERNAL_PORT_SUBTAG)));
        assert!(is_external_header(make_header(5, EXTERNAL_REF_SUBTAG)));
        assert!(!is_external_header(make_arityval(1)));
    }

    #[test]
    fn bitstring_and_binary() {
        // heap bitstring of 16 bits => 2 bytes => a binary (size word = 16)
        let heap_words = [
            make_header(2, HEAP_BITS_SUBTAG),
            Eterm::from_raw(16),
            make_small(0),
        ];
        let heap = make_boxed(heap_words.as_ptr());
        // sub-bits: [thing_word, base_flags, start, end, orig], end-start = 12 bits
        let sub_words = [
            HEADER_SUB_BITS,
            Eterm::from_raw(0),
            Eterm::from_raw(4),
            Eterm::from_raw(16),
            make_small(0),
        ];
        let sub = make_boxed(sub_words.as_ptr());
        unsafe {
            assert!(is_bitstring(heap));
            assert!(!is_not_bitstring(heap));
            assert_eq!(bitstring_size(heap), 16);
            assert!(is_binary(heap));
            assert!(is_bitstring(sub));
            assert_eq!(bitstring_size(sub), 12);
            assert!(!is_binary(sub)); // 12 bits is not byte aligned
        }
        assert!(!unsafe { is_bitstring(NIL) });
    }
}
