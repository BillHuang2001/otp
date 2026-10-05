//! The [`Table`] selection methods: `select`, `select_delete`, `select_count` and
//! the `match` family, all built on the compiler in [`super::compile`] and the
//! executor here.
//!
//! `ets:match(T, P)` is `ets:select(T, [{P, [], ['$$']}])` and
//! `ets:match_object(T, P)` is `ets:select(T, [{P, [], ['$_']}])` — verified
//! against OTP 29 (`erl_db.c` `ets_match_2`/`ets_match_object_2` build exactly
//! those specs), so both are implemented by compiling the pattern and running the
//! corresponding body.

use std::collections::BTreeSet;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use super::{atom_name, expr, pattern, MatchClause, MatchSpec};
use crate::table::Table;

/// Which body a pattern-only spec (`match_object`/`match`) uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// `'$_'` — the whole matched object (`ets:match_object/2`).
    WholeObject,
    /// `'$$'` — every bound head variable, ascending (`ets:match/2`).
    AllBindings,
}

/// Build a one-clause spec from a bare pattern term (the `match_object`/`match`
/// form).
fn spec_for_pattern(pattern_term: Eterm, atoms: &AtomTable, kind: BodyKind) -> Result<MatchSpec> {
    let mut vars = BTreeSet::new();
    let head = pattern::decode(pattern_term, atoms, &mut vars)?;
    let body = match kind {
        BodyKind::WholeObject => expr::Expr::WholeObject,
        BodyKind::AllBindings => expr::Expr::AllBindings(vars.iter().copied().collect()),
    };
    Ok(MatchSpec {
        clauses: vec![MatchClause {
            head,
            guards: Vec::new(),
            body: vec![body],
        }],
    })
}

/// Run `spec` against one object: the body results of the first clause whose head
/// matches and whose guards all hold, or `None` when no clause matches.
///
/// A guard that raises makes its clause fail (mirroring the C `matchCatch`); a
/// body that raises propagates `badarg`.
fn run_clauses(
    spec: &MatchSpec,
    object: Eterm,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> Result<Option<Vec<Eterm>>> {
    for clause in &spec.clauses {
        let mut binds: Vec<(u32, Eterm)> = Vec::new();
        if !pattern::matches(&clause.head, object, atoms, &mut binds) {
            continue;
        }
        if !guards_hold(&clause.guards, &binds, object, atoms, heap) {
            continue;
        }
        let mut results = Vec::with_capacity(clause.body.len());
        for expression in &clause.body {
            results.push(expr::eval(expression, &binds, object, atoms, heap)?);
        }
        return Ok(Some(results));
    }
    Ok(None)
}

/// Whether every guard expression evaluates to exactly `true`; a raising or
/// non-boolean guard makes the clause fail.
fn guards_hold(
    guards: &[expr::Expr],
    binds: &[(u32, Eterm)],
    object: Eterm,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> bool {
    for guard in guards {
        match expr::eval(guard, binds, object, atoms, heap) {
            Ok(value) if atom_name(value, atoms) == Some("true") => {}
            _ => return false,
        }
    }
    true
}

/// Whether a body result is exactly the atom `true` (`select_delete`/`select_count`).
fn is_true(term: Eterm, atoms: &AtomTable) -> bool {
    atom_name(term, atoms) == Some("true")
}

impl Table {
    /// The body results of every object the spec selects (`ets:select/2`).
    ///
    /// One result per body element per matching object, in the table's iteration
    /// order. Fresh tuples/lists are built on `heap`; a result that is `'$_'`, a
    /// variable or a constant aliases table or spec storage.
    pub fn select(&self, spec: &MatchSpec, atoms: &AtomTable, heap: &mut Heap) -> Result<Vec<Eterm>> {
        let mut selected = Vec::new();
        for object in self.tab2list() {
            if let Some(results) = run_clauses(spec, object, atoms, heap)? {
                selected.extend(results);
            }
        }
        Ok(selected)
    }

    /// Delete every object whose clause body evaluates to `[true]`, returning how
    /// many were deleted (`ets:select_delete/2`). The deleted objects' table-owned
    /// storage is reclaimed.
    pub fn select_delete(
        &mut self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        let mut deleted = 0;
        for object in self.tab2list() {
            if let Some(results) = run_clauses(spec, object, atoms, heap)? {
                if results.len() == 1 && is_true(results[0], atoms) {
                    self.delete_object(object, atoms)?;
                    deleted += 1;
                }
            }
        }
        Ok(deleted)
    }

    /// Count the objects whose clause body evaluates to `[true]`
    /// (`ets:select_count/2`).
    pub fn select_count(
        &self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        let mut count = 0;
        for object in self.tab2list() {
            if let Some(results) = run_clauses(spec, object, atoms, heap)? {
                if results.len() == 1 && is_true(results[0], atoms) {
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

    /// `ets:match_delete/2` — delete every matching object, returning how many
    /// were deleted. The deleted objects' storage is reclaimed.
    pub fn match_delete(
        &mut self,
        pattern_term: Eterm,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        let spec = spec_for_pattern(pattern_term, atoms, BodyKind::WholeObject)?;
        self.select_delete(&spec, atoms, heap)
    }
}
