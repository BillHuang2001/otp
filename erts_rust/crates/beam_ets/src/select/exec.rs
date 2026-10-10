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
/// A guard the oracle itself raises (`badarg`) makes its clause fail (mirroring the
/// C `matchCatch`) — see [`guards_hold`] for the exact classification — while a guard
/// this runtime **cannot evaluate** is a typed error propagated to the caller, never a
/// silent non-match. A **body** expression that raises at runtime is replaced by the
/// atom `EXIT` and evaluation continues (the C `FAIL_TERM`) — see the body comment
/// below.
///
/// # Host requirement: `EXIT` must be interned
///
/// The `FAIL_TERM` substitution resolves `EXIT` from the caller's [`AtomTable`]
/// ([`exit_term`]); the C's `FAIL_TERM` is a **static atom that always exists**, so
/// a host must **pre-intern `EXIT`** before running a spec whose body can raise.
/// Every real runtime does (`beam_rt::boot::CORE_ATOMS` seeds it, among others). If
/// it is not interned a raising body yields the typed `badarg` capability gap
/// instead of the oracle's `EXIT` term — never a silently substituted atom.
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
        if !guards_hold(&clause.guards, &binds, object, atoms, heap)? {
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
///
/// # Host requirement
///
/// A host that runs any spec whose body can raise must pre-intern `EXIT` in the
/// [`AtomTable`] it passes (see [`run_clauses`]); real runtimes do
/// (`beam_rt::boot::CORE_ATOMS`).
fn exit_term(atoms: &AtomTable) -> Result<Eterm> {
    atoms
        .lookup("EXIT")
        .map(make_atom)
        .ok_or_else(crate::error::badarg)
}

/// Whether every guard expression holds. A guard the **oracle itself raises** makes
/// its clause fail (`Ok(false)`); a guard this runtime cannot evaluate is a typed
/// error and is propagated, never turned into a silent non-match.
///
/// # The guard-error classification (mirrors the body path in [`run_clauses`])
///
/// The clause fails (`Ok(false)`) for exactly the three ways `erl` treats a guard as
/// unmet, and **only** those:
///
/// 1. a guard that **raises in the oracle** — every oracle-raising guard construct is
///    the C `badarg` (`element/2` out of range, `hd`/`tl`/`length` of a non-list,
///    `size/1` of a non-tuple/binary, `map_get/2` with an absent key, `div`/`rem` by
///    zero, a non-number arithmetic operand, …) and reaches the executor as
///    [`BeamError::BadArg`] — so `Err(BeamError::BadArg) => Ok(false)`, the C
///    `matchCatch`;
/// 2. a guard whose value is **not a boolean** (`Value::Bool`/`true`/`false` is
///    required) — `erl` treats it as unmet, not as an error;
/// 3. a guard that evaluates to `false` (or, for `and`/`or`, fails the conjunction).
///
/// Any **other** error is a capability gap of this runtime (e.g. `unsupported_ets`
/// from `ArithHook` arithmetic outside small integers), and is returned with `Err` so
/// the caller of `select`/`match_spec_run` sees a typed failure instead of a silently
/// under-matching spec (workspace `CONTEXT.md` §3 — no silent swallowing). The body
/// path below applies the same rule: `BadArg` → the C `FAIL_TERM`, everything else
/// propagated.
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
) -> Result<bool> {
    for guard in guards {
        match expr::eval(guard, binds, object, atoms, heap) {
            // The oracle's own raise: the guard is unmet, the clause is skipped.
            Err(BeamError::BadArg) => return Ok(false),
            // A capability gap: never masked as an unmet guard.
            Err(other) => return Err(other),
            // A non-boolean guard value is unmet, not an error.
            Ok(value) => match expr::value_bool(value, atoms) {
                Ok(true) => {}
                _ => return Ok(false),
            },
        }
    }
    Ok(true)
}

/// Whether a body value is the boolean `true` (`select_delete`/`select_count`).
///
/// An internal boolean is checked directly; a term-valued body (e.g. the literal
/// `true`) is resolved by name.
pub(super) fn is_true(value: expr::Value, atoms: &AtomTable) -> bool {
    expr::value_bool(value, atoms).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// The `ets:select_replace/2` static body check — the C `db_match_keeps_key`
// (`../../erts/emulator/beam/erl_db_util.c:1422`), run on the *shape* of an
// already-decoded spec.
//
// The C validates each clause of the **uncompiled** spec before it traverses
// anything (`analyze_pattern` calls `on_match_validation` per clause), so a body
// that it cannot prove keeps the key makes the whole call `badarg` even when
// nothing matches. `beam_bifs` compiles the spec before this crate sees it, so
// the check runs here over the compiled AST: `expr::decode` rejects every body
// shape the C rejects for *structural* reasons (a call, a non-proper body list,
// an unbound `'$N'`), and the AST still distinguishes the two forms the C walks
// — `{X}` (`Expr::Tuple`, whose parts are the constructed tuple's elements) and
// `{const, X}` (`Expr::Literal(X)`, holding the raw `X`).
// ---------------------------------------------------------------------------

/// Which side of the `db_match_eq_body` walk a node is.
enum BodySide<'a> {
    /// A body node of the decoded AST.
    Expr(&'a expr::Expr),
    /// A **raw** body subterm (the `{const, X}` form keeps `X` undecoded).
    Raw(Eterm),
}

/// Whether **every** clause's body provably keeps the object's key — the C
/// `db_match_keeps_key` applied to each clause (a single failure rejects the
/// whole call, exactly as `analyze_pattern` does before traversing).
fn static_keeps_key(spec: &MatchSpec, keypos: usize, atoms: &AtomTable) -> bool {
    spec.clauses
        .iter()
        .all(|clause| clause_keeps_key(&clause.head, &clause.body, keypos, atoms))
}

/// One clause of [`static_keeps_key`].
fn clause_keeps_key(
    head: &pattern::Pattern,
    body: &[Expr],
    keypos: usize,
    atoms: &AtomTable,
) -> bool {
    // 1. `body` must be a proper list of exactly one element.
    let [element] = body else {
        return false;
    };
    // 2. The whole-object body returns the same tuple.
    if matches!(element, expr::Expr::WholeObject) {
        return true;
    }
    // 4. The match head's key element must be extractable: the head must be a
    //    tuple of arity `>= keypos` (the C `db_getkey`).
    let pattern::Pattern::Tuple(head_elems) = head else {
        return false;
    };
    if head_elems.len() < keypos {
        return false;
    }
    let head_key = &head_elems[keypos - 1];

    match element {
        // 3. `{const, X}` — const mode, with the raw `X` as the subterm.
        expr::Expr::Literal(subterm) if super::tuple_elements(*subterm).is_some() => {
            let elems = super::tuple_elements(*subterm).expect("checked just above");
            // 5. The subterm's key must be extractable.
            if elems.len() < keypos {
                return false;
            }
            let sub_key = elems[keypos - 1];
            // 6. Structural equality with the head's key; 7. const mode rejects
            //    any mismatch (no `{element, KeyPos, '$_'}` rescue).
            eq_body_raw(head_key, sub_key, true, atoms)
        }
        // 3. `{X}` — the construction form; `parts` are `X`'s elements.
        expr::Expr::Tuple(parts) => {
            // 5.
            if parts.len() < keypos {
                return false;
            }
            let sub_key = &parts[keypos - 1];
            if eq_body(head_key, BodySide::Expr(sub_key), false, atoms) {
                return true;
            }
            // 8. `{element, KeyPos, '$_'}` is accepted unconditionally.
            is_element_form(sub_key, keypos)
        }
        // 3. Anything else (a bare variable, `'$_'`, a call, `'$$'`, …) is not a
        //    tuple construction.
        _ => false,
    }
}

/// `db_match_eq_body(a, b, const_mode)`: whether the head's key element `a`
/// provably names the same term as the body's key element `b`.
fn eq_body(
    head: &pattern::Pattern,
    body: BodySide<'_>,
    const_mode: bool,
    atoms: &AtomTable,
) -> bool {
    match body {
        BodySide::Expr(e) => eq_body_expr(head, e, const_mode, atoms),
        BodySide::Raw(t) => eq_body_raw(head, t, const_mode, atoms),
    }
}

/// `db_match_eq_body` for a **raw** body term (the `{const, X}` subterm tree).
fn eq_body_raw(a: &pattern::Pattern, b: Eterm, const_mode: bool, atoms: &AtomTable) -> bool {
    if is_list(b) {
        let pattern::Pattern::Cons(hcar, hcdr) = a else {
            return false;
        };
        // SAFETY: `is_list(b)` established a live cons cell in the caller-owned
        // spec term, so both `[CAR, CDR]` words are readable and `b` stays live
        // for the whole call.
        let (bcar, bcdr) = unsafe { (car(b), cdr(b)) };
        return eq_body_raw(hcar, bcar, const_mode, atoms)
            && eq_body_raw(hcdr, bcdr, const_mode, atoms);
    }
    if let Some(elems) = super::tuple_elements(b) {
        if !const_mode {
            // The `{{ … }}` double-tuple syntax unwraps to its inner tuple…
            if elems.len() == 1 && super::tuple_elements(elems[0]).is_some() {
                return eq_body_raw(a, elems[0], const_mode, atoms);
            }
            // …and `{const, X}` descends into `X` in const mode.
            if elems.len() == 2 && super::atom_name(elems[0], atoms) == Some("const") {
                return eq_body_raw(a, elems[1], true, atoms);
            }
            // A function call or any other invalid tuple syntax.
            return false;
        }
        let pattern::Pattern::Tuple(helems) = a else {
            return false;
        };
        if helems.len() != elems.len() {
            return false;
        }
        return helems
            .iter()
            .zip(elems)
            .all(|(h, t)| eq_body_raw(h, t, const_mode, atoms));
    }
    if expr::classify(b) == TermClass::Map {
        return false;
    }
    leaf_eq(a, b, const_mode, atoms)
}

/// `db_match_eq_body` for a decoded body node.
fn eq_body_expr(
    a: &pattern::Pattern,
    e: &expr::Expr,
    const_mode: bool,
    atoms: &AtomTable,
) -> bool {
    match e {
        // The raw body term of `{const, X}` (with `X` a tuple) is `{const, X}`,
        // which the C unwraps in const mode.
        expr::Expr::Literal(t) if super::tuple_elements(*t).is_some() => {
            eq_body_raw(a, *t, true, atoms)
        }
        expr::Expr::Literal(t) => leaf_eq(a, *t, const_mode, atoms),
        expr::Expr::Bind(n) => {
            !const_mode && matches!(a, pattern::Pattern::Bind(m) if m == n)
        }
        // `{X}` — already unwrapped by `decode_tuple`.
        expr::Expr::Tuple(parts) => {
            let pattern::Pattern::Tuple(helems) = a else {
                return false;
            };
            if helems.len() != parts.len() {
                return false;
            }
            helems
                .iter()
                .zip(parts)
                .all(|(h, p)| eq_body_expr(h, p, const_mode, atoms))
        }
        expr::Expr::Cons(h, t) => {
            let pattern::Pattern::Cons(ha, hb) = a else {
                return false;
            };
            eq_body_expr(ha, h, const_mode, atoms) && eq_body_expr(hb, t, const_mode, atoms)
        }
        // `'$_'`, `'$$'`, a map or a call never keeps a syntactically-known key.
        _ => false,
    }
}

/// The leaf case of `db_match_eq_body`: an immediate must be the *same* term and
/// not one of the special atoms (nor, in const mode, a match-spec variable);
/// any other boxed term compares by term equality.
fn leaf_eq(a: &pattern::Pattern, b: Eterm, const_mode: bool, atoms: &AtomTable) -> bool {
    match a {
        pattern::Pattern::Literal(lit) => {
            if !eq(*lit, b, atoms) {
                return false;
            }
            if let Some(name) = super::atom_name(*lit, atoms) {
                if name == "_" || name == "$$" || name == "$_" {
                    return false;
                }
                if const_mode && super::var_index(name).is_some() {
                    return false;
                }
            }
            true
        }
        pattern::Pattern::Bind(n) => {
            !const_mode && super::atom_name(b, atoms).and_then(super::var_index) == Some(*n)
        }
        // A wildcard, a tuple, a list or a map never equals a leaf.
        _ => false,
    }
}

/// Whether a body key is the `{element, KeyPos, '$_'}` form (the C's last
/// acceptance rule). It decodes to `element(KeyPos, '$_')`.
fn is_element_form(sub_key: &expr::Expr, keypos: usize) -> bool {
    let expr::Expr::Element(position, target) = sub_key else {
        return false;
    };
    if !matches!(&**target, expr::Expr::WholeObject) {
        return false;
    }
    let expr::Expr::Literal(inner) = &**position else {
        return false;
    };
    is_small(*inner) && inner.raw() == make_small(keypos as i64).raw()
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

    /// The body value of every object the spec selects, visiting the table in the
    /// **reverse** of [`Self::select`]'s iteration order (`ets:select_reverse/2`).
    ///
    /// The traversal order is exactly the reverse of [`Table::tab2list`]: for an
    /// `ordered_set` that is **descending numeric key order** — verified against the
    /// system `erl` (OTP 29 / erts-17.1), whose `ets:select_reverse(T, [{'$1',[],
    /// ['$1']}])` on the `ordered_set` `{1,1}..{5,5}` returns
    /// `[{5,5},{4,4},{3,3},{2,2},{1,1}]`. For the hash kinds (`set`/`bag`/
    /// `duplicate_bag`) the forward order is deliberately unspecified, so the
    /// reverse is a complete permutation of the same result multiset (the oracle
    /// walks its internal index backwards; this crate walks the reverse of its own
    /// forward order — the same "hash-kind order is unspecified" freedom documented
    /// elsewhere).
    ///
    /// Otherwise identical to [`Self::select`]: fresh tuples/lists are built on
    /// `heap`; a result that is `'$_'`, a variable or a constant aliases table or
    /// spec storage; a body whose value is a boolean is rendered as the atom
    /// `true`/`false`, which must be interned in `atoms`.
    pub fn select_reverse(
        &self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Vec<Eterm>> {
        let mut selected = Vec::new();
        for object in self.tab2list().into_iter().rev() {
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

        // The oracle validates the spec's body *shape* before it traverses
        // anything (`analyze_pattern` → `db_match_keeps_key`), so an invalid body
        // is `badarg` even when nothing matches and the table is untouched.
        if !static_keeps_key(spec, keypos, atoms) {
            return Err(crate::error::badarg());
        }
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
