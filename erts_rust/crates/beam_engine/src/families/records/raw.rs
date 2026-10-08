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

//! Raw native-record layout access — the records family's **single**
//! `#[allow(unsafe_code)]` module.
//!
//! `beam_term::record` owns the frozen `ErtsRecordDefinition` /
//! `ErtsRecordInstance` layout (the term-layout lowest common ancestor, workspace
//! `CONTEXT.md` §2) and publishes its readers/writers as `unsafe fn` — each
//! dereferences the header word of a boxed term. This module is the one place the
//! records family lifts the crate-wide `#![deny(unsafe_code)]` to call them,
//! exactly as [`crate::families::bitstrings::raw`] does for the two frozen bit
//! layouts, so the dispatcher ([`super`]) stays safe Rust. **No record layout is
//! re-derived here**: every function forwards to `beam_term::record`.
//!
//! # Safety argument
//!
//! Every entry point is called with a term the machine already holds — an X/Y-slot
//! value produced by [`crate::operand::read_operand`], a record instance this
//! module (`beam_term`) has already validated, or a definition obtained from
//! [`record_definition`]. The `beam_term` readers are themselves total over a
//! possibly-malformed input (a non-record yields `TermError::BadRecord`, or
//! `false` for the predicates), so no wrapper here can panic or read outside a
//! term's own block.
#![allow(unsafe_code)]
use beam_common::Result;
use beam_term::{Atom, AtomTable, Eterm};

/// `record_definition(x)` (`erl_record.h`) — the definition term of the
/// native-record instance `x`, or `None` when `x` is not a live native record.
///
/// The C callers have an `ASSERT(is_record(src))`; this is the total form, so a
/// non-record is reported rather than dereferenced as one.
pub(crate) fn record_definition(x: Eterm) -> Option<Eterm> {
    // SAFETY: `x` is a live machine word; `beam_term::record_definition` checks
    // `is_boxed` and the `RECORD` header before dereferencing, and only ever reads
    // the block's own words.
    unsafe { beam_term::record_definition(x) }.ok()
}

/// `erl_is_native_record(x, module, name)` (`erl_record.c:214-221`) — whether `x`
/// is a native record whose definition lives in `module` and is named `name`.
///
/// A non-record (or malformed record) yields `false`, which is exactly the C
/// caller's `if (!erl_is_native_record(..)) $FAIL($Fail)` branch.
pub(crate) fn record_is_native(x: Eterm, module: Atom, name: Atom) -> bool {
    // SAFETY: as [`record_definition`]; the `beam_term` predicate is total.
    unsafe { beam_term::record_is_native(x, module, name) }
}

/// Whether the native record `x` has the definition **name** `name`
/// (`erl_record.c:800`, the local form's `defp->name != name` test).
///
/// A non-record yields `false`.
pub(crate) fn record_name_is(x: Eterm, name: Atom) -> bool {
    // SAFETY: as [`record_definition`].
    unsafe { beam_term::record_name_is(x, name) }
}

/// The instance's field **values**, in ascending-key order (`record_values`).
///
/// A non-record or malformed record is the typed `BeamError` the `beam_term`
/// reader already produces (`TermError::BadRecord`); nothing is swallowed.
pub(crate) fn record_values(x: Eterm) -> Result<Vec<Eterm>> {
    // SAFETY: `x` is a validated native-record instance (the caller checked
    // `predicates::is_record`); `beam_term` re-validates the header and hands back
    // a slice borrowing the instance's own value words.
    Ok(unsafe { beam_term::record_values(x) }?.to_vec())
}

/// The value of the field keyed `key`, or `None` when the record has no such
/// field (`records_get_2`, `erl_record.c:1247-1274`).
pub(crate) fn record_get(x: Eterm, key: Atom, atoms: &AtomTable) -> Result<Option<Eterm>> {
    // SAFETY: as [`record_values`]; the lookup is a linear scan of the record's
    // own key words.
    Ok(unsafe { beam_term::record_get(x, beam_term::make_atom(key), atoms) }?)
}

/// `erl_is_record_accessible(src)`'s predicate (`erl_record.c:764-771`):
/// `defp->is_exported == am_true`.
///
/// A non-record yields `Ok(false)` — the C caller's `ASSERT(is_record(src))` is
/// not observable, and an unaccessible term is precisely what the guard rejects.
pub(crate) fn record_is_exported(x: Eterm) -> Result<bool> {
    let Some(def) = record_definition(x) else {
        return Ok(false);
    };
    // SAFETY: `def` came from `record_definition`, so it is a validated definition
    // block; `beam_term` re-validates its header and reads word 5.
    let exported = unsafe { beam_term::record_definition_is_exported(def) }?;
    Ok(exported == beam_term::make_atom(beam_term::AM_TRUE))
}

/// The module half of `erl_is_wildcard_record_accessible(src, module)`
/// (`erl_record.c:773-780`): whether the record's definition was created in
/// `module`.
///
/// A non-record (or a definition whose `module` word is not an atom) yields
/// `Ok(false)`.
pub(crate) fn record_definition_module_is(x: Eterm, module: Atom) -> Result<bool> {
    let Some(def) = record_definition(x) else {
        return Ok(false);
    };
    // SAFETY: `def` is a validated definition block; word 3 is its `module` atom.
    let owner = unsafe { beam_term::record_definition_module(def) }?;
    Ok(beam_term::is_atom(owner) && beam_term::atom_val(owner) == module)
}

/// The definition's `module` atom (`ErtsRecordDefinition.module`).
pub(crate) fn definition_module(def: Eterm) -> Result<Atom> {
    // SAFETY: `def` is a definition block produced by [`record_definition`].
    let term = unsafe { beam_term::record_definition_module(def) }?;
    atom_of(term)
}

/// The definition's `name` atom (`ErtsRecordDefinition.name`).
pub(crate) fn definition_name(def: Eterm) -> Result<Atom> {
    // SAFETY: as [`definition_module`]; word 4 is the `name` atom.
    let term = unsafe { beam_term::record_definition_name(def) }?;
    atom_of(term)
}

/// The definition's `is_exported` flag as a Rust `bool` (`AM_TRUE` ⇒ `true`).
pub(crate) fn definition_is_exported(def: Eterm) -> Result<bool> {
    // SAFETY: as [`definition_module`]; word 5 is the `is_exported` atom.
    let term = unsafe { beam_term::record_definition_is_exported(def) }?;
    Ok(term == beam_term::make_atom(beam_term::AM_TRUE))
}

/// The definition's cached `hash` term (`ErtsRecordDefinition.hash`).
pub(crate) fn definition_hash(def: Eterm) -> Result<Eterm> {
    // SAFETY: as [`definition_module`]; word 2 is the cached hash.
    Ok(unsafe { beam_term::record_definition_hash(def) }?)
}

/// `RECORD_DEF_FIELD_COUNT(defp)` (`erl_record.h:82-83`) — the number of fields
/// the definition describes.
pub(crate) fn definition_field_count(def: Eterm) -> Result<usize> {
    // SAFETY: as [`definition_module`]; the header arity bounds the field count.
    Ok(unsafe { beam_term::record_definition_field_count(def) }?)
}

/// The definition's field **keys** in ascending atom-index order (the order the
/// instance's values are stored in).
pub(crate) fn definition_keys(def: Eterm) -> Result<Vec<Eterm>> {
    // SAFETY: as [`definition_module`]; the keys are the definition's words
    // `6 .. 6 + n`, borrowed from its own block and copied out here.
    Ok(unsafe { beam_term::record_definition_keys(def) }?.to_vec())
}

/// The definition's field keys in **declaration** order (`field_order` applied),
/// the order `write_record` takes its input in.
pub(crate) fn declaration_keys(def: Eterm) -> Result<Vec<Eterm>> {
    // SAFETY: as [`definition_module`]; walks the definition's own `field_order`
    // tuple and key words.
    Ok(unsafe { beam_term::record_declaration_keys(def) }?)
}

/// The definition's `field_order` elements (the sorted rank of each declared
/// field, as small integers).
pub(crate) fn definition_field_order(def: Eterm) -> Result<Vec<Eterm>> {
    // SAFETY: as [`definition_module`]; the elements are the `field_order` tuple's
    // own words, copied out here.
    Ok(unsafe { beam_term::record_definition_field_order(def) }?.to_vec())
}

/// Lay out a native-record construction block — `[field_order tuple][definition]
/// [instance]` — at `hp` and return the tagged **instance** root
/// (`beam_term::write_record`).
///
/// `fields` are `(key atom term, value)` pairs in **declaration** order; callers
/// must have reserved [`beam_term::record_block_words`]`(fields.len())` words.
pub(crate) fn write_record(
    hp: *mut Eterm,
    module: Atom,
    name: Atom,
    is_exported: bool,
    fields: &[(Eterm, Eterm)],
    definition_hash: Eterm,
) -> Result<Eterm> {
    // SAFETY: the caller (the family's `write_record_instance`) reserved
    // `record_block_words(fields.len())` exclusively-owned words at `hp` via
    // `ExecutionContext::alloc_words` and keeps them unmoved until the returned
    // term is written to its destination register; every key is an atom and
    // duplicate keys are rejected by `beam_term`.
    Ok(unsafe { beam_term::write_record(hp, module, name, is_exported, fields, definition_hash) }?)
}

/// The atom encoded in the atom term `term`, or the typed `unsupported` error when
/// a malformed definition holds a non-atom word where its `module`/`name` belongs.
fn atom_of(term: Eterm) -> Result<Atom> {
    if beam_term::is_atom(term) {
        Ok(beam_term::atom_val(term))
    } else {
        Err(crate::error::unsupported(
            "native record: definition module/name is not an atom",
        ))
    }
}
