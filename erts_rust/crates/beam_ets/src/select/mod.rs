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

//! Selection: the code-level primitive and the **full** match-spec
//! compiler/executor for `ets:select/2`, `ets:select_delete/2`,
//! `ets:select_count/2` and the `match` family.
//!
//! [`SelectPredicate`] is the code-level primitive [`crate::Table::select_all`]
//! consumes: it receives an object's tuple **element slice** and returns whether
//! to keep the object. [`MatchSpec`] is the *data* form of an Erlang match spec;
//! [`compile`] decodes it and [`exec`] runs it.
//!
//! # What is supported (mirrors OTP 29 / erts-17.1 `erl_db_util.c`)
//!
//! * **Head patterns** ([`Pattern`]): arbitrary terms — `'_'` (wildcard),
//!   literals (compared with exact `=:=` — `beam_term::compare::eq`), nested
//!   tuples and (proper or improper) lists, and `'$N'` variables. A repeated
//!   `'$N'` requires the two elements to be equal (the C emits `matchCmp` for an
//!   already-bound variable). `'$_'`/`'$$'` in a head are **ordinary literal
//!   atoms** compared with exact `=:=` — they are special only in a *body* (the C
//!   `dmc_one_term` emits `matchEq` for any atom that is not `'_'`/`'$N'`,
//!   `erl_db_util.c:4117-4121`). `'$0'` **is** a valid variable
//!   (the C `db_is_variable` accepts `$0`); a malformed `$`-atom such as `'$'`,
//!   `'$x'`, `'$1x'` or `'$01'` is an ordinary **literal atom** (not an error),
//!   matching `db_is_variable` returning `-1`.
//! * **Guards**: a flat list of expressions, **all** of which must evaluate to
//!   `true`. Guard truth is an internal boolean produced by the comparisons, type
//!   tests and connectives, so it never depends on the caller's `AtomTable`
//!   having interned `true`/`false`; only a literal `true`/`false` guard *term* is
//!   resolved by name. `andalso`/`orelse` short-circuit, `and`/`or` are strict
//!   (verified against `erl`). A nested list element builds a list value (which
//!   is never `true`), so a list-of-lists guard never matches — this is exactly
//!   what the C `compile_guard_expr` emits. Supported guard constructs: `andalso`,
//!   `orelse`, `and`, `or`, `not`, `const/1`, the comparison operators
//!   (`== /= < =< > >= =:= =/=`), the type tests (`is_atom is_integer is_float
//!   is_number is_tuple is_list is_binary is_bitstring is_map is_pid is_port
//!   is_reference is_function is_boolean`) and the guard BIFs `element/2`,
//!   `abs/1`, `size/1`, `hd/1`, `tl/1`, `length/1`, `self/0`. Any other call is
//!   rejected by [`compile`] with `badarg` (an honest subset — never silently
//!   ignored). Comparison uses `beam_term::compare` (`cmp` for the exact
//!   operators, `cmp_arith` for `==`/`/=`/`<`/`=<`/`>`/`>=`), never a local
//!   re-implementation of term order.
//! * **Bodies**: a non-empty list of expressions evaluated in order; the clause's
//!   value is the value of the **last** expression, so a clause yields exactly one
//!   result per matched object (the C `compile_guard_expr` discards every
//!   intermediate value). A body element may be `'$_'` (whole object), `'$$'`
//!   (every bound head variable, ascending), `'$N'`, any constant literal, a list
//!   template, or a tuple template written `{{ … }}` (the C's tuple-construct
//!   form). Fresh tuples/lists are built on the caller's [`beam_alloc::Heap`]; a
//!   body element that is `'$_'`/a variable/a constant yields a term that
//!   **aliases** the table or the caller's spec storage (it is not copied).
//!
//! # Verified against the system `erl`
//!
//! Every semantic above (head `'$_'`/`'$$'` literals, `'$0'`/`'$x'`, repeated
//! `'$N'`, the flat
//! all-must-be-`true` guard list, guard runtime errors skipping the clause while
//! body runtime errors are `badarg`, the `{{ … }}` construct form, the
//! last-expression body value, `andalso`/`orelse` short-circuiting, and the
//! `match`/`match_object`/`match_delete` relationship) was probed against OTP 29
//! (`ets:match(T,P)` ≡ `ets:select(T,[{P,[],['$$']}])`, `ets:match_object(T,P)` ≡
//! `ets:select(T,[{P,[],['$_']}])`, `ets:match_delete(T,P)` ≡
//! `ets:select_delete(T,[{P,[],[true]}])`). See `tests/select.rs`.
//!
//! # Known gaps
//!
//! * **Map patterns in a head** are compared as literals (a map containing a
//!   wildcard/variable therefore under-matches). Maps as expressions are
//!   rejected with `badarg`.
//! * `self/0` has no process identity at the ETS layer and raises, so a guard
//!   using it never matches (the clause is skipped).
//! * `abs/1` accepts small integers only (bignums/floats raise).
//! * A **body** whose value is a boolean (e.g. `[{'is_integer','$1'}]`) is rendered
//!   as the atom `true`/`false`, so the caller's `AtomTable` must have interned
//!   them; guard *truth* is unaffected, since it is computed internally.
//!
//! # Why this module reads term words
//!
//! Decoding a spec term means reading its tuples and lists. The frozen
//! representation (`beam_term`) exposes exactly those accessors (`is_tuple`,
//! `tuple_size`, `tuple_element`, `car`, `cdr`) as `unsafe fn`, because "this word
//! is a live term of the right shape" is a caller invariant the word itself cannot
//! re-check. Reading the spec term therefore needs `unsafe`; every block below
//! carries a `// SAFETY:` rationale and the reads never go beyond the
//! caller-owned spec term (or a table-owned object, which is a validated tuple).

use std::collections::BTreeSet;

use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;

mod continuation;
mod exec;
pub mod expr;
mod pattern;
mod run;

pub use continuation::MatchContinuation;
pub use exec::BodyKind;
pub use expr::{
    set_self_hook, ArithHook, ArithOp, CmpOp, DefaultArithHook, Expr, SelfHook, TypeTest,
};
pub use pattern::Pattern;
pub use run::match_spec_run;

/// The code-level select primitive: receives an object's tuple element slice and
/// returns whether the object is kept.
pub type SelectPredicate<'a> = dyn FnMut(&[Eterm]) -> bool + 'a;

/// One match-spec clause: a head pattern, a list of guard expressions (all must
/// be `true`) and a list of body expressions (the results).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchClause {
    /// The head pattern the object is matched against.
    pub head: Pattern,
    /// The guard expressions, ANDed; empty means "no guard".
    pub guards: Vec<Expr>,
    /// The body expressions; each produces one result.
    pub body: Vec<Expr>,
}

/// A compiled match spec: an ordered list of clauses; the first clause whose head
/// matches and whose guards all hold produces the result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MatchSpec {
    /// The clauses, in order.
    pub clauses: Vec<MatchClause>,
}

/// Legacy alias kept for source compatibility: the head pattern AST.
pub type MatchPattern = Pattern;
/// Legacy alias kept for source compatibility: a head pattern element.
pub type MatchElement = Pattern;
/// Legacy alias kept for source compatibility: a guard/body expression.
pub type MatchGuard = Expr;
/// Legacy alias kept for source compatibility: a body expression.
pub type MatchBody = Expr;
/// Legacy alias kept for source compatibility: a body element.
pub type MatchBodyElement = Expr;

// ---------------------------------------------------------------------------
// Term decoding helpers
//
// These read words of the caller-owned spec term through `beam_term`'s `unsafe`
// accessors (see the module docs).
// ---------------------------------------------------------------------------

/// The name of an atom term, or `None` when `term` is not an atom (or the passed
/// table does not know it). Non-erroring.
pub(super) fn atom_name<'a>(term: Eterm, atoms: &'a AtomTable) -> Option<&'a str> {
    if is_atom(term) {
        atoms.name(atom_val(term))
    } else {
        None
    }
}

/// Decode a match-spec variable atom, reproducing the C `db_is_variable`: `'$0'`
/// and `'$N'` (no leading zero, all digits) name variable `N`; anything else is
/// not a variable (`None`).
pub(super) fn var_index(name: &str) -> Option<u32> {
    let rest = name.strip_prefix('$')?;
    let bytes = rest.as_bytes();
    let first = *bytes.first()?;
    if first == b'0' {
        // `$0` alone is variable 0; `$00`, `$01` are not variables.
        return (rest.len() == 1).then_some(0);
    }
    if !(b'1'..=b'9').contains(&first) || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    rest.parse::<u32>().ok()
}

/// The elements of a tuple term, or `None` when `term` is not a tuple.
pub(super) fn tuple_elements(term: Eterm) -> Option<Vec<Eterm>> {
    // SAFETY: `is_tuple` inspects only the header word of a boxed term and checks
    // the boxed/arityval shape first, so it is sound for any value word (it never
    // dereferences a non-boxed, e.g. immediate, term).
    if !unsafe { is_tuple(term) } {
        return None;
    }
    // SAFETY: `is_tuple` established that `term` is a live tuple whose header word
    // is a valid arityval, so its arity is readable.
    let arity = unsafe { tuple_size(term) };
    // SAFETY: every `i` in `0..arity` is `< tuple_size(term)`, which is exactly
    // `tuple_element`'s precondition; the tuple lives in the caller-owned spec.
    Some((0..arity).map(|i| unsafe { tuple_element(term, i) }).collect())
}

/// The elements of a proper list term, or `None` when `term` is not a proper list
/// (a non-cons value, or an improper list).
pub(super) fn list_elements(term: Eterm) -> Option<Vec<Eterm>> {
    let mut elements = Vec::new();
    let mut cursor = term;
    while is_list(cursor) {
        // SAFETY: `is_list(cursor)` established that `cursor` is a live cons cell,
        // so its two `[CAR, CDR]` words are readable. The spec term is caller-owned
        // and stays live for the whole call, so `cursor` never dangles.
        unsafe {
            elements.push(car(cursor));
            cursor = cdr(cursor);
        }
    }
    // A proper list terminates in `NIL`; anything else is an improper list.
    is_nil(cursor).then_some(elements)
}

/// Decode the full match spec (`ets:select/2`'s term form) into a [`MatchSpec`].
///
/// The term is an Erlang list of `{Head, Guards, Body}` clauses. Fails with
/// `badarg` for any construct outside the supported subset (a non-list spec, a
/// clause that is not a 3-tuple, a malformed guard/body shape, an empty body list,
/// an unknown guard call, or an unbound `'$N'` in a guard/body).
pub fn compile(spec: Eterm, atoms: &AtomTable) -> Result<MatchSpec> {
    let clauses = list_elements(spec).ok_or_else(badarg)?;
    let mut decoded = Vec::with_capacity(clauses.len());
    for clause in clauses {
        decoded.push(decode_clause(clause, atoms)?);
    }
    Ok(MatchSpec { clauses: decoded })
}

/// Decode one `{Head, Guards, Body}` clause.
fn decode_clause(clause: Eterm, atoms: &AtomTable) -> Result<MatchClause> {
    let elements = tuple_elements(clause).ok_or_else(badarg)?;
    if elements.len() != 3 {
        return Err(badarg());
    }
    // The head is decoded first: it establishes which `$N` are bound, which the
    // guards and the body are validated against.
    let mut vars = BTreeSet::new();
    let head = pattern::decode(elements[0], atoms, &mut vars)?;
    let guard_terms = list_elements(elements[1]).ok_or_else(badarg)?;
    let mut guards = Vec::with_capacity(guard_terms.len());
    for guard in guard_terms {
        guards.push(expr::decode(guard, atoms, &vars)?);
    }
    let body_terms = list_elements(elements[2]).ok_or_else(badarg)?;
    if body_terms.is_empty() {
        return Err(badarg());
    }
    let mut body = Vec::with_capacity(body_terms.len());
    for term in body_terms {
        body.push(expr::decode(term, atoms, &vars)?);
    }
    Ok(MatchClause { head, guards, body })
}
