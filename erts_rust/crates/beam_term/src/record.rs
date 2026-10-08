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

//! Native records — the OTP 29 `ErtsRecordDefinition` / `ErtsRecordInstance`
//! term layout.
//!
//! OTP 29 introduced *native records* (`-record` as a first-class runtime value,
//! produced by the `records:create/4` BIF family) alongside the classic
//! "tagged tuple" records. A native record is a boxed word whose interior layout
//! is owned by this crate — the lowest common ancestor for term layouts
//! (workspace `CONTEXT.md` §2) — so the writers and readers live here, next to the
//! predicates in [`crate::boxed`] and mirroring [`crate::thing`]/[`crate::refc`]/
//! [`crate::fun`].
//!
//! C ground truth (read-only reference; never edited):
//!
//! * `erts/emulator/beam/erl_record.h:41-96` — `ErtsRecordDefinition`,
//!   `ErtsRecordInstance`, `RECORD_INST_SIZE`/`RECORD_DEF_SIZE`,
//!   `RECORD_INST_FIELD_COUNT`/`RECORD_DEF_FIELD_COUNT`, `MAKE_RECORD_HEADER`,
//!   `record_field_count`.
//! * `erts/emulator/beam/erl_record.c:904-1085` — `records_create_4`, the exact
//!   contiguous construction block, field layout and sort/`field_order` rule.
//! * `erts/emulator/beam/erl_record.c:106-153` — `erts_canonical_record_def`.
//! * `erts/emulator/beam/erl_record.c:1247-1274` — `records_get_2`, the linear
//!   key scan; `:1298-1326` — `records_get_field_names_1` (the declaration-order
//!   access); `:1328-1340` — `records_is_exported_1`.
//! * `erts/emulator/beam/external.c:5257-5381` — the `RECORD_EXT` decoder (the
//!   same block and the same sort rule); `:4014-4036` — the encoder (the wire
//!   field order = declaration order).
//! * `erts/emulator/beam/erl_utils.h:267-272` — `erts_cmp_flatmap_keys`, the key
//!   sort rule (atoms compare by their raw value, i.e. by atom index).
//!
//! # Frozen layout
//!
//! A **record instance** is a boxed word (`make_boxed(instance_ptr)`) whose block is
//! [`record_inst_size_words`]`(n)` = `2 + n` words:
//!
//! ```text
//! [0]            thing_word        = make_record_header(n + 1)
//! [1]            record_definition = make_boxed(def_ptr)  (a boxed, tuple-shaped
//!                                                          pointer to the def)
//! [2 .. 2+n]     the n field VALUES, in SORTED-KEY order
//! ```
//!
//! A **record definition** is a block of [`record_def_size_words`]`(n)` = `6 + n`
//! words whose `thing_word` is an arityval (so it presents as a tuple and is
//! reachable through `tuple_val(instance->record_definition)`):
//!
//! ```text
//! [0]            thing_word  = make_arityval(RECORD_DEF_SIZE(n) - 1)  = make_arityval(5 + n)
//! [1]            field_order = a tuple [make_arityval(n)][order_0 .. order_{n-1}]
//!                             (n == 0 -> the shared `tuple0()` literal)
//! [2]            hash        = the cached `phash2`/`make_hash2` value (see below)
//! [3]            module      = atom term
//! [4]            name        = atom term
//! [5]            is_exported = the atom `true` or `false`
//! [6 .. 6+n]     the n field KEYS (atom terms), in ASCENDING ATOM-INDEX order
//! ```
//!
//! `field_order[i]` is the index (into the sorted keys/values arrays) of the field
//! **declared** `i`-th: C sets `order[fields[i].order] = make_small(i)` after
//! sorting, so `keys[unsigned_val(order[i])]` is the `i`-th declared key and
//! `values[unsigned_val(order[i])]` its value. The **construction block** laid out
//! by [`write_record`] (exactly what `records_create_4` and the ETF decoder
//! allocate) is:
//!
//! ```text
//! [ field_order tuple (n + 1 words, only when n > 0) ][ definition (6 + n) ][ instance (2 + n) ]
//! ```
//!
//! i.e. [`record_block_words`]`(n)` words total.
//!
//! # The cached `hash` is an explicit parameter
//!
//! C's `defp->hash` is `make_hash2` of the tuple
//! `{module, name, is_exported, keys in DECLARATION order}` and is part of a
//! record's `phash2`. This crate's [`crate::hashing`] is explicitly **not**
//! bit-exact to `make_hash2` (its exact values are internal and not a
//! compatibility contract), so it cannot produce that value here. [`write_record`]
//! therefore takes the definition hash as an explicit `definition_hash: Eterm`
//! parameter (stored verbatim at def word 2): a caller that needs `phash2`
//! compatibility must supply the C `make_hash2` value, while a caller that does not
//! may pass [`crate::immediate::make_small`]`(0)`.
//!
//! # `is_exported` and the `true`/`false` atoms
//!
//! [`write_record`] takes the `is_exported` **flag** and stores the atom `true` or
//! `false` (matching C's `am_true`/`am_false`). Those are the standard
//! `atom.names` indices [`AM_FALSE`]`(0)` / [`AM_TRUE`]`(1)`
//! (`erts/emulator/beam/atom.names:44` begins with `atom false true`), which
//! [`crate::atom::AtomTable`] reproduces when seeded from `atom.names`.
//!
//! # The heap-agnostic contract
//!
//! `beam_term` is a dependency *sink*: it cannot allocate, because the production
//! allocators (`beam_alloc`) and the copying collector (`beam_gc`) depend downward
//! on it. Every writer here therefore does **not** allocate: the caller reserves
//! [`record_block_words`]`(n)` words (from `beam_alloc` in production,
//! [`crate::heap::Heap`] in tests), passes the `hp: *mut Eterm` base of that block,
//! and receives back the tagged instance root word.
//!
//! # `unsafe` contract
//!
//! Every writer and reader is `unsafe`: an `Eterm` being a live, well-formed
//! native record is a caller invariant, and there is no way to read or write a raw
//! heap word safely. Each item documents, in its `# Safety` section, exactly how
//! many words the caller must have reserved, which of them are written, and how
//! long a returned borrowed slice stays valid. A reader given a non-record or a
//! malformed term returns [`TermError::BadRecord`] — never a panic and never a
//! wrong value; the two `bool` predicates ([`record_is_native`],
//! [`record_name_is`]) return `false` for a non-record.

use crate::atom::{Atom, AtomTable};
use crate::boxed::{
    boxed_val, header_arity, is_arity_value, is_boxed_term, is_record_header, make_arityval, make_boxed,
    make_record_header,
};
use crate::compare::eq;
use crate::error::{TermError, TermResult};
use crate::eterm::Eterm;
use crate::immediate::{atom_val, is_atom, is_small, make_atom, make_small, unsigned_value};
use crate::tag::MAX_ARITYVAL;
use crate::tuple::tuple0;

/// `RECORD_INST_SIZE(0)` (`erl_record.h:73-74`): the fixed words of an
/// `ErtsRecordInstance` — `[thing_word, record_definition]` — before the field
/// values.
pub const RECORD_INST_FIXED_WORDS: usize = 2;

/// `RECORD_DEF_SIZE(0)` (`erl_record.h:76-77`): the fixed words of an
/// `ErtsRecordDefinition` — `[thing_word, field_order, hash, module, name,
/// is_exported]` — before the keys.
pub const RECORD_DEF_FIXED_WORDS: usize = 6;

/// The atom `false` (`am_false`): under the standard `atom.names` seeding
/// (`atom.names:44` begins with `atom false true`) it is atom index `0`.
pub const AM_FALSE: Atom = Atom::from_raw(0);

/// The atom `true` (`am_true`): atom index `1` under the standard seeding.
pub const AM_TRUE: Atom = Atom::from_raw(1);

// `RECORD_INST_SIZE(FieldCount)` (`erl_record.h:73-74`) =
// `sizeof(ErtsRecordInstance)/sizeof(Eterm) + FieldCount`.
const _: () = assert!(RECORD_INST_FIXED_WORDS == 2, "RECORD_INST_SIZE(0)");
// `RECORD_DEF_SIZE(FieldCount)` (`erl_record.h:76-77`) =
// `sizeof(ErtsRecordDefinition)/sizeof(Eterm) + FieldCount`.
const _: () = assert!(RECORD_DEF_FIXED_WORDS == 6, "RECORD_DEF_SIZE(0)");
// `MAKE_RECORD_HEADER(n)` (`erl_record.h:85-86`) is an arityval of
// `RECORD_INST_SIZE(n) - 1` == `n + 1`; `RECORD_DEF_SIZE(n) - 1` == `n + 5` is the
// definition's arityval (an n == 0 record has a 1-arity instance header and a
// 5-arity definition header).
const _: () = assert!(record_inst_size_words(0) - 1 == 1, "MAKE_RECORD_HEADER(0)");
const _: () = assert!(record_def_size_words(0) - 1 == 5, "RECORD_DEF_SIZE(0) - 1");

/// `RECORD_INST_SIZE(FieldCount)` — the words a record **instance** occupies,
/// including its header (`erl_record.h:73-74`); `2 + FieldCount`.
#[inline]
pub const fn record_inst_size_words(field_count: usize) -> usize {
    RECORD_INST_FIXED_WORDS + field_count
}

/// `RECORD_DEF_SIZE(FieldCount)` — the words a record **definition** occupies,
/// including its header (`erl_record.h:76-77`); `6 + FieldCount`.
#[inline]
pub const fn record_def_size_words(field_count: usize) -> usize {
    RECORD_DEF_FIXED_WORDS + field_count
}

/// The total words of the construction block [`write_record`] lays out: the
/// `field_order` tuple (`field_count + 1` words, omitted when `field_count == 0`,
/// in which case the shared `tuple0()` literal is used) followed by the definition
/// and the instance (`erl_record.c:957-967`).
#[inline]
pub const fn record_block_words(field_count: usize) -> usize {
    let order_words = if field_count == 0 { 0 } else { field_count + 1 };
    order_words + record_def_size_words(field_count) + record_inst_size_words(field_count)
}

/// The word offset of the definition block within the construction block: after
/// the `field_order` tuple (which is absent when `field_count == 0`).
#[inline]
const fn definition_offset(field_count: usize) -> usize {
    if field_count == 0 {
        0
    } else {
        field_count + 1
    }
}

// ---------------------------------------------------------------------------
// Validation helpers (shared by the readers)
// ---------------------------------------------------------------------------

/// Validates `t` as a live native-record **instance** and returns its header-word
/// pointer together with its field count.
unsafe fn instance_parts(t: Eterm) -> TermResult<(*mut Eterm, usize)> {
    if !is_boxed_term(t) {
        return Err(TermError::BadRecord);
    }
    let p = boxed_val(t);
    // SAFETY: `t` is a boxed term and the caller guarantees its header word is live
    // and initialised (the standing invariant of every boxed accessor here).
    let header = unsafe { *p };
    if !is_record_header(header) {
        return Err(TermError::BadRecord);
    }
    // `record_field_count` (`erl_record.h:94-95`) is `header_arity - 1`; a record
    // header always has arity >= 1 (`MAKE_RECORD_HEADER(n) == n + 1`).
    let arity = header_arity(header);
    if arity < 1 {
        return Err(TermError::BadRecord);
    }
    Ok((p, arity - 1))
}

/// Validates `def` as a native-record **definition** term and returns its
/// header-word pointer together with its field count.
///
/// A definition is structurally a tuple (`RECORD_DEF_P` is just `tuple_val`), so
/// the header must be an arityval of at least [`RECORD_DEF_FIXED_WORDS`]` - 1`
/// words. As in C, a plain tuple of arity >= 5 is indistinguishable from a
/// definition by its header alone — callers must only pass a value obtained from
/// [`record_definition`].
unsafe fn definition_parts(def: Eterm) -> TermResult<(*mut Eterm, usize)> {
    if !is_boxed_term(def) {
        return Err(TermError::BadRecord);
    }
    let p = boxed_val(def);
    // SAFETY: `def` is a boxed term and the caller guarantees a live header word.
    let header = unsafe { *p };
    if !is_arity_value(header) {
        return Err(TermError::BadRecord);
    }
    let arity = header_arity(header);
    if arity < RECORD_DEF_FIXED_WORDS - 1 {
        return Err(TermError::BadRecord);
    }
    Ok((p, arity - (RECORD_DEF_FIXED_WORDS - 1)))
}

// ---------------------------------------------------------------------------
// Writer (heap-agnostic: the caller reserves the words)
// ---------------------------------------------------------------------------

/// Writes a native record — the construction block
/// `[field_order tuple][definition][instance]` — at `hp` and returns the tagged
/// **instance** root.
///
/// `fields` are `(key-atom-term, value)` pairs in **declaration** order. The keys
/// are sorted by atom index (`erts_cmp_flatmap_keys`, `erl_utils.h:267-272`), the
/// definition's `keys`/the instance's values are stored in that sorted order, and
/// `field_order[declaration_index]` is the field's sorted rank — exactly the rule
/// `records_create_4`'s `qsort` + `order[fields[i].order] = make_small(i)` applies.
/// `definition_hash` is stored verbatim at definition word 2 (see the module docs:
/// this crate cannot produce the C `make_hash2` value).
///
/// A duplicate key, a non-atom key, or a field count whose definition arity would
/// overflow [`MAX_ARITYVAL`] (which includes `n > MAX_ARITYVAL`) is rejected with
/// [`TermError::BadRecord`] rather than silently corrupting the block.
///
/// # Safety
///
/// `hp` must point at [`record_block_words`]`(fields.len())` exclusively-owned,
/// writable words; all of them are written by this call and must stay live (and
/// unmoved) for as long as the returned term is used. A boxed key/value term must
/// itself be a live term.
pub unsafe fn write_record(
    hp: *mut Eterm,
    module: Atom,
    name: Atom,
    is_exported: bool,
    fields: &[(Eterm, Eterm)],
    definition_hash: Eterm,
) -> TermResult<Eterm> {
    let n = fields.len();
    // The definition's arityval is `RECORD_DEF_SIZE(n) - 1 == n + 5`; a count whose
    // arity would not fit the header's arity field is rejected (this subsumes the
    // `n > MAX_ARITYVAL` guard).
    if record_def_size_words(n) - 1 > MAX_ARITYVAL {
        return Err(TermError::BadRecord);
    }
    for &(key, _) in fields {
        if !is_atom(key) {
            return Err(TermError::BadRecord);
        }
    }

    // Sort the declaration indices by atom index (ascending). `atom_val` is
    // monotonic in the raw atom word, so this reproduces `erts_cmp_flatmap_keys`.
    let mut sorted: Vec<usize> = (0..n).collect();
    sorted.sort_by_key(|&i| atom_val(fields[i].0));
    // Duplicate keys are adjacent after sorting.
    for w in 1..n {
        if fields[sorted[w - 1]].0 == fields[sorted[w]].0 {
            return Err(TermError::BadRecord);
        }
    }
    // `field_order[d]` is the sorted rank of the field declared `d`-th.
    let mut field_order = vec![0usize; n];
    for (rank, &decl) in sorted.iter().enumerate() {
        field_order[decl] = rank;
    }

    let def_off = definition_offset(n);
    let inst_off = def_off + record_def_size_words(n);

    // SAFETY: the caller reserved `record_block_words(n)` words at `hp`. The
    // `field_order` tuple occupies `hp[0..=n]` (when `n > 0`), the definition
    // `hp[def_off .. def_off + 6 + n]` and the instance
    // `hp[inst_off .. inst_off + 2 + n]`; every write below is in bounds, and all
    // written words stay live for as long as the returned root is used.
    unsafe {
        if n > 0 {
            *hp = make_arityval(n);
            for (i, &rank) in field_order.iter().enumerate() {
                *hp.add(1 + i) = make_small(rank as i64);
            }
        }

        let def = hp.add(def_off);
        *def = make_arityval(record_def_size_words(n) - 1);
        *def.add(1) = if n == 0 { tuple0() } else { make_boxed(hp) };
        *def.add(2) = definition_hash;
        *def.add(3) = make_atom(module);
        *def.add(4) = make_atom(name);
        *def.add(5) = make_atom(if is_exported { AM_TRUE } else { AM_FALSE });
        for (i, &decl) in sorted.iter().enumerate() {
            *def.add(RECORD_DEF_FIXED_WORDS + i) = fields[decl].0;
        }

        let inst = hp.add(inst_off);
        *inst = make_record_header(n + 1);
        *inst.add(1) = make_boxed(def);
        for (i, &decl) in sorted.iter().enumerate() {
            *inst.add(RECORD_INST_FIXED_WORDS + i) = fields[decl].1;
        }

        Ok(make_boxed(inst))
    }
}

// ---------------------------------------------------------------------------
// Instance readers
// ---------------------------------------------------------------------------

/// `record_field_count(x)` (`erl_record.h:94-95`) — the number of fields of a
/// native-record instance term (`header_arity - 1`).
///
/// A non-record or malformed term yields [`TermError::BadRecord`].
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a record, its whole block must be live.
pub unsafe fn record_field_count(t: Eterm) -> TermResult<usize> {
    // SAFETY: the caller guarantees a live boxed term.
    Ok(unsafe { instance_parts(t)? }.1)
}

/// The `record_definition` word of a native-record instance term — a boxed pointer
/// to the definition block (`ErtsRecordInstance.record_definition`, a tuple-shaped
/// `make_boxed(def_ptr)`).
///
/// A non-record or malformed term yields [`TermError::BadRecord`].
///
/// # Safety
///
/// As [`record_field_count`].
pub unsafe fn record_definition(t: Eterm) -> TermResult<Eterm> {
    // SAFETY: the caller guarantees a live boxed term.
    let (p, _) = unsafe { instance_parts(t)? };
    // SAFETY: `p` is a validated instance header; word 1 is its definition pointer.
    let def = unsafe { *p.add(1) };
    if !is_boxed_term(def) {
        return Err(TermError::BadRecord);
    }
    Ok(def)
}

/// The field **value** words of a native-record instance term, in sorted-key order.
///
/// A non-record or malformed term yields [`TermError::BadRecord`].
///
/// # Safety
///
/// `t` must be a live native-record instance. The returned slice borrows the
/// term's storage, so the caller must keep the owning heap alive and unmoved for
/// `'a`.
pub unsafe fn record_values<'a>(t: Eterm) -> TermResult<&'a [Eterm]> {
    // SAFETY: the caller guarantees a live boxed term.
    let (p, n) = unsafe { instance_parts(t)? };
    // SAFETY: `p` is a validated instance header; its `n` value words live at
    // `p[RECORD_INST_FIXED_WORDS..]` and stay valid for `'a` by the caller's
    // invariant.
    Ok(unsafe { core::slice::from_raw_parts(p.add(RECORD_INST_FIXED_WORDS), n) })
}

/// `records_get_2` (`erl_record.c:1247-1274`) — the value of the field whose key
/// equals `key`, or `None` if no field key matches.
///
/// The lookup is a linear scan over the (sorted) definition keys using
/// [`crate::compare::eq`], exactly like the C BIF. A non-atom `key` simply never
/// matches a key (record keys are always atoms), so it yields `Ok(None)`.
///
/// A non-record or malformed term yields [`TermError::BadRecord`].
///
/// # Safety
///
/// `t` must be a live native-record instance.
pub unsafe fn record_get(t: Eterm, key: Eterm, atoms: &AtomTable) -> TermResult<Option<Eterm>> {
    // SAFETY: the caller guarantees a live record.
    let (_, count) = unsafe { instance_parts(t)? };
    // SAFETY: as above.
    let def = unsafe { record_definition(t)? };
    // SAFETY: as above.
    let keys = unsafe { record_definition_keys(def)? };
    // SAFETY: as above.
    let values = unsafe { record_values(t)? };
    // A malformed instance whose definition arity disagrees with its own header
    // arity must not index out of bounds.
    if keys.len() != count || values.len() != count {
        return Err(TermError::BadRecord);
    }
    for i in 0..count {
        if eq(key, keys[i], atoms) {
            return Ok(Some(values[i]));
        }
    }
    Ok(None)
}

/// `erl_is_native_record` (`erl_record.c:214-221`) — whether `t` is a native
/// record whose definition module and name are `module`/`name`.
///
/// A non-record or malformed term yields `false` (the C caller has an
/// `ASSERT(is_record(src))`; here it is a total, panic-free predicate).
///
/// # Safety
///
/// `t` must be a live boxed term; if it is a record, its whole block must be live.
pub unsafe fn record_is_native(t: Eterm, module: Atom, name: Atom) -> bool {
    // SAFETY: the caller guarantees a live boxed term.
    let def = match unsafe { record_definition(t) } {
        Ok(def) => def,
        Err(_) => return false,
    };
    // SAFETY: as above.
    let p = match unsafe { definition_parts(def) } {
        Ok((p, _)) => p,
        Err(_) => return false,
    };
    // SAFETY: a validated definition block has the `module`/`name` atoms at words
    // 3/4; a malformed value that is not an atom simply fails the comparison.
    let (m, nm) = unsafe { (*p.add(3), *p.add(4)) };
    is_atom(m) && is_atom(nm) && atom_val(m) == module && atom_val(nm) == name
}

/// The native-record branch of `is_record_2` (`erl_bif_op.c:306-312`) — whether
/// `t` is a native record whose definition name is `name`.
///
/// A non-record or malformed term yields `false`.
///
/// # Safety
///
/// As [`record_is_native`].
pub unsafe fn record_name_is(t: Eterm, name: Atom) -> bool {
    // SAFETY: the caller guarantees a live boxed term.
    let def = match unsafe { record_definition(t) } {
        Ok(def) => def,
        Err(_) => return false,
    };
    // SAFETY: as above.
    let p = match unsafe { definition_parts(def) } {
        Ok((p, _)) => p,
        Err(_) => return false,
    };
    // SAFETY: a validated definition block has the `name` atom at word 4.
    let nm = unsafe { *p.add(4) };
    is_atom(nm) && atom_val(nm) == name
}

// ---------------------------------------------------------------------------
// Definition readers (take the definition term from `record_definition`)
// ---------------------------------------------------------------------------

/// `RECORD_DEF_FIELD_COUNT(defp)` (`erl_record.h:82-83`) — the number of fields a
/// definition describes (`header_arity - 5`).
///
/// A non-definition or malformed term yields [`TermError::BadRecord`].
///
/// # Safety
///
/// `def` must be a live boxed term; if it is a definition, its whole block must be
/// live.
pub unsafe fn record_definition_field_count(def: Eterm) -> TermResult<usize> {
    // SAFETY: the caller guarantees a live boxed term.
    Ok(unsafe { definition_parts(def)? }.1)
}

/// The definition's `module` atom term (word 3).
///
/// # Safety
///
/// As [`record_definition_field_count`].
pub unsafe fn record_definition_module(def: Eterm) -> TermResult<Eterm> {
    // SAFETY: the caller guarantees a live boxed term.
    let (p, _) = unsafe { definition_parts(def)? };
    // SAFETY: a validated definition block has word 3 live.
    Ok(unsafe { *p.add(3) })
}

/// The definition's `name` atom term (word 4).
///
/// # Safety
///
/// As [`record_definition_field_count`].
pub unsafe fn record_definition_name(def: Eterm) -> TermResult<Eterm> {
    // SAFETY: the caller guarantees a live boxed term.
    let (p, _) = unsafe { definition_parts(def)? };
    // SAFETY: a validated definition block has word 4 live.
    Ok(unsafe { *p.add(4) })
}

/// The definition's `is_exported` atom term (word 5) — [`AM_TRUE`] or
/// [`AM_FALSE`] (`erl_record.c:1328-1340`).
///
/// # Safety
///
/// As [`record_definition_field_count`].
pub unsafe fn record_definition_is_exported(def: Eterm) -> TermResult<Eterm> {
    // SAFETY: the caller guarantees a live boxed term.
    let (p, _) = unsafe { definition_parts(def)? };
    // SAFETY: a validated definition block has word 5 live.
    Ok(unsafe { *p.add(5) })
}

/// The definition's cached `hash` term (word 2) — the `make_hash2` value
/// [`write_record`] stored (see the module docs).
///
/// # Safety
///
/// As [`record_definition_field_count`].
pub unsafe fn record_definition_hash(def: Eterm) -> TermResult<Eterm> {
    // SAFETY: the caller guarantees a live boxed term.
    let (p, _) = unsafe { definition_parts(def)? };
    // SAFETY: a validated definition block has word 2 live.
    Ok(unsafe { *p.add(2) })
}

/// The definition's field **keys** (atom terms) in ascending atom-index order
/// (words `6 .. 6 + n`).
///
/// # Safety
///
/// `def` must be a live native-record definition. The returned slice borrows the
/// definition's storage, so the caller must keep the owning heap alive and unmoved
/// for `'a`.
pub unsafe fn record_definition_keys<'a>(def: Eterm) -> TermResult<&'a [Eterm]> {
    // SAFETY: the caller guarantees a live boxed term.
    let (p, n) = unsafe { definition_parts(def)? };
    // SAFETY: a validated definition block has its `n` key words live at
    // `p[RECORD_DEF_FIXED_WORDS..]` and they stay valid for `'a`.
    Ok(unsafe { core::slice::from_raw_parts(p.add(RECORD_DEF_FIXED_WORDS), n) })
}

/// The definition's `field_order` tuple elements (small integers) — `order[i]` is
/// the sorted rank of the field declared `i`-th (`erl_record.c:1072-1073`).
///
/// A malformed `field_order` word (not a tuple of arity `n`) yields
/// [`TermError::BadRecord`].
///
/// # Safety
///
/// As [`record_definition_keys`].
pub unsafe fn record_definition_field_order<'a>(def: Eterm) -> TermResult<&'a [Eterm]> {
    // SAFETY: the caller guarantees a live boxed term.
    let (p, n) = unsafe { definition_parts(def)? };
    // SAFETY: a validated definition block has the boxed `field_order` pointer at
    // word 1.
    let order = unsafe { *p.add(1) };
    if !is_boxed_term(order) {
        return Err(TermError::BadRecord);
    }
    let op = boxed_val(order);
    // SAFETY: `order` is a boxed pointer to a live block (caller invariant).
    let header = unsafe { *op };
    if !is_arity_value(header) || header_arity(header) != n {
        return Err(TermError::BadRecord);
    }
    // SAFETY: the `field_order` tuple has arity `n`, so its `n` element words are
    // live at `op[1..]` and stay valid for `'a`.
    Ok(unsafe { core::slice::from_raw_parts(op.add(1), n) })
}

/// The definition's keys in **declaration** order — the order
/// `records_get_field_names_1` (`erl_record.c:1298-1326`) walks:
/// `declaration_keys[i] == keys[unsigned_val(order[i])]`.
///
/// A malformed `field_order` entry (not a small integer, or out of range) yields
/// [`TermError::BadRecord`].
///
/// # Safety
///
/// As [`record_definition_keys`].
pub unsafe fn record_declaration_keys(def: Eterm) -> TermResult<Vec<Eterm>> {
    // SAFETY: the caller guarantees a live boxed term.
    let keys = unsafe { record_definition_keys(def)? };
    // SAFETY: as above.
    let order = unsafe { record_definition_field_order(def)? };
    let mut out = Vec::with_capacity(keys.len());
    for &entry in order {
        if !is_small(entry) {
            return Err(TermError::BadRecord);
        }
        match keys.get(unsigned_value(entry) as usize) {
            Some(&key) => out.push(key),
            None => return Err(TermError::BadRecord),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{is_record, make_header};
    use crate::heap::Heap;
    use crate::immediate::NIL;
    use crate::tag::RECORD_SUBTAG;
    use crate::thing::write_flatmap;

    /// A table seeded like the standard `atom.names` (`false`, `true` first), with
    /// the conventional module/name/field atoms interned after it.
    fn atoms() -> AtomTable {
        let mut table = AtomTable::new();
        table.seed(["false", "true"]);
        table.seed(["m", "r", "a", "b", "c"]);
        table
    }

    #[test]
    fn block_and_size_math() {
        // RECORD_INST_SIZE(n) = 2 + n, RECORD_DEF_SIZE(n) = 6 + n.
        assert_eq!(record_inst_size_words(0), 2);
        assert_eq!(record_inst_size_words(3), 5);
        assert_eq!(record_def_size_words(0), 6);
        assert_eq!(record_def_size_words(3), 9);
        // n == 0: no field_order tuple, so def + instance only.
        assert_eq!(record_block_words(0), 8);
        // n > 0: (n + 1) + (6 + n) + (2 + n) = 3n + 9.
        assert_eq!(record_block_words(1), 12);
        assert_eq!(record_block_words(2), 15);
        assert_eq!(record_block_words(3), 18);
        assert_eq!(definition_offset(0), 0);
        assert_eq!(definition_offset(3), 4);
    }

    #[test]
    fn declaration_order_differs_from_atom_order() {
        let t = atoms();
        let a = t.lookup("a").unwrap();
        let b = t.lookup("b").unwrap();
        let m = t.lookup("m").unwrap();
        let r = t.lookup("r").unwrap();
        assert!(a.raw() < b.raw(), "atom a must precede atom b");

        let va = make_small(11);
        let vb = make_small(22);
        // Declared [(b, vb), (a, va)] — the reverse of the atom-index order.
        let fields = [(make_atom(b), vb), (make_atom(a), va)];

        let mut heap = Heap::new(record_block_words(2));
        let hp = unsafe { heap.alloc_words(record_block_words(2)).unwrap() };
        let rec = unsafe { write_record(hp, m, r, false, &fields, make_small(0)).unwrap() };

        // Instance header: make_header(n + 1, RECORD_SUBTAG) with n == 2.
        let inst = boxed_val(rec);
        assert!(unsafe { is_record(rec) });
        assert_eq!(unsafe { *inst }, make_header(3, RECORD_SUBTAG));
        assert_eq!(unsafe { *inst }, make_record_header(3));
        assert_eq!(unsafe { record_field_count(rec) }.unwrap(), 2);

        // Definition: arityval(5 + n) = arityval(7); module/name/is_exported.
        let def = unsafe { record_definition(rec) }.unwrap();
        assert_eq!(boxed_val(def), unsafe { hp.add(3) });
        assert_eq!(unsafe { *boxed_val(def) }, make_arityval(7));
        assert_eq!(unsafe { record_definition_field_count(def) }.unwrap(), 2);
        assert_eq!(
            unsafe { record_definition_module(def) }.unwrap(),
            make_atom(m)
        );
        assert_eq!(
            unsafe { record_definition_name(def) }.unwrap(),
            make_atom(r)
        );
        assert_eq!(
            unsafe { record_definition_is_exported(def) }.unwrap(),
            make_atom(AM_FALSE)
        );
        assert_eq!(
            unsafe { record_definition_hash(def) }.unwrap(),
            make_small(0)
        );

        // Keys sorted by atom index => [a, b]; values in the same order.
        let keys = unsafe { record_definition_keys(def) }.unwrap();
        assert_eq!(keys, &[make_atom(a), make_atom(b)]);
        assert_eq!(unsafe { record_values(rec) }.unwrap(), &[va, vb]);
        // field_order maps declaration -> sorted: b (declared 0) ranks 1, a ranks 0.
        let order = unsafe { record_definition_field_order(def) }.unwrap();
        assert_eq!(order, &[make_small(1), make_small(0)]);
        // Declaration order is preserved by record_declaration_keys.
        assert_eq!(
            unsafe { record_declaration_keys(def) }.unwrap(),
            vec![make_atom(b), make_atom(a)]
        );

        // record_get finds both fields by key and reports misses for the rest.
        assert_eq!(
            unsafe { record_get(rec, make_atom(b), &t) }.unwrap(),
            Some(vb)
        );
        assert_eq!(
            unsafe { record_get(rec, make_atom(a), &t) }.unwrap(),
            Some(va)
        );
        assert_eq!(unsafe { record_get(rec, make_atom(m), &t) }.unwrap(), None);
        assert_eq!(unsafe { record_get(rec, make_small(1), &t) }.unwrap(), None);

        assert!(unsafe { record_is_native(rec, m, r) });
        assert!(!unsafe { record_is_native(rec, r, m) });
        assert!(unsafe { record_name_is(rec, r) });
        assert!(!unsafe { record_name_is(rec, m) });
    }

    #[test]
    fn empty_record() {
        let t = atoms();
        let m = t.lookup("m").unwrap();
        let r = t.lookup("r").unwrap();

        let mut heap = Heap::new(record_block_words(0));
        let hp = unsafe { heap.alloc_words(record_block_words(0)).unwrap() };
        let rec = unsafe { write_record(hp, m, r, true, &[], make_small(7)).unwrap() };

        // Instance header: make_header(1, RECORD_SUBTAG).
        assert_eq!(unsafe { *boxed_val(rec) }, make_record_header(1));
        assert_eq!(unsafe { record_field_count(rec) }.unwrap(), 0);
        assert_eq!(unsafe { record_values(rec) }.unwrap(), &[]);

        // The definition starts at word 0 (no field_order tuple).
        let def = unsafe { record_definition(rec) }.unwrap();
        assert_eq!(boxed_val(def), hp);
        assert_eq!(unsafe { *boxed_val(def) }, make_arityval(5));
        assert_eq!(unsafe { record_definition_field_count(def) }.unwrap(), 0);
        assert_eq!(
            unsafe { record_definition_is_exported(def) }.unwrap(),
            make_atom(AM_TRUE)
        );
        // n == 0 uses the shared `tuple0()` literal for the field order.
        assert_eq!(unsafe { record_definition_field_order(def) }.unwrap(), &[]);
        assert!(unsafe { record_declaration_keys(def) }.unwrap().is_empty());
        assert_eq!(
            unsafe { record_get(rec, make_atom(t.lookup("a").unwrap()), &t) }.unwrap(),
            None
        );
        assert!(unsafe { record_is_native(rec, m, r) });
    }

    #[test]
    fn one_and_three_field_round_trips() {
        let t = atoms();
        let a = t.lookup("a").unwrap();
        let b = t.lookup("b").unwrap();
        let c = t.lookup("c").unwrap();
        let m = t.lookup("m").unwrap();
        let r = t.lookup("r").unwrap();

        // n == 1: already-ordered declaration.
        let mut heap = Heap::new(record_block_words(1));
        let hp = unsafe { heap.alloc_words(record_block_words(1)).unwrap() };
        let rec = unsafe {
            write_record(
                hp,
                m,
                r,
                false,
                &[(make_atom(a), make_small(1))],
                make_small(3),
            )
        }
        .unwrap();
        assert_eq!(unsafe { record_field_count(rec) }.unwrap(), 1);
        let def = unsafe { record_definition(rec) }.unwrap();
        assert_eq!(unsafe { *boxed_val(def) }, make_arityval(6));
        assert_eq!(
            unsafe { record_definition_field_order(def) }.unwrap(),
            &[make_small(0)]
        );
        assert_eq!(unsafe { record_values(rec) }.unwrap(), &[make_small(1)]);
        assert_eq!(
            unsafe { record_get(rec, make_atom(a), &t) }.unwrap(),
            Some(make_small(1))
        );

        // n == 3, declared out of order: [c, a, b] => sorted [a, b, c].
        let mut heap = Heap::new(record_block_words(3));
        let hp = unsafe { heap.alloc_words(record_block_words(3)).unwrap() };
        let fields = [
            (make_atom(c), make_small(30)),
            (make_atom(a), make_small(10)),
            (make_atom(b), make_small(20)),
        ];
        let rec = unsafe { write_record(hp, m, r, false, &fields, make_small(5)).unwrap() };
        assert_eq!(unsafe { record_field_count(rec) }.unwrap(), 3);
        let def = unsafe { record_definition(rec) }.unwrap();
        assert_eq!(unsafe { *boxed_val(def) }, make_arityval(8));
        assert_eq!(
            unsafe { record_definition_keys(def) }.unwrap(),
            &[make_atom(a), make_atom(b), make_atom(c)]
        );
        assert_eq!(
            unsafe { record_values(rec) }.unwrap(),
            &[make_small(10), make_small(20), make_small(30)]
        );
        // Declaration [c, a, b]: c ranks 2, a ranks 0, b ranks 1.
        assert_eq!(
            unsafe { record_definition_field_order(def) }.unwrap(),
            &[make_small(2), make_small(0), make_small(1)]
        );
        assert_eq!(
            unsafe { record_declaration_keys(def) }.unwrap(),
            vec![make_atom(c), make_atom(a), make_atom(b)]
        );
        for (key, value) in [
            (a, make_small(10)),
            (b, make_small(20)),
            (c, make_small(30)),
        ] {
            assert_eq!(
                unsafe { record_get(rec, make_atom(key), &t) }.unwrap(),
                Some(value)
            );
        }
        assert_eq!(
            unsafe { record_definition_hash(def) }.unwrap(),
            make_small(5)
        );
    }

    #[test]
    fn write_rejects_duplicate_and_non_atom_keys() {
        let t = atoms();
        let a = t.lookup("a").unwrap();
        let m = t.lookup("m").unwrap();
        let r = t.lookup("r").unwrap();
        let mut heap = Heap::new(64);
        let hp = unsafe { heap.alloc_words(record_block_words(2)).unwrap() };
        assert_eq!(
            unsafe {
                write_record(
                    hp,
                    m,
                    r,
                    false,
                    &[(make_atom(a), make_small(1)), (make_atom(a), make_small(2))],
                    make_small(0),
                )
            },
            Err(TermError::BadRecord)
        );
        assert_eq!(
            unsafe {
                write_record(
                    hp,
                    m,
                    r,
                    false,
                    &[(make_small(1), make_small(2))],
                    make_small(0),
                )
            },
            Err(TermError::BadRecord)
        );
        assert_eq!(
            unsafe { write_record(hp, m, r, false, &[(NIL, make_small(2))], make_small(0)) },
            Err(TermError::BadRecord)
        );
    }

    #[test]
    fn readers_reject_non_records() {
        let t = atoms();
        let m = t.lookup("m").unwrap();
        let r = t.lookup("r").unwrap();
        let a = t.lookup("a").unwrap();

        let mut heap = Heap::new(64);
        // An ordinary tuple is not a record.
        let tuple = heap
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .unwrap();
        // A short tuple is not a definition either.
        let def_like = heap.tuple(&[make_small(1)]).unwrap();
        // A flat map (an empty map is a 3-word block).
        let map_hp = unsafe { heap.alloc_words(3).unwrap() };
        let map = unsafe { write_flatmap(map_hp, &[]) };

        for &bad in &[tuple, def_like, map, NIL, make_small(42), make_atom(m)] {
            assert_eq!(
                unsafe { record_field_count(bad) },
                Err(TermError::BadRecord)
            );
            assert_eq!(unsafe { record_definition(bad) }, Err(TermError::BadRecord));
            assert_eq!(unsafe { record_values(bad) }, Err(TermError::BadRecord));
            assert_eq!(
                unsafe { record_get(bad, make_atom(m), &t) },
                Err(TermError::BadRecord)
            );
            assert!(!unsafe { record_is_native(bad, m, r) });
            assert!(!unsafe { record_name_is(bad, r) });
            assert_eq!(
                unsafe { record_definition_field_count(bad) },
                Err(TermError::BadRecord)
            );
            assert_eq!(
                unsafe { record_definition_keys(bad) },
                Err(TermError::BadRecord)
            );
            assert_eq!(
                unsafe { record_definition_field_order(bad) },
                Err(TermError::BadRecord)
            );
            assert_eq!(
                unsafe { record_declaration_keys(bad) },
                Err(TermError::BadRecord)
            );
        }

        // A definition-only term is not an instance: its header is an arityval.
        let mut heap = Heap::new(record_block_words(1));
        let hp = unsafe { heap.alloc_words(record_block_words(1)).unwrap() };
        let rec = unsafe {
            write_record(
                hp,
                m,
                r,
                false,
                &[(make_atom(a), make_small(9))],
                make_small(0),
            )
        }
        .unwrap();
        let def = unsafe { record_definition(rec) }.unwrap();
        assert_eq!(
            unsafe { record_field_count(def) },
            Err(TermError::BadRecord)
        );
        assert_eq!(unsafe { record_values(def) }, Err(TermError::BadRecord));
        assert!(!unsafe { record_is_native(def, m, r) });

        // A short tuple is also not a valid definition.
        assert_eq!(
            unsafe { record_definition_field_count(def_like) },
            Err(TermError::BadRecord)
        );
    }

    #[test]
    fn record_definition_rejects_malformed_definition_word() {
        // An instance whose definition word is not boxed is rejected.
        let words = [make_record_header(1), make_small(0)];
        let rec = make_boxed(words.as_ptr());
        assert_eq!(unsafe { record_definition(rec) }, Err(TermError::BadRecord));
    }
}
