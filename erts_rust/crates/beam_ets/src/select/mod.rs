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
//!   tuples, (proper or improper) lists and maps, and `'$N'` variables. A repeated
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
//!   `abs/1`, `ceil/1`, `floor/1`, `max/2`, `min/2`, `node/0`, `node/1`,
//!   `round/1`, `trunc/1`, `float/1`, `size/1`, `hd/1`, `tl/1`, `length/1`,
//!   `map_size/1`, `map_get/2`, `is_map_key/2`, `tuple_size/1`, `byte_size/1`,
//!   `bit_size/1`, `binary_part/2,3`, `is_record/3`, `is_function/2`,
//!   `is_integer/3`, `xor/2`, `bnot/1`, `self/0`. Any other call is
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
//! * **Map patterns in a head** keep their **keys literal** and match their
//!   **values** structurally, exactly like tuple elements: `#{x => '_'}` matches
//!   any value for key `x` and `#{x => '$2'}` binds it. A **non-literal** key (a
//!   `'$N'` variable or `'_'`) makes the whole spec `badarg` at compile time, at
//!   every nesting depth (the C "Variable found in map key."/"Underscore found in
//!   map key."). A map literal in a *guard*/*body* is **constructed** (see the
//!   guards/bodies bullets above).
//! * `self/0` has no process identity at the ETS layer and raises, so a guard
//!   using it never matches (the clause is skipped).
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
//! carries a `// SAFETY:` rationale.
//!
//! **Every list read goes through the [`cons_cell`](crate::shape::cons_cell) shape
//! predicate** (primary tag `TAG_PRIMARY_LIST` **and** a non-null cell pointer),
//! never through `beam_term::list::is_list` alone: `is_list` mirrors the C bit trick
//! `(x & (_TAG_PRIMARY_MASK - TAG_PRIMARY_LIST)) != 0`, i.e. `!(x & 0x2)`, which is
//! **also** true for every `TAG_PRIMARY_HEADER` word — a word that is not a term at
//! all — so a stale/foreign caller-heap word could pass it and reach a `car`
//! dereference (a measured SIGSEGV). The shape test is *not* a liveness test: a
//! word whose low bits look like a cons pointer but which does not point at a live
//! cell is unverifiable by any word-local test (the same residual ERTS's C has), so
//! the ownership rule upstream — a spec word a caller stores anywhere must stay
//! rooted or be copied, and the spec term passed here must outlive the call — is
//! the real fix for that half.

use std::collections::BTreeSet;

use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;
use crate::shape::cons_cell;

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
    // SAFETY: `is_tuple` is sound for any word: it routes through
    // `beam_term::boxed::is_boxed_term`, which rejects header words (and every
    // immediate) *before* dereferencing anything — `is_boxed`'s `(x & 1) == 0` bit
    // trick is true for `TAG_PRIMARY_HEADER` too, and `is_boxed_term` is the
    // predicate that closes that hole — and only then reads the boxed word and
    // checks that it is an arityval header. So a header-shaped word is *rejected*,
    // never read.
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
///
/// The walk is gated on [`cons_cell`] (the crate-level shape predicate), so a
/// `TAG_PRIMARY_HEADER` word, any immediate except `NIL` and the all-ones word
/// all terminate the walk as "not a proper list" (`None`), which [`compile`] turns
/// into the `badarg` the oracle raises.
pub(super) fn list_elements(term: Eterm) -> Option<Vec<Eterm>> {
    let mut elements = Vec::new();
    let mut cursor = term;
    while let Some(cell) = cons_cell(cursor) {
        // SAFETY: `cons_cell` established that `cursor` is a `TAG_PRIMARY_LIST`
        // cons-cell word with a non-null cell pointer, so the two words at `cell`
        // (CAR at `+0`, CDR at `+1` — `beam_term::list::{CAR_OFFSET, CDR_OFFSET}`)
        // are addressable. Liveness comes from the caller-owns-the-spec-term
        // contract: the spec term passed to `compile`/`match_spec_run` is
        // caller-owned and outlives the call, so the cells it names are not freed
        // under us. The shape test cannot validate that reachability — a word with
        // cons-shaped low bits naming freed/foreign memory is indistinguishable from
        // a live cell by any word-local test (the residual ERTS's C shares), which
        // is why the ownership rule upstream is the real fix.
        unsafe {
            elements.push(*cell);
            cursor = *cell.add(1);
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

#[cfg(test)]
mod tests {
    //! Unit tests for the proper-list walk ([`list_elements`]) that reads through
    //! the crate-level shape gate ([`crate::shape::cons_cell`], whose own unit tests
    //! live beside it).
    //!
    //! The helpers here are `pub(super)`, so these tests live in the module rather
    //! than in `tests/`; `tests/select_word_classification.rs` covers the same
    //! contract through the public API.

    use super::*;

    /// A leaked `[CAR, CDR]` cell plus the list word naming it.
    ///
    /// The cell is leaked because the walk needs it to outlive this function's
    /// stack frame; a test process leaking a handful of 16-byte cells is harmless.
    fn cons(head: Eterm, tail: Eterm) -> Eterm {
        let cell: &'static mut [Eterm; 2] = Box::leak(Box::new([head, tail]));
        make_list(cell.as_ptr())
    }

    /// A proper list of `elements`, built from leaked cells.
    fn proper_list(elements: &[Eterm]) -> Eterm {
        let mut acc = NIL;
        for element in elements.iter().rev() {
            acc = cons(*element, acc);
        }
        acc
    }

    /// Every header-shaped word the module must refuse: arityvals of several
    /// arities plus two non-arityval thing headers (a float and a map header).
    fn header_words() -> Vec<Eterm> {
        vec![
            make_arityval_zero(),
            make_arityval(1),
            make_arityval(2),
            make_arityval(5),
            make_header(1, FLOAT_SUBTAG),
            make_header(1, MAP_SUBTAG),
        ]
    }

    #[test]
    fn list_elements_reads_a_proper_list() {
        let list = proper_list(&[make_small(1), make_small(2), make_small(3)]);
        assert_eq!(
            list_elements(list),
            Some(vec![make_small(1), make_small(2), make_small(3)])
        );
        assert_eq!(
            list_elements(cons(make_small(7), NIL)),
            Some(vec![make_small(7)])
        );
    }

    #[test]
    fn list_elements_of_nil_is_the_empty_list() {
        // `[]` is a proper (empty) list: no cons cells to read.
        assert_eq!(list_elements(NIL), Some(Vec::new()));
    }

    #[test]
    fn list_elements_of_an_improper_list_is_none() {
        // `[1 | 2]`: the walk stops at the non-cons tail `2`, which is not `NIL`.
        let improper = cons(make_small(1), make_small(2));
        assert_eq!(list_elements(improper), None);
        // `[1 | [2 | 3]]` likewise.
        assert_eq!(list_elements(cons(make_small(1), cons(make_small(2), make_small(3)))), None);
    }

    #[test]
    fn list_elements_of_a_header_word_is_none() {
        for word in header_words() {
            // The reason the old guard was unsound: `is_list`'s `!(x & 0x2)` bit
            // trick is *also* true for a header word (primary tag 0).
            assert!(is_list(word), "a header word passes the is_list bit trick");
            assert_eq!(primary_tag(word), PrimaryTag::Header);
            assert_eq!(list_elements(word), None, "a header word is not a proper list");
        }
    }

    #[test]
    fn list_elements_of_a_non_list_word_is_none() {
        // Every other primary tag: a small int, an atom, a boxed-shaped word (whose
        // pointer is null) and the all-ones word (tag IMMED1).
        let words = [
            make_small(7),
            make_small(0),
            make_small(-1),
            make_atom(Atom::from_raw(3)),
            Eterm::from_raw(0x2),
            Eterm::from_raw(usize::MAX),
        ];
        for word in words {
            assert!(!is_nil(word));
            assert_eq!(list_elements(word), None, "not a proper list: {word:?}");
        }
    }

    #[test]
    fn a_list_word_with_a_null_cell_pointer_is_refused() {
        // `raw == TAG_PRIMARY_LIST` is the cons *tag* with a zero cell pointer — the
        // null check, not the tag check, is what stops this one from dereferencing
        // address 0.
        let word = Eterm::from_raw(TAG_PRIMARY_LIST);
        assert!(is_list(word), "the tag bit alone looks like a cons cell");
        assert!(list_val(word).is_null());
        assert_eq!(crate::shape::cons_cell(word), None);
        assert_eq!(list_elements(word), None);
    }

    #[test]
    fn the_walk_reaches_every_cell_of_a_long_list() {
        // A 1 000-element list: the walk is a plain cursor loop, so this pins that it
        // neither stops early nor reads past the last cell.
        let elements: Vec<Eterm> = (0..1000_i64).map(make_small).collect();
        assert_eq!(list_elements(proper_list(&elements)), Some(elements));
    }
}
