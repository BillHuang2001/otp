//! Selection: the `select` primitive and the M1 match-spec subset.
//!
//! [`SelectPredicate`] is the code-level primitive [`crate::Table::select_all`]
//! consumes: it receives an object's tuple **element slice** and returns whether
//! to keep the object. [`MatchSpec`] is the *data* form (`ets:select/2` match
//! specs) that [`compile`] decodes into its M1 subset — head = tuple of literals
//! / `'_'` (wildcard) / `'$N'` (bind element `N`), **empty guards only**, body =
//! tuple of literals / `'$N'` / `'$_'` (whole object). Anything outside the subset
//! is rejected with `badarg` (see the crate `CONTEXT.md`, Known Issues).

use beam_common::Result;
use beam_term::prelude::*;

/// The code-level select primitive: receives an object's tuple element slice and
/// returns whether the object is kept.
pub type SelectPredicate<'a> = dyn FnMut(&[Eterm]) -> bool + 'a;

/// One element of a match-clause head pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchElement {
    /// `'_'` — matches any element.
    Wildcard,
    /// A literal element that must compare equal.
    Literal(Eterm),
    /// `'$N'` — matches any element and binds it to variable `N`.
    Bind(u32),
}

/// The head pattern of a [`MatchClause`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchPattern {
    /// A bare `'_'` head: matches any object.
    Wildcard,
    /// A tuple head: matches a tuple whose elements match the per-element
    /// patterns.
    Tuple(Vec<MatchElement>),
}

/// The guard of a [`MatchClause`].
///
/// M1 supports only the empty guard; guards (`andalso`/`orelse`, `is_*` tests,
/// `element/2`, …) are not part of the subset and cause [`compile`] to reject
/// the spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchGuard {
    /// No guard — the clause matches whenever its head matches.
    Empty,
}

/// One element of a match-clause body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchBodyElement {
    /// A literal element.
    Literal(Eterm),
    /// `'$N'` — the value bound to variable `N` by the head.
    Bind(u32),
    /// `'$_'` — the whole matched object.
    WholeObject,
}

/// The body (result template) of a [`MatchClause`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchBody {
    /// A bare `'$_'` body: the whole matched object is the result.
    WholeObject,
    /// A tuple body: the result is built from the given elements.
    Tuple(Vec<MatchBodyElement>),
}

/// One match-spec clause: a head pattern, a guard and a body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchClause {
    /// The head pattern the object is matched against.
    pub head: MatchPattern,
    /// The guard (M1: empty only).
    pub guard: MatchGuard,
    /// The body producing the result for a matched object.
    pub body: MatchBody,
}

/// A compiled match spec: an ordered list of clauses; the first matching clause
/// wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchSpec {
    /// The clauses, in order.
    pub clauses: Vec<MatchClause>,
}

/// Decode the M1 match-spec subset from its term form into a [`MatchSpec`].
///
/// Fails with `badarg` for any construct outside the subset (non-empty guards,
/// unsupported patterns/bodies, malformed structure).
pub fn compile(spec: Eterm, atoms: &AtomTable) -> Result<MatchSpec> {
    unimplemented!("M1: implementation pending")
}
