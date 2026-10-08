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

//! The [`Table`] selection methods: `select`, `select_delete`, `select_count` and
//! the `match` family, all built on the compiler in [`super::compile`] and the
//! executor here.
//!
//! `ets:match(T, P)` is `ets:select(T, [{P, [], ['$$']}])` and
//! `ets:match_object(T, P)` is `ets:select(T, [{P, [], ['$_']}])` — verified
//! against OTP 29 (`erl_db.c` `ets_match_2`/`ets_match_object_2` build exactly
//! those specs), so both are implemented by compiling the pattern and running the
//! corresponding body.
//!
//! `ets:match_delete(T, P)` is `ets:select_delete(T, [{P, [], [true]}])`: its body
//! is the literal `true`, **not** `'$_'` (an `'$_'` body would never satisfy
//! `select_delete`'s "the body value is `true`" test and would delete nothing).
//!
//! # Bodies produce one result per matched object
//!
//! A clause body is a **sequence** of expressions: each is evaluated in order and
//! the clause's value is the value of the **last** one (the C `compile_guard_expr`
//! discards every intermediate value with `matchWaste` and turns the final one
//! into `matchReturn`). So `[{H, [], ['$2', '$1']}]` yields one result per matched
//! object — the value of `'$1'` — **not** one result per body element. Verified
//! against the system `erl`: `ets:select(T, [{{'$1','$2'}, [], ['$2','$1']}])` on
//! `{1,10}`/`{2,20}` returns `[1,2]`, not `[10,1,20,2]`.

use std::collections::BTreeSet;

use beam_alloc::Heap;
use beam_common::{BeamError, Result};
use beam_term::prelude::*;

use super::{expr, pattern, MatchClause, MatchSpec};
use crate::key::key_of;
use crate::kind::TableKind;
use crate::table::Table;

/// Which body a pattern-only spec (`match_object`/`match`/`match_delete`) uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// `'$_'` — the whole matched object (`ets:match_object/2`).
    WholeObject,
    /// `'$$'` — every bound head variable, ascending (`ets:match/2`).
    AllBindings,
    /// The literal `true` (`ets:match_delete/2`'s
    /// `ets:select_delete(T, [{P, [], [true]}])`).
    LiteralTrue,
}

/// Build a one-clause spec from a bare pattern term (the `match_object`/`match`/
/// `match_delete` form).
pub(super) fn spec_for_pattern(
    pattern_term: Eterm,
    atoms: &AtomTable,
    kind: BodyKind,
) -> Result<MatchSpec> {
    let mut vars = BTreeSet::new();
    let head = pattern::decode(pattern_term, atoms, &mut vars)?;
    let body = match kind {
        BodyKind::WholeObject => expr::Expr::WholeObject,
        BodyKind::AllBindings => expr::Expr::AllBindings(vars.iter().copied().collect()),
        BodyKind::LiteralTrue => expr::Expr::Bool(true),
    };
    Ok(MatchSpec {
        clauses: vec![MatchClause {
            head,
            guards: Vec::new(),
            body: vec![body],
        }],
    })
}

/// Run `spec` against one object: the value of the first clause whose head
/// matches and whose guards all hold, or `None` when no clause matches.
///
/// A guard that raises makes its clause fail (mirroring the C `matchCatch`), while
/// a **body** expression that raises at runtime is replaced by the atom `EXIT`
/// and evaluation continues (the C `FAIL_TERM`) — see the body comment below.
pub(super) fn run_clauses(
    spec: &MatchSpec,
    object: Eterm,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> Result<Option<expr::Value>> {
    for clause in &spec.clauses {
        let mut binds: Vec<(u32, Eterm)> = Vec::new();
        if !pattern::matches(&clause.head, object, atoms, &mut binds) {
            continue;
        }
        if !guards_hold(&clause.guards, &binds, object, atoms, heap) {
            continue;
        }
        // The body is a sequence: the clause's value is the value of its **last**
        // expression (see the module docs). `compile` rejects an empty body, so a
        // clause reaching here always carries at least one expression.
        //
        // A body expression that fails at runtime does **not** abort the clause:
        // the C compiles every body under `matchCatch`, which sets `do_catch`, so
        // the `matchCall*` operations substitute `FAIL_TERM` — `am_EXIT`
        // (`../../erts/emulator/beam/erl_db_util.c`) — and evaluation continues
        // with the next expression. Verified against the system `erl` (OTP 29 /
        // erts-17.1): `ets:select(T, [{{'_','_'},[],[{hd,5}]}])` on `{1,10}`/`{2,20}`
        // is `['EXIT','EXIT']`, while `ets:select(T, [{{'_','_'},[],[{hd,5},7]}])`
        // is `[7,7]` — the failure is discarded because a later expression supplies
        // the clause's value. `select_count`/`select_delete` therefore see `EXIT`
        // (never `true`) and report `0`.
        let mut value = None;
        for expression in &clause.body {
            match expr::eval(expression, &binds, object, atoms, heap) {
                Ok(evaluated) => value = Some(evaluated),
                Err(BeamError::BadArg) => {
                    value = Some(expr::Value::Term(exit_term(atoms)?));
                }
                // Only the oracle's own `badarg` becomes `FAIL_TERM`: a typed
                // capability gap must never be masked (workspace §3).
                Err(other) => return Err(other),
            }
        }
        if let Some(value) = value {
            return Ok(Some(value));
        }
        // An empty body: only reachable for a hand-built `MatchSpec`, which the
        // public `compile` never produces — treat it as the compile error it is.
        return Err(crate::error::badarg());
    }
    Ok(None)
}

/// The term a failed body expression is replaced by: the atom `EXIT` (the C
/// `FAIL_TERM`). `badarg` when the caller's table has not interned it — the C's
/// `FAIL_TERM` is a static atom that always exists, so an uninterned `EXIT` is a
/// capability gap here, never a silently substituted atom.
fn exit_term(atoms: &AtomTable) -> Result<Eterm> {
    atoms
        .lookup("EXIT")
        .map(make_atom)
        .ok_or_else(crate::error::badarg)
}

/// Whether every guard expression evaluates to `true`; a raising or non-boolean
/// guard makes the clause fail.
///
/// Guard truth is taken from the value *kind*: a comparison/type test/connective
/// carries an internal boolean, so no `AtomTable` lookup is needed for the common
/// case; only a literal `true`/`false` guard term is resolved by name.
pub(super) fn guards_hold(
    guards: &[expr::Expr],
    binds: &[(u32, Eterm)],
    object: Eterm,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> bool {
    for guard in guards {
        match expr::eval(guard, binds, object, atoms, heap) {
            Ok(value) => match expr::value_bool(value, atoms) {
                Ok(true) => {}
                _ => return false,
            },
            Err(_) => return false,
        }
    }
    true
}

/// Whether a body value is the boolean `true` (`select_delete`/`select_count`).
///
/// An internal boolean is checked directly; a term-valued body (e.g. the literal
/// `true`) is resolved by name.
pub(super) fn is_true(value: expr::Value, atoms: &AtomTable) -> bool {
    expr::value_bool(value, atoms).unwrap_or(false)
}

impl Table {
    /// The body value of every object the spec selects (`ets:select/2`) — one
    /// result per matching object, in the table's iteration order.
    ///
    /// Fresh tuples/lists are built on `heap`; a result that is `'$_'`, a variable
    /// or a constant aliases table or spec storage. A body whose value is a boolean
    /// is rendered as the atom `true`/`false`, which must be interned in `atoms`.
    pub fn select(&self, spec: &MatchSpec, atoms: &AtomTable, heap: &mut Heap) -> Result<Vec<Eterm>> {
        let mut selected = Vec::new();
        for object in self.tab2list() {
            if let Some(value) = run_clauses(spec, object, atoms, heap)? {
                selected.push(value.into_term(atoms)?);
            }
        }
        Ok(selected)
    }

    /// Delete every object whose clause value is `true`, returning how many were
    /// deleted (`ets:select_delete/2`). The deleted objects' table-owned storage
    /// is reclaimed.
    pub fn select_delete(
        &mut self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        let mut deleted = 0;
        for object in self.tab2list() {
            if let Some(value) = run_clauses(spec, object, atoms, heap)? {
                if is_true(value, atoms) {
                    self.delete_object(object, atoms)?;
                    deleted += 1;
                }
            }
        }
        Ok(deleted)
    }

    /// Count the objects whose clause value is `true` (`ets:select_count/2`).
    pub fn select_count(
        &self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        let mut count = 0;
        for object in self.tab2list() {
            if let Some(value) = run_clauses(spec, object, atoms, heap)? {
                if is_true(value, atoms) {
                    count += 1;
                }
            }
        }
        Ok(count)
    }

    /// `ets:match_object/2` — every matching object, whole.
    pub fn match_object(
        &self,
        pattern_term: Eterm,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Vec<Eterm>> {
        let spec = spec_for_pattern(pattern_term, atoms, BodyKind::WholeObject)?;
        self.select(&spec, atoms, heap)
    }

    /// `ets:match/2` — for every matching object, the list of the head's bound
    /// `'$N'` variables in ascending variable order.
    pub fn match_(
        &self,
        pattern_term: Eterm,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Vec<Eterm>> {
        let spec = spec_for_pattern(pattern_term, atoms, BodyKind::AllBindings)?;
        self.select(&spec, atoms, heap)
    }

    /// `ets:match_delete/2` — delete every object matching the pattern, exactly as
    /// `ets:select_delete(T, [{Pattern, [], [true]}])`.
    ///
    /// Returns the number of objects deleted, like [`Self::select_delete`]. This
    /// deliberately differs from the BIF, which returns the atom `true`: the count
    /// is the only extra information the operation can report, and callers that
    /// want the BIF's `true` can ignore it (the differential battery renders the
    /// observable ETS result, `true`, separately from this count).
    pub fn match_delete(
        &mut self,
        pattern_term: Eterm,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        let spec = spec_for_pattern(pattern_term, atoms, BodyKind::LiteralTrue)?;
        self.select_delete(&spec, atoms, heap)
    }

    /// `ets:select_replace/2` — replace every object the spec selects with the
    /// value of its clause's body, returning how many objects were replaced.
    ///
    /// # Semantics (verified against the system `erl`, OTP 29 / erts-17.1)
    ///
    /// * Supported on `set`, `ordered_set` and `duplicate_bag`; on `bag` the whole
    ///   call is `badarg`. The oracle refuses the kind up front, **before** the spec
    ///   or the objects are looked at (C `ets_select_replace_2`: `DB_BAG` ⇒
    ///   `badarg`), so a spec that matches nothing is `badarg` on a `bag` too.
    /// * Objects are visited in [`Table::tab2list`] order. The first clause whose
    ///   head matches and whose guards hold supplies the replacement: the value of
    ///   the body's **last** expression (see [`run_clauses`]).
    /// * The replacement must be a tuple of arity `>= keypos` whose `keypos`
    ///   element **equals** the matched object's key ([`keeps_key`]); anything else
    ///   makes the whole call `badarg`.
    /// * A `badarg` leaves the table **unchanged**: every replacement is computed
    ///   and validated before the first mutation, matching the oracle, which
    ///   validates the spec up front (`db_match_keeps_key`) and only then traverses.
    /// * A `'$_'` body (or any body whose value equals the matched object) is the
    ///   identity: the object is still counted as replaced.
    ///
    /// # Divergence from the oracle's *static* body check
    ///
    /// The C validates the body's **shape** statically — `db_match_keeps_key`
    /// (`../../erts/emulator/beam/erl_db_util.c:1422`), installed as the match
    /// spec's `on_match_validation` — so it rejects a body that is not a
    /// one-expression list of `'$_'`, the `{{ … }}` construction form or
    /// `{const, {…}}`, and a head key of `'_'` paired with a constant body tuple,
    /// **even when nothing matches**. This implementation validates the *value* each
    /// body produces instead. The two agree on every case the differential battery
    /// pins; the value-based rule is strictly more permissive in three corners (an
    /// empty/never-matching table with an otherwise-rejected body, a
    /// multi-expression body whose last value keeps the key, and a `'_'` head key
    /// with a constant body tuple). Recorded here rather than silently tolerated.
    ///
    /// # Storage
    ///
    /// `set`/`ordered_set` apply through [`Table::insert`], which deep-copies the
    /// replacement and reclaims the replaced object's storage. `duplicate_bag` is
    /// different: several objects share a key and the replacement must land on the
    /// *specific* matched object, while [`Table::delete_object`] removes **every**
    /// object equal to its argument (and would remove a just-inserted replacement
    /// that equals the object it replaces). Its affected keys are therefore rebuilt
    /// from [`Table::take`]'s checked-out copies — the replacements and the kept
    /// objects are re-inserted, so no replacement can ever read storage that was
    /// reclaimed under it. The checked-out copies stay valid for the table's
    /// lifetime but no longer count as table memory (`take`'s documented
    /// `relinquish` semantics), so repeated replacement on a `duplicate_bag` grows
    /// its arena until `delete_all_objects`/drop.
    pub fn select_replace(
        &mut self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        // The oracle rejects `bag` before looking at the spec or the contents.
        if self.kind() == TableKind::Bag {
            return Err(crate::error::badarg());
        }
        let keypos = self.keypos();
        let multi = self.kind() == TableKind::DuplicateBag;

        // Phase 1 — compute and validate *every* replacement before mutating, so a
        // rejected one leaves the table exactly as it was.
        let mut replacements: Vec<Eterm> = Vec::new();
        let mut keys: Vec<Eterm> = Vec::new();
        for object in self.tab2list() {
            let Some(replacement) = replacement_for(spec, object, keypos, atoms, heap)? else {
                continue;
            };
            if multi {
                let key = key_of(object, keypos)?;
                if !keys.iter().any(|known| eq(*known, key, atoms)) {
                    keys.push(key);
                }
            }
            replacements.push(replacement);
        }
        let count = replacements.len();

        // Phase 2 — apply.
        if multi {
            // Rebuild each affected key from checked-out copies. Re-evaluating the
            // spec is deterministic (head and guards are pure functions of the
            // object), so it reproduces exactly the phase-1 decisions and count; it
            // is what lets the replacement target the matched object rather than
            // every object equal to it.
            for key in keys {
                let taken = self.take(key, atoms)?;
                let mut rebuilt = Vec::with_capacity(taken.len());
                for object in taken {
                    match replacement_for(spec, object, keypos, atoms, heap)? {
                        Some(replacement) => rebuilt.push(replacement),
                        None => rebuilt.push(object),
                    }
                }
                self.insert_list(&rebuilt, atoms)?;
            }
        } else {
            for replacement in replacements {
                self.insert(replacement, atoms)?;
            }
        }
        Ok(count)
    }
}

/// Evaluate `spec` against one object and produce its replacement, or `None` when
/// no clause matches.
///
/// The replacement is the clause body's value (the value of its last expression,
/// `'$_'` for the whole object); it must keep the object's key — see [`keeps_key`].
fn replacement_for(
    spec: &MatchSpec,
    object: Eterm,
    keypos: usize,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> Result<Option<Eterm>> {
    let Some(value) = run_clauses(spec, object, atoms, heap)? else {
        return Ok(None);
    };
    let replacement = value.into_term(atoms)?;
    if !keeps_key(replacement, object, keypos, atoms) {
        return Err(crate::error::badarg());
    }
    Ok(Some(replacement))
}

/// Whether a replacement object keeps the key of the object it replaces: it must
/// be a tuple of arity `>= keypos` whose `keypos` element equals the matched
/// object's key (`beam_term::compare::eq`, never a local key comparison).
///
/// This is the observable rule of `ets:select_replace/2`; the C reaches it
/// statically through `db_match_keeps_key`
/// (`../../erts/emulator/beam/erl_db_util.c:1422`) and reports `badarg` when it
/// cannot prove the key is kept. A non-tuple or too-short replacement cannot keep
/// a key, so it is a failure here too (`key_of` → `badarg`).
fn keeps_key(replacement: Eterm, matched: Eterm, keypos: usize, atoms: &AtomTable) -> bool {
    let (Ok(new_key), Ok(old_key)) = (key_of(replacement, keypos), key_of(matched, keypos)) else {
        return false;
    };
    eq(new_key, old_key, atoms)
}
