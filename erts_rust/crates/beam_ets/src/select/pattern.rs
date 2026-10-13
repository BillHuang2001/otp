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

//! Head patterns: decode a match-spec head term into a [`Pattern`] and match a
//! table object against it.
//!
//! Mirrors the C `dmc_expr` head path (`erl_db_util.c`): `'_'` is a wildcard,
//! `'$N'` binds (a repeated `'$N'` compares equal), tuples, cons cells and **maps**
//! recurse structurally, and **every other atom is an exact (`=:=`) literal** —
//! including `'$_'`/`'$$'`, which are special only in a *body* (the C `dmc_one_term`
//! emits `matchEq` for any atom that is not `'_'`/`'$N'`, `erl_db_util.c:4117-4121`).
//!
//! **Maps in a head pattern.** A head map's **keys stay literal** — a variable or
//! `'_'` key makes the whole spec `badarg` at compile time (the C
//! "Variable found in map key."/"Underscore found in map key."), at every nesting
//! depth. The **values** are matched structurally, exactly like tuple elements: a
//! wildcard/variable/literal value is compared as its element pattern, so
//! `#{x => '_'}` matches any value for key `x` and `#{x => '$2'}` binds it.
//! Probed against the system `erl` (see `tests/select.rs`).

use std::collections::BTreeSet;

use beam_common::Result;
use beam_term::prelude::*;

use super::expr::classify;
use super::{atom_name, tuple_elements, var_index};
use crate::error::badarg;

/// One head pattern node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// `'_'` — matches any term.
    Wildcard,
    /// `'$N'` — matches any term and binds it to variable `N`.
    Bind(u32),
    /// Any other term — must compare equal (exactly, `=:=`). `'$_'`/`'$$'` land
    /// here: in a **head** they are ordinary literal atoms (they are special only
    /// in a body).
    Literal(Eterm),
    /// A tuple pattern: matches a tuple of the same arity whose elements match.
    Tuple(Vec<Pattern>),
    /// A map pattern: each entry is a **literal** key term plus the recursively
    /// decoded **value** pattern. Matches a map that holds every key with a
    /// matching value (extra keys in the object are allowed — the C `matchMap`
    /// only requires `map_size(object) >= n`). A non-literal (variable/`'_'`) key
    /// is rejected at compile time, so `decode` never produces one.
    Map(Vec<(Eterm, Pattern)>),
    /// A cons pattern: matches a non-empty list whose head/tail match.
    Cons(Box<Pattern>, Box<Pattern>),
}

/// Decode a head pattern term, recording every `'$N'` it binds in `vars`.
pub(super) fn decode(term: Eterm, atoms: &AtomTable, vars: &mut BTreeSet<u32>) -> Result<Pattern> {
    if let Some(name) = atom_name(term, atoms) {
        match name {
            "_" => return Ok(Pattern::Wildcard),
            _ => {
                // `'$0'`/`'$N'` bind; any other atom — including a malformed
                // `$`-atom such as `'$_'`/`'$$'`/`'$x'` (the C `db_is_variable`
                // returns -1 for them) — is an ordinary literal.
                if let Some(index) = var_index(name) {
                    vars.insert(index);
                    return Ok(Pattern::Bind(index));
                }
            }
        }
    }
    if let Some(elements) = tuple_elements(term) {
        let mut patterns = Vec::with_capacity(elements.len());
        for element in elements {
            patterns.push(decode(element, atoms, vars)?);
        }
        return Ok(Pattern::Tuple(patterns));
    }
    if is_list(term) {
        // SAFETY: `is_list(term)` established that `term` is a live cons cell, so
        // its two `[CAR, CDR]` words are readable; the spec term is caller-owned
        // and stays live for the whole call.
        let (head_term, tail_term) = unsafe { (car(term), cdr(term)) };
        let head = Box::new(decode(head_term, atoms, vars)?);
        let tail = Box::new(decode(tail_term, atoms, vars)?);
        return Ok(Pattern::Cons(head, tail));
    }
    if classify(term) == TermClass::Map {
        // A head map: keys stay literal, values recurse. Read the pairs through the
        // shared `beam_term::map` primitive (never a local map reader).
        let pairs = map_pairs(term, atoms).map_err(|_| badarg())?;
        // The C `db_match_compile` rejects a variable/`'_'` key outright, so the
        // check runs before any value is decoded — and, because `decode` recurses
        // through tuples/lists/maps, it fires at every nesting depth.
        for (key, _) in &pairs {
            let is_key_variable = atom_name(*key, atoms)
                .map(|name| var_index(name).is_some())
                .unwrap_or(false);
            let is_key_wildcard = matches!(atom_name(*key, atoms), Some("_"));
            if is_key_variable || is_key_wildcard {
                return Err(badarg());
            }
        }
        let mut entries = Vec::with_capacity(pairs.len());
        for (key, value) in pairs {
            entries.push((key, decode(value, atoms, vars)?));
        }
        return Ok(Pattern::Map(entries));
    }
    // Any other value (immediates, floats, bignums, binrefs, …) is compared as an
    // exact literal.
    Ok(Pattern::Literal(term))
}

/// Match `object` against `pattern`, recording the bindings in `binds`.
///
/// Returns whether the pattern matched. On a failure the caller must discard
/// `binds` (a partially-filled binding list is meaningless).
pub(super) fn matches(
    pattern: &Pattern,
    object: Eterm,
    atoms: &AtomTable,
    binds: &mut Vec<(u32, Eterm)>,
) -> bool {
    match pattern {
        Pattern::Wildcard => true,
        // A repeated `'$N'` must compare equal (the C emits `matchCmp`).
        Pattern::Literal(literal) => eq(*literal, object, atoms),
        Pattern::Bind(index) => match binds.iter().find(|(var, _)| var == index) {
            Some(&(_, value)) => eq(value, object, atoms),
            None => {
                binds.push((*index, object));
                true
            }
        },
        Pattern::Tuple(patterns) => {
            // SAFETY: `is_tuple` classifies a live term through its tag/header word
            // only, so it is sound for any value word.
            if !unsafe { is_tuple(object) } {
                return false;
            }
            // SAFETY: `is_tuple` established a live tuple header, so the arity is
            // readable; the object is owned by the table for the call's duration.
            let arity = unsafe { tuple_size(object) };
            if arity != patterns.len() {
                return false;
            }
            for (index, pattern) in patterns.iter().enumerate() {
                // SAFETY: `index < arity == tuple_size(object)`.
                let element = unsafe { tuple_element(object, index) };
                if !matches(pattern, element, atoms, binds) {
                    return false;
                }
            }
            true
        }
        Pattern::Cons(head, tail) => {
            if !is_list(object) {
                return false;
            }
            // SAFETY: `is_list(object)` established a live cons cell.
            let (car_term, cdr_term) = unsafe { (car(object), cdr(object)) };
            matches(head, car_term, atoms, binds) && matches(tail, cdr_term, atoms, binds)
        }
        Pattern::Map(entries) => {
            if classify(object) != TermClass::Map {
                return false;
            }
            // Every key must be present with a matching value; an absent key (`None`)
            // or a non-map read (`Err`) is a non-match. Because all keys must be
            // present, `map_size(object) >= entries.len()` is implied (the C
            // `matchMap`'s explicit size check).
            for (key, value_pattern) in entries {
                match map_get(object, *key, atoms) {
                    Ok(Some(value)) => {
                        if !matches(value_pattern, value, atoms, binds) {
                            return false;
                        }
                    }
                    Ok(None) | Err(_) => return false,
                }
            }
            true
        }
    }
}