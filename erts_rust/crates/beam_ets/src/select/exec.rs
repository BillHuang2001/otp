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
use beam_common::Result;
use beam_term::prelude::*;

use super::{expr, pattern, MatchClause, MatchSpec};
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
fn spec_for_pattern(pattern_term: Eterm, atoms: &AtomTable, kind: BodyKind) -> Result<MatchSpec> {
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
/// A guard that raises makes its clause fail (mirroring the C `matchCatch`); a
/// body that raises propagates `badarg`.
fn run_clauses(
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
        let mut value = None;
        for expression in &clause.body {
            value = Some(expr::eval(expression, &binds, object, atoms, heap)?);
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

/// Whether every guard expression evaluates to `true`; a raising or non-boolean
/// guard makes the clause fail.
///
/// Guard truth is taken from the value *kind*: a comparison/type test/connective
/// carries an internal boolean, so no `AtomTable` lookup is needed for the common
/// case; only a literal `true`/`false` guard term is resolved by name.
fn guards_hold(
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
fn is_true(value: expr::Value, atoms: &AtomTable) -> bool {
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
}
